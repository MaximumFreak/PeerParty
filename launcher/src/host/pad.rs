//! Controllers, read in the launcher's process (the game's own reads are held back by
//! the hook's padblock.rs while the overlay shows):
//!
//! - XInput (Xbox controllers, and anything Steam Input or DS4Windows presents as one),
//!   through `XInputGetStateEx` (ordinal 100) for the Guide button.
//! - HID, one reader thread per device: DualShock 4, DualSense, Switch Pro (Bluetooth's
//!   simple 0x3F report, and the full 0x30 report when something else has turned it on)
//!   and any other HID gamepad through the HID parser (buttons, hat, X/Y). Devices that
//!   are also XInput devices (`IG_` in their path) are skipped so they don't count twice.
//!
//! Every pad maps to one logical layout: south / east / west / north face buttons, the
//! shoulders, back / start / home, the d-pad and the left stick. `Style` says which
//! glyphs to show for the pad that was used last. A Home press is noticed per pad
//! (`home_pressed`), so any connected controller opens the overlay, whatever the others
//! are holding. It counts only after that pad's Home was seen up: a pad that appears
//! with Home down (Steam Input swapping its own pad in as a Steam game starts), or Home
//! still down from closing Game Bar outside the game (`hold_home`), opens nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SOUTH: u32 = 1 << 0;
pub const EAST: u32 = 1 << 1;
pub const WEST: u32 = 1 << 2;
pub const NORTH: u32 = 1 << 3;
pub const LB: u32 = 1 << 4;
pub const RB: u32 = 1 << 5;
pub const BACK: u32 = 1 << 6;
pub const START: u32 = 1 << 7;
pub const HOME: u32 = 1 << 8;
pub const UP: u32 = 1 << 9;
pub const DOWN: u32 = 1 << 10;
pub const LEFT: u32 = 1 << 11;
pub const RIGHT: u32 = 1 << 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Style {
    #[default]
    Xbox,
    PlayStation,
    Nintendo,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct State {
    pub buttons: u32,
    /// Left stick, -1..1, up positive.
    pub lx: f32,
    pub ly: f32,
}

impl State {
    /// The stick counts as the d-pad past half way.
    pub fn with_stick(self) -> u32 {
        let mut b = self.buttons;
        if self.ly > 0.5 {
            b |= UP;
        }
        if self.ly < -0.5 {
            b |= DOWN;
        }
        if self.lx < -0.5 {
            b |= LEFT;
        }
        if self.lx > 0.5 {
            b |= RIGHT;
        }
        b
    }
}

/// `XInputGetStateEx`'s state: `XINPUT_STATE` and a DWORD of padding (20 bytes; a 16-byte
/// `XINPUT_STATE` buffer is overrun).
const STATE_LEN: usize = 20;

type GetStateFn = unsafe extern "system" fn(u32, *mut [u8; STATE_LEN]) -> u32;

struct Hid {
    path: String,
    vid: u16,
    pid: u16,
    style: Style,
    state: Arc<Mutex<State>>,
    alive: Arc<AtomicBool>,
    /// Home was down at the last poll, or hasn't been seen up yet.
    home: bool,
}

pub struct Pads {
    xinput: Option<GetStateFn>,
    /// When each XInput slot was last tried while disconnected (a call on an empty slot
    /// is slow, so those are tried every two seconds).
    tried: [Option<Instant>; 4],
    connected: [bool; 4],
    hid: Arc<Mutex<Vec<Hid>>>,
    last_scan: Option<Instant>,
    pub style: Style,
    last_buttons: u32,
    /// Home was down at the last poll, or hasn't been seen up yet, per XInput slot.
    x_home: [bool; 4],
    /// Some controller's Home went down since the last poll.
    pub home_pressed: bool,
    /// Which controller that was, for the log.
    pub home_by: String,
}

impl Pads {
    pub fn new() -> Pads {
        Pads {
            xinput: xinput(),
            tried: [None; 4],
            connected: [false; 4],
            hid: Arc::new(Mutex::new(Vec::new())),
            last_scan: None,
            style: Style::Xbox,
            last_buttons: 0,
            x_home: [true; 4],
            home_pressed: false,
            home_by: String::new(),
        }
    }

    /// Home held on every controller: it has to come up before it counts again.
    pub fn hold_home(&mut self) {
        self.x_home = [true; 4];
        for h in self.hid.lock().unwrap().iter_mut() {
            h.home = true;
        }
    }

    /// Every pad's buttons together, the stick folded into the d-pad.
    pub fn poll(&mut self) -> u32 {
        let mut all = 0u32;
        self.home_pressed = false;
        if self.last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            self.last_scan = Some(Instant::now());
            let hid = self.hid.clone();
            std::thread::spawn(move || scan_hid(&hid));
        }
        if let Some(get) = self.xinput {
            for pad in 0..4u32 {
                let i = pad as usize;
                if !self.connected[i] && self.tried[i].is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    continue;
                }
                let mut s = [0u8; STATE_LEN];
                let ok = unsafe { get(pad, &mut s) } == 0;
                self.connected[i] = ok;
                if !ok {
                    self.tried[i] = Some(Instant::now());
                    self.x_home[i] = true;
                    continue;
                }
                let b = from_xinput(&s);
                let home = b.buttons & HOME != 0;
                if home && !self.x_home[i] {
                    self.home_pressed = true;
                    self.home_by = format!("XInput slot {pad}");
                }
                self.x_home[i] = home;
                if b.buttons != 0 && b.buttons != self.last_buttons {
                    self.style = Style::Xbox;
                }
                all |= b.with_stick();
            }
        }
        let mut hid = self.hid.lock().unwrap();
        hid.retain(|h| h.alive.load(Ordering::Relaxed));
        for h in hid.iter_mut() {
            let s = *h.state.lock().unwrap();
            let home = s.buttons & HOME != 0;
            if home && !h.home {
                self.home_pressed = true;
                self.home_by = format!("HID {:04x}:{:04x}", h.vid, h.pid);
            }
            h.home = home;
            let b = s.with_stick();
            if s.buttons != 0 && s.buttons != self.last_buttons {
                self.style = h.style;
            }
            all |= b;
        }
        self.last_buttons = all;
        all
    }
}

