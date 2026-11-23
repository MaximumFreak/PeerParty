//! Windows.Gaming.Input: the controller classes' `GetCurrentReading` (Gamepad,
//! RawGameController, UINavigationController, ArcadeStick, FlightStick, RacingWheel) and
//! `Gamepad.Vibration`, driven by `input::policy()`. Their interfaces' method order and
//! the readings' layouts are Windows' own metadata's (Windows.Gaming.winmd).
//!
//! - `Policy::PassThrough`: passes through and learns raw controller axis rest positions.
//! - `Policy::BlockAll`: every reading at rest (timestamp kept): no buttons, hats
//!   centred, axes at their learnt rest.
//! - `Policy::Virtual(snap)`: Windows lists every controller on the PC, but a copy has
//!   one. One gamepad carries the copy's controller (`VirtualPad` into its reading) and
//!   the others read at rest; the copy's own physical pad is learnt as the one whose
//!   buttons go down with the copy's controller's, and until then the first one read
//!   stands in. The same goes for UINavigationController (menus) and RawGameController
//!   (its own reads as it is). Arcade sticks, flight sticks and wheels are no copy's
//!   (split screen seats gamepads) and read at rest. Vibration reaches the copy's own
//!   controller alone: its gamepad once learnt, else its XInput slot.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use peerparty_protocol::input_shm::VirtualPad;
use retour::GenericDetour;

use super::{Policy, module, policy, proc_addr};
use crate::log;
use crate::sys::hook::with_lock;

type GamepadReading = unsafe extern "system" fn(usize, *mut u8) -> i32;
type RawReading = unsafe extern "system" fn(usize, u32, *mut u8, u32, *mut i32, u32, *mut f64, *mut u64) -> i32;
type PutVibration = unsafe extern "system" fn(usize, Vibration) -> i32;

/// `Windows.Gaming.Input.GamepadVibration`: left and right motors, the triggers'.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Vibration {
    left: f64,
    right: f64,
    left_trigger: f64,
    right_trigger: f64,
}

static GAMEPAD: OnceLock<GenericDetour<GamepadReading>> = OnceLock::new();
static RAW: OnceLock<GenericDetour<RawReading>> = OnceLock::new();
static VIBRATION: OnceLock<GenericDetour<PutVibration>> = OnceLock::new();
static NAV: OnceLock<GenericDetour<GamepadReading>> = OnceLock::new();
static ARCADE: OnceLock<GenericDetour<GamepadReading>> = OnceLock::new();
static FLIGHT: OnceLock<GenericDetour<GamepadReading>> = OnceLock::new();
static WHEEL: OnceLock<GenericDetour<GamepadReading>> = OnceLock::new();
/// Which of the hooks are in (a slot taken whose hook couldn't be switched on isn't).
static ENABLED: AtomicU32 = AtomicU32::new(0);
const GAMEPAD_BIT: u32 = 1;
const RAW_BIT: u32 = 2;
static TRIED: AtomicU32 = AtomicU32::new(0);
static WGI_READS: AtomicU32 = AtomicU32::new(0);

pub(super) fn take_reads() -> u32 {
    WGI_READS.swap(0, Ordering::Relaxed)
}
/// Raw controllers' axes when held, per controller.
static AXES: Mutex<Vec<(usize, Vec<f64>)>> = Mutex::new(Vec::new());
/// Where each raw controller's axes sit while passing through.
static REST: Mutex<Vec<(usize, Vec<Rest>)>> = Mutex::new(Vec::new());

/// How often an axis was seen in each twentieth of its range (0..=1).
#[derive(Clone, Copy, Default)]
struct Rest {
    bins: [u16; 21],
}

impl Rest {
    fn bin(v: f64) -> usize {
        (v.clamp(0.0, 1.0) * 20.0).round() as usize
    }

    fn saw(&mut self, v: f64) {
        let b = &mut self.bins[Self::bin(v)];
        if *b == u16::MAX {
            for x in &mut self.bins {
                *x /= 2;
            }
        }
        self.bins[Self::bin(v)] += 1;
    }

    fn rest(&self) -> Option<f64> {
        let total: u32 = self.bins.iter().map(|&b| u32::from(b)).sum();
        if total < 120 {
            return None;
        }
        let (i, _) = self.bins.iter().enumerate().max_by_key(|(_, b)| **b)?;
        Some(i as f64 / 20.0)
    }
}

fn held_axis(rest: Option<f64>, at_open: f64) -> f64 {
    match rest {
        Some(r) => r,
        None if (at_open - 0.5).abs() < 0.15 => 0.5,
        None => at_open,
    }
}

