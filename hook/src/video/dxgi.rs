//! DXGI's present, for Direct3D 10, 11 and 12 games. `IDXGISwapChain`'s methods live in
//! dxgi.dll and every swap chain shares them, so a throwaway WARP device and swap chain
//! (on a hidden window, released straight away) gives their addresses: `Present` (slot
//! 8), `ResizeBuffers` (13), `Present1` (22 on IDXGISwapChain1) and `ResizeBuffers1` (39
//! on IDXGISwapChain3).
//!
//! On present:
//! - Reports viewport dimensions / HWND / API (`report_viewport`) whenever dirty.
//! - When the overlay frame is visible, composites it over the back buffer via D3D11 or
//!   D3D11On12.
//! - When the session's window draws the copy's frames (`capture::enabled()`), copies the
//!   back buffer, overlay and all, into a shared texture (capture.rs: `D3d11Capture` on
//!   Direct3D 11, `D3d12Frames` on Direct3D 12's direct queue).
//! - A split-screen copy is never told its frames are hidden (`DXGI_STATUS_OCCLUDED`): its
//!   window sits behind the session's, and some engines stop drawing when they hear it.
//!
//! `SetColorSpace1` (38 on IDXGISwapChain3) says how an HDR game's values are encoded: an
//! HDR10 one's R10G10B10A2 buffers hold PQ-encoded BT.2020 colour, which looks washed out
//! read as sRGB. Games set it right after making the swap chain, before its first
//! present, and again when the player turns HDR on or off without new buffers, so each
//! swap chain's is kept from the start; the frames shared with the session and the
//! overlay drawn into them follow it.
//!
//! Under a translation layer that draws with Vulkan (DXVK on Windows or under Wine), the
//! Vulkan hooks see the same frames, last: they draw and copy them, and this side only
//! reports the viewport (`super::lower_layer_presents`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::capture::{self, D3d11Capture, D3d12Frames};
use super::com::{self, Guid, HiddenWindow, IID_ID3D11DEVICE, IID_ID3D11TEXTURE2D, IID_ID3D12DEVICE, release, vcall};
use super::{d3d11, d3d12, frame};
use crate::log;
use crate::sys::hook::with_lock;

type Present = unsafe extern "system" fn(usize, u32, u32) -> i32;
type Present1 = unsafe extern "system" fn(usize, u32, u32, usize) -> i32;
type Resize = unsafe extern "system" fn(usize, u32, u32, u32, u32, u32) -> i32;
type Resize1 = unsafe extern "system" fn(usize, u32, u32, u32, u32, u32, *const u32, *const usize) -> i32;

static PRESENT: OnceLock<GenericDetour<Present>> = OnceLock::new();
static PRESENT1: OnceLock<GenericDetour<Present1>> = OnceLock::new();
static RESIZE: OnceLock<GenericDetour<Resize>> = OnceLock::new();
static RESIZE1: OnceLock<GenericDetour<Resize1>> = OnceLock::new();
static SET_COLOR_SPACE: OnceLock<GenericDetour<SetColorSpace>> = OnceLock::new();
static VIEWPORT_DIRTY: AtomicBool = AtomicBool::new(true);

type SetColorSpace = unsafe extern "system" fn(usize, u32) -> i32;

/// The colour space each swap chain's game set last (newest last; a few kept: a game has
/// one or two).
static COLOR_SPACES: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());

/// Swap chain `swap` was made: one released before at its address set nothing for it.
pub(super) fn forget(swap: usize) {
    crate::sys::lock(&COLOR_SPACES).retain(|(s, _)| *s != swap);
}

fn note_color_space(swap: usize, space: u32) {
    let mut c = crate::sys::lock(&COLOR_SPACES);
    c.retain(|(s, _)| *s != swap);
    c.push((swap, space));
    if c.len() > 8 {
        c.remove(0);
    }
}

/// How `swap`'s frames (in `format`) are encoded: what the game set, else scRGB for a
/// float swap chain (what Windows takes those as) and sRGB for the rest.
pub(super) fn color_space(swap: usize, format: u32) -> u32 {
    use peerparty_protocol::frame_ring::{COLOR_SPACE_SCRGB, COLOR_SPACE_SRGB};
    if let Some(&(_, c)) = crate::sys::lock(&COLOR_SPACES).iter().find(|(s, _)| *s == swap) {
        return c;
    }
    if capture::typeless(format) == 9 { COLOR_SPACE_SCRGB } else { COLOR_SPACE_SRGB }
}

