//! PlayFab Party's Xbox Live helper (`PartyXboxLive.dll`), shipped by Microsoft Store
//! (GDK) builds of Party games. It turns Xbox
//! users into Party chat users and PlayFab entities, asking Xbox Live for tokens and
//! privacy settings. On the LAN every `PartyXbl*` export is PeerParty's:
//!
//! - Chat users are local handles; a local one's PlayFab sign-in (`LoginToPlayFab`)
//!   completes with this copy's LAN entity (the one party.rs and the PlayFab service
//!   use), and its Xbox user id goes out in LAN presence (`peerparty:xuid`).
//! - `GetEntityIdsFromXboxLiveUserIds` maps an Xbox user id to the LAN player whose
//!   presence carries it.
//! - Privacy and permissions say everything is allowed (no Xbox Live to ask), and no
//!   token is ever requested from the game.
//!
//! Layouts: PartyXboxLive_c.h (PlayFab's Unreal plugin, Platforms/GDK/Include). State
//! changes are boxed and handed out by `PartyXblStartProcessingStateChanges`, freed at
//! Finish, as in party.rs.

use std::collections::{HashSet, VecDeque};
use std::ffi::{CString, c_char, c_void};
use std::sync::Mutex;

use crate::log;
use crate::pe::Export;

type PartyError = u32;
const OK: PartyError = 0;
const BAD: PartyError = 1;

/// The one PARTY_XBL_HANDLE handed out.
static HANDLE_MARK: u8 = 0;

fn handle() -> usize {
    &HANDLE_MARK as *const u8 as usize
}

struct ChatUser {
    xuid: u64,
    local: bool,
    context: usize,
}

/// A state change and what it points at, kept until the game finishes with it.
struct Change {
    ptr: *const u32,
    free: Box<dyn FnOnce() + Send>,
}

unsafe impl Send for Change {}

#[derive(Default)]
struct State {
    users: Vec<Box<ChatUser>>,
    handles: Vec<usize>,
    queue: VecDeque<Change>,
    out: Vec<Change>,
    out_ptrs: Vec<*const u32>,
}

unsafe impl Send for State {}

static S: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = S.lock().unwrap();
    f(g.get_or_insert_with(State::default))
}

pub fn attach(base: usize, module_name: &str, exports: &[Export]) -> HashSet<usize> {
    crate::swap::register(base, module_name, resolve);
    let n = exports.iter().filter(|e| e.name.starts_with("PartyXbl")).count();
    log!("party-xbl on for {module_name}: {n} PartyXbl functions are PeerParty's (Xbox users sign in to the LAN)");
    HashSet::new()
}

fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "PartyXblInitialize" => initialize as *const () as usize,
        "PartyXblCleanup" => ok1 as *const () as usize,
        "PartyXblStartProcessingStateChanges" => start as *const () as usize,
        "PartyXblFinishProcessingStateChanges" => finish as *const () as usize,
        "PartyXblCreateLocalChatUser" => create_local as *const () as usize,
        "PartyXblCreateRemoteChatUser" => create_remote as *const () as usize,
        "PartyXblDestroyChatUser" => destroy as *const () as usize,
        "PartyXblGetChatUsers" => get_users as *const () as usize,
        "PartyXblLoginToPlayFab" => login as *const () as usize,
        "PartyXblGetEntityIdsFromXboxLiveUserIds" => entity_ids as *const () as usize,
        "PartyXblChatUserIsLocal" => is_local as *const () as usize,
        "PartyXblChatUserGetXboxUserId" => xuid as *const () as usize,
        "PartyXblChatUserSetCustomContext" => set_context as *const () as usize,
        "PartyXblChatUserGetCustomContext" => get_context as *const () as usize,
        "PartyXblLocalChatUserGetAccessibilitySettings" => accessibility as *const () as usize,
        "PartyXblLocalChatUserGetRequiredChatPermissionInfo" => permission as *const () as usize,
        "PartyXblLocalChatUserGetCrossNetworkCommunicationPrivacySetting" => privacy as *const () as usize,
        "PartyXblGetErrorMessage" => error_message as *const () as usize,
        "PartyXblCompleteGetTokenAndSignatureRequest" => ok5 as *const () as usize,
        "PartyXblSetMemoryCallbacks" | "PartyXblSetProfilingCallbacksForMethodEntryExit" => ok2 as *const () as usize,
        "PartyXblGetMemoryCallbacks" | "PartyXblGetProfilingCallbacksForMethodEntryExit" => get_two as *const () as usize,
        "PartyXblSetThreadAffinityMask" => ok_affinity as *const () as usize,
        "PartyXblGetThreadAffinityMask" => get_affinity as *const () as usize,
        _ => return None,
    })
}

