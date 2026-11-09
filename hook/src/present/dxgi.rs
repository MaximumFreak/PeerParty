//! DXGI's present, for Direct3D 10, 11 and 12 games. `IDXGISwapChain`'s methods live in
//! dxgi.dll and every swap chain shares them, so a throwaway WARP device and swap chain
//! (on a hidden window, released straight away) gives their addresses: `Present` (slot
//! 8), `ResizeBuffers` (13), `Present1` (22 on IDXGISwapChain1) and `ResizeBuffers1` (39
//! on IDXGISwapChain3).
//!
//! On a present with the overlay showing, the swap chain's device says which API the
//! game uses: an ID3D11Device draws through d3d11.rs, an ID3D12Device through d3d12.rs.
//! A resize first lets go of anything holding the old back buffers.

use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::com::{self, Guid, HiddenWindow, IID_ID3D11DEVICE, IID_ID3D11TEXTURE2D, IID_ID3D12DEVICE, release, vcall};
use super::{d3d11, d3d12, frame};
use crate::hook::with_lock;
use crate::log;

type Present = unsafe extern "system" fn(usize, u32, u32) -> i32;
type Present1 = unsafe extern "system" fn(usize, u32, u32, usize) -> i32;
type Resize = unsafe extern "system" fn(usize, u32, u32, u32, u32, u32) -> i32;
type Resize1 = unsafe extern "system" fn(usize, u32, u32, u32, u32, u32, *const u32, *const usize) -> i32;

static PRESENT: OnceLock<GenericDetour<Present>> = OnceLock::new();
static PRESENT1: OnceLock<GenericDetour<Present1>> = OnceLock::new();
static RESIZE: OnceLock<GenericDetour<Resize>> = OnceLock::new();
static RESIZE1: OnceLock<GenericDetour<Resize1>> = OnceLock::new();

const IID_IDXGISWAPCHAIN1: Guid = Guid(0x790a45f7, 0x0d42, 0x4876, [0x98, 0x3a, 0x0a, 0x55, 0xcf, 0xe6, 0xf4, 0xaa]);

enum Drawer {
    D3d11(d3d11::Renderer),
    D3d12(d3d12::Renderer),
    /// The swap chain's device is neither (Direct3D 10): nothing drawn.
    None,
}

struct State {
    swap: usize,
    drawer: Drawer,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

#[repr(C)]
struct SwapChainDesc {
    width: u32,
    height: u32,
    refresh_num: u32,
    refresh_den: u32,
    format: u32,
    scanline: u32,
    scaling: u32,
    sample_count: u32,
    sample_quality: u32,
    usage: u32,
    buffer_count: u32,
    window: usize,
    windowed: i32,
    effect: u32,
    flags: u32,
}

pub fn install() -> bool {
    if com::module("dxgi.dll").is_null() {
        return false;
    }
    let Some(win) = HiddenWindow::new() else { return false };
    type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *const SwapChainDesc, *mut usize, *mut usize, *mut u32, *mut usize) -> i32;
    let Some(f) = com::proc(com::load("d3d11.dll"), c"D3D11CreateDeviceAndSwapChain") else { return false };
    let create: Create = unsafe { std::mem::transmute(f) };
    let desc = SwapChainDesc {
        width: 64,
        height: 64,
        refresh_num: 60,
        refresh_den: 1,
        format: 28,
        scanline: 0,
        scaling: 0,
        sample_count: 1,
        sample_quality: 0,
        usage: 0x20,
        buffer_count: 1,
        window: win.0 as usize,
        windowed: 1,
        effect: 0,
        flags: 0,
    };
    let (mut swap, mut dev, mut fl, mut ctx) = (0usize, 0usize, 0u32, 0usize);
    let hr = unsafe { create(0, 5, 0, 0, std::ptr::null(), 0, 7, &desc, &mut swap, &mut dev, &mut fl, &mut ctx) };
    if hr < 0 || swap == 0 {
        log!("present: no throwaway swap chain ({hr:#x}); DXGI not hooked");
        return false;
    }
    let present = com::slot(swap, 8);
    let resize = com::slot(swap, 13);
    let swap1 = com::query(swap, &IID_IDXGISWAPCHAIN1);
    let present1 = (swap1 != 0).then(|| com::slot(swap1, 22));
    let swap3 = com::query(swap, &com::IID_IDXGISWAPCHAIN3);
    let resize1 = (swap3 != 0).then(|| com::slot(swap3, 39));
    for o in [swap3, swap1, swap, ctx, dev] {
        release(o);
    }
    drop(win);
    d3d12::install();
    let mut n = 0;
    n += arm(&PRESENT, present, present_hook as Present, "Present");
    n += arm(&RESIZE, resize, resize_hook as Resize, "ResizeBuffers");
    if let Some(a) = present1 {
        n += arm(&PRESENT1, a, present1_hook as Present1, "Present1");
    }
    if let Some(a) = resize1 {
        n += arm(&RESIZE1, a, resize1_hook as Resize1, "ResizeBuffers1");
    }
    n > 0
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, addr: usize, detour: T, label: &str) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let target: T = unsafe { T::from_ptr(addr as *const ()) };
        match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => {
                let _ = slot.set(h);
                usize::from(crate::hook::frozen(addr, || unsafe { slot.get().unwrap().enable() }).is_ok())
            }
            Err(e) => {
                log!("present: IDXGISwapChain::{label} not hooked: {e}");
                0
            }
        }
    })
}

