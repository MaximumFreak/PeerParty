//! Fair-share GPU memory budget reporting for split-screen copies (`PEERPARTY_VRAM_SHARE`).
//!
//! When 2–4 copies run on one GPU, each queries `IDXGIAdapter3::QueryVideoMemoryInfo`
//! (D3D11/12) or `VK_EXT_memory_budget` (Vulkan) and sizes its texture streaming pool for
//! the whole GPU. Clamping the reported budget to `min(real_budget, fair_cap)`—where
//! `fair_cap` is `85% * physical_vram / N` (with a 512 MiB floor)—lets engines downsize
//! their own streaming pools gracefully without WDDM PCIe paging stutter, and never
//! double-divides when WDDM has already lowered the per-process budget.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use super::com::{self, Guid, IID_IDXGIFACTORY1, release, vcall};
use crate::log;

/// `IID_IDXGIAdapter3` (`{645967A4-1392-4310-A798-8053CE3E93FD}`).
const IID_IDXGIADAPTER3: Guid = Guid(0x645967a4, 0x1392, 0x4310, [0xa7, 0x98, 0x80, 0x53, 0xce, 0x3e, 0x93, 0xfd]);

/// Headroom left for DWM, the PeerParty compositor, and shared frame textures (85% to copies).
const USABLE_PCT: u64 = 85;
/// Safety floor (512 MiB) so tiny GPUs or 4-way splits on small cards never report near-zero.
const MIN_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// `DXGI_MEMORY_SEGMENT_GROUP_LOCAL`.
const SEGMENT_LOCAL: u32 = 0;

/// `DXGI_QUERY_VIDEO_MEMORY_INFO`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoMemoryInfo {
    pub budget: u64,
    pub current_usage: u64,
    pub available_for_reservation: u64,
    pub current_reservation: u64,
}

/// `DXGI_ADAPTER_DESC1` (ABI-exact on x86 and x64).
#[repr(C)]
struct AdapterDesc1 {
    description: [u16; 128],
    vendor_id: u32,
    device_id: u32,
    sub_sys_id: u32,
    revision: u32,
    dedicated_video_memory: usize,
    dedicated_system_memory: usize,
    shared_system_memory: usize,
    luid_low: u32,
    luid_high: i32,
    flags: u32,
}

