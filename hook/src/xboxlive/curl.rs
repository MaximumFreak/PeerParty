//! A trace of the game's Xbox Live calls through XCurl.dll, the GDK's libcurl
//! (libHttpClient sends XSAPI's calls through it), for working out what a game asks Xbox
//! Live. Only with `PEERPARTY_XBL_TRACE=1`: a game failed to load its first map with
//! these hooks (and winhttp.rs's) always on, so they stay off unless asked for.
//!
//! `curl_easy_setopt`, `curl_easy_reset` and `curl_easy_cleanup` are hooked: each
//! request to an `*.xboxlive.com` host is logged once it's done, with its full URL and
//! its request and response bodies (up to 4 KB each). libHttpClient streams the bodies
//! through the read and write callbacks it sets (CURLOPT_READFUNCTION / WRITEFUNCTION,
//! with itself as the data pointer; libHttpClient Source/HTTP/Curl/CurlEasyRequest.cpp),
//! so those are wrapped: once a handle has both a callback and its data pointer, curl is
//! given our wrapper and a slot of ours that names the game's pair. A path may name the
//! player making it (mod.rs `own_xuid_in`). Headers (the Xbox token) are never logged.
//!
//! Nothing a request does is changed.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;

// curl/curl.h option numbers: LONG = 0, OBJECTPOINT = 10000, FUNCTIONPOINT = 20000.
const OPT_WRITEDATA: u32 = 10001;
const OPT_URL: u32 = 10002;
const OPT_READDATA: u32 = 10009;
const OPT_CUSTOMREQUEST: u32 = 10036;
const OPT_UPLOAD: u32 = 46;
const OPT_POST: u32 = 47;
const OPT_NOBODY: u32 = 44;
const OPT_WRITEFUNCTION: u32 = 20011;
const OPT_READFUNCTION: u32 = 20012;

const BODY_MAX: usize = 4096;

/// `curl_easy_setopt(handle, option, ...)`: one argument after the option. It is taken
/// and passed on as two pointer-sized ones: a `curl_off_t` option (30000 and up) passes
/// 64 bits, two stack slots on x86, and the second must reach curl too. On x64 the
/// spare one is a register curl doesn't read.
type SetOpt = unsafe extern "C" fn(usize, u32, usize, usize) -> u32;
type Cleanup = unsafe extern "C" fn(usize);
/// `size_t (*)(char *buffer, size_t size, size_t nitems, void *userdata)`.
type DataFn = unsafe extern "C" fn(*mut u8, usize, usize, usize) -> usize;

static SETOPT: OnceLock<GenericDetour<SetOpt>> = OnceLock::new();
static CLEANUP: OnceLock<GenericDetour<Cleanup>> = OnceLock::new();
static RESET: OnceLock<GenericDetour<Cleanup>> = OnceLock::new();

#[derive(Default)]
struct Call {
    url: String,
    method: &'static str,
    sent: Vec<u8>,
    got: Vec<u8>,
}

/// One direction (read or write) of a handle's body callback: what the game set.
#[derive(Default, Clone, Copy)]
struct Side {
    func: usize,
    data: usize,
    data_set: bool,
}

impl Side {
    /// The game's callback is wrapped only once both it and its data pointer are known:
    /// the wrapper is passed a slot of ours as its data, and curl's own default callback
    /// (no function set) must never be handed that slot.
    fn wrapped(&self) -> bool {
        self.func != 0 && self.data_set
    }
}

/// What the wrappers get as their data pointer: one per handle address, never freed,
/// so a handle copied by `curl_easy_duphandle` (which copies our slot along) still
/// finds the callback the game set.
struct Slot {
    handle: usize,
    read: Mutex<Side>,
    write: Mutex<Side>,
}

#[derive(Default)]
struct Calls {
    by_handle: HashMap<usize, Call>,
    slots: HashMap<usize, &'static Slot>,
}

static CALLS: LazyLock<Mutex<Calls>> = LazyLock::new(Mutex::default);

/// Whether XCurl's calls are traced here (winhttp.rs then leaves them to this module).
pub fn active() -> bool {
    SETOPT.get().is_some()
}

pub fn bodies() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("PEERPARTY_XBL_TRACE").is_ok_and(|v| v == "1"))
}

/// XCurl.dll loaded (watch.rs). The number of functions hooked.
pub fn attach(exports: &[Export]) -> usize {
    let mut n = 0;
    if let Some(e) = exports.iter().find(|e| e.name == "curl_easy_setopt") {
        let target: SetOpt = unsafe { std::mem::transmute(e.addr) };
        n += usize::from(arm(&SETOPT, target, setopt as SetOpt, "curl_easy_setopt"));
    }
    if let Some(e) = exports.iter().find(|e| e.name == "curl_easy_cleanup") {
        let target: Cleanup = unsafe { std::mem::transmute(e.addr) };
        n += usize::from(arm(&CLEANUP, target, cleanup as Cleanup, "curl_easy_cleanup"));
    }
    if let Some(e) = exports.iter().find(|e| e.name == "curl_easy_reset") {
        let target: Cleanup = unsafe { std::mem::transmute(e.addr) };
        n += usize::from(arm(&RESET, target, reset as Cleanup, "curl_easy_reset"));
    }
    n
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let hook = match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => h,
            Err(e) => {
                log!("xboxlive {label} not hooked: {e}");
                return false;
            }
        };
        if slot.set(hook).is_err() {
            return false;
        }
        unsafe { slot.get().unwrap().enable() }.is_ok()
    })
}

