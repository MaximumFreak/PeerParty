//! A copy in a split-screen pane stays windowed. A game set to exclusive fullscreen
//! takes the whole monitor (and may change its mode), which the other panes can't share,
//! and a fullscreen window isn't composed by the desktop any more, so the pane showing
//! it freezes on its last frame. In a pane:
//!
//! - DXGI (Direct3D 10, 11, 12): `SetFullscreenState(TRUE)` succeeds without going
//!   fullscreen, and `GetFullscreenState` then says fullscreen, so the game carries on as
//!   it would there (some games check every frame and try again while it says no).
//!   `ResizeTarget`, which sizes the window or the display mode, leaves both as they
//!   are: the pane sizes the window. A swap chain created fullscreen is created windowed.
//! - Direct3D 9: a device created or reset fullscreen is created or reset windowed.
//!
//! A copy that isn't in a pane gets the real calls.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use super::com::{self, Guid, release, vcall};
use super::dxgi::SwapChainDesc;
use crate::log;

type SetFullscreen = unsafe extern "system" fn(usize, i32, usize) -> i32;
type GetFullscreen = unsafe extern "system" fn(usize, *mut i32, *mut usize) -> i32;
type ResizeTarget = unsafe extern "system" fn(usize, usize) -> i32;
type CreateSwapChain = unsafe extern "system" fn(usize, usize, *mut SwapChainDesc, *mut usize) -> i32;
type CreateForHwnd = unsafe extern "system" fn(usize, usize, usize, usize, *const FullscreenDesc, usize, *mut usize) -> i32;

static SET_FULLSCREEN: OnceLock<GenericDetour<SetFullscreen>> = OnceLock::new();
static GET_FULLSCREEN: OnceLock<GenericDetour<GetFullscreen>> = OnceLock::new();
static RESIZE_TARGET: OnceLock<GenericDetour<ResizeTarget>> = OnceLock::new();
static CREATE_SWAP_CHAIN: OnceLock<GenericDetour<CreateSwapChain>> = OnceLock::new();
static CREATE_FOR_HWND: OnceLock<GenericDetour<CreateForHwnd>> = OnceLock::new();

/// Swap chains the game asked to be fullscreen, kept windowed.
static WANTED: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(Mutex::default);
static TOLD: AtomicBool = AtomicBool::new(false);

const S_OK: i32 = 0;
const IID_IDXGIFACTORY: Guid = Guid(0x7b7166ec, 0x21c7, 0x44ae, [0xb2, 0x1a, 0xc9, 0xae, 0x32, 0x1a, 0xe3, 0x69]);
const IID_IDXGIFACTORY2: Guid = Guid(0x50c83a1c, 0xe072, 0x4c48, [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0]);

/// `DXGI_SWAP_CHAIN_FULLSCREEN_DESC`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FullscreenDesc {
    refresh_num: u32,
    refresh_den: u32,
    scanline: u32,
    scaling: u32,
    windowed: i32,
}

/// Whether this copy is kept windowed: it's in a split-screen pane.
pub fn keep_windowed() -> bool {
    crate::window::in_pane()
}

/// Says once that the game asked for fullscreen.
pub fn told(how: &str) {
    if !TOLD.swap(true, Ordering::Relaxed) {
        log!("video: the game asks for exclusive fullscreen ({how}): it stays windowed in its pane");
    }
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, addr: usize, detour: T, label: &str) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    if addr == 0 {
        return 0;
    }
    let target: T = unsafe { T::from_ptr(addr as *const ()) };
    usize::from(crate::sys::detour::try_arm(slot, target, detour, "video", label, true) == crate::sys::detour::Armed::Hooked)
}

/// Hooks the fullscreen calls of DXGI's swap chains and factories, found through the
/// throwaway swap chain `swap` (dxgi.rs): every swap chain and factory shares them.
pub fn install_dxgi(swap: usize) -> usize {
    let mut n = 0;
    n += arm(&SET_FULLSCREEN, com::slot(swap, 10), set_fullscreen as SetFullscreen, "IDXGISwapChain::SetFullscreenState");
    n += arm(&GET_FULLSCREEN, com::slot(swap, 11), get_fullscreen as GetFullscreen, "IDXGISwapChain::GetFullscreenState");
    n += arm(&RESIZE_TARGET, com::slot(swap, 14), resize_target as ResizeTarget, "IDXGISwapChain::ResizeTarget");
    let mut factory = 0usize;
    // IDXGIObject::GetParent.
    if vcall!(swap, 6, fn(*const Guid, *mut usize) -> i32, &IID_IDXGIFACTORY, &mut factory) >= 0 && factory != 0 {
        n += arm(&CREATE_SWAP_CHAIN, com::slot(factory, 10), create_swap_chain as CreateSwapChain, "IDXGIFactory::CreateSwapChain");
        n += arm(&MAKE_ASSOCIATION, com::slot(factory, 8), make_window_association as MakeWindowAssociation, "IDXGIFactory::MakeWindowAssociation");
        let f2 = com::query(factory, &IID_IDXGIFACTORY2);
        if f2 != 0 {
            n += arm(&CREATE_FOR_HWND, com::slot(f2, 15), create_for_hwnd as CreateForHwnd, "IDXGIFactory2::CreateSwapChainForHwnd");
            release(f2);
        }
        release(factory);
    }
    n
}

