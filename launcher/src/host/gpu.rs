//! The session window drawing the copies' frames itself (compositor.rs), from the shared
//! textures their hooks copy each frame into (`protocol::frame_ring`, hook/src/video/
//! capture.rs).
//!
//! A thread of its own owns a Direct3D 11 device on the adapter the copies render on and
//! a flip-model swap chain on the session's window. It sleeps until a copy publishes a
//! frame (the session's frame event) or the panes change, waits for the swap chain to have
//! room (frame latency 1), takes each copy's newest finished frame (`RingHeader::take`),
//! has the GPU wait for its content fence, draws it scaled into the copy's pane, presents
//! with vsync, and signals each copy's release fence after. The CPU never waits for a
//! copy, and a copy never waits for it.
//!
//! A pane whose copy has no frame here yet (another adapter, a graphics API that doesn't
//! share textures, or before its first frame) keeps its DWM thumbnail: `live` says which
//! panes this draws, and the window hides only those thumbnails.
//!
//! Colour: each copy's frames come with the colour space its game set (`SurfaceMsg::
//! color_space`): sRGB, scRGB (linear float) or HDR10 (PQ-encoded BT.2020). On a monitor
//! Windows runs in HDR, the swap chain is 16-bit float scRGB: SDR panes are decoded to
//! linear light at Windows' SDR white level (the "SDR content brightness" slider), scRGB
//! ones go as they are, HDR10 ones are decoded from PQ to nits and BT.709 primaries, so
//! mixed panes all look as they would on their own and Windows maps the result to the
//! display. On an SDR monitor the swap chain is 8-bit sRGB: SDR panes go as they are and
//! HDR ones are tone-mapped down (`roll_off`). Which one the monitor is gets looked at
//! again when the window moves to another and once a second (HDR turned on or off, the
//! SDR white level moved).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use peerparty_protocol::frame_ring::{FLAG_COMPOSITOR_LIVE, FLAG_HIDDEN, SLOTS};
use peerparty_protocol::ipc::SurfaceMsg;
use peerparty_protocol::session::PaneRect;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObjectEx};

/// Calls method `slot` of COM object `obj` (stdcall, `this` first).
macro_rules! vcall {
    ($obj:expr, $slot:expr, fn($($t:ty),*) -> $r:ty $(, $arg:expr)* $(,)?) => {{
        let obj = $obj as usize;
        let vt = unsafe { *(obj as *const *const usize) };
        let f: unsafe extern "system" fn(usize $(, $t)*) -> $r = unsafe { std::mem::transmute(*vt.add($slot)) };
        unsafe { f(obj $(, $arg)*) }
    }};
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid(u32, u16, u16, [u8; 8]);

const IID_IDXGIFACTORY1: Guid = Guid(0x770aae78, 0xf26f, 0x4dba, [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87]);
const IID_IDXGIFACTORY2: Guid = Guid(0x50c83a1c, 0xe072, 0x4c48, [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0]);
const IID_IDXGIFACTORY4: Guid = Guid(0x1bc6ea02, 0xef36, 0x464f, [0xbf, 0x0c, 0x21, 0xca, 0x39, 0xe5, 0x16, 0x8a]);
const IID_IDXGIADAPTER: Guid = Guid(0x2411e7e1, 0x12ac, 0x4ccf, [0xbd, 0x14, 0x97, 0x98, 0xe8, 0x53, 0x4d, 0xc0]);
const IID_IDXGISWAPCHAIN2: Guid = Guid(0xa8be2ac4, 0x199f, 0x4946, [0xb3, 0x31, 0x79, 0x59, 0x9f, 0xb9, 0x8d, 0xe7]);
const IID_ID3D11DEVICE1: Guid = Guid(0xa04bfb29, 0x08ef, 0x43d6, [0xa4, 0x9c, 0xa9, 0xbd, 0xbd, 0xcb, 0xe6, 0x86]);
const IID_ID3D11DEVICE5: Guid = Guid(0x8ffde202, 0xa0e7, 0x45df, [0x9e, 0x01, 0xe8, 0x37, 0x80, 0x1b, 0x5e, 0xa0]);
const IID_ID3D11DEVICECONTEXT4: Guid = Guid(0x917600da, 0xf58c, 0x4c33, [0x98, 0xd8, 0x3e, 0x15, 0xb3, 0x90, 0xfa, 0x24]);
const IID_ID3D11FENCE: Guid = Guid(0xaffde9d1, 0x1df7, 0x4bb7, [0x8a, 0x34, 0x0f, 0x46, 0x25, 0x1d, 0xab, 0x80]);
const IID_ID3D11TEXTURE2D: Guid = Guid(0x6f15aaf2, 0xd208, 0x4e89, [0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c]);

fn release(obj: usize) {
    if obj != 0 {
        vcall!(obj, 2, fn() -> u32);
    }
}

fn query(obj: usize, iid: &Guid) -> usize {
    let mut out = 0usize;
    if obj == 0 || vcall!(obj, 0, fn(*const Guid, *mut usize) -> i32, iid, &mut out) < 0 { 0 } else { out }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A function of a DLL in System32 (main.rs `only_system_dlls`). Each DLL is loaded once
/// and stays (the display is looked at every second; loading it each time added a
/// reference every time).
fn system_proc(dll: &str, name: &std::ffi::CStr) -> Option<usize> {
    use std::collections::HashMap;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    static LOADED: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);
    let m = {
        let mut l = LOADED.lock().unwrap_or_else(|e| e.into_inner());
        let l = l.get_or_insert_with(HashMap::new);
        match l.get(dll) {
            Some(&m) => m,
            None => {
                let m = unsafe { LoadLibraryExW(wide(dll).as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) } as usize;
                if m == 0 {
                    return None;
                }
                l.insert(dll.to_string(), m);
                m
            }
        }
    };
    unsafe { GetProcAddress(m as _, name.as_ptr().cast()) }.map(|f| f as usize)
}

/// A random suffix for the names nothing should guess.
fn token() -> String {
    peerparty_core::inet::crypto::random_array::<8>().iter().map(|b| format!("{b:02x}")).collect()
}

// --- What the session and its copies' hooks share -------------------------------------

pub use super::ring::RingBlock;

/// One generation of a copy's textures and fences, duplicated into this process; or of
/// its frames as pixels (`PEERPARTY_COMPOSITOR=pixels`, `ring::pixel_frames`), which the
/// window uploads itself.
pub struct SharedSurface {
    pub generation: u32,
    /// The copy's process (its log takes the pane's frame counts).
    pub pid: u32,
    textures: [HANDLE; SLOTS],
    content: HANDLE,
    release: HANDLE,
    pub format: u32,
    pub color_space: u32,
    /// The frames' width and height: what the pane is fitted to.
    pub size: (u32, u32),
    pixels: Option<super::ring::PixelFrames>,
}

unsafe impl Send for SharedSurface {}
unsafe impl Sync for SharedSurface {}

impl SharedSurface {
    /// The handles of a `surface`, duplicated out of the copy's process `pid`.
    pub fn duplicate(m: &SurfaceMsg) -> Option<SharedSurface> {
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE};
        if m.pixels.is_some() {
            let p = super::ring::PixelFrames::open(m)?;
            return Some(SharedSurface {
                generation: m.generation,
                pid: m.pid,
                textures: [std::ptr::null_mut(); SLOTS],
                content: std::ptr::null_mut(),
                release: std::ptr::null_mut(),
                format: m.format,
                color_space: m.color_space,
                size: (m.width, m.height),
                pixels: Some(p),
            });
        }
        let process = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, m.pid) };
        if process.is_null() {
            return None;
        }
        let dup = |h: u64| -> Option<HANDLE> {
            let mut out: HANDLE = std::ptr::null_mut();
            let ok = unsafe { DuplicateHandle(process, h as usize as HANDLE, GetCurrentProcess(), &mut out, 0, 0, DUPLICATE_SAME_ACCESS) } != 0;
            (ok && !out.is_null()).then_some(out)
        };
        let mut s = SharedSurface {
            generation: m.generation,
            pid: m.pid,
            textures: [std::ptr::null_mut(); SLOTS],
            content: std::ptr::null_mut(),
            release: std::ptr::null_mut(),
            format: m.format,
            color_space: m.color_space,
            size: (m.width, m.height),
            pixels: None,
        };
        let mut ok = m.textures.len() == SLOTS;
        for (i, h) in m.textures.iter().enumerate().take(SLOTS) {
            match dup(*h) {
                Some(d) => s.textures[i] = d,
                None => ok = false,
            }
        }
        match (dup(m.content), dup(m.release)) {
            (Some(c), Some(r)) => (s.content, s.release) = (c, r),
            (c, r) => {
                ok = false;
                for h in [c, r].into_iter().flatten() {
                    unsafe { CloseHandle(h) };
                }
            }
        }
        unsafe { CloseHandle(process) };
        ok.then_some(s)
    }
}

impl Drop for SharedSurface {
    fn drop(&mut self) {
        for h in self.textures.into_iter().chain([self.content, self.release]) {
            if !h.is_null() {
                unsafe { CloseHandle(h) };
            }
        }
    }
}

/// Full-texture UV rectangle `[u0, v0, u1, v1]`.
pub const FULL_UV: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// What a pane shows: where, its source UV crop, draw Z-order, whether CAS sharpening
/// applies when upscaled, from which ring, and which textures.
#[derive(Clone)]
pub struct PaneDraw {
    pub dest: PaneRect,
    /// Source UV sub-rectangle `[u0, v0, u1, v1]` in `0.0..=1.0` (`FULL_UV` for Fit/Stretch,
    /// centered crop for `PaneFit::Fill`).
    pub uv: [f32; 4],
    /// Draw order (higher draws on top, for Picture-in-Picture overlays).
    pub z: u8,
    /// Apply single-pass Contrast Adaptive Sharpening (CAS) when scaling the source up.
    pub sharpen: bool,
    pub ring: Option<Arc<RingBlock>>,
    pub surface: Option<Arc<SharedSurface>>,
}

impl PartialEq for PaneDraw {
    fn eq(&self, o: &PaneDraw) -> bool {
        let same = |a: &Option<Arc<RingBlock>>, b: &Option<Arc<RingBlock>>| match (a, b) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let same_surface = match (&self.surface, &o.surface) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        self.dest == o.dest && self.uv == o.uv && self.z == o.z && self.sharpen == o.sharpen && same(&self.ring, &o.ring) && same_surface
    }
}

struct Shared {
    /// The panes wanted, and a count of changes.
    want: Mutex<(u64, Vec<PaneDraw>)>,
    /// Which panes the thread draws.
    live: Mutex<Vec<bool>>,
    /// DXGI `Present` sync interval (`1` = full refresh, `2` = 1/2, `3` = 1/3).
    sync_interval: AtomicU32,
    quit: AtomicBool,
}