impl Pads {
    /// Each controller read, with its buttons now (`peerparty --pads`).
    /// The controllers the last poll read, by name ("XInput slot 0", "HID 054c:0ce6"),
    /// for the game's log.
    pub fn found(&self) -> Vec<String> {
        let mut out: Vec<String> = (0..4).filter(|&i| self.xinput.is_some() && self.connected[i]).map(|i| format!("XInput slot {i}")).collect();
        for h in self.hid.lock().unwrap().iter().filter(|h| h.alive.load(Ordering::Relaxed)) {
            out.push(format!("HID {:04x}:{:04x}", h.vid, h.pid));
        }
        out
    }

    pub fn describe(&mut self) -> Vec<(String, u32)> {
        let mut out = Vec::new();
        if let Some(get) = self.xinput {
            for pad in 0..4u32 {
                let mut s = [0u8; STATE_LEN];
                if unsafe { get(pad, &mut s) } == 0 {
                    out.push((format!("XInput slot {pad}"), from_xinput(&s).with_stick()));
                }
            }
        }
        for h in self.hid.lock().unwrap().iter().filter(|h| h.alive.load(Ordering::Relaxed)) {
            let serial_hint = h.path.rsplit('#').nth(1).unwrap_or("");
            out.push((format!("HID {:04x}:{:04x} {:?} ({serial_hint})", h.vid, h.pid, h.style), h.state.lock().unwrap().with_stick()));
        }
        out
    }
}

