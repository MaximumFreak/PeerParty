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
//! - `vkQueuePresentKHR`: with the overlay showing, a small command buffer uploads the
//!   frame (when it changed) into a texture and draws it over the image being presented
//!   as one quad with premultiplied-alpha blending, like the other APIs: the dimmed
//!   background and rounded corners included. It waits on the game's semaphores and
//!   signals its own, which the real present then waits on.
//!
//! The shaders are Dear ImGui's precompiled SPIR-V (vkshaders.rs): this build has no
//! shader compiler.
//!
//! These hooks go in when the game loads vulkan-1.dll (with the overlay on), because a
//! game creates its swap chain once, at start-up; until the overlay shows, each costs a
//! lookup or a flag check.

use std::collections::HashMap;
use std::ffi::{CStr, c_char};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use super::{com, frame, vkshaders};
use crate::hook::with_lock;
use crate::log;

type Pfn = usize;
type GetProc = unsafe extern "system" fn(usize, *const c_char) -> Pfn;
type CreateDevice = unsafe extern "system" fn(usize, *const u8, usize, *mut usize) -> i32;
type GetQueue = unsafe extern "system" fn(usize, u32, u32, *mut usize);
type GetQueue2 = unsafe extern "system" fn(usize, *const QueueInfo2, *mut usize);
type CreateSwapchain = unsafe extern "system" fn(usize, *const SwapchainInfo, usize, *mut u64) -> i32;
type QueuePresent = unsafe extern "system" fn(usize, *const PresentInfo) -> i32;
type DestroySwapchain = unsafe extern "system" fn(usize, u64, usize);

static GET_DEVICE_PROC: OnceLock<GenericDetour<GetProc>> = OnceLock::new();
static GET_INSTANCE_PROC: OnceLock<GenericDetour<GetProc>> = OnceLock::new();

/// The real functions behind PeerParty's versions (the last ones the game was given).
#[derive(Default)]
struct Real {
    create_device: Pfn,
    get_queue: Pfn,
    get_queue2: Pfn,
    create_swapchain: Pfn,
    destroy_swapchain: Pfn,
    present: Pfn,
}

static REAL: Mutex<Real> =
    Mutex::new(Real { create_device: 0, get_queue: 0, get_queue2: 0, create_swapchain: 0, destroy_swapchain: 0, present: 0 });

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
    format: u32,
    width: u32,
    height: u32,
    images: Vec<u64>,
    /// Per image: PeerParty's view of it and framebuffer (0 until first drawn into).
    targets: Vec<(u64, u64)>,
}

/// Per device: what drawing needs.
struct Dev {
    phys: usize,
    funcs: Option<Funcs>,
    /// Queue family -> command pool, ring of (command buffer, fence, semaphore).
    pools: HashMap<u32, (u64, Vec<(usize, u64, u64)>, usize)>,
    gfx: Option<Gfx>,
}

unsafe impl Send for Dev {}

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

/// The game loaded vulkan-1.dll (watch.rs), with the overlay on.
pub fn loaded() {
    let m = com::module("vulkan-1.dll");
    let (Some(gdpa), Some(gipa)) = (com::proc(m, c"vkGetDeviceProcAddr"), com::proc(m, c"vkGetInstanceProcAddr")) else { return };
    {
        let mut r = REAL.lock().unwrap();
        r.create_device = com::proc(m, c"vkCreateDevice").unwrap_or(0);
        r.get_queue = com::proc(m, c"vkGetDeviceQueue").unwrap_or(0);
        r.get_queue2 = com::proc(m, c"vkGetDeviceQueue2").unwrap_or(0);
        r.create_swapchain = com::proc(m, c"vkCreateSwapchainKHR").unwrap_or(0);
        r.destroy_swapchain = com::proc(m, c"vkDestroySwapchainKHR").unwrap_or(0);
        r.present = com::proc(m, c"vkQueuePresentKHR").unwrap_or(0);
    }
    let ok = with_lock(|| {
        let mut n = 0;
        for (slot, addr, detour) in [(&GET_DEVICE_PROC, gdpa, get_device_proc as GetProc), (&GET_INSTANCE_PROC, gipa, get_instance_proc as GetProc)] {
            let target: GetProc = unsafe { std::mem::transmute(addr) };
            if let Ok(h) = unsafe { GenericDetour::new(target, detour) } {
                let _ = slot.set(h);
                n += usize::from(crate::hook::frozen(addr, || unsafe { slot.get().unwrap().enable() }).is_ok());
            }
        }
        n
    });
    log!("present: Vulkan loaded, {ok} of 2 lookups hooked (the overlay can be drawn into its frames)");
}

