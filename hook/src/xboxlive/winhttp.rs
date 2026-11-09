//! The same trace as curl.rs for games whose libHttpClient runs on WinHTTP directly
//! (games that ship no XCurl.dll), also only with `PEERPARTY_XBL_TRACE=1`.
//! `WinHttpConnect` names the host and `WinHttpOpenRequest` the verb and path; each
//! `*.xboxlive.com` request is logged with its request body (`WinHttpSendRequest`'s
//! optional data, then any `WinHttpWriteData`, up to 4 KB) and may name the player
//! making it. Responses aren't logged (WinHTTP hands them over asynchronously). When
//! XCurl.dll is traced (it runs on WinHTTP too), curl.rs logs instead.
//!
//! Nothing a request does is changed.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;

type Connect = unsafe extern "system" fn(usize, *const u16, u16, u32) -> usize;
type OpenRequest = unsafe extern "system" fn(usize, *const u16, *const u16, *const u16, *const u16, usize, u32) -> usize;
type SendRequest = unsafe extern "system" fn(usize, *const u16, u32, *const u8, u32, u32, usize) -> i32;
type WriteData = unsafe extern "system" fn(usize, *const u8, u32, *mut u32) -> i32;

static CONNECT: OnceLock<GenericDetour<Connect>> = OnceLock::new();
static OPEN: OnceLock<GenericDetour<OpenRequest>> = OnceLock::new();
static SEND: OnceLock<GenericDetour<SendRequest>> = OnceLock::new();
static WRITE: OnceLock<GenericDetour<WriteData>> = OnceLock::new();

const BODY_MAX: usize = 4096;
/// Handles remembered at most; WinHTTP's are never reused while open, and old ones are
/// simply forgotten.
const KEEP: usize = 4096;

#[derive(Default)]
struct Handles {
    /// Connection -> host, for Xbox Live hosts only.
    hosts: HashMap<usize, String>,
    /// Request -> "VERB https://host/path".
    requests: HashMap<usize, String>,
}

static H: LazyLock<Mutex<Handles>> = LazyLock::new(Mutex::default);

fn wstr(p: *const u16) -> String {
    if p.is_null() { String::new() } else { crate::read::wstr(p as usize).unwrap_or_default() }
}

/// winhttp.dll loaded (watch.rs). The number of functions hooked.
pub fn attach(exports: &[Export]) -> usize {
    let find = |name: &str| exports.iter().find(|e| e.name == name).map(|e| e.addr);
    let mut n = 0;
    if let Some(a) = find("WinHttpConnect") {
        n += usize::from(arm(&CONNECT, unsafe { std::mem::transmute::<*const (), Connect>(a) }, connect as Connect, "WinHttpConnect"));
    }
    if let Some(a) = find("WinHttpOpenRequest") {
        n += usize::from(arm(&OPEN, unsafe { std::mem::transmute::<*const (), OpenRequest>(a) }, open as OpenRequest, "WinHttpOpenRequest"));
    }
    if super::curl::bodies() {
        if let Some(a) = find("WinHttpSendRequest") {
            n += usize::from(arm(&SEND, unsafe { std::mem::transmute::<*const (), SendRequest>(a) }, send as SendRequest, "WinHttpSendRequest"));
        }
        if let Some(a) = find("WinHttpWriteData") {
            n += usize::from(arm(&WRITE, unsafe { std::mem::transmute::<*const (), WriteData>(a) }, write as WriteData, "WinHttpWriteData"));
        }
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

unsafe extern "system" fn connect(session: usize, server: *const u16, port: u16, reserved: u32) -> usize {
    let h = unsafe { CONNECT.get().unwrap().call(session, server, port, reserved) };
    if h != 0 {
        let host = wstr(server).to_ascii_lowercase();
        if host.ends_with(".xboxlive.com") {
            let mut s = H.lock().unwrap();
            if s.hosts.len() >= KEEP {
                s.hosts.clear();
            }
            s.hosts.insert(h, host);
        }
    }
    h
}

unsafe extern "system" fn open(
    connection: usize,
    verb: *const u16,
    object: *const u16,
    version: *const u16,
    referrer: *const u16,
    accept: usize,
    flags: u32,
) -> usize {
    let h = unsafe { OPEN.get().unwrap().call(connection, verb, object, version, referrer, accept, flags) };
    let Some(host) = H.lock().unwrap().hosts.get(&connection).cloned() else { return h };
    let verb = Some(wstr(verb)).filter(|v| !v.is_empty()).unwrap_or_else(|| "GET".into()).to_ascii_uppercase();
    let path = Some(wstr(object)).filter(|p| !p.is_empty()).unwrap_or_else(|| "/".into());
    if let Some(x) = super::own_xuid_in(&host, &path) {
        super::learn(x);
    }
    if super::curl::active() {
        return h;
    }
    if super::curl::bodies() {
        let mut s = H.lock().unwrap();
        if s.requests.len() >= KEEP {
            s.requests.clear();
        }
        s.requests.insert(h, format!("{verb} https://{host}{path}"));
    } else {
        let shape = super::shape(&path);
        crate::log::throttled(&format!("xbl-winhttp {verb} {host}{shape}"), 2, |n| {
            log!("xbl-winhttp {verb} https://{host}{shape} n={n}");
        });
    }
    h
}

fn trace_body(request: usize, data: *const u8, len: u32, what: &str) {
    let Some(call) = H.lock().unwrap().requests.get(&request).cloned() else { return };
    let len = (len as usize).min(BODY_MAX);
    let body = if data.is_null() || len == 0 {
        String::new()
    } else {
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(data, len) }).replace(['\r', '\n'], " ")
    };
    log!("xbl-trace {call} {what} {body}");
}

unsafe extern "system" fn send(
    request: usize,
    headers: *const u16,
    headers_len: u32,
    optional: *const u8,
    optional_len: u32,
    total: u32,
    context: usize,
) -> i32 {
    trace_body(request, optional, optional_len, "sent");
    unsafe { SEND.get().unwrap().call(request, headers, headers_len, optional, optional_len, total, context) }
}

unsafe extern "system" fn write(request: usize, data: *const u8, len: u32, written: *mut u32) -> i32 {
    trace_body(request, data, len, "sent more");
    unsafe { WRITE.get().unwrap().call(request, data, len, written) }
}
