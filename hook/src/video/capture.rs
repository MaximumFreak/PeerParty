//! A split-screen copy's frames for the session's compositor (`protocol::frame_ring`).
//!
//! When the session asks (`HostMsg::Capture`, with its ring block and frame event), each
//! frame the game presents is copied on the GPU into one of three shared textures, after
//! the overlay is drawn into it, and the copy's slot is published in the ring block; the
//! session's GPU waits on the content fence before drawing it in the copy's pane. The
//! textures and both fences are NT handles in this process, sent in a `surface` (the
//! session duplicates them out of this process) whenever a new set is made: at the first
//! frame, and when the frames change size or format.
//!
//! - Direct3D 11 (`D3d11Capture`): the textures and fences are made on the game's own
//!   device and the copy goes on its immediate context.
//! - Direct3D 12 (`D3d12Capture`): made on the game's device, copied by a small command
//!   list on its direct queue.
//! - Vulkan and OpenGL (vulkan.rs, gl.rs): made on a Direct3D 11 device of PeerParty's own
//!   on the same adapter (`private_device`), and imported into the game's API.
//!
//! Neither side waits for the other on the CPU: a slot whose release the session's GPU
//! hasn't passed yet is not written (`RingHeader::pick`), and that frame isn't copied.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use peerparty_protocol::frame_ring::{RingHeader, SLOTS};
use peerparty_protocol::ipc::{HookMsg, SurfaceMsg};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

use super::pixels::CpuFrames;
use super::com::{

    Guid, IID_ID3D11DEVICE5, IID_ID3D11DEVICECONTEXT4, IID_ID3D11FENCE, IID_ID3D12COMMANDALLOCATOR, IID_ID3D12FENCE, IID_ID3D12GRAPHICSCOMMANDLIST, IID_ID3D12RESOURCE,
    IID_IDXGIFACTORY1, IID_IDXGIRESOURCE1, IID_IDXGISWAPCHAIN3, query, release, vcall,
};
use crate::log;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// The last generation handed out (each set of textures takes the next).
static GENERATION: AtomicU32 = AtomicU32::new(0);
/// Counts the session's `HostMsg::Capture`s: each one (a new ring block, or the frames
/// turned off and on) gets new sets, reported in a new `surface`, whatever the frames.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Which `HostMsg::Capture` the current setup is (part of every capture's "made for", so
/// a set made before it is never kept).
pub fn epoch() -> u64 {
    EPOCH.load(Ordering::Acquire)
}

/// The session's ring block, frame event, and optional pace event, mapped and opened.
struct Link {
    map: HANDLE,
    view: *mut u8,
    event: HANDLE,
    /// Auto-reset event signalled by the compositor right after `Present(1, 0)`.
    pace: HANDLE,
    /// A ring in a file (`ShmBacking::FileMmap`): its directory, as the session names it
    /// (where pixel files go).
    dir: Option<String>,
}

unsafe impl Send for Link {}
unsafe impl Sync for Link {}

impl Drop for Link {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            CloseHandle(self.map);
            if !self.event.is_null() {
                CloseHandle(self.event);
            }
            if !self.pace.is_null() {
                CloseHandle(self.pace);
            }
        }
    }
}

static LINK: Mutex<Option<Arc<Link>>> = Mutex::new(None);
/// The compositor's `RingHeader::presents` counter as last seen by `pace()`.
static LAST_PACED_PRESENT: AtomicU64 = AtomicU64::new(0);
/// Safety cap (ms) on waiting for the compositor's pace event so a stall or crash can
/// never deadlock a copy.
const PACE_TIMEOUT_MS: u32 = 35;

/// Whether the session wants the frames, and has said where they go.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Whether this copy is a session's whose frames will be wanted, from its start (its
/// `InstanceSpec`): Vulkan's device and swap chains are made ready for it as they're made.
pub fn wanted() -> bool {
    enabled() || crate::window::compositor_capture()
}

/// The frames go to the session as pixels in a file (`HostMsg::Capture`'s `pixels`).
static PIXELS: AtomicBool = AtomicBool::new(false);

/// Whether the session wants the frames as pixels rather than shared textures.
pub fn pixels() -> bool {
    PIXELS.load(Ordering::Acquire)
}

/// The session's `HostMsg::Capture`, for shared textures.
#[cfg(test)]
pub fn configure(on: bool, ring: Option<&str>, event: Option<&str>) {
    configure_as(on, ring, event, None, false);
}

/// The session's `HostMsg::Capture`.
pub fn configure_as(on: bool, ring: Option<&str>, event: Option<&str>, pace: Option<&str>, pixels: bool) {
    PIXELS.store(on && pixels, Ordering::Release);
    let link = if on { ring.and_then(|r| open_link(r, event, pace)).map(Arc::new) } else { None };
    let ok = link.is_some();
    *crate::sys::lock(&LINK) = link;
    LAST_PACED_PRESENT.store(0, Ordering::Release);
    // A new setup gets new sets: none of the old ones is drawn or reported again.
    CURRENT.store(0, Ordering::Release);
    crate::sys::lock(&REPORTED).clear();
    let epoch = EPOCH.fetch_add(1, Ordering::AcqRel) + 1;
    let was = ENABLED.swap(ok, Ordering::AcqRel);
    if was && ok {
        log!("video: the session set up the frames again (setup {epoch}): new textures are made and reported at the next frame");
    }
    if was != ok || on != ok {
        log!(
            "video: frames for the session's window {}",
            match (on, ok) {
                (true, true) if pixels => "on (pixels in a file)",
                (true, true) => "on (shared textures)",
                (true, false) => "asked for, but its block couldn't be opened",
                _ => "off",
            }
        );
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The ring's mapping: a named one (a plain name or `ShmBacking::WinMapping`), or one of
/// the file `ShmBacking::FileMmap` names (a Linux path under Wine).
fn open_ring(ring: &str) -> Option<(HANDLE, Option<String>)> {
    use peerparty_protocol::transport::ShmBacking;
    use windows_sys::Win32::System::Memory::{FILE_MAP_ALL_ACCESS, OpenFileMappingW, PAGE_READWRITE};
    let Some(ShmBacking::FileMmap { path }) = ShmBacking::parse(ring) else {
        let name = match ShmBacking::parse(ring) {
            Some(ShmBacking::WinMapping { name }) => name,
            _ => ring.to_string(),
        };
        let map = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide(&name).as_ptr()) };
        return (!map.is_null()).then_some((map, None));
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    let local = crate::sys::wine::windows_path(&path).into_owned();
    let file = unsafe {
        CreateFileW(
            wide(&local).as_ptr(),
            0x8000_0000 | 0x4000_0000,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if file == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return None;
    }
    let map = unsafe { windows_sys::Win32::System::Memory::CreateFileMappingW(file, std::ptr::null(), PAGE_READWRITE, 0, 0, std::ptr::null()) };
    unsafe { CloseHandle(file) };
    let dir = path.rfind(['/', '\\']).map(|i| path[..i].to_string()).filter(|d| !d.is_empty());
    (!map.is_null()).then_some((map, dir))
}

fn open_link(ring: &str, event: Option<&str>, pace: Option<&str>) -> Option<Link> {
    use windows_sys::Win32::System::Memory::{FILE_MAP_ALL_ACCESS, MapViewOfFile};
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW};
    let (map, dir) = open_ring(ring)?;
    let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, peerparty_protocol::frame_ring::BLOCK_SIZE) }.Value as *mut u8;
    if view.is_null() {
        unsafe { CloseHandle(map) };
        return None;
    }
    let event = event.map_or(std::ptr::null_mut(), |e| unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, wide(e).as_ptr()) });
    // SYNCHRONIZE (0x0010_0000) | EVENT_MODIFY_STATE (0x2) so we can wait and drain.
    let pace = pace.map_or(std::ptr::null_mut(), |e| unsafe { OpenEventW(0x0010_0000 | EVENT_MODIFY_STATE, 0, wide(e).as_ptr()) });
    let link = Link { map, view, event, pace, dir };
    let ok = unsafe { RingHeader::from_raw(link.view, peerparty_protocol::frame_ring::BLOCK_SIZE) }.is_some_and(|h| h.valid());
    ok.then_some(link)
}

