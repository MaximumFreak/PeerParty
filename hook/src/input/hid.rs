//! HID devices read directly (SDL's HIDAPI, the way SDL games read PlayStation and
//! Switch pads): while the overlay has the controllers (`Policy::BlockAll`), each read
//! from a HID device hands back the first report read after it opened, over and over;
//! when it opened with a button down (`overlay::hid_reports_fresh`), the first one read
//! once that button is up, the reports before it passing as they are (held with it down,
//! the game would read it held the whole time and press what it's on again and again).
//! In a split-screen copy (`Policy::Virtual`) its own controller (`device::is_own`) is
//! read as it is, and another player's game controller reads as unplugged
//! (ERROR_DEVICE_NOT_CONNECTED, at once): the game lets it go, and can't open it again
//! (`device::refuse_open`). It used to be held at its first report too, and a stick
//! that was off-centre then stayed that way in this copy for good.
//!
//! `ReadFile` and `ReadFileEx` are hooked here (and the writes: `WriteFile`,
//! `WriteFileEx`, HID's control codes); `GetOverlappedResult(Ex)` completions arrive
//! through the shared `crate::overlapped` dispatcher (`on_overlapped_result`), breaking
//! the old cycle between `net::ipfwd` and `padblock`. An overlapped read held still is
//! the device's own read, its report swapped for the held one when it completes: never
//! answered at once, or a loop reading until nothing is left (HIDAPI's) never ends. One
//! completed through a completion port isn't seen, and gets the device's own report.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::{Policy, device, module, policy, proc_addr};
use crate::log;
use crate::overlapped::Overlapped;
use crate::sys::hook::with_lock;

type ReadFile = unsafe extern "system" fn(usize, *mut u8, u32, *mut u32, *mut Overlapped) -> i32;

static READ: OnceLock<GenericDetour<ReadFile>> = OnceLock::new();

/// Which device handles are HID devices (`device::of_handle`), most recently used last;
/// files, pipes and the console are never kept (`GetFileType` tells them apart at once).
static KNOWN: Mutex<Vec<(usize, Option<device::Hid>)>> = Mutex::new(Vec::new());
/// How many device handles `KNOWN` keeps; the least recently used goes.
const KNOWN_MAX: usize = 128;
/// Handles of other players' controllers, unplugged for this copy (until one turns out
/// to be its own after all: `judge`).
static GONE: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static HELD: Mutex<Vec<(usize, Vec<u8>)>> = Mutex::new(Vec::new());
static PENDING: Mutex<Vec<(usize, usize, usize)>> = Mutex::new(Vec::new());
static OWN_READS: AtomicU32 = AtomicU32::new(0);
/// Reads answered with the held report while the overlay showed (`serve_held`).
static HELD_READS: AtomicU32 = AtomicU32::new(0);

/// How long a read answered with the held report takes: about a controller's report
/// interval, so a game reading in a loop doesn't spin.
const HELD_PACE: std::time::Duration = std::time::Duration::from_millis(4);

pub(super) fn take_held() -> u32 {
    HELD_READS.swap(0, Ordering::Relaxed)
}
static UNPLUGGED_READS: AtomicU32 = AtomicU32::new(0);

pub(super) fn take_counts() -> (u32, u32) {
    (OWN_READS.swap(0, Ordering::Relaxed), UNPLUGGED_READS.swap(0, Ordering::Relaxed))
}

const ERROR_IO_PENDING: u32 = 997;

pub fn install() -> usize {
    let Some(kb) = module("kernelbase.dll") else { return 0 };
    let mut n = 0;
    if let Some(a) = proc_addr(kb, c"ReadFile".as_ptr().cast()) {
        n += arm(&READ, a, read_file as ReadFile, "ReadFile");
    }
    if let Some(a) = proc_addr(kb, c"ReadFileEx".as_ptr().cast()) {
        n += arm(&READ_EX, a, read_file_ex as ReadFileEx, "ReadFileEx");
    }
    if let Some(a) = proc_addr(kb, c"WriteFile".as_ptr().cast()) {
        n += arm(&WRITE, a, write_file as WriteFile, "WriteFile");
    }
    if let Some(a) = proc_addr(kb, c"WriteFileEx".as_ptr().cast()) {
        n += arm(&WRITE_EX, a, write_file_ex as WriteFileEx, "WriteFileEx");
    }
    if let Some(a) = proc_addr(kb, c"DeviceIoControl".as_ptr().cast()) {
        n += arm(&IOCTL, a, device_io_control as DeviceIoControl, "DeviceIoControl");
    }
    n + crate::overlapped::install()
}

