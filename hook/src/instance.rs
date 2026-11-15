//! Lets a second copy of a game start on the same PC, for testing two players on one
//! machine. With PEERPARTY_MULTI_INSTANCE set (the launcher's `--multi-instance`):
//!
//! - a named mutex that looks like a single-instance check gets this process's id
//!   appended, so each copy owns its own: Unity's "Force Single Instance" one (its name
//!   holds `SingleInstanceMutex`), and any other whose name says "instance" or holds
//!   the game's program name (`MyGame_Running`). SDK and system ones (Steam's, Galaxy's,
//!   EOS's, PeerParty's, `Global\` and session names of Windows' own) are left alone:
//!   they are how a game talks to its store's client.
//! - FindWindow(Ex) doesn't find the other copy's windows (a game that looks for its own
//!   window class at start-up and quits, or hands over to it, when one is there).

use std::sync::OnceLock;

use retour::GenericDetour;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;
use crate::read;

type CreateMutexW = unsafe extern "system" fn(*const core::ffi::c_void, i32, *const u16) -> isize;
type CreateMutexExW =
    unsafe extern "system" fn(*const core::ffi::c_void, *const u16, u32, u32) -> isize;
type OpenMutexW = unsafe extern "system" fn(u32, i32, *const u16) -> isize;

static CREATE: OnceLock<GenericDetour<CreateMutexW>> = OnceLock::new();
static CREATE_EX: OnceLock<GenericDetour<CreateMutexExW>> = OnceLock::new();
static OPEN: OnceLock<GenericDetour<OpenMutexW>> = OnceLock::new();

const MARKER: &str = "SingleInstanceMutex";

/// Names of SDKs' and Windows' own mutexes, never renamed (lower case, as prefixes of
/// the name after any `Global\` / `Local\`).
const NEVER: [&str; 9] = ["steam", "valve", "galaxy", "gog", "eos", "epic", "peerparty", "playfab", "msctf"];

/// Whether a mutex named `name` is a single-instance check of the game `stem` (its
/// program's name, lower case).
fn single_instance(name: &str, stem: &str) -> bool {
    if name.contains(MARKER) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    let bare = lower.rsplit('\\').next().unwrap_or(&lower);
    if NEVER.iter().any(|n| bare.starts_with(n)) {
        return false;
    }
    bare.contains("instance") || (stem.len() >= 4 && bare.contains(stem))
}

/// This program's name without its extension, lower case.
fn stem() -> &'static str {
    static STEM: OnceLock<String> = OnceLock::new();
    STEM.get_or_init(|| {
        std::env::current_exe().ok().and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase())).unwrap_or_default()
    })
}

pub fn enabled() -> bool {
    std::env::var_os("PEERPARTY_MULTI_INSTANCE").is_some()
}

/// Hooks kernelbase's mutex entry points. kernel32's exports forward here.
pub fn attach_kernelbase(exports: &[Export]) -> usize {
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
    n
}

fn hook_one(exports: &[Export], name: &str, install: impl FnOnce(*const ()) -> bool) -> usize {
    let Some(export) = exports.iter().find(|e| e.name == name) else {
        log!("instance {name} not exported");
        return 0;
    };
    usize::from(install(export.addr))
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let hook = match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => h,
            Err(e) => {
                log!("instance {label} not hooked: {e}");
                return false;
            }
        };
        if slot.set(hook).is_err() {
            log!("instance {label} already hooked");
            return false;
        }
        if let Err(e) = unsafe { slot.get().unwrap().enable() } {
            log!("instance {label} enable failed: {e}");
            return false;
        }
        true
    })
}

/// The per-process name for a single-instance mutex, or None to leave `name` alone.
fn rename(name: *const u16, api: &str) -> Option<Vec<u16>> {
    if name.is_null() {
        return None;
    }
    let text = read::wstr(name as usize)?;
    // CreateMutexW may reach CreateMutexExW internally; rename only once.
    if !single_instance(&text, stem()) || text.contains("-peerparty-") {
        return None;
    }
    let renamed = format!("{text}-peerparty-{}", unsafe { GetCurrentProcessId() });
    log!("instance {api} {text} -> {renamed}");
    Some(renamed.encode_utf16().chain(Some(0)).collect())
}

unsafe extern "system" fn detour_create(
    attrs: *const core::ffi::c_void,
    owner: i32,
    name: *const u16,
) -> isize {
    let Some(hook) = CREATE.get() else { return 0 };
    let renamed = rename(name, "CreateMutexW");
    let name = renamed.as_ref().map_or(name, |w| w.as_ptr());
    unsafe { hook.call(attrs, owner, name) }
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
    let renamed = rename(name, "CreateMutexExW");
    let name = renamed.as_ref().map_or(name, |w| w.as_ptr());
    unsafe { hook.call(attrs, name, flags, access) }
}

unsafe extern "system" fn detour_open(access: u32, inherit: i32, name: *const u16) -> isize {
    let Some(hook) = OPEN.get() else { return 0 };
    let renamed = rename(name, "OpenMutexW");
    let name = renamed.as_ref().map_or(name, |w| w.as_ptr());
    unsafe { hook.call(access, inherit, name) }
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

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    #[test]
    fn renames_only_single_instance_mutexes_once() {
        let pid = unsafe { GetCurrentProcessId() };
        let unity = wide("Some Studio-SingleInstanceMutex-SomeGameApp");
        let got = rename(unity.as_ptr(), "test").expect("renamed");
        let got = String::from_utf16(&got[..got.len() - 1]).unwrap();
        assert_eq!(
            got,
            format!("Some Studio-SingleInstanceMutex-SomeGameApp-peerparty-{pid}")
        );
        assert!(rename(wide(&got).as_ptr(), "test").is_none());
        assert!(rename(wide("Local\\SomethingElse").as_ptr(), "test").is_none());
        assert!(single_instance("Global\\SpaceGame_Instance", "launcher"));
        assert!(single_instance("SpaceGameRunning", "spacegame"));
        assert!(!single_instance("Steam3Master_SharedMemLock", "steam3master"), "a store's own");
        assert!(!single_instance("Global\\EOSOverlayInstance", "spacegame"));
        assert!(!single_instance("Local\\RenderLock", "spacegame"));
        assert!(rename(std::ptr::null(), "test").is_none());
    }
}
