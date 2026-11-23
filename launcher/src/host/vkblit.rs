//! The Linux session's window drawing its copies' frames (x11.rs), with Vulkan: each
//! pane's newest frame, read back by its hook into a pixel file (`ring::PixelFrames`), is
//! turned into BGRA rows (tonemap.rs) straight into a host-visible staging buffer, copied
//! into an image of the pane's own, and blitted (scaled, filtered) into the target: the
//! swap chain's image, or in the tests an image of their own. No shaders, so nothing to
//! compile: a blit scales and converts formats by itself.
//!
//! Vulkan is loaded at run time (libvulkan.so.1; vulkan-1.dll for the tests on Windows),
//! so a PC without it just has no window. One frame is in flight: the CPU waits for the
//! last one's fence before reusing its staging buffers, which is this thread's own GPU
//! work and never a copy's.

// On Windows only the tests use it.
#![cfg_attr(windows, allow(dead_code, unused_imports))]

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;

use peerparty_protocol::session::PaneRect;

use super::ring::PixelFrames;

pub type Pfn = usize;

/// The functions used, looked up through `vkGetInstanceProcAddr` / `vkGetDeviceProcAddr`.
pub struct Vk {
    pub gipa: unsafe extern "system" fn(usize, *const c_char) -> Pfn,
    _lib: Library,
}

/// The Vulkan loader, kept loaded.
struct Library(*mut c_void);

unsafe impl Send for Library {}

impl Drop for Library {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::dlclose(self.0);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::FreeLibrary(self.0.cast());
        }
    }
}

impl Library {
    fn open() -> Option<(Library, Pfn)> {
        #[cfg(unix)]
        {
            for name in [c"libvulkan.so.1", c"libvulkan.so"] {
                let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
                if !h.is_null() {
                    let f = unsafe { libc::dlsym(h, c"vkGetInstanceProcAddr".as_ptr()) };
                    if !f.is_null() {
                        return Some((Library(h), f as Pfn));
                    }
                }
            }
            None
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
            let name: Vec<u16> = "vulkan-1.dll".encode_utf16().chain(Some(0)).collect();
            let h = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
            if h.is_null() {
                return None;
            }
            let f = unsafe { GetProcAddress(h, c"vkGetInstanceProcAddr".as_ptr().cast()) }?;
            Some((Library(h.cast()), f as Pfn))
        }
    }
}

impl Vk {
    pub fn load() -> Option<Vk> {
        let (lib, gipa) = Library::open()?;
        Some(Vk { gipa: unsafe { std::mem::transmute::<Pfn, unsafe extern "system" fn(usize, *const c_char) -> Pfn>(gipa) }, _lib: lib })
    }

    pub fn instance_fn(&self, instance: usize, name: &CStr) -> Pfn {
        unsafe { (self.gipa)(instance, name.as_ptr()) }
    }
}

/// Casts a looked-up function to its type (None when it isn't there).
macro_rules! vkfn {
    ($p:expr, $t:ty) => {{
        let p: Pfn = $p;
        if p == 0 { None } else { Some(unsafe { std::mem::transmute::<Pfn, $t>(p) }) }
    }};
}
pub(crate) use vkfn;

#[repr(C)]
pub struct ImageBarrier {
    pub s_type: u32,
    pub next: usize,
    pub src_access: u32,
    pub dst_access: u32,
    pub old_layout: u32,
    pub new_layout: u32,
    pub src_family: u32,
    pub dst_family: u32,
    pub image: u64,
    pub aspect: u32,
    pub base_mip: u32,
    pub mips: u32,
    pub base_layer: u32,
    pub layers: u32,
}

#[repr(C)]
struct BufferImageCopy {
    offset: u64,
    row_length: u32,
    image_height: u32,
    sub: [u32; 4],
    offset3: [i32; 3],
    extent: [u32; 3],
}

#[repr(C)]
struct ImageInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    image_type: u32,
    format: u32,
    extent: [u32; 3],
    mips: u32,
    layers: u32,
    samples: u32,
    tiling: u32,
    usage: u32,
    sharing: u32,
    family_count: u32,
    families: usize,
    initial_layout: u32,
}

#[repr(C)]
struct BufferInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    size: u64,
    usage: u32,
    sharing: u32,
    family_count: u32,
    families: usize,
}

#[repr(C)]
struct MemAlloc {
    s_type: u32,
    next: usize,
    size: u64,
    type_index: u32,
}

