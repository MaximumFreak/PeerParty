//! The stand-in for systems without a port yet: compiles anywhere, finds little.

use std::path::{Path, PathBuf};

/// The user's languages from the usual locale variables ("de_DE.UTF-8" reads as "de-DE").
pub fn languages() -> Vec<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .flat_map(|v| v.split(':').map(|s| s.split('.').next().unwrap_or_default().replace('_', "-")).collect::<Vec<_>>())
        .filter(|s| !s.is_empty() && s != "C" && s != "POSIX")
        .collect()
}

pub fn ui_fonts() -> super::UiFonts {
    super::UiFonts::default()
}

pub fn fallback_font(_c: char, _lang: &str) -> Option<super::FontFile> {
    None
}

pub fn animations_enabled() -> bool {
    true
}

pub fn background(_on: bool) {}

pub fn user_name() -> String {
    std::env::var("USER").unwrap_or_default()
}

pub fn logs_dir() -> PathBuf {
    std::env::temp_dir().join("peerparty").join("logs")
}

pub fn exe_icon(_exe: &Path, _size: i32) -> Option<super::Rgba> {
    None
}

pub fn reveal(path: &Path) {
    let dir = if path.is_file() { path.parent().unwrap_or(path) } else { path };
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

pub fn quiet(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd
}

pub fn in_front() -> bool {
    true
}
