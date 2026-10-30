//! Audio pipeline.
//!
//!   Mic ─► Ring ─► [encoder thread] channel pick ─► resample ─► high-pass ─► AGC ─► RNNoise
//!                   ─► voice activation ─► Opus (FEC, pre-roll) ─► network
//!   network ─► [mixer thread] adaptive jitter buffer per speaker ─► Opus (FEC/PLC)
//!                   ─► volume per person ─► mix ─► resample ─► Ring ─► speakers
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
use crate::dsp::{self, Agc, Biquad, ChannelPicker, SincResampler, VoiceGate};
use crate::jitter::{JitterBuffer, Stats};

const DENOISE_FRAME: usize = nnnoiseless::DenoiseState::FRAME_SIZE; // 480 = 10 ms
/// Fill level of the output buffer the mixer aims for (latency vs. dropouts)
const OUT_TARGET_MS: u32 = 60;
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
    pub agc: bool,
    pub echo_cancel: bool,
    /// Adjust the voice activation threshold automatically to the background noise
    pub auto_threshold: bool,
    pub bitrate: i32,
    pub ptt_key: u32,
}

/// Receive quality from one speaker (for display and feedback)
#[derive(Clone, Copy, Debug, Default)]
pub struct RxInfo {
    pub loss: f32,
    pub jitter_ms: f32,
    pub buffer_ms: f32,
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
    /// Mic level for the display 0.0 … 1.0 (-60 … 0 dBFS), as f32 bits
    level: AtomicU32,
    pub speaking: Mutex<HashMap<u32, Instant>>,
    /// Destination for finished voice packets (set by the network while connected)
    pub voice_out: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    pub input_error: Mutex<Option<String>>,
    pub output_error: Mutex<Option<String>>,
    /// e.g. "Scarlett 2i2 USB · 48 kHz · Eingang 1 von 2"
    pub input_info: Mutex<String>,
    /// Volume per person (user id -> factor)
    pub user_gain: Mutex<HashMap<u32, f32>>,
    /// Receive quality per speaker
    pub rx: Mutex<HashMap<u32, RxInfo>>,
    /// Loss that others report for our voice (percent) -> strength of error correction
    pub uplink_loss: AtomicU32,
    /// Current loudness per speaker (0…1, display scale) – for the speaking ring
    pub rx_level: Mutex<HashMap<u32, f32>>,
    /// Voice activation threshold actually in effect (0…1, display scale), as f32 bits
    threshold_now: AtomicU32,
}

impl Shared {
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn threshold_now(&self) -> f32 {
        f32::from_bits(self.threshold_now.load(Ordering::Relaxed))
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

/// Microphone ring together with its format
struct MicSource {
    cons: rtrb::Consumer<f32>,
    channels: usize,
    rate: u32,
    name: String,
}

/// Speaker ring together with its sample rate
struct SpeakerSink {
    prod: rtrb::Producer<f32>,
    rate: u32,
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
            input_info: Mutex::new(String::new()),
            user_gain: Mutex::new(HashMap::new()),
            rx: Mutex::new(HashMap::new()),
            uplink_loss: AtomicU32::new(0),
            rx_level: Mutex::new(HashMap::new()),
            threshold_now: AtomicU32::new(0f32.to_bits()),
        });
        // Was aus den Lautsprechern kommt, geht als Referenz an die Echo-Unterdrückung
        let (far_tx, far_rx) = rtrb::RingBuffer::<f32>::new(SAMPLE_RATE as usize);
        let (incoming_tx, incoming_rx) = mpsc::channel();
        let (dev_tx, dev_rx) = mpsc::channel();
        let (mic_tx, mic_rx) = mpsc::channel::<MicSource>();
        let (spk_tx, spk_rx) = mpsc::channel::<SpeakerSink>();