/// The buttons in `b`, by name.
pub fn names(b: u32) -> String {
    let all = [
        (SOUTH, "A/Cross"),
        (EAST, "B/Circle"),
        (WEST, "X/Square"),
        (NORTH, "Y/Triangle"),
        (LB, "LB"),
        (RB, "RB"),
        (BACK, "View/Create"),
        (START, "Menu/Options"),
        (HOME, "Home/Guide/PS"),
        (UP, "Up"),
        (DOWN, "Down"),
        (LEFT, "Left"),
        (RIGHT, "Right"),
    ];
    let v: Vec<&str> = all.iter().filter(|(x, _)| b & x != 0).map(|(_, n)| *n).collect();
    if v.is_empty() { "-".into() } else { v.join(" + ") }
}

fn xinput() -> Option<GetStateFn> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    for dll in ["xinput1_4.dll", "xinput1_3.dll"] {
        let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
        let m = unsafe { LoadLibraryW(wide.as_ptr()) };
        if m.is_null() {
            continue;
        }
        let f = unsafe { GetProcAddress(m, 100 as *const u8) }
            .or_else(|| unsafe { GetProcAddress(m, c"XInputGetState".as_ptr().cast()) });
        if let Some(f) = f {
            return Some(unsafe { std::mem::transmute::<_, GetStateFn>(f) });
        }
    }
    None
}

fn from_xinput(s: &[u8; STATE_LEN]) -> State {
    let w = u16::from_le_bytes([s[4], s[5]]);
    let map = [
        (0x0001, UP),
        (0x0002, DOWN),
        (0x0004, LEFT),
        (0x0008, RIGHT),
        (0x0010, START),
        (0x0020, BACK),
        (0x0100, LB),
        (0x0200, RB),
        (0x0400, HOME),
        (0x1000, SOUTH),
        (0x2000, EAST),
        (0x4000, WEST),
        (0x8000, NORTH),
    ];
    let buttons = map.iter().filter(|(x, _)| w & x != 0).fold(0, |b, (_, l)| b | l);
    let lx = i16::from_le_bytes([s[8], s[9]]) as f32 / 32767.0;
    let ly = i16::from_le_bytes([s[10], s[11]]) as f32 / 32767.0;
    State { buttons, lx, ly }
}

/// A d-pad hat value (0 = up, clockwise, 8 or more = centred).
fn hat(v: u8) -> u32 {
    match v {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => RIGHT | DOWN,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => LEFT | UP,
        _ => 0,
    }
}

fn axis(v: u8) -> f32 {
    (v as f32 - 128.0) / 127.0
}

/// DualShock 4 / DualSense in their common layout: sticks at 0..4, buttons from `b`.
fn playstation(r: &[u8], b: usize) -> Option<State> {
    if r.len() < b + 3 {
        return None;
    }
    let (b0, b1, b2) = (r[b], r[b + 1], r[b + 2]);
    let mut buttons = hat(b0 & 0x0f);
    let face = [(0x10, WEST), (0x20, SOUTH), (0x40, EAST), (0x80, NORTH)];
    buttons |= face.iter().filter(|(x, _)| b0 & x != 0).fold(0, |a, (_, l)| a | l);
    let more = [(0x01, LB), (0x02, RB), (0x10, BACK), (0x20, START)];
    buttons |= more.iter().filter(|(x, _)| b1 & x != 0).fold(0, |a, (_, l)| a | l);
    if b2 & 0x01 != 0 {
        buttons |= HOME;
    }
    Some(State { buttons, lx: axis(r[0]), ly: -axis(r[1]) })
}

