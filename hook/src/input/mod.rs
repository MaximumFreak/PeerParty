//! Virtual and blocked input layer for the game process.
//!
//! Replaces the older `padblock` module with a unified `Policy` queried by every input
//! API hook:
//! - `Policy::PassThrough`: zero overhead; calls go straight to Windows/driver.
//! - `Policy::BlockAll`: overlay is open (or copy has `InputAssignment::None`); physical
//!   controllers read at rest so the game doesn't react behind the overlay.
//! - `Policy::Virtual(InputSnapshot)`: multi-instance split-screen; each copy sees only
//!   its assigned virtual controller / keyboard / mouse from the session host's
//!   `InputHeader` shared memory (`shm.rs`). Where the copy's controller can be reached
//!   as it is, it is: XInput's calls for the copy's first controller go to the real slot
//!   it is (state, vibration, capabilities, battery, keystrokes), and reads of its own
//!   HID device (direct, Raw Input, DirectInput) go through untouched (`device.rs`). A
//!   pane sees one controller: the other players' aren't there for it (a HID one can't
//!   be opened and reads as unplugged, Raw Input doesn't list it and holds its reports
//!   back, one Windows.Gaming.Input gamepad carries the copy's controller and the rest
//!   read at rest). The virtual pad answers the rest (another API, or a controller the
//!   hook can't reach as it is). A key or mouse button let go still reaches a copy that
//!   lost the keyboard and mouse while it was down (`releases_only`).
//!
//! APIs covered:
//! - XInput (`xinput1_4`, `xinput1_3`, `xinput9_1_0`, `xinputuap`): `XInputGetState`,
//!   `XInputGetStateEx`, `XInputGetKeystroke`.
//! - DirectInput 8: `IDirectInputDevice8::GetDeviceState`, `GetDeviceData`.
//! - Raw Input + Win32 KBM: `GetRawInputData`, `GetAsyncKeyState`, `GetCursorPos`.
//! - Windows.Gaming.Input (`wgi.rs`), GameInput (`gameinput.rs`), direct HID (`hid.rs`).
//! - Steam Input (`steam/lan.rs` queries `input::policy()`).
//!
//! Compatibility: `PEERPARTY_SKIP=padblock` continues to disable this layer alongside
//! `PEERPARTY_SKIP=input`.

/// One `extern "system"` function per slot, each calling `$inner` with its slot index.
macro_rules! slot_fn {
    ($ty:ty, $i:literal, $inner:ident, ($($a:ident: $t:ty),*) -> $r:ty) => {{
        unsafe extern "system" fn f($($a: $t),*) -> $r {
            $inner($i, $($a),*)
        }
        f as $ty
    }};
}

macro_rules! per_slot {
    ($ty:ty, [$($i:literal),*], $inner:ident, $sig:tt -> $r:ty) => {
        [$(slot_fn!($ty, $i, $inner, $sig -> $r)),*]
    };
}

mod audio;
mod device;
mod gameinput;

pub(crate) use device::{opened_a, opened_w, refuse_open_a, refuse_open_w};
pub(crate) mod hid;
mod shm;
mod wgi;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, Once, OnceLock};

use peerparty_protocol::input_shm::{
    InputMode, InputSnapshot, MOUSE_LEFT, MOUSE_MIDDLE, MOUSE_RIGHT, MOUSE_X1, MOUSE_X2, VirtualPad,
};
use retour::GenericDetour;
use windows_sys::Win32::Foundation::POINT;

use crate::log;
use crate::overlay::pads_held_back;
use crate::sys::hook::with_lock;

/// What the input hooks do on the current call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    PassThrough,
    BlockAll,
    Virtual(InputSnapshot),
}

/// Each way of reading controllers a split-screen copy's game used, told once in its log
/// (`first_use`): which one a pane's stray input came through.
pub(super) static USED_XINPUT: AtomicBool = AtomicBool::new(false);
pub(super) static USED_RAW_HID: AtomicBool = AtomicBool::new(false);
pub(super) static USED_RAW_BUFFER: AtomicBool = AtomicBool::new(false);
pub(super) static USED_DINPUT: AtomicBool = AtomicBool::new(false);
pub(super) static USED_WGI: AtomicBool = AtomicBool::new(false);
pub(super) static USED_WGI_RAW: AtomicBool = AtomicBool::new(false);
pub(super) static USED_GAMEINPUT: AtomicBool = AtomicBool::new(false);

/// Into the log the first time a split-screen copy's game reads controllers `what` way.
pub(super) fn first_use(flag: &AtomicBool, what: &str) {
    if !flag.load(Ordering::Relaxed) && !flag.swap(true, Ordering::Relaxed) {
        log!("input: the game reads {what}");
    }
}

/// Optional test override so unit tests can exercise `BlockAll` / `Virtual` without
/// mutating global overlay or SHM state.
#[cfg(test)]
static TEST_POLICY: Mutex<Option<Policy>> = Mutex::new(None);

/// Current input policy. Fast path when the overlay is closed and no input SHM is
/// attached: two relaxed/acquire atomic loads, zero locks.
pub fn policy() -> Policy {
    #[cfg(test)]
    if let Some(p) = *TEST_POLICY.lock().unwrap_or_else(|e| e.into_inner()) {
        return p;
    }
    if pads_held_back() {
        return Policy::BlockAll;
    }
    if shm::attached()
        && let Some(snap) = shm::read()
    {
        return match snap.mode {
            InputMode::PassThrough => {
                SEAT_SEEN.store(0, Ordering::Relaxed);
                Policy::PassThrough
            }
            InputMode::BlockAll => Policy::BlockAll,
            InputMode::Virtual => {
                note_seat(&snap.pad);
                Policy::Virtual(snap)
            }
        };
    }
    Policy::PassThrough
}

/// The split-screen copy's seat as `Virtual` last said it (`seat`): 0 not known, 1 no
/// controller, 2 a controller (`SEAT_SLOT`, `SEAT_KEY`).
static SEAT_SEEN: AtomicU32 = AtomicU32::new(0);
/// Its controller's `xinput_slot + 1` (0 for none) and `hid_key`.
static SEAT_SLOT: AtomicU32 = AtomicU32::new(0);
static SEAT_KEY: AtomicU64 = AtomicU64::new(0);
/// When the seat got the controller it has now (`since_start_ms`; 0 never).
static SEAT_AT_MS: AtomicU64 = AtomicU64::new(0);

fn note_seat(pad: &VirtualPad) {
    let slot = pad.xinput_slot.map_or(0, |k| u32::from(k) + 1);
    if SEAT_SLOT.load(Ordering::Relaxed) != slot {
        SEAT_SLOT.store(slot, Ordering::Relaxed);
    }
    if SEAT_KEY.load(Ordering::Relaxed) != pad.hid_key || (pad.connected && SEAT_SEEN.load(Ordering::Relaxed) != 2) {
        SEAT_KEY.store(pad.hid_key, Ordering::Relaxed);
        SEAT_AT_MS.store(since_start_ms(), Ordering::Relaxed);
    }
    let seen = if pad.connected { 2 } else { 1 };
    if SEAT_SEEN.load(Ordering::Relaxed) != seen {
        SEAT_SEEN.store(seen, Ordering::Relaxed);
    }
}

/// A split-screen copy's seat while the overlay has the controllers (`BlockAll`): its
/// last `Virtual` snapshot's controller, at rest. The overlay changes what the game
/// reads, never which controllers it sees: shown all of them meanwhile, a game opened
/// another player's DualSense, took it as its own and stopped reading its player's Xbox
/// pad, then lost it when the overlay closed and was left with no controller.
pub(super) fn seat() -> Option<InputSnapshot> {
    #[cfg(test)]
    if TEST_POLICY.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return None;
    }
    let seen = SEAT_SEEN.load(Ordering::Relaxed);
    if seen == 0 || !shm::attached() {
        return None;
    }
    let slot = SEAT_SLOT.load(Ordering::Relaxed);
    let pad = VirtualPad {
        connected: seen == 2,
        xinput_slot: slot.checked_sub(1).and_then(|k| u8::try_from(k).ok()),
        hid_key: SEAT_KEY.load(Ordering::Relaxed),
        ..Default::default()
    };
    Some(InputSnapshot { mode: InputMode::Virtual, pad, ..Default::default() })
}

/// Which controllers the game sees: `policy()`, but a split-screen copy's seat while
/// the overlay has the controllers (`seat`), at rest.
pub(super) fn topology() -> Policy {
    match policy() {
        Policy::BlockAll => seat().map_or(Policy::BlockAll, Policy::Virtual),
        p => p,
    }
}

/// Whether physical controller input should be suppressed (true in both `BlockAll` and
/// `Virtual` modes).
pub fn suppress_physical_pads() -> bool {
    !matches!(policy(), Policy::PassThrough)
}

fn skipped() -> bool {
    crate::net::skip("padblock") || crate::net::skip("input")
}

type GetState = unsafe extern "system" fn(u32, *mut u8) -> u32;
type GetKeystroke = unsafe extern "system" fn(u32, u32, *mut u8) -> u32;
type SetState = unsafe extern "system" fn(u32, *mut u8) -> u32;
type GetCapabilities = unsafe extern "system" fn(u32, u32, *mut u8) -> u32;
type GetBattery = unsafe extern "system" fn(u32, u8, *mut u8) -> u32;
type GetRawInputData = unsafe extern "system" fn(isize, u32, *mut u8, *mut u32, u32) -> u32;
type GetRawInputBuffer = unsafe extern "system" fn(*mut u8, *mut u32, u32) -> u32;
type GetRawInputDeviceList = unsafe extern "system" fn(*mut RawDevice, *mut u32, u32) -> u32;
type GetAsyncKeyStateFn = unsafe extern "system" fn(i32) -> i16;
type GetKeyStateFn = unsafe extern "system" fn(i32) -> i16;
type GetKeyboardStateFn = unsafe extern "system" fn(*mut u8) -> i32;
type GetCursorPosFn = unsafe extern "system" fn(*mut POINT) -> i32;
type DevGetState = unsafe extern "system" fn(usize, u32, *mut u8) -> i32;
type DevGetData = unsafe extern "system" fn(usize, u32, *mut u8, *mut u32, u32) -> i32;
type DevSetCooperativeLevel = unsafe extern "system" fn(usize, isize, u32) -> i32;

/// Each hooked function gets a slot of its own (its own detour function, so a call
/// always goes on to the function it came from), kept for the life of the process.
pub(super) struct Slots<T: retour::Function, const N: usize> {
    addrs: Mutex<[usize; N]>,
    hooks: [OnceLock<GenericDetour<T>>; N],
}

impl<T: retour::Function, const N: usize> Slots<T, N> {
    pub(super) const fn new() -> Self {
        Slots { addrs: Mutex::new([0; N]), hooks: [const { OnceLock::new() }; N] }
    }
}

