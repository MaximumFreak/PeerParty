//! Drawing the overlay frame with Direct3D 11: one dynamic BGRA texture (uploaded only
//! when the launcher's frame changed), a full-screen triangle made from the vertex id,
//! premultiplied alpha blending. Every piece of pipeline state touched is saved first and
//! put back after, so the game's next frame starts as it left off. Also used for D3D12,
//! through D3D11On12 (d3d12.rs).
//!
//! Vtable slots are from d3d11.h: ID3D11Device and ID3D11DeviceContext in declaration
//! order after IUnknown (and ID3D11DeviceChild for the context).

use super::Frame;
use super::com::{compile, release, vcall};

const HLSL: &str = r#"
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
    V o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
Texture2D t : register(t0);
SamplerState s : register(s0);
float4 ps(V i) : SV_Target { return t.Sample(s, i.uv); }
"#;

const FORMAT_B8G8R8A8_UNORM: u32 = 87;

#[repr(C)]
struct Texture2dDesc {
    width: u32,
    height: u32,
    mip_levels: u32,
    array_size: u32,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    usage: u32,
    bind_flags: u32,
    cpu_access: u32,
    misc: u32,
}

#[repr(C)]
struct Mapped {
    data: *mut u8,
    row_pitch: u32,
    depth_pitch: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Viewport {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    min_depth: f32,
    max_depth: f32,
}

#[repr(C)]
struct RtvDesc {
    format: u32,
    dimension: u32,
    mip_slice: u32,
    pad: [u32; 2],
}

pub struct Renderer {
    device: usize,
    ctx: usize,
    vs: usize,
    ps: usize,
    sampler: usize,
    blend: usize,
    raster: usize,
    depth: usize,
    tex: usize,
    srv: usize,
    size: (u32, u32),
    seq: Option<u32>,
}

// The objects are only used from the game's render thread, under the present lock.
unsafe impl Send for Renderer {}

impl Renderer {
    pub fn new(device: usize) -> Option<Renderer> {
        vcall!(device, 1, fn() -> u32);
        let mut ctx = 0usize;
        vcall!(device, 40, fn(*mut usize) -> (), &mut ctx);
        let mut r = Renderer { device, ctx, vs: 0, ps: 0, sampler: 0, blend: 0, raster: 0, depth: 0, tex: 0, srv: 0, size: (0, 0), seq: None };
        let vs = compile(HLSL, c"vs", c"vs_4_0")?;
        let ps = compile(HLSL, c"ps", c"ps_4_0")?;
        let ok = |hr: i32| hr >= 0;
        if !ok(vcall!(device, 12, fn(*const u8, usize, usize, *mut usize) -> i32, vs.as_ptr(), vs.len(), 0, &mut r.vs))
            || !ok(vcall!(device, 15, fn(*const u8, usize, usize, *mut usize) -> i32, ps.as_ptr(), ps.len(), 0, &mut r.ps))
        {
            return None;
        }
        // D3D11_BLEND_DESC: premultiplied "over" on render target 0.
        let mut blend = [0u32; 2 + 8 * 8];
        blend[2] = 1; // BlendEnable
        blend[3] = 2; // SrcBlend ONE
        blend[4] = 6; // DestBlend INV_SRC_ALPHA
        blend[5] = 1; // BlendOp ADD
        blend[6] = 2; // SrcBlendAlpha ONE
        blend[7] = 6; // DestBlendAlpha INV_SRC_ALPHA
        blend[8] = 1; // BlendOpAlpha ADD
        blend[9] = 0xf; // RenderTargetWriteMask ALL
        // D3D11_RASTERIZER_DESC: solid, no culling, depth clip on.
        let raster: [u32; 10] = [3, 1, 0, 0, 0, 0, 1, 0, 0, 0];
        // D3D11_DEPTH_STENCIL_DESC: everything off (functions still valid values).
        let depth: [u32; 13] = [0, 0, 8, 0, 0xffff, 1, 1, 1, 8, 1, 1, 1, 8];
        // D3D11_SAMPLER_DESC: point filtering, clamp.
        let mut sampler = [0u32; 13];
        sampler[1] = 3;
        sampler[2] = 3;
        sampler[3] = 3;
        sampler[5] = 1;
        sampler[6] = 1;
        sampler[12] = f32::MAX.to_bits();
        if !ok(vcall!(device, 20, fn(*const u32, *mut usize) -> i32, blend.as_ptr(), &mut r.blend))
            || !ok(vcall!(device, 22, fn(*const u32, *mut usize) -> i32, raster.as_ptr(), &mut r.raster))
            || !ok(vcall!(device, 21, fn(*const u32, *mut usize) -> i32, depth.as_ptr(), &mut r.depth))
            || !ok(vcall!(device, 23, fn(*const u32, *mut usize) -> i32, sampler.as_ptr(), &mut r.sampler))
        {
            return None;
        }
        Some(r)
    }

