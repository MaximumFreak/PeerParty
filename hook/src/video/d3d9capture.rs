//! A Direct3D 9 game's frames for the session's window (capture.rs), copied at each
//! present after the overlay is drawn.
//!
//! - A 9Ex device can open a texture another device shares: two of them are made on a
//!   Direct3D 11 device of PeerParty's own on the game's adapter (legacy shared handles,
//!   which are all 9Ex opens), the back buffer is `StretchRect`ed into one, and once an
//!   event query says the game's GPU got there, PeerParty's device copies it into a free
//!   slot of its `D3d11Ring`. No CPU copy, one frame late.
//! - A plain Direct3D 9 device shares nothing: the back buffer is `StretchRect`ed into a
//!   render target and `GetRenderTargetData` queues its copy into system memory; once the
//!   query says that's done, the pixels are locked and handed to `CpuFrames` (uploaded to
//!   shared textures, or written to the session's pixel file). Two in flight, so the game
//!   never waits for a copy: a frame is skipped when both are.
//! - In pixel mode (`capture::pixels`, a game under Wine) every device reads back.
//!
//! Both copy into BGRA (A8R8G8B8): `StretchRect` converts the back buffer's format and
//! resolves a multisampled one.
//!
//! In a split-screen copy the game's Direct3DCreate9(Ex) are hooked when d3d9.dll loads
//! (`redirect`), so the Direct3D 9 hooks are in before its device is made. Plain
//! Direct3D 9 can also be drawn through DXVK instead (opt-in with `PEERPARTY_DXVK`): its
//! frames then come through Vulkan, with no copy on the CPU.

use std::sync::OnceLock;

use retour::GenericDetour;

use super::super::capture::{D3d11Ring, Kind, private_device};
use super::super::pixels::CpuFrames;
use super::com::{self, Guid, query, release, vcall};
use crate::log;

const IID_IDIRECT3DDEVICE9EX: Guid = Guid(0xb18b10ce, 0x2649, 0x405a, [0x87, 0x0f, 0x95, 0xf7, 0x77, 0xd4, 0x31, 0x3a]);
const IID_IDIRECT3D9EX: Guid = Guid(0x02177241, 0x69fc, 0x400c, [0x8f, 0xf1, 0x93, 0xa4, 0x4d, 0xf6, 0x86, 0x1d]);
const IID_IDXGIRESOURCE: Guid = Guid(0x035f3ab4, 0x482e, 0x4e50, [0xb4, 0x1f, 0x8a, 0x7f, 0x8b, 0xd8, 0x96, 0x0b]);

const D3DFMT_A8R8G8B8: u32 = 21;
const DXGI_B8G8R8A8: u32 = 87;
const IN_FLIGHT: usize = 2;

