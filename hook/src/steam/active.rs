//! In-memory recovery when `HKCU\Software\Valve\Steam\ActiveProcess` is stale.
//!
//! Valve's `steam_api` loads `steamclient{,64}.dll` from the path recorded under
//! `HKCU\Software\Valve\Steam\ActiveProcess`. If an earlier process overwrote that key
//! with a client DLL from another folder and exited without restoring it, a Steam game
//! started afterwards loads that leftover DLL instead of the running Steam client's:
//! outbound network calls can be blocked or redirected, and Steam API calls can behave
//! inconsistently or crash on shutdown.
//!
//! When `steam_api` initializes and `ActiveProcess` names a dead process and a DLL
//! outside both the current game's directory tree and the running Steam client's folder,
//! PeerParty answers `RegQueryValueExW`/`RegQueryValueExA` for `ActiveProcess`'s `pid`,
//! `SteamClientDll` and `SteamClientDll64` in memory with the running Steam client's
//! values. The host registry is never modified, and when `ActiveProcess` is healthy or
//! Steam is not running, nothing is hooked.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use retour::GenericDetour;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_SUCCESS, STILL_ACTIVE};
use windows_sys::Win32::Globalization::{CP_ACP, WideCharToMultiByte};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};

use crate::log;
use crate::sys::pe::Export;
use crate::sys::read;

const ACTIVE_PROCESS_SUBKEY: &str = "Software\\Valve\\Steam\\ActiveProcess";
const ACTIVE_PROCESS_SUFFIX: &str = "\\software\\valve\\steam\\activeprocess";

#[cfg(target_pointer_width = "64")]
const ARCH_VALUE: &str = "SteamClientDll64";
#[cfg(not(target_pointer_width = "64"))]
const ARCH_VALUE: &str = "SteamClientDll";

#[cfg(target_pointer_width = "64")]
const ARCH_DLL: &str = "steamclient64.dll";
#[cfg(not(target_pointer_width = "64"))]
const ARCH_DLL: &str = "steamclient.dll";

struct ActiveOverride {
    pid: u32,
    dll32_w: Vec<u16>,
    dll64_w: Vec<u16>,
    dll32_a: Vec<u8>,
    dll64_a: Vec<u8>,
}

static OVERRIDE: OnceLock<Option<ActiveOverride>> = OnceLock::new();

type QueryValueExW = unsafe extern "system" fn(isize, *const u16, *const u32, *mut u32, *mut u8, *mut u32) -> i32;
type QueryValueExA = unsafe extern "system" fn(isize, *const u8, *const u32, *mut u32, *mut u8, *mut u32) -> i32;
type NtQueryKeyFn = unsafe extern "system" fn(isize, u32, *mut u8, u32, *mut u32) -> i32;

static QUERY_W: OnceLock<GenericDetour<QueryValueExW>> = OnceLock::new();
static QUERY_A: OnceLock<GenericDetour<QueryValueExA>> = OnceLock::new();
static NT_QUERY_KEY: OnceLock<Option<NtQueryKeyFn>> = OnceLock::new();

