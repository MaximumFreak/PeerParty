//! DirectInput 8, in a split-screen copy and while the overlay has the controllers.
//!
//! Hooked without calling into DirectInput from a load notification or the hook's start
//! (the loader lock is held then): `DirectInput8Create` is hooked by its export, the
//! object it hands the game gives `IDirectInput8`'s methods (`CreateDevice`,
//! `EnumDevices`), and the first device made gives the device's (the A and W interfaces
//! each have their own). When DirectInput was in use before the hooks went in (the
//! overlay opening later), an object and a keyboard of the hook's own find them
//! (`hook_existing`), off the loader lock.
//!
//! In a split-screen copy (`Policy::Virtual`):
//! - another player's HID controller isn't listed (`EnumDevices`) and can't be made
//!   (`CreateDevice`, `DIERR_DEVICENOTREG`), as it can't be opened (device.rs);
//! - the copy's own HID controller reads as it is: state, buffered events, force feedback;
//! - one joystick (`bound`) carries the virtual pad when the copy's controller isn't a
//!   HID one the game reads itself (an XInput pad, whose HID side DirectInput lists too),
//!   the others read at rest and their buffers stay empty; force feedback reaches the
//!   copy's own controller alone;
//! - the keyboard and mouse read as the copy has them, buffered events too, and a key or
//!   button let go still reaches a copy that lost them while it was down;
//! - foreground access becomes background access, and a joystick that isn't the copy's
//!   own isn't taken exclusively (another copy may want it).
//!
//! A device's data format (`SetDataFormat`) says where each axis, hat and button is in
//! what it reads, so a custom format (SDL's has every axis and 128 buttons) reads right;
//! a device whose format was set before the hooks is taken as the standard joystick
//! format of its size.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use peerparty_protocol::input_shm::{InputSnapshot, VirtualKeyboard, VirtualPad};
use retour::GenericDetour;

use super::{Policy, Slots, arm, arm_once, device, guid, module, policy, proc_addr};
use crate::log;

type Create = unsafe extern "system" fn(isize, u32, *const [u8; 16], *mut usize, usize) -> i32;
type CreateDevice = unsafe extern "system" fn(usize, *const [u8; 16], *mut usize, usize) -> i32;
type EnumDevices = unsafe extern "system" fn(usize, u32, usize, usize, u32) -> i32;
type DevRelease = unsafe extern "system" fn(usize) -> u32;
type DevSetProperty = unsafe extern "system" fn(usize, usize, *const u8) -> i32;
type DevGetState = unsafe extern "system" fn(usize, u32, *mut u8) -> i32;
type DevGetData = unsafe extern "system" fn(usize, u32, *mut u8, *mut u32, u32) -> i32;
type DevSetDataFormat = unsafe extern "system" fn(usize, *const DataFormat) -> i32;
type DevSetCooperativeLevel = unsafe extern "system" fn(usize, isize, u32) -> i32;
type DevCreateEffect = unsafe extern "system" fn(usize, *const [u8; 16], *const u8, *mut usize, usize) -> i32;
type DevSendCommand = unsafe extern "system" fn(usize, u32) -> i32;

static CREATE: OnceLock<GenericDetour<Create>> = OnceLock::new();
static CREATE_DEVICE: Slots<CreateDevice, 2> = Slots::new();
static ENUM_DEVICES: Slots<EnumDevices, 2> = Slots::new();
static DEV_RELEASE: Slots<DevRelease, 2> = Slots::new();
static DEV_PROPERTY: Slots<DevSetProperty, 2> = Slots::new();
static DEV_STATE: Slots<DevGetState, 2> = Slots::new();
static DEV_DATA: Slots<DevGetData, 2> = Slots::new();
static DEV_FORMAT: Slots<DevSetDataFormat, 2> = Slots::new();
static DEV_COOP: Slots<DevSetCooperativeLevel, 2> = Slots::new();
static DEV_EFFECT: Slots<DevCreateEffect, 2> = Slots::new();
static DEV_COMMAND: Slots<DevSendCommand, 2> = Slots::new();

/// Reads held back while the overlay showed, and joystick reads in split screen (the
/// 30-second summary).
pub(super) static HELD: AtomicU32 = AtomicU32::new(0);
pub(super) static READS: AtomicU32 = AtomicU32::new(0);

const E_FAIL: i32 = 0x8000_4005_u32 as i32;
const DIERR_DEVICENOTREG: i32 = 0x8004_0154_u32 as i32;
const DIERR_UNSUPPORTED: i32 = 0x8000_4001_u32 as i32;
const DIGDD_PEEK: u32 = 1;
/// `MAKEDIPROP(4)`: `DIPROP_RANGE`; `MAKEDIPROP(12)`: `DIPROP_GUIDANDPATH`.
const DIPROP_RANGE: usize = 4;
const DIPROP_GUIDANDPATH: usize = 12;

const IID_W: [u8; 16] = guid(0xBF798031, 0x483A, 0x4DA2, [0xAA, 0x99, 0x5D, 0x64, 0xED, 0x36, 0x97, 0x00]);
const IID_A: [u8; 16] = guid(0xBF798030, 0x483A, 0x4DA2, [0xAA, 0x99, 0x5D, 0x64, 0xED, 0x36, 0x97, 0x00]);
const GUID_SYS_KEYBOARD: [u8; 16] = guid(0x6F1D2B61, 0xD5A0, 0x11CF, [0xBF, 0xC7, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00]);

pub(crate) const DISCL_EXCLUSIVE: u32 = 0x1;
pub(crate) const DISCL_NONEXCLUSIVE: u32 = 0x2;
pub(crate) const DISCL_FOREGROUND: u32 = 0x4;
pub(crate) const DISCL_BACKGROUND: u32 = 0x8;
pub(crate) const DISCL_NOWINKEY: u32 = 0x10;

fn vt(obj: usize) -> *const usize {
    unsafe { *(obj as *const *const usize) }
}

fn slot(obj: usize, i: usize) -> usize {
    unsafe { *vt(obj).add(i) }
}

/// dinput8.dll is there (loaded, or at the hooks' install): its `DirectInput8Create`
/// export is hooked, which calls nothing in it (safe under the loader lock). Off the
/// loader lock, objects made before are found too (`hook_existing`).
pub(super) fn module_loaded() -> usize {
    let Some(m) = module("dinput8.dll") else { return 0 };
    let mut n = 0;
    if let Some(a) = proc_addr(m, c"DirectInput8Create".as_ptr().cast()) {
        n += arm_once(&CREATE, a, create as Create, "DirectInput8Create");
    }
    if !crate::watch::in_loader() {
        n += hook_existing();
    }
    n
}

