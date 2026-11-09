//! Windows.Gaming.Input: `Gamepad.GetCurrentReading` reads at rest (buttons up, sticks
//! and triggers at zero, the timestamp kept), and `RawGameController.GetCurrentReading`
//! reads no buttons, hats centred and the axes as they were when the overlay opened.
//!
//! A Gamepad object exists only for a connected pad, so the method is found through one:
//! PeerParty asks the Gamepad (and RawGameController) statics for the pads, takes the
//! first one's vtable, and detours the method there (every pad shares it). Tried each
//! time the overlay opens until it works (the first time may have no pad connected), and
//! only in games that have Windows.Gaming.Input loaded.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::{module, proc_addr};
use crate::hook::with_lock;
use crate::log;
use crate::overlay::visible;

type GamepadReading = unsafe extern "system" fn(usize, *mut u8) -> i32;
type RawReading = unsafe extern "system" fn(usize, u32, *mut u8, u32, *mut i32, u32, *mut f64, *mut u64) -> i32;

static GAMEPAD: OnceLock<GenericDetour<GamepadReading>> = OnceLock::new();
static RAW: OnceLock<GenericDetour<RawReading>> = OnceLock::new();
static TRIED_GAMEPAD: AtomicBool = AtomicBool::new(false);
static TRIED_RAW: AtomicBool = AtomicBool::new(false);
/// Raw controllers' axes when the overlay opened, per controller.
static AXES: Mutex<Vec<(usize, Vec<f64>)>> = Mutex::new(Vec::new());

const IID_GAMEPAD_STATICS: [u8; 16] = super::guid(0x8BBCE529, 0xD49C, 0x39E9, [0x95, 0x60, 0xE4, 0x7D, 0xDE, 0x96, 0xB7, 0xC8]);
const IID_RAW_STATICS: [u8; 16] = super::guid(0xEB8D0792, 0xE95A, 0x4B19, [0xAF, 0xC7, 0x0A, 0x59, 0xF8, 0xBF, 0x75, 0x9E]);

static BUSY: AtomicBool = AtomicBool::new(false);

/// The overlay opened: hooks what isn't hooked yet, on a thread of our own (WinRT calls
/// need an apartment, and the game's threads keep theirs). Windows fills the pad list a
/// moment after it's first asked for, so an empty list is asked again once.
pub fn install() -> usize {
    if module("windows.gaming.input.dll").is_none() || (GAMEPAD.get().is_some() && RAW.get().is_some()) {
        return 0;
    }
    if BUSY.swap(true, Ordering::Relaxed) {
        return 0;
    }
    std::thread::spawn(|| {
        let mut n = unsafe { install_on_own_thread() };
        if GAMEPAD.get().is_none() || RAW.get().is_none() {
            std::thread::sleep(std::time::Duration::from_millis(300));
            n += unsafe { install_on_own_thread() };
        }
        if n > 0 {
            log!("padblock: Windows.Gaming.Input: {n} held back while the overlay shows");
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
        // IGamepadStatics::get_Gamepads is slot 10; IGamepad::GetCurrentReading slot 8.
        if let Some(pad) = unsafe { first_of(s, 10) } {
            n += arm(&GAMEPAD, unsafe { slot(pad, 8) }, gamepad_reading as GamepadReading, "Gamepad.GetCurrentReading");
            unsafe { release(pad) };
        } else if !TRIED_GAMEPAD.swap(true, Ordering::Relaxed) {
            log!("padblock: Windows.Gaming.Input has no gamepad connected yet; tried again next time the overlay opens");
        }
        unsafe { release(s) };
    }
    if RAW.get().is_none() {
        let s = statics("Windows.Gaming.Input.RawGameController", &IID_RAW_STATICS);
        // IRawGameControllerStatics::get_RawGameControllers is slot 10;
        // IRawGameController::GetCurrentReading slot 13.
        if let Some(c) = unsafe { first_of(s, 10) } {
            n += arm(&RAW, unsafe { slot(c, 13) }, raw_reading as RawReading, "RawGameController.GetCurrentReading");
            unsafe { release(c) };
        } else if !TRIED_RAW.swap(true, Ordering::Relaxed) {
            log!("padblock: Windows.Gaming.Input has no raw controller yet; tried again next time the overlay opens");
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

/// The first item of the IVectorView a statics method (`getter` slot) returns.
unsafe fn first_of(statics: usize, getter: usize) -> Option<usize> {
    if statics == 0 {
        return None;
    }
    let get: unsafe extern "system" fn(usize, *mut usize) -> i32 = unsafe { std::mem::transmute(slot(statics, getter)) };
    let mut view = 0usize;
    if unsafe { get(statics, &mut view) } < 0 || view == 0 {
        return None;
    }
    // IVectorView<T>: GetAt is slot 6, get_Size slot 7.
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
                match crate::hook::frozen(addr, || unsafe { cell.get().unwrap().enable() }) {
                    Ok(()) => 1,
                    Err(e) => {
                        log!("padblock {label}: {e}");
                        0
                    }
                }
            }
            Err(e) => {
                log!("padblock {label} not hooked: {e}");
                0
            }
        }
    })
}

/// How many of the two methods are hooked.
#[cfg(test)]
pub fn hooked() -> usize {
    usize::from(GAMEPAD.get().is_some()) + usize::from(RAW.get().is_some())
}

/// The overlay closed: raw controllers' axes are learnt afresh next time.
pub fn hidden() {
    AXES.lock().unwrap().clear();
}

unsafe extern "system" fn gamepad_reading(this: usize, out: *mut u8) -> i32 {
    let Some(h) = GAMEPAD.get() else { return -1 };
    let hr = unsafe { h.call(this, out) };
    if hr >= 0 && visible() && !out.is_null() {
        // GamepadReading: Timestamp (u64, kept), Buttons (u32), then six doubles.
        unsafe { std::ptr::write_bytes(out.add(8), 0, 56) };
    }
    hr
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
    if hr < 0 || !visible() {
        return hr;
    }
    if !buttons.is_null() {
        unsafe { std::ptr::write_bytes(buttons, 0, n_buttons as usize) };
    }
    if !switches.is_null() {
        // GameControllerSwitchPosition::Center.
        unsafe { std::ptr::write_bytes(switches, 0, n_switches as usize) };
    }
    if !axes.is_null() && n_axes > 0 {
        let now = unsafe { std::slice::from_raw_parts_mut(axes, n_axes as usize) };
        let mut held = AXES.lock().unwrap();
        match held.iter().find(|(c, _)| *c == this) {
            Some((_, a)) if a.len() == now.len() => now.copy_from_slice(a),
            Some(_) => {}
            None => held.push((this, now.to_vec())),
        }
    }
    hr
}
