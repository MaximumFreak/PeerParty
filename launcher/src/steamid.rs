//! A Steam game's app id, for a game started outside the Steam client: Steam's
//! `steam_api` needs it to tell Steam which game this is, and without it `SteamAPI_Init`
//! fails. Steam sets `SteamAppId` itself when it starts a game; otherwise PeerParty
//! looks, in order: the app manifest of the Steam library the game is in
//! (`steamapps\common\<dir>`), a `steam_appid.txt` next to any program in the game's
//! folder (the one started first), then any Steam library's manifest whose install
//! folder has the name of one of the exe's folders (a copy moved out of Steam).
//! PeerParty puts what it finds in `SteamAppId`, which every program the game starts
//! inherits, so a `steam_appid.txt` next to a starter covers the game it starts too.
//!
//! Only a game that uses Steam asks for one: one with a `steam_api.dll` or
//! `steam_api64.dll` in its folder. When none of those places has its id, the log and
//! the game's panel say so, with the fix: a `steam_appid.txt` holding the id.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ui::scan::{acf_value, steam_dir, steam_libraries};

/// What a look through the game's folder found.
pub struct Game {
    /// A `steam_api.dll` or `steam_api64.dll` is in it: the game uses Steam.
    pub uses_steam: bool,
    /// The folders that hold a program, the started program's first: where a
    /// `steam_appid.txt` goes.
    programs: Vec<PathBuf>,
}

/// How many folders' programs a look keeps, and how many entries it reads at most, so a
/// huge folder answers quickly.
const MAX_PROGRAM_DIRS: usize = 64;
const MAX_ENTRIES: usize = 20_000;

impl Game {
    /// Nothing looked through: for a game whose id is known already.
    pub fn none() -> Game {
        Game { uses_steam: false, programs: Vec::new() }
    }

    /// Looks through `root` (the game's folder, else the exe's) and a few folders down,
    /// the shallow ones first, where the programs are: Unreal keeps `steam_api64.dll` in `Engine\Binaries\ThirdParty\Steamworks\<ver>\Win64`
    /// and the game's program in `<Game>\Binaries\Win64`.
    pub fn look(exe: &Path, root: Option<&Path>) -> Game {
        let mut g = Game { uses_steam: false, programs: exe.parent().map(Path::to_path_buf).into_iter().collect() };
        let Some(root) = root.or(exe.parent()) else { return g };
        let mut left = MAX_ENTRIES;
        let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), 0u8)]);
        'walk: while let Some((dir, depth)) = queue.pop_front() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            let mut program = false;
            for e in entries.flatten() {
                left = match left.checked_sub(1) {
                    Some(n) => n,
                    None => break 'walk,
                };
                let Ok(kind) = e.file_type() else { continue };
                if kind.is_dir() {
                    if depth < 7 {
                        queue.push_back((e.path(), depth + 1));
                    }
                    continue;
                }
                let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                g.uses_steam |= n == "steam_api.dll" || n == "steam_api64.dll";
                program |= n.ends_with(".exe");
            }
            if program && g.programs.len() < MAX_PROGRAM_DIRS && !g.programs.iter().any(|p| p.as_os_str().eq_ignore_ascii_case(dir.as_os_str())) {
                g.programs.push(dir);
            }
        }
        g
    }

    /// The app id for the game `exe`, if any of the places above has it.
    pub fn app_id(&self, exe: &Path) -> Option<String> {
        library(exe)
            .or_else(|| self.programs.iter().find_map(|d| txt(d)))
            .or_else(|| any_library(exe))
    }

    /// Whether this is a Steam game none of the places above has an id for.
    pub fn missing(&self, exe: &Path) -> bool {
        self.uses_steam && self.app_id(exe).is_none()
    }
}

