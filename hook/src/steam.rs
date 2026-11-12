//! Typed hooks for the few Steam exports whose useful data is a return value
//! or an output struct. Everything else is a generic entry log.
//!
//! These hooks also let PeerParty hand the game Steam callbacks of its own (a LAN
//! friend's join request, a presence change), through whichever dispatch the game
//! uses: ManualDispatch (Steamworks.NET, Facepunch) gets them from
//! `GetNextCallback`, classic games through the `CCallbackBase` objects they
//! registered, after `SteamAPI_RunCallbacks`. PeerParty's own async calls (a LAN
//! lobby's `CreateLobby`) complete the same two ways: a `SteamAPICallCompleted_t` and
//! `GetAPICallResult`, or the `CCallResult` registered for the call.

use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;
use crate::read;
use crate::sdk::Sdk;

pub struct Typed {
    pub skip: HashSet<usize>,
    pub hooked: usize,
}

#[repr(C)]
struct CallbackMsg {
    steam_user: i32,
    callback: i32,
    param: *mut u8,
    param_size: i32,
}

const _: () = assert!(std::mem::offset_of!(CallbackMsg, callback) == 4);

// Steam's S_API is __cdecl. That only differs from "system" (stdcall) on x86.
type InitFn = unsafe extern "C" fn() -> bool;
type InitFlatFn = unsafe extern "C" fn(*mut i8) -> i32;
type InternalInitFn = unsafe extern "C" fn(*const i8, *mut i8) -> i32;
type BGetFn = unsafe extern "C" fn(i32, *mut CallbackMsg, *mut i32) -> bool;
type NextCbFn = unsafe extern "C" fn(i32, *mut CallbackMsg) -> bool;
type ApiResultFn =
    unsafe extern "C" fn(i32, u64, *mut core::ffi::c_void, i32, i32, *mut bool) -> bool;

static INIT: OnceLock<GenericDetour<InitFn>> = OnceLock::new();
/// The game started Steam's client API (any SteamAPI_Init form), set before it runs:
/// a dedicated server starts only a game server (net.rs `dedicated`).
static CLIENT_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the game has started Steam's client API.
pub fn client_started() -> bool {
    CLIENT_STARTED.load(std::sync::atomic::Ordering::Relaxed)
}
static INIT_SAFE: OnceLock<GenericDetour<InitFn>> = OnceLock::new();
static INIT_FLAT: OnceLock<GenericDetour<InitFlatFn>> = OnceLock::new();
static INTERNAL_INIT: OnceLock<GenericDetour<InternalInitFn>> = OnceLock::new();
static BGET: OnceLock<GenericDetour<BGetFn>> = OnceLock::new();
static NEXT_CB: OnceLock<GenericDetour<NextCbFn>> = OnceLock::new();
static API_RESULT: OnceLock<GenericDetour<ApiResultFn>> = OnceLock::new();
static MANUAL_RESULT: OnceLock<GenericDetour<ApiResultFn>> = OnceLock::new();

type PipeFn = unsafe extern "C" fn(i32);
type RunCallbacksFn = unsafe extern "C" fn();
type RegisterFn = unsafe extern "C" fn(usize, i32);
type UnregisterFn = unsafe extern "C" fn(usize);
type CallResultFn = unsafe extern "C" fn(usize, u64);

static FREE_LAST: OnceLock<GenericDetour<PipeFn>> = OnceLock::new();
static RUN_FRAME: OnceLock<GenericDetour<PipeFn>> = OnceLock::new();
static RUN_CALLBACKS: OnceLock<GenericDetour<RunCallbacksFn>> = OnceLock::new();
static REGISTER: OnceLock<GenericDetour<RegisterFn>> = OnceLock::new();
static UNREGISTER: OnceLock<GenericDetour<UnregisterFn>> = OnceLock::new();
static REGISTER_RESULT: OnceLock<GenericDetour<CallResultFn>> = OnceLock::new();
static UNREGISTER_RESULT: OnceLock<GenericDetour<CallResultFn>> = OnceLock::new();

