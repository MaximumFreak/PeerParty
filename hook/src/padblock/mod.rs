//! Keeps controllers away from the game while PeerParty's overlay shows (the launcher
//! reads them itself, for the overlay). Keyboard and mouse need nothing here: the
//! launcher's low-level hooks hold those back from every program.
//!
//! Installed the first time the overlay opens, so a game that never opens it runs
//! exactly as without PeerParty, with two exceptions in games that have the overlay on:
//! GameInput's callback registration (gameinput.rs), and XInput, hooked as soon as the
//! game loads it. Steam's overlay DLL hooks XInput too, a few seconds into a game; hooked
//! first, PeerParty's detour sits under Steam's, which calls through it, instead of on
//! top of a patch Steam may lay again (a game moved behind the overlay that way).
//! Each hooked call costs one flag check while the overlay is closed.
//!
//! When the overlay closes, the log says how many reads were held back while it showed,
//! per API: a game that moved behind it shows which path it didn't come through.
//!
//! - XInput (xinput1_4, xinput1_3, xinput9_1_0, and modules that load later):
//!   `XInputGetState` and `XInputGetStateEx` report the pad still connected and at rest;
//!   `XInputGetKeystroke` reports nothing.
//! - DirectInput 8 (pads and joysticks that aren't XInput): `GetDeviceState` reads
//!   all zero and `GetDeviceData` returns no events, for the A and W device interfaces.
//! - Raw input: `GetRawInputData` refuses HID reports (pads read as raw HID, the way SDL
//!   and Unity's Input System read PlayStation and Switch pads through WM_INPUT).
//!
//! - Windows.Gaming.Input (wgi.rs): Gamepad and RawGameController readings at rest.
//! - GameInput (gameinput.rs): readings stand still at the one from when it opened, and
//!   reading callbacks aren't called (their registration is hooked when GameInput
//!   loads, in games with the overlay on: the one thing here that goes in early).
//! - HID devices read directly (hid.rs, SDL's HIDAPI): the same report over and over.
//!
//! Steam Input is read in Steam's own process; its action reads are held in steamlan.rs.

/// One `extern "system"` function per slot, each calling `$inner` with its slot index.
macro_rules! slot_fn {
    ($ty:ty, $i:literal, $inner:ident, ($($a:ident: $t:ty),*) -> $r:ty) => {{
        unsafe extern "system" fn f($($a: $t),*) -> $r {
            $inner($i, $($a),*)
        }
        f as $ty
    }};
}

macro_rules! per_slot {
    ($ty:ty, [$($i:literal),*], $inner:ident, $sig:tt -> $r:ty) => {
        [$(slot_fn!($ty, $i, $inner, $sig -> $r)),*]
    };
}

mod gameinput;
mod hid;
mod wgi;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, Once, OnceLock};

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::overlay::visible;

type GetState = unsafe extern "system" fn(u32, *mut u8) -> u32;
type GetKeystroke = unsafe extern "system" fn(u32, u32, *mut u8) -> u32;
type GetRawInputData = unsafe extern "system" fn(isize, u32, *mut u8, *mut u32, u32) -> u32;
type DevGetState = unsafe extern "system" fn(usize, u32, *mut u8) -> i32;
type DevGetData = unsafe extern "system" fn(usize, u32, *mut u8, *mut u32, u32) -> i32;

/// Each hooked function gets a slot of its own (its own detour function, so a call
/// always goes on to the function it came from), kept for the life of the process.
pub(super) struct Slots<T: retour::Function, const N: usize> {
    addrs: Mutex<[usize; N]>,
    hooks: [OnceLock<GenericDetour<T>>; N],
}

impl<T: retour::Function, const N: usize> Slots<T, N> {
    pub(super) const fn new() -> Self {
        Slots { addrs: Mutex::new([0; N]), hooks: [const { OnceLock::new() }; N] }
    }
}

