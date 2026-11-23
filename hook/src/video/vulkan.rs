//! Vulkan games. Vulkan has no single present function to patch (games fetch the
//! driver's through `vkGetDeviceProcAddr`), so the loader's `vkGetDeviceProcAddr` and
//! `vkGetInstanceProcAddr` hand out PeerParty's versions of the few functions it needs,
//! and the loader's own exports of those are patched too:
//!
//! - `vkCreateDevice` / `vkGetDeviceQueue(2)`: which physical device and queue family each
//!   device and queue belong to.
//! - `vkCreateSwapchainKHR`: notes the images' format and size, and makes sure their
//!   usage includes COLOR_ATTACHMENT (every surface supports it) so the overlay can be
//!   drawn into them.
//! - `vkDestroySwapchainKHR`: PeerParty's views and framebuffers of its images go first.
//! - `vkDestroyDevice`: everything PeerParty made on the device goes first (after the
//!   device is idle), and the thread publishing its copies ends.
//! - `vkCreateWin32SurfaceKHR`: which window each surface is of, so a swap chain's window
//!   is known: reported with the viewport, and a translation layer's (DXVK, Zink) frames
//!   are drawn into and copied here rather than by the hooks of the API above, from its
//!   first frame (`presents_to`).
//! - `vkQueuePresentKHR`: with the overlay showing, a small command buffer uploads the
//!   frame (when it changed) into a texture and draws it over the image being presented
//!   as one quad with premultiplied-alpha blending, like the other APIs: the dimmed
//!   background and rounded corners included. It waits on the game's semaphores and
//!   signals its own, which the real present then waits on.
//!
//! The shaders are Dear ImGui's precompiled SPIR-V (vkshaders.rs): this build has no
//! shader compiler. HDR swap chains take PeerParty's own, generated the same way ahead of
//! time (tools/vk_shaders.py): an scRGB one gets the frame in linear light at SDR white,
//! an HDR10 one PQ-encoded BT.2020, as the Direct3D overlay draws them (d3d11.rs).
//!
//! These hooks go in when the game loads vulkan-1.dll (with the overlay on), because a
//! game creates its swap chain once, at start-up; until the overlay shows, each costs a
//! lookup or a flag check.
//!
//! A split-screen copy whose frames the session's window draws (vkcapture.rs) also gets:
//! the extensions for importing shared textures added to its device, TRANSFER_SRC added
//! to its swap chains' usage, and the loader's exports of these functions patched too
//! (for games that call them directly rather than through the lookups).

use std::collections::HashMap;
use std::ffi::{CStr, c_char};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use super::{com, frame, vkshaders};

#[path = "vkcapture.rs"]
mod vkcapture;
use crate::sys::hook::with_lock;
use crate::log;

type Pfn = usize;
type GetProc = unsafe extern "system" fn(usize, *const c_char) -> Pfn;
type CreateDevice = unsafe extern "system" fn(usize, *const u8, usize, *mut usize) -> i32;
type GetQueue = unsafe extern "system" fn(usize, u32, u32, *mut usize);
type GetQueue2 = unsafe extern "system" fn(usize, *const QueueInfo2, *mut usize);
type CreateSwapchain = unsafe extern "system" fn(usize, *const SwapchainInfo, usize, *mut u64) -> i32;
type QueuePresent = unsafe extern "system" fn(usize, *const PresentInfo) -> i32;
type DestroySwapchain = unsafe extern "system" fn(usize, u64, usize);
type DestroyDevice = unsafe extern "system" fn(usize, usize);
type CreateSurface = unsafe extern "system" fn(usize, *const SurfaceInfo, usize, *mut u64) -> i32;

static GET_DEVICE_PROC: OnceLock<GenericDetour<GetProc>> = OnceLock::new();
static GET_INSTANCE_PROC: OnceLock<GenericDetour<GetProc>> = OnceLock::new();

/// The real functions behind PeerParty's versions.
#[derive(Clone, Copy, Default)]
struct Real {
    create_device: Pfn,
    get_queue: Pfn,
    get_queue2: Pfn,
    create_swapchain: Pfn,
    destroy_swapchain: Pfn,
    present: Pfn,
    destroy_device: Pfn,
    create_surface: Pfn,
}

/// The last ones the game was given by either lookup. `vkGetInstanceProcAddr`'s are the
/// loader's, which work for every device; `vkGetDeviceProcAddr`'s are one device's driver
/// functions, which must only be called for that device (`DEV_REAL`).
static REAL: Mutex<Real> = Mutex::new(Real {
    create_device: 0,
    get_queue: 0,
    get_queue2: 0,
    create_swapchain: 0,
    destroy_swapchain: 0,
    present: 0,
    destroy_device: 0,
    create_surface: 0,
});

/// Per device: the real functions `vkGetDeviceProcAddr` gave for it.
static DEV_REAL: LazyLock<Mutex<HashMap<usize, Real>>> = LazyLock::new(Mutex::default);

/// The real `which` for `dev`: its own driver's when the game looked it up on that device,
/// else the last one handed out.
fn real_for(dev: Option<usize>, which: fn(&Real) -> Pfn) -> Pfn {
    if let Some(d) = dev
        && let Some(r) = crate::sys::lock(&DEV_REAL).get(&d)
        && which(r) != 0
    {
        return which(r);
    }
    which(&crate::sys::lock(&REAL))
}

/// `VkWin32SurfaceCreateInfoKHR`.
#[repr(C)]
struct SurfaceInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    instance: usize,
    hwnd: usize,
}

/// The surfaces made, and their windows (the most recent last, a few kept).
static SURFACES: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());
const MAX_SURFACES: usize = 32;
/// The swap chains alive and their windows (0 when not known), and how many have one.
static WINDOWS: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());
static WINDOWED: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct QueueInfo2 {
    s_type: u32,
    next: usize,
    flags: u32,
    family: u32,
    index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SwapchainInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    surface: u64,
    min_images: u32,
    format: u32,
    color_space: u32,
    width: u32,
    height: u32,
    layers: u32,
    usage: u32,
    sharing: u32,
    family_count: u32,
    families: usize,
    transform: u32,
    alpha: u32,
    mode: u32,
    clipped: u32,
    old: u64,
}

#[repr(C)]
struct PresentInfo {
    s_type: u32,
    next: usize,
    wait_count: u32,
    waits: *const u64,
    swap_count: u32,
    swaps: *const u64,
    indices: *const u32,
    results: *mut i32,
}

struct Swap {
    /// The device it was made on.
    dev: usize,
    format: u32,
    /// Its `VkColorSpaceKHR`.
    color_space: u32,
    width: u32,
    height: u32,
    /// Its images can be drawn into (COLOR_ATTACHMENT).
    drawable: bool,
    /// Its images can be copied from (TRANSFER_SRC).
    capturable: bool,
    images: Vec<u64>,
    /// Per image: PeerParty's view of it and framebuffer (0 until first drawn into).
    targets: Vec<(u64, u64)>,
}

impl Swap {
    fn new(dev: usize, info: &SwapchainInfo) -> Swap {
        Swap {
            dev,
            format: info.format,
            color_space: info.color_space,
            width: info.width,
            height: info.height,
            drawable: info.usage & 0x10 != 0,
            capturable: info.usage & 0x1 != 0,
            images: Vec::new(),
            targets: Vec::new(),
        }
    }
}

/// Per device: what drawing needs.
struct Dev {
    phys: usize,
    funcs: Option<Funcs>,
    /// Queue family -> command pool, ring of (command buffer, fence, semaphore).
    pools: HashMap<u32, (u64, Vec<(usize, u64, u64)>, usize)>,
    gfx: Option<Gfx>,
    /// Its frames for the session's window, when it can import shared textures.
    capture: Option<vkcapture::VkCapture>,
    /// Its queue families' `VkQueueFlags` (read the first time they're needed).
    families: Vec<u32>,
}

unsafe impl Send for Dev {}

impl Dev {
    fn new(phys: usize, capture: Option<vkcapture::VkCapture>) -> Dev {
        Dev { phys, funcs: None, pools: HashMap::new(), gfx: None, capture, families: Vec::new() }
    }

    /// Queue family `family`'s `VkQueueFlags` (0 when not known).
    fn family_flags(&mut self, family: u32) -> u32 {
        if self.families.is_empty() {
            type Families = unsafe extern "system" fn(usize, *mut u32, *mut [u32; 6]);
            if let Some(f) = com::proc(module(), c"vkGetPhysicalDeviceQueueFamilyProperties") {
                let f: Families = unsafe { std::mem::transmute(f) };
                let mut n = 0u32;
                unsafe { f(self.phys, &mut n, std::ptr::null_mut()) };
                let mut props = vec![[0u32; 6]; n as usize];
                unsafe { f(self.phys, &mut n, props.as_mut_ptr()) };
                self.families = props.iter().take(n as usize).map(|p| p[0]).collect();
            }
        }
        self.families.get(family as usize).copied().unwrap_or(0)
    }

    /// Everything PeerParty made on the device (`dev`, idle), and the views of `swaps`'
    /// images; the copies' thread ends first.
    fn destroy(mut self, dev: usize, swaps: Vec<Swap>) {
        let Some(fx) = self.funcs.take() else {
            // Nothing drawn or copied: nothing of PeerParty's on the device.
            return;
        };
        unsafe { (fx.device_wait_idle)(dev) };
        if let Some(mut c) = self.capture.take() {
            c.destroy(&fx, dev);
        }
        for s in swaps {
            destroy_targets(&fx, dev, s.targets);
        }
        for (_, (pool, ring, _)) in self.pools.drain() {
            destroy_ring(&fx, dev, pool, ring);
        }
        if let Some(g) = self.gfx.take() {
            g.destroy(&fx, dev);
        }
    }
}

/// PeerParty's views and framebuffers of a swap chain's images.
fn destroy_targets(fx: &Funcs, dev: usize, targets: Vec<(u64, u64)>) {
    for (view, fb) in targets {
        unsafe {
            (fx.destroy_framebuffer)(dev, fb, 0);
            (fx.destroy_view)(dev, view, 0);
        }
    }
}

#[derive(Default)]
struct World {
    devices: HashMap<usize, Dev>,
    queues: HashMap<usize, (usize, u32)>,
    swaps: HashMap<u64, Swap>,
}

static WORLD: LazyLock<Mutex<World>> = LazyLock::new(Mutex::default);

pub fn install() -> bool {
    // The real work happened at load (`loaded`); this only says whether it did.
    GET_DEVICE_PROC.get().is_some()
}

/// The module whose Vulkan functions get hooked: vulkan-1.dll on Windows; under Wine its
/// winevulkan.dll, which vulkan-1.dll's functions forward to and which DXVK and VKD3D load
/// themselves (so a Direct3D game's frames are seen there too).
pub(super) fn module() -> windows_sys::Win32::Foundation::HMODULE {
    if crate::sys::wine::is_wine() {
        let w = com::module("winevulkan.dll");
        if !w.is_null() {
            return w;
        }
    }
    com::module("vulkan-1.dll")
}

/// Hooks going in (`loaded` from a load notification and from the session's thread at
/// once go in one after the other).
static LOADING: Mutex<()> = Mutex::new(());
static EXPORTS_PATCHED: AtomicBool = AtomicBool::new(false);

/// The present hooks are wanted with vulkan-1.dll (or winevulkan.dll under Wine) loaded.
/// The hooks go in once; the loader's other exports too once capture is wanted.
pub fn loaded() {
    let _one = crate::sys::lock(&LOADING);
    install_now();
}

/// The game loaded vulkan-1.dll, or winevulkan.dll under Wine (watch.rs, under the loader
/// lock), with the overlay on or capture wanted: as `loaded`, but left to the thread that
/// is putting the hooks in already, if one is (it may need the loader lock this thread
/// holds, to load winevulkan.dll).
pub fn loaded_in_loader() {
    let _one = match LOADING.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    install_now();
}