thread_local! {
    static BUSY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Checks once (before `steam_api` initializes) whether `ActiveProcess` points to a dead
/// process and a foreign folder's client DLL while Steam itself is running; if so, hooks
/// `RegQueryValueExW`/`A` in this process to answer with the running Steam client.
pub fn ensure() {
    if !crate::net::enabled() || crate::net::skip("steam") {
        return;
    }
    let _ = OVERRIDE.get_or_init(detect_and_arm);
}

fn detect_and_arm() -> Option<ActiveOverride> {
    let (stale_pid, stale_dll) = read_active_process()?;
    if stale_dll.is_empty() {
        return None;
    }
    if stale_pid != 0 && process_alive(stale_pid) {
        return None;
    }
    let stale_path = PathBuf::from(&stale_dll);
    if stale_path.parent().is_some_and(|d| d.join("steam.exe").is_file()) {
        return None;
    }
    let (steam_pid, steam_dir) = find_running_steam()?;
    let exe_path = std::env::current_exe().ok();
    let api_path = steam_api_path();
    if !is_stale_foreign_path(&stale_path, exe_path.as_deref(), api_path.as_deref(), &steam_dir) {
        return None;
    }
    let dll32 = steam_dir.join("steamclient.dll").to_string_lossy().into_owned();
    let dll64 = steam_dir.join("steamclient64.dll").to_string_lossy().into_owned();
    let real_arch = if cfg!(target_pointer_width = "64") { &dll64 } else { &dll32 };
    let ov = ActiveOverride {
        pid: steam_pid,
        dll32_w: wide_nul(&dll32),
        dll64_w: wide_nul(&dll64),
        dll32_a: ansi_nul(&dll32),
        dll64_a: ansi_nul(&dll64),
    };
    let n = arm_query_hooks();
    if n == 0 {
        log!("steam: HKCU\\{ACTIVE_PROCESS_SUBKEY} points to a dead process (pid {stale_pid}) and {stale_dll}, but registry query hooks couldn't be installed");
        return None;
    }
    log!("steam: HKCU\\{ACTIVE_PROCESS_SUBKEY} points to a dead process (pid {stale_pid}) and another folder's {stale_dll}; answering this game in memory with the running Steam client (pid {steam_pid}, {real_arch})");
    Some(ov)
}

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn ansi_nul(s: &str) -> Vec<u8> {
    let w: Vec<u16> = s.encode_utf16().collect();
    if w.is_empty() {
        return vec![0];
    }
    let n = unsafe { WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, std::ptr::null_mut(), 0, std::ptr::null(), std::ptr::null_mut()) };
    if n <= 0 {
        return s.as_bytes().iter().copied().chain(Some(0)).collect();
    }
    let mut out = vec![0u8; n as usize + 1];
    unsafe { WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, out.as_mut_ptr(), n, std::ptr::null(), std::ptr::null_mut()) };
    out
}

fn read_active_process() -> Option<(u32, String)> {
    let sub = wide_nul(ACTIVE_PROCESS_SUBKEY);
    let mut h: HKEY = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut h) } != 0 || h.is_null() {
        return None;
    }
    let pid = read_reg_dword(h, "pid").unwrap_or(0);
    let dll = read_reg_string(h, ARCH_VALUE)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let other = if cfg!(target_pointer_width = "64") { "SteamClientDll" } else { "SteamClientDll64" };
            read_reg_string(h, other)
        })
        .unwrap_or_default();
    unsafe { RegCloseKey(h) };
    Some((pid, dll))
}

fn read_reg_dword(h: HKEY, name: &str) -> Option<u32> {
    let w = wide_nul(name);
    let (mut kind, mut val, mut len) = (0u32, 0u32, 4u32);
    let rc = unsafe { RegQueryValueExW(h, w.as_ptr(), std::ptr::null(), &mut kind, (&mut val as *mut u32).cast(), &mut len) };
    (rc == 0 && kind == REG_DWORD && len == 4).then_some(val)
}

