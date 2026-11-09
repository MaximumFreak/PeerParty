//! Starting a save from another id's, for games that key their saves by user id.
//!
//! Two cases, one mechanism:
//!
//! - **`--copy-saves`**: a second copy of a game on one PC starts from the first copy's
//!   progress, for testing multiplayer that a fresh save hasn't unlocked yet. A second
//!   copy plays under a LAN identity (a LAN SteamID, EOS id and PlayFab id), so a game
//!   that keys its saves by user id looks in a folder or file named after the LAN id
//!   and finds nothing. Files already there are replaced only by newer ones, so the
//!   second copy's own progress since survives until the main save moves on.
//! - **A SteamID of its own for this PC** (steamlan's `own_id_on`): the game's saves keyed
//!   by SteamID or account id are new ones, under the new id. The first time the game
//!   looks for one, the save under steam_api's SteamID is *copied* to the new name, once
//!   ever: only while nothing exists under the new name, so the game's progress under
//!   PeerParty is its own from then on. The original is only read, never written,
//!   renamed or deleted, so a game that rejects the copy (a save that names its owner
//!   inside) can only lose the copy. `PEERPARTY_STEAM_SAVES=0` (the launcher's "Start
//!   from the game's existing saves" switched off) leaves the new saves empty.
//!
//! How: the first time the game touches a path holding one of the ids as a whole path
//! component or a file name token delimited by anything but a letter or digit
//! (`<id>\slot1.sav`, `<id>_save.dat`), the same path with the source id is copied over
//! it, the whole folder or the file, wherever it lives. Saves that aren't keyed by id
//! are shared already and need nothing.
//!
//! kernelbase's file entry points are hooked (kernel32's forward there): CreateFileW/A,
//! CreateFile2, GetFileAttributesW/A/ExW/ExA, FindFirstFileExW/A and CreateDirectoryW/A
//! cover how .NET, Unity and native games open or probe a save. They're only armed when
//! needed: at kernelbase's load for `--copy-saves`, and for a SteamID of its own once
//! the game has asked for its SteamID (so a game without Steam, or one that keeps its
//! SteamID, has no file hooks). The hooks only look at the path; the game's call goes
//! on unchanged.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;
use crate::read;

/// `--copy-saves`.
pub fn enabled() -> bool {
    std::env::var_os("PEERPARTY_COPY_SAVES").is_some()
}

/// Whether a game given a SteamID of its own starts from the saves under steam_api's
/// (`PEERPARTY_STEAM_SAVES`, on unless 0).
pub fn adopt_on() -> bool {
    !std::env::var("PEERPARTY_STEAM_SAVES").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "no" | "false"))
}

/// A larger save than this (a folder of replays, say) isn't copied: copying runs inside
/// the game's own file call.
const MAX_COPY: u64 = 1 << 30;

/// One id as it appears in paths, and where its save starts from.
#[derive(Clone, Debug, PartialEq)]
struct Pair {
    /// The id the game builds paths from now.
    ours: String,
    /// The id whose save it starts from.
    from: String,
    /// Copy only while nothing exists under `ours` (a SteamID of its own); otherwise
    /// newer files replace older ones (`--copy-saves`).
    once: bool,
}

static PAIRS: Mutex<Vec<Pair>> = Mutex::new(Vec::new());
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Roots already handled this run (the side the game uses now).
static DONE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

