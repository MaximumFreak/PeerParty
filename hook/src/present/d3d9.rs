//! Direct3D 9 games: `IDirect3DDevice9::Present` (slot 17) and, for 9Ex devices,
//! `PresentEx` (121), found through a throwaway device on a hidden window. The frame is
//! a dynamic A8R8G8B8 texture drawn as a pre-transformed quad with premultiplied alpha
//! blending, inside a state block that puts every render, texture-stage and sampler
//! state back (and render target 0, which state blocks don't keep). `Reset` / `ResetEx`
//! (16 / 132) drop the texture first, as Direct3D 9 requires of default-pool resources.
//!
//! Slots are from d3d9.h, IDirect3DDevice9 in declaration order.

use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::com::{self, HiddenWindow, release, vcall};
use super::frame;
use crate::hook::with_lock;
use crate::log;

type Present = unsafe extern "system" fn(usize, usize, usize, usize, usize) -> i32;
type PresentEx = unsafe extern "system" fn(usize, usize, usize, usize, usize, u32) -> i32;
type Reset = unsafe extern "system" fn(usize, usize) -> i32;
type ResetEx = unsafe extern "system" fn(usize, usize, usize) -> i32;

static PRESENT: OnceLock<GenericDetour<Present>> = OnceLock::new();
static PRESENT_EX: OnceLock<GenericDetour<PresentEx>> = OnceLock::new();
static RESET: OnceLock<GenericDetour<Reset>> = OnceLock::new();
static RESET_EX: OnceLock<GenericDetour<ResetEx>> = OnceLock::new();

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
                release(dev);
            }
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
                addrs.push((com::slot(dev, 17), "Present"));
                addrs.push((com::slot(dev, 121), "PresentEx"));
                addrs.push((com::slot(dev, 16), "Reset"));
                addrs.push((com::slot(dev, 132), "ResetEx"));
                release(dev);
            }
            release(d3d);
        }
    }
    drop(win);
    let mut n = 0;
    for (addr, label) in addrs {
        n += match label {
            "Present" => arm(&PRESENT, addr, present_hook as Present, label),
            "PresentEx" => arm(&PRESENT_EX, addr, present_ex_hook as PresentEx, label),
            "Reset" => arm(&RESET, addr, reset_hook as Reset, label),
            _ => arm(&RESET_EX, addr, reset_ex_hook as ResetEx, label),
        };
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
                usize::from(crate::hook::frozen(addr, || unsafe { slot.get().unwrap().enable() }).is_ok())
            }
            Err(e) => {
                log!("present: IDirect3DDevice9::{label} not hooked: {e}");
                0
            }
        }
    })
}

fn drop_texture() {
    if let Some(t) = TEX.lock().unwrap().take() {
        release(t.tex);
        release(t.device);
    }
}

fn texture(device: usize, w: u32, h: u32) -> Option<usize> {
    let mut g = TEX.lock().unwrap();
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
    if t.seq != Some(f.seq) && !f.writing() {
        let mut lr = LockedRect { pitch: 0, bits: std::ptr::null_mut() };
        // IDirect3DTexture9::LockRect / UnlockRect are slots 19 / 20; D3DLOCK_DISCARD.
        if vcall!(t.tex, 19, fn(u32, *mut LockedRect, usize, u32) -> i32, 0, &mut lr, 0, 0x2000) >= 0 {
            let ok = unsafe { f.copy_to(lr.bits, lr.pitch as usize) };
            vcall!(t.tex, 20, fn(u32) -> i32, 0);
            if ok {
                t.seq = Some(f.seq);
            }
        }
    }
    t.seq.map(|_| t.tex)
}

fn draw(device: usize) {
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
        let (w, h) = (desc[6] as f32, desc[7] as f32);
        vcall!(device, 37, fn(u32, usize) -> i32, 0, back);
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

fn presenting(dev: usize, real: impl FnOnce() -> i32) -> i32 {
    let outer = !IN_PRESENT.with(|p| p.replace(true));
    if outer {
        let _ = std::panic::catch_unwind(|| draw(dev));
    }
    let hr = real();
    if outer {
        IN_PRESENT.with(|p| p.set(false));
    }
    hr
}

unsafe extern "system" fn present_hook(dev: usize, src: usize, dst: usize, wnd: usize, dirty: usize) -> i32 {
    presenting(dev, || unsafe { PRESENT.get().unwrap().call(dev, src, dst, wnd, dirty) })
}

unsafe extern "system" fn present_ex_hook(dev: usize, src: usize, dst: usize, wnd: usize, dirty: usize, flags: u32) -> i32 {
    presenting(dev, || unsafe { PRESENT_EX.get().unwrap().call(dev, src, dst, wnd, dirty, flags) })
}

unsafe extern "system" fn reset_hook(dev: usize, pp: usize) -> i32 {
    drop_texture();
    unsafe { RESET.get().unwrap().call(dev, pp) }
}

unsafe extern "system" fn reset_ex_hook(dev: usize, pp: usize, mode: usize) -> i32 {
    drop_texture();
    unsafe { RESET_EX.get().unwrap().call(dev, pp, mode) }
}