fn install_now() {
    if GET_DEVICE_PROC.get().is_some() {
        if super::capture::wanted() && !EXPORTS_PATCHED.swap(true, Ordering::AcqRel) {
            patch_exports(module());
        }
        return;
    }
    if crate::sys::wine::is_wine() && com::module("winevulkan.dll").is_null() {
        // vulkan-1.dll came first: what it forwards to, now, so both are seen.
        com::load("winevulkan.dll");
    }
    let m = module();
    if m.is_null() {
        return;
    }
    let (Some(gdpa), Some(gipa)) = (com::proc(m, c"vkGetDeviceProcAddr"), com::proc(m, c"vkGetInstanceProcAddr")) else { return };
    {
        let mut r = crate::sys::lock(&REAL);
        r.create_device = com::proc(m, c"vkCreateDevice").unwrap_or(0);
        r.get_queue = com::proc(m, c"vkGetDeviceQueue").unwrap_or(0);
        r.get_queue2 = com::proc(m, c"vkGetDeviceQueue2").unwrap_or(0);
        r.create_swapchain = com::proc(m, c"vkCreateSwapchainKHR").unwrap_or(0);
        r.destroy_swapchain = com::proc(m, c"vkDestroySwapchainKHR").unwrap_or(0);
        r.present = com::proc(m, c"vkQueuePresentKHR").unwrap_or(0);
        r.destroy_device = com::proc(m, c"vkDestroyDevice").unwrap_or(0);
        r.create_surface = com::proc(m, c"vkCreateWin32SurfaceKHR").unwrap_or(0);
    }
    // The surface export always: a game calling it directly still has its windows known.
    let surface = with_lock(|| patch(&X_CREATE_SURFACE, m, c"vkCreateWin32SurfaceKHR", create_surface_export as CreateSurface));
    if surface == 0 {
        log!("present: Vulkan's vkCreateWin32SurfaceKHR export not patched (a surface made through it has no window known)");
    }
    if super::capture::wanted() && !EXPORTS_PATCHED.swap(true, Ordering::AcqRel) {
        patch_exports(m);
    }
    let ok = with_lock(|| {
        let mut n = 0;
        for (slot, addr, detour) in [(&GET_DEVICE_PROC, gdpa, get_device_proc as GetProc), (&GET_INSTANCE_PROC, gipa, get_instance_proc as GetProc)] {
            let target: GetProc = unsafe { std::mem::transmute(addr) };
            if let Ok(h) = unsafe { GenericDetour::new(target, detour) } {
                let _ = slot.set(h);
                n += usize::from(crate::sys::hook::frozen(addr, || unsafe { slot.get().unwrap().enable() }).is_ok());
            }
        }
        n
    });
    log!("present: Vulkan loaded, {ok} of 2 lookups hooked (the overlay can be drawn into its frames)");
}

type ExportDetour<T> = OnceLock<GenericDetour<T>>;
static X_CREATE_DEVICE: ExportDetour<CreateDevice> = OnceLock::new();
static X_GET_QUEUE: ExportDetour<GetQueue> = OnceLock::new();
static X_GET_QUEUE2: ExportDetour<GetQueue2> = OnceLock::new();
static X_CREATE_SWAPCHAIN: ExportDetour<CreateSwapchain> = OnceLock::new();
static X_DESTROY_SWAPCHAIN: ExportDetour<DestroySwapchain> = OnceLock::new();
static X_PRESENT: ExportDetour<QueuePresent> = OnceLock::new();
static X_DESTROY_DEVICE: ExportDetour<DestroyDevice> = OnceLock::new();
static X_CREATE_SURFACE: ExportDetour<CreateSurface> = OnceLock::new();
/// The loader exports patched (`patch_exports`): a lookup that hands one out isn't
/// wrapped again.
static PATCHED: Mutex<Vec<Pfn>> = Mutex::new(Vec::new());

fn patch<T>(slot: &OnceLock<GenericDetour<T>>, m: windows_sys::Win32::Foundation::HMODULE, name: &CStr, detour: T) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    let Some(addr) = com::proc(m, name) else { return 0 };
    let target: T = unsafe { T::from_ptr(addr as *const ()) };
    let Ok(h) = (unsafe { GenericDetour::new(target, detour) }) else { return 0 };
    if slot.set(h).is_err() {
        return 0; // patched already
    }
    if crate::sys::hook::frozen(addr, || unsafe { slot.get().unwrap().enable() }).is_ok() {
        crate::sys::lock(&PATCHED).push(addr);
        1
    } else {
        0
    }
}

/// The loader's own exports, for a game that calls them directly.
fn patch_exports(m: windows_sys::Win32::Foundation::HMODULE) {
    let n = with_lock(|| {
        patch(&X_CREATE_DEVICE, m, c"vkCreateDevice", create_device_export as CreateDevice)
            + patch(&X_GET_QUEUE, m, c"vkGetDeviceQueue", get_queue_export as GetQueue)
            + patch(&X_GET_QUEUE2, m, c"vkGetDeviceQueue2", get_queue2_export as GetQueue2)
            + patch(&X_CREATE_SWAPCHAIN, m, c"vkCreateSwapchainKHR", create_swapchain_export as CreateSwapchain)
            + patch(&X_DESTROY_SWAPCHAIN, m, c"vkDestroySwapchainKHR", destroy_swapchain_export as DestroySwapchain)
            + patch(&X_PRESENT, m, c"vkQueuePresentKHR", queue_present_export as QueuePresent)
            + patch(&X_DESTROY_DEVICE, m, c"vkDestroyDevice", destroy_device_export as DestroyDevice)
    });
    log!("present: {n} of Vulkan's exports patched too (the session's window draws this copy's frames)");
}

/// Our version of `name`, after noting the real one: for every device, or (a
/// `vkGetDeviceProcAddr` lookup) for `dev`'s.
fn ours(name: &str, real: Pfn, dev: Option<usize>) -> Option<Pfn> {
    if real == 0 || crate::sys::lock(&PATCHED).contains(&real) {
        return None;
    }
    type Set = fn(&mut Real, Pfn);
    let (set, mine): (Set, Pfn) = match name {
        "vkCreateDevice" => (|r, p| r.create_device = p, create_device as *const () as usize),
        "vkGetDeviceQueue" => (|r, p| r.get_queue = p, get_queue as *const () as usize),
        "vkGetDeviceQueue2" => (|r, p| r.get_queue2 = p, get_queue2 as *const () as usize),
        "vkCreateSwapchainKHR" => (|r, p| r.create_swapchain = p, create_swapchain as *const () as usize),
        "vkDestroySwapchainKHR" => (|r, p| r.destroy_swapchain = p, destroy_swapchain as *const () as usize),
        "vkQueuePresentKHR" => (|r, p| r.present = p, queue_present as *const () as usize),
        "vkDestroyDevice" => (|r, p| r.destroy_device = p, destroy_device as *const () as usize),
        "vkCreateWin32SurfaceKHR" => (|r, p| r.create_surface = p, create_surface as *const () as usize),
        _ => return None,
    };
    set(&mut crate::sys::lock(&REAL), real);
    if let Some(d) = dev {
        set(crate::sys::lock(&DEV_REAL).entry(d).or_default(), real);
    }
    Some(mine)
}

fn name_of<'a>(name: *const c_char) -> &'a str {
    if name.is_null() { "" } else { unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("") }
}

unsafe extern "system" fn get_device_proc(dev: usize, name: *const c_char) -> Pfn {
    let real = unsafe { GET_DEVICE_PROC.get().unwrap().call(dev, name) };
    std::panic::catch_unwind(|| ours(name_of(name), real, Some(dev))).ok().flatten().unwrap_or(real)
}

unsafe extern "system" fn get_instance_proc(inst: usize, name: *const c_char) -> Pfn {
    let real = unsafe { GET_INSTANCE_PROC.get().unwrap().call(inst, name) };
    std::panic::catch_unwind(|| ours(name_of(name), real, None)).ok().flatten().unwrap_or(real)
}

unsafe extern "system" fn create_surface(inst: usize, info: *const SurfaceInfo, alloc: usize, out: *mut u64) -> i32 {
    let real: CreateSurface = unsafe { std::mem::transmute(real_for(None, |r| r.create_surface)) };
    let r = unsafe { real(inst, info, alloc, out) };
    note_surface(r, info, out);
    r
}

unsafe extern "system" fn create_surface_export(inst: usize, info: *const SurfaceInfo, alloc: usize, out: *mut u64) -> i32 {
    let r = unsafe { X_CREATE_SURFACE.get().unwrap().call(inst, info, alloc, out) };
    note_surface(r, info, out);
    r
}

fn note_surface(r: i32, info: *const SurfaceInfo, out: *mut u64) {
    if r != 0 || info.is_null() || out.is_null() {
        return;
    }
    let (surface, hwnd) = unsafe { (*out, (*info).hwnd) };
    let mut s = crate::sys::lock(&SURFACES);
    s.retain(|e| e.0 != surface);
    if s.len() >= MAX_SURFACES {
        s.remove(0);
    }
    s.push((surface, hwnd));
}

fn surface_window(surface: u64) -> usize {
    crate::sys::lock(&SURFACES).iter().rev().find(|e| e.0 == surface).map_or(0, |e| e.1)
}

/// Whether a swap chain alive presents to `hwnd` (a translation layer's, when the game
/// draws with another API).
pub fn presents_to(hwnd: usize) -> bool {
    hwnd != 0 && crate::sys::lock(&WINDOWS).iter().any(|w| w.1 == hwnd)
}

/// Whether Vulkan presents at all: a swap chain alive on a known window, or a present in
/// the last second.
pub fn presents_anywhere() -> bool {
    WINDOWED.load(Ordering::Acquire) > 0 || presented_recently()
}

fn note_window(swap: u64, hwnd: usize) {
    if hwnd == 0 {
        return;
    }
    let mut w = crate::sys::lock(&WINDOWS);
    w.retain(|e| e.0 != swap);
    if !w.iter().any(|e| e.1 == hwnd) {
        crate::log::throttled("video: vk window", 4, |n| {
            log!("video: a Vulkan swap chain presents to window 0x{hwnd:x}: drawn into and copied with Vulkan; the API above it, if any, leaves its frames alone (window {n})")
        });
    }
    w.push((swap, hwnd));
    WINDOWED.store(w.len(), Ordering::Release);
}

fn forget_window(swaps: &[u64]) {
    let mut w = crate::sys::lock(&WINDOWS);
    w.retain(|e| !swaps.contains(&e.0));
    WINDOWED.store(w.len(), Ordering::Release);
}

unsafe extern "system" fn create_device(phys: usize, info: *const u8, alloc: usize, out: *mut usize) -> i32 {
    let real: CreateDevice = unsafe { std::mem::transmute(real_for(None, |r| r.create_device)) };
    creating_device(&|p, i, a, o| unsafe { real(p, i, a, o) }, phys, info, alloc, out)
}

unsafe extern "system" fn create_device_export(phys: usize, info: *const u8, alloc: usize, out: *mut usize) -> i32 {
    creating_device(&|p, i, a, o| unsafe { X_CREATE_DEVICE.get().unwrap().call(p, i, a, o) }, phys, info, alloc, out)
}

/// Makes the device, with what importing shared textures takes added for a split-screen
/// copy whose frames the session draws (as the game asked, if the driver refuses).
fn creating_device(real: &dyn Fn(usize, *const u8, usize, *mut usize) -> i32, phys: usize, info: *const u8, alloc: usize, out: *mut usize) -> i32 {
    let mut can_capture = false;
    let mut r = None;
    if super::capture::wanted() && !info.is_null() {
        let given = unsafe { *(info as *const vkcapture::DeviceCreateInfo) };
        let (more, possible) = vkcapture::extensions_for(phys, &given);
        if let Some(list) = more {
            let changed = vkcapture::DeviceCreateInfo { ext_count: list.len() as u32, exts: list.as_ptr(), ..given };
            let hr = real(phys, &changed as *const vkcapture::DeviceCreateInfo as *const u8, alloc, out);
            if hr == 0 {
                can_capture = true;
                r = Some(hr);
            } else {
                log!("video: the Vulkan device refused the extensions for shared textures ({hr}); made as the game asked");
            }
        } else {
            can_capture = possible;
        }
    }
    let r = r.unwrap_or_else(|| real(phys, info, alloc, out));
    if r == 0 && !out.is_null() {
        let d = unsafe { *out };
        // Without importing, its frames can still be read back (pixel mode).
        let capture = super::capture::wanted().then(|| vkcapture::VkCapture::new(vkcapture::luid(phys), can_capture));
        crate::sys::lock(&WORLD).devices.insert(d, Dev::new(phys, capture));
    }
    r
}

unsafe extern "system" fn get_queue(dev: usize, family: u32, index: u32, out: *mut usize) {
    let real: GetQueue = unsafe { std::mem::transmute(real_for(Some(dev), |r| r.get_queue)) };
    unsafe { real(dev, family, index, out) };
    note_queue(out, dev, family);
}

unsafe extern "system" fn get_queue_export(dev: usize, family: u32, index: u32, out: *mut usize) {
    unsafe { X_GET_QUEUE.get().unwrap().call(dev, family, index, out) };
    note_queue(out, dev, family);
}

fn note_queue(out: *mut usize, dev: usize, family: u32) {
    if !out.is_null() && unsafe { *out } != 0 {
        crate::sys::lock(&WORLD).queues.insert(unsafe { *out }, (dev, family));
    }
}

unsafe extern "system" fn get_queue2(dev: usize, info: *const QueueInfo2, out: *mut usize) {
    let real: GetQueue2 = unsafe { std::mem::transmute(real_for(Some(dev), |r| r.get_queue2)) };
    unsafe { real(dev, info, out) };
    if !info.is_null() {
        note_queue(out, dev, unsafe { (*info).family });
    }
}

unsafe extern "system" fn get_queue2_export(dev: usize, info: *const QueueInfo2, out: *mut usize) {
    unsafe { X_GET_QUEUE2.get().unwrap().call(dev, info, out) };
    if !info.is_null() {
        note_queue(out, dev, unsafe { (*info).family });
    }
}

