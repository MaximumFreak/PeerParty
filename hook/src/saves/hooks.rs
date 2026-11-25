//! kernelbase's file entry points (kernel32's forward there; one kernel32 has of its own
//! is hooked there): how .NET, Unity and native games open, probe, list, copy, move and
//! delete their saves. Each looks at the path for an id-keyed save to copy in first
//! (`touch`), and in a later player's copy sends it to the player's own folder
//! (vfs.rs `route`). Handle-based renames (`SetFileInformationByHandle`) get their
//! target sent the same way, so a save written to a temporary file and renamed onto the
//! real name stays the player's. In a split-screen copy, CreateFile also sends an Unreal
//! game's settings file to PeerParty's copy of it (screen_ini.rs).
//!
//! They're only armed when needed (saves/mod.rs).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use retour::GenericDetour;

use super::vfs::{self, Intent, Route};
use super::{BUSY, ansi, touch, touch_a};
use crate::log;
use crate::sys::pe::Export;
use crate::sys::read;

/// A path the game passed, as the call should get it: None to pass the game's own, else
/// the new path (NUL-terminated), or `Done` to answer the call without making it.
enum Own<T> {
    Same,
    To(Vec<T>),
    Done,
}

fn route_str(path: Option<String>, intent: Intent) -> Route {
    path.map_or(Route::Same, |p| vfs::route(&p, intent))
}

/// `route` for a wide path.
fn own_w(name: *const u16, intent: Intent) -> Own<u16> {
    if name.is_null() || vfs::state().is_none() || BUSY.with(|b| b.get()) {
        return Own::Same;
    }
    BUSY.with(|b| b.set(true));
    let r = std::panic::catch_unwind(|| route_str(read::wstr(name as usize), intent)).unwrap_or(Route::Same);
    BUSY.with(|b| b.set(false));
    match r {
        Route::Same => Own::Same,
        Route::To(p) => Own::To(p.encode_utf16().chain(Some(0)).collect()),
        Route::Done => Own::Done,
    }
}

/// `route` for an ANSI path: the new one in the system code page.
fn own_a(name: *const u8, intent: Intent) -> Own<u8> {
    use windows_sys::Win32::Globalization::{CP_ACP, WideCharToMultiByte};
    if name.is_null() || vfs::state().is_none() || BUSY.with(|b| b.get()) {
        return Own::Same;
    }
    BUSY.with(|b| b.set(true));
    let r = std::panic::catch_unwind(|| route_str(ansi(name), intent)).unwrap_or(Route::Same);
    BUSY.with(|b| b.set(false));
    let p = match r {
        Route::Same => return Own::Same,
        Route::Done => return Own::Done,
        Route::To(p) => p,
    };
    let w: Vec<u16> = p.encode_utf16().collect();
    let n = unsafe { WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, std::ptr::null_mut(), 0, std::ptr::null(), std::ptr::null_mut()) };
    if n <= 0 {
        return Own::Same;
    }
    let mut out = vec![0u8; n as usize + 1];
    unsafe { WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, out.as_mut_ptr(), n, std::ptr::null(), std::ptr::null_mut()) };
    Own::To(out)
}

/// Unreal's settings file in a split-screen copy (screen_ini.rs), after `own_w` gave
/// the copy's own path.
fn ini_w(name: *const u16, intent: Intent) -> Own<u16> {
    if name.is_null() || BUSY.with(|b| b.get()) || crate::window::virtual_size().is_none() {
        return Own::Same;
    }
    BUSY.with(|b| b.set(true));
    let r = std::panic::catch_unwind(|| read::wstr(name as usize).and_then(|p| super::screen_ini::route(&p, intent))).unwrap_or(None);
    BUSY.with(|b| b.set(false));
    r.map_or(Own::Same, |p| Own::To(p.encode_utf16().chain(Some(0)).collect()))
}

/// A move's source the call moved away (vfs.rs `gone`).
fn gone_w(name: *const u16) {
    if name.is_null() || vfs::state().is_none() || BUSY.with(|b| b.get()) {
        return;
    }
    BUSY.with(|b| b.set(true));
    let _ = std::panic::catch_unwind(|| read::wstr(name as usize).map(|p| vfs::gone(&p)));
    BUSY.with(|b| b.set(false));
}

fn gone_a(name: *const u8) {
    if name.is_null() || vfs::state().is_none() || BUSY.with(|b| b.get()) {
        return;
    }
    BUSY.with(|b| b.set(true));
    let _ = std::panic::catch_unwind(|| ansi(name).map(|p| vfs::gone(&p)));
    BUSY.with(|b| b.set(false));
}

/// Rebinds `$name` to the routed path, kept alive in `$keep`; `Done` returns `$done`.
macro_rules! own {
    ($f:ident, $name:ident, $intent:expr, $keep:ident, $done:expr) => {
        let $keep = $f($name, $intent);
        let $name = match &$keep {
            Own::Same => $name,
            Own::To(v) => v.as_ptr(),
            Own::Done => return $done,
        };
    };
}

/// Whether CreateFile's access and disposition write, make or only read.
pub fn open_intent(access: u32, disposition: u32, flags: u32) -> Intent {
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_ALL: u32 = 0x1000_0000;
    const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
    const WRITES: u32 = 0x2 | 0x4 | 0x10 | 0x100 | 0x1_0000 | 0x4_0000 | 0x8_0000; // data, append, EA, attributes, delete, DAC, owner
    const DELETE_ON_CLOSE: u32 = 0x0400_0000;
    match disposition {
        1 | 2 | 5 => Intent::Create,
        _ if access & (GENERIC_WRITE | GENERIC_ALL | MAXIMUM_ALLOWED | WRITES) != 0 || flags & DELETE_ON_CLOSE != 0 => Intent::Write,
        // OPEN_ALWAYS makes the file if it isn't there.
        4 => Intent::Write,
        _ => Intent::Read,
    }
}

const TRUE: i32 = 1;
/// `MOVEFILE_COPY_ALLOWED`: a player's own folder may be on another drive than the path
/// the game named.
const COPY_ALLOWED: u32 = 0x2;

