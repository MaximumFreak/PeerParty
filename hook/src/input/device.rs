//! Which controller a HID handle, a Raw Input device or a device path is, by the key the
//! session host gives a copy's own controller (`input_shm::hid_key`: vendor, product and
//! serial number), and whether it's a game controller at all (a gamepad, joystick or
//! multi-axis collection). A split-screen copy sees one controller: its own, read as it
//! is. The other players' game controllers aren't there for it: opening one fails as
//! if it were unplugged (`refuse_open`, from the file hooks in saves.rs), Raw Input
//! doesn't list them (`input::raw_device_list`) and a handle opened before reads as
//! unplugged (hid.rs).

use std::cell::Cell;
use std::sync::{Mutex, OnceLock};

use peerparty_protocol::input_shm::{InputSnapshot, hid_key};

use super::{module, proc_addr};
use crate::log;

/// `HIDD_ATTRIBUTES`.
#[repr(C)]
struct Attributes {
    size: u32,
    vid: u16,
    pid: u16,
    version: u16,
}

type GetAttributes = unsafe extern "system" fn(usize, *mut Attributes) -> u8;
type GetSerial = unsafe extern "system" fn(usize, *mut u16, u32) -> u8;
type GetPreparsed = unsafe extern "system" fn(usize, *mut usize) -> u8;
type FreePreparsed = unsafe extern "system" fn(usize) -> u8;
type GetCaps = unsafe extern "system" fn(usize, *mut [u16; 32]) -> i32;
type GetDeviceInfo = unsafe extern "system" fn(isize, u32, *mut u16, *mut u32) -> u32;

const RIDI_DEVICENAME: u32 = 0x2000_0007;
const HIDP_STATUS_SUCCESS: i32 = 0x0011_0000;
const USAGE_PAGE_GENERIC: u16 = 0x01;

/// A HID device as the hook sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Hid {
    pub vid: u16,
    pub pid: u16,
    /// Its key (`input_shm::hid_key`).
    pub key: u64,
    /// A gamepad, joystick or multi-axis controller (its top collection's usage).
    pub controller: bool,
}

/// Whether a generic-desktop usage is a game controller's: joystick, gamepad,
/// multi-axis.
pub(super) fn is_controller_usage(page: u16, usage: u16) -> bool {
    page == USAGE_PAGE_GENERIC && matches!(usage, 0x04 | 0x05 | 0x08)
}

struct HidFns {
    attributes: GetAttributes,
    serial: GetSerial,
    preparsed: GetPreparsed,
    free: FreePreparsed,
    caps: GetCaps,
}

/// hid.dll's calls, from System32 (loaded when the game hasn't: a game reading Raw Input
/// or DirectInput never needs it itself).
fn hid() -> Option<&'static HidFns> {
    static FNS: OnceLock<Option<HidFns>> = OnceLock::new();
    if let Some(f) = FNS.get() {
        return f.as_ref();
    }
    let m = module("hid.dll").or_else(|| {
        // Not from a load notification: the loader lock would be held (and the next
        // call tries again).
        if crate::watch::in_loader() {
            return None;
        }
        let wide: Vec<u16> = "hid.dll".encode_utf16().chain(Some(0)).collect();
        use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
        let m = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        (!m.is_null()).then_some(m)
    })?;
    let get = |name: &std::ffi::CStr| proc_addr(m, name.as_ptr().cast());
    FNS.get_or_init(|| {
        Some(unsafe {
            HidFns {
                attributes: std::mem::transmute::<usize, GetAttributes>(get(c"HidD_GetAttributes")?),
                serial: std::mem::transmute::<usize, GetSerial>(get(c"HidD_GetSerialNumberString")?),
                preparsed: std::mem::transmute::<usize, GetPreparsed>(get(c"HidD_GetPreparsedData")?),
                free: std::mem::transmute::<usize, FreePreparsed>(get(c"HidD_FreePreparsedData")?),
                caps: std::mem::transmute::<usize, GetCaps>(get(c"HidP_GetCaps")?),
            }
        })
    })
    .as_ref()
}