/// D3DSURFACE_DESC.
#[repr(C)]
#[derive(Default)]
struct SurfaceDesc {
    format: u32,
    kind: u32,
    usage: u32,
    pool: u32,
    multisample: u32,
    quality: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct LockedRect {
    pitch: i32,
    bits: *mut u8,
}

/// One copy in flight: the surface the back buffer went into (and, read back, the
/// system-memory one), its query, and whether it's waiting to be taken.
struct InFlight {
    target: usize,
    /// 9Ex: the texture on PeerParty's device it is. Read back: the system-memory surface.
    other: usize,
    query: usize,
    pending: bool,
    /// The order it was made in (the older of two goes first).
    order: u64,
}

enum Way {
    Shared { ring: D3d11Ring, private: (usize, usize) },
    Readback { frames: CpuFrames },
}

/// One device's capture.
pub(super) struct D3d9Capture {
    device: usize,
    size: (u32, u32),
    pixels: bool,
    /// The session's capture setup it was made for (`capture::epoch`).
    epoch: u64,
    way: Way,
    copies: Vec<InFlight>,
    next_order: u64,
}

unsafe impl Send for D3d9Capture {}

impl Drop for D3d9Capture {
    fn drop(&mut self) {
        for c in self.copies.drain(..) {
            release(c.query);
            release(c.target);
            release(c.other);
        }
        if let Way::Shared { private: (dev, ctx), .. } = std::mem::replace(&mut self.way, Way::Readback { frames: CpuFrames::new(None, "d3d9") }) {
            // The ring went first (it holds its own reference to the device).
            release(ctx);
            release(dev);
        }
        release(self.device);
    }
}

/// The adapter a 9Ex device renders on.
fn adapter_luid(device: usize) -> Option<(u32, i32)> {
    let mut d3d = 0usize;
    // IDirect3DDevice9::GetDirect3D (6), GetCreationParameters (9).
    if vcall!(device, 6, fn(*mut usize) -> i32, &mut d3d) < 0 || d3d == 0 {
        return None;
    }
    let ex = query(d3d, &IID_IDIRECT3D9EX);
    release(d3d);
    if ex == 0 {
        return None;
    }
    let mut params = [0usize; 4];
    vcall!(device, 9, fn(*mut usize) -> i32, params.as_mut_ptr());
    let adapter = params[0] as u32;
    // LUID: LowPart, HighPart.
    let mut luid = [0u32; 2];
    // IDirect3D9Ex::GetAdapterLUID is slot 21.
    let hr = vcall!(ex, 21, fn(u32, *mut [u32; 2]) -> i32, adapter, &mut luid);
    release(ex);
    (hr >= 0).then_some((luid[0], luid[1] as i32))
}

impl D3d9Capture {
    /// Capture for `device` at `w x h` (the device AddRef'd here). None when nothing can
    /// be made for it.
    fn new(device: usize, w: u32, h: u32) -> Option<D3d9Capture> {
        let pixels = super::super::capture::pixels();
        let ex = query(device, &IID_IDIRECT3DDEVICE9EX);
        release(ex);
        vcall!(device, 1, fn() -> u32);
        let mut c = D3d9Capture {
            device,
            size: (w, h),
            pixels,
            epoch: super::super::capture::epoch(),
            way: Way::Readback { frames: CpuFrames::new(None, "d3d9") },
            copies: Vec::new(),
            next_order: 0,
        };
        if ex != 0 && !pixels {
            if let Some(shared) = c.make_shared(w, h) {
                log!("video: Direct3D 9Ex at {w}x{h}: its frames are copied on the GPU into textures PeerParty's device shares");
                return Some(shared);
            }
            log!("video: this Direct3D 9Ex device's frames can't be shared with PeerParty's device; they're read back instead");
            c.copies.clear();
        }
        let made = c.make_readback(w, h);
        if made {
            let why = if pixels { "the session takes pixels" } else if ex != 0 { "sharing failed" } else { "a plain Direct3D 9 device can't share textures" };
            log!("video: Direct3D 9 at {w}x{h}: its frames are read back through system memory ({why})");
        }
        made.then_some(c)
    }

    fn make_shared(&mut self, w: u32, h: u32) -> Option<D3d9Capture> {
        let luid = adapter_luid(self.device);
        let (pdev, pctx) = private_device(luid)?;
        let Some(mut ring) = D3d11Ring::new(pdev, w, h, super::super::capture::typeless(DXGI_B8G8R8A8), 0x08 | 0x20) else {
            release(pctx);
            release(pdev);
            return None;
        };
        ring.color_space = 0;
        let mut copies = Vec::new();
        let ok = (0..IN_FLIGHT).all(|_| match shared_copy(self.device, pdev, w, h) {
            Some(c) => {
                copies.push(c);
                true
            }
            None => false,
        });
        if !ok {
            for c in copies {
                release(c.query);
                release(c.target);
                release(c.other);
            }
            drop(ring);
            release(pctx);
            release(pdev);
            return None;
        }
        ring.report("d3d9");
        vcall!(self.device, 1, fn() -> u32);
        Some(D3d9Capture { device: self.device, size: (w, h), pixels: false, epoch: self.epoch, way: Way::Shared { ring, private: (pdev, pctx) }, copies, next_order: 0 })
    }

