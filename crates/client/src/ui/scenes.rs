//! Full-screen scenes: start, connect screen, connection sequence, disconnect and quit.

use tidespeak::config::normalize_address;
use tidespeak::grid::{self, clamp01, ease, tide, wrap, Grid, Rng, TideOpts, C, TIDE_T};
use tidespeak::i18n::{l, lf};
use tidespeak_proto::{fingerprint_short, PROTOCOL_VERSION};

use super::*;

fn typed(txt: &str, f: f32, f0: f32, speed: f32) -> String {
    let n = ((f - f0) * speed).max(0.0) as usize;
    txt.chars().take(n).collect()
}

fn cursor(f: f32, f0: f32, len: usize, speed: f32) -> &'static str {
    if f >= f0 && (((f - f0) * speed) as usize) < len && ((f / 3.0) as i32) % 2 == 0 {
        "█"
    } else {
        ""
    }
}

fn banner_lines() -> [String; 6] {
    let letters = |ch: char| -> [&'static str; 6] {
        match ch {
            'T' => ["████████╗", "╚══██╔══╝", "   ██║   ", "   ██║   ", "   ██║   ", "   ╚═╝   "],
            'I' => ["██╗", "██║", "██║", "██║", "██║", "╚═╝"],
            'D' => ["██████╗ ", "██╔══██╗", "██║  ██║", "██║  ██║", "██████╔╝", "╚═════╝ "],
            'E' => ["███████╗", "██╔════╝", "█████╗  ", "██╔══╝  ", "███████╗", "╚══════╝"],
            'S' => ["███████╗", "██╔════╝", "███████╗", "╚════██║", "███████║", "╚══════╝"],
            'P' => ["██████╗ ", "██╔══██╗", "██████╔╝", "██╔═══╝ ", "██║     ", "╚═╝     "],
            'A' => [" █████╗ ", "██╔══██╗", "███████║", "██╔══██║", "██║  ██║", "╚═╝  ╚═╝"],
            _ => ["██╗  ██╗", "██║ ██╔╝", "█████╔╝ ", "██╔═██╗ ", "██║  ██╗", "╚═╝  ╚═╝"],
        }
    };
    let word: Vec<char> = "TIDESPEAK".chars().collect();
    std::array::from_fn(|i| word.iter().map(|c| letters(*c)[i]).collect::<Vec<_>>().join(" "))
}

impl App {
    // ── Verbinden-Bildschirm

    pub fn draw_connect(&mut self, g: &mut Grid) {
        self.connect_screen(g, true, true);
    }