fn device(swap: usize, iid: &Guid) -> usize {
    let mut d = 0usize;
    let hr = vcall!(swap, 7, fn(*const Guid, *mut usize) -> i32, iid, &mut d);
    if hr < 0 { 0 } else { d }
}

/// Draws the overlay into `swap`'s back buffer, if it's showing.
fn draw(swap: usize) {
    let Some(f) = frame() else { return };
    let mut st = STATE.lock().unwrap();
    if st.as_ref().is_none_or(|s| s.swap != swap) {
        let drawer = if let Some(dev) = Some(device(swap, &IID_ID3D11DEVICE)).filter(|d| *d != 0) {
            let r = d3d11::Renderer::new(dev);
            release(dev);
            r.map(Drawer::D3d11).unwrap_or(Drawer::None)
        } else if let Some(dev) = Some(device(swap, &IID_ID3D12DEVICE)).filter(|d| *d != 0) {
            if !d3d12::queue_known(dev) {
                // The game hasn't submitted since the hook went in: try next frame.
                release(dev);
                return;
            }
            let r = d3d12::Renderer::new(dev, swap);
            release(dev);
            r.map(Drawer::D3d12).unwrap_or(Drawer::None)
        } else {
            Drawer::None
        };
        *st = Some(State { swap, drawer });
    }
    let Some(s) = st.as_mut() else { return };
    let mut desc: SwapChainDesc = unsafe { std::mem::zeroed() };
    vcall!(swap, 12, fn(*mut SwapChainDesc) -> i32, &mut desc);
    match &mut s.drawer {
        Drawer::D3d11(r) => {
            let mut tex = 0usize;
            if vcall!(swap, 9, fn(u32, *const Guid, *mut usize) -> i32, 0, &IID_ID3D11TEXTURE2D, &mut tex) < 0 {
                return;
            }
            let rtv = r.rtv_for(tex);
            if rtv != 0 {
                r.draw(rtv, desc.width, desc.height, &f);
                super::drew("Direct3D 11");
            }
            release(rtv);
            release(tex);
        }
        Drawer::D3d12(r) => {
            if r.draw(swap, desc.width, desc.height, &f) {
                super::drew("Direct3D 12");
            }
        }
        Drawer::None => {}
    }
}

/// Lets go of the back buffers before the game resizes them.
fn before_resize(swap: usize) {
    let mut st = STATE.lock().unwrap();
    if let Some(State { swap: s, drawer: Drawer::D3d12(r) }) = st.as_mut() {
        if *s == swap {
            r.release_buffers();
        }
    }
}

