//! The cursor of a split-screen copy: the real one is the session's, over the session's
//! window, so a copy's game never moves or confines it. Its game's calls are answered
//! for it instead:
//!
//! - `SetCursorPos` moves the copy's own cursor. An aiming game that puts it back in
//!   the middle every frame and reads how far it went then sees the session mouse's
//!   motion since (the input block's motion totals), not where the real cursor is.
//! - `ClipCursor` confines the copy's own cursor, and `GetClipCursor` reads it back. A
//!   clip set or let go is told to the session (`HookMsg::CursorLock`), which keeps the
//!   real mouse in the copy's pane meanwhile (compositor.rs `confine`).
//! - `GetPhysicalCursorPos`, `GetCursorInfo` and `GetMessagePos` say what
//!   `GetCursorPos` does.
//! - `SetCursor` (the cursor the game shows, or none) is told to the session
//!   (`HookMsg::CursorShape`), whose real pointer takes it: the game's window is under
//!   the session's, so its own cursor would never show, and a game that hides the
//!   pointer this way (SDL does) and draws its own would get a second one.
//!
//! A copy alone (no input block) has the real cursor, and its calls go through.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use peerparty_protocol::input_shm::InputSnapshot;
use retour::GenericDetour;
use windows_sys::Win32::Foundation::{POINT, RECT};

use super::{Policy, arm_once, module, policy, proc_addr, shm};
use crate::log;

type SetCursorPosFn = unsafe extern "system" fn(i32, i32) -> i32;
type ClipCursorFn = unsafe extern "system" fn(*const RECT) -> i32;
type GetClipCursorFn = unsafe extern "system" fn(*mut RECT) -> i32;
type GetMessagePosFn = unsafe extern "system" fn() -> u32;
type GetPhysicalCursorPosFn = unsafe extern "system" fn(*mut POINT) -> i32;
type GetCursorInfoFn = unsafe extern "system" fn(*mut CursorInfo) -> i32;
type ShowCursorFn = unsafe extern "system" fn(i32) -> i32;
type SetCursorFn = unsafe extern "system" fn(isize) -> isize;

static SET_POS: OnceLock<GenericDetour<SetCursorPosFn>> = OnceLock::new();
static CLIP: OnceLock<GenericDetour<ClipCursorFn>> = OnceLock::new();
static GET_CLIP: OnceLock<GenericDetour<GetClipCursorFn>> = OnceLock::new();
static MESSAGE_POS: OnceLock<GenericDetour<GetMessagePosFn>> = OnceLock::new();
static PHYSICAL_POS: OnceLock<GenericDetour<GetPhysicalCursorPosFn>> = OnceLock::new();
static CURSOR_INFO: OnceLock<GenericDetour<GetCursorInfoFn>> = OnceLock::new();
static SHOW_CURSOR: OnceLock<GenericDetour<ShowCursorFn>> = OnceLock::new();
static SET_CURSOR: OnceLock<GenericDetour<SetCursorFn>> = OnceLock::new();

/// Win32 `CURSORINFO`.
#[repr(C)]
pub(super) struct CursorInfo {
    size: u32,
    flags: u32,
    cursor: isize,
    pos: POINT,
}

/// How long after the game last put its cursor somewhere it still reads it as moved by
/// the mouse from there (an aiming game puts it back every frame).
const HELD_FOR: Duration = Duration::from_millis(500);

/// Where the game last put its cursor (`SetCursorPos`), and the session mouse's motion
/// totals then (None until the copy next has the mouse).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Anchor {
    pub x: i32,
    pub y: i32,
    pub base: Option<(i32, i32)>,
    pub at: Instant,
}

/// The game's own cursor: where it put it, its clip, and its `ShowCursor` display count
/// (starts at 0: visible when `>= 0`, hidden when `< 0`).
#[derive(Clone, Copy, Default)]
pub(super) struct Own {
    pub anchor: Option<Anchor>,
    pub clip: Option<RECT>,
    pub show_count: i32,
}