    /// `live` = fields and buttons active, `wave` = draw the wave (not during the start)
    pub fn connect_screen(&mut self, g: &mut Grid, live: bool, wave: bool) {
        self.header(g, None);
        let cx = self.cols / 2;
        if wave {
            let t = if self.cfg.anim_wave { TIDE_T + self.wave } else { TIDE_T };
            tide(g, 10.0, cx as f32, 2.6, t, &TideOpts::default());
        }
        g.center(17, cx, l("SPRACH- UND TEXTCHAT · VERSCHLÜSSELT · OHNE KONTO", "VOICE AND TEXT CHAT · ENCRYPTED · NO ACCOUNT"), C::D);
        let x0 = cx - 40;
        g.panel(20, x0, 15, 80, C::TealB, true, l("VERBINDUNG HERSTELLEN", "ESTABLISH CONNECTION"), C::B);
        g.put(23, x0 + 4, "SERVER", C::D);
        g.put(23, x0 + 14, &format!("[{}]", " ".repeat(58)), C::Teal);
        g.put(26, x0 + 4, "NAME", C::D);
        g.put(26, x0 + 14, &format!("[{}]", " ".repeat(58)), C::Teal);
        let btn = format!("[ » {} ]", l("VERBINDEN", "CONNECT"));
        let bx = x0 + 73 - btn.chars().count() as i32;
        let connecting = self.conn != Conn::Offline;
        g.put(30, bx, &btn, if connecting { C::F } else { C::TealB });
        g.put(32, x0 + 4, &format!("TIDESPEAK {} · QUIC · TLS 1.3 · OPUS · RNNOISE", self.version), C::F);
        if live {
            self.fields[0] = FieldPos { shown: true, r: 23, c: x0 + 16, w: 56 };
            self.fields[1] = FieldPos { shown: true, r: 26, c: x0 + 16, w: 56 };
            self.hits.push(Hit { r: 29, c: bx, h: 3, w: btn.chars().count() as i32, act: Act::Connect });
        }

        if let Some((addr, old, new)) = self.key_change.clone() {
            // Alarm: der Server hat seine Identität geändert
            let w = 96.min(self.cols - 8);
            let c0 = cx - w / 2;
            g.panel(36, c0, 11, w, C::RedB, true, l("ALARM", "ALERT"), C::RedB);
            g.put(37, c0 + 3, &format!("!! {}", l("IDENTITÄT DES SERVERS HAT SICH GEÄNDERT – MÖGLICHER ANGRIFF", "SERVER IDENTITY HAS CHANGED – POSSIBLE ATTACK")), C::RedB);
            for (i, ln) in wrap(
                l(
                    "Das passiert, wenn der Server neu aufgesetzt wurde – oder wenn sich jemand dazwischenschaltet. Frag beim Betreiber nach, ob der neue Fingerabdruck stimmt.",
                    "This happens when the server was reinstalled – or when someone is intercepting the connection. Ask the operator whether the new fingerprint is correct.",
                ),
                (w - 8) as usize,
            )
            .iter()
            .take(2)
            .enumerate()
            {
                g.put(38 + i as i32, c0 + 3, ln, C::T);
            }
            g.segs(41, c0 + 3, &[(l("BISHER  ", "BEFORE  "), C::D), (&old, C::D), ("   ", C::D), (&addr, C::F)]);
            g.segs(42, c0 + 3, &[(l("JETZT   ", "NOW     "), C::D), (&new, C::RedB)]);
            let cancel = format!("[ {} ]", l("ABBRECHEN", "CANCEL"));
            let trust = format!("[ {} ]", l("NEUEM SCHLÜSSEL VERTRAUEN", "TRUST NEW KEY"));
            let tx = c0 + w - 4 - trust.chars().count() as i32;
            let cxx = tx - 2 - cancel.chars().count() as i32;
            g.put(44, cxx, &cancel, C::T);
            g.put(44, tx, &trust, C::Red);
            if live {
                self.hits.push(Hit { r: 44, c: cxx, h: 1, w: cancel.chars().count() as i32, act: Act::CancelKey });
                self.hits.push(Hit { r: 44, c: tx, h: 1, w: trust.chars().count() as i32, act: Act::TrustKey });
            }
            return;
        }
        if let Some(e) = &self.error {
            for (i, ln) in wrap(e, 76).iter().take(2).enumerate() {
                g.center(36 + i as i32, cx, ln, C::Red);
            }
        }
        if !self.cfg.recent.is_empty() {
            g.segs(39, x0, &[(&format!("── {} ", l("ZULETZT VERBUNDEN", "RECENT SERVERS")), C::D), (&"─".repeat(56), C::G)]);
            let recent = self.cfg.recent.clone();
            for (i, a) in recent.iter().take(((self.rows - 44) as usize).clamp(1, 5)).enumerate() {
                let r = 41 + i as i32;
                let fp = self.cfg.known_servers.get(a).map(|f| fingerprint_short(f)).unwrap_or_default();
                let fp: String = fp.chars().take(19).collect();
                let label = format!("[ {} ]", grid::cut(a, 34));
                g.put(r, x0, &label, C::T);
                g.put(r, x0 + 40, &format!("{} · {fp}", l("BEKANNT", "KNOWN")), C::F);
                if live {
                    self.hits.push(Hit { r, c: x0, h: 1, w: label.chars().count() as i32, act: Act::Recent(i) });
                }
            }
        }
        g.center(self.rows - 2, cx, l("ERSTER BESUCH EINES SERVERS: VOLLE VERBINDUNGS-SEQUENZ", "FIRST VISIT OF A SERVER: FULL CONNECTION SEQUENCE"), C::G);
    }

    // ── Start der App

