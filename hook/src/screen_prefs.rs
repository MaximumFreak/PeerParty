//! A split-screen copy's saved screen size is its pane's. Unity keeps the resolution a
//! game last ran at in the registry (`Screenmanager Resolution Width_h…`, `…Height_h…`,
//! and for windowed mode `Screenmanager Resolution Window Width_h…` / `…Height_h…`, the
//! same names in every Unity game) and starts at it: a copy that last ran in a pane of
//! another shape, or a later player whose own registry starts empty, would otherwise draw
//! a frame of the wrong size inside its window. Here:
//!
//! - reads of those values answer the pane's width and height (window.rs `virtual_size`),
//!   `Screenmanager Resolution Use Native_h…` answers 0 so the engine uses those dimensions,
//!   and the saved fullscreen mode (`Screenmanager Fullscreen mode_h…`, older engines'
//!   `Screenmanager Is Fullscreen mode_h…`) answers windowed: a copy that last ran
//!   fullscreen would start by taking the whole monitor,
//! - a split-screen copy's writes of `Screenmanager …` values don't happen, so playing
//!   in split screen leaves the single-window settings as they were. The engine's record
//!   of the build's defaults (`… Default_h…`) is written as usual: it's the build's, not
//!   a setting, and without it the engine never reads the saved size at all (a later
//!   player's key starts with player 1's, saves/registry.rs `seed_build_defaults`), and
//! - a game that applies its own saved resolution after start-up (`Screen.SetResolution`)
//!   gets the pane's size instead of a smaller default (a larger one the engine already
//!   clamps to the pane, but a fresh save's small default would shrink the frame and leave
//!   later render targets mismatched): Unity registers that call by name through
//!   `il2cpp_add_internal_call` / `mono_add_internal_call`, which `swap_lookup` wraps.
//!
//! kernelbase's registry calls are hooked (advapi32's forward there), only in a copy
//! that has a pane.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    /// Whether the engine should take the display's native size instead of the saved one.
    UseNative,
    /// The fullscreen mode (0 exclusive, 1 fullscreen window, 2 maximized, 3 windowed).
    FullscreenMode,
    /// Older engines' fullscreen switch (1 fullscreen).
    IsFullscreen,
    /// The engine's record of the build's defaults (`… Default_h…`): when it differs from
    /// the build's, or is missing, the saved size is taken as stale and not read.
    Default,
    /// Another of the engine's screen settings (window position, stereo...).
    Other,
}

