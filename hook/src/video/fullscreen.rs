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

use std::cell::Cell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use super::com::{self, Guid, IID_IDXGIFACTORY1, release, vcall};
use super::dxgi::SwapChainDesc;
use crate::log;

type SetFullscreen = unsafe extern "system" fn(usize, i32, usize) -> i32;
type GetFullscreen = unsafe extern "system" fn(usize, *mut i32, *mut usize) -> i32;
type ResizeTarget = unsafe extern "system" fn(usize, usize) -> i32;
type CreateSwapChain = unsafe extern "system" fn(usize, usize, *mut SwapChainDesc, *mut usize) -> i32;
type CreateForHwnd = unsafe extern "system" fn(usize, usize, usize, usize, *const FullscreenDesc, usize, *mut usize) -> i32;
type OutputGetDesc = unsafe extern "system" fn(usize, *mut OutputDesc) -> i32;
type GetDisplayModeList = unsafe extern "system" fn(usize, u32, u32, *mut u32, *mut ModeDesc) -> i32;
type GetDisplayModeList1 = unsafe extern "system" fn(usize, u32, u32, *mut u32, *mut ModeDesc1) -> i32;
type FindClosestMode = unsafe extern "system" fn(usize, *const ModeDesc, *mut ModeDesc, usize) -> i32;
type FindClosestMode1 = unsafe extern "system" fn(usize, *const ModeDesc1, *mut ModeDesc1, usize) -> i32;

static SET_FULLSCREEN: OnceLock<GenericDetour<SetFullscreen>> = OnceLock::new();
static GET_FULLSCREEN: OnceLock<GenericDetour<GetFullscreen>> = OnceLock::new();
static RESIZE_TARGET: OnceLock<GenericDetour<ResizeTarget>> = OnceLock::new();
static CREATE_SWAP_CHAIN: OnceLock<GenericDetour<CreateSwapChain>> = OnceLock::new();
static CREATE_FOR_HWND: OnceLock<GenericDetour<CreateForHwnd>> = OnceLock::new();
static OUTPUT_GET_DESC: OnceLock<GenericDetour<OutputGetDesc>> = OnceLock::new();
static OUTPUT_MODES: OnceLock<GenericDetour<GetDisplayModeList>> = OnceLock::new();
static OUTPUT_MODES1: OnceLock<GenericDetour<GetDisplayModeList1>> = OnceLock::new();
static OUTPUT_CLOSEST: OnceLock<GenericDetour<FindClosestMode>> = OnceLock::new();
static OUTPUT_CLOSEST1: OnceLock<GenericDetour<FindClosestMode1>> = OnceLock::new();

/// Swap chains the game asked to be fullscreen, kept windowed.
static WANTED: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(Mutex::default);
/// Packed `(w | (h << 32))` backbuffer size of an exclusive-fullscreen swap chain when it
/// differs from the pane's virtual size (0 when inactive or matching the pane).
static EXCLUSIVE_SIZE: AtomicU64 = AtomicU64::new(0);
static TOLD: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Prevents nested re-entry when DXGI's `IDXGIOutput1::*1` methods delegate into
    /// `IDXGIOutput::*` (or vice versa) inside `dxgi.dll`.
    static IN_OUTPUT_HOOK: Cell<bool> = const { Cell::new(false) };
}

const S_OK: i32 = 0;
const DXGI_ERROR_MORE_DATA: i32 = 0x887A_0003u32 as i32;
const IID_IDXGIFACTORY: Guid = Guid(0x7b7166ec, 0x21c7, 0x44ae, [0xb2, 0x1a, 0xc9, 0xae, 0x32, 0x1a, 0xe3, 0x69]);
const IID_IDXGIFACTORY2: Guid = Guid(0x50c83a1c, 0xe072, 0x4c48, [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0]);
const IID_IDXGIOUTPUT1: Guid = Guid(0x00cddea8, 0x939b, 0x4b83, [0xa3, 0x40, 0xa6, 0x85, 0x22, 0x66, 0x66, 0xcc]);

/// `DXGI_OUTPUT_DESC`.
#[repr(C)]
struct OutputDesc {
    device_name: [u16; 32],
    desktop_left: i32,
    desktop_top: i32,
    desktop_right: i32,
    desktop_bottom: i32,
    attached: i32,
    rotation: u32,
    monitor: usize,
}