type CreateFileW = unsafe extern "system" fn(*const u16, u32, u32, usize, u32, u32, isize) -> isize;
type CreateFileA = unsafe extern "system" fn(*const u8, u32, u32, usize, u32, u32, isize) -> isize;
type CreateFile2 = unsafe extern "system" fn(*const u16, u32, u32, u32, usize) -> isize;
type GetFileAttributesW = unsafe extern "system" fn(*const u16) -> u32;
type GetFileAttributesA = unsafe extern "system" fn(*const u8) -> u32;
type GetFileAttributesExW = unsafe extern "system" fn(*const u16, i32, usize) -> i32;
type GetFileAttributesExA = unsafe extern "system" fn(*const u8, i32, usize) -> i32;
type FindFirstFileExW = unsafe extern "system" fn(*const u16, i32, usize, i32, usize, u32) -> isize;
type FindFirstFileExA = unsafe extern "system" fn(*const u8, i32, usize, i32, usize, u32) -> isize;
type FindFirstFileW = unsafe extern "system" fn(*const u16, usize) -> isize;
type FindFirstFileA = unsafe extern "system" fn(*const u8, usize) -> isize;
type CreateDirectoryW = unsafe extern "system" fn(*const u16, usize) -> i32;
type CreateDirectoryA = unsafe extern "system" fn(*const u8, usize) -> i32;
type CreateDirectoryExW = unsafe extern "system" fn(*const u16, *const u16, usize) -> i32;
type CreateDirectoryExA = unsafe extern "system" fn(*const u8, *const u8, usize) -> i32;
type OnePathW = unsafe extern "system" fn(*const u16) -> i32;
type OnePathA = unsafe extern "system" fn(*const u8) -> i32;
type SetAttrsW = unsafe extern "system" fn(*const u16, u32) -> i32;
type SetAttrsA = unsafe extern "system" fn(*const u8, u32) -> i32;
type MoveW = unsafe extern "system" fn(*const u16, *const u16) -> i32;
type MoveA = unsafe extern "system" fn(*const u8, *const u8) -> i32;
type MoveExW = unsafe extern "system" fn(*const u16, *const u16, u32) -> i32;
type MoveExA = unsafe extern "system" fn(*const u8, *const u8, u32) -> i32;
type MoveProgressW = unsafe extern "system" fn(*const u16, *const u16, usize, usize, u32) -> i32;
type MoveProgressA = unsafe extern "system" fn(*const u8, *const u8, usize, usize, u32) -> i32;
type ReplaceW = unsafe extern "system" fn(*const u16, *const u16, *const u16, u32, usize, usize) -> i32;
type ReplaceA = unsafe extern "system" fn(*const u8, *const u8, *const u8, u32, usize, usize) -> i32;
type CopyW = unsafe extern "system" fn(*const u16, *const u16, i32) -> i32;
type CopyA = unsafe extern "system" fn(*const u8, *const u8, i32) -> i32;
type CopyExW = unsafe extern "system" fn(*const u16, *const u16, usize, usize, usize, u32) -> i32;
type CopyExA = unsafe extern "system" fn(*const u8, *const u8, usize, usize, usize, u32) -> i32;
type Copy2 = unsafe extern "system" fn(*const u16, *const u16, usize) -> i32;
type HardLinkW = unsafe extern "system" fn(*const u16, *const u16, usize) -> i32;
type HardLinkA = unsafe extern "system" fn(*const u8, *const u8, usize) -> i32;
type SetInfo = unsafe extern "system" fn(isize, i32, *mut u8, u32) -> i32;
type ProfileStringW = unsafe extern "system" fn(*const u16, *const u16, *const u16, *mut u16, u32, *const u16) -> u32;
type ProfileStringA = unsafe extern "system" fn(*const u8, *const u8, *const u8, *mut u8, u32, *const u8) -> u32;
type ProfileIntW = unsafe extern "system" fn(*const u16, *const u16, i32, *const u16) -> u32;
type ProfileIntA = unsafe extern "system" fn(*const u8, *const u8, i32, *const u8) -> u32;
type WriteProfileW = unsafe extern "system" fn(*const u16, *const u16, *const u16, *const u16) -> i32;
type WriteProfileA = unsafe extern "system" fn(*const u8, *const u8, *const u8, *const u8) -> i32;
type ProfileSectionW = unsafe extern "system" fn(*const u16, *mut u16, u32, *const u16) -> u32;
type ProfileSectionA = unsafe extern "system" fn(*const u8, *mut u8, u32, *const u8) -> u32;

