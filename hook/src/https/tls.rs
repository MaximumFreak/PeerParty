//! TLS for the local HTTPS server (mod.rs), with nothing but Windows' own crypto.
//!
//! - A self-signed certificate for every name the services on the server answer for
//!   (`HttpsService::cert_names`: `*.playfabapi.com`, Unity Gaming Services' hosts,
//!   Demonware's sign-in hosts, the Xbox Live friends hosts), made once per process. Its
//!   RSA key lives in the user's key container `peerparty-playfab` (SChannel needs a
//!   persisted key; it's reused on later runs).
//! - An SChannel server stream (`AcceptSecurityContext` / `EncryptMessage` /
//!   `DecryptMessage`) over a loopback TCP connection; the client side (client.rs) uses
//!   the same stream.
//! - Trust: crypt32's `CertGetCertificateChain` is hooked, and only for that exact
//!   certificate it builds the chain with an engine whose sole root is the certificate
//!   itself. Every other chain, and every other process, is untouched; nothing goes into
//!   a certificate store.
//! - Trust for Unity's TLS (UnityTls verifies against the root list it reads itself):
//!   crypt32's `CertOpenStore` is hooked, and a ROOT system store the game opens comes
//!   back as a collection of the real store and an in-memory store with the certificate.
//!   Windows' stores aren't written; see "trust for Unity's own TLS" below.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::ptr;
use std::sync::OnceLock;

use retour::GenericDetour;
use windows_sys::Win32::Security::Authentication::Identity::{
    AcceptSecurityContext, AcquireCredentialsHandleW, DecryptMessage, DeleteSecurityContext,
    EncryptMessage, FreeContextBuffer, FreeCredentialsHandle,
    QueryContextAttributesW, SCHANNEL_CRED, SecBuffer, SecBufferDesc, SecPkgContext_StreamSizes,
    UNISP_NAME_W,
};
use windows_sys::Win32::Security::Credentials::SecHandle;
use windows_sys::Win32::Security::Cryptography::{
    CERT_ALT_NAME_ENTRY, CERT_ALT_NAME_ENTRY_0, CERT_ALT_NAME_INFO, CERT_CHAIN_ENGINE_CONFIG,
    CERT_CONTEXT, CERT_EXTENSION, CERT_EXTENSIONS, CRYPT_ALGORITHM_IDENTIFIER,
    CRYPT_INTEGER_BLOB, CRYPT_KEY_PROV_INFO, CertAddCertificateContextToStore,
    CertCreateCertificateChainEngine, CertCreateSelfSignCertificate, CertOpenStore,
    CertStrToNameW, CryptAcquireContextW, CryptEncodeObjectEx, CryptGenKey, CryptGetUserKey,
    MS_ENH_RSA_AES_PROV_W, X509_ALTERNATE_NAME, CertFreeCertificateContext,
};

use crate::sys::hook::with_lock;
use crate::sys::win::wide;
use crate::log;
use crate::sys::pe::Export;

const SUBJECT: &str = "CN=*.playfabapi.com";
const CONTAINER: &str = "peerparty-playfab";

const X509_ASN_ENCODING: u32 = 1;
const CERT_X500_NAME_STR: u32 = 3;
const PROV_RSA_AES: u32 = 24;
const CRYPT_NEWKEYSET: u32 = 0x8;
/// NTE_EXISTS: the key set is already there.
const NTE_EXISTS: i32 = 0x8009000F_u32 as i32;
const AT_KEYEXCHANGE: u32 = 1;
const CERT_ALT_NAME_DNS_NAME: u32 = 3;
const CERT_STORE_PROV_MEMORY: usize = 2;
const CERT_STORE_ADD_ALWAYS: u32 = 4;
const SECPKG_CRED_INBOUND: u32 = 1;
const SP_PROT_TLS1_2_SERVER: u32 = 0x400;
pub(super) const SECPKG_ATTR_STREAM_SIZES: u32 = 4;

pub(super) const SECBUFFER_EMPTY: u32 = 0;
const SECBUFFER_DATA: u32 = 1;
pub(super) const SECBUFFER_TOKEN: u32 = 2;
pub(super) const SECBUFFER_EXTRA: u32 = 5;
const SECBUFFER_STREAM_TRAILER: u32 = 6;
const SECBUFFER_STREAM_HEADER: u32 = 7;

const ASC_FLAGS: u32 = 0x4 // replay detect
    | 0x8 // sequence detect
    | 0x10 // confidentiality
    | 0x100 // allocate memory
    | 0x8000 // extended error
    | 0x10000; // stream

pub(super) const SEC_E_OK: i32 = 0;
pub(super) const SEC_I_CONTINUE_NEEDED: i32 = 0x0009_0312;
const SEC_I_CONTEXT_EXPIRED: i32 = 0x0009_0317;
pub(super) const SEC_E_INCOMPLETE_MESSAGE: i32 = 0x8009_0318_u32 as i32;
/// DecryptMessage: a handshake message came (TLS 1.3's key updates and tickets, a TLS
/// 1.2 renegotiation): it goes back through AcceptSecurityContext.
const SEC_I_RENEGOTIATE: i32 = 0x0009_0321;
/// The most handshake bytes held while waiting for a message to be whole.
pub(super) const MAX_HANDSHAKE: usize = 64 * 1024;

struct Cert {
    #[cfg_attr(not(test), allow(dead_code))]
    context: *mut CERT_CONTEXT,
    encoded: Vec<u8>,
    engine: isize,
    credentials: SecHandle,
}

// Read-only after creation; SChannel and crypt32 handles are usable from any thread.
unsafe impl Send for Cert {}
unsafe impl Sync for Cert {}

static CERT: OnceLock<Option<Cert>> = OnceLock::new();

pub(super) fn os_error(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
}