/// Parses one input report from a known pad. `report` starts with the report id.
fn parse_known(vid: u16, pid: u16, report: &[u8]) -> Option<State> {
    let (id, r) = report.split_first()?;
    match (vid, pid) {
        // DualShock 4 (v1, v2, wireless adapter).
        (0x054c, 0x05c4 | 0x09cc | 0x0ba0) => match id {
            0x01 => playstation(r, 4),
            0x11 if r.len() > 6 => playstation(&r[2..], 4),
            _ => None,
        },
        // DualSense, DualSense Edge.
        (0x054c, 0x0ce6 | 0x0df2) => match id {
            0x01 if r.len() >= 10 && r.len() > 30 => playstation(r, 7),
            0x01 => playstation(r, 4),
            0x31 if r.len() > 11 => playstation(&r[1..], 7),
            _ => None,
        },
        // Switch Pro Controller.
        (0x057e, 0x2009) => match id {
            0x3f if r.len() >= 3 => {
                let (b0, b1) = (r[0], r[1]);
                let mut buttons = hat(r[2]);
                // Nintendo's positions: B south, A east, Y west, X north.
                let map0 = [(0x01, SOUTH), (0x02, EAST), (0x04, WEST), (0x08, NORTH), (0x10, LB), (0x20, RB)];
                buttons |= map0.iter().filter(|(x, _)| b0 & x != 0).fold(0, |a, (_, l)| a | l);
                let map1 = [(0x01, BACK), (0x02, START), (0x10, HOME)];
                buttons |= map1.iter().filter(|(x, _)| b1 & x != 0).fold(0, |a, (_, l)| a | l);
                Some(State { buttons, lx: 0.0, ly: 0.0 })
            }
            0x30 if r.len() >= 11 => {
                let (right, shared, left) = (r[2], r[3], r[4]);
                let map = [
                    (right, 0x01, WEST),
                    (right, 0x02, NORTH),
                    (right, 0x04, SOUTH),
                    (right, 0x08, EAST),
                    (right, 0x40, RB),
                    (shared, 0x01, BACK),
                    (shared, 0x02, START),
                    (shared, 0x10, HOME),
                    (left, 0x01, DOWN),
                    (left, 0x02, UP),
                    (left, 0x04, RIGHT),
                    (left, 0x08, LEFT),
                    (left, 0x40, LB),
                ];
                let buttons = map.iter().filter(|(v, x, _)| v & x != 0).fold(0, |a, (_, _, l)| a | l);
                let raw_x = u16::from(r[5]) | (u16::from(r[6] & 0x0f) << 8);
                let raw_y = u16::from(r[6] >> 4) | (u16::from(r[7]) << 4);
                let lx = (raw_x as f32 - 2048.0) / 1400.0;
                let ly = (raw_y as f32 - 2048.0) / 1400.0;
                Some(State { buttons, lx: lx.clamp(-1.0, 1.0), ly: ly.clamp(-1.0, 1.0) })
            }
            _ => None,
        },
        _ => None,
    }
}

fn style_of(vid: u16) -> Style {
    match vid {
        0x054c => Style::PlayStation,
        0x057e => Style::Nintendo,
        _ => Style::Xbox,
    }
}

fn scan_hid(list: &Arc<Mutex<Vec<Hid>>>) {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    };
    use windows_sys::Win32::Devices::HumanInterfaceDevice::HidD_GetHidGuid;
    let mut guid = unsafe { std::mem::zeroed() };
    unsafe { HidD_GetHidGuid(&mut guid) };
    let set = unsafe { SetupDiGetClassDevsW(&guid, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE) };
    if set as isize == -1 {
        return;
    }
    let mut paths = Vec::new();
    for i in 0.. {
        let mut iface: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
        iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if unsafe { SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, i, &mut iface) } == 0 {
            break;
        }
        let mut buf = vec![0u8; 1024];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(set, &iface, detail, buf.len() as u32, std::ptr::null_mut(), std::ptr::null_mut())
        } == 0
        {
            continue;
        }
        let p = unsafe { std::ptr::addr_of!((*detail).DevicePath) } as *const u16;
        let mut n = 0;
        while unsafe { *p.add(n) } != 0 && n < 500 {
            n += 1;
        }
        paths.push(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) }));
    }
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    for path in paths {
        if path.to_ascii_lowercase().contains("ig_") {
            continue;
        }
        if list.lock().unwrap().iter().any(|h| h.path == path) {
            continue;
        }
        open_hid(list, path);
    }
}