#[repr(C)]
pub struct SubmitInfo {
    pub s_type: u32,
    pub next: usize,
    pub wait_count: u32,
    pub waits: *const u64,
    pub stages: *const u32,
    pub cmd_count: u32,
    pub cmds: *const usize,
    pub signal_count: u32,
    pub signals: *const u64,
}

pub const UNDEFINED: u32 = 0;
pub const TRANSFER_SRC: u32 = 6;
pub const TRANSFER_DST: u32 = 7;
pub const PRESENT_SRC: u32 = 1000001002;
const IGNORED: u32 = u32::MAX;
const B8G8R8A8_UNORM: u32 = 44;

/// The device-level functions the blits use.
pub struct DeviceFns {
    pub create_pool: unsafe extern "system" fn(usize, *const [usize; 3], usize, *mut u64) -> i32,
    pub destroy_pool: unsafe extern "system" fn(usize, u64, usize),
    pub alloc_cmd: unsafe extern "system" fn(usize, *const AllocInfo, *mut usize) -> i32,
    pub begin: unsafe extern "system" fn(usize, *const [usize; 4]) -> i32,
    pub end: unsafe extern "system" fn(usize) -> i32,
    pub reset_cmd: unsafe extern "system" fn(usize, u32) -> i32,
    pub barrier: unsafe extern "system" fn(usize, u32, u32, u32, u32, usize, u32, usize, u32, *const ImageBarrier),
    copy_to_image: unsafe extern "system" fn(usize, u64, u64, u32, u32, *const BufferImageCopy),
    #[cfg_attr(not(test), allow(dead_code))]
    copy_to_buffer: unsafe extern "system" fn(usize, u64, u32, u64, u32, *const BufferImageCopy),
    blit: unsafe extern "system" fn(usize, u64, u32, u64, u32, u32, *const [i32; 20], u32),
    clear: unsafe extern "system" fn(usize, u64, u32, *const [f32; 4], u32, *const [u32; 5]),
    pub submit: unsafe extern "system" fn(usize, u32, *const SubmitInfo, u64) -> i32,
    pub create_fence: unsafe extern "system" fn(usize, *const [usize; 3], usize, *mut u64) -> i32,
    pub destroy_fence: unsafe extern "system" fn(usize, u64, usize),
    pub wait_fences: unsafe extern "system" fn(usize, u32, *const u64, u32, u64) -> i32,
    pub reset_fences: unsafe extern "system" fn(usize, u32, *const u64) -> i32,
    pub create_semaphore: unsafe extern "system" fn(usize, *const [usize; 3], usize, *mut u64) -> i32,
    pub destroy_semaphore: unsafe extern "system" fn(usize, u64, usize),
    create_image: unsafe extern "system" fn(usize, *const ImageInfo, usize, *mut u64) -> i32,
    destroy_image: unsafe extern "system" fn(usize, u64, usize),
    image_reqs: unsafe extern "system" fn(usize, u64, *mut [u64; 3]),
    bind_image: unsafe extern "system" fn(usize, u64, u64, u64) -> i32,
    create_buffer: unsafe extern "system" fn(usize, *const BufferInfo, usize, *mut u64) -> i32,
    destroy_buffer: unsafe extern "system" fn(usize, u64, usize),
    buffer_reqs: unsafe extern "system" fn(usize, u64, *mut [u64; 3]),
    bind_buffer: unsafe extern "system" fn(usize, u64, u64, u64) -> i32,
    alloc_mem: unsafe extern "system" fn(usize, *const MemAlloc, usize, *mut u64) -> i32,
    free_mem: unsafe extern "system" fn(usize, u64, usize),
    map: unsafe extern "system" fn(usize, u64, u64, u64, u32, *mut *mut u8) -> i32,
    pub device_idle: unsafe extern "system" fn(usize) -> i32,
    pub destroy_device: unsafe extern "system" fn(usize, usize),
}

#[repr(C)]
pub struct AllocInfo {
    pub s_type: u32,
    pub next: usize,
    pub pool: u64,
    pub level: u32,
    pub count: u32,
}