static STATES: Slots<GetState, 8> = Slots::new();
static KEYSTROKES: Slots<GetKeystroke, 4> = Slots::new();
static RAW: OnceLock<GenericDetour<GetRawInputData>> = OnceLock::new();
static DEV_STATE: Slots<DevGetState, 2> = Slots::new();
static DEV_DATA: Slots<DevGetData, 2> = Slots::new();

static INSTALL: Once = Once::new();
/// Reads held back while the overlay showed: XInput states, raw-input HID reports,
/// DirectInput states and buffers.
static HELD_XINPUT: AtomicU32 = AtomicU32::new(0);
static HELD_RAW: AtomicU32 = AtomicU32::new(0);
static HELD_DINPUT: AtomicU32 = AtomicU32::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

const XINPUT: [&str; 4] = ["xinput1_4.dll", "xinput1_3.dll", "xinput9_1_0.dll", "xinputuap.dll"];

/// The overlay opened for the first time: hooks what the game has loaded.
pub fn install() {
    if crate::net::skip("padblock") {
        return;
    }
    INSTALL.call_once(|| {
        let mut n = 0;
        for dll in XINPUT {
            n += hook_xinput(dll);
        }
        n += hook_raw_input();
        n += hook_dinput();
        n += gameinput::install();
        n += hid::install();
        INSTALLED.store(true, Ordering::Relaxed);
        log!("padblock: {n} controller entry points held back while the overlay shows");
    });
    // Windows.Gaming.Input's methods are found through a connected pad, so this is
    // tried at every opening until it works; GameInput when its DLL arrived later.
    let n = wgi::install() + gameinput::install();
    if n > 0 {
        log!("padblock: {n} more controller entry points held back");
    }
}

/// GetOverlappedResult(Ex), for ipfwd: hooked now even if the overlay never opens
/// (`PEERPARTY_SKIP=padblock` doesn't stop it; while the overlay is closed the detours
/// only pass completions on to ipfwd).
pub(crate) fn hook_overlapped_results() -> usize {
    hid::install_results()
}

/// The overlay closed: what was held for this opening goes.
pub fn hidden() {
    if INSTALLED.load(Ordering::Relaxed) {
        wgi::hidden();
        gameinput::hidden();
        hid::hidden();
    }
    let (x, r, d) = (HELD_XINPUT.swap(0, Ordering::Relaxed), HELD_RAW.swap(0, Ordering::Relaxed), HELD_DINPUT.swap(0, Ordering::Relaxed));
    log!("padblock: held back while the overlay showed: {x} XInput reads, {r} raw-input HID reports, {d} DirectInput reads");
}

/// A controller module loaded (watch.rs): GameInput and XInput at once in a game with
/// the overlay on, DirectInput after the overlay first opened.
pub fn module_loaded(name: &str) {
    let lower = name.to_ascii_lowercase();
    let early = crate::ipc::enabled() && !crate::net::skip("padblock");
    if lower.starts_with("gameinput") {
        if early {
            gameinput::module_loaded(&lower);
        }
        return;
    }
    if !INSTALLED.load(Ordering::Relaxed) && !(early && XINPUT.contains(&lower.as_str())) {
        return;
    }
    let n = if XINPUT.contains(&lower.as_str()) {
        hook_xinput(&lower)
    } else if lower == "dinput8.dll" {
        hook_dinput()
    } else {
        return;
    };
    log!("padblock: {name} loaded, {n} hooked");
}

pub(super) fn module(name: &str) -> Option<windows_sys::Win32::Foundation::HMODULE> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let m = unsafe { GetModuleHandleW(wide.as_ptr()) };
    (!m.is_null()).then_some(m)
}

pub(super) fn proc_addr(m: windows_sys::Win32::Foundation::HMODULE, name: *const u8) -> Option<usize> {
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    unsafe { GetProcAddress(m, name) }.map(|f| f as usize)
}

