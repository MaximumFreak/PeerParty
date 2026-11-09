//! OpenGL games: `wglSwapBuffers` (what gdi32's SwapBuffers calls too). The overlay is
//! drawn with an OpenGL context of PeerParty's own, made on the game's device context
//! (so it shares its pixel format and default framebuffer): the game's context is set
//! aside for the draw and put back after, and none of its GL state is touched. The
//! drawing is OpenGL 1.1 (immediate mode, a BGRA texture, premultiplied blending), all
//! from opengl32's own exports.

use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::com;
use super::frame;
use crate::hook::with_lock;
use crate::log;

type Swap = unsafe extern "system" fn(usize) -> i32;
static SWAP: OnceLock<GenericDetour<Swap>> = OnceLock::new();

struct Gl {
    make_current: unsafe extern "system" fn(usize, usize) -> i32,
    create_context: unsafe extern "system" fn(usize) -> usize,
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

/// Our context per game device context, the texture in it, its size and frame.
struct Ctx {
    dc: usize,
    ctx: usize,
    tex: u32,
    size: (u32, u32),
    seq: Option<u32>,
}

static CTX: Mutex<Vec<Ctx>> = Mutex::new(Vec::new());

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

pub fn install() -> bool {
    let m = com::module("opengl32.dll");
    let Some(addr) = com::proc(m, c"wglSwapBuffers") else { return false };
    if gl().is_none() {
        return false;
    }
    with_lock(|| {
        let target: Swap = unsafe { std::mem::transmute(addr) };
        match unsafe { GenericDetour::new(target, swap_hook as Swap) } {
            Ok(h) => {
                let _ = SWAP.set(h);
                crate::hook::frozen(addr, || unsafe { SWAP.get().unwrap().enable() }).is_ok()
            }
            Err(e) => {
                log!("present: wglSwapBuffers not hooked: {e}");
                false
            }
        }
    })
}

const TEXTURE_2D: u32 = 0x0DE1;
const BGRA: u32 = 0x80E1;
const UNSIGNED_BYTE: u32 = 0x1401;

fn draw(dc: usize) {
    let Some(f) = frame() else { return };
    let Some(g) = gl() else { return };
    let mut all = CTX.lock().unwrap();
    if !all.iter().any(|c| c.dc == dc) {
        let ctx = unsafe { (g.create_context)(dc) };
        if ctx == 0 {
            return;
        }
        all.push(Ctx { dc, ctx, tex: 0, size: (0, 0), seq: None });
    }
    let c = all.iter_mut().find(|c| c.dc == dc).unwrap();
    let (old_dc, old_ctx) = unsafe { ((g.get_dc)(), (g.get_context)()) };
    if unsafe { (g.make_current)(dc, c.ctx) } == 0 {
        return;
    }
    let (w, h) = client_size(dc).unwrap_or((f.w, f.h));
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
        if c.seq != Some(f.seq) && !f.writing() {
            (g.tex_sub_image)(TEXTURE_2D, 0, 0, 0, f.w as i32, f.h as i32, BGRA, UNSIGNED_BYTE, f.px as *const u8);
            if frame().is_some_and(|now| now.seq == f.seq) {
                c.seq = Some(f.seq);
            }
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
            super::drew("OpenGL");
        }
        (g.make_current)(old_dc, old_ctx);
    }
}

fn client_size(dc: usize) -> Option<(u32, u32)> {
    use windows_sys::Win32::Graphics::Gdi::WindowFromDC;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    let wnd = unsafe { WindowFromDC(dc as _) };
    if wnd.is_null() {
        return None;
    }
    let mut r = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(wnd, &mut r) };
    Some(((r.right - r.left) as u32, (r.bottom - r.top) as u32))
}

unsafe extern "system" fn swap_hook(dc: usize) -> i32 {
    let _ = std::panic::catch_unwind(|| draw(dc));
    unsafe { SWAP.get().unwrap().call(dc) }
}