/// A pane's image and staging buffer, for frames of one size.
struct PaneImage {
    /// The frames it's for (a new generation's are new ones).
    frames: Arc<PixelFrames>,
    image: u64,
    image_mem: u64,
    buffer: u64,
    buffer_mem: u64,
    mapped: *mut u8,
    /// The slot and sequence in it, and whether the image holds them yet (copied).
    staged: Option<(usize, u32)>,
    copied: bool,
    /// The layout the image is in.
    layout: u32,
}

/// A pane to draw: where, and its frames with the slot and sequence to show.
pub struct BlitPane {
    pub dest: PaneRect,
    pub frames: Option<Arc<PixelFrames>>,
    pub take: Option<(usize, u32)>,
}

/// The device, its queue, and the panes' images.
pub struct Blitter {
    pub vk: Vk,
    pub instance: usize,
    pub phys: usize,
    pub dev: usize,
    pub queue: usize,
    #[allow(dead_code)]
    pub family: u32,
    pub fx: DeviceFns,
    pool: u64,
    pub cmd: usize,
    pub fence: u64,
    /// The last submission's fence hasn't been waited for.
    in_flight: bool,
    mem_types: Vec<u32>,
    panes: Vec<Option<PaneImage>>,
    destroy_instance: unsafe extern "system" fn(usize, usize),
}

unsafe impl Send for Blitter {}

#[repr(C)]
struct AppInfo {
    s_type: u32,
    next: usize,
    name: *const c_char,
    version: u32,
    engine: *const c_char,
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
    exts: *const *const c_char,
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

#[repr(C)]
struct DeviceInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    queue_count: u32,
    queues: *const QueueInfo,
    layer_count: u32,
    layers: usize,
    ext_count: u32,
    exts: *const *const c_char,
    features: usize,
}

impl Blitter {
    /// An instance with `instance_exts`, then `surface(instance)` (a window's surface,
    /// 0 for none), and a device on the first physical device with a queue that can
    /// transfer and, given a surface, present to it (`can_present`), with
    /// `device_exts`. The surface is handed back with it.
    pub fn new(
        instance_exts: &[&CStr],
        device_exts: &[&CStr],
        surface: impl FnOnce(&Vk, usize) -> u64,
        can_present: impl Fn(&Vk, usize, usize, u32, u64) -> bool,
    ) -> Option<(Blitter, u64)> {
        let vk = Vk::load()?;
        type CreateInstance = unsafe extern "system" fn(*const InstanceInfo, usize, *mut usize) -> i32;
        let create_instance = vkfn!(vk.instance_fn(0, c"vkCreateInstance"), CreateInstance)?;
        let app = AppInfo { s_type: 0, next: 0, name: c"PeerParty".as_ptr(), version: 1, engine: std::ptr::null(), engine_version: 0, api: 1 << 22 };
        let exts: Vec<*const c_char> = instance_exts.iter().map(|e| e.as_ptr()).collect();
        let ii = InstanceInfo { s_type: 1, next: 0, flags: 0, app: &app, layer_count: 0, layers: 0, ext_count: exts.len() as u32, exts: exts.as_ptr() };
        let mut instance = 0usize;
        if unsafe { create_instance(&ii, 0, &mut instance) } != 0 || instance == 0 {
            return None;
        }
        let destroy_instance = vkfn!(vk.instance_fn(instance, c"vkDestroyInstance"), unsafe extern "system" fn(usize, usize))?;
        let s = surface(&vk, instance);
        let made = Self::with_instance(vk, instance, destroy_instance, s, device_exts, can_present);
        if made.is_none() {
            // The instance (and the surface with it) goes.
            unsafe { destroy_instance(instance, 0) };
        }
        made.map(|b| (b, s))
    }