static STATES: Slots<GetState, 8> = Slots::new();
static KEYSTROKES: Slots<GetKeystroke, 4> = Slots::new();
static SET_STATES: Slots<SetState, 4> = Slots::new();
static CAPABILITIES: Slots<GetCapabilities, 4> = Slots::new();
static BATTERIES: Slots<GetBattery, 4> = Slots::new();
static RAW: OnceLock<GenericDetour<GetRawInputData>> = OnceLock::new();
static RAW_BUFFER: OnceLock<GenericDetour<GetRawInputBuffer>> = OnceLock::new();
static RAW_LIST: OnceLock<GenericDetour<GetRawInputDeviceList>> = OnceLock::new();
static ASYNC_KEY: OnceLock<GenericDetour<GetAsyncKeyStateFn>> = OnceLock::new();
static KEY_STATE: OnceLock<GenericDetour<GetKeyStateFn>> = OnceLock::new();
static KEYBOARD_STATE: OnceLock<GenericDetour<GetKeyboardStateFn>> = OnceLock::new();
static CURSOR_POS: OnceLock<GenericDetour<GetCursorPosFn>> = OnceLock::new();
static DEV_STATE: Slots<DevGetState, 2> = Slots::new();
static DEV_DATA: Slots<DevGetData, 2> = Slots::new();
static DEV_COOP: Slots<DevSetCooperativeLevel, 2> = Slots::new();
/// DirectInput devices' keys (`device.rs`), per device, asked once.
static DEV_KEYS: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());

static INSTALL: Once = Once::new();
/// Reads held back while the overlay showed: XInput states, raw-input reports, DirectInput reads.
static HELD_XINPUT: AtomicU32 = AtomicU32::new(0);
static HELD_RAW: AtomicU32 = AtomicU32::new(0);
static HELD_DINPUT: AtomicU32 = AtomicU32::new(0);
/// Per-30 s split-screen diagnostics (`log_summary`).
static XINPUT_CALLS: AtomicU32 = AtomicU32::new(0);
static XINPUT_OK: AtomicU32 = AtomicU32::new(0);
static XINPUT_ERR: AtomicU32 = AtomicU32::new(0);
static XINPUT_ACTIVE: AtomicU32 = AtomicU32::new(0);
static XINPUT_ABSENT: AtomicU32 = AtomicU32::new(0);
static XINPUT_REENTRANT: AtomicU32 = AtomicU32::new(0);
static RAW_HID_KEPT: AtomicU32 = AtomicU32::new(0);
static RAW_HID_DROPPED: AtomicU32 = AtomicU32::new(0);
static DINPUT_READS: AtomicU32 = AtomicU32::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Already inside one of our XInput hooks calling the real DLL: wrapper DLLs
    /// (`xinput1_3.dll`, `xinput9_1_0.dll`) forward into `xinput1_4.dll`, which is also
    /// hooked. Without this guard, routing controller 0 -> real slot 1..3 re-enters the
    /// inner hook with `pad != 0` and fails with `ERROR_DEVICE_NOT_CONNECTED`.
    static IN_XINPUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct XInputCall(bool);

impl XInputCall {
    fn enter() -> XInputCall {
        XInputCall(IN_XINPUT.with(|f| f.replace(true)))
    }
}

impl Drop for XInputCall {
    fn drop(&mut self) {
        IN_XINPUT.with(|f| f.set(self.0));
    }
}

fn in_xinput() -> bool {
    IN_XINPUT.with(std::cell::Cell::get)
}
/// DirectInput joysticks' axis ranges `(min, max)`, per device, asked once per opening.
static RANGES: Mutex<Vec<(usize, [Option<(i32, i32)>; 8])>> = Mutex::new(Vec::new());

const XINPUT: [&str; 4] = ["xinput1_4.dll", "xinput1_3.dll", "xinput9_1_0.dll", "xinputuap.dll"];
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

/// Loads `PEERPARTY_INPUT_SHM` at start-up (for split-screen instances) and installs input
/// hooks immediately if a virtual input block is configured.
pub fn init_from_env() {
    if skipped() {
        return;
    }
    if shm::init_from_env() {
        install();
    }
}

/// Attaches a virtual input SHM block announced by the host via `HostMsg::InputShm`.
pub fn attach_shm(name: &str) {
    if skipped() {
        return;
    }
    if shm::attach(name) {
        install();
    }
}

/// Hooks input APIs loaded in the process (called when the overlay opens or an input SHM
/// block is attached).
pub fn install() {
    if skipped() {
        return;
    }
    INSTALL.call_once(|| {
        let mut n = 0;
        for dll in XINPUT {
            n += hook_xinput(dll);
        }
        n += hook_raw_input();
        n += hook_kbm();
        n += hook_dinput();
        n += gameinput::install();
        n += hid::install();
        INSTALLED.store(true, Ordering::Relaxed);
        log!("input: {n} input entry points hooked");
        if shm::attached() {
            // A split-screen copy's CreateFile keeps other players' controllers from it
            // (device.rs `refuse_open`): the file hooks go in even when it has no saves
            // of its own. Off this thread: it may hold the loader lock.
            let _ = std::thread::Builder::new().name("peerparty-input".into()).spawn(crate::saves::arm_now);
        }
    });
    // Windows.Gaming.Input's methods are found through a connected pad, so this is
    // tried at every opening until it works; GameInput when its DLL arrived later.
    let n = wgi::install() + gameinput::install();
    if n > 0 {
        log!("input: {n} more input entry points hooked");
    }
}

/// Into the log every 30 s in split screen: how the game read controllers and what the
/// copy's virtual input block currently holds.
pub fn log_summary() {
    if !shm::attached() {
        return;
    }
    // Windows.Gaming.Input not hooked yet (no controller in its lists before): again.
    if INSTALLED.load(Ordering::Relaxed) {
        wgi::install();
    }
    let take = |a: &AtomicU32| a.swap(0, Ordering::Relaxed);
    let (calls, ok, err, active, absent, reent) = (
        take(&XINPUT_CALLS),
        take(&XINPUT_OK),
        take(&XINPUT_ERR),
        take(&XINPUT_ACTIVE),
        take(&XINPUT_ABSENT),
        take(&XINPUT_REENTRANT),
    );
    let (raw_kept, raw_drop, di) = (take(&RAW_HID_KEPT), take(&RAW_HID_DROPPED), take(&DINPUT_READS));
    let (hid_own, hid_unplugged) = hid::take_counts();
    let hid_blocked = hid::take_blocked_writes();
    let wgi_reads = wgi::take_reads();
    let own_hid = OWN_HID_READ.load(Ordering::Relaxed);
    let state = match policy() {
        Policy::Virtual(s) => format!(
            "Virtual(connected={}, xinput_slot={:?}, hid_key={:#x}, pkt={}, btns={:#06x})",
            s.pad.connected,
            s.pad.xinput_slot,
            s.pad.hid_key,
            s.pad.packet_number,
            s.pad.xinput_buttons(),
        ),
        Policy::BlockAll => "BlockAll".into(),
        Policy::PassThrough => "PassThrough".into(),
    };
    log!(
        "input: last 30 s: XInput {calls} calls ({ok} ok, {err} err, {active} active, {absent} absent, {reent} nested), HID {hid_own} own/{hid_unplugged} unplugged/{hid_blocked} writes held back,RawInput HID {raw_kept} kept/{raw_drop} dropped, DirectInput {di}, WGI {wgi_reads} ({}); state={state}, own_hid={own_hid:#x}",
        wgi::status()
    );
}

/// The overlay closed: what was held for this opening goes.
pub fn hidden() {
    RANGES.lock().unwrap_or_else(|e| e.into_inner()).clear();
    if INSTALLED.load(Ordering::Relaxed) {
        wgi::hidden();
        gameinput::hidden();
        hid::hidden();
    }
    let (x, r, d, h) = (
        HELD_XINPUT.swap(0, Ordering::Relaxed),
        HELD_RAW.swap(0, Ordering::Relaxed),
        HELD_DINPUT.swap(0, Ordering::Relaxed),
        hid::take_held(),
    );
    log!("input: held back while the overlay showed: {x} XInput reads, {r} raw-input reports, {d} DirectInput reads, {h} HID reads");
}

/// A controller module loaded (`watch.rs`): GameInput and XInput at once in a game with
/// the overlay or input SHM on, DirectInput after hooks first installed.
/// Windows' sound device API loaded (mmdevapi.dll, watch.rs): in a copy in a pane, the
/// other players' controllers' sound devices are left out of what the game finds
/// (audio.rs). Hooked at once: games pick their sound devices at start.
pub fn audio_loaded() {
    if crate::window::in_pane() && !skipped() {
        audio::install();
    }
}

pub fn module_loaded(name: &str) {
    let lower = name.to_ascii_lowercase();
    let early = (crate::ipc::enabled() || shm::attached()) && !skipped();
    if lower == "windows.gaming.input.dll" {
        // Loaded after the other hooks went in (a split-screen copy's go in when its
        // input block comes, often before the game starts on its controllers): without
        // this its reads stay unhooked, and every copy gets every controller.
        if INSTALLED.load(Ordering::Relaxed) {
            wgi::install();
        }
        return;
    }
    if lower.starts_with("gameinput") {
        if early {
            gameinput::module_loaded(&lower);
        }
        return;
    }
    if !INSTALLED.load(Ordering::Relaxed) && !(early && XINPUT.contains(&lower.as_str())) {
        return;
    }
    let n = if XINPUT.contains(&lower.as_str()) {
        hook_xinput(&lower)
    } else if lower == "dinput8.dll" {
        // Hooking it means calling into it (DirectInput8Create), which can't be done
        // from a load notification: the loader lock is held and dinput8's own start-up
        // hasn't run. A thread does it once the load is through.
        if crate::watch::in_loader() {
            let name = name.to_string();
            let _ = std::thread::Builder::new().name("peerparty-input".into()).spawn(move || {
                let n = hook_dinput();
                log!("input: {name} loaded, {n} hooked");
            });
            return;
        }
        hook_dinput()
    } else {
        return;
    };
    log!("input: {name} loaded, {n} hooked");
}

pub(super) fn module(name: &str) -> Option<windows_sys::Win32::Foundation::HMODULE> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let m = unsafe { GetModuleHandleW(wide.as_ptr()) };
    (!m.is_null()).then_some(m)
}

pub(super) fn proc_addr(m: windows_sys::Win32::Foundation::HMODULE, name: *const u8) -> Option<usize> {
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    unsafe { GetProcAddress(m, name) }.map(|f| f as usize)
}

