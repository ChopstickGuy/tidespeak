//! Audio pipeline.
//!
//!   Mic ─► Ring ─► [encoder thread] RNNoise ─► level/voice activation ─► Opus ─► network
//!   network ─► [mixer thread] jitter buffer per speaker ─► Opus (FEC/PLC) ─► mix ─► Ring ─► speakers
//!
//! The cpal callbacks only copy samples into lock-free rings (rtrb). All
//! computation happens in two ordinary threads, so the audio callbacks never
//! wait and never allocate.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use tidespeak_proto::{FRAME_SAMPLES, SAMPLE_RATE, VOICE_DOWN_HEADER, VOICE_FLAG_END};

use crate::config::VoiceMode;

const DENOISE_FRAME: usize = nnnoiseless::DenoiseState::FRAME_SIZE; // 480 = 10 ms
const MIC_RING: usize = SAMPLE_RATE as usize; // 1 s
const OUT_RING: usize = SAMPLE_RATE as usize / 2; // 0.5 s
/// Fill level of the output ring that the mixer aims for (latency vs. dropouts)
const OUT_TARGET: usize = FRAME_SAMPLES * 3;
const JITTER_SLOTS: usize = 16;
const VAD_HANGOVER_FRAMES: u32 = 15; // 300 ms keeps sending after the last sound
const SPEAKER_IDLE: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct AudioSettings {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_gain: f32,
    pub output_volume: f32,
    pub voice_mode: VoiceMode,
    pub activation_threshold: f32,
    pub noise_suppression: bool,
    pub bitrate: i32,
    pub ptt_key: u32,
}

/// State shared between the UI, the network and the audio threads
pub struct Shared {
    pub settings: Mutex<AudioSettings>,
    pub muted: AtomicBool,
    pub deafened: AtomicBool,
    pub mic_test: AtomicBool,
    /// Push-to-talk held (set by the UI; on Windows also polled globally)
    pub ptt: AtomicBool,
    pub transmitting: AtomicBool,
    /// Mic level 0.0 … 1.0 (-60 dBFS … 0 dBFS), stored as f32 bits
    level: AtomicU32,
    pub speaking: Mutex<HashMap<u32, Instant>>,
    /// Destination for finished voice packets (set by the network while connected)
    pub voice_out: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    pub input_error: Mutex<Option<String>>,
    pub output_error: Mutex<Option<String>>,
}

impl Shared {
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
}

enum Incoming {
    Voice { speaker: u32, seq: u16, flags: u8, data: Vec<u8> },
    Local(Vec<f32>),
    Reset,
}

enum DeviceCmd {
    Restart,
}

pub struct Audio {
    pub shared: Arc<Shared>,
    incoming: Sender<Incoming>,
    devices: Sender<DeviceCmd>,
}

impl Audio {
    pub fn start(settings: AudioSettings) -> Audio {
        let shared = Arc::new(Shared {
            settings: Mutex::new(settings),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            mic_test: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
            transmitting: AtomicBool::new(false),
            level: AtomicU32::new(0f32.to_bits()),
            speaking: Mutex::new(HashMap::new()),
            voice_out: Mutex::new(None),
            input_error: Mutex::new(None),
            output_error: Mutex::new(None),
        });
        let (incoming_tx, incoming_rx) = mpsc::channel();
        let (dev_tx, dev_rx) = mpsc::channel();
        let (mic_tx, mic_rx) = mpsc::channel::<rtrb::Consumer<f32>>();
        let (spk_tx, spk_rx) = mpsc::channel::<rtrb::Producer<f32>>();

        let enc_shared = shared.clone();
        let local_tx = incoming_tx.clone();
        let encoder = thread::Builder::new()
            .name("tidespeak-encoder".into())
            .spawn(move || encoder_thread(enc_shared, mic_rx, local_tx))
            .expect("Encoder-Thread");
        let mix_shared = shared.clone();
        thread::Builder::new()
            .name("tidespeak-mixer".into())
            .spawn(move || mixer_thread(mix_shared, incoming_rx, spk_rx))
            .expect("Mischer-Thread");
        let dev_shared = shared.clone();
        let encoder_handle = encoder.thread().clone();
        thread::Builder::new()
            .name("tidespeak-devices".into())
            .spawn(move || device_thread(dev_shared, dev_rx, mic_tx, spk_tx, encoder_handle))
            .expect("Geräte-Thread");

        Audio { shared, incoming: incoming_tx, devices: dev_tx }
    }

