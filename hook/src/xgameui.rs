//! The Xbox invite screens of Microsoft Store (GDK) games, answered by PeerParty's
//! overlay. A game's "Invite Friends" asks the Xbox runtime for its friends list;
//! with PeerParty's overlay running, the game gets the overlay's invite page instead
//! (inviting to the EOS lobby or whatever else invite.rs finds), and its request
//! completes at once as if the player closed Xbox's screen. Without a launcher to show
//! the overlay, Xbox's screen opens as usual.
//!
//! The GDK's functions reach `xgameruntime.dll` through its one real export,
//! `QueryApiImpl(clsid, iid, &object)`, then call a method of the object it hands back.
//! PeerParty hooks `QueryApiImpl`; the first time it hands out the XGameUi object, the
//! invite methods behind that object's vtable are hooked too. The async calls are
//! finished with the runtime's own `XAsyncRun` (IXThreadingImpl).
//!
//! The same hook hands PeerParty the XNetworking object, whose
//! `XNetworkingVerifyServerCertificate` (the GDK's certificate pin check, which XCurl
//! and libHttpClient call for each HTTPS request) passes for PeerParty's own certificate
//! only, so the Xbox Live friends hosts it answers (xboxlive/) are accepted; every other
//! server's certificate is checked by the runtime as before.
//!
//! Layouts: the interface ids and slot numbers of the XGameUI, XAsyncProvider and
//! XNetworking objects are listed below; every hooked slot is checked to point inside
//! xgameruntime.dll before it is hooked.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;

type QueryApi = unsafe extern "system" fn(*const [u8; 16], *const [u8; 16], *mut usize) -> i32;
/// `XGameUiShowMultiplayerActivityGameInviteAsync(this, async, user)`.
type ActivityInvite = unsafe extern "system" fn(usize, usize, usize) -> i32;
/// `XGameUiShowSendGameInviteAsync(this, async, user, config, template, session, text, context)`.
type SendInvite = unsafe extern "system" fn(usize, usize, usize, usize, usize, usize, usize, usize) -> i32;
/// The matching `...Result(this, async)`.
type InviteResult = unsafe extern "system" fn(usize, usize) -> i32;
type AsyncWork = unsafe extern "system" fn(usize) -> i32;
/// `XNetworkingVerifyServerCertificate(this, requestHandle, securityInformation)`.
type VerifyCert = unsafe extern "system" fn(usize, usize, usize) -> i32;

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

const CLSID_UI: [u8; 16] = guid(0xdfcd4649, 0x4ff8, 0x4043, [0xba, 0x07, 0x35, 0xd6, 0x07, 0xdf, 0x98, 0xb0]);
const IID_UI: [u8; 16] = guid(0x6eeaa73e, 0x9669, 0x43ad, [0xa2, 0xc7, 0xd0, 0xda, 0x4e, 0x1f, 0x50, 0xa1]);
/// IXGameUiImpl2..4 each extend the one before, so any of them has the activity invite.
const IID_UI_2_TO_4: [[u8; 16]; 3] = [
    guid(0x36a03122, 0x9ea3, 0x4a3a, [0xa8, 0xa4, 0x89, 0x9c, 0xfd, 0x85, 0xd7, 0xdb]),
    guid(0xade7eba1, 0x2093, 0x42ce, [0xa5, 0x44, 0xa5, 0x23, 0xa6, 0x67, 0x90, 0xe0]),
    guid(0xeaf669df, 0x5542, 0x4590, [0x99, 0xa3, 0x8d, 0xc0, 0x61, 0xf8, 0x37, 0xcc]),
];
/// XNetworkingImpl's class shares its GUID with IXNetworkingImpl; IXNetworkingImpl2 extends it.
const XNETWORKING: [u8; 16] = guid(0x37e56907, 0x2f10, 0x41e8, [0xb7, 0x2f, 0x36, 0xed, 0xb1, 0x85, 0x33, 0x1a]);
const IID_XNETWORKING_2: [u8; 16] = guid(0xbf2346b2, 0x39af, 0x4658, [0xb5, 0xea, 0x44, 0x71, 0x3c, 0x7e, 0x83, 0xb3]);
/// XThreadingImpl's class and interface share one GUID.
const THREADING: [u8; 16] = guid(0x073b7dcb, 0x1fcf, 0x4030, [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28]);

