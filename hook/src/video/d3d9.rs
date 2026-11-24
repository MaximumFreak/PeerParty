//! Direct3D 9 games: `IDirect3DDevice9::Present` (slot 17) and, for 9Ex devices,
//! `PresentEx` (121), found through a throwaway device on a hidden window. The frame is
//! a dynamic A8R8G8B8 texture drawn as a pre-transformed quad with premultiplied alpha
//! blending, inside a state block that puts every render, texture-stage and sampler
//! state back (and render target 0, which state blocks don't keep). `Reset` / `ResetEx`
//! (16 / 132) drop the texture first, as Direct3D 9 requires of default-pool resources.
//!
//! In a split-screen pane, a device created or reset fullscreen is created or reset
//! windowed (`IDirect3D9::CreateDevice` 16, `IDirect3D9Ex::CreateDeviceEx` 20): see
//! fullscreen.rs. Its display modes there read as the pane's size (d3d9modes.rs), so a
//! fullscreen device made windowed is the pane's size too.
//!
//! When the session's window draws the copy's frames, each present's back buffer is
//! copied for it too, overlay and all (d3d9capture.rs). Under DXVK (Proton's Direct3D 9,
//! or `PEERPARTY_DXVK` on Windows) the Vulkan hooks see the same frames last and handle
//! both (`super::lower_layer_presents`).
//!
//! Slots are from d3d9.h, IDirect3DDevice9 in declaration order.

use std::sync::{Mutex, OnceLock};

#[path = "d3d9capture.rs"]
mod d3d9capture;
#[path = "d3d9modes.rs"]
mod d3d9modes;
pub use d3d9capture::redirect;

use retour::GenericDetour;

use super::com::{self, HiddenWindow, release, vcall};
use super::frame;
use crate::sys::hook::with_lock;
use crate::log;

type Present = unsafe extern "system" fn(usize, usize, usize, usize, usize) -> i32;
type PresentEx = unsafe extern "system" fn(usize, usize, usize, usize, usize, u32) -> i32;
type Reset = unsafe extern "system" fn(usize, usize) -> i32;
type ResetEx = unsafe extern "system" fn(usize, usize, usize) -> i32;
type CreateDevice = unsafe extern "system" fn(usize, u32, u32, usize, u32, *mut PresentParameters, *mut usize) -> i32;
type CreateDeviceEx = unsafe extern "system" fn(usize, u32, u32, usize, u32, *mut PresentParameters, usize, *mut usize) -> i32;

type SwapPresent = unsafe extern "system" fn(usize, usize, usize, usize, usize, u32) -> i32;

static PRESENT: OnceLock<GenericDetour<Present>> = OnceLock::new();
/// A 9Ex device's own Present and Reset, when they aren't the plain device's.
static PRESENT_9EX: OnceLock<GenericDetour<Present>> = OnceLock::new();
static RESET_9EX: OnceLock<GenericDetour<Reset>> = OnceLock::new();
static PRESENT_EX: OnceLock<GenericDetour<PresentEx>> = OnceLock::new();
static RESET: OnceLock<GenericDetour<Reset>> = OnceLock::new();
static RESET_EX: OnceLock<GenericDetour<ResetEx>> = OnceLock::new();
/// `IDirect3DSwapChain9::Present` (slot 3): some games present through the device's
/// swap chain rather than the device.
static SWAP_PRESENT: OnceLock<GenericDetour<SwapPresent>> = OnceLock::new();
static CREATE: OnceLock<GenericDetour<CreateDevice>> = OnceLock::new();
/// IDirect3D9Ex's own CreateDevice, when it isn't IDirect3D9's.
static CREATE_9EX: OnceLock<GenericDetour<CreateDevice>> = OnceLock::new();
static CREATE_EX: OnceLock<GenericDetour<CreateDeviceEx>> = OnceLock::new();

struct Tex {
    device: usize,
    tex: usize,
    size: (u32, u32),
    seq: Option<u32>,
}

unsafe impl Send for Tex {}

static TEX: Mutex<Option<Tex>> = Mutex::new(None);

#[repr(C)]
struct PresentParameters {
    width: u32,
    height: u32,
    format: u32,
    count: u32,
    multisample: u32,
    quality: u32,
    swap_effect: u32,
    window: usize,
    windowed: i32,
    auto_depth: i32,
    depth_format: u32,
    flags: u32,
    refresh: u32,
    interval: u32,
}

