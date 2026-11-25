//! Drawing the overlay frame with Direct3D 11: one dynamic BGRA texture (uploaded only
//! when the launcher's frame changed), a full-screen triangle made from the vertex id,
//! premultiplied alpha blending. Every piece of pipeline state touched is saved first and
//! put back after, so the game's next frame starts as it left off. Also used for D3D12,
//! through D3D11On12 (d3d12.rs).
//!
//! Vtable slots are from d3d11.h: ID3D11Device and ID3D11DeviceContext in declaration
//! order after IUnknown (and ID3D11DeviceChild for the context).

use std::sync::OnceLock;

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
// For targets that hold linear light: the frame's sRGB colour is decoded first (an
// sRGB target encodes it again as it writes, and blends in linear light), and scaled
// to SDR white for an scRGB (float) one, where 1.0 is only 80 nits.
float4 linear_of(float4 c, float gain) {
    if (c.a <= 0) return c;
    float3 e = saturate(c.rgb / c.a);
    float3 l = e <= 0.04045 ? e / 12.92 : pow((e + 0.055) / 1.055, 2.4);
    return float4(l * c.a * gain, c.a);
}
float4 ps_srgb(V i) : SV_Target { return linear_of(t.Sample(s, i.uv), 1.0); }
float4 ps_scrgb(V i) : SV_Target { return linear_of(t.Sample(s, i.uv), SDR_WHITE); }
// HDR10 targets hold PQ-encoded BT.2020: the frame's colour, at SDR white, encoded so
// (and blended in that encoding, as a plain target blends in sRGB's).
float4 ps_pq(V i) : SV_Target {
    float4 c = t.Sample(s, i.uv);
    if (c.a <= 0) return c;
    float3 l = linear_of(float4(c.rgb / c.a, 1), SDR_WHITE * 80.0 / 10000.0).rgb;
    float3x3 to2020 = { 0.6274040, 0.3292820, 0.0433136,
                        0.0690970, 0.9195400, 0.0113612,
                        0.0163916, 0.0880132, 0.8955950 };
    float3 y = pow(saturate(mul(to2020, l)), 0.1593017578125);
    float3 p = pow((0.8359375 + 18.8515625 * y) / (1 + 18.6875 * y), 78.84375);
    return float4(p * c.a, c.a);
}
"#;

/// What scRGB 1.0 (80 nits) is multiplied by for the overlay's white: 200 nits, about
/// where Windows puts SDR content on an HDR display by default.
const SDR_WHITE: &str = "2.5";

/// How a render target's format takes the frame's (sRGB-encoded) pixels.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Target {
    /// As they are (UNORM formats, which hold sRGB-encoded values).
    Plain,
    /// An `_SRGB` format: the hardware encodes what is written.
    Srgb,
    /// R16G16B16A16_FLOAT: scRGB, linear light (HDR swap chains).
    ScRgb,
    /// R10G10B10A2_UNORM in HDR10's colour space: PQ-encoded BT.2020.
    Pq,
}

fn target_of(format: u32, hdr10: bool) -> Target {
    match format {
        29 | 91 | 93 => Target::Srgb, // R8G8B8A8 / B8G8R8A8 / B8G8R8X8 _UNORM_SRGB
        10 => Target::ScRgb,          // R16G16B16A16_FLOAT
        24 if hdr10 => Target::Pq,    // R10G10B10A2_UNORM
        _ => Target::Plain,
    }
}

const FORMAT_B8G8R8A8_UNORM: u32 = 87;

/// D3D11_1_UAV_SLOT_COUNT.
const UAV_SLOTS_11_1: usize = 64;

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
    /// The pixel shaders for linear targets (`Target::Srgb`, `Target::ScRgb`) and HDR10,
    /// made the first time a target needs one (0 until then).
    ps_srgb: usize,
    ps_scrgb: usize,
    ps_pq: usize,
    /// The target is in HDR10's colour space (the swap chain's, `dxgi::color_space`).
    pub hdr10: bool,
    sampler: usize,
    blend: usize,
    raster: usize,
    depth: usize,
    tex: usize,
    srv: usize,
    size: (u32, u32),
    seq: Option<u32>,
    /// The output-merger slots a UAV can sit in: 8 below feature level 11_1, 64 from it
    /// (D3D11_1_UAV_SLOT_COUNT); RTVs share the first 8.
    uav_slots: usize,
}

