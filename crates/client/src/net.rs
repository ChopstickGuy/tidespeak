//! Network: one QUIC connection to the server, in its own thread with a small
//! tokio runtime. The UI sends `Command`s and receives `Event`s.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use quinn::crypto::rustls::QuicClientConfig;
use tidespeak_proto::*;
use tokio::sync::mpsc;

use crate::audio::Audio;
use crate::tofu::TofuVerifier;

#[derive(Debug)]
pub enum Command {
    Connect { address: String, name: String, pinned: Option<String> },
    Disconnect,
    Join(u32),
    Chat(String),
    State { muted: bool, deafened: bool },
}

#[derive(Debug, Clone)]
pub enum Event {
    Connecting,
    Connected {
        address: String,
        fingerprint: String,
        you: u32,
        server_name: String,
        motd: String,
        channels: Vec<ChannelInfo>,
        users: Vec<UserInfo>,
    },
    /// Connection failed. `changed_key` = (pinned, now) if the server identity has changed.
    Failed { reason: String, changed_key: Option<(String, String)> },
    Disconnected { reason: String },
    UserJoined(UserInfo),
    UserUpdated(UserInfo),
    UserLeft(u32),
    Chat(ChatLine),
    History { channel: u32, lines: Vec<ChatLine> },
    /// Connection quality, about every 2 s
    Stats {
        rtt_ms: u32,
        /// Loss of our voice at the others (highest value, percent)
        uplink_loss: u32,
        /// Loss of incoming voice (average over speakers, percent)
        downlink_loss: f32,
    },
}

pub fn spawn(audio: Arc<Audio>, on_event: impl Fn(Event) + Send + Sync + 'static) -> mpsc::UnboundedSender<Command> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("tidespeak-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio");
            rt.block_on(run(audio, rx, Arc::new(on_event)));
        })
        .expect("Netz-Thread");
    tx
}

type OnEvent = Arc<dyn Fn(Event) + Send + Sync>;

async fn run(audio: Arc<Audio>, mut cmds: mpsc::UnboundedReceiver<Command>, on_event: OnEvent) {
    let mut pending: Option<Command> = None;
    loop {
        let cmd = match pending.take() {
            Some(c) => c,
            None => match cmds.recv().await {
                Some(c) => c,
                None => return,
            },
        };
        let Command::Connect { address, name, pinned } = cmd else { continue };
        on_event(Event::Connecting);
        pending = session(&audio, &mut cmds, &on_event, address, name, pinned).await;
        *audio.shared.voice_out.lock().unwrap() = None;
        audio.reset_playback();
    }
}

fn resolve(address: &str) -> Result<SocketAddr, String> {
    let mut addrs: Vec<SocketAddr> = address
        .to_socket_addrs()
        .map_err(|_| format!("Adresse „{address}“ nicht gefunden"))?
        .collect();
    // IPv4 zuerst – funktioniert in Heimnetzen am zuverlässigsten
    addrs.sort_by_key(|a| a.is_ipv6());
    addrs.into_iter().next().ok_or_else(|| format!("Adresse „{address}“ nicht gefunden"))
}

fn client_config(verifier: Arc<TofuVerifier>) -> Result<quinn::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).map_err(|e| e.to_string())?));
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap()))
        .keep_alive_interval(Some(Duration::from_secs(4)))
        .datagram_receive_buffer_size(Some(128 * 1024));
    cfg.transport_config(Arc::new(transport));
    Ok(cfg)
}