thread_local! {
    /// Set while PeerParty itself is copying, so its own file calls pass straight through.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

fn add(ours: &str, from: &str, once: bool) -> bool {
    if ours.is_empty() || from.is_empty() || ours.eq_ignore_ascii_case(from) {
        return false;
    }
    let mut p = PAIRS.lock().unwrap();
    if p.iter().any(|q| q.ours.eq_ignore_ascii_case(ours) && q.from.eq_ignore_ascii_case(from)) {
        return false;
    }
    p.push(Pair { ours: ours.to_string(), from: from.to_string(), once });
    ACTIVE.store(true, Ordering::Relaxed);
    true
}

/// `--copy-saves`: paths containing `lan` get `main`'s copy first. Several mains for one
/// LAN id are tried in the order learned, the first that exists winning.
pub fn learn(lan: &str, main: &str) {
    if enabled() && add(lan, main, false) {
        log!("saves: paths with {lan} start from {main}'s copy");
    }
}

/// The game was shown SteamID `own` in place of steam_api's `game`: saves under its
/// SteamID or account id start, once, from the ones under steam_api's (module docs).
pub fn adopt_steam(own: u64, game: u64) {
    if own == game || game == 0 {
        return;
    }
    if !adopt_on() {
        log!("saves: the game's saves under SteamID {own} start empty (PEERPARTY_STEAM_SAVES=0)");
        return;
    }
    let mut n = 0;
    for (ours, from) in steam_names(own, game) {
        n += usize::from(add(&ours, &from, true));
    }
    if n > 0 {
        log!(
            "saves: saves under SteamID {own} (account {}) start from a copy of {game}'s (account {}) the first time; the originals are only read",
            own as u32,
            game as u32
        );
        arm_now();
    }
}

/// How a SteamID shows up in a path: decimal, its account id, hex padded or not.
fn steam_names(own: u64, game: u64) -> Vec<(String, String)> {
    let mut v = vec![
        (own.to_string(), game.to_string()),
        (format!("{own:016x}"), format!("{game:016x}")),
        (format!("{own:x}"), format!("{game:x}")),
    ];
    if own as u32 != game as u32 {
        v.push(((own as u32).to_string(), (game as u32).to_string()));
    }
    v.dedup();
    v
}

/// The LAN came up: a copy in a slot other than the first maps its ids (EOS and
/// PlayFab, from its peer id) to the first slot's.
pub fn learn_lan(lan: &peerparty_core::Lan) {
    if !enabled() || lan.slot() == 0 || lan.slot() == u32::MAX {
        return;
    }
    let Some(main) = first_slot() else { return };
    learn(&lan.me().hex(), &main.hex());
    learn(&crate::playfab::entity_of(lan.me()), &crate::playfab::entity_of(main));
}

/// The first copy's LAN identity on this PC.
pub fn first_slot() -> Option<peerparty_core::PeerId> {
    let first = peerparty_core::Identity::default_dir().join("slot-0.id");
    let id = std::fs::read_to_string(&first)
        .ok()
        .and_then(|s| peerparty_core::PeerId::from_hex(&s));
    if id.is_none() {
        log!("saves: no first-slot identity at {}", first.display());
    }
    id
}

/// Where `path`'s save comes from, in the order to try: (root, source, once), the root
/// being the path up to the end of the component holding the id.
fn sources_of(path: &str, pairs: &[Pair]) -> Vec<(String, String, bool)> {
    let lower = path.to_ascii_lowercase();
    let mut out = Vec::new();
    for pair in pairs {
        let needle = pair.ours.to_ascii_lowercase();
        let mut from = 0;
        while let Some(i) = lower[from..].find(&needle).map(|i| i + from) {
            let end = i + needle.len();
            let before = lower[..i].chars().next_back();
            let after = lower[end..].chars().next();
            let edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
            if edge(before) && edge(after) {
                let stop = path[end..]
                    .find(['\\', '/'])
                    .map_or(path.len(), |j| end + j);
                let root = path[..stop].to_string();
                let source = format!("{}{}{}", &path[..i], pair.from, &path[end..stop]);
                out.push((root, source, pair.once));
                break;
            }
            from = end;
        }
    }
    out
}

fn copy_newer(from: &Path, to: &Path) -> std::io::Result<usize> {
    let meta = std::fs::metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        let mut n = 0;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            n += copy_newer(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(n);
    }
    let newer = match std::fs::metadata(to) {
        Ok(dest) => match (meta.modified(), dest.modified()) {
            (Ok(a), Ok(b)) => a > b,
            _ => false,
        },
        Err(_) => true,
    };
    if !newer {
        return Ok(0);
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, to)?;
    Ok(1)
}

/// Bytes under `p`, stopping once past `cap`.
fn size_of_tree(p: &Path, cap: u64) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    for entry in std::fs::read_dir(p).into_iter().flatten().flatten() {
        total += size_of_tree(&entry.path(), cap.saturating_sub(total));
        if total > cap {
            break;
        }
    }
    total
}