const IID_GAMEPAD_STATICS: [u8; 16] = super::guid(0x8BBCE529, 0xD49C, 0x39E9, [0x95, 0x60, 0xE4, 0x7D, 0xDE, 0x96, 0xB7, 0xC8]);
const IID_RAW_STATICS: [u8; 16] = super::guid(0xEB8D0792, 0xE95A, 0x4B19, [0xAF, 0xC7, 0x0A, 0x59, 0xF8, 0xBF, 0x75, 0x9E]);
const IID_NAV_STATICS: [u8; 16] = super::guid(0x2F14930A, 0xF6F8, 0x4A48, [0x8D, 0x89, 0x94, 0x78, 0x6C, 0xCA, 0x0C, 0x2E]);
const IID_ARCADE_STATICS: [u8; 16] = super::guid(0x5C37B8C8, 0x37B1, 0x4AD8, [0x94, 0x58, 0x20, 0x0F, 0x1A, 0x30, 0x01, 0x8E]);
const IID_FLIGHT_STATICS: [u8; 16] = super::guid(0x5514924A, 0xFECC, 0x435E, [0x83, 0xDC, 0x5C, 0xEC, 0x8A, 0x18, 0xA5, 0x20]);
const IID_WHEEL_STATICS: [u8; 16] = super::guid(0x3AC12CD5, 0x581B, 0x4936, [0x9F, 0x94, 0x69, 0xF1, 0xE6, 0x51, 0x4C, 0x7D]);

static BUSY: AtomicBool = AtomicBool::new(false);

/// Whether the gamepad and raw controller reads are both hooked (the others come along
/// when there's such a controller).
fn both() -> bool {
    ENABLED.load(Ordering::Relaxed) & (GAMEPAD_BIT | RAW_BIT) == GAMEPAD_BIT | RAW_BIT
}

pub fn install() -> usize {
    if module("windows.gaming.input.dll").is_none() || both() {
        return 0;
    }
    if BUSY.swap(true, Ordering::Relaxed) {
        return 0;
    }
    let spawned = std::thread::Builder::new().name("peerparty-wgi".into()).spawn(|| {
        // Its controller lists fill in a moment after it starts (and stay empty until a
        // controller is plugged in): tried for 20 s, then again at the next call.
        let mut n = 0;
        for i in 0..40 {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_millis(if i == 1 { 300 } else { 500 }));
            }
            n += unsafe { install_on_own_thread() };
            if both() {
                break;
            }
        }
        if n > 0 {
            log!("input: Windows.Gaming.Input: {n} hooked");
        }
        BUSY.store(false, Ordering::Relaxed);
    });
    if spawned.is_err() {
        BUSY.store(false, Ordering::Relaxed);
    }
    0
}

/// Controllers changed: the copy's own controllers are learnt again, and what has no
/// controller to hook through yet is tried again (`input::devices_changed`).
pub(super) fn devices_changed() {
    for l in [&GAMEPADS, &NAVS, &RAWS] {
        l.forget();
    }
    TRIED.store(0, Ordering::Relaxed);
}

unsafe fn install_on_own_thread() -> usize {
    let Some(combase) = module("combase.dll") else { return 0 };
    let (Some(init), Some(uninit), Some(create_string), Some(delete_string), Some(factory)) = (
        proc_addr(combase, c"RoInitialize".as_ptr().cast()),
        proc_addr(combase, c"RoUninitialize".as_ptr().cast()),
        proc_addr(combase, c"WindowsCreateString".as_ptr().cast()),
        proc_addr(combase, c"WindowsDeleteString".as_ptr().cast()),
        proc_addr(combase, c"RoGetActivationFactory".as_ptr().cast()),
    ) else {
        return 0;
    };
    let init: unsafe extern "system" fn(i32) -> i32 = unsafe { std::mem::transmute(init) };
    let uninit: unsafe extern "system" fn() = unsafe { std::mem::transmute(uninit) };
    let create_string: unsafe extern "system" fn(*const u16, u32, *mut usize) -> i32 = unsafe { std::mem::transmute(create_string) };
    let delete_string: unsafe extern "system" fn(usize) -> i32 = unsafe { std::mem::transmute(delete_string) };
    let factory: unsafe extern "system" fn(usize, *const [u8; 16], *mut usize) -> i32 = unsafe { std::mem::transmute(factory) };
    // RO_INIT_MULTITHREADED; undone at the end when it was this call's to undo.
    let inited = unsafe { init(1) } >= 0;
    let n = unsafe { hook_classes(create_string, delete_string, factory) };
    if inited {
        unsafe { uninit() };
    }
    n
}

type CreateString = unsafe extern "system" fn(*const u16, u32, *mut usize) -> i32;
type DeleteString = unsafe extern "system" fn(usize) -> i32;
type Factory = unsafe extern "system" fn(usize, *const [u8; 16], *mut usize) -> i32;

