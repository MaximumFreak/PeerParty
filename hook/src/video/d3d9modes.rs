//! A Direct3D 9 game's display modes in a split-screen pane: the adapter's mode reads as
//! the pane's size, and it's the one fullscreen mode listed. A game sizes its fullscreen
//! device from these (its first run's default, "native", a mode from the list); made
//! windowed in its pane (d3d9.rs `windowed`), that device's back buffer is the pane's
//! size, not a small listed mode stretched over the pane (a fresh setting's 640x480).
//!
//! `IDirect3D9` slots 6–8 (GetAdapterModeCount, EnumAdapterModes, GetAdapterDisplayMode),
//! and `IDirect3D9Ex` 17–19 (their Ex forms, `D3DDISPLAYMODEEX`), from d3d9.h. A 9Ex
//! object's slots 6–8 are hooked on their own when they aren't the plain object's.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use retour::GenericDetour;

use crate::log;

type ModeCount = unsafe extern "system" fn(usize, u32, u32) -> u32;
type EnumModes = unsafe extern "system" fn(usize, u32, u32, u32, *mut DisplayMode) -> i32;
type DisplayModeOf = unsafe extern "system" fn(usize, u32, *mut DisplayMode) -> i32;
type ModeCountEx = unsafe extern "system" fn(usize, u32, usize) -> u32;
type EnumModesEx = unsafe extern "system" fn(usize, u32, usize, u32, *mut DisplayModeEx) -> i32;
type DisplayModeOfEx = unsafe extern "system" fn(usize, u32, *mut DisplayModeEx, *mut u32) -> i32;

/// `D3DDISPLAYMODE`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct DisplayMode {
    width: u32,
    height: u32,
    refresh: u32,
    format: u32,
}

/// `D3DDISPLAYMODEEX`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct DisplayModeEx {
    size: u32,
    width: u32,
    height: u32,
    refresh: u32,
    format: u32,
    scanline: u32,
}

const D3DERR_INVALIDCALL: i32 = 0x8876_086C_u32 as i32;

/// Per object kind (0: `IDirect3D9`'s, 1: a 9Ex object's own, when they differ).
static COUNT: [OnceLock<GenericDetour<ModeCount>>; 2] = [OnceLock::new(), OnceLock::new()];
static ENUM: [OnceLock<GenericDetour<EnumModes>>; 2] = [OnceLock::new(), OnceLock::new()];
static CURRENT: [OnceLock<GenericDetour<DisplayModeOf>>; 2] = [OnceLock::new(), OnceLock::new()];
static COUNT_EX: OnceLock<GenericDetour<ModeCountEx>> = OnceLock::new();
static ENUM_EX: OnceLock<GenericDetour<EnumModesEx>> = OnceLock::new();
static CURRENT_EX: OnceLock<GenericDetour<DisplayModeOfEx>> = OnceLock::new();

/// The pane's size, when this copy is in one.
fn pane() -> Option<(u32, u32)> {
    if !super::super::fullscreen::keep_windowed() {
        return None;
    }
    crate::window::virtual_size().filter(|(w, h)| *w > 0 && *h > 0)
}

/// Says once what the game is told.
fn told(what: &str, (w, h): (u32, u32)) {
    static TOLD: AtomicBool = AtomicBool::new(false);
    if !TOLD.swap(true, Ordering::Relaxed) {
        log!("video: the game asks Direct3D 9 for display modes ({what}): it's told its pane's {w}x{h}, the one fullscreen mode, so a fullscreen device it makes is the pane's size");
    }
}

/// The count of modes a game is told: one while the adapter has any in that format.
fn pane_count(real: u32) -> u32 {
    real.min(1)
}

/// Mode `index` as told in a pane: the first real mode's format and the screen's refresh
/// rate (`refresh`, 0 when unknown: the real mode's), at the pane's size; none past it.
fn pane_mode(index: u32, real_first: Option<DisplayMode>, refresh: u32, pane: (u32, u32)) -> Option<DisplayMode> {
    let m = real_first.filter(|_| index == 0)?;
    Some(DisplayMode { width: pane.0, height: pane.1, refresh: if refresh != 0 { refresh } else { m.refresh }, format: m.format })
}

