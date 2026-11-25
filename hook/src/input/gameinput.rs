//! GameInput (the GDK's input API, so most Microsoft Store games): driven by `input::policy()`.
//!
//! While the overlay has the controllers (`Policy::BlockAll`), `IGameInput::GetCurrentReading`
//! keeps returning the reading it gave when held, `GetNextReading` finds nothing newer,
//! and reading callbacks aren't called.
//!
//! In a split-screen copy (`Policy::Virtual`) a controller reading reaches the game only
//! when it's the copy's own controller's. GameInput doesn't say which device is which
//! player's, so the copy's own is learnt: the device whose gamepad buttons go down with
//! the copy's controller's (the session's state of it, or its XInput slot read here).
//! Until then its controller readings aren't there; after, a reading of another device
//! is skipped over (the copy's own latest one found further back, `GetPreviousReading`,
//! or the next one of its own, `GetNextReading`), callbacks for other devices aren't
//! called, and rumble reaches the copy's own device alone. The device and its readings
//! are asked through the interfaces the Windows SDK's GameInput.h describes (v0), which
//! the later versions' objects answer too; where they don't, the copy's controller
//! readings are held at rest, and the log says so.
//!
//! Nothing here calls into GameInput from a load notification: `GameInputCreate` and
//! `GameInputInitialize` are hooked by export, and the objects they return give the rest.

use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use peerparty_protocol::input_shm::{InputSnapshot, VirtualPad};

use super::{Policy, Slots, arm, module, policy, proc_addr};
use crate::log;

type Current = unsafe extern "system" fn(usize, u32, usize, *mut usize) -> i32;
type Next = unsafe extern "system" fn(usize, usize, u32, usize, *mut usize) -> i32;
/// v0: (this, device, kind, analogThreshold, context, callback, token).
type RegisterV0 = unsafe extern "system" fn(usize, usize, u32, f32, usize, usize, *mut u64) -> i32;
/// v1 and later: (this, device, kind, context, callback, token).
type Register = unsafe extern "system" fn(usize, usize, u32, usize, usize, *mut u64) -> i32;
/// v0..v3: (this, device, kind, statusFilter, enumerationKind, context, callback, token).
type RegisterDevice = unsafe extern "system" fn(usize, usize, u32, u32, u32, usize, usize, *mut u64) -> i32;
/// v0..v3: (this, token).
type StopCallback = unsafe extern "system" fn(usize, u64);
/// v0: (this, token, timeout in microseconds): whether it's gone.
type UnregisterV0 = unsafe extern "system" fn(usize, u64, u64) -> bool;
/// v1 and later: (this, token).
type Unregister = unsafe extern "system" fn(usize, u64) -> bool;
/// v0's reading callback: (token, context, reading, hasOverrunOccurred).
type CallbackV0 = unsafe extern "system" fn(u64, usize, usize, bool);
/// v1 and later: (token, context, reading).
type Callback = unsafe extern "system" fn(u64, usize, usize);
/// v0..v3's device callback: (token, context, device, timestamp, currentStatus, previousStatus).
type DeviceCallback = unsafe extern "system" fn(u64, usize, usize, u64, u32, u32);
/// `IGameInputDevice::SetRumbleState(const GameInputRumbleParams*)`.
type Rumble = unsafe extern "system" fn(usize, *const u8);

type Create = unsafe extern "system" fn(*mut usize) -> i32;
type Initialize = unsafe extern "system" fn(*const [u8; 16], *mut usize) -> i32;

static CREATE: Slots<Create, 2> = Slots::new();
static INITIALIZE: Slots<Initialize, 2> = Slots::new();
static BUSY: AtomicBool = AtomicBool::new(false);
static CURRENT: Slots<Current, 8> = Slots::new();
static NEXT: Slots<Next, 8> = Slots::new();
static REGISTER_V0: Slots<RegisterV0, 2> = Slots::new();
static REGISTER: Slots<Register, 6> = Slots::new();
static REGISTER_DEVICE: Slots<RegisterDevice, 8> = Slots::new();
static STOP_CALLBACK: Slots<StopCallback, 8> = Slots::new();
static UNREGISTER_V0: Slots<UnregisterV0, 2> = Slots::new();
static UNREGISTER: Slots<Unregister, 6> = Slots::new();
static RUMBLE: Slots<Rumble, 2> = Slots::new();
/// Readings held while blocked: (kind, device, reading, AddRef'd).
static HELD: Mutex<Vec<(u32, usize, usize)>> = Mutex::new(Vec::new());
/// The vtables of the v0..v3 interfaces found (`GetPreviousReading` is at slot 6 there).
static PREV_VTABLES: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Reading callbacks the game registered: (token, its relay, which `Relay::free` frees).
static RELAYS: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());
/// Device callbacks the game registered: (raw Arc ptr passed to GameInput, Arc handle).
static DEVICE_RELAYS: Mutex<Vec<(usize, Arc<DeviceRelay>)>> = Mutex::new(Vec::new());
/// Connected controller devices seen in device callbacks: (dev_id, AddRef'd device, timestamp, status).
static SEEN_DEVICES: Mutex<Vec<SeenDevice>> = Mutex::new(Vec::new());
/// Wakes the background device probe when a controller is held back before `OWN` is known.
static PROBE_WAKE: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());
static PROBE_STARTED: AtomicBool = AtomicBool::new(false);

/// GameInputKindKeyboard, GameInputKindMouse, GameInputKindGamepad, GameInputDeviceConnected.
const KEYBOARD: u32 = 0x10;
const MOUSE: u32 = 0x20;
const GAMEPAD: u32 = 0x0004_0000;
const DEVICE_CONNECTED: u32 = 0x0010_0000;

/// Whether reads of `kind` are held back under `BlockAll` (anything but keyboard and
/// mouse alone; 0 is "any kind", so held).
fn held_kind(kind: u32) -> bool {
    kind == 0 || kind & !(KEYBOARD | MOUSE) != 0
}

/// Whether `kind` should be held under the current `Policy` (a split-screen copy's
/// controller readings are filtered by device instead: `own_reading`).
fn should_hold(kind: u32) -> bool {
    match policy() {
        Policy::PassThrough => false,
        Policy::BlockAll => held_kind(kind),
        Policy::Virtual(snap) => {
            if held_kind(kind) {
                return false;
            }
            let wants_kb = kind & KEYBOARD != 0;
            let wants_mouse = kind & MOUSE != 0;
            (wants_kb && !snap.keyboard.active) || (wants_mouse && !snap.mouse.active)
        }
    }
}

/// In a split-screen copy, the snapshot when `kind` reads controllers (their readings
/// are the copy's own device's alone).
fn filtered(kind: u32) -> Option<InputSnapshot> {
    match policy() {
        Policy::Virtual(snap) if held_kind(kind) => {
            super::first_use(&super::USED_GAMEINPUT, "GameInput controllers (its own device's readings only)");
            Some(snap)
        }
        _ => None,
    }
}

/// GAMEINPUT_E_READING_NOT_FOUND.
const NOT_FOUND: i32 = 0x838A_0003_u32 as i32;

const DLLS: [&str; 2] = ["gameinput.dll", "gameinputredist.dll"];

