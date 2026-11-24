//! The mouse as pointer messages, for a split-screen copy whose game turned it on
//! (`EnableMouseInPointer`, which Unity games call). Played alone, Windows hands such a
//! game the mouse as WM_POINTER* messages, which it reads back with `GetPointerInfo`, and
//! its menus follow those. In split screen the real mouse is over the session's window,
//! which forwards plain mouse messages (WM_MOUSEMOVE, WM_LBUTTONDOWN...): Windows makes
//! no pointer messages from posted ones, and a game told it can't have pointer messages
//! didn't go back to plain ones (its menus had no mouse at all).
//!
//! So the copy is told pointer messages are on (without asking Windows), and its game
//! window's procedure (window.rs `focus_proc`) gets each forwarded mouse message as the
//! pointer message Windows would have sent; `GetPointerInfo` (and its frame and history
//! forms), `GetPointerType` and `GetPointerCursorId` answer for that pointer as it was
//! at the message being handled.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use crate::log;

/// The pointer's id: the system mouse's own.
pub const MOUSE_ID: u32 = 1;
/// `PT_MOUSE`.
const PT_MOUSE: u32 = 4;

pub const WM_POINTERUPDATE: u32 = 0x0245;
pub const WM_POINTERDOWN: u32 = 0x0246;
pub const WM_POINTERUP: u32 = 0x0247;
pub const WM_POINTERENTER: u32 = 0x0249;
pub const WM_POINTERWHEEL: u32 = 0x024E;
pub const WM_POINTERHWHEEL: u32 = 0x024F;

// POINTER_FLAG_*.
const NEW: u32 = 0x1;
const INRANGE: u32 = 0x2;
const INCONTACT: u32 = 0x4;
const FIRSTBUTTON: u32 = 0x10;
const PRIMARY: u32 = 0x2000;
const DOWN: u32 = 0x1_0000;
const UPDATE: u32 = 0x2_0000;
const UP: u32 = 0x4_0000;
const WHEEL: u32 = 0x8_0000;
const HWHEEL: u32 = 0x10_0000;

const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_RBUTTONDBLCLK: u32 = 0x0206;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MBUTTONDBLCLK: u32 = 0x0209;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_XBUTTONDBLCLK: u32 = 0x020D;
const WM_MOUSEHWHEEL: u32 = 0x020E;

/// The game asked for pointer messages and was told it has them.
static ON: AtomicBool = AtomicBool::new(false);

/// Whether forwarded mouse messages go to the game as pointer messages.
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// The pointer as the forwarded mouse messages left it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    /// On screen.
    pub x: i32,
    pub y: i32,
    /// Buttons down: left, right, middle, X1, X2 (bits 0 to 4).
    pub buttons: u32,
    /// `POINTER_FLAGS` of the last message.
    pub flags: u32,
    /// `POINTER_BUTTON_CHANGE_TYPE` of the last message.
    pub change: u32,
    /// The wheel's delta of the last message (0 when it wasn't the wheel's).
    pub wheel: i32,
    pub frame: u32,
    /// A pointer message has been made (the first one says the pointer entered).
    pub entered: bool,
    /// Shift and Ctrl as the mouse message had them (`MK_SHIFT`, `MK_CONTROL`: the same
    /// bits as `POINTER_MOD_SHIFT`, `POINTER_MOD_CTRL`).
    pub keys: u32,
}

/// `MK_SHIFT | MK_CONTROL`.
const MODS: usize = 0x4 | 0x8;

/// Which button a mouse message is about (0 left .. 4 X2), and whether it went down.
fn button_of(m: u32, w: usize) -> Option<(u32, bool)> {
    let x = if (w >> 16) & 0xffff == 2 { 4 } else { 3 };
    Some(match m {
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => (0, true),
        WM_LBUTTONUP => (0, false),
        WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => (1, true),
        WM_RBUTTONUP => (1, false),
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => (2, true),
        WM_MBUTTONUP => (2, false),
        WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => (x, true),
        WM_XBUTTONUP => (x, false),
        _ => return None,
    })
}