/// The HID device `h` is open on; None when it isn't one.
pub(super) fn of_handle(h: usize) -> Option<Hid> {
    let f = hid()?;
    let mut a = Attributes { size: size_of::<Attributes>() as u32, vid: 0, pid: 0, version: 0 };
    if unsafe { (f.attributes)(h, &mut a) } == 0 {
        return None;
    }
    let mut buf = [0u16; 128];
    let text = if unsafe { (f.serial)(h, buf.as_mut_ptr(), (buf.len() * 2) as u32) } != 0 {
        let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    } else {
        String::new()
    };
    let mut controller = false;
    let mut pp = 0usize;
    if unsafe { (f.preparsed)(h, &mut pp) } != 0 && pp != 0 {
        // HIDP_CAPS: Usage, UsagePage, then lengths and counts.
        let mut caps = [0u16; 32];
        if unsafe { (f.caps)(pp, &mut caps) } == HIDP_STATUS_SUCCESS {
            controller = is_controller_usage(caps[1], caps[0]);
        }
        unsafe { (f.free)(pp) };
    }
    Some(Hid { vid: a.vid, pid: a.pid, key: hid_key(a.vid, a.pid, &text), controller })
}

thread_local! {
    /// Opening a device to ask what it is (its own CreateFile mustn't be judged).
    static ASKING: Cell<bool> = const { Cell::new(false) };
}

/// The HID device at `path` (a device interface path, NUL-terminated); None when it
/// isn't one, or can't be opened to ask.
pub(super) fn of_path(path: &[u16]) -> Option<Hid> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    if path.first().is_none_or(|c| *c == 0) || path.last() != Some(&0) {
        return None;
    }
    // No access asked: enough for the device's attributes and caps, and it never
    // competes with the game's own handle.
    let was = ASKING.with(|a| a.replace(true));
    let h = unsafe { CreateFileW(path.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    ASKING.with(|a| a.set(was));
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let d = of_handle(h as usize);
    unsafe { CloseHandle(h) };
    d
}

/// The key of the HID device at `path`; 0 when it isn't one.
pub(super) fn key_of_path(path: &[u16]) -> u64 {
    of_path(path).map_or(0, |d| d.key)
}

/// Devices already asked, by Raw Input device handle.
static RAW: Mutex<Vec<(isize, Option<Hid>)>> = Mutex::new(Vec::new());

thread_local! {
    /// Inside `of_raw` already on this thread: another program's hook of what it calls
    /// (an overlay's, of the HID functions) asked for the device list again.
    static LOOKING: Cell<bool> = const { Cell::new(false) };
}

/// Raw Input device `dev`, a HID one. None when asked again from inside its own lookup
/// (not remembered, so the outer lookup decides).
pub(super) fn of_raw(dev: isize) -> Option<Hid> {
    if let Some(&(_, d)) = RAW.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(r, _)| *r == dev) {
        return d;
    }
    if LOOKING.with(|l| l.replace(true)) {
        return None;
    }
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            LOOKING.with(|l| l.set(false));
        }
    }
    let _done = Done;
    let d = raw_path(dev).and_then(|p| of_path(&p));
    let mut known = RAW.lock().unwrap_or_else(|e| e.into_inner());
    if known.len() >= 64 {
        known.remove(0);
    }
    known.push((dev, d));
    d
}

/// The key of Raw Input device `dev`.
pub(super) fn key_of_raw(dev: isize) -> u64 {
    of_raw(dev).map_or(0, |d| d.key)
}

/// Raw Input device `dev`'s interface path, NUL-terminated.
fn raw_path(dev: isize) -> Option<Vec<u16>> {
    let m = module("user32.dll")?;
    let get: GetDeviceInfo = unsafe { std::mem::transmute(proc_addr(m, c"GetRawInputDeviceInfoW".as_ptr().cast())?) };
    let mut len = 0u32;
    unsafe { get(dev, RIDI_DEVICENAME, std::ptr::null_mut(), &mut len) };
    if len == 0 || len > 1024 {
        return None;
    }
    let mut buf = vec![0u16; len as usize + 1];
    let got = unsafe { get(dev, RIDI_DEVICENAME, buf.as_mut_ptr(), &mut len) };
    if got == u32::MAX || got == 0 {
        return None;
    }
    buf.truncate(got as usize);
    if buf.last() != Some(&0) {
        buf.push(0);
    }
    Some(buf)
}

/// Whether a device of key `key` is this copy's own controller.
pub(super) fn is_own(key: u64, snap: &InputSnapshot) -> bool {
    key != 0 && snap.pad.connected && key == snap.pad.hid_key
}

/// Whether device `d` is a game controller that isn't this copy's: another player's, or
/// any when the copy plays with the keyboard.
pub(super) fn is_others(d: &Hid, snap: &InputSnapshot) -> bool {
    d.controller && !is_own(d.key, snap)
}

