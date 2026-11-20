//! The game's own HTTPS client checking PeerParty's certificate, when that client is
//! Windows' SChannel with automatic validation (libcurl's SChannel back end asks for
//! `SCH_CRED_AUTO_CRED_VALIDATION` and a revocation check). That check doesn't go
//! through the in-process `CertGetCertificateChain` hook (playfab/tls.rs), so it
//! refuses PeerParty's self-signed certificate and the game hangs up.
//!
//! sspicli's `AcquireCredentialsHandleA/W` and `InitializeSecurityContextA/W` are
//! hooked. While the Demonware stand-in is in use, each outbound SChannel credential
//! with automatic validation gets a twin with manual validation (and no revocation
//! check). A connection to a Demonware host (whose lookups resolve to PeerParty's own
//! loopback server) uses the twin; every other connection, and every credential before
//! the stand-in is in use, is untouched and keeps Windows' check.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;
use windows_sys::Win32::Security::Authentication::Identity::{SCHANNEL_CRED, SecBufferDesc};
use windows_sys::Win32::Security::Credentials::SecHandle;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;

type AcquireA = unsafe extern "system" fn(
    *const i8,
    *const i8,
    u32,
    *const c_void,
    *const c_void,
    *const c_void,
    *const c_void,
    *mut SecHandle,
    *mut i64,
) -> i32;
type AcquireW = unsafe extern "system" fn(
    *const u16,
    *const u16,
    u32,
    *const c_void,
    *const c_void,
    *const c_void,
    *const c_void,
    *mut SecHandle,
    *mut i64,
) -> i32;
type IscA = unsafe extern "system" fn(
    *mut SecHandle,
    *mut SecHandle,
    *const i8,
    u32,
    u32,
    u32,
    *const SecBufferDesc,
    u32,
    *mut SecHandle,
    *mut SecBufferDesc,
    *mut u32,
    *mut i64,
) -> i32;
type IscW = unsafe extern "system" fn(
    *mut SecHandle,
    *mut SecHandle,
    *const u16,
    u32,
    u32,
    u32,
    *const SecBufferDesc,
    u32,
    *mut SecHandle,
    *mut SecBufferDesc,
    *mut u32,
    *mut i64,
) -> i32;

static ACQ_A: OnceLock<GenericDetour<AcquireA>> = OnceLock::new();
static ACQ_W: OnceLock<GenericDetour<AcquireW>> = OnceLock::new();
static ISC_A: OnceLock<GenericDetour<IscA>> = OnceLock::new();
static ISC_W: OnceLock<GenericDetour<IscW>> = OnceLock::new();

const SECPKG_CRED_OUTBOUND: u32 = 2;
const SCHANNEL_CRED_VERSION: u32 = 4;
const SCH_CRED_MANUAL_CRED_VALIDATION: u32 = 0x8;
const SCH_CRED_AUTO_CRED_VALIDATION: u32 = 0x20;
const SCH_CRED_REVOCATION_CHECK_END_CERT: u32 = 0x100;
const SCH_CRED_REVOCATION_CHECK_CHAIN: u32 = 0x200;
const SCH_CRED_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT: u32 = 0x400;
const SCH_CRED_IGNORE_NO_REVOCATION_CHECK: u32 = 0x800;
const SCH_CRED_IGNORE_REVOCATION_OFFLINE: u32 = 0x1000;

type Handle = (usize, usize);

fn key(h: *const SecHandle) -> Option<Handle> {
    if h.is_null() {
        return None;
    }
    let h = unsafe { &*h };
    (h.dwLower != 0 || h.dwUpper != 0).then_some((h.dwLower, h.dwUpper))
}

/// Credential -> its manual-validation twin.
static TWINS: Mutex<Option<HashMap<Handle, SecHandle>>> = Mutex::new(None);
/// Contexts started on a twin (their later calls use the twin too).
static OURS: Mutex<Option<HashSet<Handle>>> = Mutex::new(None);

