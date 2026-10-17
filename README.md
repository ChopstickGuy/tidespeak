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
- **Automatic mic level:** quiet XLR mics and audio interfaces are brought up to a clear speaking level, loud ones are
  tamed. Tidespeak also detects which input of an interface actually has the mic on it.
- **Robust over the internet:** an adaptive jitter buffer per speaker, Opus error correction that gets stronger
  automatically when listeners report packet loss, and concealment for anything that still goes missing.
- **Volume per person:** click a name in the sidebar to make someone louder or quieter, just for you.
- **Connection info:** ping, incoming loss, and how well your own voice arrives at the others.
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

### Hosting at home

1. In your router, forward **UDP** port 9987 to the local IP of the PC running the server (the server prints it under
   "Im Netzwerk"). TCP is not needed. Give that PC a fixed local IP in the router.
2. Allow the server through the firewall (Windows asks on first start; on Linux e.g. `sudo ufw allow 9987/udp`).
3. Friends connect to your **public IP** (see e.g. https://ifconfig.me) or a DynDNS name such as `myname.duckdns.org`.
4. Test from outside your network, for example from a laptop on a phone hotspot. Many routers can't reach their own
   public IP from inside.

If your connection has no public IPv4 address (DS-Lite / CGNAT, common with cable and many fibre providers), port
forwarding has no effect. The server listens on IPv4 **and IPv6**, so friends with IPv6 can still reach you via your
IPv6 address (allow UDP 9987 for IPv6 in the router). Alternatives: Tailscale/ZeroTier, or a small rented server.

### Tips for good audio

- **Audio interface / XLR mic:** set the gain on the interface so that normal speech lights up the green LED, not
  just barely. Automatic mic level does the rest. Tidespeak picks the input the mic is plugged into by itself.
- **Use headphones,** so your speakers don't feed back into your mic.
- **Wired beats Wi-Fi.** Under Settings → Verbindung you can see the loss for both directions. Up to about 2 % is
  inaudible thanks to error correction.

## How it works

| | |
|---|---|
| **Language** | Rust: as fast as C, without memory bugs |
| **Transport** | QUIC (quinn) with TLS 1.3. Text goes over a reliable stream, voice over datagrams, so a lost packet never holds anything up. One UDP port for everything. |
| **Security** | TLS 1.3 (ring), a self-generated server certificate, and a fingerprint check like SSH (trust on first use) |
| **Audio** | Opus at 48 kHz, 20 ms packets, 32/64/96 kbit/s, in-band FEC whose strength follows the loss listeners report, loss concealment, an adaptive jitter buffer per speaker (40–240 ms), and a mixer with soft limiting |
| **Mic chain** | active-channel detection, band-limited (Kaiser-windowed sinc) resampling, 80 Hz high-pass, automatic gain, RNNoise, voice activation with hysteresis and 60 ms pre-roll so word beginnings aren't cut |
| **Noise filter** | nnnoiseless (RNNoise in Rust); its speech probability also drives voice activation, so typing alone doesn't open the mic |
| **Interface** | Slint: native, no browser engine, software rendering |
| **Audio devices** | cpal: PipeWire/PulseAudio and ALSA on Linux, WASAPI on Windows |

**Lightweight:** the client uses about 25 MB of RAM and no CPU while idle. Talking costs about 3.4 % of one CPU core
for the whole mic chain (resampling, noise filter, automatic gain, Opus), and each person you hear adds about 0.15 %. In a test with 40 participants and five
people talking at once, the server used about 7 MB of RAM and roughly 15 % of one core. It never decodes Opus, it
only forwards packets to everyone in the same channel.

**Measured improvements in 3.1** (simulation with real speech: XLR mic on input 1 of a stereo interface at 44.1 kHz,
keyboard typing, and a jittery internet line):

| | 3.0 | 3.1 |
|---|---|---|
| Speech transmitted, normal mic level | 85.5 % (syllables cut off) | 99.9 % |
| Speech transmitted, quiet mic | 0 % | 96.3 % |
| Level at the listener, normal mic | -45.6 dBFS (very quiet) | -29.3 dBFS |
| Resampling distortion 44.1 → 48 kHz | -21 dB (tinny) | -95 dB |
| Missing audio, typical Wi-Fi / cable line | 9.8 % | 2.3 % (most of it rebuilt by FEC) |
| Missing audio, bad line | 30 % | 5 % |

Run it yourself with `cargo test --release -p tidespeak --lib quality_report -- --ignored --nocapture` (needs a speech
recording, see the test for details).

**What is encrypted:** everything between the app and the server (text, voice, names). The server itself can read
and hear the content, just like with TeamSpeak or Discord, so it should run on a machine you trust.

**Known limits:**
- On Linux, push-to-talk only works while the Tidespeak window has focus, because Wayland doesn't allow global
  hotkeys without extra work. Voice activation works everywhere. On Windows, push-to-talk works system-wide.
- Server and clients must use the same version (3.1 changed the protocol). An older client gets a clear message.
- The Windows GUI hasn't been tested on real hardware yet. The Windows build and tests pass in CI.

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