fn read_reg_string(h: HKEY, name: &str) -> Option<String> {
    let w = wide_nul(name);
    let (mut kind, mut len) = (0u32, 0u32);
    if unsafe { RegQueryValueExW(h, w.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut len) } != 0 || kind != REG_SZ || len < 2 || len > 4096 {
        return None;
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    let rc = unsafe { RegQueryValueExW(h, w.as_ptr(), std::ptr::null(), &mut kind, buf.as_mut_ptr().cast(), &mut len) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut code = 0u32;
    let alive = unsafe { GetExitCodeProcess(h, &mut code) != 0 } && code == STILL_ACTIVE as u32;
    unsafe { CloseHandle(h) };
    alive
}

fn find_running_steam() -> Option<(u32, PathBuf)> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap.is_null() || snap as isize == -1 {
        return None;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut found = None;
    if unsafe { Process32FirstW(snap, &mut entry) } != 0 {
        loop {
            let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if name.eq_ignore_ascii_case("steam.exe")
                && let Some(dir) = process_dir(entry.th32ProcessID)
                && dir.join(ARCH_DLL).is_file()
            {
                found = Some((entry.th32ProcessID, dir));
                break;
            }
            if unsafe { Process32NextW(snap, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snap) };
    found
}

fn process_dir(pid: u32) -> Option<PathBuf> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0 };
    unsafe { CloseHandle(h) };
    if !ok || len == 0 {
        return None;
    }
    let path = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
    path.parent().map(Path::to_path_buf)
}

fn steam_api_path() -> Option<PathBuf> {
    for dll in ["steam_api64.dll", "steam_api.dll"] {
        let w = wide_nul(dll);
        let m = unsafe { GetModuleHandleW(w.as_ptr()) };
        if !m.is_null() {
            let mut buf = [0u16; 1024];
            let len = unsafe { GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32) } as usize;
            if len > 0 {
                return Some(PathBuf::from(String::from_utf16_lossy(&buf[..len.min(buf.len())])));
            }
        }
    }
    None
}

fn norm_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
    let s = s.strip_prefix("\\\\?\\").unwrap_or(&s);
    s.trim_end_matches('\\').to_string()
}

fn same_or_nested(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    a.strip_prefix(b).is_some_and(|rest| rest.starts_with('\\'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('\\'))
}

/// Whether `stale_dll` lies outside both `steam_dir` and the current game's directory tree.
fn is_stale_foreign_path(stale_dll: &Path, exe_path: Option<&Path>, api_path: Option<&Path>, steam_dir: &Path) -> bool {
    let Some(stale_dir) = stale_dll.parent().map(norm_path).filter(|s| !s.is_empty()) else {
        return false;
    };
    let steam_norm = norm_path(steam_dir);
    if stale_dir == steam_norm {
        return false;
    }
    if let Some(dir) = exe_path.and_then(Path::parent).map(norm_path)
        && same_or_nested(&stale_dir, &dir)
    {
        return false;
    }
    if let Some(dir) = api_path.and_then(Path::parent).map(norm_path)
        && same_or_nested(&stale_dir, &dir)
    {
        return false;
    }
    true
}

fn exports_of(dll: &str) -> Vec<Export> {
    let w = wide_nul(dll);
    let base = unsafe { GetModuleHandleW(w.as_ptr()) };
    if base.is_null() {
        return Vec::new();
    }
    unsafe { crate::sys::pe::exports(base.cast()) }.map(|t| t.code).unwrap_or_default()
}

fn arm_query_hooks() -> usize {
    let kb = exports_of("kernelbase.dll");
    let adv = exports_of("advapi32.dll");
    let lists: &[&[Export]] = &[&kb, &adv];
    crate::sys::detour::batch(|| {
        arm_one(lists, "RegQueryValueExW", &QUERY_W, detour_query_w as QueryValueExW)
            + arm_one(lists, "RegQueryValueExA", &QUERY_A, detour_query_a as QueryValueExA)
    })
}

fn arm_one<T>(lists: &[&[Export]], name: &str, slot: &OnceLock<GenericDetour<T>>, detour: T) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    if slot.get().is_some() {
        return 0;
    }
    let Some(e) = lists.iter().flat_map(|l| l.iter()).find(|e| e.name == name) else {
        return 0;
    };
    let target: T = unsafe { T::from_ptr(e.addr) };
    usize::from(crate::sys::detour::try_arm(slot, target, detour, "steam-active", name, true) == crate::sys::detour::Armed::Hooked)
}

fn nt_query_key() -> Option<NtQueryKeyFn> {
    *NT_QUERY_KEY.get_or_init(|| {
        let w = wide_nul("ntdll.dll");
        let ntdll = unsafe { GetModuleHandleW(w.as_ptr()) };
        if ntdll.is_null() {
            return None;
        }
        let f = unsafe { GetProcAddress(ntdll, c"NtQueryKey".as_ptr().cast()) }?;
        Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, NtQueryKeyFn>(f) })
    })
}

