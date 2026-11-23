//! The game libraries on Linux (`platform::scan_installed_games`):
//!
//! - Steam: its folder (`~/.local/share/Steam`, `~/.steam/steam`, or Flatpak's), the
//!   libraries its `libraryfolders.vdf` lists, and Steam libraries on SD cards and other
//!   removable drives (`/run/media/...`, where SteamOS mounts them). Their app manifests
//!   are read by ui/scan.rs, as on Windows.
//! - Heroic: Epic games through its legendary configuration (`installed.json`), and
//!   its GOG games (`gog_store/installed.json`).
//! - Lutris: each game's configuration (`games/*.yml`), whose `exe` is the program.
//!
//! The parsers take text and return what they found, with tests; the scan only reads
//! files.

use std::fs;
use std::path::{Path, PathBuf};

use super::{home, xdg_dir};
use crate::ui::mainexe;
use crate::ui::scan::{Game, Source, acf_value, clean_name, steam_games};

/// Steam's folder: the first of the usual places that has a `steamapps` folder.
pub fn steam_dir() -> Option<PathBuf> {
    steam_roots().into_iter().next()
}

/// Every place a Steam install may be, those that exist, each once.
fn steam_roots() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let candidates = [
        xdg_dir("XDG_DATA_HOME", ".local/share").map(|d| d.join("Steam")),
        home().map(|h| h.join(".steam").join("steam")),
        home().map(|h| h.join(".steam").join("root")),
        home().map(|h| h.join(".var/app/com.valvesoftware.Steam/.local/share/Steam")),
    ];
    for dir in candidates.into_iter().flatten() {
        if !dir.join("steamapps").is_dir() {
            continue;
        }
        let real = fs::canonicalize(&dir).unwrap_or(dir);
        if !out.contains(&real) {
            out.push(real);
        }
    }
    out
}

/// The libraries `libraryfolders.vdf` lists (each `"path"`), as `steamapps` folders.
pub fn library_paths(vdf: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in vdf.lines() {
        if let Some(path) = acf_value(line, "path") {
            let lib = PathBuf::from(path).join("steamapps");
            if !out.contains(&lib) {
                out.push(lib);
            }
        }
    }
    out
}

/// Steam libraries on removable drives: `<mount>/steamapps` (or `<mount>/SteamLibrary/
/// steamapps`) for each drive under `/run/media` and `/run/media/<user>`, which Steam
/// may not list until the card has been in once.
fn removable_libraries(media: &Path) -> Vec<PathBuf> {
    let mut mounts = Vec::new();
    for e in fs::read_dir(media).into_iter().flatten().flatten() {
        let p = e.path();
        mounts.push(p.clone());
        for inner in fs::read_dir(&p).into_iter().flatten().flatten() {
            mounts.push(inner.path());
        }
    }
    mounts.iter().flat_map(|m| [m.join("steamapps"), m.join("SteamLibrary").join("steamapps")]).filter(|l| l.is_dir()).collect()
}

/// Steam's libraries: its own, the listed ones and those on removable drives.
pub fn steam_libraries(steam: &Path) -> Vec<PathBuf> {
    let mut libs = vec![steam.join("steamapps")];
    let vdf = fs::read_to_string(steam.join("steamapps").join("libraryfolders.vdf")).unwrap_or_default();
    for lib in library_paths(&vdf).into_iter().chain(removable_libraries(Path::new("/run/media"))) {
        let same = |l: &PathBuf| l == &lib || fs::canonicalize(l).ok().is_some_and(|c| fs::canonicalize(&lib).ok() == Some(c));
        if !libs.iter().any(same) {
            libs.push(lib);
        }
    }
    libs
}

/// One of legendary's `installed.json` entries (Heroic's Epic games).
#[derive(Debug, PartialEq)]
pub struct Installed {
    pub title: String,
    pub dir: PathBuf,
    /// The program, relative to `dir` (may be empty: found by mainexe.rs then).
    pub exe: String,
}