// IXGameUiImpl: IUnknown's 3, then ShowMessageDialog Async/Result (3, 4), ShowSendGameInvite
// Async/Result (5, 6), ... 27 methods in all; IXGameUiImpl2 starts at 30.
const SLOT_SEND: usize = 5;
const SLOT_SEND_RESULT: usize = 6;
const SLOT_ACTIVITY: usize = 30;
const SLOT_ACTIVITY_RESULT: usize = 31;
// IXThreadingImpl: XAsyncGetStatus 3, XAsyncGetResultSize 4, XAsyncCancel 5, XAsyncRun 6.
// IXNetworkingImpl: IUnknown's 3, the UDP port calls (3-7), the security information
// queries (8-13), then XNetworkingVerifyServerCertificate.
const SLOT_VERIFY_CERT: usize = 14;
const SLOT_GET_STATUS: usize = 3;
const SLOT_RUN: usize = 6;
const E_FAIL: i32 = 0x8000_4005_u32 as i32;

static QUERY: OnceLock<GenericDetour<QueryApi>> = OnceLock::new();
static SEND: OnceLock<GenericDetour<SendInvite>> = OnceLock::new();
static SEND_RESULT: OnceLock<GenericDetour<InviteResult>> = OnceLock::new();
static ACTIVITY: OnceLock<GenericDetour<ActivityInvite>> = OnceLock::new();
static ACTIVITY_RESULT: OnceLock<GenericDetour<InviteResult>> = OnceLock::new();
static VERIFY: OnceLock<GenericDetour<VerifyCert>> = OnceLock::new();
/// xgameruntime.dll's base: every vtable entry hooked must point inside it.
static MODULE: OnceLock<usize> = OnceLock::new();
/// The async blocks PeerParty finished, until the game asks for their result.
static OURS: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// xgameruntime.dll loaded (watch.rs).
pub fn attach(exports: &[Export]) -> usize {
    let Some(e) = exports.iter().find(|e| e.name == "QueryApiImpl") else {
        log!("xgameui QueryApiImpl not exported");
        return 0;
    };
    let _ = MODULE.set(module_of(e.addr as usize));
    let target: QueryApi = unsafe { std::mem::transmute(e.addr) };
    usize::from(arm(&QUERY, target, query as QueryApi, "QueryApiImpl"))
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        if slot.get().is_some() {
            return false;
        }
        let hook = match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => h,
            Err(e) => {
                log!("xgameui {label} not hooked: {e}");
                return false;
            }
        };
        let _ = slot.set(hook);
        match crate::hook::frozen(target.to_ptr() as usize, || unsafe { slot.get().unwrap().enable() }) {
            Ok(()) => true,
            Err(e) => {
                log!("xgameui {label} enable failed: {e}");
                false
            }
        }
    })
}

fn module_of(addr: usize) -> usize {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW,
    };
    let mut m = std::ptr::null_mut();
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    unsafe { GetModuleHandleExW(flags, addr as *const u16, &mut m) };
    m as usize
}

unsafe fn slot(obj: usize, i: usize) -> usize {
    unsafe { *(*(obj as *const *const usize)).add(i) }
}

/// The method at `i`, if it's xgameruntime's (a sanity check on the slot numbers).
unsafe fn method(obj: usize, i: usize) -> Option<usize> {
    let f = unsafe { slot(obj, i) };
    (f != 0 && MODULE.get().is_some_and(|m| *m != 0 && module_of(f) == *m)).then_some(f)
}

