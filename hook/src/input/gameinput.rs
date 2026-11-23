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

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use peerparty_protocol::input_shm::InputSnapshot;

use super::{Policy, Slots, arm, module, policy, proc_addr};
use crate::log;

type Current = unsafe extern "system" fn(usize, u32, usize, *mut usize) -> i32;
type Next = unsafe extern "system" fn(usize, usize, u32, usize, *mut usize) -> i32;
/// v0: (this, device, kind, analogThreshold, context, callback, token).
type RegisterV0 = unsafe extern "system" fn(usize, usize, u32, f32, usize, usize, *mut u64) -> i32;
/// v1 and later: (this, device, kind, context, callback, token).
type Register = unsafe extern "system" fn(usize, usize, u32, usize, usize, *mut u64) -> i32;
/// v0: (this, token, timeout in microseconds): whether it's gone.
type UnregisterV0 = unsafe extern "system" fn(usize, u64, u64) -> bool;
/// v1 and later: (this, token).
type Unregister = unsafe extern "system" fn(usize, u64) -> bool;
/// v0's reading callback: (token, context, reading, hasOverrunOccurred).
type CallbackV0 = unsafe extern "system" fn(u64, usize, usize, bool);
/// v1 and later: (token, context, reading).
type Callback = unsafe extern "system" fn(u64, usize, usize);
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
static UNREGISTER_V0: Slots<UnregisterV0, 2> = Slots::new();
static UNREGISTER: Slots<Unregister, 6> = Slots::new();
static RUMBLE: Slots<Rumble, 2> = Slots::new();
/// Readings held while blocked: (kind, device, reading, AddRef'd).
static HELD: Mutex<Vec<(u32, usize, usize)>> = Mutex::new(Vec::new());
/// The vtables of the v0 interfaces found (`GetPreviousReading` is known there).
static V0_VTABLES: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Reading callbacks the game registered: (token, its relay, which `Relay::free` frees).
static RELAYS: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());

/// GameInputKindKeyboard and GameInputKindMouse.
const KEYBOARD: u32 = 0x10;
const MOUSE: u32 = 0x20;

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

struct Found {
    v0: bool,
    vt: *const usize,
}

