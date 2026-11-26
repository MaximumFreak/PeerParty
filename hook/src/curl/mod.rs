//! The game's libcurl, wherever it is: XCurl.dll (the GDK's), a DLL that exports
//! `curl_easy_setopt` (libcurl.dll and the like), or a copy linked into one of the game's
//! own modules, found by the patterns the player lists in its hooks file
//! (config/hooks.rs). `curl_easy_setopt`, `curl_easy_cleanup` and `curl_easy_reset` are
//! hooked, for two things:
//!
//! - tls.rs: a request to a host PeerParty answers itself accepts PeerParty's
//!   certificate, and only it. A curl with its own TLS never asks crypt32, so without
//!   this a lookup redirected to PeerParty's HTTPS server ends in a failed certificate
//!   check. XCurl runs on WinHTTP, whose check crypt32 makes: it needs none of this.
//! - The trace, with `PEERPARTY_HTTP_TRACE=1` (or `PEERPARTY_XBL_TRACE=1`): each request
//!   to a service PeerParty stands in for (net/dns.rs `is_service_host`, and every
//!   `*.xboxlive.com` host) is logged once it's done, with its full URL and its request
//!   and response bodies (up to 4 KB each). Other hosts are never logged: their bodies
//!   are the game's business (sign-in tokens, analytics) and would bury the rest. The
//!   bodies flow through the read and write callbacks the game sets
//!   (CURLOPT_READFUNCTION / WRITEFUNCTION), so those are wrapped: once a handle has
//!   both a callback and its data pointer, curl is given our wrapper and a slot of ours
//!   that names the game's pair. Headers (tokens) are never logged.
//!
//! XCurl is hooked only for the trace: a game failed to load its first map with those
//! hooks (and xboxlive/winhttp.rs's) always on. Any other curl is hooked whenever
//! PeerParty runs (`PEERPARTY_SKIP=curl` turns that off), its bodies only wrapped for
//! the trace. A path to an Xbox Live host may name the player making it
//! (xboxlive/mod.rs `own_xuid_in`).

pub mod tls;

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use crate::config::hooks::{Function, ModuleHooks};
use crate::log;
use crate::sys::detour::Armed;
use crate::sys::pe::Export;
use crate::sys::scan::{self, Found};

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
/// A URL given as a `CURLU` handle: its host can't be read here.
const OPT_CURLU: u32 = 10282;

/// CURLE_BAD_FUNCTION_ARGUMENT.
const BAD_ARGUMENT: u32 = 43;

const BODY_MAX: usize = 4096;
/// The longest URL read from the game.
const URL_MAX: usize = 8192;
/// Requests remembered at most, for a curl whose cleanup isn't hooked.
const KEEP: usize = 4096;

/// `curl_easy_setopt(handle, option, ...)`: one argument after the option. It is taken
/// and passed on as two pointer-sized ones: a `curl_off_t` option (30000 and up) passes
/// 64 bits, two stack slots on x86, and the second must reach curl too. On x64 the
/// spare one is a register curl doesn't read.
type SetOpt = unsafe extern "C" fn(usize, u32, usize, usize) -> u32;
type Cleanup = unsafe extern "C" fn(usize);
/// `size_t (*)(char *buffer, size_t size, size_t nitems, void *userdata)`.
type DataFn = unsafe extern "C" fn(*mut u8, usize, usize, usize) -> usize;

/// Which curl a copy is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// XCurl.dll: traced only.
    XCurl,
    /// A DLL that exports curl's functions.
    Library,
    /// Found by patterns in a module that doesn't export them.
    Listed,
}

/// One curl in the process: its hooks, each calling the original through its own slot.
struct Copy {
    what: OnceLock<(Kind, String)>,
    setopt: OnceLock<GenericDetour<SetOpt>>,
    cleanup: OnceLock<GenericDetour<Cleanup>>,
    reset: OnceLock<GenericDetour<Cleanup>>,
}

