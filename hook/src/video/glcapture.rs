//! An OpenGL game's frames for the session's window (capture.rs). The shared textures are
//! made on a Direct3D 11 device of PeerParty's own (`D3d11Ring`) and opened in OpenGL
//! through `WGL_NV_DX_interop2` (NVIDIA, AMD and Intel drivers have it); at each
//! `SwapBuffers`, PeerParty's own context on the game's device context (gl.rs) blits the
//! back buffer into a texture OpenGL shares with that device, flipped to Direct3D's
//! top-down rows, and the device copies it into a free slot. (Drivers won't open a texture
//! shared by NT handle in OpenGL, so the slots can't be blitted into directly.) Locking the
//! texture for OpenGL and unlocking it orders OpenGL's writes before the Direct3D copy.
//!
//! A multisampled back buffer can't be resolved by a blit that flips it, so it's resolved
//! first, unflipped, into a texture of its own format, and flipped from there.
//!
//! When the session asks for pixels, or the driver can't share textures, the back buffer
//! is read back instead (`GlReadback`).

use std::ffi::CStr;

use super::super::capture::{D3d11Ring, Kind, Texture2dDesc, private_device};
use super::super::pixels::CpuFrames;
use super::com::vcall;
use crate::log;

const TEXTURE_2D: u32 = 0x0DE1;
const READ_FRAMEBUFFER: u32 = 0x8CA8;
const DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const COLOR_ATTACHMENT0: u32 = 0x8CE0;
const BACK: u32 = 0x0405;
const COLOR_BUFFER_BIT: u32 = 0x4000;
const NEAREST: u32 = 0x2600;
const WGL_ACCESS_WRITE_DISCARD_NV: u32 = 0x0002;
const DEVICE_LUID_EXT: u32 = 0x9599;
const FRAMEBUFFER: u32 = 0x8D40;
const SAMPLE_BUFFERS: u32 = 0x80A8;
const RED_BITS: u32 = 0x0D52;
const ALPHA_BITS: u32 = 0x0D55;
const BACK_LEFT: u32 = 0x0402;
const COLOR_ENCODING: u32 = 0x8210;
const SRGB: i32 = 0x8C40;

struct Fns {
    open_device: unsafe extern "system" fn(usize) -> usize,
    close_device: unsafe extern "system" fn(usize) -> i32,
    register: unsafe extern "system" fn(usize, usize, u32, u32, u32) -> usize,
    unregister: unsafe extern "system" fn(usize, usize) -> i32,
    lock: unsafe extern "system" fn(usize, i32, *const usize) -> i32,
    unlock: unsafe extern "system" fn(usize, i32, *const usize) -> i32,
    gen_framebuffers: unsafe extern "system" fn(i32, *mut u32),
    bind_framebuffer: unsafe extern "system" fn(u32, u32),
    framebuffer_texture: unsafe extern "system" fn(u32, u32, u32, u32, i32),
    blit: unsafe extern "system" fn(i32, i32, i32, i32, i32, i32, i32, i32, u32, u32),
    delete_framebuffers: unsafe extern "system" fn(i32, *const u32),
    gen_textures: unsafe extern "system" fn(i32, *mut u32),
    delete_textures: unsafe extern "system" fn(i32, *const u32),
    bind_texture: unsafe extern "system" fn(u32, u32),
    tex_parameteri: unsafe extern "system" fn(u32, u32, i32),
    tex_image: unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const u8),
    read_buffer: unsafe extern "system" fn(u32),
    get_integer: unsafe extern "system" fn(u32, *mut i32),
    get_error: unsafe extern "system" fn() -> u32,
    attachment: Option<unsafe extern "system" fn(u32, u32, u32, *mut i32)>,
    luid: Option<unsafe extern "system" fn(u32, *mut u8)>,
}

fn fns() -> Option<Fns> {
    let m = super::com::module("opengl32.dll");
    type GetProc = unsafe extern "system" fn(*const u8) -> usize;
    let get: GetProc = unsafe { std::mem::transmute(super::com::proc(m, c"wglGetProcAddress")?) };
    let ext = |n: &CStr| {
        let p = unsafe { get(n.as_ptr().cast()) };
        // wglGetProcAddress answers 1, 2, 3 or -1 for some it doesn't have.
        (p > 3 && p != usize::MAX).then_some(p)
    };
    macro_rules! e {
        ($n:literal) => {
            unsafe { std::mem::transmute(ext($n)?) }
        };
    }
    macro_rules! x {
        ($n:literal) => {
            unsafe { std::mem::transmute(super::com::proc(m, $n)?) }
        };
    }
    Some(Fns {
        open_device: e!(c"wglDXOpenDeviceNV"),
        close_device: e!(c"wglDXCloseDeviceNV"),
        register: e!(c"wglDXRegisterObjectNV"),
        unregister: e!(c"wglDXUnregisterObjectNV"),
        lock: e!(c"wglDXLockObjectsNV"),
        unlock: e!(c"wglDXUnlockObjectsNV"),
        gen_framebuffers: e!(c"glGenFramebuffers"),
        bind_framebuffer: e!(c"glBindFramebuffer"),
        framebuffer_texture: e!(c"glFramebufferTexture2D"),
        blit: e!(c"glBlitFramebuffer"),
        delete_framebuffers: e!(c"glDeleteFramebuffers"),
        gen_textures: x!(c"glGenTextures"),
        delete_textures: x!(c"glDeleteTextures"),
        bind_texture: x!(c"glBindTexture"),
        tex_parameteri: x!(c"glTexParameteri"),
        tex_image: x!(c"glTexImage2D"),
        read_buffer: x!(c"glReadBuffer"),
        get_integer: x!(c"glGetIntegerv"),
        get_error: x!(c"glGetError"),
        attachment: ext(c"glGetFramebufferAttachmentParameteriv").map(|p| unsafe { std::mem::transmute(p) }),
        luid: ext(c"glGetUnsignedBytevEXT").map(|p| unsafe { std::mem::transmute(p) }),
    })
}