static OWN: Mutex<Own> = Mutex::new(Own { anchor: None, clip: None, show_count: 0 });
/// The session was told the cursor is locked (`HookMsg::CursorLock`).
static LOCKED: AtomicBool = AtomicBool::new(false);
/// The session was told the cursor is hidden (`HookMsg::CursorHide`).
static HIDDEN: AtomicBool = AtomicBool::new(false);
/// The cursor the session was last told the game set (`HookMsg::CursorShape`);
/// `usize::MAX` before the first.
static SHAPE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Pure helper: the copy's cursor from snapshot `snap` (the copy has the mouse:
/// `snap.mouse.active`) and its own state, at `now`. Moved by the mouse from where the
/// game last put it while it keeps putting it somewhere (or has it clipped), else the
/// session mouse's place; inside its clip either way. `own`'s anchor learns the motion
/// totals it counts from.
pub(super) fn position(snap: &InputSnapshot, own: &mut Own, now: Instant) -> (i32, i32) {
    let totals = (snap.mouse.delta_x, snap.mouse.delta_y);
    let (x, y) = match own.anchor.as_mut() {
        Some(a) if own.clip.is_some() || now.duration_since(a.at) < HELD_FOR => {
            let (bx, by) = *a.base.get_or_insert(totals);
            (a.x.saturating_add(totals.0.wrapping_sub(bx)), a.y.saturating_add(totals.1.wrapping_sub(by)))
        }
        _ => (snap.mouse.cursor_x, snap.mouse.cursor_y),
    };
    match own.clip {
        // A clip's right and bottom are outside it.
        Some(r) => (x.clamp(r.left, (r.right - 1).max(r.left)), y.clamp(r.top, (r.bottom - 1).max(r.top))),
        None => (x, y),
    }
}

/// The copy's cursor now, for `GetCursorPos` (the copy has the mouse).
pub(super) fn now(snap: &InputSnapshot) -> (i32, i32) {
    position(snap, &mut crate::sys::lock(&OWN), Instant::now())
}

/// Whether this copy's cursor calls are answered for it (a split-screen copy).
fn ours() -> bool {
    shm::attached()
}

unsafe extern "system" fn set_cursor_pos(x: i32, y: i32) -> i32 {
    if !ours() {
        return SET_POS.get().map_or(0, |h| unsafe { h.call(x, y) });
    }
    let base = match policy() {
        Policy::Virtual(s) if s.mouse.active => Some((s.mouse.delta_x, s.mouse.delta_y)),
        _ => None,
    };
    let first = crate::sys::lock(&OWN).anchor.replace(Anchor { x, y, base, at: Instant::now() }).is_none();
    super::remember_cursor(x, y);
    if first {
        log!("input: the game moves its cursor itself (SetCursorPos to ({x}, {y})): the copy's own cursor moves, not the session's, and reads the mouse's motion from there");
    }
    1
}

unsafe extern "system" fn clip_cursor(r: *const RECT) -> i32 {
    if !ours() {
        return CLIP.get().map_or(0, |h| unsafe { h.call(r) });
    }
    let clip = (!r.is_null()).then(|| unsafe { *r });
    crate::sys::lock(&OWN).clip = clip;
    let on = clip.is_some();
    if LOCKED.swap(on, Ordering::Relaxed) != on {
        match clip {
            Some(c) => log!("input: the game locked its cursor to ({}, {})-({}, {}): its own cursor stays there; the session keeps the mouse in its pane", c.left, c.top, c.right, c.bottom),
            None => log!("input: the game let its cursor go"),
        }
        crate::ipc::send(peerparty_protocol::ipc::HookMsg::CursorLock { on });
    }
    1
}

