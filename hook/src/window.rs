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
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClientRect, GetWindow, GetWindowLongW,
    GetWindowThreadProcessId, IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOZORDER, SetWindowLongW, SetWindowPos, ShowWindow, WS_BORDER, WS_CAPTION,
    WS_CHILD, WS_DLGFRAME, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX,
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

/// Loads `PEERPARTY_INSTANCE` (if set) at start-up.
pub fn init_from_env() {
    if let Some(spec) = InstanceSpec::from_env() {
        // Its player is the session's slot for it, claimed when the engine starts.
        peerparty_core::want_slot(spec.slot);
        apply_spec(spec);
    }
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
        if wants_hooks() && (prev == 0 || !RAW_SINKS_UPGRADED.load(Ordering::Relaxed)) {
            upgrade_registered_raw_input_sinks(hwnd as isize);
        }
        if prev != hwnd && FOCUS_SPOOF.load(Ordering::Relaxed) {
            keep_focus(hwnd);
        }
    }
}

/// Game windows whose window procedure is wrapped (`focus_proc`): the window, and the
/// procedure it had.
static WRAPPED: [(AtomicUsize, AtomicUsize); 4] = [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; 4];

/// Wraps game window `hwnd`'s procedure so the focus losses Windows tells it about don't
/// reach it (`should_suppress_focus_loss`). The session's window takes the focus from
/// every copy; a copy told it lost it stops its menus taking input (Unity's UI ignores
/// the mouse and controllers then), though the game itself goes on.
fn keep_focus(hwnd: usize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GWLP_WNDPROC, SetWindowLongPtrW};
    if WRAPPED.iter().any(|(h, _)| h.load(Ordering::Acquire) == hwnd) {
        return;
    }
    let Some(slot) = WRAPPED.iter().find(|(h, _)| h.load(Ordering::Acquire) == 0) else { return };
    // The procedure is known before the window can call the wrapper.
    let old = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd as HWND, GWLP_WNDPROC) };
    if old == 0 {
        return;
    }
    slot.1.store(old as usize, Ordering::Release);
    slot.0.store(hwnd, Ordering::Release);
    type Proc = unsafe extern "system" fn(HWND, u32, usize, isize) -> isize;
    let was = unsafe { SetWindowLongPtrW(hwnd as HWND, GWLP_WNDPROC, focus_proc as Proc as usize as _) };
    if was == 0 {
        slot.0.store(0, Ordering::Release);
        return;
    }
    slot.1.store(was as usize, Ordering::Release);
    log!("window: the game's window (0x{hwnd:x}) isn't told when it loses the focus (split screen: every copy keeps it)");
}

unsafe extern "system" fn focus_proc(h: HWND, m: u32, w: usize, l: isize) -> isize {
    let old = WRAPPED.iter().find(|(x, _)| x.load(Ordering::Acquire) == h as usize).map_or(0, |(_, p)| p.load(Ordering::Acquire));
    if should_suppress_focus_loss(m, w, FOCUS_SPOOF.load(Ordering::Relaxed)) {
        static TOLD: AtomicBool = AtomicBool::new(false);
        if !TOLD.swap(true, Ordering::Relaxed) {
            log!("window: a focus loss (message 0x{m:x}) didn't reach the game");
        }
        return 0;
    }
    let call = |m: u32, w: usize, l: isize| -> isize {
        if old == 0 {
            return unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW(h, m, w, l) };
        }
        let proc: windows_sys::Win32::UI::WindowsAndMessaging::WNDPROC = unsafe { std::mem::transmute::<usize, windows_sys::Win32::UI::WindowsAndMessaging::WNDPROC>(old) };
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::CallWindowProcW(proc, h, m, w, l) }
    };
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
    call(m, w, l)
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
    apply_spec(spec);
    if wants_hooks() {
        install_loaded_user32();
    }
    if let Some(pane) = pane {
        reposition_window(pane);
    }
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

