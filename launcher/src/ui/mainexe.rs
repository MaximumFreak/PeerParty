//! Which program is a game's own, for a folder with several. The rules of the Game Bar
//! add-on's local games importer (its MainExecutable.cs, from the
//! HTPC's main_program()): a folder's `.gamebox.json` may name it; otherwise, of the .exe
//! files up to 6 folders down, not in a redistributables folder and not named as a
//! tool, a Unity game's (the one with its `<name>_Data` folder beside it), else the
//! largest. Ties go to the shallower, then by path. An old renderer's build
//! (`game_dx11.exe`) never wins over the same program beside it with a newer renderer
//! or none (`game.exe`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

pub const MAX_DEPTH: usize = 6;
const GAMEBOX: &str = ".gamebox.json";

/// Programs that are never the game. Whole names, case ignored, no ".exe".
const UTILITY_NAMES: &[&str] = &[
    r"unins\d*", r"uninstall.*",
    r".*crash[ _-]?(handler|report|pad|uploader|sender|dump|rpt).*",
    r"dxsetup", r"dxwebsetup", r"dsetup", r"vc_?redist.*", r"vcredist.*", r"dotnet.*", r"ndp\d.*", r"netfx.*",
    r"windowsdesktop-runtime.*", r"physx.*", r"oalinst", r"ue[345]?prereqsetup.*", r"easyanticheat_(eos_)?setup",
    r"7z.*", r"dosbox.*",
];

/// Never the main program of an installed game, on top of UTILITY_NAMES.
const MAIN_NOT_EXTRA: &[&str] = &[
    r"(.*[ _-])?(setup|installer)([ _-].*)?", r"(.*[ _-])?(updater|patcher)(64)?", r".*redist.*", r"directx.*",
    r"visualc.*", r"xnafx.*", r"crs-.*", r".*breakpad.*", r".*language[ ._-]?(changer|select(or)?|setup).*",
    r".*selector", r".*webhelper.*", r".*subprocess.*", r".*overlay.*", r".*injector.*", r".*bootstrap.*",
    r".*mod ?manager.*", r".*artbook.*", r"pbsvc.*", r"eacoreserver", r"eaproxyinstaller", r"eaappinstaller.*",
    r"activationui", r"cleanup", r"touchup", r"battle\.net-setup.*", r"blizzardbrowser", r"ubisoftgamelauncher.*",
    r"installermessage", r"patchprogress", r"dxwnd.*", r"gamingrepair.*", r"netimguiserver", r"cdb",
];

/// Folders whose programs are never the game.
const REDIST_DIRS: &[&str] = &[
    r"_?commonredist", r"redist", r"redistributables?", r"_redist", r"_?redsit", r"directx", r"vcredist", r"vc_?redist",
    r"dotnet", r"\.net", r"physx", r"prerequisites", r"prereqs?", r"support", r"__installer", r"installers?",
    r"_installer", r"__macosx", r"artbook", r"gamingrepair",
];

const RENDERER_TAGS: &[&str] = &[r"(dx|d3d)(9|10|11|12)", r"vulkan", r"vk"];
const OLD_RENDERER_TAGS: &[&str] = &[r"(dx|d3d)(9|10|11)"];

fn whole(names: &[&[&str]], suffix: &str) -> Regex {
    let all: Vec<&str> = names.iter().flat_map(|n| n.iter().copied()).collect();
    Regex::new(&format!("(?i)^({}){suffix}$", all.join("|"))).unwrap()
}

/// A renderer tag at the end of a name or before a separator. (The C# original uses a
/// lookahead for the separator; here it's captured and put back.)
fn tag(tags: &[&str]) -> Regex {
    Regex::new(&format!(r"(?i)[ _.-]?({})(?P<sep>$|[ _.-])", tags.join("|"))).unwrap()
}

static MAIN_NOT: LazyLock<Regex> = LazyLock::new(|| whole(&[UTILITY_NAMES, MAIN_NOT_EXTRA], r"\.exe"));
static NOT_GAME_DIR: LazyLock<Regex> = LazyLock::new(|| whole(&[REDIST_DIRS], ""));
static RENDERER_TAG: LazyLock<Regex> = LazyLock::new(|| tag(RENDERER_TAGS));
static OLD_RENDERER_TAG: LazyLock<Regex> = LazyLock::new(|| tag(OLD_RENDERER_TAGS));

/// The game's main program, or None if the folder has none.
pub fn find(folder: &Path) -> Option<PathBuf> {
    recorded(folder).or_else(|| choose(folder, &executables(folder)))
}