    fn with_instance(
        vk: Vk,
        instance: usize,
        destroy_instance: unsafe extern "system" fn(usize, usize),
        surface: u64,
        device_exts: &[&CStr],
        can_present: impl Fn(&Vk, usize, usize, u32, u64) -> bool,
    ) -> Option<Blitter> {
        type Enumerate = unsafe extern "system" fn(usize, *mut u32, *mut usize) -> i32;
        type Families = unsafe extern "system" fn(usize, *mut u32, *mut [u32; 6]);
        type MemProps = unsafe extern "system" fn(usize, *mut u8);
        let enumerate = vkfn!(vk.instance_fn(instance, c"vkEnumeratePhysicalDevices"), Enumerate)?;
        let families = vkfn!(vk.instance_fn(instance, c"vkGetPhysicalDeviceQueueFamilyProperties"), Families)?;
        let mem_props = vkfn!(vk.instance_fn(instance, c"vkGetPhysicalDeviceMemoryProperties"), MemProps)?;
        let mut n = 0u32;
        unsafe { enumerate(instance, &mut n, std::ptr::null_mut()) };
        let mut physs = vec![0usize; n as usize];
        unsafe { enumerate(instance, &mut n, physs.as_mut_ptr()) };
        let (phys, family) = physs.iter().take(n as usize).find_map(|&p| {
            let mut fc = 0u32;
            unsafe { families(p, &mut fc, std::ptr::null_mut()) };
            let mut props = vec![[0u32; 6]; fc as usize];
            unsafe { families(p, &mut fc, props.as_mut_ptr()) };
            // A graphics queue (they all transfer), presenting where there's a surface.
            (0..fc).find(|&f| props[f as usize][0] & 1 != 0 && can_present(&vk, instance, p, f, surface)).map(|f| (p, f))
        })?;
        let priority = 1.0f32;
        let qi = QueueInfo { s_type: 2, next: 0, flags: 0, family, count: 1, priorities: &priority };
        let exts: Vec<*const c_char> = device_exts.iter().map(|e| e.as_ptr()).collect();
        let di = DeviceInfo { s_type: 3, next: 0, flags: 0, queue_count: 1, queues: &qi, layer_count: 0, layers: 0, ext_count: exts.len() as u32, exts: exts.as_ptr(), features: 0 };
        type CreateDevice = unsafe extern "system" fn(usize, *const DeviceInfo, usize, *mut usize) -> i32;
        let create_device = vkfn!(vk.instance_fn(instance, c"vkCreateDevice"), CreateDevice)?;
        let mut dev = 0usize;
        if unsafe { create_device(phys, &di, 0, &mut dev) } != 0 || dev == 0 {
            return None;
        }
        type Gdpa = unsafe extern "system" fn(usize, *const c_char) -> Pfn;
        let gdpa = vkfn!(vk.instance_fn(instance, c"vkGetDeviceProcAddr"), Gdpa)?;
        let d = |n: &CStr| unsafe { gdpa(dev, n.as_ptr()) };
        macro_rules! f {
            ($n:literal) => {
                vkfn!(d($n), _)?
            };
        }
        let fx = DeviceFns {
            create_pool: f!(c"vkCreateCommandPool"),
            destroy_pool: f!(c"vkDestroyCommandPool"),
            alloc_cmd: f!(c"vkAllocateCommandBuffers"),
            begin: f!(c"vkBeginCommandBuffer"),
            end: f!(c"vkEndCommandBuffer"),
            reset_cmd: f!(c"vkResetCommandBuffer"),
            barrier: f!(c"vkCmdPipelineBarrier"),
            copy_to_image: f!(c"vkCmdCopyBufferToImage"),
            copy_to_buffer: f!(c"vkCmdCopyImageToBuffer"),
            blit: f!(c"vkCmdBlitImage"),
            clear: f!(c"vkCmdClearColorImage"),
            submit: f!(c"vkQueueSubmit"),
            create_fence: f!(c"vkCreateFence"),
            destroy_fence: f!(c"vkDestroyFence"),
            wait_fences: f!(c"vkWaitForFences"),
            reset_fences: f!(c"vkResetFences"),
            create_semaphore: f!(c"vkCreateSemaphore"),
            destroy_semaphore: f!(c"vkDestroySemaphore"),
            create_image: f!(c"vkCreateImage"),
            destroy_image: f!(c"vkDestroyImage"),
            image_reqs: f!(c"vkGetImageMemoryRequirements"),
            bind_image: f!(c"vkBindImageMemory"),
            create_buffer: f!(c"vkCreateBuffer"),
            destroy_buffer: f!(c"vkDestroyBuffer"),
            buffer_reqs: f!(c"vkGetBufferMemoryRequirements"),
            bind_buffer: f!(c"vkBindBufferMemory"),
            alloc_mem: f!(c"vkAllocateMemory"),
            free_mem: f!(c"vkFreeMemory"),
            map: f!(c"vkMapMemory"),
            device_idle: f!(c"vkDeviceWaitIdle"),
            destroy_device: f!(c"vkDestroyDevice"),
        };
        type GetQueue = unsafe extern "system" fn(usize, u32, u32, *mut usize);
        let get_queue: GetQueue = vkfn!(d(c"vkGetDeviceQueue"), GetQueue)?;
        let mut queue = 0usize;
        unsafe { get_queue(dev, family, 0, &mut queue) };
        let mut props = vec![0u8; 520];
        unsafe { mem_props(phys, props.as_mut_ptr()) };
        let count = u32::from_le_bytes(props[0..4].try_into().ok()?).min(32);
        let mem_types = (0..count as usize).map(|i| u32::from_le_bytes(props[4 + i * 8..8 + i * 8].try_into().unwrap())).collect();
        let mut b = Blitter { vk, instance, phys, dev, queue, family, fx, pool: 0, cmd: 0, fence: 0, in_flight: false, mem_types, panes: Vec::new(), destroy_instance };
        // VkCommandPoolCreateInfo (64-bit layout: sType, pNext, flags and family in one
        // word): RESET_COMMAND_BUFFER. A fence made signalled so the first wait passes.
        let pool_info: [usize; 3] = [39, 0, 0x2 | (family as usize) << 32];
        let fence_info: [usize; 3] = [8, 0, 1];
        if unsafe { (b.fx.create_pool)(dev, &pool_info, 0, &mut b.pool) } != 0 || unsafe { (b.fx.create_fence)(dev, &fence_info, 0, &mut b.fence) } != 0 {
            return None;
        }
        let ai = AllocInfo { s_type: 40, next: 0, pool: b.pool, level: 0, count: 1 };
        if unsafe { (b.fx.alloc_cmd)(dev, &ai, &mut b.cmd) } != 0 {
            return None;
        }
        Some(b)
    }

