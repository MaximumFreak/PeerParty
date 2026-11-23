//! GameInput (the GDK's input API, so most Microsoft Store games): driven by `input::policy()`.
//!
//! While blocked (or for physical devices excluded in `Virtual` mode),
//! `IGameInput::GetCurrentReading` keeps returning the reading it gave when held,
//! `GetNextReading` finds nothing newer, and reading callbacks aren't called.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Policy, Slots, arm, module, policy, proc_addr};
use crate::log;

type Current = unsafe extern "system" fn(usize, u32, usize, *mut usize) -> i32;
type Next = unsafe extern "system" fn(usize, usize, u32, usize, *mut usize) -> i32;
/// v0: (this, device, kind, analogThreshold, context, callback, token).
type RegisterV0 = unsafe extern "system" fn(usize, usize, u32, f32, usize, usize, *mut u64) -> i32;
/// v1 and later: (this, device, kind, context, callback, token).
type Register = unsafe extern "system" fn(usize, usize, u32, usize, usize, *mut u64) -> i32;
/// v0's reading callback: (token, context, reading, hasOverrunOccurred).
type CallbackV0 = unsafe extern "system" fn(u64, usize, usize, bool);
/// v1 and later: (token, context, reading).
type Callback = unsafe extern "system" fn(u64, usize, usize);

type Create = unsafe extern "system" fn(*mut usize) -> i32;
type Initialize = unsafe extern "system" fn(*const [u8; 16], *mut usize) -> i32;

static CREATE: Slots<Create, 2> = Slots::new();
static INITIALIZE: Slots<Initialize, 2> = Slots::new();
static BUSY: AtomicBool = AtomicBool::new(false);
static CALLBACKS: AtomicBool = AtomicBool::new(false);
static CURRENT: Slots<Current, 8> = Slots::new();
static NEXT: Slots<Next, 8> = Slots::new();
static REGISTER_V0: Slots<RegisterV0, 2> = Slots::new();
static REGISTER: Slots<Register, 6> = Slots::new();
/// Readings held while blocked: (kind, device, reading, AddRef'd).
static HELD: Mutex<Vec<(u32, usize, usize)>> = Mutex::new(Vec::new());

/// GameInputKindKeyboard and GameInputKindMouse.
const KEYBOARD: u32 = 0x10;
const MOUSE: u32 = 0x20;

/// Whether reads of `kind` are held back under `BlockAll` (anything but keyboard and
/// mouse alone; 0 is "any kind", so held).
fn held_kind(kind: u32) -> bool {
    kind == 0 || kind & !(KEYBOARD | MOUSE) != 0
}