    /// Voice datagram from the server: `[speaker u32][seq u16][flags u8][opus]`
    pub fn push_datagram(&self, d: &[u8]) {
        if d.len() < VOICE_DOWN_HEADER {
            return;
        }
        let speaker = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
        let seq = u16::from_le_bytes([d[4], d[5]]);
        let flags = d[6];
        let _ = self.incoming.send(Incoming::Voice { speaker, seq, flags, data: d[VOICE_DOWN_HEADER..].to_vec() });
    }

    pub fn reset_playback(&self) {
        let _ = self.incoming.send(Incoming::Reset);
        self.shared.speaking.lock().unwrap().clear();
    }

    pub fn restart_devices(&self) {
        let _ = self.devices.send(DeviceCmd::Restart);
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut AudioSettings)) {
        f(&mut self.shared.settings.lock().unwrap());
    }

    /// Short, quiet notification sound (synthesized, no sound files needed)
    pub fn cue(&self, cue: Cue) {
        let tones: &[(f32, f32)] = match cue {
            Cue::Join => &[(523.25, 0.07), (783.99, 0.11)],
            Cue::Leave => &[(659.25, 0.07), (392.0, 0.11)],
            Cue::Connected => &[(392.0, 0.06), (523.25, 0.06), (783.99, 0.12)],
            Cue::Mute => &[(311.13, 0.08)],
            Cue::Unmute => &[(466.16, 0.08)],
        };
        let mut samples = Vec::new();
        for &(freq, secs) in tones {
            let n = (secs * SAMPLE_RATE as f32) as usize;
            for i in 0..n {
                let t = i as f32 / SAMPLE_RATE as f32;
                // weicher Ein-/Ausklang, damit es nicht klickt
                let env = (i.min(n - i) as f32 / (0.008 * SAMPLE_RATE as f32)).min(1.0);
                samples.push((t * freq * std::f32::consts::TAU).sin() * 0.12 * env);
            }
        }
        samples.resize(samples.len().div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES, 0.0);
        for chunk in samples.chunks(FRAME_SAMPLES) {
            let _ = self.incoming.send(Incoming::Local(chunk.to_vec()));
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Cue {
    Join,
    Leave,
    Connected,
    Mute,
    Unmute,
}

/// Shows only devices that make sense to a person. With raw ALSA (no
/// PipeWire/PulseAudio) these are the sound cards themselves, not the dozens
/// of ALSA plugins such as "Rate Converter" or "Discard all samples".
fn listed(d: &cpal::Device) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(id) = d.id() {
        if id.host() == cpal::HostId::Alsa {
            let pcm = id.id();
            return pcm == "pipewire" || pcm == "pulse" || pcm.starts_with("sysdefault:");
        }
    }
    let _ = d;
    true
}

/// Names of all inputs and outputs (for the settings page)
pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let names = |it: Result<_, _>| -> Vec<String> {
        match it {
            Ok(devs) => {
                let mut v: Vec<String> =
                    Iterator::filter(devs, listed).map(|d: cpal::Device| d.to_string()).collect();
                v.dedup();
                v
            }
            Err(_) => Vec::new(),
        }
    };
    (names(host.input_devices()), names(host.output_devices()))
}

// ───────────────────────────────────────────────────────────────── Geräte