fn wanted(swap: usize, on: bool) {
    let mut w = crate::sys::lock(&WANTED);
    if on {
        w.insert(swap);
    } else {
        w.remove(&swap);
    }
}

unsafe extern "system" fn set_fullscreen(swap: usize, fullscreen: i32, target: usize) -> i32 {
    let real = SET_FULLSCREEN.get().unwrap();
    if !keep_windowed() {
        return unsafe { real.call(swap, fullscreen, target) };
    }
    wanted(swap, fullscreen != 0);
    if fullscreen != 0 {
        told("SetFullscreenState");
    }
    // Back to windowed if it went fullscreen before the copy was put in a pane.
    let mut now = 0i32;
    let Some(get) = GET_FULLSCREEN.get() else { return S_OK };
    if unsafe { get.call(swap, &mut now, std::ptr::null_mut()) } >= 0 && now != 0 {
        return unsafe { real.call(swap, 0, 0) };
    }
    S_OK
}

unsafe extern "system" fn get_fullscreen(swap: usize, fullscreen: *mut i32, target: *mut usize) -> i32 {
    let hr = unsafe { GET_FULLSCREEN.get().unwrap().call(swap, fullscreen, target) };
    if hr < 0 || !keep_windowed() || !crate::sys::lock(&WANTED).contains(&swap) {
        return hr;
    }
    if !fullscreen.is_null() {
        unsafe { fullscreen.write(1) };
    }
    // The output a fullscreen swap chain would be on: the one its window is on.
    if !target.is_null() && unsafe { target.read() } == 0 {
        let mut out = 0usize;
        if vcall!(swap, 15, fn(*mut usize) -> i32, &mut out) >= 0 {
            unsafe { target.write(out) };
        }
    }
    hr
}

unsafe extern "system" fn resize_target(swap: usize, mode: usize) -> i32 {
    if keep_windowed() {
        return S_OK;
    }
    unsafe { RESIZE_TARGET.get().unwrap().call(swap, mode) }
}

type MakeWindowAssociation = unsafe extern "system" fn(usize, usize, u32) -> i32;
static MAKE_ASSOCIATION: OnceLock<GenericDetour<MakeWindowAssociation>> = OnceLock::new();
/// `DXGI_MWA_NO_ALT_ENTER`.
const NO_ALT_ENTER: u32 = 2;

/// DXGI watches the game's window for Alt+Enter and switches the swap chain to
/// fullscreen itself: in a pane that's turned off, as the game's own switch is.
unsafe extern "system" fn make_window_association(factory: usize, hwnd: usize, flags: u32) -> i32 {
    let real = MAKE_ASSOCIATION.get().unwrap();
    if !keep_windowed() || flags & NO_ALT_ENTER != 0 {
        return unsafe { real.call(factory, hwnd, flags) };
    }
    static TOLD_ALT: AtomicBool = AtomicBool::new(false);
    if !TOLD_ALT.swap(true, Ordering::Relaxed) {
        log!("video: DXGI's Alt+Enter switch to fullscreen is off for the game's window (0x{hwnd:x}): it stays in its pane");
    }
    unsafe { real.call(factory, hwnd, flags | NO_ALT_ENTER) }
}

/// `DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH`.
const ALLOW_MODE_SWITCH: u32 = 0x2;

/// Strips `DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH` while the copy is kept windowed so
/// DXGI cannot change the display mode on a swap chain create or resize.
pub(super) fn keep_flags(flags: u32, windowed: bool) -> u32 {
    if windowed { flags & !ALLOW_MODE_SWITCH } else { flags }
}

/// Drops any remembered fullscreen wish for `swap` (when its window is destroyed).
pub(super) fn forget_swap(swap: usize) {
    wanted(swap, false);
}

/// A swap chain was made: what's known of the one before at its address goes (a
/// fullscreen one's wish too: a windowed one made at the same address isn't it).
fn made_swap(swap: usize, device: usize) {
    wanted(swap, false);
    super::dxgi::forget(swap);
    super::d3d12::note_swap(swap, device);
}