const IID_V0: [u8; 16] = super::guid(0x11BE2A7E, 0x4254, 0x445A, [0x9C, 0x09, 0xFF, 0xC4, 0x0F, 0x00, 0x69, 0x18]);
const IID_LATER: [[u8; 16]; 3] = [
    super::guid(0x40FFB7E4, 0x6150, 0x407A, [0xB4, 0x39, 0x13, 0x2B, 0xAD, 0xC0, 0x8D, 0x2D]),
    super::guid(0xBBAA66D2, 0x837A, 0x40F7, [0xA3, 0x03, 0x91, 0x7D, 0x50, 0x09, 0x55, 0xF4]),
    super::guid(0x20EFC1C7, 0x5D9A, 0x43BA, [0xB2, 0x6F, 0xB8, 0x07, 0xFA, 0x48, 0x60, 0x9C]),
];
/// IGameInputReading and IGameInputDevice as GameInput.h (v0) describes them.
const IID_READING_V0: [u8; 16] = super::guid(0x2156947A, 0xE1FA, 0x4DE0, [0xA3, 0x0B, 0xD8, 0x12, 0x93, 0x1D, 0xBD, 0x8D]);
const IID_DEVICE_V0: [u8; 16] = super::guid(0x31DD86FB, 0x4C1B, 0x408A, [0x86, 0x8F, 0x43, 0x9B, 0x3C, 0xD4, 0x71, 0x25]);
const IID_UNKNOWN: [u8; 16] = super::guid(0, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

pub fn module_loaded(name: &str) {
    let Some(m) = module(name) else { return };
    let mut n = 0;
    if let Some(a) = proc_addr(m, c"GameInputCreate".as_ptr().cast()) {
        n += arm(&CREATE, a, create_detours(), "GameInputCreate");
    }
    if let Some(a) = proc_addr(m, c"GameInputInitialize".as_ptr().cast()) {
        n += arm(&INITIALIZE, a, initialize_detours(), "GameInputInitialize");
    }
    if n > 0 {
        log!("input: {name}: {n} GameInput entry points hooked (its objects' methods once the game makes one)");
    }
}

fn create_detours() -> [Create; 2] {
    per_slot!(Create, [0, 1], create, (out: *mut usize) -> i32)
}

fn create(i: usize, out: *mut usize) -> i32 {
    let Some(h) = CREATE.hooks[i].get() else { return NOT_FOUND };
    let hr = unsafe { h.call(out) };
    if hr >= 0 && !out.is_null() {
        created(unsafe { *out }, true);
    }
    hr
}

fn initialize_detours() -> [Initialize; 2] {
    per_slot!(Initialize, [0, 1], initialize, (iid: *const [u8; 16], out: *mut usize) -> i32)
}

fn initialize(i: usize, iid: *const [u8; 16], out: *mut usize) -> i32 {
    let Some(h) = INITIALIZE.hooks[i].get() else { return NOT_FOUND };
    let hr = unsafe { h.call(iid, out) };
    if hr >= 0 && !out.is_null() {
        created(unsafe { *out }, false);
    }
    hr
}

/// The game made GameInput object `obj` (`legacy`: through `GameInputCreate`, which
/// answers the v0 interface and maybe later ones): its methods are hooked through it,
/// on the game's own thread (never the loader lock).
fn created(obj: usize, legacy: bool) {
    if obj == 0 || crate::watch::in_loader() || BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut found = Vec::new();
    collect(obj, legacy, &mut found);
    let n = hook_found(&found);
    BUSY.store(false, Ordering::Release);
    if n > 0 {
        log!("input: GameInput created: {n} of its methods hooked ({} interface versions)", found.len());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Api {
    V0,
    V1To3,
    Unknown,
}

struct Found {
    api: Api,
    vt: *const usize,
}

/// The interface versions object `obj` answers (each QueryInterface'd and let go).
fn collect(obj: usize, legacy: bool, found: &mut Vec<Found>) {
    let mut objects = Vec::new();
    if let Some(o) = unsafe { query(obj, &IID_V0) } {
        objects.push((Api::V0, o));
    }
    for iid in &IID_LATER {
        if let Some(o) = unsafe { query(obj, iid) } {
            objects.push((Api::V1To3, o));
        }
    }
    if objects.is_empty() && !legacy {
        // An object of a version not known here: hooked conservatively.
        objects.push((Api::Unknown, obj));
        unsafe { add_ref(obj) };
    }
    for (api, o) in objects {
        let vt = unsafe { *(o as *const *const usize) };
        if !found.iter().any(|f: &Found| f.vt == vt) {
            found.push(Found { api, vt });
        }
        unsafe { release(o) };
    }
}

fn versions() -> Vec<Found> {
    let mut found = Vec::new();
    for dll in DLLS {
        let Some(m) = module(dll) else { continue };
        if let Some(create) = proc_addr(m, c"GameInputCreate".as_ptr().cast()) {
            let mut legacy = 0usize;
            if call_create(create, &mut legacy) >= 0 && legacy != 0 {
                collect(legacy, true, &mut found);
                unsafe { release(legacy) };
            }
        }
        if let Some(init) = proc_addr(m, c"GameInputInitialize".as_ptr().cast()) {
            for iid in &IID_LATER {
                let mut o = 0usize;
                if call_initialize(init, iid, &mut o) >= 0 && o != 0 {
                    collect(o, false, &mut found);
                    unsafe { release(o) };
                }
            }
        }
    }
    found
}

/// Calls the real `GameInputCreate` at `addr` (through its hook's original once hooked:
/// the hook would hook the object it makes, which `versions` does itself).
fn call_create(addr: usize, out: &mut usize) -> i32 {
    let i = CREATE.addrs.lock().unwrap().iter().position(|a| *a == addr);
    match i.and_then(|i| CREATE.hooks[i].get()) {
        Some(h) => unsafe { h.call(out) },
        None => unsafe { std::mem::transmute::<usize, Create>(addr)(out) },
    }
}

fn call_initialize(addr: usize, iid: &[u8; 16], out: &mut usize) -> i32 {
    let i = INITIALIZE.addrs.lock().unwrap().iter().position(|a| *a == addr);
    match i.and_then(|i| INITIALIZE.hooks[i].get()) {
        Some(h) => unsafe { h.call(iid, out) },
        None => unsafe { std::mem::transmute::<usize, Initialize>(addr)(iid, out) },
    }
}

/// Calls the real `IGameInput::GetCurrentReading` on `gi` (bypassing our detour).
fn call_current(gi: usize, kind: u32, device: usize, out: &mut usize) -> i32 {
    let addr = unsafe { *(*(gi as *const *const usize)).add(4) };
    let i = CURRENT.addrs.lock().unwrap_or_else(|e| e.into_inner()).iter().position(|a| *a == addr);
    match i.and_then(|i| CURRENT.hooks[i].get()) {
        Some(h) => unsafe { h.call(gi, kind, device, out) },
        None => unsafe { std::mem::transmute::<usize, Current>(addr)(gi, kind, device, out) },
    }
}

unsafe fn query(obj: usize, iid: &[u8; 16]) -> Option<usize> {
    let vt = unsafe { *(obj as *const *const usize) };
    let qi: unsafe extern "system" fn(usize, *const [u8; 16], *mut usize) -> i32 = unsafe { std::mem::transmute(*vt) };
    let mut out = 0usize;
    (unsafe { qi(obj, iid, &mut out) } >= 0 && out != 0).then_some(out)
}

/// Hooks the methods of every interface version GameInput's DLLs answer (by objects of
/// the hook's own). Never under the loader lock (`input::install` sees to it).
pub fn install() -> usize {
    if crate::watch::in_loader() || BUSY.swap(true, Ordering::AcqRel) {
        return 0;
    }
    let found = versions();
    let n = hook_found(&found);
    BUSY.store(false, Ordering::Release);
    n
}

/// v0: GetCurrentReading 4, GetNextReading 5, GetPreviousReading 6,
/// RegisterReadingCallback 8, RegisterDeviceCallback 9, StopCallback 12,
/// UnregisterCallback 13.
/// v1..v3 drop GetTemporalReading (7), so GetPreviousReading stays at 6,
/// RegisterReadingCallback is 7, RegisterDeviceCallback is 8, StopCallback is 11, and
/// UnregisterCallback is 12 (taking no timeout).
fn hook_found(found: &[Found]) -> usize {
    let mut n = 0;
    for f in found {
        if matches!(f.api, Api::V0 | Api::V1To3) {
            let mut prev = PREV_VTABLES.lock().unwrap_or_else(|e| e.into_inner());
            if !prev.contains(&(f.vt as usize)) {
                prev.push(f.vt as usize);
            }
        }
        n += arm(&CURRENT, unsafe { *f.vt.add(4) }, current_detours(), "IGameInput::GetCurrentReading");
        n += arm(&NEXT, unsafe { *f.vt.add(5) }, next_detours(), "IGameInput::GetNextReading");
        n += if f.api == Api::V0 {
            arm(&REGISTER_V0, unsafe { *f.vt.add(8) }, register_v0_detours(), "IGameInput::RegisterReadingCallback (v0)")
        } else {
            arm(&REGISTER, unsafe { *f.vt.add(7) }, register_detours(), "IGameInput::RegisterReadingCallback")
        };
        match f.api {
            Api::V0 => {
                n += arm(&REGISTER_DEVICE, unsafe { *f.vt.add(9) }, register_device_detours(), "IGameInput::RegisterDeviceCallback (v0)");
                n += arm(&STOP_CALLBACK, unsafe { *f.vt.add(12) }, stop_callback_detours(), "IGameInput::StopCallback (v0)");
                n += arm(&UNREGISTER_V0, unsafe { *f.vt.add(13) }, per_slot!(UnregisterV0, [0, 1], unregister_v0, (this: usize, token: u64, timeout: u64) -> bool), "IGameInput::UnregisterCallback (v0)");
            }
            Api::V1To3 => {
                n += arm(&REGISTER_DEVICE, unsafe { *f.vt.add(8) }, register_device_detours(), "IGameInput::RegisterDeviceCallback");
                n += arm(&STOP_CALLBACK, unsafe { *f.vt.add(11) }, stop_callback_detours(), "IGameInput::StopCallback");
                n += arm(&UNREGISTER, unsafe { *f.vt.add(12) }, per_slot!(Unregister, [0, 1, 2, 3, 4, 5], unregister, (this: usize, token: u64) -> bool), "IGameInput::UnregisterCallback");
            }
            Api::Unknown => {
                n += arm(&UNREGISTER, unsafe { *f.vt.add(12) }, per_slot!(Unregister, [0, 1, 2, 3, 4, 5], unregister, (this: usize, token: u64) -> bool), "IGameInput::UnregisterCallback");
            }
        }
    }
    n
}

unsafe fn release(obj: usize) {
    let vt = unsafe { *(obj as *const *const usize) };
    let f: unsafe extern "system" fn(usize) -> u32 = unsafe { std::mem::transmute(*vt.add(2)) };
    unsafe { f(obj) };
}

unsafe fn add_ref(obj: usize) {
    let vt = unsafe { *(obj as *const *const usize) };
    let f: unsafe extern "system" fn(usize) -> u32 = unsafe { std::mem::transmute(*vt.add(1)) };
    unsafe { f(obj) };
}

pub fn hidden() {
    let held: Vec<(u32, usize, usize)> = HELD.lock().unwrap().drain(..).collect();
    for (_, _, r) in held {
        unsafe { release(r) };
    }
}

/// Controllers changed: the copy's own device is learnt again.
pub(super) fn devices_changed() {
    *OWN.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if !DEVICE_RELAYS.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
        && !SEEN_DEVICES.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    {
        wake_probe();
    }
}

/// The copy's own device (its identity: its IUnknown), learnt for one seat
/// (`super::seat_stamp`).
static OWN: Mutex<Option<(u64, usize)>> = Mutex::new(None);

/// `GameInputGamepadState` (28 bytes, identical across v0..v3).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct GamepadState {
    buttons: u32,
    lt: f32,
    rt: f32,
    lx: f32,
    ly: f32,
    rx: f32,
    ry: f32,
}

const ANALOG_LEARN_THRESHOLD: f32 = 0.5;
const ANALOG_MATCH_TOL: f32 = 0.15;

/// A reading's device's identity (its IUnknown), through the v0 reading interface
/// (`GetDevice`, slot 6). None when the reading doesn't answer it.
fn device_of(reading: usize) -> Option<usize> {
    let r = unsafe { query(reading, &IID_READING_V0) }?;
    let get: unsafe extern "system" fn(usize, *mut usize) = unsafe { std::mem::transmute(*(*(r as *const *const usize)).add(6)) };
    let mut dev = 0usize;
    unsafe { get(r, &mut dev) };
    unsafe { release(r) };
    if dev == 0 {
        return None;
    }
    let id = identity(dev);
    hook_rumble(dev);
    unsafe { release(dev) };
    id
}

/// An object's identity: its IUnknown (the same whichever interface it's asked through).
fn identity(obj: usize) -> Option<usize> {
    let u = unsafe { query(obj, &IID_UNKNOWN) }?;
    unsafe { release(u) };
    Some(u)
}

/// Whether `device` supports controller input (as opposed to keyboard/mouse only).
/// Reads `IGameInputDevice::GetDeviceInfo` (slot 3 of `IID_DEVICE_V0`, where
/// `supportedInput` is a `u32` at byte offset 104 on both x86 and x64). Defaults to
/// `true` if unknown so a controller is never leaked as keyboard/mouse.
fn device_is_controller(device: usize) -> bool {
    let Some(d) = (unsafe { query(device, &IID_DEVICE_V0) }) else { return true };
    let get_info: unsafe extern "system" fn(usize) -> *const u8 = unsafe { std::mem::transmute(*(*(d as *const *const usize)).add(3)) };
    let info = unsafe { get_info(d) };
    let supported = if !info.is_null() {
        let info_size = unsafe { *(info as *const u16) };
        (info_size >= 108).then(|| unsafe { *(info.add(104) as *const u32) })
    } else {
        None
    };
    unsafe { release(d) };
    supported.map_or(true, held_kind)
}

/// A reading's gamepad state (`GetGamepadState`, slot 22 of the v0 reading); None for a
/// reading that isn't a gamepad's.
fn gamepad_state(reading: usize) -> Option<GamepadState> {
    let r = unsafe { query(reading, &IID_READING_V0) }?;
    let get: unsafe extern "system" fn(usize, *mut GamepadState) -> bool = unsafe { std::mem::transmute(*(*(r as *const *const usize)).add(22)) };
    let mut s = GamepadState::default();
    let ok = unsafe { get(r, &mut s) };
    unsafe { release(r) };
    ok.then_some(s)
}

fn own_pad(snap: &InputSnapshot) -> VirtualPad {
    snap.pad.xinput_slot.and_then(|k| super::real_xinput(k.into()).map(|s| super::pad_of_xinput(k, &s))).unwrap_or(snap.pad)
}

fn pad_to_gamepad_state(pad: &VirtualPad) -> GamepadState {
    GamepadState {
        buttons: super::wgi::wgi_buttons(pad.xinput_buttons()),
        lt: f32::from(pad.left_trigger) / 255.0,
        rt: f32::from(pad.right_trigger) / 255.0,
        lx: (f32::from(pad.thumb_lx) / 32767.0).clamp(-1.0, 1.0),
        ly: (f32::from(pad.thumb_ly) / 32767.0).clamp(-1.0, 1.0),
        rx: (f32::from(pad.thumb_rx) / 32767.0).clamp(-1.0, 1.0),
        ry: (f32::from(pad.thumb_ry) / 32767.0).clamp(-1.0, 1.0),
    }
}

fn pad_active(s: &GamepadState) -> bool {
    s.buttons != 0
        || s.lt > ANALOG_LEARN_THRESHOLD
        || s.rt > ANALOG_LEARN_THRESHOLD
        || s.lx.abs() > ANALOG_LEARN_THRESHOLD
        || s.ly.abs() > ANALOG_LEARN_THRESHOLD
        || s.rx.abs() > ANALOG_LEARN_THRESHOLD
        || s.ry.abs() > ANALOG_LEARN_THRESHOLD
}

/// Whether `got` (from a GameInput reading) matches `want` (the copy's own controller):
/// either matching non-zero buttons, or a decisive stick/trigger deflection (> 0.5) where
/// buttons and all six analog axes agree within tolerance.
fn gamepad_matches(want: &GamepadState, got: &GamepadState) -> bool {
    if want.buttons != 0 {
        return got.buttons == want.buttons;
    }
    pad_active(want)
        && got.buttons == 0
        && (want.lt - got.lt).abs() <= ANALOG_MATCH_TOL
        && (want.rt - got.rt).abs() <= ANALOG_MATCH_TOL
        && (want.lx - got.lx).abs() <= ANALOG_MATCH_TOL
        && (want.ly - got.ly).abs() <= ANALOG_MATCH_TOL
        && (want.rx - got.rx).abs() <= ANALOG_MATCH_TOL
        && (want.ry - got.ry).abs() <= ANALOG_MATCH_TOL
}

fn own_for_seat() -> Option<usize> {
    let stamp = super::seat_stamp();
    OWN.lock().unwrap_or_else(|e| e.into_inner()).and_then(|(s, d)| (s == stamp).then_some(d))
}

/// Learns `dev` as this copy's controller for the current seat (unless another device was
/// already learnt for it) and replays any held device callbacks.
fn learn_own(dev: usize, how: &str) -> bool {
    let stamp = super::seat_stamp();
    let learned = {
        let mut own = OWN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((s, d)) = *own
            && s == stamp
        {
            return d == dev;
        }
        *own = Some((stamp, dev));
        true
    };
    if learned {
        log!("input: GameInput device {dev:#x} is this copy's controller ({how}): its readings reach the game, the others' don't");
        replay_device_callbacks(dev);
    }
    true
}

/// Whether `reading` is the copy's own device's; learns the device when its buttons or
/// sticks/triggers move with the copy's controller's.
fn own_reading(reading: usize, snap: &InputSnapshot) -> bool {
    if !snap.pad.connected {
        return false;
    }
    let Some(dev) = device_of(reading) else {
        crate::log::gap("input", "GameInput readings of this version don't say their device through the interface GameInput.h describes: its controllers read at rest in split screen");
        return false;
    };
    if let Some(d) = own_for_seat() {
        return d == dev;
    }
    let want = pad_to_gamepad_state(&own_pad(snap));
    if pad_active(&want) && gamepad_state(reading).is_some_and(|got| gamepad_matches(&want, &got)) {
        let how = if want.buttons != 0 { "its buttons went down with it" } else { "its stick or trigger moved with it" };
        return learn_own(dev, how);
    }
    false
}

/// Whether the copy's own device is known for its seat.
fn own_known() -> bool {
    own_for_seat().is_some()
}

fn has_previous(this: usize) -> bool {
    let vt = unsafe { *(this as *const usize) };
    PREV_VTABLES.lock().unwrap_or_else(|e| e.into_inner()).contains(&vt)
}

/// How far back or on a split-screen copy looks for its own device's reading.
const WALK: usize = 64;

fn current_detours() -> [Current; 8] {
    per_slot!(Current, [0, 1, 2, 3, 4, 5, 6, 7], current, (this: usize, kind: u32, device: usize, out: *mut usize) -> i32)
}

fn current(i: usize, this: usize, kind: u32, device: usize, out: *mut usize) -> i32 {
    let Some(h) = CURRENT.hooks[i].get() else { return NOT_FOUND };
    if out.is_null() {
        return unsafe { h.call(this, kind, device, out) };
    }
    if let Some(snap) = filtered(kind) {
        return own_current(h, this, kind, device, out, &snap);
    }
    if !should_hold(kind) {
        return unsafe { h.call(this, kind, device, out) };
    }
    // The held reading: AddRef'd under the lock (another thread's `hidden` may let it
    // go), the real call made outside it.
    {
        let held = HELD.lock().unwrap();
        if let Some(&(_, _, r)) = held.iter().find(|(k, d, _)| *k == kind && *d == device) {
            unsafe { add_ref(r) };
            unsafe { *out = r };
            return 0;
        }
    }
    let hr = unsafe { h.call(this, kind, device, out) };
    if hr >= 0 && unsafe { *out } != 0 {
        let r = unsafe { *out };
        let mut held = HELD.lock().unwrap();
        if !held.iter().any(|(k, d, _)| *k == kind && *d == device) {
            unsafe { add_ref(r) };
            held.push((kind, device, r));
        }
    }
    hr
}

/// A split-screen copy's current reading of `kind`: the latest one of its own device.
/// Walks `GetPreviousReading` (slot 6 on v0..v3) when another device's reading is newer.
fn own_current(h: &retour::GenericDetour<Current>, this: usize, kind: u32, device: usize, out: *mut usize, snap: &InputSnapshot) -> i32 {
    if device != 0
        && let Some(own_id) = own_for_seat()
        && identity(device) != Some(own_id)
    {
        unsafe { *out = 0 };
        return NOT_FOUND;
    }
    let hr = unsafe { h.call(this, kind, device, out) };
    if hr < 0 || unsafe { *out } == 0 {
        return hr;
    }
    let mut r = unsafe { *out };
    unsafe { *out = 0 };
    if own_reading(r, snap) {
        unsafe { *out = r };
        return hr;
    }
    if !has_previous(this) || !(own_known() || pad_active(&pad_to_gamepad_state(&own_pad(snap)))) {
        unsafe { release(r) };
        return NOT_FOUND;
    }
    // IGameInput::GetPreviousReading (slot 6 on v0..v3): (this, reference, kind, device, out).
    let previous: Next = unsafe { std::mem::transmute(*(*(this as *const *const usize)).add(6)) };
    for _ in 0..WALK {
        let mut p = 0usize;
        let found = unsafe { previous(this, r, kind, device, &mut p) } >= 0 && p != 0;
        unsafe { release(r) };
        if !found {
            return NOT_FOUND;
        }
        if own_reading(p, snap) {
            unsafe { *out = p };
            return 0;
        }
        r = p;
    }
    unsafe { release(r) };
    NOT_FOUND
}

fn next_detours() -> [Next; 8] {
    per_slot!(Next, [0, 1, 2, 3, 4, 5, 6, 7], next, (this: usize, reference: usize, kind: u32, device: usize, out: *mut usize) -> i32)
}

fn next(i: usize, this: usize, reference: usize, kind: u32, device: usize, out: *mut usize) -> i32 {
    let Some(h) = NEXT.hooks[i].get() else { return NOT_FOUND };
    if out.is_null() {
        return unsafe { h.call(this, reference, kind, device, out) };
    }
    if let Some(snap) = filtered(kind) {
        if device != 0
            && let Some(own_id) = own_for_seat()
            && identity(device) != Some(own_id)
        {
            unsafe { *out = 0 };
            return NOT_FOUND;
        }
        // The next reading of the copy's own device: the others' are stepped over.
        let mut at = reference;
        let mut taken = 0usize;
        for _ in 0..WALK {
            let mut r = 0usize;
            let hr = unsafe { h.call(this, at, kind, device, &mut r) };
            if taken != 0 {
                unsafe { release(taken) };
            }
            if hr < 0 || r == 0 {
                unsafe { *out = 0 };
                return if hr < 0 { hr } else { NOT_FOUND };
            }
            if own_reading(r, &snap) {
                unsafe { *out = r };
                return hr;
            }
            at = r;
            taken = r;
        }
        if taken != 0 {
            unsafe { release(taken) };
        }
        unsafe { *out = 0 };
        return NOT_FOUND;
    }
    if should_hold(kind) {
        unsafe { *out = 0 };
        return NOT_FOUND;
    }
    unsafe { h.call(this, reference, kind, device, out) }
}

struct Relay {
    callback: usize,
    context: usize,
    kind: u32,
}

fn relay_for(callback: usize, context: usize, kind: u32) -> usize {
    Box::into_raw(Box::new(Relay { callback, context, kind })) as usize
}

/// Whether a callback's reading of `kind` reaches the game.
fn passes(kind: u32, reading: usize) -> bool {
    match filtered(kind) {
        Some(snap) => reading != 0 && own_reading(reading, &snap),
        None => !should_hold(kind),
    }
}

unsafe extern "system" fn relay_v0(token: u64, relay: usize, reading: usize, overrun: bool) {
    let r = unsafe { &*(relay as *const Relay) };
    if !passes(r.kind, reading) {
        return;
    }
    let f: CallbackV0 = unsafe { std::mem::transmute(r.callback) };
    unsafe { f(token, r.context, reading, overrun) }
}

unsafe extern "system" fn relay(token: u64, relay: usize, reading: usize) {
    let r = unsafe { &*(relay as *const Relay) };
    if !passes(r.kind, reading) {
        return;
    }
    let f: Callback = unsafe { std::mem::transmute(r.callback) };
    unsafe { f(token, r.context, reading) }
}

/// A connected controller device seen by a device callback (held AddRef'd while connected
/// so it can be replayed once this copy's own controller is learnt or swapped).
struct SeenDevice {
    id: usize,
    device: usize,
    timestamp: u64,
    status: u32,
}

/// A device callback the game registered via `RegisterDeviceCallback`.
/// Holds an `AddRef`'d reference to its `IGameInput` (`gi`) while registered so the probe
/// thread can safely inspect held devices; released when unregistered.
struct DeviceRelay {
    gi: usize,
    callback: usize,
    context: usize,
    kind: u32,
    status_filter: u32,
    token: AtomicU64,
    stopped: AtomicBool,
    in_flight: Mutex<(usize, Option<std::thread::ThreadId>)>,
    in_flight_cv: Condvar,
    /// Devices announced as connected to this relay: (dev_id, AddRef'd device, last status).
    announced: Mutex<Vec<(usize, usize, u32)>>,
}

impl Drop for DeviceRelay {
    fn drop(&mut self) {
        let announced: Vec<(usize, usize, u32)> = self.announced.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        for (_, dev, _) in announced {
            if dev != 0 {
                unsafe { release(dev) };
            }
        }
        if self.gi != 0 {
            unsafe { release(self.gi) };
        }
    }
}

struct InFlight<'a>(&'a DeviceRelay);

impl<'a> InFlight<'a> {
    fn enter(r: &'a DeviceRelay) -> Self {
        let mut g = r.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        g.0 += 1;
        g.1 = Some(std::thread::current().id());
        InFlight(r)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut g = self.0.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        g.0 = g.0.saturating_sub(1);
        if g.0 == 0 {
            g.1 = None;
            self.0.in_flight_cv.notify_all();
        }
    }
}

impl DeviceRelay {
    fn wait_idle(&self) {
        let me = std::thread::current().id();
        let mut g = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        while g.0 > 0 && g.1 != Some(me) {
            g = self.in_flight_cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn record_announced(&self, dev_id: usize, device: usize, status: u32) {
        let mut ann = self.announced.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = ann.iter_mut().find(|(id, _, _)| *id == dev_id) {
            entry.2 = status;
        } else {
            unsafe { add_ref(device) };
            ann.push((dev_id, device, status));
        }
    }

    fn take_announced(&self, dev_id: usize) -> Option<(usize, u32)> {
        let mut ann = self.announced.lock().unwrap_or_else(|e| e.into_inner());
        ann.iter().position(|(id, _, _)| *id == dev_id).map(|k| {
            let (_, dev, st) = ann.swap_remove(k);
            (dev, st)
        })
    }
}

fn note_seen_device(dev_id: usize, device: usize, timestamp: u64, status: u32) {
    let mut old_ref = 0usize;
    {
        let mut seen = SEEN_DEVICES.lock().unwrap_or_else(|e| e.into_inner());
        if status & DEVICE_CONNECTED != 0 {
            if let Some(s) = seen.iter_mut().find(|s| s.id == dev_id) {
                s.timestamp = timestamp;
                s.status = status;
            } else if seen.len() < 32 {
                unsafe { add_ref(device) };
                seen.push(SeenDevice { id: dev_id, device, timestamp, status });
            }
        } else if let Some(k) = seen.iter().position(|s| s.id == dev_id) {
            old_ref = seen.swap_remove(k).device;
        }
    }
    if old_ref != 0 {
        unsafe { release(old_ref) };
    }
}

unsafe extern "system" fn device_relay(token: u64, relay_ptr: usize, device: usize, timestamp: u64, current_status: u32, previous_status: u32) {
    let r = unsafe { &*(relay_ptr as *const DeviceRelay) };
    let _ = r.token.compare_exchange(0, token, Ordering::AcqRel, Ordering::Acquire);
    if r.stopped.load(Ordering::Acquire) {
        return;
    }
    let f: DeviceCallback = unsafe { std::mem::transmute(r.callback) };
    if !matches!(super::topology(), Policy::Virtual(_)) {
        unsafe { f(token, r.context, device, timestamp, current_status, previous_status) };
        return;
    }
    // Wildcard callbacks also see keyboards and mice: pass those straight through.
    if (r.kind == 0 || r.kind & (KEYBOARD | MOUSE) != 0) && device != 0 && !device_is_controller(device) {
        unsafe { f(token, r.context, device, timestamp, current_status, previous_status) };
        return;
    }
    if device == 0 {
        return;
    }
    super::first_use(&super::USED_GAMEINPUT, "GameInput controllers (its own device's callbacks and readings only)");
    hook_rumble(device);
    let Some(dev_id) = identity(device) else { return };
    note_seen_device(dev_id, device, timestamp, current_status);

    let connected = current_status & DEVICE_CONNECTED != 0;
    match own_for_seat() {
        Some(own_id) if own_id == dev_id => {
            if connected {
                r.record_announced(dev_id, device, current_status);
                unsafe { f(token, r.context, device, timestamp, current_status, previous_status) };
            } else {
                let removed = r.take_announced(dev_id);
                unsafe { f(token, r.context, device, timestamp, current_status, previous_status) };
                if let Some((dev_ref, _)) = removed {
                    unsafe { release(dev_ref) };
                }
            }
        }
        other => {
            if connected {
                let why = if other.is_some() { "isn't this copy's controller" } else { "waiting to learn this copy's controller" };
                log!("input: GameInput controller {dev_id:#x} held back from device callback ({why})");
                if other.is_none() {
                    wake_probe();
                }
            } else if let Some((dev_ref, _)) = r.take_announced(dev_id) {
                unsafe { f(token, r.context, device, timestamp, current_status, previous_status) };
                unsafe { release(dev_ref) };
            }
        }
    }
}

/// Replays connected/disconnected notifications for all active device callbacks once
/// `own_id` is learnt (or after a seat swap).
fn replay_device_callbacks(own_id: usize) {
    let own_seen = {
        let seen = SEEN_DEVICES.lock().unwrap_or_else(|e| e.into_inner());
        seen.iter().find(|s| s.id == own_id).map(|s| {
            unsafe { add_ref(s.device) };
            (s.device, s.timestamp, s.status)
        })
    };
    let relays: Vec<Arc<DeviceRelay>> = DEVICE_RELAYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(_, a)| a.clone())
        .collect();
    for r in relays {
        if r.stopped.load(Ordering::Acquire) {
            continue;
        }
        let _guard = InFlight::enter(&r);
        if r.stopped.load(Ordering::Acquire) {
            continue;
        }
        let token = r.token.load(Ordering::Acquire);
        let f: DeviceCallback = unsafe { std::mem::transmute(r.callback) };
        // Disconnect any previously announced controller that is no longer this copy's own.
        let stale: Vec<(usize, usize, u32)> = {
            let mut ann = r.announced.lock().unwrap_or_else(|e| e.into_inner());
            let mut out = Vec::new();
            let mut k = 0;
            while k < ann.len() {
                if ann[k].0 != own_id {
                    out.push(ann.swap_remove(k));
                } else {
                    k += 1;
                }
            }
            out
        };
        for (old_id, old_dev, old_status) in stale {
            if r.status_filter & DEVICE_CONNECTED != 0 {
                log!("input: GameInput controller {old_id:#x} disconnected from device callback (seat switched to {own_id:#x})");
                unsafe { f(token, r.context, old_dev, 0, old_status & !DEVICE_CONNECTED, old_status) };
            }
            unsafe { release(old_dev) };
        }
        // Announce this copy's own controller if it is connected and not yet announced.
        if let Some((own_dev, ts, status)) = own_seen {
            let already = r.announced.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(id, _, _)| *id == own_id);
            if !already && !r.stopped.load(Ordering::Acquire) {
                r.record_announced(own_id, own_dev, status);
                log!("input: GameInput controller {own_id:#x} announced to device callback (token {token:#x})");
                unsafe { f(token, r.context, own_dev, ts, status, status & !DEVICE_CONNECTED) };
            }
        }
    }
    if let Some((own_dev, _, _)) = own_seen {
        unsafe { release(own_dev) };
    }
}

fn wake_probe() {
    if !PROBE_STARTED.swap(true, Ordering::AcqRel) {
        let _ = std::thread::Builder::new().name("peerparty-gi-probe".into()).spawn(probe_loop);
    }
    let (lock, cv) = &PROBE_WAKE;
    let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
    *g = g.wrapping_add(1);
    cv.notify_all();
}

/// Probes held controller devices while `OWN` is unknown so games that only poll devices
/// announced via `RegisterDeviceCallback` still learn this copy's controller as soon as
/// its player presses a button or moves a stick/trigger.
fn probe_loop() {
    let (lock, cv) = &PROBE_WAKE;
    loop {
        let should_poll = own_for_seat().is_none()
            && !DEVICE_RELAYS.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
            && !SEEN_DEVICES.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
        if !should_poll {
            let g = lock.lock().unwrap_or_else(|e| e.into_inner());
            drop(cv.wait(g).unwrap_or_else(|e| e.into_inner()));
            continue;
        }
        if let Policy::Virtual(snap) = super::topology()
            && snap.pad.connected
        {
            let want = pad_to_gamepad_state(&own_pad(&snap));
            if pad_active(&want) {
                let relay = DEVICE_RELAYS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|(_, a)| !a.stopped.load(Ordering::Acquire) && a.gi != 0)
                    .map(|(_, a)| a.clone());
                if let Some(r) = relay {
                    let _guard = InFlight::enter(&r);
                    if !r.stopped.load(Ordering::Acquire) {
                        let devs: Vec<(usize, usize)> = {
                            let seen = SEEN_DEVICES.lock().unwrap_or_else(|e| e.into_inner());
                            seen.iter()
                                .map(|s| {
                                    unsafe { add_ref(s.device) };
                                    (s.id, s.device)
                                })
                                .collect()
                        };
                        let mut matched = None;
                        for &(dev_id, dev_ptr) in &devs {
                            let mut reading = 0usize;
                            if call_current(r.gi, GAMEPAD, dev_ptr, &mut reading) >= 0 && reading != 0 {
                                let ok = gamepad_state(reading).is_some_and(|got| gamepad_matches(&want, &got));
                                unsafe { release(reading) };
                                if ok {
                                    matched = Some(dev_id);
                                    break;
                                }
                            }
                        }
                        for (_, dev_ptr) in devs {
                            unsafe { release(dev_ptr) };
                        }
                        drop(_guard);
                        if let Some(dev_id) = matched {
                            let how = if want.buttons != 0 { "its buttons matched a held device" } else { "its stick or trigger matched a held device" };
                            learn_own(dev_id, how);
                            continue;
                        }
                    }
                }
            }
        }
        let g = lock.lock().unwrap_or_else(|e| e.into_inner());
        drop(cv.wait_timeout(g, std::time::Duration::from_millis(25)).unwrap_or_else(|e| e.into_inner()));
    }
}