fn device_thread(
    shared: Arc<Shared>,
    cmds: Receiver<DeviceCmd>,
    mic_tx: Sender<rtrb::Consumer<f32>>,
    spk_tx: Sender<rtrb::Producer<f32>>,
    encoder: thread::Thread,
) {
    loop {
        let settings = shared.settings.lock().unwrap().clone();
        let host = cpal::default_host();

        let input = open_input(&host, settings.input_device.as_deref(), &mic_tx, encoder.clone());
        *shared.input_error.lock().unwrap() = input.as_ref().err().cloned();
        if let Err(e) = &input {
            log::warn!("Mikrofon: {e}");
        }
        let output = open_output(&host, settings.output_device.as_deref(), &spk_tx);
        *shared.output_error.lock().unwrap() = output.as_ref().err().cloned();
        if let Err(e) = &output {
            log::warn!("Ausgabe: {e}");
        }

        // Streams leben, bis ein Neustart angefordert wird
        match cmds.recv() {
            Ok(DeviceCmd::Restart) => {
                drop(input);
                drop(output);
                while cmds.try_recv().is_ok() {}
            }
            Err(_) => return,
        }
    }
}

fn find_device(host: &cpal::Host, name: Option<&str>, input: bool) -> Option<cpal::Device> {
    if let Some(name) = name {
        let found = if input { host.input_devices().ok() } else { host.output_devices().ok() }
            .and_then(|mut devs| devs.find(|d| d.to_string() == name));
        if found.is_some() {
            return found;
        }
    }
    if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }
}

/// Prefers 48 kHz (then nothing needs resampling) and float samples.
fn pick_config(
    ranges: Vec<cpal::SupportedStreamConfigRange>,
    default: Option<cpal::SupportedStreamConfig>,
) -> Option<cpal::SupportedStreamConfig> {
    let score = |r: &cpal::SupportedStreamConfigRange| {
        let fmt = match r.sample_format() {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
            SampleFormat::I32 => 2,
            SampleFormat::U16 => 3,
            _ => 9,
        };
        (fmt, r.channels())
    };
    let mut at48: Vec<_> = ranges
        .iter()
        .filter(|r| r.min_sample_rate() <= SAMPLE_RATE && r.max_sample_rate() >= SAMPLE_RATE && score(r).0 < 9)
        .cloned()
        .collect();
    at48.sort_by_key(score);
    at48.into_iter().next().map(|r| r.with_sample_rate(SAMPLE_RATE)).or(default)
}

