<div align="center">

<img src="packaging/linux/tidespeak.png" alt="Tidespeak" width="96">

# Tidespeak

**Lightweight, encrypted voice and text chat for Linux and Windows, styled after Frametide OS.**

No account, no ads, no tracking. Start a server, enter its address, start talking.

</div>

<img src="docs/screenshots/chat.png" alt="Tidespeak with channels and chat">

## Features

- **Voice channels** like TeamSpeak. Channels are listed on the left with the people in them, and a teal ring shows who is speaking.
- **Text chat per channel** with history, so people who join later see recent messages.
- **Three transmit modes:** voice activation with adjustable sensitivity, push-to-talk (F2 to F9), or always on.
- **Noise suppression** (RNNoise) built in. Keyboard clicks, fans and background hiss stay out.
- **Mute and deafen** with one click, plus quiet sounds when people join or leave.
- **Mic test**, device selection, input gain and output volume, laid out like the Frametide audio page.

<table>
<tr>
<td width="50%"><img src="docs/screenshots/connect.png" alt="Connect"><p align="center"><sub>Connect</sub></p></td>
<td width="50%"><img src="docs/screenshots/settings.png" alt="Settings"><p align="center"><sub>Settings</sub></p></td>
</tr>
</table>

The interface is currently in German.

## Quick start

1. Download the package for your system from **Releases** and extract it.
2. Start the **server**: `tidespeak-server` (Windows: `tidespeak-server.exe`).
   It prints its address and its **fingerprint**. Allow UDP port **9987** in your firewall.
3. Start **Tidespeak**, enter the server address and your name, and click **Verbinden** (Connect).
4. On the first connection Tidespeak remembers the server's fingerprint. Compare it once with what the server prints.
   If it ever changes, the app warns you.

On Linux, `./install.sh` in the extracted folder adds Tidespeak to the app menu (no root needed).

Windows may show a SmartScreen warning the first time, because the programs are not signed.

### Server options

```
tidespeak-server --name "Our Crew" --channels "Lobby,Gaming,Music,AFK" --motd "Welcome!" --port 9987
```

The server key is stored in `tidespeak-data/` (change it with `--data`). Keep this folder. Without it the server gets
a new identity and every client shows a warning.

## How it works

| | |
|---|---|
| **Language** | Rust: as fast as C, without memory bugs |
| **Transport** | QUIC (quinn) with TLS 1.3. Text goes over a reliable stream, voice over datagrams, so a lost packet never holds anything up. One UDP port for everything. |
| **Security** | TLS 1.3 (ring), a self-generated server certificate, and a fingerprint check like SSH (trust on first use) |
| **Audio** | Opus at 48 kHz, 20 ms packets, 24/48/96 kbit/s, in-band FEC and loss concealment, a jitter buffer per speaker, and a mixer with soft limiting |
| **Noise filter** | nnnoiseless (RNNoise in Rust); its speech probability also drives voice activation |
| **Interface** | Slint: native, no browser engine, software rendering |
| **Audio devices** | cpal: PipeWire/PulseAudio and ALSA on Linux, WASAPI on Windows |

**Lightweight:** the client uses about 25 MB of RAM and no CPU while idle. Talking costs about 1.6 % of one CPU core
(noise filter plus Opus), and each person you hear adds about 0.15 %. In a test with 40 participants and five
people talking at once, the server used about 7 MB of RAM and roughly 15 % of one core. It never decodes Opus, it
only forwards packets to everyone in the same channel.

**What is encrypted:** everything between the app and the server (text, voice, names). The server itself can read
and hear the content, just like with TeamSpeak or Discord, so it should run on a machine you trust.

**Known limits:**
- On Linux, push-to-talk only works while the Tidespeak window has focus, because Wayland doesn't allow global
  hotkeys without extra work. Voice activation works everywhere. On Windows, push-to-talk works system-wide.
- Not yet tested with real audio hardware or the Windows GUI. The Windows build and tests pass in CI.

## Building from source

You need Rust (stable) and CMake (for Opus).

```sh
# Linux: sudo apt install libasound2-dev libfontconfig-dev cmake
cargo build --release
# Output: target/release/tidespeak and target/release/tidespeak-server
```

On Windows, install Rust (MSVC) and CMake, then run the same command.

`tidespeak-bot` is a test tool that needs no audio hardware. It connects like a client, can "speak" a test tone,
send chat messages and report what it receives, which makes it useful for load tests.

## History

This repository started as **ASM-Speak**, a voice and text chat written entirely in x64 assembly for Windows. That
version is still available in the git history and as release
[v2.0](https://github.com/ChopstickGuy/asmspeak/releases/tag/v2.0).

## License

GPL-3.0-or-later. Font: Inter (SIL Open Font License). Icons: Lucide (ISC).
