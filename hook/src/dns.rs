//! DNS lookup hooks. The hostname is the thing phase 0 has to record: which
//! online services the game actually contacts. Lookups are unchanged, except that
//! in a game started through PeerParty PlayFab's hosts resolve to PeerParty's local PlayFab service
//! (playfab/mod.rs) and Photon Cloud's to the LAN's Photon servers (photon/mod.rs):
//! the lookup runs on that address's text instead (getaddrinfo, GetAddrInfoW,
//! GetAddrInfoExA/W, gethostbyname), or for DnsQuery_A/W and DnsQueryEx, an A record
//! with that address is the answer (made by dnsapi's own DnsRecordCopyEx, so the game
//! frees it as it frees any), and an AAAA query gets none.

use std::sync::OnceLock;

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;
use crate::read;

type GetAddrInfoA =
    unsafe extern "system" fn(*const i8, *const i8, *const core::ffi::c_void, *mut *mut u8) -> i32;
type GetAddrInfoW = unsafe extern "system" fn(
    *const u16,
    *const u16,
    *const core::ffi::c_void,
    *mut *mut u8,
) -> i32;
type GetAddrInfoExW = unsafe extern "system" fn(
    *const u16,
    *const u16,
    u32,
    *mut core::ffi::c_void,
    *const core::ffi::c_void,
    *mut *mut u8,
    *mut core::ffi::c_void,
    *mut core::ffi::c_void,
    *mut core::ffi::c_void,
    *mut *mut core::ffi::c_void,
) -> i32;
type DnsQueryA = unsafe extern "system" fn(
    *const i8,
    u16,
    u32,
    *mut core::ffi::c_void,
    *mut *mut core::ffi::c_void,
    *mut *mut core::ffi::c_void,
) -> i32;
type DnsQueryW = unsafe extern "system" fn(
    *const u16,
    u16,
    u32,
    *mut core::ffi::c_void,
    *mut *mut core::ffi::c_void,
    *mut *mut core::ffi::c_void,
) -> i32;

static GAI: OnceLock<GenericDetour<GetAddrInfoA>> = OnceLock::new();
static GAIW: OnceLock<GenericDetour<GetAddrInfoW>> = OnceLock::new();
static GAIEXW: OnceLock<GenericDetour<GetAddrInfoExW>> = OnceLock::new();
static DNSA: OnceLock<GenericDetour<DnsQueryA>> = OnceLock::new();
static DNSW: OnceLock<GenericDetour<DnsQueryW>> = OnceLock::new();

type GetAddrInfoExA = unsafe extern "system" fn(
    *const i8,
    *const i8,
    u32,
    *mut core::ffi::c_void,
    *const core::ffi::c_void,
    *mut *mut u8,
    *mut core::ffi::c_void,
    *mut core::ffi::c_void,
    *mut core::ffi::c_void,
    *mut *mut core::ffi::c_void,
) -> i32;
type GetHostByName = unsafe extern "system" fn(*const i8) -> *mut u8;
type DnsQueryEx = unsafe extern "system" fn(*const DnsQueryRequest, *mut DnsQueryResult, *mut core::ffi::c_void) -> i32;
type RecordCopyEx = unsafe extern "system" fn(*const DnsRecord, u32, u32) -> *mut DnsRecord;

static GAIEXA: OnceLock<GenericDetour<GetAddrInfoExA>> = OnceLock::new();
static GHBN: OnceLock<GenericDetour<GetHostByName>> = OnceLock::new();
static DNSEX: OnceLock<GenericDetour<DnsQueryEx>> = OnceLock::new();
/// dnsapi's DnsRecordCopyEx, which allocates a record the way DnsRecordListFree frees.
static RECORD_COPY: OnceLock<RecordCopyEx> = OnceLock::new();

/// DNS_RECORD's head and an A record's data (windns.h): pNext, pName, wType,
/// wDataLength, Flags, dwTtl, dwReserved, then DNS_A_DATA.
#[repr(C)]
struct DnsRecord {
    next: *mut DnsRecord,
    name: *const core::ffi::c_void,
    ty: u16,
    data_len: u16,
    flags: u32,
    ttl: u32,
    reserved: u32,
    ip: u32,
}

/// DNS_QUERY_REQUEST (version 1): what DnsQueryEx is asked.
#[repr(C)]
struct DnsQueryRequest {
    version: u32,
    name: *const u16,
    ty: u16,
    options: u64,
    servers: *mut core::ffi::c_void,
    interface: u32,
    callback: *mut core::ffi::c_void,
    context: *mut core::ffi::c_void,
}