unsafe fn hook_classes(create_string: CreateString, delete_string: DeleteString, factory: Factory) -> usize {
    let statics = |class: &str, iid: &[u8; 16]| -> usize {
        let wide: Vec<u16> = class.encode_utf16().collect();
        let mut hs = 0usize;
        if unsafe { create_string(wide.as_ptr(), wide.len() as u32, &mut hs) } < 0 {
            return 0;
        }
        let mut out = 0usize;
        let hr = unsafe { factory(hs, iid, &mut out) };
        unsafe { delete_string(hs) };
        if hr < 0 { 0 } else { out }
    };
    // Each class's statics list its controllers (slot 10); a controller's vtable gives the
    // methods: IGamepad get_Vibration 6, put_Vibration 7, GetCurrentReading 8;
    // IRawGameController GetCurrentReading 13; IUINavigationController 6; IArcadeStick 7;
    // IFlightStick 8; IRacingWheel 13.
    let mut n = 0;
    let mut each = |class: &str, iid: &[u8; 16], bit: u32, hook: &mut dyn FnMut(usize) -> usize| {
        if ENABLED.load(Ordering::Relaxed) & bit != 0 {
            return;
        }
        let s = statics(class, iid);
        if let Some(c) = unsafe { first_of(s, 10) } {
            let k = hook(c);
            if k > 0 {
                ENABLED.fetch_or(bit, Ordering::Relaxed);
            }
            n += k;
            unsafe { release(c) };
        } else if TRIED.fetch_or(bit, Ordering::Relaxed) & bit == 0 && bit & (GAMEPAD_BIT | RAW_BIT) != 0 {
            log!("input: Windows.Gaming.Input has no {class} connected yet; retried next time");
        }
        unsafe { release(s) };
    };
    each("Windows.Gaming.Input.Gamepad", &IID_GAMEPAD_STATICS, GAMEPAD_BIT, &mut |pad| {
        let k = arm(&GAMEPAD, unsafe { slot(pad, 8) }, gamepad_reading as GamepadReading, "Gamepad.GetCurrentReading");
        k + arm(&VIBRATION, unsafe { slot(pad, 7) }, put_vibration as PutVibration, "Gamepad.Vibration (put)")
    });
    each("Windows.Gaming.Input.RawGameController", &IID_RAW_STATICS, RAW_BIT, &mut |c| arm(&RAW, unsafe { slot(c, 13) }, raw_reading as RawReading, "RawGameController.GetCurrentReading"));
    each("Windows.Gaming.Input.UINavigationController", &IID_NAV_STATICS, 4, &mut |c| arm(&NAV, unsafe { slot(c, 6) }, nav_reading as GamepadReading, "UINavigationController.GetCurrentReading"));
    each("Windows.Gaming.Input.ArcadeStick", &IID_ARCADE_STATICS, 8, &mut |c| arm(&ARCADE, unsafe { slot(c, 7) }, arcade_reading as GamepadReading, "ArcadeStick.GetCurrentReading"));
    each("Windows.Gaming.Input.FlightStick", &IID_FLIGHT_STATICS, 16, &mut |c| arm(&FLIGHT, unsafe { slot(c, 8) }, flight_reading as GamepadReading, "FlightStick.GetCurrentReading"));
    each("Windows.Gaming.Input.RacingWheel", &IID_WHEEL_STATICS, 32, &mut |c| arm(&WHEEL, unsafe { slot(c, 13) }, wheel_reading as GamepadReading, "RacingWheel.GetCurrentReading"));
    n
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

unsafe fn first_of(statics: usize, getter: usize) -> Option<usize> {
    if statics == 0 {
        return None;
    }
    let get: unsafe extern "system" fn(usize, *mut usize) -> i32 = unsafe { std::mem::transmute(slot(statics, getter)) };
    let mut view = 0usize;
    if unsafe { get(statics, &mut view) } < 0 || view == 0 {
        return None;
    }
    let size: unsafe extern "system" fn(usize, *mut u32) -> i32 = unsafe { std::mem::transmute(slot(view, 7)) };
    let at: unsafe extern "system" fn(usize, u32, *mut usize) -> i32 = unsafe { std::mem::transmute(slot(view, 6)) };
    let mut n = 0u32;
    let mut item = 0usize;
    let found = unsafe { size(view, &mut n) } >= 0 && n > 0 && unsafe { at(view, 0, &mut item) } >= 0 && item != 0;
    unsafe { release(view) };
    found.then_some(item)
}

fn arm<T: retour::Function + Copy>(cell: &OnceLock<GenericDetour<T>>, addr: usize, detour: T, label: &str) -> usize {
    with_lock(|| {
        if cell.get().is_some() {
            return 0;
        }
        let target: T = unsafe { T::from_ptr(addr as *const ()) };
        match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => {
                let _ = cell.set(h);
                match crate::sys::hook::frozen(addr, || unsafe { cell.get().unwrap().enable() }) {
                    Ok(()) => 1,
                    Err(e) => {
                        log!("input {label}: {e}");
                        0
                    }
                }
            }
            Err(e) => {
                log!("input {label} not hooked: {e}");
                0
            }
        }
    })
}

pub fn hooked() -> usize {
    (ENABLED.load(Ordering::Relaxed) & (GAMEPAD_BIT | RAW_BIT)).count_ones() as usize
}

/// For the 30-second summary: whether its reads are watched.
pub(super) fn status() -> &'static str {
    match (module("windows.gaming.input.dll").is_some(), hooked()) {
        (false, _) => "not loaded",
        (true, 0) => "loaded, NOT hooked (its reads reach every controller)",
        (true, 1) => "loaded, partly hooked",
        _ => "hooked",
    }
}

