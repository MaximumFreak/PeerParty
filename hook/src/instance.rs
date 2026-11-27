//! Lets a second copy of a game start on the same PC, for testing two players on one
//! machine. With PEERPARTY_MULTI_INSTANCE set (the launcher's `--multi-instance`):
//!
//! - a named mutex, event, semaphore or file mapping that looks like a single-instance
//!   check gets this process's id appended, so each copy owns its own: Unity's "Force
//!   Single Instance" one (`SingleInstanceMutex`), any whose name says "instance" or
//!   "singleton" or holds the game's program name (`MyGame_Running`), and any named
//!   mutex, event or semaphore created or opened directly by the game's own executable or
//!   a DLL in its folder (whatever it's named — a codename, a GUID, an engine lock).
//!   SDK, driver, browser-runtime and Windows objects (`NEVER` prefixes, names that
//!   already embed this process's id, and calls from system/driver/SDK modules) are left
//!   alone: they are how a game talks to its store's client, drivers, child processes and
//!   Windows' own services.
//! - FindWindow(Ex) doesn't find the other copy's windows (a game that looks for its own
//!   window class at start-up and quits, or hands over to it, when one is there).
//!
//! A check that still slips past (for example a file mapping with an unrelated name) is
//! visible in the log: the start says whether another copy of the program is running, and
//! any named object the game finds already there that isn't renamed is logged once by
//! name.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, SetLastError};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

use crate::log;
use crate::sys::pe::Export;
use crate::sys::read;

type CreateMutexW = unsafe extern "system" fn(*const core::ffi::c_void, i32, *const u16) -> isize;
type CreateMutexExW =
    unsafe extern "system" fn(*const core::ffi::c_void, *const u16, u32, u32) -> isize;
type OpenMutexW = unsafe extern "system" fn(u32, i32, *const u16) -> isize;
/// CreateEventW: (attributes, manual reset, initial state, name).
type CreateEventW = unsafe extern "system" fn(*const core::ffi::c_void, i32, i32, *const u16) -> isize;
/// CreateSemaphoreW: (attributes, initial count, maximum, name).
type CreateSemaphoreW = unsafe extern "system" fn(*const core::ffi::c_void, i32, i32, *const u16) -> isize;
/// CreateSemaphoreExW: (attributes, initial, maximum, name, flags, access).
type CreateSemaphoreExW = unsafe extern "system" fn(*const core::ffi::c_void, i32, i32, *const u16, u32, u32) -> isize;
/// CreateFileMappingW: (file, attributes, protection, size high, size low, name).
type CreateFileMappingW = unsafe extern "system" fn(isize, *const core::ffi::c_void, u32, u32, u32, *const u16) -> isize;

static CREATE: OnceLock<GenericDetour<CreateMutexW>> = OnceLock::new();
static CREATE_EX: OnceLock<GenericDetour<CreateMutexExW>> = OnceLock::new();
static OPEN: OnceLock<GenericDetour<OpenMutexW>> = OnceLock::new();
static EVENT: OnceLock<GenericDetour<CreateEventW>> = OnceLock::new();
static EVENT_EX: OnceLock<GenericDetour<CreateMutexExW>> = OnceLock::new();
static OPEN_EVENT: OnceLock<GenericDetour<OpenMutexW>> = OnceLock::new();
static SEMAPHORE: OnceLock<GenericDetour<CreateSemaphoreW>> = OnceLock::new();
static SEMAPHORE_EX: OnceLock<GenericDetour<CreateSemaphoreExW>> = OnceLock::new();
static OPEN_SEMAPHORE: OnceLock<GenericDetour<OpenMutexW>> = OnceLock::new();
static MAPPING: OnceLock<GenericDetour<CreateFileMappingW>> = OnceLock::new();
static OPEN_MAPPING: OnceLock<GenericDetour<OpenMutexW>> = OnceLock::new();

const MARKER: &str = "SingleInstanceMutex";