/// Runs one connection until it ends. Returns a command that has
/// already arrived but belongs to the next connection (e.g. a new Connect).
async fn session(
    audio: &Arc<Audio>,
    cmds: &mut mpsc::UnboundedReceiver<Command>,
    on_event: &OnEvent,
    address: String,
    name: String,
    pinned: Option<String>,
) -> Option<Command> {
    let fail = |reason: String| on_event(Event::Failed { reason, changed_key: None });

    let addr = match tokio::task::spawn_blocking({
        let a = address.clone();
        move || resolve(&a)
    })
    .await
    {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            fail(e);
            return None;
        }
        Err(e) => {
            fail(e.to_string());
            return None;
        }
    };

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(TofuVerifier::new(pinned.clone(), provider));
    let cfg = match client_config(verifier.clone()) {
        Ok(c) => c,
        Err(e) => {
            fail(e);
            return None;
        }
    };
    // Socket passend zur Zieladresse (Windows reicht IPv4 nicht über IPv6-Sockets)
    let bind: SocketAddr = if addr.is_ipv4() {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let ep = match quinn::Endpoint::client(bind) {
        Ok(e) => e,
        Err(e) => {
            fail(format!("Netzwerk nicht verfügbar: {e}"));
            return None;
        }
    };
    let connecting = match ep.connect_with(cfg, addr, "tidespeak") {
        Ok(c) => c,
        Err(e) => {
            fail(format!("Verbindung nicht möglich: {e}"));
            return None;
        }
    };

    // Verbindungsaufbau – abbrechbar
    let conn = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(8), connecting) => match r {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                match (&pinned, verifier.seen()) {
                    (Some(p), Some(now)) if !p.eq_ignore_ascii_case(&now) => on_event(Event::Failed {
                        reason: "Die Identität des Servers hat sich geändert!".into(),
                        changed_key: Some((p.clone(), now)),
                    }),
                    _ => fail(format!("Server nicht erreichbar ({e})")),
                }
                return None;
            }
            Err(_) => {
                fail("Server antwortet nicht. Läuft der Server, ist UDP-Port 9987 freigegeben?".into());
                return None;
            }
        },
        c = cmds.recv() => {
            on_event(Event::Disconnected { reason: "Abgebrochen".into() });
            return c.filter(|c| matches!(c, Command::Connect { .. }));
        }
    };
    let fingerprint = verifier.seen().unwrap_or_default();

    let (mut send, mut recv) = match conn.open_bi().await {
        Ok(s) => s,
        Err(e) => {
            fail(e.to_string());
            return None;
        }
    };
    if let Err(e) = write_msg(&mut send, &ClientMsg::Hello { version: PROTOCOL_VERSION, name }).await {
        fail(e.to_string());
        return None;
    }
    let welcome = match tokio::time::timeout(Duration::from_secs(8), read_msg::<_, ServerMsg>(&mut recv)).await {
        Ok(Ok(Some(m))) => m,
        Ok(Ok(None)) | Err(_) => {
            fail("Server hat nicht geantwortet".into());
            return None;
        }
        Ok(Err(e)) => {
            fail(e.to_string());
            return None;
        }
    };
    let you = match welcome {
        ServerMsg::Welcome { you, server_name, motd, channels, users } => {
            on_event(Event::Connected {
                address: address.clone(),
                fingerprint: fingerprint.clone(),
                you,
                server_name,
                motd,
                channels,
                users,
            });
            you
        }
        ServerMsg::Rejected { reason } => {
            fail(reason);
            return None;
        }
        _ => {
            fail("Unerwartete Antwort vom Server".into());
            return None;
        }
    };
    log::info!("verbunden mit {address} als #{you}");

    // Sprache: Encoder -> Datagramme
    let (voice_tx, mut voice_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    *audio.shared.voice_out.lock().unwrap() = Some(voice_tx);
    let state_msg = ClientMsg::State {
        muted: audio.shared.muted.load(std::sync::atomic::Ordering::Relaxed),
        deafened: audio.shared.deafened.load(std::sync::atomic::Ordering::Relaxed),
    };
    let _ = write_msg(&mut send, &state_msg).await;

    let mut next = None;
    let mut feedback: std::collections::HashMap<u32, (u8, std::time::Instant)> = Default::default();
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let reason = loop {
        tokio::select! {
            _ = ticker.tick() => {
                // eigene Empfangsqualität an den Server melden -> die Sprecher passen ihre Fehlerkorrektur an
                let rx = audio.shared.rx.lock().unwrap().clone();
                if !rx.is_empty() {
                    let loss = rx.iter().map(|(id, r)| (*id, r.loss.round().clamp(0.0, 100.0) as u8)).collect();
                    let _ = write_msg(&mut send, &ClientMsg::Report { loss }).await;
                }
                feedback.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(6));
                let uplink = feedback.values().map(|(l, _)| *l as u32).max().unwrap_or(0);
                audio.shared.uplink_loss.store(uplink, std::sync::atomic::Ordering::Relaxed);
                let downlink = if rx.is_empty() { 0.0 } else { rx.values().map(|r| r.loss).sum::<f32>() / rx.len() as f32 };
                on_event(Event::Stats { rtt_ms: (conn.rtt().as_micros().div_ceil(1000) as u32).max(1), uplink_loss: uplink, downlink_loss: downlink });
            }
            msg = read_msg::<_, ServerMsg>(&mut recv) => match msg {
                Ok(Some(m)) => match m {
                    ServerMsg::UserJoined(u) => on_event(Event::UserJoined(u)),
                    ServerMsg::UserUpdated(u) => on_event(Event::UserUpdated(u)),
                    ServerMsg::UserLeft { id } => on_event(Event::UserLeft(id)),
                    ServerMsg::Chat(c) => on_event(Event::Chat(c)),
                    ServerMsg::History { channel, lines } => on_event(Event::History { channel, lines }),
                    ServerMsg::Feedback { listener, loss } => {
                        feedback.insert(listener, (loss, std::time::Instant::now()));
                    }
                    ServerMsg::Rejected { reason } => break reason,
                    ServerMsg::Welcome { .. } => {}
                },
                Ok(None) => break "Der Server hat die Verbindung beendet.".to_string(),
                Err(_) => break close_reason(&conn),
            },
            d = conn.read_datagram() => match d {
                Ok(d) => audio.push_datagram(&d),
                Err(_) => break close_reason(&conn),
            },
            Some(v) = voice_rx.recv() => {
                if conn.send_datagram(Bytes::from(v)).is_err() {
                    log::debug!("Sprachpaket verworfen");
                }
            }
            c = cmds.recv() => match c {
                Some(Command::Disconnect) | None => break String::new(),
                Some(c @ Command::Connect { .. }) => { next = Some(c); break String::new(); }
                Some(Command::Join(ch)) => { let _ = write_msg(&mut send, &ClientMsg::Join { channel: ch }).await; }
                Some(Command::Chat(t)) => { let _ = write_msg(&mut send, &ClientMsg::Chat { text: t }).await; }
                Some(Command::State { muted, deafened }) => {
                    let _ = write_msg(&mut send, &ClientMsg::State { muted, deafened }).await;
                }
            },
        }
    };
    *audio.shared.voice_out.lock().unwrap() = None;
    let _ = send.finish();
    conn.close(0u32.into(), b"bye");
    on_event(Event::Disconnected { reason });
    next
}

fn close_reason(conn: &quinn::Connection) -> String {
    match conn.close_reason() {
        Some(quinn::ConnectionError::TimedOut) => "Verbindung zum Server verloren (Zeitüberschreitung).".into(),
        Some(quinn::ConnectionError::ApplicationClosed(_)) => "Der Server wurde beendet.".into(),
        Some(e) => format!("Verbindung getrennt: {e}"),
        None => "Verbindung getrennt.".into(),
    }
}
