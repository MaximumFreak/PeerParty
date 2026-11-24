//! A later player's registry: a game's keys under `HKCU\Software\<vendor>` (Unity's
//! PlayerPrefs, `HKCU\Software\<company>\<product>`, and any game's own settings and
//! progress there) are the player's own, under
//! `HKCU\Software\PeerParty\Saves\<game>\p<N>\<vendor>\...` (protocol's `registry_key`).
//! Windows', drivers' and stores' keys (`game_registry_key`) stay shared.
//!
//! The calls that name a key by path are hooked in kernelbase (advapi32's forward
//! there; advapi32's own older ones too): opening, creating and deleting keys, and the
//! ones that read, write or delete a value by subkey. A key opened relative to a
//! redirected one is the player's already; one opened relative to a handle to
//! `HKCU\Software` itself is followed (`SOFTWARE`).
//!
//! In player 1's copy with watching on, a game key opened for writing is noted in
//! `discovered.txt`, for the launcher to copy it to other players.
//!
//! A key the player has none of, opened only to read, and that player 1's game was never
//! seen writing (one its installer wrote: where the game is, its version) is read as it
//! is (`read_through`). A subkey of HKCU some other way (`HKEY_USERS\<sid>`,
//! `RegOpenCurrentUser`) isn't followed.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use peerparty_protocol::saves::{self as layout, Discovered};
use retour::GenericDetour;

use super::vfs;
use crate::log;
use crate::sys::pe::Export;

/// `HKEY_CURRENT_USER`, as a handle value (sign-extended, as Windows defines it).
const HKCU: isize = 0x8000_0001u32 as i32 as isize;
/// Access that writes: set value, create subkey (KEY_WRITE, KEY_ALL_ACCESS), delete.
const WRITE_ACCESS: u32 = 0x2 | 0x4 | 0x1_0000 | 0x0200_0000 | 0x1000_0000 | 0x4000_0000;

/// Handles the game holds to `HKCU\Software` itself: a key opened under one is a path
/// under `Software`.
static SOFTWARE: LazyLock<Mutex<HashSet<isize>>> = LazyLock::new(Mutex::default);
static TOLD: AtomicBool = AtomicBool::new(false);

thread_local! {
    static BUSY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The key a call names (`key` and `sub`), as the player's own: (the key to call with,
/// the subkey), or None to pass the game's. `write` notes it for player 1.
pub fn route(key: isize, sub: &str, write: bool) -> Option<(isize, String)> {
    let st = vfs::state()?;
    let path = if key == HKCU {
        sub.to_string()
    } else if crate::sys::lock(&SOFTWARE).contains(&key) {
        format!("Software\\{}", sub.trim_start_matches('\\'))
    } else {
        return None;
    };
    let rest = layout::game_registry_key(&path)?;
    if st.slot == 0 {
        if write {
            let comps: Vec<&str> = rest.split('\\').filter(|c| !c.is_empty()).take(2).collect();
            vfs::note(&st, Discovered::Registry(format!("Software\\{}", comps.join("\\"))));
        }
        return None;
    }
    let to = format!("{}\\{}", layout::registry_key(&st.game, st.slot), rest);
    if !TOLD.swap(true, Ordering::Relaxed) {
        log!("saves: player {}'s registry keys are its own: HKCU\\Software\\{rest} is HKCU\\{to}", st.slot + 1);
    }
    Some((HKCU, to))
}

fn is_software(key: isize, sub: &str) -> bool {
    key == HKCU && sub.trim_matches('\\').eq_ignore_ascii_case("software")
}

fn read_w(p: *const u16) -> Option<String> {
    if p.is_null() { None } else { crate::sys::read::wstr(p as usize) }
}

/// The routed subkey for a wide call: (key, NUL-terminated subkey), or None.
fn own_w(key: isize, sub: *const u16, write: bool) -> Option<(isize, Vec<u16>)> {
    if BUSY.with(|b| b.get()) || vfs::state().is_none() {
        return None;
    }
    BUSY.with(|b| b.set(true));
    let r = std::panic::catch_unwind(|| route(key, &read_w(sub).unwrap_or_default(), write)).ok().flatten();
    BUSY.with(|b| b.set(false));
    r.map(|(k, s)| (k, s.encode_utf16().chain(Some(0)).collect()))
}

fn own_a(key: isize, sub: *const u8, write: bool) -> Option<(isize, Vec<u8>)> {
    use windows_sys::Win32::Globalization::{CP_ACP, WideCharToMultiByte};
    if BUSY.with(|b| b.get()) || vfs::state().is_none() {
        return None;
    }
    BUSY.with(|b| b.set(true));
    let r = std::panic::catch_unwind(|| route(key, &(if sub.is_null() { None } else { super::ansi(sub) }).unwrap_or_default(), write)).ok().flatten();
    BUSY.with(|b| b.set(false));
    let (k, s) = r?;
    let w: Vec<u16> = s.encode_utf16().collect();
    let n = unsafe { WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, std::ptr::null_mut(), 0, std::ptr::null(), std::ptr::null_mut()) };
    if n <= 0 {
        return None;
    }
    let mut out = vec![0u8; n as usize + 1];
    unsafe { WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, out.as_mut_ptr(), n, std::ptr::null(), std::ptr::null_mut()) };
    Some((k, out))
}