/// Names of SDKs', Windows' and PeerParty's own objects, never renamed (lower case, as
/// prefixes of the name after any `Global\` / `Local\`): a game's program name inside
/// one (`game.exe` and `GameInput...`) doesn't make it a single-instance check.
const NEVER: [&str; 14] = [
    "steam",
    "valve",
    "galaxy",
    "gog",
    "eos",
    "epic",
    "peerparty",
    "playfab",
    "msctf",
    "gameinput",
    "gamebar",
    "xbox",
    "xgame",
    "dbwinmutex",
];

/// Whether a named object `name` looks like a single-instance check of the game `stem`
/// (its program's name, lower case).
fn single_instance(name: &str, stem: &str) -> bool {
    if name.contains(MARKER) {
        return true;
    }
    if shared_on_purpose(name) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    let bare = lower.rsplit('\\').next().unwrap_or(&lower);
    bare.contains("instance") || bare.contains("singleton") || (stem.len() >= 4 && bare.contains(stem))
}

/// Whether `name` is an SDK's, Windows' or PeerParty's object (`NEVER`): shared with
/// other programs on purpose.
fn shared_on_purpose(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let bare = lower.rsplit('\\').next().unwrap_or(&lower);
    NEVER.iter().any(|n| bare.starts_with(n))
}

/// Whether `name` already embeds `pid` as a standalone decimal token (e.g.
/// `Local\SM0:7108:168:...` or `Crashpad_7108_Lock`): already per-process, so it cannot
/// collide with another copy, and renaming it could break IPC with an unhooked child.
fn contains_pid(name: &str, pid: u32) -> bool {
    if pid < 100 {
        return false;
    }
    let needle = pid.to_string();
    let bytes = name.as_bytes();
    let mut start = 0;
    while let Some(off) = name[start..].find(&needle) {
        let pos = start + off;
        let end = pos + needle.len();
        let before_ok = pos == 0 || !bytes[pos - 1].is_ascii_alphanumeric();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}

/// What a module on the call stack is when deciding whether a named sync object came from
/// the game itself: PeerParty, the kernel/CRT wrappers (`Passed`) are walked past to find
/// the real caller; a module inside the game's directory (`Game`, excluding store SDKs
/// and embedded browser runtimes) gets its named sync objects isolated per copy; any
/// other module (Windows subsystems like d3d9/combase/winmm, graphics drivers, store
/// clients) stops the walk and leaves the object shared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Caller {
    Passed,
    Game,
    Other,
}

static CALLERS: Mutex<Vec<(usize, Caller, String)>> = Mutex::new(Vec::new());

/// Directories whose binaries belong to this game (lower case, ending with `\`).
fn game_dirs() -> &'static Vec<String> {
    static DIRS: OnceLock<Vec<String>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut out = Vec::new();
        let mut push = |p: &str| {
            let mut s = p.replace('/', "\\").to_ascii_lowercase();
            while s.ends_with('\\') {
                s.pop();
            }
            if !s.is_empty() {
                s.push('\\');
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        };
        if let Some(root) = crate::config::HookConfig::global().game_root.as_deref() {
            push(root);
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned())) {
            push(&dir);
        }
        out
    })
}

/// Classifies a module by its full path (lower case) against the game's directories.
fn classify_path(path: &str, dirs: &[String]) -> Caller {
    let file = path.rsplit('\\').next().unwrap_or(path);
    if matches!(file, "kernelbase.dll" | "kernel32.dll" | "ntdll.dll" | "ucrtbase.dll" | "ucrtbased.dll")
        || file.starts_with("vcruntime")
        || file.starts_with("msvcr")
        || file.starts_with("msvcp")
        || file.starts_with("api-ms-win-")
    {
        return Caller::Passed;
    }
    if dirs.iter().any(|d| path.starts_with(d.as_str())) {
        // Store SDKs and embedded browser runtimes in the game folder share named objects
        // with external clients or helper subprocesses on purpose.
        if file.starts_with("steam_api")
            || file.starts_with("steamclient")
            || file.starts_with("eossdk")
            || file.starts_with("galaxy")
            || file.starts_with("playfab")
            || file.starts_with("vivoxsdk")
            || matches!(file, "libcef.dll" | "chromehtml.dll")
        {
            return Caller::Other;
        }
        return Caller::Game;
    }
    Caller::Other
}

