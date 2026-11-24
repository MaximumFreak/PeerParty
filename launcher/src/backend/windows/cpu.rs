//! Topology-aware CPU core partitioning for split-screen copies (`GetSystemCpuSetInformation`
//! + `SetProcessDefaultCpuSets`).
//!
//! Giving each split-screen copy its own preferred CPU Sets keeps copies from evicting each
//! other's L2/L3 caches (especially across dual-CCD Ryzen dies) while still allowing Windows
//! to borrow idle cores whenever another copy is lightly loaded—unlike hard
//! `SetProcessAffinityMask`, which can starve a busy host copy.

use std::collections::BTreeMap;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

use super::wide;

/// Parsed entry from Win32 `SYSTEM_CPU_SET_INFORMATION` (`Type == 0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuSetEntry {
    pub id: u32,
    pub group: u16,
    pub logical_index: u8,
    pub core_index: u8,
    pub llc_index: u8,
    pub efficiency_class: u8,
}

/// Queries the system's CPU Sets via `kernel32!GetSystemCpuSetInformation` (Windows 10+).
/// Returns empty on older Windows or Wine where the API is absent.
pub fn query_cpu_sets() -> Vec<CpuSetEntry> {
    type QueryFn = unsafe extern "system" fn(*mut u8, u32, *mut u32, HANDLE, u32) -> i32;
    let Some(f) = kernel32_proc(c"GetSystemCpuSetInformation") else {
        return Vec::new();
    };
    let query: QueryFn = unsafe { std::mem::transmute(f) };
    let mut needed = 0u32;
    unsafe { query(std::ptr::null_mut(), 0, &mut needed, std::ptr::null_mut(), 0) };
    if needed < 20 || needed > 4 * 1024 * 1024 {
        return Vec::new();
    }
    let mut buf = vec![0u8; needed as usize];
    let ok = unsafe { query(buf.as_mut_ptr(), needed, &mut needed, std::ptr::null_mut(), 0) };
    if ok == 0 {
        return Vec::new();
    }
    let len = (needed as usize).min(buf.len());
    parse_cpu_sets(&buf[..len])
}

/// Parses a raw buffer filled by `GetSystemCpuSetInformation`.
fn parse_cpu_sets(buf: &[u8]) -> Vec<CpuSetEntry> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 20 <= buf.len() {
        let size = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if size < 20 || off + size > buf.len() {
            break;
        }
        let kind = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
        if kind == 0 {
            let id = u32::from_le_bytes(buf[off + 8..off + 12].try_into().unwrap());
            let group = u16::from_le_bytes(buf[off + 12..off + 14].try_into().unwrap());
            let logical_index = buf[off + 14];
            let core_index = buf[off + 15];
            let llc_index = buf[off + 16];
            let efficiency_class = buf[off + 18];
            out.push(CpuSetEntry { id, group, logical_index, core_index, llc_index, efficiency_class });
        }
        off += size;
    }
    out
}

/// Partitions `sets` into `instances` balanced groups of CPU Set IDs, keeping physical-core
/// SMT siblings together, separating L3 cache domains (multi-CCD CPUs) across copies when
/// present, and distributing P-cores / E-cores fairly across all copies on hybrid CPUs.
/// Returns empty when there aren't enough logical processors (`< instances * 2`) to split
/// meaningfully.
pub fn partition_cpu_sets(sets: &[CpuSetEntry], instances: usize) -> Vec<Vec<u32>> {
    if instances < 2 || sets.len() < instances * 2 {
        return Vec::new();
    }
    // Group logical processors into physical cores (keeping SMT siblings together), keyed by
    // (group, llc_index, efficiency_class, core_index).
    let mut cores_by_domain: BTreeMap<(u16, u8), BTreeMap<u8, Vec<Vec<u32>>>> = BTreeMap::new();
    let mut phys: BTreeMap<(u16, u8, u8, u8), Vec<(u8, u32)>> = BTreeMap::new();
    for s in sets {
        phys.entry((s.group, s.llc_index, s.efficiency_class, s.core_index)).or_default().push((s.logical_index, s.id));
    }
    for ((group, llc, eff, _core), mut threads) in phys {
        threads.sort_by_key(|(l, _)| *l);
        let ids: Vec<u32> = threads.into_iter().map(|(_, id)| id).collect();
        cores_by_domain.entry((group, llc)).or_default().entry(eff).or_default().push(ids);
    }

    let mut out = vec![Vec::<u32>::new(); instances];
    let domains: Vec<_> = cores_by_domain.into_values().collect();

    // Case 1: Multiple L3 cache domains (e.g. dual-CCD Ryzen) with enough domains to keep
    // copies inside their own L3 cache. Assign contiguous slices of copies to each domain.
    if domains.len() >= 2 && domains.len() <= instances {
        for (d_idx, eff_map) in domains.iter().enumerate() {
            let start = d_idx * instances / domains.len();
            let end = ((d_idx + 1) * instances / domains.len()).max(start + 1).min(instances);
            let slice_len = end - start;
            // Distribute each efficiency class round-robin across the copies assigned to this domain.
            for cores in eff_map.values().rev() {
                for (c_idx, core_threads) in cores.iter().enumerate() {
                    let target = start + (c_idx % slice_len);
                    out[target].extend_from_slice(core_threads);
                }
            }
        }
    } else {
        // Case 2: Single L3 cache (most desktop CPUs, including Intel hybrid P+E) or more
        // cache domains than copies: distribute each efficiency class's physical cores
        // fairly round-robin across all copies so every copy gets an equal mix of P & E cores.
        let mut by_eff: BTreeMap<u8, Vec<Vec<u32>>> = BTreeMap::new();
        for eff_map in domains {
            for (eff, cores) in eff_map {
                by_eff.entry(eff).or_default().extend(cores);
            }
        }
        for cores in by_eff.into_values().rev() {
            for (c_idx, core_threads) in cores.into_iter().enumerate() {
                out[c_idx % instances].extend(core_threads);
            }
        }
    }

    // If any slot ended up empty (e.g. degenerate topology), bail out rather than applying a broken set.
    if out.iter().any(Vec::is_empty) {
        return Vec::new();
    }
    out
}