/// Rebinds `$key` and `$sub` to the player's own (kept alive in `$keep`).
macro_rules! own {
    ($f:ident, $key:ident, $sub:ident, $write:expr, $keep:ident) => {
        let $keep = $f($key, $sub, $write);
        let ($key, $sub) = match &$keep {
            Some((k, v)) => (*k, v.as_ptr()),
            None => ($key, $sub),
        };
    };
}

/// After a successful open: a handle to `HKCU\Software` itself is followed.
fn opened_w(key: isize, sub: *const u16, out: *mut isize, rc: i32) {
    if rc == 0 && !out.is_null() && read_w(sub).is_some_and(|s| is_software(key, &s)) {
        crate::sys::lock(&SOFTWARE).insert(unsafe { *out });
    }
}

fn opened_a(key: isize, sub: *const u8, out: *mut isize, rc: i32) {
    if rc == 0 && !out.is_null() && !sub.is_null() && super::ansi(sub).is_some_and(|s| is_software(key, &s)) {
        crate::sys::lock(&SOFTWARE).insert(unsafe { *out });
    }
}

type OpenExW = unsafe extern "system" fn(isize, *const u16, u32, u32, *mut isize) -> i32;
type OpenExA = unsafe extern "system" fn(isize, *const u8, u32, u32, *mut isize) -> i32;
type CreateExW = unsafe extern "system" fn(isize, *const u16, u32, *mut u16, u32, u32, usize, *mut isize, *mut u32) -> i32;
type CreateExA = unsafe extern "system" fn(isize, *const u8, u32, *mut u8, u32, u32, usize, *mut isize, *mut u32) -> i32;
type OpenW = unsafe extern "system" fn(isize, *const u16, *mut isize) -> i32;
type OpenA = unsafe extern "system" fn(isize, *const u8, *mut isize) -> i32;
type DeleteExW = unsafe extern "system" fn(isize, *const u16, u32, u32) -> i32;
type DeleteExA = unsafe extern "system" fn(isize, *const u8, u32, u32) -> i32;
type DeleteW = unsafe extern "system" fn(isize, *const u16) -> i32;
type DeleteA = unsafe extern "system" fn(isize, *const u8) -> i32;
type GetValueW = unsafe extern "system" fn(isize, *const u16, *const u16, u32, *mut u32, *mut u8, *mut u32) -> i32;
type GetValueA = unsafe extern "system" fn(isize, *const u8, *const u8, u32, *mut u32, *mut u8, *mut u32) -> i32;
type SetKeyValueW = unsafe extern "system" fn(isize, *const u16, *const u16, u32, *const u8, u32) -> i32;
type SetKeyValueA = unsafe extern "system" fn(isize, *const u8, *const u8, u32, *const u8, u32) -> i32;
type DeleteKeyValueW = unsafe extern "system" fn(isize, *const u16, *const u16) -> i32;
type DeleteKeyValueA = unsafe extern "system" fn(isize, *const u8, *const u8) -> i32;
type Close = unsafe extern "system" fn(isize) -> i32;