type WriteFile = unsafe extern "system" fn(usize, *const u8, u32, *mut u32, *mut Overlapped) -> i32;
type DeviceIoControl = unsafe extern "system" fn(usize, u32, *const u8, u32, *mut u8, u32, *mut u32, *mut Overlapped) -> i32;
/// `LPOVERLAPPED_COMPLETION_ROUTINE`: error, bytes, the read's OVERLAPPED.
type Completion = unsafe extern "system" fn(u32, u32, *mut Overlapped);
type ReadFileEx = unsafe extern "system" fn(usize, *mut u8, u32, *mut Overlapped, Option<Completion>) -> i32;
type WriteFileEx = unsafe extern "system" fn(usize, *const u8, u32, *mut Overlapped, Option<Completion>) -> i32;

static WRITE: OnceLock<GenericDetour<WriteFile>> = OnceLock::new();
static IOCTL: OnceLock<GenericDetour<DeviceIoControl>> = OnceLock::new();
static READ_EX: OnceLock<GenericDetour<ReadFileEx>> = OnceLock::new();
static WRITE_EX: OnceLock<GenericDetour<WriteFileEx>> = OnceLock::new();
/// `ReadFileEx` reads held still in flight: (overlapped, handle, buffer, the game's
/// completion routine).
static EX_PENDING: Mutex<Vec<(usize, usize, usize, usize)>> = Mutex::new(Vec::new());
/// Writes (output reports: rumble, lights, and over Bluetooth a DualSense's sound and
/// haptics) held back from other players' controllers.
static BLOCKED_WRITES: AtomicU32 = AtomicU32::new(0);

pub(super) fn take_blocked_writes() -> u32 {
    BLOCKED_WRITES.swap(0, Ordering::Relaxed)
}

/// Whether a write to `h` goes nowhere: a HID device this copy has unplugged (another
/// player's controller). Only handles already known as HID devices (read from, or seen
/// opened) are judged: every file write passes here, the log's own among them.
fn write_unplugged(h: usize) -> bool {
    if !super::shm::attached() {
        return false;
    }
    // Unplugged already: whatever else was forgotten since (`hidden`), it stays so.
    let known_hid = is_gone(h) || KNOWN.lock().unwrap().iter().any(|(k, d)| *k == h && d.is_some()) || device::opened_seen(h).is_some();
    if !known_hid {
        return false;
    }
    let gone = judge(h) == Read::Unplugged;
    if gone && BLOCKED_WRITES.fetch_add(1, Ordering::Relaxed) == 0 {
        log!("input: the game writes to another player's controller (handle {h:#x}: rumble, lights or sound): held back from it");
    }
    gone
}

unsafe extern "system" fn write_file(h: usize, buf: *const u8, len: u32, written: *mut u32, ol: *mut Overlapped) -> i32 {
    if write_unplugged(h) {
        if !written.is_null() {
            unsafe { *written = 0 };
        }
        set_last_error(ERROR_DEVICE_NOT_CONNECTED);
        return 0;
    }
    unsafe { WRITE.get().unwrap().call(h, buf, len, written, ol) }
}

unsafe extern "system" fn write_file_ex(h: usize, buf: *const u8, len: u32, ol: *mut Overlapped, done: Option<Completion>) -> i32 {
    if write_unplugged(h) {
        set_last_error(ERROR_DEVICE_NOT_CONNECTED);
        return 0;
    }
    unsafe { WRITE_EX.get().unwrap().call(h, buf, len, ol, done) }
}

/// Whether handle `h` was unplugged for this copy (another player's controller).
fn is_gone(h: usize) -> bool {
    let g = GONE.lock().unwrap();
    !g.is_empty() && g.contains(&h)
}