struct InputStream(#[allow(dead_code)] cpal::Stream);
struct OutputStream(#[allow(dead_code)] cpal::Stream);

fn open_input(
    host: &cpal::Host,
    name: Option<&str>,
    mic_tx: &Sender<rtrb::Consumer<f32>>,
    encoder: thread::Thread,
) -> Result<InputStream, String> {
    let dev = find_device(host, name, true).ok_or("kein Mikrofon gefunden")?;
    let ranges = dev.supported_input_configs().map(|r| r.collect()).unwrap_or_default();
    let cfg = pick_config(ranges, dev.default_input_config().ok()).ok_or("Mikrofon ohne passendes Format")?;
    let (prod, cons) = rtrb::RingBuffer::new(MIC_RING);
    let stream_cfg = cfg.config();
    let rate = stream_cfg.sample_rate;
    let channels = stream_cfg.channels as usize;
    let err = |e| log::warn!("Mikrofon-Stream: {e}");
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&dev, stream_cfg, channels, rate, prod, encoder, err),
        SampleFormat::I16 => build_input::<i16>(&dev, stream_cfg, channels, rate, prod, encoder, err),
        SampleFormat::I32 => build_input::<i32>(&dev, stream_cfg, channels, rate, prod, encoder, err),
        SampleFormat::U16 => build_input::<u16>(&dev, stream_cfg, channels, rate, prod, encoder, err),
        f => return Err(format!("Sampleformat {f} wird nicht unterstützt")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    let _ = mic_tx.send(cons);
    log::info!("Mikrofon: {dev} ({rate} Hz, {channels} Kanäle)");
    Ok(InputStream(stream))
}

fn build_input<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    channels: usize,
    rate: u32,
    mut prod: rtrb::Producer<f32>,
    encoder: thread::Thread,
    err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let mut rs = Resampler::new(rate, SAMPLE_RATE);
    dev.build_input_stream::<T, _, _>(
        cfg,
        move |data: &[T], _| {
            for frame in data.chunks(channels.max(1)) {
                let mono = frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / frame.len() as f32;
                rs.process(mono, |s| {
                    let _ = prod.push(s);
                });
            }
            encoder.unpark();
        },
        err,
        None,
    )
}

fn open_output(
    host: &cpal::Host,
    name: Option<&str>,
    spk_tx: &Sender<rtrb::Producer<f32>>,
) -> Result<OutputStream, String> {
    let dev = find_device(host, name, false).ok_or("keine Audioausgabe gefunden")?;
    let ranges = dev.supported_output_configs().map(|r| r.collect()).unwrap_or_default();
    let cfg = pick_config(ranges, dev.default_output_config().ok()).ok_or("Ausgabe ohne passendes Format")?;
    let (prod, cons) = rtrb::RingBuffer::new(OUT_RING);
    let stream_cfg = cfg.config();
    let rate = stream_cfg.sample_rate;
    let channels = stream_cfg.channels as usize;
    let err = |e| log::warn!("Ausgabe-Stream: {e}");
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&dev, stream_cfg, channels, rate, cons, err),
        SampleFormat::I16 => build_output::<i16>(&dev, stream_cfg, channels, rate, cons, err),
        SampleFormat::I32 => build_output::<i32>(&dev, stream_cfg, channels, rate, cons, err),
        SampleFormat::U16 => build_output::<u16>(&dev, stream_cfg, channels, rate, cons, err),
        f => return Err(format!("Sampleformat {f} wird nicht unterstützt")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    let _ = spk_tx.send(prod);
    log::info!("Ausgabe: {dev} ({rate} Hz, {channels} Kanäle)");
    Ok(OutputStream(stream))
}

fn build_output<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    channels: usize,
    rate: u32,
    mut cons: rtrb::Consumer<f32>,
    err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    // 48 kHz aus dem Ring -> Gerätetakt (linear interpoliert)
    let step = SAMPLE_RATE as f64 / rate as f64;
    let mut pos = 0.0f64;
    let mut a = 0.0f32;
    let mut b = 0.0f32;
    dev.build_output_stream::<T, _, _>(
        cfg,
        move |data: &mut [T], _| {
            for frame in data.chunks_mut(channels.max(1)) {
                while pos >= 1.0 {
                    a = b;
                    b = cons.pop().unwrap_or(0.0);
                    pos -= 1.0;
                }
                let s = a + (b - a) * pos as f32;
                pos += step;
                let v = T::from_sample(s);
                for out in frame.iter_mut() {
                    *out = v;
                }
            }
        },
        err,
        None,
    )
}

/// Simple linear resampler – plenty for speech, and costs nothing when the rates match.
struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
    passthrough: bool,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Resampler { step: from as f64 / to as f64, pos: 0.0, last: 0.0, passthrough: from == to }
    }

    fn process(&mut self, s: f32, mut out: impl FnMut(f32)) {
        if self.passthrough {
            out(s);
            return;
        }
        while self.pos < 1.0 {
            out(self.last + (s - self.last) * self.pos as f32);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.last = s;
    }
}

// ───────────────────────────────────────────────────────────── Senden

