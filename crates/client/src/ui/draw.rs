//! Drawing of the main screen: header, network, own panel, dock, radio log,
//! person card and all effects inside the network.

use std::sync::atomic::Ordering;
use std::time::Instant;

use tidespeak::grid::{self, decode, ease, tide, wrap, Bg, Grid, Rng, TideOpts, C, TIDE_T};
use tidespeak::i18n::{l, lf};
use tidespeak_proto::UserInfo;

use super::*;

pub struct ChPos {
    pub id: u32,
    pub r: i32,
    pub c: i32,
    pub w: i32,
    pub cur: bool,
}

pub struct UPos {
    pub id: u32,
    pub r: i32,
    pub c: i32,
    pub cr: i32,
    pub cc: i32,
    pub cur: bool,
}

pub struct NetLayout {
    pub server: (i32, i32),
    pub ch: Vec<ChPos>,
    pub users: Vec<UPos>,
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    None,
    Speak,
    Mute,
    Deaf,
    Away,
}

impl App {
    /// Builds the whole picture for the current scene
    pub fn draw(&mut self, rows: i32, cols: i32) {
        self.rows = rows.max(30);
        self.cols = cols.max(100);
        let mut g = Grid::new(self.rows, self.cols);
        self.hits.clear();
        self.fields = [FieldPos::default(); 3];
        match &self.scene {
            Scene::Boot { f } => {
                let f = *f;
                self.draw_boot(&mut g, f);
            }
            Scene::Connect => self.draw_connect(&mut g),
            Scene::Sequence { f, fast, ok, fail } => {
                let (f, fast, ok, fail) = (*f, *fast, *ok, fail.clone());
                self.draw_sequence(&mut g, f, fast, ok, fail);
            }
            Scene::Main => self.draw_main(&mut g),
            Scene::Disconnect { f, reason } => {
                let (f, reason) = (*f, reason.clone());
                self.draw_disconnect(&mut g, f, reason);
            }
            Scene::Quit { f } => {
                let f = *f;
                self.draw_quit(&mut g, f);
            }
        }
        if self.drawer.is_some() && matches!(self.scene, Scene::Connect | Scene::Main) {
            self.draw_drawer(&mut g);
        }
        // Fenster-Knöpfe sind immer bedienbar
        if self.rows > 0 {
            let x = self.cols - 14;
            self.hits.push(Hit { r: 0, c: x, h: 3, w: 3, act: Act::Minimize });
            self.hits.push(Hit { r: 0, c: x + 4, h: 3, w: 3, act: Act::Maximize });
            self.hits.push(Hit { r: 0, c: x + 8, h: 3, w: 3, act: Act::Close });
        }
        if let Some(i) = self.hover {
            if let Some(h) = self.hits.get(i) {
                if !matches!(h.act, Act::Swallow | Act::Field(_) | Act::Slider(_)) {
                    g.recolor(C::TealB, h.r, h.r + h.h - 1, h.c, h.c + h.w - 1);
                }
            }
        }
        self.grid = g;
        self.dirty = false;
    }

    // ── Kopfleiste

    pub fn mini_wave(g: &mut Grid, r: i32, c: i32) {
        g.segs(r, c, &[(".", C::Teal), ("-", C::Teal), ("~", C::TealB), ("*", C::Lav), ("~", C::Purple), ("-", C::Purple)]);
    }

    pub fn header(&mut self, g: &mut Grid, status: Option<(&str, C)>) {
        let cols = self.cols;
        g.frame(0, 0, 3, cols, C::TDim, true);
        Self::mini_wave(g, 1, 2);
        g.put(1, 9, "T I D E S P E A K", C::TealB);
        g.put(1, 28, "│", C::F);
        let text = match status {
            Some((s, co)) => (s.to_string(), co),
            None if self.conn == Conn::Connected => {
                let n = self.users.len();
                (
                    lf(
                        format!("NETZ · {} · {n} KNOTEN", grid::cut(&self.server_name.to_uppercase(), 40)),
                        format!("NETWORK · {} · {n} NODES", grid::cut(&self.server_name.to_uppercase(), 40)),
                    ),
                    C::D,
                )
            }
            None => ("OFFLINE".into(), C::D),
        };
        g.put(1, 30, &text.0, text.1);
        if self.conn == Conn::Connected && status.is_none() {
            let ping = if self.ping > 0 { format!("{} ms", self.ping) } else { "–".into() };
            let co = if self.ping < 80 { C::Green } else if self.ping < 150 { C::Amber } else { C::Red };
            let (delay, _) = self.audio.shared.delay_estimate(self.ping);
            let dco = if delay < 200 { C::Green } else if delay < 400 { C::Amber } else { C::Red };
            g.segs(
                1,
                cols - 78,
                &[("PING ", C::D), (&ping, co), (&format!(" · {} ", l("VERZÖGERUNG", "DELAY")), C::D), (&format!("~{delay} ms"), dco), (" · TLS 1.3", C::D)],
            );
        }
        let set = format!("[ {} ]", l("EINSTELLUNGEN", "SETTINGS"));
        let sx = cols - 17 - set.chars().count() as i32;
        g.put(1, sx, &set, if self.drawer.is_some() { C::TealB } else { C::T });
        if matches!(self.scene, Scene::Connect | Scene::Main) {
            self.hits.push(Hit { r: 0, c: sx, h: 3, w: set.chars().count() as i32, act: Act::OpenSettings });
        }
        g.put(1, cols - 14, "[_] [=] [x]", C::D);
    }