    fn memory_type(&self, bits: u64, wanted: u32) -> Option<u32> {
        (0..self.mem_types.len() as u32).find(|&i| bits & (1 << i) != 0 && self.mem_types[i as usize] & wanted == wanted)
    }

    fn alloc(&self, reqs: [u64; 3], wanted: &[u32]) -> Option<u64> {
        let type_index = wanted.iter().find_map(|w| self.memory_type(reqs[2], *w))?;
        let ma = MemAlloc { s_type: 5, next: 0, size: reqs[0], type_index };
        let mut mem = 0u64;
        (unsafe { (self.fx.alloc_mem)(self.dev, &ma, 0, &mut mem) } == 0).then_some(mem)
    }

    /// An image of `w x h` BGRA (`usage`), in device memory.
    pub fn image(&self, w: u32, h: u32, usage: u32) -> Option<(u64, u64)> {
        let ii = ImageInfo { s_type: 14, next: 0, flags: 0, image_type: 1, format: B8G8R8A8_UNORM, extent: [w, h, 1], mips: 1, layers: 1, samples: 1, tiling: 0, usage, sharing: 0, family_count: 0, families: 0, initial_layout: 0 };
        let mut image = 0u64;
        if unsafe { (self.fx.create_image)(self.dev, &ii, 0, &mut image) } != 0 {
            return None;
        }
        let mut reqs = [0u64; 3];
        unsafe { (self.fx.image_reqs)(self.dev, image, &mut reqs) };
        let Some(mem) = self.alloc(reqs, &[0x1, 0]).filter(|m| unsafe { (self.fx.bind_image)(self.dev, image, *m, 0) } == 0) else {
            unsafe { (self.fx.destroy_image)(self.dev, image, 0) };
            return None;
        };
        Some((image, mem))
    }

    /// A host-visible, coherent, mapped buffer of `size` bytes (`usage`), cached where
    /// there's such memory (the CPU reads it in the tests).
    pub fn buffer(&self, size: usize, usage: u32) -> Option<(u64, u64, *mut u8)> {
        let bi = BufferInfo { s_type: 12, next: 0, flags: 0, size: size as u64, usage, sharing: 0, family_count: 0, families: 0 };
        let mut buf = 0u64;
        if unsafe { (self.fx.create_buffer)(self.dev, &bi, 0, &mut buf) } != 0 {
            return None;
        }
        let mut reqs = [0u64; 3];
        unsafe { (self.fx.buffer_reqs)(self.dev, buf, &mut reqs) };
        let mut ptr = std::ptr::null_mut();
        let mem = self.alloc(reqs, &[0x2 | 0x4 | 0x8, 0x2 | 0x4]).filter(|m| {
            unsafe { (self.fx.bind_buffer)(self.dev, buf, *m, 0) == 0 && (self.fx.map)(self.dev, *m, 0, u64::MAX, 0, &mut ptr) == 0 }
        });
        match mem {
            Some(m) => Some((buf, m, ptr)),
            None => {
                unsafe { (self.fx.destroy_buffer)(self.dev, buf, 0) };
                None
            }
        }
    }

