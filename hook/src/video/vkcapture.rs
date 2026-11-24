//! A Vulkan game's frames for the session's window (capture.rs). The shared textures are
//! made on a Direct3D 11 device of PeerParty's own on the game's adapter (`D3d11Ring`) and
//! imported into the game's device as images (`VK_KHR_external_memory_win32`, which
//! `create_device` adds to the extensions it asks for); each present copies the swap chain
//! image into a free one on the presenting queue.
//!
//! A Vulkan device can't signal a Direct3D fence without more extensions, so a slot is
//! published once its copy is finished: a thread of PeerParty's waits on each copy's fence
//! and publishes it then (`ready` 0: the session's GPU has nothing to wait for). Until
//! then the slot is held (`in_flight`), so the next frame doesn't pick it again.
//!
//! When the session asks for pixels (`capture::pixels`: a game under Wine, where DXVK and
//! VKD3D bring every Direct3D game here too), each present's image is copied into a
//! host-visible buffer instead, and that thread hands the finished copy's rows to
//! `CpuFrames` (the session's pixel file). Nothing to import then, so a driver without
//! the import extensions still has its frames read back.

use std::ffi::{CStr, c_char};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};

use peerparty_protocol::frame_ring::SLOTS;

use super::super::capture::{self, D3d11Ring, Kind, private_device};
use super::super::pixels::CpuFrames;
use super::{BeginInfo, Funcs, ImageBarrier, ImageInfo, MemAlloc, Pfn, PresentInfo, SubmitInfo, WORLD, com, command_ring, destroy_ring, made, memory_type};
use crate::log;

/// What importing a Direct3D 11 texture takes, by name (core from Vulkan 1.1 but the
/// last; asked for when the device has them, so a 1.0 device works too).
const EXTENSIONS: [&CStr; 4] = [c"VK_KHR_external_memory", c"VK_KHR_external_memory_win32", c"VK_KHR_dedicated_allocation", c"VK_KHR_get_memory_requirements2"];
const WIN32: &CStr = c"VK_KHR_external_memory_win32";

const D3D11_TEXTURE: u32 = 0x8;
const QUEUE_FAMILY_EXTERNAL: u32 = 0xffff_fffe;
const IGNORED: u32 = u32::MAX;
const PRESENT_SRC: u32 = 1000001002;
const TRANSFER_SRC: u32 = 6;
const TRANSFER_DST: u32 = 7;
const GENERAL: u32 = 1;

/// `VkDeviceCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct DeviceCreateInfo {
    pub s_type: u32,
    pub next: usize,
    pub flags: u32,
    pub queue_count: u32,
    pub queues: usize,
    pub layer_count: u32,
    pub layers: usize,
    pub ext_count: u32,
    pub exts: *const *const c_char,
    pub features: usize,
}

/// The extension names `info` asks for.
fn asked(info: &DeviceCreateInfo) -> Vec<*const c_char> {
    if info.exts.is_null() {
        return Vec::new();
    }
    (0..info.ext_count as usize).map(|i| unsafe { *info.exts.add(i) }).collect()
}

fn has(list: &[*const c_char], name: &CStr) -> bool {
    list.iter().any(|p| !p.is_null() && unsafe { CStr::from_ptr(*p) } == name)
}

/// The physical device's extensions.
fn device_extensions(phys: usize) -> Vec<String> {
    type Enumerate = unsafe extern "system" fn(usize, *const c_char, *mut u32, *mut [u8; 260]) -> i32;
    let Some(f) = com::proc(super::module(), c"vkEnumerateDeviceExtensionProperties") else { return Vec::new() };
    let f: Enumerate = unsafe { std::mem::transmute(f) };
    let mut n = 0u32;
    if unsafe { f(phys, std::ptr::null(), &mut n, std::ptr::null_mut()) } != 0 {
        return Vec::new();
    }
    let mut props = vec![[0u8; 260]; n as usize];
    if unsafe { f(phys, std::ptr::null(), &mut n, props.as_mut_ptr()) } < 0 {
        return Vec::new();
    }
    props.iter().take(n as usize).filter_map(|p| CStr::from_bytes_until_nul(&p[..256]).ok()).map(|c| c.to_string_lossy().into_owned()).collect()
}

/// The extension list for a split-screen copy's device: the game's, and what importing
/// shared textures takes that `phys` has and the game didn't ask for. None: nothing to add
/// (or nothing possible). Whether import is possible comes with it.
pub(super) fn extensions_for(phys: usize, info: &DeviceCreateInfo) -> (Option<Vec<*const c_char>>, bool) {
    let mut list = asked(info);
    let have = device_extensions(phys);
    if !have.iter().any(|e| e.as_bytes() == WIN32.to_bytes()) {
        log!("video: this Vulkan device can't import shared textures; the session shows the copy's window instead");
        return (None, false);
    }
    let before = list.len();
    for e in EXTENSIONS {
        if !has(&list, e) && have.iter().any(|h| h.as_bytes() == e.to_bytes()) {
            list.push(e.as_ptr());
        }
    }
    ((list.len() > before).then_some(list), true)
}

