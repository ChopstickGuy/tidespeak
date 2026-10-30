//! The ASCII interface: state, network events, input and actions.
//! Drawing lives in `draw.rs` (network, chat, panels), `scenes.rs` (start,
//! connect, sequences) and `settings.rs` (settings drawer).

mod draw;
mod scenes;
mod settings;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::platform::Key;
use tidespeak::audio::{self, Audio, AudioSettings, Cue};
use tidespeak::config::{normalize_address, Config, VoiceMode};
use tidespeak::grid::{Grid, C};
use tidespeak::i18n::{self, l, lf};
use tidespeak::net::{Command, Event};
use tidespeak_proto::{fingerprint_short, sanitize_name, ChannelInfo, ChatLine, UserInfo};
use tokio::sync::mpsc::UnboundedSender;

/// Push-to-talk keys: (label, Slint key, Windows virtual key)
pub const PTT_KEYS: [(&str, Key, u32); 8] = [
    ("F2", Key::F2, 0x71),
    ("F3", Key::F3, 0x72),
    ("F4", Key::F4, 0x73),
    ("F5", Key::F5, 0x74),
    ("F6", Key::F6, 0x75),
    ("F7", Key::F7, 0x76),
    ("F8", Key::F8, 0x77),
    ("F9", Key::F9, 0x78),
];
pub const BITRATES: [i32; 3] = [32_000, 64_000, 96_000];
const MAX_LOG: usize = 400;