/// A callback registered through a relay: kept by its token, freed when it's
/// unregistered (`unregister`).
fn registered(token: *mut u64, relay: usize) {
    if token.is_null() {
        return;
    }
    let mut relays = RELAYS.lock().unwrap_or_else(|e| e.into_inner());
    if relays.len() < 256 {
        relays.push((unsafe { *token }, relay));
    }
}

fn register_v0_detours() -> [RegisterV0; 2] {
    per_slot!(RegisterV0, [0, 1], register_v0,
        (this: usize, device: usize, kind: u32, threshold: f32, context: usize, callback: usize, token: *mut u64) -> i32)
}

fn register_v0(i: usize, this: usize, device: usize, kind: u32, threshold: f32, context: usize, callback: usize, token: *mut u64) -> i32 {
    let Some(h) = REGISTER_V0.hooks[i].get() else { return NOT_FOUND };
    if callback == 0 {
        return unsafe { h.call(this, device, kind, threshold, context, callback, token) };
    }
    let r = relay_for(callback, context, kind);
    let hr = unsafe { h.call(this, device, kind, threshold, r, relay_v0 as CallbackV0 as usize, token) };
    if hr < 0 {
        drop(unsafe { Box::from_raw(r as *mut Relay) });
    } else {
        registered(token, r);
        log!("input: GameInput reading callback (v0, kinds {kind:#x}) gated");
    }
    hr
}