/// One thing for the game: a callback (`call` 0) or the result of PeerParty's call.
struct Pending {
    call: u64,
    id: i32,
    data: Vec<u8>,
    /// For the game server's dispatch only (its pipe, or callbacks it registered).
    server: bool,
    /// Dispatches it has waited through for its CCallResult to be registered.
    waited: u32,
}

/// Callbacks and call results PeerParty wants the game to see, oldest first.
static INJECT: Mutex<VecDeque<Pending>> = Mutex::new(VecDeque::new());
/// PeerParty's call handles: well away from Steam's own.
static NEXT_CALL: AtomicU64 = AtomicU64::new(0x5050_0000_0000_0001);
/// Completed calls announced through ManualDispatch, until the game reads them.
static COMPLETED: LazyLock<Mutex<HashMap<u64, (i32, Vec<u8>)>>> = LazyLock::new(Mutex::default);
/// Our call -> the CCallResult object the game registered for it.
static RESULT_OBJS: LazyLock<Mutex<HashMap<u64, usize>>> = LazyLock::new(Mutex::default);

const STEAM_API_CALL_COMPLETED: i32 = 703;

#[repr(C)]
struct CallCompleted {
    call: u64,
    id: i32,
    size: u32,
}
/// The injected callback the game is holding, until its FreeLastCallback.
static OUTSTANDING: Mutex<Option<Vec<u8>>> = Mutex::new(None);
/// The game dispatches manually (it called SteamAPI_ManualDispatch_RunFrame).
static MANUAL: AtomicBool = AtomicBool::new(false);
/// Callback id -> CCallbackBase objects the game registered (classic dispatch), each
/// with whether it's a game server's.
static REGISTERED: LazyLock<Mutex<HashMap<i32, Vec<(usize, bool)>>>> = LazyLock::new(Mutex::default);

/// Queues a callback for the game, delivered on its next dispatch.
pub fn inject(id: i32, data: Vec<u8>) {
    push(0, id, data, false);
}

/// Queues a callback for the game's server: its own dispatch pipe, or the callbacks it
/// registered as a game server's.
pub fn inject_server(id: i32, data: Vec<u8>) {
    push(0, id, data, true);
}

/// `inject_server` for a callback meant for the game server, else `inject`. A game
/// server with no dispatch of its own to deliver it (manual dispatch on the game's one
/// pipe) gets it through the game's.
pub fn inject_to(server: bool, id: i32, data: Vec<u8>) {
    let server = server && (!MANUAL.load(Ordering::Relaxed) || crate::steamgs::server_pipe().is_some());
    push(0, id, data, server);
}

/// The injected callbacks waiting, oldest first: (id, for the game server, data).
#[cfg(test)]
pub fn injected() -> Vec<(i32, bool, Vec<u8>)> {
    INJECT.lock().unwrap().iter().map(|p| (p.id, p.server, p.data.clone())).collect()
}

/// A new handle for an async call PeerParty answers itself.
pub fn new_call() -> u64 {
    NEXT_CALL.fetch_add(1, Ordering::Relaxed)
}

fn ours(call: u64) -> bool {
    call >> 48 == 0x5050
}

/// Completes PeerParty's call `call` with result struct `id`.
pub fn complete(call: u64, id: i32, data: Vec<u8>) {
    push(call, id, data, false);
}

fn push(call: u64, id: i32, data: Vec<u8>, server: bool) {
    let mut q = INJECT.lock().unwrap();
    // A game that never dispatches (or a dedicated server that never runs the client's
    // dispatch) mustn't make this grow forever: the oldest of the other kind goes first,
    // a plain callback before a call result (a game waits on a call result's answer,
    // and a later callback of the same kind usually says what a dropped one did).
    if q.len() >= 256 {
        let plain = |p: &Pending| p.call == 0;
        let at = q
            .iter()
            .position(|p| p.server != server && plain(p))
            .or_else(|| q.iter().position(plain))
            .or_else(|| q.iter().position(|p| p.server != server))
            .unwrap_or(0);
        if let Some(gone) = q.remove(at) {
            let kind = if gone.call == 0 { "callback" } else { "call result" };
            log::throttled("steam-lan queue full", 4, |n| {
                log!("steam-lan: 256 callbacks wait undelivered: the oldest {kind} {} was dropped (n={n})", gone.id)
            });
        }
    }
    q.push_back(Pending {
        call,
        id,
        data,
        server,
        waited: 0,
    });
}

