//! Windows.Gaming.Input: `Gamepad.GetCurrentReading` and `RawGameController.GetCurrentReading`
//! driven by `input::policy()`.
//!
//! - `Policy::PassThrough`: passes through and learns raw controller axis rest positions.
//! - `Policy::BlockAll`: Gamepad reads at rest (timestamp kept); RawGameController reads
//!   no buttons, hats centred and axes at their learnt rest.
//! - `Policy::Virtual(snap)`: Gamepad synthesizes `VirtualPad` into `GamepadReading` (if
//!   connected, else at rest); RawGameController holds at rest.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use peerparty_protocol::input_shm::VirtualPad;
use retour::GenericDetour;

use super::{Policy, module, policy, proc_addr};
use crate::log;
use crate::sys::hook::with_lock;

type GamepadReading = unsafe extern "system" fn(usize, *mut u8) -> i32;
type RawReading = unsafe extern "system" fn(usize, u32, *mut u8, u32, *mut i32, u32, *mut f64, *mut u64) -> i32;

static GAMEPAD: OnceLock<GenericDetour<GamepadReading>> = OnceLock::new();
static RAW: OnceLock<GenericDetour<RawReading>> = OnceLock::new();
static TRIED_GAMEPAD: AtomicBool = AtomicBool::new(false);
static TRIED_RAW: AtomicBool = AtomicBool::new(false);
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

static BUSY: AtomicBool = AtomicBool::new(false);

pub fn install() -> usize {
    if module("windows.gaming.input.dll").is_none() || (GAMEPAD.get().is_some() && RAW.get().is_some()) {
        return 0;
    }
    if BUSY.swap(true, Ordering::Relaxed) {
        return 0;
    }
    std::thread::spawn(|| {
        // Its controller lists fill in a moment after it starts (and stay empty until a
        // controller is plugged in): tried for 20 s, then again at the next call.
        let mut n = 0;
        for i in 0..40 {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_millis(if i == 1 { 300 } else { 500 }));
            }
            n += unsafe { install_on_own_thread() };
            if GAMEPAD.get().is_some() && RAW.get().is_some() {
                break;
            }
        }
        if n > 0 {
            log!("input: Windows.Gaming.Input: {n} hooked");
        }
        BUSY.store(false, Ordering::Relaxed);
    });
    0
}

unsafe fn install_on_own_thread() -> usize {
    let Some(combase) = module("combase.dll") else { return 0 };
    let (Some(init), Some(create_string), Some(delete_string), Some(factory)) = (
        proc_addr(combase, c"RoInitialize".as_ptr().cast()),
        proc_addr(combase, c"WindowsCreateString".as_ptr().cast()),
        proc_addr(combase, c"WindowsDeleteString".as_ptr().cast()),
        proc_addr(combase, c"RoGetActivationFactory".as_ptr().cast()),
    ) else {
        return 0;
    };
    let init: unsafe extern "system" fn(i32) -> i32 = unsafe { std::mem::transmute(init) };
    let create_string: unsafe extern "system" fn(*const u16, u32, *mut usize) -> i32 = unsafe { std::mem::transmute(create_string) };
    let delete_string: unsafe extern "system" fn(usize) -> i32 = unsafe { std::mem::transmute(delete_string) };
    let factory: unsafe extern "system" fn(usize, *const [u8; 16], *mut usize) -> i32 = unsafe { std::mem::transmute(factory) };
    unsafe { init(1) }; // RO_INIT_MULTITHREADED
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
    let mut n = 0;
    if GAMEPAD.get().is_none() {
        let s = statics("Windows.Gaming.Input.Gamepad", &IID_GAMEPAD_STATICS);
        if let Some(pad) = unsafe { first_of(s, 10) } {
            n += arm(&GAMEPAD, unsafe { slot(pad, 8) }, gamepad_reading as GamepadReading, "Gamepad.GetCurrentReading");
            unsafe { release(pad) };
        } else if !TRIED_GAMEPAD.swap(true, Ordering::Relaxed) {
            log!("input: Windows.Gaming.Input has no gamepad connected yet; retried next time");
        }
        unsafe { release(s) };
    }
    if RAW.get().is_none() {
        let s = statics("Windows.Gaming.Input.RawGameController", &IID_RAW_STATICS);
        if let Some(c) = unsafe { first_of(s, 10) } {
            n += arm(&RAW, unsafe { slot(c, 13) }, raw_reading as RawReading, "RawGameController.GetCurrentReading");
            unsafe { release(c) };
        } else if !TRIED_RAW.swap(true, Ordering::Relaxed) {
            log!("input: Windows.Gaming.Input has no raw controller yet; retried next time");
        }
        unsafe { release(s) };
    }
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
    usize::from(GAMEPAD.get().is_some()) + usize::from(RAW.get().is_some())
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

/// Converts XInput `wButtons` to `Windows.Gaming.Input.GamepadButtons`.
fn wgi_buttons(xinput: u16) -> u32 {
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
            let pad = if the_one(this, std::time::Instant::now()) {
                // A real XInput pad read now, in this process: not the session's copy of
                // it, which is a tick or two behind. A HID pad the game reads natively
                // isn't echoed here too.
                match snap.pad.xinput_slot.and_then(|k| super::real_xinput(k.into()).map(|s| super::pad_of_xinput(k, &s))) {
                    Some(p) => p,
                    None if snap.pad.xinput_slot.is_none() && super::own_hid_read(snap.pad.hid_key) => VirtualPad::default(),
                    None => snap.pad,
                }
            } else {
                VirtualPad::default()
            };
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
    if matches!(policy(), Policy::Virtual(_)) {
        super::first_use(&super::USED_WGI_RAW, "Windows.Gaming.Input raw controllers (held at rest in split screen)");
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

#[cfg(test)]
mod tests {
    use super::*;

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