fn register_detours() -> [Register; 6] {
    per_slot!(Register, [0, 1, 2, 3, 4, 5], register,
        (this: usize, device: usize, kind: u32, context: usize, callback: usize, token: *mut u64) -> i32)
}

fn register(i: usize, this: usize, device: usize, kind: u32, context: usize, callback: usize, token: *mut u64) -> i32 {
    let Some(h) = REGISTER.hooks[i].get() else { return NOT_FOUND };
    if callback == 0 {
        return unsafe { h.call(this, device, kind, context, callback, token) };
    }
    let r = relay_for(callback, context, kind);
    let hr = unsafe { h.call(this, device, kind, r, relay as Callback as usize, token) };
    if hr < 0 {
        drop(unsafe { Box::from_raw(r as *mut Relay) });
    } else {
        registered(token, r);
        log!("input: GameInput reading callback (kinds {kind:#x}) gated");
    }
    hr
}

fn register_device_detours() -> [RegisterDevice; 8] {
    per_slot!(RegisterDevice, [0, 1, 2, 3, 4, 5, 6, 7], register_device,
        (this: usize, device: usize, kind: u32, status_filter: u32, enum_kind: u32, context: usize, callback: usize, token: *mut u64) -> i32)
}

fn register_device(i: usize, this: usize, device: usize, kind: u32, status_filter: u32, enum_kind: u32, context: usize, callback: usize, token: *mut u64) -> i32 {
    let Some(h) = REGISTER_DEVICE.hooks[i].get() else { return NOT_FOUND };
    if callback == 0 || !held_kind(kind) {
        return unsafe { h.call(this, device, kind, status_filter, enum_kind, context, callback, token) };
    }
    if this != 0 {
        unsafe { add_ref(this) };
    }
    let arc = Arc::new(DeviceRelay {
        gi: this,
        callback,
        context,
        kind,
        status_filter,
        token: AtomicU64::new(0),
        stopped: AtomicBool::new(false),
        in_flight: Mutex::new((0, None)),
        in_flight_cv: Condvar::new(),
        announced: Mutex::new(Vec::new()),
    });
    let raw = Arc::into_raw(arc.clone()) as usize;
    {
        let mut relays = DEVICE_RELAYS.lock().unwrap_or_else(|e| e.into_inner());
        if relays.len() < 256 {
            relays.push((raw, arc.clone()));
        }
    }
    let hr = unsafe { h.call(this, device, kind, status_filter, enum_kind, raw, device_relay as DeviceCallback as usize, token) };
    if hr < 0 {
        let mut relays = DEVICE_RELAYS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = relays.iter().position(|(p, _)| *p == raw) {
            relays.swap_remove(k);
        }
        drop(relays);
        drop(unsafe { Arc::from_raw(raw as *const DeviceRelay) });
    } else {
        if !token.is_null() {
            let t = unsafe { *token };
            if t != 0 {
                arc.token.store(t, Ordering::Release);
            }
        }
        log!("input: GameInput device callback (kinds {kind:#x}, status {status_filter:#x}) gated");
    }
    hr
}