pub fn hidden() {
    AXES.lock().unwrap().clear();
}

/// Converts XInput `wButtons` to `Windows.Gaming.Input.GamepadButtons` (GameInput's
/// `GameInputGamepadButtons` are the same bits).
pub(super) fn wgi_buttons(xinput: u16) -> u32 {
    const MAP: [(u16, u32); 14] = [
        (0x0010, 0x0001), // Start -> Menu
        (0x0020, 0x0002), // Back -> View
        (0x1000, 0x0004), // A
        (0x2000, 0x0008), // B
        (0x4000, 0x0010), // X
        (0x8000, 0x0020), // Y
        (0x0001, 0x0040), // DPadUp
        (0x0002, 0x0080), // DPadDown
        (0x0004, 0x0100), // DPadLeft
        (0x0008, 0x0200), // DPadRight
        (0x0100, 0x0400), // LeftShoulder
        (0x0200, 0x0800), // RightShoulder
        (0x0040, 0x1000), // LeftThumbstick
        (0x0080, 0x2000), // RightThumbstick
    ];
    MAP.iter().filter(|(x, _)| xinput & x != 0).fold(0, |acc, (_, w)| acc | w)
}

/// Writes `VirtualPad` into a 64-byte `Windows.Gaming.Input.GamepadReading` (bytes 8..64,
/// leaving `Timestamp` at 0..8 intact).
/// Layout: `u64 Timestamp` (0), `u32 Buttons` (8), 4B pad (12), `f64 LeftTrigger` (16),
/// `f64 RightTrigger` (24), `f64 LeftThumbstickX` (32), `f64 LeftThumbstickY` (40),
/// `f64 RightThumbstickX` (48), `f64 RightThumbstickY` (56).
pub(super) fn pack_wgi_gamepad(pad: &VirtualPad, out: &mut [u8; 64]) {
    out[8..64].fill(0);
    if !pad.connected {
        return;
    }
    let btns = wgi_buttons(pad.xinput_buttons());
    out[8..12].copy_from_slice(&btns.to_le_bytes());
    let lt = f64::from(pad.left_trigger) / 255.0;
    let rt = f64::from(pad.right_trigger) / 255.0;
    let lx = (f64::from(pad.thumb_lx) / 32767.0).clamp(-1.0, 1.0);
    let ly = (f64::from(pad.thumb_ly) / 32767.0).clamp(-1.0, 1.0);
    let rx = (f64::from(pad.thumb_rx) / 32767.0).clamp(-1.0, 1.0);
    let ry = (f64::from(pad.thumb_ry) / 32767.0).clamp(-1.0, 1.0);
    for (i, v) in [lt, rt, lx, ly, rx, ry].into_iter().enumerate() {
        out[16 + i * 8..24 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
}

/// The copy's controller as a gamepad reads it now: a real XInput pad read in this
/// process (not the session's copy of it, a tick or two behind), else the session's; a
/// HID pad the game reads natively isn't echoed here too.
fn own_pad(snap: &peerparty_protocol::input_shm::InputSnapshot) -> VirtualPad {
    match snap.pad.xinput_slot.and_then(|k| super::real_xinput(k.into()).map(|s| super::pad_of_xinput(k, &s))) {
        Some(p) => p,
        None if snap.pad.xinput_slot.is_none() && super::own_hid_read(snap.pad.hid_key) => VirtualPad::default(),
        None => snap.pad,
    }
}

unsafe extern "system" fn gamepad_reading(this: usize, out: *mut u8) -> i32 {
    let Some(h) = GAMEPAD.get() else { return -1 };
    let hr = unsafe { h.call(this, out) };
    if hr < 0 || out.is_null() {
        return hr;
    }
    match policy() {
        Policy::PassThrough => {}
        Policy::BlockAll => {
            // GamepadReading: Timestamp (u64, kept), Buttons (u32), pad, six doubles.
            unsafe { std::ptr::write_bytes(out.add(8), 0, 56) };
        }
        Policy::Virtual(snap) => {
            WGI_READS.fetch_add(1, Ordering::Relaxed);
            super::first_use(&super::USED_WGI, "Windows.Gaming.Input gamepads (one is its controller, the others at rest)");
            let buf = unsafe { &mut *(out as *mut [u8; 64]) };
            let own = own_pad(&snap);
            // This pad's own buttons, before they're replaced: the one whose buttons go
            // down with the copy's controller's is its physical pad.
            let real_buttons = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            GAMEPADS.learn(this, real_buttons, wgi_buttons(own.xinput_buttons()));
            let one = match GAMEPADS.is(this) {
                Some(mine) => mine,
                None => the_one(this, std::time::Instant::now()),
            };
            let pad = if one { own } else { VirtualPad::default() };
            let real_ts = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            buf[0..8].copy_from_slice(&stamp(this, &pad, real_ts).to_le_bytes());
            pack_wgi_gamepad(&pad, buf);
        }
    }
    hr
}

/// Each gamepad's last reading handed out: (gamepad, what it said, its timestamp).
static STAMPS: Mutex<Vec<(usize, VirtualPad, u64)>> = Mutex::new(Vec::new());

/// The timestamp for gamepad `this`'s reading of `pad`: a new one whenever what it says
/// changed, else the last one. The real timestamp is the real pad's, which moves only
/// when that pad does; a game that skips readings whose timestamp didn't move kept the
/// copy's stick where it was until the pad underneath moved, which could be another
/// player's.
fn stamp(this: usize, pad: &VirtualPad, real_ts: u64) -> u64 {
    let mut stamps = STAMPS.lock().unwrap_or_else(|e| e.into_inner());
    let same = |a: &VirtualPad, b: &VirtualPad| {
        (a.connected, a.buttons, a.left_trigger, a.right_trigger, a.thumb_lx, a.thumb_ly, a.thumb_rx, a.thumb_ry)
            == (b.connected, b.buttons, b.left_trigger, b.right_trigger, b.thumb_lx, b.thumb_ly, b.thumb_rx, b.thumb_ry)
    };
    match stamps.iter_mut().find(|(p, _, _)| *p == this) {
        Some((_, last, ts)) => {
            if !same(last, pad) {
                *ts = real_ts.max(*ts + 1);
                *last = *pad;
            }
            *ts
        }
        None => {
            if stamps.len() >= 16 {
                stamps.remove(0);
            }
            stamps.push((this, *pad, real_ts));
            real_ts
        }
    }
}

/// The copy's own physical controller among one class's objects, learnt for one seat
/// (`super::seat_stamp`): the one whose buttons went down with the copy's controller's.
pub(super) struct Learner {
    what: &'static str,
    learnt: Mutex<Option<(u64, usize)>>,
}

impl Learner {
    const fn new(what: &'static str) -> Self {
        Learner { what, learnt: Mutex::new(None) }
    }

    /// Object `this` read `real` buttons while the copy's controller has `own` down.
    fn learn(&self, this: usize, real: u32, own: u32) {
        if own == 0 || real != own {
            return;
        }
        let stamp = super::seat_stamp();
        let mut l = self.learnt.lock().unwrap_or_else(|e| e.into_inner());
        if *l != Some((stamp, this)) {
            log!("input: Windows.Gaming.Input {} {this:#x} is this copy's controller (its buttons went down with it)", self.what);
            *l = Some((stamp, this));
        }
    }

    /// Whether `this` is the copy's own, once one is learnt for its seat.
    fn is(&self, this: usize) -> Option<bool> {
        let l = *self.learnt.lock().unwrap_or_else(|e| e.into_inner());
        l.filter(|(s, _)| *s == super::seat_stamp()).map(|(_, d)| d == this)
    }

    fn forget(&self) {
        *self.learnt.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

static GAMEPADS: Learner = Learner::new("gamepad");
static NAVS: Learner = Learner::new("UI navigation controller");
static RAWS: Learner = Learner::new("raw controller");

/// The gamepad that carries a split-screen copy's controller, and when it was last read.
static ONE: Mutex<Option<(usize, std::time::Instant)>> = Mutex::new(None);

/// Whether gamepad `this` is the one with the copy's controller: Windows lists every pad
/// connected to the PC, and each answering with it would be a second controller. The
/// first one read takes it, and another once it hasn't been read for two seconds (the
/// game let it go).
fn the_one(this: usize, now: std::time::Instant) -> bool {
    let mut one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    match *one {
        Some((p, _)) if p == this => {}
        Some((_, t)) if now.duration_since(t) < std::time::Duration::from_secs(2) => return false,
        _ => {}
    }
    *one = Some((this, now));
    true
}

unsafe extern "system" fn raw_reading(
    this: usize,
    n_buttons: u32,
    buttons: *mut u8,
    n_switches: u32,
    switches: *mut i32,
    n_axes: u32,
    axes: *mut f64,
    timestamp: *mut u64,
) -> i32 {
    let Some(h) = RAW.get() else { return -1 };
    let hr = unsafe { h.call(this, n_buttons, buttons, n_switches, switches, n_axes, axes, timestamp) };
    if hr < 0 {
        return hr;
    }
    if matches!(policy(), Policy::PassThrough) {
        if !axes.is_null()
            && n_axes > 0
            && n_axes <= 64
            && let Ok(mut rest) = REST.try_lock()
        {
            let now = unsafe { std::slice::from_raw_parts(axes, n_axes as usize) };
            if !rest.iter().any(|(c, _)| *c == this) && rest.len() < 16 {
                rest.push((this, vec![Rest::default(); now.len()]));
            }
            if let Some((_, r)) = rest.iter_mut().find(|(c, r)| *c == this && r.len() == now.len()) {
                for (r, v) in r.iter_mut().zip(now) {
                    r.saw(*v);
                }
            }
        }
        return hr;
    }
    if let Policy::Virtual(snap) = policy() {
        super::first_use(&super::USED_WGI_RAW, "Windows.Gaming.Input raw controllers (its own reads as it is, the others at rest)");
        // Its own physical controller reads as it is: learnt as the one whose buttons
        // went down with the copy's controller's (as many of them).
        let own = own_pad(&snap).xinput_buttons();
        let own_down = wgi_buttons(own).count_ones();
        let real_down = if buttons.is_null() || n_buttons > 1024 { 0 } else { unsafe { std::slice::from_raw_parts(buttons, n_buttons as usize) }.iter().filter(|b| **b != 0).count() as u32 };
        if snap.pad.connected && own_down > 0 {
            RAWS.learn(this, real_down, own_down);
        }
        if snap.pad.connected && RAWS.is(this) == Some(true) {
            return hr;
        }
    }
    if !buttons.is_null() {
        unsafe { std::ptr::write_bytes(buttons, 0, n_buttons as usize) };
    }
    if !switches.is_null() {
        unsafe { std::ptr::write_bytes(switches, 0, n_switches as usize) };
    }
    if !axes.is_null() && n_axes > 0 {
        let now = unsafe { std::slice::from_raw_parts_mut(axes, n_axes as usize) };
        let mut held = AXES.lock().unwrap();
        let at_open = match held.iter().find(|(c, _)| *c == this) {
            Some((_, a)) if a.len() == now.len() => a.clone(),
            Some(_) => return hr,
            None => {
                held.push((this, now.to_vec()));
                now.to_vec()
            }
        };
        drop(held);
        let rest = REST.lock().unwrap();
        let learnt = rest.iter().find(|(c, r)| *c == this && r.len() == now.len()).map(|(_, r)| r.clone());
        drop(rest);
        for (i, v) in now.iter_mut().enumerate() {
            *v = held_axis(learnt.as_ref().and_then(|r| r[i].rest()), at_open[i]);
        }
    }
    hr
}

/// `Gamepad.Vibration = ...` in split screen: the copy's own controller shakes, no
/// other: its physical gamepad once learnt, else its XInput slot (a virtual pad's
/// player has nothing to shake).
unsafe extern "system" fn put_vibration(this: usize, v: Vibration) -> i32 {
    let Some(h) = VIBRATION.get() else { return 0 };
    let Policy::Virtual(snap) = super::topology() else { return unsafe { h.call(this, v) } };
    if GAMEPADS.is(this) == Some(true) {
        return unsafe { h.call(this, v) };
    }
    if let Some(k) = snap.pad.xinput_slot.filter(|_| snap.pad.connected && super::policy() != Policy::BlockAll) {
        let motor = |x: f64| (x.clamp(0.0, 1.0) * 65535.0) as u16;
        super::real_xinput_vibrate(k.into(), motor(v.left), motor(v.right));
    }
    crate::log::throttled("input: wgi vibration", 1, |_| {
        log!("input: Windows.Gaming.Input vibration on a gamepad not learnt as this copy's: sent to its own XInput controller instead, if it has one");
    });
    0
}

/// UINavigationReading: Timestamp (u64), RequiredButtons (u32), OptionalButtons (u32).
/// The copy's own controller navigates (its physical one once learnt, else the first
/// read, from the copy's controller); the others read no buttons.
unsafe extern "system" fn nav_reading(this: usize, out: *mut u8) -> i32 {
    let Some(h) = NAV.get() else { return -1 };
    let hr = unsafe { h.call(this, out) };
    if hr < 0 || out.is_null() {
        return hr;
    }
    match policy() {
        Policy::PassThrough => {}
        Policy::BlockAll => unsafe { std::ptr::write_bytes(out.add(8), 0, 8) },
        Policy::Virtual(snap) => {
            let buf = unsafe { &mut *(out as *mut [u8; 16]) };
            let own = nav_buttons(own_pad(&snap).xinput_buttons());
            let real = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            NAVS.learn(this, real, own.0);
            let mine = snap.pad.connected && NAVS.is(this).unwrap_or_else(|| nav_one(this));
            let (req, opt) = if mine { own } else { (0, 0) };
            buf[8..12].copy_from_slice(&req.to_le_bytes());
            buf[12..16].copy_from_slice(&opt.to_le_bytes());
        }
    }
    hr
}

/// The UI navigation controller standing in for the copy's until its own is learnt.
fn nav_one(this: usize) -> bool {
    static ONE: Mutex<Option<usize>> = Mutex::new(None);
    *ONE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(this) == this
}

/// XInput buttons as UI navigation's (required, optional): Menu, View, Accept (A),
/// Cancel (B), the d-pad; Context1/2 (X, Y), PageUp/Down (the shoulders).
pub(super) fn nav_buttons(b: u16) -> (u32, u32) {
    const REQ: [(u16, u32); 8] = [(0x0010, 1), (0x0020, 2), (0x1000, 4), (0x2000, 8), (0x0001, 16), (0x0002, 32), (0x0004, 64), (0x0008, 128)];
    const OPT: [(u16, u32); 4] = [(0x4000, 1), (0x8000, 2), (0x0100, 16), (0x0200, 32)];
    let pick = |map: &[(u16, u32)]| map.iter().filter(|(x, _)| b & x != 0).fold(0, |acc, (_, w)| acc | w);
    (pick(&REQ), pick(&OPT))
}

/// A reading of a class no copy is seated on (arcade stick, flight stick, wheel): at
/// rest in split screen and while the overlay has the controllers, its timestamp kept;
/// `rest` writes the rest after the timestamp.
fn at_rest(h: &GenericDetour<GamepadReading>, this: usize, out: *mut u8, rest: impl FnOnce(*mut u8)) -> i32 {
    let hr = unsafe { h.call(this, out) };
    if hr >= 0 && !out.is_null() && policy() != Policy::PassThrough {
        super::first_use(&super::USED_WGI_OTHER, "Windows.Gaming.Input arcade sticks, flight sticks or wheels (at rest in split screen)");
        rest(out);
    }
    hr
}

/// ArcadeStickReading: Timestamp, Buttons.
unsafe extern "system" fn arcade_reading(this: usize, out: *mut u8) -> i32 {
    ARCADE.get().map_or(-1, |h| at_rest(h, this, out, |o| unsafe { std::ptr::write_bytes(o.add(8), 0, 4) }))
}

/// FlightStickReading: Timestamp, Buttons, HatSwitch (0: centre), Roll, Pitch, Yaw,
/// Throttle (doubles; 0 is centred, and the throttle closed).
unsafe extern "system" fn flight_reading(this: usize, out: *mut u8) -> i32 {
    FLIGHT.get().map_or(-1, |h| at_rest(h, this, out, |o| unsafe { std::ptr::write_bytes(o.add(8), 0, 40) }))
}

/// RacingWheelReading: Timestamp, Buttons, PatternShifterGear (kept), Wheel, Throttle,
/// Brake, Clutch, Handbrake (doubles: wheel centred, pedals up).
unsafe extern "system" fn wheel_reading(this: usize, out: *mut u8) -> i32 {
    WHEEL.get().map_or(-1, |h| {
        at_rest(h, this, out, |o| unsafe {
            std::ptr::write_bytes(o.add(8), 0, 4);
            std::ptr::write_bytes(o.add(16), 0, 40);
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_navigation_reads_the_pad() {
        assert_eq!(nav_buttons(0x1000 | 0x0001 | 0x0010), (4 | 16 | 1, 0), "A, up, Start: Accept, Up, Menu");
        assert_eq!(nav_buttons(0x4000 | 0x0200), (0, 1 | 32), "X, RB: Context1, PageDown");
        assert_eq!(size_of::<Vibration>(), 32);
    }

    /// The first controller of `class` (its statics `iid`), AddRef'd; None without one.
    fn first(class: &str, iid: &[u8; 16]) -> Option<usize> {
        let combase = module("combase.dll")?;
        let get = |n: &std::ffi::CStr| proc_addr(combase, n.as_ptr().cast());
        let init: unsafe extern "system" fn(i32) -> i32 = unsafe { std::mem::transmute(get(c"RoInitialize")?) };
        let create_string: CreateString = unsafe { std::mem::transmute(get(c"WindowsCreateString")?) };
        let delete_string: DeleteString = unsafe { std::mem::transmute(get(c"WindowsDeleteString")?) };
        let factory: Factory = unsafe { std::mem::transmute(get(c"RoGetActivationFactory")?) };
        unsafe { init(1) };
        let wide: Vec<u16> = class.encode_utf16().collect();
        let mut hs = 0usize;
        if unsafe { create_string(wide.as_ptr(), wide.len() as u32, &mut hs) } < 0 {
            return None;
        }
        let mut s = 0usize;
        let hr = unsafe { factory(hs, iid, &mut s) };
        unsafe { delete_string(hs) };
        if hr < 0 {
            return None;
        }
        let c = unsafe { first_of(s, 10) };
        unsafe { release(s) };
        c
    }

    /// With a gamepad connected, the hooks sit where Windows' metadata puts the methods:
    /// a gamepad's and its UI navigation controller's readings, and a vibration (a
    /// struct passed by value, which x86 and x64 pass differently), all go through them
    /// to the real ones.
    #[test]
    fn hooks_sit_where_the_metadata_says() {
        let wide: Vec<u16> = "Windows.Gaming.Input.dll".encode_utf16().chain(Some(0)).collect();
        unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
        install();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while !both() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let Some(pad) = first("Windows.Gaming.Input.Gamepad", &IID_GAMEPAD_STATICS) else {
            crate::skip("no gamepad connected");
            return;
        };
        assert!(GAMEPAD.get().is_some() && VIBRATION.get().is_some(), "gamepad reading and vibration hooked");
        let read: GamepadReading = unsafe { std::mem::transmute(slot(pad, 8)) };
        let mut r = [0u8; 64];
        assert!(unsafe { read(pad, r.as_mut_ptr()) } >= 0);
        let vibrate: PutVibration = unsafe { std::mem::transmute(slot(pad, 7)) };
        assert!(unsafe { vibrate(pad, Vibration::default()) } >= 0, "no vibration, through the hook");
        // get_Vibration (slot 6) reads back what was put.
        let get: unsafe extern "system" fn(usize, *mut Vibration) -> i32 = unsafe { std::mem::transmute(slot(pad, 6)) };
        let mut v = Vibration { left: 9.0, ..Default::default() };
        assert!(unsafe { get(pad, &mut v) } >= 0);
        assert_eq!(v.left, 0.0);
        unsafe { release(pad) };
        if let Some(nav) = first("Windows.Gaming.Input.UINavigationController", &IID_NAV_STATICS) {
            assert!(NAV.get().is_some(), "UI navigation hooked");
            let read: GamepadReading = unsafe { std::mem::transmute(slot(nav, 6)) };
            let mut r = [0u8; 16];
            assert!(unsafe { read(nav, r.as_mut_ptr()) } >= 0);
            unsafe { release(nav) };
        }
    }

    #[test]
    fn a_learnt_controller_is_the_copys_until_its_seat_changes() {
        let l = Learner::new("test");
        assert_eq!(l.is(0xA), None, "none learnt");
        l.learn(0xA, 4, 8);
        assert_eq!(l.is(0xA), None, "other buttons");
        l.learn(0xA, 4, 4);
        assert_eq!((l.is(0xA), l.is(0xB)), (Some(true), Some(false)));
        l.forget();
        assert_eq!(l.is(0xA), None);
    }

    #[test]
    fn a_raw_axis_held_back_reads_its_rest() {
        let mut stick = Rest::default();
        let mut trigger = Rest::default();
        for i in 0..200 {
            stick.saw(if i % 10 == 0 { 0.9 } else { 0.5 });
            trigger.saw(0.0);
        }
        assert_eq!(stick.rest(), Some(0.5));
        assert_eq!(trigger.rest(), Some(0.0));
        assert_eq!(Rest::default().rest(), None, "not seen enough");
        assert_eq!(held_axis(Some(0.5), 0.0), 0.5, "a stick held hard left at opening");
        assert_eq!(held_axis(None, 0.58), 0.5);
        assert_eq!(held_axis(None, 1.0), 1.0);
        let mut busy = Rest::default();
        for _ in 0..70_000 {
            busy.saw(0.25);
        }
        assert_eq!(busy.rest(), Some(0.25), "counts halve rather than overflow");
    }

    #[test]
    fn one_gamepad_carries_the_controller() {
        let t0 = std::time::Instant::now();
        let later = |ms| t0 + std::time::Duration::from_millis(ms);
        *ONE.lock().unwrap() = None;
        assert!(the_one(0xA0, t0), "the first read takes it");
        assert!(!the_one(0xB0, later(10)), "another pad: at rest");
        assert!(the_one(0xA0, later(20)));
        assert!(!the_one(0xB0, later(1500)));
        assert!(the_one(0xB0, later(3600)), "the first one let go");
        assert!(!the_one(0xA0, later(3610)));
    }

    /// A reading gets a new timestamp whenever what it says changes, whatever the real
    /// pad underneath does.
    #[test]
    fn a_changed_reading_gets_a_new_timestamp() {
        let pad = |lx| VirtualPad { connected: true, thumb_lx: lx, ..Default::default() };
        assert_eq!(stamp(0x51, &pad(0), 100), 100, "the first: the real one");
        assert_eq!(stamp(0x51, &pad(0), 100), 100, "unchanged");
        assert_eq!(stamp(0x51, &pad(9000), 100), 101, "the stick moved, the real pad didn't");
        assert_eq!(stamp(0x51, &pad(0), 100), 102, "and back");
        assert_eq!(stamp(0x51, &pad(0), 500), 102, "the real pad moved, the reading didn't");
        assert_eq!(stamp(0x51, &pad(5), 500), 500, "a later real timestamp is kept");
        assert_eq!(stamp(0x52, &pad(5), 7), 7, "each gamepad on its own");
    }

    #[test]
    fn wgi_gamepad_packs_virtual_pad() {
        let pad = VirtualPad {
            connected: true,
            buttons: 0x1000 | 0x0010, // A + Start
            left_trigger: 255,
            right_trigger: 0,
            thumb_lx: 32767,
            thumb_ly: -32767,
            thumb_rx: 0,
            thumb_ry: 0,
            packet_number: 1,
            ..Default::default()
        };
        let mut buf = [0u8; 64];
        buf[0..8].copy_from_slice(&0x1234_5678u64.to_le_bytes());
        pack_wgi_gamepad(&pad, &mut buf);
        assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 0x1234_5678, "timestamp preserved");
        assert_eq!(u32::from_le_bytes(buf[8..12].try_into().unwrap()), 0x0001 | 0x0004, "Menu | A");
        assert!((f64::from_le_bytes(buf[16..24].try_into().unwrap()) - 1.0).abs() < 1e-6);
        assert!((f64::from_le_bytes(buf[32..40].try_into().unwrap()) - 1.0).abs() < 1e-6);
        assert!((f64::from_le_bytes(buf[40..48].try_into().unwrap()) + 1.0).abs() < 1e-6);
    }
}