#[cfg(windows)]
fn ptt_global(vk: u32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // SAFETY: einfacher Win32-Aufruf ohne Zeiger
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

#[cfg(not(windows))]
fn ptt_global(_vk: u32) -> bool {
    false
}

fn encoder_thread(shared: Arc<Shared>, mic_rx: Receiver<rtrb::Consumer<f32>>, local_tx: Sender<Incoming>) {
    let mut consumer: Option<rtrb::Consumer<f32>> = None;
    let mut denoise = nnnoiseless::DenoiseState::new();
    let mut enc = match opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip) {
        Ok(e) => e,
        Err(e) => {
            log::error!("Opus-Encoder: {e}");
            return;
        }
    };
    let _ = enc.set_inband_fec(true);
    let _ = enc.set_packet_loss_perc(8);
    let _ = enc.set_signal(opus::Signal::Voice);
    let _ = enc.set_complexity(9);
    let mut bitrate = 0;

    let mut inbuf = [0f32; DENOISE_FRAME];
    let mut clean = [0f32; DENOISE_FRAME];
    let mut frame: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES);
    let mut prev_frame: Vec<f32> = vec![0.0; FRAME_SAMPLES];
    let mut packet = vec![0u8; 1275];
    let mut seq: u16 = 0;
    let mut sending = false;
    let mut hangover = 0u32;
    let mut level_smooth = 0f32;
    let mut vad_max = 0f32;

    loop {
        while let Ok(c) = mic_rx.try_recv() {
            consumer = Some(c);
        }
        let Some(cons) = consumer.as_mut() else {
            thread::park_timeout(Duration::from_millis(100));
            continue;
        };
        if cons.is_abandoned() && cons.slots() < DENOISE_FRAME {
            consumer = None;
            shared.level.store(0f32.to_bits(), Ordering::Relaxed);
            continue;
        }
        if cons.slots() < DENOISE_FRAME {
            thread::park_timeout(Duration::from_millis(30));
            continue;
        }

        let s = shared.settings.lock().unwrap().clone();
        if s.bitrate != bitrate {
            bitrate = s.bitrate;
            let _ = enc.set_bitrate(opus::Bitrate::Bits(bitrate));
        }

        // 10 ms holen, verstärken, entrauschen
        for v in inbuf.iter_mut() {
            *v = cons.pop().unwrap_or(0.0) * s.input_gain;
        }
        let (block, vad) = if s.noise_suppression {
            let scaled: [f32; DENOISE_FRAME] = std::array::from_fn(|i| inbuf[i] * 32768.0);
            let vad = denoise.process_frame(&mut clean, &scaled);
            for v in clean.iter_mut() {
                *v /= 32768.0;
            }
            (&clean[..], vad)
        } else {
            (&inbuf[..], 1.0)
        };
        vad_max = vad_max.max(vad);

        // Pegel für Anzeige und Sprachaktivierung
        let rms = (block.iter().map(|x| x * x).sum::<f32>() / block.len() as f32).sqrt();
        let db = 20.0 * rms.max(1e-6).log10();
        let level = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        level_smooth = if level > level_smooth { level } else { level_smooth * 0.85 + level * 0.15 };
        shared.level.store(level_smooth.to_bits(), Ordering::Relaxed);

        frame.extend_from_slice(block);
        if frame.len() < FRAME_SAMPLES {
            continue;
        }

        // Ein 20-ms-Rahmen ist voll: senden oder nicht?
        if shared.mic_test.load(Ordering::Relaxed) {
            let _ = local_tx.send(Incoming::Local(frame.clone()));
        }
        let muted = shared.muted.load(Ordering::Relaxed) || shared.deafened.load(Ordering::Relaxed);
        let wants = match s.voice_mode {
            VoiceMode::Continuous => true,
            VoiceMode::PushToTalk => shared.ptt.load(Ordering::Relaxed) || ptt_global(s.ptt_key),
            VoiceMode::Activity => {
                let loud = level_smooth >= s.activation_threshold && vad_max > 0.25;
                if loud {
                    hangover = VAD_HANGOVER_FRAMES;
                    true
                } else if hangover > 0 {
                    hangover -= 1;
                    true
                } else {
                    false
                }
            }
        };
        vad_max = 0.0;

        let out = shared.voice_out.lock().unwrap().clone();
        let tx_now = wants && !muted && out.is_some();
        if let Some(out) = out {
            if tx_now {
                // beim Einsetzen 20 ms Vorlauf mitschicken, damit der erste Laut nicht fehlt
                if !sending && s.voice_mode == VoiceMode::Activity {
                    if let Ok(n) = enc.encode_float(&prev_frame, &mut packet) {
                        seq = seq.wrapping_add(1);
                        let _ = out.send(voice_packet(seq, 0, &packet[..n]));
                    }
                }
                if let Ok(n) = enc.encode_float(&frame, &mut packet) {
                    seq = seq.wrapping_add(1);
                    let _ = out.send(voice_packet(seq, 0, &packet[..n]));
                }
            } else if sending {
                seq = seq.wrapping_add(1);
                let _ = out.send(voice_packet(seq, VOICE_FLAG_END, &[]));
                let _ = enc.reset_state();
            }
        }
        sending = tx_now;
        shared.transmitting.store(tx_now, Ordering::Relaxed);
        std::mem::swap(&mut prev_frame, &mut frame);
        frame.clear();
    }
}

