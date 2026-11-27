//! The cursor of a split-screen copy: the real one is the session's, over the session's
//! window, so a copy's game never steals or conflicts with it. Instead:
//!
//! - When this copy owns the live OS pointer (`PaneProjection::live_os`), `GetCursorPos`
//!   (and `GetPhysicalCursorPos`, `GetCursorInfo`, `GetMessagePos`, and refreshed
//!   coalesced `WM_MOUSEMOVE`) reads the real Windows hardware cursor directly in-process
//!   and projects it through the pane's screen rect and UV crop—zero IPC latency, exact
//!   solo-play freshness and DPI coordinates.
//! - A one-shot `SetCursorPos` (menu warp) inversely projects to the compositor's pane
//!   and moves the real pointer while the compositor is in front, keeping hardware and
//!   game cursors 1:1. Repeated `SetCursorPos` calls within `RECENTER_WINDOW` (an aiming
//!   game recentering every frame) switch into raw-delta anchor mode and notify the
//!   session (`CURSOR_RECENTERING`) so the real pointer stays confined and hidden without
//!   fighting it.
//! - `ClipCursor` confines the copy's own cursor and publishes its normalized rect into
//!   `CursorChannel` (plus `HookMsg::CursorLock` fallback), confining the real mouse in
//!   the pane without hiding a visible hardware cursor.
//! - `SetCursor` / `ShowCursor` / `DefWindowProc`'s class cursor publish into
//!   `CursorChannel` and wake the compositor immediately (`WM_APP_CURSOR`), holding a
//!   short-lived `CopyIcon` ring so `DestroyCursor` cannot race cross-process copying.
//!
//! A copy alone (no input block) has the real cursor, and its calls go through.

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use peerparty_protocol::input_shm::{
    CURSOR_CLIPPED, CURSOR_HAS_CLIP_RECT, CURSOR_HAS_SHAPE, CURSOR_HIDDEN_SHAPE, CURSOR_HIDDEN_SHOW,
    CURSOR_RECENTERING, CursorFeedback, InputSnapshot, PaneProjection, WM_APP_CURSOR,
};
use retour::GenericDetour;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows_sys::Win32::UI::WindowsAndMessaging::{CopyIcon, DestroyIcon, GetClientRect, HICON, PostMessageW};

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
type DestroyCursorFn = unsafe extern "system" fn(isize) -> i32;
type SetThreadDpiCtxFn = unsafe extern "system" fn(isize) -> isize;

static SET_POS: OnceLock<GenericDetour<SetCursorPosFn>> = OnceLock::new();
static CLIP: OnceLock<GenericDetour<ClipCursorFn>> = OnceLock::new();
static GET_CLIP: OnceLock<GenericDetour<GetClipCursorFn>> = OnceLock::new();
static MESSAGE_POS: OnceLock<GenericDetour<GetMessagePosFn>> = OnceLock::new();
static PHYSICAL_POS: OnceLock<GenericDetour<GetPhysicalCursorPosFn>> = OnceLock::new();
static CURSOR_INFO: OnceLock<GenericDetour<GetCursorInfoFn>> = OnceLock::new();
static SHOW_CURSOR: OnceLock<GenericDetour<ShowCursorFn>> = OnceLock::new();
static SET_CURSOR: OnceLock<GenericDetour<SetCursorFn>> = OnceLock::new();
static DESTROY_CURSOR: OnceLock<GenericDetour<DestroyCursorFn>> = OnceLock::new();
static DESTROY_ICON: OnceLock<GenericDetour<DestroyCursorFn>> = OnceLock::new();

/// Win32 `CURSORINFO`.
#[repr(C)]
pub(super) struct CursorInfo {
    size: u32,
    flags: u32,
    cursor: isize,
    pos: POINT,
}