fn open_hid(list: &Arc<Mutex<Vec<Hid>>>, path: String) {
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HIDD_ATTRIBUTES, HIDP_CAPS, HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetPreparsedData, HidP_GetCaps,
    };
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let h = unsafe {
        CreateFileW(wide.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
    };
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    let close = || unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    let mut attr: HIDD_ATTRIBUTES = unsafe { std::mem::zeroed() };
    attr.Size = size_of::<HIDD_ATTRIBUTES>() as u32;
    let mut pre = 0isize;
    if unsafe { HidD_GetAttributes(h, &mut attr) } == 0 || unsafe { HidD_GetPreparsedData(h, &mut pre) } == 0 {
        close();
        return;
    }
    let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
    unsafe { HidP_GetCaps(pre, &mut caps) };
    // Generic desktop joystick (4) or gamepad (5) only.
    if caps.UsagePage != 1 || !(caps.Usage == 4 || caps.Usage == 5) || caps.InputReportByteLength == 0 {
        unsafe { HidD_FreePreparsedData(pre) };
        close();
        return;
    }
    let state = Arc::new(Mutex::new(State::default()));
    let alive = Arc::new(AtomicBool::new(true));
    list.lock().unwrap().push(Hid {
        path,
        vid: attr.VendorID,
        pid: attr.ProductID,
        style: style_of(attr.VendorID),
        state: state.clone(),
        alive: alive.clone(),
        home: true,
    });
    let (vid, pid, len) = (attr.VendorID, attr.ProductID, caps.InputReportByteLength as usize);
    let (h, pre) = (h as usize, pre);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; len.max(64)];
        loop {
            let mut got = 0u32;
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::ReadFile(h as _, buf.as_mut_ptr(), len as u32, &mut got, std::ptr::null_mut())
            };
            if ok == 0 || got == 0 {
                break;
            }
            let report = &buf[..got as usize];
            let parsed = parse_known(vid, pid, report).or_else(|| parse_generic(pre, report));
            if let Some(s) = parsed {
                *state.lock().unwrap() = s;
            }
        }
        alive.store(false, Ordering::Relaxed);
        unsafe { HidD_FreePreparsedData(pre) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h as _) };
    });
}