fn caller_kind(m: usize) -> (Caller, String) {
    if let Some((_, k, name)) = crate::sys::lock(&CALLERS).iter().find(|(b, _, _)| *b == m) {
        return (*k, name.clone());
    }
    let mut buf = [0u16; 520];
    let len = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW(m as _, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let path = String::from_utf16_lossy(&buf[..len]).to_ascii_lowercase();
    let name = path.rsplit('\\').next().unwrap_or(&path).to_string();
    let kind = classify_path(&path, game_dirs());
    let mut c = crate::sys::lock(&CALLERS);
    if c.len() < 256 {
        c.push((m, kind, name.clone()));
    }
    (kind, name)
}

/// If the call came directly from one of the game's own modules (walking past PeerParty
/// and kernel/CRT wrappers only), returns that module's filename.
#[inline(never)]
fn game_caller() -> Option<String> {
    use windows_sys::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW};
    let mut frames = [std::ptr::null_mut(); 12];
    let n = unsafe { windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace(1, frames.len() as u32, frames.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    let owner = |addr: usize| {
        let mut m = std::ptr::null_mut();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        (unsafe { GetModuleHandleExW(flags, addr as *const u16, &mut m) } != 0).then_some(m as usize)
    };
    let ours = owner(game_caller as *const () as usize);
    for &f in &frames[..n.min(frames.len())] {
        let Some(m) = owner(f as usize) else { return None };
        if Some(m) == ours {
            continue;
        }
        let (kind, name) = caller_kind(m);
        match kind {
            Caller::Passed => continue,
            Caller::Game => return Some(name),
            Caller::Other => return None,
        }
    }
    None
}

/// This program's name without its extension, lower case.
fn stem() -> &'static str {
    static STEM: OnceLock<String> = OnceLock::new();
    STEM.get_or_init(|| {
        std::env::current_exe().ok().and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase())).unwrap_or_default()
    })
}

pub fn enabled() -> bool {
    crate::config::HookConfig::global().multi_instance
}

/// Hooks kernelbase's mutex entry points. kernel32's exports forward here.
pub fn attach_kernelbase(exports: &[Export]) -> usize {
    log_other_copies();
    let mut n = 0;
    n += hook_one(exports, "CreateMutexW", |addr| {
        let target: CreateMutexW = unsafe { std::mem::transmute(addr) };
        arm(&CREATE, target, detour_create, "CreateMutexW")
    });
    n += hook_one(exports, "CreateMutexExW", |addr| {
        let target: CreateMutexExW = unsafe { std::mem::transmute(addr) };
        arm(&CREATE_EX, target, detour_create_ex, "CreateMutexExW")
    });
    n += hook_one(exports, "OpenMutexW", |addr| {
        let target: OpenMutexW = unsafe { std::mem::transmute(addr) };
        arm(&OPEN, target, detour_open, "OpenMutexW")
    });
    n += hook_one(exports, "CreateEventW", |a| arm(&EVENT, unsafe { std::mem::transmute::<*const (), CreateEventW>(a) }, detour_event, "CreateEventW"));
    n += hook_one(exports, "CreateEventExW", |a| arm(&EVENT_EX, unsafe { std::mem::transmute::<*const (), CreateMutexExW>(a) }, detour_event_ex, "CreateEventExW"));
    n += hook_one(exports, "OpenEventW", |a| arm(&OPEN_EVENT, unsafe { std::mem::transmute::<*const (), OpenMutexW>(a) }, detour_open_event, "OpenEventW"));
    n += hook_one(exports, "CreateSemaphoreW", |a| arm(&SEMAPHORE, unsafe { std::mem::transmute::<*const (), CreateSemaphoreW>(a) }, detour_semaphore, "CreateSemaphoreW"));
    n += hook_one(exports, "CreateSemaphoreExW", |a| arm(&SEMAPHORE_EX, unsafe { std::mem::transmute::<*const (), CreateSemaphoreExW>(a) }, detour_semaphore_ex, "CreateSemaphoreExW"));
    n += hook_one(exports, "OpenSemaphoreW", |a| arm(&OPEN_SEMAPHORE, unsafe { std::mem::transmute::<*const (), OpenMutexW>(a) }, detour_open_semaphore, "OpenSemaphoreW"));
    n += hook_one(exports, "CreateFileMappingW", |a| arm(&MAPPING, unsafe { std::mem::transmute::<*const (), CreateFileMappingW>(a) }, detour_mapping, "CreateFileMappingW"));
    n += hook_one(exports, "OpenFileMappingW", |a| arm(&OPEN_MAPPING, unsafe { std::mem::transmute::<*const (), OpenMutexW>(a) }, detour_open_mapping, "OpenFileMappingW"));
    n
}

