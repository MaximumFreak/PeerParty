//! A split-screen copy of an Unreal game draws at its pane's size, and follows the pane
//! when it's resized (Layout Studio). Unreal keeps its window mode and resolution in
//! `<Project>\Saved\Config\<Platform>\GameUserSettings.ini` (`FullscreenMode`: 0
//! exclusive fullscreen, 1 fullscreen window, 2 windowed; `ResolutionSizeX`/`Y` and the
//! `LastUserConfirmed…` ones). In exclusive fullscreen the engine sizes its frames from
//! that setting and passes over its window's size changes, so a copy kept windowed in a
//! pane (video/fullscreen.rs) goes on drawing the size it started at, scaled into the
//! pane; in a fullscreen window it takes its window's size, and follows it. In a copy
//! with a pane:
//!
//! - the game opening that file to read opens a copy of it (`<data>\screen\<pid>\`) in
//!   which the window mode is a fullscreen window and the resolution the pane's. The
//!   monitor it's told of is the pane (window.rs), so its window is the pane, and a
//!   resize of the pane is one of the window it follows.
//! - the game writing it writes a file of ours. Once the game has closed it (or as the
//!   game exits), what it wrote goes to the real file with the screen values put back
//!   as they were there: split screen leaves the single-window settings as they were and
//!   keeps the rest (graphics, sound...).
//!
//! The real file is the one the copy's saves give it (vfs.rs): player 1's, or a later
//! player's own.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use super::vfs::Intent;
use crate::log;

/// Whether `path` is Unreal's saved settings file, `…\Saved\Config\<Platform>\GameUserSettings.ini`.
pub fn is_settings_ini(path: &str) -> bool {
    let mut parts = path.rsplit(['\\', '/']).filter(|p| !p.is_empty());
    let (Some(file), Some(_platform), Some(config), Some(saved)) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    file.eq_ignore_ascii_case("GameUserSettings.ini") && config.eq_ignore_ascii_case("Config") && saved.eq_ignore_ascii_case("Saved")
}

/// A screen value of the settings file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Mode,
    Width,
    Height,
    /// The window's place: only put back, a pane's isn't the single window's.
    Position,
}

fn screen_key(k: &str) -> Option<Key> {
    let is = |names: &[&str]| names.iter().any(|n| n.eq_ignore_ascii_case(k));
    if is(&["FullscreenMode", "LastConfirmedFullscreenMode", "PreferredFullscreenMode"]) {
        Some(Key::Mode)
    } else if is(&["ResolutionSizeX", "LastUserConfirmedResolutionSizeX", "DesiredScreenWidth"]) {
        Some(Key::Width)
    } else if is(&["ResolutionSizeY", "LastUserConfirmedResolutionSizeY", "DesiredScreenHeight"]) {
        Some(Key::Height)
    } else if is(&["WindowPosX", "WindowPosY"]) {
        Some(Key::Position)
    } else {
        None
    }
}

/// Unreal's `EWindowMode::WindowedFullscreen`.
const FULLSCREEN_WINDOW: &str = "1";

enum Edit {
    Keep,
    Set(String),
    Drop,
}

/// `text` with each `key=value` line edited by `edit(key, value)`; line endings kept.
fn rewrite(text: &str, mut edit: impl FnMut(&str, &str) -> Edit) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let Some((k, v)) = body.split_once('=') else {
            out.push_str(line);
            continue;
        };
        match edit(k.trim(), v.trim()) {
            Edit::Keep => out.push_str(line),
            Edit::Set(new) => {
                out.push_str(k);
                out.push('=');
                out.push_str(&new);
                out.push_str(&line[body.len()..]);
            }
            Edit::Drop => {}
        }
    }
    out
}

/// The settings file as a copy in a `w`×`h` pane reads it, and the values that changed
/// (`key old->new`).
pub(crate) fn pane_view(text: &str, w: u32, h: u32) -> (String, Vec<String>) {
    let mut changed = Vec::new();
    let out = rewrite(text, |k, v| {
        let new = match screen_key(k) {
            Some(Key::Mode) => FULLSCREEN_WINDOW.to_owned(),
            Some(Key::Width) => w.to_string(),
            Some(Key::Height) => h.to_string(),
            Some(Key::Position) | None => return Edit::Keep,
        };
        if new == v {
            return Edit::Keep;
        }
        changed.push(format!("{k} {v}->{new}"));
        Edit::Set(new)
    });
    (out, changed)
}

