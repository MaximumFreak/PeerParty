//! Virtual and blocked input layer for the game process.
//!
//! Replaces the older `padblock` module with a unified `Policy` queried by every input
//! API hook:
//! - `Policy::PassThrough`: zero overhead; calls go straight to Windows/driver.
//! - `Policy::BlockAll`: overlay is open (or copy has `InputAssignment::None`); physical
//!   controllers read at rest so the game doesn't react behind the overlay, and raw-input
//!   mouse reports are held (the launcher's mouse hook doesn't stop the relative motion),
//!   key presses too (a second barrier: Windows drops a low-level keyboard hook that is slow).
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
//!   `XInputGetStateEx`, `XInputGetKeystroke`, vibration, capabilities, battery, and the
//!   headset's sound devices (`XInputGetAudioDeviceIds`, `XInputGetDSoundAudioDeviceGuids`).
//! - DirectInput 8 (`dinput.rs`), winmm's joysticks (`joy.rs`).
//! - Raw Input + Win32 KBM: `GetRawInputData`, `GetRawInputBuffer` (32-bit games too),
//!   `GetRawInputDeviceList`, `GetRawInputDeviceInfo`, `GetAsyncKeyState`, `GetCursorPos`.
//! - Windows.Gaming.Input (`wgi.rs`), GameInput (`gameinput.rs`), direct HID (`hid.rs`).
//! - Steam Input (`steam/lan.rs` asks `input::steam_input_held`).
//!
//! Nothing here calls into an input DLL under the loader lock (the hook's start, a load
//! notification): what's hooked by export goes in then, and what needs the DLL's own
//! objects (GameInput's) waits for a thread.
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
mod cursor;
mod device;
mod dinput;
mod gameinput;
mod joy;

pub(crate) use device::{opened_a, opened_w, refuse_open_a, refuse_open_w};
pub(crate) use audio::unfiltered as audio_unfiltered;
pub(crate) use cursor::{begin_synth_setcursor, end_synth_setcursor, observe_thread_cursor, on_forwarded_mouse_move};
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
use crate::overlay::{self, pads_held_back};
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
pub(super) static USED_WINMM: AtomicBool = AtomicBool::new(false);
pub(super) static USED_WGI_OTHER: AtomicBool = AtomicBool::new(false);

/// Into the log the first time a split-screen copy's game reads controllers `what` way.
pub(super) fn first_use(flag: &AtomicBool, what: &str) {
    first_use_with(flag, || what.to_string());
}