/// Whether compositor-locked frame pacing is active for this copy right now.
pub fn sync_active() -> bool {
    if !enabled() {
        return false;
    }
    let Some(link) = crate::sys::lock(&LINK).clone() else { return false };
    !link.pace.is_null()
        && unsafe { RingHeader::from_raw(link.view, peerparty_protocol::frame_ring::BLOCK_SIZE) }.is_some_and(|h| h.sync_active())
}

/// Paces this copy to the compositor's refresh boundary when "Sync games to screen" is
/// active: returns immediately (0 ms) if the compositor already presented since our last
/// paced frame (slow game), otherwise waits for the compositor's next `Present(1, 0)`
/// with a `PACE_TIMEOUT_MS` safety cap.
pub fn pace() {
    if !enabled() {
        return;
    }
    let Some(link) = crate::sys::lock(&LINK).clone() else { return };
    if link.pace.is_null() {
        return;
    }
    let Some(h) = (unsafe { RingHeader::from_raw(link.view, peerparty_protocol::frame_ring::BLOCK_SIZE) }) else {
        return;
    };
    if !h.sync_active() {
        return;
    }
    let now = h.presents.load(Ordering::Acquire);
    let prev = LAST_PACED_PRESENT.load(Ordering::Acquire);
    if now != prev {
        LAST_PACED_PRESENT.store(now, Ordering::Release);
        // Drain any stale signal so the next fast frame waits cleanly for a fresh VBlank.
        unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(link.pace, 0) };
        return;
    }
    unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(link.pace, PACE_TIMEOUT_MS) };
    LAST_PACED_PRESENT.store(h.presents.load(Ordering::Acquire), Ordering::Release);
}

/// Where pixel files go: the ring file's directory, as the session names it (None when
/// the ring is a named mapping).
pub fn pixel_dir() -> Option<String> {
    crate::sys::lock(&LINK).as_ref()?.dir.clone()
}

/// Runs `f` on the ring block, if the session gave one.
fn with_ring<T>(f: impl FnOnce(&RingHeader) -> T) -> Option<T> {
    let l = crate::sys::lock(&LINK);
    let link = l.as_ref()?;
    let h = unsafe { RingHeader::from_raw(link.view, peerparty_protocol::frame_ring::BLOCK_SIZE) }?;
    Some(f(h))
}

/// A free slot of `generation` (`RingHeader::pick`), given how far the release fence got.
pub fn pick(generation: u32, release_completed: u64) -> Option<usize> {
    pick_except(generation, release_completed, |_| false)
}

/// `pick`, never a slot `taken` names (a copy into it still under way, not published yet).
pub fn pick_except(generation: u32, release_completed: u64, taken: impl Fn(usize) -> bool) -> Option<usize> {
    with_ring(|r| r.pick_except(generation, |v| v <= release_completed, taken)).flatten()
}

/// The set the session draws: the last one reported (0: none yet this setup), and when a
/// frame of it was last published (`now_ms`).
static CURRENT: AtomicU32 = AtomicU32::new(0);
static CURRENT_AT: AtomicU64 = AtomicU64::new(0);
/// The sets reported lately, to report one again (`take_over`).
static REPORTED: Mutex<Vec<SurfaceMsg>> = Mutex::new(Vec::new());
const KEEP_REPORTED: usize = 8;
/// How long the set the session draws may go without a frame before another set the game
/// presents is drawn instead.
const TAKE_OVER_MS: u64 = 250;

fn now_ms() -> u64 {
    static START: std::sync::LazyLock<std::time::Instant> = std::sync::LazyLock::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}

/// Sends a set's `surface`, and makes it the set the session draws.
pub(super) fn send_surface(m: SurfaceMsg) {
    CURRENT.store(m.generation, Ordering::Release);
    CURRENT_AT.store(now_ms(), Ordering::Release);
    {
        let mut r = crate::sys::lock(&REPORTED);
        r.retain(|s| s.generation != m.generation);
        if r.len() >= KEEP_REPORTED {
            r.remove(0);
        }
        r.push(m.clone());
    }
    if crate::ipc::connected() {
        crate::ipc::send(HookMsg::Surface(m));
    }
}

/// Whether a frame of `generation` goes to the session. A game with two swap chains (one
/// made for a fullscreen switch, say) can go back to presenting the first, whose set was
/// reported before the other's and isn't made again: the session draws only the set
/// reported last, so it kept the other's last frame for good. Once the set it draws has
/// had no frame for `TAKE_OVER_MS`, a set still presenting is reported again and drawn.
fn take_over(generation: u32) -> bool {
    let now = now_ms();
    let cur = CURRENT.load(Ordering::Acquire);
    if cur == 0 || cur == generation {
        CURRENT_AT.store(now, Ordering::Release);
        return true;
    }
    let quiet = now.saturating_sub(CURRENT_AT.load(Ordering::Acquire));
    if quiet < TAKE_OVER_MS {
        return false;
    }
    let again = crate::sys::lock(&REPORTED).iter().find(|m| m.generation == generation).cloned();
    let Some(m) = again else {
        crate::log::throttled("video set gone", 4, |_| log!("gap: video: set {generation} presents but its report is gone (set {cur} is drawn): its frames aren't shown"));
        return false;
    };
    crate::log::throttled("video set again", 8, |n| {
        log!("video: the game presents set {generation} ({}x{}) again and set {cur} had no frame for {quiet} ms: the session's window draws set {generation} (switch {n})", m.width, m.height)
    });
    send_surface(m);
    true
}

/// The copy into `slot` is submitted, finished at content-fence value `ready`: the
/// session is told. The event is set while the link is held: a `HostMsg::Capture` closing
/// it meanwhile would leave a handle that may already be another object's.
pub fn publish(generation: u32, slot: usize, ready: u64) {
    if !take_over(generation) {
        return;
    }
    let l = crate::sys::lock(&LINK);
    let Some(link) = l.as_ref() else { return };
    if let Some(h) = unsafe { RingHeader::from_raw(link.view, peerparty_protocol::frame_ring::BLOCK_SIZE) } {
        h.publish(generation, slot, ready);
    }
    if !link.event.is_null() {
        unsafe { windows_sys::Win32::System::Threading::SetEvent(link.event) };
    }
}

/// The next generation number.
pub fn next_generation() -> u32 {
    GENERATION.fetch_add(1, Ordering::AcqRel).wrapping_add(1).max(1)
}

/// The handle sets of the two generations before the current one, kept open so the
/// session can still duplicate them if its `surface` came late.
static RETIRED: Mutex<Vec<Vec<usize>>> = Mutex::new(Vec::new());

/// Lets go of a generation's handles, two generations later.
fn retire(handles: Vec<HANDLE>) {
    if handles.is_empty() {
        return;
    }
    let mut r = crate::sys::lock(&RETIRED);
    r.push(handles.into_iter().map(|h| h as usize).collect());
    while r.len() > 2 {
        for h in r.remove(0) {
            unsafe { CloseHandle(h as HANDLE) };
        }
    }
}

/// What a set of frames is: its size, `DXGI_FORMAT` and colour space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Kind {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub color_space: u32,
}

/// Tells the session about a new set of textures.
fn report(generation: u32, textures: &[HANDLE], content: HANDLE, release: HANDLE, k: Kind, api: &str) {
    let Kind { width, height, format, color_space } = k;
    log!("video: frames for the session's window in {width}x{height} textures (format {format}, colour space {color_space}, {api}, set {generation})");
    send_surface(SurfaceMsg {
        pid: std::process::id(),
        generation,
        textures: textures.iter().map(|h| *h as usize as u64).collect(),
        content: content as usize as u64,
        release: release as usize as u64,
        width,
        height,
        format,
        api: api.to_string(),
        color_space,
        pixels: None,
        stride: 0,
    });
}

/// The typeless format of a back buffer's format family, so a copy works whichever view
/// format (sRGB or not) the game gave its buffers; the compositor reads it as UNORM.
pub fn typeless(format: u32) -> u32 {
    match format {
        27..=29 => 27, // R8G8B8A8
        87 | 90 | 91 => 90, // B8G8R8A8
        88 | 92 | 93 => 92, // B8G8R8X8
        23..=25 => 23, // R10G10B10A2
        9..=14 => 9,   // R16G16B16A16
        f => f,
    }
}