/// The certificate and server credentials, made on first use.
fn cert() -> Option<&'static Cert> {
    CERT.get_or_init(|| match make_cert() {
        Ok(c) => {
            log!(
                "https tls certificate ready ({} bytes, key container {CONTAINER})",
                c.encoded.len()
            );
            Some(c)
        }
        Err(e) => {
            log!("https tls unavailable: {e}{}", crate::sys::wine::tag());
            None
        }
    })
    .as_ref()
}

pub fn ready() -> bool {
    cert().is_some()
}

fn make_cert() -> Result<Cert, String> {
    let container = wide(CONTAINER);
    let mut prov = 0usize;
    unsafe {
        let open = |prov: &mut usize, flags: u32| CryptAcquireContextW(prov, container.as_ptr(), MS_ENH_RSA_AES_PROV_W, PROV_RSA_AES, flags) != 0;
        // The key set is the Windows account's, shared by every process: another one
        // (a second copy of the game, another test run) can make it between our open
        // and our create. Then it's there to open.
        if !open(&mut prov, 0) && !open(&mut prov, CRYPT_NEWKEYSET) {
            let made = io::Error::last_os_error();
            if made.raw_os_error() != Some(NTE_EXISTS) || !open(&mut prov, 0) {
                return Err(format!("CryptAcquireContext: {made}"));
            }
            log!("https: another process made the key set at the same moment; using it");
        }
        let mut key = 0usize;
        if CryptGetUserKey(prov, AT_KEYEXCHANGE, &mut key) == 0
            && CryptGenKey(prov, AT_KEYEXCHANGE, 2048 << 16, &mut key) == 0
        {
            return Err(os_error("CryptGenKey"));
        }
    }

    // Subject name.
    let subject = wide(SUBJECT);
    let mut name_len = 0u32;
    let mut name;
    unsafe {
        if CertStrToNameW(
            X509_ASN_ENCODING,
            subject.as_ptr(),
            CERT_X500_NAME_STR,
            ptr::null(),
            ptr::null_mut(),
            &mut name_len,
            ptr::null_mut(),
        ) == 0
        {
            return Err(os_error("CertStrToName"));
        }
        name = vec![0u8; name_len as usize];
        if CertStrToNameW(
            X509_ASN_ENCODING,
            subject.as_ptr(),
            CERT_X500_NAME_STR,
            ptr::null(),
            name.as_mut_ptr(),
            &mut name_len,
            ptr::null_mut(),
        ) == 0
        {
            return Err(os_error("CertStrToName"));
        }
    }
    let name_blob = CRYPT_INTEGER_BLOB {
        cbData: name_len,
        pbData: name.as_mut_ptr(),
    };

    // subjectAltName: every name the services on this server answer for
    // (`HttpsService::cert_names`: PlayFab's, Unity Gaming Services', Demonware's sign-in
    // hosts and the Xbox Live friends hosts, each named by its own module).
    let mut dns: Vec<Vec<u16>> = super::cert_names().iter().map(|n| wide(n)).collect();
    let mut entries: Vec<CERT_ALT_NAME_ENTRY> = dns
        .iter_mut()
        .map(|d| CERT_ALT_NAME_ENTRY {
            dwAltNameChoice: CERT_ALT_NAME_DNS_NAME,
            Anonymous: CERT_ALT_NAME_ENTRY_0 {
                pwszDNSName: d.as_mut_ptr(),
            },
        })
        .collect();
    let alt = CERT_ALT_NAME_INFO {
        cAltEntry: entries.len() as u32,
        rgAltEntry: entries.as_mut_ptr(),
    };
    let mut alt_len = 0u32;
    let mut alt_der;
    unsafe {
        if CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            X509_ALTERNATE_NAME,
            &alt as *const _ as *const _,
            0,
            ptr::null(),
            ptr::null_mut(),
            &mut alt_len,
        ) == 0
        {
            return Err(os_error("CryptEncodeObjectEx"));
        }
        alt_der = vec![0u8; alt_len as usize];
        if CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            X509_ALTERNATE_NAME,
            &alt as *const _ as *const _,
            0,
            ptr::null(),
            alt_der.as_mut_ptr().cast(),
            &mut alt_len,
        ) == 0
        {
            return Err(os_error("CryptEncodeObjectEx"));
        }
    }
    let mut ext = CERT_EXTENSION {
        pszObjId: c"2.5.29.17".as_ptr() as *mut u8,
        fCritical: 0,
        Value: CRYPT_INTEGER_BLOB {
            cbData: alt_len,
            pbData: alt_der.as_mut_ptr(),
        },
    };
    let exts = CERT_EXTENSIONS {
        cExtension: 1,
        rgExtension: &mut ext,
    };

    let mut container_w = wide(CONTAINER);
    let mut provider_w = wide("Microsoft Enhanced RSA and AES Cryptographic Provider");
    let key_info = CRYPT_KEY_PROV_INFO {
        pwszContainerName: container_w.as_mut_ptr(),
        pwszProvName: provider_w.as_mut_ptr(),
        dwProvType: PROV_RSA_AES,
        dwFlags: 0,
        cProvParam: 0,
        rgProvParam: ptr::null_mut(),
        dwKeySpec: AT_KEYEXCHANGE,
    };
    let alg = CRYPT_ALGORITHM_IDENTIFIER {
        pszObjId: c"1.2.840.113549.1.1.11".as_ptr() as *mut u8, // sha256RSA
        Parameters: CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        },
    };
    let context = unsafe {
        CertCreateSelfSignCertificate(
            prov,
            &name_blob,
            0,
            &key_info,
            &alg,
            ptr::null(),
            ptr::null(),
            &exts,
        )
    };
    if context.is_null() {
        return Err(os_error("CertCreateSelfSignCertificate"));
    }
    // A crypt32 other than Windows' (Wine's) is taken at its word only this far: a
    // certificate with no encoding is no certificate.
    if unsafe { (*context).pbCertEncoded.is_null() || (*context).cbCertEncoded == 0 } {
        unsafe { CertFreeCertificateContext(context) };
        return Err("CertCreateSelfSignCertificate made an empty certificate".into());
    }
    let encoded = unsafe {
        std::slice::from_raw_parts((*context).pbCertEncoded, (*context).cbCertEncoded as usize)
            .to_vec()
    };

    // A chain engine whose only root is this certificate.
    let store = unsafe { CertOpenStore(CERT_STORE_PROV_MEMORY as _, 0, 0, 0, ptr::null()) };
    if store.is_null() {
        return Err(os_error("CertOpenStore"));
    }
    if unsafe { CertAddCertificateContextToStore(store, context, CERT_STORE_ADD_ALWAYS, ptr::null_mut()) }
        == 0
    {
        return Err(os_error("CertAddCertificateContextToStore"));
    }
    let mut config: CERT_CHAIN_ENGINE_CONFIG = unsafe { std::mem::zeroed() };
    config.cbSize = std::mem::size_of::<CERT_CHAIN_ENGINE_CONFIG>() as u32;
    config.hExclusiveRoot = store;
    let mut engine = ptr::null_mut();
    if unsafe { CertCreateCertificateChainEngine(&config, &mut engine) } == 0 {
        return Err(os_error("CertCreateCertificateChainEngine"));
    }

    // SChannel server credentials.
    let mut creds_cert = context;
    let mut sc: SCHANNEL_CRED = unsafe { std::mem::zeroed() };
    sc.dwVersion = 4;
    sc.cCreds = 1;
    sc.paCred = &mut creds_cert;
    sc.grbitEnabledProtocols = SP_PROT_TLS1_2_SERVER;
    let mut credentials = SecHandle {
        dwLower: 0,
        dwUpper: 0,
    };
    let rc = unsafe {
        AcquireCredentialsHandleW(
            ptr::null(),
            UNISP_NAME_W,
            SECPKG_CRED_INBOUND,
            ptr::null(),
            &sc as *const _ as *const _,
            None,
            ptr::null(),
            &mut credentials,
            ptr::null_mut(),
        )
    };
    if rc != SEC_E_OK {
        return Err(format!("AcquireCredentialsHandle {rc:#x}"));
    }
    Ok(Cert {
        context,
        encoded,
        engine: engine as isize,
        credentials,
    })
}