struct Handle(HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

/// The drawing thread of one session window.
pub struct Gpu {
    shared: Arc<Shared>,
    wake: Arc<Handle>,
    frames: Arc<Handle>,
    /// The frame event's name, for the copies' hooks (`HostMsg::Capture`).
    pub event_name: String,
    thread: Option<JoinHandle<()>>,
    /// `stop` gave up waiting for the thread.
    left_running: bool,
}

impl Gpu {
    /// The thread for window `hwnd`. Nothing is made on the GPU until a copy's textures
    /// come.
    pub fn start(hwnd: HWND) -> Option<Gpu> {
        Gpu::start_inner(hwnd, true)
    }

    #[cfg(test)]
    fn start_for_test(hwnd: HWND) -> Option<Gpu> {
        Gpu::start_inner(hwnd, false)
    }

    fn start_inner(hwnd: HWND, pause_on_occluded: bool) -> Option<Gpu> {
        let event_name = format!(r"Local\peerparty-frames-{}-{}", std::process::id(), token());
        // Auto-reset: one wake per burst of frames.
        let frames = unsafe { CreateEventW(std::ptr::null(), 0, 0, wide(&event_name).as_ptr()) };
        let wake = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if frames.is_null() || wake.is_null() {
            for h in [frames, wake] {
                if !h.is_null() {
                    unsafe { CloseHandle(h) };
                }
            }
            return None;
        }
        let shared = Arc::new(Shared {
            want: Mutex::new((0, Vec::new())),
            live: Mutex::new(Vec::new()),
            sync_interval: AtomicU32::new(1),
            quit: AtomicBool::new(false),
        });
        let (frames, wake) = (Arc::new(Handle(frames)), Arc::new(Handle(wake)));
        let (s, f, w, h) = (shared.clone(), frames.clone(), wake.clone(), hwnd as usize);
        let thread = std::thread::Builder::new().name("peerparty-compositor".into()).spawn(move || run(h, &s, f.0, w.0, pause_on_occluded)).ok()?;
        Some(Gpu { shared, wake, frames, event_name, thread: Some(thread), left_running: false })
    }

    /// The panes to draw, by copy.
    pub fn set(&self, panes: Vec<PaneDraw>) {
        let mut w = self.shared.want.lock().unwrap_or_else(|e| e.into_inner());
        if w.1 != panes {
            w.0 += 1;
            w.1 = panes;
            drop(w);
            unsafe { SetEvent(self.wake.0) };
        }
    }

    /// Sets the DXGI `Present` sync interval (`1` = full refresh, `2` = 1/2, `3` = 1/3).
    pub fn set_sync_interval(&self, interval: u8) {
        let clamped = u32::from(interval.clamp(1, 4));
        if self.shared.sync_interval.swap(clamped, Ordering::AcqRel) != clamped {
            unsafe { SetEvent(self.wake.0) };
        }
    }

    /// Whether pane `i` is drawn here (its thumbnail hidden).
    pub fn live(&self, i: usize) -> bool {
        self.shared.live.lock().unwrap_or_else(|e| e.into_inner()).get(i).copied().unwrap_or(false)
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Gpu {
    /// Ends the drawing thread, waiting a few seconds at most: whether it has ended (and
    /// no longer draws in the window). Once is enough; later calls say the same.
    pub fn stop(&mut self) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};
        self.shared.quit.store(true, Ordering::Release);
        unsafe { SetEvent(self.wake.0) };
        let Some(t) = self.thread.take() else { return !self.left_running };
        // DXGI may be sending the window a message from that thread: this one (the
        // window's) keeps answering them while it waits, for a few seconds at most.
        let start = std::time::Instant::now();
        while !t.is_finished() && start.elapsed() < std::time::Duration::from_secs(3) {
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if t.is_finished() {
            let _ = t.join();
            unsafe {
                CloseHandle(self.wake.0);
                CloseHandle(self.frames.0);
            }
            return true;
        }
        // Else it's left to finish on its own, with its events.
        eprintln!("peerparty: session: the window's drawing thread hasn't stopped after 3 s; it is left to finish on its own");
        self.left_running = true;
        false
    }
}

// --- The drawing thread ----------------------------------------------------------------

/// Present's answer when the window can't be seen.
const DXGI_STATUS_OCCLUDED: i32 = 0x087A_0001;
/// Present's flag for asking whether it could be seen, presenting nothing.
const DXGI_PRESENT_TEST: u32 = 1;
/// How often an occluded window is asked whether it can be seen again.
const OCCLUDED_RETRY: std::time::Duration = std::time::Duration::from_millis(250);

const HLSL: &str = r#"
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
    V o;
    float2 uv = float2(id & 1, id >> 1);
    o.uv = uv;
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
Texture2D t : register(t0);
SamplerState s : register(s0);
cbuffer Pane : register(b0) {
    uint input;      // the frame's values: 0 sRGB-encoded, 1 scRGB (linear), 2 HDR10 (PQ, BT.2020)
    uint output;     // the window's: 0 sRGB-encoded (an SDR display), 1 scRGB (an HDR one)
    float sdr_white; // where SDR white goes on an HDR display, in scRGB (1.0 = 80 nits)
    float hdr_white; // the nits of an HDR frame shown as white on an SDR display
    float4 uv_rect;  // (u0, v0, u1 - u0, v1 - v0) source sub-rectangle
    float2 src_inv;  // (1 / src_w, 1 / src_h) texel step for contrast-adaptive sharpening
    float sharpen;   // 0 = plain bilinear, >0 = CAS sharpening strength when upscaling
    float _pad;
};
float3 srgb_decode(float3 e) {
    e = saturate(e);
    return e <= 0.04045 ? e / 12.92 : pow((e + 0.055) / 1.055, 2.4);
}
float3 srgb_encode(float3 c) {
    c = saturate(c);
    return c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}
// SMPTE ST 2084's EOTF: PQ code values to nits.
float3 pq_nits(float3 p) {
    float3 e = pow(saturate(p), 1.0 / 78.84375);
    return 10000.0 * pow(max(e - 0.8359375, 0) / (18.8515625 - 18.6875 * e), 1.0 / 0.1593017578125);
}
// Linear BT.2020 to BT.709 primaries (out-of-gamut colour goes negative, which scRGB keeps).
static const float3x3 BT2020_TO_709 = {
     1.660491, -0.587641, -0.072850,
    -0.124550,  1.132900, -0.008349,
    -0.018151, -0.100579,  1.118730 };
// HDR highlights brought under SDR white: untouched to 0.8, then rolled off toward 1.0,
// by the brightest channel so hues don't shift.
float3 roll_off(float3 c) {
    float m = max(max(c.r, c.g), c.b);
    if (m <= 0.8) return c;
    float k = (m - 0.8) / 0.2;
    return c * ((0.8 + 0.2 * k / (1 + k)) / m);
}
// A game's stray NaN pixel black, an infinite one the largest half (by bits: the
// compiler may assume no NaN in float compares).
float3 finite(float3 c) {
    uint3 b = asuint(c) & 0x7fffffff;
    c = b > 0x7f800000 ? 0 : c;
    return clamp(c, -65504.0, 65504.0);
}
// Single-pass 5-tap Contrast Adaptive Sharpening (CAS) bounded strictly inside the
// local [min, max] neighborhood so upscaled panes stay crisp with zero haloing or NaNs,
// and flat areas remain bit-identical. Works on both [0, 1] SDR/PQ and >1 scRGB HDR.
float3 sample_cas(float2 uv) {
    float3 e = finite(t.Sample(s, uv).rgb);
    if (sharpen <= 0.0) return e;
    float2 uv_min = uv_rect.xy + src_inv * 0.5;
    float2 uv_max = max(uv_min, uv_rect.xy + uv_rect.zw - src_inv * 0.5);
    float3 b = finite(t.Sample(s, clamp(uv + float2(0, -src_inv.y), uv_min, uv_max)).rgb);
    float3 d = finite(t.Sample(s, clamp(uv + float2(-src_inv.x, 0), uv_min, uv_max)).rgb);
    float3 f = finite(t.Sample(s, clamp(uv + float2(src_inv.x, 0), uv_min, uv_max)).rgb);
    float3 h = finite(t.Sample(s, clamp(uv + float2(0, src_inv.y), uv_min, uv_max)).rgb);
    float3 mn = min(e, min(min(b, d), min(f, h)));
    float3 mx = max(e, max(max(b, d), max(f, h)));
    float3 peak = max(mx, 1.0);
    float3 amp = saturate(min(max(mn, 0.0), max(peak - mx, 0.0)) / max(mx, 1e-5));
    float3 w = -sqrt(amp) * (sharpen * 0.2);
    return clamp((e + (b + d + f + h) * w) / (1.0 + 4.0 * w), mn, mx);
}
float4 ps(V i) : SV_Target {
    float2 uv = uv_rect.xy + i.uv * uv_rect.zw;
    float3 c = sample_cas(uv);
    if (input == 0) return float4(output == 1 ? srgb_decode(c) * sdr_white : c, 1);
    float3 nits = input == 2 ? mul(BT2020_TO_709, pq_nits(c)) : c * 80.0;
    if (output == 1) return float4(nits / 80.0, 1);
    return float4(srgb_encode(roll_off(max(nits, 0) / hdr_white)), 1);
}
"#;

/// A pane's `cbuffer Pane` (48 bytes, 16-byte aligned for D3D11).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug)]
struct PaneParams {
    input: u32,
    output: u32,
    sdr_white: f32,
    hdr_white: f32,
    uv_rect: [f32; 4],
    src_inv: [f32; 2],
    sharpen: f32,
    _pad: f32,
}

/// Default CAS sharpening strength when a pane is scaled up and Sharp upscaling is on.
const CAS_STRENGTH: f32 = 0.6;

/// The nits an HDR frame's white is shown at on an SDR display (BT.2408's reference white).
const HDR_WHITE_NITS: f32 = 203.0;

/// How a frame's values are encoded, for `cbuffer Pane`'s `input`: from its colour space,
/// and its format (a float frame is linear light whatever it said).
fn input_of(format: u32, color_space: u32) -> u32 {
    use peerparty_protocol::frame_ring::{COLOR_SPACE_HDR10, COLOR_SPACE_SCRGB};
    match (format, color_space) {
        (_, COLOR_SPACE_HDR10) => 2,
        (9 | 10, _) | (_, COLOR_SPACE_SCRGB) => 1,
        _ => 0,
    }
}

fn compile(entry: &std::ffi::CStr, target: &std::ffi::CStr) -> Option<Vec<u8>> {
    type Compile = unsafe extern "system" fn(*const u8, usize, *const i8, usize, usize, *const i8, *const i8, u32, u32, *mut usize, *mut usize) -> i32;
    let f: Compile = unsafe { std::mem::transmute(system_proc("d3dcompiler_47.dll", c"D3DCompile")?) };
    let (mut code, mut errors) = (0usize, 0usize);
    let hr = unsafe { f(HLSL.as_ptr(), HLSL.len(), std::ptr::null(), 0, 0, entry.as_ptr(), target.as_ptr(), 0, 0, &mut code, &mut errors) };
    release(errors);
    if hr < 0 || code == 0 {
        return None;
    }
    // ID3DBlob: GetBufferPointer (3), GetBufferSize (4).
    let p = vcall!(code, 3, fn() -> *const u8);
    let n = vcall!(code, 4, fn() -> usize);
    let out = unsafe { std::slice::from_raw_parts(p, n) }.to_vec();
    release(code);
    Some(out)
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Luid {
    low: u32,
    high: i32,
}

/// The format a typeless shared texture (or a pixel frame) is read as: UNORM, also for
/// an sRGB one, so sRGB-encoded values stay so and the shader decodes them when it needs
/// linear light (an sRGB view would decode them once more).
fn view_format(format: u32) -> u32 {
    match format {
        27 | 29 => 28,
        90 | 91 => 87,
        92 | 93 => 88,
        23 => 24,
        9 => 10,
        f => f,
    }
}

/// A copy's textures opened on the device.
struct Opened {
    surface: Arc<SharedSurface>,
    tex: [usize; SLOTS],
    srv: [usize; SLOTS],
    content: usize,
    release: usize,
    released: u64,
    /// `cbuffer Pane`'s `input` for its frames.
    input: u32,
    /// Frames as pixels: uploaded into `tex[0]`, and the slot and sequence it holds.
    pixels: bool,
    uploaded: Option<(usize, u32)>,
    /// The slot and sequence last drawn.
    last: Option<(usize, u32)>,
}

impl Drop for Opened {
    fn drop(&mut self) {
        for o in self.srv.into_iter().chain(self.tex).chain([self.content, self.release]) {
            release(o);
        }
    }
}

struct Renderer {
    hwnd: HWND,
    /// The adapter the device is on.
    luid: Luid,
    dev: usize,
    dev1: usize,
    dev5: usize,
    ctx: usize,
    ctx4: usize,
    swap: usize,
    waitable: HANDLE,
    rtv: usize,
    size: (u32, u32),
    vs: usize,
    ps: usize,
    /// `cbuffer Pane`, and what it holds now.
    cb: usize,
    params: Option<PaneParams>,
    sampler: usize,
    /// The window's monitor as last looked at (the swap chain's format follows it), and
    /// when that was.
    display: Display,
    monitor: usize,
    display_checked: std::time::Instant,
}

unsafe impl Send for Renderer {}

impl Drop for Renderer {
    fn drop(&mut self) {
        if !self.waitable.is_null() {
            unsafe { CloseHandle(self.waitable) };
        }
        if self.ctx != 0 {
            vcall!(self.ctx, 110, fn() -> ()); // ClearState
            vcall!(self.ctx, 111, fn() -> ()); // Flush
        }
        for o in [self.rtv, self.swap, self.sampler, self.cb, self.ps, self.vs, self.ctx4, self.ctx, self.dev5, self.dev1, self.dev] {
            release(o);
        }
    }
}

/// What the window's monitor shows: HDR (Windows runs it in HDR10) or not, and Windows'
/// SDR white level on it in scRGB (1.0 = 80 nits; what SDR content's white is shown at).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Display {
    hdr: bool,
    sdr_white: f32,
}

impl Display {
    const SDR: Display = Display { hdr: false, sdr_white: 1.0 };

    /// The swap chain's format and colour space for it: 16-bit float scRGB on an HDR
    /// display, 8-bit BGRA sRGB otherwise.
    fn swap_format(&self) -> (u32, u32) {
        if self.hdr { (10, peerparty_protocol::frame_ring::COLOR_SPACE_SCRGB) } else { (87, peerparty_protocol::frame_ring::COLOR_SPACE_SRGB) }
    }

    /// `cbuffer Pane` for a pane of frames encoded as `input` with source UV `[u0, v0, u1, v1]`,
    /// source texture dimensions `src_size`, destination pixel rect `dest`, and whether
    /// Contrast Adaptive Sharpening is enabled when upscaling.
    fn params(&self, input: u32, uv: [f32; 4], src_size: (u32, u32), dest: PaneRect, sharpen_on: bool) -> PaneParams {
        let uv_w = (uv[2] - uv[0]).abs();
        let uv_h = (uv[3] - uv[1]).abs();
        let eff_w = (src_size.0 as f32 * uv_w).round() as u32;
        let eff_h = (src_size.1 as f32 * uv_h).round() as u32;
        let upscaling = sharpen_on && src_size.0 > 0 && src_size.1 > 0 && (dest.width > eff_w || dest.height > eff_h);
        PaneParams {
            input,
            output: u32::from(self.hdr),
            sdr_white: self.sdr_white,
            hdr_white: HDR_WHITE_NITS,
            uv_rect: [uv[0], uv[1], uv[2] - uv[0], uv[3] - uv[1]],
            src_inv: [if src_size.0 > 0 { 1.0 / src_size.0 as f32 } else { 0.0 }, if src_size.1 > 0 { 1.0 / src_size.1 as f32 } else { 0.0 }],
            sharpen: if upscaling { CAS_STRENGTH } else { 0.0 },
            _pad: 0.0,
        }
    }
}

const IID_IDXGIOUTPUT6: Guid = Guid(0x068346e8, 0xaaec, 0x4b84, [0xad, 0xd7, 0x13, 0x7f, 0x51, 0x3f, 0x77, 0xa1]);
const IID_IDXGISWAPCHAIN3: Guid = Guid(0x94d99bdb, 0xf1f8, 0x4ab0, [0xb2, 0x36, 0x7d, 0xa0, 0x17, 0x0e, 0xda, 0xb1]);

/// DXGI_OUTPUT_DESC1.
#[repr(C)]
struct OutputDesc1 {
    device_name: [u16; 32],
    desktop: [i32; 4],
    attached: i32,
    rotation: u32,
    monitor: usize,
    bits_per_color: u32,
    color_space: u32,
    primaries: [f32; 8],
    min_luminance: f32,
    max_luminance: f32,
    max_full_frame_luminance: f32,
}

/// What `monitor` shows, from a fresh factory (an old one's outputs keep the colour space
/// they had when it was made).
fn display_of(monitor: usize) -> Display {
    let Some(f) = factory() else { return Display::SDR };
    let mut found = None;
    'adapters: for a in 0..16u32 {
        let mut adapter = 0usize;
        // IDXGIFactory1::EnumAdapters1 (12); IDXGIAdapter::EnumOutputs (7).
        if vcall!(f, 12, fn(u32, *mut usize) -> i32, a, &mut adapter) < 0 {
            break;
        }
        for o in 0..16u32 {
            let mut output = 0usize;
            if vcall!(adapter, 7, fn(u32, *mut usize) -> i32, o, &mut output) < 0 {
                break;
            }
            let out6 = query(output, &IID_IDXGIOUTPUT6);
            release(output);
            if out6 == 0 {
                continue;
            }
            let mut d: OutputDesc1 = unsafe { std::mem::zeroed() };
            // IDXGIOutput6::GetDesc1 is slot 27.
            let ok = vcall!(out6, 27, fn(*mut OutputDesc1) -> i32, &mut d) >= 0;
            release(out6);
            if ok && d.monitor == monitor {
                found = Some(d);
                release(adapter);
                break 'adapters;
            }
        }
        release(adapter);
    }
    release(f);
    let Some(d) = found else { return Display::SDR };
    // DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020: Windows' HDR mode.
    let hdr = d.color_space == peerparty_protocol::frame_ring::COLOR_SPACE_HDR10;
    // 200 nits when Windows won't say: about where it puts SDR content by default.
    let sdr_white = if hdr { sdr_white_level(&d.device_name).unwrap_or(2.5) } else { 1.0 };
    Display { hdr, sdr_white }
}

/// Windows' SDR white level on the display `device` (`\\.\DISPLAY1`), in scRGB.
fn sdr_white_level(device: &[u16; 32]) -> Option<f32> {
    use windows_sys::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
        QueryDisplayConfig,
    };
    let (mut np, mut nm) = (0u32, 0u32);
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) } != 0 {
        return None;
    }
    let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![unsafe { std::mem::zeroed() }; np as usize];
    let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![unsafe { std::mem::zeroed() }; nm as usize];
    if unsafe { QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), std::ptr::null_mut()) } != 0 {
        return None;
    }
    let name = |n: &[u16]| n.iter().take_while(|c| **c != 0).copied().collect::<Vec<u16>>();
    let want = name(device);
    for p in paths.iter().take(np as usize) {
        let mut source: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { std::mem::zeroed() };
        source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        source.header.adapterId = p.sourceInfo.adapterId;
        source.header.id = p.sourceInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 || name(&source.viewGdiDeviceName) != want {
            continue;
        }
        let mut white: DISPLAYCONFIG_SDR_WHITE_LEVEL = unsafe { std::mem::zeroed() };
        white.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
        white.header.size = std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
        white.header.adapterId = p.targetInfo.adapterId;
        white.header.id = p.targetInfo.id;
        // SDRWhiteLevel: 1000 is 80 nits.
        if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } == 0 && white.SDRWhiteLevel > 0 {
            return Some(white.SDRWhiteLevel as f32 / 1000.0);
        }
    }
    None
}

