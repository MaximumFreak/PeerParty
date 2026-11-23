//! A split-screen copy's saved screen size is its pane's. Unity keeps the resolution a
//! game last ran at in the registry (`Screenmanager Resolution Width_h…` and `…Height_h…`,
//! the same names in every Unity game) and starts at it: a copy that last ran in a pane
//! of another shape drew a frame of that size inside its window, with black bars. Here:
//!
//! - reads of those two values answer the pane's width and height (window.rs
//!   `virtual_size`), and
//! - a split-screen copy's writes of `Screenmanager …` values don't happen, so playing
//!   in split screen leaves the single-window settings as they were.
//!
//! kernelbase's registry calls are hooked (advapi32's forward there), only in a copy
//! that has a pane.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use retour::GenericDetour;

use crate::log;
use crate::sys::pe::Export;

type QueryW = unsafe extern "system" fn(isize, *const u16, *mut u32, *mut u32, *mut u8, *mut u32) -> i32;
type QueryA = unsafe extern "system" fn(isize, *const u8, *mut u32, *mut u32, *mut u8, *mut u32) -> i32;
type GetValueW = unsafe extern "system" fn(isize, *const u16, *const u16, u32, *mut u32, *mut u8, *mut u32) -> i32;
type EnumW = unsafe extern "system" fn(isize, u32, *mut u16, *mut u32, *mut u32, *mut u32, *mut u8, *mut u32) -> i32;
type SetW = unsafe extern "system" fn(isize, *const u16, u32, u32, *const u8, u32) -> i32;
type SetA = unsafe extern "system" fn(isize, *const u8, u32, u32, *const u8, u32) -> i32;

static QUERY_W: OnceLock<GenericDetour<QueryW>> = OnceLock::new();
static QUERY_A: OnceLock<GenericDetour<QueryA>> = OnceLock::new();
static GET_W: OnceLock<GenericDetour<GetValueW>> = OnceLock::new();
static ENUM_W: OnceLock<GenericDetour<EnumW>> = OnceLock::new();
static SET_W: OnceLock<GenericDetour<SetW>> = OnceLock::new();
static SET_A: OnceLock<GenericDetour<SetA>> = OnceLock::new();

const REG_DWORD: u32 = 4;
const ERROR_SUCCESS: i32 = 0;

/// Which saved screen value a registry value name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Width,
    Height,
    /// Another of the engine's screen settings (fullscreen mode, window position...).
    Other,
}

/// Pure helper: the saved screen value `name` (no NUL) names, if it is one. The
/// "Default" ones are the build's defaults, not what the player last ran at.
pub fn screen_value(name: &str) -> Option<Screen> {
    let rest = name.strip_prefix("Screenmanager ")?;
    if rest.contains("Default") {
        return Some(Screen::Other);
    }
    Some(if rest.starts_with("Resolution Width_h") {
        Screen::Width
    } else if rest.starts_with("Resolution Height_h") {
        Screen::Height
    } else {
        Screen::Other
    })
}

fn name_w(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut v = Vec::with_capacity(64);
    for i in 0..256 {
        let c = unsafe { *p.add(i) };
        if c == 0 {
            return Some(String::from_utf16_lossy(&v));
        }
        v.push(c);
    }
    None
}

fn name_a(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut v = Vec::with_capacity(64);
    for i in 0..256 {
        let c = unsafe { *p.add(i) };
        if c == 0 {
            return Some(String::from_utf8_lossy(&v).into_owned());
        }
        v.push(c);
    }
    None
}

/// The pane's value for saved screen value `which`.
fn pane_value(which: Screen) -> Option<u32> {
    let (w, h) = crate::window::virtual_size()?;
    match which {
        Screen::Width => Some(w),
        Screen::Height => Some(h),
        Screen::Other => None,
    }
}

static TOLD_READ: AtomicBool = AtomicBool::new(false);
static TOLD_WRITE: AtomicBool = AtomicBool::new(false);

/// A read of value `name` succeeded with `data` (`len` bytes of type `kind`): the pane's
/// size goes in its place when it's the saved width or height.
fn patch(name: Option<String>, kind: *const u32, data: *mut u8, len: *const u32) {
    let Some(which) = name.as_deref().and_then(screen_value) else { return };
    let Some(v) = pane_value(which) else { return };
    if data.is_null() || len.is_null() || unsafe { *len } < 4 || (!kind.is_null() && unsafe { *kind } != REG_DWORD) {
        return;
    }
    let saved = unsafe { (data as *const u32).read_unaligned() };
    unsafe { (data as *mut u32).write_unaligned(v) };
    if saved != v && !TOLD_READ.swap(true, Ordering::Relaxed) {
        log!("window: the game's saved resolution ({saved} on one side) reads as its pane's {:?}", crate::window::virtual_size().unwrap_or_default());
    }
}

