//! Live split-screen pane resizing for Source engine Direct3D 9 games.
//!
//! Older Direct3D 9 engines do not recreate their back buffer on `WM_SIZE` or
//! `WM_EXITSIZEMOVE`; the Source engine changes resolution live through its
//! `mat_setvideomode <w> <h> <windowed>` console command, which updates the
//! Direct3D 9 device, offscreen render targets, projection aspect ratio, and
//! VGUI layout together.
//!
//! When a split-screen pane resizes in a process that has `engine.dll` and
//! `shaderapidx9.dll` loaded, this module waits briefly for rapid edits to
//! settle and then runs `mat_setvideomode <w> <h> 1` on the game's render
//! thread (from `d3d9::presenting`). Before calling `ClientCmd` (slot 7), it
//! verifies the `IVEngineClient` vtable by calling `GetScreenSize` (slot 5)
//! and checking it matches the current Direct3D 9 viewport. If `mat_setvideomode`
//! lacks `FCVAR_CLIENTCMD_CAN_EXECUTE`, that flag is temporarily added through
//! `ICvar::FindCommandBase` (after verifying the `ConCommandBase` layout and
//! name) and restored once the viewport updates or times out.

use std::ffi::{CStr, c_char};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, PAGE_READWRITE,
    PAGE_WRITECOPY, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, VirtualQuery,
};

use super::com;
use crate::log;

/// `FCVAR_CLIENTCMD_CAN_EXECUTE` in the Source SDK (`public/tier1/icvar.h`).
const FCVAR_CLIENTCMD_CAN_EXECUTE: u32 = 1 << 30;
/// `IVEngineClient` vtable slots stable across `VEngineClient012`..`014`.
const SLOT_GET_SCREEN_SIZE: usize = 5;
const SLOT_CLIENT_CMD: usize = 7;
/// `ICvar::FindCommandBase(const char *)` slot per interface version.
/// On `VEngineCvar007`, slot 14 is `FindCommandBase` (const or non-const) for
/// both 8-slot and 9-slot `IAppSystem` layouts.
const CVAR_FIND_SLOTS: [(&CStr, usize); 2] = [(c"VEngineCvar007", 14), (c"VEngineCvar004", 10)];
const CLIENT_IFACES: [&CStr; 3] = [c"VEngineClient014", c"VEngineClient013", c"VEngineClient012"];

const SETTLE_DELAY: Duration = Duration::from_millis(150);
const MIN_INTERVAL: Duration = Duration::from_millis(350);
const FLAG_RESTORE_TIMEOUT: Duration = Duration::from_secs(3);

/// Fast-path gate checked on each Direct3D 9 present.
static PENDING: AtomicBool = AtomicBool::new(false);

