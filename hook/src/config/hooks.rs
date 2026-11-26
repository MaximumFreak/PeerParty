//! The player's hooks files: functions PeerParty hooks in a module that doesn't export
//! them (a library linked into a game's exe or into one of its DLLs), found by their
//! bytes (sys/scan.rs). Bytes change from one build of a module to the next, so each
//! module has a file of its own, `%LOCALAPPDATA%\peerparty\patterns\<module>.json` (the
//! module's file name, `MyGame-Win64-Shipping.exe.json`; `PEERPARTY_PATTERNS_DIR` names
//! another folder), and its patterns are tied to a build by the module's SHA-256 or its
//! file version. PeerParty only reads these files.
//!
//! ```json
//! { "versions": [
//!     { "sha256": "9f86d0...", "curl_easy_setopt_patterns": [ "..." ], "curl_easy_cleanup_patterns": [ "..." ] },
//!     { "file_version": "4.27.2.0", "curl_easy_setopt_patterns": [ "..." ], "curl_easy_cleanup_patterns": [ "..." ] },
//!     { "curl_easy_setopt_patterns": [ "..." ], "curl_easy_cleanup_patterns": [ "..." ] } ] }
//! ```
//!
//! An entry applies when everything it names matches (no `sha256` or `file_version`: any
//! build). Of the entries that apply, one with a SHA-256 wins over one with a version,
//! which wins over one with neither; then the first. Within an entry, a function may
//! list several patterns: the first that matches exactly one place is used. Keys that
//! start with `_` are notes.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::log;
use crate::sys::scan::Pattern;
use crate::util::json::Json;

/// A function a hooks file can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Function {
    CurlEasySetopt,
    CurlEasyCleanup,
    CurlEasyReset,
}

impl Function {
    /// Its key in an entry, and its name in the log.
    const ALL: [(Function, &'static str, &'static str); 3] = [
        (Function::CurlEasySetopt, "curl_easy_setopt_patterns", "curl_easy_setopt"),
        (Function::CurlEasyCleanup, "curl_easy_cleanup_patterns", "curl_easy_cleanup"),
        (Function::CurlEasyReset, "curl_easy_reset_patterns", "curl_easy_reset"),
    ];

    pub fn name(self) -> &'static str {
        Function::ALL.iter().find(|(f, _, _)| *f == self).map_or("?", |(_, _, n)| n)
    }
}

/// One entry of a module's file: the build it's for, and its patterns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModuleHooks {
    /// Lowercase hex.
    pub sha256: Option<String>,
    pub file_version: Option<String>,
    pub functions: Vec<(Function, Vec<Pattern>)>,
}

impl ModuleHooks {
    pub fn patterns(&self, f: Function) -> &[Pattern] {
        self.functions.iter().find(|(g, _)| *g == f).map_or(&[], |(_, p)| p)
    }

    /// How sure a match is: 2 by SHA-256, 1 by version, 0 for any build.
    fn rank(&self) -> u8 {
        if self.sha256.is_some() {
            2
        } else {
            u8::from(self.file_version.is_some())
        }
    }
}

/// The files are the player's own, but bounded all the same: one this big or entries
/// this many are a mistake, not a configuration.
const MAX_FILE: u64 = 1 << 20;
const MAX_ENTRIES: usize = 64;
const MAX_PATTERNS: usize = 16;

/// The folder the files are in.
fn dir() -> Option<PathBuf> {
    super::HookConfig::global().patterns_dir.clone().or_else(|| Some(crate::util::local_store::peerparty_dir()?.join("patterns")))
}

/// The entry for the module loaded at `base` from `path`, named `name`, if its file
/// has one for this build. The module's SHA-256 is only worked out when an entry names
/// one, or to say in the log what an entry would need.
pub fn for_module(name: &str, base: usize, path: &str) -> Option<ModuleHooks> {
    for_module_in(&dir()?, name, base, path)
}

