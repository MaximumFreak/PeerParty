//! Finding installed games: Steam's app manifests (here, from wherever the system keeps
//! Steam), each store's own records (`platform::scan_installed_games`: Epic, GOG Galaxy
//! and the Xbox app on Windows, Heroic and Lutris on Linux), and the subfolders of the
//! player's game folders (`platform::default_games_dir` to begin with), whose program is
//! picked by mainexe.rs. Nothing here is specific to one PC.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use super::mainexe;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Source {
    Steam,
    Epic,
    Gog,
    Xbox,
    Folder,
    Added,
}


#[derive(Clone, Debug)]
pub struct Game {
    pub name: String,
    pub exe: PathBuf,
    pub dir: PathBuf,
    pub source: Source,
    /// Box art (Steam's library capsule), portrait.
    pub cover: Option<PathBuf>,
    /// A picture of the game that isn't portrait (Steam's header, a folder's icon.png).
    pub art: Option<PathBuf>,
}

pub enum Found {
    Game(Game),
    Done,
}

/// Folder names under a Games folder that are never games (the importer's list).
const NOT_GAME_FOLDERS: &[&str] = &[
    "artbook", "GamingRepair", "_Redist", "Redist", "redsit", "_redsit", "update_folder", "_CommonRedist",
    "Support", "__MACOSX",
];

/// Steam entries that are tools, not games.
const STEAM_TOOLS: &[&str] = &[
    "Steamworks Common Redistributables",
    "Steam Linux Runtime",
    "Proton",
    "SteamVR",
    "Steam Audio",
];

/// Steamworks' sample app, which the Steam client installs for SDK testing.
const STEAMWORKS_SAMPLE: &str = "480";

/// Scans everything, sending games as they're found (the slow folders last).
pub fn scan(folders: Vec<PathBuf>, tx: Sender<Found>) {
    let send = |g: Game| {
        let _ = tx.send(Found::Game(g));
    };
    crate::platform::scan_installed_games(&send);
    for root in folders {
        folder(&root, &send);
    }
    let _ = tx.send(Found::Done);
}

/// Strips `(v1.2)` and `[GOG]` style tags from a folder name.
pub fn clean_name(folder: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in folder.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' if depth > 0 => depth -= 1,
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.is_empty() { folder.to_string() } else { out }
}

// ---- Steam ----

/// Steam's own folder, where the system keeps it (`platform::steam_dir`).
pub fn steam_dir() -> Option<PathBuf> {
    crate::platform::steam_dir()
}

/// A top-level `"key"  "value"` from a Steam .acf / .vdf (KeyValues) file.
pub fn acf_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.trim().split('"').filter(|p| !p.trim().is_empty());
        let k = parts.next()?;
        let v = parts.next()?;
        k.eq_ignore_ascii_case(key).then(|| v.replace("\\\\", "\\"))
    })
}

pub fn steam_libraries(steam: &Path) -> Vec<PathBuf> {
    let mut libs = vec![steam.join("steamapps")];
    if let Ok(vdf) = fs::read_to_string(steam.join("steamapps").join("libraryfolders.vdf")) {
        for line in vdf.lines() {
            if let Some(path) = acf_value(line, "path") {
                let lib = PathBuf::from(path).join("steamapps");
                if !libs.iter().any(|l| l.as_os_str().eq_ignore_ascii_case(lib.as_os_str())) {
                    libs.push(lib);
                }
            }
        }
    }
    libs
}

/// Steam's cached library art for an app: (portrait capsule, landscape header).
fn steam_art(steam: &Path, id: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let cache = steam.join("appcache").join("librarycache");
    let mut cover = None;
    let mut header = None;
    for (name, slot) in [("library_600x900.jpg", &mut cover), ("header.jpg", &mut header)] {
        let flat = cache.join(format!("{id}_{name}"));
        if flat.is_file() {
            *slot = Some(flat);
            continue;
        }
        // Newer Steam: librarycache\<id>\<name>, or one folder further down.
        let dir = cache.join(id);
        let direct = dir.join(name);
        if direct.is_file() {
            *slot = Some(direct);
            continue;
        }
        if let Ok(entries) = fs::read_dir(&dir) {
            *slot = entries
                .flatten()
                .map(|e| e.path().join(name))
                .find(|p| p.is_file());
        }
    }
    (cover, header)
}

/// The games in Steam's libraries `libs` (`steam`: Steam's own folder, for the art).
pub fn steam_games(steam: &Path, libs: &[PathBuf], send: &dyn Fn(Game)) {
    for lib in libs {
        let Ok(entries) = fs::read_dir(&lib) else { continue };
        for e in entries.flatten() {
            let file = e.file_name().to_string_lossy().to_lowercase();
            if !(file.starts_with("appmanifest_") && file.ends_with(".acf")) {
                continue;
            }
            let Ok(text) = fs::read_to_string(e.path()) else { continue };
            let (Some(id), Some(name), Some(installdir)) =
                (acf_value(&text, "appid"), acf_value(&text, "name"), acf_value(&text, "installdir"))
            else {
                continue;
            };
            if id == STEAMWORKS_SAMPLE || STEAM_TOOLS.iter().any(|t| name.starts_with(t)) {
                continue;
            }
            let dir = lib.join("common").join(&installdir);
            let Some(exe) = mainexe::find(&dir) else { continue };
            let (cover, art) = steam_art(steam, &id);
            send(Game {
                name,
                exe,
                dir,
                source: Source::Steam,
                cover,
                art,
            });
        }
    }
}

// ---- folders of games ----

fn folder(root: &Path, send: &dyn Fn(Game)) {
    let Ok(entries) = fs::read_dir(root) else { return };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') || NOT_GAME_FOLDERS.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let Some(exe) = mainexe::find(&dir) else { continue };
        let icon = dir.join("icon.png");
        send(Game {
            name: clean_name(&name),
            exe,
            source: Source::Folder,
            cover: None,
            art: icon.is_file().then_some(icon),
            dir,
        });
    }
}

/// A game for a program that no library knows (added by hand, or only in Recent).
pub fn loose(exe: &Path, source: Source) -> Game {
    let dir = exe.parent().unwrap_or(Path::new("")).to_path_buf();
    // A program called Game.exe or Launcher.exe says less than its folder does.
    let stem = exe.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let folder = dir.file_name().map(|f| f.to_string_lossy().to_string());
    let name = match folder {
        Some(f) if !f.eq_ignore_ascii_case("bin") && !f.eq_ignore_ascii_case("win64") && !f.eq_ignore_ascii_case("x64") => {
            clean_name(&f)
        }
        _ => stem,
    };
    Game {
        name,
        exe: exe.to_path_buf(),
        dir,
        source,
        cover: None,
        art: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_folder_names() {
        assert_eq!(clean_name("Cool Game (v1.2) [GOG]"), "Cool Game");
        assert_eq!(clean_name("Some Game 3"), "Some Game 3");
        assert_eq!(clean_name("(only tags)"), "(only tags)");
    }

    #[test]
    fn reads_acf_values() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"1234570\"\n\t\"name\"\t\t\"Some Game's Sequel 3\"\n\t\"installdir\"\t\t\"Some Game 3\"\n}";
        assert_eq!(acf_value(acf, "appid").as_deref(), Some("1234570"));
        assert_eq!(acf_value(acf, "name").as_deref(), Some("Some Game's Sequel 3"));
        assert_eq!(acf_value(acf, "path"), None);
    }
}