/// Hooks the methods through objects of the hook's own: one per interface (A and W),
/// and a keyboard of each (its methods are every device's).
pub(super) fn hook_existing() -> usize {
    let Some(m) = module("dinput8.dll") else { return 0 };
    // Through the real one: the hooked export would hook its object too, which is fine,
    // but the log says the game made it.
    let Some(export) = proc_addr(m, c"DirectInput8Create".as_ptr().cast()) else { return 0 };
    let create = |instance: isize, version: u32, iid: &[u8; 16], out: &mut usize| -> i32 {
        match CREATE.get() {
            Some(h) => unsafe { h.call(instance, version, iid, out, 0) },
            None => unsafe { std::mem::transmute::<usize, Create>(export)(instance, version, iid, out, 0) },
        }
    };
    let instance = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) } as isize;
    let mut n = 0;
    for iid in [IID_W, IID_A] {
        let mut di = 0usize;
        if create(instance, 0x0800, &iid, &mut di) < 0 || di == 0 {
            continue;
        }
        n += hook_input_object(di);
        // IDirectInput8::CreateDevice (slot 3), the real one when hooked.
        let mut dev = 0usize;
        let made = match real_create_device(di) {
            Some(h) => unsafe { h.call(di, &GUID_SYS_KEYBOARD, &mut dev, 0) },
            None => unsafe { std::mem::transmute::<usize, CreateDevice>(slot(di, 3))(di, &GUID_SYS_KEYBOARD, &mut dev, 0) },
        };
        if made >= 0 && dev != 0 {
            n += hook_device_object(dev);
            release(dev);
        }
        release(di);
    }
    n
}

/// IUnknown::Release (slot 2).
fn release(obj: usize) -> u32 {
    let f: DevRelease = unsafe { std::mem::transmute(slot(obj, 2)) };
    unsafe { f(obj) }
}

/// The hook of `di`'s interface's `CreateDevice` (its `call` is the real one), once hooked.
fn real_create_device(di: usize) -> Option<&'static GenericDetour<CreateDevice>> {
    let at = slot(di, 3);
    let i = CREATE_DEVICE.addrs.lock().unwrap().iter().position(|a| *a == at)?;
    CREATE_DEVICE.hooks[i].get()
}

/// `IDirectInput8A/W`: CreateDevice is slot 3, EnumDevices slot 4.
fn hook_input_object(di: usize) -> usize {
    arm(&CREATE_DEVICE, slot(di, 3), per_slot!(CreateDevice, [0, 1], create_device, (this: usize, guid: *const [u8; 16], out: *mut usize, outer: usize) -> i32), "IDirectInput8::CreateDevice")
        + arm(&ENUM_DEVICES, slot(di, 4), per_slot!(EnumDevices, [0, 1], enum_devices, (this: usize, kind: u32, cb: usize, rf: usize, flags: u32) -> i32), "IDirectInput8::EnumDevices")
}

/// `IDirectInputDevice8A/W`: Release 2, SetProperty 6, GetDeviceState 9, GetDeviceData
/// 10, SetDataFormat 11, SetCooperativeLevel 13, CreateEffect 18,
/// SendForceFeedbackCommand 22.
fn hook_device_object(dev: usize) -> usize {
    arm(&DEV_RELEASE, slot(dev, 2), per_slot!(DevRelease, [0, 1], dev_release, (this: usize) -> u32), "IDirectInputDevice8::Release")
        + arm(&DEV_PROPERTY, slot(dev, 6), per_slot!(DevSetProperty, [0, 1], set_property, (this: usize, prop: usize, value: *const u8) -> i32), "IDirectInputDevice8::SetProperty")
        + arm(&DEV_STATE, slot(dev, 9), per_slot!(DevGetState, [0, 1], dev_state, (this: usize, size: u32, out: *mut u8) -> i32), "IDirectInputDevice8::GetDeviceState")
        + arm(&DEV_DATA, slot(dev, 10), per_slot!(DevGetData, [0, 1], dev_data, (this: usize, size: u32, data: *mut u8, count: *mut u32, flags: u32) -> i32), "IDirectInputDevice8::GetDeviceData")
        + arm(&DEV_FORMAT, slot(dev, 11), per_slot!(DevSetDataFormat, [0, 1], set_data_format, (this: usize, format: *const DataFormat) -> i32), "IDirectInputDevice8::SetDataFormat")
        + arm(&DEV_COOP, slot(dev, 13), per_slot!(DevSetCooperativeLevel, [0, 1], set_cooperative_level, (this: usize, hwnd: isize, flags: u32) -> i32), "IDirectInputDevice8::SetCooperativeLevel")
        + arm(&DEV_EFFECT, slot(dev, 18), per_slot!(DevCreateEffect, [0, 1], create_effect, (this: usize, guid: *const [u8; 16], params: *const u8, out: *mut usize, outer: usize) -> i32), "IDirectInputDevice8::CreateEffect")
        + arm(&DEV_COMMAND, slot(dev, 22), per_slot!(DevSendCommand, [0, 1], send_command, (this: usize, flags: u32) -> i32), "IDirectInputDevice8::SendForceFeedbackCommand")
}

unsafe extern "system" fn create(instance: isize, version: u32, iid: *const [u8; 16], out: *mut usize, outer: usize) -> i32 {
    let Some(h) = CREATE.get() else { return E_FAIL };
    let hr = unsafe { h.call(instance, version, iid, out, outer) };
    if hr >= 0 && !out.is_null() && !iid.is_null() && matches!(unsafe { *iid }, IID_W | IID_A) {
        let di = unsafe { *out };
        if di != 0 {
            let n = hook_input_object(di);
            if n > 0 {
                log!("input: DirectInput8Create: {n} IDirectInput8 methods hooked (its devices' once the first is made)");
            }
        }
    }
    hr
}

/// What kind of device one is (`GetCapabilities`' device type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Keyboard,
    Mouse,
    Joystick,
    Other,
}