/// Detours `addr` once (by address: several names can share one function), with the
/// first free slot's detour function.
pub(super) fn arm<T, const N: usize>(slots: &Slots<T, N>, addr: usize, detours: [T; N], label: &str) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let mut addrs = slots.addrs.lock().unwrap();
        if addrs.contains(&addr) {
            return 0;
        }
        let Some(i) = addrs.iter().position(|a| *a == 0) else {
            log!("input {label}: no slot left");
            return 0;
        };
        let target: T = unsafe { T::from_ptr(addr as *const ()) };
        match unsafe { GenericDetour::new(target, detours[i]) } {
            Ok(h) => {
                let _ = slots.hooks[i].set(h);
                if let Err(e) = crate::sys::hook::frozen(addr, || unsafe { slots.hooks[i].get().unwrap().enable() }) {
                    log!("input {label}: {e}");
                    // The slot keeps this detour (a OnceLock), so it can't be handed out again.
                    addrs[i] = usize::MAX;
                    return 0;
                }
                addrs[i] = addr;
                1
            }
            Err(e) => {
                log!("input {label} not hooked: {e}");
                0
            }
        }
    })
}

fn arm_once<T>(cell: &OnceLock<GenericDetour<T>>, addr: usize, detour: T, label: &str) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
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

fn hook_xinput(dll: &str) -> usize {
    let Some(m) = module(dll) else { return 0 };
    let mut n = 0;
    let mut names = Vec::new();
    for (name, label) in [
        (c"XInputGetState".as_ptr() as *const u8, "XInputGetState"),
        (100 as *const u8, "XInputGetStateEx"),
    ] {
        if let Some(a) = proc_addr(m, name) {
            let k = arm(&STATES, a, state_detours(), label);
            if k > 0 {
                names.push(format!("{label} at {a:#x}"));
            }
            n += k;
        }
    }
    if let Some(a) = proc_addr(m, c"XInputGetKeystroke".as_ptr().cast()) {
        n += arm(&KEYSTROKES, a, keystroke_detours(), "XInputGetKeystroke");
    }
    if let Some(a) = proc_addr(m, c"XInputSetState".as_ptr().cast()) {
        n += arm(&SET_STATES, a, per_slot!(SetState, [0, 1, 2, 3], set_state, (pad: u32, v: *mut u8) -> u32), "XInputSetState");
    }
    if let Some(a) = proc_addr(m, c"XInputGetCapabilities".as_ptr().cast()) {
        n += arm(&CAPABILITIES, a, per_slot!(GetCapabilities, [0, 1, 2, 3], capabilities, (pad: u32, flags: u32, out: *mut u8) -> u32), "XInputGetCapabilities");
    }
    if let Some(a) = proc_addr(m, c"XInputGetBatteryInformation".as_ptr().cast()) {
        n += arm(&BATTERIES, a, per_slot!(GetBattery, [0, 1, 2, 3], battery, (pad: u32, kind: u8, out: *mut u8) -> u32), "XInputGetBatteryInformation");
    }
    if !names.is_empty() {
        log!("input: {dll}: {}", names.join(", "));
    }
    n
}

fn state_detours() -> [GetState; 8] {
    per_slot!(GetState, [0, 1, 2, 3, 4, 5, 6, 7], get_state, (pad: u32, out: *mut u8) -> u32)
}

/// Packs `VirtualPad` into a 16-byte `XINPUT_STATE` buffer:
/// `[dwPacketNumber: u32, wButtons: u16, bLeftTrigger: u8, bRightTrigger: u8, sThumbLX..RY: i16]`.
pub(crate) fn pack_xinput_state(pad: &VirtualPad, out: &mut [u8; 16]) {
    out[0..4].copy_from_slice(&pad.packet_number.to_le_bytes());
    out[4..6].copy_from_slice(&pad.xinput_buttons().to_le_bytes());
    out[6] = pad.left_trigger;
    out[7] = pad.right_trigger;
    out[8..10].copy_from_slice(&pad.thumb_lx.to_le_bytes());
    out[10..12].copy_from_slice(&pad.thumb_ly.to_le_bytes());
    out[12..14].copy_from_slice(&pad.thumb_rx.to_le_bytes());
    out[14..16].copy_from_slice(&pad.thumb_ry.to_le_bytes());
}

/// Summarizes an `XINPUT_STATE` buffer's contents and whether any button, trigger, or
/// stick is active past a small deadzone.
fn xinput_inspect(out: *mut u8) -> Option<(u32, u16, u8, u8, i16, i16, i16, i16, bool)> {
    if out.is_null() {
        return None;
    }
    let s = unsafe { std::slice::from_raw_parts(out, 16) };
    let pkt = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    let btns = u16::from_le_bytes([s[4], s[5]]);
    let (lt, rt) = (s[6], s[7]);
    let i16_at = |at: usize| i16::from_le_bytes([s[at], s[at + 1]]);
    let (lx, ly, rx, ry) = (i16_at(8), i16_at(10), i16_at(12), i16_at(14));
    let active = btns != 0 || lt > 16 || rt > 16 || lx.unsigned_abs() > 8000 || ly.unsigned_abs() > 8000 || rx.unsigned_abs() > 8000 || ry.unsigned_abs() > 8000;
    Some((pkt, btns, lt, rt, lx, ly, rx, ry, active))
}

fn note_xinput_active(out: *mut u8) {
    if let Some((_, btns, lt, rt, lx, ly, rx, ry, true)) = xinput_inspect(out) {
        XINPUT_ACTIVE.fetch_add(1, Ordering::Relaxed);
        static FIRST_ACTIVE: AtomicBool = AtomicBool::new(false);
        if !FIRST_ACTIVE.swap(true, Ordering::Relaxed) {
            log!("input: XInput delivered first active input to the game (buttons={btns:#06x}, lt={lt}, rt={rt}, L=({lx},{ly}), R=({rx},{ry}))");
        }
    }
}

fn get_state(i: usize, pad: u32, out: *mut u8) -> u32 {
    if in_xinput() {
        XINPUT_REENTRANT.fetch_add(1, Ordering::Relaxed);
        let Some(h) = STATES.hooks[i].get() else { return ERROR_DEVICE_NOT_CONNECTED };
        return unsafe { h.call(pad, out) };
    }
    match policy() {
        Policy::PassThrough => {
            let Some(h) = STATES.hooks[i].get() else { return ERROR_DEVICE_NOT_CONNECTED };
            let _g = XInputCall::enter();
            unsafe { h.call(pad, out) }
        }
        Policy::BlockAll => {
            let Some(h) = STATES.hooks[i].get() else { return ERROR_DEVICE_NOT_CONNECTED };
            // A split-screen copy's controllers stay the ones it has (`seat`), at rest.
            let real = match seat().map(|snap| route(pad, &snap)) {
                None => pad,
                Some(Route::Real(k)) => k,
                Some(Route::Absent) => return ERROR_DEVICE_NOT_CONNECTED,
                Some(Route::Virtual) => {
                    if !out.is_null() {
                        let mut buf = [0u8; 16];
                        pack_xinput_state(&VirtualPad { connected: true, ..Default::default() }, &mut buf);
                        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), out, 16) };
                    }
                    HELD_XINPUT.fetch_add(1, Ordering::Relaxed);
                    return 0;
                }
            };
            let rc = {
                let _g = XInputCall::enter();
                unsafe { h.call(real, out) }
            };
            if rc == 0 && !out.is_null() {
                // XINPUT_STATE: packet number (kept), then the 12-byte XINPUT_GAMEPAD.
                unsafe { std::ptr::write_bytes(out.add(4), 0, 12) };
                HELD_XINPUT.fetch_add(1, Ordering::Relaxed);
            }
            rc
        }
        Policy::Virtual(snap) => {
            XINPUT_CALLS.fetch_add(1, Ordering::Relaxed);
            match route(pad, &snap) {
                Route::Real(k) => {
                    let Some(h) = STATES.hooks[i].get() else { return ERROR_DEVICE_NOT_CONNECTED };
                    let rc = {
                        let _g = XInputCall::enter();
                        unsafe { h.call(k, out) }
                    };
                    if rc == 0 {
                        XINPUT_OK.fetch_add(1, Ordering::Relaxed);
                        note_xinput_active(out);
                    } else {
                        XINPUT_ERR.fetch_add(1, Ordering::Relaxed);
                        static ERR_TOLD: AtomicBool = AtomicBool::new(false);
                        if !ERR_TOLD.swap(true, Ordering::Relaxed) {
                            log!("input: XInput real slot {k} (from controller 0, hook slot {i}) returned error {rc}");
                        }
                    }
                    if !USED_XINPUT.load(Ordering::Relaxed) {
                        let detail = match (rc, xinput_inspect(out)) {
                            (0, Some((pkt, btns, ..))) => format!("rc=0, pkt={pkt}, buttons={btns:#06x}"),
                            _ => format!("rc={rc}"),
                        };
                        first_use(&USED_XINPUT, &format!("XInput (hook slot {i}): its controller 0 is XInput slot {k} as it is ({detail}), the others aren't there"));
                    }
                    rc
                }
                Route::Virtual => {
                    first_use(&USED_XINPUT, &format!("XInput (hook slot {i}): its controller 0 is the virtual pad, the others aren't there"));
                    if !out.is_null() {
                        let mut buf = [0u8; 16];
                        pack_xinput_state(&snap.pad, &mut buf);
                        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), out, 16) };
                    }
                    XINPUT_OK.fetch_add(1, Ordering::Relaxed);
                    note_xinput_active(out);
                    0
                }
                Route::Absent => {
                    XINPUT_ABSENT.fetch_add(1, Ordering::Relaxed);
                    if pad == 0 {
                        if !snap.pad.connected {
                            static NO_SEAT_TOLD: AtomicBool = AtomicBool::new(false);
                            if !NO_SEAT_TOLD.swap(true, Ordering::Relaxed) {
                                log!("input: XInput controller 0 polled before a controller took this copy's seat (returning 1167)");
                            }
                        } else if snap.pad.xinput_slot.is_none() && own_hid_read(snap.pad.hid_key) {
                            static YIELD_TOLD: AtomicBool = AtomicBool::new(false);
                            if !YIELD_TOLD.swap(true, Ordering::Relaxed) {
                                log!("input: XInput controller 0 yields to this copy's native HID controller (hid_key={:#x})", snap.pad.hid_key);
                            }
                        }
                    }
                    ERROR_DEVICE_NOT_CONNECTED
                }
            }
        }
    }
}

/// XInput slot `slot`'s state as it is now, `XINPUT_STATE`'s 16 bytes (through the
/// detour's own way to the real call: the hooked export would route it as the copy's).
pub(super) fn real_xinput(slot: u32) -> Option<[u8; 16]> {
    let h = STATES.hooks.iter().find_map(|h| h.get())?;
    let mut s = [0u8; 16];
    let _g = XInputCall::enter();
    (unsafe { h.call(slot, s.as_mut_ptr()) } == 0).then_some(s)
}

