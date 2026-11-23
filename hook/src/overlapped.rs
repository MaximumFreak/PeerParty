//! Shared `GetOverlappedResult` / `GetOverlappedResultEx` hook dispatcher.
//!
//! Both `net::ipfwd` (overlapped Winsock receives) and `input::hid` (overlapped HID
//! device reads) need to observe completed overlapped I/O without importing each other.
//! Hooking the two kernelbase entry points once here keeps the dependency graph acyclic.

use std::sync::OnceLock;

use retour::GenericDetour;

use crate::log;
use crate::sys::hook::with_lock;
use crate::sys::win::{proc_addr, wide};

fn module(name: &str) -> Option<windows_sys::Win32::Foundation::HMODULE> {
    let m = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(wide(name).as_ptr()) };
    (!m.is_null()).then_some(m)
}

type GetResult = unsafe extern "system" fn(usize, *mut Overlapped, *mut u32, i32) -> i32;
type GetResultEx = unsafe extern "system" fn(usize, *mut Overlapped, *mut u32, u32, i32) -> i32;

#[repr(C)]
pub struct Overlapped {
    pub internal: usize,
    pub internal_high: usize,
    pub offset: u64,
    pub event: usize,
}

static RESULT: OnceLock<GenericDetour<GetResult>> = OnceLock::new();
static RESULT_EX: OnceLock<GenericDetour<GetResultEx>> = OnceLock::new();

/// Hooks `GetOverlappedResult` and `GetOverlappedResultEx` in `kernelbase.dll` once.
/// Returns the number of entry points newly hooked.
pub fn install() -> usize {
    let Some(kb) = module("kernelbase.dll") else { return 0 };
    let mut n = 0;
    if let Some(a) = proc_addr(kb, c"GetOverlappedResult") {
        n += arm(&RESULT, a, get_result as GetResult, "GetOverlappedResult");
    }
    if let Some(a) = proc_addr(kb, c"GetOverlappedResultEx") {
        n += arm(&RESULT_EX, a, get_result_ex as GetResultEx, "GetOverlappedResultEx");
    }
    n
}

fn arm<T: retour::Function + Copy>(cell: &OnceLock<GenericDetour<T>>, addr: usize, detour: T, label: &str) -> usize {
    with_lock(|| {
        if cell.get().is_some() {
            return 0;
        }
        let target: T = unsafe { T::from_ptr(addr as *const ()) };
        match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => {
                let _ = cell.set(h);
                usize::from(crate::sys::hook::frozen(addr, || unsafe { cell.get().unwrap().enable() }).is_ok())
            }
            Err(e) => {
                log!("overlapped {label} not hooked: {e}");
                0
            }
        }
    })
}

/// Hands a completed (or failed) result to its watchers, the call's last error kept: a
/// game reads it after a failure (ERROR_IO_INCOMPLETE, ERROR_OPERATION_ABORTED), and what
/// the watchers call may set another.
fn dispatch(h: usize, ol: *mut Overlapped, n: *mut u32, ok: bool) {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    let error = unsafe { GetLastError() };
    crate::net::ipfwd::overlapped_result(ol as usize, ok);
    crate::input::hid::on_overlapped_result(h, ol, n, ok);
    unsafe { SetLastError(error) };
}

unsafe extern "system" fn get_result(h: usize, ol: *mut Overlapped, n: *mut u32, wait: i32) -> i32 {
    let ok = unsafe { RESULT.get().unwrap().call(h, ol, n, wait) };
    dispatch(h, ol, n, ok != 0);
    ok
}

unsafe extern "system" fn get_result_ex(h: usize, ol: *mut Overlapped, n: *mut u32, ms: u32, alertable: i32) -> i32 {
    let ok = unsafe { RESULT_EX.get().unwrap().call(h, ol, n, ms, alertable) };
    dispatch(h, ol, n, ok != 0);
    ok
}
