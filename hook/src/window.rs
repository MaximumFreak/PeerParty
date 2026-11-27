//! Per-instance virtual viewport / resolution spoofing and focus spoofing for
//! multi-instance split-screen sessions, driven by `protocol::session::InstanceSpec`
//! (`PEERPARTY_INSTANCE` at start-up and live `HostMsg::Instance` updates).
//!
//! Opt-in: when no `InstanceSpec` asks for a virtual viewport or focus spoofing (normal
//! single-instance play), hooks either stay uninstalled or pass straight through after
//! a single atomic flag check.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Once, OnceLock};

use peerparty_protocol::session::{InstanceSpec, PaneRect};
use retour::GenericDetour;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GWLP_WNDPROC, GetClientRect, GetWindow, GetWindowLongW,
    GetWindowThreadProcessId, IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOZORDER, SetWindowLongW, SetWindowPos, WS_BORDER, WS_CAPTION,
    WS_CHILD, WS_DLGFRAME, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_MAXIMIZEBOX,
    WS_MINIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};

use crate::log;
use crate::sys::pe::Export;
use crate::sys::win::{proc_addr, wide};

fn module(name: &str) -> Option<windows_sys::Win32::Foundation::HMODULE> {
    let m = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(wide(name).as_ptr()) };
    (!m.is_null()).then_some(m)
}

type GetSystemMetricsFn = unsafe extern "system" fn(i32) -> i32;
/// `ChangeDisplaySettingsExW/A(device, mode, hwnd, flags, param)`.
type ChangeDisplayExFn = unsafe extern "system" fn(usize, usize, isize, u32, usize) -> i32;
/// `ChangeDisplaySettingsW/A(mode, flags)`.
type ChangeDisplayFn = unsafe extern "system" fn(usize, u32) -> i32;
type GetMonitorInfoWFn = unsafe extern "system" fn(isize, *mut MonitorInfo) -> i32;
type GetMonitorInfoAFn = unsafe extern "system" fn(isize, *mut MonitorInfo) -> i32;
type EnumDisplaySettingsWFn = unsafe extern "system" fn(*const u16, u32, *mut u8) -> i32;
type EnumDisplaySettingsAFn = unsafe extern "system" fn(*const u8, u32, *mut u8) -> i32;
type GetForegroundWindowFn = unsafe extern "system" fn() -> isize;
type GetActiveWindowFn = unsafe extern "system" fn() -> isize;
type GetFocusFn = unsafe extern "system" fn() -> isize;
type RegisterRawInputDevicesFn = unsafe extern "system" fn(*const RawInputDevice, u32, u32) -> i32;
type TranslateMessageFn = unsafe extern "system" fn(*const Msg) -> i32;
type TrackMouseEventFn = unsafe extern "system" fn(*mut TrackMouse) -> i32;
type DispatchMessageWFn = unsafe extern "system" fn(*const Msg) -> isize;
type CreateWindowExAFn = unsafe extern "system" fn(u32, *const u8, *const u8, u32, i32, i32, i32, i32, isize, isize, isize, *const core::ffi::c_void) -> isize;
type SetWindowPosFn = unsafe extern "system" fn(isize, isize, i32, i32, i32, i32, u32) -> i32;
type MoveWindowFn = unsafe extern "system" fn(isize, i32, i32, i32, i32, i32) -> i32;
type SetWindowLongFn = unsafe extern "system" fn(isize, i32, i32) -> i32;
type SetWindowLongPtrFn = unsafe extern "system" fn(isize, i32, isize) -> isize;
/// `GetWindowLongPtrW/A`, or on x86 `GetWindowLongW/A` (the same call there).
type GetWindowLongPtrFn = unsafe extern "system" fn(isize, i32) -> isize;
type ShowWindowFn = unsafe extern "system" fn(isize, i32) -> i32;
type EnumDisplaySettingsExWFn = unsafe extern "system" fn(*const u16, u32, *mut u8, u32) -> i32;
type EnumDisplaySettingsExAFn = unsafe extern "system" fn(*const u8, u32, *mut u8, u32) -> i32;
type GetSystemMetricsForDpiFn = unsafe extern "system" fn(i32, u32) -> i32;
type EnumDisplayMonitorsFn = unsafe extern "system" fn(isize, *const RECT, Option<unsafe extern "system" fn(isize, isize, *mut RECT, isize) -> i32>, isize) -> i32;
type CreateWindowExWFn = unsafe extern "system" fn(
    u32,
    *const u16,
    *const u16,
    u32,
    i32,
    i32,
    i32,
    i32,
    isize,
    isize,
    isize,
    *const core::ffi::c_void,
) -> isize;

/// Win32 `RAWINPUTDEVICE`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawInputDevice {
    pub usage_page: u16,
    pub usage: u16,
    pub flags: u32,
    pub hwnd_target: isize,
}

const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
const HID_USAGE_GENERIC_MOUSE: u16 = 0x02;
const HID_USAGE_GENERIC_JOYSTICK: u16 = 0x04;
const HID_USAGE_GENERIC_GAMEPAD: u16 = 0x05;
const HID_USAGE_GENERIC_KEYBOARD: u16 = 0x06;
const HID_USAGE_GENERIC_MULTI_AXIS: u16 = 0x08;
const RIDEV_REMOVE: u32 = 0x0000_0001;
const RIDEV_INPUTSINK: u32 = 0x0000_0100;
const RIDEV_CAPTUREMOUSE: u32 = 0x0000_0200;
const RIDEV_EXINPUTSINK: u32 = 0x0000_1000;

static GET_METRICS: OnceLock<GenericDetour<GetSystemMetricsFn>> = OnceLock::new();
static CHANGE_DISPLAY_EX_W: OnceLock<GenericDetour<ChangeDisplayExFn>> = OnceLock::new();
static CHANGE_DISPLAY_EX_A: OnceLock<GenericDetour<ChangeDisplayExFn>> = OnceLock::new();
static CHANGE_DISPLAY_W: OnceLock<GenericDetour<ChangeDisplayFn>> = OnceLock::new();
static CHANGE_DISPLAY_A: OnceLock<GenericDetour<ChangeDisplayFn>> = OnceLock::new();
static TOLD_DISPLAY_MODE: AtomicBool = AtomicBool::new(false);
static MONITOR_INFO_W: OnceLock<GenericDetour<GetMonitorInfoWFn>> = OnceLock::new();
static MONITOR_INFO_A: OnceLock<GenericDetour<GetMonitorInfoAFn>> = OnceLock::new();
static ENUM_DISP_W: OnceLock<GenericDetour<EnumDisplaySettingsWFn>> = OnceLock::new();
static ENUM_DISP_A: OnceLock<GenericDetour<EnumDisplaySettingsAFn>> = OnceLock::new();
static GET_FOREGROUND: OnceLock<GenericDetour<GetForegroundWindowFn>> = OnceLock::new();
static GET_ACTIVE: OnceLock<GenericDetour<GetActiveWindowFn>> = OnceLock::new();
static GET_FOCUS: OnceLock<GenericDetour<GetFocusFn>> = OnceLock::new();
static REGISTER_RAW_INPUT: OnceLock<GenericDetour<RegisterRawInputDevicesFn>> = OnceLock::new();
static CREATE_WINDOW_EX_W: OnceLock<GenericDetour<CreateWindowExWFn>> = OnceLock::new();
static TRANSLATE: OnceLock<GenericDetour<TranslateMessageFn>> = OnceLock::new();
static TRACK_MOUSE: OnceLock<GenericDetour<TrackMouseEventFn>> = OnceLock::new();
static DISPATCH_W: OnceLock<GenericDetour<DispatchMessageWFn>> = OnceLock::new();
static CREATE_WINDOW_EX_A: OnceLock<GenericDetour<CreateWindowExAFn>> = OnceLock::new();
static SET_WINDOW_POS: OnceLock<GenericDetour<SetWindowPosFn>> = OnceLock::new();
static MOVE_WINDOW: OnceLock<GenericDetour<MoveWindowFn>> = OnceLock::new();
static SET_LONG_W: OnceLock<GenericDetour<SetWindowLongFn>> = OnceLock::new();
static SET_LONG_A: OnceLock<GenericDetour<SetWindowLongFn>> = OnceLock::new();
static SET_LONG_PTR_W: OnceLock<GenericDetour<SetWindowLongPtrFn>> = OnceLock::new();
static SET_LONG_PTR_A: OnceLock<GenericDetour<SetWindowLongPtrFn>> = OnceLock::new();
static GET_LONG_PTR_W: OnceLock<GenericDetour<GetWindowLongPtrFn>> = OnceLock::new();
static GET_LONG_PTR_A: OnceLock<GenericDetour<GetWindowLongPtrFn>> = OnceLock::new();
static SHOW_WINDOW: OnceLock<GenericDetour<ShowWindowFn>> = OnceLock::new();
static ENUM_DISP_EX_W: OnceLock<GenericDetour<EnumDisplaySettingsExWFn>> = OnceLock::new();
static ENUM_DISP_EX_A: OnceLock<GenericDetour<EnumDisplaySettingsExAFn>> = OnceLock::new();
static GET_METRICS_DPI: OnceLock<GenericDetour<GetSystemMetricsForDpiFn>> = OnceLock::new();
static ENUM_MONITORS: OnceLock<GenericDetour<EnumDisplayMonitorsFn>> = OnceLock::new();

/// Win32 `TRACKMOUSEEVENT`.
#[repr(C)]
pub struct TrackMouse {
    size: u32,
    flags: u32,
    hwnd: isize,
    hover_time: u32,
}

/// Win32 `MSG` (the fields `TranslateMessage` is judged by come first).
#[repr(C)]
pub struct Msg {
    hwnd: isize,
    message: u32,
    wparam: usize,
    lparam: isize,
}
static RAW_SINKS_UPGRADED: AtomicBool = AtomicBool::new(false);

/// Window chrome bits stripped when a split-screen copy runs inside a compositor pane.
const CHROME_STYLES: u32 =
    WS_CAPTION | WS_THICKFRAME | WS_BORDER | WS_DLGFRAME | WS_MAXIMIZEBOX | WS_MINIMIZEBOX | WS_SYSMENU;

static INSTALL: Once = Once::new();
static VIEWPORT_ACTIVE: AtomicBool = AtomicBool::new(false);
static FOCUS_SPOOF: AtomicBool = AtomicBool::new(false);
static PRIMARY_HWND: AtomicUsize = AtomicUsize::new(0);
static SPEC: Mutex<Option<InstanceSpec>> = Mutex::new(None);

/// `MONITORINFO` prefix (identical for `MONITORINFO` and `MONITORINFOEXW/A`).
#[repr(C)]
struct MonitorInfo {
    cb_size: u32,
    rc_monitor: RECT,
    rc_work: RECT,
    dw_flags: u32,
}

const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const SM_CXFULLSCREEN: i32 = 16;
const SM_CYFULLSCREEN: i32 = 17;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

const ENUM_CURRENT_SETTINGS: u32 = u32::MAX;
const ENUM_REGISTRY_SETTINGS: u32 = u32::MAX - 1;

/// Byte offsets of `dmPelsWidth` and `dmPelsHeight` in `DEVMODEW` and `DEVMODEA`.
const DEVMODEW_PELS_WIDTH: usize = 172;
const DEVMODEW_PELS_HEIGHT: usize = 176;
const DEVMODEA_PELS_WIDTH: usize = 108;
const DEVMODEA_PELS_HEIGHT: usize = 112;

/// Win32 focus-loss messages suppressed while `focus_spoof` is enabled.
pub const WM_ACTIVATE: u32 = 0x0006;
pub const WM_KILLFOCUS: u32 = 0x0008;
pub const WM_ACTIVATEAPP: u32 = 0x001C;
pub const WM_NCACTIVATE: u32 = 0x0086;

/// Loads `PEERPARTY_INSTANCE` (if set) at start-up.
pub fn init_from_env() {
    if let Some(spec) = InstanceSpec::from_env() {
        // Its player is the session's slot for it, claimed when the engine starts.
        peerparty_core::want_slot(spec.slot);
        if let Some(name) = spec.identity_override.as_deref() {
            peerparty_core::want_name(name);
        }
        // A later player's saves are its own from the game's first look at them, not
        // from when its LAN comes up (saves.rs).
        if spec.focus_spoof && crate::config::HookConfig::global().player_slot.is_none() {
            crate::saves::own_slot_from_launcher(spec.slot, "a split-screen session's");
        }
        apply_spec(spec);
    }
    // The player the launcher chose: a session's copy, `--player N`, a numbered
    // `--multi-instance` copy.
    if let Some(slot) = crate::config::HookConfig::global().player_slot {
        peerparty_core::want_slot(slot);
        crate::saves::own_slot_from_launcher(slot, "the launcher's");
    }
}