fn stop_callback_detours() -> [StopCallback; 8] {
    per_slot!(StopCallback, [0, 1, 2, 3, 4, 5, 6, 7], stop_callback, (this: usize, token: u64) -> ())
}

fn stop_device_relay(token: u64) {
    if token == 0 {
        return;
    }
    let relays = DEVICE_RELAYS.lock().unwrap_or_else(|e| e.into_inner());
    for (_, arc) in relays.iter() {
        if arc.token.load(Ordering::Acquire) == token {
            arc.stopped.store(true, Ordering::Release);
        }
    }
}

fn stop_callback(i: usize, this: usize, token: u64) {
    stop_device_relay(token);
    if let Some(h) = STOP_CALLBACK.hooks[i].get() {
        unsafe { h.call(this, token) };
    }
}

fn unregister_v0(i: usize, this: usize, token: u64, timeout: u64) -> bool {
    let Some(h) = UNREGISTER_V0.hooks[i].get() else { return false };
    stop_device_relay(token);
    let gone = unsafe { h.call(this, token, timeout) };
    unregistered(token, gone);
    gone
}

fn unregister(i: usize, this: usize, token: u64) -> bool {
    let Some(h) = UNREGISTER.hooks[i].get() else { return false };
    stop_device_relay(token);
    let gone = unsafe { h.call(this, token) };
    unregistered(token, gone);
    gone
}

