//! OpenGL games: `wglSwapBuffers`, and `wglSwapLayerBuffers` and gdi32's `SwapBuffers`,
//! which a game may call instead (under Wine gdi32's doesn't go through opengl32's); a
//! swap that runs another of them inside is seen once. The overlay is drawn with an
//! OpenGL context of PeerParty's own, made on the game's device context (so it shares its
//! pixel format and default framebuffer): the game's context is set aside for the draw and
//! put back after, and none of its GL state is touched. The drawing is OpenGL 1.1
//! (immediate mode, a BGRA texture, premultiplied blending), all from opengl32's own
//! exports.
//!
//! The same context copies the frame for the session's window when it draws this copy's
//! frames (glcapture.rs), after the overlay, in the same turn as current. One context per
//! window, whatever device context the game swaps with (a game may get a new one from
//! `GetDC` every frame); it goes with the window, and its GL objects are freed with it
//! current where that still works.
//!
//! The viewport is reported at every swap, with or without the overlay or capture. Under a
//! GL-on-Vulkan driver (Zink) the Vulkan hooks see the frames last and draw and copy them
//! there (`super::lower_layer_presents`).

use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::com;
use super::frame;

#[path = "glcapture.rs"]
mod glcapture;
use crate::sys::hook::with_lock;
use crate::log;

type Swap = unsafe extern "system" fn(usize) -> i32;
type SwapLayer = unsafe extern "system" fn(usize, u32) -> i32;
static SWAP: OnceLock<GenericDetour<Swap>> = OnceLock::new();
static SWAP_LAYER: OnceLock<GenericDetour<SwapLayer>> = OnceLock::new();
static GDI_SWAP: OnceLock<GenericDetour<Swap>> = OnceLock::new();

struct Gl {
    make_current: unsafe extern "system" fn(usize, usize) -> i32,
    create_context: unsafe extern "system" fn(usize) -> usize,
    delete_context: unsafe extern "system" fn(usize) -> i32,
    get_context: unsafe extern "system" fn() -> usize,
    get_dc: unsafe extern "system" fn() -> usize,
    viewport: unsafe extern "system" fn(i32, i32, i32, i32),
    matrix_mode: unsafe extern "system" fn(u32),
    load_identity: unsafe extern "system" fn(),
    ortho: unsafe extern "system" fn(f64, f64, f64, f64, f64, f64),
    enable: unsafe extern "system" fn(u32),
    disable: unsafe extern "system" fn(u32),
    blend_func: unsafe extern "system" fn(u32, u32),
    gen_textures: unsafe extern "system" fn(i32, *mut u32),
    delete_textures: unsafe extern "system" fn(i32, *const u32),
    bind_texture: unsafe extern "system" fn(u32, u32),
    tex_parameteri: unsafe extern "system" fn(u32, u32, i32),
    pixel_storei: unsafe extern "system" fn(u32, i32),
    tex_image: unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const u8),
    tex_sub_image: unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const u8),
    begin: unsafe extern "system" fn(u32),
    end: unsafe extern "system" fn(),
    tex_coord: unsafe extern "system" fn(f32, f32),
    vertex: unsafe extern "system" fn(f32, f32),
    color: unsafe extern "system" fn(f32, f32, f32, f32),
    tex_env: unsafe extern "system" fn(u32, u32, i32),
}

static GL: OnceLock<Option<Gl>> = OnceLock::new();

/// PeerParty's context for one window (or, for a device context without one, that device
/// context), the overlay's texture in it, its size and frame, and the frames for the
/// session's window.
struct Ctx {
    /// The device context it was last made current on.
    dc: usize,
    /// The window (0: a device context without one, known by `dc`).
    hwnd: usize,
    ctx: usize,
    tex: u32,
    size: (u32, u32),
    seq: Option<u32>,
    /// Shared textures, tried once (None inside: the driver can't share them, and the
    /// frames are read back instead).
    capture: Option<Option<glcapture::GlCapture>>,
    /// Frames read back (pixel mode, or without shared textures), tried once.
    readback: Option<Option<glcapture::GlReadback>>,
}

/// The contexts, the most recently used last.
static CTX: Mutex<Vec<Ctx>> = Mutex::new(Vec::new());
/// At most this many are kept: the oldest goes first.
const MAX_CTX: usize = 8;