const IID_IDXGISWAPCHAIN1: Guid = Guid(0x790a45f7, 0x0d42, 0x4876, [0x98, 0x3a, 0x0a, 0x55, 0xcf, 0xe6, 0xf4, 0xaa]);

/// The swap chain's device, by API (its reference held by the `State`'s `Held`).
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    D3d11(usize),
    D3d12(usize),
    /// Neither (Direct3D 10): nothing drawn or copied.
    Other,
}

enum Drawer {
    D3d11(d3d11::Renderer),
    D3d12(d3d12::Renderer),
    /// It couldn't be made: not tried again for this swap chain.
    None,
}

/// A COM reference PeerParty holds, let go when dropped.
struct Held(usize);

impl Drop for Held {
    fn drop(&mut self) {
        release(self.0);
    }
}

/// One swap chain's drawing and copying. Its device is held, so a swap chain made later
/// at a released one's address (with another device) can't be taken for it with a device
/// that's gone: the device is compared too.
struct State {
    swap: usize,
    kind: Kind,
    /// Made the first time the overlay is drawn.
    drawer: Option<Drawer>,
    capture11: D3d11Capture,
    capture12: D3d12Frames,
    /// Last: let go after everything made on it.
    _device: Held,
}

/// The swap chains a game presents (most recently used last; a game has one or two, a
/// few kept so two presenting in turn don't rebuild each other's every frame).
static STATES: Mutex<Vec<State>> = Mutex::new(Vec::new());
const MAX_STATES: usize = 4;