// ---- trust: crypt32's CertGetCertificateChain ----

type GetChainFn = unsafe extern "system" fn(
    isize,
    *const CERT_CONTEXT,
    *const core::ffi::c_void,
    *mut core::ffi::c_void,
    *const core::ffi::c_void,
    u32,
    *const core::ffi::c_void,
    *mut *mut core::ffi::c_void,
) -> i32;

static GET_CHAIN: OnceLock<GenericDetour<GetChainFn>> = OnceLock::new();

/// Where crypt32's `CertGetCertificateChain` is, noted when crypt32 loads.
static CHAIN_TARGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Notes crypt32's `CertGetCertificateChain` without touching it; it's hooked only once
/// the local HTTPS server starts (`arm_crypt32`), for compatibility, and then changes
/// the chain only for PeerParty's own certificate.
pub fn note_crypt32(exports: &[Export]) -> bool {
    let Some(e) = exports.iter().find(|e| e.name == "CertGetCertificateChain") else {
        return false;
    };
    CHAIN_TARGET.store(e.addr as usize, std::sync::atomic::Ordering::Relaxed);
    true
}

/// Hooks the noted `CertGetCertificateChain` (once). 1 if it's hooked now.
pub fn arm_crypt32() -> usize {
    if GET_CHAIN.get().is_some() {
        return 1;
    }
    let addr = CHAIN_TARGET.load(std::sync::atomic::Ordering::Relaxed);
    if addr == 0 {
        return 0;
    }
    let target: GetChainFn = unsafe { std::mem::transmute(addr) };
    with_lock(|| {
        let Ok(hook) = (unsafe { GenericDetour::new(target, detour_get_chain) }) else {
            log!("https CertGetCertificateChain not hooked");
            return 0;
        };
        if GET_CHAIN.set(hook).is_err() {
            return 0;
        }
        usize::from(unsafe { GET_CHAIN.get().unwrap().enable() }.is_ok())
    })
}

/// Whether a certificate is the server's own (for the GDK's pin check, xboxlive/ui.rs,
/// and the client's refusal to talk to itself).
pub fn is_peerparty(context: *const CERT_CONTEXT) -> bool {
    is_ours(context).is_some()
}

/// The certificate's public key as curl's `CURLOPT_PINNEDPUBLICKEY` takes it: `sha256//`
/// and the base64 of the SHA-256 of its DER SubjectPublicKeyInfo. A curl whose own
/// certificate checks are off for PeerParty's hosts still talks only to the server with
/// this key (curl/tls.rs).
pub fn curl_pin() -> Option<&'static std::ffi::CStr> {
    static PIN: OnceLock<Option<std::ffi::CString>> = OnceLock::new();
    PIN.get_or_init(|| {
        let der = public_key_info(cert()?)?;
        let hash = crate::util::crypto::sha256(&der);
        std::ffi::CString::new(format!("sha256//{}", crate::util::encoding::base64(&hash))).ok()
    })
    .as_deref()
}

/// The certificate's SubjectPublicKeyInfo, DER-encoded.
fn public_key_info(cert: &Cert) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::X509_PUBLIC_KEY_INFO;
    if cert.context.is_null() {
        return None;
    }
    // SAFETY: the context is the certificate made in make_cert, never freed.
    let info = unsafe { (*cert.context).pCertInfo };
    if info.is_null() {
        return None;
    }
    let spki = unsafe { &raw const (*info).SubjectPublicKeyInfo };
    let mut len = 0u32;
    unsafe {
        if CryptEncodeObjectEx(X509_ASN_ENCODING, X509_PUBLIC_KEY_INFO, spki.cast(), 0, ptr::null(), ptr::null_mut(), &mut len) == 0 {
            return None;
        }
        let mut der = vec![0u8; len as usize];
        if CryptEncodeObjectEx(X509_ASN_ENCODING, X509_PUBLIC_KEY_INFO, spki.cast(), 0, ptr::null(), der.as_mut_ptr().cast(), &mut len) == 0 {
            return None;
        }
        der.truncate(len as usize);
        Some(der)
    }
}