/// This copy's player in a split-screen session of more than one copy (0 for the
/// first); None alone.
pub fn session_slot() -> Option<u32> {
    if !FOCUS_SPOOF.load(Ordering::Relaxed) {
        return None;
    }
    crate::sys::lock(&SPEC).as_ref().map(|s| s.slot)
}

/// Whether this copy is told it has the focus while another has it (a split-screen
/// session's copy): input it reads should keep coming in the background too.
pub fn focus_spoofed() -> bool {
    FOCUS_SPOOF.load(Ordering::Relaxed)
}

/// Whether this copy's `InstanceSpec` says the session's window draws its frames itself
/// (video/capture.rs).
pub fn compositor_capture() -> bool {
    crate::sys::lock(&SPEC).as_ref().is_some_and(|s| s.compositor_capture)
}

/// Whether this process was given an `InstanceSpec` requesting viewport or focus hooks.
pub fn wants_hooks() -> bool {
    VIEWPORT_ACTIVE.load(Ordering::Relaxed) || FOCUS_SPOOF.load(Ordering::Relaxed)
}

/// Current virtual viewport size `(width, height)`, if active.
pub fn virtual_size() -> Option<(u32, u32)> {
    if !VIEWPORT_ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let guard = crate::sys::lock(&SPEC);
    guard.as_ref().and_then(viewport_of)
}

/// Pure extraction of virtual `(width, height)` from an `InstanceSpec`: explicit
/// `virtual_width`/`virtual_height` win over `pane`'s dimensions.
pub fn viewport_of(spec: &InstanceSpec) -> Option<(u32, u32)> {
    let w = spec.virtual_width.or_else(|| spec.pane.map(|p| p.width))?;
    let h = spec.virtual_height.or_else(|| spec.pane.map(|p| p.height))?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Records the game's primary rendering window (called from `video` on Present or on
/// window creation). When split-screen hooks are active, upgrades any already-registered
/// keyboard, mouse and controller Raw Input devices to `RIDEV_INPUTSINK` so background
/// panes keep receiving `WM_INPUT` (gated per pane by `input::raw_input_data`).
pub fn note_game_window(hwnd: usize) {
    if hwnd != 0 {
        let prev = PRIMARY_HWND.swap(hwnd, Ordering::Relaxed);
        if prev != hwnd && prev != 0 {
            log!("window: the game draws in window 0x{hwnd:x} now (0x{prev:x} before)");
        }
        if wants_hooks() && (prev == 0 || !RAW_SINKS_UPGRADED.load(Ordering::Relaxed)) {
            upgrade_registered_raw_input_sinks(hwnd as isize);
        }
        if prev != hwnd && FOCUS_SPOOF.load(Ordering::Relaxed) {
            keep_focus(hwnd);
        }
    }
}

/// A game window whose window procedure is wrapped (`focus_proc`): the window, the
/// procedure it had, and whether it's a Unicode window (an ANSI one's procedure is
/// set and called through the ANSI calls, or its text messages come garbled).
#[derive(Clone, Copy)]
struct Wrapped {
    hwnd: usize,
    old: usize,
    unicode: bool,
}

/// Every wrapped window, until it's destroyed (`WM_NCDESTROY`).
static WRAPPED: Mutex<Vec<Wrapped>> = Mutex::new(Vec::new());

/// More wrapped windows than this is a game making and dropping windows: the rest are
/// left alone.
const MAX_WRAPPED: usize = 64;

/// Wraps game window `hwnd`'s procedure so the focus losses Windows tells it about don't
/// reach it (`should_suppress_focus_loss`). The session's window takes the focus from
/// every copy; a copy told it lost it stops its menus taking input (Unity's UI ignores
/// the mouse and controllers then), though the game itself goes on.
fn keep_focus(hwnd: usize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowUnicode;
    let h = hwnd as HWND;
    let mut w = crate::sys::lock(&WRAPPED);
    if w.iter().any(|x| x.hwnd == hwnd) {
        return;
    }
    if w.len() >= MAX_WRAPPED {
        crate::log::throttled("window: wrapped full", 1, |_| log!("window: {MAX_WRAPPED} game windows wrapped already: window 0x{hwnd:x} is told of its focus losses"));
        return;
    }
    let unicode = unsafe { IsWindowUnicode(h) } != 0;
    // The procedure is known before the window can call the wrapper.
    let old = unsafe { real_proc(h, unicode) };
    if old == 0 {
        return;
    }
    w.push(Wrapped { hwnd, old: old as usize, unicode });
    drop(w);
    let was = unsafe { set_real_proc(h, unicode, wrapper()) };
    let mut w = crate::sys::lock(&WRAPPED);
    if was == 0 {
        w.retain(|x| x.hwnd != hwnd);
        return;
    }
    if let Some(x) = w.iter_mut().find(|x| x.hwnd == hwnd) {
        x.old = was as usize;
    }
    log!("window: the game's window (0x{hwnd:x}, {}) isn't told when it loses the focus (split screen: every copy keeps it)", if unicode { "Unicode" } else { "ANSI" });
    tell_active_after_wrap(hwnd);
}

/// A window wrapped while it isn't in front may have lost the focus before the wrapper
/// was there (the session's window takes the front as the copy starts, often after the
/// session's own "you're active" went in), and an engine that keeps its own active flag
/// from those messages (XNA's `Game.IsActive`, set only by `WM_ACTIVATEAPP`) then ignores
/// its input for good. Told it's active once more: every loss from now on stops at the
/// wrapper, so this is the last word.
fn tell_active_after_wrap(hwnd: usize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    if !FOCUS_SPOOF.load(Ordering::Relaxed) {
        return;
    }
    let front = GET_FOREGROUND.get().map_or_else(|| unsafe { GetForegroundWindow() } as isize, |h| unsafe { h.call() }) as usize;
    let Some(posted) = post_active_unless_front(hwnd, front) else { return };
    log!(
        "window: the game's window 0x{hwnd:x} isn't in front as it's wrapped (0x{front:x} is): {} (a focus loss before the wrapper may have reached it)",
        if posted { "told it's active again" } else { "telling it it's active failed" }
    );
}

/// Posts window `hwnd` the gain of the focus (what the session's window posts a pane it
/// activates) unless it's the window in front (`front`). None when it is; else whether
/// every message went.
fn post_active_unless_front(hwnd: usize, front: usize) -> Option<bool> {
    use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;
    if front == hwnd {
        return None;
    }
    let h = hwnd as HWND;
    Some(unsafe { PostMessageW(h, WM_ACTIVATEAPP, 1, 0) != 0 && PostMessageW(h, WM_ACTIVATE, 1, 0) != 0 && PostMessageW(h, WM_SETFOCUS, 0, 0) != 0 })
}

#[cfg(target_pointer_width = "64")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrA as get_window_long_ptr_a, GetWindowLongPtrW as get_window_long_ptr_w, SetWindowLongPtrA as set_window_long_ptr_a, SetWindowLongPtrW as set_window_long_ptr_w};
#[cfg(target_pointer_width = "32")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongA as get_window_long_ptr_a, GetWindowLongW as get_window_long_ptr_w, SetWindowLongA as set_window_long_ptr_a, SetWindowLongW as set_window_long_ptr_w};

type Proc = unsafe extern "system" fn(HWND, u32, usize, isize) -> isize;

fn wrapper() -> isize {
    focus_proc as Proc as usize as isize
}

/// Window `h`'s procedure as Windows has it, past PeerParty's hooks on the calls.
unsafe fn real_proc(h: HWND, unicode: bool) -> isize {
    match (if unicode { &GET_LONG_PTR_W } else { &GET_LONG_PTR_A }).get() {
        Some(d) => unsafe { d.call(h as isize, GWLP_WNDPROC) },
        None => unsafe { (if unicode { get_window_long_ptr_w(h, GWLP_WNDPROC) } else { get_window_long_ptr_a(h, GWLP_WNDPROC) }) as isize },
    }
}

/// Sets window `h`'s procedure past PeerParty's hooks on the call; the one it had.
unsafe fn set_real_proc(h: HWND, unicode: bool, v: isize) -> isize {
    #[cfg(target_pointer_width = "64")]
    let hook = (if unicode { &SET_LONG_PTR_W } else { &SET_LONG_PTR_A }).get().map(|d| unsafe { d.call(h as isize, GWLP_WNDPROC, v) });
    #[cfg(target_pointer_width = "32")]
    let hook = (if unicode { &SET_LONG_W } else { &SET_LONG_A }).get().map(|d| unsafe { d.call(h as isize, GWLP_WNDPROC, v as i32) as isize });
    hook.unwrap_or_else(|| unsafe { (if unicode { set_window_long_ptr_w(h, GWLP_WNDPROC, v as _) } else { set_window_long_ptr_a(h, GWLP_WNDPROC, v as _) }) as isize })
}

/// What the game reads as wrapped window `h`'s procedure (`unicode`: through the W call):
/// the one the wrapper calls, as if PeerParty's weren't there; None for Windows' answer.
/// A game that sees the wrapper and sets its own procedure in front, keeping the wrapper
/// to call on, makes a loop: the wrapper calls the window's first procedure, which is the
/// game's again (SDL2 subclasses so a window whose procedure isn't its own after
/// CreateWindowEx), until the stack runs out.
fn proc_seen_by_game(h: isize, unicode: bool) -> Option<isize> {
    let x = crate::sys::lock(&WRAPPED).iter().find(|x| x.hwnd == h as usize).copied()?;
    if x.unicode != unicode {
        unwrap_window(x, "read");
        return None;
    }
    Some(x.old as isize)
}

/// The game setting wrapped window `h`'s procedure to `v`: the wrapper stays in front and
/// calls `v` from now on; the procedure the game had read before (None: Windows sets it).
fn proc_set_by_game(h: isize, unicode: bool, v: isize) -> Option<isize> {
    let x = crate::sys::lock(&WRAPPED).iter().find(|x| x.hwnd == h as usize).copied()?;
    if x.unicode != unicode {
        unwrap_window(x, "set");
        return None;
    }
    // Something set its procedure past these hooks: the wrapper isn't in front to stay.
    if unsafe { real_proc(h as HWND, unicode) } != wrapper() {
        crate::log::throttled("window: wrapper not in front", 1, |_| {
            log!("window: the game sets window 0x{h:x}'s procedure while PeerParty's focus wrapper isn't the one in front: set as asked");
        });
        return None;
    }
    if v == wrapper() {
        return Some(x.old as isize);
    }
    let mut w = crate::sys::lock(&WRAPPED);
    let e = w.iter_mut().find(|e| e.hwnd == h as usize)?;
    let prev = std::mem::replace(&mut e.old, v as usize);
    drop(w);
    crate::log::throttled("window: game set proc", 3, |_| {
        log!("window: the game set window 0x{h:x}'s procedure (0x{prev:x} -> 0x{v:x}); PeerParty's focus wrapper stays in front and calls it");
    });
    Some(prev as isize)
}

/// Puts wrapped window `x` back to the procedure it had: the game works with it through
/// the other character set's calls, whose answers (Windows' conversion handles) the
/// wrapper can't stand in for.
fn unwrap_window(x: Wrapped, what: &str) {
    let h = x.hwnd as HWND;
    let front = unsafe { real_proc(h, x.unicode) } == wrapper();
    if front {
        unsafe { set_real_proc(h, x.unicode, x.old as isize) };
        crate::sys::lock(&WRAPPED).retain(|e| e.hwnd != x.hwnd);
    }
    log!(
        "window: the game {what} window 0x{:x}'s procedure through the {} calls: {}",
        x.hwnd,
        if x.unicode { "ANSI" } else { "Unicode" },
        if front { "PeerParty's focus wrapper came off it, so the window is told when it loses the focus" } else { "PeerParty's focus wrapper isn't in front, so it stays" }
    );
}

thread_local! {
    /// The message (window, message, wparam, lparam) this thread's innermost `focus_proc`
    /// is handing to the procedure it wraps.
    static FORWARDING: std::cell::Cell<(usize, u32, usize, isize)> = const { std::cell::Cell::new((0, 0, 0, 0)) };
    /// Inside `focus_proc`'s synthetic `WM_SETCURSOR` dispatch, so nested messages don't
    /// re-enter it.
    static IN_SYNTH_SETCURSOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const WM_DESTROY: u32 = 0x0002;
const WM_SETFOCUS: u32 = 0x0007;
const WM_SETCURSOR: u32 = 0x0020;
const WM_NCDESTROY: u32 = 0x0082;
const WM_MOUSEMOVE: u32 = 0x0200;
const HTCLIENT: usize = 1;

#[cfg(test)]
pub(crate) fn wrap_window_for_test(hwnd: usize) {
    keep_focus(hwnd);
}

#[cfg(test)]
pub(crate) fn is_wrapped_for_test(hwnd: usize) -> bool {
    crate::sys::lock(&WRAPPED).iter().any(|x| x.hwnd == hwnd)
}

unsafe extern "system" fn focus_proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallWindowProcA, CallWindowProcW, DefWindowProcA, DefWindowProcW, WNDPROC};
    let me = crate::sys::lock(&WRAPPED).iter().find(|x| x.hwnd == h as usize).copied();
    let (old, unicode) = me.map_or((0, true), |x| (x.old, x.unicode));
    // The procedure it wraps handing the same message back: one that holds the wrapper as
    // its own previous procedure (set past PeerParty's hooks). Windows' default answers
    // it, where calling on would go round until the stack runs out.
    if FORWARDING.with(|f| f.get()) == (h as usize, m, w, l) {
        crate::log::throttled("window: proc loop", 1, |_| {
            log!("window: window 0x{:x}'s procedure handed message 0x{m:x} back to PeerParty's focus wrapper that gave it; Windows' default answers it (the game's procedure holds the wrapper as its previous one)", h as usize);
        });
        return unsafe { if unicode { DefWindowProcW(h, m, w, l) } else { DefWindowProcA(h, m, w, l) } };
    }
    if should_suppress_focus_loss(m, w, FOCUS_SPOOF.load(Ordering::Relaxed)) {
        static TOLD: AtomicBool = AtomicBool::new(false);
        if !TOLD.swap(true, Ordering::Relaxed) {
            log!("window: a focus loss (message 0x{m:x}) didn't reach the game");
        }
        // WM_NCACTIVATE's answer lets the frame go inactive: TRUE, as if drawn.
        return isize::from(m == WM_NCACTIVATE);
    }
    let call = |m: u32, w: usize, l: isize| -> isize {
        if old == 0 {
            return unsafe { if unicode { DefWindowProcW(h, m, w, l) } else { DefWindowProcA(h, m, w, l) } };
        }
        let proc: WNDPROC = unsafe { std::mem::transmute::<usize, WNDPROC>(old) };
        let outer = FORWARDING.with(|f| f.replace((h as usize, m, w, l)));
        let rc = unsafe { if unicode { CallWindowProcW(proc, h, m, w, l) } else { CallWindowProcA(proc, h, m, w, l) } };
        FORWARDING.with(|f| f.set(outer));
        rc
    };
    let mut l = l;
    let spoof = FOCUS_SPOOF.load(Ordering::Relaxed);
    if spoof && m == WM_MOUSEMOVE {
        l = crate::input::on_forwarded_mouse_move(h as isize, l);
    }
    // The game's window sits behind the session's, so Windows never sends it WM_SETCURSOR.
    // Synthesize it right before forwarded mouse moves and button presses (just as Windows
    // does), so engines that set their cursor in WM_SETCURSOR or via WNDCLASS.hCursor
    // update the session's hardware pointer immediately.
    if spoof
        && matches!(m, WM_MOUSEMOVE | 0x0201 | 0x0203 | 0x0204 | 0x0206 | 0x0207 | 0x0209 | 0x020B | 0x020D)
        && !IN_SYNTH_SETCURSOR.with(|f| f.replace(true))
    {
        crate::input::begin_synth_setcursor();
        let sc_l = (HTCLIENT | ((m as usize & 0xffff) << 16)) as isize;
        let _ = call(WM_SETCURSOR, h as usize, sc_l);
        let called_set_cursor = crate::input::end_synth_setcursor();
        IN_SYNTH_SETCURSOR.with(|f| f.set(false));
        if !called_set_cursor {
            let cur = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursor() } as isize;
            if cur != 0 {
                crate::input::observe_thread_cursor(cur);
            } else {
                crate::log::throttled("window: setcursor none", 1, |_| {
                    log!("window: the game's window 0x{:x} answered WM_SETCURSOR without setting a cursor, and its thread has none: the session's pointer is left as it is over its pane", h as usize);
                });
            }
        }
    }
    // A game that took the mouse as pointer messages gets the forwarded mouse as those
    // (pointer.rs).
    if let Some(msgs) = crate::pointer::convert(h, m, w, l) {
        let mut rc = 0;
        for (m, w, l, p) in msgs {
            crate::pointer::show(p);
            rc = call(m, w, l);
        }
        return rc;
    }
    if m == WM_DESTROY || m == WM_NCDESTROY {
        // Release PeerParty's swap chain COM refs before the game's destroy handler tears
        // down its graphics device.
        crate::video::window_destroyed(h as usize);
    }
    if spoof && m == WM_ACTIVATEAPP && w != 0 {
        crate::log::throttled("window: activateapp", 1, |_| {
            log!("window: the game's window 0x{:x} is told it's the active program (WM_ACTIVATEAPP)", h as usize);
        });
    }
    let _focus_gain = (spoof && ((m == WM_ACTIVATE && (w & 0xffff) != 0) || m == WM_SETFOCUS)).then(crate::input::suppress_focus_click_buttons);
    let rc = call(m, w, l);
    if m == WM_NCDESTROY {
        // The window's last message: its entry goes (a later window may get its handle).
        crate::sys::lock(&WRAPPED).retain(|x| x.hwnd != h as usize);
        let _ = PRIMARY_HWND.compare_exchange(h as usize, 0, Ordering::Relaxed, Ordering::Relaxed);
        crate::video::window_destroyed(h as usize);
    }
    rc
}

