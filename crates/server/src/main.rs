//! Tidespeak server: channels, text chat and voice relay over QUIC.
//!
//! Each client has one control stream (reliable) and sends voice as QUIC
//! datagrams. The server forwards voice packets unchanged to everyone else in
//! the same channel; Opus is never decoded here, so the server stays very light.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{BufMut, Bytes, BytesMut};
use log::{info, warn};
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tidespeak_proto::*;
use tokio::sync::mpsc;

const HISTORY_PER_CHANNEL: usize = 200;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Chat: bursts of 5 messages, then one more every 0.5 s.
const CHAT_BURST: f32 = 5.0;
const CHAT_REFILL_PER_SEC: f32 = 2.0;
/// Voice: 50 packets/s are normal (20 ms); anything well above that is dropped.
const VOICE_MAX_PER_SEC: u32 = 80;

struct Options {
    port: u16,
    name: String,
    motd: String,
    channels: Vec<String>,
    data_dir: PathBuf,
}

struct User {
    info: UserInfo,
    conn: quinn::Connection,
    tx: mpsc::UnboundedSender<ServerMsg>,
    chat_tokens: f32,
    chat_last: Instant,
    voice_window: Instant,
    voice_count: u32,
    last_report: Instant,
}

struct State {
    server_name: String,
    motd: String,
    channels: Vec<ChannelInfo>,
    users: HashMap<u32, User>,
    history: HashMap<u32, VecDeque<ChatLine>>,
    next_id: u32,
}

type Shared = Arc<Mutex<State>>;

impl State {
    fn broadcast(&self, msg: &ServerMsg, except: Option<u32>) {
        for (id, u) in &self.users {
            if Some(*id) != except {
                let _ = u.tx.send(msg.clone());
            }
        }
    }

    fn broadcast_channel(&self, channel: u32, msg: &ServerMsg) {
        for u in self.users.values().filter(|u| u.info.channel == channel) {
            let _ = u.tx.send(msg.clone());
        }
    }

    fn unique_name(&self, wanted: &str) -> String {
        let taken = |n: &str| self.users.values().any(|u| u.info.name.eq_ignore_ascii_case(n));
        if !taken(wanted) {
            return wanted.to_string();
        }
        (2..)
            .map(|i| format!("{wanted} ({i})"))
            .find(|n| !taken(n))
            .unwrap()
    }

    fn history(&self, channel: u32) -> ServerMsg {
        let lines = self.history.get(&channel).map(|h| h.iter().cloned().collect()).unwrap_or_default();
        ServerMsg::History { channel, lines }
    }
}

fn parse_args() -> Options {
    let mut o = Options {
        port: DEFAULT_PORT,
        name: "Tidespeak Server".into(),
        motd: String::new(),
        channels: vec!["Lobby".into(), "Gaming".into(), "Musik".into(), "AFK".into()],
        data_dir: PathBuf::from("tidespeak-data"),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{a} braucht einen Wert")));
        match a.as_str() {
            "-p" | "--port" => o.port = val().parse().unwrap_or_else(|_| usage("ungültiger Port")),
            "-n" | "--name" => o.name = val(),
            "-m" | "--motd" => o.motd = val(),
            "-c" | "--channels" => {
                o.channels = val().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                if o.channels.is_empty() {
                    usage("mindestens ein Kanal nötig");
                }
            }
            "-d" | "--data" => o.data_dir = PathBuf::from(val()),
            "-h" | "--help" => usage(""),
            "-V" | "--version" => {
                println!("tidespeak-server {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => usage(&format!("unbekannte Option: {other}")),
        }
    }
    o
}

fn usage(err: &str) -> ! {
    if !err.is_empty() {
        eprintln!("Fehler: {err}\n");
    }
    eprintln!(
        "tidespeak-server – Sprach- und Textchat-Server\n\n\
         Optionen:\n  \
           -p, --port <PORT>        UDP-Port (Standard {DEFAULT_PORT})\n  \
           -n, --name <NAME>        Servername\n  \
           -m, --motd <TEXT>        Begrüßungstext\n  \
           -c, --channels <A,B,C>   Kanäle (Standard: Lobby,Gaming,Musik,AFK)\n  \
           -d, --data <ORDNER>      Ordner für den Server-Schlüssel (Standard: tidespeak-data)\n  \
           -h, --help               Hilfe"
    );
    std::process::exit(if err.is_empty() { 0 } else { 2 })
}

/// Loads the server certificate or creates a new one. The certificate is the
/// server's identity – clients remember its fingerprint.
fn load_or_create_identity(dir: &Path) -> std::io::Result<(CertificateDer<'static>, PrivateKeyDer<'static>, bool)> {
    let cert_path = dir.join("server-cert.der");
    let key_path = dir.join("server-key.der");
    if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        return Ok((CertificateDer::from(cert), PrivatePkcs8KeyDer::from(key).into(), false));
    }
    std::fs::create_dir_all(dir)?;
    let generated = rcgen::generate_simple_self_signed(vec!["tidespeak".to_string()])
        .map_err(std::io::Error::other)?;
    let cert = generated.cert.der().to_vec();
    let key = generated.signing_key.serialize_der();
    write_private(&key_path, &key)?;
    std::fs::write(&cert_path, &cert)?;
    Ok((CertificateDer::from(cert), PrivatePkcs8KeyDer::from(key).into(), true))
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)
    }
}