impl Copy {
    const fn new() -> Copy {
        Copy { what: OnceLock::new(), setopt: OnceLock::new(), cleanup: OnceLock::new(), reset: OnceLock::new() }
    }

    fn kind(&self) -> Option<Kind> {
        self.what.get().map(|(k, _)| *k)
    }

    fn label(&self) -> &str {
        self.what.get().map_or("curl", |(_, l)| l.as_str())
    }
}

/// The copies a process may have hooked at once; a game has one or two.
const COPIES_MAX: usize = 4;
static COPIES: [Copy; COPIES_MAX] = [const { Copy::new() }; COPIES_MAX];
static USED: AtomicUsize = AtomicUsize::new(0);
/// The detours, one set per copy (each knows its copy by its index).
const SETOPTS: [SetOpt; COPIES_MAX] = [setopt::<0>, setopt::<1>, setopt::<2>, setopt::<3>];
const CLEANUPS: [Cleanup; COPIES_MAX] = [cleanup::<0>, cleanup::<1>, cleanup::<2>, cleanup::<3>];
const RESETS: [Cleanup; COPIES_MAX] = [reset::<0>, reset::<1>, reset::<2>, reset::<3>];
/// Every function address hooked here: one found twice (exported and listed) is
/// hooked once.
static HOOKED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

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
static TLS: LazyLock<Mutex<tls::Handles>> = LazyLock::new(Mutex::default);

/// Whether the game's calls are traced (curl's here, WinHTTP's in xboxlive/winhttp.rs).
pub fn trace() -> bool {
    let c = crate::config::HookConfig::global();
    c.http_trace || c.xbl_trace
}

/// Whether a curl other than XCurl is hooked in this game.
pub fn enabled() -> bool {
    crate::net::enabled() && !crate::net::skip("curl")
}

/// Whether XCurl's calls are traced here (xboxlive/winhttp.rs then leaves them to this module).
pub fn xcurl_traced() -> bool {
    COPIES.iter().any(|c| c.kind() == Some(Kind::XCurl) && c.setopt.get().is_some())
}

fn export(exports: &[Export], name: &str) -> usize {
    exports.iter().find(|e| e.name == name).map_or(0, |e| e.addr as usize)
}

/// XCurl.dll loaded (watch/mod.rs). The number of functions hooked.
pub fn attach_xcurl(module: &str, exports: &[Export]) -> usize {
    let f = |n| export(exports, n);
    install(Kind::XCurl, module, f("curl_easy_setopt"), f("curl_easy_cleanup"), f("curl_easy_reset"), false)
}

/// A DLL that exports `curl_easy_setopt` loaded (watch/mod.rs). The number hooked.
pub fn attach_library(module: &str, exports: &[Export]) -> usize {
    let f = |n| export(exports, n);
    install(Kind::Library, module, f("curl_easy_setopt"), f("curl_easy_cleanup"), f("curl_easy_reset"), false)
}

/// How long a module with a hooks file is searched again for what it didn't
/// show at load: a packed exe unpacks its code once it starts.
const RETRY_FOR: std::time::Duration = std::time::Duration::from_secs(60);
const RETRY_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// A module with a hooks file entry for its build loaded (watch/mod.rs): its functions are found by
/// their patterns and hooked. When `curl_easy_setopt` or `curl_easy_cleanup` isn't
/// there yet, the search goes on for a while in the background.
pub fn attach_listed(base: usize, module: &str, hooks: ModuleHooks) {
    if hooks.patterns(Function::CurlEasySetopt).is_empty() {
        log!("curl {module}: the hooks file lists no curl_easy_setopt_patterns; nothing hooked");
        return;
    }
    if install_listed(base, module, &hooks, false, true) {
        return;
    }
    log!("curl {module}: not found yet; searching again for {}s (a packed module unpacks as it starts)", RETRY_FOR.as_secs());
    let name = module.to_string();
    let started = std::thread::Builder::new().name("peerparty-curl-scan".into()).spawn(move || {
        let module = name;
        let until = std::time::Instant::now() + RETRY_FOR;
        while std::time::Instant::now() < until {
            std::thread::sleep(RETRY_EVERY);
            if install_listed(base, &module, &hooks, true, false) {
                return;
            }
        }
        if !install_listed(base, &module, &hooks, true, true) {
            log!("curl {module}: gave up searching; its curl isn't hooked");
        }
    });
    if let Err(e) = started {
        log!("curl {module}: no thread to search again: {e}");
    }
}