/// What the game wrote, with its screen values back as `before` (the real file) has
/// them, and left out where it has none.
pub(crate) fn restore(written: &str, before: &str) -> String {
    let mut saved: HashMap<String, String> = HashMap::new();
    rewrite(before, |k, v| {
        if screen_key(k).is_some() {
            saved.entry(k.to_ascii_lowercase()).or_insert_with(|| v.to_owned());
        }
        Edit::Keep
    });
    rewrite(written, |k, _| match screen_key(k) {
        None => Edit::Keep,
        Some(_) => saved.get(&k.to_ascii_lowercase()).map_or(Edit::Drop, |v| Edit::Set(v.clone())),
    })
}

/// How the file's text is stored: Unreal writes UTF-16 with a byte-order mark when the
/// text needs it, else plain bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Enc {
    Utf16,
    Utf8Bom,
    Plain,
}

fn decode(b: &[u8]) -> Option<(Enc, String)> {
    if let Some(rest) = b.strip_prefix(&[0xFF, 0xFE]) {
        if rest.len() % 2 != 0 {
            return None;
        }
        let w: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16(&w).ok().map(|s| (Enc::Utf16, s));
    }
    if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return std::str::from_utf8(rest).ok().map(|s| (Enc::Utf8Bom, s.to_owned()));
    }
    std::str::from_utf8(b).ok().map(|s| (Enc::Plain, s.to_owned()))
}

fn encode(enc: Enc, s: &str) -> Vec<u8> {
    match enc {
        Enc::Utf16 => [0xFF, 0xFE].into_iter().chain(s.encode_utf16().flat_map(u16::to_le_bytes)).collect(),
        Enc::Utf8Bom => [0xEF, 0xBB, 0xBF].into_iter().chain(s.bytes()).collect(),
        Enc::Plain => s.as_bytes().to_vec(),
    }
}

/// A settings file the game used in this copy.
struct Entry {
    /// The real file (the copy's own, vfs.rs).
    real: PathBuf,
    /// Its number, for our files' names.
    n: usize,
    /// Reads made: each gets a file of its own, as the game may still hold the last.
    reads: u32,
    /// When the game last opened our file to write, while what it wrote isn't in the
    /// real file yet.
    dirty: Option<Instant>,
}

static FILES: LazyLock<Mutex<HashMap<String, Entry>>> = LazyLock::new(Mutex::default);
static USED: AtomicBool = AtomicBool::new(false);
static MERGING: AtomicBool = AtomicBool::new(false);
/// Our file is handed out to the game first and opened by it after: a merge waits this
/// long after that, so it doesn't take the file before the game has it.
const GRACE: Duration = Duration::from_millis(300);
const ERROR_SHARING_VIOLATION: i32 = 32;

/// `<data>\screen\<pid>`: this copy's files.
fn dir() -> Option<PathBuf> {
    Some(crate::util::local_store::peerparty_dir()?.join("screen").join(std::process::id().to_string()))
}

/// Folders of copies that ended a day or more ago (one killed before it cleared its own).
fn clear_old(dir: &Path) {
    let Some(parent) = dir.parent() else { return };
    let Ok(list) = std::fs::read_dir(parent) else { return };
    let day = Duration::from_secs(24 * 3600);
    for d in list.flatten() {
        let old = d.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|e| e > day));
        if old && d.path() != dir {
            let _ = std::fs::remove_dir_all(d.path());
        }
    }
}

fn written_path(dir: &Path, n: usize) -> PathBuf {
    dir.join(format!("{n}-written-GameUserSettings.ini"))
}

/// Where the game's open of `path` (the copy's own file, vfs.rs) goes while this copy is
/// in a pane: our file, or None for `path` itself. PeerParty's own file calls (saves/mod.rs
/// `BUSY`) are on while it runs.
pub fn route(path: &str, intent: Intent) -> Option<String> {
    let (w, h) = crate::window::virtual_size()?;
    if !is_settings_ini(path) || !matches!(intent, Intent::Read | Intent::Write | Intent::Create) {
        return None;
    }
    let full = super::vfs::full_path(path)?;
    let dir = dir()?;
    if !USED.swap(true, Ordering::Relaxed) {
        clear_old(&dir);
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log!("window: {} couldn't be made ({e}): the game reads its Unreal settings as they are", dir.display());
        return None;
    }
    let mut files = crate::sys::lock(&FILES);
    let n = files.len();
    let e = files.entry(full.to_lowercase()).or_insert_with(|| Entry { real: PathBuf::from(&full), n, reads: 0, dirty: None });
    if intent == Intent::Read {
        merge(e, &dir, true);
        return view(e, &dir, w, h);
    }
    let ours = written_path(&dir, e.n);
    if intent == Intent::Write && !ours.exists() {
        // Opened to change what's there: it starts as the real file.
        let _ = std::fs::copy(&e.real, &ours);
    }
    let first = e.dirty.is_none();
    e.dirty = Some(Instant::now());
    drop(files);
    if first {
        crate::log::throttled("window: unreal settings write", 3, |n| {
            log!("window: the game saves its Unreal settings ({full}) to {}: they go to its own file once it's written, its window mode and resolution as they were there (call {n})", ours.display())
        });
    }
    start_merger();
    Some(ours.to_string_lossy().into_owned())
}