/// The adapter's LUID, for a Direct3D 11 device on the same one (Vulkan 1.1 devices).
pub(super) fn luid(phys: usize) -> Option<(u32, i32)> {
    let m = super::module();
    type Props = unsafe extern "system" fn(usize, *mut [u64; 128]);
    let props: Props = unsafe { std::mem::transmute(com::proc(m, c"vkGetPhysicalDeviceProperties")?) };
    let mut p = [0u64; 128];
    unsafe { props(phys, &mut p) };
    let api = p[0] as u32;
    if api < (1 << 22 | 1 << 12) {
        return None;
    }
    #[repr(C)]
    struct IdProps {
        s_type: u32,
        next: usize,
        device_uuid: [u8; 16],
        driver_uuid: [u8; 16],
        luid: [u8; 8],
        node_mask: u32,
        luid_valid: u32,
    }
    #[repr(C)]
    struct Props2 {
        s_type: u32,
        next: *mut IdProps,
        props: [u64; 128],
    }
    type Props2Fn = unsafe extern "system" fn(usize, *mut Props2);
    let props2: Props2Fn = unsafe { std::mem::transmute(com::proc(m, c"vkGetPhysicalDeviceProperties2")?) };
    let mut id = IdProps { s_type: 1000071004, next: 0, device_uuid: [0; 16], driver_uuid: [0; 16], luid: [0; 8], node_mask: 0, luid_valid: 0 };
    let mut p2 = Props2 { s_type: 1000059001, next: &mut id, props: [0; 128] };
    unsafe { props2(phys, &mut p2) };
    (id.luid_valid != 0).then(|| (u32::from_le_bytes(id.luid[0..4].try_into().unwrap()), i32::from_le_bytes(id.luid[4..8].try_into().unwrap())))
}

/// A swap chain format's Direct3D 11 twin (a texture of it imports as that Vulkan format,
/// which a copy from the sRGB view of the same bytes also fits).
fn twin(vk: u32) -> Option<(u32, u32)> {
    match vk {
        44 | 50 => Some((87, 44)), // B8G8R8A8
        37 | 43 => Some((28, 37)), // R8G8B8A8
        64 => Some((24, 64)),      // A2B10G10R10 = R10G10B10A2
        97 => Some((10, 97)),      // R16G16B16A16_SFLOAT
        _ => None,
    }
}

/// A `VkColorSpaceKHR`'s `DXGI_COLOR_SPACE_TYPE` (what the session reads frames as).
fn dxgi_color_space(vk: u32) -> u32 {
    use peerparty_protocol::frame_ring::{COLOR_SPACE_HDR10, COLOR_SPACE_SCRGB, COLOR_SPACE_SRGB};
    match vk {
        1000104008 => COLOR_SPACE_HDR10, // HDR10_ST2084_EXT
        1000104002 => COLOR_SPACE_SCRGB, // EXTENDED_SRGB_LINEAR_EXT
        _ => COLOR_SPACE_SRGB,
    }
}

/// A swap chain format's `DXGI_FORMAT` for its pixels read back, and its bytes per pixel.
fn pixel_format(vk: u32) -> Option<(u32, u32)> {
    match vk {
        44 | 50 => Some((87, 4)), // B8G8R8A8
        37 | 43 => Some((28, 4)), // R8G8B8A8
        64 => Some((24, 4)),      // A2B10G10R10 = R10G10B10A2
        97 => Some((10, 8)),      // R16G16B16A16_SFLOAT
        _ => None,
    }
}

/// The few device functions the copy needs that the overlay's `Funcs` doesn't.
struct More {
    copy_image: unsafe extern "system" fn(usize, u64, u32, u64, u32, u32, *const [u32; 17]),
    copy_to_buffer: unsafe extern "system" fn(usize, u64, u32, u64, u32, *const super::BufferImageCopy),
    /// Only for importing shared textures.
    win32_props: Option<unsafe extern "system" fn(usize, u32, usize, *mut Win32Props) -> i32>,
}

#[repr(C)]
struct Win32Props {
    s_type: u32,
    next: usize,
    bits: u32,
}

#[repr(C)]
struct ExternalImage {
    s_type: u32,
    next: usize,
    handle_types: u32,
}

#[repr(C)]
struct Import {
    s_type: u32,
    next: usize,
    handle_type: u32,
    handle: usize,
    name: usize,
}

#[repr(C)]
struct Dedicated {
    s_type: u32,
    next: usize,
    image: u64,
    buffer: u64,
}

struct Entry {
    cmd: usize,
    fence: u64,
    sem: u64,
    busy: Arc<AtomicBool>,
}

/// What the thread does once a copy's fence is signalled.
enum Done {
    /// Publish the slot it went into, and let it go (`in_flight`).
    Publish { generation: u32, slot: usize, in_flight: Arc<[AtomicU32; SLOTS]> },
    /// Hand the rows read back (at `bits`, `stride` apart) to the frames' sink.
    Deliver { bits: usize, kind: Kind, stride: u32, frames: Arc<std::sync::Mutex<CpuFrames>> },
}

struct Job {
    fence: u64,
    done: Done,
    busy: Arc<AtomicBool>,
}

/// Pixel mode: per copy in flight, the host-visible buffer its image goes into; the
/// frames they're for, and where they go.
struct Readback {
    buffers: Vec<(u64, u64, *mut u8)>,
    kind: Kind,
    stride: u32,
    frames: Arc<std::sync::Mutex<CpuFrames>>,
}

/// One device's capture state. The Vulkan objects go with `destroy` (with the device's
/// functions); dropped, it ends its thread and lets go of its Direct3D 11 device.
pub(super) struct VkCapture {
    luid: Option<(u32, i32)>,
    /// PeerParty's Direct3D 11 device and context (made at the first frame).
    private: Option<(usize, usize)>,
    ring: Option<D3d11Ring>,
    /// Per slot: the imported image and its memory.
    images: Vec<(u64, u64)>,
    /// The swap chain, size, format and colour space the ring is for (also when it
    /// couldn't be made), and the session's setup (`capture::epoch`).
    made_for: (u64, u32, u32, u32, u32, u64),
    vk_format: u32,
    pool: Option<(u32, u64)>,
    entries: Vec<Entry>,
    next: usize,
    tx: Option<mpsc::Sender<Job>>,
    /// The thread publishing finished copies, and what tells it to stop waiting.
    thread: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    /// Per slot: the generation a copy into it is under way for (0: none).
    in_flight: Arc<[AtomicU32; SLOTS]>,
    more: Option<More>,
    /// The device can import shared textures (else its frames can only be read back).
    can_import: bool,
    readback: Option<Readback>,
}