/// `XINPUT_STATE`'s bytes as a virtual pad on slot `slot`.
pub(super) fn pad_of_xinput(slot: u8, s: &[u8; 16]) -> VirtualPad {
    let i16_at = |at: usize| i16::from_le_bytes([s[at], s[at + 1]]);
    VirtualPad {
        connected: true,
        buttons: u32::from(u16::from_le_bytes([s[4], s[5]])),
        left_trigger: s[6],
        right_trigger: s[7],
        thumb_lx: i16_at(8),
        thumb_ly: i16_at(10),
        thumb_rx: i16_at(12),
        thumb_ry: i16_at(14),
        packet_number: u32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        xinput_slot: Some(slot),
        hid_key: 0,
    }
}

/// Where a split-screen copy's XInput call for controller `pad` goes: the copy has one
/// controller, its first; it's a real XInput slot (called as it is), or the virtual pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Real(u32),
    Virtual,
    Absent,
}

/// The copy's own HID controller (`hid_key`), once the game reads it natively (direct
/// HID, Raw Input, or DirectInput): XInput and Windows.Gaming.Input don't also synthesize
/// a virtual Xbox pad for that same controller, so the game doesn't receive its input twice.
static OWN_HID_READ: AtomicU64 = AtomicU64::new(0);

pub(super) fn note_own_hid_read(key: u64) {
    if key != 0 {
        OWN_HID_READ.store(key, Ordering::Relaxed);
    }
}

pub(super) fn own_hid_read(key: u64) -> bool {
    key != 0 && OWN_HID_READ.load(Ordering::Relaxed) == key
}

fn route(pad: u32, snap: &InputSnapshot) -> Route {
    let own_hid = OWN_HID_READ.load(Ordering::Relaxed);
    let waiting = snap.pad.xinput_slot.is_none()
        && snap.pad.hid_key != 0
        && snap.pad.hid_key != own_hid
        && device::game_opens_pads()
        && seat_age().is_some_and(|age| age < NATIVE_WAIT);
    if waiting && pad == 0 {
        static TOLD: AtomicBool = AtomicBool::new(false);
        if !TOLD.swap(true, Ordering::Relaxed) {
            log!("input: XInput controller 0 waits for the game to open this copy's HID controller itself (it opens controllers), up to {} s", NATIVE_WAIT.as_secs());
        }
    }
    route_with(pad, snap, if waiting { snap.pad.hid_key } else { own_hid })
}

/// How long a copy whose game opens HID controllers itself (`device::game_opens_pads`)
/// is shown no XInput pad for its new HID controller, waiting for the game to open it:
/// shown the virtual one meanwhile, a game took the DualSense player for an Xbox one and
/// showed Xbox buttons. One that doesn't open it gets the virtual pad after this.
const NATIVE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long the seat has had the controller it has now (`note_seat`).
fn seat_age() -> Option<std::time::Duration> {
    let at = SEAT_AT_MS.load(Ordering::Relaxed);
    (at != 0).then(|| std::time::Duration::from_millis(since_start_ms().saturating_sub(at)))
}

fn since_start_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    // 1 at the start, so 0 stays "never".
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

fn route_with(pad: u32, snap: &InputSnapshot, own_hid: u64) -> Route {
    if pad != 0 || !snap.pad.connected {
        return Route::Absent;
    }
    match snap.pad.xinput_slot {
        Some(k) => Route::Real(u32::from(k)),
        None if snap.pad.hid_key != 0 && snap.pad.hid_key == own_hid => Route::Absent,
        None => Route::Virtual,
    }
}

/// Vibration: a split-screen copy's goes to its own controller (none for a virtual pad:
/// it would shake another player's).
fn set_state(i: usize, pad: u32, v: *mut u8) -> u32 {
    let Some(h) = SET_STATES.hooks[i].get() else { return ERROR_DEVICE_NOT_CONNECTED };
    if in_xinput() {
        return unsafe { h.call(pad, v) };
    }
    match topology() {
        Policy::Virtual(snap) => match route(pad, &snap) {
            Route::Real(k) => {
                let _g = XInputCall::enter();
                unsafe { h.call(k, v) }
            }
            Route::Virtual => 0,
            Route::Absent => ERROR_DEVICE_NOT_CONNECTED,
        },
        _ => {
            let _g = XInputCall::enter();
            unsafe { h.call(pad, v) }
        }
    }
}

/// `XINPUT_CAPABILITIES` for a virtual pad: a gamepad with every button, both triggers
/// and sticks, and vibration.
pub(crate) fn virtual_capabilities(out: &mut [u8; 20]) {
    out.fill(0);
    out[0] = 1; // XINPUT_DEVTYPE_GAMEPAD
    out[1] = 1; // XINPUT_DEVSUBTYPE_GAMEPAD
    out[4..6].copy_from_slice(&0xF3FFu16.to_le_bytes());
    out[6] = 0xFF;
    out[7] = 0xFF;
    for at in [8, 10, 12, 14] {
        out[at..at + 2].copy_from_slice(&0xFFC0u16.to_le_bytes());
    }
    out[16..20].copy_from_slice(&[0xFF; 4]);
}

fn capabilities(i: usize, pad: u32, flags: u32, out: *mut u8) -> u32 {
    let Some(h) = CAPABILITIES.hooks[i].get() else { return ERROR_DEVICE_NOT_CONNECTED };
    if in_xinput() {
        return unsafe { h.call(pad, flags, out) };
    }
    match topology() {
        Policy::Virtual(snap) => {
            let r = route(pad, &snap);
            let rc = match r {
                Route::Real(k) => {
                    let _g = XInputCall::enter();
                    unsafe { h.call(k, flags, out) }
                }
                Route::Virtual => {
                    if !out.is_null() {
                        let mut caps = [0u8; 20];
                        virtual_capabilities(&mut caps);
                        unsafe { std::ptr::copy_nonoverlapping(caps.as_ptr(), out, caps.len()) };
                    }
                    0
                }
                Route::Absent => ERROR_DEVICE_NOT_CONNECTED,
            };
            static CAPS_TOLD: AtomicBool = AtomicBool::new(false);
            if pad == 0 && !CAPS_TOLD.swap(true, Ordering::Relaxed) {
                log!("input: XInputGetCapabilities(0) -> {r:?}, rc={rc}");
            }
            rc
        }
        _ => {
            let _g = XInputCall::enter();
            unsafe { h.call(pad, flags, out) }
        }
    }
}

fn battery(i: usize, pad: u32, kind: u8, out: *mut u8) -> u32 {
    let Some(h) = BATTERIES.hooks[i].get() else { return ERROR_DEVICE_NOT_CONNECTED };
    if in_xinput() {
        return unsafe { h.call(pad, kind, out) };
    }
    match topology() {
        Policy::Virtual(snap) => match route(pad, &snap) {
            Route::Real(k) => {
                let _g = XInputCall::enter();
                unsafe { h.call(k, kind, out) }
            }
            Route::Virtual => {
                if !out.is_null() {
                    // XINPUT_BATTERY_INFORMATION: wired, full.
                    unsafe { std::ptr::copy_nonoverlapping([1u8, 3].as_ptr(), out, 2) };
                }
                0
            }
            Route::Absent => ERROR_DEVICE_NOT_CONNECTED,
        },
        _ => {
            let _g = XInputCall::enter();
            unsafe { h.call(pad, kind, out) }
        }
    }
}

fn keystroke_detours() -> [GetKeystroke; 4] {
    per_slot!(GetKeystroke, [0, 1, 2, 3], get_keystroke, (pad: u32, reserved: u32, out: *mut u8) -> u32)
}

const ERROR_EMPTY: u32 = 0x10D2;
/// `XUSER_INDEX_ANY`.
const ANY_USER: u32 = 0xFF;

fn get_keystroke(i: usize, pad: u32, reserved: u32, out: *mut u8) -> u32 {
    let Some(h) = KEYSTROKES.hooks[i].get() else { return ERROR_EMPTY };
    if in_xinput() {
        return unsafe { h.call(pad, reserved, out) };
    }
    match policy() {
        Policy::PassThrough => {
            let _g = XInputCall::enter();
            unsafe { h.call(pad, reserved, out) }
        }
        Policy::BlockAll => ERROR_EMPTY,
        Policy::Virtual(snap) => match route(if pad == ANY_USER { 0 } else { pad }, &snap) {
            Route::Real(k) => {
                let rc = {
                    let _g = XInputCall::enter();
                    unsafe { h.call(k, reserved, out) }
                };
                if rc == 0 && !out.is_null() {
                    // XINPUT_KEYSTROKE's UserIndex: the copy's own first controller.
                    unsafe { *out.add(6) = 0 };
                }
                rc
            }
            Route::Virtual => ERROR_EMPTY,
            Route::Absent if pad == ANY_USER => ERROR_EMPTY,
            Route::Absent => ERROR_DEVICE_NOT_CONNECTED,
        },
    }
}

fn hook_raw_input() -> usize {
    let Some(m) = module("user32.dll") else { return 0 };
    let mut n = 0;
    if let Some(a) = proc_addr(m, c"GetRawInputData".as_ptr().cast()) {
        n += arm_once(&RAW, a, raw_input_data as GetRawInputData, "GetRawInputData");
    }
    if let Some(a) = proc_addr(m, c"GetRawInputBuffer".as_ptr().cast()) {
        n += arm_once(&RAW_BUFFER, a, raw_input_buffer as GetRawInputBuffer, "GetRawInputBuffer");
    }
    if let Some(a) = proc_addr(m, c"GetRawInputDeviceList".as_ptr().cast()) {
        n += arm_once(&RAW_LIST, a, raw_device_list as GetRawInputDeviceList, "GetRawInputDeviceList");
    }
    n
}

/// `RAWINPUTDEVICELIST`: the device's handle and its type.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawDevice {
    handle: isize,
    kind: u32,
}

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