fn gl() -> Option<&'static Gl> {
    GL.get_or_init(|| {
        let m = com::module("opengl32.dll");
        macro_rules! f {
            ($n:literal) => {
                unsafe { std::mem::transmute(com::proc(m, $n)?) }
            };
        }
        Some(Gl {
            make_current: f!(c"wglMakeCurrent"),
            create_context: f!(c"wglCreateContext"),
            delete_context: f!(c"wglDeleteContext"),
            get_context: f!(c"wglGetCurrentContext"),
            get_dc: f!(c"wglGetCurrentDC"),
            viewport: f!(c"glViewport"),
            matrix_mode: f!(c"glMatrixMode"),
            load_identity: f!(c"glLoadIdentity"),
            ortho: f!(c"glOrtho"),
            enable: f!(c"glEnable"),
            disable: f!(c"glDisable"),
            blend_func: f!(c"glBlendFunc"),
            gen_textures: f!(c"glGenTextures"),
            delete_textures: f!(c"glDeleteTextures"),
            bind_texture: f!(c"glBindTexture"),
            tex_parameteri: f!(c"glTexParameteri"),
            pixel_storei: f!(c"glPixelStorei"),
            tex_image: f!(c"glTexImage2D"),
            tex_sub_image: f!(c"glTexSubImage2D"),
            begin: f!(c"glBegin"),
            end: f!(c"glEnd"),
            tex_coord: f!(c"glTexCoord2f"),
            vertex: f!(c"glVertex2f"),
            color: f!(c"glColor4f"),
            tex_env: f!(c"glTexEnvi"),
        })
    })
    .as_ref()
}

/// Detours `name` in `m` with `detour` (once).
fn arm<T>(slot: &'static OnceLock<GenericDetour<T>>, m: windows_sys::Win32::Foundation::HMODULE, name: &std::ffi::CStr, detour: T) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    let Some(addr) = com::proc(m, name) else { return false };
    let target: T = unsafe { T::from_ptr(addr as *const ()) };
    match unsafe { GenericDetour::new(target, detour) } {
        Ok(h) => slot.set(h).is_ok() && crate::sys::hook::frozen(addr, || unsafe { slot.get().unwrap().enable() }).is_ok(),
        Err(e) => {
            log!("present: {} not hooked: {e}", name.to_string_lossy());
            false
        }
    }
}

pub fn install() -> bool {
    let m = com::module("opengl32.dll");
    if com::proc(m, c"wglSwapBuffers").is_none() || gl().is_none() {
        return false;
    }
    with_lock(|| {
        if !arm(&SWAP, m, c"wglSwapBuffers", swap_hook as Swap) {
            return false;
        }
        let layer = arm(&SWAP_LAYER, m, c"wglSwapLayerBuffers", swap_layer_hook as SwapLayer);
        let gdi = arm(&GDI_SWAP, com::module("gdi32.dll"), c"SwapBuffers", gdi_swap_hook as Swap);
        log!("present: OpenGL's swaps hooked (wglSwapBuffers{}{})", if layer { ", wglSwapLayerBuffers" } else { "" }, if gdi { ", gdi32 SwapBuffers" } else { "" });
        true
    })
}

const TEXTURE_2D: u32 = 0x0DE1;
const BGRA: u32 = 0x80E1;
const UNSIGNED_BYTE: u32 = 0x1401;

fn is_window(hwnd: usize) -> bool {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(hwnd as _) != 0 }
}

/// Lets go of `c`: its GL objects are freed with it current on its device context when
/// that still works (its window alive), else left to go with the context; then the
/// context goes. The calling thread's context is put back.
fn retire(g: &Gl, mut c: Ctx) {
    let (old_dc, old_ctx) = unsafe { ((g.get_dc)(), (g.get_context)()) };
    let current = (c.hwnd == 0 || is_window(c.hwnd)) && unsafe { (g.make_current)(c.dc, c.ctx) } != 0;
    if current {
        if c.tex != 0 {
            unsafe { (g.delete_textures)(1, &c.tex) };
        }
        c.capture = None;
        c.readback = None;
        unsafe { (g.make_current)(old_dc, old_ctx) };
    } else {
        // No GL call may reach it now: with it not current they'd reach the game's.
        if let Some(Some(cap)) = c.capture.take() {
            cap.abandon();
        }
        if let Some(Some(r)) = c.readback.take() {
            r.abandon();
        }
    }
    unsafe { (g.delete_context)(c.ctx) };
}