/// Copies `from` to `to`, which doesn't exist: into a temporary name beside it first, so
/// a copy cut short (the game closed mid-way) never passes for a finished one.
fn copy_fresh(from: &Path, to: &Path) -> std::io::Result<usize> {
    let mut tmp = to.as_os_str().to_owned();
    tmp.push(".peerparty-copy");
    let tmp = PathBuf::from(tmp);
    if tmp.is_dir() {
        std::fs::remove_dir_all(&tmp)?;
    } else if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    let n = copy_newer(from, &tmp)?;
    if let Err(e) = std::fs::rename(&tmp, to) {
        let _ = if tmp.is_dir() { std::fs::remove_dir_all(&tmp) } else { std::fs::remove_file(&tmp) };
        return Err(e);
    }
    Ok(n)
}

/// A path the game opens or probes: copy its save in first, if it has one to start from.
fn seen(path: &str) {
    let pairs = PAIRS.lock().unwrap().clone();
    let found = sources_of(path, &pairs);
    let Some((root, _, once)) = found.first().cloned() else {
        // Say which save-like paths the game uses that carry none of the ids, so a game
        // that names its saves some other way shows up in the log.
        static SHOWN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let lower = path.to_ascii_lowercase();
        if (lower.contains("savegame") || lower.contains("my games")) && SHOWN.fetch_add(1, Ordering::Relaxed) < 40 {
            log!("saves: the game's save path {path} holds none of the ids {}", pairs.iter().map(|p| p.ours.as_str()).collect::<Vec<_>>().join(", "));
        }
        return;
    };
    if !DONE.lock().unwrap().insert(root.to_ascii_lowercase()) {
        return;
    }
    let root_p = PathBuf::from(&root);
    if once && root_p.exists() {
        log!("saves: {root} is the game's own already; nothing copied");
        return;
    }
    let Some((_, source, _)) = found.iter().find(|(r, s, _)| r.eq_ignore_ascii_case(&root) && Path::new(s).exists()) else {
        log!("saves: {root} has nothing to start from ({})", found.iter().map(|f| f.1.as_str()).collect::<Vec<_>>().join(", "));
        return;
    };
    let source_p = PathBuf::from(source);
    let result = if once {
        let size = size_of_tree(&source_p, MAX_COPY);
        if size > MAX_COPY {
            log!("saves: {source} is over {} MB; not copied to {root}, the game starts it afresh", MAX_COPY >> 20);
            return;
        }
        copy_fresh(&source_p, &root_p)
    } else {
        copy_newer(&source_p, &root_p)
    };
    match result {
        Ok(n) => log!("saves: {source} -> {root} ({n} files copied{})", if once { ", once" } else { "" }),
        Err(e) => log!("saves: copying {source} -> {root} failed: {e}"),
    }
}

/// Called with every path the game opens or probes.
fn touch(name: *const u16) {
    if !ACTIVE.load(Ordering::Relaxed) || name.is_null() || BUSY.with(|b| b.get()) {
        return;
    }
    BUSY.with(|b| b.set(true));
    let _ = std::panic::catch_unwind(|| {
        if let Some(path) = read::wstr(name as usize) {
            seen(&path);
        }
    });
    BUSY.with(|b| b.set(false));
}

/// As `touch`, for an ANSI path (the system code page).
fn touch_a(name: *const u8) {
    if !ACTIVE.load(Ordering::Relaxed) || name.is_null() || BUSY.with(|b| b.get()) {
        return;
    }
    BUSY.with(|b| b.set(true));
    let _ = std::panic::catch_unwind(|| {
        if let Some(path) = ansi(name) {
            seen(&path);
        }
    });
    BUSY.with(|b| b.set(false));
}

fn ansi(p: *const u8) -> Option<String> {
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_ACP};
    let mut len = 0usize;
    while len < 32768 && unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    if len == 0 || len == 32768 {
        return None;
    }
    let n = unsafe { MultiByteToWideChar(CP_ACP, 0, p, len as i32, std::ptr::null_mut(), 0) };
    if n <= 0 {
        return None;
    }
    let mut w = vec![0u16; n as usize];
    let n = unsafe { MultiByteToWideChar(CP_ACP, 0, p, len as i32, w.as_mut_ptr(), n) };
    (n > 0).then(|| String::from_utf16_lossy(&w[..n as usize]))
}