/// `DXGI_MODE_DESC`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeDesc {
    pub width: u32,
    pub height: u32,
    pub refresh_num: u32,
    pub refresh_den: u32,
    pub format: u32,
    pub scanline: u32,
    pub scaling: u32,
}

/// `DXGI_MODE_DESC1`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeDesc1 {
    pub width: u32,
    pub height: u32,
    pub refresh_num: u32,
    pub refresh_den: u32,
    pub format: u32,
    pub scanline: u32,
    pub scaling: u32,
    pub stereo: i32,
}

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

/// Whether a swap chain of the game's is one it asked to be fullscreen.
pub fn exclusive_wanted() -> bool {
    !crate::sys::lock(&WANTED).is_empty()
}

/// Non-pane backbuffer size of an exclusive-fullscreen swap chain, if active.
pub(crate) fn exclusive_size() -> Option<(u32, u32)> {
    let v = EXCLUSIVE_SIZE.load(Ordering::Relaxed);
    (v != 0).then_some((v as u32, (v >> 32) as u32))
}

fn set_exclusive_size(size: Option<(u32, u32)>) {
    let packed = match size {
        Some((w, h)) if w >= 320 && h >= 200 => u64::from(w) | (u64::from(h) << 32),
        _ => 0,
    };
    let prev = EXCLUSIVE_SIZE.swap(packed, Ordering::Relaxed);
    if prev != packed {
        match size {
            Some((w, h)) if packed != 0 => {
                if let Some((vw, vh)) = crate::window::virtual_size() {
                    log!("video: exclusive-fullscreen backbuffer is {w}x{h} (pane {vw}x{vh}): matching window client size to {w}x{h} so cursor coordinates stay 1:1");
                }
            }
            _ => {
                if let Some((vw, vh)) = crate::window::virtual_size() {
                    log!("video: swap chain matches pane {vw}x{vh} (or left exclusive fullscreen): restoring window client size to pane");
                }
            }
        }
        crate::window::sync_window_size();
    }
}

/// Records `swap`'s backbuffer dimensions so an exclusive-fullscreen game running at a
/// lower/different resolution than its pane keeps its Win32 client area 1:1 with its
/// backbuffer (and restores full pane size when it switches to the pane's resolution).
pub(super) fn note_backbuffer_size(swap: usize, w: u32, h: u32) {
    if !keep_windowed() || w < 320 || h < 200 {
        return;
    }
    let is_exclusive = crate::sys::lock(&WANTED).contains(&swap);
    if !is_exclusive {
        if !exclusive_wanted() && EXCLUSIVE_SIZE.load(Ordering::Relaxed) != 0 {
            set_exclusive_size(None);
        }
        return;
    }
    let pane = crate::window::virtual_size();
    if pane.is_some_and(|(vw, vh)| w == vw && h == vh) {
        set_exclusive_size(None);
    } else if pane.is_some() {
        set_exclusive_size(Some((w, h)));
    }
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

/// Hooks `IDXGIOutput` and `IDXGIOutput1` display-mode/bounds calls so DX11/DX12 games
/// enumerate the split-screen pane's resolution instead of falling back to low monitor modes.
fn install_dxgi_output() -> usize {
    let dxgi = com::module("dxgi.dll");
    if dxgi.is_null() {
        return 0;
    }
    let Some(create_ptr) = com::proc(dxgi, c"CreateDXGIFactory1") else { return 0 };
    type CreateFactory1 = unsafe extern "system" fn(*const Guid, *mut usize) -> i32;
    let create: CreateFactory1 = unsafe { std::mem::transmute(create_ptr) };
    let mut factory = 0usize;
    if unsafe { create(&IID_IDXGIFACTORY1, &mut factory) } < 0 || factory == 0 {
        return 0;
    }
    let mut output = 0usize;
    for i in 0..8u32 {
        let mut adapter = 0usize;
        // IDXGIFactory1::EnumAdapters1 is slot 12.
        if vcall!(factory, 12, fn(u32, *mut usize) -> i32, i, &mut adapter) < 0 || adapter == 0 {
            break;
        }
        // IDXGIAdapter::EnumOutputs is slot 7.
        let hr = vcall!(adapter, 7, fn(u32, *mut usize) -> i32, 0, &mut output);
        release(adapter);
        if hr >= 0 && output != 0 {
            break;
        }
    }
    release(factory);
    if output == 0 {
        return 0;
    }
    let mut n = 0;
    n += arm(&OUTPUT_GET_DESC, com::slot(output, 7), output_get_desc as OutputGetDesc, "IDXGIOutput::GetDesc");
    n += arm(&OUTPUT_MODES, com::slot(output, 8), output_get_modes as GetDisplayModeList, "IDXGIOutput::GetDisplayModeList");
    n += arm(&OUTPUT_CLOSEST, com::slot(output, 9), output_find_closest as FindClosestMode, "IDXGIOutput::FindClosestMatchingMode");
    let out1 = com::query(output, &IID_IDXGIOUTPUT1);
    if out1 != 0 {
        n += arm(&OUTPUT_MODES1, com::slot(out1, 19), output_get_modes1 as GetDisplayModeList1, "IDXGIOutput1::GetDisplayModeList1");
        n += arm(&OUTPUT_CLOSEST1, com::slot(out1, 20), output_find_closest1 as FindClosestMode1, "IDXGIOutput1::FindClosestMatchingMode1");
        release(out1);
    }
    release(output);
    n
}

/// Hooks the fullscreen calls of DXGI's swap chains, factories, and outputs, found through
/// the throwaway swap chain `swap` (dxgi.rs): every swap chain, factory and output shares them.
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
    n += install_dxgi_output();
    n
}