unsafe extern "system" fn query(clsid: *const [u8; 16], iid: *const [u8; 16], out: *mut usize) -> i32 {
    let hr = unsafe { QUERY.get().unwrap().call(clsid, iid, out) };
    if hr >= 0 && !out.is_null() && !clsid.is_null() && !iid.is_null() && unsafe { *clsid } == CLSID_UI {
        let obj = unsafe { *out };
        let iid = unsafe { *iid };
        if obj != 0 && (iid == IID_UI || IID_UI_2_TO_4.contains(&iid)) {
            unsafe { hook_methods(obj, iid != IID_UI) };
        }
    }
    if hr >= 0 && !out.is_null() && !clsid.is_null() && !iid.is_null() && unsafe { *clsid } == XNETWORKING {
        let obj = unsafe { *out };
        let iid = unsafe { *iid };
        if obj != 0 && VERIFY.get().is_none() && (iid == XNETWORKING || iid == IID_XNETWORKING_2) && crate::xboxlive::wanted() {
            if let Some(f) = unsafe { method(obj, SLOT_VERIFY_CERT) }
                && arm(&VERIFY, unsafe { std::mem::transmute::<usize, VerifyCert>(f) }, verify_cert, "XNetworkingVerifyServerCertificate")
            {
                log!("xgameui: the GDK's certificate pin check hooked (PeerParty's Xbox Live hosts pass it)");
            }
        }
    }
    hr
}

/// The server certificate of a WinHTTP request (`WINHTTP_OPTION_SERVER_CERT_CONTEXT`),
/// for the pin check; XCurl and libHttpClient both run on WinHTTP.
fn server_cert(request: usize) -> Option<*const windows_sys::Win32::Security::Cryptography::CERT_CONTEXT> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    type QueryOption = unsafe extern "system" fn(usize, u32, *mut core::ffi::c_void, *mut u32) -> i32;
    const WINHTTP_OPTION_SERVER_CERT_CONTEXT: u32 = 78;
    let name: Vec<u16> = "winhttp.dll".encode_utf16().chain(Some(0)).collect();
    let module = unsafe { GetModuleHandleW(name.as_ptr()) };
    if module.is_null() || request == 0 {
        return None;
    }
    let query: QueryOption = unsafe { std::mem::transmute(GetProcAddress(module, c"WinHttpQueryOption".as_ptr().cast())?) };
    let mut cert: *const windows_sys::Win32::Security::Cryptography::CERT_CONTEXT = std::ptr::null();
    let mut len = size_of::<usize>() as u32;
    let ok = unsafe { query(request, WINHTTP_OPTION_SERVER_CERT_CONTEXT, (&raw mut cert).cast(), &mut len) };
    (ok != 0 && !cert.is_null()).then_some(cert)
}

unsafe extern "system" fn verify_cert(this: usize, request: usize, info: usize) -> i32 {
    if let Some(cert) = server_cert(request) {
        let ours = crate::playfab::tls::is_peerparty(cert);
        unsafe { windows_sys::Win32::Security::Cryptography::CertFreeCertificateContext(cert) };
        if ours {
            crate::log::throttled("xgameui pin check", 2, |n| {
                log!("xgameui: the pin check met PeerParty's certificate n={n}, passed")
            });
            return 0;
        }
    }
    unsafe { VERIFY.get().unwrap().call(this, request, info) }
}

unsafe fn hook_methods(obj: usize, extended: bool) {
    let mut n = 0;
    if SEND.get().is_none()
        && let (Some(a), Some(r)) = unsafe { (method(obj, SLOT_SEND), method(obj, SLOT_SEND_RESULT)) }
    {
        n += usize::from(arm(&SEND, unsafe { std::mem::transmute::<usize, SendInvite>(a) }, send_invite, "ShowSendGameInviteAsync"));
        n += usize::from(arm(&SEND_RESULT, unsafe { std::mem::transmute::<usize, InviteResult>(r) }, send_result, "ShowSendGameInviteResult"));
    }
    if extended
        && ACTIVITY.get().is_none()
        && let (Some(a), Some(r)) = unsafe { (method(obj, SLOT_ACTIVITY), method(obj, SLOT_ACTIVITY_RESULT)) }
    {
        n += usize::from(arm(&ACTIVITY, unsafe { std::mem::transmute::<usize, ActivityInvite>(a) }, activity_invite, "ShowMultiplayerActivityGameInviteAsync"));
        n += usize::from(arm(&ACTIVITY_RESULT, unsafe { std::mem::transmute::<usize, InviteResult>(r) }, activity_result, "ShowMultiplayerActivityGameInviteResult"));
    }
    if n > 0 {
        log!("xgameui: Xbox invite screens hooked ({n})");
    }
}