    fn make_readback(&mut self, w: u32, h: u32) -> bool {
        for _ in 0..IN_FLIGHT {
            let (mut rt, mut sys, mut q) = (0usize, 0usize, 0usize);
            // CreateRenderTarget (28): no multisampling, not lockable.
            let made = vcall!(self.device, 28, fn(u32, u32, u32, u32, u32, i32, *mut usize, usize) -> i32, w, h, D3DFMT_A8R8G8B8, 0, 0, 0, &mut rt, 0) >= 0
                // CreateOffscreenPlainSurface (36) in D3DPOOL_SYSTEMMEM.
                && vcall!(self.device, 36, fn(u32, u32, u32, u32, *mut usize, usize) -> i32, w, h, D3DFMT_A8R8G8B8, 2, &mut sys, 0) >= 0
                // CreateQuery (118): D3DQUERYTYPE_EVENT.
                && vcall!(self.device, 118, fn(u32, *mut usize) -> i32, 8, &mut q) >= 0;
            if !made {
                for o in [rt, sys, q] {
                    release(o);
                }
                log!("video: no surfaces for reading this Direct3D 9 game's frames back");
                return false;
            }
            self.copies.push(InFlight { target: rt, other: sys, query: q, pending: false, order: 0 });
        }
        true
    }

    /// Hands over the copies the game's GPU has finished, oldest first.
    fn collect(&mut self) {
        let mut order: Vec<usize> = (0..self.copies.len()).filter(|&i| self.copies[i].pending).collect();
        order.sort_by_key(|&i| self.copies[i].order);
        let (w, h) = self.size;
        for i in order {
            let c = &self.copies[i];
            // IDirect3DQuery9::GetData (7): S_OK once the GPU is past it.
            if vcall!(c.query, 7, fn(usize, u32, u32) -> i32, 0, 0, 0) != 0 {
                break;
            }
            let taken = match &mut self.way {
                Way::Shared { ring, .. } => match ring.pick() {
                    Some(slot) => {
                        // ID3D11DeviceContext::CopyResource is slot 47.
                        vcall!(ring.ctx, 47, fn(usize, usize) -> (), ring.textures[slot], c.other);
                        ring.finish(slot);
                        true
                    }
                    None => false,
                },
                Way::Readback { frames } => {
                    let mut lr = LockedRect { pitch: 0, bits: std::ptr::null_mut() };
                    // IDirect3DSurface9::LockRect (13), D3DLOCK_READONLY; UnlockRect (14).
                    if vcall!(c.other, 13, fn(*mut LockedRect, usize, u32) -> i32, &mut lr, 0, 0x10) >= 0 {
                        let kind = Kind { width: w, height: h, format: DXGI_B8G8R8A8, color_space: 0 };
                        unsafe { frames.deliver(kind, lr.bits, lr.pitch.max(0) as usize) };
                        vcall!(c.other, 14, fn() -> i32);
                    }
                    true
                }
            };
            if !taken {
                break;
            }
            self.copies[i].pending = false;
        }
    }