/// How long after the game last recentered its cursor it still reads raw motion from the
/// anchor before returning to live pointer coordinates.
const HELD_FOR: Duration = Duration::from_millis(500);
/// Two `SetCursorPos` calls closer than this indicate a recentering mouselook loop
/// rather than a one-shot menu warp.
const RECENTER_WINDOW: Duration = Duration::from_millis(250);

/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`.
const DPI_PER_MONITOR_V2: isize = -4;

/// Where the game last put its cursor during recentering (`SetCursorPos`), and the
/// session mouse's motion totals then (None until the copy next has the mouse).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Anchor {
    pub x: i32,
    pub y: i32,
    pub base: Option<(i32, i32)>,
    pub at: Instant,
}

/// The game's own cursor state: recentering anchor, clip, `ShowCursor` display count,
/// last `SetCursor` shape and short-lived `CopyIcon` copies kept alive for the host.
#[derive(Clone, Copy)]
pub(super) struct Own {
    pub anchor: Option<Anchor>,
    pub last_set: Option<Instant>,
    pub recentering: bool,
    pub clip: Option<RECT>,
    pub clip_norm: Option<[f32; 4]>,
    pub show_count: i32,
    pub has_shape: bool,
    pub shape_handle: u64,
    pub hidden_shape: bool,
    pub shape_gen: u32,
    pub held_copies: [isize; 2],
    pub held_next: usize,
}

impl Default for Own {
    fn default() -> Own {
        Own {
            anchor: None,
            last_set: None,
            recentering: false,
            clip: None,
            clip_norm: None,
            show_count: 0,
            has_shape: false,
            shape_handle: 0,
            hidden_shape: false,
            shape_gen: 0,
            held_copies: [0; 2],
            held_next: 0,
        }
    }
}

static OWN: Mutex<Own> = Mutex::new(Own {
    anchor: None,
    last_set: None,
    recentering: false,
    clip: None,
    clip_norm: None,
    show_count: 0,
    has_shape: false,
    shape_handle: 0,
    hidden_shape: false,
    shape_gen: 0,
    held_copies: [0; 2],
    held_next: 0,
});
/// The session was told the cursor is locked (`HookMsg::CursorLock`, pipe fallback).
static LOCKED: AtomicBool = AtomicBool::new(false);
/// The session was told the cursor is hidden (`HookMsg::CursorHide`, pipe fallback).
static HIDDEN: AtomicBool = AtomicBool::new(false);
/// Fast-path deduplication for `SetCursor` / `observe_thread_cursor`.
static LAST_SHAPE_RAW: AtomicUsize = AtomicUsize::new(usize::MAX);
static LAST_SHAPE_DESTROY_GEN: AtomicU32 = AtomicU32::new(0);
/// Bumped whenever the game destroys a cursor/icon so reused handle values are noticed.
static DESTROY_GEN: AtomicU32 = AtomicU32::new(0);
/// Whether the game is currently in recentering mouselook mode (fast lock-free check).
static RECENTERING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set inside `set_cursor` so `focus_proc` knows whether synthetic `WM_SETCURSOR`
    /// already called `SetCursor` directly.
    static SYNTH_SETCURSOR_HIT: Cell<bool> = const { Cell::new(false) };
    /// Guard so our own `DestroyIcon` on evicted `held_copies` doesn't bump `DESTROY_GEN`.
    static IN_OWN_DESTROY: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` with Per-Monitor V2 DPI awareness on this thread so `GetCursorPos` /
/// `SetCursorPos` operate in physical monitor pixels matching `PaneProjection::dest_*`,
/// even in DPI-unaware games.
fn with_per_monitor_v2<T>(f: impl FnOnce() -> T) -> T {
    static SET_CTX: OnceLock<Option<SetThreadDpiCtxFn>> = OnceLock::new();
    let set = *SET_CTX.get_or_init(|| {
        let m = module("user32.dll")?;
        let a = proc_addr(m, c"SetThreadDpiAwarenessContext".as_ptr().cast())?;
        Some(unsafe { std::mem::transmute::<usize, SetThreadDpiCtxFn>(a) })
    });
    let prev = set.map_or(0, |s| unsafe { s(DPI_PER_MONITOR_V2) });
    let out = f();
    if let Some(s) = set
        && prev != 0
    {
        unsafe { s(prev) };
    }
    out
}