        let enc_shared = shared.clone();
        let local_tx = incoming_tx.clone();
        let encoder = thread::Builder::new()
            .name("tidespeak-encoder".into())
            .spawn(move || encoder_thread(enc_shared, mic_rx, local_tx, far_rx))
            .expect("Encoder-Thread");
        let mix_shared = shared.clone();
        thread::Builder::new()
            .name("tidespeak-mixer".into())
            .spawn(move || mixer_thread(mix_shared, incoming_rx, spk_rx, far_tx))
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
        self.shared.rx.lock().unwrap().clear();
        self.shared.uplink_loss.store(0, Ordering::Relaxed);
        self.shared.rx_level.lock().unwrap().clear();
    }

    pub fn restart_devices(&self) {
        let _ = self.devices.send(DeviceCmd::Restart);
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut AudioSettings)) {
        f(&mut self.shared.settings.lock().unwrap());
    }

    pub fn set_user_gain(&self, id: u32, gain: f32) {
        self.shared.user_gain.lock().unwrap().insert(id, gain);
    }

    /// Short, quiet notification sound (synthesized, no sound files needed)
    pub fn cue(&self, cue: Cue) {
        let tones: &[(f32, f32)] = match cue {
            Cue::Join => &[(523.25, 0.07), (783.99, 0.11)],
            Cue::Leave => &[(659.25, 0.07), (392.0, 0.11)],
            Cue::Connected => &[(392.0, 0.06), (523.25, 0.06), (783.99, 0.12)],
            Cue::Mute => &[(311.13, 0.08)],
            Cue::Unmute => &[(466.16, 0.08)],
            Cue::Poke => &[(783.99, 0.07), (1046.5, 0.07), (1318.5, 0.12)],
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
    /// Poked or mentioned
    Poke,
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
    mic_tx: Sender<MicSource>,
    spk_tx: Sender<SpeakerSink>,
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

/// Prefers 48 kHz (no resampling needed), float samples and the device's
/// *native* channel count. Requesting mono makes PipeWire/PulseAudio mix
/// the inputs down itself, which halves the level on audio interfaces with
/// the mic on input 1. Tidespeak picks the active channel itself instead.
fn pick_config(
    ranges: Vec<cpal::SupportedStreamConfigRange>,
    default: Option<cpal::SupportedStreamConfig>,
) -> Option<cpal::SupportedStreamConfig> {
    let native = default.as_ref().map(|d| d.channels()).unwrap_or(2);
    let score = |r: &cpal::SupportedStreamConfigRange| {
        let fmt = match r.sample_format() {
            SampleFormat::F32 => 0,
            SampleFormat::I32 => 1,
            SampleFormat::I16 => 2,
            SampleFormat::U16 => 3,
            _ => 9,
        };
        (r.channels() != native, fmt, r.channels())
    };
    let mut at48: Vec<_> = ranges
        .iter()
        .filter(|r| r.min_sample_rate() <= SAMPLE_RATE && r.max_sample_rate() >= SAMPLE_RATE && score(r).1 < 9)
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
    mic_tx: &Sender<MicSource>,
    encoder: thread::Thread,
) -> Result<InputStream, String> {
    let dev = find_device(host, name, true).ok_or("kein Mikrofon gefunden")?;
    let ranges = dev.supported_input_configs().map(|r| r.collect()).unwrap_or_default();
    let cfg = pick_config(ranges, dev.default_input_config().ok()).ok_or("Mikrofon ohne passendes Format")?;
    let stream_cfg = cfg.config();
    let rate = stream_cfg.sample_rate;
    let channels = (stream_cfg.channels as usize).max(1);
    let (prod, cons) = rtrb::RingBuffer::new(rate as usize * channels);
    let err = |e| log::warn!("Mikrofon-Stream: {e}");
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&dev, stream_cfg, prod, encoder, err),
        SampleFormat::I16 => build_input::<i16>(&dev, stream_cfg, prod, encoder, err),
        SampleFormat::I32 => build_input::<i32>(&dev, stream_cfg, prod, encoder, err),
        SampleFormat::U16 => build_input::<u16>(&dev, stream_cfg, prod, encoder, err),
        f => return Err(format!("Sampleformat {f} wird nicht unterstützt")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    let _ = mic_tx.send(MicSource { cons, channels, rate, name: dev.to_string() });
    log::info!("Mikrofon: {dev} ({rate} Hz, {channels} Kanäle)");
    Ok(InputStream(stream))
}

fn build_input<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    mut prod: rtrb::Producer<f32>,
    encoder: thread::Thread,
    err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    dev.build_input_stream::<T, _, _>(
        cfg,
        move |data: &[T], _| {
            // nur kopieren – alles Weitere im Encoder-Thread
            for s in data {
                let _ = prod.push(f32::from_sample(*s));
            }
            encoder.unpark();
        },
        err,
        None,
    )
}

fn open_output(host: &cpal::Host, name: Option<&str>, spk_tx: &Sender<SpeakerSink>) -> Result<OutputStream, String> {
    let dev = find_device(host, name, false).ok_or("keine Audioausgabe gefunden")?;
    let ranges = dev.supported_output_configs().map(|r| r.collect()).unwrap_or_default();
    let cfg = pick_config(ranges, dev.default_output_config().ok()).ok_or("Ausgabe ohne passendes Format")?;
    let stream_cfg = cfg.config();
    let rate = stream_cfg.sample_rate;
    let channels = (stream_cfg.channels as usize).max(1);
    let (prod, cons) = rtrb::RingBuffer::new(rate as usize / 2);
    let err = |e| log::warn!("Ausgabe-Stream: {e}");
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&dev, stream_cfg, channels, cons, err),
        SampleFormat::I16 => build_output::<i16>(&dev, stream_cfg, channels, cons, err),
        SampleFormat::I32 => build_output::<i32>(&dev, stream_cfg, channels, cons, err),
        SampleFormat::U16 => build_output::<u16>(&dev, stream_cfg, channels, cons, err),
        f => return Err(format!("Sampleformat {f} wird nicht unterstützt")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    let _ = spk_tx.send(SpeakerSink { prod, rate });
    log::info!("Ausgabe: {dev} ({rate} Hz, {channels} Kanäle)");
    Ok(OutputStream(stream))
}

fn build_output<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    channels: usize,
    mut cons: rtrb::Consumer<f32>,
    err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    // Der Ring enthält schon Gerätetakt-Samples (Mischer resampelt) – hier nur verteilen
    dev.build_output_stream::<T, _, _>(
        cfg,
        move |data: &mut [T], _| {
            for frame in data.chunks_mut(channels) {
                let v = T::from_sample(cons.pop().unwrap_or(0.0));
                for out in frame.iter_mut() {
                    *out = v;
                }
            }
        },
        err,
        None,
    )
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

/// Microphone processing up to the finished 20 ms frame, without devices
/// (also used directly by the tests).
pub struct Capture {
    picker: ChannelPicker,
    resampler: SincResampler,
    highpass: Biquad,
    agc: Agc,
    denoise: Box<nnnoiseless::DenoiseState<'static>>,
    aec: Option<sonora::AudioProcessing>,
    mono: Vec<f32>,
    pending: Vec<f32>,
    frame: Vec<f32>,
    frame_vad: f32,
    last_vad: f32,
}

impl Capture {
    pub fn new(channels: usize, rate: u32) -> Self {
        Capture {
            picker: ChannelPicker::new(channels),
            resampler: SincResampler::new(rate, SAMPLE_RATE),
            highpass: Biquad::highpass(80.0, SAMPLE_RATE as f32),
            agc: Agc::default(),
            denoise: nnnoiseless::DenoiseState::new(),
            aec: None,
            mono: Vec::new(),
            pending: Vec::new(),
            frame: Vec::with_capacity(FRAME_SAMPLES),
            frame_vad: 0.0,
            last_vad: 0.0,
        }
    }

    /// (channel in use, number of channels), 1-based for display
    pub fn channel(&self) -> (usize, usize) {
        (self.picker.current() + 1, self.picker.channels())
    }

    pub fn agc_gain_db(&self) -> f32 {
        self.agc.gain_db()
    }

    fn aec(&mut self) -> &mut sonora::AudioProcessing {
        self.aec.get_or_insert_with(|| {
            let cfg = sonora::StreamConfig::new(SAMPLE_RATE, 1);
            let mut apm = sonora::AudioProcessing::builder()
                .config(sonora::Config {
                    echo_canceller: Some(sonora::config::EchoCanceller::default()),
                    ..Default::default()
                })
                .capture_config(cfg)
                .render_config(cfg)
                .build();
            // Ausgabepuffer + typische Gerätelatenz; AEC3 schätzt den Rest selbst
            let _ = apm.set_stream_delay_ms(OUT_TARGET_MS as i32 + 30);
            apm
        })
    }

    /// Playback signal (48 kHz mono) in 10 ms blocks – reference for echo cancellation
    pub fn render(&mut self, block: &[f32; DENOISE_FRAME], s: &AudioSettings) {
        if !s.echo_cancel {
            return;
        }
        let mut out = [0f32; DENOISE_FRAME];
        let _ = self.aec().process_render_f32(&[&block[..]], &mut [&mut out[..]]);
    }

    /// Interleaved device samples in; for every complete 20 ms frame
    /// `on_frame(pcm, speech probability, level dBFS)` is called.
    pub fn feed(&mut self, interleaved: &[f32], s: &AudioSettings, mut on_frame: impl FnMut(&[f32], f32, f32)) {
        let mut mono = std::mem::take(&mut self.mono);
        mono.clear();
        self.picker.process(interleaved, &mut mono);
        self.resampler.process(&mono, &mut self.pending);
        self.mono = mono;

        let mut start = 0;
        while self.pending.len() - start >= DENOISE_FRAME {
            let mut block = [0f32; DENOISE_FRAME];
            block.copy_from_slice(&self.pending[start..start + DENOISE_FRAME]);
            start += DENOISE_FRAME;
            self.process_block(&mut block, s);
            self.frame.extend_from_slice(&block);
            if self.frame.len() >= FRAME_SAMPLES {
                on_frame(&self.frame, self.frame_vad, dsp::rms_db(&self.frame));
                self.frame.clear();
                self.frame_vad = 0.0;
            }
        }
        self.pending.drain(..start);
    }

    fn process_block(&mut self, block: &mut [f32; DENOISE_FRAME], s: &AudioSettings) {
        self.highpass.process(block);
        if s.echo_cancel {
            let input = *block;
            let _ = self.aec().process_capture_f32(&[&input[..]], &mut [&mut block[..]]);
        }
        if (s.input_gain - 1.0).abs() > 1e-3 {
            block.iter_mut().for_each(|x| *x *= s.input_gain);
        }
        if s.agc {
            self.agc.process(block, self.last_vad);
        }
        dsp::soft_limit(block);
        // RNNoise läuft immer: Seine Sprachwahrscheinlichkeit steuert die Sprachaktivierung
        let scaled: [f32; DENOISE_FRAME] = std::array::from_fn(|i| block[i] * 32768.0);
        let mut clean = [0f32; DENOISE_FRAME];
        let vad = self.denoise.process_frame(&mut clean, &scaled);
        if s.noise_suppression {
            for (b, c) in block.iter_mut().zip(clean.iter()) {
                *b = c / 32768.0;
            }
        }
        self.last_vad = vad;
        self.frame_vad = self.frame_vad.max(vad);
    }
}

/// Turns 20 ms frames into voice packets: Opus runs continuously (clean
/// encoder state), and with voice activation the last 60 ms before the start
/// are sent too, so word beginnings are never cut off.
pub struct Transmitter {
    enc: opus::Encoder,
    gate: VoiceGate,
    preroll: VecDeque<Vec<u8>>,
    seq: u16,
    sending: bool,
    bitrate: i32,
    loss: i32,
    packet: Vec<u8>,
    noise_db: f32,
    ptt_hold: u32,
}

/// Push-to-talk release delay: keep sending this many frames after letting go (200 ms)
const PTT_RELEASE_FRAMES: u32 = 10;

impl Transmitter {
    pub fn new() -> Option<Self> {
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).ok()?;
        let _ = enc.set_inband_fec(true);
        let _ = enc.set_signal(opus::Signal::Voice);
        let _ = enc.set_complexity(10);
        Some(Transmitter {
            enc,
            gate: VoiceGate::default(),
            preroll: VecDeque::new(),
            seq: 0,
            sending: false,
            bitrate: 0,
            loss: -1,
            packet: vec![0u8; 1275],
            noise_db: -60.0,
            ptt_hold: 0,
        })
    }

    /// Voice activation threshold in effect (dBFS): manual, or automatically 12 dB above the noise
    pub fn threshold_db(&self, s: &AudioSettings) -> f32 {
        if s.auto_threshold {
            (self.noise_db + 12.0).clamp(-58.0, -30.0)
        } else {
            dsp::threshold_db(s.activation_threshold)
        }
    }

    fn packet(&mut self, flags: u8, opus: &[u8]) -> Vec<u8> {
        self.seq = self.seq.wrapping_add(1);
        voice_packet(self.seq, flags, opus)
    }

    /// One 20 ms frame. `active` = connected and not muted.
    /// `uplink_loss` = loss reported by others (percent). Returns whether we are sending.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        pcm: &[f32],
        vad: f32,
        level_db: f32,
        s: &AudioSettings,
        ptt: bool,
        active: bool,
        uplink_loss: u32,
        mut send: impl FnMut(Vec<u8>),
    ) -> bool {
        if s.bitrate != self.bitrate {
            self.bitrate = s.bitrate;
            let _ = self.enc.set_bitrate(opus::Bitrate::Bits(s.bitrate));
        }
        // Fehlerkorrektur an die tatsächliche Leitung anpassen (mindestens 5 %)
        let loss = (uplink_loss as i32 + 5).clamp(5, 40);
        if loss != self.loss {
            self.loss = loss;
            let _ = self.enc.set_packet_loss_perc(loss);
        }
        // Grundrauschen verfolgen (nur in Sprechpausen): schnell nach unten, langsam nach oben
        if vad < 0.15 && level_db > -100.0 {
            let rate = if level_db < self.noise_db { 0.2 } else { 0.01 };
            self.noise_db += (level_db - self.noise_db) * rate;
        }
        if ptt {
            self.ptt_hold = PTT_RELEASE_FRAMES;
        } else {
            self.ptt_hold = self.ptt_hold.saturating_sub(1);
        }
        let threshold = self.threshold_db(s);
        let wants = match s.voice_mode {
            VoiceMode::Continuous => true,
            VoiceMode::PushToTalk => ptt || self.ptt_hold > 0,
            VoiceMode::Activity => self.gate.decide(vad, level_db, threshold),
        };
        if !active {
            if self.sending {
                let end = self.packet(VOICE_FLAG_END, &[]);
                send(end);
            }
            self.sending = false;
            self.preroll.clear();
            return false;
        }
        let Ok(n) = self.enc.encode_float(pcm, &mut self.packet) else { return self.sending };
        let opus = self.packet[..n].to_vec();
        if wants {
            if !self.sending && s.voice_mode == VoiceMode::Activity {
                while let Some(p) = self.preroll.pop_front() {
                    let pkt = self.packet(0, &p);
                    send(pkt);
                }
            }
            self.preroll.clear();
            let pkt = self.packet(0, &opus);
            send(pkt);
        } else {
            if self.sending {
                let end = self.packet(VOICE_FLAG_END, &[]);
                send(end);
            }
            self.preroll.push_back(opus);
            while self.preroll.len() > dsp::PRE_ROLL_FRAMES {
                self.preroll.pop_front();
            }
        }
        self.sending = wants;
        wants
    }
}