/// Pure helper: adds `RIDEV_INPUTSINK` to keyboard, mouse and controller (joystick,
/// gamepad, multi-axis) raw input registrations in a split-screen session so engines
/// using Raw Input (such as Unity's New Input System) still receive `WM_INPUT` while
/// the PeerParty compositor holds Win32 foreground.
pub fn upgrade_raw_input_device(dev: &mut RawInputDevice, fallback_hwnd: isize) -> bool {
    if dev.usage_page != HID_USAGE_PAGE_GENERIC
        || !matches!(
            dev.usage,
            HID_USAGE_GENERIC_MOUSE | HID_USAGE_GENERIC_KEYBOARD | HID_USAGE_GENERIC_JOYSTICK | HID_USAGE_GENERIC_GAMEPAD | HID_USAGE_GENERIC_MULTI_AXIS
        )
        || (dev.flags & RIDEV_REMOVE) != 0
    {
        return false;
    }
    if dev.hwnd_target == 0 && fallback_hwnd != 0 {
        dev.hwnd_target = fallback_hwnd;
    }
    if dev.hwnd_target == 0 || (dev.flags & RIDEV_INPUTSINK) != 0 {
        return false;
    }
    // Windows refuses INPUTSINK beside these (the whole registration fails, and the game
    // gets no raw input at all): capturing the mouse needs the foreground anyway, and
    // EXINPUTSINK is the weaker form of the same.
    dev.flags &= !(RIDEV_CAPTUREMOUSE | RIDEV_EXINPUTSINK);
    dev.flags |= RIDEV_INPUTSINK;
    true
}

/// `PEERPARTY_SKIP=input` takes the input layer out: background Raw Input with it, which
/// without the layer's filtering hands every pane every player's keys and controllers.
fn sinks_skipped() -> bool {
    let skipped = crate::net::skip("input") || crate::net::skip("padblock");
    if skipped {
        crate::log::throttled("window: sinks skipped", 1, |_| {
            log!("window: PEERPARTY_SKIP=input: Raw Input stays foreground-only (no RIDEV_INPUTSINK)");
        });
    }
    skipped
}

fn upgrade_registered_raw_input_sinks(fallback_hwnd: isize) {
    if sinks_skipped() {
        return;
    }
    let Some(u32_mod) = module("user32.dll") else { return };
    let Some(get_addr) = proc_addr(u32_mod, c"GetRegisteredRawInputDevices") else { return };
    type GetRegisteredFn = unsafe extern "system" fn(*mut RawInputDevice, *mut u32, u32) -> u32;
    let get_registered: GetRegisteredFn = unsafe { std::mem::transmute(get_addr) };
    let cb = std::mem::size_of::<RawInputDevice>() as u32;
    let mut count = 0u32;
    let rc = unsafe { get_registered(std::ptr::null_mut(), &mut count, cb) };
    if rc == u32::MAX || count == 0 {
        return;
    }
    let mut devs = vec![RawInputDevice { usage_page: 0, usage: 0, flags: 0, hwnd_target: 0 }; count as usize];
    let got = unsafe { get_registered(devs.as_mut_ptr(), &mut count, cb) };
    if got == u32::MAX || got == 0 {
        return;
    }
    devs.truncate(got as usize);
    let mut changed = Vec::new();
    for mut d in devs {
        if upgrade_raw_input_device(&mut d, fallback_hwnd) {
            changed.push(d);
        }
    }
    if changed.is_empty() {
        RAW_SINKS_UPGRADED.store(true, Ordering::Relaxed);
        return;
    }
    let ok = if let Some(h) = REGISTER_RAW_INPUT.get() {
        unsafe { h.call(changed.as_ptr(), changed.len() as u32, cb) }
    } else if let Some(reg_addr) = proc_addr(u32_mod, c"RegisterRawInputDevices") {
        let reg: RegisterRawInputDevicesFn = unsafe { std::mem::transmute(reg_addr) };
        unsafe { reg(changed.as_ptr(), changed.len() as u32, cb) }
    } else {
        0
    };
    if ok != 0 {
        RAW_SINKS_UPGRADED.store(true, Ordering::Relaxed);
        log!("window: enabled RIDEV_INPUTSINK on {} raw input device(s)", changed.len());
    }
}

/// Applies a new or updated `InstanceSpec` (from env or live `HostMsg::Instance`),
/// installing user32 detours lazily if needed and repositioning the game window when a
/// pane is specified.
pub fn update_spec(spec: InstanceSpec) {
    let pane = spec.pane;
    let slot = spec.slot;
    let had = wants_hooks();
    let before = virtual_size();
    apply_spec(spec);
    if let (Some(_), Some((w, h))) = (before, virtual_size())
        && before != Some((w, h))
    {
        crate::video::pane_resized(w, h);
    }
    if wants_hooks() {
        install_loaded_user32();
        if !had {
            attach_late(slot);
        }
    }
    if let Some(pane) = pane {
        reposition_window(pane);
    }
}

/// A spec that turned split screen on arrived while the game runs (not at its start):
/// what start-up would have done with it is done now, for what's already there.
fn attach_late(slot: u32) {
    peerparty_core::want_slot(slot);
    let primary = PRIMARY_HWND.load(Ordering::Relaxed);
    if primary != 0 {
        upgrade_registered_raw_input_sinks(primary as isize);
        if FOCUS_SPOOF.load(Ordering::Relaxed) {
            keep_focus(primary);
        }
    }
    if virtual_size().is_some()
        && let Some(kb) = module("kernelbase.dll")
        && let Ok(table) = unsafe { crate::sys::pe::exports(kb.cast()) }
    {
        let n = crate::screen_prefs::attach_kernelbase(&table.code);
        log!("screen kernelbase hooked {n} (split screen began while the game ran)");
    }
    if module("mmdevapi.dll").is_some() {
        crate::input::audio_loaded();
    }
    log!("window: split screen began while the game ran: its window, raw input, saved screen size and audio were taken up now");
}

fn apply_spec(spec: InstanceSpec) {
    let vp = viewport_of(&spec);
    let spoof = spec.focus_spoof;
    let id = spec.id.0;
    let slot = spec.slot;
    *crate::sys::lock(&SPEC) = Some(spec);
    VIEWPORT_ACTIVE.store(vp.is_some(), Ordering::Release);
    FOCUS_SPOOF.store(spoof, Ordering::Release);
    log!(
        "window: instance {id} (slot {slot}) viewport={vp:?} focus_spoof={spoof}"
    );
}

/// Whether this copy is shown in a split-screen pane: it stays windowed, and the
/// monitor's mode is the session's, not the game's (video/fullscreen.rs).
pub fn in_pane() -> bool {
    current_pane().is_some()
}

/// A display mode change while this copy is in a pane, or one of several copies sharing
/// the screen (between panes too): it would change the monitor under every pane, so it
/// is answered as done and nothing changes. `None` passes it on.
fn keep_display_mode(mode: usize) -> Option<i32> {
    let pane = in_pane();
    if !pane && !FOCUS_SPOOF.load(Ordering::Relaxed) {
        return None;
    }
    if mode != 0 && !TOLD_DISPLAY_MODE.swap(true, Ordering::Relaxed) {
        log!(
            "window: the game changes the display mode (fullscreen): left as it is, as this copy {}",
            if pane { "is in a pane" } else { "shares the screen with other copies (it has no pane right now)" }
        );
    }
    // DISP_CHANGE_SUCCESSFUL.
    Some(0)
}