/// Whether the game will receive callback `id` from us.
pub fn delivers(id: i32) -> bool {
    MANUAL.load(Ordering::Relaxed)
        || REGISTERED
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|v| v.iter().any(|(_, server)| !server))
}

pub fn attach(sdk: Sdk, exports: &[Export]) -> Typed {
    let mut out = Typed {
        skip: HashSet::new(),
        hooked: 0,
    };
    match sdk {
        Sdk::SteamApi => {
            // A patched prologue stays out of the generic pass. If the detour was
            // never written, the generic pass logs the call instead.
            try_init(&mut out, exports, "SteamAPI_Init", &INIT, detour_init);
            try_init(
                &mut out,
                exports,
                "SteamAPI_InitSafe",
                &INIT_SAFE,
                detour_init_safe,
            );
            try_flat(&mut out, exports);
            try_internal(&mut out, exports);
            try_next(&mut out, exports);
            try_result(
                &mut out,
                exports,
                "SteamAPI_ManualDispatch_GetAPICallResult",
                &MANUAL_RESULT,
                detour_manual_result,
            );
            try_typed(
                &mut out,
                exports,
                "SteamAPI_ManualDispatch_FreeLastCallback",
                &FREE_LAST,
                detour_free_last as PipeFn,
            );
            try_typed(
                &mut out,
                exports,
                "SteamAPI_ManualDispatch_RunFrame",
                &RUN_FRAME,
                detour_run_frame as PipeFn,
            );
            try_typed(
                &mut out,
                exports,
                "SteamAPI_RunCallbacks",
                &RUN_CALLBACKS,
                detour_run_callbacks as RunCallbacksFn,
            );
            try_typed(
                &mut out,
                exports,
                "SteamAPI_RegisterCallback",
                &REGISTER,
                detour_register as RegisterFn,
            );
            try_typed(
                &mut out,
                exports,
                "SteamAPI_UnregisterCallback",
                &UNREGISTER,
                detour_unregister as UnregisterFn,
            );
            try_typed(
                &mut out,
                exports,
                "SteamAPI_RegisterCallResult",
                &REGISTER_RESULT,
                detour_register_result as CallResultFn,
            );
            try_typed(
                &mut out,
                exports,
                "SteamAPI_UnregisterCallResult",
                &UNREGISTER_RESULT,
                detour_unregister_result as CallResultFn,
            );
        }
        Sdk::SteamClient => {
            try_bget(&mut out, exports);
            try_result(
                &mut out,
                exports,
                "Steam_GetAPICallResult",
                &API_RESULT,
                detour_api_result,
            );
        }
        _ => {}
    }
    out
}

fn find<'a>(exports: &'a [Export], name: &str) -> Option<&'a Export> {
    exports.iter().find(|e| e.name == name)
}

fn remember(out: &mut Typed, addr: *const (), armed: Armed) {
    if armed.skip {
        out.skip.insert(addr as usize);
    }
    if armed.hooked {
        out.hooked += 1;
    }
}

struct Armed {
    skip: bool,
    hooked: bool,
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> Armed
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let hook = match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => h,
            Err(e) => {
                log!("typed {label} not hooked: {e}");
                // Nothing was patched. The generic pass can still log the call.
                return Armed {
                    skip: false,
                    hooked: false,
                };
            }
        };
        if slot.set(hook).is_err() {
            log!("typed {label} already hooked");
            return Armed {
                skip: false,
                hooked: false,
            };
        }
        if let Err(e) = unsafe { slot.get().unwrap().enable() } {
            log!("typed {label} enable failed: {e}");
            // enable may have written the prologue. Don't patch it again.
            return Armed {
                skip: true,
                hooked: false,
            };
        }
        Armed {
            skip: true,
            hooked: true,
        }
    })
}