/// Our version of `name`, after noting the real one.
fn ours(name: &str, real: Pfn) -> Option<Pfn> {
    if real == 0 {
        return None;
    }
    let mut r = REAL.lock().unwrap();
    Some(match name {
        "vkCreateDevice" => {
            r.create_device = real;
            create_device as *const () as usize
        }
        "vkGetDeviceQueue" => {
            r.get_queue = real;
            get_queue as *const () as usize
        }
        "vkGetDeviceQueue2" => {
            r.get_queue2 = real;
            get_queue2 as *const () as usize
        }
        "vkCreateSwapchainKHR" => {
            r.create_swapchain = real;
            create_swapchain as *const () as usize
        }
        "vkDestroySwapchainKHR" => {
            r.destroy_swapchain = real;
            destroy_swapchain as *const () as usize
        }
        "vkQueuePresentKHR" => {
            r.present = real;
            queue_present as *const () as usize
        }
        _ => return None,
    })
}

unsafe extern "system" fn get_device_proc(dev: usize, name: *const c_char) -> Pfn {
    let real = unsafe { GET_DEVICE_PROC.get().unwrap().call(dev, name) };
    let n = if name.is_null() { "" } else { unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("") };
    ours(n, real).unwrap_or(real)
}

unsafe extern "system" fn get_instance_proc(inst: usize, name: *const c_char) -> Pfn {
    let real = unsafe { GET_INSTANCE_PROC.get().unwrap().call(inst, name) };
    let n = if name.is_null() { "" } else { unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("") };
    ours(n, real).unwrap_or(real)
}

unsafe extern "system" fn create_device(phys: usize, info: *const u8, alloc: usize, out: *mut usize) -> i32 {
    let real: CreateDevice = unsafe { std::mem::transmute(REAL.lock().unwrap().create_device) };
    let r = unsafe { real(phys, info, alloc, out) };
    if r == 0 && !out.is_null() {
        let d = unsafe { *out };
        WORLD.lock().unwrap().devices.insert(d, Dev { phys, funcs: None, pools: HashMap::new(), gfx: None });
    }
    r
}

unsafe extern "system" fn get_queue(dev: usize, family: u32, index: u32, out: *mut usize) {
    let real: GetQueue = unsafe { std::mem::transmute(REAL.lock().unwrap().get_queue) };
    unsafe { real(dev, family, index, out) };
    if !out.is_null() && unsafe { *out } != 0 {
        WORLD.lock().unwrap().queues.insert(unsafe { *out }, (dev, family));
    }
}

unsafe extern "system" fn get_queue2(dev: usize, info: *const QueueInfo2, out: *mut usize) {
    let real: GetQueue2 = unsafe { std::mem::transmute(REAL.lock().unwrap().get_queue2) };
    unsafe { real(dev, info, out) };
    if !out.is_null() && !info.is_null() && unsafe { *out } != 0 {
        WORLD.lock().unwrap().queues.insert(unsafe { *out }, (dev, unsafe { (*info).family }));
    }
}