// ---- kernelbase hooks ----

type CreateFileW = unsafe extern "system" fn(*const u16, u32, u32, usize, u32, u32, isize) -> isize;
type CreateFileA = unsafe extern "system" fn(*const u8, u32, u32, usize, u32, u32, isize) -> isize;
type CreateFile2 = unsafe extern "system" fn(*const u16, u32, u32, u32, usize) -> isize;
type GetFileAttributesW = unsafe extern "system" fn(*const u16) -> u32;
type GetFileAttributesA = unsafe extern "system" fn(*const u8) -> u32;
type GetFileAttributesExW = unsafe extern "system" fn(*const u16, i32, usize) -> i32;
type GetFileAttributesExA = unsafe extern "system" fn(*const u8, i32, usize) -> i32;
type FindFirstFileExW = unsafe extern "system" fn(*const u16, i32, usize, i32, usize, u32) -> isize;
type FindFirstFileExA = unsafe extern "system" fn(*const u8, i32, usize, i32, usize, u32) -> isize;
type CreateDirectoryW = unsafe extern "system" fn(*const u16, usize) -> i32;
type CreateDirectoryA = unsafe extern "system" fn(*const u8, usize) -> i32;

static CREATE_FILE: OnceLock<GenericDetour<CreateFileW>> = OnceLock::new();
static CREATE_FILE_A: OnceLock<GenericDetour<CreateFileA>> = OnceLock::new();
static CREATE_FILE2: OnceLock<GenericDetour<CreateFile2>> = OnceLock::new();
static ATTRS: OnceLock<GenericDetour<GetFileAttributesW>> = OnceLock::new();
static ATTRS_A: OnceLock<GenericDetour<GetFileAttributesA>> = OnceLock::new();
static ATTRS_EX: OnceLock<GenericDetour<GetFileAttributesExW>> = OnceLock::new();
static ATTRS_EX_A: OnceLock<GenericDetour<GetFileAttributesExA>> = OnceLock::new();
static FIND: OnceLock<GenericDetour<FindFirstFileExW>> = OnceLock::new();
static FIND_A: OnceLock<GenericDetour<FindFirstFileExA>> = OnceLock::new();
static MKDIR: OnceLock<GenericDetour<CreateDirectoryW>> = OnceLock::new();
static MKDIR_A: OnceLock<GenericDetour<CreateDirectoryA>> = OnceLock::new();

static ARMED: AtomicBool = AtomicBool::new(false);

fn arm<T>(exports: &[Export], name: &str, slot: &OnceLock<GenericDetour<T>>, detour: T) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    if slot.get().is_some() {
        return 0;
    }
    let Some(e) = exports.iter().find(|e| e.name == name) else {
        return 0;
    };
    let target: T = unsafe { std::mem::transmute_copy(&e.addr) };
    with_lock(|| {
        let Ok(hook) = (unsafe { GenericDetour::new(target, detour) }) else {
            log!("saves {name} not hooked");
            return 0;
        };
        if slot.set(hook).is_err() {
            return 0;
        }
        usize::from(unsafe { slot.get().unwrap().enable() }.is_ok())
    })
}

/// Arms the file hooks in kernelbase (once).
pub fn attach_kernelbase(exports: &[Export]) -> usize {
    if ARMED.swap(true, Ordering::SeqCst) {
        return 0;
    }
    arm(exports, "CreateFileW", &CREATE_FILE, detour_create_file as CreateFileW)
        + arm(exports, "CreateFileA", &CREATE_FILE_A, detour_create_file_a as CreateFileA)
        + arm(exports, "CreateFile2", &CREATE_FILE2, detour_create_file2 as CreateFile2)
        + arm(exports, "GetFileAttributesW", &ATTRS, detour_attrs as GetFileAttributesW)
        + arm(exports, "GetFileAttributesA", &ATTRS_A, detour_attrs_a as GetFileAttributesA)
        + arm(exports, "GetFileAttributesExW", &ATTRS_EX, detour_attrs_ex as GetFileAttributesExW)
        + arm(exports, "GetFileAttributesExA", &ATTRS_EX_A, detour_attrs_ex_a as GetFileAttributesExA)
        + arm(exports, "FindFirstFileExW", &FIND, detour_find as FindFirstFileExW)
        + arm(exports, "FindFirstFileExA", &FIND_A, detour_find_a as FindFirstFileExA)
        + arm(exports, "CreateDirectoryW", &MKDIR, detour_mkdir as CreateDirectoryW)
        + arm(exports, "CreateDirectoryA", &MKDIR_A, detour_mkdir_a as CreateDirectoryA)
}