    /// Starts copying `back` (the back buffer about to be presented), if a copy is free.
    fn start(&mut self, back: usize) {
        let Some(i) = self.copies.iter().position(|c| !c.pending) else { return };
        let (w, h) = self.size;
        if let Way::Readback { frames } = &mut self.way
            && !frames.wants(Kind { width: w, height: h, format: DXGI_B8G8R8A8, color_space: 0 })
        {
            return;
        }
        let c = &self.copies[i];
        // StretchRect (34): the whole surface, converted (and resolved), no filtering.
        if vcall!(self.device, 34, fn(usize, usize, usize, usize, u32) -> i32, back, 0, c.target, 0, 0) < 0 {
            return;
        }
        // GetRenderTargetData (32) queues the copy into system memory.
        if matches!(self.way, Way::Readback { .. }) && vcall!(self.device, 32, fn(usize, usize) -> i32, c.target, c.other) < 0 {
            return;
        }
        // IDirect3DQuery9::Issue (6), D3DISSUE_END; GetData with D3DGETDATA_FLUSH sends it.
        vcall!(c.query, 6, fn(u32) -> i32, 1);
        vcall!(c.query, 7, fn(usize, u32, u32) -> i32, 0, 0, 1);
        self.next_order += 1;
        let c = &mut self.copies[i];
        c.pending = true;
        c.order = self.next_order;
    }
}

/// A shared copy: a texture on PeerParty's device `pdev`, opened on the game's 9Ex
/// `device` (its level 0 the `StretchRect` target), and a query.
fn shared_copy(device: usize, pdev: usize, w: u32, h: u32) -> Option<InFlight> {
    let desc = super::super::capture::Texture2dDesc {
        width: w,
        height: h,
        mip_levels: 1,
        array_size: 1,
        format: DXGI_B8G8R8A8,
        sample_count: 1,
        sample_quality: 0,
        usage: 0,
        bind_flags: 0x08 | 0x20,
        cpu_access: 0,
        misc: 0x2, // D3D11_RESOURCE_MISC_SHARED: a handle 9Ex can open
    };
    let mut tex11 = 0usize;
    if vcall!(pdev, 5, fn(*const super::super::capture::Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut tex11) < 0 || tex11 == 0 {
        return None;
    }
    let res = query(tex11, &IID_IDXGIRESOURCE);
    let mut handle = 0usize;
    // IDXGIResource::GetSharedHandle is slot 8.
    let ok = res != 0 && vcall!(res, 8, fn(*mut usize) -> i32, &mut handle) >= 0 && handle != 0;
    release(res);
    let (mut tex9, mut surf, mut q) = (0usize, 0usize, 0usize);
    // CreateTexture (23) with the handle opens it: render target, A8R8G8B8, DEFAULT pool.
    let ok = ok
        && vcall!(device, 23, fn(u32, u32, u32, u32, u32, u32, *mut usize, *mut usize) -> i32, w, h, 1, 1, D3DFMT_A8R8G8B8, 0, &mut tex9, &mut handle) >= 0
        // IDirect3DTexture9::GetSurfaceLevel (18).
        && vcall!(tex9, 18, fn(u32, *mut usize) -> i32, 0, &mut surf) >= 0
        && vcall!(device, 118, fn(u32, *mut usize) -> i32, 8, &mut q) >= 0;
    release(tex9);
    if !ok {
        for o in [surf, q, tex11] {
            release(o);
        }
        return None;
    }
    Some(InFlight { target: surf, other: tex11, query: q, pending: false, order: 0 })
}

static CAPTURE: std::sync::Mutex<Option<D3d9Capture>> = std::sync::Mutex::new(None);

/// What the last capture that couldn't be made was for (device, size, pixels, the
/// session's capture setup), so it isn't tried again every frame.
static FAILED_FOR: std::sync::Mutex<Option<(usize, (u32, u32), bool, u64)>> = std::sync::Mutex::new(None);

/// At present, after the overlay: copies `device`'s back buffer for the session.
pub(super) fn capture(device: usize) {
    let mut back = 0usize;
    // GetBackBuffer (18): swap chain 0, buffer 0, mono.
    if vcall!(device, 18, fn(u32, u32, u32, *mut usize) -> i32, 0, 0, 0, &mut back) < 0 || back == 0 {
        return;
    }
    let mut d = SurfaceDesc::default();
    // IDirect3DSurface9::GetDesc is slot 12.
    vcall!(back, 12, fn(*mut SurfaceDesc) -> i32, &mut d);
    let mut g = crate::sys::lock(&CAPTURE);
    let pixels = super::super::capture::pixels();
    let epoch = super::super::capture::epoch();
    if g.as_ref().is_some_and(|c| c.device != device || c.size != (d.width, d.height) || c.pixels != pixels || c.epoch != epoch) {
        *g = None;
    }
    if g.is_none() && d.width > 0 && d.height > 0 {
        let want = (device, (d.width, d.height), pixels, epoch);
        if *crate::sys::lock(&FAILED_FOR) == Some(want) {
            release(back);
            return;
        }
        *g = D3d9Capture::new(device, d.width, d.height);
        if g.is_none() {
            log!("video: no capture for this Direct3D 9 device at {}x{}; not tried again until it or its size changes", d.width, d.height);
            *crate::sys::lock(&FAILED_FOR) = Some(want);
            release(back);
            return;
        }
    }
    if let Some(c) = g.as_mut() {
        c.collect();
        c.start(back);
    }
    release(back);
}

/// Before Reset: default-pool surfaces must go.
pub(super) fn release_all() {
    *crate::sys::lock(&CAPTURE) = None;
}

/// The device's frames go through Vulkan underneath (DXVK) and are copied there: this
/// side's copy goes, or the session would get two sets.
pub(super) fn lower_layer_took_over() {
    if crate::sys::lock(&CAPTURE).take().is_some() {
        log!("video: this Direct3D 9 device's frames go through Vulkan underneath (DXVK): copied there; this side's copy let go");
    }
}

// --- DXVK (opt-in) ----------------------------------------------------------------------

type Create9 = unsafe extern "system" fn(u32) -> usize;
type Create9Ex = unsafe extern "system" fn(u32, *mut usize) -> i32;

static CREATE9: OnceLock<GenericDetour<Create9>> = OnceLock::new();
static CREATE9EX: OnceLock<GenericDetour<Create9Ex>> = OnceLock::new();

/// Where DXVK's d3d9.dll is for this process, from `PEERPARTY_DXVK`: a DXVK release's
/// folder (its x64 and x32 folders inside), one of those folders, or `1` for
/// PeerParty's folder's `dxvk` (`%LOCALAPPDATA%\peerparty\dxvk`).
fn dxvk_dll(setting: &str, peerparty_dir: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let s = setting.trim();
    if s.is_empty() || s == "0" || s.eq_ignore_ascii_case("off") {
        return Vec::new();
    }
    let dir = if s == "1" || s.eq_ignore_ascii_case("on") {
        match peerparty_dir {
            Some(l) => l.join("dxvk"),
            None => return Vec::new(),
        }
    } else {
        std::path::PathBuf::from(s)
    };
    let arch = if cfg!(target_pointer_width = "64") { "x64" } else { "x32" };
    vec![dir.join(arch).join("d3d9.dll"), dir.join("d3d9.dll")]
}

/// Where DXVK's d3d9.dll is, and the game's own d3d9.dll (set by `redirect`).
static DXVK_PATH: OnceLock<(std::path::PathBuf, usize)> = OnceLock::new();

/// The game's d3d9.dll (`system`) loaded in a split-screen copy: its `Direct3DCreate9`
/// and `Direct3DCreate9Ex` are hooked, so the Direct3D 9 hooks go in at the game's first
/// call (`early_hooks`), before it makes its device. Otherwise a game that makes its
/// device fullscreen in its first second, before the session asked for its frames, got a
/// real fullscreen one: Direct3D put its window at the monitor's corner, on top of the
/// session's window. With `PEERPARTY_DXVK` set (not under Wine, where d3d9.dll is already
/// DXVK's or Wine's own), they make DXVK's devices instead, so the game draws through
/// Vulkan and its frames reach the session as Vulkan ones do.
///
/// This runs in the load notification, under the loader lock: only the two entry points
/// are hooked here. DXVK itself is loaded at the game's first call (`dxvk`).
pub fn redirect(system: windows_sys::Win32::Foundation::HMODULE) {
    if !crate::window::compositor_capture() || CREATE9.get().is_some() || CREATE9EX.get().is_some() {
        return;
    }
    if !crate::sys::wine::is_wine()
        && let Ok(setting) = std::env::var(peerparty_protocol::env::DXVK)
    {
        let local = crate::util::local_store::peerparty_dir();
        match dxvk_dll(&setting, local.as_deref()).into_iter().find(|p| p.is_file()) {
            Some(path) => {
                log!("video: Direct3D 9 is to be drawn through DXVK ({}): it loads at the game's first Direct3DCreate9", path.display());
                let _ = DXVK_PATH.set((path, system as usize));
            }
            None => log!("video: PEERPARTY_DXVK is set, but no DXVK d3d9.dll for this game's architecture is there; Direct3D 9 is drawn as it is"),
        }
    }
    let mut n = 0;
    crate::sys::hook::with_lock(|| {
        if let Some(a) = com::proc(system, c"Direct3DCreate9") {
            let target: Create9 = unsafe { std::mem::transmute(a) };
            if let Ok(h) = unsafe { GenericDetour::new(target, create9 as Create9) } {
                let _ = CREATE9.set(h);
                n += usize::from(crate::sys::hook::frozen(a, || unsafe { CREATE9.get().unwrap().enable() }).is_ok());
            }
        }
        if let Some(a) = com::proc(system, c"Direct3DCreate9Ex") {
            let target: Create9Ex = unsafe { std::mem::transmute(a) };
            if let Ok(h) = unsafe { GenericDetour::new(target, create9ex as Create9Ex) } {
                let _ = CREATE9EX.set(h);
                n += usize::from(crate::sys::hook::frozen(a, || unsafe { CREATE9EX.get().unwrap().enable() }).is_ok());
            }
        }
    });
    log!("video: d3d9.dll loaded in a split-screen copy: {n} of 2 entry points hooked; the Direct3D 9 hooks go in at the game's first Direct3DCreate9");
}

thread_local! {
    /// Inside `early_hooks`: its own Direct3DCreate9 (the hooks are found through a
    /// throwaway device) goes straight through.
    static EARLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The game made its first Direct3D 9 object: the present, reset and device-creation
/// hooks go in now, on its thread (outside the loader lock), so the device it makes next
/// is made windowed in its pane and its frames are seen from the first.
fn early_hooks() {
    if EARLY.with(|e| e.replace(true)) {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        log!("video: the game makes its first Direct3D 9 object: its hooks go in before it makes a device");
        super::super::ensure_viewport_hooks();
    });
    EARLY.with(|e| e.set(false));
}

/// DXVK's two entry points, loaded the first time the game makes a Direct3D 9 object
/// (outside the loader lock). None when it didn't load: the game's own d3d9.dll is used.
fn dxvk() -> Option<(usize, usize)> {
    *DXVK.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::{LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW};
        let (path, system) = DXVK_PATH.get()?;
        let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        let dxvk = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
        if dxvk.is_null() || dxvk as usize == *system {
            log!("video: DXVK's d3d9.dll ({}) didn't load; Direct3D 9 is drawn as it is", path.display());
            return None;
        }
        let (Some(c9), Some(c9ex)) = (com::proc(dxvk, c"Direct3DCreate9"), com::proc(dxvk, c"Direct3DCreate9Ex")) else {
            log!("video: DXVK's d3d9.dll ({}) has no Direct3DCreate9; Direct3D 9 is drawn as it is", path.display());
            return None;
        };
        log!("video: Direct3D 9 drawn through DXVK ({})", path.display());
        Some((c9, c9ex))
    })
}