/// Pure helper: whether a `SetCursorPos` at `now` is part of a continuous recentering
/// loop rather than a one-shot warp.
pub(super) fn is_recentering_call(prev_set: Option<Instant>, now: Instant) -> bool {
    prev_set.is_some_and(|t| now.saturating_duration_since(t) < RECENTER_WINDOW)
}

/// Pure helper: clamps `(x, y)` inside `clip` (Win32 clip right/bottom are exclusive).
fn clamp_clip((x, y): (i32, i32), clip: Option<RECT>) -> (i32, i32) {
    match clip {
        Some(r) => (x.clamp(r.left, (r.right - 1).max(r.left)), y.clamp(r.top, (r.bottom - 1).max(r.top))),
        None => (x, y),
    }
}

/// Pure helper: the copy's cursor from snapshot `snap` (the copy has the mouse:
/// `snap.mouse.active`), an optional live-projected screen position `live`, and its own
/// state at `now`. Moved by raw mouse deltas only while actively recentering; otherwise
/// uses the live hardware pointer projection (or the snapshot's cursor fallback), inside
/// any `ClipCursor` rect either way. Returns `(pos, recentering_expired)`.
pub(super) fn position_with_live(snap: &InputSnapshot, live: Option<(i32, i32)>, own: &mut Own, now: Instant) -> ((i32, i32), bool) {
    let mut expired = false;
    let totals = (snap.mouse.delta_x, snap.mouse.delta_y);
    let pos = if own.recentering {
        match own.anchor.as_mut() {
            Some(a) if now.saturating_duration_since(a.at) < HELD_FOR => {
                let (bx, by) = *a.base.get_or_insert(totals);
                (a.x.saturating_add(totals.0.wrapping_sub(bx)), a.y.saturating_add(totals.1.wrapping_sub(by)))
            }
            _ => {
                own.recentering = false;
                own.anchor = None;
                expired = true;
                live.unwrap_or((snap.mouse.cursor_x, snap.mouse.cursor_y))
            }
        }
    } else {
        live.unwrap_or((snap.mouse.cursor_x, snap.mouse.cursor_y))
    };
    (clamp_clip(pos, own.clip), expired)
}

/// Pure helper for tests: `position_with_live` with no live OS pointer.
#[cfg(test)]
pub(super) fn position(snap: &InputSnapshot, own: &mut Own, now: Instant) -> (i32, i32) {
    position_with_live(snap, None, own, now).0
}

/// Projects the real OS hardware pointer directly into `hwnd`'s client pixels in-process.
fn live_client_pos(pane: &PaneProjection, hwnd: HWND) -> Option<(i32, i32)> {
    if hwnd.is_null() || !pane.live_os() {
        return None;
    }
    let mut phys = POINT { x: 0, y: 0 };
    if with_per_monitor_v2(|| super::real_cursor_pos(&mut phys)) == 0 {
        return None;
    }
    let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    if unsafe { GetClientRect(hwnd, &mut rc) } == 0 {
        return None;
    }
    let (cw, ch) = ((rc.right - rc.left).max(0) as u32, (rc.bottom - rc.top).max(0) as u32);
    pane.project_to_client(phys.x, phys.y, cw, ch)
}

/// Projects the real OS hardware pointer directly into the game window's screen coords
/// (in the game's own DPI context).
fn live_screen_pos(pane: &PaneProjection) -> Option<(i32, i32)> {
    let hwnd = crate::window::game_window() as HWND;
    let (gx, gy) = live_client_pos(pane, hwnd)?;
    let mut pt = POINT { x: gx, y: gy };
    unsafe { ClientToScreen(hwnd, &mut pt) };
    Some((pt.x, pt.y))
}