#[repr(C)]
struct LockedRect {
    pitch: i32,
    bits: *mut u8,
}

pub fn install() -> bool {
    let m = com::module("d3d9.dll");
    if m.is_null() {
        return false;
    }
    let Some(win) = HiddenWindow::new() else { return false };
    let pp = PresentParameters {
        width: 64,
        height: 64,
        format: 0,
        count: 1,
        multisample: 0,
        quality: 0,
        swap_effect: 1, // DISCARD
        window: win.0 as usize,
        windowed: 1,
        auto_depth: 0,
        depth_format: 0,
        flags: 0,
        refresh: 0,
        interval: 0x8000_0000, // IMMEDIATE
    };
    let mut addrs: Vec<(usize, &str)> = Vec::new();
    let mut modes = 0;
    if let Some(f) = com::proc(m, c"Direct3DCreate9") {
        let create: unsafe extern "system" fn(u32) -> usize = unsafe { std::mem::transmute(f) };
        let d3d = unsafe { create(32) };
        if d3d != 0 {
            let mut dev = 0usize;
            for kind in [1u32, 4] {
                // HAL, else NULLREF.
                if vcall!(d3d, 16, fn(u32, u32, usize, u32, *const PresentParameters, *mut usize) -> i32, 0, kind, win.0 as usize, 0x20, &pp, &mut dev) >= 0 {
                    break;
                }
            }
            if dev != 0 {
                addrs.push((com::slot(dev, 17), "Present"));
                addrs.push((com::slot(dev, 16), "Reset"));
                // IDirect3DDevice9::GetSwapChain (14): its Present (slot 3).
                let mut sc = 0usize;
                if vcall!(dev, 14, fn(u32, *mut usize) -> i32, 0, &mut sc) >= 0 && sc != 0 {
                    addrs.push((com::slot(sc, 3), "SwapChain Present"));
                    release(sc);
                }
                release(dev);
            }
            addrs.push((com::slot(d3d, 16), "CreateDevice"));
            modes += d3d9modes::install(d3d, false);
            release(d3d);
        }
    }
    if let Some(f) = com::proc(m, c"Direct3DCreate9Ex") {
        let create: unsafe extern "system" fn(u32, *mut usize) -> i32 = unsafe { std::mem::transmute(f) };
        let mut d3d = 0usize;
        if unsafe { create(32, &mut d3d) } >= 0 && d3d != 0 {
            let mut dev = 0usize;
            // IDirect3D9Ex::CreateDeviceEx is slot 20.
            if vcall!(d3d, 20, fn(u32, u32, usize, u32, *const PresentParameters, usize, *mut usize) -> i32, 0, 1, win.0 as usize, 0x20, &pp, 0, &mut dev) >= 0 && dev != 0 {
                // Usually the plain device's functions; when not, hooked as their own.
                for (slot, plain, own) in [(17, "Present", "Present (9Ex)"), (16, "Reset", "Reset (9Ex)")] {
                    let a = com::slot(dev, slot);
                    let seen = addrs.iter().any(|(x, l)| *x == a && *l == plain);
                    addrs.push((a, if seen || !addrs.iter().any(|(_, l)| *l == plain) { plain } else { own }));
                }
                addrs.push((com::slot(dev, 121), "PresentEx"));
                addrs.push((com::slot(dev, 132), "ResetEx"));
                release(dev);
            }
            let create = com::slot(d3d, 16);
            if !addrs.iter().any(|(a, l)| *a == create && *l == "CreateDevice") {
                addrs.push((create, "CreateDevice (9Ex)"));
            }
            addrs.push((com::slot(d3d, 20), "CreateDeviceEx"));
            modes += d3d9modes::install(d3d, true);
            release(d3d);
        }
    }
    drop(win);
    let mut n = 0;
    for (addr, label) in addrs {
        n += match label {
            "Present" => arm(&PRESENT, addr, present_hook as Present, label),
            "Present (9Ex)" => arm(&PRESENT_9EX, addr, present_9ex_hook as Present, label),
            "SwapChain Present" => arm(&SWAP_PRESENT, addr, swap_present_hook as SwapPresent, label),
            "PresentEx" => arm(&PRESENT_EX, addr, present_ex_hook as PresentEx, label),
            "Reset" => arm(&RESET, addr, reset_hook as Reset, label),
            "Reset (9Ex)" => arm(&RESET_9EX, addr, reset_9ex_hook as Reset, label),
            "ResetEx" => arm(&RESET_EX, addr, reset_ex_hook as ResetEx, label),
            "CreateDevice" => arm(&CREATE, addr, create_hook as CreateDevice, label),
            "CreateDevice (9Ex)" => arm(&CREATE_9EX, addr, create_9ex_hook as CreateDevice, label),
            _ => arm(&CREATE_EX, addr, create_ex_hook as CreateDeviceEx, label),
        };
    }
    if modes > 0 {
        log!("video: Direct3D 9's display-mode calls hooked ({modes}): in a split-screen pane they answer the pane's size");
    }
    n > 0
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, addr: usize, detour: T, label: &str) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    if slot.get().is_some() {
        return 0;
    }
    with_lock(|| {
        let target: T = unsafe { T::from_ptr(addr as *const ()) };
        match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => {
                let _ = slot.set(h);
                usize::from(crate::sys::hook::frozen(addr, || unsafe { slot.get().unwrap().enable() }).is_ok())
            }
            Err(e) => {
                log!("present: IDirect3DDevice9::{label} not hooked: {e}");
                0
            }
        }
    })
}