static OPEN_EX_W: OnceLock<GenericDetour<OpenExW>> = OnceLock::new();
static OPEN_EX_A: OnceLock<GenericDetour<OpenExA>> = OnceLock::new();
static CREATE_EX_W: OnceLock<GenericDetour<CreateExW>> = OnceLock::new();
static CREATE_EX_A: OnceLock<GenericDetour<CreateExA>> = OnceLock::new();
static OPEN_W: OnceLock<GenericDetour<OpenW>> = OnceLock::new();
static OPEN_A: OnceLock<GenericDetour<OpenA>> = OnceLock::new();
static CREATE_W: OnceLock<GenericDetour<OpenW>> = OnceLock::new();
static CREATE_A: OnceLock<GenericDetour<OpenA>> = OnceLock::new();
static DELETE_EX_W: OnceLock<GenericDetour<DeleteExW>> = OnceLock::new();
static DELETE_EX_A: OnceLock<GenericDetour<DeleteExA>> = OnceLock::new();
static DELETE_W: OnceLock<GenericDetour<DeleteW>> = OnceLock::new();
static DELETE_A: OnceLock<GenericDetour<DeleteA>> = OnceLock::new();
static DELETE_TREE_W: OnceLock<GenericDetour<DeleteW>> = OnceLock::new();
static DELETE_TREE_A: OnceLock<GenericDetour<DeleteA>> = OnceLock::new();
static GET_VALUE_W: OnceLock<GenericDetour<GetValueW>> = OnceLock::new();
static GET_VALUE_A: OnceLock<GenericDetour<GetValueA>> = OnceLock::new();
static SET_KEY_VALUE_W: OnceLock<GenericDetour<SetKeyValueW>> = OnceLock::new();
static SET_KEY_VALUE_A: OnceLock<GenericDetour<SetKeyValueA>> = OnceLock::new();
static DELETE_KEY_VALUE_W: OnceLock<GenericDetour<DeleteKeyValueW>> = OnceLock::new();
static DELETE_KEY_VALUE_A: OnceLock<GenericDetour<DeleteKeyValueA>> = OnceLock::new();
static CLOSE: OnceLock<GenericDetour<Close>> = OnceLock::new();

static ARMED: AtomicBool = AtomicBool::new(false);

fn arm<T>(lists: &[&[Export]], name: &str, slot: &OnceLock<GenericDetour<T>>, detour: T) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    if slot.get().is_some() {
        return 0;
    }
    let Some(e) = lists.iter().flat_map(|l| l.iter()).find(|e| e.name == name) else { return 0 };
    let target: T = unsafe { T::from_ptr(e.addr) };
    usize::from(crate::sys::detour::try_arm(slot, target, detour, "saves", name, true) == crate::sys::detour::Armed::Hooked)
}

fn exports_of(dll: &str) -> Vec<Export> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let name: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
    let base = unsafe { GetModuleHandleW(name.as_ptr()) };
    if base.is_null() {
        return Vec::new();
    }
    unsafe { crate::sys::pe::exports(base.cast()) }.map(|t| t.code).unwrap_or_default()
}

/// Hooks the registry's key calls (once), in a later player's copy or player 1's when
/// watching.
pub fn attach_kernelbase(kb: &[Export]) -> usize {
    if vfs::state().is_none() || ARMED.swap(true, Ordering::SeqCst) {
        return 0;
    }
    let adv = exports_of("advapi32.dll");
    let l: &[&[Export]] = &[kb, &adv];
    let mut n = arm(l, "RegOpenKeyExW", &OPEN_EX_W, open_ex_w as OpenExW)
        + arm(l, "RegOpenKeyExA", &OPEN_EX_A, open_ex_a as OpenExA)
        + arm(l, "RegCreateKeyExW", &CREATE_EX_W, create_ex_w as CreateExW)
        + arm(l, "RegCreateKeyExA", &CREATE_EX_A, create_ex_a as CreateExA)
        + arm(l, "RegOpenKeyW", &OPEN_W, open_w as OpenW)
        + arm(l, "RegOpenKeyA", &OPEN_A, open_a as OpenA)
        + arm(l, "RegCreateKeyW", &CREATE_W, create_w as OpenW)
        + arm(l, "RegCreateKeyA", &CREATE_A, create_a as OpenA)
        + arm(l, "RegDeleteKeyExW", &DELETE_EX_W, delete_ex_w as DeleteExW)
        + arm(l, "RegDeleteKeyExA", &DELETE_EX_A, delete_ex_a as DeleteExA)
        + arm(l, "RegDeleteKeyW", &DELETE_W, delete_w as DeleteW)
        + arm(l, "RegDeleteKeyA", &DELETE_A, delete_a as DeleteA)
        + arm(l, "RegDeleteTreeW", &DELETE_TREE_W, delete_tree_w as DeleteW)
        + arm(l, "RegDeleteTreeA", &DELETE_TREE_A, delete_tree_a as DeleteA)
        + arm(l, "RegSetKeyValueW", &SET_KEY_VALUE_W, set_key_value_w as SetKeyValueW)
        + arm(l, "RegSetKeyValueA", &SET_KEY_VALUE_A, set_key_value_a as SetKeyValueA)
        + arm(l, "RegDeleteKeyValueW", &DELETE_KEY_VALUE_W, delete_key_value_w as DeleteKeyValueW)
        + arm(l, "RegDeleteKeyValueA", &DELETE_KEY_VALUE_A, delete_key_value_a as DeleteKeyValueA)
        + arm(l, "RegCloseKey", &CLOSE, close as Close)
        + arm(l, "RegGetValueA", &GET_VALUE_A, get_value_a as GetValueA);
    // A pane's copy has RegGetValueW hooked already for its screen settings, which then
    // asks `subkey_w` here (screen_prefs.rs): one detour on it, not two.
    if !crate::screen_prefs::get_value_hooked() {
        n += arm(l, "RegGetValueW", &GET_VALUE_W, get_value_w as GetValueW);
    }
    n
}