/// Pure helper: a monitor's DXGI display mode list as a copy in pane `(vw, vh)` sees it:
/// the pane's size at each of the monitor's refresh rates, and no other size. A fullscreen
/// game matches the resolution it saved against this list (or `FindClosestMatchingMode`):
/// with the monitor's smaller modes listed, a saved one that isn't the pane's (from another
/// layout, or a low mode it once fell back to) finds a near one among them and keeps it,
/// scaled into the pane at the wrong size and aspect. Leaves an empty list empty
/// (unsupported formats stay unsupported).
pub fn pane_display_modes(raw: &[ModeDesc], vw: u32, vh: u32) -> Vec<ModeDesc> {
    if raw.is_empty() || vw == 0 || vh == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut rates: Vec<((u32, u32), ModeDesc)> = Vec::new();
    for &m in raw {
        let key = (m.refresh_num, m.refresh_den);
        if let Some((_, best)) = rates.iter_mut().find(|(k, _)| *k == key) {
            // Prefer progressive scanline and larger source mode as template.
            let m_score = (u64::from(m.scanline == 1) << 63) | (u64::from(m.width) * u64::from(m.height));
            let b_score = (u64::from(best.scanline == 1) << 63) | (u64::from(best.width) * u64::from(best.height));
            if m_score > b_score {
                *best = m;
            }
        } else if rates.len() < 8 {
            rates.push((key, m));
        }
    }
    rates.sort_by(|((n1, d1), _), ((n2, d2), _)| {
        let r1 = u64::from(*n1) * u64::from((*d2).max(1));
        let r2 = u64::from(*n2) * u64::from((*d1).max(1));
        r1.cmp(&r2)
    });
    for (_, tmpl) in rates {
        out.push(ModeDesc { width: vw, height: vh, ..tmpl });
    }
    out
}

/// Same as `pane_display_modes` for `DXGI_MODE_DESC1`.
pub fn pane_display_modes1(raw: &[ModeDesc1], vw: u32, vh: u32) -> Vec<ModeDesc1> {
    if raw.is_empty() || vw == 0 || vh == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut rates: Vec<((u32, u32, i32), ModeDesc1)> = Vec::new();
    for &m in raw {
        let key = (m.refresh_num, m.refresh_den, m.stereo);
        if let Some((_, best)) = rates.iter_mut().find(|(k, _)| *k == key) {
            let m_score = (u64::from(m.scanline == 1) << 63) | (u64::from(m.width) * u64::from(m.height));
            let b_score = (u64::from(best.scanline == 1) << 63) | (u64::from(best.width) * u64::from(best.height));
            if m_score > b_score {
                *best = m;
            }
        } else if rates.len() < 8 {
            rates.push((key, m));
        }
    }
    rates.sort_by(|((n1, d1, s1), _), ((n2, d2, s2), _)| {
        let r1 = u64::from(*n1) * u64::from((*d2).max(1));
        let r2 = u64::from(*n2) * u64::from((*d1).max(1));
        s1.cmp(s2).then_with(|| r1.cmp(&r2))
    });
    for (_, tmpl) in rates {
        out.push(ModeDesc1 { width: vw, height: vh, ..tmpl });
    }
    out
}