/// legendary's `installed.json`: `{ "<app>": { "title", "install_path", "executable",
/// "is_dlc", ... } }`. DLC isn't a game of its own.
pub fn legendary_installed(text: &str) -> Vec<Installed> {
    let Ok(serde_json::Value::Object(apps)) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let mut out: Vec<Installed> = apps
        .values()
        .filter(|a| a.get("is_dlc").and_then(|v| v.as_bool()) != Some(true))
        .filter_map(|a| {
            let s = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let (title, dir) = (s("title"), s("install_path"));
            (!title.is_empty() && !dir.is_empty()).then(|| Installed { title, dir: PathBuf::from(dir), exe: s("executable") })
        })
        .collect();
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

/// Heroic's `gog_store/installed.json`: `{ "installed": [ { "appName", "install_path",
/// "platform", ... } ] }`, Windows builds only (the ones PeerParty can start). The title
/// is the folder's: the file has none.
pub fn heroic_gog_installed(text: &str) -> Vec<Installed> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let Some(list) = v.get("installed").and_then(|l| l.as_array()) else { return Vec::new() };
    list.iter()
        .filter(|g| g.get("platform").and_then(|p| p.as_str()).is_none_or(|p| p.eq_ignore_ascii_case("windows")))
        .filter_map(|g| {
            let dir = PathBuf::from(g.get("install_path")?.as_str()?);
            let title = clean_name(&dir.file_name()?.to_string_lossy());
            Some(Installed { title, dir, exe: String::new() })
        })
        .collect()
}

/// A Lutris game's configuration (YAML): the `exe` in its `game:` section, without its
/// quotes. Only that one value is needed, so this reads just that shape.
pub fn lutris_exe(yml: &str) -> Option<String> {
    let mut in_game = false;
    for line in yml.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            in_game = line.trim_end() == "game:";
            continue;
        }
        if in_game {
            if let Some(v) = line.trim().strip_prefix("exe:") {
                let v = v.trim();
                let v = v.strip_prefix(['"', '\'']).and_then(|v| v.strip_suffix(['"', '\''])).unwrap_or(v);
                return (!v.is_empty()).then(|| v.to_string());
            }
        }
    }
    None
}

/// A Lutris configuration's file name as a name: `some-game-1712345678.yml` is
/// "Some Game" (the slug, without the time Lutris adds).
pub fn lutris_name(file_stem: &str) -> String {
    let slug = match file_stem.rsplit_once('-') {
        Some((head, tail)) if !head.is_empty() && tail.len() >= 6 && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => file_stem,
    };
    slug.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every library's games, sent as they're found.
pub fn scan_installed_games(send: &dyn Fn(Game)) {
    if let Some(steam) = steam_dir() {
        steam_games(&steam, &steam_libraries(&steam), send);
    }
    heroic(send);
    lutris(send);
}

fn config_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(c) = xdg_dir("XDG_CONFIG_HOME", ".config") {
        out.push(c);
    }
    if let Some(h) = home() {
        out.push(h.join(".var/app/com.heroicgameslauncher.hgl/config"));
    }
    out
}

fn heroic(send: &dyn Fn(Game)) {
    let mut seen: Vec<PathBuf> = Vec::new();
    for config in config_dirs() {
        let epic = [config.join("heroic/legendaryConfig/legendary/installed.json"), config.join("legendary/installed.json")];
        for file in epic {
            for g in legendary_installed(&fs::read_to_string(&file).unwrap_or_default()) {
                found(g, Source::Epic, &mut seen, send);
            }
        }
        for g in heroic_gog_installed(&fs::read_to_string(config.join("heroic/gog_store/installed.json")).unwrap_or_default()) {
            found(g, Source::Gog, &mut seen, send);
        }
    }
}

/// Sends an installed game whose program is there (named, or found in its folder).
fn found(g: Installed, source: Source, seen: &mut Vec<PathBuf>, send: &dyn Fn(Game)) {
    let named = (!g.exe.is_empty()).then(|| g.dir.join(&g.exe)).filter(|e| e.is_file());
    let Some(exe) = named.or_else(|| mainexe::find(&g.dir)) else { return };
    if seen.contains(&exe) {
        return;
    }
    seen.push(exe.clone());
    send(Game { name: g.title, exe, dir: g.dir, source, cover: None, art: None });
}