/// Any other gamepad, through Windows' HID parser: buttons 1-4 as the face buttons
/// (south, east, west, north: the common order), 5-6 shoulders, 7-8 back / start, 13 home;
/// the hat switch as the d-pad; X / Y as the left stick. Home is also whichever of the
/// usages pads give it instead of a button number: Generic Desktop's System Main Menu
/// (0x85) and Consumer's AC Home (0x223).
fn parse_generic(pre: isize, report: &[u8]) -> Option<State> {
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{HidP_GetUsageValue, HidP_GetUsages, HidP_Input};
    let mut usages = [0u16; 32];
    let mut n = usages.len() as u32;
    let rc = unsafe {
        HidP_GetUsages(HidP_Input, 0x09, 0, usages.as_mut_ptr(), &mut n, pre, report.as_ptr() as *mut _, report.len() as u32)
    };
    if rc != 0x0011_0000 {
        return None;
    }
    let map = [
        (1, SOUTH),
        (2, EAST),
        (3, WEST),
        (4, NORTH),
        (5, LB),
        (6, RB),
        (7, BACK),
        (8, START),
        (13, HOME),
    ];
    let mut buttons = 0;
    for u in &usages[..n as usize] {
        if let Some((_, b)) = map.iter().find(|(x, _)| x == u) {
            buttons |= b;
        }
    }
    for (page, usage) in [(0x01u16, 0x85u16), (0x0C, 0x223)] {
        let mut other = [0u16; 16];
        let mut m = other.len() as u32;
        let rc = unsafe {
            HidP_GetUsages(HidP_Input, page, 0, other.as_mut_ptr(), &mut m, pre, report.as_ptr() as *mut _, report.len() as u32)
        };
        if rc == 0x0011_0000 && other[..m as usize].contains(&usage) {
            buttons |= HOME;
        }
    }
    let value = |usage: u16| -> Option<u32> {
        let mut v = 0u32;
        let rc = unsafe { HidP_GetUsageValue(HidP_Input, 0x01, 0, usage, &mut v, pre, report.as_ptr() as *mut _, report.len() as u32) };
        (rc == 0x0011_0000).then_some(v)
    };
    if let Some(h) = value(0x39) {
        buttons |= hat(h as u8);
    }
    let (lx, ly) = match (value(0x30), value(0x31)) {
        (Some(x), Some(y)) if x <= 255 && y <= 255 => (axis(x as u8), -axis(y as u8)),
        (Some(x), Some(y)) => ((x as f32 - 32768.0) / 32767.0, -((y as f32 - 32768.0) / 32767.0)),
        _ => (0.0, 0.0),
    };
    Some(State { buttons, lx, ly })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_reports_parse() {
        // DualShock 4 over USB: cross and d-pad right, PS button.
        let mut r = vec![0u8; 64];
        r[0] = 0x01;
        r[1] = 128;
        r[2] = 128;
        r[5] = 0x20 | 2;
        r[7] = 0x01;
        let s = parse_known(0x054c, 0x09cc, &r).unwrap();
        assert_eq!(s.buttons, SOUTH | RIGHT | HOME);
        // Switch Pro simple report: A (east) and home.
        let s = parse_known(0x057e, 0x2009, &[0x3f, 0x02, 0x10, 0x08]).unwrap();
        assert_eq!(s.buttons, EAST | HOME);
        // XInput: A, start, d-pad up.
        let mut x = [0u8; STATE_LEN];
        x[4..6].copy_from_slice(&(0x1000u16 | 0x0010 | 0x0001).to_le_bytes());
        assert_eq!(from_xinput(&x).buttons, SOUTH | START | UP);
        // Stick folds into the d-pad.
        assert_eq!(State { buttons: 0, lx: 0.0, ly: -0.9 }.with_stick(), DOWN);
    }

    /// One XInput pad on slot 0 whose buttons are `FAKE_BUTTONS`.
    static FAKE_BUTTONS: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    unsafe extern "system" fn fake_get(pad: u32, s: *mut [u8; STATE_LEN]) -> u32 {
        if pad != 0 {
            return 1167; // ERROR_DEVICE_NOT_CONNECTED
        }
        unsafe { (&mut *s)[4..6].copy_from_slice(&FAKE_BUTTONS.load(Ordering::Relaxed).to_le_bytes()) };
        0
    }

    #[test]
    fn home_counts_only_after_it_was_seen_up() {
        let mut pads = Pads::new();
        pads.xinput = Some(fake_get);
        pads.last_scan = Some(Instant::now());
        let press = |pads: &mut Pads, buttons: u16| {
            FAKE_BUTTONS.store(buttons, Ordering::Relaxed);
            pads.poll();
            pads.home_pressed
        };
        // Down the first time the pad is seen (Steam Input's pad arriving): nothing.
        assert!(!press(&mut pads, 0x0400));
        assert!(!press(&mut pads, 0));
        assert!(press(&mut pads, 0x0400));
        assert_eq!(pads.home_by, "XInput slot 0");
        assert!(!press(&mut pads, 0x0400));
        // Still down when the game comes back to the front: nothing until it comes up.
        assert!(!press(&mut pads, 0));
        pads.hold_home();
        assert!(!press(&mut pads, 0x0400));
        assert!(!press(&mut pads, 0));
        assert!(press(&mut pads, 0x0400));
    }
}