/// The typed format of a typeless one (for resolving multisampled buffers).
fn typed(format: u32) -> u32 {
    match format {
        27 => 28,
        90 => 87,
        92 => 88,
        23 => 24,
        9 => 10,
        f => f,
    }
}

#[repr(C)]
pub(super) struct Texture2dDesc {
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    pub array_size: u32,
    pub format: u32,
    pub sample_count: u32,
    pub sample_quality: u32,
    pub usage: u32,
    pub bind_flags: u32,
    pub cpu_access: u32,
    pub misc: u32,
}

/// D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.
const MISC_SHARED_NT: u32 = 0x2 | 0x800;
/// DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE.
const SHARED_READ_WRITE: u32 = 0x8000_0000 | 1;
const GENERIC_ALL: u32 = 0x1000_0000;

/// Three shared textures and the two fences of one generation, on a Direct3D 11 device.
pub struct D3d11Ring {
    pub device: usize,
    pub ctx: usize,
    ctx4: usize,
    pub textures: [usize; SLOTS],
    content: usize,
    release_fence: usize,
    handles: Vec<HANDLE>,
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    /// How the frames' values are encoded (`frame_ring::COLOR_SPACE_*`).
    pub color_space: u32,
    /// The content fence's last value signalled.
    value: u64,
}

unsafe impl Send for D3d11Ring {}

impl D3d11Ring {
    /// A set on `device` (AddRef'd here) for frames of `width x height` in `format`, with
    /// `bind` flags on the textures. None when the device can't share fences (before
    /// Windows 10 1703, or a driver without them): the copy stays on its window.
    pub fn new(device: usize, width: u32, height: u32, format: u32, bind: u32) -> Option<D3d11Ring> {
        let dev5 = query(device, &IID_ID3D11DEVICE5);
        if dev5 == 0 {
            log!("video: this Direct3D 11 device can't share fences; the session shows the copy's window instead");
            return None;
        }
        vcall!(device, 1, fn() -> u32);
        let mut ctx = 0usize;
        vcall!(device, 40, fn(*mut usize) -> (), &mut ctx);
        let ctx4 = query(ctx, &IID_ID3D11DEVICECONTEXT4);
        let mut r = D3d11Ring {
            device,
            ctx,
            ctx4,
            textures: [0; SLOTS],
            content: 0,
            release_fence: 0,
            handles: Vec::new(),
            generation: next_generation(),
            width,
            height,
            format,
            color_space: 0,
            value: 0,
        };
        let ok = r.ctx4 != 0 && r.make(dev5, bind);
        release(dev5);
        ok.then_some(r)
    }

    fn make(&mut self, dev5: usize, bind: u32) -> bool {
        let desc = Texture2dDesc {
            width: self.width,
            height: self.height,
            mip_levels: 1,
            array_size: 1,
            format: self.format,
            sample_count: 1,
            sample_quality: 0,
            usage: 0,
            bind_flags: bind,
            cpu_access: 0,
            misc: MISC_SHARED_NT,
        };
        for i in 0..SLOTS {
            let mut tex = 0usize;
            let hr = vcall!(self.device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut tex);
            if hr < 0 || tex == 0 {
                log!("video: a shared {}x{} texture (format {}) couldn't be made ({hr:#x})", self.width, self.height, self.format);
                return false;
            }
            self.textures[i] = tex;
            let res = query(tex, &IID_IDXGIRESOURCE1);
            if res == 0 {
                return false;
            }
            let mut h: HANDLE = std::ptr::null_mut();
            // IDXGIResource1::CreateSharedHandle is slot 13.
            let hr = vcall!(res, 13, fn(usize, u32, usize, *mut HANDLE) -> i32, 0, SHARED_READ_WRITE, 0, &mut h);
            release(res);
            if hr < 0 || h.is_null() {
                log!("video: a shared texture's handle couldn't be made ({hr:#x})");
                return false;
            }
            self.handles.push(h);
        }
        for fence in [&mut self.content, &mut self.release_fence] {
            // ID3D11Device5::CreateFence is slot 68: initial value, D3D11_FENCE_FLAG_SHARED.
            let hr = vcall!(dev5, 68, fn(u64, u32, *const Guid, *mut usize) -> i32, 0, 0x2, &IID_ID3D11FENCE, fence);
            if hr < 0 || *fence == 0 {
                log!("video: a shared fence couldn't be made ({hr:#x})");
                return false;
            }
            let mut h: HANDLE = std::ptr::null_mut();
            // ID3D11Fence::CreateSharedHandle is slot 7.
            if vcall!(*fence, 7, fn(usize, u32, usize, *mut HANDLE) -> i32, 0, GENERIC_ALL, 0, &mut h) < 0 || h.is_null() {
                return false;
            }
            self.handles.push(h);
        }
        true
    }

    /// Slot `i`'s texture handle (for importing it into another API).
    pub fn handle(&self, i: usize) -> HANDLE {
        self.handles[i]
    }

    /// Tells the session about this set.
    pub fn report(&self, api: &str) {
        let k = Kind { width: self.width, height: self.height, format: self.format, color_space: self.color_space };
        report(self.generation, &self.handles[..SLOTS], self.handles[SLOTS], self.handles[SLOTS + 1], k, api);
    }

    /// How far the session's GPU got through the textures (its release fence).
    pub fn released(&self) -> u64 {
        // ID3D11Fence::GetCompletedValue is slot 8.
        vcall!(self.release_fence, 8, fn() -> u64)
    }

    /// A slot to copy the next frame into, or None to skip this one.
    pub fn pick(&self) -> Option<usize> {
        pick(self.generation, self.released())
    }

    /// `pick`, never a slot `taken` names.
    pub fn pick_except(&self, taken: impl Fn(usize) -> bool) -> Option<usize> {
        pick_except(self.generation, self.released(), taken)
    }

    /// The copy into `slot` was queued on this device's context: signal the content fence
    /// after it, send it to the GPU, and publish the slot.
    pub fn finish(&mut self, slot: usize) {
        self.value += 1;
        // ID3D11DeviceContext4::Signal is slot 147; ID3D11DeviceContext::Flush is 111.
        vcall!(self.ctx4, 147, fn(usize, u64) -> i32, self.content, self.value);
        vcall!(self.ctx, 111, fn() -> ());
        publish(self.generation, slot, self.value);
    }
}

impl Drop for D3d11Ring {
    fn drop(&mut self) {
        for t in self.textures {
            release(t);
        }
        release(self.content);
        release(self.release_fence);
        release(self.ctx4);
        release(self.ctx);
        release(self.device);
        retire(std::mem::take(&mut self.handles));
    }
}

/// A Direct3D 11 game's frames: copied (or resolved) on its own device and context.
#[derive(Default)]
pub struct D3d11Capture {
    ring: Option<D3d11Ring>,
    /// The device, frames and session setup (`epoch`) the last ring was made for, also
    /// when it failed (so a failure isn't retried every frame).
    made_for: (usize, Kind, u64),
    /// Pixel mode's (`pixels`), and what the last one that couldn't be made was for.
    readback: Option<D3d11Readback>,
    readback_failed: Option<(usize, (u32, u32, u32, u32), u32, u64)>,
}

unsafe impl Send for D3d11Capture {}

/// A back buffer format's twin for its pixels: typed (a typeless buffer's bytes are its
/// UNORM ones), and sRGB views hold the same bytes as their UNORM twins.
fn pixel_twin(format: u32) -> u32 {
    match typed(format) {
        29 => 28,
        91 => 87,
        93 => 88,
        f => f,
    }
}

/// Pixel mode for Direct3D 11: two staging textures the back buffer is copied (or
/// resolved, through a texture of its own) into, mapped without waiting once the GPU is
/// done, their rows handed to `CpuFrames`. A frame is skipped while both are in flight.
pub struct D3d11Readback {
    device: usize,
    ctx: usize,
    src: (u32, u32, u32, u32),
    kind: Kind,
    /// Per staging texture: it, and the order it was filled in (0: free).
    staging: [(usize, u64); 2],
    resolve: usize,
    next: u64,
    frames: CpuFrames,
}