/// Detours `addr` once (by address: several names can share one function), with the
/// first free slot's detour function.
pub(super) fn arm<T, const N: usize>(slots: &Slots<T, N>, addr: usize, detours: [T; N], label: &str) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let mut addrs = slots.addrs.lock().unwrap();
        if addrs.contains(&addr) {
            return 0;
        }
        let Some(i) = addrs.iter().position(|a| *a == 0) else {
            log!("padblock {label}: no slot left");
            return 0;
        };
        let target: T = unsafe { T::from_ptr(addr as *const ()) };
        match unsafe { GenericDetour::new(target, detours[i]) } {
            Ok(h) => {
                let _ = slots.hooks[i].set(h);
                if let Err(e) = crate::hook::frozen(addr, || unsafe { slots.hooks[i].get().unwrap().enable() }) {
                    log!("padblock {label}: {e}");
                    return 0;
                }
                addrs[i] = addr;
                1
            }
            Err(e) => {
                log!("padblock {label} not hooked: {e}");
                0
            }
        }
    })
}

fn hook_xinput(dll: &str) -> usize {
    let Some(m) = module(dll) else { return 0 };
    let mut n = 0;
    let mut names = Vec::new();
    for (name, label) in [(c"XInputGetState".as_ptr() as *const u8, "XInputGetState"), (100 as *const u8, "XInputGetStateEx")] {
        if let Some(a) = proc_addr(m, name) {
            let k = arm(&STATES, a, state_detours(), label);
            if k > 0 {
                names.push(format!("{label} at {a:#x}"));
            }
            n += k;
        }
    }
    if let Some(a) = proc_addr(m, c"XInputGetKeystroke".as_ptr().cast()) {
        n += arm(&KEYSTROKES, a, keystroke_detours(), "XInputGetKeystroke");
    }
    if !names.is_empty() {
        log!("padblock: {dll}: {}", names.join(", "));
    }
    n
}

fn state_detours() -> [GetState; 8] {
    per_slot!(GetState, [0, 1, 2, 3, 4, 5, 6, 7], get_state, (pad: u32, out: *mut u8) -> u32)
}

fn get_state(i: usize, pad: u32, out: *mut u8) -> u32 {
    let Some(h) = STATES.hooks[i].get() else { return 1167 };
    let rc = unsafe { h.call(pad, out) };
    if rc == 0 && visible() && !out.is_null() {
        // XINPUT_STATE: packet number (kept), then the 12-byte XINPUT_GAMEPAD.
        unsafe { std::ptr::write_bytes(out.add(4), 0, 12) };
        HELD_XINPUT.fetch_add(1, Ordering::Relaxed);
    }
    rc
}

fn keystroke_detours() -> [GetKeystroke; 4] {
    per_slot!(GetKeystroke, [0, 1, 2, 3], get_keystroke, (pad: u32, reserved: u32, out: *mut u8) -> u32)
}

fn get_keystroke(i: usize, pad: u32, reserved: u32, out: *mut u8) -> u32 {
    if visible() {
        return 0x10D2; // ERROR_EMPTY
    }
    let Some(h) = KEYSTROKES.hooks[i].get() else { return 0x10D2 };
    unsafe { h.call(pad, reserved, out) }
}

fn hook_raw_input() -> usize {
    let Some(m) = module("user32.dll") else { return 0 };
    let Some(a) = proc_addr(m, c"GetRawInputData".as_ptr().cast()) else { return 0 };
    with_lock(|| {
        if RAW.get().is_some() {
            return 0;
        }
        let target: GetRawInputData = unsafe { std::mem::transmute(a) };
        match unsafe { GenericDetour::new(target, raw_input_data as GetRawInputData) } {
            Ok(h) => {
                let _ = RAW.set(h);
                usize::from(crate::hook::frozen(a, || unsafe { RAW.get().unwrap().enable() }).is_ok())
            }
            Err(e) => {
                log!("padblock GetRawInputData not hooked: {e}");
                0
            }
        }
    })
}

const RID_INPUT: u32 = 0x1000_0003;
const RIM_TYPEHID: u32 = 2;

