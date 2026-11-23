//! The game libraries Windows keeps: Steam's folder (the registry says where), Epic's
//! launcher manifests, GOG Galaxy's registry entries and the Xbox app's packages
//! (`platform::scan_installed_games`; ui/scan.rs reads Steam's manifests and the
//! player's own folders).

use std::fs;
use std::path::{Path, PathBuf};

use crate::platform::{Hive, reg_string, reg_subkeys};
use crate::ui::scan::{Game, Source, steam_games, steam_libraries};

/// Steam's folder, from the registry (or where its installer puts it).
pub fn steam_dir() -> Option<PathBuf> {
    reg_string(Hive::User, r"Software\Valve\Steam", "SteamPath")
        .map(|p| PathBuf::from(p.replace('/', "\\")))
        .filter(|p| p.is_dir())
        .or_else(|| {
            let d = PathBuf::from(r"C:\Program Files (x86)\Steam");
            d.is_dir().then_some(d)
        })
}

/// Every store's installed games, sent as they're found: Steam's, Epic's, GOG
/// Galaxy's and the Xbox app's.
pub fn scan_installed_games(send: &dyn Fn(Game)) {
    if let Some(steam) = steam_dir() {
        steam_games(&steam, &steam_libraries(&steam), send);
    }
    epic(send);
    gog(send);
    xbox(send);
}

// ---- Epic ----

fn epic(send: &dyn Fn(Game)) {
    let data = reg_string(Hive::Machine, r"SOFTWARE\Epic Games\EpicGamesLauncher", "AppDataPath")
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

fn gog(send: &dyn Fn(Game)) {
    const KEY: &str = r"SOFTWARE\GOG.com\Games";
    for id in reg_subkeys(Hive::Machine, KEY) {
        let key = format!(r"{KEY}\{id}");
        let get = |v: &str| reg_string(Hive::Machine, &key, v).unwrap_or_default();
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
fn xbox(send: &dyn Fn(Game)) {
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