unsafe impl Send for D3d11Readback {}

impl D3d11Readback {
    /// For back buffers like `src` on `device` (AddRef'd). None when it can't be read back.
    fn new(device: usize, src: &Texture2dDesc, color_space: u32) -> Option<D3d11Readback> {
        let format = pixel_twin(src.format);
        peerparty_protocol::frame_ring::bytes_per_pixel(format)?;
        vcall!(device, 1, fn() -> u32);
        let mut ctx = 0usize;
        vcall!(device, 40, fn(*mut usize) -> (), &mut ctx);
        let mut r = D3d11Readback {
            device,
            ctx,
            src: (src.width, src.height, src.format, src.sample_count),
            kind: Kind { width: src.width, height: src.height, format, color_space },
            staging: [(0, 0); 2],
            resolve: 0,
            next: 0,
            frames: CpuFrames::new(None, "d3d11"),
        };
        // STAGING, CPU_ACCESS_READ, in the back buffer's own format.
        let staging = Texture2dDesc {
            width: src.width,
            height: src.height,
            mip_levels: 1,
            array_size: 1,
            format: src.format,
            sample_count: 1,
            sample_quality: 0,
            usage: 3,
            bind_flags: 0,
            cpu_access: 0x20000,
            misc: 0,
        };
        for s in &mut r.staging {
            if vcall!(device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &staging, 0, &mut s.0) < 0 {
                return None;
            }
        }
        if src.sample_count > 1 {
            let plain = Texture2dDesc { usage: 0, cpu_access: 0, ..staging };
            if vcall!(device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &plain, 0, &mut r.resolve) < 0 {
                return None;
            }
        }
        Some(r)
    }

    fn capture(&mut self, backbuffer: usize) {
        // Hand over what's done, oldest first: D3D11_MAP_READ with DO_NOT_WAIT answers
        // DXGI_ERROR_WAS_STILL_DRAWING until then.
        let mut order: Vec<usize> = (0..2).filter(|&i| self.staging[i].1 != 0).collect();
        order.sort_by_key(|&i| self.staging[i].1);
        for i in order {
            #[repr(C)]
            struct Mapped {
                data: *const u8,
                row_pitch: u32,
                depth_pitch: u32,
            }
            let mut m = Mapped { data: std::ptr::null(), row_pitch: 0, depth_pitch: 0 };
            // ID3D11DeviceContext::Map (14), Unmap (15).
            if vcall!(self.ctx, 14, fn(usize, u32, u32, u32, *mut Mapped) -> i32, self.staging[i].0, 0, 1, 0x10_0000, &mut m) < 0 {
                break;
            }
            unsafe { self.frames.deliver(self.kind, m.data, m.row_pitch as usize) };
            vcall!(self.ctx, 15, fn(usize, u32) -> (), self.staging[i].0, 0);
            self.staging[i].1 = 0;
        }
        let Some(i) = (0..2).find(|&i| self.staging[i].1 == 0) else { return };
        if !self.frames.wants(self.kind) {
            return;
        }
        let dst = self.staging[i].0;
        if self.resolve != 0 {
            let format = if typeless(self.src.2) == self.src.2 { typed(self.src.2) } else { self.src.2 };
            vcall!(self.ctx, 57, fn(usize, u32, usize, u32, u32) -> (), self.resolve, 0, backbuffer, 0, format);
            vcall!(self.ctx, 47, fn(usize, usize) -> (), dst, self.resolve);
        } else {
            vcall!(self.ctx, 47, fn(usize, usize) -> (), dst, backbuffer);
        }
        self.next += 1;
        self.staging[i].1 = self.next;
    }
}

impl Drop for D3d11Readback {
    fn drop(&mut self) {
        for (t, _) in self.staging {
            release(t);
        }
        release(self.resolve);
        release(self.ctx);
        release(self.device);
    }
}

impl D3d11Capture {
    /// Copies `backbuffer` (an ID3D11Texture2D of `device`, its values in `color_space`)
    /// into a free slot.
    pub fn capture(&mut self, device: usize, backbuffer: usize, color_space: u32) {
        if device == 0 || backbuffer == 0 {
            return;
        }
        let mut src: Texture2dDesc = unsafe { std::mem::zeroed() };
        // ID3D11Texture2D::GetDesc is slot 10.
        vcall!(backbuffer, 10, fn(*mut Texture2dDesc) -> (), &mut src);
        if src.width == 0 || src.height == 0 {
            return;
        }
        if pixels() {
            self.ring = None;
            let shape = (src.width, src.height, src.format, src.sample_count);
            let same = |r: &D3d11Readback| r.device == device && r.src == shape && r.kind.color_space == color_space;
            let want = (device, shape, color_space, epoch());
            if !self.readback.as_ref().is_some_and(same) && self.readback_failed != Some(want) {
                self.readback = D3d11Readback::new(device, &src, color_space);
                if self.readback.is_none() {
                    log!("video: this Direct3D 11 game's frames (format {}) can't be read back; not tried again until they change", src.format);
                    self.readback_failed = Some(want);
                }
            }
            if let Some(r) = self.readback.as_mut() {
                r.capture(backbuffer);
            }
            return;
        }
        self.readback = None;
        let fmt = typeless(src.format);
        let want = (device, Kind { width: src.width, height: src.height, format: fmt, color_space }, epoch());
        if self.made_for != want {
            self.ring = None;
            self.made_for = want;
            self.ring = D3d11Ring::new(device, src.width, src.height, fmt, 0x08 | 0x20);
            if let Some(r) = &mut self.ring {
                r.color_space = color_space;
                r.report("d3d11");
            }
        }
        let Some(r) = self.ring.as_mut() else { return };
        let Some(slot) = r.pick() else { return };
        let dst = r.textures[slot];
        if src.sample_count > 1 {
            let resolve = if src.format == fmt { typed(fmt) } else { src.format };
            // ID3D11DeviceContext::ResolveSubresource is slot 57.
            vcall!(r.ctx, 57, fn(usize, u32, usize, u32, u32) -> (), dst, 0, backbuffer, 0, resolve);
        } else {
            // ID3D11DeviceContext::CopyResource is slot 47.
            vcall!(r.ctx, 47, fn(usize, usize) -> (), dst, backbuffer);
        }
        r.finish(slot);
    }
}

// --- Direct3D 12 ----------------------------------------------------------------------

#[repr(C)]
struct HeapProperties {
    kind: u32,
    cpu_page: u32,
    pool: u32,
    creation_mask: u32,
    visible_mask: u32,
}

#[repr(C)]
struct ResourceDesc {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth: u16,
    mips: u16,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    layout: u32,
    flags: u32,
}

#[repr(C)]
struct Barrier {
    kind: u32,
    flags: u32,
    resource: usize,
    subresource: u32,
    before: u32,
    after: u32,
}

const STATE_COMMON: u32 = 0;
const STATE_COPY_DEST: u32 = 0x400;
const STATE_COPY_SOURCE: u32 = 0x800;

fn barrier(resource: usize, before: u32, after: u32) -> Barrier {
    // D3D12_RESOURCE_BARRIER_TYPE_TRANSITION, all subresources.
    Barrier { kind: 0, flags: 0, resource, subresource: 0xffff_ffff, before, after }
}

/// A Direct3D 12 game's frames: three shared committed textures and two shared fences on
/// its device, and a ring of command lists that copy the presented buffer into a slot on
/// its direct queue.
pub struct D3d12Capture {
    device: usize,
    queue: usize,
    textures: [usize; SLOTS],
    content: usize,
    release_fence: usize,
    handles: Vec<HANDLE>,
    /// (allocator, command list, the content value its last copy finishes at).
    lists: Vec<(usize, usize, u64)>,
    generation: u32,
    width: u32,
    height: u32,
    format: u32,
    value: u64,
}

unsafe impl Send for D3d12Capture {}