fn voice_packet(seq: u16, flags: u8, opus: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + opus.len());
    v.extend_from_slice(&seq.to_le_bytes());
    v.push(flags);
    v.extend_from_slice(opus);
    v
}

// ──────────────────────────────────────────────────────────── Empfangen

struct Packet {
    seq: u16,
    flags: u8,
    data: Vec<u8>,
}

/// Jitter buffer and decoder for one speaker
struct Speaker {
    dec: opus::Decoder,
    slots: [Option<Packet>; JITTER_SLOTS],
    next: u16,
    synced: bool,
    playing: bool,
    concealed: u32,
    last_packet: Instant,
}

impl Speaker {
    fn new() -> Option<Speaker> {
        Some(Speaker {
            dec: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).ok()?,
            slots: Default::default(),
            next: 0,
            synced: false,
            playing: false,
            concealed: 0,
            last_packet: Instant::now(),
        })
    }

    fn buffered(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    fn reset(&mut self) {
        self.slots = Default::default();
        self.synced = false;
        self.playing = false;
        self.concealed = 0;
        let _ = self.dec.reset_state();
    }

    fn push(&mut self, seq: u16, flags: u8, data: Vec<u8>) {
        self.last_packet = Instant::now();
        if !self.synced {
            self.next = seq;
            self.synced = true;
        }
        let ahead = seq.wrapping_sub(self.next) as i16;
        if ahead < 0 {
            return; // zu spät, schon verdeckt
        }
        if ahead as usize >= JITTER_SLOTS {
            self.reset();
            self.next = seq;
            self.synced = true;
        }
        self.slots[seq as usize % JITTER_SLOTS] = Some(Packet { seq, flags, data });
    }

    fn take(&mut self, seq: u16) -> Option<Packet> {
        let slot = &mut self.slots[seq as usize % JITTER_SLOTS];
        if slot.as_ref().is_some_and(|p| p.seq == seq) {
            slot.take()
        } else {
            None
        }
    }

    fn peek(&self, seq: u16) -> Option<&Packet> {
        self.slots[seq as usize % JITTER_SLOTS].as_ref().filter(|p| p.seq == seq)
    }

    /// Decodes the next 20 ms into `out`. `false` = this speaker is silent.
    fn next_frame(&mut self, out: &mut [f32]) -> bool {
        if !self.synced {
            return false;
        }
        if !self.playing {
            // erst mit 40 ms Vorrat starten – oder sofort, wenn der Sprecher schon fertig ist
            let has_end = self.slots.iter().flatten().any(|p| p.flags & VOICE_FLAG_END != 0);
            if self.buffered() < 2 && !has_end {
                return false;
            }
            self.playing = true;
        }
        // Verzögerung abbauen, wenn sich zu viel angesammelt hat
        if self.buffered() > 6 {
            self.take(self.next);
            self.next = self.next.wrapping_add(1);
        }

        let seq = self.next;
        if let Some(p) = self.take(seq) {
            self.next = seq.wrapping_add(1);
            self.concealed = 0;
            if p.flags & VOICE_FLAG_END != 0 && p.data.is_empty() {
                self.reset();
                return false;
            }
            return matches!(self.dec.decode_float(&p.data, out, false), Ok(n) if n == out.len());
        }

        // Paket fehlt: aus dem nächsten rekonstruieren (FEC) oder verdecken (PLC)
        self.concealed += 1;
        if self.concealed > 4 && self.buffered() == 0 {
            self.reset();
            return false;
        }
        self.next = seq.wrapping_add(1);
        let fec = self.peek(seq.wrapping_add(1)).map(|p| p.data.clone()).filter(|d| !d.is_empty());
        let res = match fec {
            Some(d) => self.dec.decode_float(&d, out, true),
            None => self.dec.decode_float(&[], out, false),
        };
        matches!(res, Ok(n) if n == out.len())
    }
}

