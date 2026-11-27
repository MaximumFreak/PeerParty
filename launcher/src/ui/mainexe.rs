//! Which program is a game's own, for a folder with several. The rules of the Game Bar
//! add-on's local games importer (its MainExecutable.cs, from the HTPC's
//! main_program()): of the .exe files up to 6 folders down, not in a redistributables
//! folder and not named as an installer or the like, a Unity game's (the one with its
//! `<name>_Data` folder beside it), else the largest. Ties go to the shallower, then by
//! path. An old renderer's build (`game_dx11.exe`) never wins over the same program
//! beside it with a newer renderer or none (`game.exe`).
//!
//! Two rules on top of the importer's, for folders where a game ships its own tools
//! beside it, each as big as the game and loading the same SDKs: a program named as a
//! tool (an editor, a packer, a dedicated server) wins only when nothing else is left,
//! and one named after the folder ("SG2" or "SomeGame" in "Some Game II") wins over a
//! bigger one that isn't, as long as it's no stub (a quarter of the biggest's size).
//!
//! And one for a folder holding two copies of one build (the same link, by the time
//! stamp in their headers) where one was changed after it shipped: the changed copy is
//! the one the folder is set up to run. Changed means its certificate table is cut off
//! or gone while the other's is whole, or, with nothing to tell them apart there, that
//! it was written later. The other ranks after it, to be picked by hand.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use super::pattern::Pattern;

pub const MAX_DEPTH: usize = 6;

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

/// Programs that come with a game and aren't it. Whole names, case ignored, no ".exe";
/// ranked last rather than left out, since a game's own name may end in one of these.
const TOOL_NAMES: &[&str] = &[
    r".*(editor|toolkit|tools?|packager|packer|unpacker|modkit|devkit|sdk|uploader|converter|compiler|configurator|config|settings|benchmark|server)([ _-]?(64|32|x64|x86))?",
    r".*(map|level|world)[ _-]?(maker|builder)([ _-]?(64|32|x64|x86))?",
];

/// Words of a program's name that say how it was built, not what it is.
const BUILD_WORDS: &[&str] = &["win64", "win32", "x64", "x86", "64", "32", "shipping", "release", "final"];

/// A stub (a launcher that starts the real program) is smaller than this share of the
/// biggest program, so naming the folder doesn't make it the game.
const STUB_SHARE: u64 = 4;

/// Two copies of one build differ in size by less than this share of the bigger, so a
/// program and its tool linked in the same second aren't taken for copies.
const COPY_SHARE: u64 = 8;

const RENDERER_TAGS: &[&str] = &[r"(dx|d3d)(9|10|11|12)", r"vulkan", r"vk"];
const OLD_RENDERER_TAGS: &[&str] = &[r"(dx|d3d)(9|10|11)"];

fn whole(names: &[&[&str]], suffix: &str) -> Pattern {
    let all: Vec<&str> = names.iter().flat_map(|n| n.iter().copied()).collect();
    Pattern::new(&format!("(?i)^({}){suffix}$", all.join("|")))
}

/// A renderer tag at the end of a name or before a separator. (The C# original uses a
/// lookahead for the separator; here it's matched and put back, `without_tags`.)
fn tag(tags: &[&str]) -> Pattern {
    Pattern::new(&format!(r"(?i)[ _.-]?({})(?P<sep>$|[ _.-])", tags.join("|")))
}

/// `name` with each renderer tag taken out (its separator kept).
fn without_tags(tags: &Pattern, name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut from = 0;
    while let Some((start, end)) = tags.find(name, from) {
        out.push_str(&name[from..start]);
        // The match ends with the separator after the tag, or at the end of the name
        // (a tag never ends in a separator character).
        let last = name.as_bytes()[end - 1];
        if b" _.-".contains(&last) {
            out.push(last as char);
        }
        from = end;
    }
    out.push_str(&name[from..]);
    out
}