fn lutris(send: &dyn Fn(Game)) {
    let dirs = [xdg_dir("XDG_DATA_HOME", ".local/share").map(|d| d.join("lutris/games")), xdg_dir("XDG_CONFIG_HOME", ".config").map(|d| d.join("lutris/games"))];
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in dirs.into_iter().flatten() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "yml")).collect();
        files.sort();
        for file in files {
            let Some(exe) = fs::read_to_string(&file).ok().as_deref().and_then(lutris_exe).map(PathBuf::from).filter(|e| e.is_file()) else { continue };
            let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = exe.parent().unwrap_or(Path::new("/")).to_path_buf();
            // Lutris keeps no store of its own here: its games show among the folders'.
            found(Installed { title: lutris_name(&stem), dir, exe: exe.to_string_lossy().into_owned() }, Source::Folder, &mut seen, send);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_lists_its_libraries() {
        let vdf = r#""libraryfolders"
{
	"0"
	{
		"path"		"/home/player/.local/share/Steam"
		"label"		""
		"apps"
		{
			"1234570"		"123456789"
		}
	}
	"1"
	{
		"path"		"/run/media/mmcblk0p1"
	}
	"2"
	{
		"path"		"/run/media/mmcblk0p1"
	}
}"#;
        assert_eq!(
            library_paths(vdf),
            [PathBuf::from("/home/player/.local/share/Steam/steamapps"), PathBuf::from("/run/media/mmcblk0p1/steamapps")]
        );
        assert!(library_paths("").is_empty());
    }

    #[test]
    fn removable_drives_with_a_library_are_found() {
        let media = std::env::temp_dir().join(format!("peerparty-media-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&media);
        fs::create_dir_all(media.join("card/steamapps")).unwrap();
        fs::create_dir_all(media.join("player/usb/SteamLibrary/steamapps")).unwrap();
        fs::create_dir_all(media.join("photos")).unwrap();
        let mut got = removable_libraries(&media);
        got.sort();
        assert_eq!(got, [media.join("card/steamapps"), media.join("player/usb/SteamLibrary/steamapps")]);
        let _ = fs::remove_dir_all(&media);
    }

    #[test]
    fn heroics_epic_games_are_read() {
        let json = r#"{
            "Moss": {"app_name": "Moss", "title": "Moss and Mortar", "install_path": "/home/player/Games/Heroic/MossAndMortar", "executable": "Binaries/Win64/Moss.exe", "is_dlc": false},
            "MossDLC": {"title": "Moss and Mortar: More Moss", "install_path": "/home/player/Games/Heroic/MossAndMortar", "is_dlc": true},
            "Broken": {"title": "", "install_path": "/x"}
        }"#;
        assert_eq!(
            legendary_installed(json),
            [Installed { title: "Moss and Mortar".into(), dir: "/home/player/Games/Heroic/MossAndMortar".into(), exe: "Binaries/Win64/Moss.exe".into() }]
        );
        assert!(legendary_installed("not json").is_empty());
        assert!(legendary_installed("[]").is_empty());
    }

    #[test]
    fn heroics_gog_games_are_read() {
        let json = r#"{"installed": [
            {"appName": "1", "platform": "windows", "install_path": "/home/player/Games/Heroic/Paper Comets (v1.2)"},
            {"appName": "2", "platform": "linux", "install_path": "/home/player/Games/Heroic/Native One"}
        ]}"#;
        assert_eq!(heroic_gog_installed(json), [Installed { title: "Paper Comets".into(), dir: "/home/player/Games/Heroic/Paper Comets (v1.2)".into(), exe: String::new() }]);
        assert!(heroic_gog_installed("{}").is_empty());
    }

    #[test]
    fn lutris_configurations_name_their_program() {
        let yml = "game:\n  exe: \"/home/player/Games/tidewatch/drive_c/Tidewatch/Tidewatch.exe\"\n  prefix: /home/player/Games/tidewatch\nsystem: {}\nwine:\n  exe: /not/this\n";
        assert_eq!(lutris_exe(yml).as_deref(), Some("/home/player/Games/tidewatch/drive_c/Tidewatch/Tidewatch.exe"));
        assert_eq!(lutris_exe("system:\n  exe: /x\n"), None, "only the game section's");
        assert_eq!(lutris_exe("game:\n  # exe: commented\n  exe: /g/x.exe\n").as_deref(), Some("/g/x.exe"));
        assert_eq!(lutris_exe("game:\n  exe:\n"), None);
        assert_eq!(lutris_name("night-market-heist-1712345678"), "Night Market Heist");
        assert_eq!(lutris_name("tidewatch"), "Tidewatch");
        assert_eq!(lutris_name("orbit-couriers-2"), "Orbit Couriers 2", "a short number is the name's own");
    }
}