fn try_init(
    out: &mut Typed,
    exports: &[Export],
    name: &'static str,
    slot: &OnceLock<GenericDetour<InitFn>>,
    detour: InitFn,
) {
    let Some(export) = find(exports, name) else {
        return;
    };
    let target: InitFn = unsafe { mem::transmute(export.addr) };
    remember(out, export.addr, arm(slot, target, detour, name));
}

fn try_flat(out: &mut Typed, exports: &[Export]) {
    let Some(export) = find(exports, "SteamAPI_InitFlat") else {
        return;
    };
    let target: InitFlatFn = unsafe { mem::transmute(export.addr) };
    remember(
        out,
        export.addr,
        arm(&INIT_FLAT, target, detour_init_flat, "SteamAPI_InitFlat"),
    );
}

fn try_internal(out: &mut Typed, exports: &[Export]) {
    let Some(export) = find(exports, "SteamInternal_SteamAPI_Init") else {
        return;
    };
    let target: InternalInitFn = unsafe { mem::transmute(export.addr) };
    remember(
        out,
        export.addr,
        arm(
            &INTERNAL_INIT,
            target,
            detour_internal_init,
            "SteamInternal_SteamAPI_Init",
        ),
    );
}

fn try_bget(out: &mut Typed, exports: &[Export]) {
    let Some(export) = find(exports, "Steam_BGetCallback") else {
        return;
    };
    let target: BGetFn = unsafe { mem::transmute(export.addr) };
    remember(
        out,
        export.addr,
        arm(&BGET, target, detour_bget, "Steam_BGetCallback"),
    );
}

fn try_next(out: &mut Typed, exports: &[Export]) {
    let Some(export) = find(exports, "SteamAPI_ManualDispatch_GetNextCallback") else {
        return;
    };
    let target: NextCbFn = unsafe { mem::transmute(export.addr) };
    remember(
        out,
        export.addr,
        arm(
            &NEXT_CB,
            target,
            detour_next,
            "SteamAPI_ManualDispatch_GetNextCallback",
        ),
    );
}

fn try_result(
    out: &mut Typed,
    exports: &[Export],
    name: &'static str,
    slot: &'static OnceLock<GenericDetour<ApiResultFn>>,
    detour: ApiResultFn,
) {
    let Some(export) = find(exports, name) else {
        return;
    };
    let target: ApiResultFn = unsafe { mem::transmute(export.addr) };
    remember(out, export.addr, arm(slot, target, detour, name));
}

fn try_typed<T>(
    out: &mut Typed,
    exports: &[Export],
    name: &'static str,
    slot: &'static OnceLock<GenericDetour<T>>,
    detour: T,
) where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    let Some(export) = find(exports, name) else {
        return;
    };
    let target: T = unsafe { mem::transmute_copy(&export.addr) };
    remember(out, export.addr, arm(slot, target, detour, name));
}

unsafe extern "C" fn detour_free_last(pipe: i32) {
    // Our injected callback isn't the SDK's to free.
    if OUTSTANDING.lock().unwrap().take().is_some() {
        return;
    }
    if let Some(hook) = FREE_LAST.get() {
        unsafe { hook.call(pipe) };
    }
}

unsafe extern "C" fn detour_run_frame(pipe: i32) {
    MANUAL.store(true, Ordering::Relaxed);
    if let Some(hook) = RUN_FRAME.get() {
        unsafe { hook.call(pipe) };
    }
    let _ = std::panic::catch_unwind(crate::steamlan::pump);
}

unsafe extern "C" fn detour_run_callbacks() {
    if let Some(hook) = RUN_CALLBACKS.get() {
        unsafe { hook.call() };
    }
    let _ = std::panic::catch_unwind(crate::steamlan::pump);
    if !MANUAL.load(Ordering::Relaxed) {
        deliver_registered(false);
    }
}

/// The game server's classic dispatch (`SteamGameServer_RunCallbacks`).
pub fn deliver_server() {
    if !MANUAL.load(Ordering::Relaxed) {
        deliver_registered(true);
    }
}

