// Release builds on Windows: no console window
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

slint::include_modules!();

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::platform::Key;
use slint::winit_030::{winit, WinitWindowAccessor};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tidespeak::audio::{self, Audio, AudioSettings, Cue};
use tidespeak::config::{normalize_address, Config, VoiceMode};
use tidespeak::net::{self, Command, Event};
use tidespeak_proto::{fingerprint_short, sanitize_name, ChannelInfo, ChatLine, UserInfo};
use tokio::sync::mpsc::UnboundedSender;

/// Push-to-talk keys: (label, Slint key, Windows virtual key)
const PTT_KEYS: [(&str, Key, u32); 8] = [
    ("F2", Key::F2, 0x71),
    ("F3", Key::F3, 0x72),
    ("F4", Key::F4, 0x73),
    ("F5", Key::F5, 0x74),
    ("F6", Key::F6, 0x75),
    ("F7", Key::F7, 0x76),
    ("F8", Key::F8, 0x77),
    ("F9", Key::F9, 0x78),
];
const BITRATES: [i32; 3] = [24_000, 48_000, 96_000];
const AVATAR_COLORS: [u32; 10] =
    [0x3584e4, 0x2ec27e, 0xe5a50a, 0xe66100, 0xc061cb, 0x1c9fb0, 0x9141ac, 0xd5375a, 0x26a269, 0x865e3c];
const MAX_LOG: usize = 400;

/// Everything the UI thread knows
struct State {
    cfg: Config,
    you: u32,
    connected_address: String,
    server_name: String,
    channels: Vec<ChannelInfo>,
    users: BTreeMap<u32, UserInfo>,
    current: u32,
    logs: HashMap<u32, Vec<Entry>>,
    speaking: HashSet<u32>,
    key_change_address: Option<String>,
    input_names: Vec<String>,
    output_names: Vec<String>,
    messages: Rc<VecModel<MessageItem>>,
    channel_model: Rc<VecModel<ChannelItem>>,
}

#[derive(Clone)]
struct Entry {
    from: Option<u32>,
    author: String,
    text: String,
    time: u64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(s.borrow_mut().as_mut().expect("Zustand")))
}

fn avatar_color(name: &str) -> Color {
    let mut h: u32 = 0x811c9dc5;
    for b in name.to_lowercase().bytes() {
        h = (h ^ b as u32).wrapping_mul(0x01000193);
    }
    Color::from_argb_encoded(0xff000000 | AVATAR_COLORS[h as usize % AVATAR_COLORS.len()])
}

fn initial(name: &str) -> SharedString {
    name.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default().into()
}

