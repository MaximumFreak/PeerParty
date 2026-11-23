//! Whether the game runs under Wine, and Wine's flavours: Proton (Steam's Wine on Linux)
//! and SteamOS's Gaming Mode, where Gamescope shows one fullscreen window at a time.
//!
//! Wine's ntdll exports `wine_get_version`; Windows' never does, so on Windows the probe
//! is one GetProcAddress that fails and nothing else changes (no variable is even read).
//! Under Wine the Linux environment shows through to the game's, which tells Proton
//! (Steam's compat-tool variables) and Gamescope (its display, the Deck's Gaming Mode)
//! apart from plain Wine.
//!
//! What PeerParty does differently there is decided here, so other modules ask one
//! question rather than probing for themselves:
//! - `prefer_ingame_overlay`: in Gamescope a window over the game isn't shown, so with
//!   the overlay's draw mode on `auto` it draws inside the game's frames (present/).
//! - `is_wine`: Windows-only probes (package identity, Windows' echo-cancelling capture
//!   DSP) are skipped; paths a launcher outside Wine hands over (`/run/user/...`) are
//!   turned into ones Wine opens (`windows_path`).

use std::borrow::Cow;
use std::ffi::c_char;
use std::sync::OnceLock;

use crate::util::json::Json;
use crate::log;

/// What the game is running under.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WineInfo {
    /// Wine's version (`wine_get_version`); None on Windows.
    pub version: Option<String>,
    /// Under Wine started by Steam's Proton compat tool.
    pub proton: bool,
    /// Under Wine inside Gamescope (SteamOS / Steam Deck Gaming Mode).
    pub gamescope: bool,
}

impl WineInfo {
    pub fn is_wine(&self) -> bool {
        self.version.is_some()
    }

    /// Whether the overlay should draw inside the game's frames when the player left the
    /// draw mode on `auto`: in Gamescope, which only shows the game's own window.
    pub fn prefer_ingame_overlay(&self) -> bool {
        self.gamescope
    }

    /// "Wine 9.0", "Proton (Wine 9.0)", "Proton (Wine 9.0) in Gamescope"; None on Windows.
    pub fn describe(&self) -> Option<String> {
        let v = self.version.as_deref()?;
        let wine = if self.proton { format!("Proton (Wine {v})") } else { format!("Wine {v}") };
        Some(if self.gamescope { format!("{wine} in Gamescope") } else { wine })
    }
}

/// The detection itself, on Wine's version (None: not Wine) and a way to read the
/// environment, so every case can be tested on Windows. Not Wine, `get_env` isn't called.
pub fn detect_env(wine_ver: Option<&str>, get_env: impl Fn(&str) -> Option<String>) -> WineInfo {
    let Some(version) = wine_ver else { return WineInfo::default() };
    let set = |k: &str| get_env(k).is_some_and(|v| !v.trim().is_empty());
    let one = |k: &str| get_env(k).is_some_and(|v| v.trim() == "1");
    let proton = set("STEAM_COMPAT_DATA_PATH")
        || set("STEAM_COMPAT_CLIENT_INSTALL_PATH")
        || get_env("WINEPREFIX").is_some_and(|p| p.to_ascii_lowercase().contains("compatdata"))
        || set("SteamGameId");
    let gamescope = set("GAMESCOPE_WAYLAND_DISPLAY")
        || get_env("XDG_CURRENT_DESKTOP").is_some_and(|d| d.trim().eq_ignore_ascii_case("gamescope"))
        || one("SteamDeck")
        || one("STEAM_DECK")
        || one("SteamGamepadUI");
    WineInfo { version: Some(version.to_string()), proton, gamescope }
}

/// Wine's version, from ntdll's `wine_get_version` (looked up once). None on Windows.
pub fn version() -> Option<&'static str> {
    static VERSION: OnceLock<Option<String>> = OnceLock::new();
    VERSION.get_or_init(probe_version).as_deref()
}

fn probe_version() -> Option<String> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    let ntdll = unsafe { GetModuleHandleA(b"ntdll.dll\0".as_ptr()) };
    if ntdll.is_null() {
        return None;
    }
    let f = unsafe { GetProcAddress(ntdll, b"wine_get_version\0".as_ptr()) }?;
    let get: unsafe extern "C" fn() -> *const c_char = unsafe { std::mem::transmute(f) };
    let p = unsafe { get() };
    // A short ASCII string Wine keeps for the process's life; anything else reads as
    // "unknown" (still Wine: only Wine exports the function).
    let mut text = String::new();
    if !p.is_null() {
        for i in 0..64 {
            let b = unsafe { *p.add(i) } as u8;
            if b == 0 {
                break;
            }
            text.push(if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' });
        }
    }
    Some(if text.trim().is_empty() { "unknown".to_string() } else { text })
}