/// Whether `m` is a mouse message pointer mode makes pointer messages of.
fn is_mouse(m: u32, w: usize) -> bool {
    matches!(m, WM_MOUSEMOVE | WM_MOUSEWHEEL | WM_MOUSEHWHEEL) || button_of(m, w).is_some()
}

/// Pure helper: the pointer messages mouse message `m` (`w`, at screen point `(x, y)`)
/// becomes, as Windows sends them for a mouse in pointer mode, each with the pointer as
/// `GetPointerInfo` reads it while that message is handled, with `p` moved on. None for
/// a message that isn't the mouse's.
pub fn translate(p: &mut Pointer, m: u32, w: usize, x: i32, y: i32) -> Option<Vec<(u32, usize, isize, Pointer)>> {
    let is_wheel = matches!(m, WM_MOUSEWHEEL | WM_MOUSEHWHEEL);
    if !is_mouse(m, w) {
        return None;
    }
    let mut out = Vec::with_capacity(2);
    // The wheel's low word has the keys too, as the buttons' do.
    p.keys = (w & MODS) as u32;
    let lp = ((x as u16 as u32) | ((y as u16 as u32) << 16)) as isize;
    let mut first = 0;
    if !p.entered {
        p.entered = true;
        first = NEW;
    }
    p.x = x;
    p.y = y;
    p.frame = p.frame.wrapping_add(1);
    p.wheel = 0;
    p.change = 0;
    let before = p.buttons;
    let msg = if let Some((b, down)) = button_of(m, w) {
        if down {
            p.buttons |= 1 << b;
        } else {
            p.buttons &= !(1 << b);
        }
        // POINTER_CHANGE_FIRSTBUTTON_DOWN is 1, its UP 2, then the second's...
        p.change = b * 2 + if down { 1 } else { 2 };
        if before == 0 && p.buttons != 0 {
            WM_POINTERDOWN
        } else if before != 0 && p.buttons == 0 {
            WM_POINTERUP
        } else {
            WM_POINTERUPDATE
        }
    } else if is_wheel {
        p.wheel = i32::from(((w >> 16) & 0xffff) as u16 as i16);
        if m == WM_MOUSEWHEEL { WM_POINTERWHEEL } else { WM_POINTERHWHEEL }
    } else {
        WM_POINTERUPDATE
    };
    let held = (0..5).filter(|b| p.buttons & (1 << b) != 0).fold(0, |a, b| a | (FIRSTBUTTON << b));
    let contact = if p.buttons != 0 { INCONTACT } else { 0 };
    let kind = match msg {
        WM_POINTERDOWN => DOWN,
        WM_POINTERUP => UP,
        WM_POINTERWHEEL => WHEEL,
        WM_POINTERHWHEEL => HWHEEL,
        _ => UPDATE,
    };
    p.flags = first | INRANGE | PRIMARY | contact | held | kind;
    if first != 0 {
        // Entering: in range, nothing down or changing yet (the message after says that).
        let enter = Pointer { flags: first | INRANGE | PRIMARY, buttons: before, change: 0, wheel: 0, ..*p };
        out.push((WM_POINTERENTER, (MOUSE_ID as usize) | (((first | INRANGE | PRIMARY) as usize) << 16), lp, enter));
    }
    let high = if is_wheel { (p.wheel as u16) as usize } else { (p.flags & 0xffff) as usize };
    out.push((msg, (MOUSE_ID as usize) | (high << 16), lp, *p));
    Some(out)
}

const NO_POINTER: Pointer = Pointer { x: 0, y: 0, buttons: 0, flags: 0, change: 0, wheel: 0, frame: 0, entered: false, keys: 0 };
/// The pointer as the forwarded messages moved it on.
static POINTER: Mutex<Pointer> = Mutex::new(NO_POINTER);
/// The pointer as the message being handled has it (`show`): what `GetPointerInfo` says.
static SHOWN: Mutex<Pointer> = Mutex::new(NO_POINTER);

/// The game window is about to handle a pointer message made with pointer `p`.
pub fn show(p: Pointer) {
    *SHOWN.lock().unwrap_or_else(|e| e.into_inner()) = p;
}