    // ── Netz

    fn net_area(&self) -> (i32, f32, f32, f32, f32) {
        let nw = self.cols - self.chat_width();
        let sx = (nw as f32 / 137.0).clamp(0.8, 1.8);
        let sy = ((self.rows - 5) as f32 / 51.0).clamp(0.72, 1.6);
        let sr = 3.0 + ((self.rows - 3) as f32 * 0.4).round();
        let sc = (nw as f32 * 0.51).round();
        (nw, sx, sy, sr, sc)
    }

    /// Channels with their people: others alphabetically, you last
    fn members(&self, ch: u32) -> Vec<&UserInfo> {
        let mut v: Vec<&UserInfo> = self.users.values().filter(|u| u.channel == ch).collect();
        v.sort_by_key(|u| (u.id == self.you, u.name.to_lowercase()));
        v
    }

    pub fn layout(&self) -> NetLayout {
        let (_, sx, sy, sr, sc) = self.net_area();
        let d = std::f32::consts::PI / 180.0;
        let cur_idx = self.channels.iter().position(|c| c.id == self.current).unwrap_or(0);
        let others: Vec<usize> = (0..self.channels.len()).filter(|i| *i != cur_idx).collect();
        let n_o = others.len();
        let mut lay = NetLayout { server: (sr as i32, sc as i32), ch: Vec::new(), users: Vec::new() };
        for (i, ch) in self.channels.iter().enumerate() {
            let cur = i == cur_idx;
            let (r, c, dir) = if cur {
                (sr, sc - 32.0 * sx, 180.0f32)
            } else {
                let j = others.iter().position(|x| *x == i).unwrap_or(0);
                let a = if n_o <= 1 { 0.0 } else { 55.0 - j as f32 * 110.0 / (n_o - 1) as f32 };
                (sr - 15.0 * sy * (a * d).sin(), sc + 34.0 * sx * (a * d).cos(), a)
            };
            let (r, c) = (r.round(), c.round());
            lay.ch.push(ChPos { id: ch.id, r: r as i32, c: c as i32, w: if cur { 24 } else { 18 }, cur });
            let users = self.members(ch.id);
            let n = users.len();
            let spread = if cur { (36.0 * (n as f32 - 1.0)).min(144.0) } else { (40.0 * (n as f32 - 1.0)).min(100.0) };
            let (rr, rc) = if cur { (13.0 * sy, 26.0 * sx) } else { (7.0 * sy, 18.0 * sx) };
            for (k, u) in users.iter().enumerate() {
                let off = if n > 1 { -spread / 2.0 + spread * k as f32 / (n - 1) as f32 } else { 0.0 };
                let a = (dir + off) * d;
                lay.users.push(UPos {
                    id: u.id,
                    r: (r - rr * a.sin()).round() as i32,
                    c: (c + rc * a.cos()).round() as i32,
                    cr: r as i32,
                    cc: c as i32,
                    cur,
                });
            }
        }
        lay
    }

    fn state_of(&self, u: &UserInfo, cur: bool) -> St {
        if u.deafened {
            St::Deaf
        } else if u.muted {
            St::Mute
        } else if cur && self.speaking.contains(&u.id) {
            St::Speak
        } else if u.away {
            St::Away
        } else {
            St::None
        }
    }