fn push<T: Send + 'static>(change: T) {
    let b = Box::new(change);
    let ptr = Box::into_raw(b);
    let addr = ptr as usize;
    with(|s| {
        s.queue.push_back(Change {
            ptr: ptr as *const u32,
            free: Box::new(move || drop(unsafe { Box::from_raw(addr as *mut T) })),
        })
    });
}

fn user_ptr(u: &ChatUser) -> usize {
    u as *const ChatUser as usize
}

fn find(h: usize) -> bool {
    with(|s| s.users.iter().any(|u| user_ptr(u) == h))
}

unsafe extern "system" fn initialize(_party: usize, _title: *const c_char, out: *mut usize) -> PartyError {
    if out.is_null() {
        return BAD;
    }
    unsafe { *out = handle() };
    log!("party-xbl initialized");
    OK
}

unsafe extern "system" fn ok1(_a: usize) -> PartyError {
    OK
}

unsafe extern "system" fn ok2(_a: usize, _b: usize) -> PartyError {
    OK
}

unsafe extern "system" fn ok5(_h: usize, _id: u32, _ok: u8, _token: usize, _sig: usize) -> PartyError {
    OK
}

unsafe extern "system" fn ok_affinity(_thread: u32, _mask: u64) -> PartyError {
    OK
}

unsafe extern "system" fn get_two(a: *mut usize, b: *mut usize) -> PartyError {
    for p in [a, b] {
        if !p.is_null() {
            unsafe { *p = 0 };
        }
    }
    OK
}

unsafe extern "system" fn get_affinity(_thread: u32, mask: *mut u64) -> PartyError {
    if !mask.is_null() {
        unsafe { *mask = 0 };
    }
    OK
}

unsafe extern "system" fn start(_h: usize, count: *mut u32, changes: *mut *const *const u32) -> PartyError {
    if count.is_null() || changes.is_null() {
        return BAD;
    }
    with(|s| {
        let batch: Vec<Change> = s.queue.drain(..).collect();
        s.out_ptrs = batch.iter().map(|c| c.ptr).collect();
        s.out.extend(batch);
        unsafe {
            *count = s.out_ptrs.len() as u32;
            *changes = s.out_ptrs.as_ptr();
        }
    });
    OK
}

unsafe extern "system" fn finish(_h: usize, _count: u32, _changes: usize) -> PartyError {
    let done: Vec<Change> = with(|s| {
        s.out_ptrs.clear();
        s.out.drain(..).collect()
    });
    for c in done {
        (c.free)();
    }
    OK
}

#[repr(C)]
struct CreateLocalCompleted {
    kind: u32,
    result: u32,
    error: u32,
    async_id: usize,
    user: usize,
}

unsafe extern "system" fn create_local(_h: usize, xuid: u64, async_id: usize, out: *mut usize) -> PartyError {
    let h = with(|s| {
        let u = Box::new(ChatUser { xuid, local: true, context: 0 });
        let h = user_ptr(&u);
        s.users.push(u);
        s.handles.push(h);
        h
    });
    if !out.is_null() {
        unsafe { *out = h };
    }
    publish_xuid(xuid);
    push(CreateLocalCompleted { kind: 0, result: 0, error: 0, async_id, user: h });
    log!("party-xbl local chat user for Xbox user {xuid}");
    OK
}

unsafe extern "system" fn create_remote(_h: usize, xuid: u64, out: *mut usize) -> PartyError {
    let h = with(|s| {
        if let Some(u) = s.users.iter().find(|u| u.xuid == xuid && !u.local) {
            return user_ptr(u);
        }
        let u = Box::new(ChatUser { xuid, local: false, context: 0 });
        let h = user_ptr(&u);
        s.users.push(u);
        s.handles.push(h);
        h
    });
    if !out.is_null() {
        unsafe { *out = h };
    }
    OK
}