/// Whether a write of value `name` doesn't happen (a split-screen copy's screen settings).
fn skip_write(name: Option<String>) -> bool {
    if !name.as_deref().is_some_and(|n| screen_value(n).is_some()) || crate::window::virtual_size().is_none() {
        return false;
    }
    if !TOLD_WRITE.swap(true, Ordering::Relaxed) {
        log!("window: the game's screen settings aren't saved from split screen (the single-window ones stay)");
    }
    true
}

unsafe extern "system" fn query_w(key: isize, name: *const u16, reserved: *mut u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    let rc = unsafe { QUERY_W.get().unwrap().call(key, name, reserved, kind, data, len) };
    if rc == ERROR_SUCCESS && !data.is_null() {
        patch(name_w(name), kind, data, len);
    }
    rc
}

unsafe extern "system" fn query_a(key: isize, name: *const u8, reserved: *mut u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    let rc = unsafe { QUERY_A.get().unwrap().call(key, name, reserved, kind, data, len) };
    if rc == ERROR_SUCCESS && !data.is_null() {
        patch(name_a(name), kind, data, len);
    }
    rc
}

unsafe extern "system" fn get_w(key: isize, sub: *const u16, name: *const u16, flags: u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    let rc = unsafe { GET_W.get().unwrap().call(key, sub, name, flags, kind, data, len) };
    if rc == ERROR_SUCCESS && !data.is_null() {
        patch(name_w(name), kind, data, len);
    }
    rc
}

unsafe extern "system" fn enum_w(key: isize, i: u32, name: *mut u16, name_len: *mut u32, reserved: *mut u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    let rc = unsafe { ENUM_W.get().unwrap().call(key, i, name, name_len, reserved, kind, data, len) };
    if rc == ERROR_SUCCESS && !data.is_null() {
        patch(name_w(name), kind, data, len);
    }
    rc
}

unsafe extern "system" fn set_w(key: isize, name: *const u16, reserved: u32, kind: u32, data: *const u8, len: u32) -> i32 {
    if skip_write(name_w(name)) {
        return ERROR_SUCCESS;
    }
    unsafe { SET_W.get().unwrap().call(key, name, reserved, kind, data, len) }
}

unsafe extern "system" fn set_a(key: isize, name: *const u8, reserved: u32, kind: u32, data: *const u8, len: u32) -> i32 {
    if skip_write(name_a(name)) {
        return ERROR_SUCCESS;
    }
    unsafe { SET_A.get().unwrap().call(key, name, reserved, kind, data, len) }
}

/// Hooks kernelbase's registry value calls, in a copy with a pane.
pub fn attach_kernelbase(exports: &[Export]) -> usize {
    if crate::window::virtual_size().is_none() {
        return 0;
    }
    use crate::sys::detour::{arm, hook_one};
    let mut n = 0;
    n += hook_one(exports, "screen", "RegQueryValueExW", |a| arm(&QUERY_W, unsafe { std::mem::transmute::<*const (), QueryW>(a) }, query_w, "screen", "RegQueryValueExW"));
    n += hook_one(exports, "screen", "RegQueryValueExA", |a| arm(&QUERY_A, unsafe { std::mem::transmute::<*const (), QueryA>(a) }, query_a, "screen", "RegQueryValueExA"));
    n += hook_one(exports, "screen", "RegGetValueW", |a| arm(&GET_W, unsafe { std::mem::transmute::<*const (), GetValueW>(a) }, get_w, "screen", "RegGetValueW"));
    n += hook_one(exports, "screen", "RegEnumValueW", |a| arm(&ENUM_W, unsafe { std::mem::transmute::<*const (), EnumW>(a) }, enum_w, "screen", "RegEnumValueW"));
    n += hook_one(exports, "screen", "RegSetValueExW", |a| arm(&SET_W, unsafe { std::mem::transmute::<*const (), SetW>(a) }, set_w, "screen", "RegSetValueExW"));
    n += hook_one(exports, "screen", "RegSetValueExA", |a| arm(&SET_A, unsafe { std::mem::transmute::<*const (), SetA>(a) }, set_a, "screen", "RegSetValueExA"));
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engines_screen_values_are_told_apart() {
        assert_eq!(screen_value("Screenmanager Resolution Width_h182942802"), Some(Screen::Width));
        assert_eq!(screen_value("Screenmanager Resolution Height_h2627697771"), Some(Screen::Height));
        assert_eq!(screen_value("Screenmanager Resolution Width Default_h680557497"), Some(Screen::Other), "the build's default");
        assert_eq!(screen_value("Screenmanager Fullscreen mode_h3630240806"), Some(Screen::Other));
        assert_eq!(screen_value("Screenmanager Resolution Use Native_h1405027254"), Some(Screen::Other));
        assert_eq!(screen_value("VSyncSetting_h1635279426"), None, "the game's own");
    }
}