fn is_ours(context: *const CERT_CONTEXT) -> Option<isize> {
    let cert = CERT.get()?.as_ref()?;
    if context.is_null() {
        return None;
    }
    let c = unsafe { &*context };
    if c.cbCertEncoded as usize != cert.encoded.len() || c.pbCertEncoded.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(c.pbCertEncoded, c.cbCertEncoded as usize) };
    (bytes == cert.encoded.as_slice()).then_some(cert.engine)
}

unsafe extern "system" fn detour_get_chain(
    engine: isize,
    context: *const CERT_CONTEXT,
    time: *const core::ffi::c_void,
    store: *mut core::ffi::c_void,
    para: *const core::ffi::c_void,
    flags: u32,
    reserved: *const core::ffi::c_void,
    out: *mut *mut core::ffi::c_void,
) -> i32 {
    let Some(hook) = GET_CHAIN.get() else { return 0 };
    let engine = match is_ours(context) {
        Some(ours) => {
            crate::log::throttled("https chain", 2, |n| {
                log!("https tls: game checked PeerParty's certificate n={n}, trusted")
            });
            ours
        }
        None => engine,
    };
    unsafe { hook.call(engine, context, time, store, para, flags, reserved, out) }
}

// ---- trust for Unity's own TLS: the game's view of the ROOT store ----
//
// UnityTls (Unity's TLS layer, on MbedTLS) doesn't ask crypt32 to judge a server: it
// reads the Windows root store and verifies the chain itself (Unity's TLS developer:
// "Unity internally queries the system certificate store to let a TLS backend library
// ... do the verification", discussions.unity.com/t/836095; UnityPlayer.dll imports
// CertOpenStore and CertEnumCertificatesInStore). MbedTLS trusts a self-signed server
// certificate that is itself in the trusted list (x509_crt_check_ee_locally_trusted in
// mbedtls library/x509_crt.c). So, once armed, a ROOT system store this process opens
// comes back as a collection store: the real store (unchanged, still the one writes go
// to) plus an in-memory store holding PeerParty's certificate (CertAddStoreToCollection,
// learn.microsoft.com/windows/win32/api/wincrypt/nf-wincrypt-certaddstoretocollection).
// Nothing is written to any store; other processes and other stores are untouched.

type OpenStoreFn = unsafe extern "system" fn(*const u8, u32, usize, u32, *const core::ffi::c_void) -> *mut core::ffi::c_void;

static OPEN_STORE: OnceLock<GenericDetour<OpenStoreFn>> = OnceLock::new();

/// Where crypt32's `CertOpenStore` is, noted when crypt32 loads.
static OPEN_STORE_TARGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const CERT_STORE_PROV_SYSTEM_A: usize = 9;
const CERT_STORE_PROV_SYSTEM_W: usize = 10;
const CERT_STORE_PROV_COLLECTION: usize = 11;
const CERT_STORE_PROV_SYSTEM_REGISTRY_A: usize = 12;
const CERT_STORE_PROV_SYSTEM_REGISTRY_W: usize = 13;
const CERT_SYSTEM_STORE_RELOCATE_FLAG: u32 = 0x8000_0000;
const CERT_STORE_DELETE_FLAG: u32 = 0x10;
const CERT_PHYSICAL_STORE_ADD_ENABLE_FLAG: u32 = 0x1;