/// Finds the listed functions and hooks them once setopt and cleanup are both found.
/// False to search again later. `report`: log each pattern's outcome.
fn install_listed(base: usize, module: &str, hooks: &ModuleHooks, late: bool, report: bool) -> bool {
    let find = |f: Function| -> Option<usize> {
        let patterns = hooks.patterns(f);
        for (i, p) in patterns.iter().enumerate() {
            match scan::find(base, p) {
                Found::At(at) => {
                    log!("curl {module}: {} at +{:#x} (pattern {})", f.name(), at - base, i + 1);
                    return Some(at);
                }
                Found::Many(n) if report => log!("curl {module}: {} pattern {} matches {n} places; not used", f.name(), i + 1),
                Found::Nowhere if report => log!("curl {module}: {} pattern {} matches nowhere", f.name(), i + 1),
                _ => {}
            }
        }
        None
    };
    let (Some(setopt), Some(cleanup)) = (find(Function::CurlEasySetopt), find(Function::CurlEasyCleanup)) else {
        if report && hooks.patterns(Function::CurlEasyCleanup).is_empty() {
            log!("curl {module}: the hooks file lists no curl_easy_cleanup_patterns: curl_easy_setopt alone isn't hooked (a handle's address comes back after cleanup)");
        }
        return false;
    };
    let reset = if hooks.patterns(Function::CurlEasyReset).is_empty() { None } else { find(Function::CurlEasyReset) };
    install(Kind::Listed, module, setopt, cleanup, reset.unwrap_or(0), late);
    true
}

/// Hooks one curl's functions; `late`: found after the game started, so its other
/// threads are held while each is patched. The number hooked.
fn install(kind: Kind, module: &str, setopt: usize, cleanup: usize, reset: usize, late: bool) -> usize {
    if setopt == 0 {
        log!("curl {module}: no curl_easy_setopt");
        return 0;
    }
    // The handle state is keyed by address: without cleanup's hook a new handle at an
    // old one's address would inherit its settings.
    if cleanup == 0 {
        log!("curl {module}: no curl_easy_cleanup; not hooked");
        return 0;
    }
    {
        let mut hooked = crate::sys::lock(&HOOKED);
        if hooked.contains(&setopt) {
            log!("curl {module}: this curl is hooked already");
            return 0;
        }
        hooked.extend([setopt, cleanup, reset].into_iter().filter(|a| *a != 0));
    }
    let i = USED.fetch_add(1, Ordering::Relaxed);
    if i >= COPIES_MAX {
        log!("curl {module}: more than {COPIES_MAX} copies of curl; not hooked");
        return 0;
    }
    let copy = &COPIES[i];
    let _ = copy.what.set((kind, module.to_string()));
    let arm = |slot: &OnceLock<GenericDetour<Cleanup>>, at: usize, detour: Cleanup, label: &str| {
        at != 0 && crate::sys::detour::try_arm(slot, unsafe { std::mem::transmute::<usize, Cleanup>(at) }, detour, "curl", label, late) == Armed::Hooked
    };
    // Cleanup and reset first: by the time setopt notes a handle, its end is seen too.
    let mut n = usize::from(arm(&copy.cleanup, cleanup, CLEANUPS[i], "curl_easy_cleanup"));
    n += usize::from(arm(&copy.reset, reset, RESETS[i], "curl_easy_reset"));
    let target: SetOpt = unsafe { std::mem::transmute::<usize, SetOpt>(setopt) };
    n += usize::from(crate::sys::detour::try_arm(&copy.setopt, target, SETOPTS[i], "curl", "curl_easy_setopt", late) == Armed::Hooked);
    let what = match kind {
        Kind::XCurl => "its calls are traced",
        _ if trace() => "PeerParty's hosts take PeerParty's certificate; calls traced",
        _ => "PeerParty's hosts take PeerParty's certificate",
    };
    log!("curl {module} hooked {n} ({what})");
    n
}