/// The screen size the user32 screen calls answer (`virtual_size`), unless a graphics
/// driver asks: it gets the real one. A driver told the pane's size finds the game's
/// borderless window covering "the whole monitor" and takes the display for it as
/// fullscreen, drawing straight to the screen (zoomed, over every window including the
/// session's and the Start menu, and unseen by screen captures).
pub(crate) fn screen_for_caller() -> Option<(u32, u32)> {
    let size = virtual_size()?;
    (!from_graphics_driver()).then_some(size)
}

/// What a module calling the screen calls is: Windows' own and PeerParty's are looked
/// past, to the module that called them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Caller {
    Passed,
    Driver,
    Other,
}

/// Modules looked at by `from_graphics_driver`, by base.
static CALLERS: Mutex<Vec<(usize, Caller)>> = Mutex::new(Vec::new());

/// Whether the call being answered comes from a graphics driver's module (one installed
/// in Windows' driver store, such as the OpenGL driver a game loads through
/// opengl32.dll), past PeerParty's and Windows' own modules on the stack.
#[inline(never)]
fn from_graphics_driver() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW};
    let mut frames = [std::ptr::null_mut(); 12];
    let n = unsafe { windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace(1, frames.len() as u32, frames.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    let owner = |addr: usize| {
        let mut m = std::ptr::null_mut();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        (unsafe { GetModuleHandleExW(flags, addr as *const u16, &mut m) } != 0).then_some(m as usize)
    };
    let ours = owner(from_graphics_driver as *const () as usize);
    for &f in &frames[..n.min(frames.len())] {
        // Code outside any module (a game's generated code, another program's patch).
        let Some(m) = owner(f as usize) else { return false };
        let kind = if Some(m) == ours { Caller::Passed } else { caller_kind(m) };
        match kind {
            Caller::Passed => continue,
            Caller::Driver => return true,
            Caller::Other => return false,
        }
    }
    false
}

fn caller_kind(m: usize) -> Caller {
    if let Some(&(_, k)) = crate::sys::lock(&CALLERS).iter().find(|(b, _)| *b == m) {
        return k;
    }
    let mut buf = [0u16; 520];
    let len = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW(m as _, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let path = String::from_utf16_lossy(&buf[..len]).to_ascii_lowercase();
    let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()).to_ascii_lowercase();
    let kind = if path.contains(r"\driverstore\") {
        Caller::Driver
    } else if path.starts_with(&format!(r"{system}\system32\")) || path.starts_with(&format!(r"{system}\syswow64\")) {
        Caller::Passed
    } else {
        Caller::Other
    };
    if kind == Caller::Driver {
        let name = path.rsplit('\\').next().unwrap_or(&path).to_string();
        log!("window: {name} (a graphics driver) asks for the screen's size: it's told the real one, not the pane's (told the pane's, it takes the display for the game's window as fullscreen)");
    }
    let mut c = crate::sys::lock(&CALLERS);
    if c.len() < 256 {
        c.push((m, kind));
    }
    kind
}

/// Current pane rectangle, if a split-screen session assigned one.
fn current_pane() -> Option<PaneRect> {
    if !VIEWPORT_ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let guard = crate::sys::lock(&SPEC);
    guard.as_ref().and_then(|s| s.pane).filter(|p| p.width > 0 && p.height > 0)
}

/// Finds this process's biggest visible unowned top-level window when `PRIMARY_HWND`
/// hasn't been reported by graphics hooks yet.
/// A standard dialog box (Windows' dialog class: a message box, a start-up prompt): the
/// session puts it over the pane at its own size; it's never the game's window.
fn is_dialog(h: HWND) -> bool {
    let mut class = [0u16; 16];
    let n = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW(h, class.as_mut_ptr(), class.len() as i32) }.max(0) as usize;
    class[..n] == *wide("#32770").strip_suffix(&[0]).unwrap_or_default()
}

fn find_own_window() -> usize {
    struct Find {
        pid: u32,
        best: HWND,
        area: i64,
    }
    unsafe extern "system" fn each(h: HWND, l: isize) -> i32 {
        let f = unsafe { &mut *(l as *mut Find) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut pid) };
        if pid != f.pid || unsafe { IsWindowVisible(h) } == 0 || !unsafe { GetWindow(h, GW_OWNER) }.is_null() || is_dialog(h) {
            return 1;
        }
        let mut r: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(h, &mut r) };
        let (w, h_px) = (r.right - r.left, r.bottom - r.top);
        if w <= 100 || h_px <= 100 {
            return 1;
        }
        let area = i64::from(w) * i64::from(h_px);
        if area > f.area {
            f.area = area;
            f.best = h;
        }
        1
    }
    let mut f = Find { pid: unsafe { GetCurrentProcessId() }, best: std::ptr::null_mut(), area: 0 };
    unsafe { EnumWindows(Some(each), &mut f as *mut Find as isize) };
    f.best as usize
}

/// The pane the game's window is to be put in next (`reposition_window`), and whether a
/// thread is doing it.
static PENDING_PANE: Mutex<Option<PaneRect>> = Mutex::new(None);
static REPOSITIONING: AtomicBool = AtomicBool::new(false);

/// Puts the game's window in `pane`, on a thread of its own: the calls wait on the
/// game's window thread, and the one asking (the session's connection) mustn't wait on
/// a game busy loading or holding a lock its window thread wants. Panes asked for while
/// one is being done: the last one is done next.
fn reposition_window(pane: PaneRect) {
    if pane.width == 0 || pane.height == 0 {
        return;
    }
    *crate::sys::lock(&PENDING_PANE) = Some(pane);
    if REPOSITIONING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("peerparty-pane".into()).spawn(|| {
        loop {
            while let Some(p) = crate::sys::lock(&PENDING_PANE).take() {
                put_in_pane(p);
            }
            REPOSITIONING.store(false, Ordering::Release);
            // One asked for between the last look and the flag going down.
            if crate::sys::lock(&PENDING_PANE).is_none() || REPOSITIONING.swap(true, Ordering::AcqRel) {
                break;
            }
        }
    });
    if spawned.is_err() {
        REPOSITIONING.store(false, Ordering::Release);
        if let Some(p) = crate::sys::lock(&PENDING_PANE).take() {
            put_in_pane(p);
        }
    }
}

/// Restores a contained split-screen game window to the taskbar if the session's
/// compositor process goes away while the game is still running (otherwise the game
/// stays trapped as a `WS_EX_TOOLWINDOW` with no taskbar button or Alt+Tab entry).
pub fn restore_from_compositor() {
    if !VIEWPORT_ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    FOCUS_SPOOF.store(false, Ordering::Release);
    let mut hwnd = live_primary() as usize;
    if hwnd == 0 {
        hwnd = find_own_window();
    }
    if hwnd == 0 {
        return;
    }
    let h = hwnd as HWND;
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SWP_ASYNCWINDOWPOS, SWP_NOMOVE, SWP_NOSIZE, ShowWindowAsync};
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        let want_ex = (ex & !(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE)) | WS_EX_APPWINDOW;
        let toggle = IsWindowVisible(h) != 0 && ((ex & WS_EX_TOOLWINDOW) != 0 || (ex & WS_EX_APPWINDOW) == 0);
        if toggle {
            ShowWindowAsync(h, SW_HIDE);
        }
        if want_ex != ex {
            SetWindowLongW(h, GWL_EXSTYLE, want_ex as i32);
        }
        SetWindowPos(h, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED | SWP_ASYNCWINDOWPOS);
        if toggle {
            ShowWindowAsync(h, SW_SHOWNOACTIVATE);
        }
    }
    log!("window: the session's window went away: restored taskbar visibility for game window 0x{hwnd:x}");
}

/// Re-applies the game window's pane placement and size asynchronously on the pane thread
/// (for example when an exclusive-fullscreen swap chain adopts a non-pane backbuffer size
/// or returns to the pane's size).
pub(crate) fn sync_window_size() {
    if let Some(pane) = current_pane() {
        reposition_window(pane);
    }
}

/// Pure helper: the client size a contained window should keep inside `pane`. Normally
/// the pane itself; if the game asked for exclusive fullscreen and created a smaller or
/// different backbuffer `exclusive`, keeping the window at that backbuffer size keeps
/// `GetClientRect`, `WM_MOUSEMOVE` and `GetCursorPos` 1:1 with the game's render/UI
/// coordinates instead of stretching them apart.
pub(crate) fn target_window_size(pane: PaneRect, exclusive: Option<(u32, u32)>) -> (i32, i32) {
    match exclusive {
        Some((w, h)) if w >= 320 && h >= 200 => (w as i32, h as i32),
        _ => (pane.width as i32, pane.height as i32),
    }
}

fn put_in_pane(pane: PaneRect) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_NOTOPMOST, SWP_ASYNCWINDOWPOS, SWP_NOMOVE, SWP_NOSIZE, ShowWindowAsync, WS_EX_TOPMOST};
    let mut hwnd = live_primary() as usize;
    if hwnd == 0 {
        hwnd = find_own_window();
        if hwnd != 0 {
            // Made too small to be contained (a window sized to its resolution after it's
            // made): not seen being made, so taken up as the game's window here, focus
            // wrapper included, or it's told of every focus loss and never asked for its
            // cursor (WM_SETCURSOR), and the session's pointer shows over its own.
            log!("window: the game's window 0x{hwnd:x} was found in the process's windows (it wasn't contained as it was made): it's the game's window now");
            note_game_window(hwnd);
        }
    }
    if hwnd == 0 {
        return;
    }
    let h = hwnd as HWND;
    let (tw, th) = target_window_size(pane, crate::video::fullscreen::exclusive_size());
    unsafe {
        let style = GetWindowLongW(h, GWL_STYLE) as u32;
        let want_style = (style & !CHROME_STYLES) | WS_POPUP;
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        let want_ex = (ex | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) & !WS_EX_APPWINDOW;
        let toggle = IsWindowVisible(h) != 0 && ((ex & WS_EX_TOOLWINDOW) == 0 || (ex & WS_EX_APPWINDOW) != 0);
        if toggle {
            ShowWindowAsync(h, SW_HIDE);
        }
        if want_style != style {
            SetWindowLongW(h, GWL_STYLE, want_style as i32);
        }
        if want_ex != ex {
            SetWindowLongW(h, GWL_EXSTYLE, want_ex as i32);
        }
        // Topmost isn't a style SetWindowLong changes.
        if ex & WS_EX_TOPMOST != 0 {
            SetWindowPos(h, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
        }
        if toggle {
            ShowWindowAsync(h, SW_SHOWNOACTIVATE);
        }
        SetWindowPos(
            h,
            std::ptr::null_mut(),
            pane.x,
            pane.y,
            tw,
            th,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_ASYNCWINDOWPOS,
        );
    }
}

/// Hooks `user32.dll` exports during module watch if an `InstanceSpec` asked for them.
pub fn attach_user32(exports: &[Export]) -> usize {
    if !wants_hooks() {
        return 0;
    }
    let mut n = 0;
    INSTALL.call_once(|| {
        n = hook_exports(exports);
    });
    n
}

fn install_loaded_user32() {
    INSTALL.call_once(|| {
        let Some(u32_mod) = module("user32.dll") else { return };
        let n = crate::sys::detour::batch(|| arm_user32(&crate::sys::detour::Source::Loaded(&|s| proc_addr(u32_mod, s))));
        log!("window: hooked {n} user32 viewport/focus entry points");
    });
}

fn hook_exports(exports: &[Export]) -> usize {
    arm_user32(&crate::sys::detour::Source::Exports(exports))
}