/// HID's own control codes (`FILE_DEVICE_KEYBOARD`, which hid.dll's `HidD_SetOutputReport`
/// and `HidD_SetFeature` send): judged as writes. A device unplugged for this copy
/// answers no control code at all (its reports, features and strings read through
/// `HidD_GetInputReport`, `HidD_GetFeature` and the like are another player's). Every
/// other handle's codes pass untouched.
unsafe extern "system" fn device_io_control(h: usize, code: u32, inb: *const u8, inl: u32, outb: *mut u8, outl: u32, ret: *mut u32, ol: *mut Overlapped) -> i32 {
    // IOCTL_HID_SET_FEATURE (0xB0191), IOCTL_HID_SET_OUTPUT_REPORT (0xB0195).
    let unplugged = super::shm::attached() && ((code >> 16) == 0xB && is_gone(h) && judge(h) == Read::Unplugged);
    if unplugged || (matches!(code, 0x000B_0191 | 0x000B_0195) && write_unplugged(h)) {
        if !ret.is_null() {
            unsafe { *ret = 0 };
        }
        set_last_error(ERROR_DEVICE_NOT_CONNECTED);
        return 0;
    }
    unsafe { IOCTL.get().unwrap().call(h, code, inb, inl, outb, outl, ret, ol) }
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
                log!("input {label} not hooked: {e}");
                0
            }
        }
    })
}

/// The overlay closed: the reports held for this opening go. Which handles are devices,
/// and which are unplugged for this copy, stay known.
pub fn hidden() {
    HELD.lock().unwrap().clear();
    PENDING.lock().unwrap().clear();
}

/// Controllers came or went (`HostMsg::DevicesChanged`): every device handle is judged
/// again at its next read, a handle unplugged for this copy too (it may be its own now).
pub fn devices_changed() {
    KNOWN.lock().unwrap().clear();
    let n = {
        let mut g = GONE.lock().unwrap();
        let n = g.len();
        g.clear();
        n
    };
    if n > 0 {
        log!("input: controllers changed: {n} HID handle(s) unplugged for this copy are judged again at their next read");
    }
}

/// A fresh CreateFile returned handle `h`: drop any stale entry from an earlier closed
/// handle that happened to share the same numeric value.
pub(super) fn reopened(h: usize) {
    GONE.lock().unwrap().retain(|k| *k != h);
    KNOWN.lock().unwrap().retain(|(k, _)| *k != h);
}

/// Whether `h` is a HID device (not a file, pipe or console, and hid.dll answers for
/// it), and what it is.
fn device_of(h: usize) -> Option<device::Hid> {
    {
        let mut known = KNOWN.lock().unwrap();
        if let Some(i) = known.iter().position(|(k, _)| *k == h) {
            let hid = known[i].1;
            // Most recently used last: the one dropped when full is the longest unread.
            let last = known.len() - 1;
            if i != last {
                let e = known.remove(i);
                known.push(e);
            }
            return hid;
        }
    }
    // A file, pipe or the console: not a device, and not kept (a game reads thousands
    // of files; their handle values come back as other files).
    if unsafe { windows_sys::Win32::Storage::FileSystem::GetFileType(h as _) } != 0 {
        return None;
    }
    let is_hid = module("hid.dll").and_then(|m| proc_addr(m, c"HidD_GetAttributes".as_ptr().cast())).is_some_and(|f| {
        let get: unsafe extern "system" fn(usize, *mut [u32; 3]) -> u8 = unsafe { std::mem::transmute(f) };
        let mut attrs = [12u32, 0, 0];
        unsafe { get(h, &mut attrs) != 0 }
    });
    let d = if is_hid { device::of_handle(h) } else { None };
    {
        let mut known = KNOWN.lock().unwrap();
        if known.len() >= KNOWN_MAX {
            known.remove(0);
        }
        known.push((h, d));
    }
    // Not under `KNOWN`: placing a handle opened before the hook (`may_unplug`) opens
    // the HID devices present, and an open forgets what was known of its handle value
    // (`reopened`), which takes `KNOWN` again (this thread waited on itself for good).
    if let Some(d) = d {
        let what = match super::topology() {
            Policy::Virtual(snap) if device::is_others(&d, &snap) && device::may_unplug(h, &d) => "a controller that isn't this copy's: unplugged for this copy",
            Policy::Virtual(snap) if device::is_others(&d, &snap) => "a controller that isn't this copy's, but an XInput controller's HID side (or one the hook can't place): as it is",
            Policy::Virtual(_) if d.controller && crate::overlay::pads_held_back() => "this copy's own controller: held at a report while the overlay has the controllers",
            Policy::Virtual(_) if d.controller => "this copy's own controller: as it is",
            Policy::Virtual(_) => "not a controller: as it is",
            _ => "held at its first report while the overlay has the controllers",
        };
        log!("input: the game reads HID {:04x}:{:04x} directly (handle {h:#x}): {what}", d.vid, d.pid);
    }
    d
}