static MAIN_NOT: LazyLock<Pattern> = LazyLock::new(|| whole(&[UTILITY_NAMES, MAIN_NOT_EXTRA], r"\.exe"));
static TOOL: LazyLock<Pattern> = LazyLock::new(|| whole(&[TOOL_NAMES], ""));
static NOT_GAME_DIR: LazyLock<Pattern> = LazyLock::new(|| whole(&[REDIST_DIRS], ""));
static RENDERER_TAG: LazyLock<Pattern> = LazyLock::new(|| tag(RENDERER_TAGS));
static OLD_RENDERER_TAG: LazyLock<Pattern> = LazyLock::new(|| tag(OLD_RENDERER_TAGS));

/// The game's main program, or None if the folder has none.
pub fn find(folder: &Path) -> Option<PathBuf> {
    choose(folder, &executables(folder))
}

/// Every program in `folder` that could be the game, the likeliest first (the one
/// `find` picks), for picking another by hand.
pub fn programs(folder: &Path) -> Vec<PathBuf> {
    rank(folder, &executables(folder)).into_iter().map(|c| c.path).collect()
}

/// A name's words for matching: lower case, letters and digits only, a roman numeral
/// as its number ("Some Game II" is some, game, 2).
fn words(name: &str) -> Vec<String> {
    name.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let w = w.to_lowercase();
            const ROMAN: &[&str] = &["ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"];
            match ROMAN.iter().position(|r| *r == w) {
                Some(i) => (i + 2).to_string(),
                None => w,
            }
        })
        .collect()
}