/// Mouse message `m` the game window `h` got as the pointer messages it stands for
/// (its point is in `h`'s client area, the wheel's on screen), each with the pointer
/// to `show` while it's handled. None: hand it on as it is.
pub fn convert(h: windows_sys::Win32::Foundation::HWND, m: u32, w: usize, l: isize) -> Option<Vec<(u32, usize, isize, Pointer)>> {
    // Every message the window gets comes here: the cheap checks first.
    if !on() || !is_mouse(m, w) {
        return None;
    }
    let mut pt = windows_sys::Win32::Foundation::POINT { x: i32::from(l as u16 as i16), y: i32::from((l >> 16) as u16 as i16) };
    if !matches!(m, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
        unsafe { windows_sys::Win32::Graphics::Gdi::ClientToScreen(h, &mut pt) };
    }
    TARGET.store(h as usize, Ordering::Relaxed);
    translate(&mut POINTER.lock().unwrap_or_else(|e| e.into_inner()), m, w, pt.x, pt.y)
}

static TARGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Win32 `POINTER_INFO`.
#[repr(C)]
struct PointerInfo {
    kind: u32,
    id: u32,
    frame: u32,
    flags: u32,
    device: isize,
    hwnd: isize,
    pixel: [i32; 2],
    himetric: [i32; 2],
    pixel_raw: [i32; 2],
    himetric_raw: [i32; 2],
    time: u32,
    history: u32,
    input_data: i32,
    key_states: u32,
    performance_count: u64,
    change: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(count: *mut i64) -> i32;
}

type GetInfo = unsafe extern "system" fn(u32, *mut PointerInfo) -> i32;
/// `GetPointerFrameInfo` / `GetPointerInfoHistory`: an id, a count in and out, the array.
type GetMany = unsafe extern "system" fn(u32, *mut u32, *mut PointerInfo) -> i32;
type GetCursorId = unsafe extern "system" fn(u32, *mut u32) -> i32;
type GetType = unsafe extern "system" fn(u32, *mut u32) -> i32;
type GetOther = unsafe extern "system" fn(u32, *mut u8) -> i32;
type Enable = unsafe extern "system" fn(i32) -> i32;
type IsOn = unsafe extern "system" fn() -> i32;

static GET_INFO: OnceLock<GenericDetour<GetInfo>> = OnceLock::new();
static GET_FRAME: OnceLock<GenericDetour<GetMany>> = OnceLock::new();
static GET_HISTORY: OnceLock<GenericDetour<GetMany>> = OnceLock::new();
static GET_CURSOR_ID: OnceLock<GenericDetour<GetCursorId>> = OnceLock::new();
static GET_TYPE: OnceLock<GenericDetour<GetType>> = OnceLock::new();
static GET_PEN: OnceLock<GenericDetour<GetOther>> = OnceLock::new();
static GET_TOUCH: OnceLock<GenericDetour<GetOther>> = OnceLock::new();
pub static ENABLE: OnceLock<GenericDetour<Enable>> = OnceLock::new();
pub static IS_ON: OnceLock<GenericDetour<IsOn>> = OnceLock::new();

/// A split-screen copy (`window::focus_spoofed`) asking for pointer messages is told it
/// has them, and gets them made from the forwarded mouse.
unsafe extern "system" fn enable(on: i32) -> i32 {
    if crate::window::focus_spoofed() {
        let was = ON.swap(on != 0, Ordering::Relaxed);
        if on != 0 && !was {
            log!("window: the game takes the mouse as pointer messages: the session's forwarded mouse reaches it as those");
        } else if on == 0 && was {
            log!("window: the game turned pointer messages off: the forwarded mouse reaches it as mouse messages again");
        }
        return 1;
    }
    ENABLE.get().map_or(0, |h| unsafe { h.call(on) })
}

unsafe extern "system" fn is_on() -> i32 {
    if crate::window::focus_spoofed() {
        return i32::from(on());
    }
    IS_ON.get().map_or(0, |h| unsafe { h.call() })
}

