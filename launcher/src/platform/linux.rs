//! Linux's side of `platform` (other Unix systems use it too): the XDG folders, the
//! system's fonts, the locale, `/proc` for processes, one launcher window per user (a
//! lock file), and the game libraries (backend/linux/scan.rs).

use std::path::{Path, PathBuf};

use super::{FontFile, Rgba, UiFonts};
use crate::backend::linux::{self, home, xdg_dir};

pub use crate::backend::linux::control::close_game;
pub use crate::backend::linux::process_exe;
pub use crate::backend::linux::scan::{scan_installed_games, steam_dir};

/// PeerParty's usual folder: `$XDG_DATA_HOME/peerparty` (`~/.local/share/peerparty`).
pub fn usual_data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share").unwrap_or_else(std::env::temp_dir).join("peerparty")
}

/// The game folder the window starts with: `~/Games`, where Heroic and Lutris put
/// games too.
pub fn default_games_dir() -> PathBuf {
    home().unwrap_or_else(|| PathBuf::from("/")).join("Games")
}

/// The user's languages from the usual locale variables ("de_DE.UTF-8" reads as "de-DE").
pub fn languages() -> Vec<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .flat_map(|v| v.split(':').map(|s| s.split(['.', '@']).next().unwrap_or_default().replace('_', "-")).collect::<Vec<_>>())
        .filter(|s| !s.is_empty() && s != "C" && s != "POSIX")
        .collect()
}

/// The folders fonts are installed in: the system's, then the user's.
fn font_dirs() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/usr/share/fonts"), PathBuf::from("/usr/local/share/fonts")];
    out.extend(xdg_dir("XDG_DATA_HOME", ".local/share").map(|d| d.join("fonts")));
    out.extend(home().map(|h| h.join(".fonts")));
    out
}

/// The first of these font files found (by file name, anywhere a few folders down in
/// the font folders), with the face to use in it.
fn first_font(names: &[(&str, u32)]) -> Option<FontFile> {
    fn find(dir: &Path, name: &str, depth: u32) -> Option<PathBuf> {
        let entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
        if let Some(e) = entries.iter().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name)) {
            return Some(e.path());
        }
        if depth == 0 {
            return None;
        }
        entries.iter().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).find_map(|e| find(&e.path(), name, depth - 1))
    }
    let dirs = font_dirs();
    names.iter().find_map(|(name, index)| dirs.iter().find_map(|d| find(d, name, 4)).map(|path| FontFile { path, index: *index }))
}

/// The interface fonts: Noto Sans (SteamOS's and most systems'), else the common others.
/// No icon font: `ui::icons` draws its shapes.
pub fn ui_fonts() -> UiFonts {
    UiFonts {
        regular: first_font(&[("NotoSans-Regular.ttf", 0), ("Cantarell-Regular.otf", 0), ("DejaVuSans.ttf", 0), ("LiberationSans-Regular.ttf", 0)]),
        semibold: first_font(&[("NotoSans-SemiBold.ttf", 0), ("NotoSans-Bold.ttf", 0), ("Cantarell-Bold.otf", 0), ("DejaVuSans-Bold.ttf", 0)]),
        mono: first_font(&[("NotoSansMono-Regular.ttf", 0), ("DejaVuSansMono.ttf", 0), ("LiberationMono-Regular.ttf", 0)]),
        icons: None,
    }
}

/// A font for a character the interface fonts lack, by Unicode block. `lang` picks the
/// face of Noto's CJK collection (Japanese, Korean, Simplified or Traditional Chinese).
pub fn fallback_font(c: char, lang: &str) -> Option<FontFile> {
    let cjk_face = match lang {
        "ja" => 0,
        "ko" => 1,
        "zh-TW" => 3,
        _ => 2,
    };
    let cjk = [("NotoSansCJK-Regular.ttc", cjk_face), ("NotoSansCJKsc-Regular.otf", 0), ("wqy-microhei.ttc", 0), ("DroidSansFallbackFull.ttf", 0)];
    let names: &[(&str, u32)] = match c as u32 {
        0x1100..=0x11FF | 0x3040..=0x30FF | 0x3130..=0x318F | 0x31F0..=0x31FF | 0xAC00..=0xD7AF | 0x2E80..=0x2FDF | 0x3000..=0x303F
        | 0x3100..=0x312F | 0x3190..=0x31EF | 0x3200..=0x9FFF | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF => &cjk,
        0x0E00..=0x0EFF => &[("NotoSansThai-Regular.ttf", 0)],
        0x0900..=0x0DFF => &[("NotoSansDevanagari-Regular.ttf", 0)],
        0x1F000..=0x1FAFF | 0x2600..=0x27BF => &[("NotoColorEmoji.ttf", 0), ("NotoSansSymbols2-Regular.ttf", 0), ("DejaVuSans.ttf", 0)],
        _ => &[("NotoSansSymbols2-Regular.ttf", 0), ("NotoSansSymbols-Regular.ttf", 0), ("DejaVuSans.ttf", 0)],
    };
    first_font(names)
}

/// The desktop's reduced-motion setting isn't one there is everywhere: animations on.
pub fn animations_enabled() -> bool {
    true
}

/// Windows' background mode has no match here (a lowered priority can't be raised
/// again without privileges): nothing changes.
pub fn background(_on: bool) {}

/// The name the user logs in with.
pub fn user_name() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default()
}

/// Where PeerParty's logs go (the hook's too, through Wine's drive Z:).
pub fn logs_dir() -> PathBuf {
    std::env::temp_dir().join("peerparty").join("logs")
}