/// What the game is running under (looked up once).
pub fn info() -> &'static WineInfo {
    static INFO: OnceLock<WineInfo> = OnceLock::new();
    INFO.get_or_init(|| detect_env(version(), |k| std::env::var(k).ok()))
}

pub fn is_wine() -> bool {
    info().is_wine()
}

pub fn is_proton() -> bool {
    info().proton
}

pub fn is_gamescope() -> bool {
    info().gamescope
}

/// See `WineInfo::prefer_ingame_overlay`.
pub fn prefer_ingame_overlay() -> bool {
    info().prefer_ingame_overlay()
}

/// Whether the overlay draws inside the game's frames for a `PEERPARTY_DRAW` value:
/// `ingame` always, `window` never, `auto` (or nothing) when `prefer` says so.
pub fn draw_ingame(draw: Option<&str>, prefer: bool) -> bool {
    match draw.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("ingame") => true,
        None => prefer,
        Some(v) if v.is_empty() || v.eq_ignore_ascii_case("auto") => prefer,
        Some(_) => false,
    }
}

/// " (under Wine 9.0)" for a log line about something that failed, "" on Windows.
pub fn tag() -> String {
    info().describe().map(|d| format!(" (under {d})")).unwrap_or_default()
}

/// Says in the log what the game runs under, when it isn't Windows itself.
pub fn log_if_present() {
    let info = info();
    let Some(what) = info.describe() else { return };
    log!(
        "wine: running under {what}{}",
        if info.prefer_ingame_overlay() { "; the overlay draws inside the game's frames unless set to a window" } else { "" }
    );
}

/// For the launcher (ipc.rs `state`): what the game runs under, Null on Windows.
pub fn to_json() -> Json {
    match version() {
        None => Json::Null,
        Some(v) => Json::obj(vec![("version", Json::str(v)), ("proton", Json::Bool(is_proton())), ("gamescope", Json::Bool(is_gamescope()))]),
    }
}

/// A path as Windows code opens it. Under Wine an absolute Unix path (a launcher outside
/// Wine names its files so: `/run/user/1000/peerparty/...`) becomes Wine's DOS name for
/// it (`wine_get_dos_file_name`, else drive Z:, which Wine maps to `/`). Anything else,
/// and everything on Windows, is left as it is.
pub fn windows_path(path: &str) -> Cow<'_, str> {
    if !is_wine() || !is_unix_absolute(path) {
        return Cow::Borrowed(path);
    }
    if let Some(dos) = dos_file_name(path) {
        return Cow::Owned(dos);
    }
    unix_to_dos(path).map_or(Cow::Borrowed(path), Cow::Owned)
}

/// `/x/y`, not `//server/share` or a Windows path.
fn is_unix_absolute(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//")
}

/// An absolute Unix path on Wine's drive Z: (`/run/a b` -> `Z:\run\a b`).
pub fn unix_to_dos(path: &str) -> Option<String> {
    is_unix_absolute(path).then(|| format!("Z:{}", path.replace('/', "\\")))
}