unsafe extern "system" fn create_swapchain(dev: usize, info: *const SwapchainInfo, alloc: usize, out: *mut u64) -> i32 {
    let real: CreateSwapchain = unsafe { std::mem::transmute(real_for(Some(dev), |r| r.create_swapchain)) };
    creating_swapchain(&|d, i, a, o| unsafe { real(d, i, a, o) }, dev, info, alloc, out)
}

unsafe extern "system" fn create_swapchain_export(dev: usize, info: *const SwapchainInfo, alloc: usize, out: *mut u64) -> i32 {
    creating_swapchain(&|d, i, a, o| unsafe { X_CREATE_SWAPCHAIN.get().unwrap().call(d, i, a, o) }, dev, info, alloc, out)
}

/// The image usages `surface` supports for swap chains of `dev` (None when not known).
fn surface_usage(dev: usize, surface: u64) -> Option<u32> {
    let phys = crate::sys::lock(&WORLD).devices.get(&dev)?.phys;
    type Caps = unsafe extern "system" fn(usize, u64, *mut [u32; 13]) -> i32;
    let caps: Caps = unsafe { std::mem::transmute(com::proc(module(), c"vkGetPhysicalDeviceSurfaceCapabilitiesKHR")?) };
    let mut c = [0u32; 13];
    // VkSurfaceCapabilitiesKHR: supportedUsageFlags is its last field.
    (unsafe { caps(phys, surface, &mut c) } == 0).then_some(c[12])
}

/// Makes the swap chain with COLOR_ATTACHMENT (the overlay draws into its images) and,
/// for a copy whose frames the session draws, TRANSFER_SRC, as far as the surface
/// supports them. Tried once when the surface says what it supports, or when the game
/// hands an old swap chain over (a failed try retires it, and a retired one can't be
/// handed over again); else with less if the surface refuses, down to what the game
/// asked.
fn creating_swapchain(real: &dyn Fn(usize, *const SwapchainInfo, usize, *mut u64) -> i32, dev: usize, info: *const SwapchainInfo, alloc: usize, out: *mut u64) -> i32 {
    if info.is_null() {
        return real(dev, info, alloc, out);
    }
    let given = unsafe { *info };
    let wanted = if super::capture::wanted() { 0x10 | 0x1 } else { 0x10 }; // COLOR_ATTACHMENT, TRANSFER_SRC
    let tries = match std::panic::catch_unwind(|| surface_usage(dev, given.surface)).ok().flatten() {
        Some(supported) => vec![given.usage | wanted & supported],
        // Every surface supports COLOR_ATTACHMENT.
        None if given.old != 0 => vec![given.usage | 0x10],
        None if wanted != 0x10 => vec![given.usage | wanted, given.usage | 0x10, given.usage],
        None => vec![given.usage | 0x10, given.usage],
    };
    let mut r = 0;
    for usage in tries {
        let copy = SwapchainInfo { usage, ..given };
        r = real(dev, &copy, alloc, out);
        if r == 0 {
            if !out.is_null() {
                let _ = std::panic::catch_unwind(|| note_swapchain(dev, unsafe { *out }, &copy));
            }
            return 0;
        }
        if given.old != 0 {
            log!("video: the Vulkan swap chain couldn't be made with usage {usage:#x} ({r}); not tried again, its old one is retired");
            return r;
        }
    }
    r
}

fn note_swapchain(dev: usize, swap: u64, info: &SwapchainInfo) {
    let hwnd = surface_window(info.surface);
    super::report_viewport(info.width, info.height, hwnd, "vulkan");
    let s = Swap::new(dev, info);
    if !s.drawable || super::capture::wanted() && !s.capturable {
        log!(
            "video: a Vulkan swap chain made without {} (usage {:#x}): {}",
            if s.drawable { "TRANSFER_SRC" } else { "COLOR_ATTACHMENT" },
            info.usage,
            if s.drawable { "its frames aren't copied for the session" } else { "the overlay isn't drawn into it" }
        );
    }
    crate::sys::lock(&WORLD).swaps.insert(swap, s);
    note_window(swap, hwnd);
}

/// The device functions drawing uses.
struct Funcs {
    get_images: unsafe extern "system" fn(usize, u64, *mut u32, *mut u64) -> i32,
    create_pool: unsafe extern "system" fn(usize, *const Flagged, usize, *mut u64) -> i32,
    alloc_cmd: unsafe extern "system" fn(usize, *const AllocInfo, *mut usize) -> i32,
    begin: unsafe extern "system" fn(usize, *const BeginInfo) -> i32,
    end: unsafe extern "system" fn(usize) -> i32,
    reset_cmd: unsafe extern "system" fn(usize, u32) -> i32,
    barrier: unsafe extern "system" fn(usize, u32, u32, u32, u32, usize, u32, usize, u32, *const ImageBarrier),
    copy: unsafe extern "system" fn(usize, u64, u64, u32, u32, *const BufferImageCopy),
    submit: unsafe extern "system" fn(usize, u32, *const SubmitInfo, u64) -> i32,
    create_semaphore: unsafe extern "system" fn(usize, *const Flagged, usize, *mut u64) -> i32,
    create_fence: unsafe extern "system" fn(usize, *const Flagged, usize, *mut u64) -> i32,
    wait_fences: unsafe extern "system" fn(usize, u32, *const u64, u32, u64) -> i32,
    reset_fences: unsafe extern "system" fn(usize, u32, *const u64) -> i32,
    create_buffer: unsafe extern "system" fn(usize, *const BufferInfo, usize, *mut u64) -> i32,
    destroy_buffer: unsafe extern "system" fn(usize, u64, usize),
    buffer_reqs: unsafe extern "system" fn(usize, u64, *mut [u64; 3]),
    alloc_mem: unsafe extern "system" fn(usize, *const MemAlloc, usize, *mut u64) -> i32,
    free_mem: unsafe extern "system" fn(usize, u64, usize),
    bind: unsafe extern "system" fn(usize, u64, u64, u64) -> i32,
    map: unsafe extern "system" fn(usize, u64, u64, u64, u32, *mut *mut u8) -> i32,
    mem_props: unsafe extern "system" fn(usize, *mut u8),
    create_image: unsafe extern "system" fn(usize, *const ImageInfo, usize, *mut u64) -> i32,
    destroy_image: unsafe extern "system" fn(usize, u64, usize),
    image_reqs: unsafe extern "system" fn(usize, u64, *mut [u64; 3]),
    bind_image: unsafe extern "system" fn(usize, u64, u64, u64) -> i32,
    create_view: unsafe extern "system" fn(usize, *const ViewInfo, usize, *mut u64) -> i32,
    destroy_view: unsafe extern "system" fn(usize, u64, usize),
    create_sampler: unsafe extern "system" fn(usize, *const SamplerInfo, usize, *mut u64) -> i32,
    create_set_layout: unsafe extern "system" fn(usize, *const SetLayoutInfo, usize, *mut u64) -> i32,
    create_desc_pool: unsafe extern "system" fn(usize, *const DescPoolInfo, usize, *mut u64) -> i32,
    alloc_sets: unsafe extern "system" fn(usize, *const SetAllocInfo, *mut u64) -> i32,
    update_sets: unsafe extern "system" fn(usize, u32, *const WriteSet, u32, usize),
    create_pipeline_layout: unsafe extern "system" fn(usize, *const PipelineLayoutInfo, usize, *mut u64) -> i32,
    create_shader: unsafe extern "system" fn(usize, *const ShaderInfo, usize, *mut u64) -> i32,
    create_pipelines: unsafe extern "system" fn(usize, u64, u32, *const PipelineInfo, usize, *mut u64) -> i32,
    create_render_pass: unsafe extern "system" fn(usize, *const RenderPassInfo, usize, *mut u64) -> i32,
    create_framebuffer: unsafe extern "system" fn(usize, *const FramebufferInfo, usize, *mut u64) -> i32,
    destroy_framebuffer: unsafe extern "system" fn(usize, u64, usize),
    begin_pass: unsafe extern "system" fn(usize, *const PassBegin, u32),
    end_pass: unsafe extern "system" fn(usize),
    bind_pipeline: unsafe extern "system" fn(usize, u32, u64),
    bind_sets: unsafe extern "system" fn(usize, u32, u64, u32, u32, *const u64, u32, usize),
    bind_vertex: unsafe extern "system" fn(usize, u32, u32, *const u64, *const u64),
    push: unsafe extern "system" fn(usize, u64, u32, u32, u32, *const f32),
    set_viewport: unsafe extern "system" fn(usize, u32, u32, *const [f32; 6]),
    set_scissor: unsafe extern "system" fn(usize, u32, u32, *const [u32; 4]),
    draw: unsafe extern "system" fn(usize, u32, u32, u32, u32),
    device_wait_idle: unsafe extern "system" fn(usize) -> i32,
    destroy_pool: unsafe extern "system" fn(usize, u64, usize),
    destroy_fence: unsafe extern "system" fn(usize, u64, usize),
    destroy_semaphore: unsafe extern "system" fn(usize, u64, usize),
    destroy_sampler: unsafe extern "system" fn(usize, u64, usize),
    destroy_set_layout: unsafe extern "system" fn(usize, u64, usize),
    destroy_desc_pool: unsafe extern "system" fn(usize, u64, usize),
    destroy_pipeline_layout: unsafe extern "system" fn(usize, u64, usize),
    destroy_shader: unsafe extern "system" fn(usize, u64, usize),
    destroy_pipeline: unsafe extern "system" fn(usize, u64, usize),
    destroy_render_pass: unsafe extern "system" fn(usize, u64, usize),
}

/// sType, pNext, flags and one more u32: command pool (queue family), fence and
/// semaphore create infos.
#[repr(C)]
struct Flagged {
    s_type: u32,
    next: usize,
    flags: u32,
    extra: u32,
}

#[repr(C)]
struct BeginInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    inheritance: usize,
}

#[repr(C)]
struct AllocInfo {
    s_type: u32,
    next: usize,
    pool: u64,
    level: u32,
    count: u32,
}

#[repr(C)]
struct ImageBarrier {
    s_type: u32,
    next: usize,
    src_access: u32,
    dst_access: u32,
    old_layout: u32,
    new_layout: u32,
    src_family: u32,
    dst_family: u32,
    image: u64,
    aspect: u32,
    base_mip: u32,
    mips: u32,
    base_layer: u32,
    layers: u32,
}

#[repr(C)]
struct BufferImageCopy {
    offset: u64,
    row_length: u32,
    image_height: u32,
    aspect: u32,
    mip: u32,
    base_layer: u32,
    layers: u32,
    x: i32,
    y: i32,
    z: i32,
    w: u32,
    h: u32,
    d: u32,
}

#[repr(C)]
struct SubmitInfo {
    s_type: u32,
    next: usize,
    wait_count: u32,
    waits: *const u64,
    stages: *const u32,
    cmd_count: u32,
    cmds: *const usize,
    signal_count: u32,
    signals: *const u64,
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
struct ViewInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    image: u64,
    view_type: u32,
    format: u32,
    components: [u32; 4],
    /// aspect, base mip, mip count, base layer, layer count.
    range: [u32; 5],
}

#[repr(C)]
struct SamplerInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    mag: u32,
    min: u32,
    mipmap: u32,
    address: [u32; 3],
    lod_bias: f32,
    anisotropy: u32,
    max_anisotropy: f32,
    compare: u32,
    compare_op: u32,
    min_lod: f32,
    max_lod: f32,
    border: u32,
    unnormalized: u32,
}

#[repr(C)]
struct SetLayoutBinding {
    binding: u32,
    kind: u32,
    count: u32,
    stages: u32,
    immutable: usize,
}

#[repr(C)]
struct SetLayoutInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    count: u32,
    bindings: *const SetLayoutBinding,
}

#[repr(C)]
struct DescPoolInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    max_sets: u32,
    size_count: u32,
    /// (type, count) pairs.
    sizes: *const [u32; 2],
}

#[repr(C)]
struct SetAllocInfo {
    s_type: u32,
    next: usize,
    pool: u64,
    count: u32,
    layouts: *const u64,
}

#[repr(C)]
struct ImageDesc {
    sampler: u64,
    view: u64,
    layout: u32,
}

#[repr(C)]
struct WriteSet {
    s_type: u32,
    next: usize,
    set: u64,
    binding: u32,
    element: u32,
    count: u32,
    kind: u32,
    images: *const ImageDesc,
    buffers: usize,
    texel_views: usize,
}

#[repr(C)]
struct PipelineLayoutInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    set_count: u32,
    sets: *const u64,
    push_count: u32,
    /// (stages, offset, size).
    pushes: *const [u32; 3],
}

#[repr(C)]
struct ShaderInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    size: usize,
    code: *const u32,
}

#[repr(C)]
struct StageInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    stage: u32,
    module: u64,
    name: *const c_char,
    specialization: usize,
}