/// DNS_QUERY_RESULT: where DnsQueryEx answers.
#[repr(C)]
struct DnsQueryResult {
    version: u32,
    status: i32,
    options: u64,
    records: *mut DnsRecord,
    reserved: *mut core::ffi::c_void,
}

const DNS_TYPE_A: u16 = 1;
const DNS_TYPE_AAAA: u16 = 28;
/// DNS_INFO_NO_RECORDS: the name is there, with no records of that type.
const DNS_INFO_NO_RECORDS: i32 = 9501;
const CHARSET_UNICODE: u32 = 1;
const CHARSET_ANSI: u32 = 3;

/// A redirected name's answer to a DnsQuery: Some(Ok(record)) for an A query (the
/// record allocated by dnsapi), Some(Err(code)) for an AAAA one, None to let the query
/// go on as it is (not redirected, another type, or no DnsRecordCopyEx).
fn dns_answer(host: &str, ty: u16, name: *const core::ffi::c_void, charset: u32) -> Option<Result<*mut DnsRecord, i32>> {
    if ty != DNS_TYPE_A && ty != DNS_TYPE_AAAA {
        return None;
    }
    let ip: std::net::Ipv4Addr = redirect(host)?.parse().ok()?;
    if ty == DNS_TYPE_AAAA {
        return Some(Err(DNS_INFO_NO_RECORDS));
    }
    a_record(*RECORD_COPY.get()?, ip, name, charset).map(Ok)
}

/// An A record for `name` (a string in `charset`) at `ip`, allocated by dnsapi.
fn a_record(copy: RecordCopyEx, ip: std::net::Ipv4Addr, name: *const core::ffi::c_void, charset: u32) -> Option<*mut DnsRecord> {
    // Section answer (1), the name's character set in bits 3-4.
    let rec = DnsRecord { next: std::ptr::null_mut(), name, ty: DNS_TYPE_A, data_len: 4, flags: 1 | (charset << 3), ttl: 60, reserved: 0, ip: u32::from_ne_bytes(ip.octets()) };
    let made = unsafe { copy(&rec, charset, charset) };
    (!made.is_null()).then_some(made)
}


pub fn attach_winsock(exports: &[Export]) -> usize {
    let mut n = 0;
    n += hook_one(exports, "getaddrinfo", |addr| {
        let target: GetAddrInfoA = unsafe { std::mem::transmute(addr) };
        arm(&GAI, target, detour_gai, "getaddrinfo")
    });
    n += hook_one(exports, "GetAddrInfoW", |addr| {
        let target: GetAddrInfoW = unsafe { std::mem::transmute(addr) };
        arm(&GAIW, target, detour_gaiw, "GetAddrInfoW")
    });
    n += hook_one(exports, "GetAddrInfoExW", |addr| {
        let target: GetAddrInfoExW = unsafe { std::mem::transmute(addr) };
        arm(&GAIEXW, target, detour_gaiexw, "GetAddrInfoExW")
    });
    n += hook_one(exports, "GetAddrInfoExA", |addr| {
        let target: GetAddrInfoExA = unsafe { std::mem::transmute(addr) };
        arm(&GAIEXA, target, detour_gaiexa, "GetAddrInfoExA")
    });
    n += hook_one(exports, "gethostbyname", |addr| {
        let target: GetHostByName = unsafe { std::mem::transmute(addr) };
        arm(&GHBN, target, detour_ghbn, "gethostbyname")
    });
    n
}

pub fn attach_dnsapi(exports: &[Export]) -> usize {
    let mut n = 0;
    n += hook_one(exports, "DnsQuery_A", |addr| {
        let target: DnsQueryA = unsafe { std::mem::transmute(addr) };
        arm(&DNSA, target, detour_dnsa, "DnsQuery_A")
    });
    n += hook_one(exports, "DnsQuery_W", |addr| {
        let target: DnsQueryW = unsafe { std::mem::transmute(addr) };
        arm(&DNSW, target, detour_dnsw, "DnsQuery_W")
    });
    if let Some(e) = exports.iter().find(|e| e.name == "DnsRecordCopyEx") {
        let _ = RECORD_COPY.set(unsafe { std::mem::transmute::<*const (), RecordCopyEx>(e.addr as *const ()) });
    }
    // Windows 8 and later.
    if exports.iter().any(|e| e.name == "DnsQueryEx") {
        n += hook_one(exports, "DnsQueryEx", |addr| {
            let target: DnsQueryEx = unsafe { std::mem::transmute(addr) };
            arm(&DNSEX, target, detour_dnsex, "DnsQueryEx")
        });
    }
    n
}