/// Arms the registry hooks now (a copy that learnt its player from the LAN).
pub fn arm_now() {
    if ARMED.load(Ordering::SeqCst) {
        return;
    }
    let kb = exports_of("kernelbase.dll");
    let n = attach_kernelbase(&kb);
    log!("saves registry hooks {n}");
}

/// For RegGetValueW's other detour (screen_prefs.rs): the routed key and subkey.
pub fn subkey_w(key: isize, sub: *const u16) -> Option<(isize, Vec<u16>)> {
    if sub.is_null() { None } else { own_w(key, sub, false) }
}

/// A read-only open of a key the later player has none of, and that player 1's game was
/// never seen writing (an installer's: the game's folder, its version): the real key is
/// read. A key the game writes (its settings, PlayerPrefs, progress) is the player's own
/// or nothing, and a handle opened to write is always the player's.
fn read_through(key: isize, sub: &str, rc: i32) -> bool {
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    if rc != ERROR_FILE_NOT_FOUND {
        return false;
    }
    let path = if key == HKCU { sub.to_string() } else { format!("Software\\{}", sub.trim_start_matches('\\')) };
    let Some(rest) = layout::game_registry_key(&path) else { return false };
    if vfs::written_by_first(&path) {
        return false;
    }
    let root: Vec<&str> = rest.split('\\').filter(|c| !c.is_empty()).take(2).collect();
    static TOLD: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    let root = root.join("\\");
    let mut told = crate::sys::lock(&TOLD);
    if told.len() < 32 && told.insert(root.to_lowercase()) {
        log!("saves: HKCU\\Software\\{root} is read as it is (only read, none of the player's own, and player 1's game was never seen writing it)");
    }
    true
}

unsafe extern "system" fn open_ex_w(key: isize, sub: *const u16, options: u32, sam: u32, out: *mut isize) -> i32 {
    let (k0, s0) = (key, sub);
    let write = sam & WRITE_ACCESS != 0;
    own!(own_w, key, sub, write, keep);
    let mut rc = unsafe { OPEN_EX_W.get().unwrap().call(key, sub, options, sam, out) };
    if keep.is_some() && !write && read_through(k0, &read_w(s0).unwrap_or_default(), rc) {
        rc = unsafe { OPEN_EX_W.get().unwrap().call(k0, s0, options, sam, out) };
    }
    opened_w(k0, s0, out, rc);
    rc
}

unsafe extern "system" fn open_ex_a(key: isize, sub: *const u8, options: u32, sam: u32, out: *mut isize) -> i32 {
    let (k0, s0) = (key, sub);
    let write = sam & WRITE_ACCESS != 0;
    own!(own_a, key, sub, write, keep);
    let mut rc = unsafe { OPEN_EX_A.get().unwrap().call(key, sub, options, sam, out) };
    if keep.is_some() && !write && !s0.is_null() && read_through(k0, &super::ansi(s0).unwrap_or_default(), rc) {
        rc = unsafe { OPEN_EX_A.get().unwrap().call(k0, s0, options, sam, out) };
    }
    opened_a(k0, s0, out, rc);
    rc
}

