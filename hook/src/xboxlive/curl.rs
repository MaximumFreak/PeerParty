//! A trace of the game's Xbox Live calls through XCurl.dll, the GDK's libcurl
//! (libHttpClient sends XSAPI's calls through it), for working out what a game asks Xbox
//! Live. Only with `PEERPARTY_XBL_TRACE=1`: a game failed to load its first map with
//! these hooks (and winhttp.rs's) always on, so they stay off unless asked for.
//!
//! `curl_easy_setopt` and `curl_easy_cleanup` are hooked: each request to an
//! `*.xboxlive.com` host is logged once it's done, with its full URL and its request
//! and response bodies (up to 4 KB each). libHttpClient streams the bodies through the
//! read and write callbacks it sets (CURLOPT_READFUNCTION / WRITEFUNCTION, with itself
//! as the data pointer; libHttpClient Source/HTTP/Curl/CurlEasyRequest.cpp), so those
//! are wrapped. A path may name the player making it (mod.rs `own_xuid_in`). Headers
//! (the Xbox token) are never logged.
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

/// `curl_easy_setopt(handle, option, ...)`: one pointer-sized argument after the
/// option, passed as a fixed one (the same registers and stack slot on x64 and x86).
type SetOpt = unsafe extern "C" fn(usize, u32, usize) -> u32;
type Cleanup = unsafe extern "C" fn(usize);
/// `size_t (*)(char *buffer, size_t size, size_t nitems, void *userdata)`.
type DataFn = unsafe extern "C" fn(*mut u8, usize, usize, usize) -> usize;

static SETOPT: OnceLock<GenericDetour<SetOpt>> = OnceLock::new();
static CLEANUP: OnceLock<GenericDetour<Cleanup>> = OnceLock::new();

#[derive(Default)]
struct Call {
    url: String,
    method: &'static str,
    read_fn: usize,
    write_fn: usize,
    sent: Vec<u8>,
    got: Vec<u8>,
}

#[derive(Default)]
struct Calls {
    by_handle: HashMap<usize, Call>,
    /// The callbacks' data pointer -> its handle.
    read_owner: HashMap<usize, usize>,
    write_owner: HashMap<usize, usize>,
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

unsafe extern "C" fn setopt(h: usize, option: u32, value: usize) -> u32 {
    let Some(hook) = SETOPT.get() else { return 43 };
    let mut value = value;
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
            OPT_READFUNCTION if bodies() && value != 0 => {
                c.by_handle.entry(h).or_default().read_fn = value;
                value = read_wrap as DataFn as usize;
            }
            OPT_WRITEFUNCTION if bodies() && value != 0 => {
                c.by_handle.entry(h).or_default().write_fn = value;
                value = write_wrap as DataFn as usize;
            }
            OPT_READDATA if bodies() => {
                c.read_owner.insert(value, h);
            }
            OPT_WRITEDATA if bodies() => {
                c.write_owner.insert(value, h);
            }
            _ => {}
        }
    }
    unsafe { hook.call(h, option, value) }
}

/// The original callback for a data pointer, and its handle.
fn original(data: usize, write: bool) -> Option<(usize, DataFn)> {
    let calls = CALLS.lock().unwrap();
    let h = *if write { calls.write_owner.get(&data) } else { calls.read_owner.get(&data) }?;
    let call = calls.by_handle.get(&h)?;
    let f = if write { call.write_fn } else { call.read_fn };
    (f != 0).then(|| (h, unsafe { std::mem::transmute::<usize, DataFn>(f) }))
}

fn keep(h: usize, bytes: &[u8], write: bool) {
    let mut calls = CALLS.lock().unwrap();
    if let Some(call) = calls.by_handle.get_mut(&h) {
        let buf = if write { &mut call.got } else { &mut call.sent };
        let room = BODY_MAX.saturating_sub(buf.len());
        buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }
}

unsafe extern "C" fn read_wrap(buf: *mut u8, size: usize, n: usize, data: usize) -> usize {
    // No original means a handle this module didn't see set up; nothing to read then.
    let Some((h, f)) = original(data, false) else { return 0 };
    let got = unsafe { f(buf, size, n, data) };
    // CURL_READFUNC_ABORT / PAUSE are large sentinels, not lengths.
    if got <= size.saturating_mul(n) && !buf.is_null() {
        keep(h, unsafe { std::slice::from_raw_parts(buf, got) }, false);
    }
    got
}

unsafe extern "C" fn write_wrap(buf: *mut u8, size: usize, n: usize, data: usize) -> usize {
    let Some((h, f)) = original(data, true) else { return 0 };
    let len = size.saturating_mul(n);
    if !buf.is_null() {
        keep(h, unsafe { std::slice::from_raw_parts(buf, len) }, true);
    }
    unsafe { f(buf, size, n, data) }
}

fn text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes).replace(['\r', '\n'], " ");
    if bytes.len() >= BODY_MAX { format!("{s}...") } else { s }
}

unsafe extern "C" fn cleanup(h: usize) {
    let call = {
        let mut calls = CALLS.lock().unwrap();
        calls.read_owner.retain(|_, owner| *owner != h);
        calls.write_owner.retain(|_, owner| *owner != h);
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
}
