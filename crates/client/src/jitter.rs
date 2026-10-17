//! Adaptive jitter buffer per speaker.
//!
//! Over the internet, packets don't arrive evenly every 20 ms. Some arrive
//! early, some late, and a few never. The buffer measures this jitter and keeps
//! just enough audio in reserve that late packets usually still play on time,
//! while staying as short as possible to keep latency low.
//! Missing packets are first reconstructed from the next one (Opus FEC), and
//! only then concealed (PLC).

use tidespeak_proto::{FRAME_SAMPLES, SAMPLE_RATE, VOICE_FLAG_END};

const SLOTS: usize = 32;
const FRAME_MS: f64 = 20.0;
pub const MIN_TARGET: usize = 2; // 40 ms
pub const MAX_TARGET: usize = 12; // 240 ms

struct Packet {
    seq: u16,
    flags: u8,
    data: Vec<u8>,
}

/// Counters for the connection display and for loss feedback to the sender
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    /// inserted frames to build up reserve (not loss)
    pub stretched: u64,
    pub decoded: u64,
    pub recovered: u64, // aus FEC rekonstruiert
    pub concealed: u64, // verdeckt (PLC)
    pub late: u64,
}

impl Stats {
    /// Share of missing frames among all played frames, in percent
    pub fn loss_percent(&self, since: &Stats) -> f32 {
        let missing = (self.recovered - since.recovered) + (self.concealed - since.concealed);
        let total = missing + (self.decoded - since.decoded);
        if total == 0 {
            0.0
        } else {
            missing as f32 * 100.0 / total as f32
        }
    }
}

pub struct JitterBuffer {
    dec: opus::Decoder,
    slots: [Option<Packet>; SLOTS],
    next: u16,
    synced: bool,
    playing: bool,
    plc_run: u32,
    /// Target reserve in frames
    target: usize,
    jitter_ms: f64,
    last_arrival: Option<(f64, u16)>,
    below_since: f64,
    over_frames: u32,
    since_stretch: u32,
    pub stats: Stats,
}

