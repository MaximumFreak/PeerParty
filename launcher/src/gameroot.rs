//! The game's install folder, for following the programs a game starts (the hook's
//! child.rs, through `PEERPARTY_GAME_ROOT`): a starter in it hands PeerParty on to the
//! game it starts, and anything outside it (a store's client, a browser, a system tool)
//! is left alone.
//!
//! Where it comes from, first match wins:
//! 1. A Steam library: `<library>\steamapps\common\<game>`.
//! 2. Epic's install list (its manifests' `InstallLocation`).
//! 3. One of the window's game folders (Settings, `C:\Games` to begin with): the folder
//!    under it that holds the game.
//! 4. Unreal's layout, `<root>\<Project>\Binaries\<Platform>\` with `<root>\Engine`.
//! 5. The program's folder, climbing out of folders named like a build's
//!    (`bin`, `Binaries`, `Win64`, `x64`, ...).
//!
//! Never a drive's root, Windows, Program Files, the user's folder or a library's own
//! root: a folder that wide would follow programs that aren't the game's.

use std::path::{Component, Path, PathBuf};

/// Folder names a game's program sits in below the game's own folder.
const BUILD_DIRS: [&str; 17] = [
    "bin", "bin32", "bin64", "binaries", "win32", "win64", "x64", "x86", "x86_64", "game", "content", "wingdk", "shipping", "release", "retail", "windows", "windowsnoeditor",
];

fn lower(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

/// Folders too wide to be one game's.
fn too_wide(dir: &Path) -> bool {
    let d = lower(dir);
    let env_dir = |k: &str| std::env::var_os(k).map(|v| lower(Path::new(&v)));
    let wide = [
        env_dir("SystemRoot"),
        env_dir("ProgramFiles"),
        env_dir("ProgramFiles(x86)"),
        env_dir("ProgramW6432"),
        env_dir("ProgramData"),
        env_dir("USERPROFILE"),
        env_dir("LOCALAPPDATA"),
        env_dir("APPDATA"),
    ];
    if wide.iter().flatten().any(|w| *w == d) {
        return true;
    }
    // A drive's root, or a folder libraries are made of.
    let parts = dir.components().filter(|c| !matches!(c, Component::RootDir)).count();
    if parts <= 1 {
        return true;
    }
    matches!(
        name_of(dir).as_str(),
        "games" | "common" | "steamapps" | "steamlibrary" | "steam" | "epic games" | "gog games" | "gog galaxy" | "xboxgames" | "program files" | "program files (x86)" | "users"
    )
}

/// The game's folder for the program `exe` (a full path), and how it was found.
pub fn find(exe: &Path, folders: &[PathBuf]) -> Option<(PathBuf, &'static str)> {
    let dir = exe.parent()?;
    let found = steam(dir)
        .map(|r| (r, "its Steam library folder"))
        .or_else(|| epic(dir).map(|r| (r, "Epic's install list")))
        .or_else(|| under_folders(dir, folders).map(|r| (r, "one of your game folders")))
        .or_else(|| unreal(dir).map(|r| (r, "Unreal's folder layout")))
        .unwrap_or_else(|| (climb(dir), "the program's folder"));
    (!too_wide(&found.0)).then_some(found)
}

/// `<library>\steamapps\common\<game>`.
fn steam(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|a| a.parent().is_some_and(|p| name_of(p) == "common" && p.parent().is_some_and(|s| name_of(s) == "steamapps")))
        .map(Path::to_path_buf)
}

/// The install folder from Epic's manifests that holds `dir`.
fn epic(dir: &Path) -> Option<PathBuf> {
    let data = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let manifests = data.join(r"Epic\EpicGamesLauncher\Data\Manifests");
    let d = lower(dir) + "\\";
    std::fs::read_dir(manifests)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("item")))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter_map(|v| v.get("InstallLocation").and_then(|l| l.as_str()).map(PathBuf::from))
        .filter(|l| d.starts_with(&(lower(l) + "\\")))
        .max_by_key(|l| lower(l).len())
}