/// Where the plain object's slots 6–8 are (0: not hooked yet).
static PLAIN: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];

/// Hooks the mode calls of `d3d`: an `IDirect3D9`, or an `IDirect3D9Ex` when `ex` (hooked
/// after the plain one: its slots 6–8 are hooked again only where they're its own).
pub(super) fn install(d3d: usize, ex: bool) -> usize {
    use super::super::com::slot;
    let k = usize::from(ex);
    let mut n = 0;
    for (i, s) in [6usize, 7, 8].into_iter().enumerate() {
        let at = slot(d3d, s);
        if ex && PLAIN[i].load(Ordering::Acquire) == at {
            continue;
        }
        if !ex {
            PLAIN[i].store(at, Ordering::Release);
        }
        n += match (s, k) {
            (6, 0) => super::arm(&COUNT[0], at, count0 as ModeCount, "GetAdapterModeCount"),
            (6, _) => super::arm(&COUNT[1], at, count1 as ModeCount, "GetAdapterModeCount (9Ex)"),
            (7, 0) => super::arm(&ENUM[0], at, enum0 as EnumModes, "EnumAdapterModes"),
            (7, _) => super::arm(&ENUM[1], at, enum1 as EnumModes, "EnumAdapterModes (9Ex)"),
            (_, 0) => super::arm(&CURRENT[0], at, current0 as DisplayModeOf, "GetAdapterDisplayMode"),
            _ => super::arm(&CURRENT[1], at, current1 as DisplayModeOf, "GetAdapterDisplayMode (9Ex)"),
        };
    }
    if ex {
        n += super::arm(&COUNT_EX, slot(d3d, 17), count_ex as ModeCountEx, "GetAdapterModeCountEx");
        n += super::arm(&ENUM_EX, slot(d3d, 18), enum_ex as EnumModesEx, "EnumAdapterModesEx");
        n += super::arm(&CURRENT_EX, slot(d3d, 19), current_ex as DisplayModeOfEx, "GetAdapterDisplayModeEx");
    }
    n
}

fn count(k: usize, this: usize, adapter: u32, format: u32) -> u32 {
    let real = unsafe { COUNT[k].get().unwrap().call(this, adapter, format) };
    match pane() {
        Some(p) => {
            told("GetAdapterModeCount", p);
            pane_count(real)
        }
        None => real,
    }
}

fn enum_modes(k: usize, this: usize, adapter: u32, format: u32, index: u32, out: *mut DisplayMode) -> i32 {
    let real = |i: u32, out: *mut DisplayMode| unsafe { ENUM[k].get().unwrap().call(this, adapter, format, i, out) };
    let Some(p) = pane() else { return real(index, out) };
    told("EnumAdapterModes", p);
    if out.is_null() {
        return real(index, out);
    }
    let mut first = DisplayMode::default();
    let first = (real(0, &mut first) >= 0).then_some(first);
    let mut now = DisplayMode::default();
    // A 9Ex object whose GetAdapterDisplayMode is the plain one's has it hooked there.
    let refresh = match CURRENT[k].get().or(CURRENT[0].get()) {
        Some(h) if unsafe { h.call(this, adapter, &mut now) } >= 0 => now.refresh,
        _ => 0,
    };
    match pane_mode(index, first, refresh, p) {
        Some(m) => {
            unsafe { *out = m };
            0
        }
        None => D3DERR_INVALIDCALL,
    }
}

fn current(k: usize, this: usize, adapter: u32, out: *mut DisplayMode) -> i32 {
    let hr = unsafe { CURRENT[k].get().unwrap().call(this, adapter, out) };
    if hr >= 0
        && !out.is_null()
        && let Some(p) = pane()
    {
        told("GetAdapterDisplayMode", p);
        unsafe { ((*out).width, (*out).height) = p };
    }
    hr
}