unsafe extern "system" fn destroy(_h: usize, user: usize) -> PartyError {
    // Kept alive (a game may read a handle in the same batch); just forgotten.
    with(|s| s.handles.retain(|h| *h != user));
    OK
}

unsafe extern "system" fn get_users(_h: usize, count: *mut u32, users: *mut *const usize) -> PartyError {
    if count.is_null() || users.is_null() {
        return BAD;
    }
    with(|s| unsafe {
        *count = s.handles.len() as u32;
        *users = s.handles.as_ptr();
    });
    OK
}

#[repr(C)]
struct LoginCompleted {
    kind: u32,
    result: u32,
    error: u32,
    user: usize,
    async_id: usize,
    entity: *const c_char,
    token: *const c_char,
    expires: i64,
    _keep: (CString, CString),
}

unsafe impl Send for LoginCompleted {}

unsafe extern "system" fn login(user: usize, async_id: usize) -> PartyError {
    if !find(user) {
        return BAD;
    }
    let entity = CString::new(crate::playfab::entity_id()).unwrap_or_default();
    let token = CString::new(format!("peerparty-{}", crate::playfab::entity_id())).unwrap_or_default();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    push(LoginCompleted {
        kind: 4,
        result: 0,
        error: 0,
        user,
        async_id,
        entity: entity.as_ptr(),
        token: token.as_ptr(),
        expires: now + 86_400,
        _keep: (entity, token),
    });
    log!("party-xbl LoginToPlayFab: signed in as the LAN entity");
    OK
}

#[repr(C)]
struct Mapping {
    xuid: u64,
    entity: *const c_char,
}

#[repr(C)]
struct EntityIdsCompleted {
    kind: u32,
    result: u32,
    error: u32,
    sandbox: *const c_char,
    user: usize,
    async_id: usize,
    count: u32,
    mappings: *const Mapping,
    _keep: (Vec<Mapping>, Vec<CString>),
}

unsafe impl Send for EntityIdsCompleted {}

const XUID_KEY: &str = "peerparty:xuid";

fn publish_xuid(xuid: u64) {
    crate::net::set_presence("xbl", vec![(XUID_KEY.into(), xuid.to_string())]);
}

/// The LAN player whose presence carries this Xbox user id.
fn peer_of_xuid(xuid: u64) -> Option<peerparty_core::PeerId> {
    let lan = crate::net::lan()?;
    let want = xuid.to_string();
    lan.peers().into_iter().map(|(p, _)| p).find(|p| lan.presence(*p).iter().any(|(k, v)| k == XUID_KEY && *v == want))
}

unsafe extern "system" fn entity_ids(_h: usize, count: u32, xuids: *const u64, user: usize, async_id: usize) -> PartyError {
    let ids: Vec<u64> = if xuids.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(xuids, count as usize) }.to_vec() };
    let mut strings = Vec::new();
    let mut maps = Vec::new();
    let mine = with(|s| s.users.iter().find(|u| u.local).map(|u| u.xuid));
    for x in &ids {
        let entity = if Some(*x) == mine {
            Some(crate::playfab::entity_id())
        } else {
            peer_of_xuid(*x).map(crate::playfab::entity_of)
        };
        if let Some(e) = entity {
            let c = CString::new(e).unwrap_or_default();
            maps.push(Mapping { xuid: *x, entity: c.as_ptr() });
            strings.push(c);
        }
    }
    log!("party-xbl entity ids for {} Xbox users: {} on the LAN", ids.len(), maps.len());
    push(EntityIdsCompleted {
        kind: 5,
        result: 0,
        error: 0,
        sandbox: c"RETAIL".as_ptr(),
        user,
        async_id,
        count: maps.len() as u32,
        mappings: maps.as_ptr(),
        _keep: (maps, strings),
    });
    OK
}

unsafe extern "system" fn is_local(user: usize, out: *mut u8) -> PartyError {
    let local = with(|s| s.users.iter().find(|u| user_ptr(u) == user).map(|u| u.local));
    match (local, out.is_null()) {
        (Some(l), false) => {
            unsafe { *out = u8::from(l) };
            OK
        }
        _ => BAD,
    }
}