/// A copy of the real file with the pane's screen values, for the game to read.
fn view(e: &mut Entry, dir: &Path, w: u32, h: u32) -> Option<String> {
    let bytes = match std::fs::read(&e.real) {
        Ok(b) => b,
        Err(err) => {
            crate::log::throttled("window: unreal settings missing", 1, |_| {
                log!("window: the game's Unreal settings ({}) aren't there yet ({err}): it starts from its defaults", e.real.display())
            });
            return None;
        }
    };
    let Some((enc, text)) = decode(&bytes) else {
        log!("window: the game's Unreal settings ({}) aren't text PeerParty reads: the game reads them as they are", e.real.display());
        return None;
    };
    let (out, changed) = pane_view(&text, w, h);
    e.reads += 1;
    let path = dir.join(format!("{}-read{}-GameUserSettings.ini", e.n, e.reads));
    if let Err(err) = std::fs::write(&path, encode(enc, &out)) {
        log!("window: {} couldn't be written ({err}): the game reads its Unreal settings as they are", path.display());
        return None;
    }
    if e.reads > 1 {
        let _ = std::fs::remove_file(dir.join(format!("{}-read{}-GameUserSettings.ini", e.n, e.reads - 1)));
    }
    crate::log::throttled("window: unreal settings read", 3, |n| {
        let what = if changed.is_empty() { "already the pane's".to_owned() } else { changed.join(", ") };
        log!(
            "window: the game reads its Unreal settings ({}) with a fullscreen window at its pane's {w}x{h} ({what}): in exclusive fullscreen Unreal keeps its size when the pane is resized, in a fullscreen window it follows (call {n})",
            e.real.display()
        )
    });
    Some(path.to_string_lossy().into_owned())
}

/// Puts what the game wrote to our file in the real one, screen values as they were
/// there, once the game has closed it (`force`: now, without waiting for `GRACE`).
/// Whether nothing is left to do.
fn merge(e: &mut Entry, dir: &Path, force: bool) -> bool {
    let Some(since) = e.dirty else { return true };
    if !force && since.elapsed() < GRACE {
        return false;
    }
    let ours = written_path(dir, e.n);
    // Opened with no sharing: that fails while the game still has it open.
    let opened = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).share_mode(0).open(&ours)
    };
    let mut bytes = Vec::new();
    match opened {
        Ok(mut f) => {
            if f.read_to_end(&mut bytes).is_err() {
                return false;
            }
        }
        Err(err) if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => return false,
        Err(_) => {
            e.dirty = None;
            return true;
        }
    }
    e.dirty = None;
    let done = (|| {
        if bytes.is_empty() {
            return "nothing written: its own file left as it was".to_owned();
        }
        let Some((enc, written)) = decode(&bytes) else { return "not text PeerParty reads: its own file left as it was".to_owned() };
        let before = std::fs::read(&e.real).ok().and_then(|b| decode(&b)).map(|(_, t)| t).unwrap_or_default();
        match super::write_whole(&e.real, &encode(enc, &restore(&written, &before))) {
            Ok(()) => "in its own file, with its window mode and resolution as they were".to_owned(),
            Err(err) => format!("its own file couldn't be written ({err})"),
        }
    })();
    let _ = std::fs::remove_file(&ours);
    log!("window: the game's saved Unreal settings ({}): {done}", e.real.display());
    true
}

/// Merges what's waiting; whether anything still is.
fn merge_all(force: bool) -> bool {
    let Some(dir) = dir() else { return false };
    let mut files = crate::sys::lock(&FILES);
    files.values_mut().fold(false, |left, e| !merge(e, &dir, force) || left)
}

/// A thread merging what the game writes once it has closed the file.
fn start_merger() {
    if MERGING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("peerparty-unreal-ini".into()).spawn(|| {
        super::BUSY.with(|b| b.set(true));
        loop {
            std::thread::sleep(Duration::from_millis(250));
            if merge_all(false) {
                continue;
            }
            MERGING.store(false, Ordering::Release);
            // One written between the look and the flag going down.
            let waiting = crate::sys::lock(&FILES).values().any(|e| e.dirty.is_some());
            if !waiting || MERGING.swap(true, Ordering::AcqRel) {
                break;
            }
        }
    });
    if spawned.is_err() {
        MERGING.store(false, Ordering::Release);
    }
}