unsafe extern "C" fn detour_register_result(obj: usize, call: u64) {
    if ours(call) {
        RESULT_OBJS.lock().unwrap().insert(call, obj);
        return;
    }
    unsafe { crate::steamown::wrap(obj) };
    if let Some(hook) = REGISTER_RESULT.get() {
        unsafe { hook.call(obj, call) };
    }
}

unsafe extern "C" fn detour_unregister_result(obj: usize, call: u64) {
    if ours(call) {
        RESULT_OBJS.lock().unwrap().remove(&call);
        return;
    }
    if let Some(hook) = UNREGISTER_RESULT.get() {
        unsafe { hook.call(obj, call) };
    }
}

unsafe extern "C" fn detour_register(obj: usize, id: i32) {
    if obj != 0 {
        // CCallbackBase: vtable, then m_nCallbackFlags (k_ECallbackFlagsGameServer = 2),
        // set before the object registers.
        let flags = unsafe { *((obj + size_of::<usize>()) as *const u8) };
        REGISTERED.lock().unwrap().entry(id).or_default().push((obj, flags & 2 != 0));
        // steam_api's own callbacks name the player as the game knows it (steamown.rs).
        unsafe { crate::steamown::wrap(obj) };
    }
    log::throttled(&format!("SteamAPI_RegisterCallback {id}"), 4, |n| {
        log!("call SteamAPI_RegisterCallback n={n} id={id} obj={obj:#x}")
    });
    if let Some(hook) = REGISTER.get() {
        unsafe { hook.call(obj, id) };
    }
}

unsafe extern "C" fn detour_unregister(obj: usize) {
    for objs in REGISTERED.lock().unwrap().values_mut() {
        objs.retain(|(o, _)| *o != obj);
    }
    if let Some(hook) = UNREGISTER.get() {
        unsafe { hook.call(obj) };
    }
}

/// Classic dispatch: `CCallbackBase::Run(void*)` on each registered object, and
/// `Run(void*, bool, SteamAPICall_t)` on a call's CCallResult. MSVC puts the two `Run`
/// overloads in reverse declaration order, so the one-argument `Run` is vtable slot 1
/// on Windows and the call-result one slot 0.
fn deliver_registered(server: bool) {
    let batch: Vec<Pending> = {
        let mut q = INJECT.lock().unwrap();
        let (mine, rest): (VecDeque<Pending>, VecDeque<Pending>) = q.drain(..).partition(|p| p.server == server);
        *q = rest;
        mine.into_iter().collect()
    };
    let mut later = Vec::new();
    for mut p in batch {
        if p.call != 0 {
            let obj = RESULT_OBJS.lock().unwrap().remove(&p.call);
            match obj {
                Some(obj) => unsafe { run_result(obj, &mut p.data, p.call) },
                // Registered right after the call, normally; give a game a moment.
                None if p.waited < 600 => {
                    p.waited += 1;
                    later.push(p);
                }
                None => log!("steam-lan call {:#x} ({}) had no listener", p.call, p.id),
            }
            continue;
        }
        // A client callback reaches every listener (as it always has); a server one only
        // the game server's.
        let objs: Vec<usize> = REGISTERED
            .lock()
            .unwrap()
            .get(&p.id)
            .map(|v| v.iter().filter(|(_, gs)| !server || *gs).map(|(o, _)| *o).collect())
            .unwrap_or_default();
        for obj in &objs {
            unsafe { run_callback(*obj, &mut p.data) };
        }
        log::throttled(&format!("steam-lan delivered {}", p.id), 8, |n| {
            log!(
                "steam-lan delivered callback {} to {} registered objects (n={n})",
                p.id,
                objs.len()
            )
        });
    }
    let mut q = INJECT.lock().unwrap();
    for p in later.into_iter().rev() {
        q.push_front(p);
    }
}

unsafe fn run_callback(obj: usize, data: &mut [u8]) {
    unsafe {
        let run = *(*(obj as *const *const usize)).add(1);
        #[cfg(target_arch = "x86_64")]
        let f: unsafe extern "C" fn(usize, *mut u8) = mem::transmute(run);
        #[cfg(target_arch = "x86")]
        let f: unsafe extern "thiscall" fn(usize, *mut u8) = mem::transmute(run);
        f(obj, data.as_mut_ptr());
    }
}