unsafe extern "system" fn xuid(user: usize, out: *mut u64) -> PartyError {
    let x = with(|s| s.users.iter().find(|u| user_ptr(u) == user).map(|u| u.xuid));
    match (x, out.is_null()) {
        (Some(x), false) => {
            unsafe { *out = x };
            OK
        }
        _ => BAD,
    }
}

unsafe extern "system" fn set_context(user: usize, ctx: usize) -> PartyError {
    with(|s| match s.users.iter_mut().find(|u| user_ptr(u) == user) {
        Some(u) => {
            u.context = ctx;
            OK
        }
        None => BAD,
    })
}

unsafe extern "system" fn get_context(user: usize, out: *mut usize) -> PartyError {
    let c = with(|s| s.users.iter().find(|u| user_ptr(u) == user).map(|u| u.context));
    match (c, out.is_null()) {
        (Some(c), false) => {
            unsafe { *out = c };
            OK
        }
        _ => BAD,
    }
}

/// PARTY_XBL_ACCESSIBILITY_SETTINGS: two PartyBools, the language (84 + 1 chars), gender.
unsafe extern "system" fn accessibility(user: usize, out: *mut u8) -> PartyError {
    if out.is_null() || !find(user) {
        return BAD;
    }
    unsafe {
        std::ptr::write_bytes(out, 0, 2 + 85 + 1 + 4);
        let lang = b"en-US";
        std::ptr::copy_nonoverlapping(lang.as_ptr(), out.add(2), lang.len());
    }
    OK
}

/// Everything allowed: send and receive audio, receive text; no restriction.
unsafe extern "system" fn permission(user: usize, _target: usize, out: *mut u32) -> PartyError {
    if out.is_null() || !find(user) {
        return BAD;
    }
    unsafe {
        *out = 0x1f;
        *out.add(1) = 0;
    }
    OK
}

unsafe extern "system" fn privacy(user: usize, out: *mut u32) -> PartyError {
    if out.is_null() || !find(user) {
        return BAD;
    }
    unsafe { *out = 0 };
    OK
}

unsafe extern "system" fn error_message(error: u32, out: *mut *const c_char) -> PartyError {
    if out.is_null() {
        return BAD;
    }
    unsafe { *out = if error == OK { c"Success".as_ptr() } else { c"peerparty: the call failed".as_ptr() } };
    let _: *const c_void = std::ptr::null();
    OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn layouts_follow_the_header() {
        // x64: PARTY_XBL_LOGIN_TO_PLAYFAB_COMPLETED_STATE_CHANGE.
        assert_eq!(offset_of!(LoginCompleted, user), 16);
        assert_eq!(offset_of!(LoginCompleted, entity), 32);
        assert_eq!(offset_of!(LoginCompleted, expires), 48);
        assert_eq!(offset_of!(CreateLocalCompleted, async_id), 16);
        assert_eq!(offset_of!(EntityIdsCompleted, mappings), 48);
        assert_eq!(size_of::<Mapping>(), 16);
    }

    #[test]
    fn chat_users_and_state_changes() {
        unsafe {
            let mut h = 0usize;
            assert_eq!(initialize(0, std::ptr::null(), &mut h), OK);
            let mut u = 0usize;
            assert_eq!(create_local(h, 42, 7, &mut u), OK);
            let mut local = 0u8;
            assert_eq!(is_local(u, &mut local), OK);
            assert_eq!(local, 1);
            let mut x = 0u64;
            assert_eq!(xuid(u, &mut x), OK);
            assert_eq!(x, 42);
            let mut perm = [0u32; 2];
            assert_eq!(permission(u, 0, perm.as_mut_ptr()), OK);
            assert_eq!(perm[0], 0x1f);
            let (mut n, mut list) = (0u32, std::ptr::null::<*const u32>());
            assert_eq!(start(h, &mut n, &mut list), OK);
            assert_eq!(n, 1);
            let first = &*(*list as *const CreateLocalCompleted);
            assert_eq!((first.kind, first.result, first.async_id, first.user), (0, 0, 7, u));
            assert_eq!(finish(h, n, list as usize), OK);
            assert_eq!(start(h, &mut n, &mut list), OK);
            assert_eq!(n, 0);
        }
    }
}