fn monitor_of(hwnd: HWND) -> usize {
    use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) as usize }
}

fn factory() -> Option<usize> {
    type Create = unsafe extern "system" fn(*const Guid, *mut usize) -> i32;
    let create: Create = unsafe { std::mem::transmute(system_proc("dxgi.dll", c"CreateDXGIFactory1")?) };
    let mut f = 0usize;
    (unsafe { create(&IID_IDXGIFACTORY1, &mut f) } >= 0).then_some(f)
}

/// The first adapter's (where frames read back as pixels are drawn).
fn default_luid() -> Option<Luid> {
    let f = factory()?;
    let mut adapter = 0usize;
    // IDXGIFactory1::EnumAdapters1 (12); IDXGIAdapter1::GetDesc1 (10).
    let ok = vcall!(f, 12, fn(u32, *mut usize) -> i32, 0, &mut adapter) >= 0;
    release(f);
    if !ok {
        return None;
    }
    // DXGI_ADAPTER_DESC1: the LUID comes after the description (128 wide chars), four
    // u32s and three sizes.
    let mut d = [0u8; 128 * 2 + 16 + 3 * 8 + 8 + 4 + 8];
    let ok = vcall!(adapter, 10, fn(*mut u8) -> i32, d.as_mut_ptr()) >= 0;
    release(adapter);
    let at = 128 * 2 + 16 + 3 * std::mem::size_of::<usize>();
    ok.then(|| Luid { low: u32::from_le_bytes(d[at..at + 4].try_into().unwrap()), high: i32::from_le_bytes(d[at + 4..at + 8].try_into().unwrap()) })
}

/// The adapter a shared texture lives on.
fn luid_of(handle: HANDLE) -> Option<Luid> {
    let f = factory()?;
    let f2 = query(f, &IID_IDXGIFACTORY2);
    release(f);
    if f2 == 0 {
        return None;
    }
    let mut l = Luid { low: 0, high: 0 };
    // IDXGIFactory2::GetSharedResourceAdapterLuid is slot 17.
    let hr = vcall!(f2, 17, fn(HANDLE, *mut Luid) -> i32, handle, &mut l);
    release(f2);
    (hr >= 0).then_some(l)
}