struct State {
    target: Option<(u32, u32)>,
    due: Instant,
    last_issued: Option<(Instant, (u32, u32))>,
    patched_cmd: Option<(usize, u32, Instant)>,
    disabled: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

type CreateInterfaceFn = unsafe extern "C" fn(*const c_char, *mut i32) -> usize;

/// Calls a 2-out-arg C++ virtual method (`thiscall` on x86, `C` on x64):
/// `void IVEngineClient::GetScreenSize(int &width, int &height)`.
#[cfg(target_arch = "x86")]
unsafe fn call_get_screen_size(iface: usize, w: &mut i32, h: &mut i32) {
    let f: unsafe extern "thiscall" fn(usize, *mut i32, *mut i32) =
        unsafe { std::mem::transmute(com::slot(iface, SLOT_GET_SCREEN_SIZE)) };
    unsafe { f(iface, w, h) };
}

#[cfg(not(target_arch = "x86"))]
unsafe fn call_get_screen_size(iface: usize, w: &mut i32, h: &mut i32) {
    let f: unsafe extern "C" fn(usize, *mut i32, *mut i32) =
        unsafe { std::mem::transmute(com::slot(iface, SLOT_GET_SCREEN_SIZE)) };
    unsafe { f(iface, w, h) };
}

/// Calls `void IVEngineClient::ClientCmd(const char *cmd)`.
#[cfg(target_arch = "x86")]
unsafe fn call_client_cmd(iface: usize, cmd: *const c_char) {
    let f: unsafe extern "thiscall" fn(usize, *const c_char) =
        unsafe { std::mem::transmute(com::slot(iface, SLOT_CLIENT_CMD)) };
    unsafe { f(iface, cmd) };
}

#[cfg(not(target_arch = "x86"))]
unsafe fn call_client_cmd(iface: usize, cmd: *const c_char) {
    let f: unsafe extern "C" fn(usize, *const c_char) =
        unsafe { std::mem::transmute(com::slot(iface, SLOT_CLIENT_CMD)) };
    unsafe { f(iface, cmd) };
}

/// Calls `ConCommandBase *ICvar::FindCommandBase(const char *name)`.
#[cfg(target_arch = "x86")]
unsafe fn call_find_command_base(cvar: usize, slot: usize, name: *const c_char) -> usize {
    let f: unsafe extern "thiscall" fn(usize, *const c_char) -> usize =
        unsafe { std::mem::transmute(com::slot(cvar, slot)) };
    unsafe { f(cvar, name) }
}

#[cfg(not(target_arch = "x86"))]
unsafe fn call_find_command_base(cvar: usize, slot: usize, name: *const c_char) -> usize {
    let f: unsafe extern "C" fn(usize, *const c_char) -> usize =
        unsafe { std::mem::transmute(com::slot(cvar, slot)) };
    unsafe { f(cvar, name) }
}

/// Formats `mat_setvideomode <w> <h> 1\0` (windowed), or `None` if the size is invalid.
fn format_videomode_cmd(w: u32, h: u32) -> Option<String> {
    if w < 320 || h < 200 || w > 16384 || h > 16384 {
        return None;
    }
    Some(format!("mat_setvideomode {w} {h} 1\0"))
}

/// Whether the process has Source engine's Direct3D 9 modules loaded.
fn source_loaded() -> bool {
    !com::module("engine.dll").is_null() && !com::module("shaderapidx9.dll").is_null()
}

/// Schedules a debounced Source live resize to `w`x`h` if Source modules are loaded.
pub(super) fn on_pane_resized(w: u32, h: u32) {
    if format_videomode_cmd(w, h).is_none() || !source_loaded() {
        return;
    }
    let now = Instant::now();
    let mut g = crate::sys::lock(&STATE);
    let s = g.get_or_insert_with(|| State {
        target: None,
        due: now,
        last_issued: None,
        patched_cmd: None,
        disabled: false,
    });
    if s.disabled {
        return;
    }
    s.target = Some((w, h));
    s.due = now + SETTLE_DELAY;
    PENDING.store(true, Ordering::Release);
}

/// Runs on the Direct3D 9 render thread after `Present`.
pub(super) fn tick_present() {
    if !PENDING.load(Ordering::Acquire) {
        return;
    }
    let now = Instant::now();
    let (vw, vh) = super::current_viewport();
    let mut issue: Option<(u32, u32)> = None;
    let mut restore: Option<(usize, u32)> = None;

    {
        let mut g = crate::sys::lock(&STATE);
        let Some(s) = g.as_mut() else {
            PENDING.store(false, Ordering::Release);
            return;
        };
        if let Some((cmd_ptr, orig_flags, deadline)) = s.patched_cmd {
            let reached = s.last_issued.is_some_and(|(_, (lw, lh))| lw == vw && lh == vh);
            if reached || now >= deadline {
                restore = Some((cmd_ptr, orig_flags));
                s.patched_cmd = None;
            }
        }
        if !s.disabled
            && let Some((tw, th)) = s.target
            && now >= s.due
        {
            if (vw, vh) == (tw, th) {
                s.target = None;
            } else if s.last_issued.is_some_and(|(at, _)| now.saturating_duration_since(at) < MIN_INTERVAL) {
                // Wait until MIN_INTERVAL elapses before issuing another reset.
            } else {
                s.target = None;
                s.last_issued = Some((now, (tw, th)));
                issue = Some((tw, th));
            }
        }
        if s.target.is_none() && s.patched_cmd.is_none() && issue.is_none() {
            PENDING.store(false, Ordering::Release);
        }
    }

    if let Some((cmd_ptr, orig_flags)) = restore {
        unsafe { restore_cmd_flags(cmd_ptr, orig_flags) };
    }
    if let Some((tw, th)) = issue {
        let patched = issue_resize(tw, th, vw, vh);
        if let Some((cmd_ptr, orig_flags)) = patched {
            let mut g = crate::sys::lock(&STATE);
            if let Some(s) = g.as_mut() {
                s.patched_cmd = Some((cmd_ptr, orig_flags, Instant::now() + FLAG_RESTORE_TIMEOUT));
                PENDING.store(true, Ordering::Release);
            }
        }
    }
}

fn disable_with(reason: &str) {
    log!("video: {reason}");
    let mut g = crate::sys::lock(&STATE);
    if let Some(s) = g.as_mut() {
        s.disabled = true;
        s.target = None;
        if s.patched_cmd.is_none() {
            PENDING.store(false, Ordering::Release);
        }
    }
}

/// Checks whether `[addr, addr + len)` lies in committed readable (and optionally writable) memory.
fn memory_accessible(addr: usize, len: usize, writable: bool) -> bool {
    if addr < 0x10000 || len == 0 {
        return false;
    }
    let Some(end) = addr.checked_add(len) else { return false };
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let got = unsafe { VirtualQuery(addr as *const _, &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
    if got == 0 || mbi.State != MEM_COMMIT {
        return false;
    }
    let prot = mbi.Protect;
    if prot == 0 || (prot & (PAGE_NOACCESS | PAGE_GUARD)) != 0 {
        return false;
    }
    if writable && (prot & (PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)) == 0 {
        return false;
    }
    let region_end = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize);
    end <= region_end
}

/// `ConCommandBase` layout in Source SDK (`public/tier1/convar.h`):
/// `[vptr, m_pNext, m_bRegistered (+pad), m_pszName, m_pszHelpString, m_nFlags]`.
const CMD_REGISTERED_OFF: usize = 2 * std::mem::size_of::<usize>();
const CMD_NAME_OFF: usize = 3 * std::mem::size_of::<usize>();
const CMD_FLAGS_OFF: usize = 5 * std::mem::size_of::<usize>();
const CMD_STRUCT_MIN_SIZE: usize = CMD_FLAGS_OFF + std::mem::size_of::<u32>();

/// Validates that `cmd_ptr` looks like the registered `mat_setvideomode` `ConCommandBase`.
unsafe fn is_mat_setvideomode_cmd(cmd_ptr: usize) -> bool {
    if !memory_accessible(cmd_ptr, CMD_STRUCT_MIN_SIZE, true) {
        return false;
    }
    let registered = unsafe { *((cmd_ptr + CMD_REGISTERED_OFF) as *const u8) };
    if registered != 1 {
        return false;
    }
    let name_ptr = unsafe { *((cmd_ptr + CMD_NAME_OFF) as *const usize) };
    let expected = b"mat_setvideomode\0";
    if !memory_accessible(name_ptr, expected.len(), false) {
        return false;
    }
    let slice = unsafe { std::slice::from_raw_parts(name_ptr as *const u8, expected.len()) };
    slice == expected
}

/// Temporarily adds `FCVAR_CLIENTCMD_CAN_EXECUTE` to `mat_setvideomode` if needed.
/// Returns `Some((cmd_ptr, orig_flags))` when the flags were changed.
unsafe fn ensure_cmd_executable(cmd_ptr: usize) -> Option<(usize, u32)> {
    if !unsafe { is_mat_setvideomode_cmd(cmd_ptr) } {
        return None;
    }
    let flags_ptr = (cmd_ptr + CMD_FLAGS_OFF) as *mut u32;
    let orig = unsafe { flags_ptr.read() };
    if (orig & FCVAR_CLIENTCMD_CAN_EXECUTE) != 0 {
        return None;
    }
    unsafe { flags_ptr.write(orig | FCVAR_CLIENTCMD_CAN_EXECUTE) };
    Some((cmd_ptr, orig))
}

/// Restores `mat_setvideomode`'s original flags if `cmd_ptr` still points to it.
unsafe fn restore_cmd_flags(cmd_ptr: usize, orig_flags: u32) {
    if unsafe { is_mat_setvideomode_cmd(cmd_ptr) } {
        let flags_ptr = (cmd_ptr + CMD_FLAGS_OFF) as *mut u32;
        unsafe { flags_ptr.write(orig_flags) };
    }
}

/// Finds `mat_setvideomode` via `vstdlib.dll`'s `ICvar` and ensures `ClientCmd` can run it.
fn allow_mat_setvideomode() -> Option<(usize, u32)> {
    let vstdlib = com::module("vstdlib.dll");
    if vstdlib.is_null() {
        return None;
    }
    let create: CreateInterfaceFn = unsafe { std::mem::transmute(com::proc(vstdlib, c"CreateInterface")?) };
    for (name, find_slot) in CVAR_FIND_SLOTS {
        let mut rc = 0i32;
        let cvar = unsafe { create(name.as_ptr(), &mut rc) };
        if cvar == 0 || !memory_accessible(cvar, std::mem::size_of::<usize>(), false) {
            continue;
        }
        let cmd_ptr = unsafe { call_find_command_base(cvar, find_slot, c"mat_setvideomode".as_ptr()) };
        if cmd_ptr == 0 {
            continue;
        }
        if let Some(patched) = unsafe { ensure_cmd_executable(cmd_ptr) } {
            log!("video: temporarily enabled FCVAR_CLIENTCMD_CAN_EXECUTE on mat_setvideomode ({})", name.to_string_lossy());
            return Some(patched);
        }
        return None;
    }
    None
}

/// Self-verifies `IVEngineClient::GetScreenSize` against `(vw, vh)`.
fn screen_size_matches(sw: i32, sh: i32, vw: u32, vh: u32) -> bool {
    sw > 0 && sh > 0 && (sw as u32).abs_diff(vw) <= 2 && (sh as u32).abs_diff(vh) <= 2
}

/// Issues `mat_setvideomode <tw> <th 1>` through `engine.dll`'s `IVEngineClient`.
fn issue_resize(tw: u32, th: u32, vw: u32, vh: u32) -> Option<(usize, u32)> {
    let cmd = format_videomode_cmd(tw, th)?;
    let engine = com::module("engine.dll");
    if engine.is_null() {
        return None;
    }
    let Some(create_addr) = com::proc(engine, c"CreateInterface") else {
        disable_with("Source engine.dll has no CreateInterface export; skipping live resize");
        return None;
    };
    let create: CreateInterfaceFn = unsafe { std::mem::transmute(create_addr) };
    let mut found: Option<(&CStr, usize)> = None;
    for name in CLIENT_IFACES {
        let mut rc = 0i32;
        let iface = unsafe { create(name.as_ptr(), &mut rc) };
        if iface != 0 && memory_accessible(iface, std::mem::size_of::<usize>(), false) {
            found = Some((name, iface));
            break;
        }
    }
    let Some((name, iface)) = found else {
        disable_with("Source engine client interface not found; skipping live resize");
        return None;
    };
    let (mut sw, mut sh) = (0i32, 0i32);
    unsafe { call_get_screen_size(iface, &mut sw, &mut sh) };
    if vw > 0 && vh > 0 && !screen_size_matches(sw, sh, vw, vh) {
        disable_with(&format!(
            "Source {} GetScreenSize ({sw}x{sh}) didn't match Direct3D 9 viewport ({vw}x{vh}); skipping live resize",
            name.to_string_lossy()
        ));
        return None;
    }
    let patched = allow_mat_setvideomode();
    log!(
        "video: asking Source engine ({}) to resize live from {sw}x{sh} to {tw}x{th} (mat_setvideomode)",
        name.to_string_lossy()
    );
    unsafe { call_client_cmd(iface, cmd.as_ptr().cast()) };
    patched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct MockConCommandBase {
        vptr: usize,
        next: usize,
        registered: u8,
        name: *const u8,
        help: *const u8,
        flags: u32,
    }

    #[test]
    fn formats_valid_videomode_commands() {
        assert_eq!(format_videomode_cmd(2409, 1440).as_deref(), Some("mat_setvideomode 2409 1440 1\0"));
        assert_eq!(format_videomode_cmd(1031, 1440).as_deref(), Some("mat_setvideomode 1031 1440 1\0"));
        assert_eq!(format_videomode_cmd(0, 1440), None);
        assert_eq!(format_videomode_cmd(200, 200), None);
        assert_eq!(format_videomode_cmd(20000, 1440), None);
    }

    #[test]
    fn screen_size_verification_allows_tiny_rounding_and_rejects_mismatch() {
        assert!(screen_size_matches(1720, 1440, 1720, 1440));
        assert!(screen_size_matches(1721, 1440, 1720, 1440));
        assert!(!screen_size_matches(1920, 1080, 1720, 1440));
        assert!(!screen_size_matches(0, 0, 1720, 1440));
        assert!(!screen_size_matches(-1, 1440, 1720, 1440));
    }

    #[test]
    fn concommand_base_layout_validates_and_restores_flags() {
        let mut cmd = MockConCommandBase {
            vptr: 0,
            next: 0,
            registered: 1,
            name: b"mat_setvideomode\0".as_ptr(),
            help: b"change video mode\0".as_ptr(),
            flags: 0x0000_0008,
        };
        let ptr = (&mut cmd as *mut MockConCommandBase) as usize;
        assert!(unsafe { is_mat_setvideomode_cmd(ptr) });
        let patched = unsafe { ensure_cmd_executable(ptr) };
        assert_eq!(patched, Some((ptr, 0x0000_0008)));
        assert_eq!(cmd.flags, 0x0000_0008 | FCVAR_CLIENTCMD_CAN_EXECUTE);
        // Second call sees the flag already present and makes no change.
        assert_eq!(unsafe { ensure_cmd_executable(ptr) }, None);
        unsafe { restore_cmd_flags(ptr, 0x0000_0008) };
        assert_eq!(cmd.flags, 0x0000_0008);

        // Wrong command name or unregistered command is rejected.
        cmd.name = b"quit\0".as_ptr();
        let ptr = (&mut cmd as *mut MockConCommandBase) as usize;
        assert!(!unsafe { is_mat_setvideomode_cmd(ptr) });
        assert_eq!(unsafe { ensure_cmd_executable(ptr) }, None);
        cmd.name = b"mat_setvideomode\0".as_ptr();
        cmd.registered = 0;
        let ptr = (&mut cmd as *mut MockConCommandBase) as usize;
        assert!(!unsafe { is_mat_setvideomode_cmd(ptr) });
    }
}
