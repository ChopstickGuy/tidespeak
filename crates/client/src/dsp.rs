//! Signal processing for the microphone path. Everything here is pure
//! computation without I/O, so it can be tested on its own (see `tests` at the end).
//!
//!   device (n channels, any rate)
//!     ─► ChannelPicker   uses the channel that is actually active (audio interfaces!)
//!     ─► SincResampler   to 48 kHz without aliasing
//!     ─► Biquad          80 Hz high-pass (rumble, footsteps, DC)
//!     ─► gain + Agc      even level for quiet and loud microphones
//!     ─► RNNoise         noise suppression + speech probability
//!     ─► VoiceGate       decides whether to send (voice activation)

use std::f32::consts::PI;

// ─────────────────────────────────────────────────────────── Kanalwahl

/// Picks the channel that carries the signal. Audio interfaces (e.g. Focusrite)
/// often show up as stereo with the mic only on input 1. Averaging the channels
/// would halve the level (-6 dB) for no reason.
pub struct ChannelPicker {
    power: Vec<f32>,
    current: usize,
}

impl ChannelPicker {
    pub fn new(channels: usize) -> Self {
        ChannelPicker { power: vec![0.0; channels.max(1)], current: 0 }
    }

    pub fn channels(&self) -> usize {
        self.power.len()
    }

    /// Channel currently in use (0-based)
    pub fn current(&self) -> usize {
        self.current
    }

    /// `interleaved` -> mono, appended to `out`
    pub fn process(&mut self, interleaved: &[f32], out: &mut Vec<f32>) {
        let n = self.power.len();
        if n == 1 {
            out.extend_from_slice(interleaved);
            return;
        }
        let frames = interleaved.len() / n;
        if frames == 0 {
            return;
        }
        // Leistung je Kanal, langsam geglättet (~1 s)
        let alpha = (frames as f32 / 48_000.0).min(1.0);
        for c in 0..n {
            let p = (0..frames).map(|f| interleaved[f * n + c].powi(2)).sum::<f32>() / frames as f32;
            self.power[c] += (p - self.power[c]) * alpha;
        }
        // Mit Hysterese wechseln (6 dB), damit es nicht hin- und herspringt
        let best = (0..n).max_by(|a, b| self.power[*a].total_cmp(&self.power[*b])).unwrap_or(0);
        if best != self.current && self.power[best] > self.power[self.current] * 4.0 {
            self.current = best;
        }
        let c = self.current;
        out.extend((0..frames).map(|f| interleaved[f * n + c]));
    }
}

// ─────────────────────────────────────────────────────────── Resampler

const SINC_TAPS: usize = 32; // Stützstellen je Ausgangswert
const SINC_PHASES: usize = 256; // Auflösung der Tabelle zwischen zwei Eingangswerten

/// Band-limited resampler (Kaiser-windowed sinc). Unlike linear interpolation
/// it creates no aliasing, which is what makes linear resampling sound "tinny"
/// or "metallic". Rates are arbitrary (44.1 → 48, 96 → 48, 48 → 44.1 …).
pub struct SincResampler {
    step: f64,
    pos: f64,
    history: Vec<f32>,
    table: Vec<f32>,
    passthrough: bool,
}