unsafe extern "system" fn get_clip_cursor(out: *mut RECT) -> i32 {
    if !ours() || out.is_null() {
        return GET_CLIP.get().map_or(0, |h| unsafe { h.call(out) });
    }
    let clip = crate::sys::lock(&OWN).clip;
    // Unclipped, the whole of the screen the game is told it has.
    let r = clip.or_else(|| crate::window::virtual_size().map(|(w, h)| RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }));
    match r {
        Some(r) => {
            unsafe { *out = r };
            1
        }
        None => GET_CLIP.get().map_or(0, |h| unsafe { h.call(out) }),
    }
}

unsafe extern "system" fn get_message_pos() -> u32 {
    if ours() {
        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { super::cursor_pos(&mut pt) } != 0 {
            return u32::from(pt.x as u16) | (u32::from(pt.y as u16) << 16);
        }
    }
    MESSAGE_POS.get().map_or(0, |h| unsafe { h.call() })
}

unsafe extern "system" fn get_physical_cursor_pos(pt: *mut POINT) -> i32 {
    if ours() {
        return unsafe { super::cursor_pos(pt) };
    }
    PHYSICAL_POS.get().map_or(0, |h| unsafe { h.call(pt) })
}

unsafe extern "system" fn get_cursor_info(ci: *mut CursorInfo) -> i32 {
    let rc = CURSOR_INFO.get().map_or(0, |h| unsafe { h.call(ci) });
    if rc != 0 && ours() && !ci.is_null() {
        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { super::cursor_pos(&mut pt) } != 0 {
            unsafe { (*ci).pos = pt };
        }
    }
    rc
}

/// Pure helper: steps a copy's virtual `ShowCursor` display count (`show != 0` adds 1,
/// else subtracts 1) and returns `(new_count, hidden)`.
pub(super) fn step_show_count(count: &mut i32, show: bool) -> (i32, bool) {
    *count = if show { count.saturating_add(1) } else { count.saturating_sub(1) };
    (*count, *count < 0)
}

unsafe extern "system" fn show_cursor(show: i32) -> i32 {
    if !ours() {
        return SHOW_CURSOR.get().map_or(0, |h| unsafe { h.call(show) });
    }
    let (count, hidden) = step_show_count(&mut crate::sys::lock(&OWN).show_count, show != 0);
    if HIDDEN.swap(hidden, Ordering::Relaxed) != hidden {
        static TOLD: AtomicBool = AtomicBool::new(false);
        if !TOLD.swap(true, Ordering::Relaxed) {
            log!("input: the game hides its cursor (ShowCursor): its own display count stays separate, and the session hides the pointer while its pane has focus");
        }
        crate::ipc::send(peerparty_protocol::ipc::HookMsg::CursorHide { hidden });
    }
    count
}

unsafe extern "system" fn set_cursor(cursor: isize) -> isize {
    let prev = SET_CURSOR.get().map_or(0, |h| unsafe { h.call(cursor) });
    if ours() && SHAPE.swap(cursor as usize, Ordering::Relaxed) != cursor as usize {
        static TOLD: AtomicUsize = AtomicUsize::new(0);
        if TOLD.fetch_add(1, Ordering::Relaxed) < 4 {
            if cursor == 0 {
                log!("input: the game hides its cursor (SetCursor(NULL)): the session hides the pointer while its pane has focus");
            } else {
                log!("input: the game sets its cursor (SetCursor 0x{cursor:x}): the session's pointer takes it while its pane has focus");
            }
        }
        crate::ipc::send(peerparty_protocol::ipc::HookMsg::CursorShape { handle: cursor as u64 });
    }
    prev
}