fn is_active_process_key(nt_path: &str) -> bool {
    nt_path.trim_end_matches('\\').to_ascii_lowercase().ends_with(ACTIVE_PROCESS_SUFFIX)
}

fn key_is_active_process(key: isize) -> bool {
    let Some(query) = nt_query_key() else {
        return false;
    };
    const KEY_NAME_INFORMATION: u32 = 3;
    let mut buf = [0u8; 512];
    let mut ret_len = 0u32;
    let st = unsafe { query(key, KEY_NAME_INFORMATION, buf.as_mut_ptr(), buf.len() as u32, &mut ret_len) };
    if st != 0 || ret_len < 4 {
        return false;
    }
    let name_bytes = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    let end = (4 + name_bytes).min(buf.len());
    if end <= 4 {
        return false;
    }
    let units: Vec<u16> = buf[4..end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let path = String::from_utf16_lossy(&units);
    is_active_process_key(&path)
}

#[derive(Clone, Copy)]
enum Field {
    Pid,
    Dll32,
    Dll64,
}

fn field_of(name: &str) -> Option<Field> {
    if name.eq_ignore_ascii_case("pid") {
        Some(Field::Pid)
    } else if name.eq_ignore_ascii_case("SteamClientDll") {
        Some(Field::Dll32)
    } else if name.eq_ignore_ascii_case("SteamClientDll64") {
        Some(Field::Dll64)
    } else {
        None
    }
}

unsafe fn write_reg_bytes(kind: u32, bytes: &[u8], lp_type: *mut u32, lp_data: *mut u8, lpcb_data: *mut u32) -> i32 {
    if !lp_type.is_null() {
        unsafe { *lp_type = kind };
    }
    let need = bytes.len() as u32;
    if lp_data.is_null() {
        if !lpcb_data.is_null() {
            unsafe { *lpcb_data = need };
        }
        return ERROR_SUCCESS as i32;
    }
    if lpcb_data.is_null() {
        return ERROR_INVALID_PARAMETER as i32;
    }
    let cap = unsafe { *lpcb_data } as usize;
    unsafe { *lpcb_data = need };
    if cap < bytes.len() {
        return ERROR_MORE_DATA as i32;
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), lp_data, bytes.len()) };
    ERROR_SUCCESS as i32
}

unsafe extern "system" fn detour_query_w(key: isize, name: *const u16, reserved: *const u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    if !name.is_null()
        && !BUSY.with(|b| b.get())
        && let Some(Some(ov)) = OVERRIDE.get()
        && let Some(s) = read::wstr(name as usize)
        && let Some(field) = field_of(&s)
    {
        BUSY.with(|b| b.set(true));
        let matched = key_is_active_process(key);
        BUSY.with(|b| b.set(false));
        if matched {
            return match field {
                Field::Pid => unsafe { write_reg_bytes(REG_DWORD, &ov.pid.to_le_bytes(), kind, data, len) },
                Field::Dll32 => {
                    let bytes = unsafe { std::slice::from_raw_parts(ov.dll32_w.as_ptr().cast::<u8>(), ov.dll32_w.len() * 2) };
                    unsafe { write_reg_bytes(REG_SZ, bytes, kind, data, len) }
                }
                Field::Dll64 => {
                    let bytes = unsafe { std::slice::from_raw_parts(ov.dll64_w.as_ptr().cast::<u8>(), ov.dll64_w.len() * 2) };
                    unsafe { write_reg_bytes(REG_SZ, bytes, kind, data, len) }
                }
            };
        }
    }
    unsafe { QUERY_W.get().unwrap().call(key, name, reserved, kind, data, len) }
}