/// `VkBaseOutStructure` header for walking Vulkan `pNext` chains.
#[repr(C)]
struct VkBaseOut {
    s_type: u32,
    p_next: *mut VkBaseOut,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkMemoryType {
    pub property_flags: u32,
    pub heap_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkMemoryHeap {
    pub size: u64,
    pub flags: u32,
}

#[repr(C)]
pub struct VkMemoryProperties {
    pub memory_type_count: u32,
    pub memory_types: [VkMemoryType; 32],
    pub memory_heap_count: u32,
    pub memory_heaps: [VkMemoryHeap; 16],
}

/// `VkPhysicalDeviceMemoryProperties2`.
#[repr(C)]
pub struct VkMemoryProperties2 {
    pub s_type: u32,
    pub p_next: *mut u8,
    pub memory_properties: VkMemoryProperties,
}

/// `VkPhysicalDeviceMemoryBudgetPropertiesEXT` (`sType = 1000237000`).
#[repr(C)]
pub struct VkMemoryBudgetProperties {
    pub s_type: u32,
    pub p_next: *mut u8,
    pub heap_budget: [u64; 16],
    pub heap_usage: [u64; 16],
}

const VK_STRUCTURE_TYPE_MEMORY_BUDGET: u32 = 1000237000;
const VK_MEMORY_HEAP_DEVICE_LOCAL_BIT: u32 = 1;

type QueryVideoMemoryInfoFn = unsafe extern "system" fn(usize, u32, u32, *mut VideoMemoryInfo) -> i32;

static QUERY_VIDEO_MEM: OnceLock<GenericDetour<QueryVideoMemoryInfoFn>> = OnceLock::new();
static ADAPTER_VRAM: LazyLock<Mutex<HashMap<usize, u64>>> = LazyLock::new(Mutex::default);
static LOGGED_DXGI: AtomicBool = AtomicBool::new(false);
static LOGGED_VK: AtomicBool = AtomicBool::new(false);

/// Fair-share divisor (`2..=16`) when `PEERPARTY_VRAM_SHARE` is enabled, else None.
pub fn share() -> Option<u32> {
    crate::config::HookConfig::global().vram_share.filter(|&n| n >= 2)
}

/// Computes the fair-share VRAM budget cap for one copy out of `share`, never exceeding
/// `real_budget` and never double-dividing when the OS already reduced `real_budget`.
pub fn fair_budget(physical_bytes: u64, real_budget: u64, share: u32) -> u64 {
    if share <= 1 || physical_bytes == 0 || real_budget == 0 {
        return real_budget;
    }
    let usable = physical_bytes.saturating_mul(USABLE_PCT) / 100;
    let floor = physical_bytes.min(MIN_BUDGET_BYTES);
    let fair_cap = (usable / u64::from(share)).max(floor);
    real_budget.min(fair_cap)
}

/// Hooks `IDXGIAdapter3::QueryVideoMemoryInfo` (slot 14) once `dxgi.dll` is loaded, when
/// fair-share VRAM budgeting is requested.
pub fn install_dxgi() -> bool {
    let Some(n) = share() else { return false };
    if QUERY_VIDEO_MEM.get().is_some() {
        return true;
    }
    let dxgi = com::module("dxgi.dll");
    if dxgi.is_null() {
        return false;
    }
    let Some(create_ptr) = com::proc(dxgi, c"CreateDXGIFactory1") else { return false };
    type CreateFactory1 = unsafe extern "system" fn(*const Guid, *mut usize) -> i32;
    let create: CreateFactory1 = unsafe { std::mem::transmute(create_ptr) };
    let mut factory = 0usize;
    if unsafe { create(&IID_IDXGIFACTORY1, &mut factory) } < 0 || factory == 0 {
        return false;
    }
    let mut adapter = 0usize;
    // IDXGIFactory1::EnumAdapters1 is slot 12.
    let hr = vcall!(factory, 12, fn(u32, *mut usize) -> i32, 0, &mut adapter);
    release(factory);
    if hr < 0 || adapter == 0 {
        return false;
    }
    let a3 = com::query(adapter, &IID_IDXGIADAPTER3);
    release(adapter);
    if a3 == 0 {
        return false;
    }
    let addr = com::slot(a3, 14);
    release(a3);
    if addr == 0 {
        return false;
    }
    let target: QueryVideoMemoryInfoFn = unsafe { std::mem::transmute(addr) };
    let armed = crate::sys::detour::try_arm(
        &QUERY_VIDEO_MEM,
        target,
        query_video_memory_info_hook as QueryVideoMemoryInfoFn,
        "video",
        "IDXGIAdapter3::QueryVideoMemoryInfo",
        true,
    );
    if armed == crate::sys::detour::Armed::Hooked {
        log!("video: DXGI VRAM budget sharing active (fair share for {n} copies)");
        true
    } else {
        QUERY_VIDEO_MEM.get().is_some()
    }
}

fn dedicated_vram_of(adapter: usize) -> u64 {
    if let Some(&bytes) = crate::sys::lock(&ADAPTER_VRAM).get(&adapter) {
        return bytes;
    }
    let mut desc: AdapterDesc1 = unsafe { std::mem::zeroed() };
    // IDXGIAdapter1::GetDesc1 is slot 10.
    let bytes = if vcall!(adapter, 10, fn(*mut AdapterDesc1) -> i32, &mut desc) >= 0 {
        desc.dedicated_video_memory as u64
    } else {
        0
    };
    let mut cache = crate::sys::lock(&ADAPTER_VRAM);
    if cache.len() >= 16 {
        cache.clear();
    }
    cache.insert(adapter, bytes);
    bytes
}

unsafe extern "system" fn query_video_memory_info_hook(
    adapter: usize,
    node: u32,
    segment_group: u32,
    info: *mut VideoMemoryInfo,
) -> i32 {
    let hr = unsafe { QUERY_VIDEO_MEM.get().unwrap().call(adapter, node, segment_group, info) };
    if hr < 0 || segment_group != SEGMENT_LOCAL || info.is_null() {
        return hr;
    }
    let Some(n) = share() else { return hr };
    let _ = std::panic::catch_unwind(|| {
        let dedicated = dedicated_vram_of(adapter);
        let mem = unsafe { &mut *info };
        let before = mem.budget;
        let capped = fair_budget(dedicated, before, n);
        if capped < before {
            mem.budget = capped;
            mem.available_for_reservation = mem.available_for_reservation.min(capped / 2);
            if !LOGGED_DXGI.swap(true, Ordering::Relaxed) {
                log!(
                    "video: DXGI local VRAM budget {} MiB -> {} MiB (fair share for {n} copies of {} MiB GPU)",
                    before / (1024 * 1024),
                    capped / (1024 * 1024),
                    dedicated / (1024 * 1024)
                );
            }
        }
    });
    hr
}

/// Walks a `VkPhysicalDeviceMemoryProperties2`'s `pNext` chain and clamps any
/// `VkPhysicalDeviceMemoryBudgetPropertiesEXT` device-local heap budgets to this copy's
/// fair share (`PEERPARTY_VRAM_SHARE`).
pub fn clamp_vulkan_budget(props: *mut VkMemoryProperties2) {
    let Some(n) = share() else { return };
    if props.is_null() {
        return;
    }
    unsafe {
        let mem = &(*props).memory_properties;
        let heap_count = (mem.memory_heap_count as usize).min(16);
        let mut cur = (*props).p_next as *mut VkBaseOut;
        // Bound chain length so a malformed or cyclic pNext can never loop forever.
        for _ in 0..64 {
            if cur.is_null() {
                break;
            }
            if (*cur).s_type == VK_STRUCTURE_TYPE_MEMORY_BUDGET {
                let budget = &mut *(cur as *mut VkMemoryBudgetProperties);
                for i in 0..heap_count {
                    let heap = mem.memory_heaps[i];
                    if heap.flags & VK_MEMORY_HEAP_DEVICE_LOCAL_BIT == 0 {
                        continue;
                    }
                    let before = budget.heap_budget[i];
                    let capped = fair_budget(heap.size, before, n);
                    if capped < before {
                        budget.heap_budget[i] = capped;
                        if !LOGGED_VK.swap(true, Ordering::Relaxed) {
                            log!(
                                "video: Vulkan device-local VRAM budget {} MiB -> {} MiB (fair share for {n} copies of {} MiB heap)",
                                before / (1024 * 1024),
                                capped / (1024 * 1024),
                                heap.size / (1024 * 1024)
                            );
                        }
                    }
                }
            }
            cur = (*cur).p_next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    #[test]
    fn fair_budget_caps_without_double_dividing() {
        // 8 GiB GPU, 2 copies: fair cap = 85% * 8 GiB / 2 = 3.4 GiB.
        let cap2 = fair_budget(8 * GIB, 7 * GIB, 2);
        assert_eq!(cap2, (8 * GIB * 85 / 100) / 2);

        // If WDDM already lowered the process budget to 2.5 GiB, keep 2.5 GiB (never double-divide).
        assert_eq!(fair_budget(8 * GIB, 2560 * MIB, 2), 2560 * MIB);

        // 4 copies on 2 GiB GPU: 85% * 2 GiB / 4 = 435 MiB -> clamped up to 512 MiB floor.
        assert_eq!(fair_budget(2 * GIB, 1800 * MIB, 4), 512 * MIB);

        // Tiny GPU under 512 MiB: floor doesn't exceed physical size or real budget.
        assert_eq!(fair_budget(256 * MIB, 200 * MIB, 4), 200 * MIB);

        // Share 0 or 1 leaves real budget untouched.
        assert_eq!(fair_budget(8 * GIB, 7 * GIB, 1), 7 * GIB);
        assert_eq!(fair_budget(8 * GIB, 7 * GIB, 0), 7 * GIB);
    }

    #[test]
    fn vulkan_pnext_budget_clamping_only_touches_device_local_heaps() {
        let mut budget = VkMemoryBudgetProperties {
            s_type: VK_STRUCTURE_TYPE_MEMORY_BUDGET,
            p_next: std::ptr::null_mut(),
            heap_budget: [0; 16],
            heap_usage: [0; 16],
        };
        budget.heap_budget[0] = 7 * GIB;
        budget.heap_budget[1] = 12 * GIB;
        let mut props = VkMemoryProperties2 {
            s_type: 1000059006,
            p_next: &mut budget as *mut _ as *mut u8,
            memory_properties: VkMemoryProperties {
                memory_type_count: 0,
                memory_types: [VkMemoryType::default(); 32],
                memory_heap_count: 2,
                memory_heaps: [VkMemoryHeap::default(); 16],
            },
        };
        props.memory_properties.memory_heaps[0] = VkMemoryHeap { size: 8 * GIB, flags: VK_MEMORY_HEAP_DEVICE_LOCAL_BIT };
        props.memory_properties.memory_heaps[1] = VkMemoryHeap { size: 16 * GIB, flags: 0 };

        // Temporarily configure PEERPARTY_VRAM_SHARE=2.
        unsafe { std::env::set_var(peerparty_protocol::env::VRAM_SHARE, "2") };
        crate::config::reload();
        clamp_vulkan_budget(&mut props);
        unsafe { std::env::remove_var(peerparty_protocol::env::VRAM_SHARE) };
        crate::config::reload();

        assert_eq!(budget.heap_budget[0], (8 * GIB * 85 / 100) / 2, "device-local heap capped");
        assert_eq!(budget.heap_budget[1], 12 * GIB, "system RAM heap untouched");
    }
}