unsafe fn run_result(obj: usize, data: &mut [u8], call: u64) {
    unsafe {
        let run = **(obj as *const *const usize);
        #[cfg(target_arch = "x86_64")]
        let f: unsafe extern "C" fn(usize, *mut u8, bool, u64) = mem::transmute(run);
        #[cfg(target_arch = "x86")]
        let f: unsafe extern "thiscall" fn(usize, *mut u8, bool, u64) = mem::transmute(run);
        f(obj, data.as_mut_ptr(), false, call);
    }
}

unsafe extern "C" fn detour_init() -> bool {
    let Some(hook) = INIT.get() else { return false };
    CLIENT_STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let ok = unsafe { hook.call() };
    log::throttled("SteamAPI_Init", 4, |n| {
        log!("call SteamAPI_Init n={n} -> {ok}")
    });
    ok
}

unsafe extern "C" fn detour_init_safe() -> bool {
    let Some(hook) = INIT_SAFE.get() else {
        return false;
    };
    CLIENT_STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let ok = unsafe { hook.call() };
    log::throttled("SteamAPI_InitSafe", 4, |n| {
        log!("call SteamAPI_InitSafe n={n} -> {ok}")
    });
    ok
}

unsafe extern "C" fn detour_init_flat(err: *mut i8) -> i32 {
    let Some(hook) = INIT_FLAT.get() else {
        return 1;
    };
    CLIENT_STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let rc = unsafe { hook.call(err) };
    let msg = if err.is_null() {
        None
    } else {
        read::cstr(err as usize)
    };
    log::throttled(&format!("SteamAPI_InitFlat {rc}"), 4, |n| {
        log!(
            "call SteamAPI_InitFlat n={n} -> {rc} err={}",
            msg.as_deref().unwrap_or("-")
        );
    });
    rc
}

unsafe extern "C" fn detour_internal_init(versions: *const i8, err: *mut i8) -> i32 {
    let Some(hook) = INTERNAL_INIT.get() else {
        return 1;
    };
    CLIENT_STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let interfaces = read::steam_version_blob(versions as usize);
    let rc = unsafe { hook.call(versions, err) };
    let msg = if err.is_null() {
        None
    } else {
        read::cstr(err as usize)
    };
    let listed = interfaces.unwrap_or_else(|| "-".into());
    log::throttled(
        &format!("SteamInternal_SteamAPI_Init {rc} {listed}"),
        4,
        |n| {
            log!(
                "call SteamInternal_SteamAPI_Init n={n} -> {rc} interfaces={listed} err={}",
                msg.as_deref().unwrap_or("-")
            );
        },
    );
    rc
}

unsafe extern "C" fn detour_bget(pipe: i32, msg: *mut CallbackMsg, call: *mut i32) -> bool {
    let _ = (pipe, call);
    let Some(hook) = BGET.get() else { return false };
    let ok = unsafe { hook.call(pipe, msg, call) };
    if ok && !msg.is_null() {
        unsafe { crate::steamown::rewrite_raw((*msg).param, (*msg).param_size.max(0) as usize) };
        let id = unsafe { (*msg).callback };
        log::throttled(&format!("callback Steam_BGetCallback {id}"), 4, |n| {
            log!("callback Steam_BGetCallback n={n} id={id}");
        });
    }
    ok
}