/// What a device's data format puts where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum What {
    /// X, Y, Z, Rx, Ry, Rz, then two sliders.
    Axis(u8),
    Pov(u8),
    Button(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Obj {
    ofs: u32,
    what: What,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Format {
    size: u32,
    objs: Vec<Obj>,
}

/// One device, as asked once (and its format as the game set it). Copied out of DEVS on
/// every read, so what's big is shared.
#[derive(Clone)]
struct Dev {
    this: usize,
    kind: Kind,
    hid: Option<device::Hid>,
    /// An XInput controller's HID side (`&IG_` in its path): XInput's routing decides.
    xinput_side: bool,
    format: Option<Arc<Format>>,
    /// Each format object's range (`DIPROP_RANGE`), asked at the first read.
    ranges: Option<Arc<[Option<(i32, i32)>]>>,
    /// The last state its buffered reads handed out (a joystick carrying the virtual pad).
    last: Option<Arc<[u8]>>,
}

static DEVS: Mutex<Vec<Dev>> = Mutex::new(Vec::new());

/// `DIDATAFORMAT`.
#[repr(C)]
pub(super) struct DataFormat {
    size: u32,
    obj_size: u32,
    flags: u32,
    data_size: u32,
    num_objs: u32,
    objs: *const ObjFormat,
}

/// `DIOBJECTDATAFORMAT`.
#[repr(C)]
struct ObjFormat {
    guid: *const [u8; 16],
    ofs: u32,
    ty: u32,
    flags: u32,
}

/// DIPROPGUIDANDPATH: DIPROPHEADER, the class GUID and the device's interface path.
#[repr(C)]
struct PropPath {
    size: u32,
    header_size: u32,
    obj: u32,
    how: u32,
    class: [u8; 16],
    path: [u16; 260],
}

/// DIPROPRANGE: DIPROPHEADER (size, header size, object, how) and the range.
#[repr(C)]
struct PropRange {
    size: u32,
    header_size: u32,
    obj: u32,
    how: u32,
    min: i32,
    max: i32,
}

/// Device `this`, asked once: its kind (`GetCapabilities`, slot 3) and, for a HID one,
/// which it is (`DIPROP_GUIDANDPATH` through `GetProperty`, slot 5).
fn dev(this: usize) -> Dev {
    if let Some(d) = DEVS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|d| d.this == this) {
        return d.clone();
    }
    type GetCaps = unsafe extern "system" fn(usize, *mut [u32; 11]) -> i32;
    let caps: GetCaps = unsafe { std::mem::transmute(slot(this, 3)) };
    let mut c = [0u32; 11];
    c[0] = 44;
    // DIDEVCAPS.dwDevType's low byte (DI8DEVTYPE_*).
    let kind = if unsafe { caps(this, &mut c) } < 0 {
        Kind::Other
    } else {
        match c[2] & 0xFF {
            0x13 => Kind::Keyboard,
            0x12 => Kind::Mouse,
            0x11 | 0x14..=0x1C => Kind::Joystick,
            _ => Kind::Other,
        }
    };
    type GetProperty = unsafe extern "system" fn(usize, usize, *mut PropPath) -> i32;
    let get: GetProperty = unsafe { std::mem::transmute(slot(this, 5)) };
    let mut p = PropPath { size: size_of::<PropPath>() as u32, header_size: 16, obj: 0, how: 0, class: [0; 16], path: [0; 260] };
    let (hid, xinput_side) = if kind != Kind::Keyboard && kind != Kind::Mouse && unsafe { get(this, DIPROP_GUIDANDPATH, &mut p) } >= 0 {
        let n = p.path.iter().position(|c| *c == 0).unwrap_or(p.path.len() - 1);
        let path = &p.path[..=n];
        (device::of_path(path), device::is_xinput_path(path))
    } else {
        (None, false)
    };
    let d = Dev { this, kind, hid, xinput_side, format: None, ranges: None, last: None };
    let mut devs = DEVS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(known) = devs.iter().find(|k| k.this == this) {
        return known.clone();
    }
    if devs.len() >= 32 {
        devs.remove(0);
    }
    devs.push(d.clone());
    d
}

fn update(this: usize, f: impl FnOnce(&mut Dev)) {
    if let Some(d) = DEVS.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|d| d.this == this) {
        f(d);
    }
}

/// The overlay closed: ranges are asked again at the next read.
pub(super) fn hidden() {
    for d in DEVS.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
        d.ranges = None;
    }
}

/// Which object `o` of a data format is: by its GUID (X axis ... POV, button), else by
/// its type (`DIDFT_AXIS`, `DIDFT_POV`, `DIDFT_BUTTON`), numbered in order.
fn parse_format(f: &DataFormat) -> Option<Format> {
    if f.objs.is_null() || f.num_objs > 1024 || f.data_size > 64 * 1024 || (f.obj_size as usize) < size_of::<ObjFormat>() {
        return None;
    }
    let mut objs = Vec::new();
    let (mut axes, mut sliders, mut povs, mut buttons) = (0u8, 0u8, 0u8, 0u8);
    for i in 0..f.num_objs as usize {
        let o = unsafe { &*((f.objs as usize + i * f.obj_size as usize) as *const ObjFormat) };
        let data1 = if o.guid.is_null() { None } else { Some(unsafe { o.guid.cast::<u32>().read_unaligned() }) };
        let what = match data1 {
            Some(0xA36D02E0) => Some(What::Axis(0)),
            Some(0xA36D02E1) => Some(What::Axis(1)),
            Some(0xA36D02E2) => Some(What::Axis(2)),
            Some(0xA36D02F4) => Some(What::Axis(3)),
            Some(0xA36D02F5) => Some(What::Axis(4)),
            Some(0xA36D02E3) => Some(What::Axis(5)),
            Some(0xA36D02E4) => {
                sliders += 1;
                Some(What::Axis(5 + sliders.min(2)))
            }
            Some(0xA36D02F2) => Some(What::Pov(0)),
            Some(0xA36D02F0) => Some(What::Button(0)),
            Some(_) => None,
            // DIDFT_AXIS, DIDFT_BUTTON, DIDFT_POV.
            None if o.ty & 0x3 != 0 => Some(What::Axis(8)),
            None if o.ty & 0xC != 0 => Some(What::Button(0)),
            None if o.ty & 0x10 != 0 => Some(What::Pov(0)),
            None => None,
        };
        let what = match what {
            // Any axis: the next one not named yet, in X ... Rz order.
            Some(What::Axis(8)) => {
                while axes < 8 && objs.iter().any(|o: &Obj| o.what == What::Axis(axes)) {
                    axes += 1;
                }
                (axes < 8).then_some(What::Axis(axes))
            }
            Some(What::Pov(_)) => {
                povs += 1;
                Some(What::Pov(povs - 1))
            }
            Some(What::Button(_)) => {
                buttons = buttons.saturating_add(1);
                Some(What::Button(buttons - 1))
            }
            w => w,
        };
        let width = if matches!(what, Some(What::Button(_))) { 1 } else { 4 };
        if let Some(what) = what
            && o.ofs + width <= f.data_size
        {
            objs.push(Obj { ofs: o.ofs, what });
        }
    }
    Some(Format { size: f.data_size, objs })
}

/// DirectInput's standard joystick format of `size` bytes (`DIJOYSTATE`, 80, or
/// `DIJOYSTATE2`, 272): eight axes, four hats, then 32 or 128 buttons.
fn standard_joystick(size: u32) -> Option<Arc<Format>> {
    static SMALL: OnceLock<Arc<Format>> = OnceLock::new();
    static LARGE: OnceLock<Arc<Format>> = OnceLock::new();
    let (cell, buttons) = match size {
        80 => (&SMALL, 32),
        272 => (&LARGE, 128),
        _ => return None,
    };
    Some(cell.get_or_init(|| Arc::new(build_standard_joystick(size, buttons))).clone())
}

fn build_standard_joystick(size: u32, buttons: u32) -> Format {
    let mut objs: Vec<Obj> = (0..8).map(|i| Obj { ofs: i * 4, what: What::Axis(i as u8) }).collect();
    objs.extend((0..4).map(|i| Obj { ofs: 32 + i * 4, what: What::Pov(i as u8) }));
    objs.extend((0..buttons).map(|i| Obj { ofs: 48 + i, what: What::Button(i as u8) }));
    Format { size, objs }
}

