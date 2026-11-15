//! Draws the launcher's overlay frame inside the game's own frames, for games where a
//! window over the game can't show (exclusive fullscreen). The launcher renders the
//! overlay (launcher/src/host) into a named shared-memory block; each API's present
//! hook here uploads it when it changed and draws it as one textured, alpha-blended
//! quad just before the game's frame goes to the screen.
//!
//! Nothing here runs until the launcher asks for it (`attach`, sent when it first needs
//! to draw inside the game): no graphics hooks for games that never need them. Once
//! installed, a present while the overlay is hidden costs one atomic load.
//!
//! - DXGI (Direct3D 10/11/12): `IDXGISwapChain::Present` / `Present1` (the vtable is
//!   found through a throwaway WARP device and swap chain). D3D11 draws directly;
//!   D3D12 through D3D11On12 on the game's direct command queue (captured from
//!   `ExecuteCommandLists`).
//! - Direct3D 9: `IDirect3DDevice9::Present` / `PresentEx` (d3d9.rs).
//! - OpenGL: `wglSwapBuffers`, drawn with a context of PeerParty's own on the game's
//!   device context so the game's GL state is never touched (gl.rs).
//! - Vulkan: `vkQueuePresentKHR` through the loader's `vkGetDeviceProcAddr`, drawing a
//!   blended quad into the swap chain image (vulkan.rs).
//!
//! The frame layout is launcher/src/host/frame.rs's.

mod com;
mod d3d11;
mod d3d12;
mod d3d9;
mod dxgi;
mod gl;
mod vkshaders;
mod vulkan;

use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, Once};

use crate::log;

pub const HEADER: usize = 256;
const MAGIC: u32 = u32::from_le_bytes(*b"LBOV");

/// The mapped frame block, null until `attach`: its base and size together, so a reader
/// never pairs one block's base with another's size. A block the launcher replaces (a
/// bigger frame) stays mapped and its `View` stays allocated: a present may still be
/// reading it, and it happens a few times a run at most.
static VIEW: AtomicPtr<View> = AtomicPtr::new(std::ptr::null_mut());
/// The block's name, to tell a new block from the one already mapped.
static VIEW_NAME: Mutex<String> = Mutex::new(String::new());

struct View {
    base: usize,
    /// Its size in bytes as mapped (what Windows says, never the header's word).
    size: usize,
}

/// The block mapped now: (base, size), or (0, 0).
fn view() -> (usize, usize) {
    let v = VIEW.load(Ordering::Acquire);
    if v.is_null() { (0, 0) } else { unsafe { ((*v).base, (*v).size) } }
}
/// Which API drew a frame last ("" until one has).
static API: Mutex<&'static str> = Mutex::new("");
static INSTALL: Once = Once::new();

/// The graphics API PeerParty draws into ("" until a frame has been drawn there).
pub fn api() -> &'static str {
    *API.lock().unwrap()
}

pub(crate) fn drew(api: &'static str) {
    let mut a = API.lock().unwrap();
    if *a != api {
        log!("present: drawing the overlay inside the game's {api} frames");
        *a = api;
    }
}

/// The game loaded vulkan-1.dll: Vulkan's hooks go in now, since a game sets up its
/// swap chain once, at start-up (vulkan.rs). Only when the player set the overlay to
/// draw inside games (`PEERPARTY_DRAW=ingame`): hooking at start-up is the one thing
/// here that touches a game that never opens the overlay, and some games don't run with
/// it. Direct3D and OpenGL are hooked when they're needed.
pub fn vulkan_loaded() {
    let ingame = std::env::var("PEERPARTY_DRAW").is_ok_and(|v| v.eq_ignore_ascii_case("ingame"));
    if crate::ipc::enabled() && ingame && !crate::net::skip("present") {
        vulkan::loaded();
    }
}