impl SincResampler {
    pub fn new(from: u32, to: u32) -> Self {
        let passthrough = from == to;
        // Grenzfrequenz relativ zur Eingangs-Nyquist-Frequenz; beim Heruntertakten tiefer legen
        let cutoff = 0.94 * (to as f64 / from as f64).min(1.0);
        let half = (SINC_TAPS / 2) as f64;
        let beta = 8.0;
        let i0_beta = bessel_i0(beta);
        let len = SINC_TAPS * SINC_PHASES + 1;
        let table = (0..len)
            .map(|i| {
                let x = i as f64 / SINC_PHASES as f64 - half; // −half … +half
                let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * cutoff * x) };
                let r = x / half;
                let window = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(beta * (1.0 - r * r).sqrt()) / i0_beta };
                (cutoff * sinc * window) as f32
            })
            .collect();
        SincResampler {
            step: from as f64 / to as f64,
            // erster Ausgangswert genau auf dem ersten echten Eingangssample (keine Verschiebung)
            pos: (SINC_TAPS / 2 + 1) as f64,
            history: vec![0.0; SINC_TAPS],
            table,
            passthrough,
        }
    }

    #[inline]
    fn kernel(&self, x: f64) -> f32 {
        // x ∈ [−half, half] -> Tabellenindex, linear zwischen zwei Phasen
        let idx = (x + (SINC_TAPS / 2) as f64) * SINC_PHASES as f64;
        if idx < 0.0 || idx >= (self.table.len() - 1) as f64 {
            return 0.0;
        }
        let i = idx as usize;
        let f = (idx - i as f64) as f32;
        self.table[i] + (self.table[i + 1] - self.table[i]) * f
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        self.history.extend_from_slice(input);
        let half = SINC_TAPS / 2;
        // pos zählt in Eingangs-Samples ab history[half-1]; erst rechnen, wenn rechts genug Daten da sind
        loop {
            let center = self.pos.floor() as usize + half - 1;
            if center + half >= self.history.len() {
                break;
            }
            let frac = self.pos - self.pos.floor();
            let mut acc = 0.0f32;
            for k in 0..SINC_TAPS {
                let idx = center + 1 + k - half; // center-half+1 … center+half
                let x = (k as f64 + 1.0 - half as f64) - frac;
                acc += self.history[idx] * self.kernel(x);
            }
            out.push(acc);
            self.pos += self.step;
        }
        // Verbrauchtes vorne abschneiden, SINC_TAPS als Vorgeschichte behalten
        let consumed = self.pos.floor() as usize;
        if consumed > 0 {
            let drop = consumed.min(self.history.len().saturating_sub(SINC_TAPS));
            if drop > 0 {
                self.history.drain(..drop);
                self.pos -= drop as f64;
            }
        }
    }
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

// ─────────────────────────────────────────────────────────── Filter

