//! Settings drawer: slides in from the right, six categories, every change is
//! applied at once and confirmed with a short typed message.

use std::sync::atomic::Ordering;
use std::time::Instant;

use tidespeak::config::VoiceMode;
use tidespeak::grid::{self, decode, ease, wrap, Grid, C};
use tidespeak::i18n::{self, l};
use tidespeak_proto::fingerprint_short;

use super::*;

/// One column of controls with its own row counter and scroll offset
struct Pane<'a> {
    d: &'a mut Grid,
    fx: &'a [Fx],
    now: Instant,
    hits: Vec<Hit>,
    r: i32,
    x: i32,
    right: i32,
    top: i32,
    bottom: i32,
    scroll: i32,
}

impl Pane<'_> {
    fn row(&self, r: i32) -> Option<i32> {
        let y = r - self.scroll;
        (y >= self.top && y <= self.bottom).then_some(y)
    }
    fn width(&self) -> i32 {
        self.right - self.x + 1
    }
    fn put(&mut self, r: i32, c: i32, s: &str, co: C) {
        if let Some(y) = self.row(r) {
            self.d.put(y, c, s, co);
        }
    }
    fn segs(&mut self, r: i32, c: i32, list: &[(&str, C)]) {
        if let Some(y) = self.row(r) {
            self.d.segs(y, c, list);
        }
    }
    fn hit(&mut self, r: i32, c: i32, h: i32, w: i32, act: Act) {
        if let Some(y) = self.row(r) {
            if self.row(r + h - 1).is_some() {
                self.hits.push(Hit { r: y, c, h, w, act });
            }
        }
    }
    fn fxp(&self, kind: &FxKind) -> Option<f32> {
        self.fx.iter().find(|f| &f.kind == kind).map(|f| f.p(self.now)).filter(|p| *p < 1.0)
    }
    fn fx_from(&self, kind: &FxKind) -> Option<(f32, f32)> {
        self.fx.iter().find(|f| &f.kind == kind).map(|f| (f.p(self.now), f.from)).filter(|p| p.0 < 1.0)
    }

    fn sec(&mut self, title: &str) {
        let n = (self.width() - 4 - title.chars().count() as i32).max(0) as usize;
        let (r, x) = (self.r, self.x);
        self.segs(r, x, &[(&format!("── {title} "), C::D), (&"─".repeat(n), C::G)]);
        self.r += 2;
    }
    fn note(&mut self, text: &str, co: C) {
        for ln in wrap(text, (self.width() - 2) as usize) {
            let (r, x) = (self.r, self.x);
            self.put(r, x, &ln, co);
            self.r += 1;
        }
        self.r += 1;
    }
    fn radio(&mut self, on: bool, label: &str, meta: &str, desc: &str, group: &'static str, i: i32) {
        let fx = self.fxp(&FxKind::Radio(group, i));
        let (mark, co) = match fx {
            Some(p) => (["(·)", "(o)", "(•)"][((p * 3.0) as usize).min(2)], C::TealB),
            None if on => ("(•)", C::Teal),
            None => ("( )", C::D),
        };
        let (r, x, right) = (self.r, self.x, self.right);
        let max = (self.width() - 6 - meta.chars().count() as i32).max(8) as usize;
        self.segs(r, x, &[(&format!("{mark} "), co), (&grid::cut(label, max), if fx.is_some() { C::TealB } else { C::T })]);
        if !meta.is_empty() {
            self.put(r, right - meta.chars().count() as i32 + 1, meta, C::F);
        }
        if !desc.is_empty() {
            self.put(r + 1, x + 4, desc, C::F);
        }
        let h = if desc.is_empty() { 1 } else { 2 };
        let w = self.width();
        self.hit(r, x, h, w, Act::Radio(group, i));
        self.r += if desc.is_empty() { 1 } else { 3 };
    }
    fn toggle(&mut self, on: bool, label: &str, desc: &str, key: &'static str) {
        let fx = self.fxp(&FxKind::Toggle(key));
        let (mark, co) = match fx {
            Some(p) => {
                let seq = if on { ["[·]", "[+]", "[*]", "[x]"] } else { ["[*]", "[+]", "[·]", "[ ]"] };
                (seq[((p * 4.0) as usize).min(3)], C::TealB)
            }
            None if on => ("[x]", C::Teal),
            None => ("[ ]", C::D),
        };
        let (r, x) = (self.r, self.x);
        self.segs(r, x, &[(&format!("{mark} "), co), (label, if fx.is_some() { C::TealB } else { C::T })]);
        if !desc.is_empty() {
            self.put(r + 1, x + 4, &grid::cut(desc, (self.width() - 5) as usize), C::F);
        }
        let w = self.width();
        self.hit(r, x, if desc.is_empty() { 1 } else { 2 }, w, Act::Toggle(key));
        self.r += if desc.is_empty() { 2 } else { 3 };
    }
    fn slider(&mut self, label: &str, val: f32, max: f32, key: &'static str) {
        let fx = self.fx_from(&FxKind::Slider(key));
        let shown = match fx {
            Some((p, from)) => from + (val - from) * ease(p),
            None => val,
        };
        let k = ((shown / max) * 24.0).round().clamp(0.0, 24.0) as usize;
        let (r, x) = (self.r, self.x);
        self.put(r, x, label, C::T);
        let mut bar = "█".repeat(k);
        if fx.is_some() && k > 0 {
            bar = format!("{}▌", "█".repeat(k - 1));
        }
        let hot = fx.is_some();
        self.segs(r, x + 16, &[("[", C::D), (&bar, if hot { C::TealB } else { C::Teal }), (&"░".repeat(24 - k), C::F), ("]", C::D), (&format!("  {:.0} %", shown), if hot { C::TealB } else { C::T })]);
        self.hit(r, x + 17, 1, 24, Act::Slider(key));
        self.r += 2;
    }
    fn meter(&mut self, label: &str, level: f32, thr: Option<f32>, active: bool) {
        let (r, x) = (self.r, self.x);
        self.put(r, x, label, C::T);
        let lv = (level * 24.0).round() as i32;
        let th = thr.map(|t| (t * 24.0).round() as i32);
        let mut cells: Vec<(String, C)> = vec![("[".into(), C::D)];
        for i in 0..24 {
            let c = if Some(i) == th {
                ("|".into(), C::B)
            } else if i < lv {
                ("█".into(), if active { C::Green } else { C::TDim })
            } else {
                ("░".into(), C::F)
            };
            cells.push(c);
        }
        cells.push(("]".into(), C::D));
        cells.push((if active { l("  SENDET", "  ON AIR") } else { l("  STILL", "  QUIET") }.into(), if active { C::Green } else { C::F }));
        let list: Vec<(&str, C)> = cells.iter().map(|(s, c)| (s.as_str(), *c)).collect();
        self.segs(r, x + 16, &list);
        self.r += 2;
    }
    fn choice(&mut self, opts: &[&str], sel: usize, key: &'static str) {
        let fx = self.fxp(&FxKind::Choice(key));
        let mut c = self.x;
        let r = self.r;
        for (i, o) in opts.iter().enumerate() {
            let txt = format!("[ {o} ]");
            let s = if i == sel && fx.is_some() { decode(&txt, fx.unwrap_or(1.0), 4) } else { txt.clone() };
            self.put(r, c, &s, if i == sel { C::TealB } else { C::D });
            let n = txt.chars().count() as i32;
            self.hit(r, c, 1, n, Act::Choice(key, i as i32));
            c += n + 1;
        }
        self.r += 2;
    }
    fn kv(&mut self, label: &str, val: &str, co: C) {
        let (r, x, right) = (self.r, self.x, self.right);
        self.put(r, x, label, C::T);
        let dots = (right - x - label.chars().count() as i32 - val.chars().count() as i32 - 2).max(0) as usize;
        self.put(r, x + label.chars().count() as i32 + 1, &".".repeat(dots), C::G);
        self.put(r, right - val.chars().count() as i32 + 1, val, co);
        self.r += 1;
    }
}