fn hook_one(exports: &[Export], name: &str, install: impl FnOnce(*const ()) -> bool) -> usize {
    crate::sys::detour::hook_one(exports, "instance", name, install)
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    crate::sys::detour::arm(slot, target, detour, "instance", label)
}

/// The per-process name for an isolated named object, or None to leave `name` alone.
/// `by_caller` enables caller-origin isolation (for mutexes, events and semaphores):
/// any named sync object created or opened directly by a game module is isolated per copy,
/// even when its name doesn't match the heuristic (e.g. a GUID or engine codename).
/// File mappings stay name-only so external tools (such as telemetry readers) still see them.
fn rename(name: *const u16, api: &str, by_caller: bool) -> Option<Vec<u16>> {
    if name.is_null() {
        return None;
    }
    let text = read::wstr(name as usize)?;
    // CreateMutexW may reach CreateMutexExW internally; rename only once.
    if text.is_empty() || text.contains("-peerparty-") {
        return None;
    }
    let pid = unsafe { GetCurrentProcessId() };
    let reason = if single_instance(&text, stem()) {
        "name".to_string()
    } else if by_caller && !shared_on_purpose(&text) && !contains_pid(&text, pid) {
        let caller = game_caller()?;
        format!("caller {caller}")
    } else {
        return None;
    };
    let renamed = format!("{text}-peerparty-{pid}");
    log_renamed(api, &text, &renamed, &reason);
    Some(renamed.encode_utf16().chain(Some(0)).collect())
}

fn log_renamed(api: &str, text: &str, renamed: &str, reason: &str) {
    static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    {
        let mut seen = crate::sys::lock(&SEEN);
        if seen.len() >= 64 || !seen.insert(text.to_string()) {
            return;
        }
    }
    log!("instance {api} {text} -> {renamed} ({reason})");
}

unsafe extern "system" fn detour_create(
    attrs: *const core::ffi::c_void,
    owner: i32,
    name: *const u16,
) -> isize {
    let Some(hook) = CREATE.get() else { return 0 };
    let renamed = rename(name, "CreateMutexW", true);
    let at = renamed.as_ref().map_or(name, |w| w.as_ptr());
    let h = unsafe { hook.call(attrs, owner, at) };
    found("CreateMutexW", name, renamed.is_some(), h, false);
    h
}

unsafe extern "system" fn detour_create_ex(
    attrs: *const core::ffi::c_void,
    name: *const u16,
    flags: u32,
    access: u32,
) -> isize {
    let Some(hook) = CREATE_EX.get() else {
        return 0;
    };
    let renamed = rename(name, "CreateMutexExW", true);
    let at = renamed.as_ref().map_or(name, |w| w.as_ptr());
    let h = unsafe { hook.call(attrs, at, flags, access) };
    found("CreateMutexExW", name, renamed.is_some(), h, false);
    h
}