unsafe extern "system" fn raw_input_data(h: isize, cmd: u32, data: *mut u8, size: *mut u32, header: u32) -> u32 {
    let Some(d) = RAW.get() else { return u32::MAX };
    let rc = unsafe { d.call(h, cmd, data, size, header) };
    if visible() && cmd == RID_INPUT && !data.is_null() && rc != u32::MAX && rc as usize >= 4 {
        // RAWINPUTHEADER.dwType comes first.
        let kind = unsafe { (data as *const u32).read_unaligned() };
        if kind == RIM_TYPEHID {
            HELD_RAW.fetch_add(1, Ordering::Relaxed);
            return u32::MAX;
        }
    }
    rc
}

fn hook_dinput() -> usize {
    let Some(m) = module("dinput8.dll") else { return 0 };
    let Some(create) = proc_addr(m, c"DirectInput8Create".as_ptr().cast()) else { return 0 };
    type Create = unsafe extern "system" fn(isize, u32, *const [u8; 16], *mut usize, usize) -> i32;
    let create: Create = unsafe { std::mem::transmute(create) };
    // IID_IDirectInput8W / A and GUID_SysKeyboard.
    let iid_w = guid(0xBF798031, 0x483A, 0x4DA2, [0xAA, 0x99, 0x5D, 0x64, 0xED, 0x36, 0x97, 0x00]);
    let iid_a = guid(0xBF798030, 0x483A, 0x4DA2, [0xAA, 0x99, 0x5D, 0x64, 0xED, 0x36, 0x97, 0x00]);
    let keyboard = guid(0x6F1D2B61, 0xD5A0, 0x11CF, [0xBF, 0xC7, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00]);
    let instance = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) } as isize;
    type CreateDevice = unsafe extern "system" fn(usize, *const [u8; 16], *mut usize, usize) -> i32;
    type Release = unsafe extern "system" fn(usize) -> u32;
    let mut n = 0;
    for iid in [iid_w, iid_a] {
        let mut di = 0usize;
        if unsafe { create(instance, 0x0800, &iid, &mut di, 0) } < 0 || di == 0 {
            continue;
        }
        // IDirectInput8: Release is slot 2, CreateDevice slot 3.
        let vt = unsafe { *(di as *const *const usize) };
        let create_device: CreateDevice = unsafe { std::mem::transmute(*vt.add(3)) };
        let release: Release = unsafe { std::mem::transmute(*vt.add(2)) };
        let mut dev = 0usize;
        if unsafe { create_device(di, &keyboard, &mut dev, 0) } >= 0 && dev != 0 {
            // IDirectInputDevice8: GetDeviceState is slot 9, GetDeviceData slot 10.
            let dvt = unsafe { *(dev as *const *const usize) };
            n += arm(&DEV_STATE, unsafe { *dvt.add(9) }, dev_state_detours(), "IDirectInputDevice8::GetDeviceState");
            n += arm(&DEV_DATA, unsafe { *dvt.add(10) }, dev_data_detours(), "IDirectInputDevice8::GetDeviceData");
            let dev_release: Release = unsafe { std::mem::transmute(*dvt.add(2)) };
            unsafe { dev_release(dev) };
        }
        unsafe { release(di) };
    }
    n
}

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

fn dev_state_detours() -> [DevGetState; 2] {
    per_slot!(DevGetState, [0, 1], dev_state, (this: usize, size: u32, out: *mut u8) -> i32)
}

fn dev_state(i: usize, this: usize, size: u32, out: *mut u8) -> i32 {
    let Some(h) = DEV_STATE.hooks[i].get() else { return 0 };
    let rc = unsafe { h.call(this, size, out) };
    if rc >= 0 && visible() && !out.is_null() {
        HELD_DINPUT.fetch_add(1, Ordering::Relaxed);
        unsafe { std::ptr::write_bytes(out, 0, size as usize) };
        // DIJOYSTATE's POV hats read "centred" as 0xFFFFFFFF, not 0 (which is "up").
        if size == 80 || size == 272 {
            for k in 0..4 {
                unsafe { (out.add(32 + k * 4) as *mut u32).write_unaligned(u32::MAX) };
            }
        }
    }
    rc
}