fn mixer_thread(shared: Arc<Shared>, incoming: Receiver<Incoming>, spk_rx: Receiver<rtrb::Producer<f32>>) {
    let mut producer: Option<rtrb::Producer<f32>> = None;
    let mut speakers: HashMap<u32, Speaker> = HashMap::new();
    let mut local: VecDeque<Vec<f32>> = VecDeque::new();
    let mut mix = vec![0f32; FRAME_SAMPLES];
    let mut tmp = vec![0f32; FRAME_SAMPLES];
    let mut last_cleanup = Instant::now();

    loop {
        while let Ok(p) = spk_rx.try_recv() {
            producer = Some(p);
        }
        loop {
            match incoming.try_recv() {
                Ok(Incoming::Voice { speaker, seq, flags, data }) => {
                    if flags & VOICE_FLAG_END == 0 {
                        shared.speaking.lock().unwrap().insert(speaker, Instant::now());
                    }
                    if let Some(sp) = match speakers.entry(speaker) {
                        std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
                        std::collections::hash_map::Entry::Vacant(e) => Speaker::new().map(|s| e.insert(s)),
                    } {
                        sp.push(seq, flags, data);
                    }
                }
                Ok(Incoming::Local(f)) => {
                    local.push_back(f);
                    while local.len() > 25 {
                        local.pop_front();
                    }
                }
                Ok(Incoming::Reset) => {
                    speakers.clear();
                    local.clear();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        if let Some(p) = producer.as_mut() {
            if p.is_abandoned() {
                producer = None;
                continue;
            }
            let volume = shared.settings.lock().unwrap().output_volume;
            let deaf = shared.deafened.load(Ordering::Relaxed);
            // so lange mischen, bis ~60 ms im Ausgabepuffer liegen
            while p.slots() >= FRAME_SAMPLES && OUT_RING - p.slots() < OUT_TARGET {
                mix.iter_mut().for_each(|v| *v = 0.0);
                for sp in speakers.values_mut() {
                    if sp.next_frame(&mut tmp) && !deaf {
                        mix.iter_mut().zip(&tmp).for_each(|(m, t)| *m += t);
                    }
                }
                if let Some(f) = local.pop_front() {
                    mix.iter_mut().zip(&f).for_each(|(m, t)| *m += t);
                }
                for v in mix.iter() {
                    // weiche Begrenzung statt hartem Clipping, wenn mehrere gleichzeitig reden
                    let x = v * volume;
                    let y = if x.abs() <= 0.8 { x } else { x.signum() * (0.8 + 0.2 * ((x.abs() - 0.8) / 0.2).tanh()) };
                    let _ = p.push(y);
                }
            }
        } else {
            // ohne Ausgabegerät trotzdem Puffer leeren, damit nichts aufläuft
            for sp in speakers.values_mut() {
                while sp.buffered() > 3 {
                    sp.next_frame(&mut tmp);
                }
            }
        }

        if last_cleanup.elapsed() > Duration::from_secs(5) {
            last_cleanup = Instant::now();
            speakers.retain(|_, s| s.last_packet.elapsed() < SPEAKER_IDLE);
            shared.speaking.lock().unwrap().retain(|_, t| t.elapsed() < SPEAKER_IDLE);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_packets(enc: &mut opus::Encoder, n: usize) -> Vec<Vec<u8>> {
        let mut t = 0usize;
        (0..n)
            .map(|_| {
                let f: Vec<f32> = (0..FRAME_SAMPLES)
                    .map(|_| {
                        t += 1;
                        (t as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.3
                    })
                    .collect();
                enc.encode_vec_float(&f, 1275).unwrap()
            })
            .collect()
    }

    #[test]
    fn jitter_buffer_handles_loss_and_reorder() {
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        enc.set_inband_fec(true).unwrap();
        enc.set_packet_loss_perc(10).unwrap();
        let pk = sine_packets(&mut enc, 20);
        let mut sp = Speaker::new().unwrap();
        let mut out = vec![0f32; FRAME_SAMPLES];
        // Ankunft wie im Netz: leicht vertauscht, Paket 5 geht verloren
        let mut played = 0;
        let mut step = |sp: &mut Speaker, out: &mut Vec<f32>| {
            if sp.next_frame(out) {
                played += 1;
            }
        };
        sp.push(0, 0, pk[0].clone());
        sp.push(2, 0, pk[2].clone());
        step(&mut sp, &mut out);
        for i in [1u16, 3, 4, 6, 7, 8, 9] {
            sp.push(i, 0, pk[i as usize].clone());
            step(&mut sp, &mut out);
        }
        step(&mut sp, &mut out);
        step(&mut sp, &mut out);
        assert_eq!(played, 10, "alle 10 Rahmen inkl. verdecktem Paket 5");
        assert!(out.iter().any(|v| v.abs() > 0.01));
        // Ende-Markierung beendet die Wiedergabe sauber
        sp.push(10, VOICE_FLAG_END, Vec::new());
        assert!(!sp.next_frame(&mut out));
        assert!(!sp.playing);
    }

    #[test]
    fn resampler_ratio() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut n = 0;
        for _ in 0..44_100 {
            r.process(0.0, |_| n += 1);
        }
        assert!((47_990..=48_010).contains(&n), "{n}");
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Rechenkosten pro Sekunde Sprache: `cargo test --release -p tidespeak pipeline_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn pipeline_cost() {
        let mut denoise = nnnoiseless::DenoiseState::new();
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        enc.set_bitrate(opus::Bitrate::Bits(48_000)).unwrap();
        enc.set_inband_fec(true).unwrap();
        enc.set_complexity(9).unwrap();
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
        let secs = 20;
        let mut t = 0usize;
        let mut pcm = vec![0f32; FRAME_SAMPLES];
        let mut out = vec![0f32; FRAME_SAMPLES];
        let mut clean = [0f32; DENOISE_FRAME];
        let mut pkt = vec![0u8; 1275];
        let (mut t_ns, mut t_enc, mut t_dec) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        for _ in 0..secs * 50 {
            for v in pcm.iter_mut() {
                t += 1;
                // Sprachähnliches Signal: Grundton mit Obertönen plus Rauschen
                let x = t as f32 / SAMPLE_RATE as f32;
                *v = 0.2 * (x * 180.0 * std::f32::consts::TAU).sin()
                    + 0.1 * (x * 720.0 * std::f32::consts::TAU).sin()
                    + 0.02 * ((t * 7919 % 1000) as f32 / 500.0 - 1.0);
            }
            let a = Instant::now();
            for half in pcm.chunks(DENOISE_FRAME) {
                let scaled: [f32; DENOISE_FRAME] = std::array::from_fn(|i| half[i] * 32768.0);
                denoise.process_frame(&mut clean, &scaled);
            }
            let b = Instant::now();
            let n = enc.encode_float(&pcm, &mut pkt).unwrap();
            let c = Instant::now();
            dec.decode_float(&pkt[..n], &mut out, false).unwrap();
            let d = Instant::now();
            t_ns += b - a;
            t_enc += c - b;
            t_dec += d - c;
        }
        let pct = |d: Duration| d.as_secs_f64() / secs as f64 * 100.0;
        println!(
            "pro Sekunde Sprache: RNNoise {:.2} %, Opus-Encoder {:.2} %, Opus-Decoder {:.2} % eines Kerns",
            pct(t_ns),
            pct(t_enc),
            pct(t_dec)
        );
    }
}