/// `FindClosestMatchingMode` in a pane: the pane's size, whatever was asked for (the
/// pane's is the only size its mode list has), keeping the refresh rate and format DXGI
/// found.
fn closest_in_pane(want: (u32, u32), closest: &mut (u32, u32), vw: u32, vh: u32) {
    if *closest != (vw, vh) {
        crate::log::throttled("video: dxgi closest mode", 4, |n| {
            log!("video: the game asks DXGI for the display mode closest to {}x{}: told the pane's {vw}x{vh}, not {}x{} (call {n})", want.0, want.1, closest.0, closest.1);
        });
    }
    *closest = (vw, vh);
}

struct OutputGuard;
impl Drop for OutputGuard {
    fn drop(&mut self) {
        IN_OUTPUT_HOOK.with(|f| f.set(false));
    }
}

fn enter_output_hook() -> Option<OutputGuard> {
    if IN_OUTPUT_HOOK.with(|f| f.replace(true)) {
        None
    } else {
        Some(OutputGuard)
    }
}

unsafe extern "system" fn output_get_desc(output: usize, desc: *mut OutputDesc) -> i32 {
    let real = OUTPUT_GET_DESC.get().unwrap();
    let hr = unsafe { real.call(output, desc) };
    if hr >= 0
        && !desc.is_null()
        && let Some((vw, vh)) = crate::window::screen_for_caller()
    {
        unsafe {
            (*desc).desktop_left = 0;
            (*desc).desktop_top = 0;
            (*desc).desktop_right = vw as i32;
            (*desc).desktop_bottom = vh as i32;
        }
    }
    hr
}

unsafe extern "system" fn output_get_modes(output: usize, format: u32, flags: u32, num_modes: *mut u32, desc: *mut ModeDesc) -> i32 {
    let real = OUTPUT_MODES.get().unwrap();
    let Some(_guard) = enter_output_hook() else {
        return unsafe { real.call(output, format, flags, num_modes, desc) };
    };
    let Some((vw, vh)) = crate::window::screen_for_caller() else {
        return unsafe { real.call(output, format, flags, num_modes, desc) };
    };
    if num_modes.is_null() {
        return unsafe { real.call(output, format, flags, num_modes, desc) };
    }
    let mut real_count = 0u32;
    let hr = unsafe { real.call(output, format, flags, &mut real_count, std::ptr::null_mut()) };
    if hr < 0 || real_count == 0 {
        unsafe { *num_modes = 0 };
        return hr;
    }
    let mut raw = vec![ModeDesc::default(); real_count as usize];
    let mut got = real_count;
    let hr2 = unsafe { real.call(output, format, flags, &mut got, raw.as_mut_ptr()) };
    if hr2 < 0 {
        return unsafe { real.call(output, format, flags, num_modes, desc) };
    }
    raw.truncate(got as usize);
    let modes = pane_display_modes(&raw, vw, vh);
    crate::log::throttled("video: dxgi output modes", 2, |_| {
        log!("video: DXGI output modes for pane {vw}x{vh}: {got} monitor mode(s) -> {} pane mode(s) (format {format})", modes.len());
    });
    if desc.is_null() {
        unsafe { *num_modes = modes.len() as u32 };
        return S_OK;
    }
    let cap = unsafe { *num_modes } as usize;
    if cap < modes.len() {
        unsafe { *num_modes = modes.len() as u32 };
        return DXGI_ERROR_MORE_DATA;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(modes.as_ptr(), desc, modes.len());
        *num_modes = modes.len() as u32;
    }
    S_OK
}