// The objects are only used from the game's render thread, under the present lock.
unsafe impl Send for Renderer {}

impl Renderer {
    pub fn new(device: usize) -> Option<Renderer> {
        vcall!(device, 1, fn() -> u32);
        let mut ctx = 0usize;
        vcall!(device, 40, fn(*mut usize) -> (), &mut ctx);
        let mut r = Renderer {
            device, ctx, vs: 0, ps: 0, ps_srgb: 0, ps_scrgb: 0, ps_pq: 0, hdr10: false, sampler: 0, blend: 0, raster: 0, depth: 0, tex: 0, srv: 0, size: (0, 0), seq: None, uav_slots: 8,
        };
        // ID3D11Device::GetFeatureLevel.
        if vcall!(device, 37, fn() -> u32) >= 0xb100 {
            r.uav_slots = UAV_SLOTS_11_1;
        }
        let vs = shader_code(Shader::Vs)?;
        let ok = |hr: i32| hr >= 0;
        if !ok(vcall!(device, 12, fn(*const u8, usize, usize, *mut usize) -> i32, vs.as_ptr(), vs.len(), 0, &mut r.vs))
            || r.pixel_shader(Shader::Ps) == 0
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
        // The frame read whole first: WRITE_DISCARD hands back undefined memory, which
        // must not be left unwritten (the texture would be drawn with it).
        let Some(px) = f.stable() else { return };
        let mut m = Mapped { data: std::ptr::null_mut(), row_pitch: 0, depth_pitch: 0 };
        if vcall!(self.ctx, 14, fn(usize, u32, u32, u32, *mut Mapped) -> i32, self.tex, 0, 4, 0, &mut m) < 0 {
            return;
        }
        unsafe { f.write_rows(&px, m.data, m.row_pitch as usize) };
        vcall!(self.ctx, 15, fn(usize, u32) -> (), self.tex, 0);
        self.seq = Some(f.seq);
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
            92 => 88, // B8G8R8X8_TYPELESS -> UNORM
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
        // ID3D11RenderTargetView::GetDesc: the view's format says how to write to it.
        let mut view = RtvDesc { format: 0, dimension: 0, mip_slice: 0, pad: [0; 2] };
        vcall!(rtv, 8, fn(*mut RtvDesc) -> (), &mut view);
        let shader = self.pixel_shader(match target_of(view.format, self.hdr10) {
            Target::Plain => Shader::Ps,
            Target::Srgb => Shader::PsSrgb,
            Target::ScRgb => Shader::PsScRgb,
            Target::Pq => Shader::PsPq,
        });
        if shader == 0 {
            return;
        }
        // Save. With the RTVs the pixel shader's UAVs, which share their slots: setting
        // the render targets alone would unbind them.
        let mut rtvs = [0usize; 8];
        let mut dsv = 0usize;
        let mut uavs = [0usize; UAV_SLOTS_11_1];
        vcall!(c, 90, fn(u32, *mut usize, *mut usize, u32, u32, *mut usize) -> (), 8, rtvs.as_mut_ptr(), &mut dsv, 0, self.uav_slots as u32, uavs.as_mut_ptr());
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
        vcall!(c, 9, fn(usize, *const usize, u32) -> (), shader, std::ptr::null(), 0);
        vcall!(c, 23, fn(usize, *const usize, u32) -> (), 0, std::ptr::null(), 0);
        vcall!(c, 60, fn(usize, *const usize, u32) -> (), 0, std::ptr::null(), 0);
        vcall!(c, 64, fn(usize, *const usize, u32) -> (), 0, std::ptr::null(), 0);
        vcall!(c, 8, fn(u32, u32, *const usize) -> (), 0, 1, &self.srv);
        vcall!(c, 10, fn(u32, u32, *const usize) -> (), 0, 1, &self.sampler);
        vcall!(c, 13, fn(u32, u32) -> (), 3, 0);

        // Restore (the Get calls added a reference to each object; Set takes its own).
        match (uavs.iter().position(|&u| u != 0), uavs.iter().rposition(|&u| u != 0)) {
            (Some(first), Some(last)) => {
                // No RTV is bound at or past the first UAV's slot (D3D11 refuses the
                // overlap), so the RTVs below it are all there were. A count of -1 keeps
                // each append/consume buffer's counter where the game left it.
                let n = last - first + 1;
                let counts = [u32::MAX; UAV_SLOTS_11_1];
                vcall!(c, 34, fn(u32, *const usize, usize, u32, u32, *const usize, *const u32) -> (),
                    first.min(8) as u32, rtvs.as_ptr(), dsv, first as u32, n as u32, uavs[first..].as_ptr(), counts.as_ptr());
            }
            _ => vcall!(c, 33, fn(u32, *const usize, usize) -> (), 8, rtvs.as_ptr(), dsv),
        }
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
        for o in rtvs.into_iter().chain(uavs).chain([dsv, blend, depth, raster, layout, vs, ps, gs, hs, ds, srv0, samp0]) {
            release(o);
        }
    }