unsafe extern "system" fn create_ex_w(key: isize, sub: *const u16, reserved: u32, class: *mut u16, options: u32, sam: u32, sa: usize, out: *mut isize, disposition: *mut u32) -> i32 {
    let (k0, s0) = (key, sub);
    own!(own_w, key, sub, true, keep);
    let rc = unsafe { CREATE_EX_W.get().unwrap().call(key, sub, reserved, class, options, sam, sa, out, disposition) };
    opened_w(k0, s0, out, rc);
    rc
}

unsafe extern "system" fn create_ex_a(key: isize, sub: *const u8, reserved: u32, class: *mut u8, options: u32, sam: u32, sa: usize, out: *mut isize, disposition: *mut u32) -> i32 {
    let (k0, s0) = (key, sub);
    own!(own_a, key, sub, true, keep);
    let rc = unsafe { CREATE_EX_A.get().unwrap().call(key, sub, reserved, class, options, sam, sa, out, disposition) };
    opened_a(k0, s0, out, rc);
    rc
}

unsafe extern "system" fn open_w(key: isize, sub: *const u16, out: *mut isize) -> i32 {
    let (k0, s0) = (key, sub);
    own!(own_w, key, sub, true, keep);
    let rc = unsafe { OPEN_W.get().unwrap().call(key, sub, out) };
    opened_w(k0, s0, out, rc);
    rc
}

unsafe extern "system" fn open_a(key: isize, sub: *const u8, out: *mut isize) -> i32 {
    let (k0, s0) = (key, sub);
    own!(own_a, key, sub, true, keep);
    let rc = unsafe { OPEN_A.get().unwrap().call(key, sub, out) };
    opened_a(k0, s0, out, rc);
    rc
}

unsafe extern "system" fn create_w(key: isize, sub: *const u16, out: *mut isize) -> i32 {
    let (k0, s0) = (key, sub);
    own!(own_w, key, sub, true, keep);
    let rc = unsafe { CREATE_W.get().unwrap().call(key, sub, out) };
    opened_w(k0, s0, out, rc);
    rc
}

unsafe extern "system" fn create_a(key: isize, sub: *const u8, out: *mut isize) -> i32 {
    let (k0, s0) = (key, sub);
    own!(own_a, key, sub, true, keep);
    let rc = unsafe { CREATE_A.get().unwrap().call(key, sub, out) };
    opened_a(k0, s0, out, rc);
    rc
}

unsafe extern "system" fn delete_ex_w(key: isize, sub: *const u16, sam: u32, reserved: u32) -> i32 {
    own!(own_w, key, sub, true, keep);
    unsafe { DELETE_EX_W.get().unwrap().call(key, sub, sam, reserved) }
}

unsafe extern "system" fn delete_ex_a(key: isize, sub: *const u8, sam: u32, reserved: u32) -> i32 {
    own!(own_a, key, sub, true, keep);
    unsafe { DELETE_EX_A.get().unwrap().call(key, sub, sam, reserved) }
}

unsafe extern "system" fn delete_w(key: isize, sub: *const u16) -> i32 {
    own!(own_w, key, sub, true, keep);
    unsafe { DELETE_W.get().unwrap().call(key, sub) }
}

unsafe extern "system" fn delete_a(key: isize, sub: *const u8) -> i32 {
    own!(own_a, key, sub, true, keep);
    unsafe { DELETE_A.get().unwrap().call(key, sub) }
}

unsafe extern "system" fn delete_tree_w(key: isize, sub: *const u16) -> i32 {
    // No subkey: the key itself, which a routed open made the player's already.
    if sub.is_null() {
        return unsafe { DELETE_TREE_W.get().unwrap().call(key, sub) };
    }
    own!(own_w, key, sub, true, keep);
    unsafe { DELETE_TREE_W.get().unwrap().call(key, sub) }
}

unsafe extern "system" fn delete_tree_a(key: isize, sub: *const u8) -> i32 {
    if sub.is_null() {
        return unsafe { DELETE_TREE_A.get().unwrap().call(key, sub) };
    }
    own!(own_a, key, sub, true, keep);
    unsafe { DELETE_TREE_A.get().unwrap().call(key, sub) }
}

