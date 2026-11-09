//! Injecting a 32-bit (WOW64) game from this x64 launcher.
//!
//! 1. Patch the entry point with `jmp $` (EB FE) and resume the suspended process.
//! 2. Wait until the main thread spins on it. By then the loader has mapped every static
//!    import, the 32-bit kernel32 included, and the game hasn't run any code of its own.
//! 3. Find `LoadLibraryW` in the game's 32-bit kernel32 (its address differs from ours)
//!    and run it on a remote thread with the hook DLL's path.
//! 4. Put the entry bytes back. The caller resumes the main thread.

use std::ffi::c_void;
use std::path::Path;
use std::{mem, ptr, thread, time::Duration, time::Instant};

use windows_sys::Win32::Foundation::{HANDLE, HMODULE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, ReadProcessMemory, WOW64_CONTEXT, WOW64_CONTEXT_CONTROL,
    WOW64_CONTEXT_INTEGER, Wow64GetThreadContext, WriteProcessMemory,
};
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtectEx};
use windows_sys::Win32::System::ProcessStatus::{
    K32EnumProcessModulesEx, K32GetModuleBaseNameW, LIST_MODULES_32BIT,
};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, PROCESS_INFORMATION, ResumeThread, SuspendThread,
    WaitForSingleObject,
};

use crate::{os_error, wide_bytes, write_remote};

/// How long the loader may take to reach the entry point (antivirus scans included).
const LOADER_TIMEOUT: Duration = Duration::from_secs(60);

pub unsafe fn inject(pi: &PROCESS_INFORMATION, dll: &Path) -> Result<(), String> {
    unsafe {
        let h = pi.hProcess;
        // A new thread's ebx holds the PEB; PEB32.ImageBaseAddress is at +8.
        let mut ctx: WOW64_CONTEXT = mem::zeroed();
        ctx.ContextFlags = WOW64_CONTEXT_INTEGER;
        if Wow64GetThreadContext(pi.hThread, &mut ctx) == 0 {
            return Err(os_error("Wow64GetThreadContext"));
        }
        let base = read_u32(h, ctx.Ebx + 8)?;
        let lfanew = read_u32(h, base + 0x3c)?;
        // AddressOfEntryPoint: signature (4) + file header (20) + 16.
        let entry = base + read_u32(h, base + lfanew + 0x28)?;

        let saved = read_bytes::<2>(h, entry)?;
        let mut old = 0u32;
        if VirtualProtectEx(
            h,
            entry as *const c_void,
            2,
            PAGE_EXECUTE_READWRITE,
            &mut old,
        ) == 0
        {
            return Err(os_error("VirtualProtectEx(entry point)"));
        }
        write_code(h, entry, &[0xEB, 0xFE])?;

        let parked = park(pi, entry);
        // Whatever happened, the game must not keep spinning on our patch.
        let restored = write_code(h, entry, &saved);
        VirtualProtectEx(h, entry as *const c_void, 2, old, &mut old);
        parked?;
        restored?;

        let load_library = find_load_library(h)?;
        let path = write_remote(pi, &wide_bytes(dll.as_os_str()))?;
        let start: unsafe extern "system" fn(*mut c_void) -> u32 =
            mem::transmute(load_library as usize);
        let thread = CreateRemoteThread(
            h,
            ptr::null(),
            0,
            Some(start),
            path as *const c_void,
            0,
            ptr::null_mut(),
        );
        if thread.is_null() {
            return Err(os_error("CreateRemoteThread"));
        }
        let waited = WaitForSingleObject(thread, LOADER_TIMEOUT.as_millis() as u32);
        let mut module = 0u32;
        GetExitCodeThread(thread, &mut module);
        windows_sys::Win32::Foundation::CloseHandle(thread);
        if waited != WAIT_OBJECT_0 {
            return Err("LoadLibraryW in the game did not finish".into());
        }
        if module == 0 {
            return Err(format!("the game could not load {}", dll.display()));
        }
        Ok(())
    }
}

/// Resumes the main thread and suspends it again once it spins on the entry point.
unsafe fn park(pi: &PROCESS_INFORMATION, entry: u32) -> Result<(), String> {
    unsafe {
        let started = Instant::now();
        if ResumeThread(pi.hThread) == u32::MAX {
            return Err(os_error("ResumeThread"));
        }
        loop {
            thread::sleep(Duration::from_millis(2));
            if SuspendThread(pi.hThread) == u32::MAX {
                return Err(os_error("SuspendThread"));
            }
            let mut ctx: WOW64_CONTEXT = mem::zeroed();
            ctx.ContextFlags = WOW64_CONTEXT_CONTROL;
            if Wow64GetThreadContext(pi.hThread, &mut ctx) != 0 && ctx.Eip == entry {
                return Ok(());
            }
            if started.elapsed() > LOADER_TIMEOUT {
                return Err("the game never reached its entry point".into());
            }
            ResumeThread(pi.hThread);
        }
    }
}