unsafe extern "system" fn detour_open(access: u32, inherit: i32, name: *const u16) -> isize {
    let Some(hook) = OPEN.get() else { return 0 };
    let renamed = rename(name, "OpenMutexW", true);
    let at = renamed.as_ref().map_or(name, |w| w.as_ptr());
    let h = unsafe { hook.call(access, inherit, at) };
    found("OpenMutexW", name, renamed.is_some(), h, true);
    h
}

/// `name`, renamed for this copy when it's a single-instance check's (`rename`).
fn named(name: *const u16, api: &str, by_caller: bool) -> (Option<Vec<u16>>, *const u16) {
    let renamed = rename(name, api, by_caller);
    let at = renamed.as_ref().map_or(name, |w| w.as_ptr());
    (renamed, at)
}

/// The most distinct names `found` logs.
const MAX_FOUND: usize = 32;

/// After a Create* or Open* call on `name` that returned `handle`: a named object this
/// copy found already there, not renamed, is logged once. With another copy running,
/// the one a game checks for itself by is among these. The game's last error is kept.
fn found(api: &str, name: *const u16, renamed: bool, handle: isize, open: bool) {
    let err = unsafe { GetLastError() };
    if !renamed && !name.is_null() && handle != 0 && (open || err == ERROR_ALREADY_EXISTS) {
        note_found(api, name, open);
    }
    unsafe { SetLastError(err) };
}

#[inline(never)]
fn note_found(api: &str, name: *const u16, open: bool) {
    static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    let Some(text) = read::wstr(name as usize) else { return };
    if text.contains("-peerparty-") || shared_on_purpose(&text) {
        return;
    }
    {
        let mut seen = crate::sys::lock(&SEEN);
        if seen.len() >= MAX_FOUND || !seen.insert(text.clone()) {
            return;
        }
    }
    crate::log::throttled("instance found", u64::MAX, |_| {
        log!(
            "instance {api} {text}: {}, and shared (not renamed: not named like a single-instance check)",
            if open { "opened one another program made" } else { "already there" }
        )
    });
}

/// Says whether other copies of this program are running, at the start: a copy that
/// stops early while one is has likely met its own single-instance check.
fn log_other_copies() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    let Some(mine) = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())) else { return };
    let me = unsafe { GetCurrentProcessId() };
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return;
    }
    let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut others = Vec::new();
    let mut ok = unsafe { Process32FirstW(snap, &mut e) } != 0;
    while ok && others.len() < 16 {
        let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
        if e.th32ProcessID != me && String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case(&mine) && same_program(e.th32ProcessID) {
            others.push(e.th32ProcessID);
        }
        ok = unsafe { Process32NextW(snap, &mut e) } != 0;
    }
    unsafe { CloseHandle(snap) };
    if others.is_empty() {
        log!("instance: no other copy of {mine} is running");
    } else {
        let pids: Vec<String> = others.iter().map(u32::to_string).collect();
        log!(
            "instance: {} other cop{} of {mine} running (pid {}); named objects this copy finds already there are logged as `instance ... already there`",
            others.len(),
            if others.len() == 1 { "y is" } else { "ies are" },
            pids.join(", ")
        );
    }
}

/// Whether process `pid` runs this program (the same file).
fn same_program(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) } != 0;
    unsafe { CloseHandle(h) };
    let theirs = ok.then(|| String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase());
    let mine = std::env::current_exe().ok().map(|p| p.to_string_lossy().to_ascii_lowercase());
    theirs.is_some() && theirs == mine
}

unsafe extern "system" fn detour_event(attrs: *const core::ffi::c_void, manual: i32, initial: i32, name: *const u16) -> isize {
    let Some(hook) = EVENT.get() else { return 0 };
    let (keep, at) = named(name, "CreateEventW", true);
    let h = unsafe { hook.call(attrs, manual, initial, at) };
    found("CreateEventW", name, keep.is_some(), h, false);
    h
}

