//! Small Win32 helpers the layers share: wide strings, which module an address is in, an
//! export by name, and what memory may be read or written.

use std::ffi::CStr;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY,
    PAGE_READWRITE, PAGE_WRITECOPY, VirtualQuery,
};

/// `s` as a NUL-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The base of the module that holds `addr` (0 when none does). The module's reference
/// count is left alone.
pub fn module_of(addr: usize) -> usize {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW,
    };
    let mut m = std::ptr::null_mut();
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    if unsafe { GetModuleHandleExW(flags, addr as *const u16, &mut m) } == 0 {
        return 0;
    }
    m as usize
}

/// An export of a loaded module, by name.
pub fn proc_addr(m: HMODULE, name: &CStr) -> Option<usize> {
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    if m.is_null() {
        return None;
    }
    unsafe { GetProcAddress(m, name.as_ptr().cast()) }.map(|f| f as usize)
}

/// The committed, unguarded region `p` is in (None below the first 64 KiB, which is
/// never mapped, or for memory that isn't there).
pub fn query(p: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    if p < 0x1_0000 {
        return None;
    }
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
    (n != 0 && info.State == MEM_COMMIT && info.Protect & PAGE_GUARD == 0).then_some(info)
}

fn can_read(protect: u32) -> bool {
    matches!(protect & 0xff, PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)
}

fn can_write(protect: u32) -> bool {
    matches!(protect & 0xff, PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)
}

/// Where the readable region `p` is in ends.
pub fn readable_end(p: usize) -> Option<usize> {
    let info = query(p).filter(|i| can_read(i.Protect))?;
    Some(info.BaseAddress as usize + info.RegionSize)
}

/// Whether every region of `[p, p + len)` allows `ok`.
fn span_allows(p: usize, len: usize, ok: fn(u32) -> bool) -> bool {
    let Some(end) = p.checked_add(len) else { return false };
    let mut at = p;
    loop {
        let Some(info) = query(at).filter(|i| ok(i.Protect)) else { return false };
        at = info.BaseAddress as usize + info.RegionSize;
        if at >= end || len == 0 {
            return true;
        }
    }
}

/// Whether `len` bytes from `p` can be read (across regions).
pub fn readable(p: usize, len: usize) -> bool {
    span_allows(p, len, can_read)
}

/// Whether `len` bytes from `p` can be written (across regions).
pub fn writable(p: usize, len: usize) -> bool {
    span_allows(p, len, can_write)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_queries_tell_good_from_bad() {
        let v = vec![0u8; 3 * 4096];
        let p = v.as_ptr() as usize;
        assert!(readable(p, v.len()) && writable(p, v.len()));
        assert!(readable(p, 0));
        assert!(!readable(0, 1) && !readable(0x1000, 1));
        assert!(!readable(usize::MAX - 4, 8), "a span past the end of memory");
        let code = wide as fn(&str) -> Vec<u16> as usize;
        assert!(readable(code, 16) && !writable(code, 16), "code reads, doesn't write");
        assert!(readable_end(p).is_some_and(|e| e >= p + v.len()));
    }

    #[test]
    fn modules_and_exports_are_found() {
        let code = wide as fn(&str) -> Vec<u16> as usize;
        assert_ne!(module_of(code), 0);
        assert_eq!(module_of(0x1000), 0);
        let k32 = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
        assert!(proc_addr(k32, c"GetCurrentProcessId").is_some());
        assert!(proc_addr(k32, c"NoSuchExport").is_none());
        assert!(proc_addr(std::ptr::null_mut(), c"GetCurrentProcessId").is_none());
        assert_eq!(wide("ab"), [97, 98, 0]);
    }
}