/// Every .exe up to MAX_DEPTH folders down, skipping folders that can't hold the game.
pub fn executables(folder: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(folder.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let path = e.path();
            if ft.is_dir() {
                let name = e.file_name().to_string_lossy().to_string();
                if depth < MAX_DEPTH && !NOT_GAME_DIR.is_match(&name) {
                    stack.push((path, depth + 1));
                }
            } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")) {
                out.push(path);
            }
        }
    }
    out
}

/// The program `.gamebox.json` names (a relative path inside the folder), if it's there.
fn recorded(folder: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(folder.join(GAMEBOX)).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let main = json.get("main")?.as_str()?.trim();
    if main.is_empty() || Path::new(main).is_absolute() || main.split(['/', '\\']).any(|p| p == "..") {
        return None;
    }
    let full = folder.join(main.replace('/', "\\"));
    (full.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")) && full.is_file()).then_some(full)
}

struct Candidate {
    path: PathBuf,
    size: u64,
    depth: usize,
    sort_key: String,
    unity: bool,
    dir: PathBuf,
    base: String,
    old_renderer: bool,
}

/// The rules over these programs of `folder`.
pub fn choose(folder: &Path, exes: &[PathBuf]) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for exe in exes {
        let Ok(rel) = exe.strip_prefix(folder) else { continue };
        let parts: Vec<String> = rel.iter().map(|p| p.to_string_lossy().to_string()).collect();
        let Some((file, dirs)) = parts.split_last() else { continue };
        if dirs.len() > MAX_DEPTH || dirs.iter().any(|d| NOT_GAME_DIR.is_match(d)) || MAIN_NOT.is_match(file) {
            continue;
        }
        let Ok(meta) = fs::metadata(exe) else { continue };
        let dir = exe.parent().unwrap_or(folder).to_path_buf();
        let stem = exe.file_stem().unwrap_or_default().to_string_lossy().to_string();
        candidates.push(Candidate {
            path: exe.clone(),
            size: meta.len(),
            depth: parts.len(),
            sort_key: parts.join("/").to_lowercase(),
            unity: dir.join(format!("{stem}_Data")).is_dir(),
            base: RENDERER_TAG.replace_all(&stem, "${sep}").to_lowercase(),
            old_renderer: OLD_RENDERER_TAG.is_match(&stem),
            dir,
        });
    }
    let keep: Vec<bool> = candidates
        .iter()
        .map(|c| {
            !(c.old_renderer
                && candidates
                    .iter()
                    .any(|o| o.dir == c.dir && o.base == c.base && !o.old_renderer))
        })
        .collect();
    let mut candidates: Vec<Candidate> = candidates
        .into_iter()
        .zip(keep)
        .filter_map(|(c, k)| k.then_some(c))
        .collect();
    if candidates.iter().any(|c| c.unity) {
        candidates.retain(|c| c.unity);
    }
    candidates.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then(a.depth.cmp(&b.depth))
            .then(a.sort_key.cmp(&b.sort_key))
    });
    candidates.into_iter().next().map(|c| c.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, usize)], dirs: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("peerparty-mainexe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in dirs {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        for (f, size) in files {
            let p = root.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, vec![0u8; *size]).unwrap();
        }
        root
    }

    #[test]
    fn picks_like_the_game_bar_importer() {
        // Unity: the program beside its _Data folder beats the bigger crash handler.
        let root = tree(
            &[("Game.exe", 10), ("UnityCrashHandler64.exe", 500), ("Tool.exe", 400)],
            &["Game_Data"],
        );
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "Game.exe");

        // Largest, skipping redistributables and tools; old renderer loses.
        let root = tree(
            &[
                ("bin/game.exe", 100),
                ("bin/game_dx11.exe", 900),
                ("_CommonRedist/vcredist_x64.exe", 9000),
                ("unins000.exe", 9000),
                ("Launcher/GameLauncher.exe", 50),
            ],
            &[],
        );
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "game.exe");
        assert!(find(&root).unwrap().parent().unwrap().ends_with("bin"));

        // .gamebox.json wins when it names a real program inside the folder.
        let root = tree(&[("a.exe", 1), ("big.exe", 100)], &[]);
        fs::write(root.join(GAMEBOX), r#"{"main":"a.exe"}"#).unwrap();
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "a.exe");
        fs::write(root.join(GAMEBOX), r#"{"main":"../x.exe"}"#).unwrap();
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "big.exe");
        let _ = fs::remove_dir_all(&root);
    }
}
