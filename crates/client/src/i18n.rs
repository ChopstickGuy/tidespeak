//! Interface language: English by default, German when the operating system
//! is set to German – or whatever the person picks in the settings.

use std::sync::atomic::{AtomicBool, Ordering};

static GERMAN: AtomicBool = AtomicBool::new(false);

/// Setting from the config: "auto", "en" or "de"
pub fn apply(setting: &str) {
    let german = match setting {
        "de" => true,
        "en" => false,
        _ => system_is_german(),
    };
    GERMAN.store(german, Ordering::Relaxed);
}

pub fn system_is_german() -> bool {
    sys_locale::get_locales().next().is_some_and(|l| l.to_lowercase().starts_with("de"))
}

pub fn is_german() -> bool {
    GERMAN.load(Ordering::Relaxed)
}

/// Picks the German or the English text
pub fn l(de: &'static str, en: &'static str) -> &'static str {
    if is_german() {
        de
    } else {
        en
    }
}

/// Like [`l`] for texts with values: `lf(format!(..), format!(..))`
pub fn lf(de: String, en: String) -> String {
    if is_german() {
        de
    } else {
        en
    }
}