fn dev_data_detours() -> [DevGetData; 2] {
    per_slot!(DevGetData, [0, 1], dev_data, (this: usize, size: u32, data: *mut u8, count: *mut u32, flags: u32) -> i32)
}

fn dev_data(i: usize, this: usize, size: u32, data: *mut u8, count: *mut u32, flags: u32) -> i32 {
    let Some(h) = DEV_DATA.hooks[i].get() else { return 0 };
    let rc = unsafe { h.call(this, size, data, count, flags) };
    if rc >= 0 && visible() && !count.is_null() {
        unsafe { *count = 0 };
    }
    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(name: &str) {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
    }

    /// GameInput's methods are found and hooked, and while the overlay shows a reading
    /// stands still (the same object comes back). Needs GameInput (Windows 11 has it).
    /// GameInput crashes when two threads create its first object at once (seen with no
    /// hooks at all), so its tests take turns. In a game the game's object comes first.
    pub(super) static GAMEINPUT_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn gameinput_readings_hold_still() {
        let _turn = GAMEINPUT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        load("GameInput.dll");
        let Some(m) = module("gameinput.dll") else { return };
        assert!(gameinput::install() >= 2, "every version's reads hooked");
        let create: unsafe extern "system" fn(*mut usize) -> i32 =
            unsafe { std::mem::transmute(proc_addr(m, c"GameInputCreate".as_ptr().cast()).unwrap()) };
        let mut gi = 0usize;
        assert!(unsafe { create(&mut gi) } >= 0);
        let vt = unsafe { *(gi as *const *const usize) };
        let current: unsafe extern "system" fn(usize, u32, usize, *mut usize) -> i32 = unsafe { std::mem::transmute(*vt.add(4)) };
        let next: unsafe extern "system" fn(usize, usize, u32, usize, *mut usize) -> i32 = unsafe { std::mem::transmute(*vt.add(5)) };
        crate::overlay::set_visible_for_test(true);
        // Keyboard readings exist on any PC.
        let (mut a, mut b, mut c) = (0usize, 0usize, 1usize);
        let ha = unsafe { current(gi, 0x10, 0, &mut a) };
        let hb = unsafe { current(gi, 0x10, 0, &mut b) };
        let hc = unsafe { next(gi, a, 0x10, 0, &mut c) };
        crate::overlay::set_visible_for_test(false);
        gameinput::hidden();
        eprintln!("GameInput: current {ha:#x} {hb:#x}, next {hc:#x}");
        if ha >= 0 {
            assert_eq!(a, b, "the same reading while the overlay shows");
            assert_eq!(c, 0);
        }
    }

    /// XInput hooked, then read through the real export: while the overlay shows a
    /// connected pad reads at rest and the read is counted (skipped with no pad).
    #[test]
    fn xinput_reads_rest_while_the_overlay_shows() {
        load("xinput1_4.dll");
        hook_xinput("xinput1_4.dll");
        let m = module("xinput1_4.dll").unwrap();
        let get: GetState = unsafe { std::mem::transmute(proc_addr(m, c"XInputGetState".as_ptr().cast()).unwrap()) };
        let mut s = [0xAAu8; 16];
        if unsafe { get(0, s.as_mut_ptr()) } != 0 {
            eprintln!("no XInput pad in slot 0: skipped");
            return;
        }
        crate::overlay::set_visible_for_test(true);
        let before = HELD_XINPUT.load(Ordering::Relaxed);
        let mut s = [0xAAu8; 16];
        let rc = unsafe { get(0, s.as_mut_ptr()) };
        crate::overlay::set_visible_for_test(false);
        assert_eq!(rc, 0);
        assert_eq!(&s[4..16], &[0u8; 12], "buttons, triggers and sticks at rest");
        assert!(HELD_XINPUT.load(Ordering::Relaxed) > before);
    }

    /// Windows.Gaming.Input: hooked when a pad is connected (skipped otherwise).
    #[test]
    fn wgi_installs_with_a_pad() {
        load("Windows.Gaming.Input.dll");
        wgi::install();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        eprintln!("Windows.Gaming.Input: {} hooked", wgi::hooked());
    }
}