    pub fn draw_boot(&mut self, g: &mut Grid, f: f32) {
        let (rows, cols) = (self.rows, self.cols);
        let cx = cols as f32 / 2.0;
        let m = ease((f - 80.0) / 25.0);
        let (cr0, sc0) = ((rows as f32 * 0.55).round(), 7.0f32.min(rows as f32 / 4.6));
        let (cr, cc, sc) = (cr0 + (10.0 - cr0) * m, cx, sc0 + (2.6 - sc0) * m);
        // Wellenringe hinter allem
        if (26.0..92.0).contains(&f) {
            for k in 0..2 {
                let q = ((f - 26.0) / 22.0 + k as f32 * 0.5) % 1.0;
                let rr = sc * 2.2 + q * 13.0;
                let co = if q < 0.7 { C::TDim } else { C::G };
                let mut a = 0.0f32;
                while a < 6.283 {
                    g.putf(cr - rr * a.sin(), cc + 2.05 * rr * a.cos(), "~", co);
                    a += 0.035;
                }
            }
        }
        // Startprotokoll
        if f < 100.0 {
            let co = (f > 84.0).then_some(C::G);
            let input = self.audio.shared.input_info.lock().unwrap().clone();
            let lines = [
                format!("AUDIO · {}", if input.is_empty() { l("Systemstandard", "System default").to_string() } else { input }),
                format!("OPUS 48 kHz · RNNOISE · {}", if self.cfg.echo_cancel { l("ECHO-UNTERDRÜCKUNG", "ECHO CANCELLATION") } else { "AEC AUS" }),
                "QUIC · TLS 1.3 · ALPN tidespeak/1".into(),
                lf(format!("{} BEKANNTE SERVER", self.cfg.known_servers.len()), format!("{} KNOWN SERVERS", self.cfg.known_servers.len())),
                lf(format!("OBERFLÄCHE ASCII · {cols}×{rows} ZEICHEN · DEUTSCH"), format!("INTERFACE ASCII · {cols}×{rows} CELLS · ENGLISH")),
            ];
            let r0 = rows - 10;
            if f >= 2.0 {
                g.put(r0, 4, &format!("TIDESPEAK {} // {}", self.version, l("KALTSTART", "COLD START")), co.unwrap_or(C::TealB));
            }
            let mut shown = 0;
            for (k, ln) in lines.iter().enumerate() {
                let t0 = 6.0 + 8.0 * k as f32;
                if f < t0 {
                    break;
                }
                shown = k + 1;
                let ok = f >= t0 + 5.0;
                g.segs(r0 + 1 + k as i32, 4, &[(if ok { "[ OK ] " } else { "[ .. ] " }, co.unwrap_or(if ok { C::Green } else { C::Amber })), (&grid::cut(ln, 70), co.unwrap_or(C::D))]);
            }
            if f >= 2.0 && f < 84.0 && ((f / 4.0) as i32) % 2 == 0 {
                g.put(r0 + 1 + shown as i32, 4, "█", C::Teal);
            }
        }
        // Schriftzug
        if (34.0..100.0).contains(&f) {
            let lines = banner_lines();
            let width = lines[0].chars().count() as i32;
            let left = cols / 2 - width / 2;
            let front = (((f - 34.0) / 26.0) * (width + 4) as f32) as i32;
            let mut rng = Rng::new((f * 31.0) as u32 + 5);
            let noise: Vec<char> = "#%&@$*+=".chars().collect();
            for (i, ln) in lines.iter().enumerate() {
                for (j, ch) in ln.chars().enumerate() {
                    let j = j as i32;
                    if ch == ' ' || j > front + 1 {
                        continue;
                    }
                    let r = 3 + i as i32;
                    if j >= front - 1 {
                        g.set(r, left + j, noise[(rng.next() * 8.0) as usize % 8], C::Teal);
                    } else {
                        g.set(r, left + j, ch, if f > 84.0 { C::G } else if ch == '█' { C::TealB } else { C::Purple });
                    }
                }
            }
        }
        // Verbinden-Bildschirm per Scanlinie einblenden
        if f >= 100.0 {
            let mut g2 = Grid::new(rows, cols);
            self.connect_screen(&mut g2, false, false);
            let rev = ((f - 100.0) * 2.2) as i32;
            g.copy_from(&g2, 0, rev, 0, cols - 1);
            if rev < rows - 1 {
                g.put(rev + 1, 0, &"═".repeat(cols as usize), C::TealB);
            }
        }
        // Die Welle: entsteht aus Punkten, wird unruhig, beruhigt sich und wandert nach oben
        let dens = clamp01((f - 6.0) / 26.0);
        if dens > 0.0 {
            let end = 110.0;
            let t = if f < end { TIDE_T - 0.0006 * (end - f) * (end - f) } else { TIDE_T + (f - end) * 0.06 };
            let yaw = if f < end { 0.45 - 0.00012 * (end - f) * (end - f) } else { 0.45 };
            let amp = if f < 70.0 { 1.7 * ease((f - 6.0) / 34.0) } else { 1.7 - 0.7 * ease((f - 70.0) / 36.0) };
            tide(g, cr, cc, sc, t, &TideOpts { density: dens, seed: f as u32 + 1, amp, yaw, ..Default::default() });
        }
        self.skip_button(g);
    }

    fn skip_button(&mut self, g: &mut Grid) {
        let s = format!("[ {} ]", l("ÜBERSPRINGEN", "SKIP"));
        let c = self.cols - 4 - s.chars().count() as i32;
        g.put(self.rows - 2, c, &s, C::F);
        self.hits.push(Hit { r: self.rows - 2, c, h: 1, w: s.chars().count() as i32, act: Act::Skip });
    }

    // ── Verbindungs-Sequenz