/// The launcher made its frame block: open it and hook the game's presents.
pub fn attach(name: &str) {
    if crate::net::skip("present") {
        return;
    }
    {
        let mut known = VIEW_NAME.lock().unwrap();
        if *known != name {
            match open(name) {
                Some((base, size)) => {
                    VIEW.store(Box::into_raw(Box::new(View { base, size })), Ordering::Release);
                    *known = name.to_string();
                }
                None => {
                    log!("present: couldn't open the overlay's frame {name}");
                    return;
                }
            }
        }
    }
    INSTALL.call_once(|| {
        let mut hooked = Vec::new();
        if dxgi::install() {
            hooked.push("DXGI");
        }
        if d3d9::install() {
            hooked.push("Direct3D 9");
        }
        if gl::install() {
            hooked.push("OpenGL");
        }
        if vulkan::install() {
            hooked.push("Vulkan");
        }
        log!("present: hooked {}", if hooked.is_empty() { "nothing (no known graphics API loaded)".into() } else { hooked.join(", ") });
    });
}

/// Maps a frame block: its base and its size as mapped, so a frame can't be read past
/// the block's end whatever its header says.
fn open(name: &str) -> Option<(usize, usize)> {
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MapViewOfFile, OpenFileMappingW, VirtualQuery};
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let map = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) };
    if map.is_null() {
        return None;
    }
    let view = unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0) };
    // The view keeps the section alive.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(map) };
    let base = view.Value as usize;
    if base == 0 {
        return None;
    }
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let got = unsafe { VirtualQuery(base as *const _, &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
    let size = if got == 0 { 0 } else { info.RegionSize };
    if size < HEADER || unsafe { (base as *const u32).read() } != MAGIC {
        return None;
    }
    log!("present: overlay frame {name} mapped ({size} bytes)");
    Some((base, size))
}

fn word(base: usize, off: usize) -> u32 {
    unsafe { (*((base + off) as *const AtomicU32)).load(Ordering::Acquire) }
}

/// A complete frame to draw: its sequence number, size and pixels (premultiplied BGRA).
pub struct Frame {
    pub seq: u32,
    pub w: u32,
    pub h: u32,
    pub px: *const u32,
    /// The block it is in.
    base: usize,
}

/// The current frame if the overlay (or a notification) is showing, else None. `None`
/// too while the launcher is halfway through writing one (the caller keeps showing the
/// last frame it uploaded).
pub fn frame() -> Option<Frame> {
    let (base, size) = view();
    if base == 0 || word(base, 8) == 0 {
        return None;
    }
    let seq = word(base, 4);
    let (w, h) = (word(base, 20), word(base, 24));
    let room = ((size - HEADER) / 4) as u64;
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > room {
        return None;
    }
    Some(Frame { seq, w, h, base, px: (base + HEADER) as *const u32 })
}

impl Frame {
    /// Whether the launcher is writing this frame right now (its sequence is odd).
    pub fn writing(&self) -> bool {
        self.seq & 1 == 1
    }

    /// The frame's pixels, copied out of the block and checked afterwards that the
    /// launcher didn't rewrite them meanwhile: None when it did (keep the old upload).
    /// The copy is what goes to the graphics API, so a frame half rewritten while it was
    /// read is never shown, not even for one frame.
    pub fn stable(&self) -> Option<std::sync::MutexGuard<'static, Vec<u32>>> {
        static COPY: Mutex<Vec<u32>> = Mutex::new(Vec::new());
        if self.writing() {
            return None;
        }
        let n = self.w as usize * self.h as usize;
        let mut copy = COPY.lock().unwrap_or_else(|e| e.into_inner());
        copy.clear();
        copy.reserve(n);
        unsafe {
            std::ptr::copy_nonoverlapping(self.px, copy.as_mut_ptr(), n);
            copy.set_len(n);
        }
        (word(self.base, 4) == self.seq).then_some(copy)
    }

    /// Copies the frame into `dst` (rows `pitch` bytes apart) if it was read whole
    /// (`stable`). False when it wasn't, and `dst` is left as it was.
    ///
    /// # Safety
    /// `dst` must hold `h` rows of `pitch` bytes, `pitch >= w * 4`.
    pub unsafe fn copy_to(&self, dst: *mut u8, pitch: usize) -> bool {
        let Some(px) = self.stable() else { return false };
        let row = self.w as usize * 4;
        for y in 0..self.h as usize {
            unsafe { std::ptr::copy_nonoverlapping((px.as_ptr() as *const u8).add(y * row), dst.add(y * pitch), row) };
        }
        true
    }
}