/// 2nd-order filter (RBJ cookbook), here as a high-pass
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn highpass(freq: f32, rate: f32) -> Self {
        let w0 = 2.0 * PI * freq / rate;
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let alpha = w0.sin() / (2.0 * q);
        let cos = w0.cos();
        let a0 = 1.0 + alpha;
        Biquad {
            b0: (1.0 + cos) / 2.0 / a0,
            b1: -(1.0 + cos) / a0,
            b2: (1.0 + cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn process(&mut self, block: &mut [f32]) {
        for x in block.iter_mut() {
            let y = self.b0 * *x + self.z1;
            self.z1 = self.b1 * *x - self.a1 * y + self.z2;
            self.z2 = self.b2 * *x - self.a2 * y;
            *x = y;
        }
    }
}

// ─────────────────────────────────────────────────────────── Pegel

pub fn rms_db(block: &[f32]) -> f32 {
    if block.is_empty() {
        return -120.0;
    }
    let p = block.iter().map(|x| x * x).sum::<f32>() / block.len() as f32;
    10.0 * p.max(1e-12).log10()
}

/// Automatic mic level: brings speech to about -20 dBFS. It only adapts while
/// someone is actually speaking (RNNoise speech probability), so pauses and
/// background noise never get pumped up.
pub struct Agc {
    gain_db: f32,
    current: f32,
}

pub const AGC_TARGET_DB: f32 = -20.0;
const AGC_MAX_DB: f32 = 36.0;
const AGC_MIN_DB: f32 = -10.0;

impl Default for Agc {
    fn default() -> Self {
        Agc { gain_db: 6.0, current: 2.0 }
    }
}

impl Agc {
    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// `speech` = speech probability of the previous block (0…1)
    pub fn process(&mut self, block: &mut [f32], speech: f32) {
        let level = rms_db(block);
        if speech > 0.6 && level > -70.0 {
            let err = AGC_TARGET_DB - (level + self.gain_db);
            if err < 0.0 {
                self.gain_db += err * 0.2; // zu laut: schnell zurück
            } else {
                self.gain_db += err.min(0.08); // zu leise: sanft hoch (≈ 8 dB/s)
            }
            self.gain_db = self.gain_db.clamp(AGC_MIN_DB, AGC_MAX_DB);
        }
        // Verstärkung weich über den Block ändern – keine Klicks
        let target = 10f32.powf(self.gain_db / 20.0);
        let n = block.len().max(1) as f32;
        for (i, x) in block.iter_mut().enumerate() {
            let g = self.current + (target - self.current) * (i as f32 + 1.0) / n;
            *x *= g;
        }
        self.current = target;
    }
}

/// Gentle limiter: below 0.7 untouched, above that soft into saturation (never clips)
pub fn soft_limit(block: &mut [f32]) {
    for x in block.iter_mut() {
        let a = x.abs();
        if a > 0.7 {
            *x = x.signum() * (0.7 + 0.3 * ((a - 0.7) / 0.3).tanh());
        }
    }
}

// ─────────────────────────────────────────────────── Sprachaktivierung

pub const GATE_HOLD_FRAMES: u32 = 20; // 400 ms Nachhalten
pub const PRE_ROLL_FRAMES: usize = 3; // 60 ms Vorlauf

/// Voice activation with hysteresis. Opens only when RNNoise detects *speech*
/// (typing, fans and clicks have a low speech probability) and the level
/// reaches the sensitivity threshold. While open, a lower bar is enough, so
/// quiet word endings don't get cut off.
#[derive(Default)]
pub struct VoiceGate {
    hold: u32,
    candidate: u32,
}

impl VoiceGate {
    /// `vad` 0…1, `level_db` dBFS of the 20 ms frame, `threshold_db` from the settings
    pub fn decide(&mut self, vad: f32, level_db: f32, threshold_db: f32) -> bool {
        // Öffnen erst nach 40 ms Sprache am Stück: ein einzelner Tastenanschlag reicht nicht.
        // Der Vorlauf (60 ms) liefert den Wortanfang trotzdem mit.
        let speechy = vad >= 0.55 && level_db >= threshold_db;
        self.candidate = if speechy { self.candidate + 1 } else { 0 };
        let open = speechy && (self.candidate >= 2 || self.hold > 0);
        let sustain = self.hold > 0 && vad >= 0.25 && level_db >= threshold_db - 10.0;
        if open {
            self.hold = GATE_HOLD_FRAMES;
        } else if sustain {
            self.hold = self.hold.max(GATE_HOLD_FRAMES / 2);
        } else {
            self.hold = self.hold.saturating_sub(1);
        }
        self.hold > 0
    }

    pub fn reset(&mut self) {
        self.hold = 0;
        self.candidate = 0;
    }
}

/// Slider 0…1 -> threshold in dBFS (-60 … 0)
pub fn threshold_db(slider: f32) -> f32 {
    -60.0 + 60.0 * slider.clamp(0.0, 1.0)
}

/// dBFS -> display value 0…1 (same scale as the slider)
pub fn level_to_slider(db: f32) -> f32 {
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Testton, in f64 berechnet: In f32 wird die Phase bei großen Argumenten ungenau
    /// und erzeugt selbst einen Rauschteppich, der die Messung verfälscht.
    fn sine(freq: f32, rate: u32, secs: f32, amp: f32) -> Vec<f32> {
        let w = 2.0 * std::f64::consts::PI * freq as f64 / rate as f64;
        (0..(rate as f32 * secs) as usize).map(|i| ((i as f64 * w).sin() * amp as f64) as f32).collect()
    }

    /// Residual after a least-squares fit of a sine at `freq`, in dB relative to the signal
    /// (everything that is not the tone: distortion, aliasing, noise). Also returns the amplitude.
    fn residual_db(x: &[f32], freq: f32, rate: u32) -> (f32, f32) {
        let w = 2.0 * std::f64::consts::PI * freq as f64 / rate as f64;
        let (mut ss, mut cc, mut sc, mut ys, mut yc) = (0f64, 0f64, 0f64, 0f64, 0f64);
        for (i, v) in x.iter().enumerate() {
            let (s, c) = ((w * i as f64).sin(), (w * i as f64).cos());
            ss += s * s;
            cc += c * c;
            sc += s * c;
            ys += *v as f64 * s;
            yc += *v as f64 * c;
        }
        let det = ss * cc - sc * sc;
        let a = (ys * cc - yc * sc) / det;
        let b = (yc * ss - ys * sc) / det;
        let (mut res, mut tot) = (0f64, 0f64);
        for (i, v) in x.iter().enumerate() {
            let f = a * (w * i as f64).sin() + b * (w * i as f64).cos();
            res += (*v as f64 - f).powi(2);
            tot += (*v as f64).powi(2);
        }
        ((10.0 * (res / tot).log10()) as f32, (a * a + b * b).sqrt() as f32)
    }

    #[test]
    fn channel_picker_uses_active_input() {
        // Audio-Interface: Mikro auf Eingang 1, Eingang 2 nur Grundrauschen
        let mic = sine(300.0, 48_000, 0.5, 0.1);
        let mut inter = Vec::new();
        for (i, v) in mic.iter().enumerate() {
            inter.push(*v);
            inter.push(((i * 7919) % 100) as f32 * 1e-6);
        }
        let mut p = ChannelPicker::new(2);
        let mut out = Vec::new();
        p.process(&inter, &mut out);
        assert_eq!(p.current(), 0);
        let db = rms_db(&out[out.len() / 2..]);
        assert!((db - rms_db(&mic)).abs() < 0.5, "Pegel bleibt erhalten: {db}");

        // Mikro an Eingang 2 -> wird erkannt
        let mut inter2 = Vec::new();
        for v in &mic {
            inter2.push(0.0);
            inter2.push(*v);
        }
        let mut p2 = ChannelPicker::new(2);
        let mut out2 = Vec::new();
        for chunk in inter2.chunks(960) {
            p2.process(chunk, &mut out2);
        }
        assert_eq!(p2.current(), 1);
    }

    #[test]
    fn resampler_keeps_tone_without_aliasing() {
        for (from, to) in [(44_100u32, 48_000u32), (96_000, 48_000), (48_000, 44_100)] {
            let x = sine(1000.0, from, 1.0, 0.5);
            let mut r = SincResampler::new(from, to);
            let mut y = Vec::new();
            for chunk in x.chunks(441) {
                r.process(chunk, &mut y);
            }
            let expected = to as usize;
            assert!((y.len() as i64 - expected as i64).abs() < 64, "{from}->{to}: {} Werte", y.len());
            let tail = &y[2000..y.len() - 2000];
            let (rest, amp) = residual_db(tail, 1000.0, to);
            assert!(rest < -60.0, "{from}->{to}: Verzerrung {rest:.1} dB");
            assert!((amp - 0.5).abs() < 0.005, "{from}->{to}: Amplitude {amp:.4}");
        }
        // Ein 36-kHz-Ton (nur bei 96 kHz möglich) darf nach 48 kHz nicht als Spiegelfrequenz (12 kHz) auftauchen
        let x = sine(36_000.0, 96_000, 0.5, 0.5);
        let mut r = SincResampler::new(96_000, 48_000);
        let mut y = Vec::new();
        r.process(&x, &mut y);
        let leak = rms_db(&y[1000..]) - rms_db(&x);
        assert!(leak < -60.0, "Aliasing {leak:.1} dB");
    }

    #[test]
    fn highpass_removes_rumble_keeps_voice() {
        let mut hp = Biquad::highpass(80.0, 48_000.0);
        let mut low = sine(20.0, 48_000, 1.0, 0.5);
        hp.process(&mut low);
        assert!(rms_db(&low[24_000..]) < rms_db(&sine(20.0, 48_000, 1.0, 0.5)) - 20.0);
        let mut hp = Biquad::highpass(80.0, 48_000.0);
        let mut voice = sine(300.0, 48_000, 1.0, 0.5);
        hp.process(&mut voice);
        assert!((rms_db(&voice[24_000..]) - rms_db(&sine(300.0, 48_000, 1.0, 0.5))).abs() < 0.5);
    }

    #[test]
    fn agc_lifts_quiet_speech() {
        let mut agc = Agc::default();
        // sehr leises Mikro: -50 dBFS
        let quiet = sine(250.0, 48_000, 6.0, 0.0045);
        let mut last = 0.0;
        for chunk in quiet.chunks(480) {
            let mut b = chunk.to_vec();
            agc.process(&mut b, 0.9);
            last = rms_db(&b);
        }
        assert!((last - AGC_TARGET_DB).abs() < 3.0, "nach 6 s bei {last:.1} dBFS");
        // ohne Sprache wird nichts angehoben
        let mut agc = Agc::default();
        let before = agc.gain_db();
        for chunk in quiet.chunks(480) {
            let mut b = chunk.to_vec();
            agc.process(&mut b, 0.05);
        }
        assert_eq!(agc.gain_db(), before);
    }

    #[test]
    fn gate_keeps_word_endings_and_ignores_typing() {
        let thr = threshold_db(0.25);
        let mut g = VoiceGate::default();
        // Tippen: laut, aber keine Sprache
        assert!(!(0..20).any(|_| g.decide(0.1, -25.0, thr)));
        // ein einzelner sprachähnlicher Klick öffnet nicht, 40 ms Sprache schon
        assert!(!g.decide(0.9, -25.0, thr));
        assert!(!g.decide(0.0, -80.0, thr));
        assert!(!g.decide(0.9, -25.0, thr));
        assert!(g.decide(0.9, -25.0, thr));
        // leises Wortende (-50 dB, unsicher) bleibt offen
        assert!(g.decide(0.3, -50.0, thr));
        // Stille: nach der Nachhaltezeit zu
        let open_frames = (0..40).take_while(|_| g.decide(0.0, -80.0, thr)).count();
        assert!((8..=GATE_HOLD_FRAMES as usize).contains(&open_frames), "{open_frames}");
    }
}