impl Renderer {
    /// A device on the adapter `luid` and a swap chain on `hwnd`.
    fn new(hwnd: HWND, luid: Luid) -> Option<Renderer> {
        let f = factory()?;
        let f4 = query(f, &IID_IDXGIFACTORY4);
        let f2 = query(f, &IID_IDXGIFACTORY2);
        let mut adapter = 0usize;
        if f4 != 0 {
            // IDXGIFactory4::EnumAdapterByLuid is slot 26.
            vcall!(f4, 26, fn(Luid, *const Guid, *mut usize) -> i32, luid, &IID_IDXGIADAPTER, &mut adapter);
        }
        type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *mut usize, *mut u32, *mut usize) -> i32;
        let create: Option<Create> = system_proc("d3d11.dll", c"D3D11CreateDevice").map(|p| unsafe { std::mem::transmute(p) });
        let mut r = Renderer {
            hwnd,
            luid,
            dev: 0,
            dev1: 0,
            dev5: 0,
            ctx: 0,
            ctx4: 0,
            swap: 0,
            waitable: std::ptr::null_mut(),
            rtv: 0,
            size: (0, 0),
            vs: 0,
            ps: 0,
            cb: 0,
            params: None,
            sampler: 0,
            display: Display::SDR,
            monitor: monitor_of(hwnd),
            display_checked: std::time::Instant::now(),
        };
        r.display = display_of(r.monitor);
        let mut fl = 0u32;
        let ok = adapter != 0
            && create.is_some_and(|c| unsafe { c(adapter, 0, 0, 0x20, std::ptr::null(), 0, 7, &mut r.dev, &mut fl, &mut r.ctx) } >= 0)
            && r.setup(f2);
        for o in [adapter, f2, f4, f] {
            release(o);
        }
        if !ok {
            eprintln!("peerparty: session: the window can't draw the copies' frames itself; their windows are shown instead");
        }
        ok.then_some(r)
    }

    fn setup(&mut self, f2: usize) -> bool {
        self.dev1 = query(self.dev, &IID_ID3D11DEVICE1);
        self.dev5 = query(self.dev, &IID_ID3D11DEVICE5);
        self.ctx4 = query(self.ctx, &IID_ID3D11DEVICECONTEXT4);
        if self.dev1 == 0 || self.dev5 == 0 || self.ctx4 == 0 || f2 == 0 {
            return false;
        }
        let (Some(vs), Some(ps)) = (compile(c"vs", c"vs_4_0"), compile(c"ps", c"ps_4_0")) else { return false };
        let ok = |hr: i32| hr >= 0;
        // D3D11_BUFFER_DESC: 16 bytes, DEFAULT, CONSTANT_BUFFER.
        let cb: [u32; 6] = [std::mem::size_of::<PaneParams>() as u32, 0, 0x4, 0, 0, 0];
        if !ok(vcall!(self.dev, 12, fn(*const u8, usize, usize, *mut usize) -> i32, vs.as_ptr(), vs.len(), 0, &mut self.vs))
            || !ok(vcall!(self.dev, 15, fn(*const u8, usize, usize, *mut usize) -> i32, ps.as_ptr(), ps.len(), 0, &mut self.ps))
            || !ok(vcall!(self.dev, 3, fn(*const u32, usize, *mut usize) -> i32, cb.as_ptr(), 0, &mut self.cb))
        {
            return false;
        }
        // D3D11_SAMPLER_DESC: bilinear, clamped.
        let mut sampler = [0u32; 13];
        sampler[0] = 0x15;
        sampler[1] = 3;
        sampler[2] = 3;
        sampler[3] = 3;
        sampler[5] = 1;
        sampler[6] = 1;
        sampler[12] = f32::MAX.to_bits();
        if !ok(vcall!(self.dev, 23, fn(*const u32, *mut usize) -> i32, sampler.as_ptr(), &mut self.sampler)) {
            return false;
        }
        let (w, h) = client_size(self.hwnd);
        // DXGI_SWAP_CHAIN_DESC1: the display's format, 2 buffers, flip-discard, its
        // latency waitable.
        let (format, space) = self.display.swap_format();
        let desc: [u32; 12] = [w.max(1), h.max(1), format, 0, 1, 0, 0x20, 2, 0, 4, 0, 0x40];
        // IDXGIFactory2::CreateSwapChainForHwnd is slot 15.
        if !ok(vcall!(f2, 15, fn(usize, HWND, *const u32, usize, usize, *mut usize) -> i32, self.dev, self.hwnd, desc.as_ptr(), 0, 0, &mut self.swap)) {
            return false;
        }
        self.set_color_space(space);
        if self.display.hdr {
            eprintln!("peerparty: session: the window's display is in HDR; it draws in scRGB (SDR white {:.0} nits)", self.display.sdr_white * 80.0);
        }
        // No Alt+Enter fullscreen of DXGI's own: the window manages itself.
        vcall!(f2, 8, fn(HWND, u32) -> i32, self.hwnd, 0x1 | 0x2);
        let swap2 = query(self.swap, &IID_IDXGISWAPCHAIN2);
        if swap2 != 0 {
            vcall!(swap2, 31, fn(u32) -> i32, 1); // SetMaximumFrameLatency
            self.waitable = vcall!(swap2, 33, fn() -> HANDLE); // GetFrameLatencyWaitableObject
            release(swap2);
        }
        self.size = (w.max(1), h.max(1));
        true
    }

    /// IDXGISwapChain3::SetColorSpace1 (38), where the swap chain has it.
    fn set_color_space(&self, space: u32) {
        let swap3 = query(self.swap, &IID_IDXGISWAPCHAIN3);
        if swap3 != 0 {
            vcall!(swap3, 38, fn(u32) -> i32, space);
            release(swap3);
        }
    }

    /// Looks at the window's monitor again when it moved to another one or a second has
    /// passed, and moves the swap chain to the format it wants. Whether anything changed
    /// (everything is drawn again).
    fn check_display(&mut self) -> bool {
        let monitor = monitor_of(self.hwnd);
        if monitor == self.monitor && self.display_checked.elapsed() < std::time::Duration::from_secs(1) {
            return false;
        }
        self.monitor = monitor;
        self.display_checked = std::time::Instant::now();
        let d = display_of(monitor);
        if d == self.display {
            return false;
        }
        let was = std::mem::replace(&mut self.display, d);
        self.params = None;
        if was.hdr != d.hdr {
            eprintln!("peerparty: session: the window's display is {}", if d.hdr { "in HDR now; it draws in scRGB" } else { "in SDR now" });
            // Resized at the next `target`, in the new format.
            self.size = (0, 0);
        }
        true
    }

    /// The back buffer's view, the buffers resized first if the window's client area was
    /// (or the display wants another format).
    fn target(&mut self) -> Option<usize> {
        let (w, h) = client_size(self.hwnd);
        if (w, h) != self.size && w > 0 && h > 0 {
            release(self.rtv);
            self.rtv = 0;
            vcall!(self.ctx, 110, fn() -> ());
            let (format, space) = self.display.swap_format();
            // IDXGISwapChain::ResizeBuffers (13), same count, the display's format, the
            // same flags.
            if vcall!(self.swap, 13, fn(u32, u32, u32, u32, u32) -> i32, 0, w, h, format, 0x40) < 0 {
                return None;
            }
            self.set_color_space(space);
            self.size = (w, h);
        }
        if self.rtv == 0 {
            let mut tex = 0usize;
            if vcall!(self.swap, 9, fn(u32, *const Guid, *mut usize) -> i32, 0, &IID_ID3D11TEXTURE2D, &mut tex) < 0 {
                return None;
            }
            vcall!(self.dev, 9, fn(usize, usize, *mut usize) -> i32, tex, 0, &mut self.rtv);
            release(tex);
        }
        (self.rtv != 0).then_some(self.rtv)
    }

    /// Draws the frames `take` says (slot and content-fence value, by pane) into `rtv`:
    /// black, then each pane's frame scaled into its destination. The panes drawn, with
    /// their slots.
    fn draw(&mut self, rtv: usize, panes: &[Pane], take: &[Option<(usize, u64)>]) -> Vec<(usize, usize)> {
        let ctx = self.ctx;
        vcall!(ctx, 50, fn(usize, *const f32) -> (), rtv, [0.0f32, 0.0, 0.0, 1.0].as_ptr());
        vcall!(ctx, 33, fn(u32, *const usize, usize) -> (), 1, &rtv, 0);
        vcall!(ctx, 17, fn(usize) -> (), 0); // IASetInputLayout
        vcall!(ctx, 24, fn(u32) -> (), 5); // TRIANGLESTRIP
        vcall!(ctx, 11, fn(usize, usize, u32) -> (), self.vs, 0, 0);
        vcall!(ctx, 9, fn(usize, usize, u32) -> (), self.ps, 0, 0);
        vcall!(ctx, 16, fn(u32, u32, *const usize) -> (), 0, 1, &self.cb); // PSSetConstantBuffers
        vcall!(ctx, 10, fn(u32, u32, *const usize) -> (), 0, 1, &self.sampler);
        let mut order: Vec<usize> = (0..panes.len()).collect();
        order.sort_by_key(|&i| (panes[i].draw.z, i));
        let mut drawn = Vec::new();
        for i in order {
            let p = &panes[i];
            let (Some(o), Some(&Some((slot, ready)))) = (p.shown(), take.get(i)) else { continue };
            let d = p.draw.dest;
            if d.width == 0 || d.height == 0 {
                continue;
            }
            if ready > 0 && !o.pixels {
                // ID3D11DeviceContext4::Wait (148): the GPU waits for the copy's frame.
                vcall!(self.ctx4, 148, fn(usize, u64) -> i32, o.content, ready);
            }
            let vp: [f32; 6] = [d.x as f32, d.y as f32, d.width as f32, d.height as f32, 0.0, 1.0];
            vcall!(ctx, 44, fn(u32, *const [f32; 6]) -> (), 1, &vp);
            let params = self.display.params(o.input, p.draw.uv, o.surface.size, d, p.draw.sharpen);
            if self.params != Some(params) {
                // ID3D11DeviceContext::UpdateSubresource is slot 48.
                vcall!(ctx, 48, fn(usize, u32, usize, *const PaneParams, u32, u32) -> (), self.cb, 0, 0, &params, 0, 0);
                self.params = Some(params);
            }
            vcall!(ctx, 8, fn(u32, u32, *const usize) -> (), 0, 1, &o.srv[if o.pixels { 0 } else { slot }]);
            vcall!(ctx, 13, fn(u32, u32) -> (), 4, 0);
            drawn.push((i, slot));
        }
        let null = 0usize;
        vcall!(ctx, 8, fn(u32, u32, *const usize) -> (), 0, 1, &null);
        drawn
    }

    /// A pixel pane's frame in `slot` (sequence `seq`) uploaded, unless it already is.
    fn upload(&self, o: &mut Opened, slot: usize, seq: u32) {
        let Some(p) = o.surface.pixels.as_ref().filter(|_| o.uploaded != Some((slot, seq))) else { return };
        let mut mapped = [0usize; 2];
        // ID3D11DeviceContext::Map (14), WRITE_DISCARD; Unmap (15).
        if vcall!(self.ctx, 14, fn(usize, u32, u32, u32, *mut [usize; 2]) -> i32, o.tex[0], 0, 4, 0, &mut mapped) < 0 {
            return;
        }
        let pitch = mapped[1] & 0xffff_ffff;
        let rows = p.slot(slot);
        let n = (p.width * peerparty_protocol::frame_ring::bytes_per_pixel(p.format).unwrap_or(4)) as usize;
        for y in 0..p.height as usize {
            let from = &rows[y * p.stride as usize..y * p.stride as usize + n];
            unsafe { std::ptr::copy_nonoverlapping(from.as_ptr(), (mapped[0] + y * pitch) as *mut u8, n) };
        }
        vcall!(self.ctx, 15, fn(usize, u32) -> (), o.tex[0], 0);
        o.uploaded = Some((slot, seq));
    }

    /// A copy's textures and fences, opened here; for pixels, a texture to upload them to.
    fn open(&self, s: &Arc<SharedSurface>) -> Option<Opened> {
        let format = view_format(s.format);
        let input = input_of(format, s.color_space);
        let mut o = Opened { surface: s.clone(), tex: [0; SLOTS], srv: [0; SLOTS], content: 0, release: 0, released: 0, input, pixels: false, uploaded: None, last: None };
        if let Some(p) = &s.pixels {
            // D3D11_TEXTURE2D_DESC: DYNAMIC, SHADER_RESOURCE, CPU_ACCESS_WRITE.
            let desc: [u32; 11] = [p.width, p.height, 1, 1, format, 1, 0, 2, 0x8, 0x10000, 0];
            if vcall!(self.dev, 5, fn(*const u32, usize, *mut usize) -> i32, desc.as_ptr(), 0, &mut o.tex[0]) < 0
                || vcall!(self.dev, 7, fn(usize, usize, *mut usize) -> i32, o.tex[0], 0, &mut o.srv[0]) < 0
            {
                return None;
            }
            o.pixels = true;
            return Some(o);
        }
        for i in 0..SLOTS {
            // ID3D11Device1::OpenSharedResource1 is slot 48.
            if vcall!(self.dev1, 48, fn(HANDLE, *const Guid, *mut usize) -> i32, s.textures[i], &IID_ID3D11TEXTURE2D, &mut o.tex[i]) < 0 {
                return None;
            }
            // D3D11_SHADER_RESOURCE_VIEW_DESC: TEXTURE2D, mip 0, one level.
            let desc: [u32; 6] = [format, 4, 0, 1, 0, 0];
            if vcall!(self.dev, 7, fn(usize, *const u32, *mut usize) -> i32, o.tex[i], desc.as_ptr(), &mut o.srv[i]) < 0 {
                return None;
            }
        }
        // ID3D11Device5::OpenSharedFence is slot 67.
        for (h, out) in [(s.content, &mut o.content), (s.release, &mut o.release)] {
            if vcall!(self.dev5, 67, fn(HANDLE, *const Guid, *mut usize) -> i32, h, &IID_ID3D11FENCE, out) < 0 {
                return None;
            }
        }
        // Start where the release fence is (another device may have signalled it).
        o.released = vcall!(o.release, 8, fn() -> u64);
        Some(o)
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    let mut r = unsafe { std::mem::zeroed() };
    if unsafe { GetClientRect(hwnd, &mut r) } == 0 {
        return (0, 0);
    }
    ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
}