/// `LoadLibraryW` in the game's 32-bit kernel32, or kernelbase if kernel32 forwards it.
unsafe fn find_load_library(h: HANDLE) -> Result<u32, String> {
    unsafe {
        let mut modules = vec![ptr::null_mut::<c_void>() as HMODULE; 1024];
        let mut needed = 0u32;
        if K32EnumProcessModulesEx(
            h,
            modules.as_mut_ptr(),
            (modules.len() * mem::size_of::<HMODULE>()) as u32,
            &mut needed,
            LIST_MODULES_32BIT,
        ) == 0
        {
            return Err(os_error("EnumProcessModulesEx"));
        }
        modules.truncate(needed as usize / mem::size_of::<HMODULE>());
        let base_of = |want: &str| {
            modules.iter().copied().find(|&m| {
                let mut buf = [0u16; 260];
                let n = K32GetModuleBaseNameW(h, m, buf.as_mut_ptr(), buf.len() as u32);
                String::from_utf16_lossy(&buf[..n as usize]).eq_ignore_ascii_case(want)
            })
        };
        for dll in ["kernel32.dll", "kernelbase.dll"] {
            if let Some(m) = base_of(dll) {
                if let Some(addr) = export32(h, m as usize as u32, "LoadLibraryW")? {
                    return Ok(addr);
                }
            }
        }
        Err("no 32-bit LoadLibraryW in the game".into())
    }
}

/// Address of a code export of a PE32 image mapped in the game, or `None` for a
/// forwarder or a missing name.
unsafe fn export32(h: HANDLE, base: u32, name: &str) -> Result<Option<u32>, String> {
    unsafe {
        let opt = base + read_u32(h, base + 0x3c)? + 24;
        if read_u16(h, opt)? != 0x10b {
            return Err("32-bit kernel32 isn't PE32".into());
        }
        // PE32 data directories start at +96; the export directory is the first.
        let export_rva = read_u32(h, opt + 96)?;
        let export_size = read_u32(h, opt + 100)?;
        let dir = base + export_rva;
        let n_names = read_u32(h, dir + 24)?;
        let funcs = base + read_u32(h, dir + 28)?;
        let names = base + read_u32(h, dir + 32)?;
        let ords = base + read_u32(h, dir + 36)?;
        // Export names are sorted, so binary search.
        let (mut lo, mut hi) = (0u32, n_names);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let here = read_cstr(h, base + read_u32(h, names + mid * 4)?)?;
            match here.as_str().cmp(name) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let ord = read_u16(h, ords + mid * 2)? as u32;
                    let rva = read_u32(h, funcs + ord * 4)?;
                    let forwarder = rva >= export_rva && rva < export_rva + export_size;
                    return Ok((!forwarder).then_some(base + rva));
                }
            }
        }
        Ok(None)
    }
}

unsafe fn read_bytes<const N: usize>(h: HANDLE, addr: u32) -> Result<[u8; N], String> {
    let mut buf = [0u8; N];
    let ok = unsafe {
        ReadProcessMemory(
            h,
            addr as usize as *const c_void,
            buf.as_mut_ptr() as *mut c_void,
            N,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(os_error(&format!("ReadProcessMemory({addr:#x})")));
    }
    Ok(buf)
}

unsafe fn read_u32(h: HANDLE, addr: u32) -> Result<u32, String> {
    unsafe { read_bytes::<4>(h, addr).map(u32::from_le_bytes) }
}

unsafe fn read_u16(h: HANDLE, addr: u32) -> Result<u16, String> {
    unsafe { read_bytes::<2>(h, addr).map(u16::from_le_bytes) }
}

unsafe fn read_cstr(h: HANDLE, addr: u32) -> Result<String, String> {
    let bytes = unsafe { read_bytes::<64>(h, addr)? };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

unsafe fn write_code(h: HANDLE, addr: u32, bytes: &[u8]) -> Result<(), String> {
    unsafe {
        let p = addr as usize as *const c_void;
        if WriteProcessMemory(
            h,
            p,
            bytes.as_ptr() as *const c_void,
            bytes.len(),
            ptr::null_mut(),
        ) == 0
        {
            return Err(os_error("WriteProcessMemory(entry point)"));
        }
        FlushInstructionCache(h, p, bytes.len());
        Ok(())
    }
}