    /// Draws the network; with `ghost` everything is barely visible (snapshot for transitions)
    pub fn draw_net(&self, g: &mut Grid, ghost: bool, flash: Option<(u32, C)>) -> NetLayout {
        let lay = self.layout();
        let k = |c: C| if ghost { C::G } else { c };
        let (sr, sc) = (lay.server.0 as f32, lay.server.1 as f32);
        for ch in &lay.ch {
            g.line(sr, sc, ch.r as f32, ch.c as f32, k(if ch.cur { C::LCur } else { C::LOth }), None, 1.0);
        }
        for p in &lay.users {
            let Some(u) = self.users.get(&p.id) else { continue };
            let st = self.state_of(u, p.cur);
            let co = match st {
                St::Speak => C::TealB,
                St::Mute | St::Deaf => C::LMute,
                _ if p.cur => C::LUser,
                _ => C::LFar,
            };
            g.line(p.cr as f32, p.cc as f32, p.r as f32, p.c as f32, k(co), None, 1.0);
        }
        let (_, _, sy, _, _) = self.net_area();
        let t = if self.cfg.anim_wave { TIDE_T + self.wave } else { TIDE_T };
        tide(g, sr, sc, 3.0, t, &TideOpts { ghost, ..Default::default() });
        let lr = lay.server.0 + (9.0 * sy.max(0.85)).round() as i32;
        g.center(lr, lay.server.1, " SERVER ", k(C::Purple));
        g.center(lr + 1, lay.server.1, &format!(" {} ", grid::cut(&self.server_name.to_uppercase(), 24)), k(C::B));
        for ch in &lay.ch {
            let n = self.users.values().filter(|u| u.channel == ch.id).count();
            let name = grid::cut(&self.channel_name(ch.id).to_uppercase(), (ch.w - 6) as usize);
            let code = self.channel_code(ch.id);
            let count = if n > 0 { format!("{n} ONLINE") } else { l("FREI", "EMPTY").into() };
            g.hex(
                ch.r,
                ch.c,
                ch.w,
                k(if ch.cur { C::TealB } else { C::T }),
                &[(&code, k(if ch.cur { C::Teal } else { C::D })), (&name, k(C::B)), (&count, k(if ch.cur { C::Teal } else { C::D }))],
            );
            if !ch.cur {
                g.center(ch.r + 3, ch.c, &format!(" [ {} ] ", l("BEITRETEN", "JOIN")), k(C::Teal));
            }
        }
        for p in &lay.users {
            let fl = flash.filter(|f| f.0 == p.id).map(|f| f.1);
            self.node(g, p, ghost, fl);
        }
        lay
    }

    fn node(&self, g: &mut Grid, p: &UPos, ghost: bool, flash: Option<C>) {
        let Some(u) = self.users.get(&p.id) else { return };
        let k = |c: C| if ghost { C::G } else { c };
        let st = self.state_of(u, p.cur);
        let me = u.id == self.you;
        let a0 = p.c - 2;
        g.clear(p.r - 1, a0 - 1, 3, 7);
        let mut co = k(match st {
            St::Speak => C::TealB,
            St::Mute | St::Deaf => C::Red,
            St::Away => C::F,
            St::None => user_color(&u.name),
        });
        if let Some(f) = flash {
            co = f;
        }
        let initial = u.name.chars().next().unwrap_or('?').to_uppercase().next().unwrap_or('?');
        let (top, mid, bot) = if st == St::Speak { ("╔═══╗", ('║', '║'), "╚═══╝") } else { ("┌───┐", ('│', '│'), "└───┘") };
        g.put(p.r - 1, a0, top, co);
        g.put(p.r, a0, &format!("{} {} {}", mid.0, initial, mid.1), co);
        g.put(p.r + 1, a0, bot, co);
        let name = if me { format!("{} {}", grid::cut(&u.name, 14), l("(du)", "(you)")) } else { grid::cut(&u.name, 16) };
        let weak = self.quality.get(&u.id).copied().unwrap_or(0) == 2;
        let status: &str = match st {
            St::Speak => l("SPRICHT", "SPEAKING"),
            St::Mute => l("STUMM", "MUTED"),
            St::Deaf => l("TON AUS", "DEAFENED"),
            St::Away => l("ABWESEND", "AWAY"),
            St::None if weak && p.cur => l("SCHWACH", "WEAK LINK"),
            St::None if me => l("DU", "YOU"),
            St::None => "",
        };
        let name_co = k(if st == St::Away { C::D } else if p.cur { C::B } else { C::T });
        let st_co = k(match st {
            St::Speak => C::TealB,
            St::Mute | St::Deaf => C::Red,
            St::Away => C::Amber,
            St::None if weak => C::Amber,
            St::None => C::D,
        });
        let scope = (st == St::Speak && !ghost).then(|| scope(self.phase, u.name.len() as f32));
        let nl = name.chars().count() as i32;
        let sl = status.chars().count() as i32;
        if p.c < p.cc {
            let end = a0 - 2;
            g.put(p.r, end - nl, &format!(" {name}"), name_co);
            if !status.is_empty() {
                g.put(p.r + 1, end - sl, &format!(" {status}"), st_co);
            }
            if let Some(s) = scope {
                g.put(p.r + 2, end - 7, &s, C::Teal);
            }
        } else {
            let s0 = a0 + 6;
            g.put(p.r, s0, &format!("{name} "), name_co);
            if !status.is_empty() {
                g.put(p.r + 1, s0, &format!("{status} "), st_co);
            }
            if let Some(s) = scope {
                g.put(p.r + 1, s0 + sl + 2, &s, C::Teal);
            }
        }
    }

    fn user_hit(&self, p: &UPos) -> Hit {
        let n = self.users.get(&p.id).map(|u| u.name.chars().count().min(16) as i32).unwrap_or(4);
        let a0 = p.c - 2;
        if p.c < p.cc {
            Hit { r: p.r - 1, c: a0 - 3 - n, h: 3, w: n + 8, act: Act::User(p.id) }
        } else {
            Hit { r: p.r - 1, c: a0, h: 3, w: n + 7, act: Act::User(p.id) }
        }
    }

    // ── Hauptbildschirm

