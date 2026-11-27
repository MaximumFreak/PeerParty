//! Extended Xbox / GIP controllers through `Windows.Gaming.Input.Gamepad`.
//!
//! XInput caps out at 4 slots (`0..3`). `Windows.Gaming.Input` enumerates every connected
//! Xbox / GIP controller (8+ simultaneously) and reads its buttons, triggers, and sticks.
//! In split-screen sessions where each copy runs in its own process and only reads XInput
//! slot 0 through PeerParty's hook, controllers #5+ route cleanly as virtual XInput pads.
//!
//! Loaded dynamically from System32 (`combase.dll`, `LOAD_LIBRARY_SEARCH_SYSTEM32`) so
//! `peerparty.exe` takes no DLL from its own folder. Polled on a dedicated MTA thread
//! stopped cleanly when dropped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use super::pad::{BACK, DOWN, EAST, LB, LEFT, LSTICK, NORTH, RB, RIGHT, RSTICK, SOUTH, START, State, UP, WEST};

/// Maximum number of WGI gamepads read at once (matches `MAX_PANES`).
pub const MAX_WGI_PADS: usize = 16;

/// One controller reported by `Windows.Gaming.Input.Gamepad`.
#[derive(Clone, Debug, PartialEq)]
pub struct WgiPad {
    /// Stable 64-bit device identifier (FNV-1a of `NonRoamableId`, else vendor/product/ordinal).
    pub id: u64,
    pub vid: u16,
    pub pid: u16,
    /// Display name from `IRawGameController2::DisplayName` (empty if unavailable).
    pub product: String,
    pub state: State,
}

/// Converts a 64-byte `Windows.Gaming.Input.GamepadReading` into `pad::State`.
/// Layout: `u64 Timestamp` (0), `u32 Buttons` (8), 4B pad (12), `f64 LeftTrigger` (16),
/// `f64 RightTrigger` (24), `f64 LeftThumbstickX` (32), `f64 LeftThumbstickY` (40),
/// `f64 RightThumbstickX` (48), `f64 RightThumbstickY` (56).
pub fn parse_reading(buf: &[u8; 64]) -> State {
    let wgi = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let f = |at: usize| f64::from_le_bytes(buf[at..at + 8].try_into().unwrap()) as f32;
    State {
        buttons: logical_buttons(wgi),
        lt: f(16).clamp(0.0, 1.0),
        rt: f(24).clamp(0.0, 1.0),
        lx: f(32).clamp(-1.0, 1.0),
        ly: f(40).clamp(-1.0, 1.0),
        rx: f(48).clamp(-1.0, 1.0),
        ry: f(56).clamp(-1.0, 1.0),
    }
}

/// Converts `Windows.Gaming.Input.GamepadButtons` into `pad.rs`'s logical button bits.
pub fn logical_buttons(wgi: u32) -> u32 {
    const MAP: [(u32, u32); 14] = [
        (0x0001, START),  // Menu
        (0x0002, BACK),   // View
        (0x0004, SOUTH),  // A
        (0x0008, EAST),   // B
        (0x0010, WEST),   // X
        (0x0020, NORTH),  // Y
        (0x0040, UP),     // DPadUp
        (0x0080, DOWN),   // DPadDown
        (0x0100, LEFT),   // DPadLeft
        (0x0200, RIGHT),  // DPadRight
        (0x0400, LB),     // LeftShoulder
        (0x0800, RB),     // RightShoulder
        (0x1000, LSTICK), // LeftThumbstick
        (0x2000, RSTICK), // RightThumbstick
    ];
    MAP.iter().filter(|(w, _)| wgi & w != 0).fold(0, |acc, (_, l)| acc | l)
}