/// Whether `path` names a HID device (`\\?\HID#...`, any case).
pub(super) fn is_hid_path(path: &[u16]) -> bool {
    const PREFIXES: [&str; 2] = ["\\\\?\\hid#", "\\\\.\\hid#"];
    PREFIXES.iter().any(|p| {
        let p: Vec<u16> = p.encode_utf16().collect();
        path.len() >= p.len() && path.iter().zip(&p).all(|(a, b)| char::from_u32(u32::from(*a)).is_some_and(|c| c.to_ascii_lowercase() as u32 == u32::from(*b)))
    })
}

/// Whether `path` is an XInput controller's HID side (`&IG_` in it): XInput's own
/// routing and Windows.Gaming.Input's decide what a copy sees of those, and Windows'
/// gamepad stack may open it for the game, so it's never refused.
pub(super) fn is_xinput_path(path: &[u16]) -> bool {
    let lower: Vec<u16> = path.iter().map(|c| if (u16::from(b'A')..=u16::from(b'Z')).contains(c) { c + 32 } else { *c }).collect();
    let ig: Vec<u16> = "&ig_".encode_utf16().collect();
    lower.windows(ig.len()).any(|w| w == ig.as_slice())
}

/// Paths already judged, with what they are and whether the last open was refused.
static PATHS: Mutex<Vec<(Vec<u16>, Option<Hid>, Option<bool>)>> = Mutex::new(Vec::new());

/// Whether the game's open of `path` (NUL-terminated or not) fails: in a split-screen
/// copy, another player's game controller isn't there for it. Called from every
/// CreateFile; a path that isn't a HID device's costs a comparison.
pub(crate) fn refuse_open(path: &[u16]) -> bool {
    if !super::shm::attached() || !is_hid_path(path) || is_xinput_path(path) || ASKING.with(Cell::get) {
        return false;
    }
    // While the overlay has the controllers too (`topology`): opened then, another
    // player's controller became the game's, and was gone when the overlay closed.
    let super::Policy::Virtual(snap) = super::topology() else { return false };
    let mut path: Vec<u16> = path.iter().copied().take_while(|c| *c != 0).collect();
    path.push(0);
    let known = PATHS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(p, _, _)| *p == path).map(|(_, d, r)| (*d, *r));
    let (d, prev_refused) = match known {
        Some((d, r)) => (d, r),
        None => {
            let d = of_path(&path);
            let mut paths = PATHS.lock().unwrap_or_else(|e| e.into_inner());
            if paths.len() >= 64 {
                paths.remove(0);
            }
            paths.push((path.clone(), d, None));
            (d, None)
        }
    };
    let Some(d) = d else { return false };
    if d.controller {
        GAME_OPENS_PADS.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let refuse = is_others(&d, &snap);
    if d.controller && prev_refused != Some(refuse) {
        if let Some(entry) = PATHS.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|(p, _, _)| *p == path) {
            entry.2 = Some(refuse);
        }
        let what = if refuse { "another player's controller: not there for this copy" } else { "this copy's own controller: as it is" };
        log!("input: the game opens HID {:04x}:{:04x}, {what}", d.vid, d.pid);
    }
    refuse
}

/// The game opens HID game controllers itself (any player's, refused or not): one may
/// be about to read its own DualSense natively, rather than XInput's (`super::route`).
static GAME_OPENS_PADS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn game_opens_pads() -> bool {
    GAME_OPENS_PADS.load(std::sync::atomic::Ordering::Relaxed)
}

/// HID devices the game opened through CreateFile (handle, and whether it's an XInput
/// controller's HID side, `is_xinput_path`). Only these can be unplugged for a copy: a
/// handle the hook didn't see opened (another way in, or before the file hooks) has no
/// path to tell an XInput controller's from another player's own, and is left alone.
static OPENED: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());

/// The game opened `path` as handle `h` (CreateFile's result). Leaves the thread's
/// last error as CreateFile set it.
fn opened(path: &[u16], h: isize) {
    // The hook's own opens, to ask a device what it is (`of_path`), aren't the game's.
    if h == -1 || h == 0 || !is_hid_path(path) || ASKING.with(Cell::get) {
        return;
    }
    let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    let _keep = Restore(err);
    let xinput = is_xinput_path(path);
    let mut hs = OPENED.lock().unwrap_or_else(|e| e.into_inner());
    hs.retain(|(k, _)| *k != h as usize);
    if hs.len() >= 64 {
        hs.remove(0);
    }
    hs.push((h as usize, xinput));
    drop(hs);
    super::hid::reopened(h as usize);
}

/// Puts the thread's last error back when dropped.
struct Restore(u32);

impl Drop for Restore {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::SetLastError(self.0) };
    }
}