/// One device context's capture, inside PeerParty's own context on it.
pub struct GlCapture {
    fns: Fns,
    /// PeerParty's Direct3D 11 device (and context), and its interop handle.
    private: (usize, usize),
    interop: usize,
    ring: Option<D3d11Ring>,
    /// The texture OpenGL blits into: Direct3D's, OpenGL's name for it, the interop object.
    shared: Option<(usize, u32, usize)>,
    fbo: u32,
    /// The size and the session's setup (`capture::epoch`) the ring is for.
    made_for: (u32, u32, u64),
    /// A multisampled back buffer's resolve: the texture (of its format) and its
    /// framebuffer.
    resolve: Option<(u32, u32)>,
    /// Its context is gone or can't be made current: no GL call is made when it's dropped.
    gl_gone: bool,
}

unsafe impl Send for GlCapture {}

impl GlCapture {
    /// Made with PeerParty's context current. None when the driver can't share textures.
    pub fn new() -> Option<GlCapture> {
        let Some(fns) = fns() else {
            log!("video: this OpenGL driver can't share textures with Direct3D; the session shows the copy's window instead");
            return None;
        };
        let luid = fns.luid.map(|f| {
            let mut b = [0u8; 8];
            unsafe { f(DEVICE_LUID_EXT, b.as_mut_ptr()) };
            (u32::from_le_bytes(b[0..4].try_into().unwrap()), i32::from_le_bytes(b[4..8].try_into().unwrap()))
        });
        let private = private_device(luid.filter(|l| *l != (0, 0)))?;
        let interop = unsafe { (fns.open_device)(private.0) };
        if interop == 0 {
            log!("video: OpenGL couldn't open the Direct3D 11 device for sharing");
            super::com::release(private.1);
            super::com::release(private.0);
            return None;
        }
        let mut fbo = 0u32;
        unsafe { (fns.gen_framebuffers)(1, &mut fbo) };
        Some(GlCapture { fns, private, interop, ring: None, shared: None, fbo, made_for: (0, 0, 0), resolve: None, gl_gone: false })
    }

    /// Lets go of it without a GL call (its context is gone, or isn't current and can't
    /// be made so): what it made in OpenGL goes with the context; its Direct3D objects go.
    pub fn abandon(mut self) {
        self.gl_gone = true;
    }

    fn drop_ring(&mut self) {
        if let Some((tex11, name, obj)) = self.shared.take() {
            if !self.gl_gone {
                unsafe {
                    (self.fns.unregister)(self.interop, obj);
                    (self.fns.delete_textures)(1, &name);
                }
            }
            super::com::release(tex11);
        }
        if let Some((tex, fbo)) = self.resolve.take()
            && !self.gl_gone
        {
            unsafe {
                (self.fns.delete_framebuffers)(1, &fbo);
                (self.fns.delete_textures)(1, &tex);
            }
        }
        self.ring = None;
    }

    /// The default framebuffer's colour format, for a texture a resolving blit into keeps
    /// it as it is (`GL_RGBA8`, `GL_SRGB8_ALPHA8`, `GL_RGB10_A2`, `GL_RGBA16F`, or the
    /// alpha-less ones). None when it isn't multisampled. Our context is current.
    fn multisampled_format(&self) -> Option<i32> {
        let f = &self.fns;
        let get = |what| {
            let mut v = 0i32;
            unsafe { (f.get_integer)(what, &mut v) };
            v
        };
        unsafe { (f.bind_framebuffer)(FRAMEBUFFER, 0) };
        if get(SAMPLE_BUFFERS) <= 0 {
            return None;
        }
        let (red, alpha) = (get(RED_BITS), get(ALPHA_BITS));
        let mut encoding = 0i32;
        if let Some(a) = f.attachment {
            unsafe { a(FRAMEBUFFER, BACK_LEFT, COLOR_ENCODING, &mut encoding) };
        }
        Some(match (red, alpha > 0, encoding == SRGB) {
            (16, _, _) => 0x881A,    // RGBA16F
            (10, _, _) => 0x8059,    // RGB10_A2
            (_, true, true) => 0x8C43, // SRGB8_ALPHA8
            (_, false, true) => 0x8C41, // SRGB8
            (_, true, false) => 0x8058, // RGBA8
            (_, false, false) => 0x8051, // RGB8
        })
    }