/// The flags of an automatic-validation credential's twin: manual validation, no
/// revocation check.
pub fn twin_flags(flags: u32) -> Option<u32> {
    if flags & SCH_CRED_AUTO_CRED_VALIDATION == 0 {
        return None;
    }
    let off = SCH_CRED_AUTO_CRED_VALIDATION
        | SCH_CRED_REVOCATION_CHECK_END_CERT
        | SCH_CRED_REVOCATION_CHECK_CHAIN
        | SCH_CRED_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT;
    Some(flags & !off | SCH_CRED_MANUAL_CRED_VALIDATION | SCH_CRED_IGNORE_NO_REVOCATION_CHECK | SCH_CRED_IGNORE_REVOCATION_OFFLINE)
}

fn is_schannel(package: &str) -> bool {
    package.eq_ignore_ascii_case("Microsoft Unified Security Protocol Provider") || package.eq_ignore_ascii_case("Schannel")
}

/// The twin's SCHANNEL_CRED for `auth`, if it's an outbound one with automatic validation.
fn twin_cred(use_: u32, auth: *const c_void) -> Option<SCHANNEL_CRED> {
    if use_ & SECPKG_CRED_OUTBOUND == 0 || auth.is_null() || !super::active() {
        return None;
    }
    let mut sc: SCHANNEL_CRED = unsafe { std::ptr::read_unaligned(auth as *const SCHANNEL_CRED) };
    if sc.dwVersion != SCHANNEL_CRED_VERSION {
        return None;
    }
    sc.dwFlags = twin_flags(sc.dwFlags)?;
    Some(sc)
}

fn remember_twin(real: *const SecHandle, twin: SecHandle) {
    if let Some(k) = key(real) {
        TWINS.lock().unwrap().get_or_insert_with(HashMap::new).insert(k, twin);
    }
}

unsafe fn c_text(p: *const i8) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

unsafe fn w_text(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 && n < 4096 {
        n += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

unsafe extern "system" fn acquire_a(
    principal: *const i8,
    package: *const i8,
    use_: u32,
    logon: *const c_void,
    auth: *const c_void,
    get_key: *const c_void,
    arg: *const c_void,
    cred: *mut SecHandle,
    expiry: *mut i64,
) -> i32 {
    let orig = ACQ_A.get().unwrap();
    let r = unsafe { orig.call(principal, package, use_, logon, auth, get_key, arg, cred, expiry) };
    if r == 0 && is_schannel(&unsafe { c_text(package) }) {
        if let Some(sc) = twin_cred(use_, auth) {
            let mut twin: SecHandle = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                orig.call(principal, package, use_, logon, &sc as *const _ as *const c_void, get_key, arg, &mut twin, std::ptr::null_mut())
            };
            if ok == 0 {
                remember_twin(cred, twin);
            }
        }
    }
    r
}

unsafe extern "system" fn acquire_w(
    principal: *const u16,
    package: *const u16,
    use_: u32,
    logon: *const c_void,
    auth: *const c_void,
    get_key: *const c_void,
    arg: *const c_void,
    cred: *mut SecHandle,
    expiry: *mut i64,
) -> i32 {
    let orig = ACQ_W.get().unwrap();
    let r = unsafe { orig.call(principal, package, use_, logon, auth, get_key, arg, cred, expiry) };
    if r == 0 && is_schannel(&unsafe { w_text(package) }) {
        if let Some(sc) = twin_cred(use_, auth) {
            let mut twin: SecHandle = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                orig.call(principal, package, use_, logon, &sc as *const _ as *const c_void, get_key, arg, &mut twin, std::ptr::null_mut())
            };
            if ok == 0 {
                remember_twin(cred, twin);
            }
        }
    }
    r
}