/// The user32 hooks, from its export list or from the loaded module (once).
fn arm_user32(src: &crate::sys::detour::Source) -> usize {
    let mut n = 0;
    macro_rules! one {
        ($cell:expr, $sym:literal, $ty:ty, $detour:ident) => {
            n += crate::sys::detour::arm_from(src, &$cell, $sym, $detour as $ty, "window");
        };
    }
    one!(GET_METRICS, c"GetSystemMetrics", GetSystemMetricsFn, detour_get_system_metrics);
    one!(MONITOR_INFO_W, c"GetMonitorInfoW", GetMonitorInfoWFn, detour_get_monitor_info_w);
    one!(MONITOR_INFO_A, c"GetMonitorInfoA", GetMonitorInfoAFn, detour_get_monitor_info_a);
    one!(ENUM_DISP_W, c"EnumDisplaySettingsW", EnumDisplaySettingsWFn, detour_enum_display_w);
    one!(ENUM_DISP_A, c"EnumDisplaySettingsA", EnumDisplaySettingsAFn, detour_enum_display_a);
    one!(GET_FOREGROUND, c"GetForegroundWindow", GetForegroundWindowFn, detour_get_foreground);
    one!(GET_ACTIVE, c"GetActiveWindow", GetActiveWindowFn, detour_get_active);
    one!(GET_FOCUS, c"GetFocus", GetFocusFn, detour_get_focus);
    one!(REGISTER_RAW_INPUT, c"RegisterRawInputDevices", RegisterRawInputDevicesFn, detour_register_raw_input);
    one!(CREATE_WINDOW_EX_W, c"CreateWindowExW", CreateWindowExWFn, detour_create_window_ex_w);
    one!(CREATE_WINDOW_EX_A, c"CreateWindowExA", CreateWindowExAFn, detour_create_window_ex_a);
    one!(SET_WINDOW_POS, c"SetWindowPos", SetWindowPosFn, detour_set_window_pos);
    one!(MOVE_WINDOW, c"MoveWindow", MoveWindowFn, detour_move_window);
    one!(SET_LONG_W, c"SetWindowLongW", SetWindowLongFn, detour_set_window_long_w);
    one!(SET_LONG_A, c"SetWindowLongA", SetWindowLongFn, detour_set_window_long_a);
    one!(SET_LONG_PTR_W, c"SetWindowLongPtrW", SetWindowLongPtrFn, detour_set_window_long_ptr_w);
    one!(SET_LONG_PTR_A, c"SetWindowLongPtrA", SetWindowLongPtrFn, detour_set_window_long_ptr_a);
    #[cfg(target_pointer_width = "64")]
    {
        one!(GET_LONG_PTR_W, c"GetWindowLongPtrW", GetWindowLongPtrFn, detour_get_window_long_ptr_w);
        one!(GET_LONG_PTR_A, c"GetWindowLongPtrA", GetWindowLongPtrFn, detour_get_window_long_ptr_a);
    }
    #[cfg(target_pointer_width = "32")]
    {
        one!(GET_LONG_PTR_W, c"GetWindowLongW", GetWindowLongPtrFn, detour_get_window_long_ptr_w);
        one!(GET_LONG_PTR_A, c"GetWindowLongA", GetWindowLongPtrFn, detour_get_window_long_ptr_a);
    }
    one!(SHOW_WINDOW, c"ShowWindow", ShowWindowFn, detour_show_window);
    one!(ENUM_DISP_EX_W, c"EnumDisplaySettingsExW", EnumDisplaySettingsExWFn, detour_enum_display_ex_w);
    one!(ENUM_DISP_EX_A, c"EnumDisplaySettingsExA", EnumDisplaySettingsExAFn, detour_enum_display_ex_a);
    one!(GET_METRICS_DPI, c"GetSystemMetricsForDpi", GetSystemMetricsForDpiFn, detour_get_system_metrics_for_dpi);
    one!(ENUM_MONITORS, c"EnumDisplayMonitors", EnumDisplayMonitorsFn, detour_enum_display_monitors);
    one!(TRANSLATE, c"TranslateMessage", TranslateMessageFn, detour_translate_message);
    one!(TRACK_MOUSE, c"TrackMouseEvent", TrackMouseEventFn, detour_track_mouse);
    one!(DISPATCH_W, c"DispatchMessageW", DispatchMessageWFn, detour_dispatch_w);
    one!(CHANGE_DISPLAY_EX_W, c"ChangeDisplaySettingsExW", ChangeDisplayExFn, detour_change_display_ex_w);
    one!(CHANGE_DISPLAY_EX_A, c"ChangeDisplaySettingsExA", ChangeDisplayExFn, detour_change_display_ex_a);
    one!(CHANGE_DISPLAY_W, c"ChangeDisplaySettingsW", ChangeDisplayFn, detour_change_display_w);
    one!(CHANGE_DISPLAY_A, c"ChangeDisplaySettingsA", ChangeDisplayFn, detour_change_display_a);
    n + crate::pointer::arm(src)
}

const WM_KEYDOWN: u32 = 0x0100;
const WM_SYSKEYDOWN: u32 = 0x0104;

/// Pure helper: whether `TranslateMessage` should leave message `msg` alone: a key the
/// session's window forwarded (`session::FORWARDED_KEY`), which came with its character
/// already.
pub fn forwarded_key(msg: u32, lparam: isize) -> bool {
    matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && lparam & peerparty_protocol::session::FORWARDED_KEY != 0
}

/// A forwarded key isn't made into a second character; it's "translated" all the same
/// (what `TranslateMessage` says for any key message).
unsafe extern "system" fn detour_translate_message(msg: *const Msg) -> i32 {
    if !msg.is_null() && forwarded_key(unsafe { (*msg).message }, unsafe { (*msg).lparam }) {
        return 1;
    }
    TRANSLATE.get().map_or(0, |h| unsafe { h.call(msg) })
}

const TME_LEAVE: u32 = 0x0002;
const TME_QUERY: u32 = 0x4000_0000;
const TME_CANCEL: u32 = 0x8000_0000;

/// Pure helper: what's left of a `TrackMouseEvent` request in a split-screen copy: no
/// "the mouse left" notice (the real mouse is over the session's window, so Windows
/// would send it at once and the game would think the mouse is outside it). None when
/// nothing is left to ask for.
pub fn track_mouse_flags(flags: u32, focus_spoof: bool) -> Option<u32> {
    if !focus_spoof || flags & (TME_QUERY | TME_CANCEL) != 0 || flags & TME_LEAVE == 0 {
        return Some(flags);
    }
    let rest = flags & !TME_LEAVE;
    // TME_NONCLIENT alone qualifies TME_LEAVE / TME_HOVER: nothing left without them.
    (rest & 0x0001 != 0).then_some(rest)
}

unsafe extern "system" fn detour_track_mouse(t: *mut TrackMouse) -> i32 {
    let Some(h) = TRACK_MOUSE.get() else { return 0 };
    if t.is_null() {
        return unsafe { h.call(t) };
    }
    let flags = unsafe { (*t).flags };
    match track_mouse_flags(flags, FOCUS_SPOOF.load(Ordering::Relaxed)) {
        Some(f) if f == flags => unsafe { h.call(t) },
        Some(f) => {
            let mut copy = TrackMouse { size: unsafe { (*t).size }, flags: f, hwnd: unsafe { (*t).hwnd }, hover_time: unsafe { (*t).hover_time } };
            unsafe { h.call(&mut copy) }
        }
        None => 1,
    }
}

/// What mouse input a split-screen copy's game window was handed (`DispatchMessageW`),
/// for its log (`log_mouse`, with the frame rate): plain mouse messages, pointer
/// messages, "the mouse left", Raw Input, and the last mouse position given.
static MOUSE_MOVES: AtomicUsize = AtomicUsize::new(0);
static MOUSE_BUTTONS: AtomicUsize = AtomicUsize::new(0);
static POINTER_MSGS: AtomicUsize = AtomicUsize::new(0);
static MOUSE_LEAVES: AtomicUsize = AtomicUsize::new(0);
static RAW_MSGS: AtomicUsize = AtomicUsize::new(0);
static LAST_MOVE: AtomicUsize = AtomicUsize::new(usize::MAX);

unsafe extern "system" fn detour_dispatch_w(msg: *const Msg) -> isize {
    if FOCUS_SPOOF.load(Ordering::Relaxed) && !msg.is_null() {
        let (m, l) = unsafe { ((*msg).message, (*msg).lparam) };
        match m {
            0x0200 => {
                MOUSE_MOVES.fetch_add(1, Ordering::Relaxed);
                LAST_MOVE.store(l as usize & 0xffff_ffff, Ordering::Relaxed);
            }
            0x0201..=0x020D => {
                MOUSE_BUTTONS.fetch_add(1, Ordering::Relaxed);
            }
            0x0241..=0x0251 => {
                POINTER_MSGS.fetch_add(1, Ordering::Relaxed);
            }
            0x02A3 => {
                MOUSE_LEAVES.fetch_add(1, Ordering::Relaxed);
            }
            0x00FF => {
                RAW_MSGS.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
    DISPATCH_W.get().map_or(0, |h| unsafe { h.call(msg) })
}

/// Into the log: the mouse input the game window was handed since the last time, where
/// the last mouse move said the mouse is, and where the game finds the cursor.
pub fn log_mouse() {
    if !FOCUS_SPOOF.load(Ordering::Relaxed) {
        return;
    }
    let take = |a: &AtomicUsize| a.swap(0, Ordering::Relaxed);
    let (moves, buttons, pointer, leaves, raw) = (take(&MOUSE_MOVES), take(&MOUSE_BUTTONS), take(&POINTER_MSGS), take(&MOUSE_LEAVES), take(&RAW_MSGS));
    let last = LAST_MOVE.load(Ordering::Relaxed);
    let at = if last == usize::MAX { "none".to_string() } else { format!("({}, {})", last as u16 as i16, (last >> 16) as u16 as i16) };
    let mut cursor = POINT_ZERO;
    let got = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor) } != 0;
    let hwnd = PRIMARY_HWND.load(Ordering::Relaxed);
    let mut client = POINT_ZERO;
    let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    if hwnd != 0 {
        unsafe {
            windows_sys::Win32::Graphics::Gdi::ClientToScreen(hwnd as HWND, &mut client);
            GetClientRect(hwnd as HWND, &mut rc);
        }
    }
    log!(
        "window: the game window got {moves} mouse moves (the last at {at}), {buttons} mouse buttons, {pointer} pointer messages, {leaves} mouse-left notices, {raw} Raw Input messages; GetCursorPos says {} and its client area is {}x{} at ({}, {})",
        if got { format!("({}, {})", cursor.x, cursor.y) } else { "nothing".into() },
        rc.right,
        rc.bottom,
        client.x,
        client.y
    );
}

const POINT_ZERO: windows_sys::Win32::Foundation::POINT = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };

/// The game's window, once a frame or its creation has shown it (0 until then).
pub fn game_window() -> usize {
    PRIMARY_HWND.load(Ordering::Relaxed)
}

/// The game's window if it's still one (0 for none, or one destroyed): a focus call
/// answered with a dead window would send the game after nothing.
fn live_primary() -> isize {
    let h = PRIMARY_HWND.load(Ordering::Relaxed);
    if h != 0 && unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(h as HWND) } != 0 { h as isize } else { 0 }
}

/// A window the game made was put in its pane: it's the game's window until a frame says
/// another is, but a second one (a splash, a launcher dialog) doesn't take over from one
/// still there.
fn note_created_window(hwnd: usize) {
    if live_primary() == 0 {
        note_game_window(hwnd);
    }
}

const WM_DEVICECHANGE: u32 = 0x0219;
const DBT_DEVNODES_CHANGED: usize = 0x0007;
const DBT_DEVICEARRIVAL: usize = 0x8000;

/// `DEV_BROADCAST_DEVICEINTERFACE_W` with an empty name.
#[repr(C)]
struct DevBroadcastInterface {
    size: u32,
    device_type: u32,
    reserved: u32,
    class: [u8; 16],
    name: [u16; 1],
}

/// A HID device interface's arrival (`GUID_DEVINTERFACE_HID`), sent with the plain
/// "devices changed": some engines look for controllers again only on an interface's
/// arrival (SDL 2.0's joystick thread), and an arrival makes the newer ones look at
/// every controller, which finds one gone too. Static: it's posted, and read later.
static HID_ARRIVAL: DevBroadcastInterface = DevBroadcastInterface {
    size: std::mem::size_of::<DevBroadcastInterface>() as u32,
    // DBT_DEVTYP_DEVICEINTERFACE.
    device_type: 5,
    reserved: 0,
    // {4D1E55B2-F16F-11CF-88CB-001111000030}
    class: [0xB2, 0x55, 0x1E, 0x4D, 0x6F, 0xF1, 0xCF, 0x11, 0x88, 0xCB, 0x00, 0x11, 0x11, 0x00, 0x00, 0x30],
    name: [0],
};