/// Applies `ids` as soft preferred CPU Sets on `process` (`SetProcessDefaultCpuSets`).
pub fn apply_cpu_sets(process: HANDLE, ids: &[u32]) -> bool {
    if process.is_null() || ids.is_empty() {
        return false;
    }
    type SetFn = unsafe extern "system" fn(HANDLE, *const u32, u32) -> i32;
    let Some(f) = kernel32_proc(c"SetProcessDefaultCpuSets") else {
        return false;
    };
    let set: SetFn = unsafe { std::mem::transmute(f) };
    unsafe { set(process, ids.as_ptr(), ids.len() as u32) != 0 }
}

/// Logical core count reported to each copy (`PEERPARTY_CPU_CORES`) so engines don't spawn
/// `instances * total_logical` worker threads. Keeps at least 4 logical cores per copy, and
/// returns `None` when the machine is already at or below that floor.
pub fn recommended_worker_cores(total_logical: u32, instances: u32) -> Option<u32> {
    if instances < 2 || total_logical <= 4 {
        return None;
    }
    let per = (total_logical / instances).max(4);
    (per < total_logical).then_some(per)
}

fn kernel32_proc(name: &std::ffi::CStr) -> Option<usize> {
    let m = unsafe { LoadLibraryExW(wide("kernel32.dll").as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if m.is_null() {
        return None;
    }
    unsafe { GetProcAddress(m, name.as_ptr().cast()) }.map(|f| f as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32, logical: u8, core: u8, llc: u8, eff: u8) -> CpuSetEntry {
        CpuSetEntry { id, group: 0, logical_index: logical, core_index: core, llc_index: llc, efficiency_class: eff }
    }

    #[test]
    fn dual_ccd_separates_cache_domains_across_copies() {
        // 16-thread dual-CCD CPU (2 CCDs x 4 cores x 2 SMT threads).
        let mut sets = Vec::new();
        for ccd in 0..2u8 {
            for core in 0..4u8 {
                let c = ccd * 4 + core;
                for smt in 0..2u8 {
                    let l = c * 2 + smt;
                    sets.push(entry(256 + u32::from(l), l, c, ccd, 0));
                }
            }
        }
        let p2 = partition_cpu_sets(&sets, 2);
        assert_eq!(p2.len(), 2);
        assert_eq!(p2[0].len(), 8);
        assert_eq!(p2[1].len(), 8);
        assert!(p2[0].iter().all(|&id| id < 264), "copy 0 stays on CCD 0: {:?}", p2[0]);
        assert!(p2[1].iter().all(|&id| id >= 264), "copy 1 stays on CCD 1: {:?}", p2[1]);

        let p4 = partition_cpu_sets(&sets, 4);
        assert_eq!(p4.len(), 4);
        assert!(p4.iter().all(|g| g.len() == 4));
        assert!(p4[0].iter().chain(&p4[1]).all(|&id| id < 264), "copies 0-1 on CCD 0");
        assert!(p4[2].iter().chain(&p4[3]).all(|&id| id >= 264), "copies 2-3 on CCD 1");
    }

    #[test]
    fn hybrid_cpu_balances_p_and_e_cores_fairly() {
        // Intel-style 4 P-cores (8 threads, eff=1) + 4 E-cores (4 threads, eff=0) on 1 L3 cache.
        let mut sets = Vec::new();
        for core in 0..4u8 {
            for smt in 0..2u8 {
                let l = core * 2 + smt;
                sets.push(entry(100 + u32::from(l), l, core, 0, 1));
            }
        }
        for e in 0..4u8 {
            let l = 8 + e;
            sets.push(entry(200 + u32::from(e), l, 4 + e, 0, 0));
        }
        let p2 = partition_cpu_sets(&sets, 2);
        assert_eq!(p2.len(), 2);
        for slot in &p2 {
            let p_threads = slot.iter().filter(|&&id| id < 200).count();
            let e_threads = slot.iter().filter(|&&id| id >= 200).count();
            assert_eq!((p_threads, e_threads), (4, 2), "each copy gets 2 P-cores (4T) + 2 E-cores (2T)");
        }
    }

    #[test]
    fn small_cpu_skips_partitioning_and_worker_capping() {
        let two = vec![entry(1, 0, 0, 0, 0), entry(2, 1, 1, 0, 0)];
        assert!(partition_cpu_sets(&two, 2).is_empty());
        assert_eq!(recommended_worker_cores(4, 2), None);
        assert_eq!(recommended_worker_cores(8, 2), Some(4));
        assert_eq!(recommended_worker_cores(16, 2), Some(8));
        assert_eq!(recommended_worker_cores(16, 4), Some(4));
        assert_eq!(recommended_worker_cores(12, 4), Some(4));
        assert_eq!(recommended_worker_cores(16, 1), None);
    }

    #[test]
    fn queries_live_host_cpu_sets() {
        let sets = query_cpu_sets();
        // Every modern Windows 10/11 test runner has at least 1 CPU set entry.
        assert!(!sets.is_empty(), "GetSystemCpuSetInformation returned entries");
    }
}