/// Whether the hook saw handle `h` opened (`Some`), and as an XInput controller's HID
/// side (`Some(true)`).
pub(super) fn opened_seen(h: usize) -> Option<bool> {
    OPENED.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == h).map(|(_, x)| *x)
}

/// Whether handle `h` (device `d`) may be unplugged for this copy: it isn't an XInput
/// controller's HID side, by the path the hook saw it opened with or, for one opened
/// before the hook (a game opening its controllers at start), by the paths of the HID
/// devices present with its vendor, product and serial (`xinput_side_of`).
pub(super) fn may_unplug(h: usize, d: &Hid) -> bool {
    match opened_seen(h) {
        Some(xinput) => !xinput,
        None => xinput_side_of(d) == Some(false),
    }
}

/// HID devices already looked up by key: whether one is an XInput controller's HID side.
static SIDES: Mutex<Vec<(u64, Option<bool>)>> = Mutex::new(Vec::new());

/// Whether HID device `d` is an XInput controller's HID side (`&IG_` in its path), found
/// among the HID devices present by its key: Some(false) when every device with its key
/// is something else, None when none is found (or the list can't be had).
pub(super) fn xinput_side_of(d: &Hid) -> Option<bool> {
    if let Some(&(_, s)) = SIDES.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == d.key) {
        return s;
    }
    let paths = hid_interface_paths();
    let matching: Vec<&Vec<u16>> = paths.iter().filter(|p| of_path(p).is_some_and(|o| o.key == d.key)).collect();
    let side = if matching.is_empty() { None } else { Some(matching.iter().any(|p| is_xinput_path(p))) };
    let shown: Vec<String> = matching.iter().map(|p| String::from_utf16_lossy(&p[..p.len().saturating_sub(1)])).collect();
    let what = match side {
        None => format!("no HID device present matches it ({} looked at): left as it is", paths.len()),
        Some(true) => format!("an XInput controller's HID side ({}): left to XInput's own routing", shown.join(", ")),
        Some(false) => format!("not an XInput controller's ({}): judged as if its opening had been seen", shown.join(", ")),
    };
    log!("input: HID {:04x}:{:04x} was opened before the hook could see its path; {what}", d.vid, d.pid);
    let mut s = SIDES.lock().unwrap_or_else(|e| e.into_inner());
    if s.len() < 64 {
        s.push((d.key, side));
    }
    side
}