    fn drop_pane(&self, p: PaneImage) {
        unsafe {
            (self.fx.destroy_image)(self.dev, p.image, 0);
            (self.fx.free_mem)(self.dev, p.image_mem, 0);
            (self.fx.destroy_buffer)(self.dev, p.buffer, 0);
            (self.fx.free_mem)(self.dev, p.buffer_mem, 0);
        }
    }

    /// Waits for the last submission (its staging buffers and images are free after).
    pub fn wait(&mut self) {
        if self.in_flight {
            unsafe { (self.fx.wait_fences)(self.dev, 1, &self.fence, 1, u64::MAX) };
            self.in_flight = false;
        }
    }

    /// Pane `i`'s frames staged: the slot `take` says turned into BGRA rows in its staging
    /// buffer, unless it already is. Called before `record`, after `wait`.
    fn stage(&mut self, i: usize, frames: &Arc<PixelFrames>, take: (usize, u32)) -> bool {
        if !super::tonemap::supported(frames.format) {
            return false;
        }
        if self.panes.len() <= i {
            self.panes.resize_with(i + 1, || None);
        }
        let same = self.panes[i].as_ref().is_some_and(|p| Arc::ptr_eq(&p.frames, frames));
        if !same {
            if let Some(old) = self.panes[i].take() {
                self.drop_pane(old);
            }
            let (w, h) = (frames.width, frames.height);
            let Some((image, image_mem)) = self.image(w, h, 0x1 | 0x2) else { return false };
            let Some((buffer, buffer_mem, mapped)) = self.buffer(w as usize * h as usize * 4, 0x1) else {
                unsafe {
                    (self.fx.destroy_image)(self.dev, image, 0);
                    (self.fx.free_mem)(self.dev, image_mem, 0);
                }
                return false;
            };
            self.panes[i] = Some(PaneImage { frames: frames.clone(), image, image_mem, buffer, buffer_mem, mapped, staged: None, copied: false, layout: UNDEFINED });
        }
        let Some(p) = self.panes[i].as_mut() else { return false };
        if p.staged != Some(take) {
            let (w, h) = (frames.width as usize, frames.height as usize);
            let dst = unsafe { std::slice::from_raw_parts_mut(p.mapped, w * h * 4) };
            super::tonemap::to_bgra(frames.format, frames.color_space, frames.slot(take.0), frames.stride as usize, w, h, dst);
            p.staged = Some(take);
            p.copied = false;
        }
        true
    }

