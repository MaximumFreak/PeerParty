//! DNS lookup hooks. The hostname is the thing phase 0 has to record: which
//! online services the game actually contacts. Lookups are unchanged, except that
//! in a game started through PeerParty PlayFab's hosts resolve to PeerParty's local PlayFab service
//! (playfab/mod.rs) and Photon Cloud's to the LAN's Photon servers (photon/mod.rs):
//! the lookup runs on that address's text instead.

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
    let rc = unsafe { hook.call(name, ty, options, extra, results, reserved) };
    log::throttled(&format!("dns DnsQuery_W {host} {ty}"), 4, |n| {
        log!("dns DnsQuery_W n={n} host={host} type={ty} -> {rc}");
    });
    rc
}