thread_local! {
    /// Inside `raw_device_list` on this thread: a hook of another program's on what the
    /// filter calls (an overlay's, of the HID functions) asked for the list again. That
    /// call gets the real list, or the two would call each other until the stack ran out.
    static LISTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Raw Input's devices, without other players' game controllers in a split-screen copy
/// (`device::is_others`): the copy never learns they're there.
unsafe extern "system" fn raw_device_list(list: *mut RawDevice, count: *mut u32, size: u32) -> u32 {
    let Some(real) = RAW_LIST.get() else { return u32::MAX };
    if LISTING.with(|l| l.replace(true)) {
        return unsafe { real.call(list, count, size) };
    }
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            LISTING.with(|l| l.set(false));
        }
    }
    let _done = Done;
    let Policy::Virtual(snap) = topology() else { return unsafe { real.call(list, count, size) } };
    if count.is_null() || size as usize != size_of::<RawDevice>() {
        return unsafe { real.call(list, count, size) };
    }
    // Every device there is, then the ones this copy may see.
    let mut all: Vec<RawDevice> = Vec::new();
    for _ in 0..4 {
        let mut n = 0u32;
        if unsafe { real.call(std::ptr::null_mut(), &mut n, size) } == u32::MAX {
            return unsafe { real.call(list, count, size) };
        }
        all = vec![RawDevice { handle: 0, kind: 0 }; n as usize + 4];
        let mut room = all.len() as u32;
        let got = unsafe { real.call(all.as_mut_ptr(), &mut room, size) };
        if got != u32::MAX {
            all.truncate(got as usize);
            break;
        }
        all.clear();
    }
    let before = all.len();
    let mut dropped = Vec::new();
    all.retain(|d| {
        if d.kind != RIM_TYPEHID {
            return true;
        }
        if let Some(h) = device::of_raw(d.handle)
            && device::is_others(&h, &snap)
        {
            dropped.push(format!("{:04x}:{:04x}", h.vid, h.pid));
            return false;
        }
        true
    });
    static TOLD: AtomicBool = AtomicBool::new(false);
    if before != all.len() && !TOLD.swap(true, Ordering::Relaxed) {
        log!("input: Raw Input's device list leaves out {} other players' controllers ({})", before - all.len(), dropped.join(", "));
    }
    let n = all.len() as u32;
    if list.is_null() {
        unsafe { *count = n };
        return 0;
    }
    if unsafe { *count } < n {
        unsafe { *count = n };
        unsafe { windows_sys::Win32::Foundation::SetLastError(ERROR_INSUFFICIENT_BUFFER) };
        return u32::MAX;
    }
    unsafe { std::ptr::copy_nonoverlapping(all.as_ptr(), list, all.len()) };
    n
}

fn hook_kbm() -> usize {
    let Some(m) = module("user32.dll") else { return 0 };
    let mut n = 0;
    if let Some(a) = proc_addr(m, c"GetAsyncKeyState".as_ptr().cast()) {
        n += arm_once(&ASYNC_KEY, a, async_key_state as GetAsyncKeyStateFn, "GetAsyncKeyState");
    }
    if let Some(a) = proc_addr(m, c"GetKeyState".as_ptr().cast()) {
        n += arm_once(&KEY_STATE, a, key_state as GetKeyStateFn, "GetKeyState");
    }
    if let Some(a) = proc_addr(m, c"GetKeyboardState".as_ptr().cast()) {
        n += arm_once(&KEYBOARD_STATE, a, keyboard_state as GetKeyboardStateFn, "GetKeyboardState");
    }
    if let Some(a) = proc_addr(m, c"GetCursorPos".as_ptr().cast()) {
        n += arm_once(&CURSOR_POS, a, cursor_pos as GetCursorPosFn, "GetCursorPos");
    }
    n
}

const RID_INPUT: u32 = 0x1000_0003;
const RIM_TYPEMOUSE: u32 = 0;
const RIM_TYPEKEYBOARD: u32 = 1;
const RIM_TYPEHID: u32 = 2;

unsafe extern "system" fn raw_input_data(h: isize, cmd: u32, data: *mut u8, size: *mut u32, header: u32) -> u32 {
    let Some(d) = RAW.get() else { return u32::MAX };
    let rc = unsafe { d.call(h, cmd, data, size, header) };
    if cmd != RID_INPUT || data.is_null() || rc == u32::MAX || (rc as usize) < 4 {
        return rc;
    }
    // `rc`: the bytes written to `data`.
    let report = unsafe { std::slice::from_raw_parts_mut(data, rc as usize) };
    if keep_raw(report) {
        rc
    } else {
        HELD_RAW.fetch_add(1, Ordering::Relaxed);
        u32::MAX
    }
}

/// Whether one raw input report (`RAWINPUT`, header first) reaches the game now, as it
/// is or rewritten (`releases_only`).
fn keep_raw(report: &mut [u8]) -> bool {
    if report.len() < RAW_HEADER {
        return true;
    }
    let kind = u32::from_le_bytes([report[0], report[1], report[2], report[3]]);
    let block = match policy() {
        Policy::PassThrough => false,
        Policy::BlockAll => kind == RIM_TYPEHID || (shm::attached() && matches!(kind, RIM_TYPEKEYBOARD | RIM_TYPEMOUSE)),
        Policy::Virtual(snap) => match kind {
            // RAWINPUTHEADER: type, size, then the device's handle.
            RIM_TYPEHID => {
                let dev = isize::from_le_bytes(report[8..8 + size_of::<isize>()].try_into().unwrap());
                first_use(&USED_RAW_HID, "Raw Input reports from HID controllers (only its own reach it)");
                let key = device::key_of_raw(dev);
                let own = device::is_own(key, &snap);
                if own {
                    note_own_hid_read(key);
                    RAW_HID_KEPT.fetch_add(1, Ordering::Relaxed);
                } else {
                    RAW_HID_DROPPED.fetch_add(1, Ordering::Relaxed);
                }
                !own
            }
            RIM_TYPEKEYBOARD => !snap.keyboard.active,
            RIM_TYPEMOUSE => !snap.mouse.active,
            _ => false,
        },
    };
    // A key or button let go still reaches a copy that doesn't have the keyboard and
    // mouse now (the focus moved to another pane, or its overlay opened while it was
    // held): it was pressed while the copy had them, and would stay down there for good.
    !block || (shm::attached() && matches!(kind, RIM_TYPEKEYBOARD | RIM_TYPEMOUSE) && releases_only(kind, report))
}

/// Raw input read in batches: the reports a copy shouldn't get (`keep_raw`) are taken
/// out and the rest moved up, each where the next block starts (8-byte aligned).
unsafe extern "system" fn raw_input_buffer(data: *mut u8, size: *mut u32, header: u32) -> u32 {
    let Some(d) = RAW_BUFFER.get() else { return u32::MAX };
    let n = unsafe { d.call(data, size, header) };
    // A 32-bit game on 64-bit Windows gets 64-bit headers in batches: not parsed here.
    if data.is_null() || n == 0 || n == u32::MAX || size_of::<usize>() == 4 || matches!(policy(), Policy::PassThrough) {
        return n;
    }
    first_use(&USED_RAW_BUFFER, "Raw Input in batches (GetRawInputBuffer)");
    // `size`: the buffer's size in bytes (the reports are inside it).
    let room = unsafe { size.as_ref() }.map_or(0, |s| *s as usize);
    let align = |o: usize| (o + 7) & !7;
    let (mut read, mut write, mut kept) = (0usize, 0usize, 0u32);
    for _ in 0..n {
        if read + RAW_HEADER > room {
            break;
        }
        let len = u32::from_le_bytes(unsafe { std::slice::from_raw_parts(data.add(read + 4), 4) }.try_into().unwrap()) as usize;
        if len < RAW_HEADER || read + len > room {
            break;
        }
        let report = unsafe { std::slice::from_raw_parts_mut(data.add(read), len) };
        if keep_raw(report) {
            if write != read {
                unsafe { std::ptr::copy(data.add(read), data.add(write), len) };
            }
            write = align(write + len);
            kept += 1;
        } else {
            HELD_RAW.fetch_add(1, Ordering::Relaxed);
        }
        read = align(read + len);
    }
    kept
}

/// `RAWINPUTHEADER`'s size: type, size, then the device's handle and a WPARAM.
const RAW_HEADER: usize = 8 + 2 * size_of::<usize>();
/// `RI_KEY_BREAK`: the key went up.
const RI_KEY_BREAK: u16 = 0x01;
/// `RI_MOUSE_*_BUTTON_UP`, left, right, middle, 4 and 5.
const MOUSE_UPS: u16 = 0x0002 | 0x0008 | 0x0020 | 0x0080 | 0x0200;
const MOUSE_MOVE_ABSOLUTE: u16 = 0x01;

/// Pure helper: whether a raw keyboard or mouse report for a copy without the keyboard
/// and mouse goes through, rewritten to its releases alone: a key up as it is; a mouse
/// report as the buttons that went up (none, often), without its motion, wheel or
/// presses. False (the report is held back) for a key down or an absolute mouse.
pub(crate) fn releases_only(kind: u32, report: &mut [u8]) -> bool {
    let u16_at = |r: &[u8], at: usize| u16::from_le_bytes([r[at], r[at + 1]]);
    match kind {
        // RAWKEYBOARD: MakeCode, Flags, Reserved, VKey, Message, ExtraInformation.
        RIM_TYPEKEYBOARD => report.len() >= RAW_HEADER + 4 && u16_at(report, RAW_HEADER + 2) & RI_KEY_BREAK != 0,
        // RAWMOUSE: usFlags, 2 bytes, usButtonFlags, usButtonData, ulRawButtons,
        // lLastX, lLastY, ulExtraInformation.
        RIM_TYPEMOUSE => {
            let m = RAW_HEADER;
            if report.len() < m + 20 {
                return false;
            }
            let ups = u16_at(report, m + 4) & MOUSE_UPS;
            // An absolute mouse's position can't be taken out: it would jump. Any
            // other report goes as its releases alone, or as nothing at all: a report
            // held back is an error the game may log every time (Unity does).
            if u16_at(report, m) & MOUSE_MOVE_ABSOLUTE != 0 {
                return false;
            }
            report[m + 4..m + 6].copy_from_slice(&ups.to_le_bytes());
            report[m + 6..m + 8].fill(0);
            report[m + 8..m + 20].fill(0);
            true
        }
        _ => false,
    }
}

/// Pure helper: answers `GetAsyncKeyState(vkey)` from a `Virtual` snapshot.
pub(crate) fn virtual_async_key_state(vkey: i32, snap: &InputSnapshot) -> i16 {
    let down = match vkey {
        0x01 => snap.mouse.active && snap.mouse.is_down(MOUSE_LEFT),
        0x02 => snap.mouse.active && snap.mouse.is_down(MOUSE_RIGHT),
        0x04 => snap.mouse.active && snap.mouse.is_down(MOUSE_MIDDLE),
        0x05 => snap.mouse.active && snap.mouse.is_down(MOUSE_X1),
        0x06 => snap.mouse.active && snap.mouse.is_down(MOUSE_X2),
        0..=255 => snap.keyboard.active && snap.keyboard.is_down(vkey as u8),
        _ => false,
    };
    if down { i16::MIN } else { 0 }
}

unsafe extern "system" fn async_key_state(vkey: i32) -> i16 {
    match policy() {
        Policy::PassThrough => ASYNC_KEY.get().map_or(0, |h| unsafe { h.call(vkey) }),
        Policy::BlockAll => {
            if shm::attached() {
                0
            } else {
                ASYNC_KEY.get().map_or(0, |h| unsafe { h.call(vkey) })
            }
        }
        Policy::Virtual(snap) => virtual_async_key_state(vkey, &snap),
    }
}