unsafe extern "system" fn count0(this: usize, adapter: u32, format: u32) -> u32 {
    count(0, this, adapter, format)
}
unsafe extern "system" fn count1(this: usize, adapter: u32, format: u32) -> u32 {
    count(1, this, adapter, format)
}
unsafe extern "system" fn enum0(this: usize, adapter: u32, format: u32, index: u32, out: *mut DisplayMode) -> i32 {
    enum_modes(0, this, adapter, format, index, out)
}
unsafe extern "system" fn enum1(this: usize, adapter: u32, format: u32, index: u32, out: *mut DisplayMode) -> i32 {
    enum_modes(1, this, adapter, format, index, out)
}
unsafe extern "system" fn current0(this: usize, adapter: u32, out: *mut DisplayMode) -> i32 {
    current(0, this, adapter, out)
}
unsafe extern "system" fn current1(this: usize, adapter: u32, out: *mut DisplayMode) -> i32 {
    current(1, this, adapter, out)
}

unsafe extern "system" fn count_ex(this: usize, adapter: u32, filter: usize) -> u32 {
    let real = unsafe { COUNT_EX.get().unwrap().call(this, adapter, filter) };
    match pane() {
        Some(p) => {
            told("GetAdapterModeCountEx", p);
            pane_count(real)
        }
        None => real,
    }
}

unsafe extern "system" fn enum_ex(this: usize, adapter: u32, filter: usize, index: u32, out: *mut DisplayModeEx) -> i32 {
    let real = |i: u32, out: *mut DisplayModeEx| unsafe { ENUM_EX.get().unwrap().call(this, adapter, filter, i, out) };
    let Some(p) = pane() else { return real(index, out) };
    told("EnumAdapterModesEx", p);
    if out.is_null() {
        return real(index, out);
    }
    let mut first = DisplayModeEx { size: std::mem::size_of::<DisplayModeEx>() as u32, ..DisplayModeEx::default() };
    let ok = real(0, &mut first) >= 0;
    let mut now = DisplayModeEx { size: std::mem::size_of::<DisplayModeEx>() as u32, ..DisplayModeEx::default() };
    let refresh = if unsafe { CURRENT_EX.get().unwrap().call(this, adapter, &mut now, std::ptr::null_mut()) } >= 0 { now.refresh } else { 0 };
    let plain = ok.then_some(DisplayMode { width: first.width, height: first.height, refresh: first.refresh, format: first.format });
    match pane_mode(index, plain, refresh, p) {
        Some(m) => {
            unsafe { *out = DisplayModeEx { size: (*out).size, width: m.width, height: m.height, refresh: m.refresh, format: m.format, scanline: first.scanline } };
            0
        }
        None => D3DERR_INVALIDCALL,
    }
}

unsafe extern "system" fn current_ex(this: usize, adapter: u32, out: *mut DisplayModeEx, rotation: *mut u32) -> i32 {
    let hr = unsafe { CURRENT_EX.get().unwrap().call(this, adapter, out, rotation) };
    if hr >= 0
        && !out.is_null()
        && let Some(p) = pane()
    {
        told("GetAdapterDisplayModeEx", p);
        unsafe { ((*out).width, (*out).height) = p };
    }
    hr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_is_the_one_mode() {
        let first = DisplayMode { width: 640, height: 480, refresh: 60, format: 22 };
        let m = pane_mode(0, Some(first), 144, (1720, 1440)).unwrap();
        assert_eq!((m.width, m.height, m.refresh, m.format), (1720, 1440, 144, 22));
        let m = pane_mode(0, Some(first), 0, (1720, 1440)).unwrap();
        assert_eq!(m.refresh, 60, "the screen's rate unknown: the real mode's");
        assert!(pane_mode(1, Some(first), 144, (1720, 1440)).is_none(), "no second mode");
        assert!(pane_mode(0, None, 144, (1720, 1440)).is_none(), "no mode in that format: none");
        assert_eq!((pane_count(0), pane_count(1), pane_count(57)), (0, 1, 1));
    }
}