/// Records a newly created swap chain, clearing stale state for its address before
/// recording whether the game asked for fullscreen so `made_swap` never wipes `want_fs`.
fn finish_create(swap: usize, device: usize, want_fs: bool) {
    made_swap(swap, device);
    if want_fs {
        wanted(swap, true);
    }
}

unsafe extern "system" fn create_swap_chain(factory: usize, device: usize, desc: *mut SwapChainDesc, out: *mut usize) -> i32 {
    let real = CREATE_SWAP_CHAIN.get().unwrap();
    if !keep_windowed() || desc.is_null() {
        let hr = unsafe { real.call(factory, device, desc, out) };
        if hr >= 0 && !out.is_null() {
            finish_create(unsafe { out.read() }, device, false);
        }
        return hr;
    }
    let windowed_ptr = unsafe { &raw mut (*desc).windowed };
    let flags_ptr = unsafe { &raw mut (*desc).flags };
    let orig_windowed = unsafe { windowed_ptr.read() };
    let orig_flags = unsafe { flags_ptr.read() };
    let want_fs = orig_windowed == 0;
    let new_flags = keep_flags(orig_flags, true);
    if want_fs {
        told("a fullscreen swap chain");
        // The game's own description, windowed for the call and put back after.
        unsafe { windowed_ptr.write(1) };
    }
    if new_flags != orig_flags {
        unsafe { flags_ptr.write(new_flags) };
    }
    let hr = unsafe { real.call(factory, device, desc, out) };
    if want_fs {
        unsafe { windowed_ptr.write(orig_windowed) };
    }
    if new_flags != orig_flags {
        unsafe { flags_ptr.write(orig_flags) };
    }
    if hr >= 0 && !out.is_null() {
        finish_create(unsafe { out.read() }, device, want_fs);
    }
    hr
}

unsafe extern "system" fn create_for_hwnd(
    factory: usize,
    device: usize,
    hwnd: usize,
    desc: usize,
    fs: *const FullscreenDesc,
    output: usize,
    out: *mut usize,
) -> i32 {
    let real = CREATE_FOR_HWND.get().unwrap();
    if !keep_windowed() {
        let hr = unsafe { real.call(factory, device, hwnd, desc, fs, output, out) };
        if hr >= 0 && !out.is_null() {
            finish_create(unsafe { out.read() }, device, false);
        }
        return hr;
    }
    // Copy DXGI_SWAP_CHAIN_DESC1 (12 u32s, Flags at index 11) so we can strip mode switch
    // without mutating the caller's const struct.
    let desc1 = (desc != 0).then(|| {
        let mut d = unsafe { *(desc as *const [u32; 12]) };
        d[11] = keep_flags(d[11], true);
        d
    });
    let desc_arg = desc1.as_ref().map_or(0, |d| d.as_ptr() as usize);
    let want_fs = !fs.is_null() && unsafe { (*fs).windowed } == 0;
    let windowed = if want_fs {
        told("a fullscreen swap chain");
        let mut w = unsafe { *fs };
        w.windowed = 1;
        w
    } else {
        FullscreenDesc { refresh_num: 0, refresh_den: 0, scanline: 0, scaling: 0, windowed: 1 }
    };
    let fs_arg = if want_fs { &raw const windowed } else { fs };
    let hr = unsafe { real.call(factory, device, hwnd, desc_arg, fs_arg, output, out) };
    if hr >= 0 && !out.is_null() {
        finish_create(unsafe { out.read() }, device, want_fs);
    }
    hr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_for_hwnd_keeps_fullscreen_wish_after_made_swap() {
        let swap = 0x5555_1230usize;
        // Creating a fullscreen swap chain must record it as wanted after clearing stale
        // state for that address (not wipe the wish immediately after setting it).
        finish_create(swap, 0, true);
        assert!(crate::sys::lock(&WANTED).contains(&swap), "fullscreen wish stays recorded");
        // A later windowed swap chain at the same address clears the old wish.
        finish_create(swap, 0, false);
        assert!(!crate::sys::lock(&WANTED).contains(&swap), "windowed swap chain clears stale wish");
        // Explicit cleanup on window destroy also removes the entry.
        finish_create(swap, 0, true);
        forget_swap(swap);
        assert!(!crate::sys::lock(&WANTED).contains(&swap));
    }

    #[test]
    fn allow_mode_switch_is_stripped_only_when_kept_windowed() {
        let flags = ALLOW_MODE_SWITCH | 0x40 | 0x800;
        assert_eq!(keep_flags(flags, true), 0x40 | 0x800);
        assert_eq!(keep_flags(flags, false), flags);
    }
}