fn drop_texture() {
    if let Some(t) = crate::sys::lock(&TEX).take() {
        release(t.tex);
        release(t.device);
    }
}

fn texture(device: usize, w: u32, h: u32) -> Option<usize> {
    let mut g = crate::sys::lock(&TEX);
    if g.as_ref().is_some_and(|t| t.device != device || t.size != (w, h)) {
        if let Some(t) = g.take() {
            release(t.tex);
            release(t.device);
        }
    }
    if g.is_none() {
        let mut tex = 0usize;
        // CreateTexture: 1 level, DYNAMIC, A8R8G8B8, DEFAULT pool.
        if vcall!(device, 23, fn(u32, u32, u32, u32, u32, u32, *mut usize, usize) -> i32, w, h, 1, 0x200, 21, 0, &mut tex, 0) < 0 {
            return None;
        }
        vcall!(device, 1, fn() -> u32);
        *g = Some(Tex { device, tex, size: (w, h), seq: None });
    }
    let t = g.as_mut()?;
    let f = frame()?;
    // Read again, the frame may have changed size since `draw` sized the texture:
    // copied into this one, it would write past the locked surface.
    // The frame read whole before the lock: a DISCARD lock left unwritten would be drawn.
    if t.seq != Some(f.seq) && !f.writing() && (f.w, f.h) == t.size
        && let Some(px) = f.stable()
    {
        let mut lr = LockedRect { pitch: 0, bits: std::ptr::null_mut() };
        // IDirect3DTexture9::LockRect / UnlockRect are slots 19 / 20; D3DLOCK_DISCARD.
        if vcall!(t.tex, 19, fn(u32, *mut LockedRect, usize, u32) -> i32, 0, &mut lr, 0, 0x2000) >= 0 {
            unsafe { f.write_rows(&px, lr.bits, lr.pitch as usize) };
            vcall!(t.tex, 20, fn(u32) -> i32, 0);
            t.seq = Some(f.seq);
        }
    }
    t.seq.map(|_| t.tex)
}

fn device_hwnd(device: usize, wnd: usize) -> usize {
    if wnd != 0 {
        return wnd;
    }
    // IDirect3DDevice9::GetCreationParameters (slot 9): Adapter, DeviceType, hFocusWindow, BehaviorFlags.
    #[repr(C)]
    struct CreationParams {
        adapter: u32,
        dev_type: u32,
        focus_window: usize,
        behavior: u32,
    }
    let mut cp = CreationParams { adapter: 0, dev_type: 0, focus_window: 0, behavior: 0 };
    if vcall!(device, 9, fn(*mut CreationParams) -> i32, &mut cp) >= 0 {
        cp.focus_window
    } else {
        0
    }
}

