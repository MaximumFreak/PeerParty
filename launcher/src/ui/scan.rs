//! Finding installed games: Steam's app
//! manifests, Epic's launcher manifests, GOG Galaxy's registry entries, and the
//! subfolders of the player's game folders (`C:\Games` to begin with), whose program
//! is picked by mainexe.rs. Nothing here is specific to one PC.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use super::mainexe;
use crate::platform::{self as win, Hive};

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
    steam(&send);
    epic(&send);
    gog(&send);
    xbox(&send);
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

pub fn steam_dir() -> Option<PathBuf> {
    win::reg_string(Hive::User, r"Software\Valve\Steam", "SteamPath")
        .map(|p| PathBuf::from(p.replace('/', "\\")))
        .filter(|p| p.is_dir())
        .or_else(|| {
            let d = PathBuf::from(r"C:\Program Files (x86)\Steam");
            d.is_dir().then_some(d)
        })
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

fn steam_libraries(steam: &Path) -> Vec<PathBuf> {
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

fn steam(send: &impl Fn(Game)) {
    let Some(steam) = steam_dir() else { return };
    for lib in steam_libraries(&steam) {
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
            let (cover, art) = steam_art(&steam, &id);
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

// ---- Epic ----

fn epic(send: &impl Fn(Game)) {
    let data = win::reg_string(Hive::Machine, r"SOFTWARE\Epic Games\EpicGamesLauncher", "AppDataPath")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData\Epic\EpicGamesLauncher\Data"));
    let Ok(entries) = fs::read_dir(data.join("Manifests")) else { return };
    for e in entries.flatten() {
        if !e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("item")) {
            continue;
        }
        let Ok(text) = fs::read_to_string(e.path()) else { continue };
        let Ok(m) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let s = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        if m.get("bIsIncompleteInstall").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let (name, location, launch) = (s("DisplayName"), s("InstallLocation"), s("LaunchExecutable"));
        if name.is_empty() || location.is_empty() || launch.is_empty() {
            continue;
        }
        let dir = PathBuf::from(&location);
        let exe = dir.join(launch.replace('/', "\\"));
        if !exe.is_file() {
            continue;
        }
        send(Game {
            name,
            exe,
            dir,
            source: Source::Epic,
            cover: None,
            art: None,
        });
    }
}

// ---- GOG Galaxy ----

fn gog(send: &impl Fn(Game)) {
    const KEY: &str = r"SOFTWARE\GOG.com\Games";
    for id in win::reg_subkeys(Hive::Machine, KEY) {
        let key = format!(r"{KEY}\{id}");
        let get = |v: &str| win::reg_string(Hive::Machine, &key, v).unwrap_or_default();
        let (name, exe, path) = (get("gameName"), get("exe"), get("path"));
        if name.is_empty() || exe.is_empty() {
            continue;
        }
        let exe = PathBuf::from(exe);
        if !exe.is_file() {
            continue;
        }
        let dir = if path.is_empty() { exe.parent().unwrap_or(Path::new("")).to_path_buf() } else { PathBuf::from(path) };
        send(Game {
            name,
            exe,
            dir,
            source: Source::Gog,
            cover: None,
            art: None,
        });
    }
}

// ---- Xbox app (Microsoft Store, GDK) ----

/// Games the Xbox app installed: `<drive>:\XboxGames\<game>\Content`, each a package
/// (appxmanifest.xml) whose MicrosoftGame.config names the game's program. They start
/// through packaged.rs.
fn xbox(send: &impl Fn(Game)) {
    for drive in b'C'..=b'Z' {
        let root = PathBuf::from(format!(r"{}:\XboxGames", drive as char));
        let Ok(entries) = fs::read_dir(&root) else { continue };
        for e in entries.flatten() {
            let content = e.path().join("Content");
            let Ok(config) = fs::read_to_string(content.join("MicrosoftGame.config")).or_else(|_| fs::read_to_string(content.join("MicrosoftGame.Config"))) else {
                continue;
            };
            let attr = |name: &str| {
                let key = format!("{name}=\"");
                let start = config.find(&key)? + key.len();
                let end = config[start..].find('"')? + start;
                Some(config[start..end].to_string())
            };
            let Some(exe) = config.find("<Executable ").and_then(|i| {
                let tag = &config[i..i + config[i..].find('>')?];
                let k = tag.find("Name=\"")? + 6;
                let n = tag[k..].find('"')?;
                Some(content.join(tag[k..k + n].replace('/', "\\")))
            }) else {
                continue;
            };
            if !exe.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let art = ["Square480x480Logo", "Square150x150Logo"].iter().find_map(|k| attr(k)).map(|p| content.join(p)).filter(|p| p.is_file());
            send(Game { name, exe, dir: content, source: Source::Xbox, cover: None, art });
        }
    }
}

// ---- folders of games ----

fn folder(root: &Path, send: &impl Fn(Game)) {
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