#[repr(C)]
struct VertexInput {
    s_type: u32,
    next: usize,
    flags: u32,
    binding_count: u32,
    /// (binding, stride, input rate).
    bindings: *const [u32; 3],
    attribute_count: u32,
    /// (location, binding, format, offset).
    attributes: *const [u32; 4],
}

/// sType, pNext, then u32s: input assembly (flags, topology, restart).
#[repr(C)]
struct InputAssembly {
    s_type: u32,
    next: usize,
    flags: u32,
    topology: u32,
    restart: u32,
}

#[repr(C)]
struct ViewportState {
    s_type: u32,
    next: usize,
    flags: u32,
    viewport_count: u32,
    viewports: usize,
    scissor_count: u32,
    scissors: usize,
}

#[repr(C)]
struct Raster {
    s_type: u32,
    next: usize,
    flags: u32,
    depth_clamp: u32,
    discard: u32,
    polygon: u32,
    cull: u32,
    front: u32,
    depth_bias: u32,
    bias: [f32; 3],
    line_width: f32,
}

#[repr(C)]
struct Multisample {
    s_type: u32,
    next: usize,
    flags: u32,
    samples: u32,
    sample_shading: u32,
    min_sample_shading: f32,
    mask: usize,
    alpha_to_coverage: u32,
    alpha_to_one: u32,
}

#[repr(C)]
struct BlendState {
    s_type: u32,
    next: usize,
    flags: u32,
    logic_enable: u32,
    logic_op: u32,
    count: u32,
    /// enable, src colour, dst colour, op, src alpha, dst alpha, op, write mask.
    attachments: *const [u32; 8],
    constants: [f32; 4],
}

#[repr(C)]
struct DynamicState {
    s_type: u32,
    next: usize,
    flags: u32,
    count: u32,
    states: *const u32,
}

#[repr(C)]
struct PipelineInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    stage_count: u32,
    stages: *const StageInfo,
    vertex: *const VertexInput,
    assembly: *const InputAssembly,
    tessellation: usize,
    viewport: *const ViewportState,
    raster: *const Raster,
    multisample: *const Multisample,
    depth: usize,
    blend: *const BlendState,
    dynamic: *const DynamicState,
    layout: u64,
    pass: u64,
    subpass: u32,
    base: u64,
    base_index: i32,
}

#[repr(C)]
struct SubpassInfo {
    flags: u32,
    bind_point: u32,
    input_count: u32,
    inputs: usize,
    color_count: u32,
    /// (attachment, layout).
    colors: *const [u32; 2],
    resolves: usize,
    depth: usize,
    preserve_count: u32,
    preserves: usize,
}

#[repr(C)]
struct RenderPassInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    attachment_count: u32,
    /// flags, format, samples, load, store, stencil load, stencil store, initial, final.
    attachments: *const [u32; 9],
    subpass_count: u32,
    subpasses: *const SubpassInfo,
    dependency_count: u32,
    /// src, dst, src stages, dst stages, src access, dst access, flags.
    dependencies: *const [u32; 7],
}

#[repr(C)]
struct FramebufferInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    pass: u64,
    count: u32,
    attachments: *const u64,
    width: u32,
    height: u32,
    layers: u32,
}

#[repr(C)]
struct PassBegin {
    s_type: u32,
    next: usize,
    pass: u64,
    framebuffer: u64,
    /// x, y (as i32), width, height.
    area: [u32; 4],
    clear_count: u32,
    clears: usize,
}

fn funcs(dev: usize) -> Option<Funcs> {
    let gdpa = GET_DEVICE_PROC.get()?;
    funcs_with(|n| unsafe { gdpa.call(dev, n.as_ptr()) }, module())

}

fn funcs_with(get: impl Fn(&CStr) -> Pfn, m: windows_sys::Win32::Foundation::HMODULE) -> Option<Funcs> {
    macro_rules! f {
        ($n:literal) => {{
            let p = get($n);
            if p == 0 {
                return None;
            }
            unsafe { std::mem::transmute(p) }
        }};
    }
    Some(Funcs {
        get_images: f!(c"vkGetSwapchainImagesKHR"),
        create_pool: f!(c"vkCreateCommandPool"),
        alloc_cmd: f!(c"vkAllocateCommandBuffers"),
        begin: f!(c"vkBeginCommandBuffer"),
        end: f!(c"vkEndCommandBuffer"),
        reset_cmd: f!(c"vkResetCommandBuffer"),
        barrier: f!(c"vkCmdPipelineBarrier"),
        copy: f!(c"vkCmdCopyBufferToImage"),
        submit: f!(c"vkQueueSubmit"),
        create_semaphore: f!(c"vkCreateSemaphore"),
        create_fence: f!(c"vkCreateFence"),
        wait_fences: f!(c"vkWaitForFences"),
        reset_fences: f!(c"vkResetFences"),
        create_buffer: f!(c"vkCreateBuffer"),
        destroy_buffer: f!(c"vkDestroyBuffer"),
        buffer_reqs: f!(c"vkGetBufferMemoryRequirements"),
        alloc_mem: f!(c"vkAllocateMemory"),
        free_mem: f!(c"vkFreeMemory"),
        bind: f!(c"vkBindBufferMemory"),
        map: f!(c"vkMapMemory"),
        mem_props: unsafe { std::mem::transmute(com::proc(m, c"vkGetPhysicalDeviceMemoryProperties")?) },
        create_image: f!(c"vkCreateImage"),
        destroy_image: f!(c"vkDestroyImage"),
        image_reqs: f!(c"vkGetImageMemoryRequirements"),
        bind_image: f!(c"vkBindImageMemory"),
        create_view: f!(c"vkCreateImageView"),
        destroy_view: f!(c"vkDestroyImageView"),
        create_sampler: f!(c"vkCreateSampler"),
        create_set_layout: f!(c"vkCreateDescriptorSetLayout"),
        create_desc_pool: f!(c"vkCreateDescriptorPool"),
        alloc_sets: f!(c"vkAllocateDescriptorSets"),
        update_sets: f!(c"vkUpdateDescriptorSets"),
        create_pipeline_layout: f!(c"vkCreatePipelineLayout"),
        create_shader: f!(c"vkCreateShaderModule"),
        create_pipelines: f!(c"vkCreateGraphicsPipelines"),
        create_render_pass: f!(c"vkCreateRenderPass"),
        create_framebuffer: f!(c"vkCreateFramebuffer"),
        destroy_framebuffer: f!(c"vkDestroyFramebuffer"),
        begin_pass: f!(c"vkCmdBeginRenderPass"),
        end_pass: f!(c"vkCmdEndRenderPass"),
        bind_pipeline: f!(c"vkCmdBindPipeline"),
        bind_sets: f!(c"vkCmdBindDescriptorSets"),
        bind_vertex: f!(c"vkCmdBindVertexBuffers"),
        push: f!(c"vkCmdPushConstants"),
        set_viewport: f!(c"vkCmdSetViewport"),
        set_scissor: f!(c"vkCmdSetScissor"),
        draw: f!(c"vkCmdDraw"),
        device_wait_idle: f!(c"vkDeviceWaitIdle"),
        destroy_pool: f!(c"vkDestroyCommandPool"),
        destroy_fence: f!(c"vkDestroyFence"),
        destroy_semaphore: f!(c"vkDestroySemaphore"),
        destroy_sampler: f!(c"vkDestroySampler"),
        destroy_set_layout: f!(c"vkDestroyDescriptorSetLayout"),
        destroy_desc_pool: f!(c"vkDestroyDescriptorPool"),
        destroy_pipeline_layout: f!(c"vkDestroyPipelineLayout"),
        destroy_shader: f!(c"vkDestroyShaderModule"),
        destroy_pipeline: f!(c"vkDestroyPipeline"),
        destroy_render_pass: f!(c"vkDestroyRenderPass"),
    })
}

const PRESENT_SRC: u32 = 1000001002;
const TRANSFER_DST: u32 = 7;
const SHADER_READ: u32 = 5;
const RING: usize = 3;
/// The frame's own format: premultiplied BGRA.
const BGRA_UNORM: u32 = 44;
const BGRA_SRGB: u32 = 50;

/// Whether a swap chain format is sRGB-encoded (the texture is then read as sRGB too, so
/// the overlay's colours come out as they were drawn).
fn srgb(format: u32) -> bool {
    matches!(format, 29 | 36 | 43 | 50 | 57)
}

/// How a swap chain's images take the frame's (sRGB-encoded) pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Encoding {
    /// As they are (or through the texture's sRGB view, for an sRGB format).
    Plain,
    /// R16G16B16A16_SFLOAT: scRGB, linear light where 1.0 is 80 nits.
    ScRgb,
    /// A2B10G10R10 / A2R10G10B10 in HDR10_ST2084: PQ-encoded BT.2020.
    Pq,
}

/// A swap chain's `Encoding`, from its `VkFormat` and `VkColorSpaceKHR` (a float one is
/// taken as scRGB whatever its colour space says, as on DXGI).
fn encoding(format: u32, color_space: u32) -> Encoding {
    match (format, color_space) {
        (97, _) => Encoding::ScRgb,
        (58 | 64, 1000104008) => Encoding::Pq,
        _ => Encoding::Plain,
    }
}

/// The overlay's texture and pipeline objects on one device.
struct Gfx {
    phys: usize,
    sampler: u64,
    set_layout: u64,
    desc_pool: u64,
    set: u64,
    layout: u64,
    vert: u64,
    frag: u64,
    /// PeerParty's own, for HDR swap chains (`Encoding`).
    hdr_vert: u64,
    scrgb_frag: u64,
    pq_frag: u64,
    /// (swap chain format, layout the image is in, encoding) -> (render pass, pipeline).
    passes: HashMap<(u32, u32, Encoding), (u64, u64)>,
    /// The texture: image, memory, size, its UNORM and sRGB views, whether it holds a
    /// frame (so is in SHADER_READ_ONLY), and which view the set points at.
    tex: Option<(u64, u64, u32, u32, [u64; 2], bool, Option<usize>)>,
    /// The quad's vertices: buffer, memory, mapped pointer, the frame size they're for.
    quad: Option<(u64, u64, *mut u8, (u32, u32))>,
    /// Host-visible staging buffer: buffer, memory, mapped pointer, size.
    staging: Option<(u64, u64, *mut u8, usize)>,
    /// The frame in staging, and the one in the texture.
    staged: Option<u32>,
    uploaded: Option<u32>,
}

unsafe impl Send for Gfx {}

fn memory_type(fx: &Funcs, phys: usize, bits: u64, wanted: u32) -> Option<u32> {
    let mut props = vec![0u8; 520];
    unsafe { (fx.mem_props)(phys, props.as_mut_ptr()) };
    let count = u32::from_le_bytes(props[0..4].try_into().ok()?);
    (0..count.min(32)).find(|i| {
        let flags = u32::from_le_bytes(props[4 + *i as usize * 8..8 + *i as usize * 8].try_into().unwrap());
        bits & (1 << i) != 0 && flags & wanted == wanted
    })
}

/// Whether a `vkCreate*` / `vkAllocate*` call (result `r`) made its object; when not, its
/// output is set to no object (a failed call leaves it undefined), so cleanup can't free
/// something that isn't ours.
fn made(r: i32, out: &mut u64) -> bool {
    if r != 0 {
        *out = 0;
    }
    r == 0
}

/// A host-visible, coherent, mapped buffer; nothing left behind when it can't be made.
fn host_buffer(fx: &Funcs, dev: usize, phys: usize, size: usize, usage: u32) -> Option<(u64, u64, *mut u8)> {
    let bi = BufferInfo { s_type: 12, next: 0, flags: 0, size: size as u64, usage, sharing: 0, family_count: 0, families: 0 };
    let mut buf = 0u64;
    if !made(unsafe { (fx.create_buffer)(dev, &bi, 0, &mut buf) }, &mut buf) {
        return None;
    }
    let mut reqs = [0u64; 3]; // size, alignment, memoryTypeBits
    unsafe { (fx.buffer_reqs)(dev, buf, &mut reqs) };
    let mut mem = 0u64;
    let mut ptr = std::ptr::null_mut();
    // HOST_VISIBLE | HOST_COHERENT
    let ok = memory_type(fx, phys, reqs[2], 0x2 | 0x4).is_some_and(|type_index| {
        let ma = MemAlloc { s_type: 5, next: 0, size: reqs[0], type_index };
        made(unsafe { (fx.alloc_mem)(dev, &ma, 0, &mut mem) }, &mut mem)
            && unsafe { (fx.bind)(dev, buf, mem, 0) } == 0
            && unsafe { (fx.map)(dev, mem, 0, u64::MAX, 0, &mut ptr) } == 0
    });
    if !ok {
        // What was made goes (freeing memory unmaps it; 0 is no object).
        unsafe {
            (fx.destroy_buffer)(dev, buf, 0);
            (fx.free_mem)(dev, mem, 0);
        }
        return None;
    }
    Some((buf, mem, ptr))
}