thread_local! {
    /// Inside one of our present hooks: a present that calls the other one internally
    /// (it's patched in place, so internal calls come through here too) draws once.
    static IN_PRESENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Draws (unless DXGI_PRESENT_TEST, which only asks whether presenting would work, or
/// nested), then presents.
fn presenting(swap: usize, flags: u32, real: impl FnOnce() -> i32) -> i32 {
    let outer = !IN_PRESENT.with(|p| p.replace(true));
    if outer && flags & 1 == 0 {
        let _ = std::panic::catch_unwind(|| draw(swap));
    }
    let hr = real();
    if outer {
        IN_PRESENT.with(|p| p.set(false));
    }
    hr
}

unsafe extern "system" fn present_hook(swap: usize, sync: u32, flags: u32) -> i32 {
    presenting(swap, flags, || unsafe { PRESENT.get().unwrap().call(swap, sync, flags) })
}

unsafe extern "system" fn present1_hook(swap: usize, sync: u32, flags: u32, params: usize) -> i32 {
    presenting(swap, flags, || unsafe { PRESENT1.get().unwrap().call(swap, sync, flags, params) })
}

unsafe extern "system" fn resize_hook(swap: usize, n: u32, w: u32, h: u32, fmt: u32, flags: u32) -> i32 {
    before_resize(swap);
    unsafe { RESIZE.get().unwrap().call(swap, n, w, h, fmt, flags) }
}

unsafe extern "system" fn resize1_hook(swap: usize, n: u32, w: u32, h: u32, fmt: u32, flags: u32, mask: *const u32, queues: *const usize) -> i32 {
    before_resize(swap);
    unsafe { RESIZE1.get().unwrap().call(swap, n, w, h, fmt, flags, mask, queues) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole path: a frame block like the launcher's, `attach`, a WARP swap chain on
    /// a hidden (never shown) window cleared blue, a Present through the hook, and the
    /// back buffer read back with the overlay blended in.
    #[test]
    fn present_hook_draws_the_shared_frame() {
        use windows_sys::Win32::System::Memory::*;
        let (w, h) = (64u32, 64u32);
        let name = format!(r"Local\peerparty-frame-test-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let size = super::super::HEADER + (w * h * 4) as usize;
        let map = unsafe { CreateFileMappingW(windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, size as u32, wide.as_ptr()) };
        assert!(!map.is_null());
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size) }.Value as *mut u32;
        unsafe {
            for (i, v) in [(0usize, u32::from_le_bytes(*b"LBOV")), (1, 2), (2, 1), (5, w), (6, h), (7, w * h)] {
                *view.add(i) = v;
            }
            for i in 0..(w * h) as usize {
                *view.add(super::super::HEADER / 4 + i) = 0x8080_0000;
            }
        }
        let _ = com::load("d3d11.dll");
        super::super::attach(&name);
        assert!(PRESENT.get().is_some(), "Present hooked");
        let win = HiddenWindow::new().unwrap();
        type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *const SwapChainDesc, *mut usize, *mut usize, *mut u32, *mut usize) -> i32;
        let create: Create = unsafe { std::mem::transmute(com::proc(com::module("d3d11.dll"), c"D3D11CreateDeviceAndSwapChain").unwrap()) };
        let desc = SwapChainDesc {
            width: w, height: h, refresh_num: 60, refresh_den: 1, format: 87, scanline: 0, scaling: 0, sample_count: 1, sample_quality: 0,
            usage: 0x20, buffer_count: 1, window: win.0 as usize, windowed: 1, effect: 1, flags: 0,
        };
        let (mut swap, mut dev, mut fl, mut ctx) = (0usize, 0usize, 0u32, 0usize);
        assert!(unsafe { create(0, 5, 0, 0x20, std::ptr::null(), 0, 7, &desc, &mut swap, &mut dev, &mut fl, &mut ctx) } >= 0);
        let mut tex = 0usize;
        assert!(vcall!(swap, 9, fn(u32, *const Guid, *mut usize) -> i32, 0, &IID_ID3D11TEXTURE2D, &mut tex) >= 0);
        let mut rtv = 0usize;
        assert!(vcall!(dev, 9, fn(usize, usize, *mut usize) -> i32, tex, 0, &mut rtv) >= 0);
        vcall!(ctx, 50, fn(usize, *const f32) -> (), rtv, [0.0f32, 0.0, 1.0, 1.0].as_ptr());
        let hr = vcall!(swap, 8, fn(u32, u32) -> i32, 0, 0);
        assert!(hr >= 0, "Present {hr:#x}");
        let px = d3d11::read_pixels(dev, ctx, tex, w, h);
        let p = px[(h / 2 * w + w / 2) as usize];
        let (r, b) = ((p >> 16) & 0xff, p & 0xff);
        assert!((r as i32 - 128).abs() <= 2 && (b as i32 - 127).abs() <= 2, "pixel {p:#x}");
        assert_eq!(super::super::api(), "Direct3D 11");
        for o in [rtv, tex, ctx, dev, swap] {
            release(o);
        }
    }
}