unsafe extern "system" fn output_get_modes1(output: usize, format: u32, flags: u32, num_modes: *mut u32, desc: *mut ModeDesc1) -> i32 {
    let real = OUTPUT_MODES1.get().unwrap();
    let Some(_guard) = enter_output_hook() else {
        return unsafe { real.call(output, format, flags, num_modes, desc) };
    };
    let Some((vw, vh)) = crate::window::screen_for_caller() else {
        return unsafe { real.call(output, format, flags, num_modes, desc) };
    };
    if num_modes.is_null() {
        return unsafe { real.call(output, format, flags, num_modes, desc) };
    }
    let mut real_count = 0u32;
    let hr = unsafe { real.call(output, format, flags, &mut real_count, std::ptr::null_mut()) };
    if hr < 0 || real_count == 0 {
        unsafe { *num_modes = 0 };
        return hr;
    }
    let mut raw = vec![ModeDesc1::default(); real_count as usize];
    let mut got = real_count;
    let hr2 = unsafe { real.call(output, format, flags, &mut got, raw.as_mut_ptr()) };
    if hr2 < 0 {
        return unsafe { real.call(output, format, flags, num_modes, desc) };
    }
    raw.truncate(got as usize);
    let modes = pane_display_modes1(&raw, vw, vh);
    crate::log::throttled("video: dxgi output modes1", 2, |_| {
        log!("video: DXGI output1 modes for pane {vw}x{vh}: {got} monitor mode(s) -> {} pane mode(s) (format {format})", modes.len());
    });
    if desc.is_null() {
        unsafe { *num_modes = modes.len() as u32 };
        return S_OK;
    }
    let cap = unsafe { *num_modes } as usize;
    if cap < modes.len() {
        unsafe { *num_modes = modes.len() as u32 };
        return DXGI_ERROR_MORE_DATA;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(modes.as_ptr(), desc, modes.len());
        *num_modes = modes.len() as u32;
    }
    S_OK
}

unsafe extern "system" fn output_find_closest(output: usize, want: *const ModeDesc, closest: *mut ModeDesc, dev: usize) -> i32 {
    let real = OUTPUT_CLOSEST.get().unwrap();
    let Some(_guard) = enter_output_hook() else {
        return unsafe { real.call(output, want, closest, dev) };
    };
    let hr = unsafe { real.call(output, want, closest, dev) };
    if hr >= 0
        && !want.is_null()
        && !closest.is_null()
        && let Some((vw, vh)) = crate::window::screen_for_caller()
    {
        let w = unsafe { ((*want).width, (*want).height) };
        let mut c = unsafe { ((*closest).width, (*closest).height) };
        closest_in_pane(w, &mut c, vw, vh);
        unsafe {
            (*closest).width = c.0;
            (*closest).height = c.1;
        }
    }
    hr
}

unsafe extern "system" fn output_find_closest1(output: usize, want: *const ModeDesc1, closest: *mut ModeDesc1, dev: usize) -> i32 {
    let real = OUTPUT_CLOSEST1.get().unwrap();
    let Some(_guard) = enter_output_hook() else {
        return unsafe { real.call(output, want, closest, dev) };
    };
    let hr = unsafe { real.call(output, want, closest, dev) };
    if hr >= 0
        && !want.is_null()
        && !closest.is_null()
        && let Some((vw, vh)) = crate::window::screen_for_caller()
    {
        let w = unsafe { ((*want).width, (*want).height) };
        let mut c = unsafe { ((*closest).width, (*closest).height) };
        closest_in_pane(w, &mut c, vw, vh);
        unsafe {
            (*closest).width = c.0;
            (*closest).height = c.1;
        }
    }
    hr
}

