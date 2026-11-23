//! Windows' oldest joystick calls (winmm.dll's `joyGetPos`, `joyGetPosEx`,
//! `joyGetDevCaps`), which some older games still poll. They number joysticks, with no
//! way to tell which device each is, so in a split-screen copy joystick 0 is the copy's
//! controller (the virtual pad) and the others aren't there; while the overlay has the
//! controllers they read at rest. Hooked by export when winmm.dll is there: nothing in it
//! is called from a load notification.

use std::sync::OnceLock;

use peerparty_protocol::input_shm::VirtualPad;
use retour::GenericDetour;

use super::{Policy, arm_once, module, proc_addr};

type GetPosEx = unsafe extern "system" fn(u32, *mut u32) -> u32;
type GetPos = unsafe extern "system" fn(u32, *mut u32) -> u32;
type GetDevCaps = unsafe extern "system" fn(usize, *mut u8, u32) -> u32;

static POS_EX: OnceLock<GenericDetour<GetPosEx>> = OnceLock::new();
static POS: OnceLock<GenericDetour<GetPos>> = OnceLock::new();
static CAPS_W: OnceLock<GenericDetour<GetDevCaps>> = OnceLock::new();
static CAPS_A: OnceLock<GenericDetour<GetDevCaps>> = OnceLock::new();

const JOYERR_NOERROR: u32 = 0;
const JOYERR_UNPLUGGED: u32 = 167;
/// `JOY_POVCENTERED`.
const POV_CENTRED: u32 = 0xFFFF;

pub(super) fn module_loaded() -> usize {
    let Some(m) = module("winmm.dll") else { return 0 };
    let mut n = 0;
    if let Some(a) = proc_addr(m, c"joyGetPosEx".as_ptr().cast()) {
        n += arm_once(&POS_EX, a, get_pos_ex as GetPosEx, "joyGetPosEx");
    }
    if let Some(a) = proc_addr(m, c"joyGetPos".as_ptr().cast()) {
        n += arm_once(&POS, a, get_pos as GetPos, "joyGetPos");
    }
    if let Some(a) = proc_addr(m, c"joyGetDevCapsW".as_ptr().cast()) {
        n += arm_once(&CAPS_W, a, caps_w as GetDevCaps, "joyGetDevCapsW");
    }
    if let Some(a) = proc_addr(m, c"joyGetDevCapsA".as_ptr().cast()) {
        n += arm_once(&CAPS_A, a, caps_a as GetDevCaps, "joyGetDevCapsA");
    }
    n
}

/// What joystick `id` is now: as it is, the virtual pad, at rest, or not there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Joy {
    Real,
    Pad(VirtualPad),
    Rest,
    Absent,
}

fn which(id: u32) -> Joy {
    match super::topology() {
        Policy::PassThrough => Joy::Real,
        Policy::BlockAll => Joy::Rest,
        Policy::Virtual(snap) => {
            super::first_use(&super::USED_WINMM, "winmm joysticks (joystick 0 is its controller, the others aren't there)");
            match (id, snap.pad.connected, super::policy()) {
                (0, true, Policy::Virtual(_)) => Joy::Pad(snap.pad),
                // The overlay has the controllers: the copy's own at rest.
                (0, true, _) => Joy::Rest,
                _ => Joy::Absent,
            }
        }
    }
}

/// A 0..65535 axis from a -1..1 value.
fn axis(norm: f64) -> u32 {
    ((norm.clamp(-1.0, 1.0) + 1.0) * 32767.5).round() as u32
}