impl JitterBuffer {
    pub fn new() -> Option<Self> {
        Some(JitterBuffer {
            dec: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).ok()?,
            slots: Default::default(),
            next: 0,
            synced: false,
            playing: false,
            plc_run: 0,
            target: MIN_TARGET,
            jitter_ms: 0.0,
            last_arrival: None,
            below_since: 0.0,
            over_frames: 0,
            since_stretch: 0,
            stats: Stats::default(),
        })
    }

    pub fn target_ms(&self) -> f64 {
        self.target as f64 * FRAME_MS
    }

    pub fn jitter_ms(&self) -> f64 {
        self.jitter_ms
    }

    pub fn buffered(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    fn reset_stream(&mut self) {
        self.slots = Default::default();
        self.synced = false;
        self.playing = false;
        self.plc_run = 0;
        self.over_frames = 0;
        let _ = self.dec.reset_state();
    }

    /// Packet received. `now_ms` = arrival time in milliseconds (any origin).
    pub fn push(&mut self, seq: u16, flags: u8, data: Vec<u8>, now_ms: f64) {
        // Laufzeitschwankung wie in RFC 3550 schätzen
        if let Some((t0, s0)) = self.last_arrival {
            let dseq = seq.wrapping_sub(s0) as i16;
            if dseq > 0 {
                let d = ((now_ms - t0) - dseq as f64 * FRAME_MS).abs();
                self.jitter_ms += (d - self.jitter_ms) / 16.0;
                self.last_arrival = Some((now_ms, seq));
            }
        } else {
            self.last_arrival = Some((now_ms, seq));
        }
        if flags & VOICE_FLAG_END != 0 && data.is_empty() {
            // Neue Sprechphase startet später mit frischer Zeitbasis
            self.last_arrival = None;
        }
        self.adapt_target(now_ms);

        if !self.synced {
            self.next = seq;
            self.synced = true;
        }
        let ahead = seq.wrapping_sub(self.next) as i16;
        if ahead < 0 {
            // zu spät – der Puffer war zu knapp
            self.stats.late += 1;
            self.target = (self.target + 1).min(MAX_TARGET);
            self.below_since = now_ms;
            return;
        }
        if ahead as usize >= SLOTS {
            self.reset_stream();
            self.next = seq;
            self.synced = true;
        }
        self.slots[seq as usize % SLOTS] = Some(Packet { seq, flags, data });
    }

    fn adapt_target(&mut self, now_ms: f64) {
        // genug Reserve für ~3× die mittlere Schwankung
        let want = (((self.jitter_ms * 3.0 + 10.0) / FRAME_MS).ceil() as usize + 1).clamp(MIN_TARGET, MAX_TARGET);
        if want > self.target {
            self.target = want;
            self.below_since = now_ms;
        } else if want < self.target {
            // nur langsam verkleinern: erst nach 4 s ruhiger Leitung um einen Rahmen
            if now_ms - self.below_since > 4000.0 {
                self.target -= 1;
                self.below_since = now_ms;
            }
        } else {
            self.below_since = now_ms;
        }
    }

    fn take(&mut self, seq: u16) -> Option<Packet> {
        let slot = &mut self.slots[seq as usize % SLOTS];
        if slot.as_ref().is_some_and(|p| p.seq == seq) {
            slot.take()
        } else {
            None
        }
    }

    fn peek(&self, seq: u16) -> Option<&Packet> {
        self.slots[seq as usize % SLOTS].as_ref().filter(|p| p.seq == seq)
    }

    /// Next 20 ms into `out`. `false` = this speaker is currently silent.
    pub fn next_frame(&mut self, out: &mut [f32]) -> bool {
        debug_assert_eq!(out.len(), FRAME_SAMPLES);
        if !self.synced {
            return false;
        }
        if !self.playing {
            let has_end = self.slots.iter().flatten().any(|p| p.flags & VOICE_FLAG_END != 0);
            if self.buffered() < self.target && !has_end {
                return false;
            }
            self.playing = true;
        }

        // Zu wenig Reserve für diese Leitung: einen verdeckten Rahmen einschieben (ohne vorzurücken),
        // damit der Puffer wächst – besser einmal 20 ms gedehnt als später immer wieder Lücken
        self.since_stretch += 1;
        if self.buffered() + 1 < self.target && self.buffered() > 0 && self.since_stretch > 5 {
            self.since_stretch = 0;
            self.stats.stretched += 1;
            return matches!(self.dec.decode_float(&[], out, false), Ok(n) if n == out.len());
        }

        // Dauerhaft deutlich zu viel Reserve (Uhren laufen auseinander) -> einen Rahmen überspringen
        if self.buffered() > self.target + 3 {
            self.over_frames += 1;
            if self.over_frames > 50 {
                self.over_frames = 0;
                if self.take(self.next).is_some() {
                    self.next = self.next.wrapping_add(1);
                }
            }
        } else {
            self.over_frames = 0;
        }

        let seq = self.next;
        if let Some(p) = self.take(seq) {
            self.next = seq.wrapping_add(1);
            if p.flags & VOICE_FLAG_END != 0 && p.data.is_empty() {
                self.reset_stream();
                return false;
            }
            self.plc_run = 0;
            if matches!(self.dec.decode_float(&p.data, out, false), Ok(n) if n == out.len()) {
                self.stats.decoded += 1;
                return true;
            }
            return false;
        }

        // Paket fehlt
        if self.buffered() == 0 {
            // nichts mehr da: kurz verdecken, dann ist die Sprechphase vorbei
            self.plc_run += 1;
            if self.plc_run > 3 {
                self.reset_stream();
                return false;
            }
        }
        self.next = seq.wrapping_add(1);
        let fec = self.peek(seq.wrapping_add(1)).filter(|p| !p.data.is_empty()).map(|p| p.data.clone());
        let ok = match fec {
            Some(d) => {
                self.stats.recovered += 1;
                self.dec.decode_float(&d, out, true)
            }
            None => {
                self.stats.concealed += 1;
                self.dec.decode_float(&[], out, false)
            }
        };
        matches!(ok, Ok(n) if n == out.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(n: usize) -> Vec<Vec<u8>> {
        let mut enc = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        enc.set_inband_fec(true).unwrap();
        enc.set_packet_loss_perc(10).unwrap();
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

    /// Simuliert eine Leitung: Pakete alle 20 ms, mit Schwankung und Verlust.
    /// Liefert (gespielt, rekonstruiert, verdeckt, Zielpuffer in ms)
    fn simulate(jitter: impl Fn(usize) -> f64, lost: impl Fn(usize) -> bool, n: usize) -> (u64, u64, u64, f64) {
        let pk = packets(n);
        let mut jb = JitterBuffer::new().unwrap();
        let mut arrivals: Vec<(f64, usize)> =
            (0..n).filter(|i| !lost(*i)).map(|i| (i as f64 * 20.0 + jitter(i), i)).collect();
        // Ende-Markierung wie beim echten Sender
        let end_at = arrivals.iter().map(|a| a.0).fold(0.0, f64::max) + 1.0;
        arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut out = vec![0f32; FRAME_SAMPLES];
        let mut ai = 0;
        let mut played = 0;
        // Wiedergabetakt: alle 20 ms ein Rahmen, ab dem ersten Paket
        let mut clock = 0.0;
        while clock < n as f64 * 20.0 + 400.0 {
            while ai < arrivals.len() && arrivals[ai].0 <= clock {
                let i = arrivals[ai].1;
                jb.push(i as u16, 0, pk[i].clone(), arrivals[ai].0);
                ai += 1;
            }
            if (end_at - 20.0..end_at).contains(&clock) {
                jb.push(n as u16, VOICE_FLAG_END, Vec::new(), end_at);
            }
            if jb.next_frame(&mut out) {
                played += 1;
            }
            clock += 20.0;
        }
        let _ = played;
        (jb.stats.decoded, jb.stats.recovered, jb.stats.concealed, jb.target_ms())
    }

    #[test]
    fn steady_line_stays_short() {
        let (dec, rec, conc, target) = simulate(|_| 1.0, |_| false, 500);
        assert_eq!(dec, 500);
        assert_eq!(rec + conc, 0);
        assert!(target <= 60.0, "Zielpuffer {target} ms");
    }

    #[test]
    fn jittery_line_adapts_instead_of_dropping() {
        // 0–90 ms zufällige Verzögerung (WLAN, Kabelinternet am Abend)
        let j = |i: usize| {
            let mut x = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x ^= x >> 29;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 32;
            (x % 91) as f64
        };
        let (dec, rec, conc, target) = simulate(j, |_| false, 1500);
        let missing = rec + conc;
        assert!(missing * 100 / (dec + missing) < 3, "{missing} von {} Rahmen fehlten", dec + missing);
        assert!(target >= 80.0, "Puffer hat sich angepasst: {target} ms");
    }

    #[test]
    fn single_losses_are_recovered_by_fec() {
        let (dec, rec, conc, _) = simulate(|_| 2.0, |i| i % 20 == 7, 1000);
        assert_eq!(dec, 950);
        assert!(rec >= 45, "FEC rekonstruierte {rec} von 50");
        assert!(conc <= 5);
    }

    #[test]
    fn end_marker_stops_cleanly() {
        let pk = packets(5);
        let mut jb = JitterBuffer::new().unwrap();
        let mut out = vec![0f32; FRAME_SAMPLES];
        for i in 0..5u16 {
            jb.push(i, 0, pk[i as usize].clone(), i as f64 * 20.0);
        }
        jb.push(5, VOICE_FLAG_END, Vec::new(), 100.0);
        let n = (0..10).filter(|_| jb.next_frame(&mut out)).count();
        assert_eq!(n, 5);
        assert!(!jb.is_playing());
        assert_eq!(jb.stats.concealed, 0);
    }
}