    fn ensure(&mut self, w: u32, h: u32) -> bool {
        let want = (w, h, super::super::capture::epoch());
        if self.made_for == want {
            return self.ring.is_some();
        }
        self.drop_ring();
        self.made_for = want;
        if let Some(format) = self.multisampled_format() {
            let f = &self.fns;
            let (mut tex, mut fbo) = (0u32, 0u32);
            unsafe {
                (f.gen_textures)(1, &mut tex);
                (f.bind_texture)(TEXTURE_2D, tex);
                (f.tex_parameteri)(TEXTURE_2D, 0x2801, NEAREST as i32);
                (f.tex_parameteri)(TEXTURE_2D, 0x2800, NEAREST as i32);
                (f.tex_image)(TEXTURE_2D, 0, format, w as i32, h as i32, 0, 0x1908, 0x1401, std::ptr::null()); // RGBA, UNSIGNED_BYTE
                (f.bind_texture)(TEXTURE_2D, 0);
                (f.gen_framebuffers)(1, &mut fbo);
                (f.bind_framebuffer)(DRAW_FRAMEBUFFER, fbo);
                (f.framebuffer_texture)(DRAW_FRAMEBUFFER, COLOR_ATTACHMENT0, TEXTURE_2D, tex, 0);
                (f.bind_framebuffer)(DRAW_FRAMEBUFFER, 0);
            }
            self.resolve = Some((tex, fbo));
            log!("video: the OpenGL back buffer is multisampled: resolved (format {format:#x}) before it's copied");
        }
        let Some(ring) = D3d11Ring::new(self.private.0, w, h, 87, 0x08 | 0x20) else { return false };
        let desc = Texture2dDesc { width: w, height: h, mip_levels: 1, array_size: 1, format: 87, sample_count: 1, sample_quality: 0, usage: 0, bind_flags: 0x08 | 0x20, cpu_access: 0, misc: 0 };
        let mut tex11 = 0usize;
        // ID3D11Device::CreateTexture2D is slot 5.
        if vcall!(self.private.0, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut tex11) < 0 {
            return false;
        }
        let mut name = 0u32;
        unsafe { (self.fns.gen_textures)(1, &mut name) };
        let obj = unsafe { (self.fns.register)(self.interop, tex11, name, TEXTURE_2D, WGL_ACCESS_WRITE_DISCARD_NV) };
        if obj == 0 {
            unsafe { (self.fns.delete_textures)(1, &name) };
            super::com::release(tex11);
            log!("video: OpenGL couldn't open a texture of the Direct3D 11 device");
            return false;
        }
        self.shared = Some((tex11, name, obj));
        ring.report("opengl");
        self.ring = Some(ring);
        true
    }

    /// Copies the back buffer (`w x h`) into a free slot (none free: this frame isn't
    /// copied). PeerParty's context on the game's device context is current. False when
    /// its frames can't be had this way (the textures or the resolve failed): read back
    /// instead.
    pub fn capture(&mut self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 {
            return true;
        }
        if !self.ensure(w, h) {
            return false;
        }
        let Some(slot) = self.ring.as_ref().and_then(|r| r.pick()) else { return true };
        let Some((tex11, tex, obj)) = self.shared else { return false };
        let f = &self.fns;
        let (wi, hi) = (w as i32, h as i32);
        unsafe {
            if (f.lock)(self.interop, 1, &obj) == 0 {
                crate::log::throttled("video: gl interop lock", 2, |n| log!("video: the shared texture couldn't be locked for OpenGL (call {n}); the frame isn't copied"));
                return true;
            }
            (f.bind_framebuffer)(READ_FRAMEBUFFER, 0);
            (f.read_buffer)(BACK);
            if let Some((_, resolve)) = self.resolve {
                while (f.get_error)() != 0 {}
                (f.bind_framebuffer)(DRAW_FRAMEBUFFER, resolve);
                (f.blit)(0, 0, wi, hi, 0, 0, wi, hi, COLOR_BUFFER_BIT, NEAREST);
                let e = (f.get_error)();
                if e != 0 {
                    (f.bind_framebuffer)(DRAW_FRAMEBUFFER, 0);
                    (f.unlock)(self.interop, 1, &obj);
                    log!("video: the multisampled OpenGL back buffer couldn't be resolved (error {e:#x})");
                    return false;
                }
                (f.bind_framebuffer)(READ_FRAMEBUFFER, resolve);
            }
            (f.bind_framebuffer)(DRAW_FRAMEBUFFER, self.fbo);
            (f.framebuffer_texture)(DRAW_FRAMEBUFFER, COLOR_ATTACHMENT0, TEXTURE_2D, tex, 0);
            (f.blit)(0, 0, wi, hi, 0, hi, wi, 0, COLOR_BUFFER_BIT, NEAREST);
            (f.framebuffer_texture)(DRAW_FRAMEBUFFER, COLOR_ATTACHMENT0, TEXTURE_2D, 0, 0);
            (f.bind_framebuffer)(DRAW_FRAMEBUFFER, 0);
            (f.bind_framebuffer)(READ_FRAMEBUFFER, 0);
            (f.unlock)(self.interop, 1, &obj);
        }
        if let Some(r) = self.ring.as_mut() {
            // ID3D11DeviceContext::CopyResource is slot 47.
            vcall!(r.ctx, 47, fn(usize, usize) -> (), r.textures[slot], tex11);
            r.finish(slot);
        }
        true
    }
}