/// The interface versions object `obj` answers (each QueryInterface'd and let go).
fn collect(obj: usize, legacy: bool, found: &mut Vec<Found>) {
    let mut objects = Vec::new();
    if let Some(o) = unsafe { query(obj, &IID_V0) } {
        objects.push((true, o));
    }
    for iid in &IID_LATER {
        if let Some(o) = unsafe { query(obj, iid) } {
            objects.push((false, o));
        }
    }
    if objects.is_empty() && !legacy {
        // An object of a version not known here: hooked as the later ones are.
        objects.push((false, obj));
        unsafe { add_ref(obj) };
    }
    for (v0, o) in objects {
        let vt = unsafe { *(o as *const *const usize) };
        if !found.iter().any(|f: &Found| f.vt == vt) {
            found.push(Found { v0, vt });
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

/// v0: GetCurrentReading 4, GetNextReading 5, RegisterReadingCallback 8,
/// UnregisterCallback 13. The later versions have no GetTemporalReading (7), so their
/// RegisterReadingCallback is 7 and UnregisterCallback 12 (which takes no timeout).
fn hook_found(found: &[Found]) -> usize {
    let mut n = 0;
    for f in found {
        if f.v0 {
            let mut v0 = V0_VTABLES.lock().unwrap_or_else(|e| e.into_inner());
            if !v0.contains(&(f.vt as usize)) {
                v0.push(f.vt as usize);
            }
        }
        n += arm(&CURRENT, unsafe { *f.vt.add(4) }, current_detours(), "IGameInput::GetCurrentReading");
        n += arm(&NEXT, unsafe { *f.vt.add(5) }, next_detours(), "IGameInput::GetNextReading");
        n += if f.v0 {
            arm(&REGISTER_V0, unsafe { *f.vt.add(8) }, register_v0_detours(), "IGameInput::RegisterReadingCallback (v0)")
        } else {
            arm(&REGISTER, unsafe { *f.vt.add(7) }, register_detours(), "IGameInput::RegisterReadingCallback")
        };
        n += if f.v0 {
            arm(&UNREGISTER_V0, unsafe { *f.vt.add(13) }, per_slot!(UnregisterV0, [0, 1], unregister_v0, (this: usize, token: u64, timeout: u64) -> bool), "IGameInput::UnregisterCallback (v0)")
        } else {
            arm(&UNREGISTER, unsafe { *f.vt.add(12) }, per_slot!(Unregister, [0, 1, 2, 3, 4, 5], unregister, (this: usize, token: u64) -> bool), "IGameInput::UnregisterCallback")
        };
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
}

/// The copy's own device (its identity: its IUnknown), learnt for one seat
/// (`super::seat_stamp`).
static OWN: Mutex<Option<(u64, usize)>> = Mutex::new(None);

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

/// A reading's gamepad buttons (`GetGamepadState`, slot 22 of the v0 reading), as
/// `GameInputGamepadButtons`; None for a reading that isn't a gamepad's.
fn gamepad_buttons(reading: usize) -> Option<u32> {
    let r = unsafe { query(reading, &IID_READING_V0) }?;
    let get: unsafe extern "system" fn(usize, *mut [u32; 7]) -> bool = unsafe { std::mem::transmute(*(*(r as *const *const usize)).add(22)) };
    let mut s = [0u32; 7];
    let ok = unsafe { get(r, &mut s) };
    unsafe { release(r) };
    ok.then_some(s[0])
}

/// The copy's controller's buttons now, as GameInput numbers them (the same as
/// Windows.Gaming.Input's).
fn own_buttons(snap: &InputSnapshot) -> u32 {
    let pad = snap.pad.xinput_slot.and_then(|k| super::real_xinput(k.into()).map(|s| super::pad_of_xinput(k, &s))).unwrap_or(snap.pad);
    super::wgi::wgi_buttons(pad.xinput_buttons())
}

/// Whether `reading` is the copy's own device's; learns the device when its buttons go
/// down with the copy's controller's.
fn own_reading(reading: usize, snap: &InputSnapshot) -> bool {
    if !snap.pad.connected {
        return false;
    }
    let Some(dev) = device_of(reading) else {
        crate::log::gap("input", "GameInput readings of this version don't say their device through the interface GameInput.h describes: its controllers read at rest in split screen");
        return false;
    };
    let stamp = super::seat_stamp();
    let mut own = OWN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((s, d)) = *own
        && s == stamp
        && d == dev
    {
        return true;
    }
    let buttons = own_buttons(snap);
    if buttons != 0 && gamepad_buttons(reading) == Some(buttons) {
        log!("input: GameInput device {dev:#x} is this copy's controller (its buttons went down with it): its readings reach the game, the others' don't");
        *own = Some((stamp, dev));
        return true;
    }
    false
}

/// Whether the copy's own device is known for its seat.
fn own_known() -> bool {
    OWN.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|(s, _)| s == super::seat_stamp())
}

fn is_v0(this: usize) -> bool {
    let vt = unsafe { *(this as *const usize) };
    V0_VTABLES.lock().unwrap_or_else(|e| e.into_inner()).contains(&vt)
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

/// A split-screen copy's current reading of `kind`: the latest one of its own device. A
/// v0 interface finds it further back when another device's is newer; a later version's
/// gives it only when it's the newest.
fn own_current(h: &retour::GenericDetour<Current>, this: usize, kind: u32, device: usize, out: *mut usize, snap: &InputSnapshot) -> i32 {
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
    if !is_v0(this) || !(own_known() || own_buttons(snap) != 0) {
        unsafe { release(r) };
        return NOT_FOUND;
    }
    // IGameInput::GetPreviousReading (v0 slot 6): (this, reference, kind, device, out).
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

fn unregister_v0(i: usize, this: usize, token: u64, timeout: u64) -> bool {
    let Some(h) = UNREGISTER_V0.hooks[i].get() else { return false };
    let gone = unsafe { h.call(this, token, timeout) };
    unregistered(token, gone);
    gone
}

fn unregister(i: usize, this: usize, token: u64) -> bool {
    let Some(h) = UNREGISTER.hooks[i].get() else { return false };
    let gone = unsafe { h.call(this, token) };
    unregistered(token, gone);
    gone
}

/// A callback unregistered: once GameInput says it's gone (no call of it is running),
/// its relay is freed. A device or other callback's token isn't ours, and isn't found.
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
        let own = OWN.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|(s, d)| s == super::seat_stamp() && identity(this) == Some(d));
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
    fn versions_and_callbacks() {
        let _turn = crate::overlay::test_turn();
        let _turn = super::super::tests::GAMEINPUT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        for dll in ["GameInput.dll", "GameInputRedist.dll"] {
            let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
            unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
        }
        let found = versions();
        eprintln!("GameInput: {} interface versions ({} v0)", found.len(), found.iter().filter(|f| f.v0).count());
        if found.is_empty() {
            return;
        }
        hook_found(&found);
        let hooked = |a: &Mutex<[usize; 6]>| a.lock().unwrap().iter().filter(|a| **a != 0).count();
        assert!(hooked(&REGISTER.addrs) > 0, "v1+ RegisterReadingCallback hooked");

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

        if found.iter().any(|f| !f.v0) {
            let m = module("gameinputredist.dll").unwrap();
            let init: Initialize = unsafe { std::mem::transmute(proc_addr(m, c"GameInputInitialize".as_ptr().cast()).unwrap()) };
            let mut gi = 0usize;
            if unsafe { init(&IID_LATER[2], &mut gi) } >= 0 && gi != 0 {
                let vt = unsafe { *(gi as *const *const usize) };
                let reg: Register = unsafe { std::mem::transmute(*vt.add(7)) };
                let unreg: Unregister = unsafe { std::mem::transmute(*vt.add(12)) };
                let mut token = 0u64;
                let hr = unsafe { reg(gi, 0, 0x10, 0x1234, game as Callback as usize, &mut token) };
                eprintln!("RegisterReadingCallback: {hr:#x}, token {token:#x}");
                assert!(hr >= 0 && token != 0);
                assert!(RELAYS.lock().unwrap().iter().any(|(t, _)| *t == token), "its relay kept by its token");
                assert!(unsafe { unreg(gi, token) });
                assert!(!RELAYS.lock().unwrap().iter().any(|(t, _)| *t == token), "freed once unregistered");
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
            eprintln!("its device: {dev:?}, gamepad buttons: {:?}", gamepad_buttons(r));
            assert!(dev.is_some(), "a reading answers the v0 reading interface");
            assert_eq!(gamepad_buttons(r), None, "a keyboard's isn't a gamepad's");
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