/// Tells this process's windows that its controllers changed (`WM_DEVICECHANGE` /
/// `DBT_DEVNODES_CHANGED`), so an engine that stops polling disconnected XInput slots
/// after start-up scans again once a controller takes this copy's seat.
pub fn notify_devices_changed() {
    use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;
    struct Collect {
        pid: u32,
        hwnds: Vec<usize>,
    }
    unsafe extern "system" fn each(h: HWND, l: isize) -> i32 {
        let c = unsafe { &mut *(l as *mut Collect) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut pid) };
        if pid == c.pid {
            c.hwnds.push(h as usize);
        }
        1
    }
    let mut c = Collect { pid: unsafe { GetCurrentProcessId() }, hwnds: Vec::new() };
    unsafe { EnumWindows(Some(each), &mut c as *mut Collect as isize) };
    // Message-only windows (SDL's and Unity's device watchers) aren't top-level ones:
    // EnumWindows doesn't list them.
    let top_level = c.hwnds.len();
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, HWND_MESSAGE};
        let mut after: HWND = std::ptr::null_mut();
        for _ in 0..4096 {
            let h = unsafe { FindWindowExW(HWND_MESSAGE, after, std::ptr::null(), std::ptr::null()) };
            if h.is_null() {
                break;
            }
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(h, &mut pid) };
            if pid == c.pid && !c.hwnds.contains(&(h as usize)) {
                c.hwnds.push(h as usize);
            }
            after = h;
        }
    }
    let message_only = c.hwnds.len() - top_level;
    let primary = PRIMARY_HWND.load(Ordering::Relaxed);
    if primary != 0 && !c.hwnds.contains(&primary) {
        c.hwnds.push(primary);
    }
    let mut posted = Vec::new();
    let arrival = &HID_ARRIVAL as *const DevBroadcastInterface as isize;
    for h in c.hwnds {
        let nodes = unsafe { PostMessageW(h as HWND, WM_DEVICECHANGE, DBT_DEVNODES_CHANGED, 0) } != 0;
        let arrived = unsafe { PostMessageW(h as HWND, WM_DEVICECHANGE, DBT_DEVICEARRIVAL, arrival) } != 0;
        if nodes || arrived {
            posted.push(format!("0x{h:x}"));
        }
    }
    log!("window: told {} game window(s) [{}] that controllers changed (WM_DEVICECHANGE, devices changed and a HID device arrived; {message_only} of them message-only)", posted.len(), posted.join(", "));
}

/// Pure helper: whether a `CreateWindowExW` call looks like the game's top-level window
/// in a split-screen session (never a child, message-only, existing tool, or tiny helper
/// window).
pub fn should_contain_created_window(ex_style: u32, style: u32, w: i32, h: i32, parent: isize) -> bool {
    if parent != 0 {
        return false;
    }
    if style & WS_CHILD != 0 || ex_style & WS_EX_TOOLWINDOW != 0 {
        return false;
    }
    const CW_USEDEFAULT: i32 = i32::MIN;
    if w == CW_USEDEFAULT || h == CW_USEDEFAULT {
        return (style & (WS_CAPTION | WS_OVERLAPPEDWINDOW | WS_POPUP)) != 0;
    }
    w >= 320 && h >= 200
}

/// Pure helper: maps `GetSystemMetrics(index)` to the spoofed viewport when applicable.
pub fn spoof_system_metric(index: i32, real: i32, vp: Option<(u32, u32)>) -> i32 {
    let Some((w, h)) = vp else { return real };
    match index {
        SM_CXSCREEN | SM_CXFULLSCREEN | SM_CXVIRTUALSCREEN => w as i32,
        SM_CYSCREEN | SM_CYFULLSCREEN | SM_CYVIRTUALSCREEN => h as i32,
        _ => real,
    }
}

/// Pure helper: whether a window message represents focus loss that should be suppressed
/// while `focus_spoof` is enabled.
pub fn should_suppress_focus_loss(msg: u32, wparam: usize, focus_spoof: bool) -> bool {
    if !focus_spoof {
        return false;
    }
    match msg {
        WM_KILLFOCUS => true,
        WM_ACTIVATE => (wparam & 0xFFFF) == 0,
        WM_ACTIVATEAPP => wparam == 0,
        // The frame drawn inactive, which some engines take as the focus gone too.
        WM_NCACTIVATE => wparam == 0,
        _ => false,
    }
}

/// Pure helper: chooses the returned HWND for `GetForegroundWindow` / `GetActiveWindow` /
/// `GetFocus`. When `focus_spoof` is active and `real` is null or belongs to another
/// process (`real_is_ours == false`), returns `primary` if known.
pub fn spoof_focus_hwnd(real: isize, real_is_ours: bool, primary: isize, focus_spoof: bool) -> isize {
    if focus_spoof && !real_is_ours && primary != 0 {
        primary
    } else {
        real
    }
}

fn is_our_window(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd as _, &mut pid) };
    pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

fn patch_monitor_info(mi: *mut MonitorInfo) {
    if mi.is_null() {
        return;
    }
    let Some((w, h)) = screen_for_caller() else { return };
    let mi = unsafe { &mut *mi };
    if (mi.cb_size as usize) < std::mem::size_of::<MonitorInfo>() {
        return;
    }
    let r = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    mi.rc_monitor = r;
    mi.rc_work = r;
}

fn patch_devmode(mode: u32, dm: *mut u8, off_w: usize, off_h: usize) {
    if dm.is_null() || (mode != ENUM_CURRENT_SETTINGS && mode != ENUM_REGISTRY_SETTINGS && mode != 0) {
        return;
    }
    let Some((w, h)) = screen_for_caller() else { return };
    unsafe {
        (dm.add(off_w) as *mut u32).write_unaligned(w);
        (dm.add(off_h) as *mut u32).write_unaligned(h);
    }
}

unsafe extern "system" fn detour_change_display_ex_w(device: usize, mode: usize, hwnd: isize, flags: u32, param: usize) -> i32 {
    keep_display_mode(mode).unwrap_or_else(|| CHANGE_DISPLAY_EX_W.get().map_or(-1, |h| unsafe { h.call(device, mode, hwnd, flags, param) }))
}

unsafe extern "system" fn detour_change_display_ex_a(device: usize, mode: usize, hwnd: isize, flags: u32, param: usize) -> i32 {
    keep_display_mode(mode).unwrap_or_else(|| CHANGE_DISPLAY_EX_A.get().map_or(-1, |h| unsafe { h.call(device, mode, hwnd, flags, param) }))
}

unsafe extern "system" fn detour_change_display_w(mode: usize, flags: u32) -> i32 {
    keep_display_mode(mode).unwrap_or_else(|| CHANGE_DISPLAY_W.get().map_or(-1, |h| unsafe { h.call(mode, flags) }))
}

unsafe extern "system" fn detour_change_display_a(mode: usize, flags: u32) -> i32 {
    keep_display_mode(mode).unwrap_or_else(|| CHANGE_DISPLAY_A.get().map_or(-1, |h| unsafe { h.call(mode, flags) }))
}

unsafe extern "system" fn detour_get_system_metrics(index: i32) -> i32 {
    let real = match GET_METRICS.get() {
        Some(h) => unsafe { h.call(index) },
        None => 0,
    };
    if !spoofed_metric(index) {
        return real;
    }
    spoof_system_metric(index, real, screen_for_caller())
}

/// Whether `spoof_system_metric` changes metric `index`: the rest skip the session's
/// lock (a game asks for metrics often, most of them not these).
fn spoofed_metric(index: i32) -> bool {
    matches!(index, SM_CXSCREEN | SM_CXFULLSCREEN | SM_CXVIRTUALSCREEN | SM_CYSCREEN | SM_CYFULLSCREEN | SM_CYVIRTUALSCREEN)
}

unsafe extern "system" fn detour_get_monitor_info_w(mon: isize, mi: *mut MonitorInfo) -> i32 {
    let Some(h) = MONITOR_INFO_W.get() else { return 0 };
    let ok = unsafe { h.call(mon, mi) };
    if ok != 0 {
        patch_monitor_info(mi);
    }
    ok
}

unsafe extern "system" fn detour_get_monitor_info_a(mon: isize, mi: *mut MonitorInfo) -> i32 {
    let Some(h) = MONITOR_INFO_A.get() else { return 0 };
    let ok = unsafe { h.call(mon, mi) };
    if ok != 0 {
        patch_monitor_info(mi);
    }
    ok
}

unsafe extern "system" fn detour_enum_display_w(dev: *const u16, mode: u32, dm: *mut u8) -> i32 {
    let Some(h) = ENUM_DISP_W.get() else { return 0 };
    let ok = unsafe { h.call(dev, mode, dm) };
    if ok != 0 {
        patch_devmode(mode, dm, DEVMODEW_PELS_WIDTH, DEVMODEW_PELS_HEIGHT);
    }
    ok
}

unsafe extern "system" fn detour_enum_display_a(dev: *const u8, mode: u32, dm: *mut u8) -> i32 {
    let Some(h) = ENUM_DISP_A.get() else { return 0 };
    let ok = unsafe { h.call(dev, mode, dm) };
    if ok != 0 {
        patch_devmode(mode, dm, DEVMODEA_PELS_WIDTH, DEVMODEA_PELS_HEIGHT);
    }
    ok
}

unsafe extern "system" fn detour_get_foreground() -> isize {
    let real = GET_FOREGROUND.get().map_or(0, |h| unsafe { h.call() });
    let spoof = FOCUS_SPOOF.load(Ordering::Relaxed);
    // The game's own window (the usual answer) is ours: no lookups.
    if !spoof || (real != 0 && real as usize == PRIMARY_HWND.load(Ordering::Relaxed)) {
        return real;
    }
    spoof_focus_hwnd(real, is_our_window(real), live_primary(), true)
}

unsafe extern "system" fn detour_get_active() -> isize {
    let real = GET_ACTIVE.get().map_or(0, |h| unsafe { h.call() });
    let spoof = FOCUS_SPOOF.load(Ordering::Relaxed);
    // The game's own window (the usual answer) is ours: no lookups.
    if !spoof || (real != 0 && real as usize == PRIMARY_HWND.load(Ordering::Relaxed)) {
        return real;
    }
    spoof_focus_hwnd(real, is_our_window(real), live_primary(), true)
}

unsafe extern "system" fn detour_get_focus() -> isize {
    let real = GET_FOCUS.get().map_or(0, |h| unsafe { h.call() });
    let spoof = FOCUS_SPOOF.load(Ordering::Relaxed);
    // The game's own window (the usual answer) is ours: no lookups.
    if !spoof || (real != 0 && real as usize == PRIMARY_HWND.load(Ordering::Relaxed)) {
        return real;
    }
    spoof_focus_hwnd(real, is_our_window(real), live_primary(), true)
}

unsafe extern "system" fn detour_register_raw_input(
    devices: *const RawInputDevice,
    count: u32,
    cb_size: u32,
) -> i32 {
    let Some(hook) = REGISTER_RAW_INPUT.get() else { return 0 };
    if !wants_hooks()
        || sinks_skipped()
        || devices.is_null()
        || count == 0
        || count > 64
        || cb_size as usize != std::mem::size_of::<RawInputDevice>()
    {
        return unsafe { hook.call(devices, count, cb_size) };
    }
    let slice = unsafe { std::slice::from_raw_parts(devices, count as usize) };
    let mut patched: Vec<RawInputDevice> = slice.to_vec();
    let fallback = PRIMARY_HWND.load(Ordering::Relaxed) as isize;
    let mut upgraded = 0usize;
    for d in &mut patched {
        if upgrade_raw_input_device(d, fallback) {
            upgraded += 1;
        }
    }
    if upgraded > 0 {
        let rc = unsafe { hook.call(patched.as_ptr(), patched.len() as u32, cb_size) };
        if rc != 0 {
            RAW_SINKS_UPGRADED.store(true, Ordering::Relaxed);
            return rc;
        }
    }
    unsafe { hook.call(devices, count, cb_size) }
}

unsafe extern "system" fn detour_create_window_ex_w(
    mut ex_style: u32,
    class_name: *const u16,
    window_name: *const u16,
    mut style: u32,
    mut x: i32,
    mut y: i32,
    mut w: i32,
    mut h: i32,
    parent: isize,
    menu: isize,
    instance: isize,
    param: *const core::ffi::c_void,
) -> isize {
    let Some(hook) = CREATE_WINDOW_EX_W.get() else { return 0 };
    let contained = contain_created(&mut ex_style, &mut style, &mut x, &mut y, &mut w, &mut h, parent);
    let hwnd = unsafe { hook.call(ex_style, class_name, window_name, style, x, y, w, h, parent, menu, instance, param) };
    if contained && hwnd != 0 {
        note_created_window(hwnd as usize);
    }
    hwnd
}

/// A window the game is making goes straight into its pane, as `reposition_window`
/// puts one already there (the pane's size: the window is the pane). Whether it did.
fn contain_created(ex_style: &mut u32, style: &mut u32, x: &mut i32, y: &mut i32, w: &mut i32, h: &mut i32, parent: isize) -> bool {
    let Some(pane) = current_pane() else { return false };
    if !should_contain_created_window(*ex_style, *style, *w, *h, parent) {
        return false;
    }
    *style = (*style & !CHROME_STYLES) | WS_POPUP;
    *ex_style = (*ex_style | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) & !(WS_EX_APPWINDOW | WS_EX_TOPMOST);
    (*x, *y, *w, *h) = (pane.x, pane.y, pane.width as i32, pane.height as i32);
    true
}