/// Wine's kernel32 `wine_get_dos_file_name(unix path) -> WCHAR*` (from the process heap).
fn dos_file_name(path: &str) -> Option<String> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapFree};
    if path.contains('\0') {
        return None;
    }
    let k32 = unsafe { GetModuleHandleA(b"kernel32.dll\0".as_ptr()) };
    if k32.is_null() {
        return None;
    }
    let f = unsafe { GetProcAddress(k32, b"wine_get_dos_file_name\0".as_ptr()) }?;
    let get: unsafe extern "C" fn(*const c_char) -> *mut u16 = unsafe { std::mem::transmute(f) };
    let unix: Vec<u8> = path.bytes().chain(Some(0)).collect();
    let p = unsafe { get(unix.as_ptr().cast()) };
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    while n < 32768 && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    let dos = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) });
    unsafe { HeapFree(GetProcessHeap(), 0, p.cast()) };
    (!dos.is_empty()).then_some(dos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    /// Windows itself: nothing is Wine, and the environment isn't even read, so a
    /// Windows user's Steam variables (Big Picture sets some) change nothing.
    #[test]
    fn native_windows_is_nothing_and_reads_nothing() {
        let reads = Cell::new(0);
        let info = detect_env(None, |_| {
            reads.set(reads.get() + 1);
            Some("1".into())
        });
        assert_eq!(info, WineInfo::default());
        assert_eq!(reads.get(), 0);
        assert!(!info.is_wine() && !info.proton && !info.gamescope && !info.prefer_ingame_overlay());
        assert_eq!(info.describe(), None);
    }

    /// This test process is Windows (or, run under Wine, Wine): the probe agrees with
    /// itself and the cached answers with the detection.
    #[test]
    fn the_probe_is_consistent() {
        assert_eq!(is_wine(), version().is_some());
        assert_eq!(info().is_wine(), is_wine());
        if !is_wine() {
            assert!(!is_proton() && !is_gamescope() && !prefer_ingame_overlay());
            assert_eq!(tag(), "");
            assert!(matches!(to_json(), Json::Null));
            // Paths are never rewritten on Windows.
            assert!(matches!(windows_path("/run/user/1000/f.bin"), Cow::Borrowed("/run/user/1000/f.bin")));
        }
    }

    #[test]
    fn plain_wine_is_neither_proton_nor_gamescope() {
        let info = detect_env(Some("9.0"), env(&[("WINEPREFIX", "/home/me/.wine"), ("HOME", "/home/me")]));
        assert!(info.is_wine() && !info.proton && !info.gamescope && !info.prefer_ingame_overlay());
        assert_eq!(info.describe().as_deref(), Some("Wine 9.0"));
        // Variables that are there but empty, or 0, say nothing.
        let info = detect_env(Some("9.0"), env(&[("STEAM_COMPAT_DATA_PATH", " "), ("SteamDeck", "0"), ("SteamGamepadUI", "")]));
        assert!(!info.proton && !info.gamescope);
    }

    #[test]
    fn proton_is_told_by_any_of_its_variables() {
        for pairs in [
            &[("STEAM_COMPAT_DATA_PATH", "/home/me/.steam/steam/steamapps/compatdata/480")][..],
            &[("STEAM_COMPAT_CLIENT_INSTALL_PATH", "/home/me/.steam/steam")],
            &[("WINEPREFIX", "/home/me/.steam/steam/steamapps/CompatData/480/pfx")],
            &[("SteamGameId", "480")],
        ] {
            let info = detect_env(Some("8.0-5"), env(pairs));
            assert!(info.proton && !info.gamescope, "{pairs:?}");
            assert_eq!(info.describe().as_deref(), Some("Proton (Wine 8.0-5)"));
        }
        // Proton's variables without Wine (a Windows PC that has them set) are nothing.
        assert!(!detect_env(None, env(&[("SteamGameId", "480")])).proton);
    }

    #[test]
    fn gamescope_is_told_by_its_display_or_the_decks_gaming_mode() {
        for pairs in [
            &[("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-0")][..],
            &[("XDG_CURRENT_DESKTOP", "gamescope")],
            &[("SteamDeck", "1")],
            &[("STEAM_DECK", "1")],
            &[("SteamGamepadUI", "1")],
        ] {
            let info = detect_env(Some("9.0"), env(pairs));
            assert!(info.gamescope && info.prefer_ingame_overlay(), "{pairs:?}");
        }
        let info = detect_env(Some("9.0"), env(&[("SteamGameId", "480"), ("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-0")]));
        assert!(info.proton && info.gamescope);
        assert_eq!(info.describe().as_deref(), Some("Proton (Wine 9.0) in Gamescope"));
        // Gamescope's variables on Windows: nothing.
        assert!(!detect_env(None, env(&[("SteamDeck", "1")])).prefer_ingame_overlay());
        // A desktop session isn't Gamescope.
        assert!(!detect_env(Some("9.0"), env(&[("XDG_CURRENT_DESKTOP", "KDE")])).gamescope);
    }

    /// The player's draw mode wins; `auto` or nothing set follows the environment.
    #[test]
    fn the_draw_mode_prefers_ingame_only_when_left_on_auto() {
        assert!(draw_ingame(Some("ingame"), false));
        assert!(draw_ingame(Some(" InGame "), false));
        assert!(!draw_ingame(Some("window"), true));
        assert!(!draw_ingame(Some("auto"), false));
        assert!(draw_ingame(Some("auto"), true));
        assert!(draw_ingame(Some(""), true));
        assert!(draw_ingame(None, true));
        assert!(!draw_ingame(None, false));
        let deck = detect_env(Some("9.0"), env(&[("SteamDeck", "1")]));
        assert!(draw_ingame(Some("auto"), deck.prefer_ingame_overlay()));
        assert!(!draw_ingame(Some("auto"), WineInfo::default().prefer_ingame_overlay()));
    }

    #[test]
    fn unix_paths_land_on_drive_z() {
        assert_eq!(unix_to_dos("/run/user/1000/peerparty/frame-1.bin").as_deref(), Some(r"Z:\run\user\1000\peerparty\frame-1.bin"));
        assert_eq!(unix_to_dos("/tmp/a b/c").as_deref(), Some(r"Z:\tmp\a b\c"));
        assert_eq!(unix_to_dos(r"Z:\run\user\1000\f.bin"), None);
        assert_eq!(unix_to_dos("Z:/run/f.bin"), None);
        assert_eq!(unix_to_dos("//server/share/f.bin"), None);
        assert_eq!(unix_to_dos("relative/f.bin"), None);
    }
}