/// Whether `kind` should be held under the current `Policy`.
fn should_hold(kind: u32) -> bool {
    match policy() {
        Policy::PassThrough => false,
        Policy::BlockAll => held_kind(kind),
        Policy::Virtual(snap) => {
            if held_kind(kind) {
                super::first_use(&super::USED_GAMEINPUT, "GameInput controllers (held at rest in split screen)");
                return true;
            }
            let wants_kb = kind & KEYBOARD != 0;
            let wants_mouse = kind & MOUSE != 0;
            (wants_kb && !snap.keyboard.active) || (wants_mouse && !snap.mouse.active)
        }
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

pub fn module_loaded(name: &str) {
    let Some(m) = module(name) else { return };
    let mut n = 0;
    if let Some(a) = proc_addr(m, c"GameInputCreate".as_ptr().cast()) {
        n += arm(&CREATE, a, create_detours(), "GameInputCreate");
    }
    if let Some(a) = proc_addr(m, c"GameInputInitialize".as_ptr().cast()) {
        n += arm(&INITIALIZE, a, initialize_detours(), "GameInputInitialize");
    }
    log!("input: {name} loaded, {n} GameInput entry points watched for reading callbacks");
}

fn create_detours() -> [Create; 2] {
    per_slot!(Create, [0, 1], create, (out: *mut usize) -> i32)
}

fn create(i: usize, out: *mut usize) -> i32 {
    let Some(h) = CREATE.hooks[i].get() else { return NOT_FOUND };
    let hr = unsafe { h.call(out) };
    if hr >= 0 {
        created();
    }
    hr
}

fn initialize_detours() -> [Initialize; 2] {
    per_slot!(Initialize, [0, 1], initialize, (iid: *const [u8; 16], out: *mut usize) -> i32)
}

fn initialize(i: usize, iid: *const [u8; 16], out: *mut usize) -> i32 {
    let Some(h) = INITIALIZE.hooks[i].get() else { return NOT_FOUND };
    let hr = unsafe { h.call(iid, out) };
    if hr >= 0 {
        created();
    }
    hr
}

fn created() {
    if CALLBACKS.load(Ordering::Acquire) || BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let n = install_callbacks_inner();
    CALLBACKS.store(true, Ordering::Release);
    BUSY.store(false, Ordering::Release);
    log!("input: GameInput created, {n} RegisterReadingCallback hooked");
}

struct Found {
    v0: bool,
    vt: *const usize,
}

fn versions() -> Vec<Found> {
    let mut found = Vec::new();
    for dll in DLLS {
        let Some(m) = module(dll) else { continue };
        let mut objects = Vec::new();
        if let Some(create) = proc_addr(m, c"GameInputCreate".as_ptr().cast()) {
            let create: unsafe extern "system" fn(*mut usize) -> i32 = unsafe { std::mem::transmute(create) };
            let mut legacy = 0usize;
            if unsafe { create(&mut legacy) } >= 0 && legacy != 0 {
                if let Some(o) = unsafe { query(legacy, &IID_V0) } {
                    objects.push((true, o));
                }
                for iid in &IID_LATER {
                    if let Some(o) = unsafe { query(legacy, iid) } {
                        objects.push((false, o));
                    }
                }
                unsafe { release(legacy) };
            }
        }
        if let Some(init) = proc_addr(m, c"GameInputInitialize".as_ptr().cast()) {
            let init: unsafe extern "system" fn(*const [u8; 16], *mut usize) -> i32 = unsafe { std::mem::transmute(init) };
            for iid in &IID_LATER {
                let mut o = 0usize;
                if unsafe { init(iid, &mut o) } >= 0 && o != 0 {
                    objects.push((false, o));
                }
            }
        }
        for (v0, o) in objects {
            let vt = unsafe { *(o as *const *const usize) };
            if !found.iter().any(|f: &Found| f.vt == vt) {
                found.push(Found { v0, vt });
            }
            unsafe { release(o) };
        }
    }
    found
}

unsafe fn query(obj: usize, iid: &[u8; 16]) -> Option<usize> {
    let vt = unsafe { *(obj as *const *const usize) };
    let qi: unsafe extern "system" fn(usize, *const [u8; 16], *mut usize) -> i32 = unsafe { std::mem::transmute(*vt) };
    let mut out = 0usize;
    (unsafe { qi(obj, iid, &mut out) } >= 0 && out != 0).then_some(out)
}

pub fn install() -> usize {
    if BUSY.swap(true, Ordering::AcqRel) {
        return 0;
    }
    let found = versions();
    BUSY.store(false, Ordering::Release);
    let mut n = 0;
    for f in found {
        n += arm(&CURRENT, unsafe { *f.vt.add(4) }, current_detours(), "IGameInput::GetCurrentReading");
        n += arm(&NEXT, unsafe { *f.vt.add(5) }, next_detours(), "IGameInput::GetNextReading");
    }
    n + install_callbacks()
}

fn install_callbacks() -> usize {
    if BUSY.swap(true, Ordering::AcqRel) {
        return 0;
    }
    let n = install_callbacks_inner();
    BUSY.store(false, Ordering::Release);
    n
}

fn install_callbacks_inner() -> usize {
    let mut n = 0;
    for f in versions() {
        n += if f.v0 {
            arm(&REGISTER_V0, unsafe { *f.vt.add(8) }, register_v0_detours(), "IGameInput::RegisterReadingCallback (v0)")
        } else {
            arm(&REGISTER, unsafe { *f.vt.add(7) }, register_detours(), "IGameInput::RegisterReadingCallback")
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
    for (_, _, r) in HELD.lock().unwrap().drain(..) {
        unsafe { release(r) };
    }
}

fn current_detours() -> [Current; 8] {
    per_slot!(Current, [0, 1, 2, 3, 4, 5, 6, 7], current, (this: usize, kind: u32, device: usize, out: *mut usize) -> i32)
}

fn current(i: usize, this: usize, kind: u32, device: usize, out: *mut usize) -> i32 {
    let Some(h) = CURRENT.hooks[i].get() else { return NOT_FOUND };
    if out.is_null() || !should_hold(kind) {
        return unsafe { h.call(this, kind, device, out) };
    }
    let mut held = HELD.lock().unwrap();
    if let Some(&(_, _, r)) = held.iter().find(|(k, d, _)| *k == kind && *d == device) {
        unsafe { add_ref(r) };
        unsafe { *out = r };
        return 0;
    }
    let hr = unsafe { h.call(this, kind, device, out) };
    if hr >= 0 && unsafe { *out } != 0 {
        let r = unsafe { *out };
        unsafe { add_ref(r) };
        held.push((kind, device, r));
    }
    hr
}

fn next_detours() -> [Next; 8] {
    per_slot!(Next, [0, 1, 2, 3, 4, 5, 6, 7], next, (this: usize, reference: usize, kind: u32, device: usize, out: *mut usize) -> i32)
}

fn next(i: usize, this: usize, reference: usize, kind: u32, device: usize, out: *mut usize) -> i32 {
    let Some(h) = NEXT.hooks[i].get() else { return NOT_FOUND };
    if !out.is_null() && should_hold(kind) {
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

unsafe extern "system" fn relay_v0(token: u64, relay: usize, reading: usize, overrun: bool) {
    let r = unsafe { &*(relay as *const Relay) };
    if should_hold(r.kind) {
        return;
    }
    let f: CallbackV0 = unsafe { std::mem::transmute(r.callback) };
    unsafe { f(token, r.context, reading, overrun) }
}

unsafe extern "system" fn relay(token: u64, relay: usize, reading: usize) {
    let r = unsafe { &*(relay as *const Relay) };
    if should_hold(r.kind) {
        return;
    }
    let f: Callback = unsafe { std::mem::transmute(r.callback) };
    unsafe { f(token, r.context, reading) }
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
        log!("input: GameInput reading callback (kinds {kind:#x}) gated");
    }
    hr
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
        install_callbacks();
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

        if let Some(f) = found.iter().find(|f| !f.v0) {
            let _ = f;
            let m = module("gameinputredist.dll").unwrap();
            let init: Initialize = unsafe { std::mem::transmute(proc_addr(m, c"GameInputInitialize".as_ptr().cast()).unwrap()) };
            let mut gi = 0usize;
            if unsafe { init(&IID_LATER[2], &mut gi) } >= 0 && gi != 0 {
                let vt = unsafe { *(gi as *const *const usize) };
                let reg: Register = unsafe { std::mem::transmute(*vt.add(7)) };
                let unreg: unsafe extern "system" fn(usize, u64) -> bool = unsafe { std::mem::transmute(*vt.add(12)) };
                let mut token = 0u64;
                let hr = unsafe { reg(gi, 0, 0x10, 0x1234, game as Callback as usize, &mut token) };
                eprintln!("RegisterReadingCallback: {hr:#x}, token {token:#x}");
                assert!(hr >= 0 && token != 0);
                assert!(unsafe { unreg(gi, token) });
                unsafe { release(gi) };
            }
        }
    }
}