    pub fn draw_main(&mut self, g: &mut Grid) {
        let now = Instant::now();
        self.header(g, None);
        let (nw, _, sy, _, _) = self.net_area();
        let rows = self.rows;
        g.frame(3, 0, rows - 3, nw, C::G, false);
        g.put(3, 2, &format!("[ {} ]", l("NETZ", "NETWORK")), C::D);
        g.put(4, 2, l("KANAL ANKLICKEN = BEITRETEN · PERSON ANKLICKEN = OPTIONEN", "CLICK A CHANNEL = JOIN · CLICK A PERSON = OPTIONS"), C::F);

        // Anstupsen/Erwähnung lässt Knoten aufblitzen
        let blink = |p: f32| if ((p * 10.0) as i32) % 2 == 0 { C::B } else { C::TealB };
        let mut flash = None;
        for f in &self.fx {
            match f.kind {
                FxKind::Poke(id) => {
                    let p = f.p(now);
                    if p >= 0.65 {
                        flash = Some((id, blink(p)));
                    }
                }
                FxKind::Dock(k) if k <= DOCK_AWAY => flash = Some((self.you, blink(f.p(now)))),
                _ => {}
            }
        }
        if flash.is_none() {
            if let Some(id) = self.card {
                flash = Some((id, C::TealB));
            }
        }

        // Netz, ggf. mit Kanalwechsel-Übergang
        let switch = self.fx.iter().find(|f| f.kind == FxKind::Switch).map(|f| (f.p(now), f.pos, f.text.clone(), f.text2.clone()));
        let lay = if let Some((p, pos, name, code)) = switch {
            let old = self.fx.iter().find(|f| f.kind == FxKind::Switch).and_then(|f| f.grid.clone());
            let mut ng = Grid::new(rows, self.cols);
            let lay = self.draw_net(&mut ng, false, flash);
            let spin = 4.0 * std::f32::consts::PI * (1.0 - (1.0 - p) * (1.0 - p));
            if p < 0.55 {
                if let Some(o) = &old {
                    g.overlay(o, 4, rows - 2, 1, nw - 2, 0);
                }
                let q = p / 0.55;
                let e = ease(q);
                let target = lay.ch.iter().find(|c| c.cur).map(|c| (c.r as f32, c.c as f32)).unwrap_or(pos);
                grid::prism(g, pos.0 + (target.0 - pos.0) * e, pos.1 + (target.1 - pos.1) * e, 2.5 + 3.5 * e, 0.35 + 0.35 * (q * 3.14).sin(), q * 6.283, C::TealB, C::Teal);
                g.center(rows - 12, (nw as f32 * 0.51) as i32, &format!(" » {} · {code} {} ", l("KANALWECHSEL", "CHANNEL SWITCH"), name.to_uppercase()), C::TealB);
            } else {
                let rev = 4 + (((p - 0.55) / 0.45) * (rows - 6) as f32) as i32;
                g.copy_from(&ng, 4, rev, 1, nw - 2);
                if let Some(o) = &old {
                    g.overlay(o, rev + 1, rows - 2, 1, nw - 2, 0);
                }
                if rev < rows - 2 {
                    g.put(rev, 1, &"═".repeat((nw - 2) as usize), C::TealB);
                }
            }
            let (sr, sc) = (lay.server.0 as f32, lay.server.1 as f32);
            g.clear(lay.server.0 - 7, lay.server.1 - 16, 14, 33);
            tide(g, sr, sc, 3.0, TIDE_T + self.wave + spin, &TideOpts { amp: 1.0 + 0.9 * (p * 3.14).sin(), yaw: 0.45 + spin * 0.25, ..Default::default() });
            lay
        } else {
            self.draw_net(g, false, flash)
        };

        // Treffer im Netz
        for ch in &lay.ch {
            if !ch.cur {
                self.hits.push(Hit { r: ch.r - 2, c: ch.c - ch.w / 2, h: 6, w: ch.w, act: Act::Join(ch.id) });
            }
        }
        for p in &lay.users {
            if p.id != self.you {
                let h = self.user_hit(p);
                self.hits.push(h);
            }
        }

        // Zielerfassung beim Beitreten
        for f in &self.fx {
            if let FxKind::Lock(id) = f.kind {
                if let Some(p) = lay.users.iter().find(|u| u.id == id) {
                    let q = (f.p(now) * 4.0).min(1.0);
                    let d = (6.0 * (1.0 - ease(q))).round() as i32;
                    let a0 = p.c - 2;
                    let co = if q < 1.0 { C::TealB } else { C::Teal };
                    let (t, b, lft, rgt) = (p.r - 2 - d / 2, p.r + 2 + d / 2, a0 - 2 - d, a0 + 6 + d);
                    g.put(t, lft, "┌─", co);
                    g.put(t, rgt - 1, "─┐", co);
                    g.put(b, lft, "└─", co);
                    g.put(b, rgt - 1, "─┘", co);
                    if q > 0.4 {
                        let txt = l("NEUER KONTAKT ERFASST", "NEW CONTACT ACQUIRED");
                        let s = if f.p(now) < 0.45 { decode(txt, (f.p(now) * 4.0 - 0.4) / 0.6, 5) } else { txt.to_string() };
                        g.put(p.r + 3, p.c - 10, &s, C::TealB);
                    }
                }
            }
        }
        // Zerfall beim Verlassen
        for f in &self.fx {
            if f.kind != FxKind::Leave {
                continue;
            }
            let Some(node) = &f.grid else { continue };
            let t = f.p(now);
            let k = t * 16.0;
            let mut rng = Rng::new(5);
            let (nr, nc) = (f.pos.0 as i32, f.pos.1 as i32);
            for r in nr - 1..=nr + 2 {
                for c in nc - 16..=nc + 16 {
                    let ch = node.get(r, c);
                    if ch == ' ' {
                        continue;
                    }
                    let vx = (rng.next() - 0.5) * 1.2;
                    let vy = -0.5 - rng.next() * 0.6;
                    let life = 0.5 + rng.next() * 0.5;
                    if t > life {
                        continue;
                    }
                    let pr = r as f32 + vy * k + 0.045 * k * k;
                    let pc = c as f32 + vx * k;
                    if pr < 4.0 || pr > (rows - 2) as f32 || pc < 1.0 || pc > (nw - 2) as f32 {
                        continue;
                    }
                    let q = t / life;
                    let sym = if q < 0.25 { ch } else if q < 0.55 { '*' } else if q < 0.8 { ':' } else { '.' };
                    let co = if q < 0.3 { C::TealB } else if q < 0.6 { C::Teal } else { C::PDim };
                    g.set(pr.round() as i32, pc.round() as i32, sym, co);
                }
            }
        }
        // Anstupsen: Paket fliegt von dir zur Person
        for f in &self.fx {
            if let FxKind::Poke(id) = f.kind {
                let p = f.p(now);
                if p < 0.65 {
                    if let (Some(me), Some(to)) = (lay.users.iter().find(|u| u.id == self.you), lay.users.iter().find(|u| u.id == id)) {
                        let e = ease(p / 0.65);
                        for k in (0..4).rev() {
                            let q = (e - k as f32 * 0.04).max(0.0);
                            let (r, c) = (me.r as f32 + (to.r - me.r) as f32 * q, me.c as f32 + (to.c - me.c) as f32 * q);
                            g.putf(r, c, if k == 0 { "◆" } else { "·" }, if k == 0 { C::TealB } else { C::Teal });
                        }
                    }
                }
            }
        }

        self.draw_briefing(g, nw);
        self.own_panel(g);
        self.dock(g, nw);
        if self.chat_open {
            self.chat(g);
        }
        self.draw_card(g, nw);
        self.draw_prio(g, nw);
        let _ = sy;
    }