/// `JOYINFOEX`'s words from `dwXpos` on: X, Y, Z, R, U, V, buttons, button number, POV.
pub(super) fn pack_ex(pad: Option<&VirtualPad>) -> [u32; 9] {
    let Some(p) = pad else { return [32767, 32767, 32767, 32767, 32767, 32767, 0, 0, POV_CENTRED] };
    let b = p.xinput_buttons();
    const ORDER: [u16; 10] = [0x1000, 0x2000, 0x4000, 0x8000, 0x0100, 0x0200, 0x0020, 0x0010, 0x0040, 0x0080];
    let buttons = ORDER.iter().enumerate().filter(|(_, m)| b & **m != 0).fold(0u32, |acc, (i, _)| acc | (1 << i));
    let pov = match (b & 1 != 0, b & 2 != 0, b & 4 != 0, b & 8 != 0) {
        (true, false, false, false) => 0,
        (true, false, false, true) => 4500,
        (false, false, false, true) => 9000,
        (false, true, false, true) => 13500,
        (false, true, false, false) => 18000,
        (false, true, true, false) => 22500,
        (false, false, true, false) => 27000,
        (true, false, true, false) => 31500,
        _ => POV_CENTRED,
    };
    [
        axis(f64::from(p.thumb_lx) / 32767.0),
        axis(-f64::from(p.thumb_ly) / 32767.0),
        axis((f64::from(p.left_trigger) - f64::from(p.right_trigger)) / 255.0),
        axis(f64::from(p.thumb_rx) / 32767.0),
        axis(-f64::from(p.thumb_ry) / 32767.0),
        32767,
        buttons,
        buttons.count_ones(),
        pov,
    ]
}

unsafe extern "system" fn get_pos_ex(id: u32, info: *mut u32) -> u32 {
    let Some(h) = POS_EX.get() else { return JOYERR_UNPLUGGED };
    let joy = which(id);
    match joy {
        Joy::Real => unsafe { h.call(id, info) },
        Joy::Absent => JOYERR_UNPLUGGED,
        Joy::Rest | Joy::Pad(_) => {
            // JOYINFOEX: dwSize, dwFlags, then the words `pack_ex` fills.
            if info.is_null() || unsafe { *info } < 52 {
                return unsafe { h.call(id, info) };
            }
            let words = pack_ex(match &joy {
                Joy::Pad(p) => Some(p),
                _ => None,
            });
            for (i, w) in words.into_iter().enumerate() {
                unsafe { *info.add(2 + i) = w };
            }
            JOYERR_NOERROR
        }
    }
}

unsafe extern "system" fn get_pos(id: u32, info: *mut u32) -> u32 {
    let Some(h) = POS.get() else { return JOYERR_UNPLUGGED };
    let joy = which(id);
    match joy {
        Joy::Real => unsafe { h.call(id, info) },
        Joy::Absent => JOYERR_UNPLUGGED,
        Joy::Rest | Joy::Pad(_) => {
            if info.is_null() {
                return unsafe { h.call(id, info) };
            }
            let w = pack_ex(match &joy {
                Joy::Pad(p) => Some(p),
                _ => None,
            });
            // JOYINFO: X, Y, Z, buttons (the first four).
            for (i, v) in [w[0], w[1], w[2], w[6] & 0xF].into_iter().enumerate() {
                unsafe { *info.add(i) = v };
            }
            JOYERR_NOERROR
        }
    }
}

/// `joyGetDevCaps`' first argument is a `UINT_PTR` id (`WAVE_MAPPER`-style -1 asks for
/// the joystick control panel's: left alone).
fn caps(h: &GenericDetour<GetDevCaps>, id: usize, caps: *mut u8, size: u32) -> u32 {
    if id != usize::MAX && which(id as u32) == Joy::Absent {
        return JOYERR_UNPLUGGED;
    }
    unsafe { h.call(id, caps, size) }
}

unsafe extern "system" fn caps_w(id: usize, out: *mut u8, size: u32) -> u32 {
    CAPS_W.get().map_or(JOYERR_UNPLUGGED, |h| caps(h, id, out, size))
}

unsafe extern "system" fn caps_a(id: usize, out: *mut u8, size: u32) -> u32 {
    CAPS_A.get().map_or(JOYERR_UNPLUGGED, |h| caps(h, id, out, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_winmm_joystick_reads_the_pad() {
        assert_eq!(pack_ex(None), [32767, 32767, 32767, 32767, 32767, 32767, 0, 0, POV_CENTRED]);
        let pad = VirtualPad { connected: true, buttons: 0x1000 | 0x2000 | 0x0008, thumb_lx: 32767, thumb_ly: 32767, ..Default::default() };
        let w = pack_ex(Some(&pad));
        assert_eq!((w[0], w[1]), (65535, 0), "right, and up (Y grows down)");
        assert_eq!((w[6], w[7], w[8]), (0b11, 2, 9000), "A and B, the d-pad right");
    }
}