/// Pure helper: the saved screen value `name` (no NUL) names, if it is one. The
/// "Default" ones are the build's defaults, not what the player last ran at.
pub fn screen_value(name: &str) -> Option<Screen> {
    let rest = name.strip_prefix("Screenmanager ")?;
    if rest.contains(" Default_h") {
        return Some(Screen::Default);
    }
    Some(if rest.starts_with("Resolution Width_h") || rest.starts_with("Resolution Window Width_h") {
        Screen::Width
    } else if rest.starts_with("Resolution Height_h") || rest.starts_with("Resolution Window Height_h") {
        Screen::Height
    } else if rest.starts_with("Resolution Use Native_h") {
        Screen::UseNative
    } else if rest.starts_with("Fullscreen mode_h") {
        Screen::FullscreenMode
    } else if rest.starts_with("Is Fullscreen mode_h") {
        Screen::IsFullscreen
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
fn pane_value_for(which: Screen, pane: Option<(u32, u32)>) -> Option<u32> {
    let (w, h) = pane?;
    match which {
        Screen::Width => Some(w),
        Screen::Height => Some(h),
        // Use the patched saved dimensions instead of letting the engine query native size.
        Screen::UseNative => Some(0),
        // Windowed: a pane is a window.
        Screen::FullscreenMode => Some(3),
        Screen::IsFullscreen => Some(0),
        Screen::Default | Screen::Other => None,
    }
}

fn pane_value(which: Screen) -> Option<u32> {
    pane_value_for(which, crate::window::virtual_size())
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
    if saved != v {
        match which {
            Screen::FullscreenMode | Screen::IsFullscreen => {
                static TOLD_MODE: AtomicBool = AtomicBool::new(false);
                if !TOLD_MODE.swap(true, Ordering::Relaxed) {
                    log!("window: the game's saved fullscreen setting ({saved}) reads as windowed: a pane is a window");
                }
            }
            Screen::Width | Screen::Height if !TOLD_READ.swap(true, Ordering::Relaxed) => {
                log!("window: the game's saved resolution ({saved} on one side) reads as its pane's {:?}", crate::window::virtual_size().unwrap_or_default());
            }
            _ => {}
        }
    }
}

/// Whether a write of value `name` doesn't happen (a split-screen copy's screen settings,
/// not the build's defaults).
fn skip_write(name: Option<String>) -> bool {
    let Some(n) = name else { return false };
    if !screen_value(&n).is_some_and(|s| s != Screen::Default) || crate::window::virtual_size().is_none() {
        return false;
    }
    if !TOLD_WRITE.swap(true, Ordering::Relaxed) {
        log!("window: the game's screen settings aren't saved from split screen (the single-window ones stay; first: {n})");
    }
    true
}

/// A read of value `name` that found none: the pane's value is answered for a saved
/// screen value all the same (a later player's own registry starts empty, and the
/// engine's default is the build's size or the whole monitor). Handles existence probes
/// (`data == null`, `len == null`) as well as size and value queries. `rc` otherwise.
fn missing_with(name: Option<String>, pane: Option<(u32, u32)>, kind: *mut u32, data: *mut u8, len: *mut u32, rc: i32) -> i32 {
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_MORE_DATA: i32 = 234;
    if rc != ERROR_FILE_NOT_FOUND {
        return rc;
    }
    let Some(which) = name.as_deref().and_then(screen_value) else { return rc };
    let Some(v) = pane_value_for(which, pane) else {
        static TOLD_NONE: AtomicBool = AtomicBool::new(false);
        if which == Screen::Default && !TOLD_NONE.swap(true, Ordering::Relaxed) {
            log!("window: the game has no record of the build's screen defaults ({}): the engine may start at the build's size, not its pane's", name.unwrap_or_default());
        }
        return rc;
    };
    if !kind.is_null() {
        unsafe { *kind = REG_DWORD };
    }
    if len.is_null() {
        return if data.is_null() { ERROR_SUCCESS } else { rc };
    }
    let room = unsafe { *len };
    unsafe { *len = 4 };
    if data.is_null() {
        return ERROR_SUCCESS;
    }
    if room < 4 {
        return ERROR_MORE_DATA;
    }
    unsafe { (data as *mut u32).write_unaligned(v) };
    // Each value once: which ones the engine asks for tells how it picks its size.
    static TOLD_MISSING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let name = name.unwrap_or_default();
    let first = {
        let mut told = crate::sys::lock(&TOLD_MISSING);
        told.len() < 8 && !told.contains(&name) && {
            told.push(name.clone());
            true
        }
    };
    if first {
        log!("window: the game has no saved screen setting {name} yet; it reads as its pane's ({v})");
    }
    ERROR_SUCCESS
}

fn missing(name: Option<String>, kind: *mut u32, data: *mut u8, len: *mut u32, rc: i32) -> i32 {
    missing_with(name, crate::window::virtual_size(), kind, data, len, rc)
}

unsafe extern "system" fn query_w(key: isize, name: *const u16, reserved: *mut u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    let rc = unsafe { QUERY_W.get().unwrap().call(key, name, reserved, kind, data, len) };
    if rc == ERROR_SUCCESS && !data.is_null() {
        patch(name_w(name), kind, data, len);
    }
    missing(name_w(name), kind, data, len, rc)
}

unsafe extern "system" fn query_a(key: isize, name: *const u8, reserved: *mut u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    let rc = unsafe { QUERY_A.get().unwrap().call(key, name, reserved, kind, data, len) };
    if rc == ERROR_SUCCESS && !data.is_null() {
        patch(name_a(name), kind, data, len);
    }
    missing(name_a(name), kind, data, len, rc)
}

unsafe extern "system" fn get_w(key: isize, sub: *const u16, name: *const u16, flags: u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    // A later player's own key, when the subkey names a game's (saves/registry.rs).
    let routed = crate::saves::registry::subkey_w(key, sub);
    let (key, sub) = routed.as_ref().map_or((key, sub), |(k, s)| (*k, s.as_ptr()));
    let rc = unsafe { GET_W.get().unwrap().call(key, sub, name, flags, kind, data, len) };
    if rc == ERROR_SUCCESS && !data.is_null() {
        patch(name_w(name), kind, data, len);
    }
    missing(name_w(name), kind, data, len, rc)
}

/// Whether RegGetValueW has this module's detour (saves/registry.rs then leaves it).
pub fn get_value_hooked() -> bool {
    GET_W.get().is_some()
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

/// A scripting runtime's call that registers an engine function by name.
type AddInternalCall = unsafe extern "C" fn(*const u8, usize);
/// `Screen.SetResolution`'s engine function: width and height, then the mode and the
/// refresh rate (or pointer to a RefreshRate struct) as they came.
type SetResolution = unsafe extern "C" fn(i32, i32, usize, usize);

static REAL_ADD_IL2CPP: AtomicUsize = AtomicUsize::new(0);
static REAL_ADD_MONO: AtomicUsize = AtomicUsize::new(0);
static REAL_SET_RESOLUTION: AtomicUsize = AtomicUsize::new(0);

/// Pure helper: whether `name` is Unity's `Screen.SetResolution` internal call.
pub fn is_set_resolution(name: &str) -> bool {
    matches!(name, "UnityEngine.Screen::SetResolution_Injected" | "UnityEngine.Screen::SetResolution")
        || name.starts_with("UnityEngine.Screen::SetResolution(")
}

/// Pure helper: the `(width, height)` passed to `Screen.SetResolution` for a copy in a
/// pane. Replaces any positive size with the pane's exact virtual size so a fresh save's
/// smaller default matches the pane just like a larger saved resolution that the engine
/// already clamps down.
pub fn pane_resolution(w: i32, h: i32, pane: Option<(u32, u32)>) -> (i32, i32) {
    if w <= 0 || h <= 0 {
        return (w, h);
    }
    let Some((pw, ph)) = pane.filter(|&(pw, ph)| pw > 0 && ph > 0) else {
        return (w, h);
    };
    (pw.min(i32::MAX as u32) as i32, ph.min(i32::MAX as u32) as i32)
}

/// Called from `sys::swap`'s `GetProcAddress` detours: in a copy with a pane, wraps
/// Unity's `il2cpp_add_internal_call` / `mono_add_internal_call` lookups so
/// `Screen.SetResolution` receives the pane's size.
pub fn swap_lookup(name: *const u8, addr: usize) -> usize {
    if addr == 0 || (name as usize) <= 0xffff || crate::window::virtual_size().is_none() {
        return addr;
    }
    let bytes = unsafe { std::ffi::CStr::from_ptr(name.cast()) }.to_bytes();
    let (slot, wrapper, label): (&AtomicUsize, AddInternalCall, &str) = match bytes {
        b"il2cpp_add_internal_call" => (&REAL_ADD_IL2CPP, add_il2cpp, "il2cpp_add_internal_call"),
        b"mono_add_internal_call" => (&REAL_ADD_MONO, add_mono, "mono_add_internal_call"),
        _ => return addr,
    };
    let prev = slot.swap(addr, Ordering::Release);
    if prev == 0 {
        log!("window: the engine looks up {label}: Screen.SetResolution will be the pane's size");
    }
    wrapper as usize
}

unsafe fn register_call(real: usize, name: *const u8, mut f: usize) {
    if real == 0 {
        return;
    }
    if !name.is_null() && f != 0 {
        if let Ok(n) = unsafe { std::ffi::CStr::from_ptr(name.cast()) }.to_str()
            && is_set_resolution(n)
        {
            let prev = REAL_SET_RESOLUTION.swap(f, Ordering::Release);
            if prev == 0 {
                log!("window: the engine registers {n}: the size the game asks for is its pane's");
            }
            f = set_resolution as SetResolution as usize;
        }
    }
    let real: AddInternalCall = unsafe { std::mem::transmute(real) };
    unsafe { real(name, f) };
}

unsafe extern "C" fn add_il2cpp(name: *const u8, f: usize) {
    unsafe { register_call(REAL_ADD_IL2CPP.load(Ordering::Acquire), name, f) };
}

unsafe extern "C" fn add_mono(name: *const u8, f: usize) {
    unsafe { register_call(REAL_ADD_MONO.load(Ordering::Acquire), name, f) };
}

unsafe extern "C" fn set_resolution(w: i32, h: i32, mode: usize, rate: usize) {
    let real = REAL_SET_RESOLUTION.load(Ordering::Acquire);
    if real == 0 {
        return;
    }
    let (rw, rh) = pane_resolution(w, h, crate::window::virtual_size());
    if (rw, rh) != (w, h) {
        let m = mode as u32;
        crate::log::throttled("window: set resolution", 3, |n| {
            log!("window: the game asks for {w}x{h} (mode {m}): its pane's {rw}x{rh} instead (call {n})");
        });
    }
    let real: SetResolution = unsafe { std::mem::transmute(real) };
    unsafe { real(rw, rh, mode, rate) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engines_screen_values_are_told_apart() {
        assert_eq!(screen_value("Screenmanager Resolution Width_h182942802"), Some(Screen::Width));
        assert_eq!(screen_value("Screenmanager Resolution Height_h2627697771"), Some(Screen::Height));
        assert_eq!(screen_value("Screenmanager Resolution Window Width_h2524650974"), Some(Screen::Width), "the windowed size");
        assert_eq!(screen_value("Screenmanager Resolution Window Height_h1684712807"), Some(Screen::Height));
        assert_eq!(screen_value("Screenmanager Resolution Use Native_h1405027254"), Some(Screen::UseNative));
        assert_eq!(screen_value("Screenmanager Resolution Width Default_h680557497"), Some(Screen::Default), "the build's default");
        assert_eq!(screen_value("Screenmanager Resolution Use Native Default_h1405981789"), Some(Screen::Default));
        assert_eq!(screen_value("Screenmanager Fullscreen mode_h3630240806"), Some(Screen::FullscreenMode));
        assert_eq!(screen_value("Screenmanager Is Fullscreen mode_h3981298716"), Some(Screen::IsFullscreen));
        assert_eq!(screen_value("Screenmanager Fullscreen mode Default_h401710285"), Some(Screen::Default), "the build's default");
        assert_eq!(screen_value("Screenmanager Window Position X_h4088080503"), Some(Screen::Other));
        assert_eq!(screen_value("VSyncSetting_h1635279426"), None, "the game's own");
    }

    #[test]
    fn missing_screen_values_answer_probes_and_reads() {
        const NOT_FOUND: i32 = 2;
        const MORE_DATA: i32 = 234;
        let pane = Some((3440, 720));

        // HasKey-style existence probe: both data and len are null.
        assert_eq!(
            missing_with(Some("Screenmanager Resolution Window Width_h2524650974".into()), pane, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), NOT_FOUND),
            ERROR_SUCCESS
        );
        assert_eq!(
            missing_with(Some("Screenmanager Resolution Use Native_h1405027254".into()), pane, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), NOT_FOUND),
            ERROR_SUCCESS
        );
        assert_eq!(
            missing_with(Some("Screenmanager Window Position X_h4088080503".into()), pane, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), NOT_FOUND),
            NOT_FOUND,
            "unhandled screen values stay missing"
        );

        // Size query: data null, len non-null.
        let (mut kind, mut len) = (0u32, 0u32);
        assert_eq!(
            missing_with(Some("Screenmanager Resolution Width_h182942802".into()), pane, &mut kind, std::ptr::null_mut(), &mut len, NOT_FOUND),
            ERROR_SUCCESS
        );
        assert_eq!((kind, len), (REG_DWORD, 4));

        // Buffer too small returns ERROR_MORE_DATA and reports needed size.
        let mut small = [0u8; 2];
        let mut small_len = 2u32;
        assert_eq!(
            missing_with(Some("Screenmanager Resolution Height_h2627697771".into()), pane, std::ptr::null_mut(), small.as_mut_ptr(), &mut small_len, NOT_FOUND),
            MORE_DATA
        );
        assert_eq!(small_len, 4);

        // Full value reads answer width, height, use-native=0, fullscreen=3.
        for (name, expected) in [
            ("Screenmanager Resolution Window Width_h2524650974", 3440u32),
            ("Screenmanager Resolution Window Height_h1684712807", 720u32),
            ("Screenmanager Resolution Use Native_h1405027254", 0u32),
            ("Screenmanager Fullscreen mode_h3630240806", 3u32),
        ] {
            let (mut kind, mut val, mut len) = (0u32, u32::MAX, 4u32);
            assert_eq!(
                missing_with(Some(name.into()), pane, &mut kind, (&mut val as *mut u32).cast(), &mut len, NOT_FOUND),
                ERROR_SUCCESS,
                "{name}"
            );
            assert_eq!((kind, val, len), (REG_DWORD, expected, 4), "{name}");
        }
    }

    #[test]
    fn the_engines_set_resolution_is_found_and_sized_to_the_pane() {
        assert!(is_set_resolution("UnityEngine.Screen::SetResolution_Injected"));
        assert!(is_set_resolution("UnityEngine.Screen::SetResolution"));
        assert!(is_set_resolution("UnityEngine.Screen::SetResolution(int,int,bool,int)"));
        assert!(!is_set_resolution("UnityEngine.Screen::set_fullScreenMode"));
        assert!(!is_set_resolution("UnityEngine.Screen::SetResolutionX"));

        assert_eq!(pane_resolution(1280, 720, Some((3440, 720))), (3440, 720), "fresh save's smaller default promoted to pane");
        assert_eq!(pane_resolution(3840, 2160, Some((3440, 720))), (3440, 720), "larger saved size matches pane");
        assert_eq!(pane_resolution(1280, 720, Some((1720, 1440))), (1720, 1440), "tall side-by-side pane");
        assert_eq!(pane_resolution(1280, 720, Some((1152, 720))), (1152, 720), "custom ratio pane");
        assert_eq!(pane_resolution(1280, 720, None), (1280, 720), "no pane");
        assert_eq!(pane_resolution(0, 0, Some((3440, 720))), (0, 0), "invalid size untouched");
    }

    #[test]
    fn only_set_resolution_is_registered_through_the_wrapper() {
        static SEEN: std::sync::Mutex<Vec<(String, usize)>> = std::sync::Mutex::new(Vec::new());
        static ASKED: std::sync::Mutex<Vec<(i32, i32, usize, usize)>> = std::sync::Mutex::new(Vec::new());
        unsafe extern "C" fn runtime_add(name: *const u8, f: usize) {
            let n = unsafe { std::ffi::CStr::from_ptr(name.cast()) }.to_string_lossy().into_owned();
            SEEN.lock().unwrap().push((n, f));
        }
        unsafe extern "C" fn engine_set_resolution(w: i32, h: i32, mode: usize, rate: usize) {
            ASKED.lock().unwrap().push((w, h, mode, rate));
        }
        REAL_ADD_IL2CPP.store(runtime_add as AddInternalCall as usize, Ordering::Release);
        let engine = engine_set_resolution as SetResolution as usize;
        unsafe {
            add_il2cpp(c"UnityEngine.Screen::get_width".as_ptr().cast(), 0x1234);
            add_il2cpp(c"UnityEngine.Screen::SetResolution_Injected".as_ptr().cast(), engine);
        }
        let seen = SEEN.lock().unwrap().clone();
        assert_eq!(seen[0], ("UnityEngine.Screen::get_width".into(), 0x1234));
        assert_eq!(seen[1].1, set_resolution as SetResolution as usize);
        // No pane active in tests: passes size and mode straight through.
        unsafe { set_resolution(1280, 720, 3, 0xdead) };
        assert_eq!(ASKED.lock().unwrap().as_slice(), &[(1280, 720, 3, 0xdead)]);
    }
}