/// The contexts of windows that are gone go.
fn evict_gone(g: &Gl, all: &mut Vec<Ctx>) {
    let mut i = 0;
    while i < all.len() {
        if all[i].hwnd != 0 && !is_window(all[i].hwnd) {
            let c = all.remove(i);
            log!("video: an OpenGL window (0x{:x}) is gone; PeerParty's context on it goes too", c.hwnd);
            retire(g, c);
        } else {
            i += 1;
        }
    }
}

/// Our context for `hwnd` (or, without a window, for `dc`), made the first time, moved to
/// the back as the most recently used, with `dc` as the one it's made current on.
fn own<'a>(g: &Gl, all: &'a mut Vec<Ctx>, dc: usize, hwnd: usize) -> Option<&'a mut Ctx> {
    let found = all.iter().position(|c| if hwnd != 0 { c.hwnd == hwnd } else { c.hwnd == 0 && c.dc == dc });
    let c = match found {
        Some(i) => {
            let mut c = all.remove(i);
            c.dc = dc;
            c
        }
        None => {
            if all.len() >= MAX_CTX {
                retire(g, all.remove(0));
            }
            let ctx = unsafe { (g.create_context)(dc) };
            if ctx == 0 {
                crate::log::throttled("video: gl context", 2, |n| log!("video: no OpenGL context of PeerParty's could be made on the game's device context (call {n})"));
                return None;
            }
            Ctx { dc, hwnd, ctx, tex: 0, size: (0, 0), seq: None, capture: None, readback: None }
        }
    };
    all.push(c);
    all.last_mut()
}

/// At each swap of `dc`: the viewport reported, then, in PeerParty's context made current
/// once, the overlay drawn and the frame copied for the session's window. Returns false
/// when a lower translation layer (Zink/Vulkan) handles the swap instead.
fn on_swap(dc: usize) -> bool {
    let size = client_size(dc);
    let hwnd = size.map_or(0, |s| s.2);
    if let Some((w, h, hwnd)) = size {
        super::report_viewport(w, h, hwnd, "opengl");
    }
    if super::lower_layer_presents(|| hwnd) {
        return false;
    }
    super::presented();
    swap_turn(dc, size, frame(), super::capture::enabled());
    true
}

/// In PeerParty's context on `dc` (its window's client area `size`): the overlay `f` drawn,
/// then the frame copied when `cap`.
fn swap_turn(dc: usize, size: Option<(u32, u32, usize)>, f: Option<super::Frame>, cap: bool) {
    let hwnd = size.map_or(0, |s| s.2);
    if f.is_none() && !cap {
        return;
    }
    let Some(g) = gl() else { return };
    let mut all = crate::sys::lock(&CTX);
    evict_gone(g, &mut all);
    let (old_dc, old_ctx) = unsafe { ((g.get_dc)(), (g.get_context)()) };
    let mut current = false;
    for attempt in 0..2 {
        let Some(c) = own(g, &mut all, dc, hwnd) else { return };
        if unsafe { (g.make_current)(dc, c.ctx) } != 0 {
            current = true;
            break;
        }
        // Not on this device context (a window made where a gone one was, with another
        // pixel format): a new one, once.
        let c = all.pop().unwrap();
        retire(g, c);
        if attempt == 1 {
            crate::log::throttled("video: gl make current", 2, |n| log!("video: PeerParty's OpenGL context couldn't be made current on the game's device context (call {n})"));
        }
    }
    if !current {
        return;
    }
    let c = all.last_mut().unwrap();
    if let Some(f) = f {
        let (w, h) = size.map_or((f.w, f.h), |s| (s.0, s.1));
        draw(g, c, &f, w, h);
    }
    if cap && let Some((w, h, _)) = size {
        copy(c, w, h);
    }
    unsafe { (g.make_current)(old_dc, old_ctx) };
}

/// Copies the frame for the session's window: into shared textures, or read back in pixel
/// mode or where the driver can't share them (and from then on, if they fail).
fn copy(c: &mut Ctx, w: u32, h: u32) {
    if !super::capture::pixels() {
        if let Some(cap) = c.capture.get_or_insert_with(glcapture::GlCapture::new) {
            if cap.capture(w, h) {
                return;
            }
            log!("video: OpenGL frames can't go through shared textures here; read back instead");
            c.capture = Some(None);
        } else if c.readback.is_none() {
            log!("video: OpenGL frames read back for the session (no shared textures)");
        }
    }
    if let Some(r) = c.readback.get_or_insert_with(glcapture::GlReadback::new) {
        r.capture(w, h);
    }
}