    fn ensure_texture(&mut self, w: u32, h: u32) -> bool {
        if self.size == (w, h) && self.tex != 0 {
            return true;
        }
        release(self.srv);
        release(self.tex);
        self.srv = 0;
        self.tex = 0;
        self.seq = None;
        let desc = Texture2dDesc {
            width: w,
            height: h,
            mip_levels: 1,
            array_size: 1,
            format: FORMAT_B8G8R8A8_UNORM,
            sample_count: 1,
            sample_quality: 0,
            usage: 2,             // DYNAMIC
            bind_flags: 8,        // SHADER_RESOURCE
            cpu_access: 0x10000,  // WRITE
            misc: 0,
        };
        if vcall!(self.device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut self.tex) < 0 {
            self.tex = 0;
            return false;
        }
        if vcall!(self.device, 7, fn(usize, usize, *mut usize) -> i32, self.tex, 0, &mut self.srv) < 0 {
            release(self.tex);
            self.tex = 0;
            return false;
        }
        self.size = (w, h);
        true
    }

    fn upload(&mut self, f: &Frame) {
        if self.seq == Some(f.seq) || f.writing() {
            return;
        }
        let mut m = Mapped { data: std::ptr::null_mut(), row_pitch: 0, depth_pitch: 0 };
        if vcall!(self.ctx, 14, fn(usize, u32, u32, u32, *mut Mapped) -> i32, self.tex, 0, 4, 0, &mut m) < 0 {
            return;
        }
        let ok = unsafe { f.copy_to(m.data, m.row_pitch as usize) };
        vcall!(self.ctx, 15, fn(usize, u32) -> (), self.tex, 0);
        if ok {
            self.seq = Some(f.seq);
        }
    }

    /// A render target view for a back buffer (typeless formats get their UNORM view).
    pub fn rtv_for(&self, texture: usize) -> usize {
        let mut d = Texture2dDesc {
            width: 0, height: 0, mip_levels: 0, array_size: 0, format: 0, sample_count: 0, sample_quality: 0, usage: 0, bind_flags: 0, cpu_access: 0, misc: 0,
        };
        vcall!(texture, 10, fn(*mut Texture2dDesc) -> (), &mut d);
        let format = match d.format {
            27 => 28, // R8G8B8A8_TYPELESS -> UNORM
            90 => 87, // B8G8R8A8_TYPELESS -> UNORM
            23 => 24, // R10G10B10A2_TYPELESS -> UNORM
            9 => 10,  // R16G16B16A16_TYPELESS -> FLOAT
            f => f,
        };
        let multisampled = d.sample_count > 1;
        let desc = RtvDesc { format, dimension: if multisampled { 6 } else { 4 }, mip_slice: 0, pad: [0; 2] };
        let mut rtv = 0usize;
        if vcall!(self.device, 9, fn(usize, *const RtvDesc, *mut usize) -> i32, texture, &desc, &mut rtv) < 0 {
            return 0;
        }
        rtv
    }