/// Arms the file hooks now, from kernelbase as it's loaded.
fn arm_now() {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    if ARMED.load(Ordering::SeqCst) {
        return;
    }
    let name: Vec<u16> = "kernelbase.dll".encode_utf16().chain(Some(0)).collect();
    let base = unsafe { GetModuleHandleW(name.as_ptr()) };
    if base.is_null() {
        log!("saves: kernelbase not found; saves aren't copied");
        return;
    }
    match unsafe { crate::pe::exports(base.cast()) } {
        Ok(t) => {
            let n = attach_kernelbase(&t.code);
            log!("saves file hooks {n}");
        }
        Err(e) => log!("saves: kernelbase's exports unreadable ({e}); saves aren't copied"),
    }
}

unsafe extern "system" fn detour_create_file(name: *const u16, access: u32, share: u32, sa: usize, disposition: u32, flags: u32, template: isize) -> isize {
    touch(name);
    unsafe { CREATE_FILE.get().unwrap().call(name, access, share, sa, disposition, flags, template) }
}

unsafe extern "system" fn detour_create_file_a(name: *const u8, access: u32, share: u32, sa: usize, disposition: u32, flags: u32, template: isize) -> isize {
    touch_a(name);
    unsafe { CREATE_FILE_A.get().unwrap().call(name, access, share, sa, disposition, flags, template) }
}

unsafe extern "system" fn detour_create_file2(name: *const u16, access: u32, share: u32, disposition: u32, params: usize) -> isize {
    touch(name);
    unsafe { CREATE_FILE2.get().unwrap().call(name, access, share, disposition, params) }
}