/// `first_use` with the words made only that first time (not on every call).
pub(super) fn first_use_with(flag: &AtomicBool, what: impl FnOnce() -> String) {
    if !flag.load(Ordering::Relaxed) && !flag.swap(true, Ordering::Relaxed) {
        log!("input: the game reads {}", what());
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

/// `policy()` for controllers: a copy of a split-screen session of several, before its
/// input block is there (the launcher sends it once the copy connects), has none yet.
/// Read through, the real XInput would show it every controller on the PC: a game that
/// looks once at start-up (SDL does) keeps the one it found, then can't open it once the
/// block says it isn't this copy's, and never looks again.
fn pad_policy() -> Policy {
    if shm::attached() || !crate::window::focus_spoofed() {
        return policy();
    }
    static FIRST: OnceLock<std::time::Instant> = OnceLock::new();
    let waited = FIRST.get_or_init(std::time::Instant::now).elapsed();
    if waited >= BLOCK_WAIT {
        static TOLD: AtomicBool = AtomicBool::new(false);
        if !TOLD.swap(true, Ordering::Relaxed) {
            log!("input: this split-screen copy's input block didn't come within {} s: it sees the PC's controllers as they are", BLOCK_WAIT.as_secs());
        }
        return policy();
    }
    static TOLD: AtomicBool = AtomicBool::new(false);
    if !TOLD.swap(true, Ordering::Relaxed) {
        log!("input: the game asks for controllers before this copy's input block came: it has none until then (not the PC's every controller)");
    }
    Policy::Virtual(InputSnapshot { mode: InputMode::Virtual, ..Default::default() })
}

/// How long a split-screen copy waits for its input block before seeing the real
/// controllers.
const BLOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

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
    let moved = SEAT_SLOT.load(Ordering::Relaxed) != slot;
    if moved {
        SEAT_SLOT.store(slot, Ordering::Relaxed);
    }
    if moved || SEAT_KEY.load(Ordering::Relaxed) != pad.hid_key || (pad.connected && SEAT_SEEN.load(Ordering::Relaxed) != 2) {
        SEAT_KEY.store(pad.hid_key, Ordering::Relaxed);
        SEAT_AT_MS.store(since_start_ms(), Ordering::Relaxed);
        // The press that gave the seat its controller (a controller takes a seat when
        // it's first used), for the game's first reads of it (`replay_opening`).
        let opening = if pad.connected { pad.buttons & 0xFFFF } else { 0 };
        OPENING_FIRST_MS.store(0, Ordering::Relaxed);
        OPENING_READS.store(0, Ordering::Relaxed);
        OPENING.store(opening, Ordering::Relaxed);
    }
    let seen = if pad.connected { 2 } else { 1 };
    if SEAT_SEEN.load(Ordering::Relaxed) != seen {
        SEAT_SEEN.store(seen, Ordering::Relaxed);
    }
}

/// The buttons (`XINPUT_GAMEPAD`'s) of the press that gave the seat its controller, until
/// the game has read them (`replay_opening`); 0 for none.
static OPENING: AtomicU32 = AtomicU32::new(0);
/// When the game first read them (`since_start_ms`; 0 not yet), and how many times.
static OPENING_FIRST_MS: AtomicU64 = AtomicU64::new(0);
static OPENING_READS: AtomicU32 = AtomicU32::new(0);
/// How long after the seat is taken the press is still given to a game that hasn't read
/// the controller yet.
const OPENING_WITHIN_MS: u64 = 2000;
/// How long, and for how many reads at least, the game reads the press down: a few
/// frames, as a quick tap lasts.
const OPENING_HOLD_MS: u64 = 80;
const OPENING_HOLD_READS: u32 = 3;

/// The press that took the seat, given to the game in its first reads of its new
/// controller. A game polls a controller that isn't there now and then only (some input
/// libraries check an absent slot every few hundred ms): it first read its new one after
/// the press that took the seat was over, and that press never reached it. Its buttons
/// are held down in `out` (an `XINPUT_STATE`) for a few frames from the game's first
/// read, with the packet number moved so a game that skips unchanged packets sees both
/// the press and its release.
fn replay_opening(out: *mut u8) {
    let buttons = OPENING.load(Ordering::Relaxed);
    if buttons == 0 || out.is_null() {
        return;
    }
    let now = since_start_ms();
    let at = SEAT_AT_MS.load(Ordering::Relaxed);
    if now.saturating_sub(at) > OPENING_WITHIN_MS && OPENING_FIRST_MS.load(Ordering::Relaxed) == 0 {
        OPENING.store(0, Ordering::Relaxed);
        log!("input: the press that took this copy's seat ({buttons:#06x}) wasn't given to the game: it didn't read its controller within {} s", OPENING_WITHIN_MS / 1000);
        return;
    }
    let first = match OPENING_FIRST_MS.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => {
            log!("input: the game first read its new controller {} ms after the press that took the seat ({buttons:#06x}): that press is given to it now, held for {OPENING_HOLD_MS} ms", now.saturating_sub(at));
            now
        }
        Err(f) => f,
    };
    // XINPUT_STATE: dwPacketNumber, then XINPUT_GAMEPAD (wButtons first).
    unsafe {
        let packet = std::ptr::read_unaligned(out as *const u32);
        std::ptr::write_unaligned(out as *mut u32, packet ^ 0x8000_0000);
        let w = std::ptr::read_unaligned(out.add(4) as *const u16);
        std::ptr::write_unaligned(out.add(4) as *mut u16, w | buttons as u16);
    }
    let reads = OPENING_READS.fetch_add(1, Ordering::Relaxed) + 1;
    if reads >= OPENING_HOLD_READS && now.saturating_sub(first) >= OPENING_HOLD_MS {
        OPENING.store(0, Ordering::Relaxed);
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

/// `seat()`, or for a split-screen copy that has never been told one (the overlay had
/// the controllers from the start, or the copy plays with none) a seat with no
/// controller: it owns none of them. Taken as no seat at all, every other player's
/// controller, its rumble and its sound device reached the copy.
pub(super) fn seat_or_nothing() -> Option<InputSnapshot> {
    seat().or_else(|| {
        (shm::attached() || crate::window::in_pane()).then(|| {
            crate::log::throttled("input: no seat yet", 1, |_| {
                log!("input: the controllers are held before this copy has a seat: it owns none of them until it has one");
            });
            InputSnapshot { mode: InputMode::Virtual, ..Default::default() }
        })
    })
}

/// Which controllers the game sees: `policy()`, but a split-screen copy's seat while
/// the overlay has the controllers (`seat_or_nothing`), at rest.
pub(super) fn topology() -> Policy {
    match pad_policy() {
        Policy::BlockAll => seat_or_nothing().map_or(Policy::BlockAll, Policy::Virtual),
        p => p,
    }
}

/// A value that changes whenever the copy's seat does (another controller, or its slot):
/// what was chosen for one seat is chosen again for the next.
pub(super) fn seat_stamp() -> u64 {
    SEAT_AT_MS.load(Ordering::Relaxed) ^ SEAT_KEY.load(Ordering::Relaxed).rotate_left(17) ^ (u64::from(SEAT_SLOT.load(Ordering::Relaxed)) << 56)
}

/// Whether Steam Input's actions read for controller `handle` are held at rest:
/// everything while the overlay has the controllers; in a split-screen copy, every
/// controller but its own. Steam Input numbers its controllers itself; `xinput_index`
/// asks it which XInput slot one is (`GetGamepadIndexForController`), which places the
/// copy's XInput controller. A copy whose controller isn't an XInput one has nothing to
/// place it by, and its Steam Input actions are held (logged once, as a gap).
pub fn steam_input_held(handle: u64, xinput_index: impl FnOnce(u64) -> Option<i32>) -> bool {
    match policy() {
        Policy::PassThrough => false,
        Policy::BlockAll => true,
        Policy::Virtual(snap) => {
            first_use(&USED_STEAM_INPUT, "Steam Input actions (its own controller's only)");
            let Some(slot) = snap.pad.xinput_slot.filter(|_| snap.pad.connected) else {
                if snap.pad.connected {
                    crate::log::gap("input", "Steam Input can't place this copy's controller (not an XInput one): its actions read at rest");
                }
                return true;
            };
            let own = xinput_index(handle) == Some(i32::from(slot));
            note_steam_handle(handle, own);
            !own
        }
    }
}

static USED_STEAM_INPUT: AtomicBool = AtomicBool::new(false);

/// Into the log when a Steam Input controller starts or stops being the copy's own.
fn note_steam_handle(handle: u64, own: bool) {
    static SEEN: Mutex<Vec<(u64, bool)>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    match seen.iter_mut().find(|(h, _)| *h == handle) {
        Some((_, was)) if *was == own => return,
        Some((_, was)) => *was = own,
        None => {
            if seen.len() >= 16 {
                seen.remove(0);
            }
            seen.push((handle, own));
        }
    }
    drop(seen);
    log!("input: Steam Input controller {handle:#x} is {}", if own { "this copy's own: its actions read as they are" } else { "another player's: its actions read at rest" });
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
type GetRawInputDeviceInfo = unsafe extern "system" fn(isize, u32, *mut u8, *mut u32) -> u32;
type GetAudioDeviceIds = unsafe extern "system" fn(u32, *mut u16, *mut u32, *mut u16, *mut u32) -> u32;
type GetDSoundGuids = unsafe extern "system" fn(u32, *mut [u8; 16], *mut [u8; 16]) -> u32;
type GetAsyncKeyStateFn = unsafe extern "system" fn(i32) -> i16;
type GetKeyStateFn = unsafe extern "system" fn(i32) -> i16;
type GetKeyboardStateFn = unsafe extern "system" fn(*mut u8) -> i32;
type GetCursorPosFn = unsafe extern "system" fn(*mut POINT) -> i32;

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
static AUDIO_IDS: Slots<GetAudioDeviceIds, 4> = Slots::new();
static DSOUND_GUIDS: Slots<GetDSoundGuids, 4> = Slots::new();
static RAW_INFO_W: OnceLock<GenericDetour<GetRawInputDeviceInfo>> = OnceLock::new();
static RAW_INFO_A: OnceLock<GenericDetour<GetRawInputDeviceInfo>> = OnceLock::new();
static RAW: OnceLock<GenericDetour<GetRawInputData>> = OnceLock::new();
static RAW_BUFFER: OnceLock<GenericDetour<GetRawInputBuffer>> = OnceLock::new();
static RAW_LIST: OnceLock<GenericDetour<GetRawInputDeviceList>> = OnceLock::new();
static ASYNC_KEY: OnceLock<GenericDetour<GetAsyncKeyStateFn>> = OnceLock::new();
static KEY_STATE: OnceLock<GenericDetour<GetKeyStateFn>> = OnceLock::new();
static KEYBOARD_STATE: OnceLock<GenericDetour<GetKeyboardStateFn>> = OnceLock::new();
static CURSOR_POS: OnceLock<GenericDetour<GetCursorPosFn>> = OnceLock::new();

static INSTALL: Once = Once::new();
/// Reads held back while the overlay showed: XInput states, raw-input reports.
static HELD_XINPUT: AtomicU32 = AtomicU32::new(0);
static HELD_RAW: AtomicU32 = AtomicU32::new(0);
/// Raw-input keyboard and mouse reports held while the overlay showed in a copy not in
/// split screen (`keep_raw`): taken out whole or passed on as their releases alone.
/// Counted apart from each other and from `HELD_RAW`, which has the other reports held
/// back (controllers').
static HELD_RAW_KEYS: AtomicU32 = AtomicU32::new(0);
static HELD_RAW_MOUSE: AtomicU32 = AtomicU32::new(0);
/// Keyboard and mouse input that still reached the game while the overlay showed (a copy
/// not in split screen lets the Win32 paths through under `BlockAll`), logged and reset by
/// `hidden`: raw-input keyboard and mouse reports, key-state answers of a key that went
/// up and down again, cursor positions that moved. GameInput's are in `gameinput`.
static REACHED_RAW_KEYS: AtomicU32 = AtomicU32::new(0);
static REACHED_RAW_MOUSE: AtomicU32 = AtomicU32::new(0);
static REACHED_KEYS: AtomicU32 = AtomicU32::new(0);
static REACHED_CURSOR: AtomicU32 = AtomicU32::new(0);
/// Per key-state path (`GetAsyncKeyState`, `GetKeyState`, `GetKeyboardState`), the keys
/// answered up during this opening: a key held from before it opened (the hotkey's Shift)
/// isn't input until it has been up.
static KEY_SEEN_UP: [[AtomicBool; 256]; 3] = [const { [const { AtomicBool::new(false) }; 256] }; 3];
/// The last cursor position answered during this opening, packed like `LAST_CURSOR`.
static REACHED_CURSOR_AT: AtomicU64 = AtomicU64::new(u64::MAX);
/// Per-30 s split-screen diagnostics (`log_summary`).
static XINPUT_CALLS: AtomicU32 = AtomicU32::new(0);
static XINPUT_OK: AtomicU32 = AtomicU32::new(0);
static XINPUT_ERR: AtomicU32 = AtomicU32::new(0);
static XINPUT_ACTIVE: AtomicU32 = AtomicU32::new(0);
static XINPUT_ABSENT: AtomicU32 = AtomicU32::new(0);
static XINPUT_REENTRANT: AtomicU32 = AtomicU32::new(0);
static RAW_HID_KEPT: AtomicU32 = AtomicU32::new(0);
static RAW_HID_DROPPED: AtomicU32 = AtomicU32::new(0);
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
/// block is attached, which for a split-screen copy is the hook's own start, under the
/// loader lock).
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
        // By export: their objects come to the hooks as the game makes them.
        n += dinput::module_loaded();
        n += joy::module_loaded();
        for dll in ["gameinput.dll", "gameinputredist.dll"] {
            if module(dll).is_some() {
                gameinput::module_loaded(dll);
            }
        }
        n += hid::install();
        INSTALLED.store(true, Ordering::Relaxed);
        log!("input: {n} input entry points hooked{}", if crate::watch::in_loader() { " (at start: GameInput's object methods and Windows.Gaming.Input's on a thread once the loader lock is free)" } else { "" });
        if shm::attached() {
            // A split-screen copy's CreateFile keeps other players' controllers from it
            // (device.rs `refuse_open`): the file hooks go in even when it has no saves
            // of its own. Off this thread: it may hold the loader lock.
            let _ = std::thread::Builder::new().name("peerparty-input".into()).spawn(crate::saves::arm_now);
        }
    });
    // Windows.Gaming.Input's methods are found through a connected pad, so this is
    // tried at every opening until it works; GameInput's through objects of its own,
    // which mustn't be made under the loader lock (its start-up waits on threads).
    if crate::watch::in_loader() {
        let _ = std::thread::Builder::new().name("peerparty-input".into()).spawn(install_objects);
    } else {
        install_objects();
    }
}