/// UDP socket for IPv4 *and* IPv6 (one socket, dual stack). Friends on
/// DS-Lite/IPv6-only connections can also connect that way. Larger buffers
/// protect against packet loss when many people talk at once.
fn bind_socket(port: u16) -> std::io::Result<(std::net::UdpSocket, bool)> {
    use socket2::{Domain, Protocol, Socket, Type};
    let open = |v6: bool| -> std::io::Result<std::net::UdpSocket> {
        let domain = if v6 { Domain::IPV6 } else { Domain::IPV4 };
        let s = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        if v6 {
            s.set_only_v6(false)?;
        }
        let _ = s.set_recv_buffer_size(4 << 20);
        let _ = s.set_send_buffer_size(4 << 20);
        s.set_nonblocking(true)?;
        let addr: SocketAddr = if v6 {
            (std::net::Ipv6Addr::UNSPECIFIED, port).into()
        } else {
            (std::net::Ipv4Addr::UNSPECIFIED, port).into()
        };
        s.bind(&addr.into())?;
        Ok(s.into())
    };
    match open(true) {
        Ok(s) => Ok((s, true)),
        Err(_) => open(false).map(|s| (s, false)),
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn local_ip() -> Option<std::net::IpAddr> {
    // Kein Paket wird gesendet: connect() wählt nur die passende Schnittstelle.
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_target(false)
        .init();
    let opts = parse_args();

    let (cert, key, created) = match load_or_create_identity(&opts.data_dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Fehler: Server-Schlüssel in {:?} nicht lesbar/schreibbar: {e}", opts.data_dir);
            std::process::exit(1);
        }
    };
    let fp = fingerprint(&cert);

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("Zertifikat");
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;

    let mut transport = quinn::TransportConfig::default();
    transport
        .max_idle_timeout(Some(Duration::from_secs(15).try_into().unwrap()))
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .max_concurrent_bidi_streams(1u32.into())
        .max_concurrent_uni_streams(0u32.into())
        .datagram_receive_buffer_size(Some(64 * 1024))
        .datagram_send_buffer_size(256 * 1024);
    let mut server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls).expect("QUIC-TLS")));
    server_config.transport_config(Arc::new(transport));

    let (socket, dual) = match bind_socket(opts.port) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Fehler: Port {} (UDP) nicht verfügbar: {e}", opts.port);
            std::process::exit(1);
        }
    };
    let endpoint = match quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        Arc::new(quinn::TokioRuntime),
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Fehler: Server konnte nicht starten: {e}");
            std::process::exit(1);
        }
    };

    let channels = opts
        .channels
        .iter()
        .enumerate()
        .map(|(i, n)| ChannelInfo { id: i as u32, name: n.clone(), topic: String::new() })
        .collect::<Vec<_>>();

    println!();
    println!("  Tidespeak Server {}", env!("CARGO_PKG_VERSION"));
    println!("  ─────────────────────────────────────────────");
    println!("  Name:            {}", opts.name);
    println!("  Port:            {} (UDP, {})", opts.port, if dual { "IPv4 + IPv6" } else { "nur IPv4" });
    if let Some(ip) = local_ip() {
        println!("  Im Netzwerk:     {ip}:{}", opts.port);
    }
    println!("  Kanäle:          {}", opts.channels.join(", "));
    println!("  Fingerabdruck:   {}", fingerprint_short(&fp));
    if created {
        println!("  (neuer Server-Schlüssel in {:?} erzeugt)", opts.data_dir);
    }
    println!("  ─────────────────────────────────────────────");
    println!("  Clients zeigen beim ersten Verbinden denselben Fingerabdruck an.");
    println!("  Von außerhalb: deine öffentliche IP (z. B. ifconfig.me) – im Router UDP {} weiterleiten.", opts.port);
    println!();

    let state: Shared = Arc::new(Mutex::new(State {
        server_name: opts.name.clone(),
        motd: opts.motd.clone(),
        channels,
        users: HashMap::new(),
        history: HashMap::new(),
        next_id: 1,
    }));

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let state = state.clone();
                tokio::spawn(async move {
                    let remote = incoming.remote_address();
                    if let Err(e) = handle_connection(incoming, state).await {
                        log::debug!("{remote}: {e}");
                    }
                });
            }
            _ = &mut shutdown => {
                info!("Server wird beendet");
                endpoint.close(0u32.into(), b"server shutdown");
                break;
            }
        }
    }
    endpoint.wait_idle().await;
}