impl D3d12Capture {
    fn new(device: usize, queue: usize, width: u32, height: u32, format: u32) -> Option<D3d12Capture> {
        vcall!(device, 1, fn() -> u32);
        vcall!(queue, 1, fn() -> u32);
        let mut c = D3d12Capture {
            device,
            queue,
            textures: [0; SLOTS],
            content: 0,
            release_fence: 0,
            handles: Vec::new(),
            lists: Vec::new(),
            generation: next_generation(),
            width,
            height,
            format,
            value: 0,
        };
        c.make().then_some(c)
    }

    fn make(&mut self) -> bool {
        let heap = HeapProperties { kind: 1, cpu_page: 0, pool: 0, creation_mask: 0, visible_mask: 0 }; // DEFAULT
        let desc = ResourceDesc {
            dimension: 3, // TEXTURE2D
            alignment: 0,
            width: u64::from(self.width),
            height: self.height,
            depth: 1,
            mips: 1,
            format: self.format,
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags: 0x1 | 0x20, // ALLOW_RENDER_TARGET | ALLOW_SIMULTANEOUS_ACCESS
        };
        for i in 0..SLOTS {
            let mut tex = 0usize;
            // ID3D12Device::CreateCommittedResource is slot 27 (heap flag SHARED).
            let hr = vcall!(
                self.device,
                27,
                fn(*const HeapProperties, u32, *const ResourceDesc, u32, usize, *const Guid, *mut usize) -> i32,
                &heap,
                0x1,
                &desc,
                STATE_COMMON,
                0,
                &IID_ID3D12RESOURCE,
                &mut tex
            );
            if hr < 0 || tex == 0 {
                log!("video: a shared Direct3D 12 texture couldn't be made ({hr:#x})");
                return false;
            }
            self.textures[i] = tex;
        }
        for fence in [&mut self.content, &mut self.release_fence] {
            // ID3D12Device::CreateFence is slot 36 (D3D12_FENCE_FLAG_SHARED).
            let hr = vcall!(self.device, 36, fn(u64, u32, *const Guid, *mut usize) -> i32, 0, 0x1, &IID_ID3D12FENCE, fence);
            if hr < 0 || *fence == 0 {
                log!("video: a shared Direct3D 12 fence couldn't be made ({hr:#x})");
                return false;
            }
        }
        let objects: Vec<usize> = self.textures.iter().copied().chain([self.content, self.release_fence]).collect();
        for o in objects {
            let mut h: HANDLE = std::ptr::null_mut();
            // ID3D12Device::CreateSharedHandle is slot 31.
            let hr = vcall!(self.device, 31, fn(usize, usize, u32, usize, *mut HANDLE) -> i32, o, 0, GENERIC_ALL, 0, &mut h);
            if hr < 0 || h.is_null() {
                log!("video: a Direct3D 12 shared handle couldn't be made ({hr:#x})");
                return false;
            }
            self.handles.push(h);
        }
        for _ in 0..SLOTS {
            let (mut alloc, mut list) = (0usize, 0usize);
            // ID3D12Device::CreateCommandAllocator (9) and CreateCommandList (12), DIRECT.
            if vcall!(self.device, 9, fn(u32, *const Guid, *mut usize) -> i32, 0, &IID_ID3D12COMMANDALLOCATOR, &mut alloc) < 0 {
                log!("video: no Direct3D 12 command allocator for copying frames");
                return false;
            }
            if vcall!(self.device, 12, fn(u32, u32, usize, usize, *const Guid, *mut usize) -> i32, 0, 0, alloc, 0, &IID_ID3D12GRAPHICSCOMMANDLIST, &mut list) < 0 {
                log!("video: no Direct3D 12 command list for copying frames");
                release(alloc);
                return false;
            }
            // A new list is open: closed until it's used.
            vcall!(list, 9, fn() -> i32);
            self.lists.push((alloc, list, 0));
        }
        true
    }

    fn completed(fence: usize) -> u64 {
        // ID3D12Fence::GetCompletedValue is slot 8.
        vcall!(fence, 8, fn() -> u64)
    }

    /// Copies `buffer` (the back buffer about to be presented) into a free slot.
    fn capture(&mut self, buffer: usize) {
        let done = Self::completed(self.content);
        let Some(li) = self.lists.iter().position(|l| l.2 <= done) else { return };
        let Some(slot) = pick(self.generation, Self::completed(self.release_fence)) else { return };
        let (alloc, list, _) = self.lists[li];
        // ID3D12CommandAllocator::Reset (8); ID3D12GraphicsCommandList::Reset (10).
        if vcall!(alloc, 8, fn() -> i32) < 0 || vcall!(list, 10, fn(usize, usize) -> i32, alloc, 0) < 0 {
            return;
        }
        let dst = self.textures[slot];
        let before = [barrier(buffer, STATE_COMMON, STATE_COPY_SOURCE), barrier(dst, STATE_COMMON, STATE_COPY_DEST)];
        let after = [barrier(buffer, STATE_COPY_SOURCE, STATE_COMMON), barrier(dst, STATE_COPY_DEST, STATE_COMMON)];
        // ResourceBarrier (26), CopyResource (17), Close (9).
        vcall!(list, 26, fn(u32, *const Barrier) -> (), 2, before.as_ptr());
        vcall!(list, 17, fn(usize, usize) -> (), dst, buffer);
        vcall!(list, 26, fn(u32, *const Barrier) -> (), 2, after.as_ptr());
        if vcall!(list, 9, fn() -> i32) < 0 {
            return;
        }
        // ID3D12CommandQueue::ExecuteCommandLists (10), then Signal (14).
        vcall!(self.queue, 10, fn(u32, *const usize) -> (), 1, &list);
        self.value += 1;
        vcall!(self.queue, 14, fn(usize, u64) -> i32, self.content, self.value);
        self.lists[li].2 = self.value;
        publish(self.generation, slot, self.value);
    }
}

