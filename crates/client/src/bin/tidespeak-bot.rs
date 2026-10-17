//! Test and load tool: logs in like a client, can "talk" (sine tone as Opus),
//! chat, and prints what it received. Without audio hardware.
//!
//!   tidespeak-bot <address> <name> [--speak SEC] [--speak-delay SEC] [--listen SEC] [--chat TEXT] [--channel ID] [--pin FP]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::crypto::rustls::QuicClientConfig;
use tidespeak::tofu::TofuVerifier;
use tidespeak_proto::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("tidespeak-bot <adresse> <name> [--speak SEK] [--listen SEK] [--chat TEXT] [--channel ID] [--pin FP] [--speak-delay SEK]");
        std::process::exit(2);
    }
    let address = tidespeak::config::normalize_address(&args[1]);
    let name = args[2].clone();
    let mut speak = 0.0f32;
    let mut listen = 3.0f32;
    let mut chat: Option<String> = None;
    let mut channel: Option<u32> = None;
    let mut pin: Option<String> = None;
    let mut speak_delay = 0.0f32;
    let mut i = 3;
    while i < args.len() {
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--speak" => speak = v.parse()?,
            "--listen" => listen = v.parse()?,
            "--chat" => chat = Some(v),
            "--channel" => channel = Some(v.parse()?),
            "--pin" => pin = Some(v),
            "--speak-delay" => speak_delay = v.parse()?,
            a => return Err(format!("unbekannt: {a}").into()),
        }
        i += 2;
    }

    let addr = tokio::net::lookup_host(&address).await?.next().ok_or("Adresse nicht gefunden")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(TofuVerifier::new(pin.clone(), provider.clone()));
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    let ep = quinn::Endpoint::client((std::net::Ipv4Addr::UNSPECIFIED, 0).into())?;
    let t0 = Instant::now();
    let conn = match ep.connect_with(cfg, addr, "tidespeak")?.await {
        Ok(c) => c,
        Err(e) => {
            println!("FEHLER Verbindung: {e} (Fingerabdruck gesehen: {:?})", verifier.seen());
            std::process::exit(1);
        }
    };
    let handshake = t0.elapsed();
    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &ClientMsg::Hello { version: PROTOCOL_VERSION, name: name.clone() }).await?;
    let you = match read_msg::<_, ServerMsg>(&mut recv).await? {
        Some(ServerMsg::Welcome { you, channels, users, server_name, .. }) => {
            println!(
                "verbunden als #{you} mit „{server_name}“ in {:.1} ms, {} Kanäle, {} Nutzer, Fingerabdruck {}",
                handshake.as_secs_f64() * 1000.0,
                channels.len(),
                users.len(),
                fingerprint_short(&verifier.seen().unwrap_or_default())
            );
            you
        }
        other => return Err(format!("unerwartet: {other:?}").into()),
    };
    if let Some(ch) = channel {
        write_msg(&mut send, &ClientMsg::Join { channel: ch }).await?;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Some(t) = &chat {
        write_msg(&mut send, &ClientMsg::Chat { text: t.clone() }).await?;
    }

    // Sprechen: 440-Hz-Ton, 20-ms-Opus-Pakete im Echtzeittakt
    let speak_conn = conn.clone();
    let speaker = tokio::spawn(async move {
        if speak <= 0.0 {
            return 0usize;
        }
        tokio::time::sleep(Duration::from_secs_f32(speak_delay)).await;
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        enc.set_inband_fec(true).unwrap();
        let frames = (speak * 50.0) as usize;
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        let mut t = 0usize;
        let mut sent = 0;
        for seq in 1..=frames as u16 {
            tick.tick().await;
            let pcm: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|_| {
                    t += 1;
                    (t as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.3
                })
                .collect();
            let opus = enc.encode_vec_float(&pcm, 1275).unwrap();
            let mut pkt = Vec::with_capacity(3 + opus.len());
            pkt.extend_from_slice(&seq.to_le_bytes());
            pkt.push(0);
            pkt.extend_from_slice(&opus);
            if speak_conn.send_datagram(Bytes::from(pkt)).is_ok() {
                sent += 1;
            }
        }
        let mut end = Vec::new();
        end.extend_from_slice(&((frames as u16).wrapping_add(1)).to_le_bytes());
        end.push(VOICE_FLAG_END);
        let _ = speak_conn.send_datagram(Bytes::from(end));
        sent
    });

    // Zuhören
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f32(listen.max(speak + speak_delay + 0.5));
    let mut per_speaker: HashMap<u32, (usize, usize, f32)> = HashMap::new(); // Pakete, dekodiert, Spitzenpegel
    let mut decoders: HashMap<u32, opus::Decoder> = HashMap::new();
    let mut chats = 0;
    let mut pcm = vec![0f32; FRAME_SAMPLES];
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            d = conn.read_datagram() => {
                let Ok(d) = d else { break };
                if d.len() < VOICE_DOWN_HEADER { continue; }
                let sp = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
                let e = per_speaker.entry(sp).or_insert((0, 0, 0.0));
                e.0 += 1;
                let payload = &d[VOICE_DOWN_HEADER..];
                if payload.is_empty() { continue; }
                let dec = decoders.entry(sp).or_insert_with(|| opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap());
                if let Ok(n) = dec.decode_float(payload, &mut pcm, false) {
                    if n == FRAME_SAMPLES {
                        e.1 += 1;
                        e.2 = e.2.max(pcm.iter().fold(0f32, |m, v| m.max(v.abs())));
                    }
                }
            }
            m = read_msg::<_, ServerMsg>(&mut recv) => match m {
                Ok(Some(ServerMsg::Chat(c))) => { chats += 1; println!("chat [{}] {}: {}", c.channel, c.name, c.text); }
                Ok(Some(ServerMsg::UserJoined(u))) => println!("beigetreten: {} (#{})", u.name, u.id),
                Ok(Some(ServerMsg::UserLeft { id })) => println!("verlassen: #{id}"),
                Ok(Some(ServerMsg::History { channel, lines })) => println!("verlauf kanal {channel}: {} zeilen", lines.len()),
                Ok(Some(_)) => {}
                _ => break,
            },
        }
    }
    let sent = speaker.await.unwrap_or(0);
    println!("bot #{you}: gesendet {sent} Sprachpakete, {chats} Chatnachrichten empfangen");
    for (sp, (pk, ok, peak)) in per_speaker {
        println!("  von #{sp}: {pk} Pakete, {ok} dekodiert, Spitzenpegel {peak:.2}");
    }
    let _ = send.finish();
    conn.close(0u32.into(), b"bye");
    ep.wait_idle().await;
    Ok(())
}