thread_local! {
    /// Set while this thread is in CertOpenStore or making the certificate: store opens
    /// from inside those are crypt32's or ours, and pass through.
    static MAKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Notes crypt32's `CertOpenStore` without touching it.
pub fn note_open_store(exports: &[Export]) -> bool {
    let Some(e) = exports.iter().find(|e| e.name == "CertOpenStore") else {
        return false;
    };
    OPEN_STORE_TARGET.store(e.addr as usize, std::sync::atomic::Ordering::Relaxed);
    true
}

/// Hooks the noted `CertOpenStore` (once): ROOT stores the game opens from now on list
/// PeerParty's certificate too. 1 if it's hooked now. Safe under the loader lock: the
/// certificate is made on the first ROOT store opened after this, not here.
pub fn arm_root_store() -> usize {
    if OPEN_STORE.get().is_some() {
        return 1;
    }
    let addr = OPEN_STORE_TARGET.load(std::sync::atomic::Ordering::Relaxed);
    if addr == 0 {
        return 0;
    }
    let target: OpenStoreFn = unsafe { std::mem::transmute(addr) };
    with_lock(|| {
        let Ok(hook) = (unsafe { GenericDetour::new(target, detour_open_store) }) else {
            log!("tls CertOpenStore not hooked");
            return 0;
        };
        if OPEN_STORE.set(hook).is_err() {
            return 0;
        }
        usize::from(unsafe { OPEN_STORE.get().unwrap().enable() }.is_ok())
    })
}

/// Whether a `CertOpenStore` call opens a system store named ROOT (any location).
fn opens_root(provider: *const u8, flags: u32, para: *const core::ffi::c_void) -> bool {
    if para.is_null() || flags & (CERT_SYSTEM_STORE_RELOCATE_FLAG | CERT_STORE_DELETE_FLAG) != 0 {
        return false;
    }
    let p = provider as usize;
    let wide_name = if p < 0x10000 {
        match p {
            CERT_STORE_PROV_SYSTEM_W | CERT_STORE_PROV_SYSTEM_REGISTRY_W => true,
            CERT_STORE_PROV_SYSTEM_A | CERT_STORE_PROV_SYSTEM_REGISTRY_A => false,
            _ => return false,
        }
    } else {
        // Named providers ("System", "SystemRegistry") take a wide store name.
        let name = unsafe { std::ffi::CStr::from_ptr(provider as *const core::ffi::c_char) };
        let name = name.to_bytes();
        if !(name.eq_ignore_ascii_case(b"System") || name.eq_ignore_ascii_case(b"SystemRegistry")) {
            return false;
        }
        true
    };
    // At most 5 units: "ROOT" and its terminator.
    let mut text = [0u16; 5];
    for (i, slot) in text.iter_mut().enumerate() {
        let c = unsafe {
            if wide_name { *(para as *const u16).add(i) } else { *(para as *const u8).add(i) as u16 }
        };
        *slot = c;
        if c == 0 {
            break;
        }
    }
    text[4] == 0 && String::from_utf16_lossy(&text[..4]).eq_ignore_ascii_case("ROOT")
}

/// The real ROOT store and an in-memory store with our certificate, as one collection
/// store; the real store itself if anything fails.
fn with_our_root(real: *mut core::ffi::c_void, ours: *const CERT_CONTEXT) -> *mut core::ffi::c_void {
    use windows_sys::Win32::Security::Cryptography::{CertAddStoreToCollection, CertCloseStore};
    unsafe {
        let mem = CertOpenStore(CERT_STORE_PROV_MEMORY as _, 0, 0, 0, ptr::null());
        if mem.is_null() {
            return real;
        }
        if CertAddCertificateContextToStore(mem, ours, CERT_STORE_ADD_ALWAYS, ptr::null_mut()) == 0 {
            CertCloseStore(mem, 0);
            return real;
        }
        let coll = CertOpenStore(CERT_STORE_PROV_COLLECTION as _, 0, 0, 0, ptr::null());
        if coll.is_null() {
            CertCloseStore(mem, 0);
            return real;
        }
        // The real store first and writable, so the game's own writes (if any) land where
        // they would have; ours is read-only and after it.
        let ok = CertAddStoreToCollection(coll, real, CERT_PHYSICAL_STORE_ADD_ENABLE_FLAG, 1) != 0
            && CertAddStoreToCollection(coll, mem, 0, 0) != 0;
        // The collection holds its own references to both.
        CertCloseStore(mem, 0);
        if !ok {
            CertCloseStore(coll, 0);
            return real;
        }
        CertCloseStore(real, 0);
        coll
    }
}

unsafe extern "system" fn detour_open_store(
    provider: *const u8,
    encoding: u32,
    prov: usize,
    flags: u32,
    para: *const core::ffi::c_void,
) -> *mut core::ffi::c_void {
    let Some(hook) = OPEN_STORE.get() else { return ptr::null_mut() };
    // A system store opens its physical stores (.Default, .LocalMachine, .GroupPolicy and
    // so on) through CertOpenStore again: only the outermost call is the game's.
    let nested = MAKING.with(|m| m.replace(true));
    let store = unsafe { hook.call(provider, encoding, prov, flags, para) };
    MAKING.with(|m| m.set(nested));
    if store.is_null() || nested || !opens_root(provider, flags, para) {
        return store;
    }
    MAKING.with(|m| m.set(true));
    let ours = cert().map(|c| c.context as *const CERT_CONTEXT);
    MAKING.with(|m| m.set(false));
    let Some(ours) = ours else { return store };
    crate::log::throttled("tls root store", 2, |n| {
        log!("tls: the game read the ROOT certificate store n={n}; PeerParty's certificate listed in this process only")
    });
    with_our_root(store, ours)
}

// ---- a server-side TLS stream ----

pub struct TlsStream {
    pub(super) sock: TcpStream,
    pub(super) ctx: SecHandle,
    pub(super) sizes: SecPkgContext_StreamSizes,
    /// Received bytes not yet decrypted.
    pub(super) incoming: Vec<u8>,
    /// Decrypted bytes not yet read.
    pub(super) plain: Vec<u8>,
    pub(super) eof: bool,
    /// A client stream's credentials and the server name it checks.
    pub(super) client: Option<(SecHandle, Vec<u16>)>,
}

pub(super) fn buf(kind: u32, data: *mut u8, len: usize) -> SecBuffer {
    SecBuffer {
        cbBuffer: len as u32,
        BufferType: kind,
        pvBuffer: data.cast(),
    }
}

pub(super) fn desc(bufs: &mut [SecBuffer]) -> SecBufferDesc {
    SecBufferDesc {
        ulVersion: 0,
        cBuffers: bufs.len() as u32,
        pBuffers: bufs.as_mut_ptr(),
    }
}

pub(super) fn read_more(sock: &mut TcpStream, into: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0u8; 16384];
    let n = sock.read(&mut chunk)?;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    into.extend_from_slice(&chunk[..n]);
    Ok(())
}

impl TlsStream {
    /// Runs the server side of the handshake.
    pub fn accept(mut sock: TcpStream) -> io::Result<TlsStream> {
        let mut ctx = SecHandle {
            dwLower: 0,
            dwUpper: 0,
        };
        let mut incoming = Vec::new();
        if let Err(e) = handshake(&mut sock, &mut ctx, false, &mut incoming) {
            if ctx.dwLower != 0 || ctx.dwUpper != 0 {
                unsafe { DeleteSecurityContext(&ctx) };
            }
            return Err(e);
        }
        let mut sizes: SecPkgContext_StreamSizes = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            QueryContextAttributesW(&ctx, SECPKG_ATTR_STREAM_SIZES, &mut sizes as *mut _ as *mut _)
        };
        if rc != SEC_E_OK {
            unsafe { DeleteSecurityContext(&ctx) };
            return Err(io::Error::other(format!("QueryContextAttributes {rc:#x}")));
        }
        Ok(TlsStream {
            sock,
            ctx,
            sizes,
            incoming,
            plain: Vec::new(),
            eof: false,
            client: None,
        })
    }
}