    fn draw_briefing(&mut self, g: &mut Grid, nw: i32) {
        let now = Instant::now();
        let Some(f) = self.fx.iter().find(|f| f.kind == FxKind::Briefing) else { return };
        if now < f.t0 {
            return;
        }
        let p = f.p(now);
        let el = now.duration_since(f.t0).as_secs_f32();
        let fade = p > 0.92;
        let w = 48.min(nw - 4);
        let c0 = 2;
        let lines = wrap(&f.text, (w - 5) as usize);
        let h = (lines.len() as i32).min(3) + 3;
        g.panel(5, c0, h, w, if fade { C::G } else { C::TDim }, false, &format!("BRIEFING · {}", grid::cut(&self.server_name.to_uppercase(), 24)), if fade { C::F } else { C::TealB });
        let mut left = (el * 37.0) as i32;
        for (i, ln) in lines.iter().take(3).enumerate() {
            let n = ln.chars().count() as i32;
            let part: String = ln.chars().take(left.max(0) as usize).collect();
            let typing = left < n && left >= 0;
            left -= n + 1;
            g.put(7 + i as i32, c0 + 3, &format!("{part}{}", if typing { "█" } else { "" }), if fade { C::F } else { C::T });
        }
        if p < 1.0 {
            self.hits.push(Hit { r: 5, c: c0, h, w, act: Act::Dismiss });
        }
    }

    fn draw_prio(&mut self, g: &mut Grid, nw: i32) {
        let now = Instant::now();
        let Some(f) = self.fx.iter().find(|f| f.kind == FxKind::Prio) else { return };
        let p = f.p(now);
        let blink = p < 0.3 && ((p * 24.0) as i32) % 2 == 0;
        let co = if p > 0.9 { C::F } else if blink { C::AmberB } else { C::RedB };
        let w = 75.min(nw - 6);
        // Ist die Personen-Karte offen (rechts unten), weicht das Banner nach links aus
        let c0 = if self.card.is_some() { ((nw - 44) - w - 2).max(2) } else { (nw - w) / 2 };
        let r0 = self.rows - 15;
        g.clear(r0, c0, 6, w);
        g.frame(r0, c0, 6, w, co, true);
        g.put(r0 + 1, c0 + 3, &format!("!! {}", f.text), co);
        let body = grid::cut(&f.text2, (w - 6) as usize);
        let body = if p < 0.15 { decode(&body, p / 0.15, 3) } else { body };
        g.put(r0 + 2, c0 + 3, &body, if p > 0.9 { C::F } else { C::B });
        g.put(r0 + 4, c0 + 3, l("ESC ODER KLICK ZUM SCHLIESSEN", "ESC OR CLICK TO DISMISS"), C::F);
        self.hits.push(Hit { r: r0, c: c0, h: 6, w, act: Act::Dismiss });
    }