/// The cursor calls, in user32 (`hook_kbm`).
pub(super) fn hook() -> usize {
    let Some(m) = module("user32.dll") else { return 0 };
    let mut n = 0;
    macro_rules! one {
        ($cell:expr, $sym:literal, $ty:ty, $detour:expr) => {
            if let Some(a) = proc_addr(m, $sym.as_ptr().cast()) {
                n += arm_once(&$cell, a, $detour as $ty, $sym.to_str().unwrap_or(""));
            }
        };
    }
    one!(SET_POS, c"SetCursorPos", SetCursorPosFn, set_cursor_pos);
    one!(CLIP, c"ClipCursor", ClipCursorFn, clip_cursor);
    one!(GET_CLIP, c"GetClipCursor", GetClipCursorFn, get_clip_cursor);
    one!(MESSAGE_POS, c"GetMessagePos", GetMessagePosFn, get_message_pos);
    one!(PHYSICAL_POS, c"GetPhysicalCursorPos", GetPhysicalCursorPosFn, get_physical_cursor_pos);
    one!(CURSOR_INFO, c"GetCursorInfo", GetCursorInfoFn, get_cursor_info);
    one!(SHOW_CURSOR, c"ShowCursor", ShowCursorFn, show_cursor);
    one!(SET_CURSOR, c"SetCursor", SetCursorFn, set_cursor);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::input_shm::{InputMode, VirtualMouse};

    fn snap(cursor: (i32, i32), totals: (i32, i32)) -> InputSnapshot {
        InputSnapshot { mode: InputMode::Virtual, mouse: VirtualMouse { active: true, cursor_x: cursor.0, cursor_y: cursor.1, delta_x: totals.0, delta_y: totals.1, ..Default::default() }, ..Default::default() }
    }

    /// An aiming game puts its cursor in the middle each frame and reads the mouse's
    /// motion from there; once it stops, the cursor is the session mouse's again.
    #[test]
    fn a_game_that_recentres_reads_the_motion() {
        let t0 = Instant::now();
        let mut own = Own::default();
        assert_eq!(position(&snap((10, 20), (100, 100)), &mut own, t0), (10, 20), "the session's mouse");
        own.anchor = Some(Anchor { x: 640, y: 360, base: None, at: t0 });
        assert_eq!(position(&snap((10, 20), (100, 100)), &mut own, t0), (640, 360), "from where it put it");
        assert_eq!(position(&snap((15, 20), (107, 95)), &mut own, t0), (647, 355), "moved by the motion since");
        own.anchor = Some(Anchor { x: 640, y: 360, base: Some((107, 95)), at: t0 });
        assert_eq!(position(&snap((15, 20), (107, 95)), &mut own, t0), (640, 360));
        let later = t0 + HELD_FOR + Duration::from_millis(1);
        assert_eq!(position(&snap((15, 20), (110, 95)), &mut own, later), (15, 20), "it stopped: the session's mouse again");
        // Clipped, it stays the game's for as long as the clip, and inside it.
        own.clip = Some(RECT { left: 600, top: 300, right: 700, bottom: 400 });
        assert_eq!(position(&snap((15, 20), (200, 95)), &mut own, later), (699, 360));
        own.anchor = None;
        assert_eq!(position(&snap((15, 20), (0, 0)), &mut own, later), (600, 300), "the session mouse's place, clipped");
    }

    #[test]
    fn show_cursor_tracks_display_count() {
        let mut c = 0;
        assert_eq!(step_show_count(&mut c, false), (-1, true));
        assert_eq!(step_show_count(&mut c, false), (-2, true));
        assert_eq!(step_show_count(&mut c, true), (-1, true));
        assert_eq!(step_show_count(&mut c, true), (0, false));
        assert_eq!(step_show_count(&mut c, true), (1, false));
    }

    #[test]
    fn cursor_info_is_windows_layout() {
        #[cfg(target_pointer_width = "64")]
        assert_eq!((std::mem::size_of::<CursorInfo>(), std::mem::offset_of!(CursorInfo, pos)), (24, 16));
        #[cfg(target_pointer_width = "32")]
        assert_eq!((std::mem::size_of::<CursorInfo>(), std::mem::offset_of!(CursorInfo, pos)), (20, 12));
    }
}