/// The first back buffer's size, as the game's viewport (`super::report_viewport`).
fn report_size(device: usize, wnd: usize) {
    let mut back = 0usize;
    // IDirect3DDevice9::GetBackBuffer (slot 18).
    if vcall!(device, 18, fn(u32, u32, u32, *mut usize) -> i32, 0, 0, 0, &mut back) < 0 || back == 0 {
        return;
    }
    // IDirect3DSurface9::GetDesc (slot 12): width and height are its 7th and 8th fields.
    let mut desc = [0u32; 8];
    vcall!(back, 12, fn(*mut u32) -> i32, desc.as_mut_ptr());
    release(back);
    super::report_viewport(desc[6], desc[7], device_hwnd(device, wnd), "d3d9");
}

fn draw(device: usize, wnd: usize) {
    let Some(f) = frame() else { return };
    let Some(tex) = texture(device, f.w, f.h) else { return };
    let mut sb = 0usize;
    // CreateStateBlock(D3DSBT_ALL) captures everything below.
    if vcall!(device, 59, fn(u32, *mut usize) -> i32, 1, &mut sb) < 0 {
        return;
    }
    let mut old_rt = 0usize;
    vcall!(device, 38, fn(u32, *mut usize) -> i32, 0, &mut old_rt);
    let mut back = 0usize;
    if vcall!(device, 18, fn(u32, u32, u32, *mut usize) -> i32, 0, 0, 0, &mut back) >= 0 {
        // IDirect3DSurface9::GetDesc (slot 12): width and height are its 7th and 8th fields.
        let mut desc = [0u32; 8];
        vcall!(back, 12, fn(*mut u32) -> i32, desc.as_mut_ptr());
        super::report_viewport(desc[6], desc[7], device_hwnd(device, wnd), "d3d9");
        let (w, h) = (desc[6] as f32, desc[7] as f32);
        vcall!(device, 37, fn(u32, usize) -> i32, 0, back);
        // The whole back buffer (SetRenderTarget resets the viewport, but a game's own may
        // be restored by drivers that don't): IDirect3DDevice9::SetViewport is slot 47.
        #[repr(C)]
        struct Viewport9 {
            x: u32,
            y: u32,
            w: u32,
            h: u32,
            min_z: f32,
            max_z: f32,
        }
        let vp = Viewport9 { x: 0, y: 0, w: desc[6], h: desc[7], min_z: 0.0, max_z: 1.0 };
        vcall!(device, 47, fn(*const Viewport9) -> i32, &vp);
        if vcall!(device, 41, fn() -> i32) >= 0 {
            for (state, value) in [(7u32, 0u32), (27, 1), (19, 2), (20, 6), (171, 1), (22, 1), (137, 0), (15, 0), (28, 0), (52, 0), (174, 0), (168, 0xf), (194, 0), (206, 0), (8, 3)] {
                vcall!(device, 57, fn(u32, u32) -> i32, state, value);
            }
            vcall!(device, 107, fn(usize) -> i32, 0);
            vcall!(device, 92, fn(usize) -> i32, 0);
            vcall!(device, 89, fn(u32) -> i32, 0x004 | 0x100); // XYZRHW | TEX1
            vcall!(device, 65, fn(u32, usize) -> i32, 0, tex);
            for (stage, kind, value) in [(0u32, 1u32, 2u32), (0, 2, 2), (0, 4, 2), (0, 5, 2), (1, 1, 1), (1, 4, 1)] {
                vcall!(device, 67, fn(u32, u32, u32) -> i32, stage, kind, value);
            }
            for (kind, value) in [(5u32, 1u32), (6, 1), (1, 3), (2, 3), (11, 0)] {
                vcall!(device, 69, fn(u32, u32, u32) -> i32, 0, kind, value);
            }
            let q = |x: f32, y: f32, u: f32, v: f32| [x - 0.5, y - 0.5, 0.0, 1.0, u, v];
            let quad = [q(0.0, 0.0, 0.0, 0.0), q(w, 0.0, 1.0, 0.0), q(0.0, h, 0.0, 1.0), q(w, h, 1.0, 1.0)];
            vcall!(device, 83, fn(u32, u32, *const [f32; 6], u32) -> i32, 5, 2, quad.as_ptr(), 24);
            vcall!(device, 42, fn() -> i32);
            super::drew("Direct3D 9");
        }
        release(back);
    }
    // IDirect3DStateBlock9::Apply is slot 5.
    vcall!(sb, 5, fn() -> i32);
    release(sb);
    if old_rt != 0 {
        vcall!(device, 37, fn(u32, usize) -> i32, 0, old_rt);
        release(old_rt);
    }
}