/// Whether a program's name (`stem`, no ".exe") is its folder's: the same letters, the
/// folder's with more after them ("SomeGameClient"), or an abbreviation of them (each
/// letter in order, from the first: "SG2" for "Some Game II"). Build words
/// ("-Win64-Shipping") don't count.
fn names_folder(stem: &str, folder: &str) -> bool {
    let folder: String = words(&super::scan::clean_name(folder)).concat();
    let stem: String = words(stem).into_iter().filter(|w| !BUILD_WORDS.contains(&w.as_str())).collect();
    if stem.chars().count() < 2 || folder.is_empty() {
        return false;
    }
    if stem.starts_with(&folder) {
        return true;
    }
    if stem.chars().next() != folder.chars().next() {
        return false;
    }
    let mut rest = folder.chars();
    stem.chars().all(|c| rest.any(|f| f == c))
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

/// Which link a program came from, and whether its certificate table is whole.
#[derive(Clone, Copy)]
struct Build {
    machine: u16,
    stamp: u32,
    certificate: bool,
}

/// `exe`'s build, from its PE headers (`len` bytes long), or None if it isn't a PE file
/// or carries no time stamp (nothing then says two programs are the same link).
fn build(exe: &Path, len: u64) -> Option<Build> {
    let mut head = Vec::with_capacity(4096);
    fs::File::open(exe).ok()?.take(4096).read_to_end(&mut head).ok()?;
    let rd16 = |o: usize| head.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let rd32 = |o: usize| head.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let nt = rd32(0x3c)? as usize;
    if rd16(0)? != 0x5a4d || rd32(nt)? != 0x4550 {
        return None;
    }
    let opt = nt + 24;
    let dirs = match rd16(opt)? {
        0x20b => opt + 112,
        0x10b => opt + 96,
        _ => return None,
    };
    // The fifth data directory names the certificate table by its file offset.
    let certificate = rd32(dirs - 4)? > 4 && {
        let (at, size) = (rd32(dirs + 32)?, rd32(dirs + 36)?);
        size > 0 && u64::from(at) + u64::from(size) <= len
    };
    let stamp = rd32(nt + 8)?;
    (stamp != 0).then_some(Build { machine: rd16(nt + 4)?, stamp, certificate })
}

struct Candidate {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    build: Option<Build>,
    /// A copy of the same build beside it was changed after it shipped.
    superseded: bool,
    depth: usize,
    sort_key: String,
    unity: bool,
    dir: PathBuf,
    base: String,
    old_renderer: bool,
    tool: bool,
    /// Named after the folder, and no stub.
    named: bool,
}

/// The rules over these programs of `folder`.
pub fn choose(folder: &Path, exes: &[PathBuf]) -> Option<PathBuf> {
    let ranked = rank(folder, exes);
    let first = ranked.first()?;
    // Said only where these rules beat the importer's (largest wins), so a game's log
    // shows why its program isn't the biggest one in its folder.
    if let Some(largest) = ranked.iter().filter(|c| c.unity == first.unity).max_by_key(|c| c.size) {
        if largest.path != first.path {
            let why = if largest.superseded {
                "the same build as shipped; this copy was changed from it"
            } else if largest.tool {
                "named as a tool"
            } else {
                "not named after the folder"
            };
            eprintln!(
                "peerparty: library: {}: {} rather than the bigger {} ({why})",
                folder.display(),
                first.sort_key,
                largest.sort_key
            );
        }
    }
    ranked.into_iter().next().map(|c| c.path)
}

/// `choose`'s candidates, best first.
fn rank(folder: &Path, exes: &[PathBuf]) -> Vec<Candidate> {
    let folder_name = folder.file_name().unwrap_or_default().to_string_lossy().to_string();
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
            modified: meta.modified().ok(),
            build: build(exe, meta.len()),
            superseded: false,
            depth: parts.len(),
            sort_key: parts.join("/").to_lowercase(),
            unity: dir.join(format!("{stem}_Data")).is_dir(),
            base: without_tags(&RENDERER_TAG, &stem).to_lowercase(),
            old_renderer: OLD_RENDERER_TAG.is_match(&stem),
            tool: TOOL.is_match(&stem),
            named: names_folder(&stem, &folder_name),
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
    let superseded: Vec<bool> = candidates.iter().map(|c| candidates.iter().any(|o| changed_copy_of(o, c))).collect();
    for (c, s) in candidates.iter_mut().zip(superseded) {
        c.superseded = s;
    }
    // A Unity game's program first (the importer drops the rest; they stay here, after
    // it, to be picked by hand).
    let unity = candidates.iter().any(|c| c.unity);
    let biggest = candidates.iter().filter(|c| !c.tool && c.unity == unity).map(|c| c.size).max().unwrap_or(0);
    for c in &mut candidates {
        c.named &= c.size.saturating_mul(STUB_SHARE) >= biggest;
    }
    candidates.sort_by(|a, b| {
        b.unity
            .cmp(&a.unity)
            .then(a.tool.cmp(&b.tool))
            .then(a.superseded.cmp(&b.superseded))
            .then(b.named.cmp(&a.named))
            .then(b.size.cmp(&a.size))
            .then(a.depth.cmp(&b.depth))
            .then(a.sort_key.cmp(&b.sort_key))
    });
    candidates
}

/// Whether `copy` is `of` (another program in its folder, from the same link) changed
/// after it shipped.
fn changed_copy_of(copy: &Candidate, of: &Candidate) -> bool {
    let (Some(a), Some(b)) = (copy.build, of.build) else { return false };
    if copy.path == of.path
        || copy.dir != of.dir
        || (a.machine, a.stamp) != (b.machine, b.stamp)
        || copy.size.abs_diff(of.size).saturating_mul(COPY_SHARE) >= copy.size.max(of.size)
    {
        return false;
    }
    if a.certificate != b.certificate {
        return b.certificate;
    }
    matches!((copy.modified, of.modified), (Some(c), Some(o)) if c > o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, usize)], dirs: &[&str]) -> PathBuf {
        tree_in("peerparty-mainexe", files, dirs)
    }

    /// The tree in a folder of this name (in a folder of its own, for this test run).
    fn tree_in(name: &str, files: &[(&str, usize)], dirs: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("peerparty-mainexe-{}", std::process::id())).join(name);
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

        let _ = fs::remove_dir_all(&root);
    }

    /// A game shipping its tools beside it, each bigger than it and loading the same
    /// SDKs: the program named after the folder wins, the tools rank last.
    #[test]
    fn tools_beside_the_game_lose() {
        let root = tree_in(
            "Mallet Keep II",
            &[("EDITOR.exe", 1250), ("MK2.exe", 700), ("MK2Toolkit.exe", 650), ("PACKAGER.exe", 640)],
            &[],
        );
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "MK2.exe");
        let names: Vec<String> = programs(&root).iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, ["MK2.exe", "EDITOR.exe", "MK2Toolkit.exe", "PACKAGER.exe"]);

        // Without the folder's name to go by, the tools still lose.
        let root = tree_in("Elsewhere", &[("LevelEditor.exe", 900), ("Run.exe", 500), ("DedicatedServer.exe", 800)], &[]);
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "Run.exe");
        // Only tools: the biggest.
        let root = tree_in("Tools only", &[("Editor.exe", 900), ("Packer.exe", 500)], &[]);
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "Editor.exe");

        // A stub named after the folder doesn't beat the real program; a shipping build
        // is named after it too.
        let root = tree_in(
            "Copper Tide",
            &[("CopperTide.exe", 10), ("Binaries/Win64/CopperTide-Win64-Shipping.exe", 900), ("Benchmark.exe", 950)],
            &[],
        );
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "CopperTide-Win64-Shipping.exe");
        let root = tree_in("Copper Tide 2", &[("CopperTide.exe", 10), ("Other.exe", 900)], &[]);
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "Other.exe");
        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    /// A 64-bit PE file of `len` bytes linked at `stamp`, its certificate table (offset,
    /// size) as given.
    fn pe(stamp: u32, certificate: (u32, u32), len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        b[0..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        b[0x40..0x44].copy_from_slice(b"PE\0\0");
        b[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
        b[0x48..0x4c].copy_from_slice(&stamp.to_le_bytes());
        let opt = 0x58;
        b[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        b[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        b[opt + 144..opt + 148].copy_from_slice(&certificate.0.to_le_bytes());
        b[opt + 148..opt + 152].copy_from_slice(&certificate.1.to_le_bytes());
        b
    }

    /// Writes `files` (name, bytes, seconds after a fixed time) into a folder `name`, in
    /// a folder of its own (the other tests here remove theirs while it runs).
    fn programs_in(name: &str, files: &[(&str, Vec<u8>, u64)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("peerparty-mainexe-copies-{}", std::process::id())).join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
        for (f, bytes, at) in files {
            fs::write(root.join(f), bytes).unwrap();
            let file = fs::File::options().write(true).open(root.join(f)).unwrap();
            file.set_modified(base + std::time::Duration::from_secs(*at)).unwrap();
        }
        root
    }

    /// Two copies of one build, one changed after it shipped: the changed copy wins
    /// over the bigger one as shipped, even one written later.
    #[test]
    fn a_changed_copy_of_the_build_wins() {
        let shipped = pe(1_509_000_000, (9000, 1000), 10_000);
        let mut cut = pe(1_509_000_000, (9000, 1000), 9_400);
        cut[0x200] = 1;
        let root = programs_in("Iron Vale", &[("iv_sp64_ship.exe", shipped.clone(), 100), ("Play.exe", cut, 50)]);
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "Play.exe");
        let names: Vec<String> = programs(&root).iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, ["Play.exe", "iv_sp64_ship.exe"]);

        // The certificate table gone, not cut off.
        let root = programs_in("Iron Vale 2", &[("Ship.exe", shipped.clone(), 100), ("Play.exe", pe(1_509_000_000, (0, 0), 9_000), 50)]);
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "Play.exe");

        // No certificates: the copy written later.
        let root = programs_in(
            "Iron Vale 3",
            &[("Run.exe", pe(1_509_000_000, (0, 0), 10_000), 50), ("Start.exe", pe(1_509_000_000, (0, 0), 9_800), 100)],
        );
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "Start.exe");

        // Another link, or too far apart in size to be a copy: the biggest, as before.
        let root = programs_in("Iron Vale 4", &[("Ship.exe", shipped.clone(), 100), ("Play.exe", pe(1_509_000_001, (0, 0), 9_400), 50)]);
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "Ship.exe");
        let root = programs_in("Iron Vale 5", &[("Ship.exe", shipped, 100), ("Play.exe", pe(1_509_000_000, (0, 0), 5_000), 50)]);
        assert_eq!(find(&root).unwrap().file_name().unwrap(), "Ship.exe");
        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn names_its_folder() {
        assert!(names_folder("MK2", "Mallet Keep II"));
        assert!(names_folder("MalletKeep2", "Mallet Keep II"));
        assert!(names_folder("mallet_keep", "Mallet Keep (v1.2) [GOG]"));
        assert!(names_folder("MalletKeepClient", "Mallet Keep"));
        assert!(names_folder("MalletKeep-Win64-Shipping", "Mallet Keep"));
        assert!(!names_folder("MK2Toolkit", "Mallet Keep II"));
        assert!(!names_folder("EDITOR", "Mallet Keep II"));
        assert!(!names_folder("K2", "Mallet Keep II"), "an abbreviation starts where the name does");
        assert!(!names_folder("M", "Mallet Keep II"), "one letter says nothing");
        assert!(TOOL.is_match("MK2Toolkit"));
        assert!(TOOL.is_match("Map Maker x64"));
        assert!(!TOOL.is_match("MK2"));
    }

    /// The rules as built, against the regex crate's reading of the same patterns.
    #[test]
    fn rules_match_like_regex() {
        let all: Vec<&str> = [UTILITY_NAMES, MAIN_NOT_EXTRA].iter().flat_map(|n| n.iter().copied()).collect();
        let main_not = regex::Regex::new(&format!(r"(?i)^({})\.exe$", all.join("|"))).unwrap();
        let dirs = regex::Regex::new(&format!("(?i)^({})$", REDIST_DIRS.join("|"))).unwrap();
        let tool = regex::Regex::new(&format!("(?i)^({})$", TOOL_NAMES.join("|"))).unwrap();
        let renderer = regex::Regex::new(&format!(r"(?i)[ _.-]?({})(?P<sep>$|[ _.-])", RENDERER_TAGS.join("|"))).unwrap();
        let names = [
            "unins000.exe", "UnityCrashHandler64.exe", "game.exe", "Game_dx11.exe", "setup.exe", "my setup tool.exe",
            "dxwnd.exe", "cdb.exe", "cdbx.exe", "Overlay64.exe", "GameLauncher.exe", "Battle.net-Setup.exe", "redist",
            "_CommonRedist", "Support", "supporting", "__MACOSX", "game_dx11_vk", "Game-D3D12", "game.vulkan.x", "dx9",
            "game dx10 ", "game_dx13", "game__dx11", "über_dx11", "日本_dx9", "pbsvc_r.exe", "Touchup.exe", "",
            "EDITOR", "MK2Toolkit", "Map Maker x64", "LevelBuilder_64", "DedicatedServer", "Settings", "tool", "mk2",
        ];
        for n in names {
            assert_eq!(MAIN_NOT.is_match(n), main_not.is_match(n), "main-not {n:?}");
            assert_eq!(NOT_GAME_DIR.is_match(n), dirs.is_match(n), "dir {n:?}");
            assert_eq!(TOOL.is_match(n), tool.is_match(n), "tool {n:?}");
            assert_eq!(without_tags(&RENDERER_TAG, n), renderer.replace_all(n, "${sep}"), "tags {n:?}");
        }
    }
}