unsafe extern "system" fn detour_attrs(name: *const u16) -> u32 {
    touch(name);
    unsafe { ATTRS.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_attrs_a(name: *const u8) -> u32 {
    touch_a(name);
    unsafe { ATTRS_A.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_attrs_ex(name: *const u16, level: i32, info: usize) -> i32 {
    touch(name);
    unsafe { ATTRS_EX.get().unwrap().call(name, level, info) }
}

unsafe extern "system" fn detour_attrs_ex_a(name: *const u8, level: i32, info: usize) -> i32 {
    touch_a(name);
    unsafe { ATTRS_EX_A.get().unwrap().call(name, level, info) }
}

unsafe extern "system" fn detour_find(name: *const u16, level: i32, data: usize, op: i32, filter: usize, flags: u32) -> isize {
    touch(name);
    unsafe { FIND.get().unwrap().call(name, level, data, op, filter, flags) }
}

unsafe extern "system" fn detour_find_a(name: *const u8, level: i32, data: usize, op: i32, filter: usize, flags: u32) -> isize {
    touch_a(name);
    unsafe { FIND_A.get().unwrap().call(name, level, data, op, filter, flags) }
}

unsafe extern "system" fn detour_mkdir(name: *const u16, sa: usize) -> i32 {
    touch(name);
    unsafe { MKDIR.get().unwrap().call(name, sa) }
}

unsafe extern "system" fn detour_mkdir_a(name: *const u8, sa: usize) -> i32 {
    touch_a(name);
    unsafe { MKDIR_A.get().unwrap().call(name, sa) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(ours: &str, from: &str, once: bool) -> Pair {
        Pair { ours: ours.into(), from: from.into(), once }
    }

    #[test]
    fn finds_the_id_component_and_its_main_twin() {
        let pairs = vec![
            pair("76561202255233024", "76561198000000001", false),
            pair("abcdef0123456789abcdef0123456789", "00112233445566778899aabbccddeeff", false),
        ];
        let got = sources_of(r"C:\Users\x\AppData\LocalLow\Co\Game\76561202255233024\slot1\save.dat", &pairs);
        assert_eq!(
            got,
            vec![(
                r"C:\Users\x\AppData\LocalLow\Co\Game\76561202255233024".into(),
                r"C:\Users\x\AppData\LocalLow\Co\Game\76561198000000001".into(),
                false
            )]
        );
        let got = sources_of(r"D:\Saves\profile_76561202255233024.sav", &pairs);
        assert_eq!(got[0].1, r"D:\Saves\profile_76561198000000001.sav");
        // Hex ids match whatever their case; ids inside longer numbers don't.
        assert!(!sources_of(r"C:\s\ABCDEF0123456789ABCDEF0123456789\a", &pairs).is_empty());
        assert!(sources_of(r"C:\s\176561202255233024\a", &pairs).is_empty());
        assert!(sources_of(r"C:\s\nothing\here", &pairs).is_empty());
    }

    #[test]
    fn several_sources_for_one_id_in_order() {
        let pairs = vec![pair("222", "111", false), pair("222", "000", false)];
        let got = sources_of(r"C:\s\222\a.sav", &pairs);
        assert_eq!(got.iter().map(|g| g.1.as_str()).collect::<Vec<_>>(), [r"C:\s\111", r"C:\s\000"]);
    }

    #[test]
    fn a_steamid_shows_up_four_ways() {
        let own = 0x0110_0001_8000_0042u64;
        let game = 0x0110_0001_0000_1234u64;
        let names = steam_names(own, game);
        assert!(names.contains(&(own.to_string(), game.to_string())));
        assert!(names.contains(&("2147483714".into(), "4660".into())));
        assert!(names.contains(&("0110000180000042".into(), "0110000100001234".into())));
        assert!(names.contains(&("110000180000042".into(), "110000100001234".into())));
    }

    #[test]
    fn copies_folders_keeping_newer_files() {
        let base = std::env::temp_dir().join(format!("peerparty-saves-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (main, lan) = (base.join("111"), base.join("222"));
        std::fs::create_dir_all(main.join("sub")).unwrap();
        std::fs::write(main.join("a.sav"), "main").unwrap();
        std::fs::write(main.join("sub").join("b.sav"), "main").unwrap();
        assert_eq!(copy_newer(&main, &lan).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(lan.join("sub").join("b.sav")).unwrap(), "main");
        // A file the LAN copy wrote since is newer and stays.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(lan.join("a.sav"), "lan").unwrap();
        assert_eq!(copy_newer(&main, &lan).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(lan.join("a.sav")).unwrap(), "lan");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_fresh_copy_leaves_nothing_half_done_and_the_original_alone() {
        let base = std::env::temp_dir().join(format!("peerparty-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (old, new) = (base.join("4660"), base.join("2147483714"));
        std::fs::create_dir_all(old.join("sub")).unwrap();
        std::fs::write(old.join("a.sav"), "old").unwrap();
        std::fs::write(old.join("sub").join("b.sav"), "old").unwrap();
        // Left over from a copy cut short.
        std::fs::create_dir_all(base.join("2147483714.peerparty-copy")).unwrap();
        assert_eq!(copy_fresh(&old, &new).unwrap(), 2);
        assert!(!base.join("2147483714.peerparty-copy").exists());
        assert_eq!(std::fs::read_to_string(new.join("sub").join("b.sav")).unwrap(), "old");
        std::fs::write(new.join("a.sav"), "new").unwrap();
        assert_eq!(std::fs::read_to_string(old.join("a.sav")).unwrap(), "old");
        // A file save, too.
        std::fs::write(base.join("4660_save.dat"), "old").unwrap();
        assert_eq!(copy_fresh(&base.join("4660_save.dat"), &base.join("2147483714_save.dat")).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(base.join("2147483714_save.dat")).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_tree_is_sized_up_to_the_cap() {
        let base = std::env::temp_dir().join(format!("peerparty-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("d")).unwrap();
        std::fs::write(base.join("a"), [0u8; 100]).unwrap();
        std::fs::write(base.join("d").join("b"), [0u8; 50]).unwrap();
        assert_eq!(size_of_tree(&base, 1000), 150);
        assert!(size_of_tree(&base, 10) > 10);
        let _ = std::fs::remove_dir_all(&base);
    }
}