static CREATE_FILE: OnceLock<GenericDetour<CreateFileW>> = OnceLock::new();
static CREATE_FILE_A: OnceLock<GenericDetour<CreateFileA>> = OnceLock::new();
static CREATE_FILE2: OnceLock<GenericDetour<CreateFile2>> = OnceLock::new();
static ATTRS: OnceLock<GenericDetour<GetFileAttributesW>> = OnceLock::new();
static ATTRS_A: OnceLock<GenericDetour<GetFileAttributesA>> = OnceLock::new();
static ATTRS_EX: OnceLock<GenericDetour<GetFileAttributesExW>> = OnceLock::new();
static ATTRS_EX_A: OnceLock<GenericDetour<GetFileAttributesExA>> = OnceLock::new();
static FIND: OnceLock<GenericDetour<FindFirstFileExW>> = OnceLock::new();
static FIND_A: OnceLock<GenericDetour<FindFirstFileExA>> = OnceLock::new();
static FIND_PLAIN: OnceLock<GenericDetour<FindFirstFileW>> = OnceLock::new();
static FIND_PLAIN_A: OnceLock<GenericDetour<FindFirstFileA>> = OnceLock::new();
static MKDIR: OnceLock<GenericDetour<CreateDirectoryW>> = OnceLock::new();
static MKDIR_A: OnceLock<GenericDetour<CreateDirectoryA>> = OnceLock::new();
static MKDIR_EX: OnceLock<GenericDetour<CreateDirectoryExW>> = OnceLock::new();
static MKDIR_EX_A: OnceLock<GenericDetour<CreateDirectoryExA>> = OnceLock::new();
static DELETE: OnceLock<GenericDetour<OnePathW>> = OnceLock::new();
static DELETE_A: OnceLock<GenericDetour<OnePathA>> = OnceLock::new();
static RMDIR: OnceLock<GenericDetour<OnePathW>> = OnceLock::new();
static RMDIR_A: OnceLock<GenericDetour<OnePathA>> = OnceLock::new();
static SET_ATTRS: OnceLock<GenericDetour<SetAttrsW>> = OnceLock::new();
static SET_ATTRS_A: OnceLock<GenericDetour<SetAttrsA>> = OnceLock::new();
static MOVE: OnceLock<GenericDetour<MoveW>> = OnceLock::new();
static MOVE_A: OnceLock<GenericDetour<MoveA>> = OnceLock::new();
static MOVE_EX: OnceLock<GenericDetour<MoveExW>> = OnceLock::new();
static MOVE_EX_A: OnceLock<GenericDetour<MoveExA>> = OnceLock::new();
static MOVE_PROGRESS: OnceLock<GenericDetour<MoveProgressW>> = OnceLock::new();
static MOVE_PROGRESS_A: OnceLock<GenericDetour<MoveProgressA>> = OnceLock::new();
static REPLACE: OnceLock<GenericDetour<ReplaceW>> = OnceLock::new();
static REPLACE_A: OnceLock<GenericDetour<ReplaceA>> = OnceLock::new();
static COPY: OnceLock<GenericDetour<CopyW>> = OnceLock::new();
static COPY_A: OnceLock<GenericDetour<CopyA>> = OnceLock::new();
static COPY_EX: OnceLock<GenericDetour<CopyExW>> = OnceLock::new();
static COPY_EX_A: OnceLock<GenericDetour<CopyExA>> = OnceLock::new();
static COPY2: OnceLock<GenericDetour<Copy2>> = OnceLock::new();
static HARD_LINK: OnceLock<GenericDetour<HardLinkW>> = OnceLock::new();
static HARD_LINK_A: OnceLock<GenericDetour<HardLinkA>> = OnceLock::new();
static SET_INFO: OnceLock<GenericDetour<SetInfo>> = OnceLock::new();
static PROFILE_STR: OnceLock<GenericDetour<ProfileStringW>> = OnceLock::new();
static PROFILE_STR_A: OnceLock<GenericDetour<ProfileStringA>> = OnceLock::new();
static PROFILE_INT: OnceLock<GenericDetour<ProfileIntW>> = OnceLock::new();
static PROFILE_INT_A: OnceLock<GenericDetour<ProfileIntA>> = OnceLock::new();
static WRITE_PROFILE: OnceLock<GenericDetour<WriteProfileW>> = OnceLock::new();
static WRITE_PROFILE_A: OnceLock<GenericDetour<WriteProfileA>> = OnceLock::new();
static PROFILE_SECTION: OnceLock<GenericDetour<ProfileSectionW>> = OnceLock::new();
static PROFILE_SECTION_A: OnceLock<GenericDetour<ProfileSectionA>> = OnceLock::new();

static ARMED: AtomicBool = AtomicBool::new(false);

/// Hooks `name` from kernelbase's exports, else kernel32's own (an export kernel32
/// forwards to kernelbase is kernelbase's).
fn arm<T>(kb: &[Export], k32: &[Export], name: &str, slot: &OnceLock<GenericDetour<T>>, detour: T) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    if slot.get().is_some() {
        return 0;
    }
    let Some(e) = kb.iter().chain(k32).find(|e| e.name == name) else {
        return 0;
    };
    // `arm_now` hooks these while the game runs (its threads may be in them): the
    // others are paused while each prologue is written (not at the hook's own start,
    // before the game has threads: `sys::hook::frozen`).
    let target: T = unsafe { T::from_ptr(e.addr) };
    usize::from(crate::sys::detour::try_arm(slot, target, detour, "saves", name, true) == crate::sys::detour::Armed::Hooked)
}

/// kernel32's exports (its own code, forwarders left out by `pe::exports`).
fn kernel32_exports() -> Vec<Export> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let name: Vec<u16> = "kernel32.dll".encode_utf16().chain(Some(0)).collect();
    let base = unsafe { GetModuleHandleW(name.as_ptr()) };
    if base.is_null() {
        return Vec::new();
    }
    unsafe { crate::sys::pe::exports(base.cast()) }.map(|t| t.code).unwrap_or_default()
}