#[repr(C)]
pub(super) struct SwapChainDesc {
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
    pub(super) windowed: i32,
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
    // WARP, else the GPU's driver (Wine's DXGI and DXVK have no WARP).
    let mut hr = 0;
    for driver in [5u32, 1] {
        hr = unsafe { create(0, driver, 0, 0, std::ptr::null(), 0, 7, &desc, &mut swap, &mut dev, &mut fl, &mut ctx) };
        if hr >= 0 && swap != 0 {
            break;
        }
        log!("video: no throwaway {} swap chain ({hr:#x})", if driver == 5 { "WARP" } else { "hardware" });
    }
    if hr < 0 || swap == 0 {
        log!("video: no throwaway swap chain; DXGI not hooked");
        return false;
    }
    let present = com::slot(swap, 8);
    let resize = com::slot(swap, 13);
    let swap1 = com::query(swap, &IID_IDXGISWAPCHAIN1);
    let present1 = (swap1 != 0).then(|| com::slot(swap1, 22));
    let swap3 = com::query(swap, &com::IID_IDXGISWAPCHAIN3);
    let resize1 = (swap3 != 0).then(|| com::slot(swap3, 39));
    let set_color_space = (swap3 != 0).then(|| com::slot(swap3, 38));
    let fullscreen = super::fullscreen::install_dxgi(swap);
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
    if let Some(a) = set_color_space {
        n += arm(&SET_COLOR_SPACE, a, set_color_space_hook as SetColorSpace, "SetColorSpace1");
    }
    log!("video: {fullscreen} DXGI fullscreen calls hooked (a copy in a pane stays windowed)");
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
                usize::from(crate::sys::hook::frozen(addr, || unsafe { slot.get().unwrap().enable() }).is_ok())
            }
            Err(e) => {
                log!("video: IDXGISwapChain::{label} not hooked: {e}");
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

/// `swap`'s device by API, and its reference (held).
fn kind_of(swap: usize) -> (Kind, Held) {
    if let Some(dev) = Some(device(swap, &IID_ID3D11DEVICE)).filter(|d| *d != 0) {
        (Kind::D3d11(dev), Held(dev))
    } else if let Some(dev) = Some(device(swap, &IID_ID3D12DEVICE)).filter(|d| *d != 0) {
        (Kind::D3d12(dev), Held(dev))
    } else {
        (Kind::Other, Held(0))
    }
}

/// `swap`'s state (made, or moved to the back as the most recently used).
fn state_for(st: &mut Vec<State>, swap: usize) -> &mut State {
    let (kind, held) = kind_of(swap);
    if let Some(i) = st.iter().position(|s| s.swap == swap) {
        let s = st.remove(i);
        if s.kind == kind {
            st.push(s);
            return st.last_mut().unwrap();
        }
        log!("video: a swap chain made where a released one was (another device): its drawing and copying start over");
    }
    if st.len() >= MAX_STATES {
        st.remove(0);
    }
    st.push(State { swap, kind, drawer: None, capture11: D3d11Capture::default(), capture12: D3d12Frames::default(), _device: held });
    if st.len() == 2 {
        log!("video: the game presents more than one swap chain; each keeps its own drawing and copying");
    }
    st.last_mut().unwrap()
}

/// The overlay's drawer for `s`, made the first time. None while a Direct3D 12 game's
/// queue isn't known: neither the one its swap chain was made on nor one it submitted on
/// (its ExecuteCommandLists hook goes in here if d3d12.dll wasn't loaded yet when the
/// other hooks did).
fn drawer(s: &mut State) -> Option<&mut Drawer> {
    let (kind, swap) = (s.kind, s.swap);
    // The swap chain moved to another queue (ResizeBuffers1): drawn on that one now.
    if let Some(Drawer::D3d12(r)) = &s.drawer
        && d3d12::swap_queue(swap).is_some_and(|q| q != r.queue)
    {
        s.drawer = None;
    }
    if s.drawer.is_none()
        && let Kind::D3d12(dev) = kind
        && d3d12::swap_queue(swap).is_none()
        && !d3d12::queue_known(dev)
    {
        d3d12::install();
        return None;
    }
    Some(s.drawer.get_or_insert_with(|| match kind {
        Kind::D3d11(dev) => d3d11::Renderer::new(dev).map(Drawer::D3d11).unwrap_or(Drawer::None),
        Kind::D3d12(dev) => d3d12::Renderer::new(dev, swap).map(Drawer::D3d12).unwrap_or(Drawer::None),
        Kind::Other => Drawer::None,
    }))
}

/// Handles outbound capture, viewport reporting, and inbound overlay blitting for `swap`.
fn on_present(swap: usize) {
    let f = frame();
    let cap = capture::enabled();
    let dirty = VIEWPORT_DIRTY.swap(false, Ordering::AcqRel);
    if f.is_none() && !cap && !dirty {
        return;
    }

    let mut desc: SwapChainDesc = unsafe { std::mem::zeroed() };
    vcall!(swap, 12, fn(*mut SwapChainDesc) -> i32, &mut desc);

    // Drawn with Vulkan underneath: the Vulkan hooks draw into and copy these frames.
    let (f, cap) = if super::lower_layer_presents() { (None, false) } else { (f, cap) };
    if f.is_none() && !cap {
        let api = if let Some(dev) = Some(device(swap, &IID_ID3D11DEVICE)).filter(|d| *d != 0) {
            release(dev);
            "d3d11"
        } else if let Some(dev) = Some(device(swap, &IID_ID3D12DEVICE)).filter(|d| *d != 0) {
            release(dev);
            "d3d12"
        } else {
            "dxgi"
        };
        super::report_viewport(desc.width, desc.height, desc.window, api);
        return;
    }

    let mut st = crate::sys::lock(&STATES);
    let s = state_for(&mut st, swap);
    let space = color_space(swap, desc.format);
    let hdr10 = space == peerparty_protocol::frame_ring::COLOR_SPACE_HDR10;
    match s.kind {
        Kind::D3d11(dev) => {
            super::report_viewport(desc.width, desc.height, desc.window, "d3d11");
            let mut tex = 0usize;
            if vcall!(swap, 9, fn(u32, *const Guid, *mut usize) -> i32, 0, &IID_ID3D11TEXTURE2D, &mut tex) < 0 {
                return;
            }
            if let Some(f) = f
                && let Some(Drawer::D3d11(r)) = drawer(s)
            {
                let rtv = r.rtv_for(tex);
                if rtv != 0 {
                    r.hdr10 = hdr10;
                    r.draw(rtv, desc.width, desc.height, &f);
                    super::drew("Direct3D 11");
                }
                release(rtv);
            }
            // After the overlay: the session shows the frame as the window would.
            if cap {
                s.capture11.capture(dev, tex, space);
            }
            release(tex);
        }
        Kind::D3d12(dev) => {
            super::report_viewport(desc.width, desc.height, desc.window, "d3d12");
            if let Some(f) = f
                && let Some(Drawer::D3d12(r)) = drawer(s)
                && r.draw(swap, desc.width, desc.height, &f, hdr10)
            {
                super::drew("Direct3D 12");
            }
            if cap && capture::pixels() {
                // Under Wine a Direct3D 12 game draws through VKD3D, whose Vulkan frames are
                // read back; Direct3D 12's own aren't (yet).
                crate::log::throttled("video: d3d12 pixels", 1, |_| log!("video: this Direct3D 12 game's frames can't be read back as pixels; the session doesn't show them"));
            } else if cap {
                // The copy goes on the queue the swap chain presents on, after the frame:
                // the one it was made on, else the first direct queue the game used (its
                // ExecuteCommandLists hook goes in now if d3d12.dll came after the others).
                match d3d12::swap_queue(swap).or_else(|| d3d12::queue_of(dev)) {
                    Some(queue) => s.capture12.capture(dev, queue, swap, space),
                    None => {
                        d3d12::install();
                        crate::log::throttled("video: d3d12 queue", 1, |_| log!("video: waiting to see the Direct3D 12 game's command queue before copying its frames"));
                    }
                }
            }
        }
        Kind::Other => {
            super::report_viewport(desc.width, desc.height, desc.window, "dxgi");
        }
    }
}

/// Lets go of the back buffers before the game resizes them. The frames' copies hold
/// none between frames, so they stay: they're remade when the buffers' size, format or
/// colour space changes (a game may call this every frame without changing anything, and
/// a new set each time would leave the session's pane between sets).
fn before_resize(swap: usize, w: u32, h: u32) {
    crate::log::throttled("video: resize buffers", 3, |n| log!("video: the game resizes its swap chain's buffers to {w}x{h} (0: its window's size) (call {n})"));
    VIEWPORT_DIRTY.store(true, Ordering::Release);
    let mut st = crate::sys::lock(&STATES);
    if let Some(s) = st.iter_mut().find(|s| s.swap == swap)
        && let Some(Drawer::D3d12(r)) = &mut s.drawer
    {
        r.release_buffers();
    }
}

thread_local! {
    /// Inside one of our present hooks: a present that calls the other one internally
    /// draws once.
    static IN_PRESENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn presenting(swap: usize, flags: u32, real: impl FnOnce() -> i32) -> i32 {
    let outer = !IN_PRESENT.with(|p| p.replace(true));
    if outer && flags & 1 == 0 {
        if !super::lower_layer_presents() {
            super::presented();
        }

        let _ = std::panic::catch_unwind(|| on_present(swap));
    }
    let hr = real();
    if outer {
        IN_PRESENT.with(|p| p.set(false));
    }
    not_occluded(hr)
}

/// DXGI_STATUS_OCCLUDED.
const OCCLUDED: i32 = 0x087A_0001;

/// A split-screen copy's window is behind the session's: its presents are answered as
/// shown, so a game that stops drawing while hidden keeps going.
fn not_occluded(hr: i32) -> i32 {
    if hr == OCCLUDED && crate::window::in_pane() { 0 } else { hr }
}

unsafe extern "system" fn present_hook(swap: usize, sync: u32, flags: u32) -> i32 {
    presenting(swap, flags, || unsafe { PRESENT.get().unwrap().call(swap, sync, flags) })
}

unsafe extern "system" fn present1_hook(swap: usize, sync: u32, flags: u32, params: usize) -> i32 {
    presenting(swap, flags, || unsafe { PRESENT1.get().unwrap().call(swap, sync, flags, params) })
}

unsafe extern "system" fn resize_hook(swap: usize, n: u32, w: u32, h: u32, fmt: u32, flags: u32) -> i32 {
    let _ = std::panic::catch_unwind(|| before_resize(swap, w, h));
    unsafe { RESIZE.get().unwrap().call(swap, n, w, h, fmt, flags) }
}

unsafe extern "system" fn set_color_space_hook(swap: usize, space: u32) -> i32 {
    let hr = unsafe { SET_COLOR_SPACE.get().unwrap().call(swap, space) };
    if hr >= 0 {
        let _ = std::panic::catch_unwind(|| {
            if color_space(swap, 0) != space {
                log!("video: the game's frames are in colour space {space}");
            }
            note_color_space(swap, space);
        });
    }
    hr
}

unsafe extern "system" fn resize1_hook(swap: usize, n: u32, w: u32, h: u32, fmt: u32, flags: u32, mask: *const u32, queues: *const usize) -> i32 {
    let _ = std::panic::catch_unwind(|| before_resize(swap, w, h));
    let hr = unsafe { RESIZE1.get().unwrap().call(swap, n, w, h, fmt, flags, mask, queues) };
    // A Direct3D 12 swap chain may present on another queue from here on (the first
    // buffer's): the overlay and the copies follow it.
    if hr >= 0 && !queues.is_null() && n > 0 {
        let q = unsafe { *queues };
        let _ = std::panic::catch_unwind(|| d3d12::note_swap(swap, q));
    }
    hr
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::capture;

    /// What a game set sticks to its swap chain until one is made at the same address;
    /// without it a float swap chain is scRGB and the rest sRGB.
    #[test]
    fn colour_spaces_follow_their_swap_chain() {
        use peerparty_protocol::frame_ring::{COLOR_SPACE_HDR10, COLOR_SPACE_SCRGB, COLOR_SPACE_SRGB};
        let (a, b) = (0x7000_1230usize, 0x7000_4560usize);
        assert_eq!(color_space(a, 24), COLOR_SPACE_SRGB);
        assert_eq!(color_space(a, 10), COLOR_SPACE_SCRGB);
        note_color_space(a, COLOR_SPACE_HDR10);
        assert_eq!(color_space(a, 24), COLOR_SPACE_HDR10);
        assert_eq!(color_space(b, 24), COLOR_SPACE_SRGB, "another swap chain's own");
        note_color_space(a, COLOR_SPACE_SRGB);
        assert_eq!(color_space(a, 24), COLOR_SPACE_SRGB, "HDR turned off");
        note_color_space(a, COLOR_SPACE_HDR10);
        forget(a);
        assert_eq!(color_space(a, 24), COLOR_SPACE_SRGB, "a new swap chain where it was");
    }

    #[test]
    fn present_hook_draws_the_shared_frame() {
        use windows_sys::Win32::System::Memory::*;
        let (w, h) = (64u32, 64u32);
        let name = format!(r"Local\peerparty-frame-test-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let size = peerparty_protocol::shm::block_size((w * h) as usize).unwrap();
        let map = unsafe {
            CreateFileMappingW(
                windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                size as u32,
                wide.as_ptr(),
            )
        };
        assert!(!map.is_null());
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size) }.Value as *mut u32;
        let header = unsafe { peerparty_protocol::shm::FrameHeader::from_raw(view as *const u8, size) }.unwrap();
        header.init(w * h);
        let frame = peerparty_protocol::shm::FrameWrite { visible: true, w, h, ..Default::default() };
        header.publish(&frame, || {
            for i in 0..(w * h) as usize {
                unsafe { *view.add(peerparty_protocol::shm::HEADER_SIZE / 4 + i) = 0x8080_0000 };
            }
        });
        let _ = com::load("d3d11.dll");
        super::super::attach(&name);
        assert!(PRESENT.get().is_some(), "Present hooked");
        let win = HiddenWindow::new().unwrap();
        type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *const SwapChainDesc, *mut usize, *mut usize, *mut u32, *mut usize) -> i32;
        let create: Create = unsafe { std::mem::transmute(com::proc(com::module("d3d11.dll"), c"D3D11CreateDeviceAndSwapChain").unwrap()) };
        let desc = SwapChainDesc {
            width: w,
            height: h,
            refresh_num: 60,
            refresh_den: 1,
            format: 87,
            scanline: 0,
            scaling: 0,
            sample_count: 1,
            sample_quality: 0,
            usage: 0x20,
            buffer_count: 1,
            window: win.0 as usize,
            windowed: 1,
            effect: 1,
            flags: 0,
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

    /// A Direct3D 12 game whose swap chain was made after the hooks went in (as an Unreal
    /// one's is): with no ExecuteCommandLists hook to learn its queue, the queue it made
    /// the swap chain on carries the copy, and its frames are published. (The present
    /// hook's work is called directly: installing every hook here would race other tests.)
    #[test]
    fn d3d12_presents_are_copied_for_the_session() {
        const IID_IDXGIFACTORY2: Guid = Guid(0x50c83a1c, 0xe072, 0x4c48, [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0]);
        const IID_IDXGIFACTORY4: Guid = Guid(0x1bc6ea02, 0xef36, 0x464f, [0xbf, 0x0c, 0x21, 0xca, 0x39, 0xe5, 0x16, 0x8a]);
        const IID_IDXGIADAPTER: Guid = Guid(0x2411e7e1, 0x12ac, 0x4ccf, [0xbd, 0x14, 0x97, 0x98, 0xe8, 0x53, 0x4d, 0xc0]);
        let _turn = crate::sys::lock(&capture::tests::SERIAL);
        let _ = com::load("d3d11.dll");
        let (_map, name) = capture::tests::session_ring();
        capture::configure(true, Some(&name), None);

        type CreateFactory = unsafe extern "system" fn(*const Guid, *mut usize) -> i32;
        let cf: CreateFactory = unsafe { std::mem::transmute(com::proc(com::load("dxgi.dll"), c"CreateDXGIFactory1").unwrap()) };
        let mut factory = 0usize;
        assert!(unsafe { cf(&com::IID_IDXGIFACTORY1, &mut factory) } >= 0);
        let (f2, f4) = (com::query(factory, &IID_IDXGIFACTORY2), com::query(factory, &IID_IDXGIFACTORY4));
        let mut adapter = 0usize;
        assert!(vcall!(f4, 27, fn(*const Guid, *mut usize) -> i32, &IID_IDXGIADAPTER, &mut adapter) >= 0);
        type Create12 = unsafe extern "system" fn(usize, u32, *const Guid, *mut usize) -> i32;
        let c12: Create12 = unsafe { std::mem::transmute(com::proc(com::load("d3d12.dll"), c"D3D12CreateDevice").unwrap()) };
        let mut dev = 0usize;
        assert!(unsafe { c12(adapter, 0xb000, &IID_ID3D12DEVICE, &mut dev) } >= 0);
        let mut queue = 0usize;
        assert!(vcall!(dev, 8, fn(*const u32, *const Guid, *mut usize) -> i32, [0u32; 4].as_ptr(), &com::IID_ID3D12COMMANDQUEUE, &mut queue) >= 0);
        let win = HiddenWindow::new().unwrap();
        // DXGI_SWAP_CHAIN_DESC1: 64x32 BGRA, 2 buffers, flip-discard.
        let desc: [u32; 12] = [64, 32, 87, 0, 1, 0, 0x20, 2, 0, 4, 0, 0];
        let mut swap = 0usize;
        let hr = vcall!(f2, 15, fn(usize, usize, *const u32, usize, usize, *mut usize) -> i32, queue, win.0 as usize, desc.as_ptr(), 0, 0, &mut swap);
        assert!(hr >= 0 && swap != 0, "CreateSwapChainForHwnd {hr:#x}");
        // What fullscreen.rs's CreateSwapChainForHwnd hook notes.
        d3d12::note_swap(swap, queue);
        assert_eq!(d3d12::swap_queue(swap), Some(queue));
        for _ in 0..3 {
            on_present(swap);
            let hr = vcall!(swap, 8, fn(u32, u32) -> i32, 0, 0);
            assert!(hr >= 0, "Present {hr:#x}");
        }
        let p = capture::published().expect("a Direct3D 12 frame published for the session");
        assert!(p.ready > 0);
        capture::configure(false, None, None);
        // The swap chain's state goes with it.
        crate::sys::lock(&STATES).retain(|s| s.swap != swap);
        for o in [swap, queue, dev, adapter, f4, f2, factory] {
            release(o);
        }
    }
}