    fn own_panel(&mut self, g: &mut Grid) {
        let r0 = self.rows - 9;
        g.panel(r0, 2, 6, 40, C::G, false, l("DU", "YOU"), C::D);
        let (st, co) = if self.deafened {
            (l("TON AUS", "DEAFENED"), C::Red)
        } else if self.muted {
            (l("STUMM", "MUTED"), C::Red)
        } else if self.audio.shared.transmitting.load(Ordering::Relaxed) {
            (l("SENDET", "ON AIR"), C::TealB)
        } else if self.away {
            (l("ABWESEND", "AWAY"), C::Amber)
        } else {
            (l("BEREIT", "READY"), C::Green)
        };
        let name = self.users.get(&self.you).map(|u| u.name.clone()).unwrap_or_else(|| self.cfg.name.clone());
        let initial = name.chars().next().unwrap_or('?').to_uppercase().next().unwrap_or('?');
        g.put(r0 + 1, 4, &format!("({initial})"), if self.muted { C::Red } else { user_color(&name) });
        g.put(r0 + 1, 8, &grid::cut(&name, 20), C::B);
        g.right(r0 + 1, 39, st, co);
        let mode = match self.cfg.voice_mode {
            VoiceMode::Activity => format!("{} · {}", l("SPRACHAKTIVIERUNG", "VOICE ACTIVATION"), if self.cfg.auto_threshold { "AUTO" } else { l("MANUELL", "MANUAL") }),
            VoiceMode::PushToTalk => format!("PUSH-TO-TALK · {}", self.cfg.ptt_key),
            VoiceMode::Continuous => l("DAUERHAFT SENDEN", "ALWAYS ON").into(),
        };
        g.put(r0 + 2, 8, &mode, C::F);
        let set = format!("[ {} ]", l("EINSTELLUNGEN", "SETTINGS"));
        g.put(r0 + 4, 8, &set, C::T);
        self.hits.push(Hit { r: r0 + 4, c: 8, h: 1, w: set.chars().count() as i32, act: Act::OpenSettings });
    }

    fn dock(&mut self, g: &mut Grid, nw: i32) {
        let now = Instant::now();
        let items: Vec<(u8, String, C)> = vec![
            (DOCK_MIC, if self.muted && !self.deafened { l("[ STUMM ]", "[ MUTED ]") } else { l("[ MIKRO ]", "[ MIC ]") }.into(), if self.muted { C::Red } else { C::T }),
            (DOCK_DEAF, if self.deafened { l("[ AUS ]", "[ OFF ]") } else { l("[ TON ]", "[ SOUND ]") }.into(), if self.deafened { C::Red } else { C::T }),
            (DOCK_AWAY, l("[ ABWESEND ]", "[ AWAY ]").into(), if self.away { C::Amber } else { C::T }),
            (DOCK_CHAT, "[ CHAT ]".into(), if self.chat_open { C::Teal } else { C::T }),
            (DOCK_SETTINGS, l("[ EINST. ]", "[ SETTINGS ]").into(), C::T),
            (DOCK_LEAVE, l("[ VERLASSEN ]", "[ LEAVE ]").into(), C::Red),
        ];
        let inner: i32 = items.iter().map(|i| i.1.chars().count() as i32).sum::<i32>() + 2 * (items.len() as i32 - 1);
        let w = inner + 6;
        let left = ((nw - w) / 2).max(44);
        let top = self.rows - 6;
        g.clear(top, left, 4, w);
        g.put(top, left + 2, &"_".repeat((w - 4) as usize), C::TDim);
        g.set(top + 1, left + 1, '/', C::TDim);
        g.set(top + 1, left + w - 2, '\\', C::TDim);
        g.set(top + 2, left, '<', C::TDim);
        g.set(top + 2, left + w - 1, '>', C::TDim);
        g.put(top + 3, left + 1, &format!("\\{}/", "_".repeat((w - 4) as usize)), C::TDim);
        let mut c = left + 3;
        for (key, label, co) in items {
            let fx = self.fx.iter().find(|f| f.kind == FxKind::Dock(key)).map(|f| f.p(now)).filter(|p| *p < 1.0);
            let s = match fx {
                Some(p) => decode(&label, p, 9),
                None => label.clone(),
            };
            g.put(top + 2, c, &s, if fx.is_some() { C::TealB } else { co });
            let n = label.chars().count() as i32;
            self.hits.push(Hit { r: top + 1, c, h: 3, w: n, act: Act::Dock(key) });
            c += n + 2;
        }
    }