/// Calls method `i` of the runtime's threading object.
unsafe fn threading<R>(f: impl FnOnce(usize) -> R) -> Option<R> {
    let mut obj = 0usize;
    let hr = unsafe { QUERY.get()?.call(&THREADING, &THREADING, &mut obj) };
    if hr < 0 || obj == 0 {
        log!("xgameui: no XThreadingImpl ({hr:#x})");
        return None;
    }
    let r = f(obj);
    let release: unsafe extern "system" fn(usize) -> u32 = unsafe { std::mem::transmute(slot(obj, 2)) };
    unsafe { release(obj) };
    Some(r)
}

unsafe extern "system" fn done(_async: usize) -> i32 {
    0
}

/// Shows the overlay's invite page and finishes the game's request. None when there's
/// no overlay (Xbox's screen is shown then).
fn take(async_block: usize, what: &str) -> Option<i32> {
    if !crate::ipc::connected() || !crate::overlay::page("invite", None) {
        log!("xgameui: {what}: no peerparty overlay, Xbox's screen opens");
        return None;
    }
    // Marked before it starts: the game may ask for the result as soon as it's done.
    OURS.lock().unwrap().get_or_insert_with(HashSet::new).insert(async_block);
    let hr = unsafe {
        threading(|t| {
            let run: unsafe extern "system" fn(usize, usize, AsyncWork) -> i32 = std::mem::transmute(slot(t, SLOT_RUN));
            run(t, async_block, done)
        })
    }
    .unwrap_or(E_FAIL);
    if hr < 0 {
        OURS.lock().unwrap().as_mut().map(|s| s.remove(&async_block));
    }
    log!("xgameui: {what}: PeerParty's invite page instead ({hr:#x})");
    Some(hr)
}

/// The result of a request PeerParty finished, or None for one Xbox's screen took.
fn result(async_block: usize) -> Option<i32> {
    if !OURS.lock().unwrap().as_mut().is_some_and(|s| s.remove(&async_block)) {
        return None;
    }
    unsafe {
        threading(|t| {
            let status: unsafe extern "system" fn(usize, usize, u8) -> i32 = std::mem::transmute(slot(t, SLOT_GET_STATUS));
            status(t, async_block, 0)
        })
    }
    .or(Some(0))
}

unsafe extern "system" fn activity_invite(this: usize, async_block: usize, user: usize) -> i32 {
    take(async_block, "ShowMultiplayerActivityGameInvite")
        .unwrap_or_else(|| unsafe { ACTIVITY.get().unwrap().call(this, async_block, user) })
}

unsafe extern "system" fn activity_result(this: usize, async_block: usize) -> i32 {
    result(async_block).unwrap_or_else(|| unsafe { ACTIVITY_RESULT.get().unwrap().call(this, async_block) })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn send_invite(
    this: usize,
    async_block: usize,
    user: usize,
    config: usize,
    template: usize,
    session: usize,
    text: usize,
    context: usize,
) -> i32 {
    take(async_block, "ShowSendGameInvite").unwrap_or_else(|| unsafe {
        SEND.get().unwrap().call(this, async_block, user, config, template, session, text, context)
    })
}

unsafe extern "system" fn send_result(this: usize, async_block: usize) -> i32 {
    result(async_block).unwrap_or_else(|| unsafe { SEND_RESULT.get().unwrap().call(this, async_block) })
}