/// A display mode change while this copy is in a pane: it would change the monitor
/// under every pane, so it is answered as done and nothing changes. `None` passes it on.
fn keep_display_mode(mode: usize) -> Option<i32> {
    if !in_pane() {
        return None;
    }
    if mode != 0 && !TOLD_DISPLAY_MODE.swap(true, Ordering::Relaxed) {
        log!("window: the game changes the display mode (fullscreen): left as it is, as this copy is in a pane");
    }
    // DISP_CHANGE_SUCCESSFUL.
    Some(0)
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
        if pid != f.pid || unsafe { IsWindowVisible(h) } == 0 || !unsafe { GetWindow(h, GW_OWNER) }.is_null() {
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

fn reposition_window(pane: PaneRect) {
    if pane.width == 0 || pane.height == 0 {
        return;
    }
    let mut hwnd = PRIMARY_HWND.load(Ordering::Relaxed);
    if hwnd == 0 {
        hwnd = find_own_window();
        if hwnd != 0 {
            PRIMARY_HWND.store(hwnd, Ordering::Relaxed);
        }
    }
    if hwnd == 0 {
        return;
    }
    let h = hwnd as HWND;
    unsafe {
        let style = GetWindowLongW(h, GWL_STYLE) as u32;
        let want_style = (style & !CHROME_STYLES) | WS_POPUP;
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        let want_ex = (ex | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) & !WS_EX_APPWINDOW;
        let toggle = IsWindowVisible(h) != 0 && ((ex & WS_EX_TOOLWINDOW) == 0 || (ex & WS_EX_APPWINDOW) != 0);
        if toggle {
            ShowWindow(h, SW_HIDE);
        }
        if want_style != style {
            SetWindowLongW(h, GWL_STYLE, want_style as i32);
        }
        if want_ex != ex {
            SetWindowLongW(h, GWL_EXSTYLE, want_ex as i32);
        }
        if toggle {
            ShowWindow(h, SW_SHOWNOACTIVATE);
        }
        SetWindowPos(
            h,
            std::ptr::null_mut(),
            pane.x,
            pane.y,
            pane.width as i32,
            pane.height as i32,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
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
        let mut n = 0usize;
        macro_rules! hook_proc {
            ($cell:expr, $sym:literal, $ty:ty, $detour:ident) => {
                if let Some(a) = proc_addr(u32_mod, $sym) {
                    let target: $ty = unsafe { std::mem::transmute(a) };
                    // user32 is loaded and the game may be running in these: its other
                    // threads are paused while each prologue is written.
                    if crate::sys::detour::try_arm(&$cell, target, $detour as $ty, "window", $sym.to_str().unwrap_or(""), true) == crate::sys::detour::Armed::Hooked {
                        n += 1;
                    }
                }
            };
        }
        hook_proc!(GET_METRICS, c"GetSystemMetrics", GetSystemMetricsFn, detour_get_system_metrics);
        hook_proc!(MONITOR_INFO_W, c"GetMonitorInfoW", GetMonitorInfoWFn, detour_get_monitor_info_w);
        hook_proc!(MONITOR_INFO_A, c"GetMonitorInfoA", GetMonitorInfoAFn, detour_get_monitor_info_a);
        hook_proc!(ENUM_DISP_W, c"EnumDisplaySettingsW", EnumDisplaySettingsWFn, detour_enum_display_w);
        hook_proc!(ENUM_DISP_A, c"EnumDisplaySettingsA", EnumDisplaySettingsAFn, detour_enum_display_a);
        hook_proc!(GET_FOREGROUND, c"GetForegroundWindow", GetForegroundWindowFn, detour_get_foreground);
        hook_proc!(GET_ACTIVE, c"GetActiveWindow", GetActiveWindowFn, detour_get_active);
        hook_proc!(GET_FOCUS, c"GetFocus", GetFocusFn, detour_get_focus);
        hook_proc!(REGISTER_RAW_INPUT, c"RegisterRawInputDevices", RegisterRawInputDevicesFn, detour_register_raw_input);
        hook_proc!(CREATE_WINDOW_EX_W, c"CreateWindowExW", CreateWindowExWFn, detour_create_window_ex_w);
        hook_proc!(TRANSLATE, c"TranslateMessage", TranslateMessageFn, detour_translate_message);
        n += crate::pointer::hook_loaded(|s| proc_addr(u32_mod, s));
        hook_proc!(TRACK_MOUSE, c"TrackMouseEvent", TrackMouseEventFn, detour_track_mouse);
        hook_proc!(DISPATCH_W, c"DispatchMessageW", DispatchMessageWFn, detour_dispatch_w);
        hook_proc!(CHANGE_DISPLAY_EX_W, c"ChangeDisplaySettingsExW", ChangeDisplayExFn, detour_change_display_ex_w);
        hook_proc!(CHANGE_DISPLAY_EX_A, c"ChangeDisplaySettingsExA", ChangeDisplayExFn, detour_change_display_ex_a);
        hook_proc!(CHANGE_DISPLAY_W, c"ChangeDisplaySettingsW", ChangeDisplayFn, detour_change_display_w);
        hook_proc!(CHANGE_DISPLAY_A, c"ChangeDisplaySettingsA", ChangeDisplayFn, detour_change_display_a);
        log!("window: hooked {n} user32 viewport/focus entry points");
    });
}

fn hook_exports(exports: &[Export]) -> usize {
    let mut n = 0;
    let mut one = |name: &str, f: Box<dyn FnOnce(*const ()) -> bool>| {
        n += crate::sys::detour::hook_one(exports, "window", name, f);
    };
    one("GetSystemMetrics", Box::new(|a| crate::sys::detour::arm(&GET_METRICS, unsafe { std::mem::transmute::<*const (), GetSystemMetricsFn>(a) }, detour_get_system_metrics, "window", "GetSystemMetrics")));
    one("GetMonitorInfoW", Box::new(|a| crate::sys::detour::arm(&MONITOR_INFO_W, unsafe { std::mem::transmute::<*const (), GetMonitorInfoWFn>(a) }, detour_get_monitor_info_w, "window", "GetMonitorInfoW")));
    one("GetMonitorInfoA", Box::new(|a| crate::sys::detour::arm(&MONITOR_INFO_A, unsafe { std::mem::transmute::<*const (), GetMonitorInfoAFn>(a) }, detour_get_monitor_info_a, "window", "GetMonitorInfoA")));
    one("EnumDisplaySettingsW", Box::new(|a| crate::sys::detour::arm(&ENUM_DISP_W, unsafe { std::mem::transmute::<*const (), EnumDisplaySettingsWFn>(a) }, detour_enum_display_w, "window", "EnumDisplaySettingsW")));
    one("EnumDisplaySettingsA", Box::new(|a| crate::sys::detour::arm(&ENUM_DISP_A, unsafe { std::mem::transmute::<*const (), EnumDisplaySettingsAFn>(a) }, detour_enum_display_a, "window", "EnumDisplaySettingsA")));
    one("GetForegroundWindow", Box::new(|a| crate::sys::detour::arm(&GET_FOREGROUND, unsafe { std::mem::transmute::<*const (), GetForegroundWindowFn>(a) }, detour_get_foreground, "window", "GetForegroundWindow")));
    one("GetActiveWindow", Box::new(|a| crate::sys::detour::arm(&GET_ACTIVE, unsafe { std::mem::transmute::<*const (), GetActiveWindowFn>(a) }, detour_get_active, "window", "GetActiveWindow")));
    one("GetFocus", Box::new(|a| crate::sys::detour::arm(&GET_FOCUS, unsafe { std::mem::transmute::<*const (), GetFocusFn>(a) }, detour_get_focus, "window", "GetFocus")));
    one("RegisterRawInputDevices", Box::new(|a| crate::sys::detour::arm(&REGISTER_RAW_INPUT, unsafe { std::mem::transmute::<*const (), RegisterRawInputDevicesFn>(a) }, detour_register_raw_input, "window", "RegisterRawInputDevices")));
    one("CreateWindowExW", Box::new(|a| crate::sys::detour::arm(&CREATE_WINDOW_EX_W, unsafe { std::mem::transmute::<*const (), CreateWindowExWFn>(a) }, detour_create_window_ex_w, "window", "CreateWindowExW")));
    one("TranslateMessage", Box::new(|a| crate::sys::detour::arm(&TRANSLATE, unsafe { std::mem::transmute::<*const (), TranslateMessageFn>(a) }, detour_translate_message, "window", "TranslateMessage")));
    one("TrackMouseEvent", Box::new(|a| crate::sys::detour::arm(&TRACK_MOUSE, unsafe { std::mem::transmute::<*const (), TrackMouseEventFn>(a) }, detour_track_mouse, "window", "TrackMouseEvent")));
    one("DispatchMessageW", Box::new(|a| crate::sys::detour::arm(&DISPATCH_W, unsafe { std::mem::transmute::<*const (), DispatchMessageWFn>(a) }, detour_dispatch_w, "window", "DispatchMessageW")));
    one("ChangeDisplaySettingsExW", Box::new(|a| crate::sys::detour::arm(&CHANGE_DISPLAY_EX_W, unsafe { std::mem::transmute::<*const (), ChangeDisplayExFn>(a) }, detour_change_display_ex_w, "window", "ChangeDisplaySettingsExW")));
    one("ChangeDisplaySettingsExA", Box::new(|a| crate::sys::detour::arm(&CHANGE_DISPLAY_EX_A, unsafe { std::mem::transmute::<*const (), ChangeDisplayExFn>(a) }, detour_change_display_ex_a, "window", "ChangeDisplaySettingsExA")));
    one("ChangeDisplaySettingsW", Box::new(|a| crate::sys::detour::arm(&CHANGE_DISPLAY_W, unsafe { std::mem::transmute::<*const (), ChangeDisplayFn>(a) }, detour_change_display_w, "window", "ChangeDisplaySettingsW")));
    one("ChangeDisplaySettingsA", Box::new(|a| crate::sys::detour::arm(&CHANGE_DISPLAY_A, unsafe { std::mem::transmute::<*const (), ChangeDisplayFn>(a) }, detour_change_display_a, "window", "ChangeDisplaySettingsA")));
    n + crate::pointer::hook_exports(exports)
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

const WM_DEVICECHANGE: u32 = 0x0219;
const DBT_DEVNODES_CHANGED: usize = 0x0007;

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
    for h in c.hwnds {
        if unsafe { PostMessageW(h as HWND, WM_DEVICECHANGE, DBT_DEVNODES_CHANGED, 0) } != 0 {
            posted.push(format!("0x{h:x}"));
        }
    }
    log!("window: told {} game window(s) [{}] that controllers changed (WM_DEVICECHANGE; {message_only} of them message-only)", posted.len(), posted.join(", "));
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
    let Some((w, h)) = virtual_size() else { return };
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
    let Some((w, h)) = virtual_size() else { return };
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
    spoof_system_metric(index, real, virtual_size())
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
    if !spoof {
        return real;
    }
    spoof_focus_hwnd(real, is_our_window(real), PRIMARY_HWND.load(Ordering::Relaxed) as isize, true)
}

unsafe extern "system" fn detour_get_active() -> isize {
    let real = GET_ACTIVE.get().map_or(0, |h| unsafe { h.call() });
    let spoof = FOCUS_SPOOF.load(Ordering::Relaxed);
    if !spoof {
        return real;
    }
    spoof_focus_hwnd(real, is_our_window(real), PRIMARY_HWND.load(Ordering::Relaxed) as isize, true)
}

unsafe extern "system" fn detour_get_focus() -> isize {
    let real = GET_FOCUS.get().map_or(0, |h| unsafe { h.call() });
    let spoof = FOCUS_SPOOF.load(Ordering::Relaxed);
    if !spoof {
        return real;
    }
    spoof_focus_hwnd(real, is_our_window(real), PRIMARY_HWND.load(Ordering::Relaxed) as isize, true)
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
    let mut contained = false;
    if let (Some((vw, vh)), Some(pane)) = (virtual_size(), current_pane())
        && should_contain_created_window(ex_style, style, w, h, parent)
    {
        style = (style & !CHROME_STYLES) | WS_POPUP;
        ex_style = (ex_style | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) & !WS_EX_APPWINDOW;
        x = pane.x;
        y = pane.y;
        w = vw as i32;
        h = vh as i32;
        contained = true;
    }
    let hwnd = unsafe { hook.call(ex_style, class_name, window_name, style, x, y, w, h, parent, menu, instance, param) };
    if contained && hwnd != 0 {
        note_game_window(hwnd as usize);
    }
    hwnd
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
        // Removals and other devices are left alone.
        let mut rem = RawInputDevice { usage_page: 1, usage: 6, flags: RIDEV_REMOVE, hwnd_target: 0 };
        assert!(!upgrade_raw_input_device(&mut rem, 0x200));
        let mut consumer = RawInputDevice { usage_page: 0x0C, usage: 1, flags: 0, hwnd_target: 0x100 };
        assert!(!upgrade_raw_input_device(&mut consumer, 0x200));
    }
}