    pub fn chat(&mut self, g: &mut Grid) {
        let cw = self.chat_width();
        let x0 = self.cols - cw;
        let rows = self.rows;
        g.clear(3, x0, rows - 3, cw);
        g.frame(3, x0, rows - 3, cw, C::G, false);
        g.segs(3, x0 + 2, &[("[ ", C::D), ("●", C::Teal), (&format!(" {} · {} ]", l("FUNKPROTOKOLL", "RADIO LOG"), self.channel_code(self.current)), C::D)]);
        g.put(rows - 5, x0, &format!("├{}┤", "─".repeat((cw - 2) as usize)), C::G);
        g.put(rows - 4, x0 + 2, ">", C::Teal);
        if self.texts[2].is_empty() {
            g.put(rows - 4, x0 + 4, &format!("{} {}", l("Nachricht an", "Message to"), grid::cut(&self.channel_name(self.current), 20)), C::F);
        }
        g.put(rows - 2, x0 + 2, l("ENTER = SENDEN · @NAME = ERWÄHNEN", "ENTER = SEND · @NAME = MENTION"), C::F);
        self.fields[2] = FieldPos { shown: true, r: rows - 4, c: x0 + 4, w: cw - 6 };
        self.hits.push(Hit { r: rows - 4, c: x0 + 4, h: 1, w: cw - 6, act: Act::Field(2) });

        // Zeilen bauen (von unten nach oben sichtbar)
        let ww = (cw - 7).max(10) as usize;
        let msgs = self.messages().to_vec();
        let typing = self.fxp(&FxKind::Type).filter(|p| *p < 1.0);
        let decrypt = self.fxp(&FxKind::Decrypt).filter(|p| *p < 1.0);
        let mut lines: Vec<(Vec<(String, C)>, bool)> = Vec::new();
        let n = msgs.len();
        for (i, m) in msgs.iter().enumerate() {
            let last = i + 1 == n;
            match m.from {
                None => {
                    for (j, ln) in wrap(&m.text, ww - 3).into_iter().enumerate() {
                        lines.push((vec![(if j == 0 { format!("// {ln}") } else { format!("   {ln}") }, C::F)], false));
                    }
                }
                Some(from) => {
                    if i > 0 {
                        lines.push((Vec::new(), false));
                    }
                    let name_co = if m.mention { C::Blue } else { user_color(&m.author) };
                    lines.push((vec![(format!("[{}] ", clock(m.time)), C::F), (m.author.to_uppercase(), name_co)], m.mention));
                    let mine = from == self.you;
                    let text = match (last, typing, decrypt) {
                        (true, Some(p), _) if mine => {
                            let k = (m.text.chars().count() as f32 * p) as usize;
                            format!("{}█", m.text.chars().take(k).collect::<String>())
                        }
                        (true, _, Some(p)) if !mine => decode(&m.text, ((p - 0.25) / 0.75).clamp(0.0, 1.0), 13),
                        _ => m.text.clone(),
                    };
                    let hot = last && ((typing.is_some() && mine) || (decrypt.is_some() && !mine));
                    for ln in wrap(&text, ww) {
                        lines.push((vec![(format!("  {ln}"), if hot { C::Teal } else { C::T })], m.mention));
                    }
                }
            }
        }
        let avail = (rows - 11).max(1) as usize;
        let max_scroll = lines.len().saturating_sub(avail) as i32;
        self.chat_scroll = self.chat_scroll.min(max_scroll);
        let end = lines.len() - self.chat_scroll as usize;
        let start = end.saturating_sub(avail);
        let shown = &lines[start..end];
        let r0 = rows - 6 - shown.len() as i32;
        for (i, (segs, mention)) in shown.iter().enumerate() {
            let r = r0 + i as i32;
            if *mention {
                g.fill_bg(r, x0 + 1, 1, cw - 2, Bg::Mention);
                g.put(r, x0 + 1, "▌", C::Blue);
            }
            let list: Vec<(&str, C)> = segs.iter().map(|(s, c)| (s.as_str(), *c)).collect();
            g.segs(r, x0 + 3, &list);
        }
        if msgs.is_empty() {
            g.put(rows - 7, x0 + 3, &format!("// {}", l("Noch keine Nachrichten", "No messages yet")), C::F);
        }
        if self.chat_scroll > 0 {
            g.put(4, x0 + cw - 12, &format!("↑ {}", self.chat_scroll), C::F);
        }
    }