/// The ANSI form, for games built without Unicode.
unsafe extern "system" fn detour_create_window_ex_a(
    mut ex_style: u32,
    class_name: *const u8,
    window_name: *const u8,
    mut style: u32,
    mut x: i32,
    mut y: i32,
    mut w: i32,
    mut h: i32,
    parent: isize,
    menu: isize,
    instance: isize,
    param: *const core::ffi::c_void,
) -> isize {
    let Some(hook) = CREATE_WINDOW_EX_A.get() else { return 0 };
    let contained = contain_created(&mut ex_style, &mut style, &mut x, &mut y, &mut w, &mut h, parent);
    let hwnd = unsafe { hook.call(ex_style, class_name, window_name, style, x, y, w, h, parent, menu, instance, param) };
    if contained && hwnd != 0 {
        note_created_window(hwnd as usize);
    }
    hwnd
}

/// The pane the game's own window is kept in, when `h` is that window.
fn contained(h: isize) -> Option<PaneRect> {
    if h == 0 || h as usize != PRIMARY_HWND.load(Ordering::Relaxed) {
        return None;
    }
    current_pane()
}

/// Says once in the log that the game tried to move its window out of its pane.
fn told_kept(what: &str) {
    static TOLD: AtomicUsize = AtomicUsize::new(0);
    if TOLD.fetch_add(1, Ordering::Relaxed) < 3 {
        log!("window: the game's {what} would take its window out of its pane: kept there");
    }
}

const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER_FLAG: u32 = 0x0004;