/// What a read from `h` gets now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    /// The device's own report.
    Real,
    /// The first report read while the overlay has the controllers.
    Held,
    /// Nothing: another player's controller, unplugged for this split-screen copy.
    Unplugged,
}

fn judge(h: usize) -> Read {
    // A handle closed and its value reused for a file isn't a device any more.
    if is_gone(h) && unsafe { windows_sys::Win32::Storage::FileSystem::GetFileType(h as _) } == 0 {
        // The copy's seat may have changed since it was unplugged: its own now, it's
        // plugged back in (and read as it is from here on).
        let own = match (super::topology(), device_of(h)) {
            (Policy::Virtual(snap), Some(d)) => device::is_own(&d, &snap).then_some(d),
            _ => None,
        };
        let Some(d) = own else {
            UNPLUGGED_READS.fetch_add(1, Ordering::Relaxed);
            return Read::Unplugged;
        };
        GONE.lock().unwrap().retain(|k| *k != h);
        log!("input: HID {:04x}:{:04x} (handle {h:#x}) is this copy's own controller now: plugged back in", d.vid, d.pid);
    }
    match policy() {
        Policy::PassThrough => Read::Real,
        Policy::BlockAll => match (device_of(h), super::seat_or_nothing()) {
            // A split-screen copy: another player's controller stays unplugged, and only
            // its own is held.
            (Some(d), Some(seat)) if device::is_others(&d, &seat) && device::may_unplug(h, &d) => {
                let mut g = GONE.lock().unwrap();
                if !g.contains(&h) && g.len() < 64 {
                    g.push(h);
                }
                UNPLUGGED_READS.fetch_add(1, Ordering::Relaxed);
                Read::Unplugged
            }
            (Some(d), Some(_)) if !d.controller => Read::Real,
            (Some(_), _) => Read::Held,
            (None, _) => Read::Real,
        },
        Policy::Virtual(snap) => {
            let Some(d) = device_of(h) else { return Read::Real };
            let (r, why) = if device::is_own(&d, &snap) {
                super::note_own_hid_read(snap.pad.hid_key);
                OWN_READS.fetch_add(1, Ordering::Relaxed);
                (Read::Real, "this copy's own controller")
            } else if !d.controller {
                (Read::Real, "not a game controller")
            } else if device::may_unplug(h, &d) {
                let mut g = GONE.lock().unwrap();
                if !g.contains(&h) && g.len() < 64 {
                    g.push(h);
                }
                UNPLUGGED_READS.fetch_add(1, Ordering::Relaxed);
                (Read::Unplugged, if snap.pad.connected { "another player's controller" } else { "a controller, while this copy has none of its own yet" })
            } else {
                (Read::Real, "not this copy's, but an XInput controller's HID side (or one the hook can't place): XInput's routing decides")
            };
            note_read(h, &d, r, why);
            r
        }
    }
}

/// What each HID handle read last (`judge`, split screen), so a change goes in the log.
static LAST_READ: Mutex<Vec<(usize, Read)>> = Mutex::new(Vec::new());

/// Into the log when handle `h` (device `d`) starts reading as `r`, and why.
fn note_read(h: usize, d: &device::Hid, r: Read, why: &str) {
    let mut last = LAST_READ.lock().unwrap();
    match last.iter_mut().find(|(k, _)| *k == h) {
        Some((_, was)) if *was == r => return,
        Some((_, was)) => *was = r,
        None => {
            if last.len() >= 64 {
                last.remove(0);
            }
            last.push((h, r));
        }
    }
    drop(last);
    let how = match r {
        Read::Real => "as it is",
        Read::Held => "held still",
        Read::Unplugged => "unplugged",
    };
    let opened = match device::opened_seen(h) {
        Some(_) => "opened where the hook saw it",
        None => "opened before the hook could see it",
    };
    log!("input: HID {:04x}:{:04x} (handle {h:#x}, {opened}) now reads {how}: {why}", d.vid, d.pid);
}

/// Whether reads from `h` hold still now (the overlay has the controllers).
fn held(h: usize) -> bool {
    judge(h) == Read::Held
}