    pub fn draw_sequence(&mut self, g: &mut Grid, f: f32, fast: bool, ok: bool, fail: Option<String>) {
        let (rows, cols) = (self.rows, self.cols);
        let cx = cols / 2;
        let addr = normalize_address(&self.texts[0]);
        let name = self.texts[1].trim().to_string();
        let glitch_end = if fast { 5.0 } else { 10.0 };
        let trans = if fast { 30.0 } else { 146.0 };
        let gate = if fast { 16.0 } else { 62.0 };

        if f < glitch_end {
            // Bildstörung: der Verbinden-Bildschirm zerfällt
            let mut g0 = Grid::new(rows, cols);
            self.connect_screen(&mut g0, false, true);
            let q = f / glitch_end;
            let mut rng = Rng::new((f * 97.0) as u32 + 1);
            let noise: Vec<char> = "▓▒░#@%&$".chars().collect();
            for k in 0..g.ch.len() {
                let x = rng.next();
                if x < q {
                    g.ch[k] = if q > 0.7 && x < q * 0.7 { ' ' } else { noise[(rng.next() * 8.0) as usize % 8] };
                    g.co[k] = if rng.next() < 0.5 { C::TDim } else { C::PDim };
                } else {
                    g.ch[k] = g0.ch[k];
                    g.co[k] = g0.co[k];
                }
            }
            return;
        }
        if f >= trans && ok {
            // Übergang ins Netz: Scanlinie baut die App auf, die Welle formt sich
            let mut g2 = Grid::new(rows, cols);
            self.draw_main(&mut g2);
            self.hits.clear();
            self.fields = [FieldPos::default(); 3];
            let rev = ((f - trans) * if fast { 4.5 } else { 2.4 }) as i32;
            g.copy_from(&g2, 0, rev, 0, cols - 1);
            if rev < rows - 1 {
                g.put(rev + 1, 0, &"═".repeat(cols as usize), C::TealB);
            }
            self.skip_button(g);
            return;
        }

        // Kopfzeile der Sequenz und rasender Paketmitschnitt an den Seiten
        g.put(1, 3, &format!("TIDESPEAK // {}", l("VERBINDUNGSAUFBAU", "ESTABLISHING CONNECTION")), C::TealB);
        g.put(1, cols - 16, &format!("T+{:.2} s", f * 0.04).replace('.', if tidespeak::i18n::is_german() { "," } else { "." }), C::Teal);
        g.put(2, 3, &"─".repeat((cols - 6) as usize), C::TDim);
        let hexw = if cols >= 190 { 31 } else { 19 };
        let bytes = if hexw > 20 { 8 } else { 4 };
        if f >= 8.0 {
            for r in 4..rows - 2 {
                if r as f32 > 4.0 + (f - 8.0) * 3.0 {
                    break;
                }
                for c0 in [2, cols - hexw - 2] {
                    let line_no = r + (f * 1.5) as i32 + if c0 > 100 { 50 } else { 0 };
                    let mut rng = Rng::new((line_no as u32).wrapping_mul(7919).wrapping_add(c0 as u32));
                    let mut s = format!("0x{:04x} ", (rng.next() * 65536.0) as u32);
                    for _ in 0..bytes {
                        s.push_str(&format!(" {:02x}", (rng.next() * 256.0) as u32));
                    }
                    g.put(r, c0, &s, if line_no % 11 == 0 { C::TDim } else { C::G });
                }
            }
        }
        let lx = hexw + 5;
        let rx = cols - hexw - 6;
        let pw = (rx - lx - 3) / 2;

        // 1 · Befehlszeile
        let cmd = format!("> tidespeak connect {addr} --name {name}");
        g.put(4, lx + 2, &format!("{}{}", typed(&cmd, f, if fast { 2.0 } else { 10.0 }, if fast { 6.0 } else { 2.5 }), cursor(f, 10.0, cmd.chars().count(), 2.5)), C::TealB);

        // 2 · Leitung zum Server (ohne erfundene Stationen – nur du, das Paket und das Ziel)
        if !fast && f >= 18.0 {
            g.panel(6, lx, 9, rx - lx + 1, C::TDim, false, l("LEITUNG", "ROUTE"), C::B);
            g.put(8, lx + 4, l("DU", "YOU"), C::B);
            let target = grid::cut(&addr, 40);
            g.right(8, rx - 4, &target, C::B);
            let p = if ok { clamp01((f - 22.0) / 26.0) } else { clamp01((f - 22.0) / 26.0).min(0.92) };
            let (x0, x1) = (lx + 4, rx - 4);
            let head = x0 + ((x1 - x0) as f32 * p) as i32;
            g.put(9, x0, &"─".repeat((head - x0).max(0) as usize), C::Teal);
            if p < 1.0 {
                g.put(9, head, "»", C::TealB);
            }
            g.set(9, x0, '◆', C::TealB);
            g.set(9, x1, '◆', if p >= 1.0 { C::TealB } else { C::G });
            g.put(11, lx + 4, &format!("{}  {}", l("ZIEL", "TARGET"), addr), C::D);
            let status = if ok {
                (l("ANTWORT ERHALTEN · VERBINDUNG STEHT", "REPLY RECEIVED · LINK ESTABLISHED").to_string(), C::Green)
            } else if fail.is_some() {
                (l("KEINE ANTWORT", "NO REPLY").to_string(), C::Red)
            } else {
                (format!("{} {}", l("WARTE AUF ANTWORT", "WAITING FOR REPLY"), ["|", "/", "-", "\\"][(f as usize) % 4]), C::Amber)
            };
            g.put(12, lx + 4, &status.0, status.1);
        }

        // 3 · Handshake
        let hs_row = if fast { 7 } else { 17 };
        if f >= if fast { 3.0 } else { 24.0 } {
            g.panel(hs_row, lx, 13, pw, C::TDim, false, "HANDSHAKE", C::B);
            let steps = [
                l("ADRESSE AUFLÖSEN", "RESOLVE ADDRESS").to_string(),
                l("QUIC-VERBINDUNG", "QUIC CONNECTION").to_string(),
                l("TLS 1.3 SCHLÜSSELTAUSCH", "TLS 1.3 KEY EXCHANGE").to_string(),
                "ALPN tidespeak/1".to_string(),
                lf(format!("PROTOKOLL v{PROTOCOL_VERSION}"), format!("PROTOCOL v{PROTOCOL_VERSION}")),
            ];
            for (i, st) in steps.iter().enumerate() {
                let (s0, len) = if fast { (3.0 + i as f32 * 2.6, 2.6) } else { (26.0 + i as f32 * 8.0, 8.0) };
                let mut q = clamp01((f - s0) / len);
                if !ok && i == 4 {
                    q = q.min(0.6);
                }
                let failed = fail.is_some() && q < 1.0 && q > 0.0;
                let k = (q * 12.0).round() as usize;
                let r = hs_row + 2 + i as i32 * 2;
                g.put(r, lx + 2, &format!("[0{}] {}", i + 1, grid::cut(st, (pw - 26).max(8) as usize)), if q > 0.0 { C::T } else { C::F });
                let bx = lx + pw - 21;
                g.segs(r, bx, &[("[", C::D), (&"█".repeat(k), if q >= 1.0 { C::Green } else if failed { C::Red } else { C::Amber }), (&"░".repeat(12 - k), C::G), ("]", C::D)]);
                let tag = if q >= 1.0 {
                    ("OK".to_string(), C::Green)
                } else if failed {
                    ("!!".to_string(), C::RedB)
                } else if q > 0.0 {
                    (format!("{} %", (q * 100.0) as i32), C::Amber)
                } else {
                    (String::new(), C::F)
                };
                g.put(r, bx + 16, &tag.0, tag.1);
            }
        }

        // Fehlschlag: roter Kasten mit dem Grund
        if let Some(reason) = &fail {
            let w = 80.min(cols - 10);
            let c0 = cx - w / 2;
            g.panel(rows / 2 - 4, c0, 9, w, C::RedB, true, l("VERBINDUNG FEHLGESCHLAGEN", "CONNECTION FAILED"), C::RedB);
            for (i, ln) in wrap(reason, (w - 6) as usize).iter().take(4).enumerate() {
                g.put(rows / 2 - 2 + i as i32, c0 + 3, ln, C::T);
            }
            self.skip_button(g);
            return;
        }

        // 4 · Schlüssel: der echte Fingerabdruck rastet Zeichen für Zeichen ein
        if !fast && f >= gate {
            let kx = lx + pw + 3;
            g.panel(17, kx, 13, pw, C::TDim, false, l("SCHLÜSSEL", "KEY"), C::B);
            g.put(19, kx + 3, l("SERVER-FINGERABDRUCK · SHA-256", "SERVER FINGERPRINT · SHA-256"), C::D);
            let fp: Vec<char> = self.fingerprint.chars().filter(|c| c.is_ascii_hexdigit()).take(32).map(|c| c.to_ascii_uppercase()).collect();
            let mut rng = Rng::new((f * 31.0) as u32 + 3);
            let hex: Vec<char> = "0123456789ABCDEF".chars().collect();
            for (j, ch) in fp.iter().enumerate() {
                let lock = f >= gate + 2.0 + j as f32 * 1.0;
                let c = if lock { *ch } else { hex[(rng.next() * 16.0) as usize % 16] };
                let row = 21 + (j / 16) as i32;
                let col = kx + 4 + (j % 16) as i32 * 2 + ((j % 16) / 4) as i32 * 2;
                g.set(row, col, c, if lock { C::TealB } else { C::TDim });
            }
            if f >= gate + 36.0 {
                if self.new_server {
                    g.put(25, kx + 3, l("» NEUER SERVER – ABDRUCK GESPEICHERT", "» NEW SERVER – FINGERPRINT SAVED"), C::Green);
                    g.put(27, kx + 3, l("ÄNDERT ER SICH, SCHLÄGT TIDESPEAK ALARM", "IF IT EVER CHANGES, TIDESPEAK RAISES AN ALERT"), C::F);
                } else {
                    g.put(25, kx + 3, l("» BEKANNTER SERVER – ABDRUCK STIMMT", "» KNOWN SERVER – FINGERPRINT MATCHES"), C::Green);
                }
            }
        }

        // 5 · Anmeldung
        if !fast && f >= 98.0 {
            g.panel(32, cx - 40, 11, 81, C::TDim, false, l("ANMELDUNG", "SIGN-IN"), C::B);
            let me = self.users.get(&self.you).map(|u| u.name.clone()).unwrap_or(name.clone());
            let fields = [
                (l("KENNUNG", "IDENT"), me.to_uppercase()),
                ("SERVER", self.server_name.to_uppercase()),
                (l("KANAL", "CHANNEL"), self.channel_name(self.current).to_uppercase()),
                (l("ZUGANG", "ACCESS"), l("OHNE KONTO · VERSCHLÜSSELT", "NO ACCOUNT · ENCRYPTED").to_string()),
            ];
            for (i, (k, v)) in fields.iter().enumerate() {
                let f0 = 100.0 + i as f32 * 5.0;
                if f < f0 {
                    continue;
                }
                let r = 34 + i as i32;
                g.put(r, cx - 36, k, C::D);
                g.put(r, cx - 26, &".".repeat(20), C::G);
                g.put(r, cx - 4, &format!("{}{}", typed(&grid::cut(v, 40), f, f0, 4.0), cursor(f, f0, v.chars().count(), 4.0)), C::B);
            }
            if f >= 118.0 {
                let done = f >= 126.0;
                let s = if done {
                    format!("[ OK ] {}", l("AUTHENTIFIZIERT", "AUTHENTICATED"))
                } else {
                    format!("[ {} ] {} …", ["|", "/", "-", "\\"][(f as usize) % 4], l("AUTHENTIFIZIERE", "AUTHENTICATING"))
                };
                g.put(40, cx - 36, &s, if done { C::Green } else { C::Amber });
            }
        }

        // 6 · Zugang gewährt
        let b0 = if fast { 16.0 } else { 126.0 };
        if ok && f >= b0 {
            if f >= b0 + 12.0 {
                g.recolor(C::G, 0, rows - 1, 0, cols - 1);
            }
            let flash = f < b0 + 10.0 && ((f / 2.0) as i32) % 2 == 0;
            let co = if flash { C::Green } else { C::TealB };
            let w = 108.min(cols - 8);
            let c0 = cx - w / 2;
            let r0 = rows / 2 - 6;
            g.clear(r0, c0, 11, w);
            g.frame(r0, c0, 11, w, co, true);
            g.frame(r0 + 1, c0 + 2, 9, w - 4, if f >= b0 + 12.0 { C::G } else { C::TDim }, false);
            g.center(r0 + 4, cx, l("Z U G A N G   G E W Ä H R T", "A C C E S S   G R A N T E D"), co);
            let n = self.users.len();
            g.center(
                r0 + 6,
                cx,
                &lf(
                    format!("WILLKOMMEN IN {} · {n} KNOTEN ONLINE", self.server_name.to_uppercase()),
                    format!("WELCOME TO {} · {n} NODES ONLINE", self.server_name.to_uppercase()),
                ),
                if f >= b0 + 12.0 { C::D } else { C::B },
            );
            g.center(r0 + 8, cx, &typed(&format!("{} · {}", l("VERSCHLÜSSELT ÜBER QUIC · TLS 1.3", "ENCRYPTED VIA QUIC · TLS 1.3"), addr.to_uppercase()), f, b0 + 2.0, 4.0), C::Teal);
        }
        self.skip_button(g);
    }

