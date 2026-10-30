//! Tidespeak protocol, shared by server and client.
//!
//! Transport is QUIC (TLS 1.3). Every client opens exactly one bidirectional
//! stream for control messages (length-prefixed postcard frames). Voice goes
//! through unreliable QUIC datagrams, so a lost packet never delays the next one.
//!
//! Voice datagram, client -> server:  `[seq u16 LE][flags u8][opus …]`
//! Voice datagram, server -> client:  `[speaker u32 LE][seq u16 LE][flags u8][opus …]`

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"tidespeak/1";
pub const PROTOCOL_VERSION: u32 = 3;
pub const DEFAULT_PORT: u16 = 9987;
pub const MAX_NAME_CHARS: usize = 32;
pub const MAX_CHAT_CHARS: usize = 2000;
pub const MAX_POKE_CHARS: usize = 200;
pub const MAX_FRAME_BYTES: usize = 512 * 1024;

/// Voice: 48 kHz mono, 20 ms per packet.
pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 960;

/// Last packet of a talk spurt: the receiver ends playback cleanly.
pub const VOICE_FLAG_END: u8 = 1;
pub const VOICE_UP_HEADER: usize = 3;
pub const VOICE_DOWN_HEADER: usize = 7;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChannelInfo {
    pub id: u32,
    pub name: String,
    pub topic: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserInfo {
    pub id: u32,
    pub name: String,
    pub channel: u32,
    pub muted: bool,
    pub deafened: bool,
    /// Away (AFK)
    pub away: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChatLine {
    pub channel: u32,
    pub from: u32,
    pub name: String,
    pub text: String,
    /// Unix time in seconds
    pub time: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ClientMsg {
    Hello { version: u32, name: String },
    Join { channel: u32 },
    Chat { text: String },
    State { muted: bool, deafened: bool, away: bool },
    /// Poke another user (with an optional short message)
    Poke { target: u32, text: String },
    /// Receive quality per speaker: (speaker id, loss in percent). About every 2 s.
    Report { loss: Vec<(u32, u8)> },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ServerMsg {
    Welcome {
        you: u32,
        server_name: String,
        motd: String,
        channels: Vec<ChannelInfo>,
        users: Vec<UserInfo>,
    },
    Rejected { reason: String },
    UserJoined(UserInfo),
    UserUpdated(UserInfo),
    UserLeft { id: u32 },
    Chat(ChatLine),
    History { channel: u32, lines: Vec<ChatLine> },
    /// How well your voice arrives at `listener`: loss in percent
    Feedback { listener: u32, loss: u8 },
    /// Someone poked you
    Poked { from: u32, name: String, text: String },
}

#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    TooLarge(usize),
    Decode(postcard::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "{e}"),
            FrameError::TooLarge(n) => write!(f, "frame too large ({n} bytes)"),
            FrameError::Decode(e) => write!(f, "invalid message: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// Writes one message: `[u32 LE length][postcard]`.
pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = postcard::to_stdvec(msg).map_err(FrameError::Decode)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    Ok(())
}

/// Reads one message. `Ok(None)` means the other side closed the stream cleanly.
pub async fn read_msg<R, T>(r: &mut R) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    postcard::from_bytes(&body).map(Some).map_err(FrameError::Decode)
}

/// SHA-256 of the server certificate as uppercase hex (64 characters).
pub fn fingerprint(cert_der: &[u8]) -> String {
    Sha256::digest(cert_der).iter().map(|b| format!("{b:02X}")).collect()
}

/// Short, readable form for display: `AB12 CD34 EF56 …` (first 16 bytes).
pub fn fingerprint_short(full: &str) -> String {
    full.as_bytes()
        .chunks(4)
        .take(8)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cleans up a display name: no control characters, trimmed, at most `MAX_NAME_CHARS` long.
pub fn sanitize_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    cleaned.trim().to_string()
}

/// Cleans up chat text: keeps line breaks, drops all other control characters.
pub fn sanitize_chat(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .take(MAX_CHAT_CHARS)
        .collect();
    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let msg = ClientMsg::Hello { version: 1, name: "Jürgen 🎧".into() };
        let mut buf = Vec::new();
        write_msg(&mut buf, &msg).await.unwrap();
        let mut rd = &buf[..];
        let back: ClientMsg = read_msg(&mut rd).await.unwrap().unwrap();
        assert_eq!(back, msg);
        let eof: Option<ClientMsg> = read_msg(&mut rd).await.unwrap();
        assert!(eof.is_none());
    }

    #[test]
    fn names() {
        assert_eq!(sanitize_name("  Anna\u{0007} "), "Anna");
        assert_eq!(sanitize_name(&"x".repeat(100)).chars().count(), MAX_NAME_CHARS);
        assert_eq!(sanitize_chat("hi\nthere\r"), "hi\nthere");
    }

    #[test]
    fn fingerprints() {
        let fp = fingerprint(b"test");
        assert_eq!(fp.len(), 64);
        assert_eq!(fingerprint_short(&fp).len(), 8 * 4 + 7);
    }
}