unsafe extern "system" fn create_swapchain(dev: usize, info: *const SwapchainInfo, alloc: usize, out: *mut u64) -> i32 {
    let real: CreateSwapchain = unsafe { std::mem::transmute(REAL.lock().unwrap().create_swapchain) };
    if info.is_null() {
        return unsafe { real(dev, info, alloc, out) };
    }
    let mut copy = unsafe { *info };
    copy.usage |= 0x10; // COLOR_ATTACHMENT
    let mut r = unsafe { real(dev, &copy, alloc, out) };
    if r != 0 {
        // The surface refused the extra usage: create it as the game asked.
        r = unsafe { real(dev, info, alloc, out) };
    } else if !out.is_null() {
        WORLD.lock().unwrap().swaps.insert(unsafe { *out }, Swap { format: copy.format, width: copy.width, height: copy.height, images: Vec::new(), targets: Vec::new() });
    }
    r
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
    funcs_with(|n| unsafe { gdpa.call(dev, n.as_ptr()) }, com::module("vulkan-1.dll"))
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

/// The overlay's texture and pipeline objects on one device.
struct Gfx {
    phys: usize,
    sampler: u64,
    set: u64,
    layout: u64,
    vert: u64,
    frag: u64,
    /// (swap chain format, layout the image is in) -> (render pass, pipeline).
    passes: HashMap<(u32, u32), (u64, u64)>,
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

/// A host-visible, coherent, mapped buffer.
fn host_buffer(fx: &Funcs, dev: usize, phys: usize, size: usize, usage: u32) -> Option<(u64, u64, *mut u8)> {
    let bi = BufferInfo { s_type: 12, next: 0, flags: 0, size: size as u64, usage, sharing: 0, family_count: 0, families: 0 };
    let mut buf = 0u64;
    if unsafe { (fx.create_buffer)(dev, &bi, 0, &mut buf) } != 0 {
        return None;
    }
    let mut reqs = [0u64; 3]; // size, alignment, memoryTypeBits
    unsafe { (fx.buffer_reqs)(dev, buf, &mut reqs) };
    let type_index = memory_type(fx, phys, reqs[2], 0x2 | 0x4)?; // HOST_VISIBLE | HOST_COHERENT
    let ma = MemAlloc { s_type: 5, next: 0, size: reqs[0], type_index };
    let mut mem = 0u64;
    let mut ptr = std::ptr::null_mut();
    if unsafe { (fx.alloc_mem)(dev, &ma, 0, &mut mem) } != 0
        || unsafe { (fx.bind)(dev, buf, mem, 0) } != 0
        || unsafe { (fx.map)(dev, mem, 0, u64::MAX, 0, &mut ptr) } != 0
    {
        return None;
    }
    Some((buf, mem, ptr))
}

impl Gfx {
    /// The objects that don't depend on the swap chain or the frame.
    fn new(fx: &Funcs, dev: usize, phys: usize) -> Option<Gfx> {
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
        let mut sampler = 0u64;
        if unsafe { (fx.create_sampler)(dev, &si, 0, &mut sampler) } != 0 {
            return None;
        }
        // One combined image sampler for the fragment shader.
        let binding = SetLayoutBinding { binding: 0, kind: 1, count: 1, stages: 0x10, immutable: 0 };
        let sli = SetLayoutInfo { s_type: 32, next: 0, flags: 0, count: 1, bindings: &binding };
        let mut set_layout = 0u64;
        if unsafe { (fx.create_set_layout)(dev, &sli, 0, &mut set_layout) } != 0 {
            return None;
        }
        let size = [1u32, 1];
        let dpi = DescPoolInfo { s_type: 33, next: 0, flags: 0, max_sets: 1, size_count: 1, sizes: &size };
        let mut pool = 0u64;
        if unsafe { (fx.create_desc_pool)(dev, &dpi, 0, &mut pool) } != 0 {
            return None;
        }
        let sai = SetAllocInfo { s_type: 34, next: 0, pool, count: 1, layouts: &set_layout };
        let mut set = 0u64;
        if unsafe { (fx.alloc_sets)(dev, &sai, &mut set) } != 0 {
            return None;
        }
        // uScale, uTranslate for the vertex shader.
        let push = [0x1u32, 0, 16];
        let pli = PipelineLayoutInfo { s_type: 30, next: 0, flags: 0, set_count: 1, sets: &set_layout, push_count: 1, pushes: &push };
        let mut layout = 0u64;
        if unsafe { (fx.create_pipeline_layout)(dev, &pli, 0, &mut layout) } != 0 {
            return None;
        }
        let shader = |code: &[u32]| {
            let info = ShaderInfo { s_type: 16, next: 0, flags: 0, size: code.len() * 4, code: code.as_ptr() };
            let mut m = 0u64;
            (unsafe { (fx.create_shader)(dev, &info, 0, &mut m) } == 0).then_some(m)
        };
        let vert = shader(&vkshaders::VERT)?;
        let frag = shader(&vkshaders::FRAG)?;
        Some(Gfx {
            phys,
            sampler,
            set,
            layout,
            vert,
            frag,
            passes: HashMap::new(),
            tex: None,
            quad: None,
            staging: None,
            staged: None,
            uploaded: None,
        })
    }

    /// A render pass that keeps what's in the image (in `layout` before and after) and
    /// its pipeline, per format.
    fn pass(&mut self, fx: &Funcs, dev: usize, format: u32, layout: u32) -> Option<(u64, u64)> {
        if let Some(p) = self.passes.get(&(format, layout)) {
            return Some(*p);
        }
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
        if unsafe { (fx.create_render_pass)(dev, &rpi, 0, &mut pass) } != 0 {
            return None;
        }
        let main = c"main";
        let stages = [
            StageInfo { s_type: 18, next: 0, flags: 0, stage: 0x1, module: self.vert, name: main.as_ptr(), specialization: 0 },
            StageInfo { s_type: 18, next: 0, flags: 0, stage: 0x10, module: self.frag, name: main.as_ptr(), specialization: 0 },
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
            return None;
        }
        self.passes.insert((format, layout), (pass, pipeline));
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
            if unsafe { (fx.create_image)(dev, &ii, 0, &mut image) } != 0 {
                return false;
            }
            let mut reqs = [0u64; 3];
            unsafe { (fx.image_reqs)(dev, image, &mut reqs) };
            let Some(type_index) = memory_type(fx, self.phys, reqs[2], 0x1).or_else(|| memory_type(fx, self.phys, reqs[2], 0)) else {
                return false;
            };
            let ma = MemAlloc { s_type: 5, next: 0, size: reqs[0], type_index };
            let mut mem = 0u64;
            if unsafe { (fx.alloc_mem)(dev, &ma, 0, &mut mem) } != 0 || unsafe { (fx.bind_image)(dev, image, mem, 0) } != 0 {
                return false;
            }
            let mut views = [0u64; 2];
            for (i, format) in [BGRA_UNORM, BGRA_SRGB].into_iter().enumerate() {
                let vi = ViewInfo { s_type: 15, next: 0, flags: 0, image, view_type: 1, format, components: [0; 4], range: [1, 0, 1, 0, 1] };
                if unsafe { (fx.create_view)(dev, &vi, 0, &mut views[i]) } != 0 {
                    return false;
                }
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

    /// Points the descriptor set at the texture's view for this swap chain's encoding.
    fn bind_view(&mut self, fx: &Funcs, dev: usize, srgb: bool, wait: &dyn Fn()) {
        let Some(t) = self.tex.as_mut() else { return };
        let which = usize::from(srgb);
        if t.6 == Some(which) {
            return;
        }
        if t.6.is_some() {
            wait();
        }
        let image = ImageDesc { sampler: self.sampler, view: t.4[which], layout: SHADER_READ };
        let write = WriteSet { s_type: 35, next: 0, set: self.set, binding: 0, element: 0, count: 1, kind: 1, images: &image, buffers: 0, texel_views: 0 };
        unsafe { (fx.update_sets)(dev, 1, &write, 0, 0) };
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
        // Pixels to clip space, top-left at (-1, -1).
        let push = [2.0 / sw as f32, 2.0 / sh as f32, -1.0, -1.0];
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
            self.staging = Some((buf, mem, ptr, need));
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

/// Draws the overlay over the image `present` is about to show. Returns the semaphore
/// the present must wait on instead of the game's, or None to present untouched.
fn draw(queue: usize, info: &PresentInfo) -> Option<u64> {
    let f = frame()?;
    if info.swap_count == 0 {
        return None;
    }
    let mut w = WORLD.lock().unwrap();
    let (dev, family) = *w.queues.get(&queue)?;
    let swap_id = unsafe { *info.swaps };
    let index = unsafe { *info.indices } as usize;
    let world = &mut *w;
    let swap = world.swaps.get_mut(&swap_id)?;
    let d = world.devices.get_mut(&dev)?;
    if d.funcs.is_none() {
        d.funcs = funcs(dev);
    }
    let fx = d.funcs.as_ref()?;
    if swap.images.is_empty() {
        let mut n = 0u32;
        unsafe { (fx.get_images)(dev, swap_id, &mut n, std::ptr::null_mut()) };
        swap.images = vec![0; n as usize];
        unsafe { (fx.get_images)(dev, swap_id, &mut n, swap.images.as_mut_ptr()) };
        swap.targets = vec![(0, 0); n as usize];
    }
    let image = *swap.images.get(index)?;
    let (sw, sh) = (swap.width, swap.height);
    // Command pool and ring for this queue family.
    if !d.pools.contains_key(&family) {
        // RESET_COMMAND_BUFFER, on the presenting queue's family.
        let pool_info = Flagged { s_type: 39, next: 0, flags: 0x2, extra: family };
        let mut pool = 0u64;
        if unsafe { (fx.create_pool)(dev, &pool_info, 0, &mut pool) } != 0 {
            return None;
        }
        let mut ring = Vec::new();
        for _ in 0..RING {
            let mut cmd = 0usize;
            let ai = AllocInfo { s_type: 40, next: 0, pool, level: 0, count: 1 };
            let mut fence = 0u64;
            let mut sem = 0u64;
            let fence_info = Flagged { s_type: 8, next: 0, flags: 1, extra: 0 }; // signalled
            let sem_info = Flagged { s_type: 9, next: 0, flags: 0, extra: 0 };
            unsafe {
                (fx.alloc_cmd)(dev, &ai, &mut cmd);
                (fx.create_fence)(dev, &fence_info, 0, &mut fence);
                (fx.create_semaphore)(dev, &sem_info, 0, &mut sem);
            }
            // A command buffer is dispatchable: it carries the device's dispatch pointer,
            // which the loader expects to find set when it's allocated outside the loader.
            unsafe { *(cmd as *mut usize) = *(dev as *const usize) };
            ring.push((cmd, fence, sem));
        }
        d.pools.insert(family, (pool, ring, 0));
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
    g.bind_view(fx, dev, srgb(swap.format), &wait);
    let pass = g.pass(fx, dev, swap.format, PRESENT_SRC)?;
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
    let (cmd, fence, sem) = ring[*next];
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
        return None;
    }
    Some(sem)
}

unsafe extern "system" fn destroy_swapchain(dev: usize, swap: u64, alloc: usize) {
    let real: DestroySwapchain = unsafe { std::mem::transmute(REAL.lock().unwrap().destroy_swapchain) };
    if let Ok(mut w) = WORLD.lock() {
        let world = &mut *w;
        if let (Some(s), Some(d)) = (world.swaps.remove(&swap), world.devices.get(&dev)) {
            if let Some(fx) = d.funcs.as_ref() {
                let fences: Vec<u64> = d.pools.values().flat_map(|p| p.1.iter().map(|r| r.1)).collect();
                if !fences.is_empty() {
                    unsafe { (fx.wait_fences)(dev, fences.len() as u32, fences.as_ptr(), 1, 1_000_000_000) };
                }
                for (view, fb) in s.targets {
                    unsafe {
                        if fb != 0 {
                            (fx.destroy_framebuffer)(dev, fb, 0);
                        }
                        if view != 0 {
                            (fx.destroy_view)(dev, view, 0);
                        }
                    }
                }
            }
        }
    }
    unsafe { real(dev, swap, alloc) }
}

unsafe extern "system" fn queue_present(queue: usize, info: *const PresentInfo) -> i32 {
    let real: QueuePresent = unsafe { std::mem::transmute(REAL.lock().unwrap().present) };
    if info.is_null() {
        return unsafe { real(queue, info) };
    }
    let i = unsafe { &*info };
    match std::panic::catch_unwind(|| draw(queue, i)).ok().flatten() {
        Some(sem) => {
            super::drew("Vulkan");
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
            unsafe { real(queue, &changed) }
        }
        None => unsafe { real(queue, info) },
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

    /// A device of the PC's own (skipped without a Vulkan driver), an offscreen image
    /// cleared blue, and a frame that is half clear and half 50% red: drawn the way a
    /// present draws it, the blend must leave blue on the clear half and mix the other.
    #[test]
    fn draws_a_blended_frame_over_an_image() {
        let m = com::load("vulkan-1.dll");
        let Some(gipa) = com::proc(m, c"vkGetInstanceProcAddr") else { return };
        let gipa: GetProc = unsafe { std::mem::transmute(gipa) };
        let ip = |inst: usize, n: &CStr| unsafe { gipa(inst, n.as_ptr()) };
        type CreateInstance = unsafe extern "system" fn(*const InstanceInfo, usize, *mut usize) -> i32;
        let create_instance: CreateInstance = unsafe { std::mem::transmute(ip(0, c"vkCreateInstance")) };
        let app = AppInfo { s_type: 0, next: 0, name: 0, version: 0, engine: 0, engine_version: 0, api: 1 << 22 };
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

        // The target: a 64x32 BGRA image, cleared blue.
        let (w, h) = (64u32, 32u32);
        let ii = ImageInfo {
            s_type: 14,
            next: 0,
            flags: 0,
            image_type: 1,
            format: BGRA_UNORM,
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
        let vi = ViewInfo { s_type: 15, next: 0, flags: 0, image: target, view_type: 1, format: BGRA_UNORM, components: [0; 4], range: [1, 0, 1, 0, 1] };
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
                px[y * w as usize + x] = if x >= w as usize / 2 { 0x8080_0000 } else { 0 };
            }
        }
        g.staging = Some((sbuf, smem, sptr, (w * h * 4) as usize));
        g.staged = Some(2);
        g.bind_view(&fx, dev, false, &wait);
        let general = 1;
        let pass = g.pass(&fx, dev, BGRA_UNORM, general).expect("render pass and pipeline");
        let fbi = FramebufferInfo { s_type: 37, next: 0, flags: 0, pass: pass.0, count: 1, attachments: &tview, width: w, height: h, layers: 1 };
        let mut fb = 0u64;
        assert_eq!(unsafe { (fx.create_framebuffer)(dev, &fbi, 0, &mut fb) }, 0);
        let (rbuf, _, rptr) = host_buffer(&fx, dev, phys, (w * h * 4) as usize, 0x2).unwrap();

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
            clear(cmd, target, general, &[0.0, 0.0, 1.0, 1.0], 1, &[1, 0, 1, 0, 1]);
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
        let out = unsafe { std::slice::from_raw_parts(rptr as *const u32, (w * h) as usize) };
        let left = out[(h as usize / 2) * w as usize + 4];
        let right = out[(h as usize / 2) * w as usize + w as usize - 4];
        eprintln!("Vulkan blend: left {left:#010x}, right {right:#010x}");
        assert_eq!(left & 0x00ff_ffff, 0x0000_00ff, "left {left:#x}");
        let (rr, bb) = ((right >> 16) & 0xff, right & 0xff);
        assert!((rr as i32 - 128).abs() <= 2 && (bb as i32 - 127).abs() <= 2, "right {right:#x}");
        let destroy_dev: unsafe extern "system" fn(usize, usize) = unsafe { std::mem::transmute(dp(c"vkDestroyDevice")) };
        unsafe { destroy_dev(dev, 0) };
    }
}