fn for_module_in(dir: &Path, name: &str, base: usize, path: &str) -> Option<ModuleHooks> {
    // A loader name is a file name; anything else can't name a file of ours.
    if name.is_empty() || name.contains(['/', '\\', ':']) || name.starts_with('.') {
        return None;
    }
    let file = dir.join(format!("{name}.json"));
    let size = std::fs::metadata(&file).ok().filter(|m| m.is_file())?.len();
    if size > MAX_FILE {
        log!("hooks: {} is over {MAX_FILE} bytes; not read", file.display());
        return None;
    }
    let Ok(text) = std::fs::read_to_string(&file) else {
        log!("hooks: {} can't be read", file.display());
        return None;
    };
    let (entries, problems) = parse(&text);
    for p in &problems {
        log!("hooks: {}: {p}", file.display());
    }
    let version = unsafe { crate::sys::pe::file_version(base as *const u8) };
    let mut hash = None;
    let mut sha = || hash.get_or_insert_with(|| sha256_of(Path::new(path))).clone();
    let wants_hash = entries.iter().any(|e| e.sha256.is_some());
    let found = choose(&entries, version.as_deref(), || if wants_hash { sha() } else { None });
    let shown = |h: Option<String>| h.map(|h| format!(", sha256 {h}")).unwrap_or_default();
    match found {
        Some(i) => {
            let by = ["for any build", "by its version", "by its sha256"][entries[i].rank() as usize];
            log!("hooks: {name} (version {}): entry {} of {} applies, {by}", version.as_deref().unwrap_or("none"), i + 1, file.display());
            Some(entries[i].clone())
        }
        None => {
            log!(
                "hooks: {name} (version {}{}): no entry of {} is for this build",
                version.as_deref().unwrap_or("none"),
                shown(sha()),
                file.display()
            );
            None
        }
    }
}

/// The entry that applies (its index), for a module with `version` and the SHA-256
/// `hash` gives (asked only when an entry names one).
pub fn choose(entries: &[ModuleHooks], version: Option<&str>, hash: impl FnOnce() -> Option<String>) -> Option<usize> {
    let mut hash = Some(hash);
    let mut known: Option<Option<String>> = None;
    let mut best: Option<usize> = None;
    for (i, e) in entries.iter().enumerate() {
        if let Some(want) = &e.sha256 {
            let have = known.get_or_insert_with(|| hash.take().and_then(|f| f()));
            if have.as_deref() != Some(want.as_str()) {
                continue;
            }
        }
        if let Some(want) = &e.file_version {
            if version != Some(want.as_str()) {
                continue;
            }
        }
        if best.is_none_or(|b| e.rank() > entries[b].rank()) {
            best = Some(i);
        }
    }
    best
}

/// A file's SHA-256, lowercase hex, read in pieces (a game's exe can be hundreds of MB).
fn sha256_of(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(crate::util::encoding::hex(&hasher.finalize()))
}