impl Gfx {
    /// The objects that don't depend on the swap chain or the frame; none left behind
    /// when one can't be made.
    fn new(fx: &Funcs, dev: usize, phys: usize) -> Option<Gfx> {
        let mut g = Gfx {
            phys,
            sampler: 0,
            set_layout: 0,
            desc_pool: 0,
            set: 0,
            layout: 0,
            vert: 0,
            frag: 0,
            hdr_vert: 0,
            scrgb_frag: 0,
            pq_frag: 0,
            passes: HashMap::new(),
            tex: None,
            quad: None,
            staging: None,
            staged: None,
            uploaded: None,
        };
        if g.make(fx, dev) {
            Some(g)
        } else {
            log!("video: the overlay's Vulkan objects couldn't be made; it isn't drawn into this device's frames");
            g.destroy(fx, dev);
            None
        }
    }

    /// Everything it made on `dev` (idle, or done with all of it).
    fn destroy(self, fx: &Funcs, dev: usize) {
        unsafe {
            for (_, (pass, pipeline)) in self.passes {
                (fx.destroy_pipeline)(dev, pipeline, 0);
                (fx.destroy_render_pass)(dev, pass, 0);
            }
            if let Some((image, mem, _, _, views, _, _)) = self.tex {
                for v in views {
                    (fx.destroy_view)(dev, v, 0);
                }
                (fx.destroy_image)(dev, image, 0);
                (fx.free_mem)(dev, mem, 0);
            }
            for (buf, mem) in [self.quad.map(|q| (q.0, q.1)), self.staging.map(|s| (s.0, s.1))].into_iter().flatten() {
                (fx.destroy_buffer)(dev, buf, 0);
                (fx.free_mem)(dev, mem, 0);
            }
            for m in [self.vert, self.frag, self.hdr_vert, self.scrgb_frag, self.pq_frag] {
                (fx.destroy_shader)(dev, m, 0);
            }
            (fx.destroy_pipeline_layout)(dev, self.layout, 0);
            // Its set goes with it.
            (fx.destroy_desc_pool)(dev, self.desc_pool, 0);
            (fx.destroy_set_layout)(dev, self.set_layout, 0);
            (fx.destroy_sampler)(dev, self.sampler, 0);
        }
    }

    fn make(&mut self, fx: &Funcs, dev: usize) -> bool {
        let si = SamplerInfo {
            s_type: 31,
            next: 0,
            flags: 0,
            mag: 0, // NEAREST: the frame maps 1:1 onto the game's pixels
            min: 0,
            mipmap: 0,
            address: [2; 3], // CLAMP_TO_EDGE
            lod_bias: 0.0,
            anisotropy: 0,
            max_anisotropy: 1.0,
            compare: 0,
            compare_op: 0,
            min_lod: 0.0,
            max_lod: 0.0,
            border: 0,
            unnormalized: 0,
        };
        if !made(unsafe { (fx.create_sampler)(dev, &si, 0, &mut self.sampler) }, &mut self.sampler) {
            return false;
        }
        // For the fragment shaders: a combined image sampler (ImGui's), and the same texture
        // and sampler apart (PeerParty's HDR ones).
        let bindings = [
            SetLayoutBinding { binding: 0, kind: 1, count: 1, stages: 0x10, immutable: 0 },
            SetLayoutBinding { binding: 1, kind: 2, count: 1, stages: 0x10, immutable: 0 },
            SetLayoutBinding { binding: 2, kind: 0, count: 1, stages: 0x10, immutable: 0 },
        ];
        let sli = SetLayoutInfo { s_type: 32, next: 0, flags: 0, count: bindings.len() as u32, bindings: bindings.as_ptr() };
        if !made(unsafe { (fx.create_set_layout)(dev, &sli, 0, &mut self.set_layout) }, &mut self.set_layout) {
            return false;
        }
        // (type, count): COMBINED_IMAGE_SAMPLER, SAMPLED_IMAGE, SAMPLER.
        let sizes = [[1u32, 1], [2, 1], [0, 1]];
        let dpi = DescPoolInfo { s_type: 33, next: 0, flags: 0, max_sets: 1, size_count: sizes.len() as u32, sizes: sizes.as_ptr() };
        if !made(unsafe { (fx.create_desc_pool)(dev, &dpi, 0, &mut self.desc_pool) }, &mut self.desc_pool) {
            return false;
        }
        let sai = SetAllocInfo { s_type: 34, next: 0, pool: self.desc_pool, count: 1, layouts: &self.set_layout };
        if !made(unsafe { (fx.alloc_sets)(dev, &sai, &mut self.set) }, &mut self.set) {
            return false;
        }
        // uScale, uTranslate for the vertex shader.
        let push = [0x1u32, 0, 16];
        let pli = PipelineLayoutInfo { s_type: 30, next: 0, flags: 0, set_count: 1, sets: &self.set_layout, push_count: 1, pushes: &push };
        if !made(unsafe { (fx.create_pipeline_layout)(dev, &pli, 0, &mut self.layout) }, &mut self.layout) {
            return false;
        }
        for (code, out) in [
            (&vkshaders::VERT[..], &mut self.vert),
            (&vkshaders::FRAG[..], &mut self.frag),
            (&vkshaders::HDR_VERT[..], &mut self.hdr_vert),
            (&vkshaders::SCRGB_FRAG[..], &mut self.scrgb_frag),
            (&vkshaders::PQ_FRAG[..], &mut self.pq_frag),
        ] {
            let info = ShaderInfo { s_type: 16, next: 0, flags: 0, size: code.len() * 4, code: code.as_ptr() };
            if !made(unsafe { (fx.create_shader)(dev, &info, 0, &mut *out) }, out) {
                return false;
            }
        }
        true
    }

    /// A render pass that keeps what's in the image (in `layout` before and after) and
    /// its pipeline, per format and encoding.
    fn pass(&mut self, fx: &Funcs, dev: usize, format: u32, layout: u32, enc: Encoding) -> Option<(u64, u64)> {
        if let Some(p) = self.passes.get(&(format, layout, enc)) {
            return Some(*p);
        }
        let (vert, frag) = match enc {
            Encoding::Plain => (self.vert, self.frag),
            Encoding::ScRgb => (self.hdr_vert, self.scrgb_frag),
            Encoding::Pq => (self.hdr_vert, self.pq_frag),
        };
        // LOAD, STORE; stencil DONT_CARE.
        let attachment = [0u32, format, 1, 0, 0, 2, 1, layout, layout];
        let color = [0u32, 2]; // COLOR_ATTACHMENT_OPTIMAL
        let subpass = SubpassInfo {
            flags: 0,
            bind_point: 0,
            input_count: 0,
            inputs: 0,
            color_count: 1,
            colors: &color,
            resolves: 0,
            depth: 0,
            preserve_count: 0,
            preserves: 0,
        };
        // After whatever the game (and the semaphores it handed to present) did; drawing
        // reads and writes the colour attachment.
        let dependency = [u32::MAX, 0, 0x400 | 0x1000, 0x400, 0x100 | 0x1000, 0x80 | 0x100, 0];
        let rpi = RenderPassInfo {
            s_type: 38,
            next: 0,
            flags: 0,
            attachment_count: 1,
            attachments: &attachment,
            subpass_count: 1,
            subpasses: &subpass,
            dependency_count: 1,
            dependencies: &dependency,
        };
        let mut pass = 0u64;
        if !made(unsafe { (fx.create_render_pass)(dev, &rpi, 0, &mut pass) }, &mut pass) {
            return None;
        }
        let main = c"main";
        let stages = [
            StageInfo { s_type: 18, next: 0, flags: 0, stage: 0x1, module: vert, name: main.as_ptr(), specialization: 0 },
            StageInfo { s_type: 18, next: 0, flags: 0, stage: 0x10, module: frag, name: main.as_ptr(), specialization: 0 },
        ];
        // Position and UV (2 floats each), colour (4 bytes): 20 bytes a vertex.
        let binding = [0u32, 20, 0];
        let attributes = [[0u32, 0, 103, 0], [1, 0, 103, 8], [2, 0, 37, 16]];
        let vertex = VertexInput { s_type: 19, next: 0, flags: 0, binding_count: 1, bindings: &binding, attribute_count: 3, attributes: attributes.as_ptr() };
        let assembly = InputAssembly { s_type: 20, next: 0, flags: 0, topology: 3, restart: 0 }; // TRIANGLE_LIST
        let viewport = ViewportState { s_type: 22, next: 0, flags: 0, viewport_count: 1, viewports: 0, scissor_count: 1, scissors: 0 };
        let raster = Raster {
            s_type: 23,
            next: 0,
            flags: 0,
            depth_clamp: 0,
            discard: 0,
            polygon: 0,
            cull: 0,
            front: 0,
            depth_bias: 0,
            bias: [0.0; 3],
            line_width: 1.0,
        };
        let multisample = Multisample {
            s_type: 24,
            next: 0,
            flags: 0,
            samples: 1,
            sample_shading: 0,
            min_sample_shading: 0.0,
            mask: 0,
            alpha_to_coverage: 0,
            alpha_to_one: 0,
        };
        // Premultiplied "over": ONE, ONE_MINUS_SRC_ALPHA, ADD, for colour and alpha.
        let blend_attachment = [1u32, 1, 7, 0, 1, 7, 0, 0xF];
        let blend = BlendState { s_type: 26, next: 0, flags: 0, logic_enable: 0, logic_op: 0, count: 1, attachments: &blend_attachment, constants: [0.0; 4] };
        let dynamic_states = [0u32, 1]; // VIEWPORT, SCISSOR
        let dynamic = DynamicState { s_type: 27, next: 0, flags: 0, count: 2, states: dynamic_states.as_ptr() };
        let pi = PipelineInfo {
            s_type: 28,
            next: 0,
            flags: 0,
            stage_count: 2,
            stages: stages.as_ptr(),
            vertex: &vertex,
            assembly: &assembly,
            tessellation: 0,
            viewport: &viewport,
            raster: &raster,
            multisample: &multisample,
            depth: 0,
            blend: &blend,
            dynamic: &dynamic,
            layout: self.layout,
            pass,
            subpass: 0,
            base: 0,
            base_index: -1,
        };
        let mut pipeline = 0u64;
        if unsafe { (fx.create_pipelines)(dev, 0, 1, &pi, 0, &mut pipeline) } != 0 {
            unsafe { (fx.destroy_render_pass)(dev, pass, 0) };
            return None;
        }
        if enc != Encoding::Plain {
            log!("video: the overlay is drawn into an HDR Vulkan swap chain (format {format}, {enc:?}): at SDR white, encoded for it");
        }
        self.passes.insert((format, layout, enc), (pass, pipeline));
        Some((pass, pipeline))
    }

    /// The texture (frame-sized, BGRA, viewable as UNORM or sRGB) and the quad's vertices.
    /// `wait` is called before anything the GPU may still be reading is replaced.
    fn ensure_frame_objects(&mut self, fx: &Funcs, dev: usize, w: u32, h: u32, wait: &dyn Fn()) -> bool {
        if self.tex.is_none_or(|t| (t.2, t.3) != (w, h)) {
            wait();
            if let Some((image, mem, _, _, views, _, _)) = self.tex.take() {
                unsafe {
                    for v in views {
                        (fx.destroy_view)(dev, v, 0);
                    }
                    (fx.destroy_image)(dev, image, 0);
                    (fx.free_mem)(dev, mem, 0);
                }
            }
            let ii = ImageInfo {
                s_type: 14,
                next: 0,
                flags: 0x8, // MUTABLE_FORMAT: viewed as UNORM or sRGB
                image_type: 1,
                format: BGRA_UNORM,
                extent: [w, h, 1],
                mips: 1,
                layers: 1,
                samples: 1,
                tiling: 0,
                usage: 0x2 | 0x4, // TRANSFER_DST | SAMPLED
                sharing: 0,
                family_count: 0,
                families: 0,
                initial_layout: 0,
            };
            let mut image = 0u64;
            if !made(unsafe { (fx.create_image)(dev, &ii, 0, &mut image) }, &mut image) {
                return false;
            }
            let mut reqs = [0u64; 3];
            unsafe { (fx.image_reqs)(dev, image, &mut reqs) };
            let mut mem = 0u64;
            let mut views = [0u64; 2];
            let ok = memory_type(fx, self.phys, reqs[2], 0x1).or_else(|| memory_type(fx, self.phys, reqs[2], 0)).is_some_and(|type_index| {
                let ma = MemAlloc { s_type: 5, next: 0, size: reqs[0], type_index };
                made(unsafe { (fx.alloc_mem)(dev, &ma, 0, &mut mem) }, &mut mem) && unsafe { (fx.bind_image)(dev, image, mem, 0) } == 0
            }) && [BGRA_UNORM, BGRA_SRGB].into_iter().zip(views.iter_mut()).all(|(format, view)| {
                let vi = ViewInfo { s_type: 15, next: 0, flags: 0, image, view_type: 1, format, components: [0; 4], range: [1, 0, 1, 0, 1] };
                made(unsafe { (fx.create_view)(dev, &vi, 0, &mut *view) }, view)
            });
            if !ok {
                unsafe {
                    for v in views {
                        (fx.destroy_view)(dev, v, 0);
                    }
                    (fx.destroy_image)(dev, image, 0);
                    (fx.free_mem)(dev, mem, 0);
                }
                return false;
            }
            self.tex = Some((image, mem, w, h, views, false, None));
            self.uploaded = None;
        }
        if self.quad.is_none_or(|q| q.3 != (w, h)) {
            wait();
            if self.quad.is_none() {
                let Some((buf, mem, ptr)) = host_buffer(fx, dev, self.phys, 6 * 20, 0x80) else { return false }; // VERTEX_BUFFER
                self.quad = Some((buf, mem, ptr, (0, 0)));
            }
            let q = self.quad.as_mut().unwrap();
            let (fw, fh) = (w as f32, h as f32);
            let corners = [(0.0, 0.0), (fw, 0.0), (0.0, fh), (fw, 0.0), (fw, fh), (0.0, fh)];
            for (i, (x, y)) in corners.into_iter().enumerate() {
                let v = [x, y, x / fw, y / fh];
                unsafe {
                    std::ptr::copy_nonoverlapping(v.as_ptr() as *const u8, q.2.add(i * 20), 16);
                    std::ptr::write_unaligned(q.2.add(i * 20 + 16) as *mut u32, u32::MAX); // white, opaque
                }
            }
            q.3 = (w, h);
        }
        true
    }