/// The host and path of an `https://host/path?query` URL.
pub(crate) fn split_url(url: &str) -> (&str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    }
}

fn host_of(url: &str) -> String {
    split_url(url).0.rsplit('@').next().unwrap_or_default().split(':').next().unwrap_or_default().to_ascii_lowercase()
}

fn is_xbox(url: &str) -> bool {
    host_of(url).ends_with(".xboxlive.com")
}

/// Whether a request to `url` is logged by the trace.
fn traced(kind: Kind, url: &str) -> bool {
    is_xbox(url) || (kind != Kind::XCurl && crate::net::dns::is_service_host(&host_of(url)))
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
    let mut s = crate::sys::lock(side);
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

unsafe extern "C" fn setopt<const I: usize>(h: usize, option: u32, value: usize, high: usize) -> u32 {
    let copy = &COPIES[I];
    let Some(hook) = copy.setopt.get() else { return BAD_ARGUMENT };
    let kind = copy.kind().unwrap_or(Kind::XCurl);
    let mut value = value;
    // A replacement for a string option, kept alive until curl has copied it.
    let mut text_value = None;
    let url = (option == OPT_URL && value != 0).then(|| crate::sys::read::utf8(value, URL_MAX).unwrap_or_default());
    if kind != Kind::XCurl {
        if let Some(url) = &url {
            certificates(copy, h, url);
        } else if tls::is_option(option) {
            if let Some(pin) = crate::https::tls::curl_pin() {
                let text = (option == tls::OPT_PINNEDPUBLICKEY && value != 0)
                    .then(|| crate::sys::read::utf8(value, URL_MAX).and_then(|t| CString::new(t).ok()))
                    .flatten();
                if let Some(v) = crate::sys::lock(&TLS).set(h, option, value, text, pin) {
                    value = v.arg();
                    text_value = Some(v);
                }
            }
        } else if option == OPT_CURLU && value != 0 {
            crate::log::throttled(&format!("curl {} curlu", copy.label()), 1, |_| {
                log!("gap: curl {}: a URL given as CURLOPT_CURLU; its host isn't read, so its certificate is left to curl", copy.label());
            });
        }
    }
    let mut also = None;
    {
        let mut calls = crate::sys::lock(&CALLS);
        let c = &mut *calls;
        if c.by_handle.len() >= KEEP && !c.by_handle.contains_key(&h) {
            c.by_handle.clear();
        }
        match option {
            OPT_URL if value != 0 => {
                let call = c.by_handle.entry(h).or_default();
                if call.method.is_empty() {
                    call.method = "GET";
                }
                call.url = url.unwrap_or_default();
            }
            OPT_CUSTOMREQUEST if value != 0 => {
                let m = crate::sys::read::cstr(value).unwrap_or_default().to_ascii_uppercase();
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
            OPT_READFUNCTION | OPT_WRITEFUNCTION | OPT_READDATA | OPT_WRITEDATA if trace() => {
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
    let result = unsafe { hook.call(h, option, value, high) };
    drop(text_value);
    result
}

/// A handle's URL is set: when its host is PeerParty's, curl is told to take PeerParty's
/// key; when it was and isn't any more, the game's settings go back (tls.rs).
fn certificates(copy: &Copy, h: usize, url: &str) {
    let local = tls::https_host(url).is_some_and(|host| answered_here(&host));
    let Some(pin) = (if local || crate::sys::lock(&TLS).is_ours(h) { crate::https::tls::curl_pin() } else { None }) else {
        return;
    };
    let plan = crate::sys::lock(&TLS).url(h, local, pin);
    let Some(hook) = copy.setopt.get() else { return };
    for (i, (option, v)) in plan.iter().enumerate() {
        let code = unsafe { hook.call(h, *option, v.arg(), 0) };
        if i == 0 && local && code != 0 {
            crate::sys::lock(&TLS).unpinned(h);
            crate::log::throttled(&format!("curl {} no pin", copy.label()), 1, |_| {
                log!("gap: curl {} can't pin a public key (CURLcode {code}): its certificate check is left as it is, and refuses PeerParty's", copy.label());
            });
            return;
        }
    }
    if local {
        let host = tls::https_host(url).unwrap_or_default();
        crate::log::throttled(&format!("curl {} pin {host}", copy.label()), 1, |_| {
            log!("curl {}: {host} is answered here; that request accepts PeerParty's certificate only", copy.label());
        });
    }
}

fn answered_here(host: &str) -> bool {
    #[cfg(test)]
    if crate::sys::lock(&tests::LOCAL_HOST).as_deref() == Some(host) {
        return true;
    }
    crate::net::dns::answered_here(host)
}

/// The handle, and the game's callback and data, behind a wrapper's data pointer:
/// always one of our slots (the wrapper is only ever set together with one), which are
/// never freed.
fn original(data: usize, write: bool) -> Option<(usize, DataFn, usize)> {
    if data == 0 {
        return None;
    }
    let slot = unsafe { &*(data as *const Slot) };
    let s = *crate::sys::lock(if write { &slot.write } else { &slot.read });
    (s.func != 0).then(|| (slot.handle, unsafe { std::mem::transmute::<usize, DataFn>(s.func) }, s.data))
}

fn keep(h: usize, bytes: &[u8], write: bool) {
    let mut calls = crate::sys::lock(&CALLS);
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

/// `curl_easy_reset`: every option back to curl's default, so nothing is wrapped or
/// pinned.
unsafe extern "C" fn reset<const I: usize>(h: usize) {
    {
        let mut calls = crate::sys::lock(&CALLS);
        if let Some(slot) = calls.slots.get(&h) {
            *crate::sys::lock(&slot.read) = Side::default();
            *crate::sys::lock(&slot.write) = Side::default();
        }
        calls.by_handle.remove(&h);
    }
    crate::sys::lock(&TLS).forget(h);
    if let Some(hook) = COPIES[I].reset.get() {
        unsafe { hook.call(h) }
    }
}

fn text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes).replace(['\r', '\n'], " ");
    if bytes.len() >= BODY_MAX { format!("{s}...") } else { s }
}

unsafe extern "C" fn cleanup<const I: usize>(h: usize) {
    let copy = &COPIES[I];
    let kind = copy.kind().unwrap_or(Kind::XCurl);
    let call = {
        // The slot itself stays (never freed): a copy made of this handle may still
        // point at it. Only the handle's claim on it goes, since curl_easy_init can
        // hand the same address out again, and a new handle starts with nothing wrapped.
        let mut calls = crate::sys::lock(&CALLS);
        calls.slots.remove(&h);
        calls.by_handle.remove(&h)
    };
    crate::sys::lock(&TLS).forget(h);
    if let Some(call) = call.filter(|c| traced(kind, &c.url)) {
        let (host, path) = split_url(&call.url);
        if let Some(x) = crate::xboxlive::own_xuid_in(host, path) {
            crate::xboxlive::learn(x);
        }
        if trace() {
            log!("http-trace {} {} {} sent {} got {}", copy.label(), call.method, call.url, text(&call.sent), text(&call.got));
        } else {
            let host = host_of(&call.url);
            let shape = crate::xboxlive::shape(path);
            crate::log::throttled(&format!("curl {} {} {host}{shape}", copy.label(), call.method), 2, |n| {
                log!("curl {} {} https://{host}{shape} n={n}", copy.label(), call.method);
            });
        }
    }
    if let Some(hook) = copy.cleanup.get() {
        unsafe { hook.call(h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host the tests have answered here, without a service's DNS behind it.
    pub(super) static LOCAL_HOST: Mutex<Option<String>> = Mutex::new(None);

    unsafe extern "C" fn sink(_: *mut u8, size: usize, n: usize, _: usize) -> usize {
        size * n
    }

    /// A real libcurl DLL (`PPTEST_LIBCURL`, a 64-bit one in a 64-bit test: Git for
    /// Windows ships `mingw64\bin\libcurl-4.dll`), hooked as the hook does any DLL that
    /// exports curl, and called the way a game calls it: `curl_easy_setopt` through its
    /// `...`. The game's code never turns curl's checks off, yet the request to
    /// PeerParty's server goes through, so the hook saw the URL through the ellipsis and
    /// gave curl the pin; with the hook gone the same handle would fail the check.
    #[test]
    fn a_libcurl_dll_reaches_peerpartys_server_through_its_hooked_setopt() {
        let Ok(path) = std::env::var("PPTEST_LIBCURL") else {
            eprintln!("skipped: PPTEST_LIBCURL not set");
            return;
        };
        use windows_sys::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
        assert!(!module.is_null(), "{path} doesn't load");
        let table = unsafe { crate::sys::pe::exports(module as *const u8).unwrap() };
        let f = |n: &str| export(&table.code, n);
        // What the game calls: curl's own declarations, the setopt one with its ellipsis.
        type Init = unsafe extern "C" fn() -> usize;
        type Setopt = unsafe extern "C" fn(usize, u32, ...) -> u32;
        type Perform = unsafe extern "C" fn(usize) -> u32;
        type GetInfo = unsafe extern "C" fn(usize, u32, ...) -> u32;
        type Done = unsafe extern "C" fn(usize);
        let init: Init = unsafe { std::mem::transmute(f("curl_easy_init")) };
        let setopt: Setopt = unsafe { std::mem::transmute(f("curl_easy_setopt")) };
        let perform: Perform = unsafe { std::mem::transmute(f("curl_easy_perform")) };
        let getinfo: GetInfo = unsafe { std::mem::transmute(f("curl_easy_getinfo")) };
        let done: Done = unsafe { std::mem::transmute(f("curl_easy_cleanup")) };
        assert_eq!(attach_library("libcurl-test.dll", &table.code), 3, "setopt, cleanup and reset");

        let ip = crate::https::server().expect("server").to_string();
        *crate::sys::lock(&LOCAL_HOST) = Some(ip.clone());
        let url = std::ffi::CString::new(format!("https://{ip}/Client/LoginWithCustomID")).unwrap();
        let body = std::ffi::CString::new("{\"CustomId\":\"x\"}").unwrap();
        let h = unsafe { init() };
        assert_ne!(h, 0);
        unsafe {
            assert_eq!(setopt(h, OPT_URL, url.as_ptr()), 0);
            assert_eq!(setopt(h, OPT_WRITEFUNCTION, sink as DataFn), 0);
            // CURLOPT_POSTFIELDS, and a curl_off_t one: CURLOPT_POSTFIELDSIZE_LARGE.
            assert_eq!(setopt(h, 10015, body.as_ptr()), 0);
            assert_eq!(setopt(h, 30120, body.as_bytes().len() as i64), 0);
            // CURLOPT_TIMEOUT.
            assert_eq!(setopt(h, 13, 10 as std::ffi::c_long), 0);
        }
        assert!(crate::sys::lock(&TLS).is_ours(h), "the hook read the URL through the ellipsis");
        let code = unsafe { perform(h) };
        let mut status: std::ffi::c_long = 0;
        // CURLINFO_RESPONSE_CODE.
        unsafe { getinfo(h, 0x200002, &mut status as *mut std::ffi::c_long) };
        assert_eq!(code, 0, "curl_easy_perform: CURLcode {code}");
        assert!(status >= 200, "an HTTP answer: {status}");
        // Pointed at another host, the handle is the game's again.
        let other = std::ffi::CString::new("https://www.example.com/").unwrap();
        unsafe { setopt(h, OPT_URL, other.as_ptr()) };
        assert!(!crate::sys::lock(&TLS).is_ours(h));
        unsafe { done(h) };
        // The control: the same request, with the host not answered here, so the hook
        // leaves the handle alone; curl's own check refuses PeerParty's certificate.
        *crate::sys::lock(&LOCAL_HOST) = None;
        let h = unsafe { init() };
        unsafe {
            setopt(h, OPT_URL, url.as_ptr());
            setopt(h, OPT_WRITEFUNCTION, sink as DataFn);
            setopt(h, 13, 10 as std::ffi::c_long);
        }
        assert!(!crate::sys::lock(&TLS).is_ours(h));
        let code = unsafe { perform(h) };
        // CURLE_PEER_FAILED_VERIFICATION, or CURLE_SSL_CONNECT_ERROR from some back ends.
        assert!(matches!(code, 60 | 35), "without the pin curl must refuse: CURLcode {code}");
        unsafe { done(h) };
    }

    #[test]
    fn splits_urls() {
        assert_eq!(split_url("https://social.xboxlive.com/users/xuid(1)/people?view=All"), ("social.xboxlive.com", "/users/xuid(1)/people?view=All"));
        assert_eq!(split_url("https://rta.xboxlive.com"), ("rta.xboxlive.com", "/"));
        assert!(is_xbox("https://sessiondirectory.xboxlive.com/handles/query"));
        assert!(is_xbox("https://user@Social.XboxLive.com:443/x"));
        assert!(!is_xbox("https://telemetry.example.net/x"));
        assert!(!is_xbox("https://xboxlive.com.example.org/"));
    }

    /// Windows' own curl.exe, with the settings tls.rs gives a handle (checks off, the
    /// pin on), reaches PeerParty's server; with another key pinned it doesn't connect.
    #[test]
    fn a_real_curl_takes_peerpartys_server_by_its_pin_alone() {
        let curl = std::path::Path::new(&std::env::var("SystemRoot").unwrap_or_default()).join(r"System32\curl.exe");
        if !curl.is_file() {
            eprintln!("skipped: no curl.exe");
            return;
        }
        let ip = crate::https::server().expect("server");
        let pin = crate::https::tls::curl_pin().expect("pin").to_str().unwrap().to_string();
        let run = |pin: &str| {
            use std::os::windows::process::CommandExt;
            std::process::Command::new(&curl)
                .creation_flags(0x0800_0000)
                .args(["-s", "-o", "NUL", "-w", "%{http_code}", "--max-time", "10", "-k", "--pinnedpubkey", pin])
                .arg(format!("https://{ip}/"))
                .output()
                .expect("curl.exe")
        };
        let out = run(&pin);
        let code = String::from_utf8_lossy(&out.stdout).to_string();
        // CURLE_NOT_BUILT_IN, CURLE_UNKNOWN_OPTION: a curl.exe without pinning.
        if matches!(out.status.code(), Some(2 | 4 | 48)) {
            eprintln!("skipped: this curl.exe can't pin ({:?})", out.status.code());
            return;
        }
        assert!(out.status.success(), "curl failed: {:?} {}", out.status.code(), String::from_utf8_lossy(&out.stderr));
        assert!(code.len() == 3 && code != "000", "an HTTP answer: {code}");
        let wrong = format!("sha256//{}", crate::util::encoding::base64(&[7u8; 32]));
        let out = run(&wrong);
        // CURLE_SSL_PINNEDPUBKEYNOTMATCH.
        assert_eq!(out.status.code(), Some(90), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn the_trace_keeps_to_known_services() {
        assert!(traced(Kind::XCurl, "https://social.xboxlive.com/x"));
        assert!(!traced(Kind::XCurl, "https://a1b2.playfabapi.com/Client/Login"), "XCurl's trace is Xbox Live's");
        assert!(traced(Kind::Listed, "https://a1b2.playfabapi.com/Client/Login"));
        assert!(traced(Kind::Library, "https://profile.xboxlive.com/users"));
        assert!(!traced(Kind::Listed, "https://telemetry.example.net/x"));
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