/// The server side of a handshake on `ctx`: the first one (`have_ctx` false), or one
/// the client starts later (`SEC_I_RENEGOTIATE`). `incoming` holds what was read and
/// not used; what is left over after the handshake stays there.
fn handshake(sock: &mut TcpStream, ctx: &mut SecHandle, mut have_ctx: bool, incoming: &mut Vec<u8>) -> io::Result<()> {
    let cert = cert().ok_or_else(|| io::Error::other("no certificate"))?;
    {
        loop {
            if incoming.is_empty() {
                read_more(sock, incoming)?;
            }
            if incoming.len() > MAX_HANDSHAKE {
                return Err(io::Error::other("handshake too long"));
            }
            let mut in_bufs = [
                buf(SECBUFFER_TOKEN, incoming.as_mut_ptr(), incoming.len()),
                buf(SECBUFFER_EMPTY, ptr::null_mut(), 0),
            ];
            let in_desc = desc(&mut in_bufs);
            let mut out_bufs = [buf(SECBUFFER_TOKEN, ptr::null_mut(), 0)];
            let mut out_desc = desc(&mut out_bufs);
            let mut attrs = 0u32;
            let rc = unsafe {
                AcceptSecurityContext(
                    &cert.credentials,
                    if have_ctx { &*ctx } else { ptr::null() },
                    &in_desc,
                    ASC_FLAGS,
                    0,
                    &mut *ctx,
                    &mut out_desc,
                    &mut attrs,
                    ptr::null_mut(),
                )
            };
            // A first call that wanted more input made no context yet: the next one
            // starts it again (passing the empty handle would fail it as invalid).
            if rc != SEC_E_INCOMPLETE_MESSAGE {
                have_ctx = true;
            }
            let out = out_bufs[0];
            if out.cbBuffer > 0 && !out.pvBuffer.is_null() {
                let token =
                    unsafe { std::slice::from_raw_parts(out.pvBuffer as *const u8, out.cbBuffer as usize) };
                let sent = sock.write_all(token);
                unsafe { FreeContextBuffer(out.pvBuffer) };
                sent?;
            }
            match rc {
                SEC_E_INCOMPLETE_MESSAGE => {
                    read_more(sock, incoming)?;
                    continue;
                }
                SEC_I_CONTINUE_NEEDED | SEC_E_OK => {
                    if in_bufs[1].BufferType == SECBUFFER_EXTRA && in_bufs[1].cbBuffer > 0 {
                        let keep = in_bufs[1].cbBuffer as usize;
                        incoming.drain(..incoming.len() - keep);
                    } else {
                        incoming.clear();
                    }
                    if rc == SEC_E_OK {
                        return Ok(());
                    }
                }
                other => {
                    return Err(io::Error::other(format!(
                        "AcceptSecurityContext {other:#x}"
                    )));
                }
            }
        }
    }
}

impl TlsStream {

    /// A read timeout on the socket underneath: a read that times out fails with
    /// WouldBlock / TimedOut and loses nothing (Wire polls its outbox between reads).
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.sock.set_read_timeout(timeout)
    }

    /// Decrypts at least one record into `plain`, or reports the end of the stream.
    fn fill(&mut self) -> io::Result<bool> {
        while self.plain.is_empty() && !self.eof {
            if self.incoming.is_empty() {
                match read_more(&mut self.sock, &mut self.incoming) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        self.eof = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            let mut bufs = [
                buf(SECBUFFER_DATA, self.incoming.as_mut_ptr(), self.incoming.len()),
                buf(SECBUFFER_EMPTY, ptr::null_mut(), 0),
                buf(SECBUFFER_EMPTY, ptr::null_mut(), 0),
                buf(SECBUFFER_EMPTY, ptr::null_mut(), 0),
            ];
            let d = desc(&mut bufs);
            let rc = unsafe { DecryptMessage(&self.ctx, &d, 0, ptr::null_mut()) };
            match rc {
                SEC_E_OK => {
                    // The next record's bytes (EXTRA) are moved to the front of the same
                    // buffer: one allocation for the connection, not one a record.
                    let mut extra = None;
                    for b in &bufs {
                        if b.BufferType == SECBUFFER_DATA && b.cbBuffer > 0 {
                            self.plain.extend_from_slice(unsafe {
                                std::slice::from_raw_parts(b.pvBuffer as *const u8, b.cbBuffer as usize)
                            });
                        } else if b.BufferType == SECBUFFER_EXTRA && b.cbBuffer > 0 {
                            extra = Some((b.pvBuffer as usize, b.cbBuffer as usize));
                        }
                    }
                    let base = self.incoming.as_ptr() as usize;
                    match extra {
                        Some((at, n)) if at >= base && at - base + n <= self.incoming.len() => {
                            self.incoming.copy_within(at - base..at - base + n, 0);
                            self.incoming.truncate(n);
                        }
                        Some((at, n)) => self.incoming = unsafe { std::slice::from_raw_parts(at as *const u8, n) }.to_vec(),
                        None => self.incoming.clear(),
                    }
                }
                SEC_E_INCOMPLETE_MESSAGE => match read_more(&mut self.sock, &mut self.incoming) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => self.eof = true,
                    Err(e) => return Err(e),
                },
                SEC_I_CONTEXT_EXPIRED => self.eof = true,
                SEC_I_RENEGOTIATE => {
                    // The record's handshake message goes to AcceptSecurityContext (as
                    // the extra buffer); any data that came with it is kept.
                    let mut extra = Vec::new();
                    for b in &bufs {
                        if b.BufferType == SECBUFFER_DATA && b.cbBuffer > 0 {
                            self.plain.extend_from_slice(unsafe { std::slice::from_raw_parts(b.pvBuffer as *const u8, b.cbBuffer as usize) });
                        } else if b.BufferType == SECBUFFER_EXTRA && b.cbBuffer > 0 {
                            extra = unsafe { std::slice::from_raw_parts(b.pvBuffer as *const u8, b.cbBuffer as usize) }.to_vec();
                        }
                    }
                    self.incoming = extra;
                    match &self.client {
                        Some((creds, target)) => super::client::client_handshake(&mut self.sock, creds, target, &mut self.ctx, true, &mut self.incoming)?,
                        None => handshake(&mut self.sock, &mut self.ctx, true, &mut self.incoming)?,
                    }
                }
                other => return Err(io::Error::other(format!("DecryptMessage {other:#x}"))),
            }
        }
        Ok(!self.plain.is_empty())
    }
}