/// Builds the `CursorFeedback` value from `own` and publishes it into `CursorChannel`,
/// then wakes the compositor window immediately via `WM_APP_CURSOR`.
fn publish_own(own: &Own) {
    let Some(ch) = shm::cursor_channel() else { return };
    let mut flags = 0u32;
    if own.has_shape {
        flags |= CURSOR_HAS_SHAPE;
    }
    if own.show_count < 0 {
        flags |= CURSOR_HIDDEN_SHOW;
    }
    if own.hidden_shape {
        flags |= CURSOR_HIDDEN_SHAPE;
    }
    if own.clip.is_some() {
        flags |= CURSOR_CLIPPED;
    }
    if own.recentering {
        flags |= CURSOR_RECENTERING;
    }
    let clip_bits = match own.clip_norm {
        Some(r) => {
            flags |= CURSOR_HAS_CLIP_RECT;
            r.map(f32::to_bits)
        }
        None => [0; 4],
    };
    let fb = CursorFeedback { seq: 0, handle: own.shape_handle, flags, clip_bits };
    ch.publish_feedback(&fb, own.shape_gen);
    let comp = shm::compositor_hwnd();
    if comp != 0 {
        unsafe { PostMessageW(comp as usize as HWND, WM_APP_CURSOR, 0, 0) };
    }
}

/// Flushes any cursor state captured before `shm::attach` finished.
pub(super) fn on_shm_attached() {
    let own = *crate::sys::lock(&OWN);
    if own.has_shape || own.show_count < 0 || own.clip.is_some() || own.recentering {
        publish_own(&own);
    }
}

/// The copy's cursor now, for `GetCursorPos` (the copy has the mouse).
pub(super) fn now(snap: &InputSnapshot) -> (i32, i32) {
    // Project live OS cursor outside `OWN` lock so we never hold a mutex across Win32 calls.
    let live = (!RECENTERING.load(Ordering::Relaxed) && snap.pane.live_os()).then(|| live_screen_pos(&snap.pane)).flatten();
    let mut guard = crate::sys::lock(&OWN);
    let (pos, expired) = position_with_live(snap, live, &mut guard, Instant::now());
    if expired {
        RECENTERING.store(false, Ordering::Relaxed);
        let copy = *guard;
        drop(guard);
        publish_own(&copy);
        return pos;
    }
    pos
}

/// Called from `window.rs` `focus_proc` when a forwarded `WM_MOUSEMOVE` arrives:
/// acknowledges the coalesced move in `CursorChannel` and substitutes the live-projected
/// hardware cursor position so the game always sees zero-age coordinates.
pub(crate) fn on_forwarded_mouse_move(hwnd: isize, lparam: isize) -> isize {
    let ch = shm::cursor_channel();
    let seq = ch.map(|c| c.ack_latest_move());
    if !ours() || RECENTERING.load(Ordering::Relaxed) {
        return lparam;
    }
    let Policy::Virtual(snap) = policy() else { return lparam };
    if !snap.mouse.active || !snap.pane.live_os() {
        return lparam;
    }
    let h = hwnd as HWND;
    let mut pos = live_client_pos(&snap.pane, h);
    // If another move arrived concurrently right after our ack, ack again and re-read
    // once so the final stop position can never be missed.
    if let (Some(c), Some(s)) = (ch, seq)
        && c.move_seq.load(Ordering::SeqCst) != s
    {
        c.ack_latest_move();
        if let Some(newer) = live_client_pos(&snap.pane, h) {
            pos = Some(newer);
        }
    }
    if let Some((gx, gy)) = pos {
        let mut pt = POINT { x: gx, y: gy };
        unsafe { ClientToScreen(h, &mut pt) };
        super::remember_cursor(pt.x, pt.y);
        return ((gx as u16 as u32) | ((gy as u16 as u32) << 16)) as isize;
    }
    lparam
}