unsafe extern "C" fn detour_next(pipe: i32, msg: *mut CallbackMsg) -> bool {
    let Some(hook) = NEXT_CB.get() else {
        return false;
    };
    // Ours go first, one at a time: each is freed by the game's next FreeLastCallback.
    // A call result goes out as SteamAPICallCompleted_t; the game then fetches it with
    // GetAPICallResult.
    if !msg.is_null() {
        let mut out = OUTSTANDING.lock().unwrap();
        if out.is_none() {
            // The game server's pipe gets its callbacks; every other pipe the rest.
            let server = crate::steamgs::server_pipe() == Some(pipe);
            let next = {
                let mut q = INJECT.lock().unwrap();
                q.iter().position(|p| p.server == server).and_then(|i| q.remove(i))
            };
            if let Some(p) = next {
                let (id, mut data) = if p.call == 0 {
                    (p.id, p.data)
                } else {
                    let done = CallCompleted {
                        call: p.call,
                        id: p.id,
                        size: p.data.len() as u32,
                    };
                    COMPLETED.lock().unwrap().insert(p.call, (p.id, p.data));
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            &done as *const CallCompleted as *const u8,
                            size_of::<CallCompleted>(),
                        )
                    };
                    (STEAM_API_CALL_COMPLETED, bytes.to_vec())
                };
                unsafe {
                    (*msg).steam_user = 1;
                    (*msg).callback = id;
                    (*msg).param = data.as_mut_ptr();
                    (*msg).param_size = data.len() as i32;
                }
                log::throttled(&format!("steam-lan manual {id}"), 8, |n| {
                    log!("steam-lan delivered callback {id} ({} bytes, n={n})", data.len())
                });
                *out = Some(data);
                return true;
            }
        }
    }
    let ok = unsafe { hook.call(pipe, msg) };
    if ok && !msg.is_null() {
        unsafe { crate::steamown::rewrite_raw((*msg).param, (*msg).param_size.max(0) as usize) };
        let id = unsafe { (*msg).callback };
        log::throttled(&format!("callback ManualDispatch {id}"), 4, |n| {
            log!("callback SteamAPI_ManualDispatch_GetNextCallback n={n} id={id}");
        });
    }
    ok
}

unsafe extern "C" fn detour_api_result(
    pipe: i32,
    call: u64,
    buf: *mut core::ffi::c_void,
    cub: i32,
    expected: i32,
    failed: *mut bool,
) -> bool {
    unsafe {
        log_api_result(
            &API_RESULT,
            "Steam_GetAPICallResult",
            pipe,
            call,
            buf,
            cub,
            expected,
            failed,
        )
    }
}

unsafe extern "C" fn detour_manual_result(
    pipe: i32,
    call: u64,
    buf: *mut core::ffi::c_void,
    cub: i32,
    expected: i32,
    failed: *mut bool,
) -> bool {
    if ours(call) {
        // Kept when asked for the wrong callback, as Steam does: the game can still ask
        // again with the right one.
        let mut completed = COMPLETED.lock().unwrap();
        let Some((id, _)) = completed.get(&call) else {
            return false;
        };
        if *id != expected {
            if !failed.is_null() {
                unsafe { *failed = true };
            }
            return false;
        }
        let Some((id, data)) = completed.remove(&call) else { return false };
        drop(completed);
        let n = data.len().min(cub.max(0) as usize);
        unsafe {
            if !buf.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, n);
            }
            if !failed.is_null() {
                *failed = false;
            }
        }
        return id == expected;
    }
    unsafe {
        log_api_result(
            &MANUAL_RESULT,
            "SteamAPI_ManualDispatch_GetAPICallResult",
            pipe,
            call,
            buf,
            cub,
            expected,
            failed,
        )
    }
}

unsafe fn log_api_result(
    slot: &OnceLock<GenericDetour<ApiResultFn>>,
    label: &'static str,
    pipe: i32,
    call: u64,
    buf: *mut core::ffi::c_void,
    cub: i32,
    expected: i32,
    failed: *mut bool,
) -> bool {
    let Some(hook) = slot.get() else { return false };
    let ok = unsafe { hook.call(pipe, call, buf, cub, expected, failed) };
    if ok && (failed.is_null() || !unsafe { *failed }) {
        unsafe { crate::steamown::rewrite_raw(buf as *mut u8, cub.max(0) as usize) };
    }
    let fail = if failed.is_null() {
        "-".to_string()
    } else {
        unsafe { (*failed).to_string() }
    };
    log::throttled(&format!("callback {label} {expected} {ok}"), 4, |n| {
        log!("callback {label} n={n} id={expected} call={call:#x} ok={ok} failed={fail}");
    });
    ok
}
