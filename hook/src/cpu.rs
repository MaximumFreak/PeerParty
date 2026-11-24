//! Logical processor count scaling for split-screen copies (`PEERPARTY_CPU_CORES`).
//!
//! When N copies of a modern engine run on one PC, each queries the logical CPU count and
//! spawns a full worker-thread pool, causing N×cores busy-spinning threads to thrash L3
//! caches and spinlocks. Reporting a fair share (`max(cores / N, 4)`) from `GetSystemInfo`,
//! `GetNativeSystemInfo`, and `GetActiveProcessorCount` keeps thread pools right-sized
//! while Windows CPU Sets (set by the launcher) still allow spillover onto idle cores.
//!
//! Opt-in: stays completely uninstalled unless `PEERPARTY_CPU_CORES` asks for a cap lower
//! than the machine's logical processor count.

use std::ffi::{CStr, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use retour::GenericDetour;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

use crate::log;
use crate::sys::win::{proc_addr, wide};

/// Win32 `SYSTEM_INFO` (exact ABI layout on both x86 and x64).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SystemInfo {
    pub arch: u16,
    pub reserved: u16,
    pub page_size: u32,
    pub min_app_addr: *mut c_void,
    pub max_app_addr: *mut c_void,
    pub active_processor_mask: usize,
    pub number_of_processors: u32,
    pub processor_type: u32,
    pub allocation_granularity: u32,
    pub processor_level: u16,
    pub processor_revision: u16,
}

type GetSystemInfoFn = unsafe extern "system" fn(*mut SystemInfo);
type GetActiveProcessorCountFn = unsafe extern "system" fn(u16) -> u32;

const ALL_PROCESSOR_GROUPS: u16 = 0xFFFF;

static CAP: AtomicU32 = AtomicU32::new(0);
static LOGGED: AtomicBool = AtomicBool::new(false);

static GET_SYSTEM_INFO: OnceLock<GenericDetour<GetSystemInfoFn>> = OnceLock::new();
static GET_NATIVE_SYSTEM_INFO: OnceLock<GenericDetour<GetSystemInfoFn>> = OnceLock::new();
static GET_ACTIVE_PROCESSOR_COUNT: OnceLock<GenericDetour<GetActiveProcessorCountFn>> = OnceLock::new();

/// Arms CPU core-count hooks if `PEERPARTY_CPU_CORES` requests a cap smaller than the
/// machine's logical processor count.
pub fn init_from_env() {
    let Some(cap) = crate::config::HookConfig::global().cpu_cores.filter(|&c| c >= 1) else {
        return;
    };
    let mut real_info = SystemInfo::default();
    if let Some(addr) = resolve_kernel(c"GetSystemInfo") {
        let f: GetSystemInfoFn = unsafe { std::mem::transmute(addr) };
        unsafe { f(&mut real_info) };
    }
    if real_info.number_of_processors <= cap {
        return;
    }
    CAP.store(cap, Ordering::Release);
    let mut hooked = 0usize;
    if let Some(addr) = resolve_kernel(c"GetSystemInfo") {
        let target: GetSystemInfoFn = unsafe { std::mem::transmute(addr) };
        if crate::sys::detour::try_arm(&GET_SYSTEM_INFO, target, get_system_info_hook as GetSystemInfoFn, "cpu", "GetSystemInfo", true) == crate::sys::detour::Armed::Hooked {
            hooked += 1;
        }
    }
    if let Some(addr) = resolve_kernel(c"GetNativeSystemInfo") {
        let target: GetSystemInfoFn = unsafe { std::mem::transmute(addr) };
        if crate::sys::detour::try_arm(&GET_NATIVE_SYSTEM_INFO, target, get_native_system_info_hook as GetSystemInfoFn, "cpu", "GetNativeSystemInfo", true) == crate::sys::detour::Armed::Hooked {
            hooked += 1;
        }
    }
    if let Some(addr) = resolve_kernel(c"GetActiveProcessorCount") {
        let target: GetActiveProcessorCountFn = unsafe { std::mem::transmute(addr) };
        if crate::sys::detour::try_arm(&GET_ACTIVE_PROCESSOR_COUNT, target, get_active_processor_count_hook as GetActiveProcessorCountFn, "cpu", "GetActiveProcessorCount", true) == crate::sys::detour::Armed::Hooked {
            hooked += 1;
        }
    }
    if hooked > 0 {
        log!("cpu: reporting {cap} of {} logical cores to this split-screen copy ({hooked} hooks)", real_info.number_of_processors);
    }
}