/// Arms the file hooks (once).
pub fn attach_kernelbase(kb: &[Export]) -> usize {
    if ARMED.swap(true, Ordering::SeqCst) {
        return 0;
    }
    let k32 = kernel32_exports();
    let k = &k32[..];
    arm(kb, k, "CreateFileW", &CREATE_FILE, detour_create_file as CreateFileW)
        + arm(kb, k, "CreateFileA", &CREATE_FILE_A, detour_create_file_a as CreateFileA)
        + arm(kb, k, "CreateFile2", &CREATE_FILE2, detour_create_file2 as CreateFile2)
        + arm(kb, k, "GetFileAttributesW", &ATTRS, detour_attrs as GetFileAttributesW)
        + arm(kb, k, "GetFileAttributesA", &ATTRS_A, detour_attrs_a as GetFileAttributesA)
        + arm(kb, k, "GetFileAttributesExW", &ATTRS_EX, detour_attrs_ex as GetFileAttributesExW)
        + arm(kb, k, "GetFileAttributesExA", &ATTRS_EX_A, detour_attrs_ex_a as GetFileAttributesExA)
        + arm(kb, k, "FindFirstFileExW", &FIND, detour_find as FindFirstFileExW)
        + arm(kb, k, "FindFirstFileExA", &FIND_A, detour_find_a as FindFirstFileExA)
        // kernelbase's FindFirstFileW doesn't go through its FindFirstFileExW.
        + arm(kb, k, "FindFirstFileW", &FIND_PLAIN, detour_find_plain as FindFirstFileW)
        + arm(kb, k, "FindFirstFileA", &FIND_PLAIN_A, detour_find_plain_a as FindFirstFileA)
        + arm(kb, k, "CreateDirectoryW", &MKDIR, detour_mkdir as CreateDirectoryW)
        + arm(kb, k, "CreateDirectoryA", &MKDIR_A, detour_mkdir_a as CreateDirectoryA)
        + arm(kb, k, "CreateDirectoryExW", &MKDIR_EX, detour_mkdir_ex as CreateDirectoryExW)
        + arm(kb, k, "CreateDirectoryExA", &MKDIR_EX_A, detour_mkdir_ex_a as CreateDirectoryExA)
        + arm(kb, k, "DeleteFileW", &DELETE, detour_delete as OnePathW)
        + arm(kb, k, "DeleteFileA", &DELETE_A, detour_delete_a as OnePathA)
        + arm(kb, k, "RemoveDirectoryW", &RMDIR, detour_rmdir as OnePathW)
        + arm(kb, k, "RemoveDirectoryA", &RMDIR_A, detour_rmdir_a as OnePathA)
        + arm(kb, k, "SetFileAttributesW", &SET_ATTRS, detour_set_attrs as SetAttrsW)
        + arm(kb, k, "SetFileAttributesA", &SET_ATTRS_A, detour_set_attrs_a as SetAttrsA)
        + arm(kb, k, "MoveFileW", &MOVE, detour_move as MoveW)
        + arm(kb, k, "MoveFileA", &MOVE_A, detour_move_a as MoveA)
        + arm(kb, k, "MoveFileExW", &MOVE_EX, detour_move_ex as MoveExW)
        + arm(kb, k, "MoveFileExA", &MOVE_EX_A, detour_move_ex_a as MoveExA)
        + arm(kb, k, "MoveFileWithProgressW", &MOVE_PROGRESS, detour_move_progress as MoveProgressW)
        + arm(kb, k, "MoveFileWithProgressA", &MOVE_PROGRESS_A, detour_move_progress_a as MoveProgressA)
        + arm(kb, k, "ReplaceFileW", &REPLACE, detour_replace as ReplaceW)
        + arm(kb, k, "ReplaceFileA", &REPLACE_A, detour_replace_a as ReplaceA)
        + arm(kb, k, "CopyFileW", &COPY, detour_copy as CopyW)
        + arm(kb, k, "CopyFileA", &COPY_A, detour_copy_a as CopyA)
        + arm(kb, k, "CopyFileExW", &COPY_EX, detour_copy_ex as CopyExW)
        + arm(kb, k, "CopyFileExA", &COPY_EX_A, detour_copy_ex_a as CopyExA)
        + arm(kb, k, "CopyFile2", &COPY2, detour_copy2 as Copy2)
        + arm(kb, k, "CreateHardLinkW", &HARD_LINK, detour_hard_link as HardLinkW)
        + arm(kb, k, "CreateHardLinkA", &HARD_LINK_A, detour_hard_link_a as HardLinkA)
        + arm(kb, k, "SetFileInformationByHandle", &SET_INFO, detour_set_info as SetInfo)
        + arm(kb, k, "GetPrivateProfileStringW", &PROFILE_STR, detour_profile_str as ProfileStringW)
        + arm(kb, k, "GetPrivateProfileStringA", &PROFILE_STR_A, detour_profile_str_a as ProfileStringA)
        + arm(kb, k, "GetPrivateProfileIntW", &PROFILE_INT, detour_profile_int as ProfileIntW)
        + arm(kb, k, "GetPrivateProfileIntA", &PROFILE_INT_A, detour_profile_int_a as ProfileIntA)
        + arm(kb, k, "WritePrivateProfileStringW", &WRITE_PROFILE, detour_write_profile as WriteProfileW)
        + arm(kb, k, "WritePrivateProfileStringA", &WRITE_PROFILE_A, detour_write_profile_a as WriteProfileA)
        + arm(kb, k, "GetPrivateProfileSectionW", &PROFILE_SECTION, detour_profile_section as ProfileSectionW)
        + arm(kb, k, "GetPrivateProfileSectionA", &PROFILE_SECTION_A, detour_profile_section_a as ProfileSectionA)
}

/// Whether the hooks are in.
pub fn armed() -> bool {
    ARMED.load(Ordering::SeqCst)
}

/// A split-screen copy can't open another player's game controller (input/device.rs).
fn not_there() -> isize {
    unsafe { windows_sys::Win32::Foundation::SetLastError(windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND) };
    -1
}

/// A call answered done without making it (`Route::Done`).
fn done() -> i32 {
    unsafe { windows_sys::Win32::Foundation::SetLastError(0) };
    TRUE
}

unsafe extern "system" fn detour_create_file(name: *const u16, access: u32, share: u32, sa: usize, disposition: u32, flags: u32, template: isize) -> isize {
    if crate::input::refuse_open_w(name) {
        return not_there();
    }
    touch(name);
    let asked = name;
    let intent = open_intent(access, disposition, flags);
    own!(own_w, name, intent, keep, not_there());
    own!(ini_w, name, intent, keep_ini, not_there());
    let h = unsafe { CREATE_FILE.get().unwrap().call(name, access, share, sa, disposition, flags, template) };
    crate::input::opened_w(asked, h);
    h
}

unsafe extern "system" fn detour_create_file_a(name: *const u8, access: u32, share: u32, sa: usize, disposition: u32, flags: u32, template: isize) -> isize {
    if crate::input::refuse_open_a(name) {
        return not_there();
    }
    touch_a(name);
    let asked = name;
    own!(own_a, name, open_intent(access, disposition, flags), keep, not_there());
    let h = unsafe { CREATE_FILE_A.get().unwrap().call(name, access, share, sa, disposition, flags, template) };
    crate::input::opened_a(asked, h);
    h
}

unsafe extern "system" fn detour_create_file2(name: *const u16, access: u32, share: u32, disposition: u32, params: usize) -> isize {
    if crate::input::refuse_open_w(name) {
        return not_there();
    }
    touch(name);
    let asked = name;
    // CREATEFILE2_EXTENDED_PARAMETERS: dwSize, dwFileAttributes, dwFileFlags.
    let flags = if params != 0 && crate::sys::win::readable(params, 12) { unsafe { *((params + 8) as *const u32) } } else { 0 };
    let intent = open_intent(access, disposition, flags);
    own!(own_w, name, intent, keep, not_there());
    own!(ini_w, name, intent, keep_ini, not_there());
    let h = unsafe { CREATE_FILE2.get().unwrap().call(name, access, share, disposition, params) };
    crate::input::opened_w(asked, h);
    h
}