unsafe extern "system" fn key_state(vkey: i32) -> i16 {
    match policy() {
        Policy::PassThrough => KEY_STATE.get().map_or(0, |h| unsafe { h.call(vkey) }),
        Policy::BlockAll => {
            if shm::attached() {
                0
            } else {
                KEY_STATE.get().map_or(0, |h| unsafe { h.call(vkey) })
            }
        }
        Policy::Virtual(snap) => virtual_async_key_state(vkey, &snap),
    }
}

unsafe extern "system" fn keyboard_state(out: *mut u8) -> i32 {
    if out.is_null() {
        return 0;
    }
    match policy() {
        Policy::PassThrough => KEYBOARD_STATE.get().map_or(0, |h| unsafe { h.call(out) }),
        Policy::BlockAll => {
            if shm::attached() {
                unsafe { std::ptr::write_bytes(out, 0, 256) };
                1
            } else {
                KEYBOARD_STATE.get().map_or(0, |h| unsafe { h.call(out) })
            }
        }
        Policy::Virtual(snap) => {
            let buf = unsafe { std::slice::from_raw_parts_mut(out, 256) };
            for (vk, slot) in buf.iter_mut().enumerate() {
                *slot = if virtual_async_key_state(vk as i32, &snap) < 0 { 0x80 } else { 0 };
            }
            1
        }
    }
}

/// The last cursor position a split-screen copy was given while it had the mouse, packed
/// (x in the low half); u64::MAX for none yet.
static LAST_CURSOR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
/// Where a copy that has never had the mouse sees the cursor: Windows' own "nowhere"
/// (where it puts minimized windows), off every screen.
const OFF_WINDOW: (i32, i32) = (-32000, -32000);

unsafe extern "system" fn cursor_pos(pt: *mut POINT) -> i32 {
    if let Policy::Virtual(snap) = policy()
        && !pt.is_null()
    {
        if snap.mouse.active {
            unsafe {
                (*pt).x = snap.mouse.cursor_x;
                (*pt).y = snap.mouse.cursor_y;
            }
            LAST_CURSOR.store(u64::from(snap.mouse.cursor_x as u32) | (u64::from(snap.mouse.cursor_y as u32) << 32), Ordering::Relaxed);
            return 1;
        }
        // Another pane has the mouse: this copy's cursor stays where it last was (the
        // real one is over another copy, maybe over this one's window too: every copy's
        // window sits at the same place behind the session's). One that has never had
        // the mouse sees it far off its window, so nothing in it is hovered.
        let last = LAST_CURSOR.load(Ordering::Relaxed);
        let (x, y) = if last != u64::MAX { (last as u32 as i32, (last >> 32) as u32 as i32) } else { OFF_WINDOW };
        unsafe {
            (*pt).x = x;
            (*pt).y = y;
        }
        return 1;
    }
    CURSOR_POS.get().map_or(0, |h| unsafe { h.call(pt) })
}

fn hook_dinput() -> usize {
    let Some(m) = module("dinput8.dll") else { return 0 };
    let Some(create) = proc_addr(m, c"DirectInput8Create".as_ptr().cast()) else { return 0 };
    type Create = unsafe extern "system" fn(isize, u32, *const [u8; 16], *mut usize, usize) -> i32;
    let create: Create = unsafe { std::mem::transmute(create) };
    // IID_IDirectInput8W / A and GUID_SysKeyboard.
    let iid_w = guid(0xBF798031, 0x483A, 0x4DA2, [0xAA, 0x99, 0x5D, 0x64, 0xED, 0x36, 0x97, 0x00]);
    let iid_a = guid(0xBF798030, 0x483A, 0x4DA2, [0xAA, 0x99, 0x5D, 0x64, 0xED, 0x36, 0x97, 0x00]);
    let keyboard = guid(0x6F1D2B61, 0xD5A0, 0x11CF, [0xBF, 0xC7, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00]);
    let instance = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) } as isize;
    type CreateDevice = unsafe extern "system" fn(usize, *const [u8; 16], *mut usize, usize) -> i32;
    type Release = unsafe extern "system" fn(usize) -> u32;
    let mut n = 0;
    for iid in [iid_w, iid_a] {
        let mut di = 0usize;
        if unsafe { create(instance, 0x0800, &iid, &mut di, 0) } < 0 || di == 0 {
            continue;
        }
        // IDirectInput8: Release is slot 2, CreateDevice slot 3.
        let vt = unsafe { *(di as *const *const usize) };
        let create_device: CreateDevice = unsafe { std::mem::transmute(*vt.add(3)) };
        let release: Release = unsafe { std::mem::transmute(*vt.add(2)) };
        let mut dev = 0usize;
        if unsafe { create_device(di, &keyboard, &mut dev, 0) } >= 0 && dev != 0 {
            // IDirectInputDevice8: GetDeviceState is slot 9, GetDeviceData slot 10.
            let dvt = unsafe { *(dev as *const *const usize) };
            n += arm(&DEV_STATE, unsafe { *dvt.add(9) }, dev_state_detours(), "IDirectInputDevice8::GetDeviceState");
            n += arm(&DEV_DATA, unsafe { *dvt.add(10) }, dev_data_detours(), "IDirectInputDevice8::GetDeviceData");
            // SetCooperativeLevel is slot 13.
            n += arm(
                &DEV_COOP,
                unsafe { *dvt.add(13) },
                per_slot!(DevSetCooperativeLevel, [0, 1], set_cooperative_level, (this: usize, hwnd: isize, flags: u32) -> i32),
                "IDirectInputDevice8::SetCooperativeLevel",
            );
            let dev_release: Release = unsafe { std::mem::transmute(*dvt.add(2)) };
            unsafe { dev_release(dev) };
        }
        unsafe { release(di) };
    }
    n
}

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

const DISCL_EXCLUSIVE: u32 = 0x1;
const DISCL_NONEXCLUSIVE: u32 = 0x2;
const DISCL_FOREGROUND: u32 = 0x4;
const DISCL_BACKGROUND: u32 = 0x8;
const DISCL_NOWINKEY: u32 = 0x10;

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

/// Whether DirectInput device `this` is a keyboard or mouse (`GetCapabilities`, slot 3).
fn keyboard_or_mouse(this: usize) -> bool {
    type GetCaps = unsafe extern "system" fn(usize, *mut [u32; 11]) -> i32;
    let get: GetCaps = unsafe { std::mem::transmute(*(*(this as *const *const usize)).add(3)) };
    let mut caps = [0u32; 11];
    caps[0] = 44;
    // DIDEVCAPS.dwDevType's low byte: DI8DEVTYPE_MOUSE 0x12, DI8DEVTYPE_KEYBOARD 0x13.
    (unsafe { get(this, &mut caps) }) >= 0 && matches!(caps[2] & 0xFF, 0x12 | 0x13)
}

fn set_cooperative_level(i: usize, this: usize, hwnd: isize, flags: u32) -> i32 {
    let Some(h) = DEV_COOP.hooks[i].get() else { return 0 };
    if crate::window::focus_spoofed() {
        let want = background_level(flags, keyboard_or_mouse(this));
        if want != flags {
            let rc = unsafe { h.call(this, hwnd, want) };
            if rc >= 0 {
                static TOLD: AtomicBool = AtomicBool::new(false);
                if !TOLD.swap(true, Ordering::Relaxed) {
                    log!("input: DirectInput asked for foreground access ({flags:#x}); given background ({want:#x}), so its pane keeps its input while another has the focus");
                }
                return rc;
            }
        }
    }
    unsafe { h.call(this, hwnd, flags) }
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

/// DirectInput device `this`'s key (`device.rs`), asked once; 0 for a device that
/// isn't a HID one (the system keyboard and mouse).
fn dev_key(this: usize) -> u64 {
    if let Some(&(_, k)) = DEV_KEYS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(d, _)| *d == this) {
        return k;
    }
    // IDirectInputDevice8::GetProperty is slot 5; DIPROP_GUIDANDPATH is MAKEDIPROP(12).
    type GetProperty = unsafe extern "system" fn(usize, usize, *mut PropPath) -> i32;
    let get: GetProperty = unsafe { std::mem::transmute(*(*(this as *const *const usize)).add(5)) };
    let mut p = PropPath { size: size_of::<PropPath>() as u32, header_size: 16, obj: 0, how: 0, class: [0; 16], path: [0; 260] };
    let key = if unsafe { get(this, 12, &mut p) } >= 0 {
        let n = p.path.iter().position(|c| *c == 0).unwrap_or(p.path.len() - 1);
        device::key_of_path(&p.path[..=n.min(p.path.len() - 1)])
    } else {
        0
    };
    let mut keys = DEV_KEYS.lock().unwrap_or_else(|e| e.into_inner());
    if keys.len() >= 16 {
        keys.remove(0);
    }
    keys.push((this, key));
    key
}

fn dev_state_detours() -> [DevGetState; 2] {
    per_slot!(DevGetState, [0, 1], dev_state, (this: usize, size: u32, out: *mut u8) -> i32)
}

fn dev_state(i: usize, this: usize, size: u32, out: *mut u8) -> i32 {
    let Some(h) = DEV_STATE.hooks[i].get() else { return 0 };
    let rc = unsafe { h.call(this, size, out) };
    if rc < 0 || out.is_null() {
        return rc;
    }
    match policy() {
        Policy::PassThrough => {}
        Policy::BlockAll => {
            HELD_DINPUT.fetch_add(1, Ordering::Relaxed);
            let buf = unsafe { std::slice::from_raw_parts_mut(out, size as usize) };
            if size == 80 || size == 272 {
                let c = centres_of(&ranges(this));
                at_rest(buf, &c);
            } else {
                buf.fill(0);
            }
        }
        Policy::Virtual(snap) => {
            let buf = unsafe { std::slice::from_raw_parts_mut(out, size as usize) };
            if size == 80 || size == 272 {
                DINPUT_READS.fetch_add(1, Ordering::Relaxed);
                first_use(&USED_DINPUT, "DirectInput joysticks (only its own moves)");
                let key = dev_key(this);
                if device::is_own(key, &snap) {
                    note_own_hid_read(key);
                    // The copy's own controller: its real state.
                    return rc;
                }
                let r = ranges(this);
                // Another player's controller rests; with none of the copy's own to be
                // found (an XInput pad, or no key), every joystick reads the virtual pad.
                if snap.pad.connected && snap.pad.hid_key == 0 {
                    pack_dinput_pad(buf, &snap.pad, &r);
                } else {
                    at_rest(buf, &centres_of(&r));
                }
            } else if size == 256 {
                buf.fill(0);
                if snap.keyboard.active {
                    pack_dinput_keyboard(buf, &snap.keyboard);
                }
            } else if !snap.mouse.active {
                buf.fill(0);
            }
        }
    }
    rc
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

/// The `(min, max)` of each DIJOYSTATE axis's range (lX, lY, lZ, lRx, lRy, lRz, two sliders),
/// None for one the device doesn't have.
fn ranges(this: usize) -> [Option<(i32, i32)>; 8] {
    let mut known = RANGES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, r)) = known.iter().find(|(d, _)| *d == this) {
        return *r;
    }
    // IDirectInputDevice8::GetProperty is slot 5; DIPROP_RANGE is MAKEDIPROP(4), and
    // DIPH_BYOFFSET finds an axis by its offset in the standard joystick format.
    type GetProperty = unsafe extern "system" fn(usize, usize, *mut PropRange) -> i32;
    let get: GetProperty = unsafe { std::mem::transmute(*(*(this as *const *const usize)).add(5)) };
    let mut out = [None; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        let mut r = PropRange { size: 24, header_size: 16, obj: i as u32 * 4, how: 1, min: 0, max: 0 };
        if unsafe { get(this, 4, &mut r) } >= 0 {
            *slot = Some((r.min, r.max));
        }
    }
    if known.len() < 16 {
        known.push((this, out));
    }
    out
}