fn hook_one(exports: &[Export], name: &str, install: impl FnOnce(*const ()) -> bool) -> usize {
    let Some(export) = exports.iter().find(|e| e.name == name) else {
        log!("dns {name} not exported");
        return 0;
    };
    usize::from(install(export.addr))
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let hook = match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => h,
            Err(e) => {
                log!("dns {label} not hooked: {e}");
                return false;
            }
        };
        if slot.set(hook).is_err() {
            log!("dns {label} already hooked");
            return false;
        }
        if let Err(e) = unsafe { slot.get().unwrap().enable() } {
            log!("dns {label} enable failed: {e}");
            return false;
        }
        true
    })
}

/// Where a lookup of `host` goes instead: PeerParty's PlayFab service, its Unity Gaming
/// Services and Xbox Live friends stand-ins (ugs/mod.rs, xboxlive/mod.rs, on the same
/// HTTPS server), or the copy of PeerParty on the LAN that hosts Photon's servers
/// (photon/mod.rs).
fn redirect(host: &str) -> Option<String> {
    if BYPASS.with(|b| b.get()) {
        return None;
    }
    // A service the player turned off for this game: the overlay says what that costs.
    if crate::net::enabled() {
        let h = host.trim_end_matches('.').to_ascii_lowercase();
        if crate::photon::is_photon_host(&h) {
            crate::net::needed_but_off("photon");
        } else if h.ends_with(crate::playfab::tls::HOST_SUFFIX) || h == "playfabapi.com" {
            crate::net::needed_but_off("playfab");
        } else if crate::ugs::is_host(&h) {
            crate::net::needed_but_off("ugs");
        } else if crate::xboxlive::service_of(&h).is_some() {
            crate::net::needed_but_off("xboxlive");
        }
    }
    crate::playfab::redirect(host)
        .or_else(|| crate::ugs::redirect(host))
        .or_else(|| crate::xboxlive::redirect(host))
        .or_else(|| crate::photon::redirect(host))
}