#[derive(Clone)]
pub struct Entry {
    pub from: Option<u32>,
    pub author: String,
    pub text: String,
    pub time: u64,
    pub mention: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conn {
    Offline,
    Connecting,
    Connected,
}

pub enum Scene {
    /// Start of the app (wave forms, then the connect screen appears)
    Boot { f: f32 },
    Connect,
    /// Connection sequence; waits at a gate until the server has answered
    Sequence { f: f32, fast: bool, ok: bool, fail: Option<String> },
    Main,
    /// Leaving the server (or losing the connection, then `reason` is set)
    Disconnect { f: f32, reason: Option<String> },
    /// Closing the app
    Quit { f: f32 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum FxKind {
    Dock(u8),
    Type,
    Decrypt,
    Lock(u32),
    Leave,
    Poke(u32),
    Card,
    Prio,
    Toast,
    DrawerOpen,
    DrawerClose,
    Tab,
    Toggle(&'static str),
    Radio(&'static str, i32),
    Slider(&'static str),
    Choice(&'static str),
    Verify,
    Switch,
    Briefing,
}

pub struct Fx {
    pub kind: FxKind,
    pub t0: Instant,
    pub dur: f32,
    pub text: String,
    pub text2: String,
    pub from: f32,
    /// Snapshot (e.g. the old network for the channel switch, or a node that dissolves)
    pub grid: Option<Box<Grid>>,
    pub pos: (f32, f32),
}

impl Fx {
    pub fn p(&self, now: Instant) -> f32 {
        (now.duration_since(self.t0).as_secs_f32() / self.dur).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Connect,
    Recent(usize),
    TrustKey,
    CancelKey,
    Join(u32),
    User(u32),
    CardClose,
    CardVolume,
    CardPoke,
    CardMute,
    Dock(u8),
    OpenSettings,
    CloseSettings,
    Tab(usize),
    Toggle(&'static str),
    Radio(&'static str, i32),
    Slider(&'static str),
    Choice(&'static str, i32),
    Verify,
    Minimize,
    Maximize,
    Close,
    Skip,
    Field(u8),
    Dismiss,
    Swallow,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub r: i32,
    pub c: i32,
    pub h: i32,
    pub w: i32,
    pub act: Act,
}

/// What the window should do (handled by `main.rs`)
pub enum WinAct {
    Minimize,
    Maximize,
    Drag,
    Quit,
    Focus(i32),
    SetText(u8, String),
    Attention,
}

#[derive(Clone, Copy, Default)]
pub struct FieldPos {
    pub shown: bool,
    pub r: i32,
    pub c: i32,
    pub w: i32,
}

pub struct Drawer {
    pub tab: usize,
    pub scroll: i32,
}

pub const DOCK_MIC: u8 = 0;
pub const DOCK_DEAF: u8 = 1;
pub const DOCK_AWAY: u8 = 2;
pub const DOCK_CHAT: u8 = 3;
pub const DOCK_SETTINGS: u8 = 4;
pub const DOCK_LEAVE: u8 = 5;

pub struct App {
    pub cfg: Config,
    pub audio: Arc<Audio>,
    tx: UnboundedSender<Command>,
    pub version: &'static str,

    // Verbindung und Server
    pub conn: Conn,
    pub you: u32,
    pub address: String,
    pub server_name: String,
    pub fingerprint: String,
    pub channels: Vec<ChannelInfo>,
    pub users: BTreeMap<u32, UserInfo>,
    pub current: u32,
    pub logs: HashMap<u32, Vec<Entry>>,
    pub speaking: HashSet<u32>,
    pub quality: HashMap<u32, i32>,
    pub ping: u32,
    pub rx_loss: f32,
    pub tx_loss: u32,
    pub muted: bool,
    pub deafened: bool,
    pub away: bool,
    pub error: Option<String>,
    pub key_change: Option<(String, String, String)>,
    pub session_start: Instant,
    pub new_server: bool,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub verify_done: bool,

    // Oberfläche
    pub scene: Scene,
    pub drawer: Option<Drawer>,
    pub fx: Vec<Fx>,
    pub hits: Vec<Hit>,
    pub hover: Option<usize>,
    pub card: Option<u32>,
    pub chat_open: bool,
    pub chat_scroll: i32,
    pub fields: [FieldPos; 3],
    pub texts: [String; 3],
    pub grid: Grid,
    pub cols: i32,
    pub rows: i32,
    pub wave: f32,
    pub phase: f32,
    pub level: f32,
    pub win: Vec<WinAct>,
    pub dirty: bool,
    last_slow: Instant,
    press: Option<(f32, f32)>,
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn clock(unix: u64) -> String {
    use chrono::{Local, TimeZone};
    Local.timestamp_opt(unix as i64, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

pub fn audio_settings(cfg: &Config) -> AudioSettings {
    let ptt = PTT_KEYS.iter().position(|k| k.0 == cfg.ptt_key).unwrap_or(0);
    AudioSettings {
        input_device: cfg.input_device.clone(),
        output_device: cfg.output_device.clone(),
        input_gain: cfg.input_gain,
        output_volume: cfg.output_volume,
        voice_mode: cfg.voice_mode,
        activation_threshold: cfg.activation_threshold,
        noise_suppression: cfg.noise_suppression,
        agc: cfg.auto_gain,
        echo_cancel: cfg.echo_cancel,
        auto_threshold: cfg.auto_threshold,
        bitrate: cfg.opus_bitrate,
        ptt_key: PTT_KEYS[ptt].2,
    }
}

/// Colour per name (stable)
pub fn user_color(name: &str) -> C {
    let h = name.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    [C::U0, C::U1, C::U2, C::U3, C::U4, C::U5][(h % 6) as usize]
}

pub fn mentions(text: &str, me: &str) -> bool {
    !me.is_empty() && text.to_lowercase().contains(&format!("@{}", me.to_lowercase()))
}

impl App {
    pub fn new(cfg: Config, audio: Arc<Audio>, tx: UnboundedSender<Command>) -> App {
        let boot = cfg.anim_start;
        let texts = [cfg.address.clone(), cfg.name.clone(), String::new()];
        let now = Instant::now();
        App {
            cfg,
            audio,
            tx,
            version: env!("CARGO_PKG_VERSION"),
            conn: Conn::Offline,
            you: 0,
            address: String::new(),
            server_name: String::new(),
            fingerprint: String::new(),
            channels: Vec::new(),
            users: BTreeMap::new(),
            current: 0,
            logs: HashMap::new(),
            speaking: HashSet::new(),
            quality: HashMap::new(),
            ping: 0,
            rx_loss: 0.0,
            tx_loss: 0,
            muted: false,
            deafened: false,
            away: false,
            error: None,
            key_change: None,
            session_start: now,
            new_server: false,
            inputs: Vec::new(),
            outputs: Vec::new(),
            verify_done: false,
            scene: if boot { Scene::Boot { f: 0.0 } } else { Scene::Connect },
            drawer: None,
            fx: Vec::new(),
            hits: Vec::new(),
            hover: None,
            card: None,
            chat_open: true,
            chat_scroll: 0,
            fields: [FieldPos::default(); 3],
            texts,
            grid: Grid::new(56, 180),
            cols: 180,
            rows: 56,
            wave: 0.0,
            phase: 0.0,
            level: 0.0,
            win: Vec::new(),
            dirty: true,
            last_slow: now,
            press: None,
        }
    }

    // ── kleine Helfer

    pub fn kick(&mut self, kind: FxKind, dur: f32) -> &mut Fx {
        self.fx.retain(|f| f.kind != kind);
        self.fx.push(Fx { kind, t0: Instant::now(), dur, text: String::new(), text2: String::new(), from: 0.0, grid: None, pos: (0.0, 0.0) });
        self.dirty = true;
        self.fx.last_mut().unwrap()
    }

    pub fn fx(&self, kind: &FxKind) -> Option<&Fx> {
        self.fx.iter().find(|f| &f.kind == kind)
    }

    pub fn fxp(&self, kind: &FxKind) -> Option<f32> {
        self.fx(kind).map(|f| f.p(Instant::now()))
    }

    /// Terminal sounds (can be switched off)
    pub fn sound(&self, cue: Cue) {
        if self.cfg.terminal_sounds {
            self.audio.cue(cue);
        }
    }

    pub fn toast(&mut self, text: String) {
        let f = self.kick(FxKind::Toast, 1.9);
        f.text = text;
    }

    fn my_name(&self) -> String {
        self.users.get(&self.you).map(|u| u.name.clone()).unwrap_or_default()
    }

    pub fn channel_name(&self, id: u32) -> String {
        self.channels.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_default()
    }

    pub fn channel_code(&self, id: u32) -> String {
        format!("CH-{:02}", self.channels.iter().position(|c| c.id == id).unwrap_or(0) + 1)
    }

    pub fn messages(&self) -> &[Entry] {
        self.logs.get(&self.current).map(|v| v.as_slice()).unwrap_or(&[])
    }

    fn push_entry(&mut self, channel: u32, e: Entry) {
        let log = self.logs.entry(channel).or_default();
        log.push(e);
        if log.len() > MAX_LOG {
            log.drain(..log.len() - MAX_LOG);
        }
        if channel == self.current {
            self.chat_scroll = 0;
        }
        self.dirty = true;
    }

    fn system_line(&mut self, channel: u32, text: String) {
        self.push_entry(channel, Entry { from: None, author: String::new(), text, time: now_unix(), mention: false });
    }

    fn apply_mute(&mut self) {
        let was = self.audio.shared.muted.swap(self.muted, Ordering::Relaxed);
        self.audio.shared.deafened.store(self.deafened, Ordering::Relaxed);
        if was != self.muted {
            self.audio.cue(if self.muted { Cue::Mute } else { Cue::Unmute });
        }
        let _ = self.tx.send(Command::State { muted: self.muted, deafened: self.deafened, away: self.away });
        self.dirty = true;
    }

    pub fn apply_user_gains(&self) {
        for u in self.users.values() {
            let g = self.cfg.user_volumes.get(&u.name).copied().unwrap_or(1.0);
            self.audio.set_user_gain(u.id, g);
        }
    }

    pub fn apply_settings(&mut self) {
        let a = audio_settings(&self.cfg);
        self.audio.update_settings(|s| *s = a);
        self.cfg.save();
        self.dirty = true;
    }

    pub fn refresh_devices(&mut self) {
        let (inputs, outputs) = audio::list_devices();
        self.inputs = inputs;
        self.outputs = outputs;
        self.dirty = true;
    }

    // ── Verbinden / Trennen

    pub fn start_connect(&mut self, forced: Option<String>) {
        let address = forced.unwrap_or_else(|| normalize_address(&self.texts[0]));
        let name = sanitize_name(&self.texts[1]);
        if name.is_empty() {
            self.error = Some(l("Bitte gib einen Namen ein.", "Please enter a name.").into());
            self.dirty = true;
            return;
        }
        self.error = None;
        self.key_change = None;
        self.cfg.name = name.clone();
        self.cfg.address = self.texts[0].trim().to_string();
        self.cfg.save();
        let pinned = self.cfg.known_servers.get(&address).cloned();
        let fast = pinned.is_some() || !self.cfg.anim_connect;
        self.conn = Conn::Connecting;
        self.scene = Scene::Sequence { f: 0.0, fast, ok: false, fail: None };
        self.win.push(WinAct::Focus(-1));
        self.sound(Cue::Tick);
        let _ = self.tx.send(Command::Connect { address, name, pinned });
        self.dirty = true;
    }

    pub fn leave(&mut self) {
        let _ = self.tx.send(Command::Disconnect);
        self.drawer = None;
        self.card = None;
        self.scene = Scene::Disconnect { f: 0.0, reason: None };
        self.sound(Cue::Alarm);
        self.dirty = true;
    }

    /// Window close: play the farewell (if wanted), then quit
    pub fn request_close(&mut self) {
        if matches!(self.scene, Scene::Quit { .. }) {
            self.win.push(WinAct::Quit);
            return;
        }
        if self.conn != Conn::Offline {
            let _ = self.tx.send(Command::Disconnect);
        }
        if self.cfg.anim_start {
            self.drawer = None;
            self.scene = Scene::Quit { f: 0.0 };
            self.sound(Cue::Shutdown);
            self.dirty = true;
        } else {
            self.win.push(WinAct::Quit);
        }
    }

    // ── Netzwerk-Ereignisse

    pub fn net_event(&mut self, ev: Event) {
        self.dirty = true;
        match ev {
            Event::Connecting => {
                self.conn = Conn::Connecting;
            }
            Event::Connected { address, fingerprint, you, server_name, motd, channels, users } => {
                let new = !self.cfg.known_servers.contains_key(&address);
                if new {
                    self.cfg.known_servers.insert(address.clone(), fingerprint.clone());
                }
                self.cfg.remember_server(&address);
                self.cfg.save();
                self.new_server = new;
                self.conn = Conn::Connected;
                self.you = you;
                self.address = address;
                self.server_name = server_name;
                self.fingerprint = fingerprint.clone();
                self.channels = channels;
                self.users = users.into_iter().map(|u| (u.id, u)).collect();
                self.current = self.users.get(&you).map(|u| u.channel).unwrap_or(0);
                self.logs.clear();
                self.speaking.clear();
                self.muted = false;
                self.deafened = false;
                self.away = false;
                self.verify_done = self.cfg.verified_servers.contains(&self.address);
                self.session_start = Instant::now();
                self.chat_scroll = 0;
                let ch = self.current;
                if !motd.is_empty() {
                    self.system_line(ch, format!("BRIEFING: {motd}"));
                    let f = self.kick(FxKind::Briefing, 9.5);
                    f.text = motd;
                    f.t0 += Duration::from_millis(if new { 6800 } else { 1600 });
                }
                if new {
                    self.system_line(
                        ch,
                        lf(
                            format!("Neuer Server – Fingerabdruck gespeichert: {}", fingerprint_short(&fingerprint)),
                            format!("New server – fingerprint saved: {}", fingerprint_short(&fingerprint)),
                        ),
                    );
                }
                self.apply_user_gains();
                if let Scene::Sequence { ok, .. } = &mut self.scene {
                    *ok = true;
                } else {
                    self.scene = Scene::Main;
                }
                self.audio.cue(Cue::Connected);
                self.refresh_devices();
            }
            Event::Failed { reason, changed_key } => {
                self.conn = Conn::Offline;
                if let Some((old, new)) = changed_key {
                    self.key_change = Some((normalize_address(&self.texts[0]), fingerprint_short(&old), fingerprint_short(&new)));
                    self.sound(Cue::Alarm);
                }
                self.error = Some(reason.clone());
                if let Scene::Sequence { fail, .. } = &mut self.scene {
                    *fail = Some(reason);
                } else {
                    self.scene = Scene::Connect;
                }
            }
            Event::Disconnected { reason } => {
                let by_user = matches!(self.scene, Scene::Disconnect { .. } | Scene::Quit { .. });
                self.conn = Conn::Offline;
                self.ping = 0;
                self.tx_loss = 0;
                self.rx_loss = 0.0;
                self.card = None;
                self.away = false;
                if !by_user {
                    if matches!(self.scene, Scene::Main) && self.cfg.anim_connect {
                        self.scene = Scene::Disconnect { f: 0.0, reason: Some(reason.clone()) };
                        self.sound(Cue::Alarm);
                    } else if !matches!(self.scene, Scene::Sequence { .. }) {
                        self.scene = Scene::Connect;
                    }
                    self.error = Some(reason);
                }
            }
            Event::UserJoined(u) => {
                let mine = u.channel == self.current;
                if mine {
                    self.system_line(u.channel, lf(format!("{} ist da", u.name), format!("{} is here", u.name)));
                    if self.cfg.anim_join {
                        self.kick(FxKind::Lock(u.id), 2.4);
                    }
                }
                let g = self.cfg.user_volumes.get(&u.name).copied().unwrap_or(1.0);
                self.audio.set_user_gain(u.id, g);
                self.users.insert(u.id, u);
                if mine {
                    self.audio.cue(Cue::Join);
                }
            }
            Event::UserUpdated(u) => {
                let old = self.users.get(&u.id).cloned();
                let moved = old.as_ref().is_some_and(|o| o.channel != u.channel);
                if u.id == self.you && moved {
                    let target = u.channel;
                    if self.cfg.anim_switch && matches!(self.scene, Scene::Main) {
                        self.start_switch(target);
                    }
                    self.users.insert(u.id, u);
                    self.current = target;
                    self.chat_scroll = 0;
                    self.card = None;
                    let name = self.channel_name(target);
                    self.system_line(target, lf(format!("Du bist {name} beigetreten"), format!("You joined {name}")));
                    self.audio.cue(Cue::Join);
                    return;
                }
                if moved {
                    let old_ch = old.map(|o| o.channel).unwrap_or(u32::MAX);
                    if u.channel == self.current {
                        self.system_line(u.channel, lf(format!("{} ist dem Kanal beigetreten", u.name), format!("{} joined the channel", u.name)));
                        if self.cfg.anim_join {
                            self.kick(FxKind::Lock(u.id), 2.4);
                        }
                        self.audio.cue(Cue::Join);
                    } else if old_ch == self.current {
                        self.start_leave(u.id);
                        self.system_line(old_ch, lf(format!("{} hat den Kanal verlassen", u.name), format!("{} left the channel", u.name)));
                        self.audio.cue(Cue::Leave);
                    }
                }
                self.users.insert(u.id, u);
            }
            Event::UserLeft(id) => {
                if let Some(u) = self.users.get(&id).cloned() {
                    if u.channel == self.current {
                        self.start_leave(id);
                        self.system_line(u.channel, lf(format!("{} hat den Server verlassen", u.name), format!("{} left the server", u.name)));
                        self.audio.cue(Cue::Leave);
                    }
                }
                self.users.remove(&id);
                if self.card == Some(id) {
                    self.card = None;
                }
            }
            Event::Chat(c) => self.on_chat(c),
            Event::Poked { name, text } => {
                let ch = self.current;
                let line = if text.is_empty() {
                    lf(format!("{name} hat dich angestupst"), format!("{name} poked you"))
                } else {
                    lf(format!("{name} hat dich angestupst: {text}"), format!("{name} poked you: {text}"))
                };
                self.system_line(ch, line);
                let f = self.kick(FxKind::Prio, 4.2);
                f.text = lf(format!("ANGESTUPST VON {}", name.to_uppercase()), format!("POKED BY {}", name.to_uppercase()));
                f.text2 = text;
                self.audio.cue(Cue::Poke);
                self.win.push(WinAct::Attention);
            }
            Event::Stats { rtt_ms, uplink_loss, downlink_loss } => {
                self.ping = rtt_ms;
                self.tx_loss = uplink_loss;
                self.rx_loss = downlink_loss;
            }
            Event::History { channel, lines } => {
                let me = self.my_name();
                let mut log: Vec<Entry> = lines.iter().map(|c| self.chat_entry(c, &me)).collect();
                if let Some(old) = self.logs.remove(&channel) {
                    log.extend(old.into_iter().filter(|e| e.from.is_none()));
                }
                self.logs.insert(channel, log);
            }
        }
    }

    fn chat_entry(&self, c: &ChatLine, me: &str) -> Entry {
        Entry { from: Some(c.from), author: c.name.clone(), text: c.text.clone(), time: c.time, mention: c.from != self.you && mentions(&c.text, me) }
    }

    fn on_chat(&mut self, c: ChatLine) {
        let me = self.my_name();
        let e = self.chat_entry(&c, &me);
        let mention = e.mention;
        let mine = c.from == self.you;
        let here = c.channel == self.current;
        self.push_entry(c.channel, e);
        if here {
            if mine {
                self.kick(FxKind::Type, 0.45);
            } else {
                self.kick(FxKind::Decrypt, 0.7);
                self.sound(Cue::Tick);
            }
        }
        if mention {
            let f = self.kick(FxKind::Prio, 4.2);
            f.text = lf(format!("PRIORITÄTSNACHRICHT · {}", c.name.to_uppercase()), format!("PRIORITY MESSAGE · {}", c.name.to_uppercase()));
            f.text2 = c.text.chars().take(140).collect();
            self.audio.cue(Cue::Poke);
            self.win.push(WinAct::Attention);
        }
    }

    // ── Zeittakt

    /// Advances animations; returns whether the picture must be redrawn
    pub fn tick(&mut self, dt: f32) -> bool {
        let now = Instant::now();
        if self.fx.iter().any(|f| f.kind == FxKind::DrawerClose && now.duration_since(f.t0).as_secs_f32() >= f.dur) {
            self.drawer = None;
        }
        let before = self.fx.len();
        self.fx.retain(|f| now.duration_since(f.t0).as_secs_f32() < f.dur || f.t0 > now);
        if self.fx.len() != before {
            self.dirty = true;
        }

        // Wer spricht, Verbindungsqualität
        let speaking: HashSet<u32> = {
            let mut s: HashSet<u32> = self
                .audio
                .shared
                .speaking
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, t)| now.duration_since(**t) < Duration::from_millis(350))
                .map(|(id, _)| *id)
                .collect();
            if self.you != 0 && self.audio.shared.transmitting.load(Ordering::Relaxed) {
                s.insert(self.you);
            }
            s
        };
        let quality: HashMap<u32, i32> = self
            .audio
            .shared
            .rx
            .lock()
            .unwrap()
            .iter()
            .map(|(id, r)| (*id, if r.loss < 3.0 { 0 } else if r.loss < 10.0 { 1 } else { 2 }))
            .collect();
        if speaking != self.speaking || quality != self.quality {
            self.speaking = speaking;
            self.quality = quality;
            self.dirty = true;
        }

        // Szenen mit eigenem Zeitablauf
        let mut animating = !self.fx.is_empty();
        let mut next: Option<Scene> = None;
        match &mut self.scene {
            Scene::Boot { f } => {
                *f += dt * 30.0;
                animating = true;
                if *f >= 128.0 {
                    next = Some(Scene::Connect);
                }
            }
            Scene::Sequence { f, fast, ok, fail } => {
                animating = true;
                let gate = if *fast { 16.0 } else { 62.0 };
                if fail.is_some() {
                    *f += dt * 25.0;
                } else if *f < gate || *ok {
                    *f += dt * 25.0;
                    if !*ok && *f > gate {
                        *f = gate;
                    }
                }
                let end = if *fast { 44.0 } else { 172.0 };
                if fail.is_some() && *f > gate + 40.0 {
                    next = Some(Scene::Connect);
                } else if *ok && *f >= end {
                    next = Some(Scene::Main);
                }
            }
            Scene::Disconnect { f, .. } => {
                *f += dt * 35.0;
                animating = true;
                if *f >= 118.0 {
                    next = Some(Scene::Connect);
                }
            }
            Scene::Quit { f } => {
                *f += dt * 40.0;
                animating = true;
                if *f >= 62.0 {
                    self.win.push(WinAct::Quit);
                }
            }
            Scene::Connect | Scene::Main => {}
        }
        if let Some(s) = next {
            self.enter(s);
        }

        // Langsame Animationen: rollende Welle, Oszilloskop, Pegel in den Einstellungen (ca. 8 Bilder/s)
        let slow_due = now.duration_since(self.last_slow) >= Duration::from_millis(120);
        if slow_due {
            let mut slow = false;
            if matches!(self.scene, Scene::Main | Scene::Connect) && self.cfg.anim_wave {
                self.wave += 0.06;
                slow = true;
            }
            if matches!(self.scene, Scene::Main) && self.speaking.iter().any(|id| self.users.get(id).is_some_and(|u| u.channel == self.current)) {
                self.phase += 0.35;
                slow = true;
            }
            if self.drawer.is_some() {
                let lv = (self.audio.shared.level() * 24.0).round() / 24.0;
                if lv != self.level {
                    self.level = lv;
                    slow = true;
                }
                if self.audio.shared.mic_test.load(Ordering::Relaxed) || self.audio.shared.transmitting.load(Ordering::Relaxed) {
                    slow = true;
                }
            }
            if slow {
                self.last_slow = now;
                self.dirty = true;
            }
        }
        if !self.drawer.is_some() && self.audio.shared.mic_test.load(Ordering::Relaxed) {
            self.audio.shared.mic_test.store(false, Ordering::Relaxed);
        }
        let redraw = animating || self.dirty;
        redraw
    }

    fn enter(&mut self, s: Scene) {
        match s {
            Scene::Connect => {
                self.drawer = None;
                self.card = None;
                if self.conn == Conn::Connected {
                    // Verbindung kam während einer anderen Sequenz – direkt ins Netz
                    self.scene = Scene::Main;
                    return;
                }
                // Sitzungsdaten leeren
                self.users.clear();
                self.channels.clear();
                self.logs.clear();
                self.speaking.clear();
                self.you = 0;
                self.fingerprint.clear();
                self.fx.retain(|f| matches!(f.kind, FxKind::Toast));
                self.win.push(WinAct::SetText(0, self.texts[0].clone()));
            }
            Scene::Main => {
                self.sound(Cue::Tick);
            }
            _ => {}
        }
        self.scene = s;
        self.dirty = true;
    }

    pub fn skip(&mut self) {
        match &mut self.scene {
            Scene::Boot { .. } => self.enter(Scene::Connect),
            Scene::Sequence { f, fast, ok, fail } => {
                if *ok {
                    *f = if *fast { 44.0 } else { 172.0 };
                } else if fail.is_none() {
                    *f = f.max(if *fast { 16.0 } else { 62.0 });
                }
            }
            Scene::Disconnect { .. } => self.enter(Scene::Connect),
            Scene::Quit { .. } => self.win.push(WinAct::Quit),
            _ => {}
        }
        self.dirty = true;
    }

    // ── Eingaben

    fn hit_at(&self, x: f32, y: f32) -> Option<usize> {
        let (c, r) = ((x / tidespeak::render::CELL_W) as i32, (y / tidespeak::render::CELL_H) as i32);
        self.hits.iter().rposition(|h| r >= h.r && r < h.r + h.h && c >= h.c && c < h.c + h.w)
    }

    pub fn pointer(&mut self, kind: &str, x: f32, y: f32) {
        match kind {
            "move" => {
                if let Some((px, py)) = self.press {
                    if (px - x).abs() + (py - y).abs() > 4.0 && py < 3.0 * tidespeak::render::CELL_H {
                        self.press = None;
                        self.win.push(WinAct::Drag);
                    }
                }
                let h = self.hit_at(x, y).filter(|i| self.hits[*i].act != Act::Swallow);
                if h != self.hover {
                    self.hover = h;
                    self.dirty = true;
                }
            }
            "leave" => {
                if self.hover.is_some() {
                    self.hover = None;
                    self.dirty = true;
                }
            }
            "double" => {
                if y < 3.0 * tidespeak::render::CELL_H && self.hit_at(x, y).is_none() {
                    self.win.push(WinAct::Maximize);
                }
            }
            "down" => {
                if matches!(self.scene, Scene::Boot { .. } | Scene::Quit { .. } | Scene::Disconnect { .. }) {
                    self.skip();
                    return;
                }
                match self.hit_at(x, y) {
                    Some(i) => {
                        let hit = self.hits[i].clone();
                        let rel = ((x / tidespeak::render::CELL_W) - hit.c as f32) / hit.w.max(1) as f32;
                        self.activate(hit.act, rel.clamp(0.0, 1.0));
                    }
                    None => {
                        if y < 3.0 * tidespeak::render::CELL_H {
                            self.press = Some((x, y));
                        } else if self.card.is_some() {
                            self.card = None;
                            self.dirty = true;
                        }
                        self.win.push(WinAct::Focus(-1));
                    }
                }
            }
            "up" => self.press = None,
            _ => {}
        }
    }

    pub fn scroll(&mut self, x: f32, _y: f32, dy: f32) {
        let c = (x / tidespeak::render::CELL_W) as i32;
        let edge = self.cols - self.drawer_width();
        if let Some(d) = &mut self.drawer {
            if c >= edge {
                d.scroll = (d.scroll - (dy / 16.0).round() as i32 * 2).max(0);
                self.dirty = true;
                return;
            }
        }
        if matches!(self.scene, Scene::Main) && self.chat_open && c >= self.cols - self.chat_width() {
            self.chat_scroll = (self.chat_scroll + (dy / 16.0).round() as i32 * 2).max(0);
            self.dirty = true;
        }
    }

    /// Keys: Esc, push-to-talk. Returns true if handled.
    pub fn key(&mut self, text: &str, down: bool) -> bool {
        if down && text == "\u{1b}" {
            if !matches!(self.scene, Scene::Connect | Scene::Main) {
                self.skip();
            } else if self.drawer.is_some() {
                self.close_drawer();
            } else if self.card.is_some() {
                self.card = None;
            } else {
                self.fx.retain(|f| f.kind != FxKind::Prio && f.kind != FxKind::Briefing);
            }
            self.dirty = true;
            return true;
        }
        if down && matches!(self.scene, Scene::Boot { .. } | Scene::Quit { .. }) {
            self.skip();
            return true;
        }
        if self.cfg.voice_mode != VoiceMode::PushToTalk {
            return false;
        }
        let Some(k) = PTT_KEYS.iter().find(|k| k.0 == self.cfg.ptt_key) else { return false };
        if text == slint::SharedString::from(k.1).as_str() {
            self.audio.shared.ptt.store(down, Ordering::Relaxed);
            return true;
        }
        false
    }

    pub fn field_edited(&mut self, i: usize, text: String) {
        self.texts[i] = text;
        if i == 2 {
            self.sound(Cue::Tick);
        }
    }

    pub fn field_accepted(&mut self, i: usize) {
        match i {
            0 | 1 if matches!(self.scene, Scene::Connect) => self.start_connect(None),
            2 => {
                let t = self.texts[2].trim().to_string();
                if !t.is_empty() {
                    let _ = self.tx.send(Command::Chat(t));
                    self.texts[2].clear();
                    self.win.push(WinAct::SetText(2, String::new()));
                    self.chat_scroll = 0;
                }
            }
            _ => {}
        }
    }

    pub fn drawer_width(&self) -> i32 {
        92.min(self.cols - 30)
    }

    pub fn chat_width(&self) -> i32 {
        if self.chat_open {
            ((self.cols as f32 * 0.26) as i32).clamp(38, 52)
        } else {
            0
        }
    }

    pub fn open_drawer(&mut self) {
        if self.drawer.is_none() {
            self.drawer = Some(Drawer { tab: 0, scroll: 0 });
            self.kick(FxKind::DrawerOpen, 0.32);
            self.refresh_devices();
            self.sound(Cue::Tick);
        }
    }

    pub fn close_drawer(&mut self) {
        if self.drawer.is_some() && self.fx(&FxKind::DrawerClose).is_none() {
            self.kick(FxKind::DrawerClose, 0.24);
        }
    }

    fn activate(&mut self, act: Act, rel: f32) {
        self.dirty = true;
        match act {
            Act::Connect => {
                if self.conn == Conn::Offline {
                    self.start_connect(None);
                }
            }
            Act::Recent(i) => {
                if let Some(a) = self.cfg.recent.get(i).cloned() {
                    self.texts[0] = a.clone();
                    self.win.push(WinAct::SetText(0, a));
                    self.start_connect(None);
                }
            }
            Act::TrustKey => {
                if let Some((addr, _, _)) = self.key_change.take() {
                    self.cfg.known_servers.remove(&addr);
                    self.cfg.verified_servers.remove(&addr);
                    self.start_connect(Some(addr));
                }
            }
            Act::CancelKey => {
                self.key_change = None;
                self.error = None;
            }
            Act::Join(id) => {
                if id != self.current && self.conn == Conn::Connected {
                    let _ = self.tx.send(Command::Join(id));
                    self.sound(Cue::Tick);
                }
            }
            Act::User(id) => {
                self.card = Some(id);
                self.kick(FxKind::Card, 0.24);
                self.sound(Cue::Tick);
            }
            Act::CardClose => self.card = None,
            Act::CardVolume | Act::CardMute => {
                if let Some(u) = self.card.and_then(|id| self.users.get(&id).cloned()) {
                    let cur = self.cfg.user_volumes.get(&u.name).copied().unwrap_or(1.0);
                    let gain = if act == Act::CardMute {
                        if cur < 0.01 {
                            1.0
                        } else {
                            0.0
                        }
                    } else {
                        (rel * 20.0).round() / 10.0
                    };
                    self.audio.set_user_gain(u.id, gain);
                    if (gain - 1.0).abs() < 0.02 {
                        self.cfg.user_volumes.remove(&u.name);
                    } else {
                        self.cfg.user_volumes.insert(u.name.clone(), gain);
                    }
                    self.cfg.save();
                }
            }
            Act::CardPoke => {
                if let Some(id) = self.card {
                    let _ = self.tx.send(Command::Poke { target: id, text: String::new() });
                    self.kick(FxKind::Poke(id), 0.9);
                    self.sound(Cue::Beep);
                }
            }
            Act::Dock(k) => {
                self.kick(FxKind::Dock(k), 0.28);
                match k {
                    DOCK_MIC => {
                        if self.deafened {
                            self.deafened = false;
                            self.muted = false;
                        } else {
                            self.muted = !self.muted;
                        }
                        self.apply_mute();
                    }
                    DOCK_DEAF => {
                        self.deafened = !self.deafened;
                        self.muted = self.deafened;
                        self.apply_mute();
                    }
                    DOCK_AWAY => {
                        self.away = !self.away;
                        self.apply_mute();
                    }
                    DOCK_CHAT => self.chat_open = !self.chat_open,
                    DOCK_SETTINGS => self.open_drawer(),
                    DOCK_LEAVE => self.leave(),
                    _ => {}
                }
            }
            Act::OpenSettings => {
                if self.drawer.is_some() {
                    self.close_drawer();
                } else {
                    self.open_drawer();
                }
            }
            Act::CloseSettings => self.close_drawer(),
            Act::Tab(i) => {
                if let Some(d) = &mut self.drawer {
                    if d.tab != i {
                        d.tab = i;
                        d.scroll = 0;
                        self.kick(FxKind::Tab, 0.26);
                        self.sound(Cue::Tick);
                    }
                }
            }
            Act::Toggle(key) => self.toggle(key),
            Act::Radio(group, i) => self.radio(group, i),
            Act::Slider(key) => self.slider(key, rel),
            Act::Choice(key, i) => self.choice(key, i),
            Act::Verify => {
                if !self.address.is_empty() {
                    self.cfg.verified_servers.insert(self.address.clone());
                    self.cfg.save();
                    self.verify_done = true;
                    self.kick(FxKind::Verify, 1.5);
                    self.toast(l("Fingerabdruck bestätigt", "Fingerprint confirmed").into());
                    self.sound(Cue::Granted);
                }
            }
            Act::Minimize => self.win.push(WinAct::Minimize),
            Act::Maximize => self.win.push(WinAct::Maximize),
            Act::Close => self.request_close(),
            Act::Skip => self.skip(),
            Act::Field(i) => self.win.push(WinAct::Focus(i as i32)),
            Act::Dismiss => self.fx.retain(|f| f.kind != FxKind::Prio && f.kind != FxKind::Briefing),
            Act::Swallow => {}
        }
    }

    fn confirm(&mut self, what: String) {
        self.toast(what);
        self.sound(Cue::Tick);
    }

    fn toggle(&mut self, key: &'static str) {
        let c = &mut self.cfg;
        let (label, v) = match key {
            "agc" => {
                c.auto_gain = !c.auto_gain;
                (l("Automatische Lautstärke", "Automatic gain"), c.auto_gain)
            }
            "aec" => {
                c.echo_cancel = !c.echo_cancel;
                (l("Echo-Unterdrückung", "Echo cancellation"), c.echo_cancel)
            }
            "ns" => {
                c.noise_suppression = !c.noise_suppression;
                (l("Hintergrundgeräusche entfernen", "Noise suppression"), c.noise_suppression)
            }
            "autosens" => {
                c.auto_threshold = !c.auto_threshold;
                (l("Automatische Empfindlichkeit", "Automatic sensitivity"), c.auto_threshold)
            }
            "test" => {
                let v = !self.audio.shared.mic_test.load(Ordering::Relaxed);
                self.audio.shared.mic_test.store(v, Ordering::Relaxed);
                (l("Mikrofon testen", "Mic test"), v)
            }
            "sounds" => {
                c.terminal_sounds = !c.terminal_sounds;
                (l("Terminal-Klänge", "Terminal sounds"), c.terminal_sounds)
            }
            "wave" => {
                c.anim_wave = !c.anim_wave;
                (l("Server-Welle in Bewegung", "Moving server wave"), c.anim_wave)
            }
            "connect" => {
                c.anim_connect = !c.anim_connect;
                (l("Verbindungs-Sequenz", "Connection sequence"), c.anim_connect)
            }
            "start" => {
                c.anim_start = !c.anim_start;
                (l("Start und Beenden", "Start and quit"), c.anim_start)
            }
            "join" => {
                c.anim_join = !c.anim_join;
                (l("Beitreten und Verlassen", "Join and leave"), c.anim_join)
            }
            "switch" => {
                c.anim_switch = !c.anim_switch;
                (l("Kanalwechsel", "Channel switch"), c.anim_switch)
            }
            _ => return,
        };
        self.kick(FxKind::Toggle(key), 0.32);
        self.confirm(format!("{label}: {}", if v { l("an", "on") } else { l("aus", "off") }));
        self.apply_settings();
    }

    fn radio(&mut self, group: &'static str, i: i32) {
        let label = match group {
            "out" => {
                let name = if i == 0 { None } else { self.outputs.get(i as usize - 1).cloned() };
                if self.cfg.output_device == name {
                    return;
                }
                self.cfg.output_device = name.clone();
                self.audio.update_settings(|a| a.output_device = name.clone());
                self.audio.restart_devices();
                name.unwrap_or_else(|| l("Systemstandard", "System default").into())
            }
            "in" => {
                let name = if i == 0 { None } else { self.inputs.get(i as usize - 1).cloned() };
                if self.cfg.input_device == name {
                    return;
                }
                self.cfg.input_device = name.clone();
                self.audio.update_settings(|a| a.input_device = name.clone());
                self.audio.restart_devices();
                name.unwrap_or_else(|| l("Systemstandard", "System default").into())
            }
            "mode" => {
                let m = VoiceMode::from_index(i);
                if self.cfg.voice_mode == m {
                    return;
                }
                self.cfg.voice_mode = m;
                [l("Sprachaktivierung", "Voice activation"), "Push-to-Talk", l("Dauerhaft senden", "Always on")][i as usize % 3].into()
            }
            "lang" => {
                let v = ["auto", "en", "de"][i as usize % 3];
                if self.cfg.language == v {
                    return;
                }
                self.cfg.language = v.into();
                i18n::apply(v);
                ["Auto", "English", "Deutsch"][i as usize % 3].into()
            }
            _ => return,
        };
        self.kick(FxKind::Radio(group, i), 0.3);
        self.confirm(label);
        self.apply_settings();
    }

    fn slider(&mut self, key: &'static str, rel: f32) {
        let (from, label, unit) = match key {
            "vol" => {
                let from = self.cfg.output_volume * 100.0;
                self.cfg.output_volume = (rel * 2.0 * 20.0).round() / 20.0;
                (from, l("Lautstärke", "Volume"), self.cfg.output_volume * 100.0)
            }
            "gain" => {
                let from = self.cfg.input_gain * 100.0;
                self.cfg.input_gain = (rel * 4.0 * 20.0).round() / 20.0;
                (from, l("Verstärkung", "Gain"), self.cfg.input_gain * 100.0)
            }
            "sens" => {
                let from = self.cfg.activation_threshold * 100.0;
                self.cfg.activation_threshold = (rel * 20.0).round() / 20.0;
                (from, l("Empfindlichkeit", "Sensitivity"), self.cfg.activation_threshold * 100.0)
            }
            _ => return,
        };
        let f = self.kick(FxKind::Slider(key), 0.28);
        f.from = from;
        self.confirm(format!("{label} {:.0} %", unit));
        self.apply_settings();
    }

    fn choice(&mut self, key: &'static str, i: i32) {
        let label = match key {
            "ptt" => {
                let k = PTT_KEYS[(i as usize).min(PTT_KEYS.len() - 1)];
                if self.cfg.ptt_key == k.0 {
                    return;
                }
                self.cfg.ptt_key = k.0.into();
                format!("{}: {}", l("Push-to-Talk-Taste", "Push-to-talk key"), k.0)
            }
            "quality" => {
                let b = BITRATES[(i as usize).min(2)];
                if self.cfg.opus_bitrate == b {
                    return;
                }
                self.cfg.opus_bitrate = b;
                format!("{}: {} kbit/s", l("Sprachqualität", "Voice quality"), b / 1000)
            }
            _ => return,
        };
        self.kick(FxKind::Choice(key), 0.3);
        self.confirm(label);
        self.apply_settings();
    }

    // ── Kanalwechsel und Verlassen (Schnappschüsse fürs Zeichnen)

    fn start_switch(&mut self, target: u32) {
        let mut old = Grid::new(self.rows, self.cols);
        let layout = self.draw_net(&mut old, true, None);
        let pos = layout.ch.iter().find(|c| c.id == target).map(|c| (c.r as f32, c.c as f32)).unwrap_or((24.0, 38.0));
        let (name, code) = (self.channel_name(target), self.channel_code(target));
        let f = self.kick(FxKind::Switch, 1.15);
        f.grid = Some(Box::new(old));
        f.pos = pos;
        f.text = name;
        f.text2 = code;
    }

    fn start_leave(&mut self, id: u32) {
        if !self.cfg.anim_join || !matches!(self.scene, Scene::Main) {
            return;
        }
        let mut g = Grid::new(self.rows, self.cols);
        let layout = self.draw_net(&mut g, false, None);
        if let Some(u) = layout.users.iter().find(|u| u.id == id) {
            let mut node = Grid::new(self.rows, self.cols);
            node.copy_from(&g, u.r - 1, u.r + 2, u.c - 16, u.c + 16);
            let f = self.kick(FxKind::Leave, 0.8);
            f.grid = Some(Box::new(node));
            f.pos = (u.r as f32, u.c as f32);
        }
    }

    /// Text field positions in logical pixels: (shown, x, y, w)
    pub fn field_px(&self, i: usize) -> (bool, f32, f32, f32) {
        let f = self.fields[i];
        (f.shown, f.c as f32 * tidespeak::render::CELL_W, f.r as f32 * tidespeak::render::CELL_H, f.w as f32 * tidespeak::render::CELL_W)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let cfg = Config::default();
        let audio = Arc::new(Audio::start(audio_settings(&cfg)));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(rx);
        App::new(cfg, audio, tx)
    }

    fn user(id: u32, name: &str, channel: u32) -> UserInfo {
        UserInfo { id, name: name.into(), channel, muted: id % 4 == 0, deafened: false, away: id % 5 == 0 }
    }

    fn connected() -> App {
        let mut a = app();
        let channels = ["Lobby", "Gaming", "Musik", "AFK", "Ein sehr langer Kanalname"]
            .iter()
            .enumerate()
            .map(|(i, n)| ChannelInfo { id: i as u32, name: n.to_string(), topic: String::new() })
            .collect();
        let mut users = vec![user(1, "Tim", 0)];
        for i in 2..14 {
            users.push(user(i, &format!("Person mit langem Namen {i}"), i % 4));
        }
        a.net_event(Event::Connected {
            address: "127.0.0.1:9987".into(),
            fingerprint: "e0d4d370db730d8386fe103bf2cd8201aa".into(),
            you: 1,
            server_name: "Frametide Crew".into(),
            motd: "Willkommen! Bitte Push-to-Talk benutzen.".into(),
            channels,
            users,
        });
        a
    }

    const SIZES: [(i32, i32); 4] = [(45, 170), (56, 180), (72, 240), (30, 100)];

    #[test]
    fn every_scene_draws_at_any_size() {
        let mut a = app();
        for (rows, cols) in SIZES {
            let mut f = 0.0;
            while f < 130.0 {
                a.scene = Scene::Boot { f };
                a.draw(rows, cols);
                f += 7.0;
            }
            a.scene = Scene::Connect;
            a.error = Some("Server antwortet nicht. Läuft der Server, ist UDP-Port 9987 freigegeben?".into());
            a.draw(rows, cols);
            a.key_change = Some(("127.0.0.1:9987".into(), "AAAA BBBB".into(), "CCCC DDDD".into()));
            a.draw(rows, cols);
            a.key_change = None;
        }

        let mut a = connected();
        for (rows, cols) in SIZES {
            for fast in [false, true] {
                let mut f = 0.0;
                while f < 175.0 {
                    a.scene = Scene::Sequence { f, fast, ok: true, fail: None };
                    a.draw(rows, cols);
                    a.scene = Scene::Sequence { f, fast, ok: false, fail: (f > 60.0).then(|| "Zeitüberschreitung".to_string()) };
                    a.draw(rows, cols);
                    f += 6.0;
                }
            }
            a.scene = Scene::Main;
            a.speaking.insert(2);
            a.card = Some(2);
            a.kick(FxKind::Lock(3), 2.4);
            a.kick(FxKind::Poke(2), 0.9);
            a.kick(FxKind::Prio, 4.2).text2 = "x".repeat(300);
            a.kick(FxKind::Dock(DOCK_MIC), 0.3);
            a.kick(FxKind::Decrypt, 0.7);
            a.start_switch(1);
            a.start_leave(5);
            for i in 0..30 {
                a.on_chat(ChatLine { channel: a.current, from: 2 + i % 3, name: "Mia".into(), text: format!("@Tim Nachricht {i} {}", "lang ".repeat(i as usize)), time: 1_700_000_000 });
            }
            a.chat_scroll = 1000;
            a.draw(rows, cols);
            for tab in 0..6 {
                a.drawer = Some(Drawer { tab, scroll: 50 });
                a.kick(FxKind::DrawerOpen, 0.3);
                a.draw(rows, cols);
            }
            a.drawer = None;
            let mut f = 0.0;
            while f < 120.0 {
                a.scene = Scene::Disconnect { f, reason: (f > 50.0).then(|| "Verbindung verloren".to_string()) };
                a.draw(rows, cols);
                a.scene = Scene::Quit { f: f / 2.0 };
                a.draw(rows, cols);
                f += 5.0;
            }
            a.scene = Scene::Main;
        }
    }

    #[test]
    fn layout_keeps_nodes_apart() {
        let mut a = connected();
        a.users.retain(|id, _| *id < 8);
        a.draw(56, 180);
        let lay = a.layout();
        for (i, p) in lay.users.iter().enumerate() {
            for q in &lay.users[i + 1..] {
                let apart = (p.r - q.r).abs() >= 3 || (p.c - q.c).abs() >= 6;
                assert!(apart, "Knoten {} und {} überlappen", p.id, q.id);
            }
            assert!(p.r >= 4 && p.r < 56 && p.c >= 1 && p.c < 180 - a.chat_width(), "Knoten {} außerhalb", p.id);
        }
    }
}
