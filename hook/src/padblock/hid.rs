//! HID devices read directly (SDL's HIDAPI, the way SDL games read PlayStation and
//! Switch pads): while the overlay shows, each read from a HID device hands back the
//! first report read after the overlay opened, over and over. Pads' report formats differ
//! (and some carry a checksum), so repeating a real report is the one way to show any of
//! them standing still.
//!
//! `ReadFile` and `GetOverlappedResult(Ex)` in kernelbase are hooked when the overlay
//! first opens. While it's closed each call costs one flag check; while it shows, a
//! handle's first read asks whether it's a HID device (once per handle per opening).

use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::{module, proc_addr};
use crate::hook::with_lock;
use crate::log;
use crate::overlay::pads_held_back;

type ReadFile = unsafe extern "system" fn(usize, *mut u8, u32, *mut u32, *mut Overlapped) -> i32;
type GetResult = unsafe extern "system" fn(usize, *mut Overlapped, *mut u32, i32) -> i32;
type GetResultEx = unsafe extern "system" fn(usize, *mut Overlapped, *mut u32, u32, i32) -> i32;

#[repr(C)]
pub struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u64,
    event: usize,
}

static READ: OnceLock<GenericDetour<ReadFile>> = OnceLock::new();
static RESULT: OnceLock<GenericDetour<GetResult>> = OnceLock::new();
static RESULT_EX: OnceLock<GenericDetour<GetResultEx>> = OnceLock::new();

/// Per overlay opening: which handles are HID devices, each one's held report, and reads
/// still in flight (overlapped, handle, buffer).
static KNOWN: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());
static HELD: Mutex<Vec<(usize, Vec<u8>)>> = Mutex::new(Vec::new());
static PENDING: Mutex<Vec<(usize, usize, usize)>> = Mutex::new(Vec::new());

const ERROR_IO_PENDING: u32 = 997;

pub fn install() -> usize {
    let Some(kb) = module("kernelbase.dll") else { return 0 };
    let mut n = 0;
    if let Some(a) = proc_addr(kb, c"ReadFile".as_ptr().cast()) {
        n += arm(&READ, a, read_file as ReadFile, "ReadFile");
    }
    n + install_results()
}

/// GetOverlappedResult(Ex) only: also what ipfwd learns overlapped receives finish from
/// (each detour tells it first). Hooked once, by whichever asks first.
pub fn install_results() -> usize {
    let Some(kb) = module("kernelbase.dll") else { return 0 };
    let mut n = 0;
    if let Some(a) = proc_addr(kb, c"GetOverlappedResult".as_ptr().cast()) {
        n += arm(&RESULT, a, get_result as GetResult, "GetOverlappedResult");
    }
    if let Some(a) = proc_addr(kb, c"GetOverlappedResultEx".as_ptr().cast()) {
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
                usize::from(crate::hook::frozen(addr, || unsafe { cell.get().unwrap().enable() }).is_ok())
            }
            Err(e) => {
                log!("padblock {label} not hooked: {e}");
                0
            }
        }
    })
}

/// The overlay closed: everything learnt for this opening goes.
pub fn hidden() {
    KNOWN.lock().unwrap().clear();
    HELD.lock().unwrap().clear();
    PENDING.lock().unwrap().clear();
}

/// Whether `h` is a HID device: not a file, pipe or console, and hid.dll answers for it.
fn is_hid(h: usize) -> bool {
    if let Some(&(_, hid)) = KNOWN.lock().unwrap().iter().find(|(k, _)| *k == h) {
        return hid;
    }
    let hid = unsafe { windows_sys::Win32::Storage::FileSystem::GetFileType(h as _) } == 0
        && module("hid.dll").and_then(|m| proc_addr(m, c"HidD_GetAttributes".as_ptr().cast())).is_some_and(|f| {
            let get: unsafe extern "system" fn(usize, *mut [u32; 3]) -> u8 = unsafe { std::mem::transmute(f) };
            // HIDD_ATTRIBUTES: Size, VendorID / ProductID, VersionNumber.
            let mut attrs = [12u32, 0, 0];
            unsafe { get(h, &mut attrs) != 0 }
        });
    let mut known = KNOWN.lock().unwrap();
    if known.len() < 256 {
        known.push((h, hid));
    }
    if hid {
        log!("padblock: HID device read directly (handle {h:#x}); its reports hold still while the overlay shows");
    }
    hid
}

/// A read from a HID device finished with `n` bytes in `buf`: the first is kept, later
/// ones are replaced with it.
fn hold(h: usize, buf: *mut u8, n: u32) {
    if buf.is_null() || n == 0 {
        return;
    }
    let mut held = HELD.lock().unwrap();
    match held.iter().find(|(k, _)| *k == h) {
        Some((_, r)) if r.len() == n as usize => unsafe { std::ptr::copy_nonoverlapping(r.as_ptr(), buf, r.len()) },
        Some(_) => {}
        None => held.push((h, unsafe { std::slice::from_raw_parts(buf, n as usize) }.to_vec())),
    }
}

fn last_error() -> u32 {
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

fn set_last_error(e: u32) {
    unsafe { windows_sys::Win32::Foundation::SetLastError(e) }
}

unsafe extern "system" fn read_file(h: usize, buf: *mut u8, len: u32, read: *mut u32, ol: *mut Overlapped) -> i32 {
    let real = READ.get().unwrap();
    let ok = unsafe { real.call(h, buf, len, read, ol) };
    if !pads_held_back() {
        return ok;
    }
    let err = last_error();
    if is_hid(h) {
        if ok != 0 {
            let n = if !read.is_null() {
                unsafe { *read }
            } else if !ol.is_null() {
                unsafe { (*ol).internal_high as u32 }
            } else {
                0
            };
            hold(h, buf, n);
        } else if err == ERROR_IO_PENDING && !ol.is_null() {
            let mut p = PENDING.lock().unwrap();
            p.retain(|(o, _, _)| *o != ol as usize);
            if p.len() < 64 {
                p.push((ol as usize, h, buf as usize));
            }
        }
    }
    set_last_error(err);
    ok
}

/// An overlapped read finished: hold its report if it was a HID read made while the
/// overlay showed.
fn finished(h: usize, ol: *mut Overlapped, n: *mut u32) {
    let entry = {
        let mut p = PENDING.lock().unwrap();
        let i = p.iter().position(|(o, k, _)| *o == ol as usize && *k == h);
        i.map(|i| p.swap_remove(i))
    };
    if let (Some((_, _, buf)), false) = (entry, n.is_null()) {
        hold(h, buf as *mut u8, unsafe { *n });
    }
}

unsafe extern "system" fn get_result(h: usize, ol: *mut Overlapped, n: *mut u32, wait: i32) -> i32 {
    let ok = unsafe { RESULT.get().unwrap().call(h, ol, n, wait) };
    crate::ipfwd::overlapped_result(ol as usize, ok != 0);
    if ok != 0 && pads_held_back() {
        let err = last_error();
        finished(h, ol, n);
        set_last_error(err);
    }
    ok
}

unsafe extern "system" fn get_result_ex(h: usize, ol: *mut Overlapped, n: *mut u32, ms: u32, alertable: i32) -> i32 {
    let ok = unsafe { RESULT_EX.get().unwrap().call(h, ol, n, ms, alertable) };
    crate::ipfwd::overlapped_result(ol as usize, ok != 0);
    if ok != 0 && pads_held_back() {
        let err = last_error();
        finished(h, ol, n);
        set_last_error(err);
    }
    ok
}