unsafe extern "system" fn get_value_w(key: isize, sub: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    if sub.is_null() {
        return unsafe { GET_VALUE_W.get().unwrap().call(key, sub, value, flags, kind, data, len) };
    }
    let (k0, s0) = (key, sub);
    own!(own_w, key, sub, false, keep);
    let rc = unsafe { GET_VALUE_W.get().unwrap().call(key, sub, value, flags, kind, data, len) };
    if keep.is_some() && read_through(k0, &read_w(s0).unwrap_or_default(), rc) {
        return unsafe { GET_VALUE_W.get().unwrap().call(k0, s0, value, flags, kind, data, len) };
    }
    rc
}

unsafe extern "system" fn get_value_a(key: isize, sub: *const u8, value: *const u8, flags: u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32 {
    if sub.is_null() {
        return unsafe { GET_VALUE_A.get().unwrap().call(key, sub, value, flags, kind, data, len) };
    }
    let (k0, s0) = (key, sub);
    own!(own_a, key, sub, false, keep);
    let rc = unsafe { GET_VALUE_A.get().unwrap().call(key, sub, value, flags, kind, data, len) };
    if keep.is_some() && read_through(k0, &super::ansi(s0).unwrap_or_default(), rc) {
        return unsafe { GET_VALUE_A.get().unwrap().call(k0, s0, value, flags, kind, data, len) };
    }
    rc
}

unsafe extern "system" fn set_key_value_w(key: isize, sub: *const u16, value: *const u16, kind: u32, data: *const u8, len: u32) -> i32 {
    if sub.is_null() {
        return unsafe { SET_KEY_VALUE_W.get().unwrap().call(key, sub, value, kind, data, len) };
    }
    own!(own_w, key, sub, true, keep);
    unsafe { SET_KEY_VALUE_W.get().unwrap().call(key, sub, value, kind, data, len) }
}

unsafe extern "system" fn set_key_value_a(key: isize, sub: *const u8, value: *const u8, kind: u32, data: *const u8, len: u32) -> i32 {
    if sub.is_null() {
        return unsafe { SET_KEY_VALUE_A.get().unwrap().call(key, sub, value, kind, data, len) };
    }
    own!(own_a, key, sub, true, keep);
    unsafe { SET_KEY_VALUE_A.get().unwrap().call(key, sub, value, kind, data, len) }
}

unsafe extern "system" fn delete_key_value_w(key: isize, sub: *const u16, value: *const u16) -> i32 {
    if sub.is_null() {
        return unsafe { DELETE_KEY_VALUE_W.get().unwrap().call(key, sub, value) };
    }
    own!(own_w, key, sub, true, keep);
    unsafe { DELETE_KEY_VALUE_W.get().unwrap().call(key, sub, value) }
}

unsafe extern "system" fn delete_key_value_a(key: isize, sub: *const u8, value: *const u8) -> i32 {
    if sub.is_null() {
        return unsafe { DELETE_KEY_VALUE_A.get().unwrap().call(key, sub, value) };
    }
    own!(own_a, key, sub, true, keep);
    unsafe { DELETE_KEY_VALUE_A.get().unwrap().call(key, sub, value) }
}

unsafe extern "system" fn close(key: isize) -> i32 {
    // Before the handle's value can be reused.
    if key != 0 {
        crate::sys::lock(&SOFTWARE).remove(&key);
    }
    unsafe { CLOSE.get().unwrap().call(key) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A later player's game keys are under PeerParty's key for it, Windows' stay;
    /// player 1's are only noted.
    #[test]
    fn a_later_players_keys_are_its_own() {
        let _g = crate::sys::lock(&vfs::tests::TEST_LOCK);
        let base = std::env::temp_dir().join(format!("peerparty-reg-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        vfs::tests::install(vfs::tests::test_state(&base, 1, false));
        assert_eq!(route(HKCU, r"Software\PeerPartyTest\Game", false), Some((HKCU, r"Software\PeerParty\Saves\test-game-00000000\p2\PeerPartyTest\Game".into())));
        assert_eq!(route(HKCU, r"Software\Microsoft\Windows", true), None);
        assert_eq!(route(HKCU, r"Software", true), None);
        assert_eq!(route(0x1234, r"PeerPartyTest\Game", true), None, "a key not followed");
        crate::sys::lock(&SOFTWARE).insert(0x1234);
        assert_eq!(route(0x1234, r"PeerPartyTest\Game", true), Some((HKCU, r"Software\PeerParty\Saves\test-game-00000000\p2\PeerPartyTest\Game".into())), "under a handle to Software");
        crate::sys::lock(&SOFTWARE).remove(&0x1234);
        vfs::tests::install(vfs::tests::test_state(&base, 0, false));
        assert_eq!(route(HKCU, r"Software\PeerPartyTest\Game\Sub", true), None);
        let noted = std::fs::read_to_string(base.join(r"data\saves\test-game-00000000\player-1\discovered.txt")).unwrap();
        assert!(Discovered::parse_all(&noted).contains(&Discovered::Registry(r"Software\PeerPartyTest\Game".into())), "{noted}");
        vfs::tests::uninstall();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An installer's key, only read, reads through; one player 1's game writes doesn't.
    #[test]
    fn only_keys_the_game_never_writes_read_through() {
        let _g = crate::sys::lock(&vfs::tests::TEST_LOCK);
        let base = std::env::temp_dir().join(format!("peerparty-reg-through-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let st = vfs::tests::test_state(&base, 1, false);
        std::fs::create_dir_all(&st.first).unwrap();
        std::fs::write(st.first.join(layout::DISCOVERED), "registry\tSoftware\\PeerPartyTest\\Game\r\n").unwrap();
        vfs::tests::install(st);
        vfs::load_noted();
        assert!(read_through(HKCU, r"Software\Python\PythonCore\3.12\PythonPath", 2), "an installer's key");
        assert!(!read_through(HKCU, r"Software\PeerPartyTest\Game", 2), "the game writes it");
        assert!(!read_through(HKCU, r"Software\PeerPartyTest\Game\Sub", 2), "below a key the game writes");
        assert!(!read_through(HKCU, r"Software\Python\PythonCore", 0), "the player has it");
        assert!(!read_through(HKCU, r"Software\Microsoft\Windows", 2), "not a game's");
        vfs::tests::uninstall();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The real calls: player 2 writes `HKCU\Software\PeerPartyTest\Game`'s value; the
    /// real key is untouched and the value is in player 2's key.
    #[test]
    fn the_registry_calls_go_to_the_players_key() {
        use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, REG_DWORD, REG_OPTION_NON_VOLATILE, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, RRF_RT_REG_DWORD, KEY_READ};
        let _g = crate::sys::lock(&vfs::tests::TEST_LOCK);
        let base = std::env::temp_dir().join(format!("peerparty-reg-{}-{}", std::process::id(), std::mem::size_of::<usize>()));
        let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let mut st = vfs::tests::test_state(&base, 1, false);
        // A game id of this run's own: x64 and x86 tests may run at once.
        st.game = format!("test-game-{:08x}", std::process::id() ^ (std::mem::size_of::<usize>() as u32));
        let game = st.game.clone();
        let sub = format!(r"Software\PeerPartyTest{}\Game", std::process::id());
        let mine = format!(r"{}\PeerPartyTest{}\Game", layout::registry_key(&game, 1), std::process::id());
        vfs::tests::install(st);
        let kb = exports_of("kernelbase.dll");
        attach_kernelbase(&kb);
        let mut h: HKEY = std::ptr::null_mut();
        let rc = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, w(&sub).as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_ALL_ACCESS, std::ptr::null(), &mut h, std::ptr::null_mut()) };
        assert_eq!(rc, 0);
        let v: u32 = 2;
        assert_eq!(unsafe { RegSetValueExW(h, w("Progress").as_ptr(), 0, REG_DWORD, (&v as *const u32).cast(), 4) }, 0);
        unsafe { RegCloseKey(h) };
        vfs::tests::uninstall();
        // Seen as player 1 (nothing routed): the real key isn't there, player 2's holds it.
        let mut real: HKEY = std::ptr::null_mut();
        assert_ne!(unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, w(&sub).as_ptr(), 0, KEY_READ, &mut real) }, 0, "the real key untouched");
        let (mut got, mut len) = (0u32, 4u32);
        let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, w(&mine).as_ptr(), w("Progress").as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut got as *mut u32).cast(), &mut len) };
        assert_eq!((rc, got), (0, 2));
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, w(&layout::registry_key(&game, 1).rsplit_once('\\').unwrap().0.to_string()).as_ptr()) };
        let _ = std::fs::remove_dir_all(&base);
    }
}