    pub fn flush(&self) {
        vcall!(self.ctx, 111, fn() -> ());
    }

    /// The pixel shader `which`, made now if it hasn't been (0 if it can't be).
    fn pixel_shader(&mut self, which: Shader) -> usize {
        let device = self.device;
        let slot = match which {
            Shader::Ps => &mut self.ps,
            Shader::PsSrgb => &mut self.ps_srgb,
            Shader::PsScRgb => &mut self.ps_scrgb,
            Shader::PsPq => &mut self.ps_pq,
            Shader::Vs => return self.vs,
        };
        if *slot == 0
            && let Some(code) = shader_code(which)
            && vcall!(device, 15, fn(*const u8, usize, usize, *mut usize) -> i32, code.as_ptr(), code.len(), 0, slot) < 0
        {
            *slot = 0;
        }
        *slot
    }
}

#[derive(Clone, Copy)]
enum Shader {
    Vs,
    Ps,
    PsSrgb,
    PsScRgb,
    PsPq,
}

/// A shader's bytecode, compiled once a process: a renderer is made again for each new
/// device or swap chain, and compiling is what made the overlay's first frame slow.
fn shader_code(which: Shader) -> Option<&'static [u8]> {
    static CODE: [OnceLock<Option<Vec<u8>>>; 5] = [const { OnceLock::new() }; 5];
    let (i, entry, target) = match which {
        Shader::Vs => (0, c"vs", c"vs_4_0"),
        Shader::Ps => (1, c"ps", c"ps_4_0"),
        Shader::PsSrgb => (2, c"ps_srgb", c"ps_4_0"),
        Shader::PsScRgb => (3, c"ps_scrgb", c"ps_4_0"),
        Shader::PsPq => (4, c"ps_pq", c"ps_4_0"),
    };
    CODE[i]
        .get_or_init(|| compile(&format!("#define SDR_WHITE {SDR_WHITE}\n{HLSL}"), entry, target))
        .as_deref()
}