thread_local! {
    /// Inside a present hook already (PresentEx may run Present's code): draw once.
    static IN_PRESENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn presenting(dev: usize, wnd: usize, real: impl FnOnce() -> i32) -> i32 {
    let outer = !IN_PRESENT.with(|p| p.replace(true));
    if outer {
        // The size it presents at, to the session (only sent when it changes), as the
        // other APIs do: the pane is fitted to it.
        let _ = std::panic::catch_unwind(|| report_size(dev, wnd));
    }
    // Drawn with Vulkan underneath (DXVK): drawn into and copied there.
    let lower = outer && super::lower_layer_presents(|| device_hwnd(dev, wnd));
    if lower {
        d3d9capture::lower_layer_took_over();
    }
    if outer && !lower {
        super::presented();
        let _ = std::panic::catch_unwind(|| draw(dev, wnd));
        // After the overlay: the session shows the frame as the window would.
        if super::capture::enabled() {
            let _ = std::panic::catch_unwind(|| d3d9capture::capture(dev));
        }
    }
    let hr = real();
    if outer {
        IN_PRESENT.with(|p| p.set(false));
        if !lower {
            super::capture::pace();
        }
    }
    // S_PRESENT_OCCLUDED (9Ex): a split-screen copy's window is behind the session's, and
    // is answered as shown so it keeps drawing.
    if hr == 0x0876_0868 && crate::window::in_pane() { 0 } else { hr }
}

unsafe extern "system" fn present_hook(dev: usize, src: usize, dst: usize, wnd: usize, dirty: usize) -> i32 {
    presenting(dev, wnd, || unsafe { PRESENT.get().unwrap().call(dev, src, dst, wnd, dirty) })
}

unsafe extern "system" fn present_9ex_hook(dev: usize, src: usize, dst: usize, wnd: usize, dirty: usize) -> i32 {
    presenting(dev, wnd, || unsafe { PRESENT_9EX.get().unwrap().call(dev, src, dst, wnd, dirty) })
}

unsafe extern "system" fn present_ex_hook(dev: usize, src: usize, dst: usize, wnd: usize, dirty: usize, flags: u32) -> i32 {
    presenting(dev, wnd, || unsafe { PRESENT_EX.get().unwrap().call(dev, src, dst, wnd, dirty, flags) })
}

/// A present through the device's swap chain: drawn and copied as the device's own when
/// it is the device's first (implicit) swap chain, whose back buffer those use.
unsafe extern "system" fn swap_present_hook(sc: usize, src: usize, dst: usize, wnd: usize, dirty: usize, flags: u32) -> i32 {
    let real = || unsafe { SWAP_PRESENT.get().unwrap().call(sc, src, dst, wnd, dirty, flags) };
    let mut dev = 0usize;
    // IDirect3DSwapChain9::GetDevice is slot 8.
    if IN_PRESENT.with(|p| p.get()) || vcall!(sc, 8, fn(*mut usize) -> i32, &mut dev) < 0 || dev == 0 {
        return real();
    }
    let mut first = 0usize;
    let implicit = vcall!(dev, 14, fn(u32, *mut usize) -> i32, 0, &mut first) >= 0 && first == sc;
    release(first);
    let hr = if implicit { presenting(dev, wnd, real) } else { real() };
    release(dev);
    hr
}

unsafe extern "system" fn reset_hook(dev: usize, pp: usize) -> i32 {
    drop_texture();
    d3d9capture::release_all();
    windowed(pp as *mut PresentParameters, |_| unsafe { RESET.get().unwrap().call(dev, pp) })
}

unsafe extern "system" fn reset_9ex_hook(dev: usize, pp: usize) -> i32 {
    drop_texture();
    d3d9capture::release_all();
    windowed(pp as *mut PresentParameters, |_| unsafe { RESET_9EX.get().unwrap().call(dev, pp) })
}

unsafe extern "system" fn reset_ex_hook(dev: usize, pp: usize, mode: usize) -> i32 {
    drop_texture();
    d3d9capture::release_all();

    windowed(pp as *mut PresentParameters, |w| unsafe { RESET_EX.get().unwrap().call(dev, pp, if w { 0 } else { mode }) })
}

unsafe extern "system" fn create_hook(d3d: usize, adapter: u32, kind: u32, hwnd: usize, flags: u32, pp: *mut PresentParameters, out: *mut usize) -> i32 {
    windowed(pp, |_| unsafe { CREATE.get().unwrap().call(d3d, adapter, kind, hwnd, flags, pp, out) })
}

unsafe extern "system" fn create_9ex_hook(d3d: usize, adapter: u32, kind: u32, hwnd: usize, flags: u32, pp: *mut PresentParameters, out: *mut usize) -> i32 {
    windowed(pp, |_| unsafe { CREATE_9EX.get().unwrap().call(d3d, adapter, kind, hwnd, flags, pp, out) })
}

unsafe extern "system" fn create_ex_hook(d3d: usize, adapter: u32, kind: u32, hwnd: usize, flags: u32, pp: *mut PresentParameters, mode: usize, out: *mut usize) -> i32 {
    // A windowed device takes no fullscreen display mode.
    windowed(pp, |w| unsafe { CREATE_EX.get().unwrap().call(d3d, adapter, kind, hwnd, flags, pp, if w { 0 } else { mode }, out) })
}

/// Runs `call` with a fullscreen device's parameters made windowed while this copy is
/// in a pane (`call` is told whether they were), and puts the game's own values back
/// after, so the game still sees what it asked for.
fn windowed(pp: *mut PresentParameters, call: impl FnOnce(bool) -> i32) -> i32 {
    if pp.is_null() || !super::fullscreen::keep_windowed() || unsafe { (*pp).windowed } != 0 {
        return call(false);
    }
    super::fullscreen::told("Direct3D 9");
    let game = unsafe { (*pp).clone_values() };
    let fixed = windowed_values(game);
    if (fixed.interval, fixed.format) != (game.interval, game.format) {
        log!(
            "present: the game's fullscreen Direct3D 9 device asked for present interval {:#x} and back buffer format {}; windowed takes {:#x} and {}",
            game.interval,
            game.format,
            fixed.interval,
            fixed.format
        );
    }
    unsafe { (*pp).set_values(fixed) };
    let hr = call(true);
    unsafe { (*pp).set_values(game) };
    hr
}

/// What `windowed` changes of a device's parameters.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Values {
    windowed: i32,
    refresh: u32,
    interval: u32,
    format: u32,
}