/// The credential a context call should use: the twin for a connection to a
/// Demonware host (or a context already started on the twin), else `cred` itself.
fn credential_for(cred: *mut SecHandle, ctx: *mut SecHandle, target: &str) -> (*mut SecHandle, bool) {
    let Some(k) = key(cred) else { return (cred, false) };
    let started_here = key(ctx).is_some_and(|c| OURS.lock().unwrap().as_ref().is_some_and(|s| s.contains(&c)));
    let first_to_demonware = key(ctx).is_none() && super::active() && super::is_host(target);
    if !(started_here || first_to_demonware) {
        return (cred, false);
    }
    let mut twins = TWINS.lock().unwrap();
    match twins.as_mut().and_then(|t| t.get_mut(&k)) {
        Some(t) => {
            if first_to_demonware {
                crate::log::throttled(&format!("demonware tls {target}"), 2, |n| {
                    log!("demonware tls: {target} is PeerParty's own server; the game's certificate check for it is PeerParty's n={n}")
                });
            }
            (t as *mut SecHandle, true)
        }
        None => (cred, false),
    }
}

fn note_context(new_ctx: *mut SecHandle) {
    if let Some(c) = key(new_ctx) {
        OURS.lock().unwrap().get_or_insert_with(HashSet::new).insert(c);
    }
}

unsafe extern "system" fn isc_a(
    cred: *mut SecHandle,
    ctx: *mut SecHandle,
    target: *const i8,
    req: u32,
    reserved: u32,
    rep: u32,
    input: *const SecBufferDesc,
    reserved2: u32,
    new_ctx: *mut SecHandle,
    output: *mut SecBufferDesc,
    attrs: *mut u32,
    expiry: *mut i64,
) -> i32 {
    let (use_cred, twin) = credential_for(cred, ctx, &unsafe { c_text(target) });
    let r = unsafe { ISC_A.get().unwrap().call(use_cred, ctx, target, req, reserved, rep, input, reserved2, new_ctx, output, attrs, expiry) };
    if twin {
        note_context(new_ctx);
    }
    r
}

unsafe extern "system" fn isc_w(
    cred: *mut SecHandle,
    ctx: *mut SecHandle,
    target: *const u16,
    req: u32,
    reserved: u32,
    rep: u32,
    input: *const SecBufferDesc,
    reserved2: u32,
    new_ctx: *mut SecHandle,
    output: *mut SecBufferDesc,
    attrs: *mut u32,
    expiry: *mut i64,
) -> i32 {
    let (use_cred, twin) = credential_for(cred, ctx, &unsafe { w_text(target) });
    let r = unsafe { ISC_W.get().unwrap().call(use_cred, ctx, target, req, reserved, rep, input, reserved2, new_ctx, output, attrs, expiry) };
    if twin {
        note_context(new_ctx);
    }
    r
}