/// Each axis's `(min, max)` (`DIPROP_RANGE`, by offset), None for the rest.
fn ranges_of(this: usize, f: &Format) -> Vec<Option<(i32, i32)>> {
    type GetProperty = unsafe extern "system" fn(usize, usize, *mut PropRange) -> i32;
    let get: GetProperty = unsafe { std::mem::transmute(slot(this, 5)) };
    f.objs
        .iter()
        .map(|o| {
            if !matches!(o.what, What::Axis(_)) {
                return None;
            }
            // DIPH_BYOFFSET.
            let mut r = PropRange { size: 24, header_size: 16, obj: o.ofs, how: 1, min: 0, max: 0 };
            (unsafe { get(this, DIPROP_RANGE, &mut r) } >= 0 && r.max > r.min).then_some((r.min, r.max))
        })
        .collect()
}

pub(crate) fn middle(min: i32, max: i32) -> i32 {
    ((min as i64 + max as i64) / 2) as i32
}

fn map_axis(norm: f64, range: (i32, i32)) -> i32 {
    let (min, max) = (range.0 as f64, range.1 as f64);
    let t = ((norm.clamp(-1.0, 1.0) + 1.0) * 0.5).clamp(0.0, 1.0);
    (min + t * (max - min)).round() as i32
}

/// XInput's buttons on a joystick's first eleven (A, B, X, Y, LB, RB, Back, Start, the
/// sticks' clicks, Guide), as DirectInput lists an Xbox pad's.
const BTN_BITS: [u16; 11] = [0x1000, 0x2000, 0x4000, 0x8000, 0x0100, 0x0200, 0x0020, 0x0010, 0x0040, 0x0080, 0x0400];

/// The d-pad as a hat, hundredths of a degree (`u32::MAX`: centred).
fn pov_of(b: u16) -> u32 {
    match (b & 0x0001 != 0, b & 0x0002 != 0, b & 0x0004 != 0, b & 0x0008 != 0) {
        (true, false, false, false) => 0,
        (true, false, false, true) => 4500,
        (false, false, false, true) => 9000,
        (false, true, false, true) => 13500,
        (false, true, false, false) => 18000,
        (false, true, true, false) => 22500,
        (false, false, true, false) => 27000,
        (true, false, true, false) => 31500,
        _ => u32::MAX,
    }
}