fn wanted(swap: usize, on: bool) {
    let empty = {
        let mut w = crate::sys::lock(&WANTED);
        if on {
            w.insert(swap);
        } else {
            w.remove(&swap);
        }
        w.is_empty()
    };
    if empty && EXCLUSIVE_SIZE.load(Ordering::Relaxed) != 0 {
        set_exclusive_size(None);
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
    // Shows whether the game re-applies its video mode (after its pane was resized, say).
    crate::log::throttled("video: set fullscreen", 6, |n| {
        log!("video: the game sets its swap chain {} (SetFullscreenState, call {n}): it stays windowed in its pane", if fullscreen != 0 { "fullscreen" } else { "windowed" });
    });
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
        if mode != 0 {
            let m = unsafe { *(mode as *const ModeDesc) };
            crate::log::throttled("video: resize target", 4, |n| {
                log!("video: the game asks for its window or display mode at {}x{} (ResizeTarget, call {n}): left as it is, the pane sizes the window", m.width, m.height);
            });
        }
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
        let swap = unsafe { out.read() };
        finish_create(swap, device, want_fs);
        if want_fs {
            let (w, h) = unsafe { ((*desc).width, (*desc).height) };
            note_backbuffer_size(swap, w, h);
        }
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
        let swap = unsafe { out.read() };
        finish_create(swap, device, want_fs);
        if want_fs
            && let Some(d) = desc1
        {
            note_backbuffer_size(swap, d[0], d[1]);
        }
    }
    hr
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(w: u32, h: u32, num: u32, den: u32) -> ModeDesc {
        ModeDesc { width: w, height: h, refresh_num: num, refresh_den: den, format: 28, scanline: 1, scaling: 0 }
    }

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

    #[test]
    fn dxgi_struct_layouts_match_windows_abi() {
        assert_eq!(std::mem::size_of::<ModeDesc>(), 28);
        assert_eq!(std::mem::size_of::<ModeDesc1>(), 32);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<OutputDesc>(), 96);
        #[cfg(target_pointer_width = "32")]
        assert_eq!(std::mem::size_of::<OutputDesc>(), 92);
        assert_eq!(std::mem::offset_of!(OutputDesc, desktop_left), 64);
        assert_eq!(std::mem::offset_of!(OutputDesc, monitor), 88);
    }

    #[test]
    fn pane_display_modes_list_only_the_pane_size() {
        let raw = vec![
            mode(720, 576, 60000, 1001),
            mode(1280, 720, 60000, 1001),
            mode(1280, 720, 100000, 1000),
            mode(1920, 1080, 100000, 1000),
            mode(3440, 1440, 100000, 1000),
        ];
        // Only the pane's size, once per refresh rate, in ascending order: a saved low mode
        // (720x576) or one from another layout has nothing smaller to land on.
        let got = pane_display_modes(&raw, 1720, 1440);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|m| (m.width, m.height) == (1720, 1440)));
        assert_eq!((got[0].refresh_num, got[1].refresh_num), (60000, 100000));
        // A pane the size of a real mode lists it once per rate, not twice.
        let exact = pane_display_modes(&raw, 1280, 720);
        assert_eq!(exact.len(), 2);
        assert!(exact.iter().all(|m| (m.width, m.height) == (1280, 720)));
        // Empty (unsupported format) stays empty.
        assert!(pane_display_modes(&[], 1720, 1440).is_empty());
        let raw1: Vec<ModeDesc1> = raw.iter().map(|m| ModeDesc1 { width: m.width, height: m.height, refresh_num: m.refresh_num, refresh_den: m.refresh_den, format: m.format, scanline: m.scanline, scaling: m.scaling, stereo: 0 }).collect();
        let got1 = pane_display_modes1(&raw1, 1793, 1440);
        assert_eq!(got1.len(), 2);
        assert!(got1.iter().all(|m| (m.width, m.height) == (1793, 1440)));
    }

    #[test]
    fn closest_mode_in_a_pane_is_the_pane() {
        // A saved low mode, one larger than the pane and "any size" all come back as the pane.
        for (want, real) in [((720, 540), (720, 576)), ((2560, 1080), (2560, 1080)), ((0, 0), (3440, 1440)), ((1720, 1440), (1600, 1200))] {
            let mut c = real;
            closest_in_pane(want, &mut c, 1720, 1440);
            assert_eq!(c, (1720, 1440), "asked {want:?}");
        }
    }

    #[test]
    fn exclusive_window_size_keeps_valid_backbuffer() {
        let pane = peerparty_protocol::session::PaneRect { x: 0, y: 0, width: 1720, height: 1440 };
        assert_eq!(crate::window::target_window_size(pane, None), (1720, 1440));
        assert_eq!(crate::window::target_window_size(pane, Some((720, 576))), (720, 576));
        assert_eq!(crate::window::target_window_size(pane, Some((64, 64))), (1720, 1440), "tiny throwaway ignored");
    }

    #[test]
    fn dxgi_output_hooks_install_on_real_adapter() {
        let _ = com::load("dxgi.dll");
        let n = install_dxgi_output();
        // On a headless/software-only machine with no outputs this may be 0; whenever a
        // real output exists we must have hooked GetDesc + GetDisplayModeList + FindClosest.
        if OUTPUT_GET_DESC.get().is_some() {
            assert!(OUTPUT_MODES.get().is_some());
            assert!(OUTPUT_CLOSEST.get().is_some());
            let _ = n;
        }
    }
}