fn hold(h: usize, buf: *mut u8, n: u32) {
    if buf.is_null() || n == 0 {
        return;
    }
    let fresh = crate::overlay::hid_reports_fresh();
    let mut held = HELD.lock().unwrap();
    match held.iter_mut().find(|(k, _)| *k == h) {
        Some((_, r)) if fresh => {
            r.clear();
            r.extend_from_slice(unsafe { std::slice::from_raw_parts(buf, n as usize) });
        }
        Some((_, r)) if r.len() == n as usize => unsafe { std::ptr::copy_nonoverlapping(r.as_ptr(), buf, r.len()) },
        Some(_) => {}
        None => held.push((h, unsafe { std::slice::from_raw_parts(buf, n as usize) }.to_vec())),
    }
}

/// Copies `h`'s held report into `buf` (room for `len`): its size, or None when there's
/// none yet or it doesn't fit.
///
/// # Safety
/// `buf` holds `len` bytes.
unsafe fn serve_held(h: usize, buf: *mut u8, len: u32) -> Option<u32> {
    if buf.is_null() {
        return None;
    }
    let held = HELD.lock().unwrap();
    let (_, r) = held.iter().find(|(k, _)| *k == h)?;
    if r.is_empty() || r.len() > len as usize {
        return None;
    }
    unsafe { std::ptr::copy_nonoverlapping(r.as_ptr(), buf, r.len()) };
    Some(r.len() as u32)
}

fn last_error() -> u32 {
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

fn set_last_error(e: u32) {
    unsafe { windows_sys::Win32::Foundation::SetLastError(e) }
}

const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

unsafe extern "system" fn read_file(h: usize, buf: *mut u8, len: u32, read: *mut u32, ol: *mut Overlapped) -> i32 {
    let real = READ.get().unwrap();
    // Most reads are of files: the policy is only asked when an input block or the
    // overlay could change anything.
    let read_now = if super::shm::attached() || crate::overlay::pads_held_back() { judge(h) } else { Read::Real };
    if read_now == Read::Unplugged {
        if !read.is_null() {
            unsafe { *read = 0 };
        }
        set_last_error(ERROR_DEVICE_NOT_CONNECTED);
        return 0;
    }
    // Held, with its report in hand, read and waited for (no OVERLAPPED): answered with
    // it, about a report's time later, and no real one is started. An overlapped read
    // isn't answered at once: a loop that reads until a read finds nothing (HIDAPI's
    // `hid_read_timeout(0)` drain) would never end, and the game's thread with it. It is
    // the device's own read, which ends when the device sends its next report, and takes
    // the held report then (`hold`, at once or through `GetOverlappedResult`).
    if read_now == Read::Held
        && ol.is_null()
        && !crate::overlay::hid_reports_fresh()
        && let Some(n) = unsafe { serve_held(h, buf, len) }
    {
        std::thread::sleep(HELD_PACE);
        if !read.is_null() {
            unsafe { *read = n };
        }
        HELD_READS.fetch_add(1, Ordering::Relaxed);
        set_last_error(0);
        return 1;
    }
    let ok = unsafe { real.call(h, buf, len, read, ol) };
    let err = last_error();
    if !ol.is_null() {
        forget(ol);
        set_last_error(err);
    }
    if read_now == Read::Held {
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
            if p.len() < 64 {
                p.push((ol as usize, h, buf as usize));
            }
        }
    }
    set_last_error(err);
    ok
}

fn forget(ol: *mut Overlapped) {
    let mut p = PENDING.lock().unwrap();
    if !p.is_empty() {
        p.retain(|(o, _, _)| *o != ol as usize);
    }
}

/// `ReadFileEx`: the read's completion routine runs as an APC on the reading thread. A
/// controller unplugged for this copy fails at once; one held still is read as it is,
/// its report swapped for the held one before the game's routine sees it (`held_done`).
unsafe extern "system" fn read_file_ex(h: usize, buf: *mut u8, len: u32, ol: *mut Overlapped, done: Option<Completion>) -> i32 {
    let real = READ_EX.get().unwrap();
    let read_now = if super::shm::attached() || crate::overlay::pads_held_back() { judge(h) } else { Read::Real };
    match (read_now, done) {
        (Read::Unplugged, _) => {
            set_last_error(ERROR_DEVICE_NOT_CONNECTED);
            0
        }
        (Read::Held, Some(f)) if !ol.is_null() => {
            {
                let mut p = EX_PENDING.lock().unwrap();
                p.retain(|(o, _, _, _)| *o != ol as usize);
                if p.len() >= 64 {
                    p.remove(0);
                }
                p.push((ol as usize, h, buf as usize, f as usize));
            }
            let ok = unsafe { real.call(h, buf, len, ol, Some(held_done)) };
            if ok == 0 {
                let err = last_error();
                EX_PENDING.lock().unwrap().retain(|(o, _, _, _)| *o != ol as usize);
                set_last_error(err);
            }
            ok
        }
        _ => unsafe { real.call(h, buf, len, ol, done) },
    }
}