/// The hooks found through an input DLL's own objects.
fn install_objects() {
    let n = wgi::install() + gameinput::install();
    if n > 0 {
        log!("input: {n} more input entry points hooked");
    }
}

/// Controllers came or went (`HostMsg::DevicesChanged`, the session's seat changes too):
/// what was learnt of devices is asked again, and Windows.Gaming.Input hooked if it had
/// no controller to hook through before.
pub fn devices_changed() {
    if skipped() || !INSTALLED.load(Ordering::Relaxed) {
        return;
    }
    device::forget_devices();
    hid::devices_changed();
    dinput::devices_changed();
    audio::devices_changed();
    wgi::devices_changed();
    gameinput::devices_changed();
    if !crate::watch::in_loader() {
        install_objects();
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
    let (raw_kept, raw_drop, di) = (take(&RAW_HID_KEPT), take(&RAW_HID_DROPPED), take(&dinput::READS));
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
    if INSTALLED.load(Ordering::Relaxed) {
        dinput::hidden();
        wgi::hidden();
        gameinput::hidden();
        hid::hidden();
    }
    log!("{}", take_held());
    log!("{}", take_reached());
}

fn take(counter: &AtomicU32) -> u32 {
    counter.swap(0, Ordering::Relaxed)
}

/// The "held back" log line for the opening that ended, and a fresh start for the next.
fn take_held() -> String {
    let (x, r, rk, rm, d, h) = (take(&HELD_XINPUT), take(&HELD_RAW), take(&HELD_RAW_KEYS), take(&HELD_RAW_MOUSE), take(&dinput::HELD), hid::take_held());
    format!("input: held back while the overlay showed: {x} XInput reads, {r} raw-input reports, {rk} raw-input keyboard reports, {rm} raw-input mouse reports, {d} DirectInput reads, {h} HID reads")
}

/// The "reached the game" log line for the opening that ended, and a fresh start for the
/// next (the counts, and the keys' and the cursor's state of it).
fn take_reached() -> String {
    let [gk, gm, gp] = gameinput::take_reached();
    let (rk, rm, ks, cur) = (take(&REACHED_RAW_KEYS), take(&REACHED_RAW_MOUSE), take(&REACHED_KEYS), take(&REACHED_CURSOR));
    for path in &KEY_SEEN_UP {
        path.iter().for_each(|k| k.store(false, Ordering::Relaxed));
    }
    REACHED_CURSOR_AT.store(u64::MAX, Ordering::Relaxed);
    format!(
        "input: reached the game while the overlay showed: GameInput {gk} keyboard/{gm} mouse/{gp} controller readings, {rk} raw-input keyboard reports, {rm} raw-input mouse reports, {ks} key-state presses, {cur} cursor moves"
    )
}

/// A key-state path answered `vkey` as `down`: whether that's a press during this opening
/// (`KEY_SEEN_UP`; path 0 `GetAsyncKeyState`, 1 `GetKeyState`, 2 `GetKeyboardState`).
fn key_pressed(path: usize, vkey: usize, down: bool) -> bool {
    let Some(seen) = KEY_SEEN_UP[path].get(vkey) else { return false };
    if down {
        return seen.swap(false, Ordering::Relaxed);
    }
    seen.store(true, Ordering::Relaxed);
    false
}

/// A `GetAsyncKeyState` or `GetKeyState` answer that went to the game while the overlay showed.
fn note_key_answer(path: usize, vkey: i32, answer: i16) {
    if overlay::visible() && usize::try_from(vkey).is_ok_and(|k| key_pressed(path, k, answer < 0)) {
        REACHED_KEYS.fetch_add(1, Ordering::Relaxed);
    }
}

/// A `GetCursorPos` answer that went to the game while the overlay showed: a move when it
/// differs from the one before it in this opening.
fn note_cursor_answer(x: i32, y: i32) {
    if !overlay::visible() {
        return;
    }
    let now = u64::from(x as u32) | (u64::from(y as u32) << 32);
    let before = REACHED_CURSOR_AT.swap(now, Ordering::Relaxed);
    if before != u64::MAX && before != now {
        REACHED_CURSOR.fetch_add(1, Ordering::Relaxed);
    }
}

/// A raw-input keyboard or mouse report kept for the game while the overlay showed: a
/// press or a motion, not a key or button let go.
fn note_raw_reached(kind: u32, report: &[u8], header: usize) {
    if !overlay::visible() {
        return;
    }
    match kind {
        RIM_TYPEKEYBOARD if report.len() >= header + 4 && u16::from_le_bytes([report[header + 2], report[header + 3]]) & RI_KEY_BREAK == 0 => {
            REACHED_RAW_KEYS.fetch_add(1, Ordering::Relaxed);
        }
        RIM_TYPEMOUSE if raw_mouse_input(report, header) => {
            REACHED_RAW_MOUSE.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// Whether a raw mouse report (its header `header` bytes) carries motion, a wheel turn or a
/// button press, not only buttons let go. RAWMOUSE as `releases_only_at` has it:
/// usButtonFlags at 4, lLastX at 12, lLastY at 16.
fn raw_mouse_input(report: &[u8], header: usize) -> bool {
    if report.len() < header + 20 {
        return false;
    }
    let m = header;
    u16::from_le_bytes([report[m + 4], report[m + 5]]) & !MOUSE_UPS != 0
        || i32::from_le_bytes(report[m + 12..m + 16].try_into().unwrap()) != 0
        || i32::from_le_bytes(report[m + 16..m + 20].try_into().unwrap()) != 0
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
        // By its export alone: calling into it (DirectInput8Create) can't be done from
        // a load notification, where the loader lock is held and dinput8's own start-up
        // hasn't run.
        dinput::module_loaded()
    } else if lower == "winmm.dll" {
        joy::module_loaded()
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
    if let Some(a) = proc_addr(m, c"XInputGetAudioDeviceIds".as_ptr().cast()) {
        n += arm(&AUDIO_IDS, a, per_slot!(GetAudioDeviceIds, [0, 1, 2, 3], audio_ids, (pad: u32, render: *mut u16, render_n: *mut u32, capture: *mut u16, capture_n: *mut u32) -> u32), "XInputGetAudioDeviceIds");
    }
    if let Some(a) = proc_addr(m, c"XInputGetDSoundAudioDeviceGuids".as_ptr().cast()) {
        n += arm(&DSOUND_GUIDS, a, per_slot!(GetDSoundGuids, [0, 1, 2, 3], dsound_guids, (pad: u32, render: *mut [u8; 16], capture: *mut [u8; 16]) -> u32), "XInputGetDSoundAudioDeviceGuids");
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
    match pad_policy() {
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
                        replay_opening(out);
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
                    first_use_with(&USED_XINPUT, || format!("XInput (hook slot {i}): its controller 0 is the virtual pad, the others aren't there"));
                    if !out.is_null() {
                        let mut buf = [0u8; 16];
                        pack_xinput_state(&snap.pad, &mut buf);
                        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), out, 16) };
                        replay_opening(out);
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

/// Shakes XInput slot `slot`'s motors (`XINPUT_VIBRATION`), through the detour's own way
/// to the real call.
pub(super) fn real_xinput_vibrate(slot: u32, left: u16, right: u16) {
    let Some(h) = SET_STATES.hooks.iter().find_map(|h| h.get()) else { return };
    let mut v = [0u8; 4];
    v[0..2].copy_from_slice(&left.to_le_bytes());
    v[2..4].copy_from_slice(&right.to_le_bytes());
    let _g = XInputCall::enter();
    unsafe { h.call(slot, v.as_mut_ptr()) };
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

/// A controller's headset (its sound devices): a split-screen copy's goes to its own
/// controller's, a virtual pad has none, the others' aren't there.
fn audio_ids(i: usize, pad: u32, render: *mut u16, render_n: *mut u32, capture: *mut u16, capture_n: *mut u32) -> u32 {
    let Some(h) = AUDIO_IDS.hooks[i].get() else { return ERROR_DEVICE_NOT_CONNECTED };
    if in_xinput() {
        return unsafe { h.call(pad, render, render_n, capture, capture_n) };
    }
    match topology() {
        Policy::Virtual(snap) => match route(pad, &snap) {
            Route::Real(k) => {
                let _g = XInputCall::enter();
                unsafe { h.call(k, render, render_n, capture, capture_n) }
            }
            Route::Virtual => {
                // No headset: empty ids, counts 0.
                for (s, n) in [(render, render_n), (capture, capture_n)] {
                    if !s.is_null() && !n.is_null() && unsafe { *n } > 0 {
                        unsafe { *s = 0 };
                    }
                    if !n.is_null() {
                        unsafe { *n = 0 };
                    }
                }
                0
            }
            Route::Absent => ERROR_DEVICE_NOT_CONNECTED,
        },
        _ => {
            let _g = XInputCall::enter();
            unsafe { h.call(pad, render, render_n, capture, capture_n) }
        }
    }
}

/// `XInputGetDSoundAudioDeviceGuids` (the older XInputs'): as `audio_ids`, GUID_NULL for
/// none.
fn dsound_guids(i: usize, pad: u32, render: *mut [u8; 16], capture: *mut [u8; 16]) -> u32 {
    let Some(h) = DSOUND_GUIDS.hooks[i].get() else { return ERROR_DEVICE_NOT_CONNECTED };
    if in_xinput() {
        return unsafe { h.call(pad, render, capture) };
    }
    match topology() {
        Policy::Virtual(snap) => match route(pad, &snap) {
            Route::Real(k) => {
                let _g = XInputCall::enter();
                unsafe { h.call(k, render, capture) }
            }
            Route::Virtual => {
                for g in [render, capture] {
                    if !g.is_null() {
                        unsafe { *g = [0; 16] };
                    }
                }
                0
            }
            Route::Absent => ERROR_DEVICE_NOT_CONNECTED,
        },
        _ => {
            let _g = XInputCall::enter();
            unsafe { h.call(pad, render, capture) }
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
    match pad_policy() {
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
    if let Some(a) = proc_addr(m, c"GetRawInputDeviceInfoW".as_ptr().cast()) {
        n += arm_once(&RAW_INFO_W, a, raw_device_info_w as GetRawInputDeviceInfo, "GetRawInputDeviceInfoW");
    }
    if let Some(a) = proc_addr(m, c"GetRawInputDeviceInfoA".as_ptr().cast()) {
        n += arm_once(&RAW_INFO_A, a, raw_device_info_a as GetRawInputDeviceInfo, "GetRawInputDeviceInfoA");
    }
    n
}

/// The hook of `GetRawInputDeviceInfoW` (its `call` is the real one), once hooked:
/// device.rs asks devices' paths, which the hook itself would ask again.
pub(super) fn real_raw_device_info_w() -> Option<&'static GenericDetour<GetRawInputDeviceInfo>> {
    RAW_INFO_W.get()
}

/// Raw Input's device information: another player's game controller isn't there for a
/// split-screen copy (a handle kept from before the list left it out: not found).
fn raw_device_info(real: &GenericDetour<GetRawInputDeviceInfo>, dev: isize, cmd: u32, data: *mut u8, size: *mut u32) -> u32 {
    if let Policy::Virtual(snap) = topology()
        && let Some(d) = device::of_raw(dev)
        && device::is_others(&d, &snap)
    {
        unsafe { windows_sys::Win32::Foundation::SetLastError(windows_sys::Win32::Foundation::ERROR_INVALID_HANDLE) };
        return u32::MAX;
    }
    unsafe { real.call(dev, cmd, data, size) }
}

unsafe extern "system" fn raw_device_info_w(dev: isize, cmd: u32, data: *mut u8, size: *mut u32) -> u32 {
    RAW_INFO_W.get().map_or(u32::MAX, |h| raw_device_info(h, dev, cmd, data, size))
}

unsafe extern "system" fn raw_device_info_a(dev: isize, cmd: u32, data: *mut u8, size: *mut u32) -> u32 {
    RAW_INFO_A.get().map_or(u32::MAX, |h| raw_device_info(h, dev, cmd, data, size))
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
    n + cursor::hook()
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
    // `rc`: the bytes written to `data` (the process's own layout, whatever Windows'
    // is: `GetRawInputData` converts it).
    let report = unsafe { std::slice::from_raw_parts_mut(data, rc as usize) };
    if keep_raw(report, RAW_HEADER) { rc } else { u32::MAX }
}

/// Whether one raw input report (`RAWINPUT`, a `header`-byte header first) reaches the
/// game now, as it is or rewritten (`releases_only`). Only game controllers are held
/// back from a copy: another HID device (a headset's buttons, a touchpad) is no
/// player's. While the overlay shows, a mouse's and a keyboard's reports are held too, in
/// a copy not in split screen as well: the launcher's low-level mouse hook stops the clicks
/// but not the relative motion Windows hands to raw input, so the camera would still turn;
/// its keyboard hook takes the keys, but Windows silently removes a low-level hook that
/// exceeds its timeout, so this is a second barrier. Counts the reports it holds back,
/// whole or not (`HELD_RAW`, `HELD_RAW_KEYS`, `HELD_RAW_MOUSE`).
fn keep_raw(report: &mut [u8], header: usize) -> bool {
    if report.len() < header {
        return true;
    }
    let kind = u32::from_le_bytes([report[0], report[1], report[2], report[3]]);
    // RAWINPUTHEADER: type, size, then the device's handle (its low bytes first in a
    // 64-bit header too).
    let dev = || isize::from_le_bytes(report[8..8 + size_of::<isize>()].try_into().unwrap());
    let mut overlay_held = false;
    let block = match policy() {
        Policy::PassThrough => false,
        Policy::BlockAll => match kind {
            RIM_TYPEHID => device::of_raw(dev()).is_none_or(|d| d.controller),
            // Not `BlockAll` alone: a controller button held when the overlay closed keeps
            // it on, and the keyboard and mouse are back at once.
            RIM_TYPEKEYBOARD | RIM_TYPEMOUSE => {
                overlay_held = !shm::attached() && overlay::visible();
                shm::attached() || overlay_held
            }
            _ => false,
        },
        Policy::Virtual(snap) => match kind {
            RIM_TYPEHID => {
                first_use(&USED_RAW_HID, "Raw Input reports from HID controllers (only its own reach it)");
                match device::of_raw(dev()) {
                    Some(d) if device::is_own(&d, &snap) => {
                        note_own_hid_read(snap.pad.hid_key);
                        RAW_HID_KEPT.fetch_add(1, Ordering::Relaxed);
                        false
                    }
                    Some(d) if !d.controller => false,
                    _ => {
                        RAW_HID_DROPPED.fetch_add(1, Ordering::Relaxed);
                        true
                    }
                }
            }
            RIM_TYPEKEYBOARD => !snap.keyboard.active,
            RIM_TYPEMOUSE => !snap.mouse.active,
            _ => false,
        },
    };
    // A key or button let go still reaches a copy that doesn't have the keyboard and
    // mouse now (the focus moved to another pane, or its overlay opened while it was
    // held): it was pressed while the copy had them, and would stay down there for good.
    // The motion counts before the rewrite takes it out.
    let held_input = overlay_held && kind == RIM_TYPEMOUSE && raw_mouse_input(report, header);
    let kept = !block
        || ((shm::attached() || overlay_held) && matches!(kind, RIM_TYPEKEYBOARD | RIM_TYPEMOUSE) && releases_only_at(kind, report, header));
    if overlay_held && kept {
        // What reaches the game now: releases only, so this stays 0.
        note_raw_reached(kind, report, header);
    }
    // An absolute mouse report is held whole, whatever it carries.
    if !kept || held_input {
        let count = match (overlay_held, kind) {
            (false, _) => &HELD_RAW,
            (true, RIM_TYPEKEYBOARD) => &HELD_RAW_KEYS,
            (true, _) => &HELD_RAW_MOUSE,
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
    kept
}

/// Whether this is a 32-bit process on 64-bit Windows (WOW64, on x64 or Arm).
fn wow64() -> bool {
    static WOW: OnceLock<bool> = OnceLock::new();
    *WOW.get_or_init(|| {
        if size_of::<usize>() == 8 {
            return false;
        }
        let mut wow = 0i32;
        let ok = unsafe { windows_sys::Win32::System::Threading::IsWow64Process(windows_sys::Win32::System::Threading::GetCurrentProcess(), &mut wow) };
        ok != 0 && wow != 0
    })
}

/// `GetRawInputBuffer`'s reports' header size and alignment in this process: a 32-bit
/// game on 64-bit Windows gets 64-bit headers (24 bytes, the handle and WPARAM 8 bytes
/// each) on 8-byte boundaries, which the 32-bit headers' own sizes don't say.
fn batch_layout() -> (usize, usize) {
    if size_of::<usize>() == 8 || wow64() { (24, 8) } else { (16, 4) }
}

/// Raw input read in batches: the reports a copy shouldn't get (`keep_raw`) are taken
/// out and the rest moved up, each where the next block starts (aligned in memory, as
/// `NEXTRAWINPUTBLOCK` finds it).
unsafe extern "system" fn raw_input_buffer(data: *mut u8, size: *mut u32, header: u32) -> u32 {
    let Some(d) = RAW_BUFFER.get() else { return u32::MAX };
    let n = unsafe { d.call(data, size, header) };
    if data.is_null() || n == 0 || n == u32::MAX || matches!(policy(), Policy::PassThrough) {
        return n;
    }
    first_use_with(&USED_RAW_BUFFER, || format!("Raw Input in batches (GetRawInputBuffer, {}-byte headers)", batch_layout().0));
    // `size`: the buffer's size in bytes (the reports are inside it).
    let room = unsafe { size.as_ref() }.map_or(0, |s| *s as usize);
    unsafe { filter_batch(data, room, n, batch_layout(), keep_raw) }
}

/// Keeps the `n` reports in the `room` bytes at `data` that `keep` takes (`layout`: the
/// header's size and the blocks' alignment), moved up; how many are kept.
///
/// # Safety
/// `data` holds `room` bytes.
unsafe fn filter_batch(data: *mut u8, room: usize, n: u32, (header, align): (usize, usize), keep: impl Fn(&mut [u8], usize) -> bool) -> u32 {
    let base = data as usize;
    // The next block's offset: the end of this one, aligned as an address.
    let next = |o: usize| ((base + o + align - 1) & !(align - 1)) - base;
    let (mut read, mut write, mut kept) = (0usize, 0usize, 0u32);
    for _ in 0..n {
        if read + header > room {
            break;
        }
        let len = u32::from_le_bytes(unsafe { std::slice::from_raw_parts(data.add(read + 4), 4) }.try_into().unwrap()) as usize;
        if len < header || read + len > room {
            break;
        }
        let report = unsafe { std::slice::from_raw_parts_mut(data.add(read), len) };
        if keep(report, header) {
            if write != read {
                unsafe { std::ptr::copy(data.add(read), data.add(write), len) };
            }
            write = next(write + len);
            kept += 1;
        }
        read = next(read + len);
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

/// `releases_only_at` for a report of the process's own layout.
#[cfg(test)]
fn releases_only(kind: u32, report: &mut [u8]) -> bool {
    releases_only_at(kind, report, RAW_HEADER)
}

/// Pure helper: whether a raw keyboard or mouse report (its header `header` bytes) for a
/// copy without the keyboard and mouse goes through, rewritten to its releases alone: a
/// key up as it is; a mouse report as the buttons that went up (none, often), without its
/// motion, wheel or presses. False (the report is held back) for a key down or an
/// absolute mouse.
fn releases_only_at(kind: u32, report: &mut [u8], header: usize) -> bool {
    let u16_at = |r: &[u8], at: usize| u16::from_le_bytes([r[at], r[at + 1]]);
    match kind {
        // RAWKEYBOARD: MakeCode, Flags, Reserved, VKey, Message, ExtraInformation.
        RIM_TYPEKEYBOARD => report.len() >= header + 4 && u16_at(report, header + 2) & RI_KEY_BREAK != 0,
        // RAWMOUSE: usFlags, 2 bytes, usButtonFlags, usButtonData, ulRawButtons,
        // lLastX, lLastY, ulExtraInformation.
        RIM_TYPEMOUSE => {
            let m = header;
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

/// Keys seen down at the last `GetAsyncKeyState` and `GetKeyState` of a `Virtual` copy,
/// and each key's toggle as `GetKeyState` counts presses.
static ASYNC_DOWN: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static STATE_DOWN: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static TOGGLED: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

thread_local! {
    /// Inside `focus_proc` dispatching a synthetic `WM_ACTIVATE` / `WM_SETFOCUS`: report
    /// mouse buttons as up to `GetAsyncKeyState` / `GetKeyState` so toolkits (such as
    /// SDL2's `focus_click_pending`) never mistake a split-screen pane activation for a
    /// focus-only click and swallow its button release.
    static IN_FOCUS_GAIN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub(crate) struct FocusGainGuard;

pub(crate) fn suppress_focus_click_buttons() -> FocusGainGuard {
    IN_FOCUS_GAIN.with(|n| n.set(n.get().saturating_add(1)));
    FocusGainGuard
}

impl Drop for FocusGainGuard {
    fn drop(&mut self) {
        IN_FOCUS_GAIN.with(|n| n.set(n.get().saturating_sub(1)));
    }
}

fn suppress_mouse_button(vkey: i32) -> bool {
    matches!(vkey, 0x01 | 0x02 | 0x04 | 0x05 | 0x06) && IN_FOCUS_GAIN.with(|n| n.get() > 0)
}

/// Pure helper: `GetAsyncKeyState`'s low bit (pressed since the last call) from whether
/// the key is `down` now and was at the last call (`seen`, updated).
fn pressed_since(down: bool, seen: &AtomicBool) -> i16 {
    let was = seen.swap(down, Ordering::Relaxed);
    i16::from(down && !was)
}

/// Pure helper: `GetKeyState`'s low bit for a key a `Virtual` copy's keyboard has: it
/// flips at each press (`seen`: down at the last call; `toggled` updated).
fn toggle_on_press(down: bool, seen: &AtomicBool, toggled: &AtomicBool) -> i16 {
    if down && !seen.swap(down, Ordering::Relaxed) {
        toggled.fetch_xor(true, Ordering::Relaxed);
    } else if !down {
        seen.store(false, Ordering::Relaxed);
    }
    i16::from(toggled.load(Ordering::Relaxed))
}

unsafe extern "system" fn async_key_state(vkey: i32) -> i16 {
    if suppress_mouse_button(vkey) {
        return 0;
    }
    match policy() {
        Policy::PassThrough => ASYNC_KEY.get().map_or(0, |h| unsafe { h.call(vkey) }),
        Policy::BlockAll => {
            if shm::attached() {
                0
            } else {
                let answer = ASYNC_KEY.get().map_or(0, |h| unsafe { h.call(vkey) });
                note_key_answer(0, vkey, answer);
                answer
            }
        }
        Policy::Virtual(snap) => {
            let v = virtual_async_key_state(vkey, &snap);
            match usize::try_from(vkey).ok().and_then(|k| ASYNC_DOWN.get(k)) {
                Some(seen) => v | pressed_since(v < 0, seen),
                None => v,
            }
        }
    }
}

/// Caps Lock, Num Lock and Scroll Lock: their toggles are the keyboard's own, one for
/// every copy (their lights say so), so `GetKeyState`'s low bit is the real one.
fn toggle_of(vkey: i32) -> i16 {
    if !matches!(vkey, 0x14 | 0x90 | 0x91) {
        return 0;
    }
    KEY_STATE.get().map_or(0, |h| unsafe { h.call(vkey) }) & 1
}

unsafe extern "system" fn key_state(vkey: i32) -> i16 {
    if suppress_mouse_button(vkey) {
        return 0;
    }
    match policy() {
        Policy::PassThrough => KEY_STATE.get().map_or(0, |h| unsafe { h.call(vkey) }),
        Policy::BlockAll => {
            if shm::attached() {
                toggle_of(vkey)
            } else {
                let answer = KEY_STATE.get().map_or(0, |h| unsafe { h.call(vkey) });
                note_key_answer(1, vkey, answer);
                answer
            }
        }
        Policy::Virtual(snap) => {
            let v = virtual_async_key_state(vkey, &snap);
            // The lock keys' toggles are the keyboard's own (`toggle_of`); any other
            // key's flips at each of this copy's presses of it.
            let toggle = match usize::try_from(vkey).ok().filter(|_| !matches!(vkey, 0x14 | 0x90 | 0x91)) {
                Some(k) if k < 256 => toggle_on_press(v < 0, &STATE_DOWN[k], &TOGGLED[k]),
                _ => toggle_of(vkey),
            };
            v | toggle
        }
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
                for vk in [0x14, 0x90, 0x91] {
                    unsafe { *out.add(vk) = toggle_of(vk as i32) as u8 };
                }
                1
            } else {
                let rc = KEYBOARD_STATE.get().map_or(0, |h| unsafe { h.call(out) });
                if rc != 0 && overlay::visible() {
                    let buf = unsafe { std::slice::from_raw_parts(out, 256) };
                    // Every key's state is followed; the call counts once at most.
                    let pressed = buf.iter().enumerate().fold(false, |any, (vk, b)| key_pressed(2, vk, b & 0x80 != 0) | any);
                    if pressed {
                        REACHED_KEYS.fetch_add(1, Ordering::Relaxed);
                    }
                }
                rc
            }
        }
        Policy::Virtual(snap) => {
            let buf = unsafe { std::slice::from_raw_parts_mut(out, 256) };
            for (vk, slot) in buf.iter_mut().enumerate() {
                *slot = if virtual_async_key_state(vk as i32, &snap) < 0 { 0x80 } else { 0 } | toggle_of(vk as i32) as u8;
            }
            1
        }
    }
}

/// The last cursor position a split-screen copy was given while it had the mouse, packed
/// (x in the low half); u64::MAX for none yet.
static LAST_CURSOR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// The copy's cursor is at `(x, y)` now (it had the mouse, or its game put it there).
fn remember_cursor(x: i32, y: i32) {
    LAST_CURSOR.store(u64::from(x as u32) | (u64::from(y as u32) << 32), Ordering::Relaxed);
}
/// Where a copy that has never had the mouse sees the cursor: Windows' own "nowhere"
/// (where it puts minimized windows), off every screen.
const OFF_WINDOW: (i32, i32) = (-32000, -32000);

/// Calls the real (unhooked) Win32 `GetCursorPos` directly.
pub(super) fn real_cursor_pos(pt: *mut POINT) -> i32 {
    match CURSOR_POS.get() {
        Some(h) => unsafe { h.call(pt) },
        None => unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(pt) },
    }
}

unsafe extern "system" fn cursor_pos(pt: *mut POINT) -> i32 {
    // While a split-screen copy's overlay has its input, the real cursor (another
    // pane's, or over the overlay) isn't its: it stays where it last was.
    let p = match policy() {
        Policy::BlockAll if shm::attached() => Policy::Virtual(InputSnapshot { mode: InputMode::Virtual, ..Default::default() }),
        p => p,
    };
    let block_all = matches!(p, Policy::BlockAll);
    if let Policy::Virtual(snap) = p
        && !pt.is_null()
    {
        if snap.mouse.active {
            let (x, y) = cursor::now(&snap);
            unsafe {
                (*pt).x = x;
                (*pt).y = y;
            }
            remember_cursor(x, y);
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
    let rc = CURSOR_POS.get().map_or(0, |h| unsafe { h.call(pt) });
    if block_all && rc != 0 && !pt.is_null() {
        let at = unsafe { *pt };
        note_cursor_answer(at.x, at.y);
    }
    rc
}

pub(super) const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::input_shm::{VirtualKeyboard, VirtualMouse};

    /// The press that took the seat reaches the game's first reads of its controller,
    /// held a few reads and frames, then the controller reads as it is.
    #[test]
    fn the_press_that_took_the_seat_reaches_the_game() {
        note_seat(&VirtualPad { connected: false, ..Default::default() });
        note_seat(&VirtualPad { connected: true, xinput_slot: Some(2), buttons: 0x1000, ..Default::default() });
        // The game reads the controller once the press is over: buttons up, packet 7.
        let read = || {
            let mut s = [0u8; 16];
            s[..4].copy_from_slice(&7u32.to_le_bytes());
            replay_opening(s.as_mut_ptr());
            (u32::from_le_bytes(s[..4].try_into().unwrap()), u16::from_le_bytes([s[4], s[5]]))
        };
        let first = read();
        assert_eq!(first.1, 0x1000, "the press is given");
        assert_ne!(first.0, 7, "as a packet of its own");
        read();
        read();
        std::thread::sleep(std::time::Duration::from_millis(OPENING_HOLD_MS + 20));
        assert_eq!(read().1, 0x1000, "held for its time");
        assert_eq!(read(), (7, 0), "then the controller as it is");
    }

    fn load(name: &str) {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
    }

    pub(super) static GAMEINPUT_TESTS: Mutex<()> = Mutex::new(());

    /// `GetAsyncKeyState`'s low bit is a press since the last call; `GetKeyState`'s
    /// flips at each press.
    #[test]
    fn key_state_low_bits_follow_presses() {
        let seen = AtomicBool::new(false);
        assert_eq!((pressed_since(true, &seen), pressed_since(true, &seen), pressed_since(false, &seen), pressed_since(true, &seen)), (1, 0, 0, 1));
        let (seen, toggled) = (AtomicBool::new(false), AtomicBool::new(false));
        let t: Vec<i16> = [true, true, false, true, false].iter().map(|d| toggle_on_press(*d, &seen, &toggled)).collect();
        assert_eq!(t, [1, 1, 1, 0, 0]);
    }

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

    /// A 32-bit game on 64-bit Windows gets its batches with 64-bit headers on 8-byte
    /// boundaries; one dropped report moves the next up to where it would start.
    #[test]
    fn batches_keep_their_layout() {
        for (header, align) in [(24usize, 8usize), (16, 4)] {
            // Three keyboard reports of `header + 16 + 2` bytes (not a multiple of 8):
            // the middle one's a key down to drop.
            let len = header + 18;
            let step = (len + align - 1) & !(align - 1);
            let mut buf = vec![0u8; step * 3 + 64];
            // Starting on an aligned address, as Windows hands it.
            let at = buf.as_ptr().align_offset(8);
            for i in 0..3 {
                let r = at + i * step;
                buf[r..r + 4].copy_from_slice(&RIM_TYPEKEYBOARD.to_le_bytes());
                buf[r + 4..r + 8].copy_from_slice(&(len as u32).to_le_bytes());
                buf[r + header + 2] = if i == 1 { 0 } else { RI_KEY_BREAK as u8 };
                buf[r + header + 6] = 0x41 + i as u8;
            }
            let room = buf.len() - at;
            let kept = unsafe { filter_batch(buf.as_mut_ptr().add(at), room, 3, (header, align), |r, h| r[h + 2] & RI_KEY_BREAK as u8 != 0) };
            assert_eq!(kept, 2);
            assert_eq!((buf[at + header + 6], buf[at + step + header + 6]), (0x41, 0x43), "the third moved up to the second's place");
        }
    }

    #[test]
    fn xinput_packs_virtual_pad() {
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
        gameinput::install();
        assert!(gameinput::reads_hooked() >= 2, "every version's reads hooked");
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

    /// A built `RAWINPUT` report of `kind` (16-byte header) with `bytes` at `at` in its body.
    fn raw_report(kind: u32, at: usize, bytes: &[u8]) -> Vec<u8> {
        let header = 16;
        let mut r = vec![0u8; header + 24];
        r[..4].copy_from_slice(&kind.to_le_bytes());
        r[header + at..header + at + bytes.len()].copy_from_slice(bytes);
        r
    }

    /// Zeroes the held-raw counts and what reached the game, for a test of `keep_raw`.
    fn reset_raw_counts() {
        take_reached();
        for c in [&HELD_RAW, &HELD_RAW_KEYS, &HELD_RAW_MOUSE] {
            c.store(0, Ordering::Relaxed);
        }
    }

    /// Nothing raw reached the game: the overlay's hold took every press and motion out.
    fn assert_no_raw_reached() {
        let line = take_reached();
        assert!(line.contains(" 0 raw-input keyboard reports, 0 raw-input mouse reports"), "{line}");
    }

    /// `reports` read as one batch while the overlay shows: the buffer, where the reports
    /// start in it, their size and how many `filter_batch` kept.
    fn read_batch_while_shown(reports: &[Vec<u8>]) -> (Vec<u8>, usize, usize, u32) {
        let (header, align) = (16usize, 4usize);
        let len = reports[0].len();
        let mut buf = vec![0u8; len * reports.len() + 64];
        let at = buf.as_ptr().align_offset(8);
        for (i, r) in reports.iter().enumerate() {
            let o = at + i * len;
            buf[o..o + len].copy_from_slice(r);
            buf[o + 4..o + 8].copy_from_slice(&(len as u32).to_le_bytes());
        }
        let room = buf.len() - at;
        reset_raw_counts();
        crate::overlay::set_visible_for_test(true);
        let kept = unsafe { filter_batch(buf.as_mut_ptr().add(at), room, reports.len() as u32, (header, align), keep_raw) };
        crate::overlay::set_visible_for_test(false);
        (buf, at, len, kept)
    }

    /// While the overlay shows in a copy not in split screen, a raw-input key press (the
    /// prefixed ones too), mouse move, wheel turn or button press is held back and counted
    /// apart; only a release goes through (a mouse report as its releases alone), and an
    /// absolute mouse report is held whole. Nothing reaches the game. Hidden, or with only
    /// the controller hold on (`BlockAll` after the overlay closed on a held button), every
    /// report passes as it is and nothing counts.
    #[test]
    fn raw_keyboard_and_mouse_held_while_shown() {
        let _turn = crate::overlay::test_turn();
        let header = 16;
        let key = |flags: u16| raw_report(RIM_TYPEKEYBOARD, 2, &flags.to_le_bytes());
        let moved = raw_report(RIM_TYPEMOUSE, 12, &5i32.to_le_bytes());
        let wheel = raw_report(RIM_TYPEMOUSE, 4, &0x0400u16.to_le_bytes());
        let click = raw_report(RIM_TYPEMOUSE, 4, &1u16.to_le_bytes());
        let button_up = raw_report(RIM_TYPEMOUSE, 4, &MOUSE_UPS.to_le_bytes());
        let mut drag_end = raw_report(RIM_TYPEMOUSE, 4, &(MOUSE_UPS & 0x0002 | 1).to_le_bytes());
        drag_end[header + 12..header + 16].copy_from_slice(&7i32.to_le_bytes());
        let absolute = raw_report(RIM_TYPEMOUSE, 0, &MOUSE_MOVE_ABSOLUTE.to_le_bytes());
        // The report with its motion, wheel and presses taken out, `ups` left.
        let emptied = |r: &Vec<u8>, ups: u16| {
            let mut e = r.clone();
            e[header + 4..header + 20].fill(0);
            e[header + 4..header + 6].copy_from_slice(&ups.to_le_bytes());
            e
        };
        // (report, kept, what the game gets, [keyboard, mouse] counts); key flags: 1 break, 2 E0, 4 E1.
        let cases = [
            (key(0), false, key(0), [1, 0]),
            (key(RI_KEY_BREAK), true, key(RI_KEY_BREAK), [0, 0]),
            (key(2), false, key(2), [1, 0]),
            (key(2 | RI_KEY_BREAK), true, key(2 | RI_KEY_BREAK), [0, 0]),
            (key(4), false, key(4), [1, 0]),
            (key(4 | RI_KEY_BREAK), true, key(4 | RI_KEY_BREAK), [0, 0]),
            (moved.clone(), true, emptied(&moved, 0), [0, 1]),
            (wheel.clone(), true, emptied(&wheel, 0), [0, 1]),
            (click.clone(), true, emptied(&click, 0), [0, 1]),
            (button_up.clone(), true, button_up.clone(), [0, 0]),
            (drag_end.clone(), true, emptied(&drag_end, 0x0002), [0, 1]),
            (absolute.clone(), false, absolute.clone(), [0, 1]),
        ];
        reset_raw_counts();

        // Hidden: with the controller hold on too (the overlay closed with a button still down) as without.
        let _reset = PolicyReset;
        for policy in [None, Some(Policy::BlockAll)] {
            *TEST_POLICY.lock().unwrap() = policy;
            for (report, ..) in &cases {
                let mut c = report.clone();
                assert!(keep_raw(&mut c, header));
                assert_eq!(&c, report, "untouched");
            }
        }
        *TEST_POLICY.lock().unwrap() = None;
        assert_eq!((HELD_RAW_KEYS.load(Ordering::Relaxed), HELD_RAW_MOUSE.load(Ordering::Relaxed)), (0, 0), "hidden: nothing held");

        crate::overlay::set_visible_for_test(true);
        for (report, kept, out, held) in &cases {
            let before = [HELD_RAW_KEYS.load(Ordering::Relaxed), HELD_RAW_MOUSE.load(Ordering::Relaxed)];
            let mut c = report.clone();
            assert_eq!(keep_raw(&mut c, header), *kept, "{report:?}");
            assert_eq!(&c, out, "{report:?}");
            let now = [HELD_RAW_KEYS.load(Ordering::Relaxed), HELD_RAW_MOUSE.load(Ordering::Relaxed)];
            assert_eq!([now[0].wrapping_sub(before[0]), now[1].wrapping_sub(before[1])], *held, "{report:?}");
        }
        assert_eq!(HELD_RAW.load(Ordering::Relaxed), 0, "not as a controller's");
        crate::overlay::set_visible_for_test(false);
        assert_no_raw_reached();
    }

    /// A batch read with every kind in it: the key press, the mouse move and the HID report
    /// are held, the key release stays, each kind counted once, in its own count.
    #[test]
    fn raw_batch_of_every_kind_while_shown() {
        let _turn = crate::overlay::test_turn();
        let header = 16;
        let (buf, at, len, kept) = read_batch_while_shown(&[
            raw_report(RIM_TYPEKEYBOARD, 2, &0u16.to_le_bytes()),
            raw_report(RIM_TYPEKEYBOARD, 2, &RI_KEY_BREAK.to_le_bytes()),
            raw_report(RIM_TYPEMOUSE, 12, &5i32.to_le_bytes()),
            raw_report(RIM_TYPEHID, 0, &[0u8; 4]),
        ]);
        // The HID report is from an unknown device: held as a controller's.
        assert_eq!(kept, 2, "the key release and the emptied mouse report stay");
        assert_eq!(HELD_RAW_KEYS.load(Ordering::Relaxed), 1, "the key press");
        assert_eq!(HELD_RAW_MOUSE.load(Ordering::Relaxed), 1, "the mouse move");
        assert_eq!(HELD_RAW.load(Ordering::Relaxed), 1, "the HID report");
        let kind_at = |i: usize| u32::from_le_bytes(buf[at + i * len..at + i * len + 4].try_into().unwrap());
        assert_eq!(kind_at(0), RIM_TYPEKEYBOARD);
        assert_eq!(buf[at + header + 2], RI_KEY_BREAK as u8, "the release, moved up over the press");
        assert_eq!(kind_at(1), RIM_TYPEMOUSE);
        assert_eq!(buf[at + len + header + 4..at + 2 * len], [0u8; 20], "the move is held, left empty");
        assert_no_raw_reached();
    }

    /// The batch read (`GetRawInputBuffer`) holds the mouse as the single read does: what is
    /// kept moves up to where the next report starts, and every held report is counted.
    #[test]
    fn raw_mouse_batch_is_held_and_packed() {
        let _turn = crate::overlay::test_turn();
        let header = 16;
        let mut drag_end = raw_report(RIM_TYPEMOUSE, 4, &(0x0002u16 | 1).to_le_bytes());
        drag_end[header + 12..header + 16].copy_from_slice(&7i32.to_le_bytes());
        let mut moved = raw_report(RIM_TYPEMOUSE, 12, &5i32.to_le_bytes());
        moved[header + 4..header + 6].copy_from_slice(&1u16.to_le_bytes());
        let (buf, at, len, kept) = read_batch_while_shown(&[
            moved,
            drag_end,
            raw_report(RIM_TYPEMOUSE, 0, &MOUSE_MOVE_ABSOLUTE.to_le_bytes()),
            raw_report(RIM_TYPEMOUSE, 4, &MOUSE_UPS.to_le_bytes()),
            raw_report(RIM_TYPEKEYBOARD, 2, &0u16.to_le_bytes()),
        ]);
        assert_eq!(kept, 3, "the absolute report and the key press are taken out");
        assert_eq!(HELD_RAW_MOUSE.load(Ordering::Relaxed), 3, "the moved, the released-with-motion and the absolute report");
        assert_eq!(HELD_RAW.load(Ordering::Relaxed), 0);
        let body = |i: usize| &buf[at + i * len + header..at + (i + 1) * len];
        assert_eq!(body(0)[4..20], [0u8; 16], "the move with a press is held, left empty");
        assert_eq!(u16::from_le_bytes([body(1)[4], body(1)[5]]), 0x0002, "the release alone");
        assert_eq!(body(1)[12..20], [0u8; 8]);
        assert_eq!(u16::from_le_bytes([body(2)[4], body(2)[5]]), MOUSE_UPS, "the plain release, moved up over the absolute one");
        assert_no_raw_reached();
    }

    /// The hide line names the raw-input keyboard and mouse reports held apart from the
    /// other counts, and resets them.
    #[test]
    fn hide_reports_held_raw_keyboard_and_mouse() {
        let _turn = crate::overlay::test_turn();
        let _turn = GAMEINPUT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        HELD_XINPUT.store(1, Ordering::Relaxed);
        HELD_RAW.store(2, Ordering::Relaxed);
        HELD_RAW_KEYS.store(4, Ordering::Relaxed);
        HELD_RAW_MOUSE.store(3, Ordering::Relaxed);
        dinput::HELD.store(0, Ordering::Relaxed);
        hid::take_held();
        assert_eq!(
            take_held(),
            "input: held back while the overlay showed: 1 XInput reads, 2 raw-input reports, 4 raw-input keyboard reports, 3 raw-input mouse reports, 0 DirectInput reads, 0 HID reads"
        );
        assert!(take_held().contains("0 raw-input reports, 0 raw-input keyboard reports, 0 raw-input mouse reports, "), "reset");
    }

    /// A key held from before the opening never counts; one that went up and down counts
    /// once, however long it stays down; a keyboard state counts once at most.
    #[test]
    fn key_state_counts_a_press_during_the_opening() {
        let _turn = crate::overlay::test_turn();
        take_reached();
        note_key_answer(0, 0x41, i16::MIN);
        assert_eq!(REACHED_KEYS.load(Ordering::Relaxed), 0, "hidden: nothing");
        crate::overlay::set_visible_for_test(true);
        // The hotkey's Shift, down from before the opening.
        for _ in 0..3 {
            note_key_answer(0, 0x10, i16::MIN);
        }
        assert_eq!(REACHED_KEYS.load(Ordering::Relaxed), 0);
        note_key_answer(0, 0x41, 0);
        note_key_answer(0, 0x41, i16::MIN);
        note_key_answer(0, 0x41, i16::MIN);
        assert_eq!(REACHED_KEYS.load(Ordering::Relaxed), 1);
        // Another path follows its own answers.
        note_key_answer(1, 0x41, i16::MIN);
        assert_eq!(REACHED_KEYS.load(Ordering::Relaxed), 1);
        for vk in [0x42, 0x43] {
            assert!(!key_pressed(2, vk, false));
        }
        let pressed = [0x42, 0x43].iter().fold(false, |any, &vk| key_pressed(2, vk, true) | any);
        assert!(pressed);
        crate::overlay::set_visible_for_test(false);
        assert!(take_reached().contains(" 1 key-state presses"));
        // The next opening starts from nothing: the key is held again, not pressed.
        crate::overlay::set_visible_for_test(true);
        note_key_answer(0, 0x41, i16::MIN);
        crate::overlay::set_visible_for_test(false);
        assert!(take_reached().contains(" 0 key-state presses"));
    }

    /// A cursor answer counts when it differs from the previous one of this opening.
    #[test]
    fn cursor_counts_a_move_during_the_opening() {
        let _turn = crate::overlay::test_turn();
        take_reached();
        note_cursor_answer(1, 1);
        note_cursor_answer(2, 2);
        crate::overlay::set_visible_for_test(true);
        note_cursor_answer(10, 10);
        note_cursor_answer(10, 10);
        assert_eq!(REACHED_CURSOR.load(Ordering::Relaxed), 0, "the first answer is where it was");
        note_cursor_answer(11, 10);
        note_cursor_answer(11, 10);
        note_cursor_answer(10, 10);
        crate::overlay::set_visible_for_test(false);
        assert!(take_reached().contains(" 2 cursor moves"));
        assert!(take_reached().contains(" 0 cursor moves"), "reset");
    }

    /// The hide line carries every count, in order, and resets them.
    #[test]
    fn hide_reports_and_resets_what_reached_the_game() {
        let _turn = crate::overlay::test_turn();
        let _turn = GAMEINPUT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        take_reached();
        REACHED_RAW_KEYS.store(1, Ordering::Relaxed);
        REACHED_RAW_MOUSE.store(2, Ordering::Relaxed);
        REACHED_KEYS.store(3, Ordering::Relaxed);
        REACHED_CURSOR.store(4, Ordering::Relaxed);
        assert_eq!(
            take_reached(),
            "input: reached the game while the overlay showed: GameInput 0 keyboard/0 mouse/0 controller readings, 1 raw-input keyboard reports, 2 raw-input mouse reports, 3 key-state presses, 4 cursor moves"
        );
        assert!(take_reached().contains("0 raw-input keyboard reports, 0 raw-input mouse reports, 0 key-state presses, 0 cursor moves"));
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
    fn focus_gain_suppresses_only_mouse_buttons_and_nests() {
        assert!(!suppress_mouse_button(0x01));
        {
            let _g1 = suppress_focus_click_buttons();
            for vk in [0x01, 0x02, 0x04, 0x05, 0x06] {
                assert!(suppress_mouse_button(vk), "mouse vk 0x{vk:x} suppressed during focus gain");
            }
            assert!(!suppress_mouse_button(0x10), "keyboard shift unaffected");
            assert!(!suppress_mouse_button(0x41), "keyboard A unaffected");
            {
                let _g2 = suppress_focus_click_buttons();
                assert!(suppress_mouse_button(0x01));
            }
            assert!(suppress_mouse_button(0x01), "outer guard still active");
        }
        assert!(!suppress_mouse_button(0x01), "restored after drop");
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