unsafe extern "system" fn detour_event_ex(attrs: *const core::ffi::c_void, name: *const u16, flags: u32, access: u32) -> isize {
    let Some(hook) = EVENT_EX.get() else { return 0 };
    let (keep, at) = named(name, "CreateEventExW", true);
    let h = unsafe { hook.call(attrs, at, flags, access) };
    found("CreateEventExW", name, keep.is_some(), h, false);
    h
}

unsafe extern "system" fn detour_open_event(access: u32, inherit: i32, name: *const u16) -> isize {
    let Some(hook) = OPEN_EVENT.get() else { return 0 };
    let (keep, at) = named(name, "OpenEventW", true);
    let h = unsafe { hook.call(access, inherit, at) };
    found("OpenEventW", name, keep.is_some(), h, true);
    h
}

unsafe extern "system" fn detour_semaphore(attrs: *const core::ffi::c_void, initial: i32, max: i32, name: *const u16) -> isize {
    let Some(hook) = SEMAPHORE.get() else { return 0 };
    let (keep, at) = named(name, "CreateSemaphoreW", true);
    let h = unsafe { hook.call(attrs, initial, max, at) };
    found("CreateSemaphoreW", name, keep.is_some(), h, false);
    h
}

unsafe extern "system" fn detour_semaphore_ex(attrs: *const core::ffi::c_void, initial: i32, max: i32, name: *const u16, flags: u32, access: u32) -> isize {
    let Some(hook) = SEMAPHORE_EX.get() else { return 0 };
    let (keep, at) = named(name, "CreateSemaphoreExW", true);
    let h = unsafe { hook.call(attrs, initial, max, at, flags, access) };
    found("CreateSemaphoreExW", name, keep.is_some(), h, false);
    h
}

unsafe extern "system" fn detour_open_semaphore(access: u32, inherit: i32, name: *const u16) -> isize {
    let Some(hook) = OPEN_SEMAPHORE.get() else { return 0 };
    let (keep, at) = named(name, "OpenSemaphoreW", true);
    let h = unsafe { hook.call(access, inherit, at) };
    found("OpenSemaphoreW", name, keep.is_some(), h, true);
    h
}

unsafe extern "system" fn detour_mapping(file: isize, attrs: *const core::ffi::c_void, protect: u32, high: u32, low: u32, name: *const u16) -> isize {
    let Some(hook) = MAPPING.get() else { return 0 };
    let (keep, at) = named(name, "CreateFileMappingW", false);
    let h = unsafe { hook.call(file, attrs, protect, high, low, at) };
    found("CreateFileMappingW", name, keep.is_some(), h, false);
    h
}

unsafe extern "system" fn detour_open_mapping(access: u32, inherit: i32, name: *const u16) -> isize {
    let Some(hook) = OPEN_MAPPING.get() else { return 0 };
    let (keep, at) = named(name, "OpenFileMappingW", false);
    let h = unsafe { hook.call(access, inherit, at) };
    found("OpenFileMappingW", name, keep.is_some(), h, true);
    h
}

// ---- FindWindow ----

type FindWindowW = unsafe extern "system" fn(*const u16, *const u16) -> isize;
type FindWindowA = unsafe extern "system" fn(*const u8, *const u8) -> isize;
type FindWindowExW = unsafe extern "system" fn(isize, isize, *const u16, *const u16) -> isize;
type FindWindowExA = unsafe extern "system" fn(isize, isize, *const u8, *const u8) -> isize;

static FIND_W: OnceLock<GenericDetour<FindWindowW>> = OnceLock::new();
static FIND_A: OnceLock<GenericDetour<FindWindowA>> = OnceLock::new();
static FIND_EX_W: OnceLock<GenericDetour<FindWindowExW>> = OnceLock::new();
static FIND_EX_A: OnceLock<GenericDetour<FindWindowExA>> = OnceLock::new();