/// A program's icon: a Windows program's resources aren't read here, so none (the
/// window draws its letter tile instead).
pub fn exe_icon(_exe: &Path, _size: i32) -> Option<Rgba> {
    None
}

/// Opens a folder (a file's folder) in the desktop's file manager.
pub fn reveal(path: &Path) {
    let dir = if path.is_file() { path.parent().unwrap_or(path) } else { path };
    let _ = std::process::Command::new("xdg-open").arg(dir).stdin(std::process::Stdio::null()).spawn();
}

/// Opens a web page (an https link only) in the desktop's browser.
pub fn open_url(url: &str) {
    if url.starts_with("https://") {
        let _ = std::process::Command::new("xdg-open").arg(url).stdin(std::process::Stdio::null()).spawn();
    }
}

/// Starts a helper as it is: there's no console window to keep away.
pub fn quiet(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd
}

/// Whether the window is the one the player uses: under Gamescope the app in front is
/// the only one, and a desktop doesn't say, so yes.
pub fn in_front() -> bool {
    true
}

/// Only one launcher window runs per user: a second would keep its own copy of the
/// settings and save over the first's. The first holds a lock on `launcher.lock` in the
/// runtime folder for as long as it runs (the system lets go when it ends). False when
/// another one has it.
///
/// `after_update`: the window that put this program in place started it and is closing,
/// still holding the lock; it is waited for (10 s at most).
pub fn first_launcher(after_update: bool) -> bool {
    let path = linux::runtime_dir().join("launcher.lock");
    let tries = if after_update { 100 } else { 1 };
    for i in 0..tries {
        match lock(&path) {
            Some(true) => return true,
            // A lock that can't be made at all counts as free: a launcher never refuses
            // to open over it.
            None => return true,
            Some(false) if i + 1 < tries => std::thread::sleep(std::time::Duration::from_millis(100)),
            Some(false) => {}
        }
    }
    eprintln!("peerparty: the PeerParty window is open already");
    false
}

/// Takes the lock on `path` for the rest of the process: Some(false) when another
/// process holds it, None when it can't be tried.
fn lock(path: &Path) -> Option<bool> {
    use std::os::fd::IntoRawFd;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
    let fd = f.into_raw_fd();
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // Kept open (and locked) until the process ends.
        return Some(true);
    }
    let busy = std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK);
    unsafe { libc::close(fd) };
    if busy { Some(false) } else { None }
}

/// Where browsers save downloads: `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else
/// `~/Downloads`.
pub fn downloads_dir() -> Option<PathBuf> {
    let home = home()?;
    let dirs = xdg_dir("XDG_CONFIG_HOME", ".config").and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok()).unwrap_or_default();
    Some(user_dir(&dirs, "XDG_DOWNLOAD_DIR", &home).unwrap_or_else(|| home.join("Downloads")))
}

/// One folder from `user-dirs.dirs` (`XDG_DOWNLOAD_DIR="$HOME/Downloads"`).
fn user_dir(text: &str, key: &str, home: &Path) -> Option<PathBuf> {
    text.lines().find_map(|l| {
        let v = l.trim().strip_prefix(key)?.trim_start().strip_prefix('=')?.trim().trim_matches('"');
        let p = match v.strip_prefix("$HOME") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(v),
        };
        p.is_absolute().then_some(p)
    })
}

/// How many other processes run the program `exe`.
pub fn copies_running(exe: &Path) -> usize {
    let me = std::process::id();
    linux::processes().into_iter().filter(|&p| p != me && process_exe(p).as_deref() == Some(exe)).count()
}

/// The local time now: year, month, day, hour, minute, second.
pub fn local_time() -> [u16; 6] {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return [1970, 1, 1, 0, 0, 0];
    }
    let n = |v: libc::c_int| v.clamp(0, i32::from(u16::MAX)) as u16;
    [n(tm.tm_year + 1900), n(tm.tm_mon + 1), n(tm.tm_mday), n(tm.tm_hour), n(tm.tm_min), n(tm.tm_sec)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_download_folder_comes_from_user_dirs() {
        let home = Path::new("/home/player");
        let text = "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Downloads/Web\"\n";
        assert_eq!(user_dir(text, "XDG_DOWNLOAD_DIR", home), Some(PathBuf::from("/home/player/Downloads/Web")));
        assert_eq!(user_dir("XDG_DOWNLOAD_DIR=\"/data/dl\"", "XDG_DOWNLOAD_DIR", home), Some(PathBuf::from("/data/dl")));
        assert_eq!(user_dir("XDG_DOWNLOAD_DIR=\"relative\"", "XDG_DOWNLOAD_DIR", home), None);
        assert_eq!(user_dir("", "XDG_DOWNLOAD_DIR", home), None);
    }

    #[test]
    fn this_process_and_the_clock() {
        assert_eq!(process_exe(std::process::id()), std::env::current_exe().ok());
        let [y, mo, d, h, mi, s] = local_time();
        assert!(y >= 2024 && (1..=12).contains(&mo) && (1..=31).contains(&d) && h < 24 && mi < 60 && s < 61);
    }

    /// A second holder of the lock is refused while the first has it.
    #[test]
    fn one_launcher_at_a_time() {
        let path = std::env::temp_dir().join(format!("peerparty-lock-test-{}", std::process::id()));
        assert_eq!(lock(&path), Some(true));
        assert_eq!(lock(&path), Some(false), "flock is per open file: a second open is a second holder");
        let _ = std::fs::remove_file(&path);
    }
}