unsafe extern "system" fn detour_attrs(name: *const u16) -> u32 {
    touch(name);
    own!(own_w, name, Intent::Read, keep, u32::MAX);
    unsafe { ATTRS.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_attrs_a(name: *const u8) -> u32 {
    touch_a(name);
    own!(own_a, name, Intent::Read, keep, u32::MAX);
    unsafe { ATTRS_A.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_attrs_ex(name: *const u16, level: i32, info: usize) -> i32 {
    touch(name);
    own!(own_w, name, Intent::Read, keep, 0);
    unsafe { ATTRS_EX.get().unwrap().call(name, level, info) }
}

unsafe extern "system" fn detour_attrs_ex_a(name: *const u8, level: i32, info: usize) -> i32 {
    touch_a(name);
    own!(own_a, name, Intent::Read, keep, 0);
    unsafe { ATTRS_EX_A.get().unwrap().call(name, level, info) }
}

unsafe extern "system" fn detour_find(name: *const u16, level: i32, data: usize, op: i32, filter: usize, flags: u32) -> isize {
    touch(name);
    let asked = name;
    own!(own_w, name, Intent::Read, keep, not_there());
    if matches!(keep, Own::Same) && vfs::state().is_some_and(|s| s.slot != 0) {
        if let Some(p) = read::wstr(asked as usize) {
            in_game_dir_listing(&p);
        }
    }
    unsafe { FIND.get().unwrap().call(name, level, data, op, filter, flags) }
}

unsafe extern "system" fn detour_find_a(name: *const u8, level: i32, data: usize, op: i32, filter: usize, flags: u32) -> isize {
    touch_a(name);
    own!(own_a, name, Intent::Read, keep, not_there());
    unsafe { FIND_A.get().unwrap().call(name, level, data, op, filter, flags) }
}

unsafe extern "system" fn detour_find_plain(name: *const u16, data: usize) -> isize {
    touch(name);
    let asked = name;
    own!(own_w, name, Intent::Read, keep, not_there());
    if matches!(keep, Own::Same) && vfs::state().is_some_and(|s| s.slot != 0) {
        if let Some(p) = read::wstr(asked as usize) {
            in_game_dir_listing(&p);
        }
    }
    unsafe { FIND_PLAIN.get().unwrap().call(name, data) }
}

unsafe extern "system" fn detour_find_plain_a(name: *const u8, data: usize) -> isize {
    touch_a(name);
    own!(own_a, name, Intent::Read, keep, not_there());
    unsafe { FIND_PLAIN_A.get().unwrap().call(name, data) }
}

/// A later player lists a folder in the game's folder: its own files there aren't in it.
fn in_game_dir_listing(pattern: &str) {
    let Some(st) = vfs::state() else { return };
    if BUSY.with(|b| b.get()) {
        return;
    }
    BUSY.with(|b| b.set(true));
    let _ = std::panic::catch_unwind(|| {
        if let Some(full) = vfs::full_path(pattern) {
            if matches!(peerparty_protocol::saves::classify(&st.roots, &full), Some((peerparty_protocol::saves::Zone::GameDir, _))) {
                vfs::listing_gap(&full);
            }
        }
    });
    BUSY.with(|b| b.set(false));
}

unsafe extern "system" fn detour_mkdir(name: *const u16, sa: usize) -> i32 {
    touch(name);
    own!(own_w, name, Intent::Mkdir, keep, 0);
    unsafe { MKDIR.get().unwrap().call(name, sa) }
}

unsafe extern "system" fn detour_mkdir_a(name: *const u8, sa: usize) -> i32 {
    touch_a(name);
    own!(own_a, name, Intent::Mkdir, keep, 0);
    unsafe { MKDIR_A.get().unwrap().call(name, sa) }
}

unsafe extern "system" fn detour_mkdir_ex(template: *const u16, name: *const u16, sa: usize) -> i32 {
    touch(name);
    own!(own_w, template, Intent::Read, keep_t, 0);
    own!(own_w, name, Intent::Mkdir, keep, 0);
    unsafe { MKDIR_EX.get().unwrap().call(template, name, sa) }
}

unsafe extern "system" fn detour_mkdir_ex_a(template: *const u8, name: *const u8, sa: usize) -> i32 {
    touch_a(name);
    own!(own_a, template, Intent::Read, keep_t, 0);
    own!(own_a, name, Intent::Mkdir, keep, 0);
    unsafe { MKDIR_EX_A.get().unwrap().call(template, name, sa) }
}

// Calls that only change files: the player's own path (no `touch`: that copies an
// id-keyed save in before the game reads it).

unsafe extern "system" fn detour_delete(name: *const u16) -> i32 {
    own!(own_w, name, Intent::Delete, keep, done());
    unsafe { DELETE.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_delete_a(name: *const u8) -> i32 {
    own!(own_a, name, Intent::Delete, keep, done());
    unsafe { DELETE_A.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_rmdir(name: *const u16) -> i32 {
    own!(own_w, name, Intent::RemoveDir, keep, done());
    unsafe { RMDIR.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_rmdir_a(name: *const u8) -> i32 {
    own!(own_a, name, Intent::RemoveDir, keep, done());
    unsafe { RMDIR_A.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_set_attrs(name: *const u16, attrs: u32) -> i32 {
    own!(own_w, name, Intent::Write, keep, done());
    unsafe { SET_ATTRS.get().unwrap().call(name, attrs) }
}

unsafe extern "system" fn detour_set_attrs_a(name: *const u8, attrs: u32) -> i32 {
    own!(own_a, name, Intent::Write, keep, done());
    unsafe { SET_ATTRS_A.get().unwrap().call(name, attrs) }
}

/// Whether either path of a move was sent elsewhere: then copying across drives is let.
fn moved<T>(a: &Own<T>, b: &Own<T>) -> bool {
    matches!(a, Own::To(_)) || matches!(b, Own::To(_))
}

unsafe extern "system" fn detour_move(from: *const u16, to: *const u16) -> i32 {
    let asked = from;
    own!(own_w, from, Intent::MoveFrom, keep_f, done());
    own!(own_w, to, Intent::Create, keep_t, done());
    let ok = if moved(&keep_f, &keep_t) {
        // MoveFileW is MoveFileExW with COPY_ALLOWED already.
        unsafe { MOVE_EX.get().map_or_else(|| MOVE.get().unwrap().call(from, to), |m| m.call(from, to, COPY_ALLOWED)) }
    } else {
        unsafe { MOVE.get().unwrap().call(from, to) }
    };
    if ok != 0 {
        gone_w(asked);
    }
    ok
}

unsafe extern "system" fn detour_move_a(from: *const u8, to: *const u8) -> i32 {
    let asked = from;
    own!(own_a, from, Intent::MoveFrom, keep_f, done());
    own!(own_a, to, Intent::Create, keep_t, done());
    let ok = if moved(&keep_f, &keep_t) {
        unsafe { MOVE_EX_A.get().map_or_else(|| MOVE_A.get().unwrap().call(from, to), |m| m.call(from, to, COPY_ALLOWED)) }
    } else {
        unsafe { MOVE_A.get().unwrap().call(from, to) }
    };
    if ok != 0 {
        gone_a(asked);
    }
    ok
}

unsafe extern "system" fn detour_move_ex(from: *const u16, to: *const u16, flags: u32) -> i32 {
    let asked = from;
    own!(own_w, from, Intent::MoveFrom, keep_f, done());
    own!(own_w, to, Intent::Create, keep_t, done());
    let flags = if moved(&keep_f, &keep_t) { flags | COPY_ALLOWED } else { flags };
    let ok = unsafe { MOVE_EX.get().unwrap().call(from, to, flags) };
    if ok != 0 {
        gone_w(asked);
    }
    ok
}

unsafe extern "system" fn detour_move_ex_a(from: *const u8, to: *const u8, flags: u32) -> i32 {
    let asked = from;
    own!(own_a, from, Intent::MoveFrom, keep_f, done());
    own!(own_a, to, Intent::Create, keep_t, done());
    let flags = if moved(&keep_f, &keep_t) { flags | COPY_ALLOWED } else { flags };
    let ok = unsafe { MOVE_EX_A.get().unwrap().call(from, to, flags) };
    if ok != 0 {
        gone_a(asked);
    }
    ok
}

unsafe extern "system" fn detour_move_progress(from: *const u16, to: *const u16, progress: usize, data: usize, flags: u32) -> i32 {
    let asked = from;
    own!(own_w, from, Intent::MoveFrom, keep_f, done());
    own!(own_w, to, Intent::Create, keep_t, done());
    let flags = if moved(&keep_f, &keep_t) { flags | COPY_ALLOWED } else { flags };
    let ok = unsafe { MOVE_PROGRESS.get().unwrap().call(from, to, progress, data, flags) };
    if ok != 0 {
        gone_w(asked);
    }
    ok
}

unsafe extern "system" fn detour_move_progress_a(from: *const u8, to: *const u8, progress: usize, data: usize, flags: u32) -> i32 {
    let asked = from;
    own!(own_a, from, Intent::MoveFrom, keep_f, done());
    own!(own_a, to, Intent::Create, keep_t, done());
    let flags = if moved(&keep_f, &keep_t) { flags | COPY_ALLOWED } else { flags };
    let ok = unsafe { MOVE_PROGRESS_A.get().unwrap().call(from, to, progress, data, flags) };
    if ok != 0 {
        gone_a(asked);
    }
    ok
}

unsafe extern "system" fn detour_replace(replaced: *const u16, replacement: *const u16, backup: *const u16, flags: u32, a: usize, b: usize) -> i32 {
    let asked = replacement;
    own!(own_w, replaced, Intent::Write, keep_r, 0);
    own!(own_w, replacement, Intent::MoveFrom, keep_n, 0);
    own!(own_w, backup, Intent::Create, keep_b, 0);
    let ok = unsafe { REPLACE.get().unwrap().call(replaced, replacement, backup, flags, a, b) };
    if ok != 0 {
        gone_w(asked);
    }
    ok
}

unsafe extern "system" fn detour_replace_a(replaced: *const u8, replacement: *const u8, backup: *const u8, flags: u32, a: usize, b: usize) -> i32 {
    let asked = replacement;
    own!(own_a, replaced, Intent::Write, keep_r, 0);
    own!(own_a, replacement, Intent::MoveFrom, keep_n, 0);
    own!(own_a, backup, Intent::Create, keep_b, 0);
    let ok = unsafe { REPLACE_A.get().unwrap().call(replaced, replacement, backup, flags, a, b) };
    if ok != 0 {
        gone_a(asked);
    }
    ok
}

unsafe extern "system" fn detour_copy(from: *const u16, to: *const u16, fail_if_exists: i32) -> i32 {
    touch(from);
    own!(own_w, from, Intent::Read, keep_f, 0);
    own!(own_w, to, Intent::Create, keep_t, 0);
    unsafe { COPY.get().unwrap().call(from, to, fail_if_exists) }
}

unsafe extern "system" fn detour_copy_a(from: *const u8, to: *const u8, fail_if_exists: i32) -> i32 {
    touch_a(from);
    own!(own_a, from, Intent::Read, keep_f, 0);
    own!(own_a, to, Intent::Create, keep_t, 0);
    unsafe { COPY_A.get().unwrap().call(from, to, fail_if_exists) }
}

unsafe extern "system" fn detour_copy_ex(from: *const u16, to: *const u16, progress: usize, data: usize, cancel: usize, flags: u32) -> i32 {
    touch(from);
    own!(own_w, from, Intent::Read, keep_f, 0);
    own!(own_w, to, Intent::Create, keep_t, 0);
    unsafe { COPY_EX.get().unwrap().call(from, to, progress, data, cancel, flags) }
}

unsafe extern "system" fn detour_copy_ex_a(from: *const u8, to: *const u8, progress: usize, data: usize, cancel: usize, flags: u32) -> i32 {
    touch_a(from);
    own!(own_a, from, Intent::Read, keep_f, 0);
    own!(own_a, to, Intent::Create, keep_t, 0);
    unsafe { COPY_EX_A.get().unwrap().call(from, to, progress, data, cancel, flags) }
}

/// `CopyFile2`: how .NET's `File.Copy` copies (an HRESULT back).
unsafe extern "system" fn detour_copy2(from: *const u16, to: *const u16, params: usize) -> i32 {
    const E_FAIL: i32 = 0x8000_4005_u32 as i32;
    touch(from);
    own!(own_w, from, Intent::Read, keep_f, E_FAIL);
    own!(own_w, to, Intent::Create, keep_t, E_FAIL);
    unsafe { COPY2.get().unwrap().call(from, to, params) }
}

unsafe extern "system" fn detour_hard_link(new: *const u16, existing: *const u16, sa: usize) -> i32 {
    own!(own_w, new, Intent::Create, keep_n, 0);
    own!(own_w, existing, Intent::Read, keep_e, 0);
    unsafe { HARD_LINK.get().unwrap().call(new, existing, sa) }
}

unsafe extern "system" fn detour_hard_link_a(new: *const u8, existing: *const u8, sa: usize) -> i32 {
    own!(own_a, new, Intent::Create, keep_n, 0);
    own!(own_a, existing, Intent::Read, keep_e, 0);
    unsafe { HARD_LINK_A.get().unwrap().call(new, existing, sa) }
}

/// `FILE_RENAME_INFO` / `FILE_LINK_INFORMATION`'s target, when it is a full path (not
/// one relative to a folder handle): where its name starts and its length in bytes.
fn rename_target(info: *const u8, size: u32) -> Option<(usize, usize, String)> {
    let ptr = std::mem::size_of::<usize>();
    // Flags (4 bytes, padded to a pointer), RootDirectory, FileNameLength, FileName.
    let root_at = ptr.max(4);
    let len_at = root_at + ptr;
    let name_at = len_at + 4;
    if info.is_null() || (size as usize) < name_at || !crate::sys::win::readable(info as usize, name_at) {
        return None;
    }
    let root = unsafe { *(info.add(root_at) as *const usize) };
    let len = unsafe { *(info.add(len_at) as *const u32) } as usize;
    if root != 0 || len == 0 || len % 2 != 0 || name_at + len > size as usize || len > 0x10000 {
        return None;
    }
    let w = unsafe { std::slice::from_raw_parts(info.add(name_at) as *const u16, len / 2) };
    let name = String::from_utf16_lossy(w);
    // `\??\C:\...` is the NT spelling of a full path.
    let plain = name.strip_prefix(r"\??\").unwrap_or(&name).to_string();
    let full = plain.get(1..3) == Some(":\\");
    full.then_some((name_at, len, plain))
}

/// A handle's rename or hard link onto a path: the player's own path for the target, so
/// a save written to a temporary file and renamed in stays in its folder.
unsafe extern "system" fn detour_set_info(handle: isize, class: i32, info: *mut u8, size: u32) -> i32 {
    const RENAME: i32 = 3;
    const LINK: i32 = 11;
    const RENAME_EX: i32 = 22;
    const LINK_EX: i32 = 23;
    let real = || unsafe { SET_INFO.get().unwrap().call(handle, class, info, size) };
    if !matches!(class, RENAME | LINK | RENAME_EX | LINK_EX) || vfs::state().is_none() || BUSY.with(|b| b.get()) {
        return real();
    }
    let Some((name_at, _len, target)) = rename_target(info, size) else { return real() };
    BUSY.with(|b| b.set(true));
    let r = std::panic::catch_unwind(|| vfs::route(&target, Intent::Create)).unwrap_or(Route::Same);
    BUSY.with(|b| b.set(false));
    let Route::To(new) = r else { return real() };
    let w: Vec<u16> = new.encode_utf16().collect();
    let mut buf = vec![0u8; name_at + w.len() * 2 + 2];
    unsafe { std::ptr::copy_nonoverlapping(info, buf.as_mut_ptr(), name_at) };
    let len_at = name_at - 4;
    buf[len_at..name_at].copy_from_slice(&((w.len() * 2) as u32).to_le_bytes());
    for (i, c) in w.iter().enumerate() {
        buf[name_at + i * 2..name_at + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
    }
    crate::log::throttled("saves rename", 8, |_| log!("saves: a handle's rename onto {target} goes to {new}"));
    unsafe { SET_INFO.get().unwrap().call(handle, class, buf.as_mut_ptr(), buf.len() as u32) }
}

// `.ini` files read and written through Windows' profile calls.

unsafe extern "system" fn detour_profile_str(app: *const u16, key: *const u16, default: *const u16, out: *mut u16, size: u32, file: *const u16) -> u32 {
    own!(own_w, file, Intent::Read, keep, 0);
    unsafe { PROFILE_STR.get().unwrap().call(app, key, default, out, size, file) }
}

unsafe extern "system" fn detour_profile_str_a(app: *const u8, key: *const u8, default: *const u8, out: *mut u8, size: u32, file: *const u8) -> u32 {
    own!(own_a, file, Intent::Read, keep, 0);
    unsafe { PROFILE_STR_A.get().unwrap().call(app, key, default, out, size, file) }
}

unsafe extern "system" fn detour_profile_int(app: *const u16, key: *const u16, default: i32, file: *const u16) -> u32 {
    own!(own_w, file, Intent::Read, keep, default as u32);
    unsafe { PROFILE_INT.get().unwrap().call(app, key, default, file) }
}

unsafe extern "system" fn detour_profile_int_a(app: *const u8, key: *const u8, default: i32, file: *const u8) -> u32 {
    own!(own_a, file, Intent::Read, keep, default as u32);
    unsafe { PROFILE_INT_A.get().unwrap().call(app, key, default, file) }
}

unsafe extern "system" fn detour_write_profile(app: *const u16, key: *const u16, value: *const u16, file: *const u16) -> i32 {
    own!(own_w, file, Intent::Write, keep, 0);
    unsafe { WRITE_PROFILE.get().unwrap().call(app, key, value, file) }
}

unsafe extern "system" fn detour_write_profile_a(app: *const u8, key: *const u8, value: *const u8, file: *const u8) -> i32 {
    own!(own_a, file, Intent::Write, keep, 0);
    unsafe { WRITE_PROFILE_A.get().unwrap().call(app, key, value, file) }
}

unsafe extern "system" fn detour_profile_section(app: *const u16, out: *mut u16, size: u32, file: *const u16) -> u32 {
    own!(own_w, file, Intent::Read, keep, 0);
    unsafe { PROFILE_SECTION.get().unwrap().call(app, out, size, file) }
}

unsafe extern "system" fn detour_profile_section_a(app: *const u8, out: *mut u8, size: u32, file: *const u8) -> u32 {
    own!(own_a, file, Intent::Read, keep, 0);
    unsafe { PROFILE_SECTION_A.get().unwrap().call(app, out, size, file) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `f` as player 1 sees the disk: this thread's file calls aren't routed.
    fn unrouted<T>(f: impl FnOnce() -> T) -> T {
        BUSY.with(|b| b.set(true));
        let r = f();
        BUSY.with(|b| b.set(false));
        r
    }

    #[test]
    fn open_intents_follow_access_and_disposition() {
        const READ: u32 = 0x8000_0000;
        const WRITE: u32 = 0x4000_0000;
        assert_eq!(open_intent(READ, 3, 0), Intent::Read);
        assert_eq!(open_intent(READ, 4, 0), Intent::Write, "OPEN_ALWAYS makes the file");
        assert_eq!(open_intent(READ | WRITE, 3, 0), Intent::Write);
        assert_eq!(open_intent(0x4, 3, 0), Intent::Write, "append");
        assert_eq!(open_intent(0x1_0000, 3, 0), Intent::Write, "delete access");
        assert_eq!(open_intent(READ, 3, 0x0400_0000), Intent::Write, "delete on close");
        for d in [1, 2, 5] {
            assert_eq!(open_intent(WRITE, d, 0), Intent::Create);
        }
        assert_eq!(open_intent(0x80, 3, 0), Intent::Read, "attributes only");
    }

    #[test]
    fn a_rename_infos_full_target_is_found() {
        let name: Vec<u16> = r"\??\C:\x\save.dat".encode_utf16().collect();
        let ptr = std::mem::size_of::<usize>();
        let name_at = 2 * ptr + 4;
        let mut buf = vec![0u8; name_at + name.len() * 2];
        buf[ptr + ptr..ptr + ptr + 4].copy_from_slice(&((name.len() * 2) as u32).to_le_bytes());
        for (i, c) in name.iter().enumerate() {
            buf[name_at + i * 2..name_at + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        let (at, len, target) = rename_target(buf.as_ptr(), buf.len() as u32).unwrap();
        assert_eq!((at, len, target.as_str()), (name_at, name.len() * 2, r"C:\x\save.dat"));
        // Relative to a folder handle: left alone.
        buf[ptr..2 * ptr].copy_from_slice(&1usize.to_le_bytes()[..ptr]);
        assert!(rename_target(buf.as_ptr(), buf.len() as u32).is_none());
        assert!(rename_target(buf.as_ptr(), 4).is_none(), "too short");
    }

    /// The real calls: a later player's CopyFile2 (.NET's `File.Copy`), CreateFileW, a
    /// handle rename and a listing go to its own folder; player 1's files stay.
    #[test]
    fn the_file_calls_go_to_the_players_folder() {
        use windows_sys::Win32::Storage::FileSystem::{CopyFile2, CreateFileW, FILE_RENAME_INFO, FindClose, FindFirstFileW, SetFileInformationByHandle, WIN32_FIND_DATAW};
        let _g = crate::sys::lock(&vfs::tests::TEST_LOCK);
        let base = std::env::temp_dir().join(format!("peerparty-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join(r"real\LocalLow\PeerPartyTest\Game");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("data_full_-99.data"), "player 1").unwrap();
        let kb = {
            use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
            let n: Vec<u16> = "kernelbase.dll".encode_utf16().chain(Some(0)).collect();
            unsafe { crate::sys::pe::exports(GetModuleHandleW(n.as_ptr()).cast()) }.unwrap().code
        };
        attach_kernelbase(&kb);
        let w = |p: &std::path::Path| p.to_string_lossy().encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        vfs::tests::install(vfs::tests::test_state(&base, 1, false));
        let mine = base.join(r"data\saves\test-game-00000000\player-2\fs\LocalLow\PeerPartyTest\Game");
        // Player 2 has none: a copy of the backup onto it fails as player 1's isn't read.
        let (src, dst) = (w(&real.join("data_full_-99.data")), w(&real.join("data_full_-99.data.bak")));
        let hr = unsafe { CopyFile2(src.as_ptr(), dst.as_ptr(), std::ptr::null()) };
        assert!(hr < 0, "player 1's file isn't player 2's");
        assert!(!unrouted(|| real.join("data_full_-99.data.bak").exists()), "a failed copy made nothing beside player 1's");
        // Its own save, written through CreateFileW, then copied with CopyFile2.
        let h = unsafe { CreateFileW(src.as_ptr(), 0x4000_0000, 0, std::ptr::null(), 2, 0x80, std::ptr::null_mut()) };
        assert_ne!(h as isize, -1);
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
        assert!(unrouted(|| mine.join("data_full_-99.data").exists()), "made in player 2's folder");
        assert!(!unrouted(|| real.join("data_full_-99.data.bak").exists()), "CreateFileW made nothing beside player 1's");
        let hr = unsafe { CopyFile2(src.as_ptr(), dst.as_ptr(), std::ptr::null()) };
        assert!(hr >= 0, "{hr:x}");
        assert!(unrouted(|| mine.join("data_full_-99.data.bak").exists()));
        assert!(!unrouted(|| real.join("data_full_-99.data.bak").exists()), "nothing beside player 1's");
        assert_eq!(unrouted(|| std::fs::read_to_string(real.join("data_full_-99.data")).unwrap()), "player 1");
        // A temporary file renamed onto the save through its handle stays in its folder.
        let tmp = base.join("shared-tmp.dat");
        unrouted(|| std::fs::write(&tmp, "player 2 new").unwrap());
        let ht = unsafe { CreateFileW(w(&tmp).as_ptr(), 0x8000_0000 | 0x1_0000, 7, std::ptr::null(), 3, 0x80, std::ptr::null_mut()) };
        assert_ne!(ht as isize, -1);
        let target: Vec<u16> = real.join("data_full_0.data").to_string_lossy().encode_utf16().collect();
        let head = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let mut info = vec![0u8; head + target.len() * 2 + 2];
        let fi = info.as_mut_ptr() as *mut FILE_RENAME_INFO;
        unsafe {
            (*fi).FileNameLength = (target.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(target.as_ptr(), std::ptr::addr_of_mut!((*fi).FileName) as *mut u16, target.len());
        }
        let ok = unsafe { SetFileInformationByHandle(ht, 3, info.as_ptr().cast(), info.len() as u32) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(ht) };
        assert_ne!(ok, 0, "rename");
        assert_eq!(unrouted(|| std::fs::read_to_string(mine.join("data_full_0.data")).unwrap()), "player 2 new");
        assert!(!unrouted(|| real.join("data_full_0.data").exists()));
        // A listing of the game's folder of saves is player 2's.
        let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let pat = w(&real.join("*.data"));
        let hf = unsafe { FindFirstFileW(pat.as_ptr(), &mut fd) };
        assert_ne!(hf as isize, -1);
        let mut names = vec![String::from_utf16_lossy(&fd.cFileName[..fd.cFileName.iter().position(|c| *c == 0).unwrap()])];
        while unsafe { windows_sys::Win32::Storage::FileSystem::FindNextFileW(hf, &mut fd) } != 0 {
            names.push(String::from_utf16_lossy(&fd.cFileName[..fd.cFileName.iter().position(|c| *c == 0).unwrap()]));
        }
        unsafe { FindClose(hf) };
        names.sort();
        assert_eq!(names, ["data_full_-99.data", "data_full_0.data"]);
        vfs::tests::uninstall();
        let _ = std::fs::remove_dir_all(&base);
    }
}