/// Draws the overlay `f` over the back buffer (`w x h`), our context current.
fn draw(g: &Gl, c: &mut Ctx, f: &super::Frame, w: u32, h: u32) {
    unsafe {
        if c.tex == 0 {
            (g.gen_textures)(1, &mut c.tex);
        }
        (g.bind_texture)(TEXTURE_2D, c.tex);
        (g.pixel_storei)(0x0CF5, 4); // UNPACK_ALIGNMENT
        (g.pixel_storei)(0x0CF2, 0); // UNPACK_ROW_LENGTH
        if c.size != (f.w, f.h) {
            (g.tex_parameteri)(TEXTURE_2D, 0x2801, 0x2600); // MIN_FILTER NEAREST
            (g.tex_parameteri)(TEXTURE_2D, 0x2800, 0x2600); // MAG_FILTER NEAREST
            (g.tex_image)(TEXTURE_2D, 0, 0x8058, f.w as i32, f.h as i32, 0, BGRA, UNSIGNED_BYTE, std::ptr::null());
            c.size = (f.w, f.h);
            c.seq = None;
        }
        if c.seq != Some(f.seq)
            && let Some(px) = f.stable()
        {
            (g.tex_sub_image)(TEXTURE_2D, 0, 0, 0, f.w as i32, f.h as i32, BGRA, UNSIGNED_BYTE, px.as_ptr() as *const u8);
            c.seq = Some(f.seq);
        }
        if c.seq.is_some() {
            (g.viewport)(0, 0, w as i32, h as i32);
            (g.matrix_mode)(0x1701);
            (g.load_identity)();
            (g.ortho)(0.0, 1.0, 1.0, 0.0, -1.0, 1.0);
            (g.matrix_mode)(0x1700);
            (g.load_identity)();
            for cap in [0x0B71u32, 0x0B44, 0x0C11, 0x0B50, 0x0BC0] {
                (g.disable)(cap); // DEPTH_TEST, CULL_FACE, SCISSOR_TEST, LIGHTING, ALPHA_TEST
            }
            (g.enable)(TEXTURE_2D);
            (g.enable)(0x0BE2); // BLEND
            (g.blend_func)(1, 0x0303); // ONE, ONE_MINUS_SRC_ALPHA
            (g.tex_env)(0x2300, 0x2200, 0x1E01); // TEXTURE_ENV_MODE REPLACE
            (g.color)(1.0, 1.0, 1.0, 1.0);
            (g.begin)(7); // QUADS
            for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
                (g.tex_coord)(u, v);
                (g.vertex)(u, v);
            }
            (g.end)();
            // What the copy reads next is the default framebuffer's texture state, not
            // this one's.
            (g.disable)(TEXTURE_2D);
            (g.disable)(0x0BE2);
            (g.bind_texture)(TEXTURE_2D, 0);
            super::drew("OpenGL");
        }
    }
}

fn client_size(dc: usize) -> Option<(u32, u32, usize)> {
    use windows_sys::Win32::Graphics::Gdi::WindowFromDC;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    let wnd = unsafe { WindowFromDC(dc as _) };
    if wnd.is_null() {
        return None;
    }
    let mut r = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(wnd, &mut r) };
    Some(((r.right - r.left) as u32, (r.bottom - r.top) as u32, wnd as usize))
}

thread_local! {
    /// Inside one of the swap hooks: a swap that calls another inside is seen once.
    static IN_SWAP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn swapping(dc: usize, real: impl FnOnce() -> i32) -> i32 {
    let outer = !IN_SWAP.with(|s| s.replace(true));
    let handled = outer && std::panic::catch_unwind(|| on_swap(dc)).unwrap_or(false);
    let r = real();
    if outer {
        IN_SWAP.with(|s| s.set(false));
        if handled {
            super::capture::pace();
        }
    }
    r
}

unsafe extern "system" fn swap_hook(dc: usize) -> i32 {
    swapping(dc, || unsafe { SWAP.get().unwrap().call(dc) })
}

/// WGL_SWAP_MAIN_PLANE: the swap that shows the frame.
const MAIN_PLANE: u32 = 1;

unsafe extern "system" fn swap_layer_hook(dc: usize, planes: u32) -> i32 {
    let real = || unsafe { SWAP_LAYER.get().unwrap().call(dc, planes) };
    if planes & MAIN_PLANE == 0 { real() } else { swapping(dc, real) }
}

unsafe extern "system" fn gdi_swap_hook(dc: usize) -> i32 {
    swapping(dc, || unsafe { GDI_SWAP.get().unwrap().call(dc) })
}