/// A pane on the drawing thread.
struct Pane {
    draw: PaneDraw,
    opened: Option<Opened>,
    /// The set before `opened`, its last frame shown until `opened` has one of its own
    /// (a copy's new set, after a resize, comes a frame or so before its first frame).
    previous: Option<Opened>,
    /// The surface couldn't be opened here (another adapter): not tried again.
    failed: Option<Arc<SharedSurface>>,
    /// The adapter that surface's textures are on, when it isn't the device's.
    failed_on: Option<Luid>,
    /// How its frames were shown lately (`Shown`).
    shown: Shown,
}

impl Default for Pane {
    fn default() -> Pane {
        Pane {
            draw: PaneDraw { dest: PaneRect { x: 0, y: 0, width: 0, height: 0 }, uv: FULL_UV, z: 0, sharpen: false, ring: None, surface: None },
            opened: None,
            previous: None,
            failed: None,
            failed_on: None,
            shown: Shown::default(),
        }
    }
}

/// How a pane's frames were shown since `since`, said in its copy's log every
/// `SHOWN_EVERY`: an even pane shows each of its frames once; one shown twice and the
/// next never shown is a stutter.
#[derive(Default)]
struct Shown {
    since: Option<std::time::Instant>,
    /// The sequence number last drawn.
    last: Option<u32>,
    new: u32,
    again: u32,
    missed: u32,
    presents: u32,
}

const SHOWN_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

impl Shown {
    /// The window presented with frame `seq` of this pane drawn (None: a frame of an older
    /// set, not counted).
    fn presented(&mut self, seq: Option<u32>) {
        self.presents += 1;
        let Some(seq) = seq else { return };
        match self.last {
            Some(l) if l == seq => self.again += 1,
            Some(l) => {
                self.new += 1;
                // Sequence numbers are 24 bits (`frame_ring::pack`); a jump of more than
                // a few seconds' frames is a ring set up again, not frames missed.
                let gap = seq.wrapping_sub(l) & 0xff_ffff;
                if gap <= 1000 {
                    self.missed += gap - 1;
                }
            }
            None => self.new += 1,
        }
        self.last = Some(seq);
    }

    /// The line for the last `SHOWN_EVERY`, once it's over (and the counts start again).
    fn report(&mut self, now: std::time::Instant) -> Option<String> {
        let since = *self.since.get_or_insert(now);
        let secs = now.duration_since(since).as_secs_f32();
        if secs < SHOWN_EVERY.as_secs_f32() {
            return None;
        }
        let line = format!(
            "{} new frames shown ({:.0} a second), {} shown again while it had none newer, {} it made never shown; the window drew {:.0} a second",
            self.new,
            self.new as f32 / secs,
            self.again,
            self.missed,
            self.presents as f32 / secs
        );
        *self = Shown { since: Some(now), last: self.last, ..Shown::default() };
        Some(line)
    }
}

/// Forgets everything opened on the window's device (it's lost, or being replaced): the
/// copies' textures are opened again on the next one. Nothing of the old device may be
/// drawn or signalled on a new one.
fn reset_gpu(panes: &mut [Pane]) {
    for p in panes {
        p.opened = None;
        p.previous = None;
        p.failed = None;
        p.failed_on = None;
    }
}

impl Pane {
    /// The set drawn: the newest one with a frame (the previous one until the new one
    /// has its first).
    fn shown(&self) -> Option<&Opened> {
        self.opened.as_ref().filter(|o| o.last.is_some()).or(self.previous.as_ref()).or(self.opened.as_ref())
    }
}

/// Each pane's newest frame (`RingHeader::take`), its pixels uploaded: whether any is new,
/// what each pane draws (slot and content-fence value), and whether that's its current
/// set's frame taken now (only those are released: an old set's fence means nothing to
/// the copy now).
/// Whether `take_frames` would find something new to draw: a pane's copy has published
/// a frame not drawn yet, or a pane has moved on from its previous set. Takes and uploads
/// nothing (that's done once, after the wait for room in the swap chain).
fn any_fresh(panes: &[Pane]) -> bool {
    panes.iter().any(|p| {
        let visible = p.draw.dest.width > 0 && p.draw.dest.height > 0;
        match (p.opened.as_ref(), p.draw.ring.as_ref(), visible) {
            (Some(o), Some(ring), true) => {
                ring.header().peek(o.surface.generation).is_some_and(|f| o.last != Some(f)) || (o.last.is_some() && p.previous.is_some())
            }
            _ => false,
        }
    })
}

fn take_frames(r: &mut Renderer, panes: &mut [Pane], dirty: &mut bool) -> (bool, Vec<Option<(usize, u64)>>, Vec<bool>) {
    let mut fresh = false;
    let mut take: Vec<Option<(usize, u64)>> = Vec::with_capacity(panes.len());
    let mut taken: Vec<bool> = Vec::with_capacity(panes.len());
    for p in panes.iter_mut() {
        let mut t = None;
        let mut now = false;
        let visible = p.draw.dest.width > 0 && p.draw.dest.height > 0;
        if let (Some(o), Some(ring), true) = (p.opened.as_mut(), p.draw.ring.as_ref(), visible) {
            match ring.header().take(o.surface.generation) {
                Some(f) => {
                    if o.last != Some((f.slot, f.seq)) {
                        fresh = true;
                    }
                    o.last = Some((f.slot, f.seq));
                    t = Some((f.slot, f.ready));
                    now = true;
                    // Pixels are copied out now: the hook may write the slot once
                    // it's no longer the one taken.
                    r.upload(o, f.slot, f.seq);
                }
                // A newer generation's surface hasn't come yet: the last frame again.
                None => t = o.last.map(|(slot, _)| (slot, 0)),
            }
            if o.last.is_some() && p.previous.take().is_some() {
                *dirty = true;
            }
        }
        if t.is_none()
            && let Some(prev) = p.previous.as_ref()
        {
            // Neither waited on nor released: that set is done with.
            t = prev.last.map(|(slot, _)| (slot, 0));
        }
        take.push(t);
        taken.push(now);
    }
    (fresh, take, taken)
}

fn run(hwnd: usize, shared: &Shared, frames: HANDLE, wake: HANDLE, pause_on_occluded: bool) {
    let hwnd = hwnd as HWND;
    let mut panes: Vec<Pane> = Vec::new();
    let mut seen = u64::MAX;
    let mut gpu: Option<Renderer> = None;
    // The adapter no device could be made on, so it isn't retried every frame.
    let mut failed_luid: Option<Luid> = None;
    // The adapter the next device goes on, when a copy's frames moved to another one.
    let mut prefer: Option<Luid> = None;
    // How many times the device was given up (logged the first few).
    let mut lost = 0u32;
    let mut dirty = true;
    // Since when Present has said the window can't be seen (DXGI_STATUS_OCCLUDED: the
    // screen locked, another window full screen over it): nothing is drawn meanwhile,
    // and a test present every OCCLUDED_RETRY says when it can be again.
    let mut occluded: Option<std::time::Instant> = None;
    loop {
        let events = [frames, wake];
        unsafe { WaitForMultipleObjects(2, events.as_ptr(), 0, 250) };
        if shared.quit.load(Ordering::Acquire) {
            break;
        }
        // What the session wants.
        {
            let w = shared.want.lock().unwrap_or_else(|e| e.into_inner());
            if w.0 != seen {
                seen = w.0;
                panes.resize_with(w.1.len(), Pane::default);
                for (p, d) in panes.iter_mut().zip(w.1.iter()) {
                    let same_surface = match (&p.draw.surface, &d.surface) {
                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                        (None, None) => true,
                        _ => false,
                    };
                    if !same_surface {
                        // A new set: the old one's last frame stays up meanwhile.
                        let old = p.opened.take().filter(|o| o.last.is_some());
                        p.previous = if d.surface.is_some() { old.or(p.previous.take()) } else { None };
                        p.failed = None;
                        p.failed_on = None;
                        if d.surface.is_none()
                            && let Some(r) = &p.draw.ring
                        {
                            if r.header().set_flag(FLAG_COMPOSITOR_LIVE, false) {
                                r.signal_pace();
                            }
                            r.header().stop_reading();
                        }
                    }
                    p.draw = d.clone();
                }
                dirty = true;
            }
        }
        // A device on the copies' adapter, once there's something to draw.
        if gpu.is_none()
            && let Some(luid) = prefer.take().or_else(|| {
                let s = panes.iter().find_map(|p| p.draw.surface.clone())?;
                if s.pixels.is_some() { default_luid() } else { luid_of(s.textures[0]) }
            })
            && failed_luid != Some(luid)
        {
            gpu = Renderer::new(hwnd, luid);
            if gpu.is_none() {
                failed_luid = Some(luid);
            }
            dirty = true;
        }
        let Some(r) = gpu.as_mut() else {
            set_live(shared, &panes, false);
            continue;
        };
        if r.check_display() {
            dirty = true;
        }
        for (i, p) in panes.iter_mut().enumerate() {
            if p.opened.is_none()
                && let Some(s) = p.draw.surface.clone()
                && p.failed.as_ref().is_none_or(|f| !Arc::ptr_eq(f, &s))
            {
                p.opened = r.open(&s);
                if p.opened.is_none() {
                    let on = if s.pixels.is_some() { None } else { luid_of(s.textures[0]).filter(|l| *l != r.luid) };
                    eprintln!(
                        "peerparty: session: copy {}'s frames (set {}, format {}) can't be drawn by the window's device{}; its window is shown instead",
                        i + 1,
                        s.generation,
                        s.format,
                        if on.is_some() { " (they're on another graphics adapter)" } else { "" }
                    );
                    p.failed = Some(s);
                    p.failed_on = on;
                    // Not the old set's last frame forever: the thumbnail takes over.
                    if p.previous.take().is_some() {
                        dirty = true;
                    }
                    if let Some(ring) = &p.draw.ring {
                        if ring.header().set_flag(FLAG_COMPOSITOR_LIVE, false) {
                            ring.signal_pace();
                        }
                        ring.header().stop_reading();
                    }
                }
            }
        }
        // The copies' frames moved to another adapter (a splash screen on one GPU, the
        // game on the other): with nothing drawn from this device, move to that one.
        if panes.iter().all(|p| p.opened.is_none() && p.previous.is_none())
            && let Some(other) = panes.iter().find_map(|p| p.failed_on).filter(|l| failed_luid != Some(*l))
        {
            eprintln!("peerparty: session: the copies' frames are on another graphics adapter now; the window's device moves there");
            reset_gpu(&mut panes);
            gpu = None;
            prefer = Some(other);
            set_live(shared, &panes, false);
            continue;
        }
        // Minimized: nothing is shown, so nothing is taken, uploaded or presented.
        if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsIconic(hwnd) } != 0 {
            set_live(shared, &panes, false);
            continue;
        }
        // Not seen: as minimized, until a test present (DXGI_PRESENT_TEST, nothing shown)
        // says it is again. Without vsync to wait on, drawing would only spin.
        if let Some(since) = occluded {
            if since.elapsed() < OCCLUDED_RETRY || vcall!(r.swap, 8, fn(u32, u32) -> i32, 0, DXGI_PRESENT_TEST) == DXGI_STATUS_OCCLUDED {
                if since.elapsed() >= OCCLUDED_RETRY {
                    occluded = Some(std::time::Instant::now());
                }
                set_live(shared, &panes, false);
                continue;
            }
            eprintln!("peerparty: session: the window can be seen again; drawing resumes");
            occluded = None;
            dirty = true;
        }
        let fresh = any_fresh(&panes);
        set_live(shared, &panes, true);
        if !fresh && !dirty {
            continue;
        }
        let sync_interval = shared.sync_interval.load(Ordering::Acquire).clamp(1, 4);
        if !r.waitable.is_null() {
            unsafe { WaitForSingleObjectEx(r.waitable, 100 * sync_interval, 1) };
        }
        // Taken once there's room (only looked at before the wait): the frame event woke
        // this for one copy's frame, and another copy's of this same refresh, published
        // while waiting, is drawn now. Taken only before the wait, the copy that published
        // second each refresh had its frame drawn a refresh late, shown twice and skipped
        // as the games' timing drifted; taken both times, each frame was copied twice.
        let (_, take, taken) = take_frames(r, &mut panes, &mut dirty);
        let Some(rtv) = r.target() else {
            lost += 1;
            if lost <= 5 {
                eprintln!("peerparty: session: the window's swap chain couldn't be resized; making a new device (time {lost})");
            }
            reset_gpu(&mut panes);
            gpu = None;
            failed_luid = None;
            continue;
        };
        let drawn = r.draw(rtv, &panes, &take);
        // IDXGISwapChain::Present (8): vsync (1 = full, 2 = 1/2, 3 = 1/3 display refresh).
        let hr = vcall!(r.swap, 8, fn(u32, u32) -> i32, sync_interval, 0);
        if pause_on_occluded && hr == DXGI_STATUS_OCCLUDED && occluded.is_none() {
            eprintln!("peerparty: session: the window can't be seen (the screen is locked, or a full-screen window covers it); nothing is drawn until it can");
            occluded = Some(std::time::Instant::now());
            set_live(shared, &panes, false);
        }
        for (i, slot) in drawn {
            let p = &mut panes[i];
            let Some(ring) = p.draw.ring.as_ref() else { continue };
            ring.header().presented();
            ring.signal_pace();
            // The previous set's frame (drawn while the new one has none), or the last
            // frame of a set the copy has moved on from, isn't released.
            let Some(o) = p.opened.as_mut().filter(|o| !o.pixels && taken[i]) else { continue };
            o.released += 1;
            vcall!(r.ctx4, 147, fn(usize, u64) -> i32, o.release, o.released);
            ring.header().release(o.surface.generation, slot, o.released);
        }
        vcall!(r.ctx, 111, fn() -> ());
        dirty = false;
        // Each pane's frames as shown, in its copy's log now and then.
        let now = std::time::Instant::now();
        for (i, p) in panes.iter_mut().enumerate() {
            let Some(o) = p.opened.as_ref() else { continue };
            p.shown.presented(o.last.map(|(_, seq)| seq));
            if let Some(line) = p.shown.report(now) {
                crate::launch::note(o.surface.pid, &format!("session: player {}'s pane over the last {} s: {line}", i + 1, SHOWN_EVERY.as_secs()));
            }
        }
        // DXGI_ERROR_DEVICE_REMOVED / DEVICE_RESET: start over on a new device.
        if hr == 0x887A_0005_u32 as i32 || hr == 0x887A_0007_u32 as i32 {
            eprintln!("peerparty: session: the window's graphics device was lost ({hr:#x}); making a new one");
            set_live(shared, &panes, false);
            reset_gpu(&mut panes);
            gpu = None;
            failed_luid = None;
        }
    }
    for p in &panes {
        if let Some(r) = &p.draw.ring {
            r.header().set_flag(FLAG_COMPOSITOR_LIVE, false);
            r.header().set_flag(FLAG_HIDDEN, false);
            r.signal_pace();
            r.header().stop_reading();
        }
    }
    drop(panes);
    drop(gpu);
}