fn clock(unix: u64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(unix as i64, 0) {
        chrono::LocalResult::Single(t) => t.format("%H:%M").to_string(),
        _ => String::new(),
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn audio_settings(cfg: &Config) -> AudioSettings {
    let ptt = PTT_KEYS.iter().position(|k| k.0 == cfg.ptt_key).unwrap_or(0);
    AudioSettings {
        input_device: cfg.input_device.clone(),
        output_device: cfg.output_device.clone(),
        input_gain: cfg.input_gain,
        output_volume: cfg.output_volume,
        voice_mode: cfg.voice_mode,
        activation_threshold: cfg.activation_threshold,
        noise_suppression: cfg.noise_suppression,
        bitrate: cfg.opus_bitrate,
        ptt_key: PTT_KEYS[ptt].2,
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,tidespeak=info"))
        .format_target(false)
        .init();

    let cfg = Config::load();
    let audio = Arc::new(Audio::start(audio_settings(&cfg)));
    AUDIO_FOR_CUES.with(|a| *a.borrow_mut() = Some(audio.clone()));
    let window = MainWindow::new().expect("Fenster");
    // Wayland/X11: passt zu tidespeak.desktop, damit GNOME Symbol und Fenster verbindet
    let _ = slint::set_xdg_app_id("tidespeak");

    let weak = window.as_weak();
    let net_tx = net::spawn(audio.clone(), move |ev| {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                on_event(&w, ev);
            }
        });
    });

    let messages = Rc::new(VecModel::<MessageItem>::default());
    let channel_model = Rc::new(VecModel::<ChannelItem>::default());
    window.set_messages(ModelRc::from(messages.clone()));
    window.set_channels(ModelRc::from(channel_model.clone()));

    // Einstellungen ins Fenster
    window.set_version(env!("CARGO_PKG_VERSION").into());
    window.set_address(cfg.address.clone().into());
    window.set_nick(cfg.name.clone().into());
    window.set_input_gain(cfg.input_gain / 4.0);
    window.set_output_volume(cfg.output_volume / 2.0);
    window.set_threshold(cfg.activation_threshold);
    window.set_voice_mode(cfg.voice_mode.index());
    window.set_noise_suppression(cfg.noise_suppression);
    window.set_quality_index(BITRATES.iter().position(|b| *b == cfg.opus_bitrate).unwrap_or(1) as i32);
    let keys: Vec<SharedString> = PTT_KEYS.iter().map(|k| k.0.into()).collect();
    window.set_ptt_keys(ModelRc::new(VecModel::from(keys)));
    let ptt_index = PTT_KEYS.iter().position(|k| k.0 == cfg.ptt_key).unwrap_or(0);
    window.set_ptt_key_index(ptt_index as i32);
    window.set_ptt_key_name(PTT_KEYS[ptt_index].0.into());

    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            cfg,
            you: 0,
            connected_address: String::new(),
            server_name: String::new(),
            channels: Vec::new(),
            users: BTreeMap::new(),
            current: 0,
            logs: HashMap::new(),
            speaking: HashSet::new(),
            key_change_address: None,
            input_names: Vec::new(),
            output_names: Vec::new(),
            messages,
            channel_model,
        })
    });
    refresh_me(&window);
    refresh_recents(&window);

    wire_callbacks(&window, &audio, &net_tx);

    // Regelmäßige, günstige Aktualisierung: wer spricht, Pegel, Fensterzustand
    let timer = slint::Timer::default();
    {
        let weak = window.as_weak();
        let audio = audio.clone();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(80), move || {
            let Some(w) = weak.upgrade() else { return };
            tick(&w, &audio);
        });
    }

    window.run().expect("Ereignisschleife");
    with_state(|s| s.cfg.save());
    let _ = net_tx.send(Command::Disconnect);
    std::thread::sleep(Duration::from_millis(100));
}