impl App {
    pub fn draw_drawer(&mut self, g: &mut Grid) {
        let now = Instant::now();
        let (rows, cols) = (self.rows, self.cols);
        let dw = self.drawer_width();
        let left = cols - dw;
        let (tab, scroll) = self.drawer.as_ref().map(|d| (d.tab, d.scroll)).unwrap_or((0, 0));

        // Hintergrund abdunkeln, Eingabefelder darunter ausblenden
        g.recolor(C::G, 3, rows - 1, 0, cols - 1);
        self.fields = [FieldPos::default(); 3];
        self.hits.clear();
        self.hits.push(Hit { r: 3, c: 0, h: rows - 3, w: left, act: Act::CloseSettings });
        self.hits.push(Hit { r: 0, c: 0, h: 3, w: cols, act: Act::Swallow });
        let set = format!("[ {} ]", l("EINSTELLUNGEN", "SETTINGS"));
        let sx = cols - 17 - set.chars().count() as i32;
        g.recolor(C::TealB, 1, 1, sx, sx + set.chars().count() as i32 - 1);
        self.hits.push(Hit { r: 0, c: sx, h: 3, w: set.chars().count() as i32, act: Act::OpenSettings });

        let mut d = Grid::new(rows, cols);
        d.frame(3, left, rows - 3, dw, C::TealB, true);
        d.put(3, left + 2, &format!("[ {} ]", l("EINSTELLUNGEN", "SETTINGS")), C::B);
        for r in 4..rows - 1 {
            d.set(r, left + 21, '│', C::G);
        }
        let tabs: [(&str, &str, &str); 6] = if i18n::is_german() {
            [
                ("01", "AUDIO", "EIN- UND AUSGABE"),
                ("02", "SPRACHE", "SPRACHÜBERTRAGUNG"),
                ("03", "KLANG", "KLANG UND ANIMATION"),
                ("04", "VERBINDUNG", "LEITUNG"),
                ("05", "SICHERHEIT", "SICHERHEIT"),
                ("06", "ALLGEMEIN", "APP-SPRACHE · LANGUAGE"),
            ]
        } else {
            [
                ("01", "AUDIO", "INPUT AND OUTPUT"),
                ("02", "VOICE", "VOICE TRANSMISSION"),
                ("03", "SOUND", "SOUND AND ANIMATION"),
                ("04", "CONNECTION", "LINK"),
                ("05", "SECURITY", "SECURITY"),
                ("06", "GENERAL", "LANGUAGE · SPRACHE"),
            ]
        };
        let mut hits = vec![Hit { r: 3, c: left, h: rows - 3, w: dw, act: Act::Swallow }];
        for (i, t) in tabs.iter().enumerate() {
            let r = 6 + i as i32 * 2;
            let on = tab == i;
            if on {
                d.set(r, left + 1, '▌', C::TealB);
            }
            d.segs(r, left + 3, &[(&format!("{}  ", t.0), if on { C::TealB } else { C::F }), (t.1, if on { C::B } else { C::T })]);
            hits.push(Hit { r, c: left + 1, h: 1, w: 19, act: Act::Tab(i) });
        }
        d.put(rows - 3, left + 3, &format!("TIDESPEAK {}", self.version), C::F);
        let x = left + 24;
        let right = cols - 4;
        d.put(5, x, &format!("{} // {}", tabs[tab].0, tabs[tab].1), C::TealB);
        d.put(6, x, tabs[tab].2, C::B);
        d.put(5, right - 4, "[ X ]", C::T);
        hits.push(Hit { r: 5, c: right - 4, h: 1, w: 5, act: Act::CloseSettings });

        // Bestätigung unten
        match self.fx.iter().find(|f| f.kind == FxKind::Toast) {
            Some(f) => {
                let p = f.p(now);
                let txt = format!("» {} · {}", f.text.to_uppercase(), l("ÜBERNOMMEN", "APPLIED"));
                let txt = grid::cut(&txt, (right - x) as usize);
                let n = (txt.chars().count() as f32 * (p * 4.0).min(1.0)) as usize;
                let s: String = txt.chars().take(n).collect();
                d.put(rows - 2, x, &format!("{s}{}", if n < txt.chars().count() { "█" } else { "" }), if p > 0.8 { C::Teal } else { C::TealB });
            }
            None => {
                d.put(rows - 2, x, l("ÄNDERUNGEN WERDEN SOFORT ÜBERNOMMEN", "CHANGES APPLY IMMEDIATELY"), C::F);
            }
        }

        // Inhalt
        let mut p = Pane { d: &mut d, fx: &self.fx, now, hits: Vec::new(), r: 9, x, right, top: 8, bottom: rows - 4, scroll };
        let c = &self.cfg;
        match tab {
            0 => {
                p.sec(l("AUSGABE", "OUTPUT"));
                p.radio(c.output_device.is_none(), l("Systemstandard", "System default"), "", "", "out", 0);
                for (i, n) in self.outputs.iter().enumerate() {
                    p.radio(c.output_device.as_deref() == Some(n.as_str()), n, "", "", "out", i as i32 + 1);
                }
                if let Some(e) = self.audio.shared.output_error.lock().unwrap().clone() {
                    p.note(&format!("!! {e}"), C::Red);
                }
                p.r += 1;
                p.slider(l("LAUTSTÄRKE", "VOLUME"), c.output_volume * 100.0, 200.0, "vol");
                p.sec(l("MIKROFON", "MICROPHONE"));
                let info = self.audio.shared.input_info.lock().unwrap().clone();
                let meta = info.rsplit(" · ").next().unwrap_or("").to_uppercase();
                p.radio(c.input_device.is_none(), l("Systemstandard", "System default"), if c.input_device.is_none() { &meta } else { "" }, "", "in", 0);
                for (i, n) in self.inputs.iter().enumerate() {
                    let sel = c.input_device.as_deref() == Some(n.as_str());
                    p.radio(sel, n, if sel { &meta } else { "" }, "", "in", i as i32 + 1);
                }
                if let Some(e) = self.audio.shared.input_error.lock().unwrap().clone() {
                    p.note(&format!("!! {e}"), C::Red);
                }
                p.r += 1;
                let thr = (c.voice_mode == VoiceMode::Activity).then(|| if c.auto_threshold { self.audio.shared.threshold_now() } else { c.activation_threshold });
                let active = self.audio.shared.transmitting.load(Ordering::Relaxed) || self.audio.shared.mic_test.load(Ordering::Relaxed);
                p.meter(l("PEGEL", "LEVEL"), self.level, thr, active);
                p.slider(l("VERSTÄRKUNG", "GAIN"), c.input_gain * 100.0, 400.0, "gain");
                p.toggle(c.auto_gain, l("Automatische Lautstärke", "Automatic gain"), l("Gleicht leise und laute Mikrofone aus – ideal für XLR", "Evens out quiet and loud mics – ideal for XLR"), "agc");
                p.toggle(c.echo_cancel, l("Echo-Unterdrückung", "Echo cancellation"), l("Wichtig, wenn du Lautsprecher statt Kopfhörer nutzt", "Important when you use speakers instead of headphones"), "aec");
                p.toggle(self.audio.shared.mic_test.load(Ordering::Relaxed), l("Mikrofon testen", "Test microphone"), l("Du hörst dich selbst – so klingst du für andere", "You hear yourself – this is how others hear you"), "test");
            }
            1 => {
                p.sec(l("WANN GESENDET WIRD", "WHEN TO TRANSMIT"));
                p.radio(c.voice_mode == VoiceMode::Activity, l("Sprachaktivierung", "Voice activation"), "", l("Sendet automatisch, sobald du sprichst", "Transmits automatically when you speak"), "mode", 0);
                if c.voice_mode == VoiceMode::Activity {
                    p.slider(l("EMPFINDLICHKEIT", "SENSITIVITY"), c.activation_threshold * 100.0, 100.0, "sens");
                    p.toggle(c.auto_threshold, l("Automatisch an den Raum anpassen", "Adapt to the room automatically"), "", "autosens");
                    let thr = if c.auto_threshold { self.audio.shared.threshold_now() } else { c.activation_threshold };
                    p.meter(l("PEGEL", "LEVEL"), self.level, Some(thr), self.audio.shared.transmitting.load(Ordering::Relaxed));
                }
                p.radio(c.voice_mode == VoiceMode::PushToTalk, "Push-to-Talk", "", l("Sendet nur, solange die Taste gedrückt ist", "Transmits only while the key is held"), "mode", 1);
                if c.voice_mode == VoiceMode::PushToTalk {
                    let sel = PTT_KEYS.iter().position(|k| k.0 == c.ptt_key).unwrap_or(0);
                    let keys: Vec<&str> = PTT_KEYS.iter().map(|k| k.0).collect();
                    p.choice(&keys, sel, "ptt");
                    p.note(l("Unter Windows systemweit, unter Linux solange Tidespeak im Vordergrund ist.", "System-wide on Windows, on Linux while Tidespeak has focus."), C::F);
                }
                p.radio(c.voice_mode == VoiceMode::Continuous, l("Dauerhaft senden", "Always on"), "", l("Offenes Mikrofon – am besten mit Headset", "Open mic – best with a headset"), "mode", 2);
            }
            2 => {
                p.sec(l("MIKROFONSIGNAL", "MIC SIGNAL"));
                p.toggle(c.noise_suppression, l("Hintergrundgeräusche entfernen", "Remove background noise"), l("Tastatur, Lüfter und Rauschen (RNNoise, läuft lokal)", "Keyboard, fans and hiss (RNNoise, runs locally)"), "ns");
                p.toggle(c.echo_cancel, l("Echo-Unterdrückung", "Echo cancellation"), l("Andere hören sich nicht selbst über deine Lautsprecher", "Others don't hear themselves through your speakers"), "aec");
                p.toggle(c.auto_gain, l("Automatische Lautstärke", "Automatic gain"), l("Hält deine Stimme gleichmäßig laut", "Keeps your voice at an even level"), "agc");
                p.sec(l("SPRACHQUALITÄT", "VOICE QUALITY"));
                let sel = BITRATES.iter().position(|b| *b == c.opus_bitrate).unwrap_or(1);
                p.choice(&[l("SPARSAM", "LOW"), "STANDARD", l("HOCH", "HIGH")], sel, "quality");
                p.note(l("32 / 64 / 96 kbit/s – höhere Qualität braucht etwas mehr Bandbreite.", "32 / 64 / 96 kbit/s – higher quality needs a little more bandwidth."), C::F);
                p.sec(l("ANIMATIONEN UND KLÄNGE", "ANIMATIONS AND SOUNDS"));
                p.toggle(c.terminal_sounds, l("Terminal-Klänge", "Terminal sounds"), l("Leise Klicks und Signaltöne bei Sequenzen", "Quiet clicks and beeps during sequences"), "sounds");
                p.toggle(c.anim_wave, l("Server-Welle in Bewegung", "Moving server wave"), l("Rollt langsam weiter, etwa 8 Bilder pro Sekunde", "Keeps rolling slowly, about 8 frames per second"), "wave");
                p.toggle(c.anim_connect, l("Verbindungs-Sequenz", "Connection sequence"), l("Agentenfilm-Sequenz beim ersten Besuch eines Servers", "Spy-movie sequence on the first visit of a server"), "connect");
                p.toggle(c.anim_start, l("Start und Beenden", "Start and quit"), l("Wellen-Sequenz beim Öffnen und Schließen", "Wave sequence when opening and closing"), "start");
                p.toggle(c.anim_join, l("Beitreten und Verlassen", "Join and leave"), l("Zielerfassung und Zerfall, unter einer Sekunde", "Target lock and dissolve, under a second"), "join");
                p.toggle(c.anim_switch, l("Kanalwechsel", "Channel switch"), l("Drahtgitter-Übergang, gut eine Sekunde", "Wireframe transition, about a second"), "switch");
            }
            3 => {
                p.sec(l("LEITUNG", "LINK"));
                if self.conn == Conn::Connected {
                    let ping = if self.ping > 0 { format!("{} ms", self.ping) } else { "–".into() };
                    let pco = if self.ping < 80 { C::Green } else if self.ping < 150 { C::Amber } else { C::Red };
                    p.kv(l("PING ZUM SERVER", "PING TO SERVER"), &ping, pco);
                    let rx = format!("{:.1} %", self.rx_loss);
                    let rx = if i18n::is_german() { rx.replace('.', ",") } else { rx };
                    let rco = if self.rx_loss < 2.0 { C::Green } else if self.rx_loss < 8.0 { C::Amber } else { C::Red };
                    p.kv(l("EMPFANG · VERLUST", "RECEIVE · LOSS"), &rx, rco);
                    let tco = if self.tx_loss < 2 { C::Green } else if self.tx_loss < 8 { C::Amber } else { C::Red };
                    p.kv(l("DEINE STIMME BEI DEN ANDEREN", "YOUR VOICE AT THE OTHERS"), &format!("{} %", self.tx_loss), tco);
                    p.kv("SERVER", &self.address, C::T);
                    p.r += 1;
                    p.sec(l("VERZÖGERUNG MUND-ZU-OHR", "MOUTH-TO-EAR DELAY"));
                    let (total, parts) = self.audio.shared.delay_estimate(self.ping);
                    let tco = if total < 200 { C::Green } else if total < 400 { C::Amber } else { C::Red };
                    p.kv(l("GESAMT (GESCHÄTZT)", "TOTAL (ESTIMATED)"), &format!("~{total} ms"), tco);
                    for (key, ms) in parts {
                        let label = match key {
                            "mic" => l("  MIKROFON (GERÄT + 20-MS-PAKET)", "  MICROPHONE (DEVICE + 20 MS PACKET)"),
                            "net" => l("  NETZ (SPRECHER → SERVER → DU)", "  NETWORK (SPEAKER → SERVER → YOU)"),
                            "jitter" => l("  JITTER-PUFFER", "  JITTER BUFFER"),
                            "mix" => l("  MISCHER", "  MIXER"),
                            _ => l("  WIEDERGABE (GERÄT)", "  PLAYBACK (DEVICE)"),
                        };
                        let co = if key != "net" && key != "jitter" && ms > 150 { C::Red } else { C::D };
                        p.kv(label, &format!("{ms} ms"), co);
                    }
                    p.r += 1;
                    p.note(
                        l(
                            "Unter etwa 200 ms fällt man sich kaum ins Wort. Ist ein Gerätewert rot, puffert der Soundserver zu viel – Tidespeak fordert 20 ms an.",
                            "Below about 200 ms people rarely talk over each other. If a device value is red, the sound server buffers too much – Tidespeak asks for 20 ms.",
                        ),
                        C::F,
                    );
                    p.note(
                        l(
                            "Verlust bis 2 % ist unproblematisch – Tidespeak rekonstruiert fehlende Pakete. Darüber hilft meist ein LAN-Kabel statt WLAN.",
                            "Up to 2 % loss is fine – Tidespeak rebuilds missing packets. Above that, a LAN cable instead of Wi-Fi usually helps.",
                        ),
                        C::F,
                    );
                } else {
                    p.note(l("// Nicht verbunden – die Werte erscheinen, sobald du mit einem Server verbunden bist.", "// Not connected – the values appear once you are connected to a server."), C::F);
                }
            }
            4 => {
                p.sec(l("FINGERABDRUCK DES SERVERS", "SERVER FINGERPRINT"));
                if self.conn == Conn::Connected && !self.fingerprint.is_empty() {
                    let fp = fingerprint_short(&self.fingerprint);
                    let vf = p.fxp(&FxKind::Verify);
                    let q = vf.unwrap_or(if self.verify_done { 1.0 } else { 0.0 });
                    let top = p.r;
                    p.put(top + 1, x + 2, &self.address, C::T);
                    let shown = if q > 0.0 && q < 0.55 { decode(&fp, q / 0.55, 3) } else { fp.clone() };
                    p.put(top + 2, x + 2, &shown, if q > 0.0 && q < 0.55 { C::Teal } else { C::TealB });
                    if q > 0.0 {
                        let fr = ((q - 0.35) / 0.5).clamp(0.0, 1.0);
                        let co = if q >= 1.0 { C::Green } else { C::TealB };
                        let fw = (right - x + 1).min(62);
                        let mut pts: Vec<(i32, i32, char)> = Vec::new();
                        for i in 0..fw {
                            pts.push((top, x + i, if i == 0 { '┌' } else if i == fw - 1 { '┐' } else { '─' }));
                        }
                        for i in 1..3 {
                            pts.push((top + i, x + fw - 1, '│'));
                        }
                        for i in (0..fw).rev() {
                            pts.push((top + 3, x + i, if i == 0 { '└' } else if i == fw - 1 { '┘' } else { '─' }));
                        }
                        for i in (1..3).rev() {
                            pts.push((top + i, x, '│'));
                        }
                        let n = (pts.len() as f32 * fr).round() as usize;
                        for (r, c, ch) in pts.into_iter().take(n) {
                            p.put(r, c, &ch.to_string(), co);
                        }
                        if q > 0.82 {
                            let stamp = format!("[ {} ]", l("VERIFIZIERT", "VERIFIED"));
                            let s = if q < 1.0 { decode(&stamp, (q - 0.82) / 0.18, 6) } else { stamp.clone() };
                            p.put(top + 1, x + fw - 2 - stamp.chars().count() as i32, &s, C::Green);
                        }
                    }
                    p.r = top + 5;
                    if !self.verify_done {
                        let b = format!("[ {} ]", l("FINGERABDRUCK BESTÄTIGEN", "CONFIRM FINGERPRINT"));
                        let r = p.r;
                        p.put(r, x, &b, C::TealB);
                        p.hit(r, x, 1, b.chars().count() as i32, Act::Verify);
                        p.r += 2;
                    }
                    p.note(
                        l(
                            "Vergleiche ihn mit der Anzeige des Servers – stimmen sie überein, bist du wirklich mit ihm verbunden.",
                            "Compare it with what the server shows – if they match, you are really connected to it.",
                        ),
                        C::F,
                    );
                } else {
                    p.note(
                        l(
                            "// Verbinde dich mit einem Server, um seinen Fingerabdruck zu sehen. Tidespeak merkt sich jeden Server beim ersten Besuch und schlägt Alarm, wenn sich seine Identität ändert.",
                            "// Connect to a server to see its fingerprint. Tidespeak remembers every server on the first visit and raises an alert if its identity changes.",
                        ),
                        C::F,
                    );
                }
                p.sec(l("VERSCHLÜSSELUNG", "ENCRYPTION"));
                p.kv("TRANSPORT", "QUIC · TLS 1.3", C::T);
                p.kv(l("KONTO", "ACCOUNT"), l("KEINS NÖTIG", "NONE NEEDED"), C::T);
            }
            _ => {
                p.sec(l("SPRACHE DER APP · LANGUAGE", "LANGUAGE · SPRACHE DER APP"));
                let sys = if i18n::system_is_german() { l("DEUTSCH ERKANNT", "GERMAN DETECTED") } else { l("ENGLISCH ERKANNT", "ENGLISH DETECTED") };
                p.radio(c.language == "auto", l("Automatisch – wie das System", "Automatic – like the system"), sys, l("Englisch, außer das System steht auf Deutsch", "English, unless the system is set to German"), "lang", 0);
                p.radio(c.language == "en", "English", "", "", "lang", 1);
                p.radio(c.language == "de", "Deutsch", "", "", "lang", 2);
                p.r += 1;
                p.note(l("Die Wahl gilt sofort, ohne Neustart.", "The choice applies immediately, without a restart."), C::F);
            }
        }
        let content_end = p.r;
        hits.extend(p.hits);
        if let Some(dr) = &mut self.drawer {
            let max = (content_end - (rows - 4)).max(0);
            dr.scroll = dr.scroll.min(max);
            if max > 0 {
                d.put(rows - 3, right - 10, if scroll < max { "▼ MEHR" } else { "▲" }, C::F);
            }
        }

        // Reiterwechsel: Scanlinie baut den Inhalt neu auf
        if let Some(pt) = self.fx.iter().find(|f| f.kind == FxKind::Tab).map(|f| f.p(now)).filter(|p| *p < 1.0) {
            let rev = 8 + (ease(pt) * (rows - 12) as f32) as i32;
            d.clear(rev + 1, x, (rows - 4 - rev).max(0), right - x + 1);
            if rev < rows - 4 {
                d.put(rev, x, &"═".repeat((right - x + 1) as usize), C::TealB);
            }
        }

        // Herein- bzw. hinausgleiten
        let shift = if let Some(q) = self.fx.iter().find(|f| f.kind == FxKind::DrawerClose).map(|f| f.p(now)) {
            (ease(q) * dw as f32) as i32
        } else if let Some(q) = self.fx.iter().find(|f| f.kind == FxKind::DrawerOpen).map(|f| f.p(now)).filter(|p| *p < 1.0) {
            ((1.0 - ease(q)) * dw as f32) as i32
        } else {
            0
        };
        g.clear(3, left + shift, rows - 3, (dw - shift).max(0));
        g.overlay(&d, 3, rows - 1, left, cols - 1, shift);
        if shift > 0 {
            for r in 3..rows {
                g.set(r, left + shift - 1, '▓', C::TealB);
                g.set(r, left + shift - 2, '░', C::Teal);
            }
        } else {
            self.hits.extend(hits);
        }
    }
}