/// Hooks user32's FindWindow(Ex)W/A.
pub fn attach_user32(exports: &[Export]) -> usize {
    let mut n = 0;
    n += hook_one(exports, "FindWindowW", |addr| arm(&FIND_W, unsafe { std::mem::transmute::<*const (), FindWindowW>(addr) }, detour_find_w, "FindWindowW"));
    n += hook_one(exports, "FindWindowA", |addr| arm(&FIND_A, unsafe { std::mem::transmute::<*const (), FindWindowA>(addr) }, detour_find_a, "FindWindowA"));
    n += hook_one(exports, "FindWindowExW", |addr| arm(&FIND_EX_W, unsafe { std::mem::transmute::<*const (), FindWindowExW>(addr) }, detour_find_ex_w, "FindWindowExW"));
    n += hook_one(exports, "FindWindowExA", |addr| arm(&FIND_EX_A, unsafe { std::mem::transmute::<*const (), FindWindowExA>(addr) }, detour_find_ex_a, "FindWindowExA"));
    n
}

/// Whether a window belongs to another running copy of this program.
fn other_copy(hwnd: isize) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    if hwnd == 0 {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd as _, &mut pid) };
    if pid == 0 || pid == unsafe { GetCurrentProcessId() } {
        return false;
    }
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) } != 0;
    unsafe { CloseHandle(h) };
    let theirs = ok.then(|| String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase());
    let mine = std::env::current_exe().ok().map(|p| p.to_string_lossy().to_ascii_lowercase());
    let same = theirs.is_some() && theirs == mine;
    if same {
        log!("instance FindWindow: a window of the other copy (pid {pid}) is passed over");
    }
    same
}

/// The first match past the other copy's windows: FindWindow(Ex) asked again from after
/// each one passed over (a top-level search is FindWindowEx with no parent).
fn skip_others(first: isize, mut next: impl FnMut(isize) -> isize) -> isize {
    let mut w = first;
    for _ in 0..64 {
        if !other_copy(w) {
            return w;
        }
        w = next(w);
    }
    0
}

unsafe extern "system" fn detour_find_w(class: *const u16, title: *const u16) -> isize {
    let Some(hook) = FIND_W.get() else { return 0 };
    let first = unsafe { hook.call(class, title) };
    match FIND_EX_W.get() {
        Some(ex) => skip_others(first, |after| unsafe { ex.call(0, after, class, title) }),
        None if other_copy(first) => 0,
        None => first,
    }
}

unsafe extern "system" fn detour_find_a(class: *const u8, title: *const u8) -> isize {
    let Some(hook) = FIND_A.get() else { return 0 };
    let first = unsafe { hook.call(class, title) };
    match FIND_EX_A.get() {
        Some(ex) => skip_others(first, |after| unsafe { ex.call(0, after, class, title) }),
        None if other_copy(first) => 0,
        None => first,
    }
}

unsafe extern "system" fn detour_find_ex_w(parent: isize, after: isize, class: *const u16, title: *const u16) -> isize {
    let Some(hook) = FIND_EX_W.get() else { return 0 };
    let first = unsafe { hook.call(parent, after, class, title) };
    skip_others(first, |after| unsafe { hook.call(parent, after, class, title) })
}