    // ── Verbindung trennen

    pub fn draw_disconnect(&mut self, g: &mut Grid, f: f32, reason: Option<String>) {
        let (rows, cols) = (self.rows, self.cols);
        let lost = reason.is_some();
        if f < 110.0 && !self.channels.is_empty() {
            self.draw_main(g);
            self.hits.clear();
            self.fields = [FieldPos::default(); 3];
            let nw = self.cols - self.chat_width();
            // Netz erlischt, Welle wird flach
            if f >= 6.0 {
                g.recolor(C::G, 3, rows - 1, 0, nw - 1);
            }
            let lay = self.layout();
            g.clear(lay.server.0 - 7, lay.server.1 - 16, 14, 33);
            tide(g, lay.server.0 as f32, lay.server.1 as f32, 3.0, TIDE_T + self.wave + f * 0.12, &TideOpts { amp: 1.0 - ease(f / 50.0), ..Default::default() });
            g.clear(1, 30, 1, cols - 80);
            let blink = f < 12.0 && (f as i32) % 4 < 2;
            let head = if lost { l("VERBINDUNG VERLOREN", "CONNECTION LOST") } else { l("TRENNE VERBINDUNG", "DISCONNECTING") };
            let secs = self.session_start.elapsed().as_secs();
            g.segs(
                1,
                30,
                &[
                    (&format!("{head} · {}", grid::cut(&self.server_name.to_uppercase(), 30)), if blink { C::RedB } else { C::AmberB }),
                    (&format!("   {} {:02}:{:02}:{:02}", l("SITZUNG", "SESSION"), secs / 3600, secs / 60 % 60, secs % 60), C::D),
                ],
            );
            if blink {
                g.frame(0, 0, rows, cols, C::RedB, true);
            }
            let pw = ((nw - 18) / 2).min(56);
            // Ablauf
            if f >= 6.0 {
                g.panel(6, 8, 13, pw, C::Amber, false, l("SITZUNG BEENDEN", "END SESSION"), C::B);
                let ch = self.channel_name(self.current).to_uppercase();
                let steps = [
                    lf(format!("KANAL {} VERLASSEN", grid::cut(&ch, 14)), format!("LEAVE CHANNEL {}", grid::cut(&ch, 14))),
                    l("AUDIO-STREAMS STOPPEN", "STOP AUDIO STREAMS").to_string(),
                    l("QUIC-VERBINDUNG SCHLIESSEN", "CLOSE QUIC CONNECTION").to_string(),
                    l("SITZUNGSSCHLÜSSEL VERWERFEN", "DISCARD SESSION KEYS").to_string(),
                    l("SITZUNGSDATEN LEEREN", "CLEAR SESSION DATA").to_string(),
                ];
                for (i, st) in steps.iter().enumerate() {
                    let f0 = 8.0 + i as f32 * 9.0;
                    if f < f0 {
                        continue;
                    }
                    let ok = f >= f0 + 6.0;
                    let mark = if ok { "[ OK ] ".to_string() } else { format!("[ {}  ] ", ["|", "/", "-", "\\"][(f as usize) % 4]) };
                    g.segs(8 + i as i32 * 2, 11, &[(&mark, if ok { C::Green } else { C::Amber }), (&format!("0{}  {}", i + 1, typed(st, f, f0, 3.0)), C::T)]);
                }
            }
            // Schlüsselmaterial wird Zeichen für Zeichen vernichtet
            if f >= 10.0 {
                let kx = 8 + pw + 3;
                g.panel(6, kx, 13, pw, C::Amber, false, l("SITZUNG", "SESSION"), C::B);
                let mut rng = Rng::new(77);
                let hex: Vec<char> = "0123456789ABCDEF".chars().collect();
                for j in 0..64 {
                    let ch = hex[(rng.next() * 16.0) as usize % 16];
                    let age = f - 30.0 - j as f32 * 0.45;
                    let (c, co) = if age < 0.0 {
                        (ch, C::TealB)
                    } else if age < 2.0 {
                        ('█', C::RedB)
                    } else if age < 4.0 {
                        ('▓', C::Red)
                    } else if age < 6.0 {
                        ('░', C::RDim)
                    } else {
                        (' ', C::G)
                    };
                    let col = kx + 4 + (j % 16) * 3 + ((j % 16) / 4) * 2;
                    if col < kx + pw - 2 {
                        g.set(9 + j / 16, col, c, co);
                    }
                }
                if f >= 66.0 {
                    g.put(15, kx + 3, l("» SITZUNGSSCHLÜSSEL VERWORFEN", "» SESSION KEYS DISCARDED"), C::RedB);
                }
            }
            // Die Leitung zieht sich zurück
            if f >= 14.0 {
                let w = nw - 16;
                g.panel(21, 8, 7, w, C::Amber, false, l("LEITUNG", "ROUTE"), C::B);
                g.put(23, 12, l("DU", "YOU"), C::B);
                g.right(23, 8 + w - 5, "SERVER", if f >= 50.0 { C::F } else { C::B });
                let p = clamp01((f - 22.0) / 28.0);
                let (x0, x1) = (12, 8 + w - 5);
                let head = x1 - ((x1 - x0) as f32 * p) as i32;
                g.put(24, x0, &"─".repeat((head - x0).max(0) as usize), C::Teal);
                if p < 1.0 {
                    g.put(24, head, "«", C::RedB);
                }
                g.set(24, x0, '◆', C::TealB);
                g.set(24, x1, '◆', if head >= x1 { C::TealB } else { C::G });
                if f >= 52.0 {
                    g.put(26, 12, l("LEITUNG GESCHLOSSEN", "ROUTE CLOSED"), C::Green);
                }
            }
            // Großes Banner
            if f >= 70.0 {
                let flash = f < 80.0 && ((f / 2.0) as i32) % 2 == 0;
                let w = (nw - 30).min(104);
                let c0 = (nw - w) / 2;
                g.clear(31, c0, 9, w);
                g.frame(31, c0, 9, w, if flash { C::AmberB } else { C::RedB }, true);
                let title = if lost { l("V E R B I N D U N G   V E R L O R E N", "C O N N E C T I O N   L O S T") } else { l("V E R B I N D U N G   G E T R E N N T", "D I S C O N N E C T E D") };
                g.center(34, c0 + w / 2, title, if flash { C::AmberB } else { C::RedB });
                let sub = match &reason {
                    Some(r) => grid::cut(r, (w - 6) as usize),
                    None => lf(
                        format!("SITZUNG {:02}:{:02}:{:02} · ALLE SITZUNGSSCHLÜSSEL VERWORFEN", secs / 3600, secs / 60 % 60, secs % 60),
                        format!("SESSION {:02}:{:02}:{:02} · ALL SESSION KEYS DISCARDED", secs / 3600, secs / 60 % 60, secs % 60),
                    ),
                };
                g.center(36, c0 + w / 2, &typed(&sub, f, 72.0, 4.0), C::D);
            }
        }
        // Wischen zurück zum Verbinden-Bildschirm
        if f >= 92.0 || self.channels.is_empty() {
            let mut c = Grid::new(rows, cols);
            self.connect_screen(&mut c, false, true);
            let edge = if self.channels.is_empty() { -3 } else { (cols - 1 - ((f - 92.0) * 12.0) as i32).max(-3) };
            for r in 0..rows {
                for x in (edge + 1).max(0)..cols {
                    let k = (r * cols + x) as usize;
                    g.ch[k] = c.ch[k];
                    g.co[k] = c.co[k];
                    g.bg[k] = c.bg[k];
                }
                if edge >= 0 {
                    g.set(r, edge, '▓', C::RedB);
                    g.set(r, edge - 1, '▒', C::Red);
                    g.set(r, edge - 2, '░', C::RDim);
                }
            }
        }
        self.skip_button(g);
    }

