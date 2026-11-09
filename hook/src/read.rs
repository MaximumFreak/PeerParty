//! Safe reads of argument pointers. A bad pointer must skip the detail, not crash the game.

use std::mem;

use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
    VirtualQuery,
};

fn region_end(p: usize) -> Option<usize> {
    if p < 0x1_0000 {
        return None;
    }
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { mem::zeroed() };
    let n = unsafe {
        VirtualQuery(
            p as *const _,
            &mut info,
            mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if n == 0 || info.State != MEM_COMMIT || info.Protect & PAGE_GUARD != 0 {
        return None;
    }
    let prot = info.Protect & 0xff;
    let readable = matches!(
        prot,
        PAGE_READONLY
            | PAGE_READWRITE
            | PAGE_WRITECOPY
            | PAGE_EXECUTE_READ
            | PAGE_EXECUTE_READWRITE
            | PAGE_EXECUTE_WRITECOPY
    );
    if !readable {
        return None;
    }
    Some(info.BaseAddress as usize + info.RegionSize)
}

/// Whether `len` bytes from `p` can be read.
pub fn readable(p: usize, len: usize) -> bool {
    region_end(p).is_some_and(|end| p.checked_add(len).is_some_and(|last| last <= end))
}

/// ASCII string, or `None` when `p` isn't a readable C string.
pub fn cstr(p: usize) -> Option<String> {
    let end = region_end(p)?;
    let mut out = String::new();
    let mut cur = p;
    while cur < end && out.len() < 512 {
        let b = unsafe { (cur as *const u8).read() };
        if b == 0 {
            return Some(out);
        }
        if !(0x20..0x7f).contains(&b) {
            return None;
        }
        out.push(b as char);
        cur += 1;
    }
    None
}

pub fn wstr(p: usize) -> Option<String> {
    let end = region_end(p)?;
    let mut units = Vec::new();
    let mut cur = p;
    while cur + 1 < end && units.len() < 512 {
        let u = unsafe { (cur as *const u16).read() };
        if u == 0 {
            return Some(String::from_utf16_lossy(&units));
        }
        units.push(u);
        cur += 2;
    }
    None
}

/// `SteamUser023\0SteamFriends017\0\0`, the blob passed to `SteamInternal_SteamAPI_Init`.
/// Stops at the extra NUL, so adjacent unrelated strings in `.rdata` stay out.
pub fn steam_version_blob(p: usize) -> Option<String> {
    let mut parts = Vec::new();
    let mut cur = p;
    let mut budget = 4096usize;
    for _ in 0..64 {
        let s = cstr(cur)?;
        if s.is_empty() || !s.starts_with("Steam") || s.len() + 1 > budget {
            break;
        }
        budget -= s.len() + 1;
        cur += s.len() + 1;
        parts.push(s);
        let end = region_end(cur)?;
        if cur >= end || unsafe { (cur as *const u8).read() } == 0 {
            break;
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

pub fn ipv4(b: &[u8]) -> String {
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

pub fn ipv6(b: &[u8]) -> String {
    (0..8)
        .map(|i| format!("{:x}", u16::from_be_bytes([b[i * 2], b[i * 2 + 1]])))
        .collect::<Vec<_>>()
        .join(":")
}

const PTR: usize = mem::size_of::<usize>();
/// `ai_addr` in both `addrinfo` and `ADDRINFOEXW`: four ints, `ai_addrlen` (size_t),
/// `ai_canonname`, then `ai_addr`.
const AI_ADDR: usize = 16 + 2 * PTR;
/// `addrinfo.ai_next`, right after `ai_addr`.
pub const ADDRINFO_NEXT: usize = AI_ADDR + PTR;
/// `ADDRINFOEXW.ai_next`, after `ai_addr`, `ai_blob`, `ai_bloblen` and `ai_provider`.
pub const ADDRINFOEX_NEXT: usize = AI_ADDR + 4 * PTR;

/// First few addresses from an `addrinfo` / `ADDRINFOEX` chain.
pub fn addrinfos(head: *const u8, next_offset: usize) -> String {
    let mut out = Vec::new();
    let mut cur = head;
    while !cur.is_null() && out.len() < 4 {
        let Some(end) = region_end(cur as usize) else {
            break;
        };
        if (cur as usize) + next_offset + PTR > end {
            break;
        }
        let addr = unsafe { cur.add(AI_ADDR).cast::<*const u8>().read() };
        let next = unsafe { cur.add(next_offset).cast::<*const u8>().read() };
        if let Some(s) = sockaddr(addr) {
            out.push(s);
        }
        if next == cur {
            break;
        }
        cur = next;
    }
    out.join(",")
}

fn sockaddr(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let end = region_end(p as usize)?;
    if (p as usize) + 8 > end {
        return None;
    }
    match unsafe { p.cast::<u16>().read() } {
        2 => {
            let b = unsafe { std::slice::from_raw_parts(p.add(4), 4) };
            Some(ipv4(b))
        }
        23 => {
            if (p as usize) + 24 > end {
                return None;
            }
            let b = unsafe { std::slice::from_raw_parts(p.add(8), 16) };
            Some(ipv6(b))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn reads_cstr_and_stops_at_non_ascii() {
        let s = CString::new("SteamMatchMaking009").unwrap();
        assert_eq!(
            cstr(s.as_ptr() as usize).as_deref(),
            Some("SteamMatchMaking009")
        );
        assert_eq!(cstr(0), None);
        assert_eq!(cstr(1), None);
    }

    #[test]
    fn version_blob_stops_at_the_extra_nul() {
        let mut blob = b"SteamUser023\0SteamFriends017\0\0not a version".to_vec();
        let got = steam_version_blob(blob.as_mut_ptr() as usize).unwrap();
        assert_eq!(got, "SteamUser023 SteamFriends017");
    }

    #[test]
    fn formats_sockaddr_chain() {
        #[repr(C)]
        struct Addr {
            family: u16,
            port: u16,
            ip: [u8; 4],
        }
        #[repr(C)]
        struct Info {
            pad: [u8; AI_ADDR],
            addr: *const Addr,
            next: *const Info,
        }
        let sa = Addr {
            family: 2,
            port: 0,
            ip: [1, 2, 3, 4],
        };
        let info = Info {
            pad: [0; AI_ADDR],
            addr: &sa,
            next: std::ptr::null(),
        };
        assert_eq!(std::mem::offset_of!(Info, next), ADDRINFO_NEXT);
        assert_eq!(
            addrinfos((&info as *const Info).cast(), ADDRINFO_NEXT),
            "1.2.3.4"
        );
        assert_eq!(
            ipv6(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            "2001:db8:0:0:0:0:0:1"
        );
    }
}