/// The manifest of the Steam library the exe is installed in: the exe sits under
/// `<library>\steamapps\common\<installdir>`, and the library's `appmanifest_<id>.acf`
/// names that installdir.
fn library(exe: &Path) -> Option<String> {
    let install = exe.ancestors().find(|a| {
        let common = a.parent();
        let steamapps = common.and_then(Path::parent);
        common.and_then(Path::file_name).is_some_and(|n| n.eq_ignore_ascii_case("common"))
            && steamapps.and_then(Path::file_name).is_some_and(|n| n.eq_ignore_ascii_case("steamapps"))
    })?;
    let dir = install.file_name()?.to_string_lossy().to_string();
    manifests(install.parent()?.parent()?)
        .iter()
        .find(|text| acf_value(text, "installdir").is_some_and(|d| d.eq_ignore_ascii_case(&dir)))
        .and_then(|text| acf_value(text, "appid"))
}

/// A `steam_appid.txt` in `dir` that holds an id.
fn txt(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("steam_appid.txt")).ok()?;
    let id = text.trim();
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

/// Whichever installed Steam game's install folder has the name of one of the exe's
/// folders (nearest first), for a copy that lives outside the Steam library.
fn any_library(exe: &Path) -> Option<String> {
    let libs = steam_dir().map(|s| steam_libraries(&s)).unwrap_or_default();
    let all: Vec<String> = libs.iter().flat_map(|l| manifests(l)).collect();
    for folder in exe.ancestors().skip(1).filter_map(Path::file_name) {
        let folder = folder.to_string_lossy();
        if let Some(text) = all.iter().find(|t| acf_value(t, "installdir").is_some_and(|d| d.eq_ignore_ascii_case(&folder))) {
            return acf_value(text, "appid");
        }
    }
    None
}

/// The text of every `appmanifest_<id>.acf` in a library's `steamapps` folder.
fn manifests(steamapps: &Path) -> Vec<String> {
    let Ok(dir) = fs::read_dir(steamapps) else { return Vec::new() };
    dir.flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_ascii_lowercase();
            n.starts_with("appmanifest_") && n.ends_with(".acf")
        })
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_app_id_of_a_game_in_a_steam_library() {
        let lib = std::env::temp_dir().join(format!("peerparty-acf-{}", std::process::id()));
        let game = lib.join("steamapps").join("common").join("Some Game").join("bin");
        fs::create_dir_all(&game).unwrap();
        fs::write(
            lib.join("steamapps").join("appmanifest_42.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"42\"\n\t\"installdir\"\t\t\"Some Game\"\n}\n",
        )
        .unwrap();
        assert_eq!(library(&game.join("game.exe")).as_deref(), Some("42"));
        assert_eq!(library(&lib.join("elsewhere.exe")), None);
        let _ = fs::remove_dir_all(&lib);
    }

    /// An Unreal-shaped Steam game: missing an id until a steam_appid.txt gives one,
    /// next to the starter or next to the program it starts. Without steam_api it is
    /// no Steam game, and never missing one.
    #[test]
    fn a_steam_game_without_an_id_is_missing_one() {
        let root = std::env::temp_dir().join(format!("peerparty-noid-{}-zq7x", std::process::id()));
        let shipping = root.join("Game").join("Binaries").join("Win64");
        fs::create_dir_all(&shipping).unwrap();
        fs::write(shipping.join("Game-Win64-Shipping.exe"), b"").unwrap();
        let starter = root.join("Game.exe");
        fs::write(&starter, b"").unwrap();
        assert!(!Game::look(&starter, Some(&root)).missing(&starter), "no steam_api: not a Steam game");

        let api = root.join("Engine").join("Binaries").join("ThirdParty").join("Steamworks").join("Steamv157").join("Win64");
        fs::create_dir_all(&api).unwrap();
        fs::write(api.join("steam_api64.dll"), b"").unwrap();
        let g = Game::look(&starter, Some(&root));
        assert!(g.uses_steam);
        assert!(g.missing(&starter));

        // Next to the program the starter starts.
        fs::write(shipping.join("steam_appid.txt"), "480\n").unwrap();
        assert_eq!(Game::look(&starter, Some(&root)).app_id(&starter).as_deref(), Some("480"));
        // Next to the starter, which comes first.
        fs::write(root.join("steam_appid.txt"), "481").unwrap();
        assert_eq!(Game::look(&starter, Some(&root)).app_id(&starter).as_deref(), Some("481"));
        let _ = fs::remove_dir_all(&root);
    }
}
