//! Settings: `~/.config/tidespeak/config.toml` (Linux) or `%APPDATA%\Tidespeak\config.toml` (Windows).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VoiceMode {
    /// Sends automatically while you speak
    #[default]
    Activity,
    /// Sends only while the push-to-talk key is held
    PushToTalk,
    /// Always sends (open mic)
    Continuous,
}

impl VoiceMode {
    pub fn index(self) -> i32 {
        match self {
            VoiceMode::Activity => 0,
            VoiceMode::PushToTalk => 1,
            VoiceMode::Continuous => 2,
        }
    }
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => VoiceMode::PushToTalk,
            2 => VoiceMode::Continuous,
            _ => VoiceMode::Activity,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub name: String,
    pub address: String,
    /// Most recent servers first
    pub recent: Vec<String>,
    /// Server address -> pinned certificate fingerprint (trust on first use)
    pub known_servers: BTreeMap<String, String>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_gain: f32,
    pub output_volume: f32,
    pub voice_mode: VoiceMode,
    /// Voice activation threshold, 0.0 (very sensitive) … 1.0
    pub activation_threshold: f32,
    pub noise_suppression: bool,
    /// Automatic mic level (AGC)
    pub auto_gain: bool,
    /// Echo cancellation (useful with speakers instead of headphones)
    pub echo_cancel: bool,
    /// Voice activation sensitivity follows the background noise automatically
    pub auto_threshold: bool,
    pub opus_bitrate: i32,
    pub ptt_key: String,
    /// Volume per person (by name, since ids change on every connect)
    pub user_volumes: BTreeMap<String, f32>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: default_name(),
            address: "127.0.0.1".into(),
            recent: Vec::new(),
            known_servers: BTreeMap::new(),
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_volume: 1.0,
            voice_mode: VoiceMode::Activity,
            activation_threshold: 0.25,
            noise_suppression: true,
            auto_gain: true,
            echo_cancel: true,
            auto_threshold: true,
            opus_bitrate: 64_000,
            ptt_key: "F2".into(),
            user_volumes: BTreeMap::new(),
        }
    }
}

fn default_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "Gast".into())
}

fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "Tidespeak").map(|d| d.config_dir().join("config.toml"))
}

impl Config {
    pub fn load() -> Config {
        let mut c: Config = path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        // Version 3.0 speicherte 48 kbit/s als Standard – auf die neuen Stufen heben
        if ![32_000, 64_000, 96_000].contains(&c.opus_bitrate) {
            c.opus_bitrate = 64_000;
        }
        c
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match toml::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&p, s) {
                    log::warn!("Einstellungen nicht gespeichert: {e}");
                }
            }
            Err(e) => log::warn!("Einstellungen nicht gespeichert: {e}"),
        }
    }

    pub fn remember_server(&mut self, address: &str) {
        self.recent.retain(|a| a != address);
        self.recent.insert(0, address.to_string());
        self.recent.truncate(6);
    }
}

/// `host`, `host:port`, `[v6]:port` -> canonical form with port
pub fn normalize_address(input: &str) -> String {
    let s = input.trim();
    let s = if s.is_empty() { "127.0.0.1" } else { s };
    let has_port = if s.starts_with('[') {
        s.contains("]:")
    } else {
        s.matches(':').count() == 1
    };
    if has_port {
        s.to_string()
    } else if s.contains(':') && !s.starts_with('[') {
        format!("[{s}]:{}", tidespeak_proto::DEFAULT_PORT)
    } else {
        format!("{s}:{}", tidespeak_proto::DEFAULT_PORT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(normalize_address(""), "127.0.0.1:9987");
        assert_eq!(normalize_address("example.org"), "example.org:9987");
        assert_eq!(normalize_address("10.0.0.2:1234"), "10.0.0.2:1234");
        assert_eq!(normalize_address("::1"), "[::1]:9987");
        assert_eq!(normalize_address("[::1]:5"), "[::1]:5");
    }
}