    /// Points the descriptor set at the texture's view for this swap chain's encoding
    /// (the combined sampler; the HDR shaders' texture is always the UNORM view, which
    /// they decode themselves).
    fn bind_view(&mut self, fx: &Funcs, dev: usize, srgb: bool, wait: &dyn Fn()) {
        let Some(t) = self.tex.as_mut() else { return };
        let which = usize::from(srgb);
        if t.6 == Some(which) {
            return;
        }
        if t.6.is_some() {
            wait();
        }
        let combined = ImageDesc { sampler: self.sampler, view: t.4[which], layout: SHADER_READ };
        let texture = ImageDesc { sampler: 0, view: t.4[0], layout: SHADER_READ };
        let sampler = ImageDesc { sampler: self.sampler, view: 0, layout: 0 };
        let write = |binding, kind, image: &ImageDesc| WriteSet { s_type: 35, next: 0, set: self.set, binding, element: 0, count: 1, kind, images: image, buffers: 0, texel_views: 0 };
        let writes = [write(0, 1, &combined), write(1, 2, &texture), write(2, 0, &sampler)];
        unsafe { (fx.update_sets)(dev, writes.len() as u32, writes.as_ptr(), 0, 0) };
        t.6 = Some(which);
    }

    /// Records into `cmd`: the texture upload from staging (when the frame changed), then
    /// the quad drawn over `framebuffer` (sw x sh) in `pass`.
    fn record(&mut self, fx: &Funcs, cmd: usize, pass: (u64, u64), framebuffer: u64, sw: u32, sh: u32) {
        let Some(t) = self.tex.as_mut() else { return };
        let (Some(staging), Some(quad)) = (self.staging, self.quad) else { return };
        let tex_image = t.0;
        let barrier = |old, new, src, dst| ImageBarrier {
            s_type: 45,
            next: 0,
            src_access: src,
            dst_access: dst,
            old_layout: old,
            new_layout: new,
            src_family: u32::MAX,
            dst_family: u32::MAX,
            image: tex_image,
            aspect: 1,
            base_mip: 0,
            mips: 1,
            base_layer: 0,
            layers: 1,
        };
        if self.staged.is_some() && self.uploaded != self.staged {
            let (old, src_stage, src_access) = if t.5 { (SHADER_READ, 0x80, 0x20) } else { (0, 0x1, 0) };
            let to_dst = barrier(old, TRANSFER_DST, src_access, 0x1000);
            let to_read = barrier(TRANSFER_DST, SHADER_READ, 0x1000, 0x20);
            let region = BufferImageCopy {
                offset: 0,
                row_length: t.2,
                image_height: t.3,
                aspect: 1,
                mip: 0,
                base_layer: 0,
                layers: 1,
                x: 0,
                y: 0,
                z: 0,
                w: t.2,
                h: t.3,
                d: 1,
            };
            unsafe {
                (fx.barrier)(cmd, src_stage, 0x1000, 0, 0, 0, 0, 0, 1, &to_dst);
                (fx.copy)(cmd, staging.0, t.0, TRANSFER_DST, 1, &region);
                (fx.barrier)(cmd, 0x1000, 0x80, 0, 0, 0, 0, 0, 1, &to_read);
            }
            t.5 = true;
            self.uploaded = self.staged;
        }
        if !t.5 {
            return;
        }
        let begin = PassBegin { s_type: 43, next: 0, pass: pass.0, framebuffer, area: [0, 0, sw, sh], clear_count: 0, clears: 0 };
        let viewport = [0.0, 0.0, sw as f32, sh as f32, 0.0, 1.0];
        let scissor = [0, 0, sw, sh];
        // The quad's pixels to clip space, top-left at (-1, -1): the frame fills the
        // swapchain image whatever their sizes, as on the other back ends.
        let (qw, qh) = self.quad.map_or((sw, sh), |q| q.3);
        let push = [2.0 / qw.max(1) as f32, 2.0 / qh.max(1) as f32, -1.0, -1.0];
        let offset = 0u64;
        unsafe {
            (fx.begin_pass)(cmd, &begin, 0);
            (fx.bind_pipeline)(cmd, 0, pass.1);
            (fx.set_viewport)(cmd, 0, 1, &viewport);
            (fx.set_scissor)(cmd, 0, 1, &scissor);
            (fx.bind_sets)(cmd, 0, self.layout, 0, 1, &self.set, 0, 0);
            (fx.bind_vertex)(cmd, 0, 1, &quad.0, &offset);
            (fx.push)(cmd, self.layout, 0x1, 0, 16, push.as_ptr());
            (fx.draw)(cmd, 6, 1, 0, 0);
            (fx.end_pass)(cmd);
        }
    }

    /// Copies the shared frame into staging when it's newer than what's there.
    fn stage(&mut self, fx: &Funcs, dev: usize, f: &super::Frame, wait: &dyn Fn()) -> bool {
        let need = (f.w * f.h * 4) as usize;
        if self.staging.is_none_or(|s| s.3 < need) {
            wait();
            let Some((buf, mem, ptr)) = host_buffer(fx, dev, self.phys, need, 0x1) else { return false }; // TRANSFER_SRC
            // The smaller one goes (the wait above let the GPU finish with it), not left
            // behind on every growth.
            if let Some((old_buf, old_mem, _, _)) = self.staging.replace((buf, mem, ptr, need)) {
                unsafe {
                    (fx.destroy_buffer)(dev, old_buf, 0);
                    (fx.free_mem)(dev, old_mem, 0);
                }
            }
            self.staged = None;
        }
        if self.staged != Some(f.seq) && !f.writing() {
            wait();
            if unsafe { f.copy_to(self.staging.unwrap().2, f.w as usize * 4) } {
                self.staged = Some(f.seq);
            }
        }
        self.staged.is_some()
    }
}

/// The swap chain's images, the first time they're needed.
fn ensure_images(fx: &Funcs, dev: usize, swap_id: u64, swap: &mut Swap) {
    if swap.images.is_empty() {
        let mut n = 0u32;
        unsafe { (fx.get_images)(dev, swap_id, &mut n, std::ptr::null_mut()) };
        swap.images = vec![0; n as usize];
        unsafe { (fx.get_images)(dev, swap_id, &mut n, swap.images.as_mut_ptr()) };
        swap.targets = vec![(0, 0); n as usize];
    }
}

/// Draws the overlay over the image `present` is about to show. Returns the semaphore
/// the present must wait on instead of the game's, or None to present untouched.
fn draw(queue: usize, info: &PresentInfo) -> Option<u64> {
    let f = frame()?;
    if info.swap_count == 0 {
        return None;
    }
    let mut w = crate::sys::lock(&WORLD);
    let (dev, family) = *w.queues.get(&queue)?;
    let swap_id = unsafe { *info.swaps };
    let index = unsafe { *info.indices } as usize;
    let world = &mut *w;
    let swap = world.swaps.get_mut(&swap_id)?;
    if !swap.drawable {
        return None;
    }
    let d = world.devices.get_mut(&dev)?;
    let flags = d.family_flags(family);
    if flags != 0 && flags & 0x1 == 0 {
        // A queue that only presents: no drawing can be recorded for it.
        crate::log::throttled("video: vk present-only queue", 1, |_| {
            log!("video: the game presents on a Vulkan queue without graphics (family {family}, flags {flags:#x}); the overlay isn't drawn into its frames")
        });
        return None;
    }
    if d.funcs.is_none() {
        d.funcs = funcs(dev);
    }
    let fx = d.funcs.as_ref()?;
    ensure_images(fx, dev, swap_id, swap);
    let image = *swap.images.get(index)?;
    let (sw, sh) = (swap.width, swap.height);
    // Command pool and ring for this queue family.
    if !d.pools.contains_key(&family) {
        let ring = command_ring(fx, dev, family, RING, true)?;
        d.pools.insert(family, ring);
    }
    let all_fences: Vec<u64> = d.pools.values().flat_map(|p| p.1.iter().map(|r| r.1)).collect();
    let wait = || unsafe {
        (fx.wait_fences)(dev, all_fences.len() as u32, all_fences.as_ptr(), 1, 1_000_000_000);
    };
    if d.gfx.is_none() {
        d.gfx = Gfx::new(fx, dev, d.phys);
    }
    let g = d.gfx.as_mut()?;
    if !g.stage(fx, dev, &f, &wait) || !g.ensure_frame_objects(fx, dev, f.w, f.h, &wait) {
        return None;
    }
    let enc = encoding(swap.format, swap.color_space);
    g.bind_view(fx, dev, enc == Encoding::Plain && srgb(swap.format), &wait);
    let pass = g.pass(fx, dev, swap.format, PRESENT_SRC, enc)?;
    // A view and framebuffer per swap chain image, made the first time it's drawn into.
    let target = swap.targets.get_mut(index)?;
    if target.1 == 0 {
        let vi = ViewInfo { s_type: 15, next: 0, flags: 0, image, view_type: 1, format: swap.format, components: [0; 4], range: [1, 0, 1, 0, 1] };
        if unsafe { (fx.create_view)(dev, &vi, 0, &mut target.0) } != 0 {
            return None;
        }
        let fbi = FramebufferInfo { s_type: 37, next: 0, flags: 0, pass: pass.0, count: 1, attachments: &target.0, width: sw, height: sh, layers: 1 };
        if unsafe { (fx.create_framebuffer)(dev, &fbi, 0, &mut target.1) } != 0 {
            return None;
        }
    }
    let framebuffer = target.1;
    let (_, ring, next) = d.pools.get_mut(&family)?;
    let at = *next;
    let (cmd, fence, sem) = ring[at];
    *next = (*next + 1) % RING;
    unsafe {
        (fx.wait_fences)(dev, 1, &fence, 1, 1_000_000_000);
        (fx.reset_fences)(dev, 1, &fence);
        (fx.reset_cmd)(cmd, 0);
        let begin = BeginInfo { s_type: 42, next: 0, flags: 0x1, inheritance: 0 }; // ONE_TIME_SUBMIT
        (fx.begin)(cmd, &begin);
    }
    g.record(fx, cmd, pass, framebuffer, sw, sh);
    unsafe { (fx.end)(cmd) };
    let stages = vec![0x10000u32; info.wait_count as usize];
    let submit = SubmitInfo {
        s_type: 4,
        next: 0,
        wait_count: info.wait_count,
        waits: info.waits,
        stages: stages.as_ptr(),
        cmd_count: 1,
        cmds: &cmd,
        signal_count: 1,
        signals: &sem,
    };
    if unsafe { (fx.submit)(queue, 1, &submit, fence) } != 0 {
        // The fence was reset for this submit and nothing will signal it now: a signalled
        // one takes its place, or every later use of this entry would wait a second on it.
        let fence_info = Flagged { s_type: 8, next: 0, flags: 1, extra: 0 };
        let mut fresh = 0u64;
        unsafe { (fx.destroy_fence)(dev, fence, 0) };
        if made(unsafe { (fx.create_fence)(dev, &fence_info, 0, &mut fresh) }, &mut fresh) {
            ring[at].1 = fresh;
        } else {
            // No fence: the ring goes, and is made again at the next frame.
            if let Some((pool, ring, _)) = d.pools.remove(&family) {
                destroy_ring(fx, dev, pool, ring);
            }
        }
        crate::log::throttled("video: vk overlay submit", 3, |n| log!("video: the overlay's Vulkan commands couldn't be submitted (call {n}); the frame is shown without it"));
        return None;
    }
    Some(sem)
}

