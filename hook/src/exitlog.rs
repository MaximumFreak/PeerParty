//! Why a game ended: a process exit with a non-zero code (`ExitProcess` and the C
//! runtime's `exit` / `abort` reach ntdll's `RtlExitUserProcess`; `TerminateProcess` on
//! itself goes through kernelbase) is logged with the code and the modules on the
//! caller's stack, so an `abort()` deep in a game (exit code 3, no crash dump) still
//! says where it came from.

use std::sync::OnceLock;

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;

type Terminate = unsafe extern "system" fn(usize, u32) -> i32;

static EXIT: OnceLock<GenericDetour<unsafe extern "system" fn(u32)>> = OnceLock::new();
static TERMINATE: OnceLock<GenericDetour<Terminate>> = OnceLock::new();

pub fn attach(kernelbase: &[crate::pe::Export]) -> usize {
    let mut n = 0;
    let ntdll: Vec<u16> = "ntdll.dll".encode_utf16().chain(Some(0)).collect();
    let m = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(ntdll.as_ptr()) };
    if let Some(f) = unsafe { windows_sys::Win32::System::LibraryLoader::GetProcAddress(m, c"RtlExitUserProcess".as_ptr().cast()) } {
        // It never returns; declared returning nothing so the detour can carry on to it.
        let target: unsafe extern "system" fn(u32) = unsafe { std::mem::transmute(f) };
        n += arm(&EXIT, target, exit_hook as unsafe extern "system" fn(u32));
    }
    if let Some(e) = kernelbase.iter().find(|e| e.name == "TerminateProcess") {
        let target: Terminate = unsafe { std::mem::transmute(e.addr) };
        n += arm(&TERMINATE, target, terminate_hook as Terminate);
    }
    n
}

fn arm<T: retour::Function + Copy>(cell: &OnceLock<GenericDetour<T>>, target: T, detour: T) -> usize {
    with_lock(|| match unsafe { GenericDetour::new(target, detour) } {
        Ok(h) => {
            let _ = cell.set(h);
            usize::from(unsafe { cell.get().unwrap().enable() }.is_ok())
        }
        Err(e) => {
            log!("exitlog: not hooked: {e}");
            0
        }
    })
}

/// "module+offset" for each return address on the caller's stack.
fn stack() -> String {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleExW,
    };
    let mut frames = [std::ptr::null_mut(); 40];
    let n = unsafe { windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace(2, frames.len() as u32, frames.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    let mut out = Vec::new();
    for &f in &frames[..n] {
        let mut m = std::ptr::null_mut();
        let ok = unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, f as *const u16, &mut m) };
        if ok == 0 {
            out.push(format!("{f:?}"));
            continue;
        }
        let mut buf = [0u16; 260];
        let len = unsafe { GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let path = String::from_utf16_lossy(&buf[..len]);
        let name = path.rsplit('\\').next().unwrap_or(&path).to_string();
        out.push(format!("{name}+{:#x}", f as usize - m as usize));
    }
    out.join(" < ")
}

unsafe extern "system" fn exit_hook(code: u32) {
    if code != 0 {
        log!("exit: the process exits with code {code} ({code:#x}); called from {}", stack());
    }
    crate::net::on_exit();
    unsafe { EXIT.get().unwrap().call(code) }
}

unsafe extern "system" fn terminate_hook(process: usize, code: u32) -> i32 {
    let me = process == usize::MAX || unsafe { windows_sys::Win32::System::Threading::GetProcessId(process as _) } == std::process::id();
    if me && code != 0 {
        log!("exit: the process terminates itself with code {code} ({code:#x}); called from {}", stack());
    }
    if me {
        crate::net::on_exit();
    }
    unsafe { TERMINATE.get().unwrap().call(process, code) }
}