fn centres_of(ranges: &[Option<(i32, i32)>; 8]) -> [Option<i32>; 8] {
    let mut c = [None; 8];
    for (dst, r) in c.iter_mut().zip(ranges) {
        *dst = r.map(|(min, max)| middle(min, max));
    }
    c
}

fn middle(min: i32, max: i32) -> i32 {
    ((min as i64 + max as i64) / 2) as i32
}

fn map_axis(norm: f64, range: (i32, i32)) -> i32 {
    let (min, max) = (range.0 as f64, range.1 as f64);
    let t = ((norm.clamp(-1.0, 1.0) + 1.0) * 0.5).clamp(0.0, 1.0);
    (min + t * (max - min)).round() as i32
}

/// A DIJOYSTATE (80 bytes) or DIJOYSTATE2 (272) at rest: buttons up, POV hats centred
/// (0xFFFFFFFF, not 0, which is "up"), the axes and sliders at `centres`, the rest 0.
fn at_rest(out: &mut [u8], centres: &[Option<i32>; 8]) {
    out.fill(0);
    for (i, c) in centres.iter().enumerate() {
        if let Some(c) = c {
            out[i * 4..i * 4 + 4].copy_from_slice(&c.to_le_bytes());
        }
    }
    for k in 0..4 {
        out[32 + k * 4..36 + k * 4].copy_from_slice(&u32::MAX.to_le_bytes());
    }
}

/// Packs `VirtualPad` into a `DIJOYSTATE` (80B) or `DIJOYSTATE2` (272B) buffer.
pub(crate) fn pack_dinput_pad(out: &mut [u8], pad: &VirtualPad, ranges: &[Option<(i32, i32)>; 8]) {
    let c = centres_of(ranges);
    at_rest(out, &c);
    let default_range = (0i32, 65535i32);
    let axis = |idx: usize, norm: f64| {
        let r = ranges[idx].unwrap_or(default_range);
        map_axis(norm, r)
    };
    let lx = f64::from(pad.thumb_lx) / 32767.0;
    let ly = -f64::from(pad.thumb_ly) / 32767.0;
    let rx = f64::from(pad.thumb_rx) / 32767.0;
    let ry = -f64::from(pad.thumb_ry) / 32767.0;
    let lz = (f64::from(pad.left_trigger) - f64::from(pad.right_trigger)) / 255.0;
    let rz = (f64::from(pad.right_trigger) / 255.0) * 2.0 - 1.0;

    let vals = [axis(0, lx), axis(1, ly), axis(2, lz), axis(3, rx), axis(4, ry), axis(5, rz)];
    for (i, v) in vals.into_iter().enumerate() {
        if ranges[i].is_some() || i < 2 {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
    }

    // D-pad bits (0x0001 UP, 0x0002 DOWN, 0x0004 LEFT, 0x0008 RIGHT) -> POV[0] hundredths of degree.
    let b = pad.xinput_buttons();
    let pov: u32 = match (b & 0x0001 != 0, b & 0x0002 != 0, b & 0x0004 != 0, b & 0x0008 != 0) {
        (true, false, false, false) => 0,
        (true, false, false, true) => 4500,
        (false, false, false, true) => 9000,
        (false, true, false, true) => 13500,
        (false, true, false, false) => 18000,
        (false, true, true, false) => 22500,
        (false, false, true, false) => 27000,
        (true, false, true, false) => 31500,
        _ => u32::MAX,
    };
    out[32..36].copy_from_slice(&pov.to_le_bytes());

    // XInput face/shoulder/menu/thumb buttons -> DirectInput rgbButtons[0..11] (at offset 48).
    const BTN_BITS: [u16; 11] = [
        0x1000, // A -> 0
        0x2000, // B -> 1
        0x4000, // X -> 2
        0x8000, // Y -> 3
        0x0100, // LB -> 4
        0x0200, // RB -> 5
        0x0020, // Back -> 6
        0x0010, // Start -> 7
        0x0040, // LThumb -> 8
        0x0080, // RThumb -> 9
        0x0400, // Guide -> 10
    ];
    for (idx, mask) in BTN_BITS.into_iter().enumerate() {
        out[48 + idx] = if b & mask != 0 { 0x80 } else { 0x00 };
    }
}

fn pack_dinput_keyboard(out: &mut [u8], kb: &peerparty_protocol::input_shm::VirtualKeyboard) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
    for vk in 1u8..=254 {
        if kb.is_down(vk) {
            let sc = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) } as usize;
            if sc > 0 && sc < out.len() {
                out[sc] = 0x80;
            }
        }
    }
}

fn dev_data_detours() -> [DevGetData; 2] {
    per_slot!(DevGetData, [0, 1], dev_data, (this: usize, size: u32, data: *mut u8, count: *mut u32, flags: u32) -> i32)
}