thread_local! {
    /// Set on PeerParty's own threads whose lookups must reach the real hosts (modes.rs).
    static BYPASS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// This thread's lookups are never redirected.
pub fn bypass_this_thread() {
    BYPASS.with(|b| b.set(true));
}

/// Each redirect address as a C string and a wide string, kept for the whole run so
/// lookups that finish later can keep pointing at it (a handful of addresses at most).
fn interned(ip: String) -> (&'static std::ffi::CStr, &'static [u16]) {
    type Texts = Vec<(String, &'static std::ffi::CStr, &'static [u16])>;
    static TEXTS: std::sync::Mutex<Texts> = std::sync::Mutex::new(Vec::new());
    let mut t = TEXTS.lock().unwrap();
    if let Some((_, a, w)) = t.iter().find(|(s, _, _)| *s == ip) {
        return (a, w);
    }
    let a: &'static std::ffi::CStr = Box::leak(std::ffi::CString::new(ip.clone()).unwrap().into_boxed_c_str());
    let w: &'static [u16] = Box::leak(ip.encode_utf16().chain(Some(0)).collect::<Vec<u16>>().into_boxed_slice());
    t.push((ip, a, w));
    (a, w)
}

fn redirect_a(host: &str) -> Option<*const i8> {
    Some(interned(redirect(host)?).0.as_ptr())
}

fn redirect_w(host: &str) -> Option<*const u16> {
    Some(interned(redirect(host)?).1.as_ptr())
}

fn host_a(p: *const i8) -> String {
    if p.is_null() {
        "-".into()
    } else {
        read::cstr(p as usize).unwrap_or_else(|| "?".into())
    }
}

fn host_w(p: *const u16) -> String {
    if p.is_null() {
        "-".into()
    } else {
        read::wstr(p as usize).unwrap_or_else(|| "?".into())
    }
}

unsafe extern "system" fn detour_gai(
    node: *const i8,
    service: *const i8,
    hints: *const core::ffi::c_void,
    result: *mut *mut u8,
) -> i32 {
    let Some(hook) = GAI.get() else { return 11001 };
    let host = host_a(node);
    let service_name = host_a(service);
    let node = redirect_a(&host).unwrap_or(node);
    let rc = unsafe { hook.call(node, service, hints, result) };
    let addrs = if rc == 0 && !result.is_null() {
        read::addrinfos(unsafe { *result } as *const u8, read::ADDRINFO_NEXT)
    } else {
        String::new()
    };
    log::throttled(&format!("dns getaddrinfo {host}"), 4, |n| {
        log!("dns getaddrinfo n={n} host={host} service={service_name} -> {rc} {addrs}");
    });
    rc
}

unsafe extern "system" fn detour_gaiw(
    node: *const u16,
    service: *const u16,
    hints: *const core::ffi::c_void,
    result: *mut *mut u8,
) -> i32 {
    let Some(hook) = GAIW.get() else { return 11001 };
    let host = host_w(node);
    let service_name = host_w(service);
    let node = redirect_w(&host).unwrap_or(node);
    let rc = unsafe { hook.call(node, service, hints, result) };
    let addrs = if rc == 0 && !result.is_null() {
        read::addrinfos(unsafe { *result } as *const u8, read::ADDRINFO_NEXT)
    } else {
        String::new()
    };
    log::throttled(&format!("dns GetAddrInfoW {host}"), 4, |n| {
        log!("dns GetAddrInfoW n={n} host={host} service={service_name} -> {rc} {addrs}");
    });
    rc
}

unsafe extern "system" fn detour_gaiexw(
    name: *const u16,
    service: *const u16,
    namespace: u32,
    nsp: *mut core::ffi::c_void,
    hints: *const core::ffi::c_void,
    result: *mut *mut u8,
    timeout: *mut core::ffi::c_void,
    overlapped: *mut core::ffi::c_void,
    completion: *mut core::ffi::c_void,
    handle: *mut *mut core::ffi::c_void,
) -> i32 {
    let Some(hook) = GAIEXW.get() else {
        return 11001;
    };
    let host = host_w(name);
    let service_name = host_w(service);
    let name = redirect_w(&host).unwrap_or(name);
    let rc = unsafe {
        hook.call(
            name, service, namespace, nsp, hints, result, timeout, overlapped, completion, handle,
        )
    };
    // Overlapped lookups complete later; the addresses aren't filled in yet.
    let addrs = if rc == 0 && overlapped.is_null() && !result.is_null() {
        read::addrinfos(unsafe { *result } as *const u8, read::ADDRINFOEX_NEXT)
    } else {
        String::new()
    };
    log::throttled(&format!("dns GetAddrInfoExW {host}"), 4, |n| {
        log!("dns GetAddrInfoExW n={n} host={host} service={service_name} -> {rc} {addrs}");
    });
    rc
}

unsafe extern "system" fn detour_dnsa(
    name: *const i8,
    ty: u16,
    options: u32,
    extra: *mut core::ffi::c_void,
    results: *mut *mut core::ffi::c_void,
    reserved: *mut *mut core::ffi::c_void,
) -> i32 {
    let Some(hook) = DNSA.get() else { return 9003 };
    let host = host_a(name);
    if !results.is_null()
        && let Some(answer) = dns_answer(&host, ty, name.cast(), CHARSET_ANSI)
    {
        log!("dns DnsQuery_A host={host} type={ty} -> answered here");
        return match answer {
            Ok(r) => {
                unsafe { *results = r.cast() };
                0
            }
            Err(code) => code,
        };
    }
    let rc = unsafe { hook.call(name, ty, options, extra, results, reserved) };
    log::throttled(&format!("dns DnsQuery_A {host} {ty}"), 4, |n| {
        log!("dns DnsQuery_A n={n} host={host} type={ty} -> {rc}");
    });
    rc
}

unsafe extern "system" fn detour_dnsw(
    name: *const u16,
    ty: u16,
    options: u32,
    extra: *mut core::ffi::c_void,
    results: *mut *mut core::ffi::c_void,
    reserved: *mut *mut core::ffi::c_void,
) -> i32 {
    let Some(hook) = DNSW.get() else { return 9003 };
    let host = host_w(name);
    if !results.is_null()
        && let Some(answer) = dns_answer(&host, ty, name.cast(), CHARSET_UNICODE)
    {
        log!("dns DnsQuery_W host={host} type={ty} -> answered here");
        return match answer {
            Ok(r) => {
                unsafe { *results = r.cast() };
                0
            }
            Err(code) => code,
        };
    }
    let rc = unsafe { hook.call(name, ty, options, extra, results, reserved) };
    log::throttled(&format!("dns DnsQuery_W {host} {ty}"), 4, |n| {
        log!("dns DnsQuery_W n={n} host={host} type={ty} -> {rc}");
    });
    rc
}

unsafe extern "system" fn detour_gaiexa(
    name: *const i8,
    service: *const i8,
    namespace: u32,
    nsp: *mut core::ffi::c_void,
    hints: *const core::ffi::c_void,
    result: *mut *mut u8,
    timeout: *mut core::ffi::c_void,
    overlapped: *mut core::ffi::c_void,
    completion: *mut core::ffi::c_void,
    handle: *mut *mut core::ffi::c_void,
) -> i32 {
    let Some(hook) = GAIEXA.get() else {
        return 11001;
    };
    let host = host_a(name);
    let name = redirect_a(&host).unwrap_or(name);
    let rc = unsafe { hook.call(name, service, namespace, nsp, hints, result, timeout, overlapped, completion, handle) };
    log::throttled(&format!("dns GetAddrInfoExA {host}"), 4, |n| {
        log!("dns GetAddrInfoExA n={n} host={host} -> {rc}");
    });
    rc
}

unsafe extern "system" fn detour_ghbn(name: *const i8) -> *mut u8 {
    let Some(hook) = GHBN.get() else {
        return std::ptr::null_mut();
    };
    let host = host_a(name);
    // gethostbyname takes a dotted address too.
    let name = redirect_a(&host).unwrap_or(name);
    let r = unsafe { hook.call(name) };
    log::throttled(&format!("dns gethostbyname {host}"), 4, |n| {
        log!("dns gethostbyname n={n} host={host} -> {}", if r.is_null() { "failed" } else { "found" });
    });
    r
}

unsafe extern "system" fn detour_dnsex(req: *const DnsQueryRequest, res: *mut DnsQueryResult, cancel: *mut core::ffi::c_void) -> i32 {
    let Some(hook) = DNSEX.get() else { return 9003 };
    if !req.is_null() && !res.is_null() {
        let (name, ty) = unsafe { ((*req).name, (*req).ty) };
        let host = host_w(name);
        if let Some(answer) = dns_answer(&host, ty, name.cast(), CHARSET_UNICODE) {
            // Answered at once: a request with a callback may complete synchronously.
            log!("dns DnsQueryEx host={host} type={ty} -> answered here");
            let (status, records) = match answer {
                Ok(r) => (0, r),
                Err(code) => (code, std::ptr::null_mut()),
            };
            unsafe {
                (*res).status = status;
                (*res).records = records;
            }
            return status;
        }
    }
    unsafe { hook.call(req, res, cancel) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record dnsapi makes from ours reads back as an A record for the name and
    /// address, and frees as any DnsQuery answer does.
    #[test]
    fn an_answer_is_a_record_dnsapi_made() {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        let lib: Vec<u16> = "dnsapi.dll".encode_utf16().chain(Some(0)).collect();
        let m = unsafe { LoadLibraryW(lib.as_ptr()) };
        assert!(!m.is_null());
        let copy: RecordCopyEx = unsafe { std::mem::transmute(GetProcAddress(m, c"DnsRecordCopyEx".as_ptr().cast()).unwrap()) };
        let free: unsafe extern "system" fn(*mut core::ffi::c_void, u32) = unsafe { std::mem::transmute(GetProcAddress(m, c"DnsFree".as_ptr().cast()).unwrap()) };
        let name: Vec<u16> = "titleid.playfabapi.com".encode_utf16().chain(Some(0)).collect();
        let r = a_record(copy, std::net::Ipv4Addr::new(127, 80, 1, 2), name.as_ptr().cast(), CHARSET_UNICODE).expect("a record");
        let rec = unsafe { &*r };
        assert_eq!((rec.ty, rec.data_len), (DNS_TYPE_A, 4));
        assert_eq!(rec.ip.to_ne_bytes(), [127, 80, 1, 2]);
        assert_ne!(rec.name, name.as_ptr().cast(), "the name is dnsapi's copy");
        assert_eq!(read::wstr(rec.name as usize).as_deref(), Some("titleid.playfabapi.com"));
        // DnsFreeRecordList.
        unsafe { free(r.cast(), 1) };
    }
}