unsafe extern "system" fn detour_query_a(key: isize, name: *const u8, reserved: *const u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    if !name.is_null()
        && !BUSY.with(|b| b.get())
        && let Some(Some(ov)) = OVERRIDE.get()
        && let Some(s) = read::cstr(name as usize)
        && let Some(field) = field_of(&s)
    {
        BUSY.with(|b| b.set(true));
        let matched = key_is_active_process(key);
        BUSY.with(|b| b.set(false));
        if matched {
            return match field {
                Field::Pid => unsafe { write_reg_bytes(REG_DWORD, &ov.pid.to_le_bytes(), kind, data, len) },
                Field::Dll32 => unsafe { write_reg_bytes(REG_SZ, &ov.dll32_a, kind, data, len) },
                Field::Dll64 => unsafe { write_reg_bytes(REG_SZ, &ov.dll64_a, kind, data, len) },
            };
        }
    }
    unsafe { QUERY_A.get().unwrap().call(key, name, reserved, kind, data, len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_path_detection_keeps_steam_and_same_game_paths() {
        let steam = Path::new(r"C:\Program Files (x86)\Steam");
        let exe = Path::new(r"C:\Program Files (x86)\Steam\steamapps\common\GameA\GameA.exe");
        let api = Path::new(r"C:\Program Files (x86)\Steam\steamapps\common\GameA\GameA_Data\Plugins\x86_64\steam_api64.dll");

        assert!(
            is_stale_foreign_path(Path::new(r"C:\Games\OtherGame\steamclient64.dll"), Some(exe), Some(api), steam),
            "client DLL in another game's folder is foreign"
        );
        assert!(
            !is_stale_foreign_path(Path::new(r"C:\Program Files (x86)\Steam\steamclient64.dll"), Some(exe), Some(api), steam),
            "Steam's own client DLL is never foreign"
        );
        assert!(
            !is_stale_foreign_path(Path::new(r"C:\Games\OtherGame\steamclient64.dll"), Some(Path::new(r"C:\Games\OtherGame\Bin\app.exe")), None, steam),
            "client DLL in a parent directory of this game is not foreign"
        );
        assert!(
            !is_stale_foreign_path(Path::new(r"C:\Program Files (x86)\Steam\steamapps\common\GameA\steamclient64.dll"), Some(exe), Some(api), steam),
            "client DLL in this game's own directory is not foreign"
        );
    }

    #[test]
    fn active_process_key_suffix_matches_nt_paths() {
        assert!(is_active_process_key(r"\REGISTRY\USER\S-1-5-21-123\Software\Valve\Steam\ActiveProcess"));
        assert!(is_active_process_key(r"\registry\user\s-1-5-21-123\software\valve\steam\activeprocess\"));
        assert!(!is_active_process_key(r"\REGISTRY\USER\S-1-5-21-123\Software\Valve\Steam"));
        assert!(!is_active_process_key(r"\REGISTRY\USER\S-1-5-21-123\Software\Other\ActiveProcess"));
    }

    #[test]
    fn write_reg_bytes_follows_regqueryvalueex_contract() {
        let mut kind = 0u32;
        let mut len = 0u32;
        let bytes = 8200u32.to_le_bytes();
        // Size query (null buffer).
        assert_eq!(unsafe { write_reg_bytes(REG_DWORD, &bytes, &mut kind, std::ptr::null_mut(), &mut len) }, 0);
        assert_eq!((kind, len), (REG_DWORD, 4));
        // Too-small buffer -> ERROR_MORE_DATA and updates required length.
        let mut small = [0u8; 2];
        len = 2;
        assert_eq!(unsafe { write_reg_bytes(REG_DWORD, &bytes, &mut kind, small.as_mut_ptr(), &mut len) }, ERROR_MORE_DATA as i32);
        assert_eq!(len, 4);
        // Exact-fit buffer -> ERROR_SUCCESS and copies bytes.
        let mut out = [0u8; 4];
        len = 4;
        assert_eq!(unsafe { write_reg_bytes(REG_DWORD, &bytes, &mut kind, out.as_mut_ptr(), &mut len) }, 0);
        assert_eq!(u32::from_le_bytes(out), 8200);
        assert_eq!(len, 4);
    }
}