    fn draw_card(&mut self, g: &mut Grid, nw: i32) {
        let Some(id) = self.card else { return };
        let Some(u) = self.users.get(&id).cloned() else { return };
        let r0 = self.rows - 15;
        let c0 = nw - 44;
        let mut cg = Grid::new(self.rows, self.cols);
        let cur = u.channel == self.current;
        let st = self.state_of(&u, cur);
        cg.frame(r0, c0, 8, 42, C::TealB, false);
        cg.put(r0, c0 + 2, &format!("[ {} · {} ]", grid::cut(&u.name.to_uppercase(), 20), self.channel_code(u.channel)), C::B);
        let initial = u.name.chars().next().unwrap_or('?').to_uppercase().next().unwrap_or('?');
        cg.put(r0 + 1, c0 + 2, &format!("[{initial}]"), if st == St::Speak { C::TealB } else { user_color(&u.name) });
        cg.put(r0 + 1, c0 + 7, &grid::cut(&u.name, 26), C::B);
        cg.put(r0 + 1, c0 + 37, "[x]", C::D);
        let (stx, sco) = match st {
            St::Speak => (l("SPRICHT", "SPEAKING"), C::TealB),
            St::Mute => (l("STUMM", "MUTED"), C::Red),
            St::Deaf => (l("TON AUS", "DEAFENED"), C::Red),
            St::Away => (l("ABWESEND", "AWAY"), C::Amber),
            St::None => (l("VERBUNDEN", "CONNECTED"), C::D),
        };
        cg.put(r0 + 2, c0 + 7, stx, sco);
        let rx = self.audio.shared.rx.lock().unwrap().get(&id).copied();
        let (loss, buf, jit, lco) = match rx {
            Some(r) => (
                format!("{:.1} %", r.loss),
                format!("{:.0} ms", r.buffer_ms),
                format!("{:.0} ms", r.jitter_ms),
                if r.loss < 3.0 { C::Green } else if r.loss < 10.0 { C::Amber } else { C::Red },
            ),
            None => ("–".into(), "–".into(), "–".into(), C::D),
        };
        let loss = if tidespeak::i18n::is_german() { loss.replace('.', ",") } else { loss };
        cg.segs(r0 + 3, c0 + 2, &[(l("VERLUST ", "LOSS "), C::F), (&loss, lco), (l(" PUFFER ", " BUFFER "), C::F), (&buf, C::T), (" JITTER ", C::F), (&jit, C::T)]);
        let vol = (self.cfg.user_volumes.get(&u.name).copied().unwrap_or(1.0) * 100.0).round() as i32;
        let k = ((vol as f32 / 200.0) * 20.0).round() as usize;
        cg.segs(
            r0 + 4,
            c0 + 2,
            &[("VOL ", C::F), ("[", C::D), (&"█".repeat(k.min(20)), if vol == 0 { C::Red } else { C::Teal }), (&"░".repeat(20 - k.min(20)), C::F), ("]", C::D), (&format!(" {vol} %"), C::T)],
        );
        let poked = self.fx.iter().any(|f| f.kind == FxKind::Poke(id));
        let poke = if poked { l("[ ANGESTUPST ]", "[ POKED ]") } else { l("[ ANSTUPSEN ]", "[ POKE ]") };
        cg.put(r0 + 6, c0 + 2, poke, if poked { C::TealB } else { C::T });
        let mute = if vol == 0 { l("[ WIEDER ANHÖREN ]", "[ UNMUTE FOR ME ]") } else { l("[ FÜR MICH STUMM ]", "[ MUTE FOR ME ]") };
        cg.put(r0 + 6, c0 + 18, mute, if vol == 0 { C::Red } else { C::T });

        let open = self.fxp(&FxKind::Card).filter(|p| *p < 1.0).map(ease).unwrap_or(1.0);
        let wv = ((42.0 * open).round() as i32).max(1);
        g.clear(r0, c0, 8, wv);
        g.copy_from(&cg, r0, r0 + 7, c0, c0 + wv - 1);
        if open < 1.0 {
            for r in r0..r0 + 8 {
                g.set(r, c0 + wv - 1, '▓', C::TealB);
            }
        }
        self.hits.push(Hit { r: r0, c: c0, h: 8, w: 42, act: Act::Swallow });
        self.hits.push(Hit { r: r0 + 1, c: c0 + 36, h: 1, w: 5, act: Act::CardClose });
        self.hits.push(Hit { r: r0 + 4, c: c0 + 7, h: 1, w: 20, act: Act::CardVolume });
        self.hits.push(Hit { r: r0 + 6, c: c0 + 2, h: 1, w: poke.chars().count() as i32, act: Act::CardPoke });
        self.hits.push(Hit { r: r0 + 6, c: c0 + 18, h: 1, w: mute.chars().count() as i32, act: Act::CardMute });
    }
}

/// Small oscilloscope of block characters, only while someone speaks
fn scope(ph: f32, seed: f32) -> String {
    let bars: Vec<char> = "▁▂▃▄▅▆▇".chars().collect();
    (0..8)
        .map(|i| {
            let i = i as f32;
            let v = 0.5 + 0.5 * (ph * 2.3 + i * 0.9 + seed).sin() * (ph * 1.1 + i * 0.35).sin();
            bars[((v.abs() * 7.0) as usize).min(6)]
        })
        .collect()
}