/// Pure helper: a `SetWindowPos` on the game's window in pane `p`: its place and size
/// are the pane's, and it isn't put in front of anything (it stays behind the session's
/// window). Returns the call's x, y, width, height and flags.
pub fn kept_window_pos(p: PaneRect, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> (i32, i32, i32, i32, u32) {
    let (x, y) = if flags & SWP_NOMOVE == 0 { (p.x, p.y) } else { (x, y) };
    let (cx, cy) = if flags & SWP_NOSIZE == 0 { (p.width as i32, p.height as i32) } else { (cx, cy) };
    (x, y, cx, cy, flags | SWP_NOZORDER_FLAG)
}

/// Pure helper: a style the game sets on its window in a pane: no frame, no topmost,
/// no taskbar button.
pub fn kept_style(index: i32, v: u32) -> u32 {
    match index {
        GWL_STYLE => (v & !CHROME_STYLES) | WS_POPUP,
        GWL_EXSTYLE => (v | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) & !(WS_EX_APPWINDOW | WS_EX_TOPMOST),
        _ => v,
    }
}

/// Pure helper: a `ShowWindow` command on the game's window in a pane: never minimized
/// or maximized (its frames would stop, or it would cover the screen), never activated
/// (it would take the foreground from the session's window).
pub fn kept_show(cmd: i32) -> i32 {
    const SW_SHOWNOACTIVATE_CMD: i32 = 4;
    match cmd {
        // SHOWNORMAL, SHOWMINIMIZED, MAXIMIZE, SHOW, MINIMIZE, SHOWMINNOACTIVE, RESTORE,
        // SHOWDEFAULT, FORCEMINIMIZE.
        1 | 2 | 3 | 5 | 6 | 7 | 9 | 10 | 11 => SW_SHOWNOACTIVATE_CMD,
        c => c,
    }
}

unsafe extern "system" fn detour_set_window_pos(h: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32 {
    let Some(hook) = SET_WINDOW_POS.get() else { return 0 };
    let Some(mut p) = contained(h) else { return unsafe { hook.call(h, after, x, y, cx, cy, flags) } };
    let (tw, th) = target_window_size(p, crate::video::fullscreen::exclusive_size());
    (p.width, p.height) = (tw as u32, th as u32);
    let (nx, ny, ncx, ncy, nflags) = kept_window_pos(p, x, y, cx, cy, flags);
    // HWND_NOTOPMOST only takes it down: that one goes through.
    const HWND_NOTOPMOST: isize = -2;
    if after == HWND_NOTOPMOST {
        return unsafe { hook.call(h, after, nx, ny, ncx, ncy, flags) };
    }
    if (nx, ny, ncx, ncy) != (x, y, cx, cy) || (flags & SWP_NOZORDER_FLAG == 0 && after != 0) {
        told_kept("SetWindowPos");
    }
    unsafe { hook.call(h, 0, nx, ny, ncx, ncy, nflags) }
}

unsafe extern "system" fn detour_move_window(h: isize, x: i32, y: i32, w: i32, ht: i32, repaint: i32) -> i32 {
    let Some(hook) = MOVE_WINDOW.get() else { return 0 };
    match contained(h) {
        Some(p) => {
            let (tw, th) = target_window_size(p, crate::video::fullscreen::exclusive_size());
            if (x, y, w, ht) != (p.x, p.y, tw, th) {
                told_kept("MoveWindow");
            }
            unsafe { hook.call(h, p.x, p.y, tw, th, repaint) }
        }
        None => unsafe { hook.call(h, x, y, w, ht, repaint) },
    }
}

unsafe extern "system" fn detour_set_window_long_w(h: isize, index: i32, v: i32) -> i32 {
    let Some(hook) = SET_LONG_W.get() else { return 0 };
    // x86's SetWindowLongW is its SetWindowLongPtrW (on x64 it refuses GWL_WNDPROC).
    if cfg!(target_pointer_width = "32")
        && index == GWLP_WNDPROC
        && let Some(prev) = proc_set_by_game(h, true, v as isize)
    {
        return prev as i32;
    }
    let v = if contained(h).is_some() { kept_style(index, v as u32) as i32 } else { v };
    unsafe { hook.call(h, index, v) }
}

unsafe extern "system" fn detour_set_window_long_a(h: isize, index: i32, v: i32) -> i32 {
    let Some(hook) = SET_LONG_A.get() else { return 0 };
    if cfg!(target_pointer_width = "32")
        && index == GWLP_WNDPROC
        && let Some(prev) = proc_set_by_game(h, false, v as isize)
    {
        return prev as i32;
    }
    let v = if contained(h).is_some() { kept_style(index, v as u32) as i32 } else { v };
    unsafe { hook.call(h, index, v) }
}

unsafe extern "system" fn detour_set_window_long_ptr_w(h: isize, index: i32, v: isize) -> isize {
    let Some(hook) = SET_LONG_PTR_W.get() else { return 0 };
    if index == GWLP_WNDPROC
        && let Some(prev) = proc_set_by_game(h, true, v)
    {
        return prev;
    }
    let v = if contained(h).is_some() && matches!(index, GWL_STYLE | GWL_EXSTYLE) { kept_style(index, v as u32) as isize } else { v };
    unsafe { hook.call(h, index, v) }
}

unsafe extern "system" fn detour_set_window_long_ptr_a(h: isize, index: i32, v: isize) -> isize {
    let Some(hook) = SET_LONG_PTR_A.get() else { return 0 };
    if index == GWLP_WNDPROC
        && let Some(prev) = proc_set_by_game(h, false, v)
    {
        return prev;
    }
    let v = if contained(h).is_some() && matches!(index, GWL_STYLE | GWL_EXSTYLE) { kept_style(index, v as u32) as isize } else { v };
    unsafe { hook.call(h, index, v) }
}

unsafe extern "system" fn detour_get_window_long_ptr_w(h: isize, index: i32) -> isize {
    let Some(hook) = GET_LONG_PTR_W.get() else { return 0 };
    if index == GWLP_WNDPROC
        && let Some(p) = proc_seen_by_game(h, true)
    {
        return p;
    }
    unsafe { hook.call(h, index) }
}

unsafe extern "system" fn detour_get_window_long_ptr_a(h: isize, index: i32) -> isize {
    let Some(hook) = GET_LONG_PTR_A.get() else { return 0 };
    if index == GWLP_WNDPROC
        && let Some(p) = proc_seen_by_game(h, false)
    {
        return p;
    }
    unsafe { hook.call(h, index) }
}

unsafe extern "system" fn detour_show_window(h: isize, cmd: i32) -> i32 {
    let Some(hook) = SHOW_WINDOW.get() else { return 0 };
    let cmd = match contained(h) {
        Some(_) => {
            let kept = kept_show(cmd);
            if kept != cmd {
                static TOLD: AtomicUsize = AtomicUsize::new(0);
                if TOLD.fetch_add(1, Ordering::Relaxed) < 3 {
                    log!("window: the game's ShowWindow({cmd}) on its window is shown without minimizing, maximizing or taking the foreground ({kept})");
                }
            }
            kept
        }
        None => cmd,
    };
    unsafe { hook.call(h, cmd) }
}

/// `EnumDisplaySettingsExW/A`: as `EnumDisplaySettingsW/A`, the pane's size.
unsafe extern "system" fn detour_enum_display_ex_w(dev: *const u16, mode: u32, dm: *mut u8, flags: u32) -> i32 {
    let Some(h) = ENUM_DISP_EX_W.get() else { return 0 };
    let ok = unsafe { h.call(dev, mode, dm, flags) };
    if ok != 0 {
        patch_devmode(mode, dm, DEVMODEW_PELS_WIDTH, DEVMODEW_PELS_HEIGHT);
    }
    ok
}

unsafe extern "system" fn detour_enum_display_ex_a(dev: *const u8, mode: u32, dm: *mut u8, flags: u32) -> i32 {
    let Some(h) = ENUM_DISP_EX_A.get() else { return 0 };
    let ok = unsafe { h.call(dev, mode, dm, flags) };
    if ok != 0 {
        patch_devmode(mode, dm, DEVMODEA_PELS_WIDTH, DEVMODEA_PELS_HEIGHT);
    }
    ok
}

unsafe extern "system" fn detour_get_system_metrics_for_dpi(index: i32, dpi: u32) -> i32 {
    let real = GET_METRICS_DPI.get().map_or(0, |h| unsafe { h.call(index, dpi) });
    if !spoofed_metric(index) {
        return real;
    }
    spoof_system_metric(index, real, screen_for_caller())
}

type MonitorEnumProc = unsafe extern "system" fn(isize, isize, *mut RECT, isize) -> i32;

/// `EnumDisplayMonitors` of the whole desktop in a pane: one monitor, the one the pane
/// is on, the pane's size (as `GetMonitorInfo` says it). A game that lists monitors to
/// pick the biggest, or to span them, finds only its pane.
unsafe extern "system" fn detour_enum_display_monitors(hdc: isize, clip: *const RECT, f: Option<MonitorEnumProc>, data: isize) -> i32 {
    let Some(hook) = ENUM_MONITORS.get() else { return 0 };
    let (Some(pane), Some((w, h)), Some(f)) = (current_pane(), screen_for_caller(), f) else {
        return unsafe { hook.call(hdc, clip, f, data) };
    };
    if hdc != 0 || !clip.is_null() {
        return unsafe { hook.call(hdc, clip, Some(f), data) };
    }
    let at = windows_sys::Win32::Foundation::POINT { x: pane.x.saturating_add(pane.width as i32 / 2), y: pane.y.saturating_add(pane.height as i32 / 2) };
    // MONITOR_DEFAULTTOPRIMARY.
    let mon = unsafe { windows_sys::Win32::Graphics::Gdi::MonitorFromPoint(at, 1) } as isize;
    let mut r = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    unsafe { f(mon, 0, &mut r, data) };
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::session::InstanceId;

    #[test]
    fn a_split_screen_copy_is_never_told_the_mouse_left() {
        assert_eq!(track_mouse_flags(TME_LEAVE, true), None, "nothing left to ask");
        assert_eq!(track_mouse_flags(TME_LEAVE | 0x0001, true), Some(0x0001), "hover stays");
        assert_eq!(track_mouse_flags(TME_LEAVE | 0x0010, true), None, "non-client leave alone");
        assert_eq!(track_mouse_flags(TME_LEAVE | TME_CANCEL, true), Some(TME_LEAVE | TME_CANCEL));
        assert_eq!(track_mouse_flags(TME_LEAVE | TME_QUERY, true), Some(TME_LEAVE | TME_QUERY));
        assert_eq!(track_mouse_flags(TME_LEAVE, false), Some(TME_LEAVE), "a copy alone: as asked");
    }

    #[test]
    fn viewport_prefers_explicit_virtual_size_over_pane() {
        let mut spec = InstanceSpec::solo(InstanceId(0), 0);
        assert_eq!(viewport_of(&spec), None);
        spec.pane = Some(PaneRect { x: 0, y: 0, width: 960, height: 1080 });
        assert_eq!(viewport_of(&spec), Some((960, 1080)));
        spec.virtual_width = Some(1920);
        spec.virtual_height = Some(1080);
        assert_eq!(viewport_of(&spec), Some((1920, 1080)));
        spec.virtual_width = Some(0);
        assert_eq!(viewport_of(&spec), None, "zero dimension is ignored");
    }

    #[test]
    fn system_metrics_spoof_only_screen_dimensions() {
        assert_eq!(spoof_system_metric(SM_CXSCREEN, 2560, Some((960, 540))), 960);
        assert_eq!(spoof_system_metric(SM_CYSCREEN, 1440, Some((960, 540))), 540);
        assert_eq!(spoof_system_metric(SM_CXVIRTUALSCREEN, 5120, Some((960, 540))), 960);
        assert_eq!(spoof_system_metric(SM_CYVIRTUALSCREEN, 1440, Some((960, 540))), 540);
        assert_eq!(spoof_system_metric(5, 1, Some((960, 540))), 1, "unrelated metrics stay real");
        assert_eq!(spoof_system_metric(SM_CXSCREEN, 2560, None), 2560);
        // The detours' quick check passes every metric the helper changes.
        for i in 0..200 {
            if spoof_system_metric(i, -7, Some((960, 540))) != -7 {
                assert!(spoofed_metric(i), "metric {i}");
            }
        }
    }

    #[test]
    fn focus_spoofing_preserves_own_window_and_masks_loss() {
        assert_eq!(spoof_focus_hwnd(0x100, false, 0x200, true), 0x200);
        assert_eq!(spoof_focus_hwnd(0, false, 0x200, true), 0x200);
        assert_eq!(spoof_focus_hwnd(0x300, true, 0x200, true), 0x300, "another window of ours stays focused");
        assert_eq!(spoof_focus_hwnd(0x100, false, 0x200, false), 0x100, "off leaves real focus alone");
        assert!(should_suppress_focus_loss(WM_KILLFOCUS, 0, true));
        assert!(should_suppress_focus_loss(WM_ACTIVATE, 0, true));
        assert!(!should_suppress_focus_loss(WM_ACTIVATE, 1, true));
        assert!(should_suppress_focus_loss(WM_ACTIVATEAPP, 0, true));
        assert!(!should_suppress_focus_loss(WM_ACTIVATEAPP, 1, true));
        assert!(!should_suppress_focus_loss(WM_KILLFOCUS, 0, false));
    }

    #[test]
    fn containment_targets_only_top_level_game_windows() {
        assert!(should_contain_created_window(0, WS_OVERLAPPEDWINDOW, 1920, 1080, 0));
        assert!(should_contain_created_window(0, WS_POPUP, 1280, 720, 0));
        assert!(should_contain_created_window(0, WS_OVERLAPPEDWINDOW, i32::MIN, i32::MIN, 0));
        assert!(!should_contain_created_window(0, WS_CHILD, 1920, 1080, 0));
        assert!(!should_contain_created_window(0, WS_OVERLAPPEDWINDOW, 1920, 1080, 0x1234));
        assert!(!should_contain_created_window(WS_EX_TOOLWINDOW, WS_POPUP, 1920, 1080, 0));
        assert!(!should_contain_created_window(0, WS_POPUP, 64, 64, 0));
    }

    /// The game's own moves, styles and show commands on its window keep it in its pane.
    #[test]
    fn the_window_stays_in_its_pane() {
        let p = PaneRect { x: 960, y: 0, width: 960, height: 1080 };
        assert_eq!(kept_window_pos(p, 0, 0, 1920, 1080, 0), (960, 0, 960, 1080, SWP_NOZORDER_FLAG));
        assert_eq!(kept_window_pos(p, 5, 6, 7, 8, SWP_NOMOVE | SWP_NOSIZE), (5, 6, 7, 8, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER_FLAG), "only the z-order");
        assert_eq!(kept_style(GWL_STYLE, WS_OVERLAPPEDWINDOW) & (WS_CAPTION | WS_THICKFRAME | WS_POPUP), WS_POPUP);
        let ex = kept_style(GWL_EXSTYLE, WS_EX_TOPMOST | WS_EX_APPWINDOW);
        assert_eq!(ex & (WS_EX_TOPMOST | WS_EX_APPWINDOW | WS_EX_TOOLWINDOW), WS_EX_TOOLWINDOW);
        assert_eq!(kept_style(-21, 7), 7, "GWLP_USERDATA is the game's");
        assert_eq!((kept_show(6), kept_show(3), kept_show(5), kept_show(0), kept_show(8)), (4, 4, 4, 0, 8));
    }

    const TEST_MSG: u32 = 0x0400 + 0x2d1;

    thread_local! {
        /// What the test class's procedure calls on (0: Windows' default), and how often
        /// it and the subclass procedure saw `TEST_MSG`.
        static CLASS_PREV: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
        static CLASS_SEEN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static SUB_PREV: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
        static SUB_SEEN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn call_on(prev: isize, h: HWND, m: u32, w: usize, l: isize) -> isize {
        use windows_sys::Win32::UI::WindowsAndMessaging::{CallWindowProcW, DefWindowProcW, WNDPROC};
        if prev == 0 {
            return unsafe { DefWindowProcW(h, m, w, l) };
        }
        unsafe { CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(prev), h, m, w, l) }
    }

    /// A class procedure that subclasses the way SDL2 does: what it found in the window
    /// after creating it, if not itself, is called after it for every message.
    unsafe extern "system" fn class_proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
        if m == TEST_MSG {
            CLASS_SEEN.with(|c| c.set(c.get() + 1));
        }
        call_on(CLASS_PREV.with(|c| c.get()), h, m, w, l)
    }

    unsafe extern "system" fn sub_proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
        if m == TEST_MSG {
            SUB_SEEN.with(|c| c.set(c.get() + 1));
        }
        call_on(SUB_PREV.with(|c| c.get()), h, m, w, l)
    }

    fn class_window() -> HWND {
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, RegisterClassW, WNDCLASSW};
        static CLASS: Once = Once::new();
        let name = wide("PeerPartyTestChain");
        CLASS.call_once(|| {
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(class_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) },
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: name.as_ptr(),
            };
            assert_ne!(unsafe { RegisterClassW(&wc) }, 0);
        });
        CLASS_PREV.with(|c| c.set(0));
        CLASS_SEEN.with(|c| c.set(0));
        SUB_PREV.with(|c| c.set(0));
        SUB_SEEN.with(|c| c.set(0));
        let h = unsafe { CreateWindowExW(0, name.as_ptr(), name.as_ptr(), WS_POPUP, 0, 0, 64, 64, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) };
        assert!(!h.is_null());
        h
    }

    fn send_test(h: HWND) {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(h, TEST_MSG, 7, 9) };
    }

    /// A window wrapped behind another is posted the focus gain, in the order the session
    /// posts it; the window in front isn't.
    #[test]
    fn a_window_wrapped_behind_another_is_told_it_is_active() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW};
        let h = class_window();
        let mut posted = Vec::new();
        let drain = |posted: &mut Vec<(u32, usize)>| {
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            while unsafe { PeekMessageW(&mut msg, h, 0, 0, PM_REMOVE) } != 0 {
                if matches!(msg.message, WM_ACTIVATEAPP | WM_ACTIVATE | WM_SETFOCUS) {
                    posted.push((msg.message, msg.wParam));
                }
            }
        };
        drain(&mut posted);
        posted.clear();
        assert_eq!(post_active_unless_front(h as usize, h as usize), None, "the window in front has the focus already");
        drain(&mut posted);
        assert!(posted.is_empty());
        assert_eq!(post_active_unless_front(h as usize, 0), Some(true));
        drain(&mut posted);
        assert_eq!(posted, [(WM_ACTIVATEAPP, 1), (WM_ACTIVATE, 1), (WM_SETFOCUS, 0)]);
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(h) };
    }

    /// The game reading its wrapped window's procedure gets its own class procedure, so a
    /// class procedure that subclasses whatever isn't itself leaves it be, and each
    /// message reaches it once.
    #[test]
    fn a_wrapped_window_shows_the_game_its_own_procedure() {
        let h = class_window();
        keep_focus(h as usize);
        assert_eq!(unsafe { real_proc(h, true) }, wrapper());
        let seen = proc_seen_by_game(h as isize, true).unwrap();
        assert_eq!(seen, class_proc as Proc as usize as isize);
        send_test(h);
        assert_eq!(CLASS_SEEN.with(|c| c.get()), 1);
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(h) };
        assert!(!is_wrapped_for_test(h as usize));
    }

    /// A procedure the game sets on its wrapped window goes behind the wrapper, and the one
    /// it's told it replaced is the one it read: every procedure sees each message once.
    #[test]
    fn a_procedure_the_game_sets_goes_behind_the_wrapper() {
        let h = class_window();
        keep_focus(h as usize);
        let prev = proc_set_by_game(h as isize, true, sub_proc as Proc as usize as isize).unwrap();
        assert_eq!(prev, class_proc as Proc as usize as isize);
        SUB_PREV.with(|c| c.set(prev));
        assert_eq!(unsafe { real_proc(h, true) }, wrapper(), "the wrapper stays in front");
        assert_eq!(proc_seen_by_game(h as isize, true), Some(sub_proc as Proc as usize as isize));
        send_test(h);
        assert_eq!((SUB_SEEN.with(|c| c.get()), CLASS_SEEN.with(|c| c.get())), (1, 1));
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(h) };
    }

    /// A class procedure that took the wrapper as its previous one past the hooks hands
    /// every message back to it: the wrapper answers with Windows' default instead of
    /// going round until the stack runs out.
    #[test]
    fn a_procedure_handing_messages_back_to_the_wrapper_does_not_loop() {
        use windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW;
        let h = class_window();
        keep_focus(h as usize);
        // What a class procedure that subclasses finds and does, with no hooks in this
        // test process.
        let found = unsafe { real_proc(h, true) };
        assert_eq!(found, wrapper());
        CLASS_PREV.with(|c| c.set(found));
        unsafe { SetWindowLongPtrW(h, GWLP_WNDPROC, class_proc as Proc as usize as _) };
        send_test(h);
        assert_eq!(CLASS_SEEN.with(|c| c.get()), 2, "once from Windows, once from the wrapper");
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(h) };
    }

    /// The game using the other character set's calls on its wrapped window takes the
    /// wrapper off it (their answers are Windows' conversion handles).
    #[test]
    fn the_other_character_set_takes_the_wrapper_off() {
        let h = class_window();
        keep_focus(h as usize);
        assert_eq!(proc_seen_by_game(h as isize, false), None);
        assert!(!is_wrapped_for_test(h as usize));
        assert_eq!(unsafe { real_proc(h, true) }, class_proc as Proc as usize as isize);
        send_test(h);
        assert_eq!(CLASS_SEEN.with(|c| c.get()), 1);
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(h) };
    }

    #[test]
    fn raw_input_devices_get_inputsink() {
        let mut kb = RawInputDevice { usage_page: 1, usage: 6, flags: 0, hwnd_target: 0x100 };
        assert!(upgrade_raw_input_device(&mut kb, 0x200));
        assert_eq!(kb.flags, RIDEV_INPUTSINK);
        assert_eq!(kb.hwnd_target, 0x100);
        // Null target adopts fallback HWND so RIDEV_INPUTSINK is valid.
        let mut ms = RawInputDevice { usage_page: 1, usage: 2, flags: 0, hwnd_target: 0 };
        assert!(upgrade_raw_input_device(&mut ms, 0x200));
        assert_eq!((ms.flags, ms.hwnd_target), (RIDEV_INPUTSINK, 0x200));
        // Controllers too: a background pane's copy reads its own.
        for usage in [4, 5, 8] {
            let mut pad = RawInputDevice { usage_page: 1, usage, flags: 0, hwnd_target: 0x100 };
            assert!(upgrade_raw_input_device(&mut pad, 0x200), "usage {usage}");
            assert_eq!(pad.flags, RIDEV_INPUTSINK);
        }
        // INPUTSINK can't go beside CAPTUREMOUSE or EXINPUTSINK: those go.
        let mut cap = RawInputDevice { usage_page: 1, usage: 2, flags: RIDEV_CAPTUREMOUSE | 0x30, hwnd_target: 0x100 };
        assert!(upgrade_raw_input_device(&mut cap, 0x200));
        assert_eq!(cap.flags, RIDEV_INPUTSINK | 0x30);
        let mut ex = RawInputDevice { usage_page: 1, usage: 6, flags: RIDEV_EXINPUTSINK, hwnd_target: 0x100 };
        assert!(upgrade_raw_input_device(&mut ex, 0x200));
        assert_eq!(ex.flags, RIDEV_INPUTSINK);
        // Removals and other devices are left alone.
        let mut rem = RawInputDevice { usage_page: 1, usage: 6, flags: RIDEV_REMOVE, hwnd_target: 0 };
        assert!(!upgrade_raw_input_device(&mut rem, 0x200));
        let mut consumer = RawInputDevice { usage_page: 0x0C, usage: 1, flags: 0, hwnd_target: 0x100 };
        assert!(!upgrade_raw_input_device(&mut consumer, 0x200));
    }
}