impl Read for TlsStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !self.fill()? {
            return Ok(0);
        }
        let n = out.len().min(self.plain.len());
        out[..n].copy_from_slice(&self.plain[..n]);
        self.plain.drain(..n);
        Ok(n)
    }
}

impl Write for TlsStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let max = (self.sizes.cbMaximumMessage as usize).max(1);
        let header = self.sizes.cbHeader as usize;
        let trailer = self.sizes.cbTrailer as usize;
        for chunk in data.chunks(max) {
            let mut record = vec![0u8; header + chunk.len() + trailer];
            record[header..header + chunk.len()].copy_from_slice(chunk);
            let base = record.as_mut_ptr();
            let mut bufs = [
                buf(SECBUFFER_STREAM_HEADER, base, header),
                buf(SECBUFFER_DATA, unsafe { base.add(header) }, chunk.len()),
                buf(SECBUFFER_STREAM_TRAILER, unsafe { base.add(header + chunk.len()) }, trailer),
                buf(SECBUFFER_EMPTY, ptr::null_mut(), 0),
            ];
            let d = desc(&mut bufs);
            let rc = unsafe { EncryptMessage(&self.ctx, 0, &d, 0) };
            if rc != SEC_E_OK {
                return Err(io::Error::other(format!("EncryptMessage {rc:#x}")));
            }
            let len = (bufs[0].cbBuffer + bufs[1].cbBuffer + bufs[2].cbBuffer) as usize;
            self.sock.write_all(&record[..len])?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

impl Drop for TlsStream {
    fn drop(&mut self) {
        unsafe { DeleteSecurityContext(&self.ctx) };
        if let Some((creds, _)) = &self.client {
            unsafe { FreeCredentialsHandle(creds) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use windows_sys::Win32::Security::Cryptography::{
        CERT_CHAIN_PARA, CERT_CHAIN_POLICY_PARA, CERT_CHAIN_POLICY_STATUS,
        CertCloseStore, CertEnumCertificatesInStore, CertFreeCertificateChain, CertGetCertificateChain,
        CertVerifyCertificateChainPolicy,
    };

    #[repr(C)]
    struct SslPolicy {
        size: u32,
        auth_type: u32,
        checks: u32,
        server_name: *const u16,
    }

    /// The SSL policy verdict (0 = trusted) for our certificate and `host`, the way
    /// .NET's SslStream asks: build the chain with the default engine, then check it.
    fn verdict(host: &str) -> u32 {
        let cert = cert().unwrap();
        let mut para: CERT_CHAIN_PARA = unsafe { std::mem::zeroed() };
        para.cbSize = std::mem::size_of::<CERT_CHAIN_PARA>() as u32;
        let mut chain = ptr::null_mut();
        let built = unsafe {
            CertGetCertificateChain(ptr::null_mut(), cert.context, ptr::null(), ptr::null_mut(), &para, 0, ptr::null(), &mut chain)
        };
        assert_ne!(built, 0, "{}", io::Error::last_os_error());
        let name = wide(host);
        let mut ssl = SslPolicy {
            size: std::mem::size_of::<SslPolicy>() as u32,
            auth_type: 2, // AUTHTYPE_SERVER
            checks: 0,
            server_name: name.as_ptr(),
        };
        let policy = CERT_CHAIN_POLICY_PARA {
            cbSize: std::mem::size_of::<CERT_CHAIN_POLICY_PARA>() as u32,
            dwFlags: 0,
            pvExtraPolicyPara: &mut ssl as *mut _ as *mut _,
        };
        let mut status: CERT_CHAIN_POLICY_STATUS = unsafe { std::mem::zeroed() };
        status.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_STATUS>() as u32;
        let ok = unsafe { CertVerifyCertificateChainPolicy(4 as _, chain, &policy, &mut status) };
        unsafe { CertFreeCertificateChain(chain) };
        assert_ne!(ok, 0);
        status.dwError
    }

    /// The pin is Windows' own SHA-256 of the certificate's public key info.
    #[test]
    fn curls_pin_is_the_certificates_key() {
        use windows_sys::Win32::Security::Cryptography::CryptHashPublicKeyInfo;
        const CALG_SHA_256: u32 = 0x800c;
        let cert = cert().expect("certificate");
        let spki = unsafe { &raw const (*(*cert.context).pCertInfo).SubjectPublicKeyInfo };
        let mut hash = [0u8; 32];
        let mut len = hash.len() as u32;
        let ok = unsafe { CryptHashPublicKeyInfo(0, CALG_SHA_256, 0, X509_ASN_ENCODING, spki, hash.as_mut_ptr(), &mut len) };
        assert_ne!(ok, 0);
        let pin = curl_pin().expect("pin").to_str().unwrap();
        assert_eq!(pin, format!("sha256//{}", crate::util::encoding::base64(&hash)));
    }

    #[test]
    fn makes_a_certificate_that_only_the_hook_trusts() {
        let cert = cert().expect("certificate");
        assert!(!cert.encoded.is_empty());
        assert_eq!(is_ours(cert.context), Some(cert.engine));
        assert_eq!(is_ours(ptr::null()), None);
        // Before the hook Windows doesn't trust it (an untrusted root); after, it does,
        // for PlayFab's names only.
        assert_ne!(verdict("a3cac.playfabapi.com"), 0);
        let crypt32 = crate::watch::modules()
            .into_iter()
            .find(|(_, n)| n.eq_ignore_ascii_case("crypt32.dll"))
            .expect("crypt32 loaded");
        let table = unsafe { crate::sys::pe::exports(crypt32.0 as *const u8).unwrap() };
        assert!(note_crypt32(&table.code));
        assert_eq!(arm_crypt32(), 1);
        assert_eq!(verdict("a3cac.playfabapi.com"), 0);
        assert_eq!(verdict("playfabapi.com"), 0);
        // Unity Gaming Services' hosts (ugs/) ride the same certificate.
        assert_eq!(verdict("lobby.services.api.unity.com"), 0);
        assert_eq!(verdict("player-auth.services.api.unity.com"), 0);
        assert_eq!(verdict("wire.unity3d.com"), 0);
        assert_eq!(verdict("cloud-save.services.api.unity.com"), 0);
        assert_eq!(verdict("config.unity3d.com"), 0);
        // And Demonware's sign-in hosts (demonware/), one label under its names only.
        assert_eq!(verdict("title-pc-auth3.prod.demonware.net"), 0);
        assert_eq!(verdict("umbrella.demonware.net"), 0);
        assert_eq!(verdict("stun.us.demonware.net"), 0x800B_010F, "CERT_E_CN_NO_MATCH");
        // So do the Xbox Live friends hosts (xboxlive/), and no other Xbox Live host.
        assert_eq!(verdict("social.xboxlive.com"), 0);
        assert_eq!(verdict("profile.xboxlive.com"), 0);
        assert_eq!(verdict("userpresence.xboxlive.com"), 0);
        assert_eq!(verdict("privacy.xboxlive.com"), 0);
        assert_eq!(verdict("achievements.xboxlive.com"), 0x800B_010F, "CERT_E_CN_NO_MATCH");
        assert_eq!(verdict("www.example.com"), 0x800B_010F, "CERT_E_CN_NO_MATCH");

        // The client side refuses our server's certificate even now that the chain hook
        // trusts it here: Schannel's own check doesn't go through the hook (an untrusted
        // root), and past it ours refuses PeerParty's certificate. A redirected lookup
        // must not have PeerParty talk to itself (xboxlive/real.rs).
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let _ = TlsStream::accept(s);
        });
        let e = TlsStream::connect(TcpStream::connect(("127.0.0.1", port)).unwrap(), "privacy.xboxlive.com").err().expect("refused");
        assert!(e.to_string().contains("PeerParty's own") || e.to_string().contains("0x80090325"), "{e}");
        let _ = server.join();

        // Unity's TLS reads the ROOT store and checks the chain itself: once armed, the
        // ROOT store this process opens lists our certificate; other stores don't, and
        // Windows' own ROOT store (opened through the original function) doesn't either.
        assert!(note_open_store(&table.code));
        let count_ours = |store: *mut core::ffi::c_void| {
            let (mut n, mut ours, mut prev) = (0, 0, ptr::null_mut::<CERT_CONTEXT>());
            loop {
                prev = unsafe { CertEnumCertificatesInStore(store, prev) };
                if prev.is_null() {
                    break;
                }
                n += 1;
                if is_ours(prev).is_some() {
                    ours += 1;
                }
            }
            unsafe { CertCloseStore(store, 0) };
            (n, ours)
        };
        let open = |name: &str, provider: usize| {
            let w = wide(name);
            let a = format!("{name}\0");
            let para = if provider == CERT_STORE_PROV_SYSTEM_A { a.as_ptr() as *const _ } else { w.as_ptr() as *const _ };
            // CERT_SYSTEM_STORE_CURRENT_USER | CERT_STORE_READONLY_FLAG, as a reader would.
            let s = unsafe { CertOpenStore(provider as _, 0, 0, 0x0001_0000 | 0x8000, para) };
            assert!(!s.is_null(), "{name}: {}", io::Error::last_os_error());
            s
        };
        let (before, none) = count_ours(open("ROOT", CERT_STORE_PROV_SYSTEM_W));
        assert_eq!(none, 0, "not listed before arming");
        assert_eq!(arm_root_store(), 1);
        assert_eq!(arm_root_store(), 1, "arming twice is harmless");
        let (after, ours) = count_ours(open("ROOT", CERT_STORE_PROV_SYSTEM_W));
        assert_eq!((after, ours), (before + 1, 1));
        assert_eq!(count_ours(open("root", CERT_STORE_PROV_SYSTEM_A)).1, 1, "ANSI name, any case");
        assert_eq!(count_ours(open("MY", CERT_STORE_PROV_SYSTEM_W)).1, 0, "other stores untouched");
        assert_eq!(count_ours(open("CA", CERT_STORE_PROV_SYSTEM_W)).1, 0);
        // (Its physical stores open through the detour too, so as crypt32's own nested opens.)
        let real = unsafe {
            let w = wide("ROOT");
            MAKING.with(|m| m.set(true));
            let s = OPEN_STORE.get().unwrap().call(CERT_STORE_PROV_SYSTEM_W as _, 0, 0, 0x0001_0000 | 0x8000, w.as_ptr() as *const _);
            MAKING.with(|m| m.set(false));
            s
        };
        assert_eq!(count_ours(real), (before, 0), "Windows' ROOT store itself is unchanged");
        assert!(opens_root(b"System\0".as_ptr(), 0x0001_0000, wide("Root").as_ptr() as *const _));
        assert!(!opens_root(b"System\0".as_ptr(), 0x0001_0000, wide("ROOTS").as_ptr() as *const _));
        assert!(!opens_root(CERT_STORE_PROV_MEMORY as _, 0, ptr::null()));
    }
}