impl PresentParameters {
    fn clone_values(&self) -> Values {
        Values { windowed: self.windowed, refresh: self.refresh, interval: self.interval, format: self.format }
    }

    fn set_values(&mut self, v: Values) {
        self.windowed = v.windowed;
        self.refresh = v.refresh;
        self.interval = v.interval;
        self.format = v.format;
    }
}

/// A fullscreen device's values made legal for a windowed one: windowed, no refresh
/// rate, a present interval of at most one (D3DPRESENT_INTERVAL_TWO/THREE/FOUR are
/// fullscreen only: the call fails), and a 16-bit back buffer (fullscreen only too) left
/// to the desktop's format.
fn windowed_values(v: Values) -> Values {
    // D3DPRESENT_INTERVAL_TWO, THREE, FOUR.
    let interval = if matches!(v.interval, 2 | 4 | 8) { 1 } else { v.interval };
    // D3DFMT_R5G6B5, X1R5G5B5, A1R5G5B5, A4R4G4B4, X4R4G4B4 -> D3DFMT_UNKNOWN.
    let format = if matches!(v.format, 23 | 24 | 25 | 26 | 30) { 0 } else { v.format };
    Values { windowed: 1, refresh: 0, interval, format }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fullscreen_only_values_are_made_windowed() {
        let v = windowed_values(Values { windowed: 0, refresh: 144, interval: 2, format: 23 });
        assert_eq!(v, Values { windowed: 1, refresh: 0, interval: 1, format: 0 });
        let v = windowed_values(Values { windowed: 0, refresh: 60, interval: 0x8000_0000, format: 22 });
        assert_eq!((v.interval, v.format), (0x8000_0000, 22), "immediate and X8R8G8B8 stay");
    }
}