impl Drop for GlCapture {
    fn drop(&mut self) {
        self.drop_ring();
        if !self.gl_gone {
            unsafe {
                (self.fns.delete_framebuffers)(1, &self.fbo);
                (self.fns.close_device)(self.interop);
            }
        }
        super::com::release(self.private.1);
        super::com::release(self.private.0);
    }
}

// --- Pixel mode -------------------------------------------------------------------------

const PIXEL_PACK_BUFFER: u32 = 0x88EB;
const STREAM_READ: u32 = 0x88E1;
const MAP_READ_BIT: u32 = 0x0001;
const SYNC_GPU_COMMANDS_COMPLETE: u32 = 0x9117;
const ALREADY_SIGNALED: u32 = 0x911A;
const CONDITION_SATISFIED: u32 = 0x911C;
const WAIT_FAILED: u32 = 0x911D;
const GL_BGRA: u32 = 0x80E1;
const GL_UNSIGNED_BYTE: u32 = 0x1401;

struct ReadFns {
    gen_buffers: unsafe extern "system" fn(i32, *mut u32),
    delete_buffers: unsafe extern "system" fn(i32, *const u32),
    bind_buffer: unsafe extern "system" fn(u32, u32),
    buffer_data: unsafe extern "system" fn(u32, isize, usize, u32),
    map_range: unsafe extern "system" fn(u32, isize, isize, u32) -> *const u8,
    unmap: unsafe extern "system" fn(u32) -> u8,
    fence: unsafe extern "system" fn(u32, u32) -> usize,
    client_wait: unsafe extern "system" fn(usize, u32, u64) -> u32,
    delete_sync: unsafe extern "system" fn(usize),
    bind_framebuffer: unsafe extern "system" fn(u32, u32),
    read_pixels: unsafe extern "system" fn(i32, i32, i32, i32, u32, u32, usize),
    read_buffer: unsafe extern "system" fn(u32),
    flush: unsafe extern "system" fn(),
}

fn read_fns() -> Option<ReadFns> {
    let m = super::com::module("opengl32.dll");
    type GetProc = unsafe extern "system" fn(*const u8) -> usize;
    let get: GetProc = unsafe { std::mem::transmute(super::com::proc(m, c"wglGetProcAddress")?) };
    let ext = |n: &CStr| {
        let p = unsafe { get(n.as_ptr().cast()) };
        (p > 3 && p != usize::MAX).then_some(p)
    };
    macro_rules! e {
        ($n:literal) => {
            unsafe { std::mem::transmute(ext($n)?) }
        };
    }
    macro_rules! x {
        ($n:literal) => {
            unsafe { std::mem::transmute(super::com::proc(m, $n)?) }
        };
    }
    Some(ReadFns {
        gen_buffers: e!(c"glGenBuffers"),
        delete_buffers: e!(c"glDeleteBuffers"),
        bind_buffer: e!(c"glBindBuffer"),
        buffer_data: e!(c"glBufferData"),
        map_range: e!(c"glMapBufferRange"),
        unmap: e!(c"glUnmapBuffer"),
        fence: e!(c"glFenceSync"),
        client_wait: e!(c"glClientWaitSync"),
        delete_sync: e!(c"glDeleteSync"),
        bind_framebuffer: e!(c"glBindFramebuffer"),
        read_pixels: x!(c"glReadPixels"),
        read_buffer: x!(c"glReadBuffer"),
        flush: x!(c"glFlush"),
    })
}

/// Pixel mode (`capture::pixels`): each `SwapBuffers`' back buffer read into one of two
/// pixel-pack buffers without waiting, and handed to `CpuFrames` once its fence says the
/// copy is done, turned top row first. A frame is skipped while both are in flight.
pub struct GlReadback {
    fns: ReadFns,
    /// Per buffer: its name, its fence (0: free), the order it was read in.
    buffers: [(u32, usize, u64); 2],
    next: u64,
    size: (u32, u32),
    rows: Vec<u8>,
    frames: CpuFrames,
    /// Its context is gone or can't be made current: no GL call is made when it's dropped.
    gl_gone: bool,
}

unsafe impl Send for GlReadback {}

impl GlReadback {
    /// Made with PeerParty's context current. None without buffer objects and fences
    /// (OpenGL 3.2).
    pub fn new() -> Option<GlReadback> {
        let Some(fns) = read_fns() else {
            log!("video: this OpenGL driver can't read frames back without waiting; no frames for the session");
            return None;
        };
        let mut names = [0u32; 2];
        unsafe { (fns.gen_buffers)(2, names.as_mut_ptr()) };
        Some(GlReadback { fns, buffers: [(names[0], 0, 0), (names[1], 0, 0)], next: 0, size: (0, 0), rows: Vec::new(), frames: CpuFrames::new(None, "opengl"), gl_gone: false })
    }