/// A callback unregistered: once GameInput says it's gone (no call of it is running),
/// its relay is freed. A system-button or other callback's token isn't ours, and isn't found.
fn unregistered(token: u64, gone: bool) {
    if !gone {
        return;
    }
    let relay = {
        let mut relays = RELAYS.lock().unwrap_or_else(|e| e.into_inner());
        relays.iter().position(|(t, _)| *t == token).map(|k| relays.swap_remove(k).1)
    };
    if let Some(r) = relay {
        drop(unsafe { Box::from_raw(r as *mut Relay) });
    }
    let dev_relay = {
        let mut relays = DEVICE_RELAYS.lock().unwrap_or_else(|e| e.into_inner());
        relays.iter().position(|(_, a)| a.token.load(Ordering::Acquire) == token).map(|k| relays.swap_remove(k))
    };
    if let Some((raw, arc)) = dev_relay {
        arc.stopped.store(true, Ordering::Release);
        arc.wait_idle();
        drop(unsafe { Arc::from_raw(raw as *const DeviceRelay) });
        drop(arc);
    }
}

/// Hooks `SetRumbleState` (slot 10 of the v0 device interface) through device `dev`.
fn hook_rumble(dev: usize) {
    static TRIED: AtomicBool = AtomicBool::new(false);
    if TRIED.load(Ordering::Relaxed) {
        return;
    }
    let Some(d) = (unsafe { query(dev, &IID_DEVICE_V0) }) else { return };
    let at = unsafe { *(*(d as *const *const usize)).add(10) };
    unsafe { release(d) };
    if !TRIED.swap(true, Ordering::Relaxed) && arm(&RUMBLE, at, per_slot!(Rumble, [0, 1], rumble, (this: usize, params: *const u8) -> ()), "IGameInputDevice::SetRumbleState") > 0 {
        log!("input: GameInput rumble hooked (in split screen it reaches this copy's own controller alone)");
    }
}