/// Stable 64-bit FNV-1a hash for a WGI device id string (never 0).
pub fn stable_id(non_roamable_id: &str, vid: u16, pid: u16, ordinal: usize) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let feed = |h: &mut u64, bytes: &[u8]| {
        for &b in bytes {
            *h ^= u64::from(b);
            *h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    let trimmed = non_roamable_id.trim();
    if !trimmed.is_empty() {
        feed(&mut h, b"wgi:");
        feed(&mut h, trimmed.as_bytes());
    } else {
        feed(&mut h, b"wgi-ord:");
        feed(&mut h, &vid.to_le_bytes());
        feed(&mut h, &pid.to_le_bytes());
        feed(&mut h, &(ordinal as u32).to_le_bytes());
    }
    if h == 0 { 1 } else { h }
}

/// Background reader for `Windows.Gaming.Input.Gamepad`.
pub struct WgiPads {
    list: Arc<Mutex<Vec<WgiPad>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WgiPads {
    #[cfg(windows)]
    pub fn start() -> WgiPads {
        let list = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, s) = (list.clone(), stop.clone());
        let thread = std::thread::Builder::new().name("peerparty-wgi-pads".into()).spawn(move || win::run(l, s)).ok();
        WgiPads { list, stop, thread }
    }

    #[cfg(not(windows))]
    pub fn start() -> WgiPads {
        WgiPads { list: Arc::new(Mutex::new(Vec::new())), stop: Arc::new(AtomicBool::new(false)), thread: None }
    }

    /// Empty reader with no background thread (used in unit tests).
    #[cfg(test)]
    pub fn empty() -> WgiPads {
        WgiPads { list: Arc::new(Mutex::new(Vec::new())), stop: Arc::new(AtomicBool::new(false)), thread: None }
    }

    /// Injects synthetic WGI gamepads in unit tests.
    #[cfg(test)]
    pub fn set_fake(&self, pads: Vec<WgiPad>) {
        *self.list.lock().unwrap_or_else(|e| e.into_inner()) = pads;
    }

    /// Latest snapshot of connected WGI gamepads.
    pub fn snapshot(&self) -> Vec<WgiPad> {
        self.list.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for WgiPads {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

    use super::{MAX_WGI_PADS, WgiPad, parse_reading, stable_id};

    const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
        let (a, b, c) = (d1.to_le_bytes(), d2.to_le_bytes(), d3.to_le_bytes());
        [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]]
    }

    const IID_GAMEPAD_STATICS: [u8; 16] = guid(0x8BBCE529, 0xD49C, 0x39E9, [0x95, 0x60, 0xE4, 0x7D, 0xDE, 0x96, 0xB7, 0xC8]);
    const IID_RAW_STATICS: [u8; 16] = guid(0xEB8D0792, 0xE95A, 0x4B19, [0xAF, 0xC7, 0x0A, 0x59, 0xF8, 0xBF, 0x75, 0x9E]);
    const IID_IGAME_CONTROLLER: [u8; 16] = guid(0x1BAF6522, 0x5F64, 0x42C5, [0x82, 0x67, 0xB9, 0xFE, 0x22, 0x15, 0xBF, 0xBD]);
    const IID_IRAW_GAME_CONTROLLER2: [u8; 16] = guid(0x43C0C035, 0xBB73, 0x4756, [0xA7, 0x87, 0x3E, 0xD6, 0xBE, 0xA6, 0x17, 0xBD]);

    type RoInit = unsafe extern "system" fn(i32) -> i32;
    type RoUninit = unsafe extern "system" fn();
    type WinCreateString = unsafe extern "system" fn(*const u16, u32, *mut usize) -> i32;
    type WinDeleteString = unsafe extern "system" fn(usize) -> i32;
    type WinGetStringRawBuffer = unsafe extern "system" fn(usize, *mut u32) -> *const u16;
    type RoGetFactory = unsafe extern "system" fn(usize, *const [u8; 16], *mut usize) -> i32;

    struct ComBase {
        init: RoInit,
        uninit: RoUninit,
        create_string: WinCreateString,
        delete_string: WinDeleteString,
        raw_buffer: WinGetStringRawBuffer,
        factory: RoGetFactory,
    }

    impl ComBase {
        fn load() -> Option<ComBase> {
            let wide: Vec<u16> = "combase.dll".encode_utf16().chain(Some(0)).collect();
            let m = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
            if m.is_null() {
                return None;
            }
            let sym = |name: &std::ffi::CStr| unsafe { GetProcAddress(m, name.as_ptr().cast()) };
            Some(ComBase {
                init: unsafe { std::mem::transmute(sym(c"RoInitialize")?) },
                uninit: unsafe { std::mem::transmute(sym(c"RoUninitialize")?) },
                create_string: unsafe { std::mem::transmute(sym(c"WindowsCreateString")?) },
                delete_string: unsafe { std::mem::transmute(sym(c"WindowsDeleteString")?) },
                raw_buffer: unsafe { std::mem::transmute(sym(c"WindowsGetStringRawBuffer")?) },
                factory: unsafe { std::mem::transmute(sym(c"RoGetActivationFactory")?) },
            })
        }

        unsafe fn statics(&self, class: &str, iid: &[u8; 16]) -> usize {
            let wide: Vec<u16> = class.encode_utf16().collect();
            let mut hs = 0usize;
            if unsafe { (self.create_string)(wide.as_ptr(), wide.len() as u32, &mut hs) } < 0 {
                return 0;
            }
            let mut out = 0usize;
            let hr = unsafe { (self.factory)(hs, iid, &mut out) };
            unsafe { (self.delete_string)(hs) };
            if hr < 0 { 0 } else { out }
        }

        unsafe fn take_hstring(&self, hs: usize) -> String {
            if hs == 0 {
                return String::new();
            }
            let mut len = 0u32;
            let ptr = unsafe { (self.raw_buffer)(hs, &mut len) };
            let s = if !ptr.is_null() && len > 0 {
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len as usize) })
            } else {
                String::new()
            };
            unsafe { (self.delete_string)(hs) };
            s
        }
    }

    unsafe fn slot(obj: usize, i: usize) -> usize {
        unsafe { *(*(obj as *const *const usize)).add(i) }
    }

    unsafe fn release(obj: usize) {
        if obj != 0 {
            let f: unsafe extern "system" fn(usize) -> u32 = unsafe { std::mem::transmute(slot(obj, 2)) };
            unsafe { f(obj) };
        }
    }

    unsafe fn query_interface(obj: usize, iid: &[u8; 16]) -> usize {
        if obj == 0 {
            return 0;
        }
        let qi: unsafe extern "system" fn(usize, *const [u8; 16], *mut usize) -> i32 = unsafe { std::mem::transmute(slot(obj, 0)) };
        let mut out = 0usize;
        if unsafe { qi(obj, iid, &mut out) } >= 0 { out } else { 0 }
    }

    /// Reads `(vid, pid, non_roamable_id, display_name)` for a `Gamepad` via
    /// `RawGameController::FromGameController` + `IRawGameController2`.
    unsafe fn raw_meta(com: &ComBase, raw_statics: usize, gamepad: usize) -> (u16, u16, String, String) {
        if raw_statics == 0 || gamepad == 0 {
            return (0, 0, String::new(), String::new());
        }
        let gc = unsafe { query_interface(gamepad, &IID_IGAME_CONTROLLER) };
        if gc == 0 {
            return (0, 0, String::new(), String::new());
        }
        // IRawGameControllerStatics::FromGameController is vtable slot 11.
        let from_gc: unsafe extern "system" fn(usize, usize, *mut usize) -> i32 = unsafe { std::mem::transmute(slot(raw_statics, 11)) };
        let mut raw = 0usize;
        let hr = unsafe { from_gc(raw_statics, gc, &mut raw) };
        unsafe { release(gc) };
        if hr < 0 || raw == 0 {
            return (0, 0, String::new(), String::new());
        }
        let get_pid: unsafe extern "system" fn(usize, *mut u16) -> i32 = unsafe { std::mem::transmute(slot(raw, 9)) };
        let get_vid: unsafe extern "system" fn(usize, *mut u16) -> i32 = unsafe { std::mem::transmute(slot(raw, 10)) };
        let (mut vid, mut pid) = (0u16, 0u16);
        let _ = unsafe { get_vid(raw, &mut vid) };
        let _ = unsafe { get_pid(raw, &mut pid) };
        let mut non_roamable = String::new();
        let mut display = String::new();
        let raw2 = unsafe { query_interface(raw, &IID_IRAW_GAME_CONTROLLER2) };
        if raw2 != 0 {
            let get_id: unsafe extern "system" fn(usize, *mut usize) -> i32 = unsafe { std::mem::transmute(slot(raw2, 7)) };
            let get_name: unsafe extern "system" fn(usize, *mut usize) -> i32 = unsafe { std::mem::transmute(slot(raw2, 8)) };
            let mut hs = 0usize;
            if unsafe { get_id(raw2, &mut hs) } >= 0 {
                non_roamable = unsafe { com.take_hstring(hs) };
            }
            let mut hs_name = 0usize;
            if unsafe { get_name(raw2, &mut hs_name) } >= 0 {
                display = unsafe { com.take_hstring(hs_name) };
            }
            unsafe { release(raw2) };
        }
        unsafe { release(raw) };
        (vid, pid, non_roamable, display.trim().to_string())
    }

    pub(super) fn run(list: Arc<Mutex<Vec<WgiPad>>>, stop: Arc<AtomicBool>) {
        let Some(com) = ComBase::load() else { return };
        // RO_INIT_MULTITHREADED (1).
        let inited = unsafe { (com.init)(1) } >= 0;
        let gp_statics = unsafe { com.statics("Windows.Gaming.Input.Gamepad", &IID_GAMEPAD_STATICS) };
        let raw_statics = unsafe { com.statics("Windows.Gaming.Input.RawGameController", &IID_RAW_STATICS) };
        if gp_statics != 0 {
            // IGamepadStatics::get_Gamepads is slot 10.
            let get_gamepads: unsafe extern "system" fn(usize, *mut usize) -> i32 = unsafe { std::mem::transmute(slot(gp_statics, 10)) };
            while !stop.load(Ordering::Acquire) {
                let mut out = Vec::new();
                let mut view = 0usize;
                if unsafe { get_gamepads(gp_statics, &mut view) } >= 0 && view != 0 {
                    let get_size: unsafe extern "system" fn(usize, *mut u32) -> i32 = unsafe { std::mem::transmute(slot(view, 7)) };
                    let get_at: unsafe extern "system" fn(usize, u32, *mut usize) -> i32 = unsafe { std::mem::transmute(slot(view, 6)) };
                    let mut n = 0u32;
                    if unsafe { get_size(view, &mut n) } >= 0 {
                        let count = (n as usize).min(MAX_WGI_PADS);
                        for idx in 0..count {
                            let mut gp = 0usize;
                            if unsafe { get_at(view, idx as u32, &mut gp) } < 0 || gp == 0 {
                                continue;
                            }
                            let read_fn: unsafe extern "system" fn(usize, *mut [u8; 64]) -> i32 = unsafe { std::mem::transmute(slot(gp, 8)) };
                            let mut buf = [0u8; 64];
                            if unsafe { read_fn(gp, &mut buf) } >= 0 {
                                let (vid, pid, nrid, product) = unsafe { raw_meta(&com, raw_statics, gp) };
                                let id = stable_id(&nrid, vid, pid, idx);
                                out.push(WgiPad { id, vid, pid, product, state: parse_reading(&buf) });
                            }
                            unsafe { release(gp) };
                        }
                    }
                    unsafe { release(view) };
                }
                *list.lock().unwrap_or_else(|e| e.into_inner()) = out;
                // Poll briskly when controllers exist, gently when idle.
                let sleep_ms = if list.lock().unwrap_or_else(|e| e.into_inner()).is_empty() { 250 } else { 8 };
                std::thread::sleep(Duration::from_millis(sleep_ms));
            }
        }
        unsafe {
            release(raw_statics);
            release(gp_statics);
            if inited {
                (com.uninit)();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wgi_reading_decodes_buttons_triggers_and_sticks() {
        let mut buf = [0u8; 64];
        // A (0x0004) + Menu (0x0001) + DPadRight (0x0200) + LeftShoulder (0x0400) + RightThumbstick (0x2000).
        let btns: u32 = 0x0004 | 0x0001 | 0x0200 | 0x0400 | 0x2000;
        buf[8..12].copy_from_slice(&btns.to_le_bytes());
        buf[16..24].copy_from_slice(&0.75f64.to_le_bytes());
        buf[24..32].copy_from_slice(&1.0f64.to_le_bytes());
        buf[32..40].copy_from_slice(&(-0.5f64).to_le_bytes());
        buf[40..48].copy_from_slice(&0.8f64.to_le_bytes());
        buf[48..56].copy_from_slice(&0.25f64.to_le_bytes());
        buf[56..64].copy_from_slice(&(-1.0f64).to_le_bytes());
        let s = parse_reading(&buf);
        assert_eq!(s.buttons, SOUTH | START | RIGHT | LB | RSTICK);
        assert!((s.lt - 0.75).abs() < 0.01 && (s.rt - 1.0).abs() < 0.01);
        assert!((s.lx + 0.5).abs() < 0.01 && (s.ly - 0.8).abs() < 0.01);
        assert!((s.rx - 0.25).abs() < 0.01 && (s.ry + 1.0).abs() < 0.01);
    }

    #[test]
    fn stable_id_is_deterministic_and_nonzero() {
        let a = stable_id("HID#VID_045E&PID_0B12#1234", 0x045E, 0x0B12, 0);
        let b = stable_id("HID#VID_045E&PID_0B12#1234", 0x045E, 0x0B12, 4);
        let c = stable_id("HID#VID_045E&PID_0B12#5678", 0x045E, 0x0B12, 0);
        assert_ne!(a, 0);
        assert_eq!(a, b, "non-roamable id stays stable across enumerations");
        assert_ne!(a, c);
        let o0 = stable_id("", 0x045E, 0x0B12, 0);
        let o1 = stable_id("", 0x045E, 0x0B12, 1);
        assert_ne!(o0, 0);
        assert_ne!(o0, o1);
    }
}