    /// Lets go of it without a GL call (`GlCapture::abandon`).
    pub fn abandon(mut self) {
        self.gl_gone = true;
    }

    fn kind(&self) -> Kind {
        Kind { width: self.size.0, height: self.size.1, format: 87, color_space: 0 }
    }

    /// Hands over the reads the GPU finished, oldest first.
    fn collect(&mut self) {
        let f = &self.fns;
        let mut order: Vec<usize> = (0..2).filter(|&i| self.buffers[i].1 != 0).collect();
        order.sort_by_key(|&i| self.buffers[i].2);
        let (w, h) = self.size;
        let row = w as usize * 4;
        for i in order {
            let (name, sync, _) = self.buffers[i];
            let r = unsafe { (f.client_wait)(sync, 0, 0) };
            if r == WAIT_FAILED {
                // Its read will never be known finished: the buffer is free again.
                unsafe { (f.delete_sync)(sync) };
                self.buffers[i].1 = 0;
                crate::log::throttled("video: gl wait failed", 2, |n| log!("video: an OpenGL fence for a frame read back failed (call {n}); that frame is dropped"));
                continue;
            }
            if r != ALREADY_SIGNALED && r != CONDITION_SATISFIED {
                break;
            }
            unsafe {
                (f.delete_sync)(sync);
                (f.bind_buffer)(PIXEL_PACK_BUFFER, name);
                let p = (f.map_range)(PIXEL_PACK_BUFFER, 0, (row * h as usize) as isize, MAP_READ_BIT);
                if !p.is_null() {
                    // OpenGL's rows are bottom-up.
                    self.rows.resize(row * h as usize, 0);
                    for y in 0..h as usize {
                        std::ptr::copy_nonoverlapping(p.add((h as usize - 1 - y) * row), self.rows.as_mut_ptr().add(y * row), row);
                    }
                    (f.unmap)(PIXEL_PACK_BUFFER);
                }
                (f.bind_buffer)(PIXEL_PACK_BUFFER, 0);
                if !p.is_null() {
                    let kind = self.kind();
                    self.frames.deliver(kind, self.rows.as_ptr(), row);
                }
            }
            self.buffers[i].1 = 0;
        }
    }

    /// Reads the back buffer (`w x h`) into a free buffer. PeerParty's context on the
    /// game's device context is current.
    pub fn capture(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        if self.size != (w, h) {
            // Reads of the old size are let go.
            for b in &mut self.buffers {
                if b.1 != 0 {
                    unsafe { (self.fns.delete_sync)(b.1) };
                    b.1 = 0;
                }
            }
            self.size = (w, h);
            for b in &self.buffers {
                unsafe {
                    (self.fns.bind_buffer)(PIXEL_PACK_BUFFER, b.0);
                    (self.fns.buffer_data)(PIXEL_PACK_BUFFER, (w as usize * h as usize * 4) as isize, 0, STREAM_READ);
                }
            }
            unsafe { (self.fns.bind_buffer)(PIXEL_PACK_BUFFER, 0) };
        }
        self.collect();
        let Some(i) = (0..2).find(|&i| self.buffers[i].1 == 0) else { return };
        let kind = self.kind();
        if !self.frames.wants(kind) {
            return;
        }
        let f = &self.fns;
        unsafe {
            (f.bind_framebuffer)(READ_FRAMEBUFFER, 0);
            (f.read_buffer)(BACK);
            (f.bind_buffer)(PIXEL_PACK_BUFFER, self.buffers[i].0);
            (f.read_pixels)(0, 0, w as i32, h as i32, GL_BGRA, GL_UNSIGNED_BYTE, 0);
            (f.bind_buffer)(PIXEL_PACK_BUFFER, 0);
            let sync = (f.fence)(SYNC_GPU_COMMANDS_COMPLETE, 0);
            (f.flush)();
            self.next += 1;
            self.buffers[i] = (self.buffers[i].0, sync, self.next);
        }
    }
}