static DXVK: OnceLock<Option<(usize, usize)>> = OnceLock::new();

unsafe extern "system" fn create9(version: u32) -> usize {
    if EARLY.with(|e| e.get()) {
        return unsafe { CREATE9.get().unwrap().call(version) };
    }
    match dxvk() {
        Some((c9, _)) => {
            let f: Create9 = unsafe { std::mem::transmute(c9) };
            unsafe { f(version) }
        }
        None => {
            early_hooks();
            unsafe { CREATE9.get().unwrap().call(version) }
        }
    }
}

unsafe extern "system" fn create9ex(version: u32, out: *mut usize) -> i32 {
    if EARLY.with(|e| e.get()) {
        return unsafe { CREATE9EX.get().unwrap().call(version, out) };
    }
    match dxvk() {
        Some((_, c9ex)) => {
            let f: Create9Ex = unsafe { std::mem::transmute(c9ex) };
            unsafe { f(version, out) }
        }
        None => {
            early_hooks();
            unsafe { CREATE9EX.get().unwrap().call(version, out) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::capture::{self as cap, tests as ct};
    use super::*;
    use windows_sys::Win32::Foundation::HANDLE;

    /// A windowed Direct3D 9 (or 9Ex) device of this PC's on a hidden window, its back
    /// buffer (64x32) filled with `color`. None without a Direct3D 9 driver.
    fn device(ex: bool, color: u32) -> Option<(usize, usize, com::HiddenWindow)> {
        let m = com::load("d3d9.dll");
        let win = com::HiddenWindow::new()?;
        let pp = super::super::PresentParameters {
            width: 64,
            height: 32,
            format: 22, // X8R8G8B8
            count: 1,
            multisample: 0,
            quality: 0,
            swap_effect: 1,
            window: win.0 as usize,
            windowed: 1,
            auto_depth: 0,
            depth_format: 0,
            flags: 0,
            refresh: 0,
            interval: 0x8000_0000,
        };
        let (mut d3d, mut dev) = (0usize, 0usize);
        if ex {
            let create: unsafe extern "system" fn(u32, *mut usize) -> i32 = unsafe { std::mem::transmute(com::proc(m, c"Direct3DCreate9Ex")?) };
            if unsafe { create(32, &mut d3d) } < 0 {
                return None;
            }
            vcall!(d3d, 20, fn(u32, u32, usize, u32, *const super::super::PresentParameters, usize, *mut usize) -> i32, 0, 1, win.0 as usize, 0x20, &pp, 0, &mut dev);
        } else {
            let create: unsafe extern "system" fn(u32) -> usize = unsafe { std::mem::transmute(com::proc(m, c"Direct3DCreate9")?) };
            d3d = unsafe { create(32) };
            vcall!(d3d, 16, fn(u32, u32, usize, u32, *const super::super::PresentParameters, *mut usize) -> i32, 0, 1, win.0 as usize, 0x20, &pp, &mut dev);
        }
        if dev == 0 {
            release(d3d);
            return None;
        }
        let mut back = 0usize;
        vcall!(dev, 18, fn(u32, u32, u32, *mut usize) -> i32, 0, 0, 0, &mut back);
        // ColorFill (35): the whole back buffer.
        vcall!(dev, 35, fn(usize, usize, u32) -> i32, back, 0, color);
        release(back);
        Some((d3d, dev, win))
    }

    /// Captures `dev`'s frames until one is published. (Not presented: another test's
    /// hooks would draw its overlay in.)
    fn until_published(dev: usize) -> peerparty_protocol::frame_ring::Published {
        let start = std::time::Instant::now();
        loop {
            capture(dev);
            if let Some(p) = cap::published() {
                return p;
            }
            assert!(start.elapsed().as_secs() < 5, "a frame published within 5 s");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The pixel at the middle of shared texture `handle` (64x32), opened on a Direct3D
    /// 11 device of the adapter `luid`.
    fn middle(handle: HANDLE, luid: Option<(u32, i32)>) -> u32 {
        let (other, octx) = private_device(luid).expect("a Direct3D 11 device");
        let dev1 = query(other, &com::IID_ID3D11DEVICE1);
        let mut tex = 0usize;
        let hr = vcall!(dev1, 48, fn(HANDLE, *const Guid, *mut usize) -> i32, handle, &com::IID_ID3D11TEXTURE2D, &mut tex);
        assert!(hr >= 0 && tex != 0, "OpenSharedResource1 {hr:#x}");
        let px = super::super::super::d3d11::read_pixels(other, octx, tex, 64, 32);
        for o in [tex, dev1, octx, other] {
            release(o);
        }
        px[16 * 64 + 32] & 0x00ff_ffff
    }

    /// A 9Ex device's back buffer goes through a texture it shares with PeerParty's
    /// device into a slot of the ring, without a CPU copy.
    #[test]
    fn d3d9ex_frames_are_shared() {
        let _turn = crate::sys::lock(&ct::SERIAL);
        let Some((d3d, dev, _win)) = device(true, 0xff00_ff00) else {
            eprintln!("no Direct3D 9Ex device: skipped");
            return;
        };
        let (_map, name) = ct::session_ring();
        cap::configure(true, Some(&name), None);
        let p = until_published(dev);
        let (handle, luid) = {
            let g = crate::sys::lock(&CAPTURE);
            let c = g.as_ref().unwrap();
            let Way::Shared { ring, .. } = &c.way else { panic!("a 9Ex device shares") };
            (ring.handle(p.slot), adapter_luid(dev))
        };
        assert_eq!(middle(handle, luid), 0x00ff00, "green arrived");
        cap::configure(false, None, None);
        release_all();
        release(dev);
        release(d3d);
    }

    /// A plain Direct3D 9 device's back buffer is read back and uploaded into a shared
    /// texture; asked for pixels, it goes into the pixel file instead.
    #[test]
    fn plain_d3d9_frames_are_read_back() {
        let _turn = crate::sys::lock(&ct::SERIAL);
        let Some((d3d, dev, _win)) = device(false, 0xff00_00ff) else {
            eprintln!("no Direct3D 9 device: skipped");
            return;
        };
        let (_map, name) = ct::session_ring();
        cap::configure(true, Some(&name), None);
        let p = until_published(dev);
        let handle = {
            let g = crate::sys::lock(&CAPTURE);
            let c = g.as_ref().unwrap();
            let Way::Readback { frames } = &c.way else { panic!("a plain device reads back") };
            frames.texture_handle(p.slot).expect("uploaded to shared textures")
        };
        assert_eq!(middle(handle, None), 0x0000ff, "blue arrived");
        cap::configure(false, None, None);
        release_all();

        let (ring, dir) = ct::session_ring_file();
        cap::configure_as(true, Some(&ring), None, None, true);
        let p = until_published(dev);
        assert_eq!(p.ready, 0, "pixels need no fence");
        let row = peerparty_protocol::frame_ring::slot_bytes(64 * 4, 32).unwrap();
        let px = ct::pixel_slot(&dir.0, p.slot, row, 64 * 32 * 4);
        let mid = (16 * 64 + 32) * 4;
        assert_eq!(&px[mid..mid + 3], &[0xff, 0, 0], "blue (BGRA) in the pixel file");
        cap::configure(false, None, None);
        release_all();
        release(dev);
        release(d3d);
    }

    #[test]
    fn dxvk_is_found_by_setting() {
        let arch = if cfg!(target_pointer_width = "64") { "x64" } else { "x32" };
        assert!(dxvk_dll("", None).is_empty());
        assert!(dxvk_dll("off", None).is_empty());
        assert!(dxvk_dll("1", None).is_empty(), "no PeerParty folder, nowhere to look");
        let p = dxvk_dll(r"C:\dxvk-2.5", None);
        assert_eq!(p[0], std::path::Path::new(r"C:\dxvk-2.5").join(arch).join("d3d9.dll"));
        assert_eq!(p[1], std::path::Path::new(r"C:\dxvk-2.5").join("d3d9.dll"));
        let p = dxvk_dll("on", Some(std::path::Path::new(r"C:\Users\a\AppData\Local\peerparty")));
        assert_eq!(p[0], std::path::Path::new(r"C:\Users\a\AppData\Local").join("peerparty").join("dxvk").join(arch).join("d3d9.dll"));
    }
}