fn rate_text(rate: u32) -> String {
    if rate % 1000 == 0 {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f32 / 1000.0).replace('.', ",")
    }
}

fn encoder_thread(
    shared: Arc<Shared>,
    mic_rx: Receiver<MicSource>,
    local_tx: Sender<Incoming>,
    mut far_rx: rtrb::Consumer<f32>,
) {
    let Some(mut tx) = Transmitter::new() else {
        log::error!("Opus-Encoder konnte nicht erzeugt werden");
        return;
    };
    let mut src: Option<MicSource> = None;
    let mut cap: Option<Capture> = None;
    let mut buf: Vec<f32> = Vec::new();
    let mut level_smooth = 0f32;
    let mut shown_channel = (0, 0);

    loop {
        while let Ok(m) = mic_rx.try_recv() {
            cap = Some(Capture::new(m.channels, m.rate));
            src = Some(m);
            shown_channel = (0, 0);
        }
        let (Some(m), Some(c)) = (src.as_mut(), cap.as_mut()) else {
            thread::park_timeout(Duration::from_millis(100));
            continue;
        };
        let avail = m.cons.slots();
        if m.cons.is_abandoned() && avail < m.channels {
            src = None;
            cap = None;
            shared.level.store(0f32.to_bits(), Ordering::Relaxed);
            continue;
        }
        if avail < m.channels * (m.rate as usize / 100) {
            thread::park_timeout(Duration::from_millis(30));
            continue;
        }
        buf.clear();
        for _ in 0..avail - avail % m.channels {
            buf.push(m.cons.pop().unwrap_or(0.0));
        }

        let s = shared.settings.lock().unwrap().clone();
        // Lautsprecher-Signal an die Echo-Unterdrückung (oder verwerfen, wenn sie aus ist)
        while far_rx.slots() >= DENOISE_FRAME {
            let mut block = [0f32; DENOISE_FRAME];
            for v in block.iter_mut() {
                *v = far_rx.pop().unwrap_or(0.0);
            }
            c.render(&block, &s);
        }
        let ptt = shared.ptt.load(Ordering::Relaxed) || ptt_global(s.ptt_key);
        let muted = shared.muted.load(Ordering::Relaxed) || shared.deafened.load(Ordering::Relaxed);
        let out = shared.voice_out.lock().unwrap().clone();
        let active = out.is_some() && !muted;
        let uplink = shared.uplink_loss.load(Ordering::Relaxed);
        let mic_test = shared.mic_test.load(Ordering::Relaxed);
        let mut transmitting = shared.transmitting.load(Ordering::Relaxed);
        c.feed(&buf, &s, |pcm, vad, level_db| {
            let lv = dsp::level_to_slider(level_db);
            level_smooth = if lv > level_smooth { lv } else { level_smooth * 0.8 + lv * 0.2 };
            if mic_test {
                let _ = local_tx.send(Incoming::Local(pcm.to_vec()));
            }
            transmitting = tx.frame(pcm, vad, level_db, &s, ptt, active, uplink, |p| {
                if let Some(o) = &out {
                    let _ = o.send(p);
                }
            });
        });
        shared.level.store(level_smooth.to_bits(), Ordering::Relaxed);
        shared.transmitting.store(transmitting, Ordering::Relaxed);
        shared.threshold_now.store(dsp::level_to_slider(tx.threshold_db(&s)).to_bits(), Ordering::Relaxed);

        let ch = c.channel();
        if ch != shown_channel {
            shown_channel = ch;
            let input = if ch.1 > 1 { format!("Eingang {} von {}", ch.0, ch.1) } else { "Mono".into() };
            *shared.input_info.lock().unwrap() = format!("{} · {} · {}", m.name, rate_text(m.rate), input);
        }
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

struct SpeakerState {
    jb: JitterBuffer,
    snapshot: Stats,
    last_packet: Instant,
}

fn mixer_thread(
    shared: Arc<Shared>,
    incoming: Receiver<Incoming>,
    spk_rx: Receiver<SpeakerSink>,
    mut far_tx: rtrb::Producer<f32>,
) {
    let clock = Instant::now();
    let mut sink: Option<(SpeakerSink, SincResampler, usize)> = None;
    let mut speakers: HashMap<u32, SpeakerState> = HashMap::new();
    let mut local: VecDeque<Vec<f32>> = VecDeque::new();
    let mut mix = vec![0f32; FRAME_SAMPLES];
    let mut tmp = vec![0f32; FRAME_SAMPLES];
    let mut resampled: Vec<f32> = Vec::new();
    let mut last_stats = Instant::now();
    let mut last_cleanup = Instant::now();

    loop {
        while let Ok(sk) = spk_rx.try_recv() {
            let capacity = sk.prod.slots();
            let rs = SincResampler::new(SAMPLE_RATE, sk.rate);
            sink = Some((sk, rs, capacity));
        }
        loop {
            match incoming.try_recv() {
                Ok(Incoming::Voice { speaker, seq, flags, data }) => {
                    if flags & VOICE_FLAG_END == 0 {
                        shared.speaking.lock().unwrap().insert(speaker, Instant::now());
                    }
                    let now_ms = clock.elapsed().as_secs_f64() * 1000.0;
                    let state = match speakers.entry(speaker) {
                        std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
                        std::collections::hash_map::Entry::Vacant(e) => JitterBuffer::new()
                            .map(|jb| e.insert(SpeakerState { jb, snapshot: Stats::default(), last_packet: Instant::now() })),
                    };
                    if let Some(st) = state {
                        st.last_packet = Instant::now();
                        st.jb.push(seq, flags, data, now_ms);
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

        if let Some((sk, rs, capacity)) = sink.as_mut() {
            if sk.prod.is_abandoned() {
                sink = None;
                continue;
            }
            let target = (sk.rate * OUT_TARGET_MS / 1000) as usize;
            let per_frame = FRAME_SAMPLES * sk.rate as usize / SAMPLE_RATE as usize + 64;
            let volume = shared.settings.lock().unwrap().output_volume;
            let deaf = shared.deafened.load(Ordering::Relaxed);
            let gains = shared.user_gain.lock().unwrap().clone();
            // so lange mischen, bis ~60 ms im Ausgabepuffer liegen
            while *capacity - sk.prod.slots() < target && sk.prod.slots() >= per_frame {
                mix.iter_mut().for_each(|v| *v = 0.0);
                let mut levels = Vec::with_capacity(speakers.len());
                for (id, st) in speakers.iter_mut() {
                    if st.jb.next_frame(&mut tmp) && !deaf {
                        let g = gains.get(id).copied().unwrap_or(1.0);
                        mix.iter_mut().zip(&tmp).for_each(|(m, t)| *m += t * g);
                        levels.push((*id, dsp::level_to_slider(dsp::rms_db(&tmp))));
                    } else {
                        levels.push((*id, 0.0));
                    }
                }
                {
                    let mut rl = shared.rx_level.lock().unwrap();
                    for (id, l) in levels {
                        rl.insert(id, l);
                    }
                }
                if let Some(f) = local.pop_front() {
                    mix.iter_mut().zip(&f).for_each(|(m, t)| *m += t);
                }
                mix.iter_mut().for_each(|v| *v *= volume);
                // weiche Begrenzung statt hartem Clipping, wenn mehrere gleichzeitig reden
                dsp::soft_limit(&mut mix);
                // dasselbe Signal als Echo-Referenz (ist der Ring voll, ist der Encoder gerade nicht aktiv)
                if far_tx.slots() >= FRAME_SAMPLES {
                    for v in &mix {
                        let _ = far_tx.push(*v);
                    }
                }
                resampled.clear();
                rs.process(&mix, &mut resampled);
                for v in &resampled {
                    let _ = sk.prod.push(*v);
                }
            }
        } else {
            // ohne Ausgabegerät trotzdem Puffer leeren, damit nichts aufläuft (Pegel für die Anzeige bleibt)
            let mut rl = shared.rx_level.lock().unwrap();
            for (id, st) in speakers.iter_mut() {
                while st.jb.buffered() > 3 {
                    if st.jb.next_frame(&mut tmp) {
                        rl.insert(*id, dsp::level_to_slider(dsp::rms_db(&tmp)));
                    }
                }
            }
        }

        if last_stats.elapsed() >= Duration::from_secs(2) {
            last_stats = Instant::now();
            let mut rx = shared.rx.lock().unwrap();
            rx.clear();
            for (id, st) in speakers.iter_mut() {
                let s = st.jb.stats;
                let frames = (s.decoded + s.recovered + s.concealed)
                    - (st.snapshot.decoded + st.snapshot.recovered + st.snapshot.concealed);
                if frames > 0 {
                    rx.insert(
                        *id,
                        RxInfo {
                            loss: s.loss_percent(&st.snapshot),
                            jitter_ms: st.jb.jitter_ms() as f32,
                            buffer_ms: st.jb.target_ms() as f32,
                        },
                    );
                }
                st.snapshot = s;
            }
        }
        if last_cleanup.elapsed() > Duration::from_secs(5) {
            last_cleanup = Instant::now();
            speakers.retain(|_, s| s.last_packet.elapsed() < SPEAKER_IDLE);
            let alive: Vec<u32> = speakers.keys().copied().collect();
            shared.rx_level.lock().unwrap().retain(|id, _| alive.contains(id));
            shared.speaking.lock().unwrap().retain(|_, t| t.elapsed() < SPEAKER_IDLE);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> AudioSettings {
        AudioSettings {
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_volume: 1.0,
            voice_mode: VoiceMode::Activity,
            activation_threshold: 0.25,
            noise_suppression: true,
            agc: true,
            echo_cancel: false,
            auto_threshold: false,
            bitrate: 64_000,
            ptt_key: 0,
        }
    }

    #[test]
    fn preroll_is_sent_before_first_word() {
        let mut tx = Transmitter::new().unwrap();
        let s = settings();
        let pcm = vec![0.01f32; FRAME_SAMPLES];
        let mut sent = Vec::new();
        for _ in 0..10 {
            tx.frame(&pcm, 0.0, -80.0, &s, false, true, 0, |p| sent.push(p));
        }
        assert!(sent.is_empty(), "Stille wird nicht gesendet");
        // erster Sprachrahmen allein öffnet noch nicht (könnte ein Klick sein) …
        tx.frame(&pcm, 0.95, -25.0, &s, false, true, 0, |p| sent.push(p));
        assert!(sent.is_empty());
        // … der zweite schon – und der erste kommt über den Vorlauf mit
        tx.frame(&pcm, 0.95, -25.0, &s, false, true, 0, |p| sent.push(p));
        assert_eq!(sent.len(), 1 + dsp::PRE_ROLL_FRAMES, "Vorlauf + aktueller Rahmen");
        let seqs: Vec<u16> = sent.iter().map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)), "fortlaufend: {seqs:?}");
        // Ende -> Ende-Markierung
        sent.clear();
        for _ in 0..40 {
            tx.frame(&pcm, 0.0, -80.0, &s, false, true, 0, |p| sent.push(p));
        }
        let last = sent.last().unwrap();
        assert_eq!(last[2], VOICE_FLAG_END);
        assert_eq!(last.len(), 3);
    }

    #[test]
    fn echo_from_speakers_is_removed() {
        // Lautsprecher spielen sprachähnliches Signal; es kommt 60 ms später, halb so laut, ins Mikro zurück
        let s = AudioSettings { agc: false, noise_suppression: false, echo_cancel: true, ..settings() };
        let mut cap = Capture::new(1, SAMPLE_RATE);
        let mut x = 0x1234_5678u32;
        let mut lp = 0f32;
        let far: Vec<f32> = (0..SAMPLE_RATE as usize * 6)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let n = (x as f32 / u32::MAX as f32) * 2.0 - 1.0;
                lp += (n - lp) * 0.2; // tiefpassgefiltertes Rauschen …
                let syllable = ((i as f32 / 4800.0).sin().abs() > 0.3) as u8 as f32; // … in Silben
                lp * 0.6 * syllable
            })
            .collect();
        let delay = SAMPLE_RATE as usize * 60 / 1000;
        let mic: Vec<f32> = (0..far.len()).map(|i| if i >= delay { far[i - delay] * 0.5 } else { 0.0 }).collect();
        let mut out = Vec::new();
        for (f, m) in far.chunks_exact(DENOISE_FRAME).zip(mic.chunks_exact(DENOISE_FRAME)) {
            let mut block = [0f32; DENOISE_FRAME];
            block.copy_from_slice(f);
            cap.render(&block, &s);
            cap.feed(m, &s, |pcm, _, _| out.extend_from_slice(pcm));
        }
        let tail = out.len() / 2;
        let erle = dsp::rms_db(&mic[tail..]) - dsp::rms_db(&out[tail..]);
        println!("Echo-Dämpfung: {erle:.1} dB");
        assert!(erle > 15.0, "Echo nur um {erle:.1} dB gedämpft");
    }

    #[test]
    fn echo_cancel_keeps_own_voice_without_echo() {
        // Headset: kein Echo, aber Lautsprecher spielen etwas -> eigene Stimme bleibt erhalten
        let s = AudioSettings { agc: false, noise_suppression: false, echo_cancel: true, ..settings() };
        let mut cap = Capture::new(1, SAMPLE_RATE);
        let voice: Vec<f32> = (0..SAMPLE_RATE as usize * 4)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                ((t * 220.0 * std::f32::consts::TAU).sin() * 0.2 + (t * 660.0 * std::f32::consts::TAU).sin() * 0.1)
                    * (((t * 3.0).sin().abs() > 0.2) as u8 as f32)
            })
            .collect();
        let far: Vec<f32> = (0..voice.len()).map(|i| ((i as f32) * 0.011).sin() * 0.3).collect();
        let mut out = Vec::new();
        for (f, m) in far.chunks_exact(DENOISE_FRAME).zip(voice.chunks_exact(DENOISE_FRAME)) {
            let mut block = [0f32; DENOISE_FRAME];
            block.copy_from_slice(f);
            cap.render(&block, &s);
            cap.feed(m, &s, |pcm, _, _| out.extend_from_slice(pcm));
        }
        let half = out.len() / 2;
        let diff = dsp::rms_db(&voice[half..out.len()]) - dsp::rms_db(&out[half..]);
        println!("eigene Stimme: {diff:.1} dB leiser");
        assert!(diff < 3.0, "eigene Stimme um {diff:.1} dB gedämpft");
    }

    #[test]
    fn audio_interface_mic_on_input_1_keeps_full_level() {
        // Stereo-Interface mit 44,1 kHz, Mikro nur auf Eingang 1
        let s = AudioSettings { agc: false, noise_suppression: false, ..settings() };
        let mut cap = Capture::new(2, 44_100);
        let mut inter = Vec::new();
        for i in 0..44_100 * 2 {
            let v = (i as f32 * 300.0 * std::f32::consts::TAU / 44_100.0).sin() * 0.1;
            inter.push(v);
            inter.push(0.0);
        }
        let mut levels = Vec::new();
        for chunk in inter.chunks(882) {
            cap.feed(chunk, &s, |_, _, db| levels.push(db));
        }
        let tail = &levels[levels.len() - 20..];
        let avg = tail.iter().sum::<f32>() / tail.len() as f32;
        assert!((avg - (-23.0)).abs() < 1.0, "Pegel {avg:.1} dBFS (Sinus 0,1 = -23 dBFS)");
        assert_eq!(cap.channel(), (1, 2));
        assert!((95..=105).contains(&levels.len()), "{} Rahmen für 2 s", levels.len());
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Complete new mic chain (stereo 44.1 kHz -> channel pick, resampling, high-pass,
    /// AGC, RNNoise, voice activation, Opus) – CPU share while talking
    #[test]
    #[ignore]
    fn capture_chain_cost() {
        let s = AudioSettings {
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_volume: 1.0,
            voice_mode: VoiceMode::Continuous,
            activation_threshold: 0.25,
            noise_suppression: true,
            agc: true,
            echo_cancel: true,
            auto_threshold: true,
            bitrate: 64_000,
            ptt_key: 0,
        };
        let secs = 20usize;
        let inter: Vec<f32> = (0..44_100 * secs)
            .flat_map(|i| {
                let x = i as f32 / 44_100.0;
                let v = 0.1 * (x * 180.0 * std::f32::consts::TAU).sin() + 0.05 * (x * 1100.0 * std::f32::consts::TAU).sin();
                [v, 0.0]
            })
            .collect();
        let mut cap = Capture::new(2, 44_100);
        let mut tx = Transmitter::new().unwrap();
        let mut n = 0usize;
        let t = Instant::now();
        let far = [0.05f32; DENOISE_FRAME];
        for chunk in inter.chunks(882) {
            cap.render(&far, &s);
            cap.feed(chunk, &s, |pcm, vad, db| {
                tx.frame(pcm, vad, db, &s, false, true, 0, |_| n += 1);
            });
        }
        let pct = t.elapsed().as_secs_f64() / secs as f64 * 100.0;
        println!("Mikrofonkette inkl. Echo-Unterdrückung: {pct:.2} % eines Kerns beim Sprechen ({n} Pakete)");
    }

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

/// Quality comparison old vs. new pipeline with real speech ("Ben's setup").
/// `TS_SPEECH44=… TS_SPEECH48=… cargo test --release -p tidespeak quality_report -- --ignored --nocapture`
/// (f32 mono raw files of the same recording at 44.1 and 48 kHz)
#[cfg(test)]
mod sim {
    use super::*;
    use crate::jitter::JitterBuffer;

    fn load(var: &str) -> Option<Vec<f32>> {
        let b = std::fs::read(std::env::var(var).ok()?).ok()?;
        Some(b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    // ── Nachbau der Version 3.0.0 (zum Vergleich) ─────────────────────────
    struct OldLinear {
        step: f64,
        pos: f64,
        last: f32,
    }
    impl OldLinear {
        fn process(&mut self, s: f32, out: &mut Vec<f32>) {
            while self.pos < 1.0 {
                out.push(self.last + (s - self.last) * self.pos as f32);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.last = s;
        }
    }

    /// returns per 20 ms frame: (sent?, pcm)
    fn old_capture(inter: &[f32], channels: usize, rate: u32) -> Vec<(bool, Vec<f32>)> {
        let mut rs = OldLinear { step: rate as f64 / SAMPLE_RATE as f64, pos: 0.0, last: 0.0 };
        let mut mono48 = Vec::new();
        for f in inter.chunks(channels) {
            rs.process(f.iter().sum::<f32>() / channels as f32, &mut mono48);
        }
        let mut dn = nnnoiseless::DenoiseState::new();
        let (mut level_smooth, mut vad_max, mut hang) = (0f32, 0f32, 0u32);
        let mut frames = Vec::new();
        let mut frame = Vec::new();
        for block in mono48.chunks_exact(DENOISE_FRAME) {
            let scaled: [f32; DENOISE_FRAME] = std::array::from_fn(|i| block[i] * 32768.0);
            let mut clean = [0f32; DENOISE_FRAME];
            let vad = dn.process_frame(&mut clean, &scaled);
            let clean: Vec<f32> = clean.iter().map(|v| v / 32768.0).collect();
            vad_max = vad_max.max(vad);
            let level = ((dsp::rms_db(&clean) + 60.0) / 60.0).clamp(0.0, 1.0);
            level_smooth = if level > level_smooth { level } else { level_smooth * 0.85 + level * 0.15 };
            frame.extend_from_slice(&clean);
            if frame.len() == FRAME_SAMPLES {
                let loud = level_smooth >= 0.35 && vad_max > 0.25;
                let send = if loud {
                    hang = 15;
                    true
                } else if hang > 0 {
                    hang -= 1;
                    true
                } else {
                    false
                };
                vad_max = 0.0;
                frames.push((send, std::mem::take(&mut frame)));
            }
        }
        // 1 Rahmen Vorlauf
        let mut out = frames.clone();
        for i in 1..frames.len() {
            if frames[i].0 && !frames[i - 1].0 {
                out[i - 1].0 = true;
            }
        }
        out
    }

    /// Alter Jitter-Puffer: fest 40 ms, ab 6 Paketen wird verworfen
    fn old_jitter_missing(arrivals: &[(f64, u16, Vec<u8>)], n: usize) -> f64 {
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
        let mut slots: HashMap<u16, Vec<u8>> = HashMap::new();
        let (mut next, mut playing, mut started) = (0u16, false, false);
        let (mut ok, mut missing) = (0usize, 0usize);
        let mut out = vec![0f32; FRAME_SAMPLES];
        let mut ai = 0;
        let mut clock = 0.0;
        while clock < n as f64 * 20.0 + 500.0 {
            while ai < arrivals.len() && arrivals[ai].0 <= clock {
                let (_, s, d) = &arrivals[ai];
                if !started {
                    next = *s;
                    started = true;
                }
                if (s.wrapping_sub(next) as i16) >= 0 {
                    slots.insert(*s, d.clone());
                }
                ai += 1;
            }
            if started {
                if !playing && slots.len() >= 2 {
                    playing = true;
                }
                if playing && (ok + missing) < n {
                    if slots.len() > 6 {
                        slots.remove(&next);
                        next = next.wrapping_add(1);
                        missing += 1;
                    }
                    if let Some(d) = slots.remove(&next) {
                        let _ = dec.decode_float(&d, &mut out, false);
                        ok += 1;
                    } else {
                        let _ = dec.decode_float(&[], &mut out, false);
                        missing += 1;
                    }
                    next = next.wrapping_add(1);
                }
            }
            clock += 20.0;
        }
        missing as f64 * 100.0 / (ok + missing).max(1) as f64
    }

    fn new_jitter_missing(arrivals: &[(f64, u16, Vec<u8>)], n: usize) -> (f64, f64) {
        let mut jb = JitterBuffer::new().unwrap();
        let mut out = vec![0f32; FRAME_SAMPLES];
        let mut ai = 0;
        let mut clock = 0.0;
        while clock < n as f64 * 20.0 + 500.0 {
            while ai < arrivals.len() && arrivals[ai].0 <= clock {
                let (t, s, d) = &arrivals[ai];
                jb.push(*s, 0, d.clone(), *t);
                ai += 1;
            }
            jb.next_frame(&mut out);
            clock += 20.0;
        }
        let s = jb.stats;
        let missing = (s.recovered + s.concealed) as f64;
        // vollständig verlorene (nicht rekonstruierbare) Rahmen
        (missing * 100.0 / (s.decoded as f64 + missing).max(1.0), s.concealed as f64 * 100.0 / (s.decoded as f64 + missing).max(1.0))
    }

    /// Residual after a least-squares fit of all tones (independent of delay), dB
    fn multitone_residual(x: &[f32], tones: &[f64], rate: f64) -> f32 {
        let k = tones.len() * 2;
        let basis = |i: usize, j: usize| {
            let w = 2.0 * std::f64::consts::PI * tones[j / 2] / rate * i as f64;
            if j % 2 == 0 { w.sin() } else { w.cos() }
        };
        // Normalgleichungen lösen (Gauß)
        let mut a = vec![vec![0f64; k + 1]; k];
        for (i, v) in x.iter().enumerate() {
            let b: Vec<f64> = (0..k).map(|j| basis(i, j)).collect();
            for r in 0..k {
                for c in 0..k {
                    a[r][c] += b[r] * b[c];
                }
                a[r][k] += b[r] * *v as f64;
            }
        }
        for c in 0..k {
            let p = (c..k).max_by(|x, y| a[*x][c].abs().total_cmp(&a[*y][c].abs())).unwrap();
            a.swap(c, p);
            for r in 0..k {
                if r != c {
                    let f = a[r][c] / a[c][c];
                    for cc in c..=k {
                        a[r][cc] -= f * a[c][cc];
                    }
                }
            }
        }
        let coef: Vec<f64> = (0..k).map(|r| a[r][k] / a[r][r]).collect();
        let (mut res, mut tot) = (0f64, 0f64);
        for (i, v) in x.iter().enumerate() {
            let fit: f64 = (0..k).map(|j| coef[j] * basis(i, j)).sum();
            res += (*v as f64 - fit).powi(2);
            tot += (*v as f64).powi(2);
        }
        (10.0 * (res / tot).log10()) as f32
    }

    /// Delay-aligned signal-to-noise ratio between test and reference (dB)
    fn aligned_snr(test: &[f32], reference: &[f32]) -> f32 {
        let n = test.len().min(reference.len()) - 200;
        let mut best = f32::MIN;
        for lag in 0..60usize {
            let (mut sig, mut err) = (0f64, 0f64);
            for i in 100..n {
                let r = reference[i] as f64;
                let t = test[i + lag] as f64;
                sig += r * r;
                err += (t - r) * (t - r);
            }
            best = best.max((10.0 * (sig / err).log10()) as f32);
        }
        best
    }

    #[test]
    #[ignore]
    fn quality_report() {
        let (Some(s44), Some(s48)) = (load("TS_SPEECH44"), load("TS_SPEECH48")) else {
            eprintln!("TS_SPEECH44/TS_SPEECH48 nicht gesetzt – übersprungen");
            return;
        };

        // 1) Resampling-Qualität (blechern?): Mehrton-Signal 44,1 -> 48 kHz, alles außer den Tönen ist Verzerrung
        let tones = [440.0f64, 1250.0, 3100.0, 6300.0, 9800.0, 14_500.0];
        let multi: Vec<f32> = (0..44_100 * 2)
            .map(|i| tones.iter().map(|f| (2.0 * std::f64::consts::PI * f * i as f64 / 44_100.0).sin() * 0.12).sum::<f64>() as f32)
            .collect();
        let mut lin = OldLinear { step: 44_100.0 / 48_000.0, pos: 0.0, last: 0.0 };
        let mut old_rs = Vec::new();
        for v in &multi {
            lin.process(*v, &mut old_rs);
        }
        let mut sinc = SincResampler::new(44_100, 48_000);
        let mut new_rs = Vec::new();
        for c in multi.chunks(441) {
            sinc.process(c, &mut new_rs);
        }
        println!("\nResampling 44,1 -> 48 kHz, Verzerrung + Spiegelfrequenzen (niedriger = sauberer):");
        println!("  alt (linear): {:6.1} dB", multitone_residual(&old_rs[4000..80_000], &tones, 48_000.0));
        println!("  neu (sinc):   {:6.1} dB", multitone_residual(&new_rs[4000..80_000], &tones, 48_000.0));
        let _ = &s48;

        // 2) Bens Aufbau: leises XLR-Mikro auf Eingang 1 eines Stereo-Interfaces, 44,1 kHz,
        //    Tastatur zwischendurch und in einer Sprechpause
        let peak = s44.iter().fold(0f32, |m, v| m.max(v.abs()));
        let mut packets: Vec<(u16, Vec<u8>)> = Vec::new();
        for (title, peak_db) in [("leise ausgesteuert, Spitzen -34 dBFS", -34.0f32), ("normal ausgesteuert, Spitzen -14 dBFS", -14.0)] {
        let gain = 10f32.powf(peak_db / 20.0) / peak;
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let len = s44.len();
        let pause = (len * 10 / 30, len * 14 / 30); // 4 s Pause mit Tippen
        let mut mic: Vec<f32> = s44.iter().enumerate().map(|(i, v)| if (pause.0..pause.1).contains(&i) { 0.0 } else { v * gain }).collect();
        let mut clicks = vec![false; len / 882 + 1];
        let mut t = 0usize;
        while t < len {
            t += (44_100.0 * (0.08 + rng.next() * 0.25)) as usize; // ~4–5 Anschläge pro Sekunde
            if t >= len || (rng.next() > 0.35 && !(pause.0..pause.1).contains(&t)) {
                continue;
            }
            for k in 0..220 {
                if t + k < len {
                    mic[t + k] += ((rng.next() * 2.0 - 1.0) * 0.05 * (-(k as f64) / 40.0).exp()) as f32;
                }
            }
            clicks[t / 882] = true;
        }
        let mut inter = Vec::with_capacity(len * 2);
        for v in &mic {
            let floor = ((rng.next() * 2.0 - 1.0) * 3e-4) as f32; // Grundrauschen ~ -76 dBFS
            inter.push(v + floor);
            inter.push(floor);
        }

        // Referenz: in welchen 20-ms-Rahmen wird wirklich gesprochen?
        let ref_db: Vec<f32> = s48.chunks(FRAME_SAMPLES).map(dsp::rms_db).collect();
        let loudest = ref_db.iter().cloned().fold(f32::MIN, f32::max);
        let speech: Vec<bool> = ref_db
            .iter()
            .enumerate()
            .map(|(i, db)| *db > loudest - 30.0 && !(pause.0 / 882..pause.1 / 882).contains(&i))
            .collect();
        let typing_only: Vec<bool> = (0..speech.len())
            .map(|i| (pause.0 / 882..pause.1 / 882).contains(&i) && clicks.get(i).copied().unwrap_or(false))
            .collect();

        let report = |name: &str, sent: &[bool], pcm: &[Vec<f32>]| {
            let tol = |i: usize| sent.get(i).copied().unwrap_or(false) || sent.get(i + 1).copied().unwrap_or(false);
            let sp = speech.iter().filter(|s| **s).count();
            let covered = (0..speech.len().min(sent.len())).filter(|i| speech[*i] && tol(*i)).count();
            let ty = typing_only.iter().filter(|s| **s).count();
            let typed = (0..typing_only.len().min(sent.len())).filter(|i| typing_only[*i] && sent[*i]).count();
            let lv: Vec<f32> = (0..speech.len().min(pcm.len())).filter(|i| speech[*i] && sent[*i]).map(|i| dsp::rms_db(&pcm[i])).collect();
            let level = lv.iter().sum::<f32>() / lv.len().max(1) as f32;
            println!(
                "  {name}: Sprache übertragen {:5.1} %   Tippen allein gesendet {:5.1} %   Pegel beim Zuhörer {:6.1} dBFS",
                covered as f32 * 100.0 / sp.max(1) as f32,
                typed as f32 * 100.0 / ty.max(1) as f32,
                level
            );
        };

        println!("\nBens Aufbau (XLR an Eingang 1, Stereo-Interface, 44,1 kHz, Tippen), {title}:");
        let old = old_capture(&inter, 2, 44_100);
        let (o_sent, o_pcm): (Vec<bool>, Vec<Vec<f32>>) = old.into_iter().unzip();
        report("alt", &o_sent, &o_pcm);

        let s = AudioSettings {
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_volume: 1.0,
            voice_mode: VoiceMode::Activity,
            activation_threshold: 0.25,
            noise_suppression: true,
            agc: true,
            echo_cancel: false,
            auto_threshold: false,
            bitrate: 64_000,
            ptt_key: 0,
        };
        let mut cap = Capture::new(2, 44_100);
        let mut tx = Transmitter::new().unwrap();
        let mut n_sent = Vec::new();
        let mut n_pcm = Vec::new();
        packets.clear();
        for chunk in inter.chunks(882) {
            cap.feed(chunk, &s, |pcm, vad, db| {
                let before = packets.len();
                let on = tx.frame(pcm, vad, db, &s, false, true, 0, |p| {
                    packets.push((u16::from_le_bytes([p[0], p[1]]), p[3..].to_vec()));
                });
                // Vorlauf-Pakete zählen für die Rahmen davor
                let extra = packets.len().saturating_sub(before + 1);
                let k = n_sent.len();
                for j in 1..=extra.min(k) {
                    n_sent[k - j] = true;
                }
                n_sent.push(on);
                n_pcm.push(pcm.to_vec());
            });
        }
        report("neu", &n_sent, &n_pcm);
        println!("  (AGC-Verstärkung am Ende: {:+.1} dB)", cap.agc_gain_db());
        }

        // 3) Internet-Leitung: Schwankung + Verlust, gleiche Pakete durch alten und neuen Puffer
        println!("\nLeitung über das Internet (gleiche Pakete, alter vs. neuer Jitter-Puffer):");
        let audio: Vec<(u16, Vec<u8>)> = packets.into_iter().filter(|p| !p.1.is_empty()).collect();
        // fortlaufend nummerieren (eine durchgehende Sprechphase)
        let audio: Vec<(u16, Vec<u8>)> = audio.into_iter().enumerate().map(|(i, p)| (i as u16, p.1)).collect();
        for (name, base_jitter, spike, loss) in
            [("gute Leitung", 4.0, 0.0, 0.0), ("WLAN / Kabel am Abend", 25.0, 0.03, 0.02), ("schlechte Leitung", 45.0, 0.06, 0.05)]
        {
            let mut rng = Rng(0xfeed_beef_1234_5678);
            // Pakete bleiben in Reihenfolge; ein Stau (Bufferbloat) hält auch die folgenden auf
            let mut arrivals: Vec<(f64, u16, Vec<u8>)> = Vec::new();
            let mut last_arrival = 0.0f64;
            for (i, (s, d)) in audio.iter().enumerate() {
                let mut delay = 30.0 + rng.next() * base_jitter;
                if rng.next() < spike {
                    delay += 80.0 + rng.next() * 80.0;
                }
                let arrival = (i as f64 * 20.0 + delay).max(last_arrival + 0.5);
                last_arrival = arrival;
                if rng.next() < loss {
                    continue;
                }
                arrivals.push((arrival, *s, d.clone()));
            }
            let old = old_jitter_missing(&arrivals, audio.len());
            let (new_missing, new_lost) = new_jitter_missing(&arrivals, audio.len());
            println!(
                "  {name:<22} fehlende Rahmen: alt {old:5.1} %   neu {new_missing:5.1} % (davon {:4.1} % per FEC zurückgeholt)",
                new_missing - new_lost
            );
        }
    }
}