/// Brackets `focus_proc`'s synthetic `WM_SETCURSOR` dispatch so it knows whether the
/// game's own handler called `SetCursor` directly.
pub(crate) fn begin_synth_setcursor() {
    SYNTH_SETCURSOR_HIT.with(|f| f.set(false));
}

pub(crate) fn end_synth_setcursor() -> bool {
    SYNTH_SETCURSOR_HIT.with(|f| f.replace(false))
}

/// Observes the thread's cursor after synthetic `WM_SETCURSOR` (catches `WNDCLASS.hCursor`
/// and `SetClassLongPtr(GCLP_HCURSOR)` set inside `DefWindowProc` via `NtUserSetCursor`).
pub(crate) fn observe_thread_cursor(cursor: isize) {
    if ours() && cursor != 0 {
        record_cursor_shape(cursor);
    }
}

/// Whether this copy's cursor calls are answered for it (a split-screen copy).
fn ours() -> bool {
    shm::attached()
}

/// Normalizes a screen `RECT` against the game window's client rect (`0.0..=1.0`).
fn normalize_clip(r: RECT) -> Option<[f32; 4]> {
    let hwnd = crate::window::game_window() as HWND;
    if hwnd.is_null() {
        return None;
    }
    let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    if unsafe { GetClientRect(hwnd, &mut rc) } == 0 {
        return None;
    }
    let (cw, ch) = ((rc.right - rc.left) as f32, (rc.bottom - rc.top) as f32);
    if cw <= 1.0 || ch <= 1.0 {
        return None;
    }
    let mut tl = POINT { x: r.left, y: r.top };
    let mut br = POINT { x: r.right, y: r.bottom };
    unsafe {
        ScreenToClient(hwnd, &mut tl);
        ScreenToClient(hwnd, &mut br);
    }
    Some([
        (tl.x as f32 / cw).clamp(0.0, 1.0),
        (tl.y as f32 / ch).clamp(0.0, 1.0),
        (br.x as f32 / cw).clamp(0.0, 1.0),
        (br.y as f32 / ch).clamp(0.0, 1.0),
    ])
}

unsafe extern "system" fn set_cursor_pos(x: i32, y: i32) -> i32 {
    if !ours() {
        return SET_POS.get().map_or(0, |h| unsafe { h.call(x, y) });
    }
    let now = Instant::now();
    let snap = match policy() {
        Policy::Virtual(s) if s.mouse.active => Some(s),
        _ => None,
    };
    let (recenter, changed_recenter, first_recenter, copy_for_publish) = {
        let mut own = crate::sys::lock(&OWN);
        let recenter = is_recentering_call(own.last_set, now);
        own.last_set = Some(now);
        let was_recentering = own.recentering;
        let first = recenter && own.anchor.is_none();
        if recenter {
            own.recentering = true;
            let base = snap.map(|s| (s.mouse.delta_x, s.mouse.delta_y));
            own.anchor = Some(Anchor { x, y, base, at: now });
        } else {
            own.recentering = false;
            own.anchor = None;
        }
        RECENTERING.store(own.recentering, Ordering::Relaxed);
        (recenter, was_recentering != own.recentering, first, *own)
    };
    super::remember_cursor(x, y);
    if changed_recenter {
        publish_own(&copy_for_publish);
    }
    if first_recenter {
        log!("input: the game recenters its cursor (SetCursorPos to ({x}, {y})): reading raw mouse deltas from there while it keeps recentering");
    }
    // One-shot warp (menu / UI): inversely map to the compositor's pane and move the
    // real OS cursor so hardware cursor and game stay 1:1.
    if !recenter
        && let Some(s) = snap
        && s.pane.live_os()
        && s.pane.compositor_front()
    {
        let hwnd = crate::window::game_window() as HWND;
        if !hwnd.is_null() {
            let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            let mut pt = POINT { x, y };
            if unsafe { GetClientRect(hwnd, &mut rc) != 0 && ScreenToClient(hwnd, &mut pt) != 0 } {
                let (cw, ch) = ((rc.right - rc.left).max(0) as u32, (rc.bottom - rc.top).max(0) as u32);
                if let Some((sx, sy)) = s.pane.unproject_from_client(pt.x, pt.y, cw, ch)
                    && let Some(real) = SET_POS.get()
                {
                    with_per_monitor_v2(|| unsafe { real.call(sx, sy) });
                }
            }
        }
    }
    1
}