/// The host and path of an `https://host/path?query` URL.
pub(crate) fn split_url(url: &str) -> (&str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    }
}

fn is_xbox(url: &str) -> bool {
    let host = split_url(url).0.split(':').next().unwrap_or_default().to_ascii_lowercase();
    host.ends_with(".xboxlive.com")
}

fn new_slot(h: usize) -> &'static Slot {
    Box::leak(Box::new(Slot { handle: h, read: Mutex::new(Side::default()), write: Mutex::new(Side::default()) }))
}

/// A body option set on a handle (`func`: its callback, else its data pointer): what
/// curl is given instead, and the other option of the pair to set too when this one
/// starts or ends the wrapping.
fn body_option(slot: &'static Slot, write: bool, func: bool, value: usize) -> (usize, Option<(u32, usize)>) {
    let (side, wrapper, func_opt, data_opt) = if write {
        (&slot.write, write_wrap as DataFn as usize, OPT_WRITEFUNCTION, OPT_WRITEDATA)
    } else {
        (&slot.read, read_wrap as DataFn as usize, OPT_READFUNCTION, OPT_READDATA)
    };
    let mut s = side.lock().unwrap();
    let was = s.wrapped();
    if func {
        s.func = value;
    } else {
        s.data = value;
        s.data_set = true;
    }
    let now = s.wrapped();
    let ours = slot as *const Slot as usize;
    let value = match (now, func) {
        (true, true) => wrapper,
        (true, false) => ours,
        (false, _) => value,
    };
    let other = match (was, now, func) {
        (false, true, true) => Some((data_opt, ours)),
        (false, true, false) => Some((func_opt, wrapper)),
        // The callback set back to curl's own: the game's data pointer goes back too.
        (true, false, true) => Some((data_opt, s.data)),
        _ => None,
    };
    (value, other)
}

unsafe extern "C" fn setopt(h: usize, option: u32, value: usize, high: usize) -> u32 {
    let Some(hook) = SETOPT.get() else { return 43 };
    let mut value = value;
    let mut also = None;
    {
        let mut calls = CALLS.lock().unwrap();
        let c = &mut *calls;
        match option {
            OPT_URL if value != 0 => {
                let url = crate::read::cstr(value).unwrap_or_default();
                let call = c.by_handle.entry(h).or_default();
                if call.method.is_empty() {
                    call.method = "GET";
                }
                call.url = url;
            }
            OPT_CUSTOMREQUEST if value != 0 => {
                let m = crate::read::cstr(value).unwrap_or_default().to_ascii_uppercase();
                c.by_handle.entry(h).or_default().method = match m.as_str() {
                    "DELETE" => "DELETE",
                    "PUT" => "PUT",
                    "PATCH" => "PATCH",
                    "POST" => "POST",
                    "HEAD" => "HEAD",
                    _ => "OTHER",
                };
            }
            OPT_POST if value != 0 => c.by_handle.entry(h).or_default().method = "POST",
            OPT_UPLOAD if value != 0 => c.by_handle.entry(h).or_default().method = "PUT",
            OPT_NOBODY if value != 0 => c.by_handle.entry(h).or_default().method = "HEAD",
            OPT_READFUNCTION | OPT_WRITEFUNCTION | OPT_READDATA | OPT_WRITEDATA => {
                let slot: &'static Slot = c.slots.entry(h).or_insert_with(|| new_slot(h));
                let write = matches!(option, OPT_WRITEFUNCTION | OPT_WRITEDATA);
                let func = matches!(option, OPT_READFUNCTION | OPT_WRITEFUNCTION);
                (value, also) = body_option(slot, write, func, value);
            }
            _ => {}
        }
    }
    if let Some((opt, v)) = also {
        unsafe { hook.call(h, opt, v, 0) };
    }
    unsafe { hook.call(h, option, value, high) }
}

/// The handle, and the game's callback and data, behind a wrapper's data pointer:
/// always one of our slots (the wrapper is only ever set together with one), which are
/// never freed.
fn original(data: usize, write: bool) -> Option<(usize, DataFn, usize)> {
    if data == 0 {
        return None;
    }
    let slot = unsafe { &*(data as *const Slot) };
    let s = *if write { &slot.write } else { &slot.read }.lock().unwrap();
    (s.func != 0).then(|| (slot.handle, unsafe { std::mem::transmute::<usize, DataFn>(s.func) }, s.data))
}