unsafe extern "system" fn detour_find_ex_a(parent: isize, after: isize, class: *const u8, title: *const u8) -> isize {
    let Some(hook) = FIND_EX_A.get() else { return 0 };
    let first = unsafe { hook.call(parent, after, class, title) };
    skip_others(first, |after| unsafe { hook.call(parent, after, class, title) })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sys::win::wide;

    #[test]
    fn renames_only_single_instance_mutexes_once() {
        let pid = unsafe { GetCurrentProcessId() };
        let unity = wide("Some Studio-SingleInstanceMutex-SomeGameApp");
        let got = rename(unity.as_ptr(), "test", false).expect("renamed");
        let got = String::from_utf16(&got[..got.len() - 1]).unwrap();
        assert_eq!(
            got,
            format!("Some Studio-SingleInstanceMutex-SomeGameApp-peerparty-{pid}")
        );
        assert!(rename(wide(&got).as_ptr(), "test", false).is_none());
        assert!(rename(wide("Local\\SomethingElse").as_ptr(), "test", false).is_none());
        assert!(single_instance("Global\\SpaceGame_Instance", "launcher"));
        assert!(single_instance("engine_singleton_mutex", "spacegame"));
        assert!(single_instance("SpaceGameRunning", "spacegame"));
        assert!(!single_instance("Steam3Master_SharedMemLock", "steam3master"), "a store's own");
        assert!(!single_instance("Global\\EOSOverlayInstance", "spacegame"));
        assert!(!single_instance("DBWinMutex", "spacegame"));
        assert!(!single_instance("Local\\RenderLock", "spacegame"));
        assert!(rename(std::ptr::null(), "test", true).is_none());
        // A program named `game` doesn't make Windows' GameInput or Game Bar objects its own.
        assert!(!single_instance("Local\\GameInputRedistSvcMutex", "game"));
        assert!(!single_instance("GameBarPresenceWriter", "game"));
        assert!(single_instance("Local\\GameLauncherRunning", "game"));
        assert!(!single_instance("Local\\peerparty-frame-7-0102", "frame"), "PeerParty's own mappings");
    }

    #[test]
    fn pid_detection_skips_per_process_names_only() {
        assert!(contains_pid("Local\\SM0:7108:168:WilStaging_02", 7108));
        assert!(contains_pid("Crashpad_7108_Lock", 7108));
        assert!(contains_pid("7108", 7108));
        assert!(!contains_pid("Local\\SM0:11124:168:WilStaging_02", 7108));
        assert!(!contains_pid("Global\\{52817108-3561-4705-820a-2b3b78be92ba}", 7108));
        assert!(!contains_pid("engine_7108x_mutex", 7108));
        assert!(!contains_pid("app_42_lock", 42));
    }

    #[test]
    fn caller_classification_isolates_game_modules_only() {
        let dirs = vec![r"c:\games\space game\".to_string()];
        assert_eq!(classify_path(r"c:\games\space game\spacegame.exe", &dirs), Caller::Game);
        assert_eq!(classify_path(r"c:\games\space game\bin\launcher.dll", &dirs), Caller::Game);
        assert_eq!(classify_path(r"c:\windows\system32\kernelbase.dll", &dirs), Caller::Passed);
        assert_eq!(classify_path(r"c:\windows\syswow64\kernel32.dll", &dirs), Caller::Passed);
        assert_eq!(classify_path(r"c:\games\space game\vcruntime140.dll", &dirs), Caller::Passed);
        assert_eq!(classify_path(r"c:\windows\system32\d3d9.dll", &dirs), Caller::Other);
        assert_eq!(classify_path(r"c:\windows\system32\driverstore\filerepository\nv\nvd3dum.dll", &dirs), Caller::Other);
        assert_eq!(classify_path(r"c:\games\space game\bin\steam_api.dll", &dirs), Caller::Other);
        assert_eq!(classify_path(r"c:\games\space game\bin\libcef.dll", &dirs), Caller::Other);
        assert_eq!(classify_path(r"c:\program files (x86)\steam\steamclient.dll", &dirs), Caller::Other);
    }

    #[test]
    fn noting_a_found_object_keeps_the_games_last_error() {
        assert!(shared_on_purpose("Local\\Steam3Master_SharedMemLock"));
        assert!(!shared_on_purpose("Local\\{0F2A}Lock"));
        let name = wide("Local\\SomeGameStartupLock");
        for (err, open) in [(ERROR_ALREADY_EXISTS, false), (0, true), (5, false)] {
            unsafe { SetLastError(err) };
            found("CreateMutexW", name.as_ptr(), false, 1, open);
            assert_eq!(unsafe { GetLastError() }, err);
        }
        unsafe { SetLastError(ERROR_ALREADY_EXISTS) };
        found("CreateMutexW", std::ptr::null(), false, 1, false);
        assert_eq!(unsafe { GetLastError() }, ERROR_ALREADY_EXISTS);
    }
}