async fn handle_connection(incoming: quinn::Incoming, state: Shared) -> Result<(), Box<dyn std::error::Error>> {
    let conn = incoming.await?;
    let remote = conn.remote_address();
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi()).await??;
    let hello: Option<ClientMsg> = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_msg(&mut recv)).await??;
    let name = match hello {
        Some(ClientMsg::Hello { version, name }) if version == PROTOCOL_VERSION => name,
        Some(ClientMsg::Hello { .. }) => {
            let reason = "Diese Tidespeak-Version passt nicht zum Server – bitte aktualisieren.".to_string();
            write_msg(&mut send, &ServerMsg::Rejected { reason }).await?;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(200)).await;
            return Ok(());
        }
        _ => return Err("kein gültiges Hello".into()),
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();
    let (id, info) = {
        let mut st = state.lock().unwrap();
        let id = st.next_id;
        st.next_id = st.next_id.wrapping_add(1).max(1);
        let base = sanitize_name(&name);
        let base = if base.is_empty() { "Gast".to_string() } else { base };
        let info = UserInfo { id, name: st.unique_name(&base), channel: 0, muted: false, deafened: false };
        let mut users: Vec<UserInfo> = st.users.values().map(|u| u.info.clone()).collect();
        users.push(info.clone());
        let _ = tx.send(ServerMsg::Welcome {
            you: id,
            server_name: st.server_name.clone(),
            motd: st.motd.clone(),
            channels: st.channels.clone(),
            users,
        });
        let _ = tx.send(st.history(0));
        st.broadcast(&ServerMsg::UserJoined(info.clone()), None);
        st.users.insert(
            id,
            User {
                info: info.clone(),
                conn: conn.clone(),
                tx,
                chat_tokens: CHAT_BURST,
                chat_last: Instant::now(),
                voice_window: Instant::now(),
                voice_count: 0,
                last_report: Instant::now() - Duration::from_secs(10),
            },
        );
        (id, info)
    };
    info!("{} beigetreten ({remote})", info.name);

    // Sender: Nachrichten aus der Warteschlange in den Kontroll-Stream schreiben
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });

    // Sprache: Datagramme an alle im selben Kanal weiterleiten
    let voice_state = state.clone();
    let voice_conn = conn.clone();
    let voice = tokio::spawn(async move {
        while let Ok(data) = voice_conn.read_datagram().await {
            relay_voice(&voice_state, id, &data);
        }
    });

    let result = control_loop(&state, id, &mut recv).await;

    voice.abort();
    writer.abort();
    let name = {
        let mut st = state.lock().unwrap();
        let name = st.users.remove(&id).map(|u| u.info.name).unwrap_or_default();
        st.broadcast(&ServerMsg::UserLeft { id }, None);
        name
    };
    info!("{name} hat den Server verlassen");
    conn.close(0u32.into(), b"bye");
    result
}