/// A device's state in format `f`: `pad` as a joystick, or at rest (axes centred, hats
/// centred, buttons up) for None. Bytes no object of the format covers read 0.
fn pack(f: &Format, ranges: &[Option<(i32, i32)>], out: &mut [u8], pad: Option<&VirtualPad>) {
    let n = (f.size as usize).min(out.len());
    out[..n].fill(0);
    let b = pad.map_or(0, VirtualPad::xinput_buttons);
    for (i, o) in f.objs.iter().enumerate() {
        let at = o.ofs as usize;
        match o.what {
            What::Axis(a) => {
                let range = ranges.get(i).copied().flatten().unwrap_or((0, 65535));
                let v = match pad {
                    Some(p) => {
                        let norm = match a {
                            0 => f64::from(p.thumb_lx) / 32767.0,
                            1 => -f64::from(p.thumb_ly) / 32767.0,
                            2 => (f64::from(p.left_trigger) - f64::from(p.right_trigger)) / 255.0,
                            3 => f64::from(p.thumb_rx) / 32767.0,
                            4 => -f64::from(p.thumb_ry) / 32767.0,
                            5 => (f64::from(p.right_trigger) / 255.0) * 2.0 - 1.0,
                            _ => 0.0,
                        };
                        map_axis(norm, range)
                    }
                    None => middle(range.0, range.1),
                };
                if at + 4 <= n {
                    out[at..at + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            What::Pov(k) => {
                let v = if k == 0 && pad.is_some() { pov_of(b) } else { u32::MAX };
                if at + 4 <= n {
                    out[at..at + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            What::Button(k) => {
                let down = pad.is_some() && BTN_BITS.get(k as usize).is_some_and(|m| b & m != 0);
                if at < n {
                    out[at] = if down { 0x80 } else { 0 };
                }
            }
        }
    }
}

/// How many of a joystick's buttons read down in `state` (format `f`).
fn buttons_down(f: &Format, state: &[u8]) -> u32 {
    f.objs.iter().filter(|o| matches!(o.what, What::Button(_)) && state.get(o.ofs as usize).is_some_and(|b| b & 0x80 != 0)).count() as u32
}

/// The extended keys' DirectInput codes (`DIK_*`: their scan code with 0x80), by virtual
/// key: the layout's mapping gives the keypad key that shares each scan code (Left is
/// the keypad's 4 there).
const EXTENDED: [(u8, usize); 19] = [
    (0x13, 0xC5), // Pause (an E1 sequence)
    (0x21, 0xC9), // Page Up
    (0x22, 0xD1), // Page Down
    (0x23, 0xCF), // End
    (0x24, 0xC7), // Home
    (0x25, 0xCB), // Left
    (0x26, 0xC8), // Up
    (0x27, 0xCD), // Right
    (0x28, 0xD0), // Down
    (0x2C, 0xB7), // Print Screen
    (0x2D, 0xD2), // Insert
    (0x2E, 0xD3), // Delete
    (0x5B, 0xDB), // left Windows
    (0x5C, 0xDC), // right Windows
    (0x5D, 0xDD), // Menu
    (0x6F, 0xB5), // keypad divide
    (0xA3, 0x9D), // right Ctrl
    (0xA5, 0xB8), // right Alt
    (0x90, 0x45), // Num Lock
];

/// The DirectInput keyboard state (256 scan codes, `DIK_*`) of a virtual keyboard: an
/// extended key (arrows, the right Ctrl and Alt, Insert ... Page Down, the keypad's
/// divide) is its scan code with 0x80, as DirectInput numbers it; the rest are the
/// keyboard layout's scan codes.
pub(crate) fn pack_keyboard(out: &mut [u8], kb: &VirtualKeyboard) {
    let map = dik_map();
    for vk in 1u8..=254 {
        if kb.is_down(vk) {
            let sc = map[vk as usize].load(Ordering::Relaxed) as usize;
            if sc > 0 && sc < out.len() {
                out[sc] = 0x80;
            }
        }
    }
}

/// Each virtual key's `DIK_*` code (0: none) for the thread's keyboard layout, worked out
/// again only when the layout changes: a game polls the keyboard every frame.
fn dik_map() -> &'static [AtomicU8; 256] {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW};
    static MAP: [AtomicU8; 256] = [const { AtomicU8::new(0) }; 256];
    // The layout it was made for, plus one (0: not made yet).
    static FOR: AtomicUsize = AtomicUsize::new(0);
    let layout = (unsafe { GetKeyboardLayout(0) } as usize).wrapping_add(1);
    if FOR.load(Ordering::Acquire) != layout {
        for vk in 1u8..=254 {
            let sc = match EXTENDED.iter().find(|(v, _)| *v == vk) {
                Some(&(_, dik)) => dik,
                None => {
                    let ex = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) };
                    let low = (ex & 0xFF) as usize;
                    if low == 0 { 0 } else if ex & 0xFF00 == 0xE000 { low | 0x80 } else { low }
                }
            };
            MAP[vk as usize].store(sc.min(255) as u8, Ordering::Relaxed);
        }
        FOR.store(layout, Ordering::Release);
    }
    &MAP
}

/// The seat the virtual pad's joystick was chosen for (`super::seat_stamp`), the
/// joystick, and whether it was learnt (its own buttons went down with the copy's
/// controller's) rather than taken as the first one read.
static BOUND: Mutex<Option<(u64, usize, bool)>> = Mutex::new(None);

/// Whether joystick `this` carries the virtual pad: one does, the first one read for
/// the copy's seat, or the one whose buttons went down with the copy's controller's
/// (`own_down` and `real_down` buttons, as read now).
fn bound(this: usize, own_down: u32, real_down: u32) -> bool {
    let stamp = super::seat_stamp();
    let mut b = BOUND.lock().unwrap_or_else(|e| e.into_inner());
    let learnt = own_down > 0 && own_down == real_down;
    match *b {
        Some((s, d, true)) if s == stamp => d == this,
        Some((s, d, false)) if s == stamp && !learnt => d == this,
        Some((s, d, false)) if s == stamp && d == this => {
            log!("input: DirectInput joystick {this:#x} carries this copy's controller (its buttons went down with the controller's)");
            *b = Some((s, d, true));
            true
        }
        _ => {
            log!(
                "input: DirectInput joystick {this:#x} carries this copy's controller ({})",
                if learnt { "its buttons went down with the controller's" } else { "the first one read" }
            );
            *b = Some((stamp, this, learnt));
            true
        }
    }
}

/// The copy's controller as the virtual pad reads now: an XInput pad read in this
/// process (fresher than the session's copy of it), else the session's.
fn own_pad(snap: &InputSnapshot) -> VirtualPad {
    snap.pad.xinput_slot.and_then(|k| super::real_xinput(k.into()).map(|s| super::pad_of_xinput(k, &s))).unwrap_or(snap.pad)
}

/// What joystick `d` reads in split screen: its own state (the copy's own HID
/// controller), the virtual pad, or rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Joy {
    Own,
    Pad(VirtualPad),
    Rest,
}

fn joystick(d: &Dev, snap: &InputSnapshot, real_down: u32) -> Joy {
    if d.hid.is_some_and(|h| device::is_own(&h, snap)) {
        super::note_own_hid_read(snap.pad.hid_key);
        return Joy::Own;
    }
    // Another player's HID controller, or the copy's own HID one that the game reads
    // natively some other way (it would get its input twice).
    let others = d.hid.is_some_and(|h| device::is_others(&h, snap) && h.placed()) && !d.xinput_side;
    if !snap.pad.connected || others || (snap.pad.xinput_slot.is_none() && super::own_hid_read(snap.pad.hid_key)) {
        return Joy::Rest;
    }
    let pad = own_pad(snap);
    let own_down = BTN_BITS.iter().filter(|m| pad.xinput_buttons() & **m != 0).count() as u32;
    if bound(d.this, own_down, real_down) { Joy::Pad(pad) } else { Joy::Rest }
}

fn format_of(d: &Dev, size: u32) -> Option<Arc<Format>> {
    d.format.clone().filter(|f| f.size == size).or_else(|| standard_joystick(size))
}

fn ranges_for(d: &Dev, f: &Format) -> Arc<[Option<(i32, i32)>]> {
    if let Some(r) = d.ranges.as_ref().filter(|r| r.len() == f.objs.len()) {
        return r.clone();
    }
    let r: Arc<[Option<(i32, i32)>]> = ranges_of(d.this, f).into();
    update(d.this, |k| k.ranges = Some(r.clone()));
    r
}

fn dev_state(i: usize, this: usize, size: u32, out: *mut u8) -> i32 {
    let Some(h) = DEV_STATE.hooks[i].get() else { return E_FAIL };
    let rc = unsafe { h.call(this, size, out) };
    if rc < 0 || out.is_null() || size == 0 || size > 64 * 1024 {
        return rc;
    }
    let p = policy();
    if p == Policy::PassThrough {
        return rc;
    }
    let d = dev(this);
    let buf = unsafe { std::slice::from_raw_parts_mut(out, size as usize) };
    let kind = match d.kind {
        Kind::Other if size == 256 => Kind::Keyboard,
        Kind::Other if matches!(size, 16 | 20) => Kind::Mouse,
        Kind::Other if matches!(size, 80 | 272) => Kind::Joystick,
        k => k,
    };
    match (kind, p) {
        (Kind::Keyboard, Policy::Virtual(snap)) => {
            buf.fill(0);
            if snap.keyboard.active && size == 256 {
                pack_keyboard(buf, &snap.keyboard);
            }
        }
        (Kind::Mouse, Policy::Virtual(snap)) => {
            if !snap.mouse.active {
                buf.fill(0);
            }
        }
        (Kind::Keyboard | Kind::Mouse, _) => {
            HELD.fetch_add(1, Ordering::Relaxed);
            buf.fill(0);
        }
        (Kind::Joystick, p) => {
            let Some(f) = format_of(&d, size) else {
                // A format the hook can't place (set before it, not a standard one): a
                // controller that isn't the copy's reads nothing at all.
                if !matches!(p, Policy::Virtual(snap) if d.hid.is_some_and(|h| device::is_own(&h, &snap))) {
                    buf.fill(0);
                }
                return rc;
            };
            let r = ranges_for(&d, &f);
            match p {
                Policy::Virtual(snap) => {
                    READS.fetch_add(1, Ordering::Relaxed);
                    super::first_use(&super::USED_DINPUT, "DirectInput joysticks (its own reads as it is; one carries its controller, the others rest)");
                    match joystick(&d, &snap, buttons_down(&f, buf)) {
                        Joy::Own => {}
                        Joy::Pad(pad) => pack(&f, &r, buf, Some(&pad)),
                        Joy::Rest => pack(&f, &r, buf, None),
                    }
                }
                _ => {
                    HELD.fetch_add(1, Ordering::Relaxed);
                    pack(&f, &r, buf, None);
                }
            }
        }
        (Kind::Other, _) => {}
    }
    rc
}

/// `DIDEVICEOBJECTDATA`: offset, data, time stamp, sequence, then the app's value.
const EVENT_MIN: usize = 16;

/// Keeps the events `keep` takes from a buffered read's `n` events of `size` bytes each,
/// moved up; how many are kept.
fn keep_events(data: *mut u8, size: usize, n: u32, keep: impl Fn(u32, u32) -> bool) -> u32 {
    let mut w = 0usize;
    for r in 0..n as usize {
        let e = unsafe { data.add(r * size) };
        let (ofs, value) = unsafe { (*(e as *const u32), *(e.add(4) as *const u32)) };
        if keep(ofs, value) {
            if w != r {
                unsafe { std::ptr::copy(e, data.add(w * size), size) };
            }
            w += 1;
        }
    }
    w as u32
}

/// A buffered read's sequence numbers for the virtual pad's events.
static SEQUENCE: AtomicU32 = AtomicU32::new(1);

fn dev_data(i: usize, this: usize, size: u32, data: *mut u8, count: *mut u32, flags: u32) -> i32 {
    let Some(h) = DEV_DATA.hooks[i].get() else { return E_FAIL };
    let room = if count.is_null() { 0 } else { unsafe { *count } };
    let rc = unsafe { h.call(this, size, data, count, flags) };
    let p = policy();
    if p == Policy::PassThrough || rc < 0 || count.is_null() || data.is_null() || (size as usize) < EVENT_MIN {
        return rc;
    }
    let d = dev(this);
    let n = unsafe { *count };
    let sz = size as usize;
    match (d.kind, p) {
        (Kind::Keyboard, Policy::Virtual(snap)) if snap.keyboard.active => {}
        (Kind::Mouse, Policy::Virtual(snap)) if snap.mouse.active => {}
        // A key let go, or a mouse button (DIMOFS_BUTTON0..7), still reaches a copy that
        // doesn't have them now: it was pressed while it did.
        (Kind::Keyboard, _) => unsafe { *count = keep_events(data, sz, n, |_, v| v & 0x80 == 0) },
        (Kind::Mouse, _) => unsafe { *count = keep_events(data, sz, n, |ofs, v| (12..20).contains(&ofs) && v & 0x80 == 0) },
        (Kind::Joystick, Policy::Virtual(snap)) => {
            // Its own events are the device's own; any other joystick's are dropped,
            // and the one carrying the virtual pad gets its changes as events.
            let Some(f) = d.format.clone().or_else(|| standard_joystick(80)) else { return rc };
            match joystick(&d, &snap, 0) {
                Joy::Own => {}
                Joy::Rest => unsafe { *count = 0 },
                Joy::Pad(pad) => {
                    let r = ranges_for(&d, &f);
                    let mut now = vec![0u8; f.size as usize];
                    pack(&f, &r, &mut now, Some(&pad));
                    let before = d.last.clone().filter(|l| l.len() == now.len()).unwrap_or_else(|| {
                        let mut rest = vec![0u8; f.size as usize];
                        pack(&f, &r, &mut rest, None);
                        rest.into()
                    });
                    let mut given = before.to_vec();
                    let mut w = 0u32;
                    let at = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() };
                    for o in &f.objs {
                        if w >= room {
                            break;
                        }
                        let (a, width) = (o.ofs as usize, if matches!(o.what, What::Button(_)) { 1 } else { 4 });
                        if now[a..a + width] == before[a..a + width] {
                            continue;
                        }
                        let value = if width == 1 { u32::from(now[a]) } else { u32::from_le_bytes(now[a..a + 4].try_into().unwrap()) };
                        let e = unsafe { data.add(w as usize * sz) };
                        unsafe {
                            std::ptr::write_bytes(e, 0, sz);
                            *(e as *mut u32) = o.ofs;
                            *(e.add(4) as *mut u32) = value;
                            *(e.add(8) as *mut u32) = at;
                            *(e.add(12) as *mut u32) = SEQUENCE.fetch_add(1, Ordering::Relaxed);
                        }
                        given[a..a + width].copy_from_slice(&now[a..a + width]);
                        w += 1;
                    }
                    unsafe { *count = w };
                    if flags & DIGDD_PEEK == 0 {
                        update(this, |k| k.last = Some(given.into()));
                    }
                }
            }
        }
        (Kind::Joystick, _) => unsafe { *count = 0 },
        (Kind::Other, _) => {}
    }
    rc
}

unsafe extern "system" fn enum_one(inst: *const u8, rf: usize) -> i32 {
    let ctx = unsafe { &mut *(rf as *mut EnumCtx) };
    // DIDEVICEINSTANCE: dwSize, guidInstance (4), guidProduct (20), dwDevType (36).
    let ty = unsafe { *(inst.add(36) as *const u32) } & 0xFF;
    if (0x14..=0x1C).contains(&ty) && hidden_from_copy(ctx, unsafe { inst.add(4) } as *const [u8; 16]) {
        ctx.dropped += 1;
        // DIENUM_CONTINUE.
        return 1;
    }
    let f: unsafe extern "system" fn(*const u8, usize) -> i32 = unsafe { std::mem::transmute(ctx.callback) };
    unsafe { f(inst, ctx.context) }
}

struct EnumCtx {
    callback: usize,
    context: usize,
    di: usize,
    dropped: u32,
}

/// Whether the game controller `guid` is another player's HID one (made with the real
/// `CreateDevice` to ask its path, then let go).
fn hidden_from_copy(ctx: &EnumCtx, guid: *const [u8; 16]) -> bool {
    let Policy::Virtual(snap) = super::topology() else { return false };
    let Some(real) = real_create_device(ctx.di) else { return false };
    let mut d = 0usize;
    if unsafe { real.call(ctx.di, guid, &mut d, 0) } < 0 || d == 0 {
        return false;
    }
    let info = dev(d);
    release(d);
    info.hid.is_some_and(|h| device::is_others(&h, &snap) && h.placed()) && !info.xinput_side
}

fn enum_devices(i: usize, this: usize, kind: u32, cb: usize, rf: usize, flags: u32) -> i32 {
    let Some(h) = ENUM_DEVICES.hooks[i].get() else { return E_FAIL };
    if cb == 0 || !matches!(super::topology(), Policy::Virtual(_)) {
        return unsafe { h.call(this, kind, cb, rf, flags) };
    }
    let mut ctx = EnumCtx { callback: cb, context: rf, di: this, dropped: 0 };
    let hr = unsafe { h.call(this, kind, enum_one as *const () as usize, &mut ctx as *mut EnumCtx as usize, flags) };
    static TOLD: AtomicU32 = AtomicU32::new(u32::MAX);
    if TOLD.swap(ctx.dropped, Ordering::Relaxed) != ctx.dropped && ctx.dropped > 0 {
        log!("input: DirectInput's device list leaves out {} other players' controllers", ctx.dropped);
    }
    hr
}

fn create_device(i: usize, this: usize, guid: *const [u8; 16], out: *mut usize, outer: usize) -> i32 {
    let Some(h) = CREATE_DEVICE.hooks[i].get() else { return E_FAIL };
    let hr = unsafe { h.call(this, guid, out, outer) };
    if hr < 0 || out.is_null() || unsafe { *out } == 0 {
        return hr;
    }
    let made = unsafe { *out };
    let n = hook_device_object(made);
    if n > 0 {
        log!("input: DirectInput device made: {n} of its methods hooked");
    }
    if let Policy::Virtual(snap) = super::topology() {
        let d = dev(made);
        if d.kind == Kind::Joystick
            && !d.xinput_side
            && let Some(hid) = d.hid.filter(|h| device::is_others(h, &snap) && h.placed())
        {
            log!("input: the game makes DirectInput device HID {:04x}:{:04x}: another player's controller, not there for this copy", hid.vid, hid.pid);
            release(made);
            unsafe { *out = 0 };
            return DIERR_DEVICENOTREG;
        }
    }
    hr
}

fn dev_release(i: usize, this: usize) -> u32 {
    let Some(h) = DEV_RELEASE.hooks[i].get() else { return 0 };
    let left = unsafe { h.call(this) };
    if left == 0 {
        DEVS.lock().unwrap_or_else(|e| e.into_inner()).retain(|d| d.this != this);
        let mut b = BOUND.lock().unwrap_or_else(|e| e.into_inner());
        if b.is_some_and(|(_, d, _)| d == this) {
            *b = None;
        }
    }
    left
}

fn set_property(i: usize, this: usize, prop: usize, value: *const u8) -> i32 {
    let Some(h) = DEV_PROPERTY.hooks[i].get() else { return E_FAIL };
    let hr = unsafe { h.call(this, prop, value) };
    if hr >= 0 && prop == DIPROP_RANGE {
        update(this, |d| d.ranges = None);
    }
    hr
}

fn set_data_format(i: usize, this: usize, format: *const DataFormat) -> i32 {
    let Some(h) = DEV_FORMAT.hooks[i].get() else { return E_FAIL };
    let hr = unsafe { h.call(this, format) };
    if hr >= 0 && !format.is_null() {
        let f = parse_format(unsafe { &*format });
        let d = dev(this);
        if d.kind == Kind::Joystick
            && let Some(f) = &f
            && standard_joystick(f.size).as_deref() != Some(f)
        {
            log!("input: DirectInput joystick {this:#x} reads a format of its own ({} bytes, {} objects)", f.size, f.objs.len());
        }
        let f = f.map(Arc::new);
        update(this, |d| {
            d.format = f;
            d.ranges = None;
            d.last = None;
        });
    }
    hr
}

/// The cooperative level a split-screen copy asks for instead of `flags`: in the
/// background, as its pane's game is while another pane has the focus (DirectInput
/// stops a foreground device the moment its window isn't in front, whatever the game
/// is told). A keyboard or mouse can't be exclusive in the background.
pub(crate) fn background_level(flags: u32, keyboard_or_mouse: bool) -> u32 {
    if flags & DISCL_FOREGROUND == 0 {
        return flags;
    }
    let mut f = (flags & !(DISCL_FOREGROUND | DISCL_NOWINKEY)) | DISCL_BACKGROUND;
    if keyboard_or_mouse && f & DISCL_EXCLUSIVE != 0 {
        f = (f & !DISCL_EXCLUSIVE) | DISCL_NONEXCLUSIVE;
    }
    f
}

/// Exclusive access to a joystick that isn't the copy's own would keep another copy (its
/// player's) from it: shared instead.
pub(crate) fn shared_level(flags: u32) -> u32 {
    if flags & DISCL_EXCLUSIVE == 0 { flags } else { (flags & !DISCL_EXCLUSIVE) | DISCL_NONEXCLUSIVE }
}

/// Whether device `d` is the split-screen copy's own controller.
fn is_own(d: &Dev) -> bool {
    matches!(super::topology(), Policy::Virtual(snap) if d.hid.is_some_and(|h| device::is_own(&h, &snap)))
}

fn set_cooperative_level(i: usize, this: usize, hwnd: isize, flags: u32) -> i32 {
    let Some(h) = DEV_COOP.hooks[i].get() else { return E_FAIL };
    if crate::window::focus_spoofed() || super::shm::attached() {
        let d = dev(this);
        let mut want = background_level(flags, matches!(d.kind, Kind::Keyboard | Kind::Mouse));
        if d.kind == Kind::Joystick && super::shm::attached() && !is_own(&d) {
            want = shared_level(want);
        }
        if want != flags {
            let rc = unsafe { h.call(this, hwnd, want) };
            if rc >= 0 {
                static TOLD: AtomicBool = AtomicBool::new(false);
                if !TOLD.swap(true, Ordering::Relaxed) {
                    log!("input: DirectInput asked for access {flags:#x}; given {want:#x} (background, so its pane keeps its input while another has the focus; a joystick not this copy's own is shared)");
                }
                return rc;
            }
        }
    }
    unsafe { h.call(this, hwnd, flags) }
}

/// Force feedback in split screen: only on the copy's own controller (another player's
/// would shake in their hands).
fn feedback_refused(this: usize, what: &str) -> bool {
    if !matches!(super::topology(), Policy::Virtual(_)) {
        return false;
    }
    let d = dev(this);
    if is_own(&d) {
        return false;
    }
    crate::log::throttled(&format!("input: dinput ff {what}"), 1, |_| {
        log!("input: DirectInput {what} on a joystick that isn't this copy's own controller: refused");
    });
    true
}

fn create_effect(i: usize, this: usize, guid: *const [u8; 16], params: *const u8, out: *mut usize, outer: usize) -> i32 {
    let Some(h) = DEV_EFFECT.hooks[i].get() else { return E_FAIL };
    if feedback_refused(this, "CreateEffect") {
        if !out.is_null() {
            unsafe { *out = 0 };
        }
        return DIERR_UNSUPPORTED;
    }
    unsafe { h.call(this, guid, params, out, outer) }
}

fn send_command(i: usize, this: usize, flags: u32) -> i32 {
    let Some(h) = DEV_COMMAND.hooks[i].get() else { return E_FAIL };
    if feedback_refused(this, "SendForceFeedbackCommand") {
        return 0;
    }
    unsafe { h.call(this, flags) }
}

/// Controllers changed: which device is which is asked again, and the virtual pad's
/// joystick chosen again.
pub(super) fn devices_changed() {
    DEVS.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().for_each(|d| d.ranges = None);
    *BOUND.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dinput_joysticks_read_centred() {
        assert_eq!(middle(0, 65535), 32767);
        assert_eq!(middle(-1000, 1000), 0);
        assert_eq!(middle(i32::MIN, i32::MAX), 0);
        for size in [80u32, 272] {
            let f = standard_joystick(size).unwrap();
            let mut ranges = vec![None; f.objs.len()];
            ranges[0] = Some((0, 65535));
            ranges[1] = Some((0, 65535));
            ranges[3] = Some((-1000, 1000));
            ranges[4] = Some((-5, -5 + 2));
            let mut s = vec![0xAAu8; size as usize];
            pack(&f, &ranges, &mut s, None);
            let int = |o: usize| i32::from_le_bytes(s[o..o + 4].try_into().unwrap());
            assert_eq!([int(0), int(4), int(12), int(16)], [32767, 32767, 0, -4]);
            assert_eq!(int(8), 32767, "an axis with no range asked: the default one's middle");
            assert!((0..4).all(|k| int(32 + k * 4) == -1), "hats centred");
            assert!(s[48..].iter().all(|b| *b == 0), "buttons up, the rest 0");
        }
    }

    #[test]
    fn a_virtual_pad_packs_into_any_format() {
        let pad = VirtualPad {
            connected: true,
            buttons: 0x1000 | 0x0001 | 0x0008, // A + D-pad Up-Right
            left_trigger: 200,
            right_trigger: 50,
            thumb_lx: 32767,
            thumb_ly: 32767,
            thumb_rx: -32767,
            ..Default::default()
        };
        let f = standard_joystick(80).unwrap();
        let mut ranges = vec![None; f.objs.len()];
        ranges[3] = Some((-1000, 1000));
        let mut di = vec![0u8; 80];
        pack(&f, &ranges, &mut di, Some(&pad));
        let int = |o: usize| i32::from_le_bytes(di[o..o + 4].try_into().unwrap());
        assert_eq!(int(0), 65535, "LX right");
        assert_eq!(int(4), 0, "LY up (inverted in DI)");
        assert_eq!(int(12), -1000, "RX left");
        assert_eq!(int(32), 4500, "NE POV hat");
        assert_eq!((di[48], di[49]), (0x80, 0), "A down, B up");
        assert_eq!(buttons_down(&f, &di), 1);

        // A format of the game's own: buttons first, then an axis named by type alone,
        // then the Y axis by GUID, a hat at the end.
        let y = guid(0xA36D02E1, 0xC9F3, 0x11CF, [0xBF, 0xC7, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00]);
        let pov = guid(0xA36D02F2, 0xC9F3, 0x11CF, [0xBF, 0xC7, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00]);
        let objs = [
            ObjFormat { guid: std::ptr::null(), ofs: 0, ty: 0x0C | 0x00FF_FF00, flags: 0 },
            ObjFormat { guid: std::ptr::null(), ofs: 1, ty: 0x0C | 0x00FF_FF00, flags: 0 },
            ObjFormat { guid: std::ptr::null(), ofs: 4, ty: 0x03 | 0x00FF_FF00, flags: 0 },
            ObjFormat { guid: &y, ofs: 8, ty: 0x03 | 0x00FF_FF00, flags: 0 },
            ObjFormat { guid: &pov, ofs: 12, ty: 0x10 | 0x00FF_FF00, flags: 0 },
            ObjFormat { guid: std::ptr::null(), ofs: 30, ty: 0x03, flags: 0 },
        ];
        let df = DataFormat { size: 24 + size_of::<usize>() as u32, obj_size: size_of::<ObjFormat>() as u32, flags: 1, data_size: 16, num_objs: objs.len() as u32, objs: objs.as_ptr() };
        let f = parse_format(&df).unwrap();
        assert_eq!(
            f.objs,
            vec![
                Obj { ofs: 0, what: What::Button(0) },
                Obj { ofs: 1, what: What::Button(1) },
                Obj { ofs: 4, what: What::Axis(0) },
                Obj { ofs: 8, what: What::Axis(1) },
                Obj { ofs: 12, what: What::Pov(0) },
            ],
            "the axis past the data's end is left out"
        );
        let mut s = vec![0u8; 16];
        pack(&f, &[None; 5], &mut s, Some(&pad));
        assert_eq!((s[0], s[1]), (0x80, 0));
        assert_eq!(i32::from_le_bytes(s[4..8].try_into().unwrap()), 65535);
        assert_eq!(u32::from_le_bytes(s[12..16].try_into().unwrap()), 4500);
    }

    #[test]
    fn buffered_events_are_filtered_in_place() {
        // Three keyboard events of 24 bytes: A down, A up, B down.
        let mut buf = vec![0u8; 72];
        for (i, (ofs, v)) in [(0x1Eu32, 0x80u32), (0x1E, 0), (0x30, 0x80)].into_iter().enumerate() {
            buf[i * 24..i * 24 + 4].copy_from_slice(&ofs.to_le_bytes());
            buf[i * 24 + 4..i * 24 + 8].copy_from_slice(&v.to_le_bytes());
            buf[i * 24 + 12] = i as u8;
        }
        let kept = keep_events(buf.as_mut_ptr(), 24, 3, |_, v| v & 0x80 == 0);
        assert_eq!(kept, 1, "the release alone");
        assert_eq!((u32::from_le_bytes(buf[0..4].try_into().unwrap()), buf[12]), (0x1E, 1));
    }

    /// DirectInput's foreground access becomes background access in a split-screen
    /// copy; a keyboard or mouse can't stay exclusive there.
    #[test]
    fn dinput_asks_for_the_background() {
        assert_eq!(background_level(DISCL_EXCLUSIVE | DISCL_FOREGROUND, false), DISCL_EXCLUSIVE | DISCL_BACKGROUND, "a joystick keeps exclusive (force feedback needs it)");
        assert_eq!(background_level(DISCL_EXCLUSIVE | DISCL_FOREGROUND | DISCL_NOWINKEY, true), DISCL_NONEXCLUSIVE | DISCL_BACKGROUND);
        assert_eq!(background_level(DISCL_NONEXCLUSIVE | DISCL_FOREGROUND, true), DISCL_NONEXCLUSIVE | DISCL_BACKGROUND);
        assert_eq!(background_level(DISCL_NONEXCLUSIVE | DISCL_BACKGROUND, true), DISCL_NONEXCLUSIVE | DISCL_BACKGROUND, "background already");
        assert_eq!(shared_level(DISCL_EXCLUSIVE | DISCL_BACKGROUND), DISCL_NONEXCLUSIVE | DISCL_BACKGROUND, "another player's joystick is shared");
    }

    #[test]
    fn extended_keys_get_their_dinput_codes() {
        let mut kb = VirtualKeyboard { active: true, ..Default::default() };
        for vk in [0x25u8, 0xA3, 0x20, 0x13] {
            kb.set(vk, true); // Left, right Ctrl, Space, Pause
        }
        let mut s = [0u8; 256];
        pack_keyboard(&mut s, &kb);
        let down: Vec<usize> = (0..256).filter(|i| s[*i] != 0).collect();
        // DIK_SPACE 0x39, DIK_RCONTROL 0x9D, DIK_PAUSE 0xC5, DIK_LEFT 0xCB.
        assert_eq!(down, vec![0x39, 0x9D, 0xC5, 0xCB]);
    }

    #[test]
    fn the_first_joystick_read_carries_the_pad_until_the_right_one_shows() {
        *BOUND.lock().unwrap() = None;
        assert!(bound(0xA, 0, 0), "the first one read");
        assert!(!bound(0xB, 0, 0));
        assert!(!bound(0xB, 2, 1), "its buttons don't match the controller's");
        assert!(bound(0xB, 2, 2), "its buttons went down with the controller's: learnt");
        assert!(!bound(0xA, 1, 1), "learnt holds");
        *BOUND.lock().unwrap() = None;
    }
}