/// The folder right below one of `folders` that holds `dir`.
fn under_folders(dir: &Path, folders: &[PathBuf]) -> Option<PathBuf> {
    let d = lower(dir);
    let folder = folders
        .iter()
        .filter(|f| !f.as_os_str().is_empty())
        .filter(|f| d.starts_with(&(lower(f) + "\\")))
        .max_by_key(|f| lower(f).len())?;
    let f = lower(folder);
    dir.ancestors().find(|a| a.parent().is_some_and(|p| lower(p) == f)).map(Path::to_path_buf)
}

/// `<root>\<Project>\Binaries\<Platform>` with `<root>\Engine` beside the project.
fn unreal(dir: &Path) -> Option<PathBuf> {
    let binaries = dir.parent()?;
    if name_of(binaries) != "binaries" {
        return None;
    }
    let root = binaries.parent()?.parent()?;
    root.join("Engine").is_dir().then(|| root.to_path_buf())
}

/// `dir`, climbing out of build-named folders.
fn climb(dir: &Path) -> PathBuf {
    let mut d = dir;
    while BUILD_DIRS.contains(&name_of(d).as_str()) {
        match d.parent() {
            Some(p) if !too_wide(p) => d = p,
            _ => break,
        }
    }
    d.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn steam_and_game_folders() {
        let p = Path::new(r"D:\SteamLibrary\steamapps\common\Some Game\Binaries\Win64");
        assert_eq!(steam(p).unwrap(), Path::new(r"D:\SteamLibrary\steamapps\common\Some Game"));
        assert!(steam(Path::new(r"D:\Games\Some Game")).is_none());
        let folders = [PathBuf::from(r"C:\Games"), PathBuf::from(r"D:\Games\")];
        assert_eq!(under_folders(Path::new(r"c:\games\Some Game\bin"), &folders).unwrap(), Path::new(r"c:\games\Some Game"));
        assert_eq!(under_folders(Path::new(r"D:\Games\Other\x64\Release"), &folders).unwrap(), Path::new(r"D:\Games\Other"));
        assert!(under_folders(Path::new(r"C:\Games"), &folders).is_none());
        assert!(under_folders(Path::new(r"C:\Gamesish\X"), &folders).is_none());
    }

    #[test]
    fn climbs_out_of_build_folders_but_not_too_far() {
        assert_eq!(climb(Path::new(r"E:\Stuff\Some Game\bin\x64")), Path::new(r"E:\Stuff\Some Game"));
        assert_eq!(climb(Path::new(r"E:\Stuff\Some Game")), Path::new(r"E:\Stuff\Some Game"));
        // A drive's root is never the game's folder.
        assert_eq!(climb(Path::new(r"E:\bin")), Path::new(r"E:\bin"));
        assert_eq!(find(Path::new(r"E:\bin\game.exe"), &[]).unwrap().0, Path::new(r"E:\bin"));
        assert!(find(Path::new(r"E:\game.exe"), &[]).is_none());
        assert!(too_wide(Path::new(r"D:\SteamLibrary\steamapps\common")));
        assert!(too_wide(Path::new(r"C:\Games")));
        assert!(!too_wide(Path::new(r"C:\Games\Some Game")));
    }

    #[test]
    fn finds_unreal_layouts() {
        let base = std::env::temp_dir().join(format!("peerparty-root-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("Some Game");
        let bin = root.join("Proj").join("Binaries").join("Win64");
        fs::create_dir_all(&bin).unwrap();
        assert!(unreal(&bin).is_none(), "no Engine folder yet");
        fs::create_dir_all(root.join("Engine")).unwrap();
        assert_eq!(unreal(&bin).unwrap(), root);
        let (found, _) = find(&bin.join("Proj-Win64-Shipping.exe"), &[]).unwrap();
        assert_eq!(found, root);
        let _ = fs::remove_dir_all(&base);
    }
}