impl Drop for GlReadback {
    fn drop(&mut self) {
        if self.gl_gone {
            return;
        }
        for b in &self.buffers {
            unsafe {
                if b.1 != 0 {
                    (self.fns.delete_sync)(b.1);
                }
                (self.fns.delete_buffers)(1, &b.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::capture;
    use super::super::com;

    /// PIXELFORMATDESCRIPTOR.
    #[repr(C)]
    #[derive(Default)]
    struct Pfd {
        size: u16,
        version: u16,
        flags: u32,
        pixel_type: u8,
        color_bits: u8,
        rest: [u8; 22],
        layer_type: u8,
        reserved: u8,
        masks: [u32; 3],
    }

    /// A hidden window's OpenGL back buffer cleared magenta, copied as a `SwapBuffers`
    /// copies it (skipped where the driver can't share textures with Direct3D): it arrives
    /// in the published slot's Direct3D 11 texture, top row first.
    #[test]
    fn opengl_frames_reach_a_d3d11_device() {
        let _turn = crate::sys::lock(&capture::tests::SERIAL);
        let win = com::HiddenWindow::new().unwrap();
        let dc = unsafe { windows_sys::Win32::Graphics::Gdi::GetDC(win.0) } as usize;
        let gdi = com::load("gdi32.dll");
        type Choose = unsafe extern "system" fn(usize, *const Pfd) -> i32;
        type SetFormat = unsafe extern "system" fn(usize, i32, *const Pfd) -> i32;
        let choose: Choose = unsafe { std::mem::transmute(com::proc(gdi, c"ChoosePixelFormat").unwrap()) };
        let set: SetFormat = unsafe { std::mem::transmute(com::proc(gdi, c"SetPixelFormat").unwrap()) };
        let mut pfd = Pfd { size: std::mem::size_of::<Pfd>() as u16, version: 1, flags: 0x4 | 0x20 | 0x1, pixel_type: 0, color_bits: 32, ..Default::default() };
        pfd.rest[14] = 24; // depth bits
        let format = unsafe { choose(dc, &pfd) };
        assert!(format > 0 && unsafe { set(dc, format, &pfd) } != 0);
        let gl = com::load("opengl32.dll");
        type Create = unsafe extern "system" fn(usize) -> usize;
        type MakeCurrent = unsafe extern "system" fn(usize, usize) -> i32;
        let create: Create = unsafe { std::mem::transmute(com::proc(gl, c"wglCreateContext").unwrap()) };
        let make_current: MakeCurrent = unsafe { std::mem::transmute(com::proc(gl, c"wglMakeCurrent").unwrap()) };
        let game = unsafe { create(dc) };
        assert!(game != 0 && unsafe { make_current(dc, game) } != 0);
        if super::fns().is_none() {
            eprintln!("no WGL_NV_DX_interop2: skipped");
            unsafe { make_current(0, 0) };
            return;
        }
        type ClearColor = unsafe extern "system" fn(f32, f32, f32, f32);
        type Clear = unsafe extern "system" fn(u32);
        type Scissor = unsafe extern "system" fn(i32, i32, i32, i32);
        type Enable = unsafe extern "system" fn(u32);
        type Finish = unsafe extern "system" fn();
        let clear_color: ClearColor = unsafe { std::mem::transmute(com::proc(gl, c"glClearColor").unwrap()) };
        let clear: Clear = unsafe { std::mem::transmute(com::proc(gl, c"glClear").unwrap()) };
        let scissor: Scissor = unsafe { std::mem::transmute(com::proc(gl, c"glScissor").unwrap()) };
        let enable: Enable = unsafe { std::mem::transmute(com::proc(gl, c"glEnable").unwrap()) };
        let disable: Enable = unsafe { std::mem::transmute(com::proc(gl, c"glDisable").unwrap()) };
        let finish: Finish = unsafe { std::mem::transmute(com::proc(gl, c"glFinish").unwrap()) };
        let (w, h) = (64u32, 64u32);
        unsafe {
            clear_color(1.0, 0.0, 1.0, 1.0);
            clear(0x4000);
            // The top quarter (OpenGL's rows count from the bottom) blue.
            enable(0x0C11);
            scissor(0, 48, 64, 16);
            clear_color(0.0, 0.0, 1.0, 1.0);
            clear(0x4000);
            disable(0x0C11);
            finish();
        }
        let (_map, name) = capture::tests::session_ring();
        capture::configure(true, Some(&name), None);
        copy_only(dc);
        let p = capture::published().expect("published");
        let all = crate::sys::lock(&super::super::CTX);
        let cap = all.iter().find(|c| c.dc == dc).and_then(|c| c.capture.as_ref()).and_then(|c| c.as_ref()).expect("a capture");
        let ring = cap.ring.as_ref().unwrap();
        assert_eq!((ring.width, ring.height), (w, h), "the client area's size");
        let (other, octx) = capture::private_device(None).unwrap();
        let dev1 = com::query(other, &com::IID_ID3D11DEVICE1);
        let mut tex = 0usize;
        let hr = com::vcall!(dev1, 48, fn(windows_sys::Win32::Foundation::HANDLE, *const com::Guid, *mut usize) -> i32, ring.handle(p.slot), &com::IID_ID3D11TEXTURE2D, &mut tex);
        assert!(hr >= 0 && tex != 0, "OpenSharedResource1 {hr:#x}");
        let px = super::super::super::d3d11::read_pixels(other, octx, tex, w, h);
        assert_eq!(px[(w / 2) as usize] & 0x00ff_ffff, 0x0000ff, "the top row is the top: blue");
        assert_eq!(px[(h / 2 * w + w / 2) as usize] & 0x00ff_ffff, 0xff00ff, "magenta below");
        drop(all);
        capture::configure(false, None, None);

        // Asked for pixels: read back, a frame later, into the pixel file, top row first.
        let (ring, dir) = capture::tests::session_ring_file();
        capture::configure_as(true, Some(&ring), None, None, true);
        let start = std::time::Instant::now();
        let p = loop {
            copy_only(dc);
            if let Some(p) = capture::published() {
                break p;
            }
            assert!(start.elapsed().as_secs() < 5, "read back within 5 s");
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let bytes = capture::tests::pixel_slot(&dir.0, p.slot, peerparty_protocol::frame_ring::slot_bytes(w * 4, h).unwrap(), (w * h * 4) as usize);
        let at = |x: u32, y: u32| &bytes[((y * w + x) * 4) as usize..((y * w + x) * 4 + 3) as usize];
        assert_eq!(at(w / 2, 0), &[0xff, 0, 0], "the top row is the top: blue (BGRA)");
        assert_eq!(at(w / 2, h / 2), &[0xff, 0, 0xff], "magenta below");
        capture::configure(false, None, None);
        for o in [tex, dev1, octx, other] {
            com::release(o);
        }
        unsafe { make_current(0, 0) };
    }

    /// A swap's copy for the session, without the overlay (another test in this process
    /// may have one attached, which would be drawn into the frame first).
    fn copy_only(dc: usize) {
        super::super::swap_turn(dc, super::super::client_size(dc), None, true);
    }

    type MakeCurrent = unsafe extern "system" fn(usize, usize) -> i32;

    fn make_current() -> MakeCurrent {
        unsafe { std::mem::transmute(com::proc(com::load("opengl32.dll"), c"wglMakeCurrent").unwrap()) }
    }

    /// A hidden window with pixel format `format` (0: a plain double-buffered RGBA one),
    /// its device context, and a game context current on it.
    fn gl_window(format: i32) -> Option<(com::HiddenWindow, usize, usize)> {
        let win = com::HiddenWindow::new()?;
        let dc = unsafe { windows_sys::Win32::Graphics::Gdi::GetDC(win.0) } as usize;
        let gdi = com::load("gdi32.dll");
        type Choose = unsafe extern "system" fn(usize, *const Pfd) -> i32;
        type Describe = unsafe extern "system" fn(usize, i32, u32, *mut Pfd) -> i32;
        type SetFormat = unsafe extern "system" fn(usize, i32, *const Pfd) -> i32;
        let choose: Choose = unsafe { std::mem::transmute(com::proc(gdi, c"ChoosePixelFormat")?) };
        let describe: Describe = unsafe { std::mem::transmute(com::proc(gdi, c"DescribePixelFormat")?) };
        let set: SetFormat = unsafe { std::mem::transmute(com::proc(gdi, c"SetPixelFormat")?) };
        let mut pfd = Pfd { size: std::mem::size_of::<Pfd>() as u16, version: 1, flags: 0x4 | 0x20 | 0x1, pixel_type: 0, color_bits: 32, ..Default::default() };
        pfd.rest[14] = 24; // depth bits
        let format = if format != 0 { format } else { unsafe { choose(dc, &pfd) } };
        if format <= 0 {
            return None;
        }
        unsafe { describe(dc, format, std::mem::size_of::<Pfd>() as u32, &mut pfd) };
        if unsafe { set(dc, format, &pfd) } == 0 {
            return None;
        }
        type Create = unsafe extern "system" fn(usize) -> usize;
        let create: Create = unsafe { std::mem::transmute(com::proc(com::load("opengl32.dll"), c"wglCreateContext")?) };
        let ctx = unsafe { create(dc) };
        (ctx != 0 && unsafe { make_current()(dc, ctx) } != 0).then_some((win, dc, ctx))
    }

    /// Clears the back buffer magenta with its top quarter blue (OpenGL's rows count from
    /// the bottom), the game's context current.
    fn paint(w: i32, h: i32) {
        let gl = com::load("opengl32.dll");
        type ClearColor = unsafe extern "system" fn(f32, f32, f32, f32);
        type Clear = unsafe extern "system" fn(u32);
        type Scissor = unsafe extern "system" fn(i32, i32, i32, i32);
        type Enable = unsafe extern "system" fn(u32);
        type Finish = unsafe extern "system" fn();
        let clear_color: ClearColor = unsafe { std::mem::transmute(com::proc(gl, c"glClearColor").unwrap()) };
        let clear: Clear = unsafe { std::mem::transmute(com::proc(gl, c"glClear").unwrap()) };
        let scissor: Scissor = unsafe { std::mem::transmute(com::proc(gl, c"glScissor").unwrap()) };
        let enable: Enable = unsafe { std::mem::transmute(com::proc(gl, c"glEnable").unwrap()) };
        let disable: Enable = unsafe { std::mem::transmute(com::proc(gl, c"glDisable").unwrap()) };
        let finish: Finish = unsafe { std::mem::transmute(com::proc(gl, c"glFinish").unwrap()) };
        unsafe {
            clear_color(1.0, 0.0, 1.0, 1.0);
            clear(0x4000);
            enable(0x0C11);
            scissor(0, h * 3 / 4, w, h / 4);
            clear_color(0.0, 0.0, 1.0, 1.0);
            clear(0x4000);
            disable(0x0C11);
            finish();
        }
    }

    /// A back buffer with 4 samples a pixel (skipped where the driver has no such pixel
    /// format or can't share textures): it can't be resolved by the flipping blit, so it's
    /// resolved first; the frame arrives whole and top row first.
    #[test]
    fn multisampled_opengl_frames_are_resolved_first() {
        let _turn = crate::sys::lock(&capture::tests::SERIAL);
        let Some((_plain, _, _)) = gl_window(0) else { return };
        type GetProc = unsafe extern "system" fn(*const u8) -> usize;
        let get: GetProc = unsafe { std::mem::transmute(com::proc(com::load("opengl32.dll"), c"wglGetProcAddress").unwrap()) };
        let p = unsafe { get(c"wglChoosePixelFormatARB".as_ptr().cast()) };
        if p <= 3 || p == usize::MAX || super::fns().is_none() {
            eprintln!("no wglChoosePixelFormatARB or no WGL_NV_DX_interop2: skipped");
            unsafe { make_current()(0, 0) };
            return;
        }
        type ChooseArb = unsafe extern "system" fn(usize, *const i32, *const f32, u32, *mut i32, *mut u32) -> i32;
        let choose: ChooseArb = unsafe { std::mem::transmute(p) };
        // Window, OpenGL, double-buffered, RGBA, 32 colour bits, 8 alpha, 24 depth, 4 samples.
        let attribs = [0x2001, 1, 0x2010, 1, 0x2011, 1, 0x2013, 0x202B, 0x2014, 32, 0x201B, 8, 0x2022, 24, 0x2041, 1, 0x2042, 4, 0];
        let probe = com::HiddenWindow::new().unwrap();
        let pdc = unsafe { windows_sys::Win32::Graphics::Gdi::GetDC(probe.0) } as usize;
        let (mut format, mut n) = (0i32, 0u32);
        if unsafe { choose(pdc, attribs.as_ptr(), std::ptr::null(), 1, &mut format, &mut n) } == 0 || n == 0 {
            eprintln!("no multisampled pixel format: skipped");
            unsafe { make_current()(0, 0) };
            return;
        }
        let Some((win, dc, _game)) = gl_window(format) else { panic!("a multisampled window") };
        let (w, h) = (64u32, 64u32);
        paint(w as i32, h as i32);
        let (_map, name) = capture::tests::session_ring();
        capture::configure(true, Some(&name), None);
        copy_only(dc);
        let p = capture::published().expect("published");
        let all = crate::sys::lock(&super::super::CTX);
        let cap = all.iter().find(|c| c.hwnd == win.0 as usize).and_then(|c| c.capture.as_ref()).and_then(|c| c.as_ref()).expect("shared textures");
        assert!(cap.resolve.is_some(), "resolved through a texture of its own");
        let handle = cap.ring.as_ref().unwrap().handle(p.slot);
        drop(all);
        let (other, octx) = capture::private_device(None).unwrap();
        let dev1 = com::query(other, &com::IID_ID3D11DEVICE1);
        let mut tex = 0usize;
        let hr = com::vcall!(dev1, 48, fn(windows_sys::Win32::Foundation::HANDLE, *const com::Guid, *mut usize) -> i32, handle, &com::IID_ID3D11TEXTURE2D, &mut tex);
        assert!(hr >= 0 && tex != 0, "OpenSharedResource1 {hr:#x}");
        let px = super::super::super::d3d11::read_pixels(other, octx, tex, w, h);
        assert_eq!(px[(w / 2) as usize] & 0x00ff_ffff, 0x0000ff, "the top row is the top: blue");
        assert_eq!(px[(h / 2 * w + w / 2) as usize] & 0x00ff_ffff, 0xff00ff, "magenta below");
        capture::configure(false, None, None);
        for o in [tex, dev1, octx, other] {
            com::release(o);
        }
        unsafe { make_current()(0, 0) };
    }

    /// PeerParty's context is the window's, whatever device context a swap comes with, and
    /// goes when the window does.
    #[test]
    fn opengl_contexts_follow_windows() {
        let _turn = crate::sys::lock(&capture::tests::SERIAL);
        let Some((a, dc_a, _)) = gl_window(0) else { return };
        let (_map, name) = capture::tests::session_ring();
        capture::configure(true, Some(&name), None);
        let hwnd_a = a.0 as usize;
        let of = |hwnd: usize| crate::sys::lock(&super::super::CTX).iter().filter(|c| c.hwnd == hwnd).count();
        copy_only(dc_a);
        let dc_a2 = unsafe { windows_sys::Win32::Graphics::Gdi::GetDC(a.0) } as usize;
        copy_only(dc_a2);
        assert_eq!(of(hwnd_a), 1, "one context for the window");
        assert!(crate::sys::lock(&super::super::CTX).iter().any(|c| c.hwnd == hwnd_a && c.dc == dc_a2), "made current on the latest device context");
        let Some((b, dc_b, _)) = gl_window(0) else { panic!("a second window") };
        drop(a);
        copy_only(dc_b);
        assert_eq!(of(hwnd_a), 0, "the gone window's context went");
        assert_eq!(of(b.0 as usize), 1);
        capture::configure(false, None, None);
        unsafe { make_current()(0, 0) };
    }
}