fn wire_callbacks(window: &MainWindow, audio: &Arc<Audio>, net_tx: &UnboundedSender<Command>) {
    // ── Verbindung
    {
        let weak = window.as_weak();
        let tx = net_tx.clone();
        window.on_connect(move || {
            let Some(w) = weak.upgrade() else { return };
            start_connect(&w, &tx, None);
        });
    }
    {
        let weak = window.as_weak();
        let tx = net_tx.clone();
        window.on_connect_recent(move |addr| {
            let Some(w) = weak.upgrade() else { return };
            w.set_address(addr.clone());
            start_connect(&w, &tx, None);
        });
    }
    {
        let tx = net_tx.clone();
        window.on_cancel(move || {
            let _ = tx.send(Command::Disconnect);
        });
    }
    {
        let tx = net_tx.clone();
        window.on_disconnect(move || {
            let _ = tx.send(Command::Disconnect);
        });
    }
    {
        let weak = window.as_weak();
        let tx = net_tx.clone();
        window.on_trust_new_key(move || {
            let Some(w) = weak.upgrade() else { return };
            let addr = with_state(|s| s.key_change_address.take());
            if let Some(addr) = addr {
                with_state(|s| s.cfg.known_servers.remove(&addr));
                w.set_key_changed(false);
                start_connect(&w, &tx, Some(addr));
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_forget_error(move || {
            let Some(w) = weak.upgrade() else { return };
            w.set_key_changed(false);
            w.set_error_text("".into());
        });
    }

    // ── Kanäle und Chat
    {
        let tx = net_tx.clone();
        window.on_join_channel(move |id| {
            let current = with_state(|s| s.current);
            if id as u32 != current {
                let _ = tx.send(Command::Join(id as u32));
            }
        });
    }
    {
        let tx = net_tx.clone();
        window.on_send_chat(move |text| {
            let t = text.trim();
            if !t.is_empty() {
                let _ = tx.send(Command::Chat(t.to_string()));
            }
        });
    }

    // ── Stumm / Ton aus
    {
        let weak = window.as_weak();
        let tx = net_tx.clone();
        let audio = audio.clone();
        window.on_toggle_mute(move || {
            let Some(w) = weak.upgrade() else { return };
            let deaf = w.get_deafened();
            let muted = if deaf {
                // Ton wieder an schaltet auch das Mikro frei
                w.set_deafened(false);
                false
            } else {
                !w.get_muted()
            };
            w.set_muted(muted);
            apply_mute(&w, &audio, &tx);
        });
    }
    {
        let weak = window.as_weak();
        let tx = net_tx.clone();
        let audio = audio.clone();
        window.on_toggle_deafen(move || {
            let Some(w) = weak.upgrade() else { return };
            let deaf = !w.get_deafened();
            w.set_deafened(deaf);
            w.set_muted(deaf);
            apply_mute(&w, &audio, &tx);
        });
    }

    // ── Einstellungen
    {
        let weak = window.as_weak();
        let audio = audio.clone();
        window.on_refresh_devices(move || {
            let Some(w) = weak.upgrade() else { return };
            let (inputs, outputs) = audio::list_devices();
            with_state(|s| {
                s.input_names = inputs;
                s.output_names = outputs;
            });
            refresh_devices(&w, &audio);
        });
    }
    {
        let weak = window.as_weak();
        let audio = audio.clone();
        window.on_select_input(move |i| {
            let Some(w) = weak.upgrade() else { return };
            with_state(|s| {
                s.cfg.input_device = if i == 0 { None } else { s.input_names.get(i as usize - 1).cloned() };
                s.cfg.save();
                let dev = s.cfg.input_device.clone();
                audio.update_settings(|a| a.input_device = dev);
            });
            audio.restart_devices();
            refresh_devices(&w, &audio);
        });
    }
    {
        let weak = window.as_weak();
        let audio = audio.clone();
        window.on_select_output(move |i| {
            let Some(w) = weak.upgrade() else { return };
            with_state(|s| {
                s.cfg.output_device = if i == 0 { None } else { s.output_names.get(i as usize - 1).cloned() };
                s.cfg.save();
                let dev = s.cfg.output_device.clone();
                audio.update_settings(|a| a.output_device = dev);
            });
            audio.restart_devices();
            refresh_devices(&w, &audio);
        });
    }
    {
        let weak = window.as_weak();
        let audio = audio.clone();
        window.on_settings_changed(move || {
            let Some(w) = weak.upgrade() else { return };
            let key = PTT_KEYS[(w.get_ptt_key_index().max(0) as usize).min(PTT_KEYS.len() - 1)];
            w.set_ptt_key_name(key.0.into());
            audio.shared.mic_test.store(w.get_mic_test(), Ordering::Relaxed);
            with_state(|s| {
                let c = &mut s.cfg;
                c.input_gain = w.get_input_gain() * 4.0;
                c.output_volume = w.get_output_volume() * 2.0;
                c.activation_threshold = w.get_threshold();
                c.voice_mode = VoiceMode::from_index(w.get_voice_mode());
                c.noise_suppression = w.get_noise_suppression();
                c.opus_bitrate = BITRATES[(w.get_quality_index().max(0) as usize).min(2)];
                c.ptt_key = key.0.to_string();
                c.save();
                let a = audio_settings(c);
                audio.update_settings(|s| *s = a);
            });
        });
    }
    {
        let audio = audio.clone();
        window.on_key_event(move |text, down| {
            let (mode, key) = with_state(|s| (s.cfg.voice_mode, s.cfg.ptt_key.clone()));
            if mode != VoiceMode::PushToTalk {
                return false;
            }
            let Some(k) = PTT_KEYS.iter().find(|k| k.0 == key) else { return false };
            if text == SharedString::from(k.1) {
                audio.shared.ptt.store(down, Ordering::Relaxed);
                return true;
            }
            false
        });
    }

    // ── Fenster (eigene Kopfleiste)
    {
        let weak = window.as_weak();
        window.on_start_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww: &winit::window::Window| {
                    let _ = ww.drag_window();
                });
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_start_resize(move |dir| {
            use winit::window::ResizeDirection as D;
            let d = match dir {
                0 => D::North,
                1 => D::NorthEast,
                2 => D::East,
                3 => D::SouthEast,
                4 => D::South,
                5 => D::SouthWest,
                6 => D::West,
                _ => D::NorthWest,
            };
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww: &winit::window::Window| {
                    let _ = ww.drag_resize_window(d);
                });
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_toggle_maximize(move || {
            if let Some(w) = weak.upgrade() {
                let m = !w.window().is_maximized();
                w.window().set_maximized(m);
                w.set_is_maximized(m);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_minimize(move || {
            if let Some(w) = weak.upgrade() {
                w.window().set_minimized(true);
            }
        });
    }
    window.on_close_window(|| {
        let _ = slint::quit_event_loop();
    });
}

fn apply_mute(w: &MainWindow, audio: &Audio, tx: &UnboundedSender<Command>) {
    let (muted, deafened) = (w.get_muted(), w.get_deafened());
    let was = audio.shared.muted.swap(muted, Ordering::Relaxed);
    audio.shared.deafened.store(deafened, Ordering::Relaxed);
    if was != muted {
        audio.cue(if muted { Cue::Mute } else { Cue::Unmute });
    }
    let _ = tx.send(Command::State { muted, deafened });
}

fn start_connect(w: &MainWindow, tx: &UnboundedSender<Command>, forced_address: Option<String>) {
    let address = forced_address.unwrap_or_else(|| normalize_address(&w.get_address()));
    let name = sanitize_name(&w.get_nick());
    if name.is_empty() {
        w.set_error_text("Bitte gib einen Namen ein.".into());
        return;
    }
    w.set_error_text("".into());
    w.set_key_changed(false);
    let pinned = with_state(|s| {
        s.cfg.name = name.clone();
        s.cfg.address = w.get_address().trim().to_string();
        s.cfg.save();
        s.cfg.known_servers.get(&address).cloned()
    });
    refresh_me(w);
    let _ = tx.send(Command::Connect { address, name, pinned });
}

fn refresh_me(w: &MainWindow) {
    let name = w.get_nick();
    let name = if name.trim().is_empty() { SharedString::from("?") } else { name };
    w.set_me_initial(initial(&name));
    w.set_me_color(avatar_color(&name));
}

fn refresh_recents(w: &MainWindow) {
    let items: Vec<RecentItem> = with_state(|s| {
        s.cfg
            .recent
            .iter()
            .map(|a| RecentItem {
                address: a.clone().into(),
                subtitle: s
                    .cfg
                    .known_servers
                    .get(a)
                    .map(|fp| format!("Bekannter Server · {}", fingerprint_short(fp)))
                    .unwrap_or_default()
                    .into(),
            })
            .collect()
    });
    w.set_recents(ModelRc::new(VecModel::from(items)));
}

fn refresh_devices(w: &MainWindow, audio: &Audio) {
    let list = |names: &[String], selected: &Option<String>| -> ModelRc<DeviceItem> {
        let mut v = vec![DeviceItem { name: "Systemstandard".into(), selected: selected.is_none() }];
        v.extend(names.iter().map(|n| DeviceItem { name: n.into(), selected: selected.as_deref() == Some(n) }));
        ModelRc::new(VecModel::from(v))
    };
    with_state(|s| {
        w.set_input_devices(list(&s.input_names, &s.cfg.input_device));
        w.set_output_devices(list(&s.output_names, &s.cfg.output_device));
    });
    w.set_input_error(audio.shared.input_error.lock().unwrap().clone().unwrap_or_default().into());
    w.set_output_error(audio.shared.output_error.lock().unwrap().clone().unwrap_or_default().into());
}

fn tick(w: &MainWindow, audio: &Audio) {
    let transmitting = audio.shared.transmitting.load(Ordering::Relaxed);
    w.set_transmitting(transmitting);
    if w.get_page() == 2 {
        w.set_level(audio.shared.level());
    } else if w.get_mic_test() {
        // Mikrofontest endet beim Verlassen der Einstellungen
        w.set_mic_test(false);
        audio.shared.mic_test.store(false, Ordering::Relaxed);
    }
    let maximized = w.window().is_maximized();
    if w.get_is_maximized() != maximized {
        w.set_is_maximized(maximized);
    }
    let now = Instant::now();
    let mut speaking: HashSet<u32> = audio
        .shared
        .speaking
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, t)| now.duration_since(**t) < Duration::from_millis(350))
        .map(|(id, _)| *id)
        .collect();
    let changed = with_state(|s| {
        if transmitting && s.you != 0 {
            speaking.insert(s.you);
        }
        if speaking != s.speaking {
            s.speaking = speaking;
            true
        } else {
            false
        }
    });
    if changed {
        refresh_channels();
    }
}

fn refresh_channels() {
    with_state(|s| {
        let items: Vec<ChannelItem> = s
            .channels
            .iter()
            .map(|c| {
                let mut users: Vec<&UserInfo> = s.users.values().filter(|u| u.channel == c.id).collect();
                users.sort_by_key(|u| u.name.to_lowercase());
                let users: Vec<UserItem> = users
                    .into_iter()
                    .map(|u| UserItem {
                        id: u.id as i32,
                        name: u.name.clone().into(),
                        initial: initial(&u.name),
                        color: avatar_color(&u.name),
                        speaking: s.speaking.contains(&u.id),
                        muted: u.muted,
                        deafened: u.deafened,
                        me: u.id == s.you,
                    })
                    .collect();
                ChannelItem {
                    id: c.id as i32,
                    name: c.name.clone().into(),
                    current: c.id == s.current,
                    users: ModelRc::new(VecModel::from(users)),
                }
            })
            .collect();
        // Modell an Ort und Stelle aktualisieren statt neu zu setzen – kein Flackern
        if s.channel_model.row_count() == items.len() {
            for (i, it) in items.into_iter().enumerate() {
                s.channel_model.set_row_data(i, it);
            }
        } else {
            s.channel_model.set_vec(items);
        }
    });
}

fn message_item(prev: Option<&Entry>, e: &Entry) -> MessageItem {
    let system = e.from.is_none();
    let grouped = !system
        && prev.is_some_and(|p| p.from == e.from && e.time.saturating_sub(p.time) < 300);
    MessageItem {
        author: e.author.clone().into(),
        initial: initial(&e.author),
        color: avatar_color(&e.author),
        time: clock(e.time).into(),
        text: e.text.clone().into(),
        system,
        grouped,
    }
}

fn rebuild_messages(s: &State) {
    let log = s.logs.get(&s.current).map(|v| v.as_slice()).unwrap_or(&[]);
    let items: Vec<MessageItem> =
        log.iter().enumerate().map(|(i, e)| message_item(if i > 0 { log.get(i - 1) } else { None }, e)).collect();
    s.messages.set_vec(items);
}

fn push_entry(s: &mut State, channel: u32, e: Entry) {
    let log = s.logs.entry(channel).or_default();
    let item = message_item(log.last(), &e);
    log.push(e);
    let trimmed = log.len() > MAX_LOG;
    if trimmed {
        log.drain(..log.len() - MAX_LOG);
    }
    if channel == s.current {
        if trimmed {
            rebuild_messages(s);
        } else {
            s.messages.push(item);
        }
    }
}

fn system_line(s: &mut State, channel: u32, text: String) {
    push_entry(s, channel, Entry { from: None, author: String::new(), text, time: now_unix() });
}

fn chat_entry(c: &ChatLine) -> Entry {
    Entry { from: Some(c.from), author: c.name.clone(), text: c.text.clone(), time: c.time }
}

fn channel_name(s: &State, id: u32) -> String {
    s.channels.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_default()
}

fn on_event(w: &MainWindow, ev: Event) {
    match ev {
        Event::Connecting => {
            w.set_conn_state(1);
            w.set_error_text("".into());
        }
        Event::Connected { address, fingerprint, you, server_name, motd, channels, users } => {
            let new_server = with_state(|s| {
                let new = !s.cfg.known_servers.contains_key(&address);
                if new {
                    s.cfg.known_servers.insert(address.clone(), fingerprint.clone());
                }
                s.cfg.remember_server(&address);
                s.cfg.save();
                s.you = you;
                s.connected_address = address.clone();
                s.server_name = server_name.clone();
                s.channels = channels;
                s.users = users.into_iter().map(|u| (u.id, u)).collect();
                s.current = s.users.get(&you).map(|u| u.channel).unwrap_or(0);
                s.logs.clear();
                s.speaking.clear();
                new
            });
            w.set_conn_state(2);
            w.set_server_name(server_name.clone().into());
            w.set_server_address(address.clone().into());
            w.set_fingerprint(fingerprint_short(&fingerprint).into());
            w.set_page(1);
            with_state(|s| {
                let ch = s.current;
                w.set_channel_name(channel_name(s, ch).into());
                w.set_online_count(s.users.len() as i32);
                rebuild_messages(s);
                if !motd.is_empty() {
                    system_line(s, ch, motd.clone());
                }
                if new_server {
                    system_line(
                        s,
                        ch,
                        format!(
                            "Neuer Server – Fingerabdruck gespeichert: {}. Ändert er sich, warnt dich Tidespeak.",
                            fingerprint_short(&fingerprint)
                        ),
                    );
                }
            });
            refresh_channels();
            refresh_recents(w);
            w.invoke_refresh_devices();
            cue_for(w, Cue::Connected);
        }
        Event::Failed { reason, changed_key } => {
            w.set_conn_state(0);
            w.set_page(if w.get_page() == 2 { 2 } else { 0 });
            w.set_error_text(reason.into());
            if let Some((old, new)) = changed_key {
                w.set_key_old(fingerprint_short(&old).into());
                w.set_key_new(fingerprint_short(&new).into());
                w.set_key_changed(true);
                with_state(|s| s.key_change_address = Some(normalize_address(&w.get_address())));
            }
        }
        Event::Disconnected { reason } => {
            w.set_conn_state(0);
            if w.get_page() == 1 {
                w.set_page(0);
            }
            w.set_error_text(reason.into());
            w.set_fingerprint("".into());
            with_state(|s| {
                s.users.clear();
                s.channels.clear();
                s.you = 0;
                s.channel_model.set_vec(Vec::new());
            });
        }
        Event::UserJoined(u) => {
            let mine = with_state(|s| {
                let mine = u.channel == s.current;
                if mine {
                    system_line(s, u.channel, format!("{} ist da", u.name));
                }
                s.users.insert(u.id, u);
                w.set_online_count(s.users.len() as i32);
                mine
            });
            refresh_channels();
            if mine {
                cue_for(w, Cue::Join);
            }
        }
        Event::UserUpdated(u) => {
            let cue = with_state(|s| {
                let old = s.users.insert(u.id, u.clone());
                let moved = old.as_ref().is_some_and(|o| o.channel != u.channel);
                if u.id == s.you && moved {
                    s.current = u.channel;
                    w.set_channel_name(channel_name(s, u.channel).into());
                    rebuild_messages(s);
                    return Some(Cue::Join);
                }
                if moved {
                    let old_ch = old.map(|o| o.channel).unwrap_or(u32::MAX);
                    if u.channel == s.current {
                        system_line(s, u.channel, format!("{} ist dem Kanal beigetreten", u.name));
                        return Some(Cue::Join);
                    }
                    if old_ch == s.current {
                        system_line(s, old_ch, format!("{} hat den Kanal verlassen", u.name));
                        return Some(Cue::Leave);
                    }
                }
                None
            });
            refresh_channels();
            if let Some(c) = cue {
                cue_for(w, c);
            }
        }
        Event::UserLeft(id) => {
            let mine = with_state(|s| {
                let gone = s.users.remove(&id);
                w.set_online_count(s.users.len() as i32);
                match gone {
                    Some(u) if u.channel == s.current => {
                        system_line(s, u.channel, format!("{} hat den Server verlassen", u.name));
                        true
                    }
                    _ => false,
                }
            });
            refresh_channels();
            if mine {
                cue_for(w, Cue::Leave);
            }
        }
        Event::Chat(c) => with_state(|s| push_entry(s, c.channel, chat_entry(&c))),
        Event::History { channel, lines } => with_state(|s| {
            // Verlauf vom Server zuerst, eigene Hinweise (Begrüßung usw.) bleiben danach erhalten
            let mut log: Vec<Entry> = lines.iter().map(chat_entry).collect();
            if let Some(old) = s.logs.remove(&channel) {
                log.extend(old.into_iter().filter(|e| e.from.is_none()));
            }
            s.logs.insert(channel, log);
            if channel == s.current {
                rebuild_messages(s);
            }
        }),
    }
}

fn cue_for(_w: &MainWindow, cue: Cue) {
    AUDIO_FOR_CUES.with(|a| {
        if let Some(a) = a.borrow().as_ref() {
            a.cue(cue);
        }
    });
}

thread_local! {
    static AUDIO_FOR_CUES: RefCell<Option<Arc<Audio>>> = const { RefCell::new(None) };
}