unsafe extern "system" fn clip_cursor(r: *const RECT) -> i32 {
    if !ours() {
        return CLIP.get().map_or(0, |h| unsafe { h.call(r) });
    }
    let clip = (!r.is_null()).then(|| unsafe { *r });
    let clip_norm = clip.and_then(normalize_clip);
    let copy = {
        let mut own = crate::sys::lock(&OWN);
        own.clip = clip;
        own.clip_norm = clip_norm;
        *own
    };
    publish_own(&copy);
    let on = clip.is_some();
    if LOCKED.swap(on, Ordering::Relaxed) != on {
        match clip {
            Some(c) => log!("input: the game confined its cursor to ({}, {})-({}, {}): the session confines the mouse to its pane (visibility unchanged)", c.left, c.top, c.right, c.bottom),
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
    let (count, hidden, copy) = {
        let mut own = crate::sys::lock(&OWN);
        let (count, hidden) = step_show_count(&mut own.show_count, show != 0);
        (count, hidden, *own)
    };
    if HIDDEN.swap(hidden, Ordering::Relaxed) != hidden {
        static TOLD: AtomicBool = AtomicBool::new(false);
        if !TOLD.swap(true, Ordering::Relaxed) {
            log!("input: the game hides its cursor (ShowCursor): its own display count stays separate, and the session hides the pointer while its pane has focus");
        }
        publish_own(&copy);
        crate::ipc::send(peerparty_protocol::ipc::HookMsg::CursorHide { hidden });
    }
    count
}

fn record_cursor_shape(cursor: isize) {
    let dgen = DESTROY_GEN.load(Ordering::Relaxed);
    if LAST_SHAPE_RAW.load(Ordering::Relaxed) == cursor as usize && LAST_SHAPE_DESTROY_GEN.load(Ordering::Relaxed) == dgen {
        return;
    }
    let held = if cursor == 0 {
        0isize
    } else {
        // Keep an owned copy alive in this process so a subsequent `DestroyCursor` by
        // the game cannot invalidate the handle before the compositor copies it.
        let dup = unsafe { CopyIcon(cursor as HICON) } as isize;
        if dup != 0 { dup } else { cursor }
    };
    let (evicted, copy) = {
        let mut own = crate::sys::lock(&OWN);
        let evicted = if held != 0 && held != cursor {
            let slot = own.held_next % own.held_copies.len();
            own.held_next = own.held_next.wrapping_add(1);
            let old = own.held_copies[slot];
            own.held_copies[slot] = held;
            old
        } else {
            0
        };
        own.has_shape = true;
        own.shape_handle = held as usize as u64;
        own.hidden_shape = cursor == 0;
        own.shape_gen = own.shape_gen.wrapping_add(1);
        LAST_SHAPE_RAW.store(cursor as usize, Ordering::Relaxed);
        LAST_SHAPE_DESTROY_GEN.store(dgen, Ordering::Relaxed);
        (evicted, *own)
    };
    if evicted != 0 {
        IN_OWN_DESTROY.with(|f| f.set(true));
        unsafe { DestroyIcon(evicted as HICON) };
        IN_OWN_DESTROY.with(|f| f.set(false));
    }
    if ours() {
        static TOLD: AtomicUsize = AtomicUsize::new(0);
        if TOLD.fetch_add(1, Ordering::Relaxed) < 4 {
            if cursor == 0 {
                log!("input: the game hides its cursor (SetCursor(NULL)): the session hides the pointer over its pane");
            } else {
                log!("input: the game sets its cursor (SetCursor 0x{cursor:x}): the session's pointer takes it over its pane");
            }
        }
        publish_own(&copy);
        crate::ipc::send(peerparty_protocol::ipc::HookMsg::CursorShape { handle: copy.shape_handle });
    }
}

unsafe extern "system" fn set_cursor(cursor: isize) -> isize {
    SYNTH_SETCURSOR_HIT.with(|f| f.set(true));
    let prev = SET_CURSOR.get().map_or(0, |h| unsafe { h.call(cursor) });
    if ours() || crate::window::focus_spoofed() {
        record_cursor_shape(cursor);
    }
    prev
}

unsafe extern "system" fn destroy_cursor(cursor: isize) -> i32 {
    let rc = DESTROY_CURSOR.get().map_or(0, |h| unsafe { h.call(cursor) });
    if rc != 0 && !IN_OWN_DESTROY.with(Cell::get) {
        DESTROY_GEN.fetch_add(1, Ordering::Relaxed);
    }
    rc
}

unsafe extern "system" fn destroy_icon(icon: isize) -> i32 {
    let rc = DESTROY_ICON.get().map_or(0, |h| unsafe { h.call(icon) });
    if rc != 0 && !IN_OWN_DESTROY.with(Cell::get) {
        DESTROY_GEN.fetch_add(1, Ordering::Relaxed);
    }
    rc
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
    one!(DESTROY_CURSOR, c"DestroyCursor", DestroyCursorFn, destroy_cursor);
    one!(DESTROY_ICON, c"DestroyIcon", DestroyCursorFn, destroy_icon);
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
    /// motion from there; once it stops, the cursor returns to the live pointer (even
    /// if a clip rect is active, as strategy/ARPG games confine visible cursors).
    #[test]
    fn a_game_that_recentres_reads_the_motion() {
        let t0 = Instant::now();
        let mut own = Own::default();
        assert!(!is_recentering_call(None, t0), "first call is a one-shot warp");
        assert!(is_recentering_call(Some(t0), t0 + Duration::from_millis(16)), "second frame call is recentering");
        assert!(!is_recentering_call(Some(t0), t0 + RECENTER_WINDOW + Duration::from_millis(1)), "far-apart call is a one-shot warp");

        assert_eq!(position(&snap((10, 20), (100, 100)), &mut own, t0), (10, 20), "the session's mouse");
        own.recentering = true;
        own.anchor = Some(Anchor { x: 640, y: 360, base: None, at: t0 });
        assert_eq!(position(&snap((10, 20), (100, 100)), &mut own, t0), (640, 360), "from where it put it");
        assert_eq!(position(&snap((15, 20), (107, 95)), &mut own, t0), (647, 355), "moved by the motion since");
        own.anchor = Some(Anchor { x: 640, y: 360, base: Some((107, 95)), at: t0 });
        assert_eq!(position(&snap((15, 20), (107, 95)), &mut own, t0), (640, 360));
        let later = t0 + HELD_FOR + Duration::from_millis(1);
        // Even with a clip rect set, stopping recentering returns to live/session coords clamped to clip.
        own.clip = Some(RECT { left: 600, top: 300, right: 700, bottom: 400 });
        let (pos, expired) = position_with_live(&snap((15, 20), (200, 95)), Some((650, 350)), &mut own, later);
        assert!(expired && !own.recentering);
        assert_eq!(pos, (650, 350), "live pointer inside clip");
        let (clamped, _) = position_with_live(&snap((15, 20), (0, 0)), Some((10, 20)), &mut own, later);
        assert_eq!(clamped, (600, 300), "live pointer clamped to clip");
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