unsafe impl Send for VkCapture {}

impl VkCapture {
    pub(super) fn new(luid: Option<(u32, i32)>, can_import: bool) -> VkCapture {
        VkCapture {
            luid,
            private: None,
            ring: None,
            images: Vec::new(),
            made_for: (0, 0, 0, 0, 0, 0),
            vk_format: 0,
            pool: None,
            entries: Vec::new(),
            next: 0,
            tx: None,
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new([const { AtomicU32::new(0) }; SLOTS]),
            more: None,
            can_import,
            readback: None,
        }
    }

    /// Whether it made anything (on the device or its own).
    pub(super) fn made_anything(&self) -> bool {
        self.private.is_some() || self.pool.is_some() || self.readback.is_some()
    }

    /// Everything it made on `dev` (idle): the thread ends first.
    pub(super) fn destroy(&mut self, fx: &Funcs, dev: usize) {
        self.idle();
        self.end_thread();
        self.drop_images(fx, dev);
        self.drop_readback(fx, dev);
        if let Some((_, pool)) = self.pool.take() {
            let ring = self.entries.drain(..).map(|e| (e.cmd, e.fence, e.sem)).collect();
            destroy_ring(fx, dev, pool, ring);
        }
    }

    /// Tells the thread to stop (it finishes the copy it waits on, a tenth of a second at
    /// most once the device is idle) and waits for it.
    fn end_thread(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn drop_readback(&mut self, fx: &Funcs, dev: usize) {
        self.idle();
        if let Some(r) = self.readback.take() {
            for (buf, mem, _) in r.buffers {
                unsafe {
                    (fx.destroy_buffer)(dev, buf, 0);
                    (fx.free_mem)(dev, mem, 0);
                }
            }
        }
    }

    /// Pixel mode's buffers, one per copy in flight, for frames of `kind`.
    fn ensure_readback(&mut self, fx: &Funcs, dev: usize, phys: usize, kind: Kind, bpp: u32) -> bool {
        if self.readback.as_ref().is_some_and(|r| r.kind == kind) {
            return true;
        }
        let frames = self.readback.as_ref().map(|r| r.frames.clone());
        self.drop_readback(fx, dev);
        let stride = kind.width * bpp;
        let size = stride as usize * kind.height as usize;
        let mut buffers = Vec::new();
        for _ in 0..SLOTS + 1 {
            // TRANSFER_DST.
            match super::host_buffer(fx, dev, phys, size, 0x2) {
                Some(b) => buffers.push(b),
                None => {
                    log!("video: no host-visible Vulkan buffer for reading frames back");
                    for (buf, mem, _) in buffers {
                        unsafe {
                            (fx.destroy_buffer)(dev, buf, 0);
                            (fx.free_mem)(dev, mem, 0);
                        }
                    }
                    return false;
                }
            }
        }
        let frames = frames.unwrap_or_else(|| Arc::new(std::sync::Mutex::new(CpuFrames::new(self.luid, "vulkan"))));
        self.readback = Some(Readback { buffers, kind, stride, frames });
        true
    }

    /// Waits (a second at most) for every copy in flight, so images can go.
    pub(super) fn idle(&self) {
        let start = std::time::Instant::now();
        while self.entries.iter().any(|e| e.busy.load(Ordering::Acquire)) && start.elapsed().as_millis() < 1000 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn drop_images(&mut self, fx: &Funcs, dev: usize) {
        self.idle();
        for (image, mem) in self.images.drain(..) {
            unsafe {
                (fx.destroy_image)(dev, image, 0);
                (fx.free_mem)(dev, mem, 0);
            }
        }
        self.ring = None;
    }

    /// The ring and its images for frames of `swap` (w x h, Vulkan `format` and
    /// `color_space`).
    #[allow(clippy::too_many_arguments)]
    fn ensure(&mut self, fx: &Funcs, dev: usize, phys: usize, swap: u64, w: u32, h: u32, format: u32, color_space: u32) -> bool {
        let want = (swap, w, h, format, color_space, capture::epoch());
        if self.made_for == want {
            return self.ring.is_some();
        }
        self.drop_images(fx, dev);
        self.made_for = want;
        let Some((dxgi, vk)) = twin(format) else {
            log!("video: Vulkan frames in format {format} can't be shared; the session shows the copy's window instead");
            return false;
        };
        if self.private.is_none() {
            self.private = private_device(self.luid);
        }
        let Some((device, _)) = self.private else { return false };
        let Some(mut ring) = D3d11Ring::new(device, w, h, dxgi, 0x08 | 0x20) else { return false };
        ring.color_space = dxgi_color_space(color_space);
        let Some(more) = self.more.as_ref() else { return false };
        for slot in 0..SLOTS {
            match import(fx, more, dev, phys, ring.handle(slot) as usize, w, h, vk) {
                Some(im) => self.images.push(im),
                None => {
                    log!("video: a shared texture couldn't be imported into the Vulkan device");
                    self.drop_images(fx, dev);
                    return false;
                }
            }
        }
        self.vk_format = vk;
        ring.report("vulkan");
        self.ring = Some(ring);
        true
    }

    /// The command pool and copies in flight on queue `family`, and the thread that
    /// publishes finished copies.
    fn ensure_pool(&mut self, fx: &Funcs, dev: usize, family: u32) -> bool {
        if self.pool.is_some_and(|p| p.0 == family) {
            return true;
        }
        if self.pool.is_some() {
            // Presents from another family: kept simple, not copied.
            crate::log::throttled("video: vk capture family", 1, |_| {
                log!("video: the game presents on Vulkan queues of more than one family; frames presented on family {family} aren't copied for the session")
            });
            return false;
        }
        let Some((pool, ring, _)) = command_ring(fx, dev, family, SLOTS + 1, false) else { return false };
        let (tx, rx) = mpsc::channel::<Job>();
        let wait = fx.wait_fences as usize;
        let stop = self.stop.clone();
        let spawned = std::thread::Builder::new().name("peerparty-vkframes".into()).spawn(move || {
            let wait: unsafe extern "system" fn(usize, u32, *const u64, u32, u64) -> i32 = unsafe { std::mem::transmute(wait) };
            for job in rx {
                loop {
                    let r = unsafe { wait(dev, 1, &job.fence, 1, 100_000_000) };
                    if r == 0 {
                        match &job.done {
                            Done::Publish { generation, slot, .. } => capture::publish(*generation, *slot, 0),
                            Done::Deliver { bits, kind, stride, frames } => {
                                let mut f = crate::sys::lock(frames);
                                unsafe { f.deliver(*kind, *bits as *const u8, *stride as usize) };
                            }
                        }
                        break;
                    }
                    // The device is lost, or going (`end_thread`): not waited on any more.
                    if r < 0 || stop.load(Ordering::Acquire) {
                        break;
                    }
                }
                if let Done::Publish { generation, slot, in_flight } = &job.done {
                    let _ = in_flight[*slot].compare_exchange(*generation, 0, Ordering::AcqRel, Ordering::Acquire);
                }
                job.busy.store(false, Ordering::Release);
            }
        });
        let Ok(thread) = spawned else {
            destroy_ring(fx, dev, pool, ring);
            return false;
        };
        self.entries = ring.into_iter().map(|(cmd, fence, sem)| Entry { cmd, fence, sem, busy: Arc::new(AtomicBool::new(false)) }).collect();
        self.tx = Some(tx);
        self.thread = Some(thread);
        self.pool = Some((family, pool));
        true
    }
}

impl Drop for VkCapture {
    fn drop(&mut self) {
        // Its Vulkan objects went with `destroy`, or go with the device; the thread is
        // told to stop and ends on its own.
        self.stop.store(true, Ordering::Release);
        self.tx = None;
        self.ring = None;
        if let Some((device, ctx)) = self.private.take() {
            com::release(ctx);
            com::release(device);
        }
    }
}

/// An image of `w x h` in `format` on the memory of the Direct3D 11 texture `handle`.
fn import(fx: &Funcs, more: &More, dev: usize, phys: usize, handle: usize, w: u32, h: u32, format: u32) -> Option<(u64, u64)> {
    let external = ExternalImage { s_type: 1000072001, next: 0, handle_types: D3D11_TEXTURE };
    let ii = ImageInfo {
        s_type: 14,
        next: &external as *const ExternalImage as usize,
        flags: 0,
        image_type: 1,
        format,
        extent: [w, h, 1],
        mips: 1,
        layers: 1,
        samples: 1,
        tiling: 0,
        usage: 0x1 | 0x2, // TRANSFER_SRC | TRANSFER_DST
        sharing: 0,
        family_count: 0,
        families: 0,
        initial_layout: 0,
    };
    let mut image = 0u64;
    if !made(unsafe { (fx.create_image)(dev, &ii, 0, &mut image) }, &mut image) {
        return None;
    }
    let mut reqs = [0u64; 3];
    unsafe { (fx.image_reqs)(dev, image, &mut reqs) };
    let mut props = Win32Props { s_type: 1000073002, next: 0, bits: 0 };
    let Some(win32_props) = more.win32_props else {
        unsafe { (fx.destroy_image)(dev, image, 0) };
        return None;
    };
    let bits = if unsafe { win32_props(dev, D3D11_TEXTURE, handle, &mut props) } == 0 && props.bits != 0 { reqs[2] & u64::from(props.bits) } else { reqs[2] };
    let Some(type_index) = memory_type(fx, phys, bits, 0x1).or_else(|| memory_type(fx, phys, bits, 0)) else {
        unsafe { (fx.destroy_image)(dev, image, 0) };
        return None;
    };
    let dedicated = Dedicated { s_type: 1000127001, next: 0, image, buffer: 0 };
    let import = Import { s_type: 1000073000, next: &dedicated as *const Dedicated as usize, handle_type: D3D11_TEXTURE, handle, name: 0 };
    let ma = MemAlloc { s_type: 5, next: &import as *const Import as usize, size: reqs[0], type_index };
    let mut mem = 0u64;
    if unsafe { (fx.alloc_mem)(dev, &ma, 0, &mut mem) } != 0 {
        unsafe { (fx.destroy_image)(dev, image, 0) };
        return None;
    }
    if unsafe { (fx.bind_image)(dev, image, mem, 0) } != 0 {
        unsafe {
            (fx.destroy_image)(dev, image, 0);
            (fx.free_mem)(dev, mem, 0);
        }
        return None;
    }
    Some((image, mem))
}

fn more(get: impl Fn(&CStr) -> Pfn) -> Option<More> {
    let (c, b, p) = (get(c"vkCmdCopyImage"), get(c"vkCmdCopyImageToBuffer"), get(c"vkGetMemoryWin32HandlePropertiesKHR"));
    (c != 0 && b != 0).then(|| unsafe {
        More { copy_image: std::mem::transmute(c), copy_to_buffer: std::mem::transmute(b), win32_props: (p != 0).then(|| std::mem::transmute(p)) }
    })
}

/// Copies the image `info` presents into a free slot, after `wait` (the overlay's
/// semaphore) or the game's semaphores. Returns the semaphore the present must wait on
/// instead, or None when this frame isn't copied.
pub(super) fn capture(queue: usize, info: &PresentInfo, wait: Option<u64>) -> Option<u64> {
    if info.swap_count == 0 || !capture::enabled() {
        return None;
    }
    if capture::pixels() {
        return read_back(queue, info, wait);
    }
    let mut w = crate::sys::lock(&WORLD);
    let (dev, family) = *w.queues.get(&queue)?;
    let swap_id = unsafe { *info.swaps };
    let index = unsafe { *info.indices } as usize;
    let world = &mut *w;
    let swap = world.swaps.get_mut(&swap_id)?;
    if !swap.capturable {
        return None;
    }
    let d = world.devices.get_mut(&dev)?;
    if !copies_on(d, family) {
        return None;
    }
    if d.funcs.is_none() {
        d.funcs = super::funcs(dev);
    }
    let fx = d.funcs.as_ref()?;
    super::ensure_images(fx, dev, swap_id, swap);
    let image = *swap.images.get(index)?;
    let (sw, sh, format, space) = (swap.width, swap.height, swap.format, swap.color_space);
    let phys = d.phys;
    let c = d.capture.as_mut()?;
    if !c.can_import {
        return None;
    }
    if c.more.is_none() {
        let gdpa = super::GET_DEVICE_PROC.get()?;
        c.more = more(|n| unsafe { gdpa.call(dev, n.as_ptr()) });
    }
    if !c.ensure(fx, dev, phys, swap_id, sw, sh, format, space) || !c.ensure_pool(fx, dev, family) {
        return None;
    }
    let ring = c.ring.as_ref()?;
    let generation = ring.generation;
    let in_flight = c.in_flight.clone();
    let slot = ring.pick_except(|s| in_flight[s].load(Ordering::Acquire) == generation)?;
    let e = (0..c.entries.len()).map(|i| (c.next + i) % c.entries.len()).find(|&i| !c.entries[i].busy.load(Ordering::Acquire))?;
    c.next = (e + 1) % c.entries.len();
    let (cmd, fence, sem) = (c.entries[e].cmd, c.entries[e].fence, c.entries[e].sem);
    let dst = c.images[slot].0;
    let more = c.more.as_ref()?;
    let bar = |image, old, new, src_access, dst_access, src_family, dst_family| ImageBarrier {
        s_type: 45,
        next: 0,
        src_access,
        dst_access,
        old_layout: old,
        new_layout: new,
        src_family,
        dst_family,
        image,
        aspect: 1,
        base_mip: 0,
        mips: 1,
        base_layer: 0,
        layers: 1,
    };
    let before = [bar(image, PRESENT_SRC, TRANSFER_SRC, 0, 0x800, IGNORED, IGNORED), bar(dst, 0, TRANSFER_DST, 0, 0x1000, QUEUE_FAMILY_EXTERNAL, family)];
    let after = [bar(image, TRANSFER_SRC, PRESENT_SRC, 0x800, 0, IGNORED, IGNORED), bar(dst, TRANSFER_DST, GENERAL, 0x1000, 0, family, QUEUE_FAMILY_EXTERNAL)];
    // VkImageCopy: colour layer 0 of mip 0 both sides, offsets 0, the whole extent.
    let region: [u32; 17] = [1, 0, 0, 1, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0, sw, sh, 1];
    unsafe {
        (fx.reset_fences)(dev, 1, &fence);
        (fx.reset_cmd)(cmd, 0);
        let begin = BeginInfo { s_type: 42, next: 0, flags: 0x1, inheritance: 0 };
        (fx.begin)(cmd, &begin);
        (fx.barrier)(cmd, 0x1000, 0x1000, 0, 0, 0, 0, 0, 2, before.as_ptr());
        (more.copy_image)(cmd, image, TRANSFER_SRC, dst, TRANSFER_DST, 1, &region);
        (fx.barrier)(cmd, 0x1000, 0x2000, 0, 0, 0, 0, 0, 2, after.as_ptr());
        (fx.end)(cmd);
    }
    let (wait_count, waits) = match wait.as_ref() {
        Some(s) => (1, s as *const u64),
        None => (info.wait_count, info.waits),
    };
    let stages = vec![0x1000u32; wait_count as usize];
    let submit = SubmitInfo { s_type: 4, next: 0, wait_count, waits, stages: stages.as_ptr(), cmd_count: 1, cmds: &cmd, signal_count: 1, signals: &sem };
    in_flight[slot].store(generation, Ordering::Release);
    if unsafe { (fx.submit)(queue, 1, &submit, fence) } != 0 {
        in_flight[slot].store(0, Ordering::Release);
        crate::log::throttled("video: vk copy submit", 3, |n| log!("video: a Vulkan frame's copy couldn't be submitted (call {n}); the session keeps the one before"));
        return None;
    }
    c.entries[e].busy.store(true, Ordering::Release);
    let job = Job { fence, done: Done::Publish { generation, slot, in_flight: in_flight.clone() }, busy: c.entries[e].busy.clone() };
    if c.tx.as_ref().is_none_or(|tx| tx.send(job).is_err()) {
        c.entries[e].busy.store(false, Ordering::Release);
        in_flight[slot].store(0, Ordering::Release);
    }
    Some(sem)
}

/// Whether frames presented on queue `family` of `d` can be copied: not on a queue that
/// can only present (no graphics, compute or transfer), which takes no copy.
fn copies_on(d: &mut super::Dev, family: u32) -> bool {
    let flags = d.family_flags(family);
    if flags != 0 && flags & 0x7 == 0 {
        crate::log::throttled("video: vk present-only queue copy", 1, |_| {
            log!("video: the game presents on a Vulkan queue that can't copy (family {family}, flags {flags:#x}); its frames aren't copied for the session")
        });
        return false;
    }
    true
}

/// Pixel mode: copies the image `info` presents into a host-visible buffer, after `wait`
/// or the game's semaphores; the thread hands it over once finished. The semaphore the
/// present must wait on instead, or None when this frame isn't copied.
fn read_back(queue: usize, info: &PresentInfo, wait: Option<u64>) -> Option<u64> {
    let mut w = crate::sys::lock(&WORLD);
    let (dev, family) = *w.queues.get(&queue)?;
    let swap_id = unsafe { *info.swaps };
    let index = unsafe { *info.indices } as usize;
    let world = &mut *w;
    let swap = world.swaps.get_mut(&swap_id)?;
    if !swap.capturable {
        return None;
    }
    let d = world.devices.get_mut(&dev)?;
    if !copies_on(d, family) {
        return None;
    }
    if d.funcs.is_none() {
        d.funcs = super::funcs(dev);
    }
    let fx = d.funcs.as_ref()?;
    super::ensure_images(fx, dev, swap_id, swap);
    let image = *swap.images.get(index)?;
    let (sw, sh, format, space) = (swap.width, swap.height, swap.format, swap.color_space);
    let phys = d.phys;
    let c = d.capture.as_mut()?;
    if c.more.is_none() {
        let gdpa = super::GET_DEVICE_PROC.get()?;
        c.more = more(|n| unsafe { gdpa.call(dev, n.as_ptr()) });
    }
    let Some((dxgi, bpp)) = pixel_format(format) else {
        crate::log::throttled("video: vk pixel format", 1, |_| log!("video: Vulkan frames in format {format} can't be read back for the session"));
        return None;
    };
    let kind = Kind { width: sw, height: sh, format: dxgi, color_space: dxgi_color_space(space) };
    if !c.ensure_readback(fx, dev, phys, kind, bpp) || !c.ensure_pool(fx, dev, family) {
        return None;
    }
    let r = c.readback.as_ref()?;
    // Nowhere free to put it: not copied at all.
    if !crate::sys::lock(&r.frames).wants(kind) {
        return None;
    }
    let e = (0..c.entries.len()).map(|i| (c.next + i) % c.entries.len()).find(|&i| !c.entries[i].busy.load(Ordering::Acquire))?;
    c.next = (e + 1) % c.entries.len();
    let (cmd, fence, sem) = (c.entries[e].cmd, c.entries[e].fence, c.entries[e].sem);
    let (buffer, _, bits) = r.buffers[e];
    let more = c.more.as_ref()?;
    let bar = |old, new, src_access, dst_access| ImageBarrier {
        s_type: 45,
        next: 0,
        src_access,
        dst_access,
        old_layout: old,
        new_layout: new,
        src_family: IGNORED,
        dst_family: IGNORED,
        image,
        aspect: 1,
        base_mip: 0,
        mips: 1,
        base_layer: 0,
        layers: 1,
    };
    let region = super::BufferImageCopy { offset: 0, row_length: 0, image_height: 0, aspect: 1, mip: 0, base_layer: 0, layers: 1, x: 0, y: 0, z: 0, w: sw, h: sh, d: 1 };
    unsafe {
        (fx.reset_fences)(dev, 1, &fence);
        (fx.reset_cmd)(cmd, 0);
        let begin = BeginInfo { s_type: 42, next: 0, flags: 0x1, inheritance: 0 };
        (fx.begin)(cmd, &begin);
        (fx.barrier)(cmd, 0x1000, 0x1000, 0, 0, 0, 0, 0, 1, &bar(PRESENT_SRC, TRANSFER_SRC, 0, 0x800));
        (more.copy_to_buffer)(cmd, image, TRANSFER_SRC, buffer, 1, &region);
        // The buffer's writes made visible to the host; the image back for presenting.
        (fx.barrier)(cmd, 0x1000, 0x2000 | 0x4000, 0, 0, 0, 0, 0, 1, &bar(TRANSFER_SRC, PRESENT_SRC, 0x800, 0));
        (fx.end)(cmd);
    }
    let (wait_count, waits) = match wait.as_ref() {
        Some(s) => (1, s as *const u64),
        None => (info.wait_count, info.waits),
    };
    let stages = vec![0x1000u32; wait_count as usize];
    let submit = SubmitInfo { s_type: 4, next: 0, wait_count, waits, stages: stages.as_ptr(), cmd_count: 1, cmds: &cmd, signal_count: 1, signals: &sem };
    if unsafe { (fx.submit)(queue, 1, &submit, fence) } != 0 {
        return None;
    }
    c.entries[e].busy.store(true, Ordering::Release);
    let done = Done::Deliver { bits: bits as usize, kind, stride: r.stride, frames: r.frames.clone() };
    let job = Job { fence, done, busy: c.entries[e].busy.clone() };
    if c.tx.as_ref().is_none_or(|tx| tx.send(job).is_err()) {
        c.entries[e].busy.store(false, Ordering::Release);
    }
    Some(sem)
}

#[cfg(test)]
mod tests {
    use super::super::{AllocInfo, Dev, Flagged, GetProc, GetQueue, Swap, funcs_with};
    use super::*;

    #[repr(C)]
    struct AppInfo {
        s_type: u32,
        next: usize,
        name: usize,
        version: u32,
        engine: usize,
        engine_version: u32,
        api: u32,
    }

    #[repr(C)]
    struct InstanceInfo {
        s_type: u32,
        next: usize,
        flags: u32,
        app: *const AppInfo,
        layer_count: u32,
        layers: usize,
        ext_count: u32,
        exts: usize,
    }

    #[repr(C)]
    struct QueueInfo {
        s_type: u32,
        next: usize,
        flags: u32,
        family: u32,
        count: u32,
        priorities: *const f32,
    }

    /// A device of the PC's own (skipped without a Vulkan driver that imports shared
    /// textures): an image cleared green, presented through `capture`, arrives in the
    /// Direct3D 11 texture of the slot published, read on another Direct3D 11 device.
    #[test]
    fn vulkan_frames_reach_a_d3d11_device() {
        let _turn = crate::sys::lock(&capture::tests::SERIAL);
        let m = com::load("vulkan-1.dll");
        let Some(gipa) = com::proc(m, c"vkGetInstanceProcAddr") else { return };
        let gipa: GetProc = unsafe { std::mem::transmute(gipa) };
        let ip = |inst: usize, n: &CStr| unsafe { gipa(inst, n.as_ptr()) };
        type CreateInstance = unsafe extern "system" fn(*const InstanceInfo, usize, *mut usize) -> i32;
        let create_instance: CreateInstance = unsafe { std::mem::transmute(ip(0, c"vkCreateInstance")) };
        let app = AppInfo { s_type: 0, next: 0, name: 0, version: 0, engine: 0, engine_version: 0, api: 1 << 22 | 1 << 12 };
        let ii = InstanceInfo { s_type: 1, next: 0, flags: 0, app: &app, layer_count: 0, layers: 0, ext_count: 0, exts: 0 };
        let mut inst = 0usize;
        if unsafe { create_instance(&ii, 0, &mut inst) } != 0 {
            eprintln!("no Vulkan instance: skipped");
            return;
        }
        type Enumerate = unsafe extern "system" fn(usize, *mut u32, *mut usize) -> i32;
        let enumerate: Enumerate = unsafe { std::mem::transmute(ip(inst, c"vkEnumeratePhysicalDevices")) };
        let mut n = 1u32;
        let mut phys = 0usize;
        unsafe { enumerate(inst, &mut n, &mut phys) };
        if phys == 0 {
            eprintln!("no Vulkan device: skipped");
            return;
        }
        type Families = unsafe extern "system" fn(usize, *mut u32, *mut [u32; 6]);
        let families: Families = unsafe { std::mem::transmute(ip(inst, c"vkGetPhysicalDeviceQueueFamilyProperties")) };
        let mut fc = 0u32;
        unsafe { families(phys, &mut fc, std::ptr::null_mut()) };
        let mut props = vec![[0u32; 6]; fc as usize];
        unsafe { families(phys, &mut fc, props.as_mut_ptr()) };
        let family = props.iter().position(|p| p[0] & 1 != 0).expect("a graphics queue") as u32;
        let priority = 1.0f32;
        let qi = QueueInfo { s_type: 2, next: 0, flags: 0, family, count: 1, priorities: &priority };
        let game = [c"VK_KHR_swapchain".as_ptr()];
        let given = DeviceCreateInfo { s_type: 3, next: 0, flags: 0, queue_count: 1, queues: &qi as *const QueueInfo as usize, layer_count: 0, layers: 0, ext_count: 1, exts: game.as_ptr(), features: 0 };
        let (list, possible) = extensions_for(phys, &given);
        if !possible {
            eprintln!("this Vulkan device can't import shared textures: skipped");
            return;
        }
        let list = list.expect("the import extensions added");
        assert!(has(&list, WIN32) && has(&list, c"VK_KHR_swapchain"));
        let di = DeviceCreateInfo { ext_count: list.len() as u32, exts: list.as_ptr(), ..given };
        type CreateDev = unsafe extern "system" fn(usize, *const DeviceCreateInfo, usize, *mut usize) -> i32;
        let create_dev: CreateDev = unsafe { std::mem::transmute(ip(inst, c"vkCreateDevice")) };
        let mut dev = 0usize;
        assert_eq!(unsafe { create_dev(phys, &di, 0, &mut dev) }, 0);
        let gdpa: GetProc = unsafe { std::mem::transmute(ip(inst, c"vkGetDeviceProcAddr")) };
        let dp = |n: &CStr| unsafe { gdpa(dev, n.as_ptr()) };
        let fx = funcs_with(dp, m).expect("device functions");
        let get_queue: GetQueue = unsafe { std::mem::transmute(dp(c"vkGetDeviceQueue")) };
        let mut queue = 0usize;
        unsafe { get_queue(dev, family, 0, &mut queue) };

        // The "swap chain image": 64x32 BGRA, cleared green, left in PRESENT_SRC.
        let (w, h) = (64u32, 32u32);
        let ii = ImageInfo { s_type: 14, next: 0, flags: 0, image_type: 1, format: 44, extent: [w, h, 1], mips: 1, layers: 1, samples: 1, tiling: 0, usage: 0x1 | 0x2, sharing: 0, family_count: 0, families: 0, initial_layout: 0 };
        let mut image = 0u64;
        assert_eq!(unsafe { (fx.create_image)(dev, &ii, 0, &mut image) }, 0);
        let mut reqs = [0u64; 3];
        unsafe { (fx.image_reqs)(dev, image, &mut reqs) };
        let ma = MemAlloc { s_type: 5, next: 0, size: reqs[0], type_index: memory_type(&fx, phys, reqs[2], 0x1).unwrap() };
        let mut mem = 0u64;
        assert_eq!(unsafe { (fx.alloc_mem)(dev, &ma, 0, &mut mem) }, 0);
        assert_eq!(unsafe { (fx.bind_image)(dev, image, mem, 0) }, 0);
        let pool_info = Flagged { s_type: 39, next: 0, flags: 0x2, extra: family };
        let mut pool = 0u64;
        assert_eq!(unsafe { (fx.create_pool)(dev, &pool_info, 0, &mut pool) }, 0);
        let ai = AllocInfo { s_type: 40, next: 0, pool, level: 0, count: 1 };
        let mut cmd = 0usize;
        assert_eq!(unsafe { (fx.alloc_cmd)(dev, &ai, &mut cmd) }, 0);
        let bar = |old, new, src, dst| ImageBarrier { s_type: 45, next: 0, src_access: src, dst_access: dst, old_layout: old, new_layout: new, src_family: IGNORED, dst_family: IGNORED, image, aspect: 1, base_mip: 0, mips: 1, base_layer: 0, layers: 1 };
        type Clear = unsafe extern "system" fn(usize, u64, u32, *const [f32; 4], u32, *const [u32; 5]);
        let clear: Clear = unsafe { std::mem::transmute(dp(c"vkCmdClearColorImage")) };
        unsafe {
            (fx.begin)(cmd, &BeginInfo { s_type: 42, next: 0, flags: 0x1, inheritance: 0 });
            (fx.barrier)(cmd, 0x1, 0x1000, 0, 0, 0, 0, 0, 1, &bar(0, TRANSFER_DST, 0, 0x1000));
            clear(cmd, image, TRANSFER_DST, &[0.0, 1.0, 0.0, 1.0], 1, &[1, 0, 1, 0, 1]);
            (fx.barrier)(cmd, 0x1000, 0x2000, 0, 0, 0, 0, 0, 1, &bar(TRANSFER_DST, PRESENT_SRC, 0x1000, 0));
            (fx.end)(cmd);
        }
        let submit = SubmitInfo { s_type: 4, next: 0, wait_count: 0, waits: std::ptr::null(), stages: std::ptr::null(), cmd_count: 1, cmds: &cmd, signal_count: 0, signals: std::ptr::null() };
        assert_eq!(unsafe { (fx.submit)(queue, 1, &submit, 0) }, 0);
        type WaitIdle = unsafe extern "system" fn(usize) -> i32;
        let idle: WaitIdle = unsafe { std::mem::transmute(dp(c"vkQueueWaitIdle")) };
        unsafe { idle(queue) };

        let (_map, name) = capture::tests::session_ring();
        capture::configure(true, Some(&name), None);
        let luid = luid(phys);
        let swap_id = 0x7777u64;
        {
            let mut world = crate::sys::lock(&WORLD);
            let mut cap = VkCapture::new(luid, true);
            cap.more = more(dp);
            let mut d = Dev::new(phys, Some(cap));
            d.funcs = funcs_with(dp, m);
            world.devices.insert(dev, d);
            world.queues.insert(queue, (dev, family));
            world.swaps.insert(swap_id, Swap { dev, format: 44, color_space: 0, width: w, height: h, drawable: true, capturable: true, images: vec![image], targets: vec![(0, 0)] });
        }
        let index = 0u32;
        let info = PresentInfo { s_type: 1000001001, next: 0, wait_count: 0, waits: std::ptr::null(), swap_count: 1, swaps: &swap_id, indices: &index, results: std::ptr::null_mut() };
        assert!(capture(queue, &info, None).is_some(), "the frame was copied");
        let start = std::time::Instant::now();
        let p = loop {
            if let Some(p) = capture::published() {
                break p;
            }
            assert!(start.elapsed().as_secs() < 5, "published within 5 s");
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!((p.slot, p.ready), (0, 0), "published once finished");
        unsafe { idle(queue) };
        let start = std::time::Instant::now();
        while crate::sys::lock(&WORLD).devices[&dev].capture.as_ref().unwrap().in_flight.iter().any(|g| g.load(Ordering::Acquire) != 0) {
            assert!(start.elapsed().as_secs() < 5, "the published slot is let go");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let handle = crate::sys::lock(&WORLD).devices[&dev].capture.as_ref().unwrap().ring.as_ref().unwrap().handle(p.slot);
        let (other, octx) = private_device(luid).expect("a Direct3D 11 device on the adapter");
        let dev1 = com::query(other, &com::IID_ID3D11DEVICE1);
        let mut tex = 0usize;
        let hr = com::vcall!(dev1, 48, fn(windows_sys::Win32::Foundation::HANDLE, *const com::Guid, *mut usize) -> i32, handle, &com::IID_ID3D11TEXTURE2D, &mut tex);
        assert!(hr >= 0 && tex != 0, "OpenSharedResource1 {hr:#x}");
        let px = super::super::super::d3d11::read_pixels(other, octx, tex, w, h);
        assert_eq!(px[(h / 2 * w + w / 2) as usize] & 0x00ff_ffff, 0x00ff00, "green arrived");
        capture::configure(false, None, None);

        // Asked for pixels: read back into the pixel file instead.
        let (ring, dir) = capture::tests::session_ring_file();
        capture::configure_as(true, Some(&ring), None, None, true);
        assert!(capture(queue, &info, None).is_some(), "the frame was read back");
        let start = std::time::Instant::now();
        let p = loop {
            if let Some(p) = capture::published() {
                break p;
            }
            assert!(start.elapsed().as_secs() < 5, "published within 5 s");
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        unsafe { idle(queue) };
        let bytes = capture::tests::pixel_slot(&dir.0, p.slot, peerparty_protocol::frame_ring::slot_bytes(w * 4, h).unwrap(), (w * h * 4) as usize);
        let mid = ((h / 2 * w + w / 2) * 4) as usize;
        assert_eq!(&bytes[mid..mid + 4], &[0, 0xff, 0, 0xff], "green (BGRA) in the pixel file");

        capture::configure(false, None, None);
        // The game destroys its device: PeerParty's objects on it go first, its copies'
        // thread ends, and nothing of the device is kept.
        super::super::forget_device(dev);
        {
            let world = crate::sys::lock(&WORLD);
            assert!(!world.devices.contains_key(&dev) && !world.queues.contains_key(&queue) && !world.swaps.contains_key(&swap_id));
        }
        unsafe {
            (fx.destroy_pool)(dev, pool, 0);
            (fx.destroy_image)(dev, image, 0);
            (fx.free_mem)(dev, mem, 0);
        }
        let destroy_dev: unsafe extern "system" fn(usize, usize) = unsafe { std::mem::transmute(dp(c"vkDestroyDevice")) };
        unsafe { destroy_dev(dev, 0) };
        for o in [tex, dev1, octx, other] {
            com::release(o);
        }
    }
}