/// sspicli loaded: hooks its credential and client-context calls. How many are hooked.
pub fn attach(exports: &[Export]) -> usize {
    let find = |name: &str| exports.iter().find(|e| e.name == name).map(|e| e.addr as usize);
    let mut n = 0;
    with_lock(|| {
        macro_rules! hook {
            ($cell:ident, $ty:ty, $name:literal, $detour:ident) => {
                if $cell.get().is_none() {
                    if let Some(addr) = find($name) {
                        let target: $ty = unsafe { std::mem::transmute(addr) };
                        if let Ok(h) = unsafe { GenericDetour::new(target, $detour) } {
                            if unsafe { h.enable() }.is_ok() && $cell.set(h).is_ok() {
                                n += 1;
                            }
                        }
                    }
                }
            };
        }
        hook!(ACQ_A, AcquireA, "AcquireCredentialsHandleA", acquire_a);
        hook!(ACQ_W, AcquireW, "AcquireCredentialsHandleW", acquire_w);
        hook!(ISC_A, IscA, "InitializeSecurityContextA", isc_a);
        hook!(ISC_W, IscW, "InitializeSecurityContextW", isc_w);
    });
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_automatic_validation_gets_a_manual_twin() {
        // libcurl's SChannel with peer verification on.
        let twin = twin_flags(SCH_CRED_AUTO_CRED_VALIDATION | SCH_CRED_REVOCATION_CHECK_CHAIN).unwrap();
        assert_eq!(twin & SCH_CRED_AUTO_CRED_VALIDATION, 0);
        assert_eq!(twin & SCH_CRED_REVOCATION_CHECK_CHAIN, 0);
        assert_ne!(twin & SCH_CRED_MANUAL_CRED_VALIDATION, 0);
        assert_ne!(twin & SCH_CRED_IGNORE_NO_REVOCATION_CHECK, 0);
        // Other flags stay.
        assert_ne!(twin_flags(SCH_CRED_AUTO_CRED_VALIDATION | 0x4).unwrap() & 0x4, 0);
        // A credential that already validates by hand is left alone.
        assert_eq!(twin_flags(SCH_CRED_MANUAL_CRED_VALIDATION), None);
    }

    #[test]
    fn only_demonware_connections_use_the_twin() {
        crate::demonware::set_active();
        let mut cred = SecHandle { dwLower: 0x1111, dwUpper: 0x2222 };
        let twin = SecHandle { dwLower: 0x3333, dwUpper: 0x4444 };
        remember_twin(&cred, twin);
        let mut none = SecHandle { dwLower: 0, dwUpper: 0 };
        let (c, used) = credential_for(&mut cred, &mut none, "title-pc-auth3.prod.demonware.net");
        assert!(used);
        assert_eq!(unsafe { (*c).dwLower }, 0x3333);
        let (c, used) = credential_for(&mut cred, &mut none, "www.example.com");
        assert!(!used);
        assert_eq!(unsafe { (*c).dwLower }, 0x1111);
        // A context started on the twin keeps it, whatever name its later calls give.
        let mut ctx = SecHandle { dwLower: 0x5555, dwUpper: 1 };
        note_context(&mut ctx);
        let (c, used) = credential_for(&mut cred, &mut ctx, "");
        assert!(used);
        assert_eq!(unsafe { (*c).dwLower }, 0x3333);
    }

    /// A client like libcurl's SChannel back end (automatic validation with a
    /// revocation check) reaches PeerParty's HTTPS server under a Demonware name, and
    /// still refuses it under any other name.
    #[test]
    fn a_curl_like_schannel_client_accepts_peerpartys_server_for_demonware_only() {
        use std::net::TcpStream;
        use windows_sys::Win32::Security::Authentication::Identity::{AcquireCredentialsHandleW, FreeCredentialsHandle, UNISP_NAME_W};
        let ip = crate::playfab::server().expect("service started");
        let sspicli = crate::watch::modules()
            .into_iter()
            .find(|(_, n)| n.eq_ignore_ascii_case("sspicli.dll"))
            .expect("sspicli loaded");
        let table = unsafe { crate::pe::exports(sspicli.0 as *const u8).unwrap() };
        assert_eq!(attach(&table.code), 4);
        crate::demonware::set_active();
        let try_host = |host: &str| -> std::io::Result<()> {
            let mut sc: SCHANNEL_CRED = unsafe { std::mem::zeroed() };
            sc.dwVersion = SCHANNEL_CRED_VERSION;
            sc.dwFlags = SCH_CRED_AUTO_CRED_VALIDATION | SCH_CRED_REVOCATION_CHECK_CHAIN;
            let mut creds = SecHandle { dwLower: 0, dwUpper: 0 };
            let rc = unsafe {
                AcquireCredentialsHandleW(
                    std::ptr::null(),
                    UNISP_NAME_W,
                    SECPKG_CRED_OUTBOUND,
                    std::ptr::null(),
                    &sc as *const _ as *const _,
                    None,
                    std::ptr::null(),
                    &mut creds,
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, 0);
            let mut sock = TcpStream::connect((ip, 443))?;
            sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
            let target: Vec<u16> = host.encode_utf16().chain(Some(0)).collect();
            let mut ctx = SecHandle { dwLower: 0, dwUpper: 0 };
            let mut incoming = Vec::new();
            let r = crate::playfab::tls::client_handshake(&mut sock, &creds, &target, &mut ctx, false, &mut incoming);
            unsafe { FreeCredentialsHandle(&creds) };
            r
        };
        try_host("title-pc-auth3.prod.demonware.net").expect("handshake under a Demonware name");
        assert!(try_host("www.example.com").is_err(), "any other name keeps Windows' check");
    }
}