/// Rumble in split screen: another player's device keeps still.
fn rumble(i: usize, this: usize, params: *const u8) {
    let Some(h) = RUMBLE.hooks[i].get() else { return };
    if matches!(super::topology(), Policy::Virtual(_)) {
        let own = own_for_seat().is_some_and(|d| identity(this) == Some(d));
        if !own {
            crate::log::throttled("input: gameinput rumble", 1, |_| {
                log!("input: GameInput rumble on a device that isn't this copy's own controller: held back");
            });
            return;
        }
    }
    unsafe { h.call(this, params) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_and_mouse_readings_go_through() {
        assert!(!held_kind(KEYBOARD) && !held_kind(MOUSE) && !held_kind(KEYBOARD | MOUSE));
        for kind in [0, 0x4, 0x40000, 0x40000 | KEYBOARD] {
            assert!(held_kind(kind), "{kind:#x}");
        }
    }

    #[test]
    fn gamepad_matching_covers_buttons_and_axes() {
        let rest = GamepadState::default();
        assert!(!pad_active(&rest));
        assert!(!gamepad_matches(&rest, &rest), "at rest never matches");

        let a_down = GamepadState { buttons: 0x4, ..rest };
        assert!(pad_active(&a_down));
        assert!(gamepad_matches(&a_down, &a_down));
        assert!(!gamepad_matches(&a_down, &GamepadState { buttons: 0x8, ..rest }));

        let stick_up = GamepadState { ly: 0.9, ..rest };
        assert!(pad_active(&stick_up));
        assert!(gamepad_matches(&stick_up, &GamepadState { ly: 0.85, ..rest }), "within analog tolerance");
        assert!(!gamepad_matches(&stick_up, &GamepadState { ly: -0.9, ..rest }), "opposite stick rejected");
        assert!(!gamepad_matches(&stick_up, &rest), "idle pad rejected");
        assert!(!gamepad_matches(&stick_up, &GamepadState { ly: 0.9, rx: 0.5, ..rest }), "mismatched other axis rejected");

        let trigger = GamepadState { rt: 0.8, ..rest };
        assert!(gamepad_matches(&trigger, &GamepadState { rt: 0.75, ..rest }));
        assert!(!gamepad_matches(&trigger, &GamepadState { lt: 0.8, ..rest }));
    }

    /// Minimal fake COM controller object for testing `device_relay`, `replay_device_callbacks`,
    /// and seat swaps without requiring physical controllers plugged in.
    #[repr(C)]
    struct FakeDevice {
        vt: *const [usize; 11],
        refs: std::sync::atomic::AtomicU32,
        info: [u8; 112],
    }

    fn fake_vt() -> *const [usize; 11] {
        static VT: std::sync::OnceLock<[usize; 11]> = std::sync::OnceLock::new();
        VT.get_or_init(|| [
            fake_qi as *const () as usize,
            fake_addref as *const () as usize,
            fake_release as *const () as usize,
            fake_get_info as *const () as usize,
            0, 0, 0, 0, 0, 0,
            fake_rumble as *const () as usize,
        ])
    }

    unsafe extern "system" fn fake_qi(this: usize, iid: *const [u8; 16], out: *mut usize) -> i32 {
        let id = unsafe { *iid };
        if id == IID_UNKNOWN || id == IID_DEVICE_V0 {
            unsafe { fake_addref(this) };
            unsafe { *out = this };
            0
        } else {
            unsafe { *out = 0 };
            0x8000_4002_u32 as i32
        }
    }

    unsafe extern "system" fn fake_addref(this: usize) -> u32 {
        let d = unsafe { &*(this as *const FakeDevice) };
        d.refs.fetch_add(1, Ordering::SeqCst) + 1
    }

    unsafe extern "system" fn fake_release(this: usize) -> u32 {
        let d = unsafe { &*(this as *const FakeDevice) };
        d.refs.fetch_sub(1, Ordering::SeqCst).saturating_sub(1)
    }

    unsafe extern "system" fn fake_get_info(this: usize) -> *const u8 {
        let d = unsafe { &*(this as *const FakeDevice) };
        d.info.as_ptr()
    }

    unsafe extern "system" fn fake_rumble(_this: usize, _params: *const u8) {}

    fn make_fake_device(supported: u32) -> Box<FakeDevice> {
        let mut info = [0u8; 112];
        info[0..2].copy_from_slice(&112u16.to_le_bytes());
        info[104..108].copy_from_slice(&supported.to_le_bytes());
        Box::new(FakeDevice { vt: fake_vt(), refs: std::sync::atomic::AtomicU32::new(1), info })
    }

    #[test]
    fn device_relay_filters_and_replays_own_controller_only() {
        let _turn = crate::overlay::test_turn();
        let _turn = super::super::tests::GAMEINPUT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let pad_a = make_fake_device(GAMEPAD);
        let pad_b = make_fake_device(GAMEPAD);
        let kb = make_fake_device(KEYBOARD);
        let a_ptr = (&*pad_a as *const FakeDevice) as usize;
        let b_ptr = (&*pad_b as *const FakeDevice) as usize;
        let kb_ptr = (&*kb as *const FakeDevice) as usize;
        assert!(device_is_controller(a_ptr));
        assert!(!device_is_controller(kb_ptr));

        static EVENTS: Mutex<Vec<(usize, u32, u32)>> = Mutex::new(Vec::new());
        EVENTS.lock().unwrap().clear();
        unsafe extern "system" fn on_device(_: u64, _: usize, dev: usize, _: u64, cur: u32, prev: u32) {
            EVENTS.lock().unwrap().push((dev, cur, prev));
        }

        let snap = InputSnapshot {
            mode: peerparty_protocol::input_shm::InputMode::Virtual,
            pad: VirtualPad { connected: true, ..Default::default() },
            ..Default::default()
        };
        *super::super::TEST_POLICY.lock().unwrap() = Some(Policy::Virtual(snap));
        *OWN.lock().unwrap() = None;
        SEEN_DEVICES.lock().unwrap().clear();

        let arc = Arc::new(DeviceRelay {
            gi: 0,
            callback: on_device as DeviceCallback as usize,
            context: 0,
            kind: 0,
            status_filter: DEVICE_CONNECTED,
            token: AtomicU64::new(0x99),
            stopped: AtomicBool::new(false),
            in_flight: Mutex::new((0, None)),
            in_flight_cv: Condvar::new(),
            announced: Mutex::new(Vec::new()),
        });
        let raw = Arc::into_raw(arc.clone()) as usize;
        DEVICE_RELAYS.lock().unwrap().push((raw, arc.clone()));

        // Keyboard passes through immediately; both gamepads are held while OWN is unknown.
        unsafe { device_relay(0x99, raw, kb_ptr, 1, DEVICE_CONNECTED, 0) };
        unsafe { device_relay(0x99, raw, a_ptr, 2, DEVICE_CONNECTED, 0) };
        unsafe { device_relay(0x99, raw, b_ptr, 3, DEVICE_CONNECTED, 0) };
        assert_eq!(*EVENTS.lock().unwrap(), vec![(kb_ptr, DEVICE_CONNECTED, 0)]);

        // Learning pad B as this copy's own announces B alone (never A).
        learn_own(b_ptr, "test");
        assert_eq!(*EVENTS.lock().unwrap(), vec![(kb_ptr, DEVICE_CONNECTED, 0), (b_ptr, DEVICE_CONNECTED, 0)]);

        // Seat swap: clearing OWN and learning pad A disconnects B and connects A.
        devices_changed();
        learn_own(a_ptr, "test swap");
        assert_eq!(
            *EVENTS.lock().unwrap(),
            vec![
                (kb_ptr, DEVICE_CONNECTED, 0),
                (b_ptr, DEVICE_CONNECTED, 0),
                (b_ptr, 0, DEVICE_CONNECTED),
                (a_ptr, DEVICE_CONNECTED, 0),
            ]
        );

        // Unregistering frees the relay cleanly and stops further replays.
        unregistered(0x99, true);
        assert!(DEVICE_RELAYS.lock().unwrap().is_empty());
        SEEN_DEVICES.lock().unwrap().drain(..).for_each(|s| unsafe { release(s.device) });
        *OWN.lock().unwrap() = None;
        *super::super::TEST_POLICY.lock().unwrap() = None;
    }

    #[test]
    fn versions_and_callbacks() {
        let _turn = crate::overlay::test_turn();
        let _turn = super::super::tests::GAMEINPUT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        for dll in ["GameInput.dll", "GameInputRedist.dll"] {
            let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
            unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
        }
        let found = versions();
        eprintln!("GameInput: {} interface versions ({} v0)", found.len(), found.iter().filter(|f| f.api == Api::V0).count());
        if found.is_empty() {
            return;
        }
        hook_found(&found);
        let hooked6 = |a: &Mutex<[usize; 6]>| a.lock().unwrap().iter().filter(|a| **a != 0).count();
        let hooked8 = |a: &Mutex<[usize; 8]>| a.lock().unwrap().iter().filter(|a| **a != 0).count();
        assert!(hooked6(&REGISTER.addrs) > 0, "v1+ RegisterReadingCallback hooked");
        assert!(hooked8(&REGISTER_DEVICE.addrs) > 0, "RegisterDeviceCallback hooked");
        assert!(hooked8(&STOP_CALLBACK.addrs) > 0, "StopCallback hooked");

        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        unsafe extern "system" fn game(_: u64, context: usize, _: usize) {
            assert_eq!(context, 0x1234);
            CALLS.fetch_add(1, Ordering::SeqCst);
        }
        let r = relay_for(game as Callback as usize, 0x1234, 0x40000);
        let keys = relay_for(game as Callback as usize, 0x1234, KEYBOARD | MOUSE);
        crate::overlay::set_visible_for_test(true);
        unsafe { relay(1, r, 0) };
        assert_eq!(CALLS.load(Ordering::SeqCst), 0, "held back while the overlay shows");
        unsafe { relay(1, keys, 0) };
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "keyboard and mouse go through");
        crate::overlay::set_visible_for_test(false);
        unsafe { relay(1, r, 0) };
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);

        if found.iter().any(|f| f.api == Api::V1To3) {
            let m = module("gameinputredist.dll").unwrap();
            let init: Initialize = unsafe { std::mem::transmute(proc_addr(m, c"GameInputInitialize".as_ptr().cast()).unwrap()) };
            let mut gi = 0usize;
            if unsafe { init(&IID_LATER[2], &mut gi) } >= 0 && gi != 0 {
                let vt = unsafe { *(gi as *const *const usize) };
                let reg: Register = unsafe { std::mem::transmute(*vt.add(7)) };
                let reg_dev: RegisterDevice = unsafe { std::mem::transmute(*vt.add(8)) };
                let unreg: Unregister = unsafe { std::mem::transmute(*vt.add(12)) };
                let mut token = 0u64;
                let hr = unsafe { reg(gi, 0, 0x10, 0x1234, game as Callback as usize, &mut token) };
                eprintln!("RegisterReadingCallback: {hr:#x}, token {token:#x}");
                assert!(hr >= 0 && token != 0);
                assert!(RELAYS.lock().unwrap().iter().any(|(t, _)| *t == token), "its relay kept by its token");
                assert!(unsafe { unreg(gi, token) });
                assert!(!RELAYS.lock().unwrap().iter().any(|(t, _)| *t == token), "freed once unregistered");

                unsafe extern "system" fn dev_cb(_: u64, _: usize, _: usize, _: u64, _: u32, _: u32) {}
                let mut dtoken = 0u64;
                let hr = unsafe { reg_dev(gi, 0, GAMEPAD, DEVICE_CONNECTED, 0, 0, dev_cb as DeviceCallback as usize, &mut dtoken) };
                assert!(hr >= 0 && dtoken != 0, "RegisterDeviceCallback works through hook: {hr:#x}");
                assert!(DEVICE_RELAYS.lock().unwrap().iter().any(|(_, a)| a.token.load(Ordering::Acquire) == dtoken));
                assert!(unsafe { unreg(gi, dtoken) });
                assert!(!DEVICE_RELAYS.lock().unwrap().iter().any(|(_, a)| a.token.load(Ordering::Acquire) == dtoken));
                assert!(has_previous(gi), "v1..v3 recorded in PREV_VTABLES for GetPreviousReading");
                unsafe { release(gi) };
            }
        }
    }

    /// A reading of the keyboard (always there) says its device through the v0 reading
    /// interface, whichever version made it: what a split-screen copy tells devices by.
    #[test]
    fn readings_say_their_device() {
        let _turn = crate::overlay::test_turn();
        let _turn = super::super::tests::GAMEINPUT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        for dll in ["GameInput.dll", "GameInputRedist.dll"] {
            let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
            unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
        }
        let Some(m) = module("gameinput.dll") else { return };
        let mut gi = 0usize;
        if call_create(proc_addr(m, c"GameInputCreate".as_ptr().cast()).unwrap(), &mut gi) < 0 || gi == 0 {
            return;
        }
        let vt = unsafe { *(gi as *const *const usize) };
        let current: Current = unsafe { std::mem::transmute(*vt.add(4)) };
        let mut r = 0usize;
        // The hook's own detour isn't in the way: no policy but PassThrough here.
        let hr = unsafe { current(gi, KEYBOARD, 0, &mut r) };
        eprintln!("GameInput keyboard reading: {hr:#x}");
        if hr >= 0 && r != 0 {
            let dev = device_of(r);
            eprintln!("its device: {dev:?}, gamepad state: {:?}", gamepad_state(r));
            assert!(dev.is_some(), "a reading answers the v0 reading interface");
            assert_eq!(gamepad_state(r), None, "a keyboard's isn't a gamepad's");
            // Also verify `device_is_controller` identifies the keyboard as non-controller.
            if let Some(rv0) = unsafe { query(r, &IID_READING_V0) } {
                let get: unsafe extern "system" fn(usize, *mut usize) = unsafe { std::mem::transmute(*(*(rv0 as *const *const usize)).add(6)) };
                let mut raw_dev = 0usize;
                unsafe { get(rv0, &mut raw_dev) };
                unsafe { release(rv0) };
                if raw_dev != 0 {
                    assert!(!device_is_controller(raw_dev), "keyboard device isn't classified as a controller");
                    unsafe { release(raw_dev) };
                }
            }
            unsafe { release(r) };
        }
        unsafe { release(gi) };
    }
}

/// How many `GetCurrentReading` and `GetNextReading` methods are hooked.
#[cfg(test)]
pub(super) fn reads_hooked() -> usize {
    let n = |a: &Mutex<[usize; 8]>| a.lock().unwrap().iter().filter(|a| **a != 0 && **a != usize::MAX).count();
    n(&CURRENT.addrs) + n(&NEXT.addrs)
}