    /// Draws `f` over render target `rtv` (`w` x `h` pixels), leaving the game's state.
    pub fn draw(&mut self, rtv: usize, w: u32, h: u32, f: &Frame) {
        if !self.ensure_texture(f.w, f.h) {
            return;
        }
        self.upload(f);
        if self.seq.is_none() {
            return;
        }
        let c = self.ctx;
        // Save.
        let mut rtvs = [0usize; 8];
        let mut dsv = 0usize;
        vcall!(c, 89, fn(u32, *mut usize, *mut usize) -> (), 8, rtvs.as_mut_ptr(), &mut dsv);
        let (mut blend, mut factor, mut mask) = (0usize, [0f32; 4], 0u32);
        vcall!(c, 91, fn(*mut usize, *mut f32, *mut u32) -> (), &mut blend, factor.as_mut_ptr(), &mut mask);
        let (mut depth, mut stencil_ref) = (0usize, 0u32);
        vcall!(c, 92, fn(*mut usize, *mut u32) -> (), &mut depth, &mut stencil_ref);
        let mut raster = 0usize;
        vcall!(c, 94, fn(*mut usize) -> (), &mut raster);
        let mut nvp = 16u32;
        let mut vps = [Viewport::default(); 16];
        vcall!(c, 95, fn(*mut u32, *mut Viewport) -> (), &mut nvp, vps.as_mut_ptr());
        let mut layout = 0usize;
        vcall!(c, 78, fn(*mut usize) -> (), &mut layout);
        let mut topology = 0u32;
        vcall!(c, 83, fn(*mut u32) -> (), &mut topology);
        let (mut vs, mut ps, mut gs, mut hs, mut ds) = (0usize, 0usize, 0usize, 0usize, 0usize);
        vcall!(c, 76, fn(*mut usize, *mut usize, *mut u32) -> (), &mut vs, std::ptr::null_mut(), std::ptr::null_mut());
        vcall!(c, 74, fn(*mut usize, *mut usize, *mut u32) -> (), &mut ps, std::ptr::null_mut(), std::ptr::null_mut());
        vcall!(c, 82, fn(*mut usize, *mut usize, *mut u32) -> (), &mut gs, std::ptr::null_mut(), std::ptr::null_mut());
        vcall!(c, 98, fn(*mut usize, *mut usize, *mut u32) -> (), &mut hs, std::ptr::null_mut(), std::ptr::null_mut());
        vcall!(c, 102, fn(*mut usize, *mut usize, *mut u32) -> (), &mut ds, std::ptr::null_mut(), std::ptr::null_mut());
        let mut srv0 = 0usize;
        vcall!(c, 73, fn(u32, u32, *mut usize) -> (), 0, 1, &mut srv0);
        let mut samp0 = 0usize;
        vcall!(c, 75, fn(u32, u32, *mut usize) -> (), 0, 1, &mut samp0);

        // Draw.
        let vp = Viewport { x: 0.0, y: 0.0, w: w as f32, h: h as f32, min_depth: 0.0, max_depth: 1.0 };
        vcall!(c, 33, fn(u32, *const usize, usize) -> (), 1, &rtv, 0);
        vcall!(c, 35, fn(usize, *const f32, u32) -> (), self.blend, [0f32; 4].as_ptr(), 0xffff_ffff);
        vcall!(c, 36, fn(usize, u32) -> (), self.depth, 0);
        vcall!(c, 43, fn(usize) -> (), self.raster);
        vcall!(c, 44, fn(u32, *const Viewport) -> (), 1, &vp);
        vcall!(c, 17, fn(usize) -> (), 0);
        vcall!(c, 24, fn(u32) -> (), 4); // TRIANGLELIST
        vcall!(c, 11, fn(usize, *const usize, u32) -> (), self.vs, std::ptr::null(), 0);
        vcall!(c, 9, fn(usize, *const usize, u32) -> (), self.ps, std::ptr::null(), 0);
        vcall!(c, 23, fn(usize, *const usize, u32) -> (), 0, std::ptr::null(), 0);
        vcall!(c, 60, fn(usize, *const usize, u32) -> (), 0, std::ptr::null(), 0);
        vcall!(c, 64, fn(usize, *const usize, u32) -> (), 0, std::ptr::null(), 0);
        vcall!(c, 8, fn(u32, u32, *const usize) -> (), 0, 1, &self.srv);
        vcall!(c, 10, fn(u32, u32, *const usize) -> (), 0, 1, &self.sampler);
        vcall!(c, 13, fn(u32, u32) -> (), 3, 0);

        // Restore (the Get calls added a reference to each object; Set takes its own).
        vcall!(c, 33, fn(u32, *const usize, usize) -> (), 8, rtvs.as_ptr(), dsv);
        vcall!(c, 35, fn(usize, *const f32, u32) -> (), blend, factor.as_ptr(), mask);
        vcall!(c, 36, fn(usize, u32) -> (), depth, stencil_ref);
        vcall!(c, 43, fn(usize) -> (), raster);
        vcall!(c, 44, fn(u32, *const Viewport) -> (), nvp, vps.as_ptr());
        vcall!(c, 17, fn(usize) -> (), layout);
        vcall!(c, 24, fn(u32) -> (), topology);
        vcall!(c, 11, fn(usize, *const usize, u32) -> (), vs, std::ptr::null(), 0);
        vcall!(c, 9, fn(usize, *const usize, u32) -> (), ps, std::ptr::null(), 0);
        vcall!(c, 23, fn(usize, *const usize, u32) -> (), gs, std::ptr::null(), 0);
        vcall!(c, 60, fn(usize, *const usize, u32) -> (), hs, std::ptr::null(), 0);
        vcall!(c, 64, fn(usize, *const usize, u32) -> (), ds, std::ptr::null(), 0);
        vcall!(c, 8, fn(u32, u32, *const usize) -> (), 0, 1, &srv0);
        vcall!(c, 10, fn(u32, u32, *const usize) -> (), 0, 1, &samp0);
        for o in rtvs.into_iter().chain([dsv, blend, depth, raster, layout, vs, ps, gs, hs, ds, srv0, samp0]) {
            release(o);
        }
    }