fn set_live(shared: &Shared, panes: &[Pane], presenting: bool) {
    let live: Vec<bool> = panes.iter().map(|p| p.shown().is_some_and(|o| o.last.is_some())).collect();
    let any_visible = panes.iter().any(|p| p.draw.dest.width > 0 && p.draw.dest.height > 0);
    for (p, &has_frame) in panes.iter().zip(live.iter()) {
        if let Some(ring) = &p.draw.ring {
            let visible = p.draw.dest.width > 0 && p.draw.dest.height > 0;
            let active = presenting && has_frame && visible;
            // Throttle only panes explicitly hidden while another pane is zoomed on screen
            // (never while the whole session window is minimized or occluded).
            let hidden = presenting && any_visible && !visible;
            let was_live = ring.header().set_flag(FLAG_COMPOSITOR_LIVE, active);
            let was_hidden = ring.header().set_flag(FLAG_HIDDEN, hidden);
            if (was_live && !active) || (was_hidden && !hidden) {
                ring.signal_pace();
            }
        }
    }
    let mut l = shared.live.lock().unwrap_or_else(|e| e.into_inner());
    if *l != live {
        *l = live;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane's frames as shown: new ones, one shown twice, one never shown, and the
    /// line once the time is up (the counts start again after it).
    #[test]
    fn a_panes_frames_are_counted_as_shown() {
        let t0 = std::time::Instant::now();
        let mut s = Shown::default();
        assert!(s.report(t0).is_none(), "starts the clock");
        for seq in [1, 2, 2, 4, 5] {
            s.presented(Some(seq));
        }
        s.presented(None);
        assert_eq!((s.new, s.again, s.missed, s.presents), (4, 1, 1, 6));
        assert!(s.report(t0 + std::time::Duration::from_secs(5)).is_none());
        let line = s.report(t0 + SHOWN_EVERY).unwrap();
        assert!(line.starts_with("4 new frames shown"), "{line}");
        assert!(line.contains("1 shown again") && line.contains("1 it made never shown"), "{line}");
        assert_eq!((s.new, s.again, s.missed, s.presents, s.last), (0, 0, 0, 0, Some(5)));
        s.presented(Some(2));
        assert_eq!(s.missed, 0, "back to 2 after 5: a ring set up again, nothing missed");
        s.last = Some(0xff_ffff);
        s.presented(Some(0));
        assert_eq!(s.missed, 0, "0 follows 0xffffff");
        s.presented(Some(3));
        assert_eq!(s.missed, 2);
    }

    /// D3D11_TEXTURE2D_DESC.
    #[repr(C)]
    struct TexDesc {
        width: u32,
        height: u32,
        mips: u32,
        array: u32,
        format: u32,
        samples: u32,
        quality: u32,
        usage: u32,
        bind: u32,
        cpu: u32,
        misc: u32,
    }

    fn surface_again(m: &SurfaceMsg) -> SharedSurface {
        SharedSurface::duplicate(m).expect("handles duplicated")
    }

    /// `tex`'s pixels (BGRA, w x h), through a staging copy.
    fn read(r: &Renderer, tex: usize, w: u32, h: u32) -> Vec<u32> {
        let desc = TexDesc { width: w, height: h, mips: 1, array: 1, format: 87, samples: 1, quality: 0, usage: 3, bind: 0, cpu: 0x20000, misc: 0 };
        let mut staging = 0usize;
        assert!(vcall!(r.dev, 5, fn(*const TexDesc, usize, *mut usize) -> i32, &desc, 0, &mut staging) >= 0);
        vcall!(r.ctx, 47, fn(usize, usize) -> (), staging, tex);
        let mut mapped = [0usize; 2];
        assert!(vcall!(r.ctx, 14, fn(usize, u32, u32, u32, *mut [usize; 2]) -> i32, staging, 0, 1, 0, &mut mapped) >= 0);
        let pitch = mapped[1] & 0xffff_ffff;
        let mut out = Vec::new();
        for y in 0..h as usize {
            out.extend_from_slice(unsafe { std::slice::from_raw_parts((mapped[0] + y * pitch) as *const u32, w as usize) });
        }
        vcall!(r.ctx, 15, fn(usize, u32) -> (), staging, 0);
        release(staging);
        out
    }

    /// The whole way round on WARP, in this process: a "copy" (a device of its own) puts a
    /// frame in a shared texture and publishes it; the window's thread opens the textures
    /// on a device of its own on the same adapter, draws the frame, presents, and signals
    /// the release fence, which the copy's device sees completed.
    #[test]
    fn a_published_frame_is_drawn_and_released() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_POPUP};
        let class = wide("STATIC");
        let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 128, 64, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) };
        assert!(!hwnd.is_null());

        // The copy's side: WARP, three shared textures (the first cleared red) and two fences.
        type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *mut usize, *mut u32, *mut usize) -> i32;
        let create: Create = unsafe { std::mem::transmute(system_proc("d3d11.dll", c"D3D11CreateDevice").unwrap()) };
        let (mut dev, mut fl, mut ctx) = (0usize, 0u32, 0usize);
        assert!(unsafe { create(0, 5, 0, 0x20, std::ptr::null(), 0, 7, &mut dev, &mut fl, &mut ctx) } >= 0);
        let dev5 = query(dev, &IID_ID3D11DEVICE5);
        let ctx4 = query(ctx, &IID_ID3D11DEVICECONTEXT4);
        const IID_IDXGIRESOURCE1: Guid = Guid(0x30961379, 0x4609, 0x4a41, [0x99, 0x8e, 0x54, 0xfe, 0x56, 0x7e, 0xe0, 0xc1]);
        let (w, h) = (32u32, 16u32);
        let desc = TexDesc { width: w, height: h, mips: 1, array: 1, format: 87, samples: 1, quality: 0, usage: 0, bind: 0x28, cpu: 0, misc: 0x802 };
        let mut textures = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..SLOTS {
            let mut t = 0usize;
            assert!(vcall!(dev, 5, fn(*const TexDesc, usize, *mut usize) -> i32, &desc, 0, &mut t) >= 0);
            let r1 = query(t, &IID_IDXGIRESOURCE1);
            let mut hnd: HANDLE = std::ptr::null_mut();
            assert!(vcall!(r1, 13, fn(usize, u32, usize, *mut HANDLE) -> i32, 0, 0x8000_0001, 0, &mut hnd) >= 0);
            release(r1);
            textures.push(t);
            handles.push(hnd as usize as u64);
        }
        let mut rtv = 0usize;
        assert!(vcall!(dev, 9, fn(usize, usize, *mut usize) -> i32, textures[0], 0, &mut rtv) >= 0);
        vcall!(ctx, 50, fn(usize, *const f32) -> (), rtv, [1.0f32, 0.0, 0.0, 1.0].as_ptr());
        let mut fences = [0usize; 2];
        let mut fence_handles = [0u64; 2];
        for i in 0..2 {
            assert!(vcall!(dev5, 68, fn(u64, u32, *const Guid, *mut usize) -> i32, 0, 0x2, &IID_ID3D11FENCE, &mut fences[i]) >= 0);
            let mut hnd: HANDLE = std::ptr::null_mut();
            assert!(vcall!(fences[i], 7, fn(usize, u32, usize, *mut HANDLE) -> i32, 0, 0x1000_0000, 0, &mut hnd) >= 0);
            fence_handles[i] = hnd as usize as u64;
        }
        vcall!(ctx4, 147, fn(usize, u64) -> i32, fences[0], 1);
        vcall!(ctx, 111, fn() -> ());
        let ring = RingBlock::create(std::process::id()).expect("a ring block");
        ring.header().publish(1, 0, 1);

        // The session's side.
        let m = SurfaceMsg {
            pid: std::process::id(),
            generation: 1,
            textures: handles,
            content: fence_handles[0],
            release: fence_handles[1],
            width: w,
            height: h,
            format: 87,
            api: "d3d11".into(),
            color_space: 0,
            pixels: None,
            stride: 0,
        };
        let surface = Arc::new(SharedSurface::duplicate(&m).expect("handles duplicated"));
        // Test window isn't shown on an interactive desktop (and may run on an isolated
        // non-input desktop where Present returns DXGI_STATUS_OCCLUDED).
        let gpu = Gpu::start_for_test(hwnd).expect("the drawing thread");
        gpu.set(vec![PaneDraw { dest: PaneRect { x: 0, y: 0, width: 128, height: 64 }, uv: FULL_UV, z: 0, sharpen: true, ring: Some(ring.clone()), surface: Some(surface) }]);
        let start = std::time::Instant::now();
        while !(gpu.live(0) && ring.header().presents.load(Ordering::SeqCst) > 0) {
            assert!(start.elapsed().as_secs() < 10, "drawn within 10 s");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(ring.header().reading.load(Ordering::SeqCst), 1 << 32, "it samples slot 0 of set 1");
        let released = ring.header().released[0].load(Ordering::SeqCst);
        assert!(released > 0, "slot 0's release noted");
        while vcall!(fences[1], 8, fn() -> u64) < released {
            assert!(start.elapsed().as_secs() < 10, "the copy's device sees the release");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The copy may now write any slot but the one being sampled.
        assert_eq!(ring.header().pick(1, |v| v <= released), Some(1));

        // A new frame in slot 1 wakes the thread through the frame event.
        ring.header().publish(1, 1, 1);
        let event = unsafe { windows_sys::Win32::System::Threading::OpenEventW(0x2, 0, wide(&gpu.event_name).as_ptr()) };
        assert!(!event.is_null(), "the hooks can open the frame event");
        unsafe { SetEvent(event) };
        while ring.header().reading.load(Ordering::SeqCst) != (1 << 32 | 1) {
            assert!(start.elapsed().as_secs() < 10, "slot 1 taken");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(gpu);
        assert_eq!(ring.header().reading.load(Ordering::SeqCst), peerparty_protocol::frame_ring::NONE, "it lets go when it stops");

        // What it draws: the frame scaled into its pane, black around it.
        let mut r = Renderer::new(hwnd, luid_of(surface_again(&m).textures[0]).unwrap()).expect("a renderer");
        // Drawn for an SDR display, whatever this PC's is.
        r.display = Display::SDR;
        let s2 = Arc::new(surface_again(&m));
        let mut pane = Pane::default();
        pane.draw = PaneDraw { dest: PaneRect { x: 32, y: 16, width: 64, height: 32 }, uv: FULL_UV, z: 0, sharpen: true, ring: None, surface: Some(s2.clone()) };
        pane.opened = r.open(&s2);
        assert!(pane.opened.is_some(), "the textures open on the window's device");
        let target_desc = TexDesc { width: 128, height: 64, mips: 1, array: 1, format: 87, samples: 1, quality: 0, usage: 0, bind: 0x20, cpu: 0, misc: 0 };
        let mut target = 0usize;
        assert!(vcall!(r.dev, 5, fn(*const TexDesc, usize, *mut usize) -> i32, &target_desc, 0, &mut target) >= 0);
        let mut trtv = 0usize;
        assert!(vcall!(r.dev, 9, fn(usize, usize, *mut usize) -> i32, target, 0, &mut trtv) >= 0);
        assert_eq!(r.draw(trtv, std::slice::from_ref(&pane), &[Some((0, 1))]), vec![(0, 0)]);
        let px = read(&r, target, 128, 64);
        assert_eq!(px[40 * 128 / 2 + 64] & 0x00ff_ffff, 0xff0000, "red inside the pane");
        assert_eq!(px[8 * 128 + 8] & 0x00ff_ffff, 0, "black outside it");
        assert_eq!(px[16 * 128 + 31] & 0x00ff_ffff, 0, "just left of it");
        assert_eq!(px[16 * 128 + 32] & 0x00ff_ffff, 0xff0000, "its top-left pixel");
        drop(pane);
        for o in [trtv, target] {
            release(o);
        }
        drop(r);
        unsafe {
            CloseHandle(event);
            DestroyWindow(hwnd);
        }
        for o in textures.into_iter().chain(fences).chain([rtv, ctx4, dev5, ctx, dev]) {
            release(o);
        }
    }

    /// PQ's inverse EOTF: nits to a code value.
    fn pq(nits: f32) -> f32 {
        let y = (nits / 10000.0).powf(0.159_301_76);
        ((0.835_937_5 + 18.851_563 * y) / (1.0 + 18.6875 * y)).powf(78.84375)
    }

    fn half(h: u16) -> f32 {
        let (s, e, m) = ((h >> 15) & 1, i32::from((h >> 10) & 0x1f), f32::from(h & 0x3ff));
        let v = match e {
            0 => m * 2f32.powi(-24),
            31 => f32::INFINITY,
            e => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
        };
        if s == 1 { -v } else { v }
    }

    /// One pixel of `tex` (an R16G16B16A16_FLOAT texture, `w x h`).
    fn read_float(r: &Renderer, tex: usize, w: u32, h: u32, x: u32, y: u32) -> [f32; 4] {
        let desc = TexDesc { width: w, height: h, mips: 1, array: 1, format: 10, samples: 1, quality: 0, usage: 3, bind: 0, cpu: 0x20000, misc: 0 };
        let mut staging = 0usize;
        assert!(vcall!(r.dev, 5, fn(*const TexDesc, usize, *mut usize) -> i32, &desc, 0, &mut staging) >= 0);
        vcall!(r.ctx, 47, fn(usize, usize) -> (), staging, tex);
        let mut mapped = [0usize; 2];
        assert!(vcall!(r.ctx, 14, fn(usize, u32, u32, u32, *mut [usize; 2]) -> i32, staging, 0, 1, 0, &mut mapped) >= 0);
        let pitch = mapped[1] & 0xffff_ffff;
        let p = unsafe { std::slice::from_raw_parts((mapped[0] + y as usize * pitch + x as usize * 8) as *const u16, 4) };
        let out = [half(p[0]), half(p[1]), half(p[2]), half(p[3])];
        vcall!(r.ctx, 15, fn(usize, u32) -> (), staging, 0);
        release(staging);
        out
    }

    /// A "copy's" set of shared textures in `format`, each cleared to `color`, and its
    /// fences, on WARP device `dev`: what a hook reports.
    fn shared_set(dev: usize, ctx: usize, format: u32, color: [f32; 4], color_space: u32) -> (SurfaceMsg, Vec<usize>) {
        const IID_IDXGIRESOURCE1: Guid = Guid(0x30961379, 0x4609, 0x4a41, [0x99, 0x8e, 0x54, 0xfe, 0x56, 0x7e, 0xe0, 0xc1]);
        let dev5 = query(dev, &IID_ID3D11DEVICE5);
        let (w, h) = (16u32, 16u32);
        let desc = TexDesc { width: w, height: h, mips: 1, array: 1, format, samples: 1, quality: 0, usage: 0, bind: 0x28, cpu: 0, misc: 0x802 };
        let (mut objects, mut handles) = (Vec::new(), Vec::new());
        for _ in 0..SLOTS {
            let mut t = 0usize;
            assert!(vcall!(dev, 5, fn(*const TexDesc, usize, *mut usize) -> i32, &desc, 0, &mut t) >= 0, "a shared texture in format {format}");
            let mut rtv = 0usize;
            assert!(vcall!(dev, 9, fn(usize, usize, *mut usize) -> i32, t, 0, &mut rtv) >= 0);
            vcall!(ctx, 50, fn(usize, *const f32) -> (), rtv, color.as_ptr());
            release(rtv);
            let r1 = query(t, &IID_IDXGIRESOURCE1);
            let mut hnd: HANDLE = std::ptr::null_mut();
            assert!(vcall!(r1, 13, fn(usize, u32, usize, *mut HANDLE) -> i32, 0, 0x8000_0001, 0, &mut hnd) >= 0);
            release(r1);
            objects.push(t);
            handles.push(hnd as usize as u64);
        }
        let mut fence_handles = [0u64; 2];
        for fh in &mut fence_handles {
            let mut f = 0usize;
            assert!(vcall!(dev5, 68, fn(u64, u32, *const Guid, *mut usize) -> i32, 0, 0x2, &IID_ID3D11FENCE, &mut f) >= 0);
            let mut hnd: HANDLE = std::ptr::null_mut();
            assert!(vcall!(f, 7, fn(usize, u32, usize, *mut HANDLE) -> i32, 0, 0x1000_0000, 0, &mut hnd) >= 0);
            *fh = hnd as usize as u64;
            objects.push(f);
        }
        release(dev5);
        vcall!(ctx, 111, fn() -> ());
        let m = SurfaceMsg {
            pid: std::process::id(),
            generation: 1,
            textures: handles,
            content: fence_handles[0],
            release: fence_handles[1],
            width: w,
            height: h,
            format,
            api: "d3d11".into(),
            color_space,
            pixels: None,
            stride: 0,
        };
        (m, objects)
    }

    /// Pane `set` of `sets` drawn into a new 16x16 target in `format` for `display`.
    fn draw_for(r: &mut Renderer, sets: &[(SurfaceMsg, Vec<usize>)], format: u32, display: Display, set: usize) -> usize {
        r.display = display;
        r.params = None;
        let s = Arc::new(surface_again(&sets[set].0));
        let mut pane = Pane::default();
        pane.draw = PaneDraw { dest: PaneRect { x: 0, y: 0, width: 16, height: 16 }, uv: FULL_UV, z: 0, sharpen: false, ring: None, surface: Some(s.clone()) };
        pane.opened = r.open(&s);
        assert!(pane.opened.is_some(), "set {set} opens");
        let desc = TexDesc { width: 16, height: 16, mips: 1, array: 1, format, samples: 1, quality: 0, usage: 0, bind: 0x20, cpu: 0, misc: 0 };
        let mut t = 0usize;
        assert!(vcall!(r.dev, 5, fn(*const TexDesc, usize, *mut usize) -> i32, &desc, 0, &mut t) >= 0);
        let mut rtv = 0usize;
        assert!(vcall!(r.dev, 9, fn(usize, usize, *mut usize) -> i32, t, 0, &mut rtv) >= 0);
        assert_eq!(r.draw(rtv, std::slice::from_ref(&pane), &[Some((0, 0))]).len(), 1);
        release(rtv);
        t
    }

    fn near(got: f32, want: f32, what: &str) {
        assert!((got - want).abs() <= want.abs() * 0.02 + 0.005, "{what}: {got}, not {want}");
    }

    /// Each kind of frame drawn for each kind of display, on WARP: an HDR10 frame decoded
    /// from PQ to scRGB nits (its BT.2020 red going out of BT.709's gamut, kept) or
    /// tone-mapped to sRGB, an SDR frame at Windows' SDR white or as it is, and a float
    /// (scRGB) frame as it is.
    #[test]
    fn hdr10_and_sdr_frames_on_hdr_and_sdr_displays() {
        use peerparty_protocol::frame_ring::{COLOR_SPACE_HDR10, COLOR_SPACE_SCRGB, COLOR_SPACE_SRGB};
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_POPUP};
        let class = wide("STATIC");
        let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 16, 16, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) };
        type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *mut usize, *mut u32, *mut usize) -> i32;
        let create: Create = unsafe { std::mem::transmute(system_proc("d3d11.dll", c"D3D11CreateDevice").unwrap()) };
        let (mut dev, mut fl, mut ctx) = (0usize, 0u32, 0usize);
        assert!(unsafe { create(0, 5, 0, 0x20, std::ptr::null(), 0, 7, &mut dev, &mut fl, &mut ctx) } >= 0);

        let grey100 = pq(100.0);
        let sets = [
            shared_set(dev, ctx, 24, [grey100, grey100, grey100, 1.0], COLOR_SPACE_HDR10),
            shared_set(dev, ctx, 24, [grey100, 0.0, 0.0, 1.0], COLOR_SPACE_HDR10),
            shared_set(dev, ctx, 24, [pq(203.0), pq(203.0), pq(203.0), 1.0], COLOR_SPACE_HDR10),
            shared_set(dev, ctx, 87, [0.5, 0.5, 0.5, 1.0], COLOR_SPACE_SRGB),
            shared_set(dev, ctx, 10, [2.5, 2.5, 2.5, 1.0], COLOR_SPACE_SCRGB),
        ];
        let mut r = Renderer::new(hwnd, luid_of(surface_again(&sets[0].0).textures[0]).unwrap()).expect("a renderer");
        let hdr = Display { hdr: true, sdr_white: 2.5 };

        // An HDR display (scRGB, 1.0 = 80 nits).
        let t = draw_for(&mut r, &sets, 10, hdr, 0);
        let p = read_float(&r, t, 16, 16, 8, 8);
        for c in &p[..3] {
            near(*c, 100.0 / 80.0, "100-nit grey");
        }
        release(t);
        let t = draw_for(&mut r, &sets, 10, hdr, 1);
        let p = read_float(&r, t, 16, 16, 8, 8);
        near(p[0], 1.660_491 * 100.0 / 80.0, "BT.2020 red's red");
        near(p[1], -0.124_55 * 100.0 / 80.0, "BT.2020 red's green, out of gamut");
        release(t);
        let t = draw_for(&mut r, &sets, 10, hdr, 3);
        near(read_float(&r, t, 16, 16, 8, 8)[1], 0.215_86 * 2.5, "SDR grey at SDR white");
        release(t);
        let t = draw_for(&mut r, &sets, 10, hdr, 4);
        near(read_float(&r, t, 16, 16, 8, 8)[2], 2.5, "scRGB as it is");
        release(t);

        // An SDR display (sRGB-encoded BGRA).
        let px = |r: &Renderer, t: usize| read(r, t, 16, 16)[8 * 16 + 8];
        let t = draw_for(&mut r, &sets, 87, Display::SDR, 2);
        let g = (px(&r, t) >> 8) & 0xff;
        assert!((242..=245).contains(&g), "203-nit grey rolled off just under white: {g}");
        release(t);
        let t = draw_for(&mut r, &sets, 87, Display::SDR, 3);
        assert_eq!(px(&r, t) & 0x00ff_ffff, 0x80_8080, "SDR as it is");
        release(t);
        let t = draw_for(&mut r, &sets, 87, Display::SDR, 0);
        let g = (px(&r, t) >> 8) & 0xff;
        assert!((183..=189).contains(&g), "100-nit grey, under the knee: {g}");
        release(t);

        drop(r);
        for (_, objects) in sets {
            for o in objects {
                release(o);
            }
        }
        release(ctx);
        release(dev);
        unsafe { DestroyWindow(hwnd) };
    }

    /// Frames as pixels (`PEERPARTY_COMPOSITOR=pixels`, a Linux session's way): a pixel
    /// file as a hook writes one, its slot uploaded into the pane's texture and drawn, on
    /// the first adapter's device.
    #[test]
    fn a_pixel_file_pane_is_uploaded_and_drawn() {
        use peerparty_protocol::frame_ring::{pixels_size, slot_bytes};
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_POPUP};
        let dir = std::env::temp_dir().join(format!("peerparty-gpu-pixels-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h) = (16u32, 16u32);
        let stride = w * 4;
        let mut bytes = vec![0u8; pixels_size(stride, h).unwrap()];
        let s1 = slot_bytes(stride, h).unwrap();
        for px in bytes[s1..s1 + (stride * h) as usize].chunks_mut(4) {
            px.copy_from_slice(&[0, 0, 0xff, 0xff]); // BGRA red
        }
        let path = dir.join("pixels-7-1");
        std::fs::write(&path, &bytes).unwrap();
        let m = SurfaceMsg {
            pid: 7,
            generation: 1,
            textures: Vec::new(),
            content: 0,
            release: 0,
            width: w,
            height: h,
            format: 87,
            api: "vulkan".into(),
            color_space: 0,
            pixels: Some(path.to_string_lossy().into_owned()),
            stride,
        };
        let s = Arc::new(SharedSurface::duplicate(&m).expect("the pixel file maps"));
        let class = wide("STATIC");
        let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 16, 16, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) };
        let mut r = Renderer::new(hwnd, default_luid().expect("an adapter")).expect("a renderer");
        r.display = Display::SDR;
        let mut pane = Pane::default();
        pane.draw = PaneDraw { dest: PaneRect { x: 0, y: 0, width: 16, height: 16 }, uv: FULL_UV, z: 0, sharpen: false, ring: None, surface: Some(s.clone()) };
        pane.opened = r.open(&s);
        let o = pane.opened.as_mut().expect("its texture");
        assert!(o.pixels);
        r.upload(o, 1, 5);
        assert_eq!(o.uploaded, Some((1, 5)));
        let desc = TexDesc { width: 16, height: 16, mips: 1, array: 1, format: 87, samples: 1, quality: 0, usage: 0, bind: 0x20, cpu: 0, misc: 0 };
        let mut t = 0usize;
        assert!(vcall!(r.dev, 5, fn(*const TexDesc, usize, *mut usize) -> i32, &desc, 0, &mut t) >= 0);
        let mut rtv = 0usize;
        assert!(vcall!(r.dev, 9, fn(usize, usize, *mut usize) -> i32, t, 0, &mut rtv) >= 0);
        assert_eq!(r.draw(rtv, std::slice::from_ref(&pane), &[Some((1, 0))]), vec![(0, 1)]);
        assert_eq!(read(&r, t, 16, 16)[8 * 16 + 8] & 0x00ff_ffff, 0xff0000, "red from slot 1");
        release(rtv);
        release(t);
        drop(pane);
        drop(r);
        drop(s);
        unsafe { DestroyWindow(hwnd) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inputs_follow_colour_space_and_format() {

        assert_eq!(input_of(24, 12), 2);
        assert_eq!(input_of(24, 0), 0);
        assert_eq!(input_of(10, 0), 1, "float frames are linear light");
        assert_eq!(input_of(87, 10), 1);
        assert_eq!(input_of(87, 0), 0);
        assert_eq!(Display::SDR.swap_format(), (87, 0));
        assert_eq!(Display { hdr: true, sdr_white: 3.0 }.swap_format(), (10, 10));
        // CAS sharpening activates only when enabled AND upscaling.
        let p_up = Display::SDR.params(0, FULL_UV, (640, 360), PaneRect { x: 0, y: 0, width: 960, height: 540 }, true);
        assert!(p_up.sharpen > 0.0);
        let p_off = Display::SDR.params(0, FULL_UV, (640, 360), PaneRect { x: 0, y: 0, width: 960, height: 540 }, false);
        assert_eq!(p_off.sharpen, 0.0);
        let p_native = Display::SDR.params(0, FULL_UV, (960, 540), PaneRect { x: 0, y: 0, width: 960, height: 540 }, true);
        assert_eq!(p_native.sharpen, 0.0, "1:1 native pane skips CAS");
    }

    /// Upscaling a low-res high-contrast pattern with CAS ON sharpens mid-edge contrast
    /// relative to plain bilinear (CAS OFF) while staying strictly bounded in [min, max].
    #[test]
    fn cas_upscaling_sharpens_edges_and_can_be_disabled() {
        use peerparty_protocol::frame_ring::{pixels_size, slot_bytes};
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_POPUP};
        let dir = std::env::temp_dir().join(format!("peerparty-gpu-cas-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (sw, sh) = (8u32, 8u32);
        let stride = sw * 4;
        let mut bytes = vec![0u8; pixels_size(stride, sh).unwrap()];
        let s0 = slot_bytes(stride, sh).unwrap();
        let _ = s0;
        // Left half dark grey (40), right half light grey (215): bilinear blends across the
        // middle columns when scaled 4x to 32x32, while CAS steepens that edge cleanly.
        for y in 0..sh {
            for x in 0..sw {
                let v = if x < sw / 2 { 40u8 } else { 215u8 };
                let off = (y * stride + x * 4) as usize;
                bytes[off..off + 4].copy_from_slice(&[v, v, v, 0xff]);
            }
        }
        let path = dir.join("pixels-cas-1");
        std::fs::write(&path, &bytes).unwrap();
        let m = SurfaceMsg {
            pid: 1,
            generation: 1,
            textures: Vec::new(),
            content: 0,
            release: 0,
            width: sw,
            height: sh,
            format: 87,
            api: "d3d11".into(),
            color_space: 0,
            pixels: Some(path.to_string_lossy().into_owned()),
            stride,
        };
        let s = Arc::new(SharedSurface::duplicate(&m).expect("pixel file maps"));
        let class = wide("STATIC");
        let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 32, 32, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) };
        let mut r = Renderer::new(hwnd, default_luid().expect("adapter")).expect("renderer");
        r.display = Display::SDR;
        let desc = TexDesc { width: 32, height: 32, mips: 1, array: 1, format: 87, samples: 1, quality: 0, usage: 0, bind: 0x20, cpu: 0, misc: 0 };
        let mut target = 0usize;
        assert!(vcall!(r.dev, 5, fn(*const TexDesc, usize, *mut usize) -> i32, &desc, 0, &mut target) >= 0);
        let mut rtv = 0usize;
        assert!(vcall!(r.dev, 9, fn(usize, usize, *mut usize) -> i32, target, 0, &mut rtv) >= 0);

        let mut pane = Pane::default();
        pane.opened = r.open(&s);
        r.upload(pane.opened.as_mut().unwrap(), 0, 1);

        // Draw with CAS OFF (plain bilinear).
        pane.draw = PaneDraw { dest: PaneRect { x: 0, y: 0, width: 32, height: 32 }, uv: FULL_UV, z: 0, sharpen: false, ring: None, surface: Some(s.clone()) };
        r.draw(rtv, std::slice::from_ref(&pane), &[Some((0, 0))]);
        let plain = read(&r, target, 32, 32);

        // Draw with CAS ON.
        pane.draw.sharpen = true;
        r.draw(rtv, std::slice::from_ref(&pane), &[Some((0, 0))]);
        let sharp = read(&r, target, 32, 32);

        // Check mid-row pixels across the vertical edge: every pixel stays bounded in [40, 215],
        // and the transition pixels just left/right of the seam become sharper (higher contrast).
        let row = 16 * 32;
        for x in 0..32 {
            let v = sharp[row + x] & 0xff;
            assert!((40..=215).contains(&v), "no ringing/overshoot at x={x}: {v}");
        }
        let plain_diff = (plain[row + 16] & 0xff) as i32 - (plain[row + 15] & 0xff) as i32;
        let sharp_diff = (sharp[row + 16] & 0xff) as i32 - (sharp[row + 15] & 0xff) as i32;
        assert!(sharp_diff > plain_diff, "CAS increased edge contrast ({sharp_diff} vs {plain_diff})");

        release(rtv);
        release(target);
        drop(pane);
        drop(r);
        drop(s);
        unsafe { DestroyWindow(hwnd) };
        let _ = std::fs::remove_dir_all(&dir);
    }
}