impl Drop for Renderer {
    fn drop(&mut self) {
        for o in [self.srv, self.tex, self.sampler, self.depth, self.raster, self.blend, self.ps_pq, self.ps_scrgb, self.ps_srgb, self.ps, self.vs, self.ctx, self.device] {
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
        let f = Frame { seq: 2, w, h, px: px.as_ptr(), header: None };
        // The frame isn't in shared memory: upload it by hand.
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

    /// A game's pixel-shader UAV, bound beside its render target, must still be bound
    /// (with its append counter as it was) once the overlay is drawn, and the saved
    /// references dropped again. Slot 1 at feature level 11_0; slot 9, past the RTVs'
    /// eight, at 11_1.
    #[test]
    fn keeps_the_games_pixel_shader_uavs() {
        let m = super::super::com::load("d3d11.dll");
        type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *mut usize, *mut u32, *mut usize) -> i32;
        let create: Create = unsafe { std::mem::transmute(super::super::com::proc(m, c"D3D11CreateDevice").unwrap()) };
        let refs = |o: usize| {
            vcall!(o, 1, fn() -> u32);
            vcall!(o, 2, fn() -> u32)
        };
        for (level, slot) in [(0xb000u32, 1usize), (0xb100, 9)] {
            let (mut device, mut ctx, mut fl) = (0usize, 0usize, 0u32);
            let hr = unsafe { create(0, 5, 0, 0x20, &level, 1, 7, &mut device, &mut fl, &mut ctx) };
            assert!(hr >= 0, "D3D11CreateDevice {level:#x} {hr:#x}");
            let (w, h) = (16u32, 16u32);
            let desc = Texture2dDesc {
                width: w, height: h, mip_levels: 1, array_size: 1, format: FORMAT_B8G8R8A8_UNORM, sample_count: 1, sample_quality: 0,
                usage: 0, bind_flags: 0x20, cpu_access: 0, misc: 0,
            };
            let mut target = 0usize;
            assert!(vcall!(device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut target) >= 0);
            // An append buffer: structured, 16 uints, its UAV with a counter.
            let buf_desc: [u32; 6] = [64, 0, 0x80, 0, 0x40, 4];
            let mut buf = 0usize;
            assert!(vcall!(device, 3, fn(*const u32, usize, *mut usize) -> i32, buf_desc.as_ptr(), 0, &mut buf) >= 0);
            let uav_desc: [u32; 5] = [0, 1, 0, 16, 4];
            let mut uav = 0usize;
            assert!(vcall!(device, 8, fn(usize, *const u32, *mut usize) -> i32, buf, uav_desc.as_ptr(), &mut uav) >= 0);
            let mut r = Renderer::new(device).expect("renderer");
            let rtv = r.rtv_for(target);
            assert_ne!(rtv, 0);
            vcall!(ctx, 34, fn(u32, *const usize, usize, u32, u32, *const usize, *const u32) -> (), 1, &rtv, 0, slot as u32, 1, &uav, &5u32);
            let (rtv_refs, uav_refs) = (refs(rtv), refs(uav));
            let px = vec![0x8080_8080u32; (w * h) as usize];
            let f = Frame { seq: 2, w, h, px: px.as_ptr(), header: None };
            assert!(r.ensure_texture(w, h));
            r.seq = Some(2);
            r.draw(rtv, w, h, &f);

            let (mut rtvs, mut dsv, mut uavs) = ([0usize; 8], 0usize, [0usize; UAV_SLOTS_11_1]);
            vcall!(ctx, 90, fn(u32, *mut usize, *mut usize, u32, u32, *mut usize) -> (), 8, rtvs.as_mut_ptr(), &mut dsv, 0, r.uav_slots as u32, uavs.as_mut_ptr());
            assert_eq!(rtvs[0], rtv, "feature level {level:#x}");
            assert_eq!(uavs[slot], uav, "feature level {level:#x}");
            assert_eq!(uavs.iter().filter(|&&u| u != 0).count(), 1);
            for o in rtvs.into_iter().chain(uavs).chain([dsv]) {
                release(o);
            }
            assert_eq!((refs(rtv), refs(uav)), (rtv_refs, uav_refs), "references left behind");
            // The counter is still the 5 it was bound with (an initial count of 0 would
            // have reset it).
            let staging_desc: [u32; 6] = [16, 3, 0, 0x20000, 0, 0];
            let mut staging = 0usize;
            assert!(vcall!(device, 3, fn(*const u32, usize, *mut usize) -> i32, staging_desc.as_ptr(), 0, &mut staging) >= 0);
            vcall!(ctx, 49, fn(usize, u32, usize) -> (), staging, 0, uav);
            let mut mp = Mapped { data: std::ptr::null_mut(), row_pitch: 0, depth_pitch: 0 };
            assert!(vcall!(ctx, 14, fn(usize, u32, u32, u32, *mut Mapped) -> i32, staging, 0, 1, 0, &mut mp) >= 0);
            let count = unsafe { *(mp.data as *const u32) };
            vcall!(ctx, 15, fn(usize, u32) -> (), staging, 0);
            assert_eq!(count, 5, "feature level {level:#x}");

            vcall!(ctx, 110, fn() -> ());
            release(staging);
            release(rtv);
            drop(r);
            release(uav);
            release(buf);
            release(target);
            release(ctx);
            release(device);
        }
    }

    /// An sRGB back buffer encodes what is written: the overlay's grey must land as the
    /// same grey (encoded once), not brightened by a second encoding.
    #[test]
    fn an_srgb_target_gets_the_frame_encoded_once() {
        let m = super::super::com::load("d3d11.dll");
        type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *mut usize, *mut u32, *mut usize) -> i32;
        let create: Create = unsafe { std::mem::transmute(super::super::com::proc(m, c"D3D11CreateDevice").unwrap()) };
        let (mut device, mut ctx, mut fl) = (0usize, 0usize, 0u32);
        let hr = unsafe { create(0, 5, 0, 0x20, std::ptr::null(), 0, 7, &mut device, &mut fl, &mut ctx) };
        assert!(hr >= 0, "D3D11CreateDevice {hr:#x}");
        let (w, h) = (16u32, 16u32);
        let desc = Texture2dDesc {
            width: w, height: h, mip_levels: 1, array_size: 1, format: 91, sample_count: 1, sample_quality: 0,
            usage: 0, bind_flags: 0x20, cpu_access: 0, misc: 0,
        };
        let mut target = 0usize;
        assert!(vcall!(device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut target) >= 0);
        let mut r = Renderer::new(device).expect("renderer");
        let rtv = r.rtv_for(target);
        assert_ne!(rtv, 0);
        vcall!(ctx, 50, fn(usize, *const f32) -> (), rtv, [0.0f32, 0.0, 0.0, 1.0].as_ptr());
        let px = vec![0xff80_8080u32; (w * h) as usize];
        let f = Frame { seq: 2, w, h, px: px.as_ptr(), header: None };
        assert!(r.ensure_texture(w, h));
        let mut mp = Mapped { data: std::ptr::null_mut(), row_pitch: 0, depth_pitch: 0 };
        assert!(vcall!(ctx, 14, fn(usize, u32, u32, u32, *mut Mapped) -> i32, r.tex, 0, 4, 0, &mut mp) >= 0);
        for y in 0..h as usize {
            unsafe { std::ptr::copy_nonoverlapping(px.as_ptr().add(y * w as usize) as *const u8, mp.data.add(y * mp.row_pitch as usize), w as usize * 4) };
        }
        vcall!(ctx, 15, fn(usize, u32) -> (), r.tex, 0);
        r.seq = Some(2);
        r.draw(rtv, w, h, &f);
        let out = read_pixels(device, ctx, target, w, h);
        let g = out[(h as usize / 2) * w as usize + 4] & 0xff;
        assert!((g as i32 - 0x80).abs() <= 2, "grey {:#x}", out[(h as usize / 2) * w as usize + 4]);
        assert_eq!(target_of(10, false), Target::ScRgb);
        assert_eq!(target_of(87, true), Target::Plain);
        assert_eq!(target_of(24, false), Target::Plain);
        assert_eq!(target_of(24, true), Target::Pq);

        release(rtv);
        drop(r);
        release(target);
        release(ctx);
        release(device);
    }
}