    /// Records into `cmd`: `target` (an image of `size`, in `layout`) cleared black, each
    /// pane's staged frame copied into its image and blitted into its destination, and
    /// the target left in `final_layout`. The panes drawn.
    pub fn record(&mut self, target: u64, size: (u32, u32), layout: u32, final_layout: u32, panes: &[BlitPane]) -> Vec<usize> {
        let mut drawn = Vec::new();
        for (i, pane) in panes.iter().enumerate() {
            if let (Some(f), Some(t)) = (&pane.frames, pane.take)
                && pane.dest.width > 0
                && pane.dest.height > 0
                && self.stage(i, f, t)
            {
                drawn.push(i);
            }
        }
        let fx = &self.fx;
        let cmd = self.cmd;
        let bar = |image, old, new, src, dst| ImageBarrier {
            s_type: 45,
            next: 0,
            src_access: src,
            dst_access: dst,
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
        let range: [u32; 5] = [1, 0, 1, 0, 1];
        unsafe {
            (fx.reset_cmd)(cmd, 0);
            // VkCommandBufferBeginInfo: ONE_TIME_SUBMIT, no inheritance.
            let begin: [usize; 4] = [42, 0, 0x1, 0];
            (fx.begin)(cmd, &begin);
            // TRANSFER stage (0x1000) on both sides of every barrier: all of this is copies.
            (fx.barrier)(cmd, 0x1000, 0x1000, 0, 0, 0, 0, 0, 1, &bar(target, layout, TRANSFER_DST, 0, 0x1000));
            (fx.clear)(cmd, target, TRANSFER_DST, &[0.0, 0.0, 0.0, 1.0], 1, &range);
        }
        for &i in &drawn {
            let Some(p) = self.panes[i].as_mut() else { continue };
            let (w, h) = (p.frames.width, p.frames.height);
            unsafe {
                if !p.copied {
                    (fx.barrier)(cmd, 0x1000, 0x1000, 0, 0, 0, 0, 0, 1, &bar(p.image, p.layout, TRANSFER_DST, 0, 0x1000));
                    let region = BufferImageCopy { offset: 0, row_length: 0, image_height: 0, sub: [1, 0, 0, 1], offset3: [0; 3], extent: [w, h, 1] };
                    (fx.copy_to_image)(cmd, p.buffer, p.image, TRANSFER_DST, 1, &region);
                    (fx.barrier)(cmd, 0x1000, 0x1000, 0, 0, 0, 0, 0, 1, &bar(p.image, TRANSFER_DST, TRANSFER_SRC, 0x1000, 0x800));
                    p.layout = TRANSFER_SRC;
                    p.copied = true;
                }
                // The pane, inside the target (a blit can't reach past it).
                let d = panes[i].dest;
                let (x0, y0) = (d.x.clamp(0, size.0 as i32), d.y.clamp(0, size.1 as i32));
                let x1 = d.x.saturating_add(d.width as i32).clamp(x0, size.0 as i32);
                let y1 = d.y.saturating_add(d.height as i32).clamp(y0, size.1 as i32);
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                // VkImageBlit: source layer, its whole extent; destination layer, the pane.
                let blit: [i32; 20] = [1, 0, 0, 1, 0, 0, 0, w as i32, h as i32, 1, 1, 0, 0, 1, x0, y0, 0, x1, y1, 1];
                (fx.blit)(cmd, p.image, TRANSFER_SRC, target, TRANSFER_DST, 1, &blit, 1); // LINEAR
            }
        }
        unsafe {
            let to = if final_layout == PRESENT_SRC { 0 } else { 0x800 };
            (fx.barrier)(cmd, 0x1000, 0x1000 | 0x2000, 0, 0, 0, 0, 0, 1, &bar(target, TRANSFER_DST, final_layout, 0x1000, to));
        }
        drawn
    }

    /// Ends `cmd` and submits it, after `wait` (0: nothing) and signalling `signal`.
    pub fn submit(&mut self, wait: u64, signal: u64) -> bool {
        let stage = 0x1000u32;
        unsafe {
            (self.fx.end)(self.cmd);
            (self.fx.reset_fences)(self.dev, 1, &self.fence);
        }
        let si = SubmitInfo {
            s_type: 4,
            next: 0,
            wait_count: u32::from(wait != 0),
            waits: &wait,
            stages: &stage,
            cmd_count: 1,
            cmds: &self.cmd,
            signal_count: u32::from(signal != 0),
            signals: &signal,
        };
        let ok = unsafe { (self.fx.submit)(self.queue, 1, &si, self.fence) } == 0;
        self.in_flight = ok;
        ok
    }

    /// Pane `i`'s images go (its copy left, or it's shown another way).
    pub fn forget(&mut self, i: usize) {
        if let Some(p) = self.panes.get_mut(i).and_then(Option::take) {
            self.wait();
            self.drop_pane(p);
        }
    }

    /// Copies `image` (BGRA, `w x h`, in TRANSFER_SRC) into `buffer` (tests).
    #[cfg(test)]
    fn read_back(&mut self, image: u64, w: u32, h: u32, buffer: u64) {
        unsafe {
            (self.fx.reset_cmd)(self.cmd, 0);
            (self.fx.begin)(self.cmd, &[42, 0, 1, 0]);
            let region = BufferImageCopy { offset: 0, row_length: 0, image_height: 0, sub: [1, 0, 0, 1], offset3: [0; 3], extent: [w, h, 1] };
            (self.fx.copy_to_buffer)(self.cmd, image, TRANSFER_SRC, buffer, 1, &region);
        }
        self.submit(0, 0);
        self.wait();
    }
}

impl Drop for Blitter {
    fn drop(&mut self) {
        unsafe { (self.fx.device_idle)(self.dev) };
        for p in std::mem::take(&mut self.panes).into_iter().flatten() {
            self.drop_pane(p);
        }
        unsafe {
            (self.fx.destroy_fence)(self.dev, self.fence, 0);
            (self.fx.destroy_pool)(self.dev, self.pool, 0);
            (self.fx.destroy_device)(self.dev, 0);
            (self.destroy_instance)(self.instance, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::frame_ring::{pixels_size, slot_bytes};
    use peerparty_protocol::ipc::SurfaceMsg;

    /// A pixel file of `w x h` BGRA frames whose slot 2 is `color`, mapped.
    fn frames(dir: &std::path::Path, w: u32, h: u32, color: [u8; 4], format: u32, color_space: u32) -> Arc<PixelFrames> {
        let stride = w * 4;
        let mut bytes = vec![0u8; pixels_size(stride, h).unwrap()];
        let s = slot_bytes(stride, h).unwrap() * 2;
        for px in bytes[s..s + (stride * h) as usize].chunks_mut(4) {
            px.copy_from_slice(&color);
        }
        let path = dir.join(format!("pixels-{w}-{h}-{format}"));
        std::fs::write(&path, &bytes).unwrap();
        let m = SurfaceMsg {
            pid: 1,
            generation: 1,
            textures: Vec::new(),
            content: 0,
            release: 0,
            width: w,
            height: h,
            format,
            api: "vulkan".into(),
            color_space,
            pixels: Some(path.to_string_lossy().into_owned()),
            stride,
        };
        Arc::new(PixelFrames::open(&m).unwrap())
    }

    /// Two panes' frames blitted, scaled, into a target image on this PC's Vulkan device
    /// (skipped without one): each lands in its pane, black around them, an HDR10 one
    /// tone-mapped.
    #[test]
    fn panes_are_blitted_into_their_places() {
        let Some((mut b, _)) = Blitter::new(&[], &[], |_, _| 0, |_, _, _, _, _| true) else {
            eprintln!("no Vulkan device: skipped");
            return;
        };
        let dir = std::env::temp_dir().join(format!("peerparty-vkblit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let red = frames(&dir, 8, 8, [0, 0, 255, 255], 87, 0);
        // PQ code 520 (100 nits) grey, R10G10B10A2.
        let grey = (520u32 | 520 << 10 | 520 << 20 | 3 << 30).to_le_bytes();
        let hdr = frames(&dir, 4, 4, grey, 24, 12);
        let (w, h) = (64u32, 32u32);
        let (target, target_mem) = b.image(w, h, 0x1 | 0x2).expect("a target");
        let panes = [
            BlitPane { dest: PaneRect { x: 0, y: 0, width: 32, height: 32 }, frames: Some(red.clone()), take: Some((2, 7)) },
            BlitPane { dest: PaneRect { x: 40, y: 8, width: 16, height: 16 }, frames: Some(hdr.clone()), take: Some((2, 1)) },
            BlitPane { dest: PaneRect { x: 0, y: 0, width: 0, height: 0 }, frames: None, take: None },
        ];
        b.wait();
        assert_eq!(b.record(target, (w, h), UNDEFINED, TRANSFER_SRC, &panes), vec![0, 1]);
        assert!(b.submit(0, 0));
        b.wait();
        let (buf, buf_mem, ptr) = b.buffer((w * h * 4) as usize, 0x2).expect("a readback buffer");
        b.read_back(target, w, h, buf);
        let px = |x: u32, y: u32| unsafe { std::slice::from_raw_parts(ptr.add(((y * w + x) * 4) as usize), 4) }.to_vec();
        assert_eq!(px(16, 16), vec![0, 0, 255, 255], "red, scaled to its pane");
        assert_eq!(px(36, 16), vec![0, 0, 0, 255], "black between");
        let g = px(48, 16);
        assert!((183..=189).contains(&g[1]) && g[0] == g[1], "100-nit grey tone-mapped: {g:?}");
        assert_eq!(px(48, 2), vec![0, 0, 0, 255], "black above the second pane");
        // The same frame again isn't staged again; the next is.
        assert_eq!(b.panes[0].as_ref().unwrap().staged, Some((2, 7)));
        unsafe {
            (b.fx.destroy_buffer)(b.dev, buf, 0);
            (b.fx.free_mem)(b.dev, buf_mem, 0);
            (b.fx.destroy_image)(b.dev, target, 0);
            (b.fx.free_mem)(b.dev, target_mem, 0);
        }
        b.forget(0);
        assert!(b.panes[0].is_none());
        drop(b);
        drop((red, hdr));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