fn resolve_kernel(name: &CStr) -> Option<usize> {
    for dll in ["kernelbase.dll", "kernel32.dll"] {
        let m = unsafe { GetModuleHandleW(wide(dll).as_ptr()) };
        if !m.is_null()
            && let Some(addr) = proc_addr(m, name)
        {
            return Some(addr);
        }
    }
    None
}

/// Keeps at most `cap` set bits of `mask` (lowest set bits first).
pub fn clamp_mask(mask: usize, cap: u32) -> usize {
    if cap == 0 || mask.count_ones() <= cap {
        return mask;
    }
    let mut out = 0usize;
    let mut rem = mask;
    let mut kept = 0u32;
    while rem != 0 && kept < cap {
        let lsb = rem & rem.wrapping_neg();
        out |= lsb;
        rem ^= lsb;
        kept += 1;
    }
    out
}

/// Clamps `info.number_of_processors` and `info.active_processor_mask` to `cap`.
pub fn clamp_system_info(info: &mut SystemInfo, cap: u32) {
    if cap == 0 || info.number_of_processors <= cap {
        return;
    }
    info.number_of_processors = cap;
    info.active_processor_mask = clamp_mask(info.active_processor_mask, cap);
}

/// Clamps `GetActiveProcessorCount` for group 0 or `ALL_PROCESSOR_GROUPS`.
pub fn clamp_processor_count(group: u16, count: u32, cap: u32) -> u32 {
    if cap == 0 || count == 0 || (group != 0 && group != ALL_PROCESSOR_GROUPS) {
        return count;
    }
    count.min(cap)
}

unsafe extern "system" fn get_system_info_hook(info: *mut SystemInfo) {
    unsafe { GET_SYSTEM_INFO.get().unwrap().call(info) };
    let cap = CAP.load(Ordering::Acquire);
    if !info.is_null() && cap > 0 {
        let before = unsafe { (*info).number_of_processors };
        clamp_system_info(unsafe { &mut *info }, cap);
        if before > cap && !LOGGED.swap(true, Ordering::Relaxed) {
            log!("cpu: clamped reported processor count {before} -> {cap}");
        }
    }
}

unsafe extern "system" fn get_native_system_info_hook(info: *mut SystemInfo) {
    unsafe { GET_NATIVE_SYSTEM_INFO.get().unwrap().call(info) };
    let cap = CAP.load(Ordering::Acquire);
    if !info.is_null() && cap > 0 {
        clamp_system_info(unsafe { &mut *info }, cap);
    }
}

unsafe extern "system" fn get_active_processor_count_hook(group: u16) -> u32 {
    let real = unsafe { GET_ACTIVE_PROCESSOR_COUNT.get().unwrap().call(group) };
    let cap = CAP.load(Ordering::Acquire);
    clamp_processor_count(group, real, cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_system_info_and_processor_counts() {
        let mut info = SystemInfo { number_of_processors: 16, active_processor_mask: 0xFFFF, ..Default::default() };
        clamp_system_info(&mut info, 8);
        assert_eq!(info.number_of_processors, 8);
        assert_eq!(info.active_processor_mask, 0x00FF);

        // Non-contiguous affinity mask keeps the lowest `cap` set bits.
        assert_eq!(clamp_mask(0b1010_1010, 2), 0b0000_1010);
        assert_eq!(clamp_mask(0b1010, 4), 0b1010);

        // Never inflates a smaller machine.
        let mut small = SystemInfo { number_of_processors: 4, active_processor_mask: 0xF, ..Default::default() };
        clamp_system_info(&mut small, 8);
        assert_eq!(small.number_of_processors, 4);
        assert_eq!(small.active_processor_mask, 0xF);

        assert_eq!(clamp_processor_count(0, 16, 6), 6);
        assert_eq!(clamp_processor_count(ALL_PROCESSOR_GROUPS, 16, 6), 6);
        assert_eq!(clamp_processor_count(1, 16, 6), 16, "other groups untouched");
        assert_eq!(clamp_processor_count(0, 4, 6), 4, "never inflates");
        assert_eq!(clamp_processor_count(0, 16, 0), 16, "0 disables");
    }
}