/// The interface paths (NUL-terminated) of the HID devices present, from cfgmgr32.dll
/// (System32): not through the hooked Raw Input device list.
pub(super) fn hid_interface_paths() -> Vec<Vec<u16>> {
    use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    // GUID_DEVINTERFACE_HID {4D1E55B2-F16F-11CF-88CB-001111000030}.
    const HID: [u8; 16] = super::guid(0x4D1E55B2, 0xF16F, 0x11CF, [0x88, 0xCB, 0x00, 0x11, 0x11, 0x00, 0x00, 0x30]);
    type Size = unsafe extern "system" fn(*mut u32, *const [u8; 16], *const u16, u32) -> u32;
    type List = unsafe extern "system" fn(*const [u8; 16], *const u16, *mut u16, u32, u32) -> u32;
    if crate::watch::in_loader() {
        return Vec::new();
    }
    let m = module("cfgmgr32.dll").or_else(|| {
        let wide: Vec<u16> = "cfgmgr32.dll".encode_utf16().chain(Some(0)).collect();
        let m = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        (!m.is_null()).then_some(m)
    });
    let Some(m) = m else { return Vec::new() };
    let (Some(size), Some(list)) = (proc_addr(m, c"CM_Get_Device_Interface_List_SizeW".as_ptr().cast()), proc_addr(m, c"CM_Get_Device_Interface_ListW".as_ptr().cast())) else {
        return Vec::new();
    };
    let (size, list): (Size, List) = unsafe { (std::mem::transmute(size), std::mem::transmute(list)) };
    // A device may arrive between the two calls: tried a few times.
    for _ in 0..3 {
        let mut len = 0u32;
        // CM_GET_DEVICE_INTERFACE_LIST_PRESENT (0); CR_SUCCESS (0), CR_BUFFER_SMALL (0x1a).
        if unsafe { size(&mut len, &HID, std::ptr::null(), 0) } != 0 || len == 0 || len > 1 << 20 {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        match unsafe { list(&HID, std::ptr::null(), buf.as_mut_ptr(), len, 0) } {
            0 => {
                return buf.split(|c| *c == 0).filter(|p| !p.is_empty()).map(|p| p.iter().copied().chain(Some(0)).collect()).collect();
            }
            0x1a => continue,
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

/// A wide path's first characters, up to 1024 (NUL not included).
fn wide_path(name: *const u16) -> Vec<u16> {
    let mut path = Vec::with_capacity(128);
    for i in 0..1024 {
        let c = unsafe { *name.add(i) };
        if c == 0 {
            break;
        }
        path.push(c);
    }
    path
}

/// The game's CreateFileW (or CreateFile2) of `name` returned `h`.
pub(crate) fn opened_w(name: *const u16, h: isize) {
    if !name.is_null() && super::shm::attached() {
        opened(&wide_path(name), h);
    }
}

/// The game's CreateFileA of `name` returned `h`.
pub(crate) fn opened_a(name: *const u8, h: isize) {
    if name.is_null() || !super::shm::attached() {
        return;
    }
    let mut path = Vec::with_capacity(128);
    for i in 0..1024 {
        let c = unsafe { *name.add(i) };
        if c == 0 {
            break;
        }
        path.push(u16::from(c));
    }
    opened(&path, h);
}

/// `refuse_open` for a wide path pointer (CreateFileW, CreateFile2).
pub(crate) fn refuse_open_w(name: *const u16) -> bool {
    if name.is_null() || !super::shm::attached() {
        return false;
    }
    let mut path = Vec::with_capacity(128);
    for i in 0..1024 {
        let c = unsafe { *name.add(i) };
        if c == 0 {
            break;
        }
        path.push(c);
        // Not a device path: no need to read on.
        if path.len() == 8 && !is_hid_path(&path) {
            return false;
        }
    }
    refuse_open(&path)
}

/// `refuse_open` for an ANSI path pointer (CreateFileA).
pub(crate) fn refuse_open_a(name: *const u8) -> bool {
    if name.is_null() || !super::shm::attached() {
        return false;
    }
    let mut path = Vec::with_capacity(128);
    for i in 0..1024 {
        let c = unsafe { *name.add(i) };
        if c == 0 {
            break;
        }
        path.push(u16::from(c));
        if path.len() == 8 && !is_hid_path(&path) {
            return false;
        }
    }
    refuse_open(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::input_shm::{InputMode, VirtualPad};

    #[test]
    fn only_the_copys_own_device_is_its_own() {
        let snap = |connected, hid_key| InputSnapshot { mode: InputMode::Virtual, pad: VirtualPad { connected, hid_key, ..Default::default() }, ..Default::default() };
        assert!(is_own(7, &snap(true, 7)));
        assert!(!is_own(8, &snap(true, 7)), "another player's");
        assert!(!is_own(0, &snap(true, 0)), "no key is nobody's");
        assert!(!is_own(7, &snap(false, 7)), "its controller is gone");
        assert_eq!(key_of_path(&[0]), 0);
        assert_eq!(key_of_path(&"\\\\?\\nothing-here".encode_utf16().chain(Some(0)).collect::<Vec<_>>()), 0);
        assert_eq!(key_of_raw(0x7777_0000), 0, "no such Raw Input device");
        let pad = |key| Hid { vid: 1, pid: 2, key, controller: true };
        assert!(is_others(&pad(8), &snap(true, 7)));
        assert!(!is_others(&pad(7), &snap(true, 7)));
        assert!(is_others(&pad(7), &snap(false, 0)), "a keyboard player's copy has no controller");
        assert!(!is_others(&Hid { controller: false, ..pad(8) }, &snap(true, 7)), "not a controller: left alone");
    }

    #[test]
    fn hid_paths_are_told_apart() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        assert!(is_hid_path(&w("\\\\?\\HID#VID_054C&PID_0DF2#7&1&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}")));
        assert!(is_hid_path(&w("\\\\?\\hid#{00001124-0000-1000-8000-00805f9b34fb}_vid&0002054c")));
        assert!(!is_hid_path(&w("C:\\Games\\save.dat")));
        assert!(!is_hid_path(&w("\\\\?\\USB#VID_045E&PID_028E")));
        assert!(!is_hid_path(&w("\\\\?\\hi")));
        assert!(is_xinput_path(&w("\\\\?\\HID#VID_045E&PID_028E&IG_00#4&fb85800&0&0000#{4d1e55b2}")));
        assert!(!is_xinput_path(&w("\\\\?\\HID#VID_054C&PID_0DF2#7&1&0000#{4d1e55b2}")));
        assert!(is_controller_usage(1, 5) && is_controller_usage(1, 4) && is_controller_usage(1, 8));
        assert!(!is_controller_usage(1, 6), "a keyboard");
        assert!(!is_controller_usage(0x0c, 5), "consumer controls");
    }
}