/// The game is exiting: what it wrote last goes to its own file, and this copy's files go.
pub fn on_exit() {
    if !USED.load(Ordering::Relaxed) {
        return;
    }
    let was = super::BUSY.with(|b| b.replace(true));
    if merge_all(true) {
        log!("window: the game exits with its last Unreal settings still open: they're left in {}", dir().map(|d| d.display().to_string()).unwrap_or_default());
    } else if let Some(d) = dir() {
        let _ = std::fs::remove_dir_all(d);
    }
    super::BUSY.with(|b| b.set(was));
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAVED: &str = "[/Script/Game.MyGameUserSettings]\r\nbUseVSync=True\r\nResolutionSizeX=2560\r\nResolutionSizeY=1440\r\nLastUserConfirmedResolutionSizeX=2560\r\nLastUserConfirmedResolutionSizeY=1440\r\nWindowPosX=-1\r\nWindowPosY=-1\r\nFullscreenMode=0\r\nLastConfirmedFullscreenMode=0\r\nPreferredFullscreenMode=0\r\nVersion=5\r\n\r\n[ScalabilityGroups]\r\nsg.ShadowQuality=3\r\n";

    #[test]
    fn finds_unreal_settings_files_only() {
        assert!(is_settings_ini(r"C:\Users\p\AppData\Local\Lantern\Saved\Config\WindowsNoEditor\GameUserSettings.ini"));
        assert!(is_settings_ini(r"\\?\C:\x\Saved\Config\Windows\gameusersettings.INI"));
        assert!(is_settings_ini("C:/x/Saved/Config/Windows/GameUserSettings.ini"));
        assert!(!is_settings_ini(r"C:\x\Saved\Config\WindowsNoEditor\Engine.ini"));
        assert!(!is_settings_ini(r"C:\x\Config\GameUserSettings.ini"));
        assert!(!is_settings_ini(r"C:\x\Lantern\Config\DefaultGameUserSettings.ini"));
        assert!(!is_settings_ini("GameUserSettings.ini"));
    }

    #[test]
    fn pane_reads_a_fullscreen_window_of_its_size() {
        let (view, changed) = pane_view(SAVED, 1147, 1440);
        assert!(view.contains("ResolutionSizeX=1147\r\n") && view.contains("LastUserConfirmedResolutionSizeX=1147\r\n"), "{view}");
        assert!(view.contains("ResolutionSizeY=1440\r\n"));
        assert!(view.contains("\r\nFullscreenMode=1\r\n") && view.contains("LastConfirmedFullscreenMode=1") && view.contains("PreferredFullscreenMode=1"));
        // Everything else as it was, line endings too.
        assert!(view.contains("WindowPosX=-1\r\n") && view.contains("sg.ShadowQuality=3\r\n") && view.contains("Version=5\r\n\r\n[Scal"));
        assert_eq!(view.len(), SAVED.len());
        // Two widths and three modes; the height is the pane's already.
        assert_eq!(changed.len(), 5, "{changed:?}");
        assert!(changed.contains(&"FullscreenMode 0->1".to_owned()));
        // Already the pane's: nothing to say.
        let (again, none) = pane_view(&view, 1147, 1440);
        assert_eq!(again, view);
        assert!(none.is_empty());
    }

    #[test]
    fn saved_settings_keep_the_single_window_screen() {
        let (view, _) = pane_view(SAVED, 1147, 1440);
        // The game saves what it has, a graphics setting changed and the pane's place.
        let written = view.replace("sg.ShadowQuality=3", "sg.ShadowQuality=1").replace("WindowPosX=-1", "WindowPosX=1720");
        let merged = restore(&written, SAVED);
        assert_eq!(merged, SAVED.replace("sg.ShadowQuality=3", "sg.ShadowQuality=1"));
        // A value the real file never had isn't added: the engine's default stays.
        let fresh = restore("[S]\nFullscreenMode=1\nResolutionSizeX=1147\nbUseVSync=False\n", "");
        assert_eq!(fresh, "[S]\nbUseVSync=False\n");
    }

    #[test]
    fn encodings_come_back_as_they_were() {
        for enc in [Enc::Utf16, Enc::Utf8Bom, Enc::Plain] {
            let b = encode(enc, "[S]\r\nName=Zoë\r\n");
            assert_eq!(decode(&b), Some((enc, "[S]\r\nName=Zoë\r\n".to_owned())));
        }
        assert_eq!(decode(&[0xFF, 0xFE, 0x41]), None);
        assert_eq!(decode(&[0x80, 0x81]), None);
    }
}