/// The mouse pointer's `POINTER_INFO` as the message being handled has it.
fn info_now() -> PointerInfo {
    let p = *SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    let mut count = 0i64;
    unsafe { QueryPerformanceCounter(&mut count) };
    // HIMETRIC: hundredths of a millimetre, at 96 pixels to the inch.
    let himetric = [p.x * 2540 / 96, p.y * 2540 / 96];
    PointerInfo {
        kind: PT_MOUSE,
        id: MOUSE_ID,
        frame: p.frame,
        flags: p.flags,
        device: 1,
        hwnd: TARGET.load(Ordering::Relaxed) as isize,
        pixel: [p.x, p.y],
        himetric,
        pixel_raw: [p.x, p.y],
        himetric_raw: himetric,
        time: unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() },
        history: 1,
        input_data: p.wheel,
        key_states: p.keys,
        performance_count: count as u64,
        change: p.change,
    }
}

/// A frame or a history of the mouse pointer: one entry, the message's (Windows keeps
/// no more for a pointer it didn't make). Asked with no room, the count it needs.
unsafe fn one_entry(count: *mut u32, out: *mut PointerInfo) -> i32 {
    if count.is_null() {
        return 0;
    }
    let room = unsafe { *count };
    unsafe { *count = 1 };
    if out.is_null() {
        return 1;
    }
    if room < 1 {
        // ERROR_INSUFFICIENT_BUFFER.
        unsafe { windows_sys::Win32::Foundation::SetLastError(122) };
        return 0;
    }
    unsafe { out.write(info_now()) };
    1
}

unsafe extern "system" fn get_frame(id: u32, count: *mut u32, out: *mut PointerInfo) -> i32 {
    if on() && id == MOUSE_ID {
        return unsafe { one_entry(count, out) };
    }
    GET_FRAME.get().map_or(0, |h| unsafe { h.call(id, count, out) })
}

unsafe extern "system" fn get_history(id: u32, count: *mut u32, out: *mut PointerInfo) -> i32 {
    if on() && id == MOUSE_ID {
        return unsafe { one_entry(count, out) };
    }
    GET_HISTORY.get().map_or(0, |h| unsafe { h.call(id, count, out) })
}

/// The mouse's cursor: id 0, the system's one.
unsafe extern "system" fn get_cursor_id(id: u32, out: *mut u32) -> i32 {
    if on() && id == MOUSE_ID && !out.is_null() {
        unsafe { *out = 0 };
        return 1;
    }
    GET_CURSOR_ID.get().map_or(0, |h| unsafe { h.call(id, out) })
}

unsafe extern "system" fn get_info(id: u32, out: *mut PointerInfo) -> i32 {
    if on() && id == MOUSE_ID && !out.is_null() {
        unsafe { out.write(info_now()) };
        return 1;
    }
    GET_INFO.get().map_or(0, |h| unsafe { h.call(id, out) })
}

unsafe extern "system" fn get_type(id: u32, out: *mut u32) -> i32 {
    if on() && id == MOUSE_ID && !out.is_null() {
        unsafe { *out = PT_MOUSE };
        return 1;
    }
    GET_TYPE.get().map_or(0, |h| unsafe { h.call(id, out) })
}

/// A mouse has no pen or touch data.
unsafe extern "system" fn get_pen(id: u32, out: *mut u8) -> i32 {
    if on() && id == MOUSE_ID {
        return 0;
    }
    GET_PEN.get().map_or(0, |h| unsafe { h.call(id, out) })
}

unsafe extern "system" fn get_touch(id: u32, out: *mut u8) -> i32 {
    if on() && id == MOUSE_ID {
        return 0;
    }
    GET_TOUCH.get().map_or(0, |h| unsafe { h.call(id, out) })
}