/// A command pool on queue `family` (its buffers resettable) and `n` entries of a
/// command buffer, a fence (`signalled` or not) and a semaphore; none of it left behind
/// when something can't be made.
fn command_ring(fx: &Funcs, dev: usize, family: u32, n: usize, signalled: bool) -> Option<(u64, Vec<(usize, u64, u64)>, usize)> {
    let pool_info = Flagged { s_type: 39, next: 0, flags: 0x2, extra: family };
    let mut pool = 0u64;
    if !made(unsafe { (fx.create_pool)(dev, &pool_info, 0, &mut pool) }, &mut pool) {
        return None;
    }
    let mut ring = Vec::new();
    for _ in 0..n {
        let mut cmd = 0usize;
        let ai = AllocInfo { s_type: 40, next: 0, pool, level: 0, count: 1 };
        let (mut fence, mut sem) = (0u64, 0u64);
        let fence_info = Flagged { s_type: 8, next: 0, flags: u32::from(signalled), extra: 0 };
        let sem_info = Flagged { s_type: 9, next: 0, flags: 0, extra: 0 };
        let ok = unsafe { (fx.alloc_cmd)(dev, &ai, &mut cmd) } == 0 && cmd != 0;
        let ok = ok && made(unsafe { (fx.create_fence)(dev, &fence_info, 0, &mut fence) }, &mut fence);
        let ok = ok && made(unsafe { (fx.create_semaphore)(dev, &sem_info, 0, &mut sem) }, &mut sem);
        if !ok {
            unsafe {
                (fx.destroy_fence)(dev, fence, 0);
            }
            destroy_ring(fx, dev, pool, ring);
            log!("video: Vulkan command buffers couldn't be made on queue family {family}");
            return None;
        }
        // A command buffer is dispatchable: it carries the device's dispatch pointer,
        // which the loader expects to find set when it's allocated outside the loader.
        unsafe { *(cmd as *mut usize) = *(dev as *const usize) };
        ring.push((cmd, fence, sem));
    }
    Some((pool, ring, 0))
}

/// A `command_ring`'s objects (none in use any more).
fn destroy_ring(fx: &Funcs, dev: usize, pool: u64, ring: Vec<(usize, u64, u64)>) {
    unsafe {
        for (_, fence, sem) in ring {
            (fx.destroy_fence)(dev, fence, 0);
            (fx.destroy_semaphore)(dev, sem, 0);
        }
        // Its command buffers go with it.
        (fx.destroy_pool)(dev, pool, 0);
    }
}

unsafe extern "system" fn destroy_swapchain(dev: usize, swap: u64, alloc: usize) {
    let real: DestroySwapchain = unsafe { std::mem::transmute(real_for(Some(dev), |r| r.destroy_swapchain)) };
    let _ = std::panic::catch_unwind(|| forget_swapchain(dev, swap));
    unsafe { real(dev, swap, alloc) }
}

unsafe extern "system" fn destroy_swapchain_export(dev: usize, swap: u64, alloc: usize) {
    let _ = std::panic::catch_unwind(|| forget_swapchain(dev, swap));
    unsafe { X_DESTROY_SWAPCHAIN.get().unwrap().call(dev, swap, alloc) }
}

/// PeerParty's views and framebuffers of a swap chain's images go before it does, once
/// the overlay's draws into them and the session's copies of them are done.
fn forget_swapchain(dev: usize, swap: u64) {
    forget_window(&[swap]);
    let mut w = crate::sys::lock(&WORLD);
    let world = &mut *w;
    if let (Some(s), Some(d)) = (world.swaps.remove(&swap), world.devices.get(&dev)) {
        if let Some(c) = d.capture.as_ref() {
            c.idle();
        }
        if let Some(fx) = d.funcs.as_ref() {
            let fences: Vec<u64> = d.pools.values().flat_map(|p| p.1.iter().map(|r| r.1)).collect();
            if !fences.is_empty() {
                unsafe { (fx.wait_fences)(dev, fences.len() as u32, fences.as_ptr(), 1, 1_000_000_000) };
            }
            destroy_targets(fx, dev, s.targets);
        }
    }
}

unsafe extern "system" fn destroy_device(dev: usize, alloc: usize) {
    let real: DestroyDevice = unsafe { std::mem::transmute(real_for(Some(dev), |r| r.destroy_device)) };
    let _ = std::panic::catch_unwind(|| forget_device(dev));
    unsafe { real(dev, alloc) }
}

unsafe extern "system" fn destroy_device_export(dev: usize, alloc: usize) {
    let _ = std::panic::catch_unwind(|| forget_device(dev));
    unsafe { X_DESTROY_DEVICE.get().unwrap().call(dev, alloc) }
}

/// Everything PeerParty made on a device goes before it does (after it's idle), with
/// what it knew of its queues and swap chains.
fn forget_device(dev: usize) {
    let (d, swaps) = {
        let mut w = crate::sys::lock(&WORLD);
        let Some(d) = w.devices.remove(&dev) else { return };
        w.queues.retain(|_, q| q.0 != dev);
        let ids: Vec<u64> = w.swaps.iter().filter(|(_, s)| s.dev == dev).map(|(id, _)| *id).collect();
        let swaps: Vec<Swap> = ids.iter().filter_map(|id| w.swaps.remove(id)).collect();
        forget_window(&ids);
        (d, swaps)
    };
    let drew = d.gfx.is_some();
    let copied = d.capture.as_ref().is_some_and(|c| c.made_anything());
    d.destroy(dev, swaps);
    crate::sys::lock(&DEV_REAL).remove(&dev);
    if drew || copied {
        log!("video: the game destroyed a Vulkan device; what PeerParty made on it (the overlay's objects, the frames' copies) went first");
    }
}

/// When a Vulkan frame was last presented, in milliseconds since `BASE` (0: never).
static LAST_PRESENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static BASE: OnceLock<std::time::Instant> = OnceLock::new();