    pub fn flush(&self) {
        vcall!(self.ctx, 111, fn() -> ());
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        for o in [self.srv, self.tex, self.sampler, self.depth, self.raster, self.blend, self.ps, self.vs, self.ctx, self.device] {
            release(o);
        }
    }
}

/// Reads back a render target texture's top-left pixel region, for tests.
#[cfg(test)]
pub fn read_pixels(device: usize, ctx: usize, tex: usize, w: u32, h: u32) -> Vec<u32> {
    let desc = Texture2dDesc {
        width: w, height: h, mip_levels: 1, array_size: 1, format: FORMAT_B8G8R8A8_UNORM, sample_count: 1, sample_quality: 0,
        usage: 3, bind_flags: 0, cpu_access: 0x20000, misc: 0,
    };
    let mut staging = 0usize;
    assert!(vcall!(device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut staging) >= 0);
    vcall!(ctx, 47, fn(usize, usize) -> (), staging, tex);
    let mut m = Mapped { data: std::ptr::null_mut(), row_pitch: 0, depth_pitch: 0 };
    assert!(vcall!(ctx, 14, fn(usize, u32, u32, u32, *mut Mapped) -> i32, staging, 0, 1, 0, &mut m) >= 0);
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h as usize {
        let row = unsafe { std::slice::from_raw_parts(m.data.add(y * m.row_pitch as usize) as *const u32, w as usize) };
        out.extend_from_slice(row);
    }
    vcall!(ctx, 15, fn(usize, u32) -> (), staging, 0);
    release(staging);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WARP device, an offscreen render target filled blue, and a frame that is half
    /// clear and half 50% red: the blend must leave blue on the clear half and mix the
    /// other. No window, no GPU.
    #[test]
    fn draws_a_frame_over_a_render_target() {
        let m = super::super::com::load("d3d11.dll");
        type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *mut usize, *mut u32, *mut usize) -> i32;
        let create: Create = unsafe { std::mem::transmute(super::super::com::proc(m, c"D3D11CreateDevice").unwrap()) };
        let (mut device, mut ctx, mut fl) = (0usize, 0usize, 0u32);
        let hr = unsafe { create(0, 5, 0, 0x20, std::ptr::null(), 0, 7, &mut device, &mut fl, &mut ctx) };
        assert!(hr >= 0, "D3D11CreateDevice {hr:#x}");
        let (w, h) = (64u32, 32u32);
        let desc = Texture2dDesc {
            width: w, height: h, mip_levels: 1, array_size: 1, format: FORMAT_B8G8R8A8_UNORM, sample_count: 1, sample_quality: 0,
            usage: 0, bind_flags: 0x20, cpu_access: 0, misc: 0,
        };
        let mut target = 0usize;
        assert!(vcall!(device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut target) >= 0);
        let mut r = Renderer::new(device).expect("renderer");
        let rtv = r.rtv_for(target);
        assert_ne!(rtv, 0);
        vcall!(ctx, 50, fn(usize, *const f32) -> (), rtv, [0.0f32, 0.0, 1.0, 1.0].as_ptr());
        // Left half clear, right half 50% red, premultiplied.
        let mut px = vec![0u32; (w * h) as usize];
        for y in 0..h as usize {
            for x in (w as usize / 2)..w as usize {
                px[y * w as usize + x] = 0x8080_0000;
            }
        }
        let f = Frame { seq: 2, w, h, px: px.as_ptr() };
        // The frame isn't in shared memory, so copy_to's re-check would read address 0:
        // upload it by hand instead.
        assert!(r.ensure_texture(w, h));
        let mut m = Mapped { data: std::ptr::null_mut(), row_pitch: 0, depth_pitch: 0 };
        assert!(vcall!(ctx, 14, fn(usize, u32, u32, u32, *mut Mapped) -> i32, r.tex, 0, 4, 0, &mut m) >= 0);
        for y in 0..h as usize {
            unsafe { std::ptr::copy_nonoverlapping(px.as_ptr().add(y * w as usize) as *const u8, m.data.add(y * m.row_pitch as usize), w as usize * 4) };
        }
        vcall!(ctx, 15, fn(usize, u32) -> (), r.tex, 0);
        r.seq = Some(2);
        r.draw(rtv, w, h, &f);
        let out = read_pixels(device, ctx, target, w, h);
        let left = out[(h as usize / 2) * w as usize + 4];
        let right = out[(h as usize / 2) * w as usize + w as usize - 4];
        assert_eq!(left & 0x00ff_ffff, 0x0000_00ff, "left {left:#x}");
        let (rr, bb) = ((right >> 16) & 0xff, right & 0xff);
        assert!((rr as i32 - 128).abs() <= 2 && (bb as i32 - 127).abs() <= 2, "right {right:#x}");
        release(rtv);
        drop(r);
        release(target);
        release(ctx);
        release(device);
    }
}