impl Drop for D3d12Capture {
    fn drop(&mut self) {
        // The lists may still be running: wait (briefly) for the last copy.
        let last = self.value;
        if last > 0 && Self::completed(self.content) < last {
            let ev = unsafe { windows_sys::Win32::System::Threading::CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
            if !ev.is_null() {
                // ID3D12Fence::SetEventOnCompletion is slot 9.
                vcall!(self.content, 9, fn(u64, HANDLE) -> i32, last, ev);
                unsafe {
                    windows_sys::Win32::System::Threading::WaitForSingleObject(ev, 1000);
                    CloseHandle(ev);
                }
            }
        }
        if last > 0 && Self::completed(self.content) < last {
            // Still running (a hung GPU, or a device being removed): its allocators,
            // lists and textures are left alive rather than freed under the GPU.
            log!("video: a Direct3D 12 frame copy didn't finish in a second; its objects are left alive rather than freed while in use");
            self.lists.clear();
            retire(std::mem::take(&mut self.handles));
            return;
        }
        for (alloc, list, _) in self.lists.drain(..) {
            release(list);
            release(alloc);
        }
        for t in self.textures {
            release(t);
        }
        release(self.content);
        release(self.release_fence);
        release(self.queue);
        release(self.device);
        retire(std::mem::take(&mut self.handles));
    }
}

/// A Direct3D 12 swap chain's capture: remade when its buffers change.
#[derive(Default)]
pub struct D3d12Frames {
    cap: Option<D3d12Capture>,
    /// Device, queue, frames and session setup (`epoch`) the capture was made for.
    made_for: (usize, usize, Kind, u64),
}

unsafe impl Send for D3d12Frames {}

impl D3d12Frames {
    /// Copies the buffer `swap` is about to present (its values in `color_space`), on
    /// `queue` of `device`.
    pub fn capture(&mut self, device: usize, queue: usize, swap: usize, color_space: u32) {
        let swap3 = query(swap, &IID_IDXGISWAPCHAIN3);
        if swap3 == 0 {
            crate::log::throttled("video: d3d12 no swapchain3", 1, |_| log!("video: this Direct3D 12 swap chain has no IDXGISwapChain3; its frames can't be copied"));
            return;
        }
        // IDXGISwapChain3::GetCurrentBackBufferIndex (36); IDXGISwapChain::GetBuffer (9).
        let index = vcall!(swap3, 36, fn() -> u32);
        let mut buffer = 0usize;
        let hr = vcall!(swap3, 9, fn(u32, *const Guid, *mut usize) -> i32, index, &IID_ID3D12RESOURCE, &mut buffer);
        release(swap3);
        if hr < 0 || buffer == 0 {
            return;
        }
        let mut desc: ResourceDesc = unsafe { std::mem::zeroed() };
        // ID3D12Resource::GetDesc (slot 10) returns its struct through a hidden pointer.
        vcall!(buffer, 10, fn(*mut ResourceDesc) -> *mut ResourceDesc, &mut desc);
        let (w, h, fmt) = (desc.width as u32, desc.height, typeless(desc.format));
        let kind = Kind { width: w, height: h, format: fmt, color_space };
        // The queue too: copies go on the queue the frames are presented on.
        let want = (device, queue, kind, epoch());
        if self.made_for != want {
            self.cap = None;
            self.made_for = want;
            if desc.sample_count > 1 {
                log!("video: this Direct3D 12 swap chain is multisampled; the session shows the copy's window instead");
            }
            if w > 0 && h > 0 && desc.sample_count <= 1 {
                self.cap = D3d12Capture::new(device, queue, w, h, fmt);
                if let Some(c) = &self.cap {
                    report(c.generation, &c.handles[..SLOTS], c.handles[SLOTS], c.handles[SLOTS + 1], kind, "d3d12");
                }
            }
        }
        if let Some(c) = self.cap.as_mut() {
            c.capture(buffer);
        }
        release(buffer);
    }
}

// --- A device of PeerParty's own, for Vulkan and OpenGL games ---------------------------

#[repr(C)]
struct AdapterDesc {
    description: [u16; 128],
    vendor: u32,
    device: u32,
    subsys: u32,
    revision: u32,
    video_memory: usize,
    system_memory: usize,
    shared_memory: usize,
    luid_low: u32,
    luid_high: i32,
}

/// A Direct3D 11 device (and its immediate context) on the adapter with `luid` (the
/// default one when None or not found).
pub fn private_device(luid: Option<(u32, i32)>) -> Option<(usize, usize)> {
    let m = super::com::load("d3d11.dll");
    type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *mut usize, *mut u32, *mut usize) -> i32;
    let create: Create = unsafe { std::mem::transmute(super::com::proc(m, c"D3D11CreateDevice")?) };
    let adapter = luid.and_then(adapter_by_luid).unwrap_or(0);
    let (mut dev, mut fl, mut ctx) = (0usize, 0u32, 0usize);
    // D3D_DRIVER_TYPE_UNKNOWN with an adapter, HARDWARE without; BGRA_SUPPORT.
    let hr = unsafe { create(adapter, if adapter != 0 { 0 } else { 1 }, 0, 0x20, std::ptr::null(), 0, 7, &mut dev, &mut fl, &mut ctx) };
    release(adapter);
    if hr < 0 || dev == 0 {
        log!("video: no Direct3D 11 device for sharing frames ({hr:#x})");
        return None;
    }
    Some((dev, ctx))
}

fn adapter_by_luid((low, high): (u32, i32)) -> Option<usize> {
    let m = super::com::load("dxgi.dll");
    type CreateFactory = unsafe extern "system" fn(*const Guid, *mut usize) -> i32;
    let create: CreateFactory = unsafe { std::mem::transmute(super::com::proc(m, c"CreateDXGIFactory1")?) };
    let mut factory = 0usize;
    if unsafe { create(&IID_IDXGIFACTORY1, &mut factory) } < 0 {
        return None;
    }
    let mut found = None;
    for i in 0..16u32 {
        let mut a = 0usize;
        // IDXGIFactory::EnumAdapters (7); IDXGIAdapter::GetDesc (8).
        if vcall!(factory, 7, fn(u32, *mut usize) -> i32, i, &mut a) < 0 {
            break;
        }
        let mut d: AdapterDesc = unsafe { std::mem::zeroed() };
        if vcall!(a, 8, fn(*mut AdapterDesc) -> i32, &mut d) >= 0 && d.luid_low == low && d.luid_high == high {
            found = Some(a);
            break;
        }
        release(a);
    }
    release(factory);
    found
}