fn dev_data(i: usize, this: usize, size: u32, data: *mut u8, count: *mut u32, flags: u32) -> i32 {
    let Some(h) = DEV_DATA.hooks[i].get() else { return 0 };
    let rc = unsafe { h.call(this, size, data, count, flags) };
    let pass = match policy() {
        Policy::PassThrough => true,
        Policy::BlockAll => false,
        // The copy's own controller's buffered events are its own.
        Policy::Virtual(snap) => snap.pad.hid_key != 0 && device::is_own(dev_key(this), &snap),
    };
    if rc >= 0 && !pass && !count.is_null() {
        unsafe { *count = 0 };
    }
    rc
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::input_shm::{VirtualKeyboard, VirtualMouse};

    fn load(name: &str) {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
    }

    pub(super) static GAMEINPUT_TESTS: Mutex<()> = Mutex::new(());

    /// A copy without the keyboard and mouse gets their releases alone.
    #[test]
    fn a_copy_without_the_keyboard_still_sees_keys_let_go() {
        let key = |flags: u16| {
            let mut r = vec![0u8; RAW_HEADER + 16];
            r[RAW_HEADER + 2..RAW_HEADER + 4].copy_from_slice(&flags.to_le_bytes());
            r
        };
        assert!(releases_only(RIM_TYPEKEYBOARD, &mut key(RI_KEY_BREAK)), "a key up");
        assert!(!releases_only(RIM_TYPEKEYBOARD, &mut key(0)), "a key down is held back");
        assert!(!releases_only(RIM_TYPEKEYBOARD, &mut [0u8; 8]), "too short");
        let mouse = |flags: u16, buttons: u16, dx: i32| {
            let mut r = vec![0u8; RAW_HEADER + 24];
            let m = RAW_HEADER;
            r[m..m + 2].copy_from_slice(&flags.to_le_bytes());
            r[m + 4..m + 6].copy_from_slice(&buttons.to_le_bytes());
            r[m + 6..m + 8].copy_from_slice(&120u16.to_le_bytes());
            r[m + 12..m + 16].copy_from_slice(&dx.to_le_bytes());
            r
        };
        // Left up and right down, with motion: the left up alone, no motion.
        let mut r = mouse(0, 0x0002 | 0x0004, 40);
        assert!(releases_only(RIM_TYPEMOUSE, &mut r));
        let m = RAW_HEADER;
        assert_eq!(u16::from_le_bytes([r[m + 4], r[m + 5]]), 0x0002);
        assert_eq!(&r[m + 6..m + 20], &[0u8; 14], "no wheel data, buttons or motion");
        let mut r = mouse(0, 0x0001, 5);
        assert!(releases_only(RIM_TYPEMOUSE, &mut r), "a press goes as nothing");
        assert_eq!(&r[m + 4..m + 20], &[0u8; 16], "no press, no motion");
        let mut r = mouse(0, 0, 5);
        assert!(releases_only(RIM_TYPEMOUSE, &mut r), "motion alone goes as nothing");
        assert_eq!(&r[m + 12..m + 16], &[0u8; 4]);
        assert!(!releases_only(RIM_TYPEMOUSE, &mut mouse(MOUSE_MOVE_ABSOLUTE, 0x0002, 5)), "an absolute mouse would jump");
        assert!(!releases_only(RIM_TYPEHID, &mut [0u8; 64]));
    }

    #[test]
    fn dinput_joysticks_read_centred() {
        assert_eq!(middle(0, 65535), 32767);
        assert_eq!(middle(-1000, 1000), 0);
        assert_eq!(middle(i32::MIN, i32::MAX), 0);
        for size in [80usize, 272] {
            let mut s = vec![0xAAu8; size];
            at_rest(&mut s, &[Some(32767), Some(32767), None, Some(0), Some(-5), None, Some(127), None]);
            let int = |o: usize| i32::from_le_bytes(s[o..o + 4].try_into().unwrap());
            assert_eq!([int(0), int(4), int(8), int(12), int(16), int(20), int(24), int(28)], [32767, 32767, 0, 0, -5, 0, 127, 0]);
            assert!((0..4).all(|k| int(32 + k * 4) == -1), "hats centred");
            assert!(s[48..].iter().all(|b| *b == 0), "buttons up, the rest 0");
        }
    }

    #[test]
    fn xinput_and_dinput_pack_virtual_pad() {
        let pad = VirtualPad {
            connected: true,
            buttons: 0x1000 | 0x0001 | 0x0008, // A + D-pad Up-Right
            left_trigger: 200,
            right_trigger: 50,
            thumb_lx: 32767,
            thumb_ly: 32767,
            thumb_rx: -32767,
            thumb_ry: 0,
            packet_number: 42,
            ..Default::default()
        };
        let mut xi = [0u8; 16];
        pack_xinput_state(&pad, &mut xi);
        assert_eq!(u32::from_le_bytes(xi[0..4].try_into().unwrap()), 42);
        assert_eq!(u16::from_le_bytes(xi[4..6].try_into().unwrap()), 0x1009);
        assert_eq!((xi[6], xi[7]), (200, 50));
        assert_eq!(i16::from_le_bytes(xi[8..10].try_into().unwrap()), 32767);
        assert_eq!(i16::from_le_bytes(xi[12..14].try_into().unwrap()), -32767);

        let mut di = vec![0u8; 80];
        let ranges = [Some((0, 65535)), Some((0, 65535)), Some((0, 65535)), Some((-1000, 1000)), Some((-1000, 1000)), None, None, None];
        pack_dinput_pad(&mut di, &pad, &ranges);
        let int = |o: usize| i32::from_le_bytes(di[o..o + 4].try_into().unwrap());
        assert_eq!(int(0), 65535, "LX right");
        assert_eq!(int(4), 0, "LY up (inverted in DI)");
        assert_eq!(int(12), -1000, "RX left");
        assert_eq!(int(32), 4500, "NE POV hat");
        assert_eq!(di[48], 0x80, "A button");
        assert_eq!(di[49], 0x00, "B released");
    }

    /// A split-screen copy's first controller is the real slot its controller is, or
    /// the virtual pad; its other controllers aren't there.
    #[test]
    fn xinput_calls_go_to_the_copys_own_controller() {
        // TEST_POLICY is the whole process's: the other tests that set it take turns too.
        let _turn = crate::overlay::test_turn();
        let _reset = PolicyReset;
        let snap = |connected, xinput_slot| InputSnapshot {
            mode: InputMode::Virtual,
            pad: VirtualPad { connected, xinput_slot, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(route(0, &snap(true, Some(2))), Route::Real(2));
        assert_eq!(route(1, &snap(true, Some(2))), Route::Absent);
        assert_eq!(route(0, &snap(true, None)), Route::Virtual);
        assert_eq!(route(0, &snap(false, Some(2))), Route::Absent, "its controller is gone");
        let hid_snap = InputSnapshot {
            mode: InputMode::Virtual,
            pad: VirtualPad { connected: true, xinput_slot: None, hid_key: 0xbeef, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(route_with(0, &hid_snap, 0), Route::Virtual, "HID pad not read natively falls back to virtual XInput");
        assert_eq!(route_with(0, &hid_snap, 0xbeef), Route::Absent, "HID pad read natively yields in XInput");
        assert_eq!(route_with(0, &hid_snap, 0xcafe), Route::Virtual, "another HID key does not suppress this one");
        let mut caps = [0u8; 20];
        virtual_capabilities(&mut caps);
        assert_eq!((caps[0], caps[1], u16::from_le_bytes([caps[4], caps[5]])), (1, 1, 0xF3FF));
        // Vibration from a virtual pad shakes nobody's controller; a real slot gets it.
        load("xinput1_4.dll");
        hook_xinput("xinput1_4.dll");
        let m = module("xinput1_4.dll").unwrap();
        let set: SetState = unsafe { std::mem::transmute(proc_addr(m, c"XInputSetState".as_ptr().cast()).unwrap()) };
        let mut v = [0u8; 4];
        *TEST_POLICY.lock().unwrap() = Some(Policy::Virtual(snap(true, None)));
        let virtual_rc = unsafe { set(0, v.as_mut_ptr()) };
        let other_rc = unsafe { set(1, v.as_mut_ptr()) };
        *TEST_POLICY.lock().unwrap() = None;
        assert_eq!((virtual_rc, other_rc), (0, ERROR_DEVICE_NOT_CONNECTED));
        // A wrapper DLL (xinput1_3) forwarding a remapped slot (> 0) into hooked xinput1_4
        // must pass through instead of being rejected by `pad != 0`.
        load("xinput1_3.dll");
        hook_xinput("xinput1_3.dll");
        if let Some(m13) = module("xinput1_3.dll")
            && let Some(a13) = proc_addr(m13, c"XInputGetState".as_ptr().cast())
        {
            let get13: GetState = unsafe { std::mem::transmute(a13) };
            let get14: GetState = unsafe { std::mem::transmute(proc_addr(m, c"XInputGetState".as_ptr().cast()).unwrap()) };
            for slot in 0..4u32 {
                let mut direct = [0u8; 16];
                let expected = {
                    let _g = XInputCall::enter();
                    unsafe { get14(slot, direct.as_mut_ptr()) }
                };
                *TEST_POLICY.lock().unwrap() = Some(Policy::Virtual(snap(true, Some(slot as u8))));
                let mut routed = [0u8; 16];
                let got = unsafe { get13(0, routed.as_mut_ptr()) };
                *TEST_POLICY.lock().unwrap() = None;
                assert_eq!(got, expected, "xinput1_3 controller 0 -> real slot {slot}");
            }
        }
    }

    /// DirectInput's foreground access becomes background access in a split-screen
    /// copy; a keyboard or mouse can't stay exclusive there.
    #[test]
    fn dinput_asks_for_the_background() {
        assert_eq!(background_level(DISCL_EXCLUSIVE | DISCL_FOREGROUND, false), DISCL_EXCLUSIVE | DISCL_BACKGROUND, "a joystick keeps exclusive (force feedback needs it)");
        assert_eq!(background_level(DISCL_EXCLUSIVE | DISCL_FOREGROUND | DISCL_NOWINKEY, true), DISCL_NONEXCLUSIVE | DISCL_BACKGROUND);
        assert_eq!(background_level(DISCL_NONEXCLUSIVE | DISCL_FOREGROUND, true), DISCL_NONEXCLUSIVE | DISCL_BACKGROUND);
        assert_eq!(background_level(DISCL_NONEXCLUSIVE | DISCL_BACKGROUND, true), DISCL_NONEXCLUSIVE | DISCL_BACKGROUND, "background already");
    }

    #[test]
    fn virtual_kbm_async_key_state() {
        let mut kb = VirtualKeyboard { active: true, ..Default::default() };
        kb.set(0x57, true); // 'W'
        let snap = InputSnapshot {
            mode: InputMode::Virtual,
            keyboard: kb,
            mouse: VirtualMouse { active: true, buttons: MOUSE_LEFT | MOUSE_X2, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(virtual_async_key_state(0x57, &snap), i16::MIN);
        assert_eq!(virtual_async_key_state(0x41, &snap), 0);
        assert_eq!(virtual_async_key_state(0x01, &snap), i16::MIN);
        assert_eq!(virtual_async_key_state(0x02, &snap), 0);
        assert_eq!(virtual_async_key_state(0x06, &snap), i16::MIN);
    }

    #[test]
    fn gameinput_readings_hold_still() {
        let _turn = crate::overlay::test_turn();
        let _turn = GAMEINPUT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        load("GameInput.dll");
        let Some(m) = module("gameinput.dll") else { return };
        assert!(gameinput::install() >= 2, "every version's reads hooked");
        let create: unsafe extern "system" fn(*mut usize) -> i32 =
            unsafe { std::mem::transmute(proc_addr(m, c"GameInputCreate".as_ptr().cast()).unwrap()) };
        let mut gi = 0usize;
        assert!(unsafe { create(&mut gi) } >= 0);
        let vt = unsafe { *(gi as *const *const usize) };
        let current: unsafe extern "system" fn(usize, u32, usize, *mut usize) -> i32 = unsafe { std::mem::transmute(*vt.add(4)) };
        let next: unsafe extern "system" fn(usize, usize, u32, usize, *mut usize) -> i32 = unsafe { std::mem::transmute(*vt.add(5)) };
        crate::overlay::set_visible_for_test(true);
        let kind = 0;
        let (mut a, mut b, mut c) = (0usize, 0usize, 1usize);
        let ha = unsafe { current(gi, kind, 0, &mut a) };
        let hb = unsafe { current(gi, kind, 0, &mut b) };
        let hc = unsafe { next(gi, a, kind, 0, &mut c) };
        crate::overlay::set_visible_for_test(false);
        gameinput::hidden();
        eprintln!("GameInput: current {ha:#x} {hb:#x}, next {hc:#x}");
        if ha >= 0 {
            assert_eq!(a, b, "the same reading while the overlay shows");
            assert_eq!(c, 0);
        }
    }

    /// Puts the test policy back when a test ends, failed assertion or not.
    struct PolicyReset;
    impl Drop for PolicyReset {
        fn drop(&mut self) {
            *TEST_POLICY.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    #[test]
    fn xinput_reads_rest_while_the_overlay_shows() {
        let _turn = crate::overlay::test_turn();
        let _reset = PolicyReset;
        load("xinput1_4.dll");
        hook_xinput("xinput1_4.dll");
        let m = module("xinput1_4.dll").unwrap();
        let get: GetState = unsafe { std::mem::transmute(proc_addr(m, c"XInputGetState".as_ptr().cast()).unwrap()) };
        // Also test Virtual mode regardless of physical pad presence.
        let vpad = VirtualPad { connected: true, buttons: 0x2000, thumb_lx: 10000, packet_number: 9, ..Default::default() };
        *TEST_POLICY.lock().unwrap() = Some(Policy::Virtual(InputSnapshot {
            mode: InputMode::Virtual,
            pad: vpad,
            ..Default::default()
        }));
        let mut s = [0u8; 16];
        assert_eq!(unsafe { get(0, s.as_mut_ptr()) }, 0);
        assert_eq!(u32::from_le_bytes(s[0..4].try_into().unwrap()), 9);
        assert_eq!(u16::from_le_bytes(s[4..6].try_into().unwrap()), 0x2000);
        assert_eq!(unsafe { get(1, s.as_mut_ptr()) }, ERROR_DEVICE_NOT_CONNECTED, "slots 1..3 hidden");
        *TEST_POLICY.lock().unwrap() = None;

        let mut s = [0xAAu8; 16];
        if unsafe { get(0, s.as_mut_ptr()) } != 0 {
            eprintln!("no XInput pad in slot 0: physical BlockAll check skipped");
            return;
        }
        crate::overlay::set_visible_for_test(true);
        let before = HELD_XINPUT.load(Ordering::Relaxed);
        let mut s = [0xAAu8; 16];
        let rc = unsafe { get(0, s.as_mut_ptr()) };
        crate::overlay::set_visible_for_test(false);
        assert_eq!(rc, 0);
        assert_eq!(&s[4..16], &[0u8; 12], "buttons, triggers and sticks at rest");
        assert!(HELD_XINPUT.load(Ordering::Relaxed) > before);
    }

    #[test]
    fn wgi_installs_with_a_pad() {
        load("Windows.Gaming.Input.dll");
        wgi::install();
        // Its hooks go in on another thread: up to 1.5 s for both, no longer than needed.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while wgi::hooked() < 2 && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        eprintln!("Windows.Gaming.Input: {} hooked", wgi::hooked());
    }
}
