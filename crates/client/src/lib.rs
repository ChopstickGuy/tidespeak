//! Tidespeak client: network, audio, settings and the character-grid renderer.
//! The interface logic lives in the `tidespeak` binary (`main.rs`, `ui/`).

pub mod audio;
pub mod config;
pub mod dsp;
pub mod grid;
pub mod i18n;
pub mod jitter;
pub mod net;
pub mod render;
pub mod tofu;