/// The newest frame published (tests).
#[cfg(test)]
pub fn published() -> Option<peerparty_protocol::frame_ring::Published> {
    with_ring(|r| r.published()).flatten()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::com;
    use super::*;

    fn warp() -> (usize, usize) {
        let m = com::load("d3d11.dll");
        type Create = unsafe extern "system" fn(usize, u32, usize, u32, *const u32, u32, u32, *mut usize, *mut u32, *mut usize) -> i32;
        let create: Create = unsafe { std::mem::transmute(com::proc(m, c"D3D11CreateDevice").unwrap()) };
        let (mut device, mut ctx, mut fl) = (0usize, 0usize, 0u32);
        let hr = unsafe { create(0, 5, 0, 0x20, std::ptr::null(), 0, 7, &mut device, &mut fl, &mut ctx) };
        assert!(hr >= 0, "D3D11CreateDevice {hr:#x}");
        (device, ctx)
    }

    /// The tests that link to a ring block take turns (the link is one per process).
    pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

    /// A ring block of this process's own, as the session makes one.
    pub(crate) fn session_ring() -> (HANDLE, String) {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE};
        let name = format!(r"Local\peerparty-ring-test-{}-{}", std::process::id(), next_generation());
        let map = unsafe { CreateFileMappingW(windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, 4096, wide(&name).as_ptr()) };
        assert!(!map.is_null());
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, 4096) }.Value as *mut u8;
        unsafe { RingHeader::from_raw(view, 4096) }.unwrap().init();
        (map, name)
    }

    /// A ring block in a file of its own (`ShmBacking::FileMmap`), as a Linux session
    /// makes one: its `file:` name, and the file's directory (deleted after).
    pub(crate) fn session_ring_file() -> (String, TempDir) {
        let dir = std::env::temp_dir().join(format!("peerparty-ring-test-{}-{}", std::process::id(), next_generation()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ring");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let (map, _) = open_ring(&format!("file:{}", path.display())).expect("the ring file maps");
        let view = unsafe { windows_sys::Win32::System::Memory::MapViewOfFile(map, windows_sys::Win32::System::Memory::FILE_MAP_ALL_ACCESS, 0, 0, 4096) }.Value as *mut u8;
        unsafe { RingHeader::from_raw(view, 4096) }.unwrap().init();
        (format!("file:{}", path.display()), TempDir(dir))
    }

    /// A directory removed when dropped.
    pub(crate) struct TempDir(pub std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The pixel file a test's frames went into: its slot `slot`'s first `n` bytes.
    pub(crate) fn pixel_slot(dir: &std::path::Path, slot: usize, slot_bytes: usize, n: usize) -> Vec<u8> {
        let f = std::fs::read_dir(dir).unwrap().filter_map(Result::ok).find(|e| e.file_name().to_string_lossy().starts_with("pixels-")).expect("a pixel file");
        let bytes = std::fs::read(f.path()).unwrap();
        bytes[slot * slot_bytes..slot * slot_bytes + n].to_vec()
    }

    /// A game that goes back to an earlier swap chain: its set, reported before the
    /// other's, is drawn again once the other has gone quiet, and not before.
    #[test]
    fn an_earlier_set_presenting_again_is_drawn_again() {
        let _turn = crate::sys::lock(&SERIAL);
        let (_map, name) = session_ring();
        configure(true, Some(&name), None);
        let (a, b) = (next_generation(), next_generation());
        for g in [a, b] {
            send_surface(SurfaceMsg { generation: g, width: 4, height: 4, ..SurfaceMsg::default() });
        }
        publish(a, 0, 1);
        assert!(published().is_none(), "set b was just reported: a's frame isn't drawn");
        publish(b, 1, 1);
        assert_eq!(published().map(|p| p.generation), Some(b));
        std::thread::sleep(std::time::Duration::from_millis(TAKE_OVER_MS + 50));
        publish(a, 0, 2);
        assert_eq!(published().map(|p| p.generation), Some(a), "b went quiet: a is reported again and drawn");
        assert_eq!(CURRENT.load(Ordering::Acquire), a);
        publish(b, 2, 2);
        assert_eq!(published().map(|p| p.generation), Some(a), "b presenting once more doesn't take it straight back");
        configure(false, None, None);
        assert_eq!(CURRENT.load(Ordering::Acquire), 0, "a new setup draws no old set");
    }

    /// A Direct3D 11 frame goes into a shared texture another device opens: the pixels

    /// arrive, the content fence reaches the published value, and the slots go round.
    #[test]
    fn d3d11_frames_reach_another_device() {
        let _turn = crate::sys::lock(&SERIAL);
        let (_map, name) = session_ring();
        configure(true, Some(&name), None);
        assert!(enabled());
        let (device, ctx) = warp();
        let (w, h) = (32u32, 16u32);
        let desc = Texture2dDesc { width: w, height: h, mip_levels: 1, array_size: 1, format: 87, sample_count: 1, sample_quality: 0, usage: 0, bind_flags: 0x20, cpu_access: 0, misc: 0 };
        let mut back = 0usize;
        assert!(vcall!(device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut back) >= 0);
        let mut rtv = 0usize;
        assert!(vcall!(device, 9, fn(usize, usize, *mut usize) -> i32, back, 0, &mut rtv) >= 0);
        vcall!(ctx, 50, fn(usize, *const f32) -> (), rtv, [0.0f32, 1.0, 0.0, 1.0].as_ptr());

        let mut cap = D3d11Capture::default();
        cap.capture(device, back, 0);
        let ring = cap.ring.as_ref().expect("a ring on WARP");
        assert_eq!((ring.width, ring.height, ring.format), (w, h, 90), "typeless BGRA");
        let p = with_ring(|r| r.published()).flatten().expect("published");
        assert_eq!((p.generation, p.slot, p.ready), (ring.generation, 0, 1));

        // The session's side: a second device opens the texture and fence by handle.
        let (other, octx) = warp();
        let dev1 = query(other, &super::super::com::IID_ID3D11DEVICE1);
        let mut tex = 0usize;
        let hr = vcall!(dev1, 48, fn(HANDLE, *const Guid, *mut usize) -> i32, ring.handles[p.slot], &super::super::com::IID_ID3D11TEXTURE2D, &mut tex);
        assert!(hr >= 0 && tex != 0, "OpenSharedResource1 {hr:#x}");
        let dev5 = query(other, &IID_ID3D11DEVICE5);
        let mut fence = 0usize;
        assert!(vcall!(dev5, 67, fn(HANDLE, *const Guid, *mut usize) -> i32, ring.handles[SLOTS], &IID_ID3D11FENCE, &mut fence) >= 0);
        let start = std::time::Instant::now();
        while vcall!(fence, 8, fn() -> u64) < p.ready && start.elapsed().as_secs() < 5 {
            std::thread::yield_now();
        }
        assert!(vcall!(fence, 8, fn() -> u64) >= p.ready, "the content fence got there");
        // Read through a typed copy (the shared texture is typeless).
        let typed_desc = Texture2dDesc { format: 87, ..desc };
        let mut plain = 0usize;
        assert!(vcall!(other, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &typed_desc, 0, &mut plain) >= 0);
        vcall!(octx, 47, fn(usize, usize) -> (), plain, tex);
        let px = super::super::d3d11::read_pixels(other, octx, plain, w, h);
        assert_eq!(px[(h / 2 * w + w / 2) as usize] & 0x00ff_ffff, 0x00ff00, "green arrived");

        // The next frame goes to slot 1 (slot 0 is newest); with the compositor then
        // sampling slot 1, the one after goes to slot 2.
        cap.capture(device, back, 0);
        let p1 = with_ring(|r| r.published()).flatten().unwrap();
        assert_eq!(p1.slot, 1);
        with_ring(|r| r.take(p1.generation)).flatten().unwrap();
        cap.capture(device, back, 0);
        assert_eq!(with_ring(|r| r.published()).flatten().unwrap().slot, 2);

        // The same buffers again (a game resizing them without changing anything) keep
        // the set; a resize makes a new generation.
        assert_eq!(cap.ring.as_ref().unwrap().generation, p.generation, "the same set");
        let small = Texture2dDesc { width: 8, height: 8, ..desc };
        let mut back2 = 0usize;
        assert!(vcall!(device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &small, 0, &mut back2) >= 0);
        cap.capture(device, back2, 0);

        let g2 = cap.ring.as_ref().unwrap().generation;
        assert!(g2 > p.generation);
        configure(false, None, None);
        drop(cap);
        for o in [fence, dev5, plain, tex, dev1, octx, other, rtv, back, back2, ctx, device] {
            release(o);
        }
    }

    /// Asked for pixels, a Direct3D 11 frame is read back through a staging texture,
    /// without waiting, and lands in the pixel file a present or two later.
    #[test]
    fn d3d11_frames_are_read_back_into_pixels() {
        let _turn = crate::sys::lock(&SERIAL);
        let (ring, dir) = session_ring_file();
        configure_as(true, Some(&ring), None, None, true);
        assert!(pixels());
        assert!(pixel_dir().is_some_and(|d| !d.is_empty()));
        let (device, ctx) = warp();
        let (w, h) = (32u32, 16u32);
        let desc = Texture2dDesc { width: w, height: h, mip_levels: 1, array_size: 1, format: 87, sample_count: 1, sample_quality: 0, usage: 0, bind_flags: 0x20, cpu_access: 0, misc: 0 };
        let mut back = 0usize;
        assert!(vcall!(device, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &desc, 0, &mut back) >= 0);
        let mut rtv = 0usize;
        assert!(vcall!(device, 9, fn(usize, usize, *mut usize) -> i32, back, 0, &mut rtv) >= 0);
        vcall!(ctx, 50, fn(usize, *const f32) -> (), rtv, [1.0f32, 0.0, 0.0, 1.0].as_ptr());
        let mut cap = D3d11Capture::default();
        let start = std::time::Instant::now();
        let p = loop {
            cap.capture(device, back, 0);
            if let Some(p) = with_ring(|r| r.published()).flatten() {
                break p;
            }
            assert!(start.elapsed().as_secs() < 5, "read back within 5 s");
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert!(cap.ring.is_none(), "no shared textures in pixel mode");
        let bytes = pixel_slot(&dir.0, p.slot, peerparty_protocol::frame_ring::slot_bytes(w * 4, h).unwrap(), (w * h * 4) as usize);
        let mid = ((h / 2 * w + w / 2) * 4) as usize;
        assert_eq!(&bytes[mid..mid + 4], &[0, 0, 0xff, 0xff], "red (BGRA) in the pixel file");
        configure(false, None, None);
        drop(cap);
        for o in [rtv, back, ctx, device] {
            release(o);
        }
    }

    /// A Direct3D 12 frame (a texture cleared red on a WARP device's queue) goes into a

    /// shared committed texture that a Direct3D 11 device opens, as the session does.
    #[test]
    fn d3d12_frames_reach_a_d3d11_device() {
        const IID_IDXGIFACTORY4: Guid = Guid(0x1bc6ea02, 0xef36, 0x464f, [0xbf, 0x0c, 0x21, 0xca, 0x39, 0xe5, 0x16, 0x8a]);
        const IID_IDXGIADAPTER: Guid = Guid(0x2411e7e1, 0x12ac, 0x4ccf, [0xbd, 0x14, 0x97, 0x98, 0xe8, 0x53, 0x4d, 0xc0]);
        const IID_ID3D12DESCRIPTORHEAP: Guid = Guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        let _turn = crate::sys::lock(&SERIAL);
        let (_map, name) = session_ring();
        configure(true, Some(&name), None);
        let dxgi = com::load("dxgi.dll");
        type CreateFactory = unsafe extern "system" fn(*const Guid, *mut usize) -> i32;
        let cf: CreateFactory = unsafe { std::mem::transmute(com::proc(dxgi, c"CreateDXGIFactory1").unwrap()) };
        let mut factory = 0usize;
        assert!(unsafe { cf(&IID_IDXGIFACTORY1, &mut factory) } >= 0);
        let f4 = query(factory, &IID_IDXGIFACTORY4);
        let mut adapter = 0usize;
        assert!(vcall!(f4, 27, fn(*const Guid, *mut usize) -> i32, &IID_IDXGIADAPTER, &mut adapter) >= 0, "EnumWarpAdapter");
        let d3d12 = com::load("d3d12.dll");
        type Create12 = unsafe extern "system" fn(usize, u32, *const Guid, *mut usize) -> i32;
        let c12: Create12 = unsafe { std::mem::transmute(com::proc(d3d12, c"D3D12CreateDevice").unwrap()) };
        let mut dev = 0usize;
        assert!(unsafe { c12(adapter, 0xb000, &super::super::com::IID_ID3D12DEVICE, &mut dev) } >= 0);
        let mut queue = 0usize;
        assert!(vcall!(dev, 8, fn(*const u32, *const Guid, *mut usize) -> i32, [0u32; 4].as_ptr(), &super::super::com::IID_ID3D12COMMANDQUEUE, &mut queue) >= 0);
        let (w, h) = (32u32, 16u32);
        let heap = HeapProperties { kind: 1, cpu_page: 0, pool: 0, creation_mask: 0, visible_mask: 0 };
        let desc = ResourceDesc { dimension: 3, alignment: 0, width: w.into(), height: h, depth: 1, mips: 1, format: 87, sample_count: 1, sample_quality: 0, layout: 0, flags: 0x1 };
        let mut buffer = 0usize;
        let hr = vcall!(dev, 27, fn(*const HeapProperties, u32, *const ResourceDesc, u32, usize, *const Guid, *mut usize) -> i32, &heap, 0, &desc, 0x4, 0, &IID_ID3D12RESOURCE, &mut buffer);
        assert!(hr >= 0, "the game's buffer {hr:#x}");
        let mut rtv_heap = 0usize;
        assert!(vcall!(dev, 14, fn(*const u32, *const Guid, *mut usize) -> i32, [2u32, 1, 0, 0].as_ptr(), &IID_ID3D12DESCRIPTORHEAP, &mut rtv_heap) >= 0);
        let mut handle = 0usize;
        vcall!(rtv_heap, 9, fn(*mut usize) -> *mut usize, &mut handle);
        vcall!(dev, 20, fn(usize, usize, usize) -> (), buffer, 0, handle);
        let (mut alloc, mut list) = (0usize, 0usize);
        assert!(vcall!(dev, 9, fn(u32, *const Guid, *mut usize) -> i32, 0, &IID_ID3D12COMMANDALLOCATOR, &mut alloc) >= 0);
        assert!(vcall!(dev, 12, fn(u32, u32, usize, usize, *const Guid, *mut usize) -> i32, 0, 0, alloc, 0, &IID_ID3D12GRAPHICSCOMMANDLIST, &mut list) >= 0);
        vcall!(list, 48, fn(usize, *const f32, u32, usize) -> (), handle, [1.0f32, 0.0, 0.0, 1.0].as_ptr(), 0, 0);
        let to_present = barrier(buffer, 0x4, STATE_COMMON);
        vcall!(list, 26, fn(u32, *const Barrier) -> (), 1, &to_present);
        assert!(vcall!(list, 9, fn() -> i32) >= 0);
        vcall!(queue, 10, fn(u32, *const usize) -> (), 1, &list);

        let mut cap = D3d12Capture::new(dev, queue, w, h, typeless(87)).expect("shared Direct3D 12 textures on WARP");
        cap.capture(buffer);
        let p = with_ring(|r| r.published()).flatten().expect("published");
        assert_eq!((p.generation, p.slot, p.ready), (cap.generation, 0, 1));

        let (other, octx) = warp();
        let dev1 = query(other, &super::super::com::IID_ID3D11DEVICE1);
        let mut tex = 0usize;
        let hr = vcall!(dev1, 48, fn(HANDLE, *const Guid, *mut usize) -> i32, cap.handles[p.slot], &super::super::com::IID_ID3D11TEXTURE2D, &mut tex);
        assert!(hr >= 0 && tex != 0, "OpenSharedResource1 {hr:#x}");
        let dev5 = query(other, &IID_ID3D11DEVICE5);
        let mut fence = 0usize;
        assert!(vcall!(dev5, 67, fn(HANDLE, *const Guid, *mut usize) -> i32, cap.handles[SLOTS], &IID_ID3D11FENCE, &mut fence) >= 0);
        let start = std::time::Instant::now();
        while vcall!(fence, 8, fn() -> u64) < p.ready && start.elapsed().as_secs() < 5 {
            std::thread::yield_now();
        }
        assert!(vcall!(fence, 8, fn() -> u64) >= p.ready);
        let typed_desc = Texture2dDesc { width: w, height: h, mip_levels: 1, array_size: 1, format: 87, sample_count: 1, sample_quality: 0, usage: 0, bind_flags: 0x20, cpu_access: 0, misc: 0 };
        let mut plain = 0usize;
        assert!(vcall!(other, 5, fn(*const Texture2dDesc, usize, *mut usize) -> i32, &typed_desc, 0, &mut plain) >= 0);
        vcall!(octx, 47, fn(usize, usize) -> (), plain, tex);
        let px = super::super::d3d11::read_pixels(other, octx, plain, w, h);
        assert_eq!(px[(h / 2 * w + w / 2) as usize] & 0x00ff_ffff, 0xff0000, "red arrived");
        configure(false, None, None);
        drop(cap);
        for o in [fence, dev5, plain, tex, dev1, octx, other, list, alloc, rtv_heap, buffer, queue, dev, adapter, f4, factory] {
            release(o);
        }
    }

    #[test]
    fn typeless_families() {
        for (f, t) in [(28, 27), (29, 27), (87, 90), (91, 90), (88, 92), (24, 23), (10, 9), (2, 2)] {
            assert_eq!(typeless(f), t, "{f}");
        }
        for t in [27, 90, 92, 23, 9] {
            assert_eq!(typeless(typed(t)), t);
        }
    }

    /// Compositor-locked frame pacing: returns immediately (0 ms) when the compositor
    /// already advanced `presents` (slow game), wakes right away when the pace event is
    /// signalled (fast game), and never stalls past `PACE_TIMEOUT_MS` if unsignalled.
    #[test]
    fn pace_waits_for_compositor_and_skips_slow_frames() {
        use peerparty_protocol::frame_ring::{FLAG_COMPOSITOR_LIVE, FLAG_SYNC_WANTED};
        use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};
        let _turn = crate::sys::lock(&SERIAL);
        let (_map, ring_name) = session_ring();
        let pace_name = format!(r"Local\peerparty-pace-test-{}", std::process::id());
        let pace_ev = unsafe { CreateEventW(std::ptr::null(), 0, 0, wide(&pace_name).as_ptr()) };
        assert!(!pace_ev.is_null());
        configure_as(true, Some(&ring_name), None, Some(&pace_name), false);
        assert!(!sync_active(), "flags not set yet");
        with_ring(|h| {
            h.set_flag(FLAG_SYNC_WANTED, true);
            h.set_flag(FLAG_COMPOSITOR_LIVE, true);
        });
        assert!(sync_active());

        // Slow game: compositor already presented -> 0 ms wait.
        with_ring(|h| h.presented());
        let t0 = std::time::Instant::now();
        pace();
        assert!(t0.elapsed().as_millis() < 15, "slow game must not stall");

        // Fast game: compositor signals pace event in 8 ms -> wakes right away.
        let ev_addr = pace_ev as usize;
        let signaler = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(8));
            unsafe { SetEvent(ev_addr as HANDLE) };
        });
        let t1 = std::time::Instant::now();
        pace();
        signaler.join().unwrap();
        assert!(t1.elapsed().as_millis() < 100, "woke on pace signal");

        // Unsignalled safety timeout: returns in ~PACE_TIMEOUT_MS, never deadlocks.
        let t2 = std::time::Instant::now();
        pace();
        let ms = t2.elapsed().as_millis();
        assert!(ms >= 20 && ms < 200, "safety timeout bounded ({ms} ms)");

        configure(false, None, None);
        unsafe { CloseHandle(pace_ev) };
    }
}