/// The pointer calls, from user32's export list or the loaded module
/// (`window::arm_user32`).
pub fn arm(src: &crate::sys::detour::Source) -> usize {
    let mut n = 0;
    macro_rules! one {
        ($cell:expr, $sym:literal, $ty:ty, $detour:expr) => {
            n += crate::sys::detour::arm_from(src, &$cell, $sym, $detour as $ty, "window");
        };
    }
    one!(ENABLE, c"EnableMouseInPointer", Enable, enable);
    one!(IS_ON, c"IsMouseInPointerEnabled", IsOn, is_on);
    one!(GET_INFO, c"GetPointerInfo", GetInfo, get_info);
    one!(GET_TYPE, c"GetPointerType", GetType, get_type);
    one!(GET_FRAME, c"GetPointerFrameInfo", GetMany, get_frame);
    one!(GET_HISTORY, c"GetPointerInfoHistory", GetMany, get_history);
    one!(GET_CURSOR_ID, c"GetPointerCursorId", GetCursorId, get_cursor_id);
    one!(GET_PEN, c"GetPointerPenInfo", GetOther, get_pen);
    one!(GET_TOUCH, c"GetPointerTouchInfo", GetOther, get_touch);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarded_mouse_messages_become_pointer_messages() {
        let mut p = Pointer::default();
        let hi = |w: usize| ((w >> 16) & 0xffff) as u32;
        // The first move: the pointer enters, then moves.
        let m = translate(&mut p, WM_MOUSEMOVE, 0, 100, 50).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].0, WM_POINTERENTER);
        assert_eq!(m[0].3.flags & (DOWN | UPDATE | INCONTACT), 0, "entering says nothing of what comes next");
        assert_eq!((m[1].0, m[1].1 & 0xffff, m[1].2), (WM_POINTERUPDATE, MOUSE_ID as usize, 100 | (50 << 16)));
        assert_eq!(hi(m[1].1) & (INRANGE | PRIMARY | NEW), INRANGE | PRIMARY | NEW);
        assert_eq!(p.flags & (INCONTACT | UPDATE), UPDATE);
        // Left down (Shift held): the pointer goes down, in contact, the first button.
        let m = translate(&mut p, WM_LBUTTONDOWN, 1 | 0x4, 100, 50).unwrap();
        assert_eq!((m[0].3.keys, m[0].3.flags & DOWN), (0x4, DOWN));
        assert_eq!((m.len(), m[0].0), (1, WM_POINTERDOWN));
        assert_eq!(p.flags & (INCONTACT | FIRSTBUTTON | DOWN | NEW), INCONTACT | FIRSTBUTTON | DOWN);
        assert_eq!(p.change, 1, "POINTER_CHANGE_FIRSTBUTTON_DOWN");
        // Right down too: an update; right up: an update; left up: the pointer goes up.
        assert_eq!(translate(&mut p, WM_RBUTTONDOWN, 3, 101, 50).unwrap()[0].0, WM_POINTERUPDATE);
        assert_eq!(p.change, 3);
        assert_eq!(translate(&mut p, WM_RBUTTONUP, 1, 101, 50).unwrap()[0].0, WM_POINTERUPDATE);
        assert_eq!(p.change, 4);
        assert_eq!(translate(&mut p, WM_LBUTTONUP, 0, 101, 50).unwrap()[0].0, WM_POINTERUP);
        assert_eq!((p.buttons, p.flags & (INCONTACT | UP)), (0, UP));
        // The wheel: its delta in the high word.
        let m = translate(&mut p, WM_MOUSEWHEEL, (120usize) << 16, 101, 50).unwrap();
        assert_eq!((m[0].0, hi(m[0].1), p.wheel), (WM_POINTERWHEEL, 120, 120));
        let m = translate(&mut p, WM_MOUSEWHEEL, ((-120i16) as u16 as usize) << 16, 101, 50).unwrap();
        assert_eq!(p.wheel, -120);
        assert_eq!(hi(m[0].1) as u16 as i16, -120);
        // X2 is the fifth button.
        translate(&mut p, WM_XBUTTONDOWN, 2 << 16, 0, 0).unwrap();
        assert_eq!((p.buttons, p.change), (1 << 4, 9));
        assert_eq!(translate(&mut p, 0x0100, 0, 0, 0), None, "a key isn't the mouse");
    }

    #[test]
    fn pointer_info_is_windows_layout() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<PointerInfo>(), 96);
            assert_eq!(std::mem::offset_of!(PointerInfo, pixel), 32);
            assert_eq!(std::mem::offset_of!(PointerInfo, time), 64);
            assert_eq!(std::mem::offset_of!(PointerInfo, performance_count), 80);
            assert_eq!(std::mem::offset_of!(PointerInfo, change), 88);
        }
    }
}