async fn control_loop(state: &Shared, id: u32, recv: &mut quinn::RecvStream) -> Result<(), Box<dyn std::error::Error>> {
    while let Some(msg) = read_msg::<_, ClientMsg>(recv).await? {
        let mut st = state.lock().unwrap();
        match msg {
            ClientMsg::Hello { .. } => {}
            ClientMsg::Join { channel } => {
                if !st.channels.iter().any(|c| c.id == channel) {
                    continue;
                }
                let Some(u) = st.users.get_mut(&id) else { break };
                if u.info.channel == channel {
                    continue;
                }
                u.info.channel = channel;
                let info = u.info.clone();
                let history = st.history(channel);
                if let Some(u) = st.users.get(&id) {
                    let _ = u.tx.send(history);
                }
                st.broadcast(&ServerMsg::UserUpdated(info), None);
            }
            ClientMsg::Chat { text } => {
                let text = sanitize_chat(&text);
                if text.is_empty() {
                    continue;
                }
                let Some(u) = st.users.get_mut(&id) else { break };
                let now = Instant::now();
                u.chat_tokens = (u.chat_tokens + now.duration_since(u.chat_last).as_secs_f32() * CHAT_REFILL_PER_SEC)
                    .min(CHAT_BURST);
                u.chat_last = now;
                if u.chat_tokens < 1.0 {
                    continue; // zu schnell – Nachricht verwerfen
                }
                u.chat_tokens -= 1.0;
                let line = ChatLine { channel: u.info.channel, from: id, name: u.info.name.clone(), text, time: now_unix() };
                let channel = line.channel;
                let history = st.history.entry(channel).or_default();
                history.push_back(line.clone());
                while history.len() > HISTORY_PER_CHANNEL {
                    history.pop_front();
                }
                st.broadcast_channel(channel, &ServerMsg::Chat(line));
            }
            ClientMsg::Report { loss } => {
                // Empfangsqualität an die jeweiligen Sprecher weitergeben (höchstens 1× pro Sekunde)
                let Some(u) = st.users.get_mut(&id) else { break };
                if u.last_report.elapsed() < Duration::from_secs(1) {
                    continue;
                }
                u.last_report = Instant::now();
                for (speaker, pct) in loss.into_iter().take(64) {
                    if speaker != id {
                        if let Some(s) = st.users.get(&speaker) {
                            let _ = s.tx.send(ServerMsg::Feedback { listener: id, loss: pct.min(100) });
                        }
                    }
                }
            }
            ClientMsg::State { muted, deafened } => {
                let Some(u) = st.users.get_mut(&id) else { break };
                u.info.muted = muted || deafened;
                u.info.deafened = deafened;
                let info = u.info.clone();
                st.broadcast(&ServerMsg::UserUpdated(info), None);
            }
        }
    }
    Ok(())
}

fn relay_voice(state: &Shared, from: u32, data: &[u8]) {
    if data.len() < VOICE_UP_HEADER || data.len() > 1200 {
        return;
    }
    let mut st = state.lock().unwrap();
    let Some(sender) = st.users.get_mut(&from) else { return };
    if sender.info.muted {
        return;
    }
    let now = Instant::now();
    if now.duration_since(sender.voice_window) >= Duration::from_secs(1) {
        sender.voice_window = now;
        sender.voice_count = 0;
    }
    sender.voice_count += 1;
    if sender.voice_count > VOICE_MAX_PER_SEC {
        if sender.voice_count == VOICE_MAX_PER_SEC + 1 {
            warn!("{}: zu viele Sprachpakete, werden verworfen", sender.info.name);
        }
        return;
    }
    let channel = sender.info.channel;

    let mut buf = BytesMut::with_capacity(4 + data.len());
    buf.put_u32_le(from);
    buf.put_slice(data);
    let packet: Bytes = buf.freeze();
    for u in st.users.values() {
        if u.info.id != from && u.info.channel == channel && !u.info.deafened {
            let _ = u.conn.send_datagram(packet.clone());
        }
    }
}