    // ── App beenden

    pub fn draw_quit(&mut self, g: &mut Grid, f: f32) {
        let (rows, cols) = (self.rows, self.cols);
        let (cr, cc) = (rows / 2, cols / 2);
        if f < 30.0 {
            let mut base = Grid::new(rows, cols);
            if self.conn == Conn::Connected || !self.channels.is_empty() {
                self.draw_main(&mut base);
            } else {
                self.connect_screen(&mut base, false, false);
                let t = TIDE_T + self.wave + 0.03 * f * f;
                tide(&mut base, 10.0, cc as f32, 2.6, t, &TideOpts { amp: 1.0 - ease(f / 12.0), yaw: 0.45 + 0.01 * f * f, ..Default::default() });
            }
            self.hits.clear();
            self.fields = [FieldPos::default(); 3];
            let sweep = rows - 1 - (clamp01(f / 9.0) * (rows - 4) as f32) as i32;
            base.recolor(C::G, sweep, rows - 1, 0, cols - 1);
            if sweep < rows - 1 {
                base.put(sweep, 1, &"─".repeat((cols - 2) as usize), C::TDim);
            }
            if f < 10.0 {
                g.copy_from(&base, 0, rows - 1, 0, cols - 1);
            } else {
                // Röhrenbildschirm: alles staucht sich zu einer Linie
                let s = 1.0 - ease((f - 10.0) / 12.0);
                for r in 0..rows {
                    let dr = (r - cr) as f32;
                    if dr.abs() > rows as f32 / 2.0 * s + 0.5 {
                        continue;
                    }
                    let src = (cr as f32 + dr / s.max(0.02)).round() as i32;
                    if src < 0 || src >= rows {
                        continue;
                    }
                    for c in 0..cols {
                        let k = (src * cols + c) as usize;
                        if base.ch[k] == ' ' {
                            continue;
                        }
                        g.set(r, c, base.ch[k], if s < 0.3 { C::TealB } else { base.co[k] });
                    }
                }
                if f >= 20.0 {
                    let half = (cols as f32 / 2.0 * (1.0 - ease((f - 20.0) / 9.0))).round() as i32;
                    g.clear(cr, 0, 1, cols);
                    if half > 0 {
                        g.put(cr, cc - half, &"━".repeat((half * 2) as usize), C::TealB);
                    }
                }
            }
        } else {
            if f < 33.0 {
                g.set(cr, cc, '●', C::TealB);
            }
            if (31.0..46.0).contains(&f) {
                let q = (f - 31.0) / 15.0;
                let sc = 1.6 * (q * 3.14).sin();
                if sc > 0.15 {
                    tide(g, cr as f32, cc as f32, sc * 1.6, TIDE_T + q * 8.0, &TideOpts { yaw: 0.45 + q * 4.0, amp: (q * 3.14).sin() * 1.4, ..Default::default() });
                }
            }
            if f >= 36.0 {
                let txt = if self.conn == Conn::Offline && self.channels.is_empty() { l("BIS BALD", "SEE YOU SOON") } else { l("VERBINDUNG GETRENNT · BIS BALD", "DISCONNECTED · SEE YOU SOON") };
                let n = (txt.chars().count() as f32 * clamp01((f - 36.0) / 8.0)) as usize;
                let co = if f < 50.0 { C::TealB } else if f < 55.0 { C::D } else { C::F };
                let s: String = txt.chars().take(n).collect();
                g.center(cr + 6, cc, &format!("{s}{}", if n < txt.chars().count() { "█" } else { "" }), co);
            }
        }
    }
}