/// A file's entries, and what in it was left out and why.
pub fn parse(text: &str) -> (Vec<ModuleHooks>, Vec<String>) {
    let mut problems = Vec::new();
    let Some(root) = Json::parse(text.trim_start_matches('\u{feff}')) else {
        return (Vec::new(), vec!["not JSON".into()]);
    };
    let mut out: Vec<ModuleHooks> = Vec::new();
    for (n, entry) in root.get("versions").as_arr().iter().enumerate() {
        let at = format!("versions[{n}]");
        if out.len() == MAX_ENTRIES {
            problems.push(format!("more than {MAX_ENTRIES} entries; the rest are left out"));
            break;
        }
        let mut hooks = ModuleHooks::default();
        let mut usable = true;
        for (key, value) in entry.as_obj() {
            if key.starts_with('_') {
                continue;
            }
            if key == "sha256" {
                match value.as_str().map(|s| s.trim().to_ascii_lowercase()) {
                    Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => hooks.sha256 = Some(h),
                    _ => {
                        problems.push(format!("{at}: sha256 isn't 64 hex digits"));
                        usable = false;
                    }
                }
                continue;
            }
            if key == "file_version" {
                match value.as_str().map(str::trim).filter(|v| !v.is_empty()) {
                    Some(v) => hooks.file_version = Some(v.to_string()),
                    None => {
                        problems.push(format!("{at}: file_version isn't a version"));
                        usable = false;
                    }
                }
                continue;
            }
            let Some((f, _, _)) = Function::ALL.iter().find(|(_, k, _)| *k == key) else {
                problems.push(format!("{at}: unknown key {key:?}"));
                continue;
            };
            let mut patterns = Vec::new();
            for (i, p) in value.as_arr().iter().enumerate() {
                if patterns.len() == MAX_PATTERNS {
                    problems.push(format!("{at}: {key}: more than {MAX_PATTERNS} patterns; the rest are left out"));
                    break;
                }
                match p.as_str().map(Pattern::parse) {
                    Some(Ok(p)) => patterns.push(p),
                    Some(Err(e)) => problems.push(format!("{at}: {key}[{i}]: {e}")),
                    None => problems.push(format!("{at}: {key}[{i}] is not a string")),
                }
            }
            if !patterns.is_empty() {
                hooks.functions.push((*f, patterns));
            }
        }
        // An entry whose build can't be told is never used: it would apply to every build.
        if usable && !hooks.functions.is_empty() {
            out.push(hooks);
        } else if usable {
            problems.push(format!("{at}: no usable pattern"));
        }
    }
    if out.is_empty() && root.get("versions").as_arr().is_empty() {
        problems.push("no \"versions\" list".into());
    }
    (out, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETOPT: &str = "89 54 24 10 4C 89 44 24 18 4C 89 4C 24 20";

    fn entry(sha256: Option<&str>, version: Option<&str>) -> ModuleHooks {
        ModuleHooks {
            sha256: sha256.map(str::to_string),
            file_version: version.map(str::to_string),
            functions: vec![(Function::CurlEasySetopt, vec![Pattern::parse(SETOPT).unwrap()])],
        }
    }

    #[test]
    fn a_hooks_file_reads() {
        let text = format!(
            r#"{{ "_comment": "notes",
              "versions": [
                {{ "sha256": "{}", "_note": "x", "curl_easy_setopt_patterns": [ "{SETOPT}", "not a pattern", "01 02" ], "Typo": [] }},
                {{ "file_version": " 1.2.3.4 ", "curl_easy_cleanup_patterns": [ "40 53 48 83 EC 20 48 8B D9 48 85 C9 0F 84 ?? ?? 00 00" ] }},
                {{ "sha256": "short", "curl_easy_setopt_patterns": [ "{SETOPT}" ] }},
                {{ "curl_easy_reset_patterns": [ 5 ] }}
              ] }}"#,
            "AB".repeat(32)
        );
        let (hooks, problems) = parse(&text);
        assert_eq!(hooks.len(), 2, "{problems:?}");
        assert_eq!(hooks[0].sha256.as_deref(), Some("ab".repeat(32).as_str()));
        assert_eq!(hooks[0].patterns(Function::CurlEasySetopt).len(), 1);
        assert_eq!(hooks[1].file_version.as_deref(), Some("1.2.3.4"));
        assert_eq!(hooks[1].patterns(Function::CurlEasyCleanup).len(), 1);
        assert!(hooks[1].patterns(Function::CurlEasyReset).is_empty());
        // Two bad patterns, the typo, the short hash, the entry with nothing usable.
        assert_eq!(problems.len(), 6, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("Typo")));
        assert_eq!(Function::CurlEasyReset.name(), "curl_easy_reset");
    }

    #[test]
    fn the_most_specific_matching_entry_applies() {
        let h = "ab".repeat(32);
        let entries = vec![entry(None, None), entry(None, Some("1.0.0.0")), entry(Some(&h), None), entry(Some(&h), Some("2.0.0.0"))];
        // By hash, over the version and the catch-all.
        assert_eq!(choose(&entries, Some("1.0.0.0"), || Some(h.clone())), Some(2));
        // An entry needs all it names: hash right, version wrong.
        assert_eq!(choose(&entries[3..], Some("1.0.0.0"), || Some(h.clone())), None);
        assert_eq!(choose(&entries[3..], Some("2.0.0.0"), || Some(h.clone())), Some(0));
        // Another build: by version, else the catch-all.
        assert_eq!(choose(&entries, Some("1.0.0.0"), || Some("cd".repeat(32))), Some(1));
        assert_eq!(choose(&entries, Some("9.9.9.9"), || None), Some(0));
        assert_eq!(choose(&entries, None, || None), Some(0));
        // No catch-all: nothing for an unknown build.
        assert_eq!(choose(&entries[1..], Some("9.9.9.9"), || None), None);
        // The hash is worked out only when an entry names one.
        assert_eq!(choose(&entries[..2], Some("1.0.0.0"), || panic!("hashed")), Some(1));
    }

    #[test]
    fn a_files_sha256_is_its_contents() {
        let path = std::env::temp_dir().join(format!("peerparty-hooks-test-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_of(&path).as_deref(), Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(sha256_of(&path), None);
    }

    /// docs/examples/patterns/ reads as the docs say, with nothing left out.
    #[test]
    fn the_example_file_reads() {
        let (hooks, problems) = parse(include_str!("../../../docs/examples/patterns/MyGame-Win64-Shipping.exe.json"));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(hooks.len(), 3);
        assert!(hooks.iter().all(|m| !m.patterns(Function::CurlEasySetopt).is_empty() && !m.patterns(Function::CurlEasyCleanup).is_empty()));
        assert_eq!(hooks.iter().map(ModuleHooks::rank).collect::<Vec<_>>(), [2, 1, 0]);
    }

    #[test]
    fn a_broken_file_reads_as_none() {
        assert_eq!(parse("{").0, Vec::new());
        assert_eq!(parse("[]").0, Vec::new());
        assert_eq!(parse("{\"versions\": 3}").0, Vec::new());
        assert_eq!(parse("{\"Modules\": {}}").1, vec!["no \"versions\" list".to_string()]);
    }

    /// A loaded module's file in a folder of hooks files: its entry by its version, then by
    /// its SHA-256; another build's entries don't apply.
    #[test]
    fn a_loaded_modules_file_is_found_and_its_build_told() {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        let wide: Vec<u16> = "kernel32.dll".encode_utf16().chain(Some(0)).collect();
        let base = unsafe { GetModuleHandleW(wide.as_ptr()) } as usize;
        let path = Path::new(&std::env::var("SystemRoot").unwrap()).join("System32").join("kernel32.dll");
        let path = path.to_str().unwrap();
        let version = unsafe { crate::sys::pe::file_version(base as *const u8) }.unwrap();
        let hash = sha256_of(Path::new(path)).unwrap();
        let dir = std::env::temp_dir().join(format!("peerparty-hooks-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The file's name in any case, as Windows finds it.
        let write = |entries: &[String]| std::fs::write(dir.join("KERNEL32.DLL.json"), format!("{{\"versions\": [{}]}}", entries.join(","))).unwrap();
        let e = |cond: String, byte: &str| format!(r#"{{ {cond} "curl_easy_setopt_patterns": [ "{byte} 01 02 03 04 05" ] }}"#);
        let first = |m: Option<ModuleHooks>| m.map(|m| m.patterns(Function::CurlEasySetopt)[0].clone());
        let p = |byte: &str| Some(Pattern::parse(&format!("{byte} 01 02 03 04 05")).unwrap());
        let by_version = |v: &str| format!(r#""file_version": "{v}","#);
        write(&[e(by_version("0.0.0.1"), "AA"), e(by_version(&version), "BB"), e(String::new(), "CC")]);
        assert_eq!(first(for_module_in(&dir, "kernel32.dll", base, path)), p("BB"));
        write(&[e(by_version(&version), "BB"), e(format!(r#""sha256": "{}","#, hash.to_uppercase()), "DD")]);
        assert_eq!(first(for_module_in(&dir, "kernel32.dll", base, path)), p("DD"));
        write(&[e(by_version("0.0.0.1"), "AA")]);
        assert_eq!(for_module_in(&dir, "kernel32.dll", base, path), None);
        assert_eq!(for_module_in(&dir, "user32.dll", base, path), None, "no file");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_plain_file_names_name_a_file() {
        for bad in ["", "..\\x.exe", "a/b.dll", "C:x.exe", ".hidden"] {
            assert_eq!(for_module(bad, 0, ""), None, "{bad}");
        }
    }
}