fn keep(h: usize, bytes: &[u8], write: bool) {
    let mut calls = CALLS.lock().unwrap();
    if let Some(call) = calls.by_handle.get_mut(&h) {
        let buf = if write { &mut call.got } else { &mut call.sent };
        let room = BODY_MAX.saturating_sub(buf.len());
        buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }
}

/// CURL_READFUNC_ABORT.
const READ_ABORT: usize = 0x1000_0000;

unsafe extern "C" fn read_wrap(buf: *mut u8, size: usize, n: usize, data: usize) -> usize {
    let Some((h, f, game_data)) = original(data, false) else { return READ_ABORT };
    let got = unsafe { f(buf, size, n, game_data) };
    // CURL_READFUNC_ABORT / PAUSE are large sentinels, not lengths.
    if got <= size.saturating_mul(n) && !buf.is_null() {
        keep(h, unsafe { std::slice::from_raw_parts(buf, got) }, false);
    }
    got
}

unsafe extern "C" fn write_wrap(buf: *mut u8, size: usize, n: usize, data: usize) -> usize {
    let Some((h, f, game_data)) = original(data, true) else { return 0 };
    let len = size.saturating_mul(n);
    if !buf.is_null() {
        keep(h, unsafe { std::slice::from_raw_parts(buf, len) }, true);
    }
    unsafe { f(buf, size, n, game_data) }
}

/// `curl_easy_reset`: every option back to curl's default, so nothing is wrapped.
unsafe extern "C" fn reset(h: usize) {
    {
        let mut calls = CALLS.lock().unwrap();
        if let Some(slot) = calls.slots.get(&h) {
            *slot.read.lock().unwrap() = Side::default();
            *slot.write.lock().unwrap() = Side::default();
        }
        calls.by_handle.remove(&h);
    }
    if let Some(hook) = RESET.get() {
        unsafe { hook.call(h) }
    }
}

fn text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes).replace(['\r', '\n'], " ");
    if bytes.len() >= BODY_MAX { format!("{s}...") } else { s }
}

unsafe extern "C" fn cleanup(h: usize) {
    let call = {
        // The handle's slot stays as it is: a copy made of this handle may still use it.
        let mut calls = CALLS.lock().unwrap();
        calls.by_handle.remove(&h)
    };
    if let Some(call) = call.filter(|c| is_xbox(&c.url)) {
        let (host, path) = split_url(&call.url);
        if let Some(x) = super::own_xuid_in(host, path) {
            super::learn(x);
        }
        if bodies() {
            log!("xbl-trace {} {} sent {} got {}", call.method, call.url, text(&call.sent), text(&call.got));
        } else {
            let shape = super::shape(path);
            crate::log::throttled(&format!("xbl-curl {} {host}{shape}", call.method), 2, |n| {
                log!("xbl-curl {} https://{host}{shape} n={n}", call.method);
            });
        }
    }
    if let Some(hook) = CLEANUP.get() {
        unsafe { hook.call(h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_urls() {
        assert_eq!(split_url("https://social.xboxlive.com/users/xuid(1)/people?view=All"), ("social.xboxlive.com", "/users/xuid(1)/people?view=All"));
        assert_eq!(split_url("https://rta.xboxlive.com"), ("rta.xboxlive.com", "/"));
        assert!(is_xbox("https://sessiondirectory.xboxlive.com/handles/query"));
        assert!(!is_xbox("https://telemetry.example.net/x"));
        assert!(!is_xbox("https://xboxlive.com.example.org/"));
    }

    #[test]
    fn a_callback_is_wrapped_once_its_data_is_known_in_either_order() {
        let ours = |s: &'static Slot| s as *const Slot as usize;
        let wrapper = write_wrap as DataFn as usize;
        // Callback first: passed on as it is until the data comes, then both swap.
        let s = new_slot(1);
        assert_eq!(body_option(s, true, true, 0x1111), (0x1111, None));
        assert_eq!(body_option(s, true, false, 0x2222), (ours(s), Some((OPT_WRITEFUNCTION, wrapper))));
        assert_eq!(original(ours(s), true).map(|(h, f, d)| (h, f as usize, d)), Some((1, 0x1111, 0x2222)));
        // Data first.
        let s = new_slot(2);
        assert_eq!(body_option(s, true, false, 0x2222), (0x2222, None));
        assert_eq!(body_option(s, true, true, 0x1111), (wrapper, Some((OPT_WRITEDATA, ours(s)))));
        // Back to curl's own callback: the game's data pointer goes back too.
        assert_eq!(body_option(s, true, true, 0), (0, Some((OPT_WRITEDATA, 0x2222))));
        // Data alone never becomes the slot (curl's default callback would write to it).
        let s = new_slot(3);
        assert_eq!(body_option(s, false, false, 0x3333), (0x3333, None));
        assert!(original(ours(s), false).is_none());
    }
}