fn now_ms() -> u64 {
    BASE.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

fn note_present() {
    LAST_PRESENT.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
}

/// Whether a Vulkan swap chain presented in the last second.
pub fn presented_recently() -> bool {
    let t = LAST_PRESENT.load(std::sync::atomic::Ordering::Relaxed);
    t != 0 && now_ms().saturating_sub(t) < 1000
}

unsafe extern "system" fn queue_present(queue: usize, info: *const PresentInfo) -> i32 {
    let dev = crate::sys::lock(&WORLD).queues.get(&queue).map(|q| q.0);
    let real: QueuePresent = unsafe { std::mem::transmute(real_for(dev, |r| r.present)) };
    presenting(&|q, i| unsafe { real(q, i) }, queue, info)
}

unsafe extern "system" fn queue_present_export(queue: usize, info: *const PresentInfo) -> i32 {
    presenting(&|q, i| unsafe { X_PRESENT.get().unwrap().call(q, i) }, queue, info)
}

/// The overlay drawn into the image, then the image copied for the session's window;
/// the present waits on whichever went last.
fn presenting(real: &dyn Fn(usize, *const PresentInfo) -> i32, queue: usize, info: *const PresentInfo) -> i32 {
    if info.is_null() {
        return real(queue, info);
    }
    note_present();
    super::presented();
    let i = unsafe { &*info };
    let drew = std::panic::catch_unwind(|| draw(queue, i)).ok().flatten();
    if drew.is_some() {
        super::drew("Vulkan");
    }
    let copied = std::panic::catch_unwind(|| vkcapture::capture(queue, i, drew)).ok().flatten();
    match copied.or(drew) {
        Some(sem) => {
            let changed = PresentInfo {
                s_type: i.s_type,
                next: i.next,
                wait_count: 1,
                waits: &sem,
                swap_count: i.swap_count,
                swaps: i.swaps,
                indices: i.indices,
                results: i.results,
            };
            real(queue, &changed)
        }
        None => real(queue, info),
    }
}

#[cfg(test)]
mod tests {
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
        exts: usize,
        features: usize,
    }

    fn swapchain_info(surface: u64, old: u64) -> SwapchainInfo {
        SwapchainInfo {
            s_type: 1000001000,
            next: 0,
            flags: 0,
            surface,
            min_images: 2,
            format: 44,
            color_space: 0,
            width: 64,
            height: 32,
            layers: 1,
            usage: 0x10,
            sharing: 0,
            family_count: 0,
            families: 0,
            transform: 1,
            alpha: 1,
            mode: 2,
            clipped: 1,
            old,
        }
    }

    /// A swap chain handed an old one is tried once (a failed try retires the old one,
    /// which can't be handed over again); without one, with less until the surface takes
    /// it, and it's known with the usage it got.
    #[test]
    fn a_swap_chain_replacing_another_is_tried_once() {
        let calls = std::cell::RefCell::new(Vec::new());
        let failing = |_: usize, i: *const SwapchainInfo, _: usize, _: *mut u64| {
            calls.borrow_mut().push((unsafe { (*i).usage }, unsafe { (*i).old }));
            -1000001004 // VK_ERROR_OUT_OF_DATE_KHR
        };
        let mut out = 0u64;
        let info = SwapchainInfo { usage: 0x1, ..swapchain_info(0x51, 0x77) };
        assert_eq!(creating_swapchain(&failing, 0xd1, &info, 0, &mut out), -1000001004);
        assert_eq!(*calls.borrow(), vec![(0x11, 0x77)], "one try, with COLOR_ATTACHMENT");

        // A surface that takes only what the game asked for (how many tries before it
        // depends on whether capture is wanted, which another test may have turned on).
        calls.borrow_mut().clear();
        let exact = |_: usize, i: *const SwapchainInfo, _: usize, o: *mut u64| {
            let usage = unsafe { (*i).usage };
            calls.borrow_mut().push((usage, 0));
            if usage != 0x2 {
                return -3; // VK_ERROR_INITIALIZATION_FAILED
            }
            unsafe { *o = 0x5a5a };
            0
        };
        let info = SwapchainInfo { usage: 0x2, ..swapchain_info(0x52, 0) };
        assert_eq!(creating_swapchain(&exact, 0xd2, &info, 0, &mut out), 0);
        let usages: Vec<u32> = calls.borrow().iter().map(|c| c.0).collect();
        assert!(usages.len() >= 2 && usages[0] & 0x12 == 0x12, "with COLOR_ATTACHMENT first: {usages:x?}");
        assert_eq!(*usages.last().unwrap(), 0x2, "as the game asked, the last try");
        let world = crate::sys::lock(&WORLD);
        let s = &world.swaps[&0x5a5a];
        assert!(!s.drawable && !s.capturable && s.dev == 0xd2, "known, and not drawn into");
        drop(world);
        forget_swapchain(0xd2, 0x5a5a);
    }

    /// A swap chain made on a surface of a known window marks that window as presented
    /// by Vulkan (the API above it leaves its frames alone) from the moment it's made,
    /// until it's destroyed.
    #[test]
    fn a_vulkan_swap_chain_latches_its_window() {
        let hwnd = 0x7e57_0001usize;
        let mut surface = 0xabc0_0001u64;
        let si = SurfaceInfo { s_type: 1000009000, next: 0, flags: 0, instance: 0, hwnd };
        note_surface(0, &si, &mut surface);
        assert!(!presents_to(hwnd));
        assert_eq!(surface_window(surface), hwnd);
        let swap = 0xabc0_0002u64;
        // What `note_swapchain` does, without reporting a made-up window as the game's.
        note_window(swap, surface_window(surface));
        assert!(presents_to(hwnd) && presents_anywhere());
        assert!(super::super::lower_layer_presents(|| hwnd), "latched, without a present yet");
        assert!(!presents_to(hwnd + 1), "another window isn't");
        forget_swapchain(0xd3, swap);
        assert!(!presents_to(hwnd), "let go with the swap chain");
    }

    /// Draws a 64x32 `frame(x, y)` (premultiplied BGRA) the way a present draws it, over
    /// an image of `format` (`bpp` bytes a pixel) cleared to `clear`, with the pipeline for
    /// `enc`, on a device of the PC's own; the image's bytes after. None (skipped) without
    /// a Vulkan driver, or one that can't blend into `format`.
    fn draw_once(format: u32, bpp: u32, enc: Encoding, clear_to: [f32; 4], frame: impl Fn(usize, usize) -> u32) -> Option<Vec<u8>> {
        let m = com::load("vulkan-1.dll");
        let gipa: GetProc = unsafe { std::mem::transmute(com::proc(m, c"vkGetInstanceProcAddr")?) };
        let ip = |inst: usize, n: &CStr| unsafe { gipa(inst, n.as_ptr()) };
        type CreateInstance = unsafe extern "system" fn(*const InstanceInfo, usize, *mut usize) -> i32;
        let create_instance: CreateInstance = unsafe { std::mem::transmute(ip(0, c"vkCreateInstance")) };
        let app = AppInfo { s_type: 0, next: 0, name: 0, version: 0, engine: 0, engine_version: 0, api: 1 << 22 };
        let ii = InstanceInfo { s_type: 1, next: 0, flags: 0, app: &app, layer_count: 0, layers: 0, ext_count: 0, exts: 0 };
        let mut inst = 0usize;
        if unsafe { create_instance(&ii, 0, &mut inst) } != 0 {
            eprintln!("no Vulkan instance: skipped");
            return None;
        }
        type Enumerate = unsafe extern "system" fn(usize, *mut u32, *mut usize) -> i32;
        let enumerate: Enumerate = unsafe { std::mem::transmute(ip(inst, c"vkEnumeratePhysicalDevices")) };
        let mut n = 1u32;
        let mut phys = 0usize;
        unsafe { enumerate(inst, &mut n, &mut phys) };
        if phys == 0 {
            eprintln!("no Vulkan device: skipped");
            return None;
        }
        // optimalTilingFeatures: COLOR_ATTACHMENT_BLEND and TRANSFER_SRC.
        type FormatProps = unsafe extern "system" fn(usize, u32, *mut [u32; 3]);
        let format_props: FormatProps = unsafe { std::mem::transmute(ip(inst, c"vkGetPhysicalDeviceFormatProperties")) };
        let mut fp = [0u32; 3];
        unsafe { format_props(phys, format, &mut fp) };
        if fp[1] & 0x100 == 0 {
            eprintln!("this Vulkan device can't blend into format {format}: skipped");
            return None;
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
        // The swap chain extension: PeerParty's drawing looks its functions up with the rest.
        let exts = [c"VK_KHR_swapchain".as_ptr()];
        let di = DeviceInfo { s_type: 3, next: 0, flags: 0, queue_count: 1, queues: &qi, layer_count: 0, layers: 0, ext_count: 1, exts: exts.as_ptr() as usize, features: 0 };
        type CreateDev = unsafe extern "system" fn(usize, *const DeviceInfo, usize, *mut usize) -> i32;
        let create_dev: CreateDev = unsafe { std::mem::transmute(ip(inst, c"vkCreateDevice")) };
        let mut dev = 0usize;
        assert_eq!(unsafe { create_dev(phys, &di, 0, &mut dev) }, 0);
        let gdpa: GetProc = unsafe { std::mem::transmute(ip(inst, c"vkGetDeviceProcAddr")) };
        let dp = |n: &CStr| unsafe { gdpa(dev, n.as_ptr()) };
        let fx = funcs_with(dp, m).expect("device functions");
        let get_queue: GetQueue = unsafe { std::mem::transmute(dp(c"vkGetDeviceQueue")) };
        let mut queue = 0usize;
        unsafe { get_queue(dev, family, 0, &mut queue) };

        // The target, standing in for a swap chain image.
        let (w, h) = (64u32, 32u32);
        let ii = ImageInfo {
            s_type: 14,
            next: 0,
            flags: 0,
            image_type: 1,
            format,
            extent: [w, h, 1],
            mips: 1,
            layers: 1,
            samples: 1,
            tiling: 0,
            usage: 0x10 | 0x1 | 0x2,
            sharing: 0,
            family_count: 0,
            families: 0,
            initial_layout: 0,
        };
        let mut target = 0u64;
        assert_eq!(unsafe { (fx.create_image)(dev, &ii, 0, &mut target) }, 0);
        let mut reqs = [0u64; 3];
        unsafe { (fx.image_reqs)(dev, target, &mut reqs) };
        let ma = MemAlloc { s_type: 5, next: 0, size: reqs[0], type_index: memory_type(&fx, phys, reqs[2], 0x1).unwrap() };
        let mut tmem = 0u64;
        assert_eq!(unsafe { (fx.alloc_mem)(dev, &ma, 0, &mut tmem) }, 0);
        assert_eq!(unsafe { (fx.bind_image)(dev, target, tmem, 0) }, 0);
        let vi = ViewInfo { s_type: 15, next: 0, flags: 0, image: target, view_type: 1, format, components: [0; 4], range: [1, 0, 1, 0, 1] };
        let mut tview = 0u64;
        assert_eq!(unsafe { (fx.create_view)(dev, &vi, 0, &mut tview) }, 0);

        // The overlay's objects, with the frame put in staging by hand (copy_to reads the
        // shared block, which a test doesn't have).
        let mut g = Gfx::new(&fx, dev, phys).expect("pipeline objects");
        let wait = || {};
        assert!(g.ensure_frame_objects(&fx, dev, w, h, &wait));
        let (sbuf, smem, sptr) = host_buffer(&fx, dev, phys, (w * h * 4) as usize, 0x1).unwrap();
        let px = unsafe { std::slice::from_raw_parts_mut(sptr as *mut u32, (w * h) as usize) };
        for y in 0..h as usize {
            for x in 0..w as usize {
                px[y * w as usize + x] = frame(x, y);
            }
        }
        g.staging = Some((sbuf, smem, sptr, (w * h * 4) as usize));
        g.staged = Some(2);
        g.bind_view(&fx, dev, false, &wait);
        let general = 1;
        let pass = g.pass(&fx, dev, format, general, enc).expect("render pass and pipeline");
        let fbi = FramebufferInfo { s_type: 37, next: 0, flags: 0, pass: pass.0, count: 1, attachments: &tview, width: w, height: h, layers: 1 };
        let mut fb = 0u64;
        assert_eq!(unsafe { (fx.create_framebuffer)(dev, &fbi, 0, &mut fb) }, 0);
        let size = (w * h * bpp) as usize;
        let (rbuf, _, rptr) = host_buffer(&fx, dev, phys, size, 0x2).unwrap();

        let pool_info = Flagged { s_type: 39, next: 0, flags: 0x2, extra: family };
        let mut pool = 0u64;
        assert_eq!(unsafe { (fx.create_pool)(dev, &pool_info, 0, &mut pool) }, 0);
        let ai = AllocInfo { s_type: 40, next: 0, pool, level: 0, count: 1 };
        let mut cmd = 0usize;
        assert_eq!(unsafe { (fx.alloc_cmd)(dev, &ai, &mut cmd) }, 0);
        let begin = BeginInfo { s_type: 42, next: 0, flags: 0x1, inheritance: 0 };
        let barrier = |old, new, src, dst| ImageBarrier {
            s_type: 45,
            next: 0,
            src_access: src,
            dst_access: dst,
            old_layout: old,
            new_layout: new,
            src_family: u32::MAX,
            dst_family: u32::MAX,
            image: target,
            aspect: 1,
            base_mip: 0,
            mips: 1,
            base_layer: 0,
            layers: 1,
        };
        type Clear = unsafe extern "system" fn(usize, u64, u32, *const [f32; 4], u32, *const [u32; 5]);
        let clear: Clear = unsafe { std::mem::transmute(dp(c"vkCmdClearColorImage")) };
        type CopyOut = unsafe extern "system" fn(usize, u64, u32, u64, u32, *const BufferImageCopy);
        let copy_out: CopyOut = unsafe { std::mem::transmute(dp(c"vkCmdCopyImageToBuffer")) };
        let region = BufferImageCopy { offset: 0, row_length: w, image_height: h, aspect: 1, mip: 0, base_layer: 0, layers: 1, x: 0, y: 0, z: 0, w, h, d: 1 };
        unsafe {
            (fx.begin)(cmd, &begin);
            (fx.barrier)(cmd, 0x1, 0x1000, 0, 0, 0, 0, 0, 1, &barrier(0, general, 0, 0x1000));
            clear(cmd, target, general, &clear_to, 1, &[1, 0, 1, 0, 1]);
            (fx.barrier)(cmd, 0x1000, 0x400, 0, 0, 0, 0, 0, 1, &barrier(general, general, 0x1000, 0x80 | 0x100));
        }
        g.record(&fx, cmd, pass, fb, w, h);
        unsafe {
            (fx.barrier)(cmd, 0x400, 0x1000, 0, 0, 0, 0, 0, 1, &barrier(general, general, 0x100, 0x800));
            copy_out(cmd, target, general, rbuf, 1, &region);
            (fx.end)(cmd);
        }
        let submit = SubmitInfo {
            s_type: 4,
            next: 0,
            wait_count: 0,
            waits: std::ptr::null(),
            stages: std::ptr::null(),
            cmd_count: 1,
            cmds: &cmd,
            signal_count: 0,
            signals: std::ptr::null(),
        };
        assert_eq!(unsafe { (fx.submit)(queue, 1, &submit, 0) }, 0);
        let idle: unsafe extern "system" fn(usize) -> i32 = unsafe { std::mem::transmute(dp(c"vkQueueWaitIdle")) };
        assert_eq!(unsafe { idle(queue) }, 0);
        let out = unsafe { std::slice::from_raw_parts(rptr as *const u8, size) }.to_vec();
        unsafe { (fx.device_wait_idle)(dev) };
        g.destroy(&fx, dev);
        let destroy_dev: unsafe extern "system" fn(usize, usize) = unsafe { std::mem::transmute(dp(c"vkDestroyDevice")) };
        unsafe { destroy_dev(dev, 0) };
        Some(out)
    }

    /// Pixel `(x, y)` of a 64-wide image read back, `bpp` bytes a pixel.
    fn pixel(out: &[u8], x: usize, y: usize, bpp: usize) -> &[u8] {
        let at = (y * 64 + x) * bpp;
        &out[at..at + bpp]
    }

    fn half(b: &[u8]) -> f32 {
        let h = u16::from_le_bytes([b[0], b[1]]);
        let (sign, exp, frac) = ((h >> 15) as i32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
        let v = if exp == 0 { frac * 2f32.powi(-24) } else { (1.0 + frac / 1024.0) * 2f32.powi(exp - 15) };
        if sign == 1 { -v } else { v }
    }

    /// An image cleared blue, and a frame that is half clear (left) and half 50% red: the
    /// blend leaves blue on the clear half and mixes the other.
    #[test]
    fn draws_a_blended_frame_over_an_image() {
        let Some(out) = draw_once(BGRA_UNORM, 4, Encoding::Plain, [0.0, 0.0, 1.0, 1.0], |x, _| if x >= 32 { 0x8080_0000 } else { 0 }) else { return };
        let (left, right) = (pixel(&out, 4, 16, 4), pixel(&out, 60, 16, 4));
        eprintln!("Vulkan blend: left {left:?}, right {right:?} (BGRA)");
        assert_eq!(&left[..3], &[0xff, 0, 0], "left {left:?}");
        assert!((right[2] as i32 - 128).abs() <= 2 && (right[0] as i32 - 127).abs() <= 2, "right {right:?}");
    }

    /// An scRGB image (float, linear light) cleared to half blue: where the frame is 50%
    /// red (the top half) it arrives as red at SDR white (2.5) times its coverage, blended
    /// in linear light; where it's clear (the bottom) the image is as it was.
    #[test]
    fn draws_the_frame_into_an_scrgb_image_at_sdr_white() {
        let Some(out) = draw_once(97, 8, Encoding::ScRgb, [0.0, 0.0, 0.5, 1.0], |_, y| if y < 16 { 0x8080_0000 } else { 0 }) else { return };
        let rgba = |x, y| {
            let p = pixel(&out, x, y, 8);
            [half(&p[0..2]), half(&p[2..4]), half(&p[4..6]), half(&p[6..8])]
        };
        let (top, bottom) = (rgba(32, 4), rgba(32, 28));
        eprintln!("Vulkan scRGB: top {top:?}, bottom {bottom:?}");
        let a = 128.0 / 255.0;
        let near = |v: f32, want: f32| (v - want).abs() < 0.01;
        assert!(near(top[0], 2.5 * a) && near(top[1], 0.0) && near(top[2], 0.5 * (1.0 - a)) && near(top[3], 1.0), "top {top:?}");
        assert!(near(bottom[0], 0.0) && near(bottom[2], 0.5) && near(bottom[3], 1.0), "bottom (not flipped) {bottom:?}");
    }

    /// An HDR10 image (A2B10G10R10, PQ): the frame's opaque white (the top half) arrives
    /// as SDR white (200 nits) PQ-encoded; its clear half (the bottom) leaves the image
    /// black.
    #[test]
    fn draws_the_frame_into_an_hdr10_image_pq_encoded() {
        let Some(out) = draw_once(64, 4, Encoding::Pq, [0.0, 0.0, 0.0, 0.0], |_, y| if y < 16 { 0xffff_ffff } else { 0 }) else { return };
        let rgb = |x, y| {
            let v = u32::from_le_bytes(pixel(&out, x, y, 4).try_into().unwrap());
            [v & 0x3ff, (v >> 10) & 0x3ff, (v >> 20) & 0x3ff]
        };
        let (top, bottom) = (rgb(32, 4), rgb(32, 28));
        // PQ of 200 / 10000 (BT.709 white is BT.2020 white).
        let y = (200.0f64 / 10000.0).powf(0.1593017578125);
        let want = (((0.8359375 + 18.8515625 * y) / (1.0 + 18.6875 * y)).powf(78.84375) * 1023.0).round() as i32;
        eprintln!("Vulkan HDR10: top {top:?}, bottom {bottom:?}, white wanted {want}");
        assert!(top.iter().all(|c| (*c as i32 - want).abs() <= 2), "top {top:?}, want {want}");
        assert_eq!(bottom, [0, 0, 0], "bottom (not flipped)");
    }
}