/// A held `ReadFileEx` read finished: the held report in its buffer (while still held),
/// then the game's own completion routine.
unsafe extern "system" fn held_done(err: u32, n: u32, ol: *mut Overlapped) {
    let entry = {
        let mut p = EX_PENDING.lock().unwrap();
        p.iter().position(|(o, _, _, _)| *o == ol as usize).map(|i| p.swap_remove(i))
    };
    let Some((_, h, buf, f)) = entry else { return };
    if err == 0 && held(h) {
        hold(h, buf as *mut u8, n);
        HELD_READS.fetch_add(1, Ordering::Relaxed);
    }
    let f: Completion = unsafe { std::mem::transmute(f) };
    unsafe { f(err, n, ol) };
}

const ERROR_IO_INCOMPLETE: u32 = 996;
const WAIT_TIMEOUT_ERR: u32 = 258;
const WAIT_IO_COMPLETION_ERR: u32 = 192;

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

/// Called by `crate::overlapped` whenever `GetOverlappedResult` or `GetOverlappedResultEx`
/// returns. Preserves the calling thread's `GetLastError()`.
pub(crate) fn on_overlapped_result(h: usize, ol: *mut Overlapped, n: *mut u32, ok: bool) {
    if !ok && !ol.is_null() {
        let err = last_error();
        if !matches!(err, ERROR_IO_INCOMPLETE | WAIT_TIMEOUT_ERR | WAIT_IO_COMPLETION_ERR) {
            forget(ol);
        }
        set_last_error(err);
    }
    if ok && !PENDING.lock().unwrap().is_empty() {
        let err = last_error();
        if held(h) {
            finished(h, ol, n);
        } else {
            // Read while it was held, finished once it isn't (the overlay closed): the
            // report is the device's own.
            forget(ol);
        }
        set_last_error(err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Files aren't devices, and aren't remembered: a game reading thousands of files
    /// never fills `KNOWN`, pushing its controllers out.
    #[test]
    fn files_are_not_devices_and_not_kept() {
        use std::os::windows::io::AsRawHandle;
        let path = std::env::temp_dir().join(format!("peerparty-hid-test-{}.bin", std::process::id()));
        std::fs::write(&path, b"not a controller").unwrap();
        let files: Vec<std::fs::File> = (0..KNOWN_MAX + 8).map(|_| std::fs::File::open(&path).unwrap()).collect();
        for f in &files {
            assert!(device_of(f.as_raw_handle() as usize).is_none());
        }
        let known = KNOWN.lock().unwrap();
        assert!(!files.iter().any(|f| known.iter().any(|(k, _)| *k == f.as_raw_handle() as usize)), "no file handle is kept");
        drop(known);
        drop(files);
        let _ = std::fs::remove_file(&path);
    }

    /// While the overlay holds the controllers, a read of a held device gets the report
    /// kept for it, the same each time, and only one that fits; a device with none kept
    /// yet gets none (its first real read is kept).
    #[test]
    fn a_held_device_reads_its_kept_report() {
        let h = 0x7000_1234usize;
        let mut buf = [0u8; 8];
        assert_eq!(unsafe { serve_held(h, buf.as_mut_ptr(), 8) }, None, "nothing kept yet");
        let first = [1u8, 2, 3, 4];
        let mut got = first;
        hold(h, got.as_mut_ptr(), 4);
        // A later report is overwritten with the kept one.
        let mut later = [9u8, 9, 9, 9];
        hold(h, later.as_mut_ptr(), 4);
        assert_eq!(later, first);
        assert_eq!(unsafe { serve_held(h, buf.as_mut_ptr(), 8) }, Some(4));
        assert_eq!(&buf[..4], &first);
        let mut small = [0u8; 2];
        assert_eq!(unsafe { serve_held(h, small.as_mut_ptr(), 2) }, None, "doesn't fit");
        HELD.lock().unwrap().retain(|(k, _)| *k != h);
    }
}
