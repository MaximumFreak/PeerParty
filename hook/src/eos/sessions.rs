//! EOS Sessions on the LAN core's lobbies (namespace "eos.session"). This layer's rules:
//!
//! - a new session is Pending; StartSession makes it InProgress and EndSession Ended.
//! - the host is registered in its own session; only a registered player registers
//!   others, and open connections are capacity minus registered players.
//! - destroying a session we host ends it for everyone; one we joined, we just leave.
//!
//! Sessions are named locally ("GameSession"); the core lobby id is the session id.
//! The host address, join-in-progress, state and registered players ride along as
//! hidden attributes ("PeerParty:..."), never listed to the game. A session whose
//! host never set an address reads as the host's LAN IP on the other copies.

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, c_char};
use std::sync::{LazyLock, Mutex};

use peerparty_core::{Attr, Event, Filter, JoinResult, Lan, Lobby, LobbyError, Permission, Query, Value};

use super::abi::{self, result::*};
use super::ids::{self, intern, text};
use super::lobby::{op, read_attr};
use super::{
    Kind, add_listener, call, hand_out, lan, later, later_after, listeners, owned_str, release,
    remove_listener, trace,
};

pub(super) const NS: &str = "eos.session";
const HIDDEN: &str = "peerparty:";
const KEY_HOST: &str = "peerparty:host";
const KEY_JIP: &str = "peerparty:joininprogress";
const KEY_STATE: &str = "peerparty:state";
const KEY_REGISTERED: &str = "peerparty:registered";

const SESSION_ALREADY_EXISTS: i32 = 5003;
const SESSION_INVALID: i32 = 5005;
const SESSION_TOO_MANY_PLAYERS: i32 = 5001;
const SESSION_NOT_ALLOWED: i32 = 5018;

// EOS_EOnlineSessionState
const STATE_PENDING: i32 = 2;
const STATE_IN_PROGRESS: i32 = 4;
const STATE_ENDED: i32 = 6;

#[allow(non_snake_case)]
#[repr(C)]
struct CreateOptions {
    ApiVersion: i32,
    SessionName: *const c_char,
    BucketId: *const c_char,
    MaxPlayers: u32,
    LocalUserId: usize,
    bPresenceEnabled: i32,
    SessionId: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct JoinOptions {
    ApiVersion: i32,
    SessionName: *const c_char,
    SessionHandle: usize,
    LocalUserId: usize,
    bPresenceEnabled: i32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct PlayersOptions {
    ApiVersion: i32,
    SessionName: *const c_char,
    Players: *const usize,
    Count: u32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct SendInviteOptions {
    ApiVersion: i32,
    SessionName: *const c_char,
    LocalUserId: usize,
    TargetUserId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct RejectInviteOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    InviteId: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct NameAndUser {
    ApiVersion: i32,
    SessionName: *const c_char,
    TargetUserId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct RemoveParameterOptions {
    ApiVersion: i32,
    Key: *const c_char,
    ComparisonOp: i32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct UpdateInfo {
    ResultCode: i32,
    ClientData: usize,
    SessionName: *const c_char,
    SessionId: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct RegisterInfo {
    ResultCode: i32,
    ClientData: usize,
    Players: *const usize,
    Count: u32,
    Sanctioned: *const usize,
    SanctionedCount: u32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct Settings {
    ApiVersion: i32,
    BucketId: *const c_char,
    NumPublicConnections: u32,
    bAllowJoinInProgress: i32,
    PermissionLevel: i32,
    bInvitesAllowed: i32,
    bSanctionsEnabled: i32,
    AllowedPlatformIds: *const u32,
    AllowedPlatformIdsCount: u32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct DetailsInfo {
    ApiVersion: i32,
    SessionId: *const c_char,
    HostAddress: *const c_char,
    NumOpenPublicConnections: u32,
    Settings: *const Settings,
    OwnerUserId: usize,
    OwnerServerClientId: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct ActiveInfo {
    ApiVersion: i32,
    SessionName: *const c_char,
    LocalUserId: usize,
    State: i32,
    SessionDetails: *const DetailsInfo,
}

#[allow(non_snake_case)]
#[repr(C)]
struct InviteReceivedInfo {
    ClientData: usize,
    LocalUserId: usize,
    TargetUserId: usize,
    InviteId: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct InviteAcceptedInfo {
    ClientData: usize,
    SessionId: *const c_char,
    LocalUserId: usize,
    TargetUserId: usize,
    InviteId: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct JoinAcceptedInfo {
    ClientData: usize,
    LocalUserId: usize,
    UiEventId: u64,
}

struct Session {
    id: String,
    presence: bool,
}

struct ModObj {
    name: String,
    creating: bool,
    edited: Lobby,
    presence: bool,
}

struct SearchObj {
    query: Query,
    results: Vec<Lobby>,
}

#[derive(Default)]
struct Handles {
    /// Our sessions by local name.
    sessions: HashMap<String, Session>,
    /// Every core id known to be a session, ours or seen.
    ids: HashSet<String>,
    mods: HashMap<usize, Box<ModObj>>,
    searches: HashMap<usize, Box<SearchObj>>,
    finds: HashMap<u64, (usize, usize, usize, bool)>,
    details: HashMap<usize, Box<Lobby>>,
    actives: HashMap<usize, Box<String>>,
    /// core id -> (name, presence, callback, client data) for a join in flight.
    joins: HashMap<String, (String, bool, usize, usize)>,
    invites: Vec<(String, Lobby, peerparty_core::PeerId)>,
    ui_events: HashMap<u64, Lobby>,
    next_invite: u64,
    /// Registered players of sessions we joined, as we registered them.
    registered: HashMap<String, Vec<peerparty_core::PeerId>>,
}

static H: LazyLock<Mutex<Handles>> = LazyLock::new(Mutex::default);

fn key<T>(b: &T) -> usize {
    b as *const T as usize
}

/// Whether a core lobby id is a session's.
pub(super) fn is_session(id: &str) -> bool {
    H.lock().unwrap().ids.contains(id)
}

/// The presence-enabled session we're in, for invites.
pub(super) fn current() -> Option<Lobby> {
    let id = {
        let h = H.lock().unwrap();
        let s = h.sessions.values().find(|s| s.presence).or_else(|| h.sessions.values().next())?;
        s.id.clone()
    };
    lan()?.lobby(&id)
}

pub fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "EOS_Sessions_CreateSessionModification" => create_modification as *const () as usize,
        "EOS_Sessions_UpdateSessionModification" => update_modification as *const () as usize,
        "EOS_Sessions_UpdateSession" => update_session as *const () as usize,
        "EOS_Sessions_DestroySession" => destroy_session as *const () as usize,
        "EOS_Sessions_JoinSession" => join_session as *const () as usize,
        "EOS_Sessions_StartSession" => start_session as *const () as usize,
        "EOS_Sessions_EndSession" => end_session as *const () as usize,
        "EOS_Sessions_RegisterPlayers" => register_players as *const () as usize,
        "EOS_Sessions_UnregisterPlayers" => unregister_players as *const () as usize,
        "EOS_Sessions_SendInvite" => send_invite as *const () as usize,
        "EOS_Sessions_RejectInvite" => reject_invite as *const () as usize,
        "EOS_Sessions_QueryInvites" => query_invites as *const () as usize,
        "EOS_Sessions_GetInviteCount" => invite_count as *const () as usize,
        "EOS_Sessions_GetInviteIdByIndex" => invite_id_by_index as *const () as usize,
        "EOS_Sessions_CreateSessionSearch" => create_search as *const () as usize,
        "EOS_Sessions_CopyActiveSessionHandle" => copy_active as *const () as usize,
        "EOS_Sessions_CopySessionHandleByInviteId" => copy_by_invite as *const () as usize,
        "EOS_Sessions_CopySessionHandleByUiEventId" => copy_by_ui_event as *const () as usize,
        "EOS_Sessions_CopySessionHandleForPresence" => copy_for_presence as *const () as usize,
        "EOS_Sessions_IsUserInSession" => is_user_in_session as *const () as usize,
        "EOS_Sessions_DumpSessionState" => dump_state as *const () as usize,
        "EOS_Sessions_AddNotifySessionInviteReceived" => notify_invite_received as *const () as usize,
        "EOS_Sessions_AddNotifySessionInviteAccepted" => notify_invite_accepted as *const () as usize,
        "EOS_Sessions_AddNotifyJoinSessionAccepted" => notify_join_accepted as *const () as usize,
        n if n.starts_with("EOS_Sessions_AddNotify") => notify_never as *const () as usize,
        n if n.starts_with("EOS_Sessions_RemoveNotify") => remove_notify as *const () as usize,

        "EOS_SessionModification_SetBucketId" => mod_set_bucket as *const () as usize,
        "EOS_SessionModification_SetHostAddress" => mod_set_host as *const () as usize,
        "EOS_SessionModification_SetPermissionLevel" => mod_set_permission as *const () as usize,
        "EOS_SessionModification_SetJoinInProgressAllowed" => mod_set_jip as *const () as usize,
        "EOS_SessionModification_SetMaxPlayers" => mod_set_max as *const () as usize,
        "EOS_SessionModification_SetInvitesAllowed" => mod_set_invites as *const () as usize,
        "EOS_SessionModification_SetAllowedPlatformIds" => mod_ignore as *const () as usize,
        "EOS_SessionModification_AddAttribute" => mod_add_attribute as *const () as usize,
        "EOS_SessionModification_RemoveAttribute" => mod_remove_attribute as *const () as usize,
        "EOS_SessionModification_Release" => mod_release as *const () as usize,

        "EOS_SessionSearch_SetParameter" => search_set_parameter as *const () as usize,
        "EOS_SessionSearch_RemoveParameter" => search_remove_parameter as *const () as usize,
        "EOS_SessionSearch_SetSessionId" => search_set_id as *const () as usize,
        "EOS_SessionSearch_SetTargetUserId" => search_set_target as *const () as usize,
        "EOS_SessionSearch_SetMaxResults" => search_set_max as *const () as usize,
        "EOS_SessionSearch_Find" => search_find as *const () as usize,
        "EOS_SessionSearch_GetSearchResultCount" => search_count as *const () as usize,
        "EOS_SessionSearch_CopySearchResultByIndex" => search_copy as *const () as usize,
        "EOS_SessionSearch_Release" => search_release as *const () as usize,

        "EOS_SessionDetails_CopyInfo" => details_copy_info as *const () as usize,
        "EOS_SessionDetails_GetSessionAttributeCount" => details_attr_count as *const () as usize,
        "EOS_SessionDetails_CopySessionAttributeByIndex" => details_attr_by_index as *const () as usize,
        "EOS_SessionDetails_CopySessionAttributeByKey" => details_attr_by_key as *const () as usize,
        "EOS_SessionDetails_Release" => details_release as *const () as usize,
        "EOS_SessionDetails_Info_Release" | "EOS_SessionDetails_Attribute_Release"
        | "EOS_ActiveSession_Info_Release" => free as *const () as usize,

        "EOS_ActiveSession_CopyInfo" => active_copy_info as *const () as usize,
        "EOS_ActiveSession_GetRegisteredPlayerCount" => active_player_count as *const () as usize,
        "EOS_ActiveSession_GetRegisteredPlayerByIndex" => active_player_by_index as *const () as usize,
        "EOS_ActiveSession_Release" => active_release as *const () as usize,
        _ => return None,
    })
}

// ---- helpers ----

fn name_of(p: *const c_char) -> Option<String> {
    unsafe { text(p) }.filter(|s| !s.is_empty())
}

unsafe fn ptr_text(o: *const abi::PtrOptions) -> Option<String> {
    if o.is_null() {
        return None;
    }
    name_of(unsafe { (*o).Value } as *const c_char)
}

fn session_id(name: &str) -> Option<String> {
    H.lock().unwrap().sessions.get(name).map(|s| s.id.clone())
}

fn hidden(l: &Lobby, key: &str) -> Option<String> {
    l.attrs.iter().find(|a| a.key == key).map(|a| a.value.text())
}

fn set_hidden(l: &mut Lobby, key: &str, value: String) {
    peerparty_core::set_attr(
        &mut l.attrs,
        Attr {
            key: key.into(),
            value: Value::Str(value),
            public: true,
        },
    );
}

fn registered(l: &Lobby) -> Vec<peerparty_core::PeerId> {
    hidden(l, KEY_REGISTERED)
        .unwrap_or_default()
        .split(',')
        .filter_map(peerparty_core::PeerId::from_hex)
        .collect()
}

fn set_registered(l: &mut Lobby, players: &[peerparty_core::PeerId]) {
    let v = players.iter().map(|p| p.hex()).collect::<Vec<_>>().join(",");
    set_hidden(l, KEY_REGISTERED, v);
}

fn state(l: &Lobby) -> i32 {
    hidden(l, KEY_STATE)
        .and_then(|v| v.parse().ok())
        .unwrap_or(STATE_PENDING)
}

fn lobby_error(e: LobbyError) -> i32 {
    match e {
        LobbyError::NotFound | LobbyError::NotMember | LobbyError::OwnerUnknown => NOT_FOUND,
        LobbyError::AlreadyIn => SESSION_ALREADY_EXISTS,
        LobbyError::NotOwner => SESSION_NOT_ALLOWED,
    }
}

fn done(cb: usize, cd: usize, result: i32) {
    later(move || {
        let info = abi::Completion {
            ResultCode: result,
            ClientData: cd,
        };
        unsafe { call(cb, &info) };
    });
}

/// Applies an edit to a session we host.
fn edit_hosted(name: &str, f: impl FnOnce(&mut Lobby)) -> Result<(), i32> {
    let id = session_id(name).ok_or(NOT_FOUND)?;
    let lan = lan().ok_or(NO_CONNECTION)?;
    let mut l = lan.lobby(&id).ok_or(NOT_FOUND)?;
    if !lan.is_owner(&id) {
        return Err(SESSION_NOT_ALLOWED);
    }
    f(&mut l);
    lan.update(&id, Some(&l), &[], &[]).map_err(lobby_error)
}

// ---- lifecycle ----

unsafe extern "system" fn create_modification(_h: usize, o: *const CreateOptions, out: *mut usize) -> i32 {
    if o.is_null() || out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let o = unsafe { &*o };
    let Some(name) = name_of(o.SessionName) else {
        return INVALID_PARAMETERS;
    };
    let mut l = Lobby::new(NS);
    l.bucket = unsafe { text(o.BucketId) }.unwrap_or_default();
    l.max_members = o.MaxPlayers.max(1);
    l.join_by_id = true;
    l.host_migration = false;
    if o.ApiVersion >= 3 {
        l.id = unsafe { text(o.SessionId) }.unwrap_or_default();
    }
    let m = Box::new(ModObj {
        name,
        creating: true,
        edited: l,
        presence: o.ApiVersion >= 2 && o.bPresenceEnabled != 0,
    });
    let k = key(&*m);
    H.lock().unwrap().mods.insert(k, m);
    unsafe { *out = k };
    SUCCESS
}

unsafe extern "system" fn update_modification(_h: usize, o: *const abi::PtrOptions, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(name) = (unsafe { ptr_text(o) }) else {
        return INVALID_PARAMETERS;
    };
    let Some(l) = session_id(&name).and_then(|id| lan()?.lobby(&id)) else {
        return NOT_FOUND;
    };
    let m = Box::new(ModObj {
        name,
        creating: false,
        edited: l,
        presence: false,
    });
    let k = key(&*m);
    H.lock().unwrap().mods.insert(k, m);
    unsafe { *out = k };
    SUCCESS
}

fn with_mod(h: usize, f: impl FnOnce(&mut ModObj) -> i32) -> i32 {
    match H.lock().unwrap().mods.get_mut(&h) {
        Some(m) => f(m),
        None => INVALID_PARAMETERS,
    }
}

unsafe extern "system" fn mod_set_bucket(h: usize, o: *const abi::PtrOptions) -> i32 {
    let Some(b) = (unsafe { ptr_text(o) }) else {
        return INVALID_PARAMETERS;
    };
    with_mod(h, |m| {
        m.edited.bucket = b;
        SUCCESS
    })
}

unsafe extern "system" fn mod_set_host(h: usize, o: *const abi::PtrOptions) -> i32 {
    let Some(a) = (unsafe { ptr_text(o) }) else {
        return INVALID_PARAMETERS;
    };
    with_mod(h, |m| {
        set_hidden(&mut m.edited, KEY_HOST, a);
        SUCCESS
    })
}

unsafe extern "system" fn mod_set_permission(h: usize, o: *const abi::I32Options) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let p = Permission::from_u8(unsafe { (*o).Value } as u8);
    with_mod(h, |m| {
        m.edited.permission = p;
        SUCCESS
    })
}

unsafe extern "system" fn mod_set_jip(h: usize, o: *const abi::I32Options) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let allow = unsafe { (*o).Value } != 0;
    with_mod(h, |m| {
        set_hidden(&mut m.edited, KEY_JIP, (allow as u8).to_string());
        SUCCESS
    })
}

unsafe extern "system" fn mod_set_max(h: usize, o: *const abi::U32Options) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let n = unsafe { (*o).Value };
    with_mod(h, |m| {
        m.edited.max_members = n.max(1);
        SUCCESS
    })
}

unsafe extern "system" fn mod_set_invites(h: usize, o: *const abi::I32Options) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let allow = unsafe { (*o).Value } != 0;
    with_mod(h, |m| {
        m.edited.allow_invites = allow;
        SUCCESS
    })
}

unsafe extern "system" fn mod_ignore(h: usize, _o: usize) -> i32 {
    with_mod(h, |_| SUCCESS)
}

/// Sessions advertise with 1 and keep private with 0: the lobby's visibility the
/// other way round.
unsafe extern "system" fn mod_add_attribute(h: usize, o: *const abi::AddAttributeOptions) -> i32 {
    let Some(a) = (!o.is_null())
        .then(|| unsafe {
            let advertise = (*o).Visibility == 1;
            read_attr((*o).Attribute, if advertise { abi::VISIBILITY_PUBLIC } else { 1 })
        })
        .flatten()
        .filter(|a| !a.key.starts_with(HIDDEN))
    else {
        return INVALID_PARAMETERS;
    };
    with_mod(h, |m| {
        peerparty_core::set_attr(&mut m.edited.attrs, a);
        SUCCESS
    })
}

unsafe extern "system" fn mod_remove_attribute(h: usize, o: *const abi::KeyOptions) -> i32 {
    let Some(k) = (!o.is_null()).then(|| unsafe { text((*o).Key) }).flatten() else {
        return INVALID_PARAMETERS;
    };
    with_mod(h, |m| {
        peerparty_core::remove_attr(&mut m.edited.attrs, &k);
        SUCCESS
    })
}

unsafe extern "system" fn mod_release(h: usize) {
    H.lock().unwrap().mods.remove(&h);
}

unsafe extern "system" fn update_session(_h: usize, o: *const abi::PtrOptions, cd: usize, cb: usize) {
    let m = (!o.is_null())
        .then(|| {
            let h = H.lock().unwrap();
            h.mods
                .get(&unsafe { (*o).Value })
                .map(|m| (m.name.clone(), m.creating, m.edited.clone(), m.presence))
        })
        .flatten();
    let finish = move |result: i32, name: String, id: String| {
        later(move || {
            let info = UpdateInfo {
                ResultCode: result,
                ClientData: cd,
                SessionName: intern(&name),
                SessionId: intern(&id),
            };
            unsafe { call(cb, &info) };
        });
    };
    let (Some((name, creating, mut edited, presence)), Some(lan)) = (m, lan()) else {
        return finish(INVALID_PARAMETERS, String::new(), String::new());
    };
    if creating {
        if session_id(&name).is_some() {
            return finish(SESSION_ALREADY_EXISTS, name, String::new());
        }
        // The host is registered in its own session.
        set_registered(&mut edited, &[lan.me()]);
        set_hidden(&mut edited, KEY_STATE, STATE_PENDING.to_string());
        match lan.create_lobby(edited) {
            Ok(l) => {
                trace("EOS_Sessions_UpdateSession", || {
                    format!("created {name} id={} bucket={} max={}", l.id, l.bucket, l.max_members)
                });
                let mut h = H.lock().unwrap();
                h.ids.insert(l.id.clone());
                h.sessions.insert(
                    name.clone(),
                    Session {
                        id: l.id.clone(),
                        presence,
                    },
                );
                drop(h);
                finish(SUCCESS, name, l.id)
            }
            Err(e) => finish(lobby_error(e), name, String::new()),
        }
    } else {
        let id = edited.id.clone();
        let r = lan.update(&id, Some(&edited), &[], &[]);
        trace("EOS_Sessions_UpdateSession", || format!("updated {name} -> {r:?}"));
        finish(r.map_or_else(lobby_error, |_| SUCCESS), name, id)
    }
}

unsafe extern "system" fn destroy_session(_h: usize, o: *const abi::PtrOptions, cd: usize, cb: usize) {
    let Some(name) = (unsafe { ptr_text(o) }) else {
        return done(cb, cd, INVALID_PARAMETERS);
    };
    let s = H.lock().unwrap().sessions.remove(&name);
    let Some(s) = s else {
        return done(cb, cd, NOT_FOUND);
    };
    let r = lan().map_or(Err(LobbyError::NotFound), |l| l.leave(&s.id));
    trace("EOS_Sessions_DestroySession", || format!("{name} {} -> {r:?}", s.id));
    done(cb, cd, SUCCESS);
}

unsafe extern "system" fn join_session(_h: usize, o: *const JoinOptions, cd: usize, cb: usize) {
    if o.is_null() {
        return done(cb, cd, INVALID_PARAMETERS);
    }
    let o = unsafe { &*o };
    let name = name_of(o.SessionName).unwrap_or_default();
    let target = H.lock().unwrap().details.get(&o.SessionHandle).map(|l| (**l).clone());
    let (Some(lan), Some(target), false) = (lan(), target, name.is_empty()) else {
        return done(cb, cd, INVALID_PARAMETERS);
    };
    if session_id(&name).is_some() {
        return done(cb, cd, SESSION_ALREADY_EXISTS);
    }
    let presence = o.ApiVersion >= 2 && o.bPresenceEnabled != 0;
    trace("EOS_Sessions_JoinSession", || format!("{name} -> {} owner={}", target.id, target.owner));
    H.lock().unwrap().ids.insert(target.id.clone());
    match lan.join(&target) {
        Ok(()) => {
            H.lock().unwrap().joins.insert(target.id, (name, presence, cb, cd));
        }
        Err(e) => done(cb, cd, lobby_error(e)),
    }
}

fn set_state(o: *const abi::PtrOptions, cd: usize, cb: usize, new: i32, allowed: &[i32]) {
    let Some(name) = (unsafe { ptr_text(o) }) else {
        return done(cb, cd, INVALID_PARAMETERS);
    };
    let Some(id) = session_id(&name) else {
        return done(cb, cd, NOT_FOUND);
    };
    let Some(lan) = lan() else {
        return done(cb, cd, NO_CONNECTION);
    };
    // Members follow the host's state; only the host moves it.
    if !lan.is_owner(&id) {
        return done(cb, cd, SUCCESS);
    }
    let now = lan.lobby(&id).map_or(STATE_PENDING, |l| state(&l));
    if !allowed.contains(&now) {
        return done(cb, cd, INVALID_PARAMETERS);
    }
    let r = edit_hosted(&name, |l| set_hidden(l, KEY_STATE, new.to_string()));
    done(cb, cd, r.err().unwrap_or(SUCCESS));
}

unsafe extern "system" fn start_session(_h: usize, o: *const abi::PtrOptions, cd: usize, cb: usize) {
    set_state(o, cd, cb, STATE_IN_PROGRESS, &[STATE_PENDING, STATE_ENDED]);
}

unsafe extern "system" fn end_session(_h: usize, o: *const abi::PtrOptions, cd: usize, cb: usize) {
    set_state(o, cd, cb, STATE_ENDED, &[STATE_IN_PROGRESS]);
}

fn players(o: &PlayersOptions) -> Vec<peerparty_core::PeerId> {
    if o.Players.is_null() {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(o.Players, o.Count as usize) }
        .iter()
        .filter_map(|p| ids::peer_of_puid(*p))
        .collect()
}

/// Players a register call changed, echoed back in its callback.
fn players_done(cb: usize, cd: usize, result: i32, changed: Vec<peerparty_core::PeerId>) {
    later(move || {
        let ids: Vec<usize> = changed.iter().map(|p| ids::puid(*p)).collect();
        let info = RegisterInfo {
            ResultCode: result,
            ClientData: cd,
            Players: ids.as_ptr(),
            Count: ids.len() as u32,
            Sanctioned: std::ptr::null(),
            SanctionedCount: 0,
        };
        unsafe { call(cb, &info) };
    });
}

fn change_registration(o: *const PlayersOptions, cd: usize, cb: usize, add: bool) {
    if o.is_null() {
        return players_done(cb, cd, INVALID_PARAMETERS, Vec::new());
    }
    let o = unsafe { &*o };
    let (Some(name), Some(lan)) = (name_of(o.SessionName), lan()) else {
        return players_done(cb, cd, INVALID_PARAMETERS, Vec::new());
    };
    let Some(id) = session_id(&name) else {
        return players_done(cb, cd, NOT_FOUND, Vec::new());
    };
    let Some(l) = lan.lobby(&id) else {
        return players_done(cb, cd, NOT_FOUND, Vec::new());
    };
    let hosting = lan.is_owner(&id);
    let mut list = if hosting {
        registered(&l)
    } else {
        H.lock().unwrap().registered.get(&id).cloned().unwrap_or_else(|| registered(&l))
    };
    // Epic answers Success and echoes every player asked about, also those already
    // (un)registered: the host is registered here at creation, and Unreal's EOS
    // subsystem drops the session when registering itself answers anything else.
    let mut changed = Vec::new();
    for p in players(o) {
        let there = list.contains(&p);
        if add && !there {
            if list.len() as u32 >= l.max_members {
                return players_done(cb, cd, SESSION_TOO_MANY_PLAYERS, changed);
            }
            list.push(p);
        } else if !add && there {
            list.retain(|x| *x != p);
        }
        changed.push(p);
    }
    let count = list.len();
    if hosting {
        let _ = edit_hosted(&name, |l| set_registered(l, &list));
    } else {
        H.lock().unwrap().registered.insert(id, list);
    }
    crate::log!(
        "eos-lan call EOS_Sessions_{}RegisterPlayers {name}: {} asked, {} registered",
        if add { "" } else { "Un" },
        changed.len(),
        count
    );
    players_done(cb, cd, SUCCESS, changed);
}

unsafe extern "system" fn register_players(_h: usize, o: *const PlayersOptions, cd: usize, cb: usize) {
    change_registration(o, cd, cb, true);
}

unsafe extern "system" fn unregister_players(_h: usize, o: *const PlayersOptions, cd: usize, cb: usize) {
    // The unregister callback is the register one without the sanctioned list.
    change_registration(o, cd, cb, false);
}

// ---- invites ----

unsafe extern "system" fn send_invite(_h: usize, o: *const SendInviteOptions, cd: usize, cb: usize) {
    let (name, target) = if o.is_null() {
        (None, None)
    } else {
        unsafe { (name_of((*o).SessionName), ids::peer_of_puid((*o).TargetUserId)) }
    };
    let lobby = name.and_then(|n| session_id(&n)).and_then(|id| lan()?.lobby(&id));
    let r = match (lan(), lobby, target) {
        (Some(l), Some(lobby), Some(t)) if l.invite(Some(t), "", Some(&lobby)) > 0 => SUCCESS,
        (_, None, _) => NOT_FOUND,
        _ => INVALID_PARAMETERS,
    };
    done(cb, cd, r);
}

unsafe extern "system" fn reject_invite(_h: usize, o: *const RejectInviteOptions, cd: usize, cb: usize) {
    let id = (!o.is_null()).then(|| unsafe { text((*o).InviteId) }).flatten().unwrap_or_default();
    let mut h = H.lock().unwrap();
    let before = h.invites.len();
    h.invites.retain(|(i, _, _)| *i != id);
    let r = if h.invites.len() < before { SUCCESS } else { NOT_FOUND };
    drop(h);
    done(cb, cd, r);
}

unsafe extern "system" fn query_invites(_h: usize, o: *const abi::PtrOptions, cd: usize, cb: usize) {
    let local = if o.is_null() { 0 } else { unsafe { (*o).Value } };
    later(move || {
        let info = abi::CompletionId {
            ResultCode: SUCCESS,
            ClientData: cd,
            Id: local,
        };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn invite_count(_h: usize, _o: usize) -> u32 {
    H.lock().unwrap().invites.len() as u32
}

unsafe extern "system" fn invite_id_by_index(
    _h: usize,
    o: *const abi::IndexOptions,
    buf: *mut c_char,
    len: *mut i32,
) -> i32 {
    if o.is_null() || len.is_null() {
        return INVALID_PARAMETERS;
    }
    let i = unsafe { (*o).Index } as usize;
    let Some(id) = H.lock().unwrap().invites.get(i).map(|(id, _, _)| id.clone()) else {
        return NOT_FOUND;
    };
    let need = id.len() as i32 + 1;
    unsafe {
        if buf.is_null() || *len < need {
            *len = need;
            return LIMIT_EXCEEDED;
        }
        std::ptr::copy_nonoverlapping(id.as_ptr(), buf as *mut u8, id.len());
        *buf.add(id.len()) = 0;
        *len = need;
    }
    SUCCESS
}

/// A LAN invite to a session: the game hears it was received, then it is accepted once
/// the player accepts it (as lobby invites are).
fn receive_invite(from: peerparty_core::PeerId, connect: &str, lobby: Lobby) {
    let (invite_id, ui_event) = {
        let mut h = H.lock().unwrap();
        h.next_invite += 1;
        let n = h.next_invite;
        let id = format!("peerparty-session-invite-{n}");
        h.ids.insert(lobby.id.clone());
        h.invites.push((id.clone(), lobby.clone(), from));
        h.ui_events.insert(n, lobby.clone());
        (id, n)
    };
    let Some(me) = lan().map(|l| l.me()) else { return };
    let (local, sender) = (ids::puid(me), ids::puid(from));
    trace("SessionInvite", || format!("{invite_id} session={} from={from}", lobby.id));
    let decision = crate::invite::decision(from, connect, Some(&lobby));
    for (f, cd) in listeners(Kind::SessionInviteReceived, None).into_iter().filter(|_| decision.received()) {
        let info = InviteReceivedInfo {
            ClientData: cd,
            LocalUserId: local,
            TargetUserId: sender,
            InviteId: intern(&invite_id),
        };
        unsafe { call(f, &info) };
    }
    if !decision.accept() {
        return;
    }
    let accepted = listeners(Kind::SessionInviteAccepted, None);
    if !accepted.is_empty() {
        for (f, cd) in accepted {
            let info = InviteAcceptedInfo {
                ClientData: cd,
                SessionId: intern(&lobby.id),
                LocalUserId: local,
                TargetUserId: sender,
                InviteId: intern(&invite_id),
            };
            unsafe { call(f, &info) };
        }
        return;
    }
    for (f, cd) in listeners(Kind::JoinSessionAccepted, None) {
        let info = JoinAcceptedInfo {
            ClientData: cd,
            LocalUserId: local,
            UiEventId: ui_event,
        };
        unsafe { call(f, &info) };
    }
}

fn hand_out_details(l: Lobby, out: *mut usize) -> i32 {
    let d = Box::new(l);
    let k = key(&*d);
    H.lock().unwrap().details.insert(k, d);
    unsafe { *out = k };
    SUCCESS
}

unsafe extern "system" fn copy_by_invite(_h: usize, o: *const abi::PtrOptions, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let id = unsafe { ptr_text(o) }.unwrap_or_default();
    let l = H.lock().unwrap().invites.iter().find(|(i, ..)| *i == id).map(|(_, l, _)| l.clone());
    match l {
        Some(l) => hand_out_details(l, out),
        None => NOT_FOUND,
    }
}

unsafe extern "system" fn copy_by_ui_event(_h: usize, o: *const abi::UiEventIdOptions, out: *mut usize) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let l = H.lock().unwrap().ui_events.get(&unsafe { (*o).UiEventId }).cloned();
    match l {
        Some(l) => hand_out_details(l, out),
        None => NOT_FOUND,
    }
}

unsafe extern "system" fn copy_for_presence(_h: usize, _o: usize, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let id = H.lock().unwrap().sessions.values().find(|s| s.presence).map(|s| s.id.clone());
    match id.and_then(|id| lan()?.lobby(&id)) {
        Some(l) => hand_out_details(l, out),
        None => NOT_FOUND,
    }
}

unsafe extern "system" fn is_user_in_session(_h: usize, o: *const NameAndUser) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let (name, user) = unsafe { (name_of((*o).SessionName), ids::peer_of_puid((*o).TargetUserId)) };
    let l = name.and_then(|n| session_id(&n)).and_then(|id| lan()?.lobby(&id));
    match (l, user) {
        (Some(l), Some(u)) if l.member(u).is_some() => SUCCESS,
        (None, _) => SESSION_INVALID,
        _ => NOT_FOUND,
    }
}

unsafe extern "system" fn dump_state(_h: usize, o: *const abi::PtrOptions) -> i32 {
    let name = unsafe { ptr_text(o) }.unwrap_or_default();
    match session_id(&name).and_then(|id| lan()?.lobby(&id)) {
        Some(l) => {
            crate::log!("eos-lan session {name}: {l:?}");
            SUCCESS
        }
        None => NOT_FOUND,
    }
}

// ---- search ----

unsafe extern "system" fn create_search(_h: usize, o: *const abi::U32Options, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    let mut query = Query::new(NS);
    if !o.is_null() && unsafe { (*o).Value } > 0 {
        query.max_results = unsafe { (*o).Value }.min(200);
    }
    let s = Box::new(SearchObj {
        query,
        results: Vec::new(),
    });
    let k = key(&*s);
    H.lock().unwrap().searches.insert(k, s);
    unsafe { *out = k };
    SUCCESS
}

fn with_search(h: usize, f: impl FnOnce(&mut SearchObj) -> i32) -> i32 {
    match H.lock().unwrap().searches.get_mut(&h) {
        Some(s) => f(s),
        None => INVALID_PARAMETERS,
    }
}

fn filter_key(f: &Filter) -> &str {
    match f {
        Filter::Attr { key, .. } => key,
        Filter::Bucket { .. } => "bucket",
        Filter::MinMembers(_) => "nonemptyonly",
        Filter::MinSlots(_) => "minslotsavailable",
    }
}

unsafe extern "system" fn search_set_parameter(h: usize, o: *const abi::SetParameterOptions) -> i32 {
    let Some(a) = (!o.is_null())
        .then(|| unsafe { read_attr((*o).Parameter, abi::VISIBILITY_PUBLIC) })
        .flatten()
    else {
        return INVALID_PARAMETERS;
    };
    let op = op(unsafe { (*o).ComparisonOp });
    let as_int = |v: &Value| match v {
        Value::Int(i) => *i,
        Value::Double(d) => *d as i64,
        Value::Bool(b) => *b as i64,
        _ => 0,
    };
    let filter = match a.key.to_ascii_lowercase().as_str() {
        "bucket" => Filter::Bucket {
            op,
            value: a.value.text(),
        },
        "minslotsavailable" => Filter::MinSlots(as_int(&a.value)),
        "nonemptyonly" => Filter::MinMembers(1),
        // Every session has its host in it.
        "emptyonly" => return with_search(h, |_| SUCCESS),
        _ => Filter::Attr {
            key: a.key.clone(),
            op,
            value: a.value,
        },
    };
    with_search(h, |s| {
        let k = filter_key(&filter).to_string();
        s.query.filters.retain(|f| !filter_key(f).eq_ignore_ascii_case(&k));
        s.query.filters.push(filter);
        SUCCESS
    })
}

unsafe extern "system" fn search_remove_parameter(h: usize, o: *const RemoveParameterOptions) -> i32 {
    let Some(k) = (!o.is_null()).then(|| unsafe { text((*o).Key) }).flatten() else {
        return INVALID_PARAMETERS;
    };
    with_search(h, |s| {
        s.query.filters.retain(|f| !filter_key(f).eq_ignore_ascii_case(&k));
        SUCCESS
    })
}

unsafe extern "system" fn search_set_id(h: usize, o: *const abi::PtrOptions) -> i32 {
    let id = unsafe { ptr_text(o) };
    with_search(h, |s| {
        s.query.lobby_id = id;
        SUCCESS
    })
}

unsafe extern "system" fn search_set_target(h: usize, o: *const abi::PtrOptions) -> i32 {
    let target = (!o.is_null()).then(|| ids::peer_of_puid(unsafe { (*o).Value })).flatten();
    if target.is_none() {
        return INVALID_PARAMETERS;
    }
    with_search(h, |s| {
        s.query.member = target;
        SUCCESS
    })
}

unsafe extern "system" fn search_set_max(h: usize, o: *const abi::U32Options) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let n = unsafe { (*o).Value };
    with_search(h, |s| {
        s.query.max_results = n.clamp(1, 200);
        SUCCESS
    })
}

unsafe extern "system" fn search_find(h: usize, _o: usize, cd: usize, cb: usize) {
    let query = H.lock().unwrap().searches.get(&h).map(|s| s.query.clone());
    let (Some(query), Some(lan)) = (query, lan()) else {
        return done(cb, cd, INVALID_PARAMETERS);
    };
    // A search for one session or player that finds nothing is NotFound.
    let specific = query.lobby_id.is_some() || query.member.is_some();
    let search = lan.search(query);
    H.lock().unwrap().finds.insert(search, (h, cb, cd, specific));
}

unsafe extern "system" fn search_count(h: usize, _o: usize) -> u32 {
    H.lock().unwrap().searches.get(&h).map_or(0, |s| s.results.len() as u32)
}

unsafe extern "system" fn search_copy(h: usize, o: *const abi::U32Options, out: *mut usize) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let i = unsafe { (*o).Value } as usize;
    let l = H.lock().unwrap().searches.get(&h).and_then(|s| s.results.get(i).cloned());
    match l {
        Some(l) => hand_out_details(l, out),
        None => NOT_FOUND,
    }
}

unsafe extern "system" fn search_release(h: usize) {
    H.lock().unwrap().searches.remove(&h);
}

// ---- details ----

fn with_details<R>(h: usize, missing: R, f: impl FnOnce(&Lobby) -> R) -> R {
    match H.lock().unwrap().details.get(&h) {
        Some(l) => f(l),
        None => missing,
    }
}

/// Where to reach a session's host: what it set, else its LAN IP.
fn host_address(l: &Lobby) -> String {
    // A host across the internet: the address its game set is no use here and may be
    // its own (even public) IP, which games show; it gets the stand-in instead.
    let remote = lan().is_some_and(|lan| l.owner != lan.me() && lan.peer_is_internet(l.owner));
    if let Some(a) = hidden(l, KEY_HOST).filter(|a| !(remote && peerparty_core::inet::redact::has_address(a))) {
        return a;
    }
    let Some(lan) = lan() else {
        return "127.0.0.1".into();
    };
    if l.owner == lan.me() {
        return "127.0.0.1".into();
    }
    // Its LAN address when it's on the LAN (also after moving there from a tunnel),
    // else its stand-in.
    lan.game_address(l.owner).map_or("127.0.0.1".into(), |ip| ip.to_string())
}

fn hand_out_info(l: &Lobby) -> (DetailsInfo, Settings, Vec<CString>) {
    let mut keep = Vec::new();
    let settings = Settings {
        ApiVersion: 4,
        BucketId: owned_str(&mut keep, &l.bucket),
        NumPublicConnections: l.max_members,
        bAllowJoinInProgress: hidden(l, KEY_JIP).is_none_or(|v| v != "0") as i32,
        PermissionLevel: l.permission as i32,
        bInvitesAllowed: l.allow_invites as i32,
        bSanctionsEnabled: 0,
        AllowedPlatformIds: std::ptr::null(),
        AllowedPlatformIdsCount: 0,
    };
    let taken = registered(l).len().max(1) as u32;
    let info = DetailsInfo {
        ApiVersion: 2,
        SessionId: owned_str(&mut keep, &l.id),
        HostAddress: owned_str(&mut keep, &host_address(l)),
        NumOpenPublicConnections: l.max_members.saturating_sub(taken),
        Settings: std::ptr::null(),
        OwnerUserId: ids::puid(l.owner),
        OwnerServerClientId: std::ptr::null(),
    };
    (info, settings, keep)
}

struct InfoBlock {
    info: DetailsInfo,
    settings: Settings,
}

/// A details info (with its settings) in one handed-out block; returns the info.
fn hand_out_details_info(l: &Lobby) -> usize {
    let (info, settings, keep) = hand_out_info(l);
    let p = hand_out(InfoBlock { info, settings }, keep) as *mut InfoBlock;
    unsafe {
        (*p).info.Settings = &(*p).settings;
        &(*p).info as *const DetailsInfo as usize
    }
}

unsafe extern "system" fn details_copy_info(h: usize, _o: usize, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let p = with_details(h, 0, hand_out_details_info);
    if p == 0 {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = p };
    SUCCESS
}

fn visible(l: &Lobby) -> Vec<Attr> {
    l.attrs.iter().filter(|a| !a.key.starts_with(HIDDEN)).cloned().collect()
}

/// An `EOS_SessionDetails_Attribute`, freed by `EOS_SessionDetails_Attribute_Release`.
fn hand_out_attr(a: &Attr) -> usize {
    let mut keep = Vec::new();
    let key = owned_str(&mut keep, &a.key);
    let (value, ty) = match &a.value {
        Value::Bool(b) => (abi::AttrValue { AsBool: *b as i32 }, abi::ATTR_BOOL),
        Value::Int(i) => (abi::AttrValue { AsInt64: *i }, abi::ATTR_INT64),
        Value::Double(d) => (abi::AttrValue { AsDouble: *d }, abi::ATTR_DOUBLE),
        Value::Str(s) => (
            abi::AttrValue {
                AsUtf8: owned_str(&mut keep, s),
            },
            abi::ATTR_STRING,
        ),
    };
    struct Both {
        attr: abi::Attribute,
        data: abi::AttributeData,
    }
    let p = hand_out(
        Both {
            attr: abi::Attribute {
                ApiVersion: 1,
                Data: std::ptr::null_mut(),
                // EOS_SAAT_Advertise for what searchers see.
                Visibility: a.public as i32,
            },
            data: abi::AttributeData {
                ApiVersion: 1,
                Key: key,
                Value: value,
                ValueType: ty,
            },
        },
        keep,
    ) as *mut Both;
    unsafe {
        (*p).attr.Data = &mut (*p).data;
        &(*p).attr as *const abi::Attribute as usize
    }
}

unsafe extern "system" fn details_attr_count(h: usize, _o: usize) -> u32 {
    with_details(h, 0, |l| visible(l).len() as u32)
}

unsafe extern "system" fn details_attr_by_index(h: usize, o: *const abi::U32Options, out: *mut usize) -> i32 {
    if o.is_null() || out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let i = unsafe { (*o).Value } as usize;
    match with_details(h, None, |l| visible(l).get(i).cloned()) {
        Some(a) => {
            unsafe { *out = hand_out_attr(&a) };
            SUCCESS
        }
        None => NOT_FOUND,
    }
}

unsafe extern "system" fn details_attr_by_key(h: usize, o: *const abi::KeyOptions, out: *mut usize) -> i32 {
    if o.is_null() || out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(k) = (unsafe { text((*o).Key) }) else {
        return INVALID_PARAMETERS;
    };
    match with_details(h, None, |l| visible(l).into_iter().find(|a| a.key.eq_ignore_ascii_case(&k))) {
        Some(a) => {
            unsafe { *out = hand_out_attr(&a) };
            SUCCESS
        }
        None => NOT_FOUND,
    }
}

unsafe extern "system" fn details_release(h: usize) {
    H.lock().unwrap().details.remove(&h);
}

unsafe extern "system" fn free(p: usize) {
    release(p);
}

// ---- active sessions ----

unsafe extern "system" fn copy_active(_h: usize, o: *const abi::PtrOptions, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(name) = (unsafe { ptr_text(o) }) else {
        return INVALID_PARAMETERS;
    };
    if session_id(&name).is_none() {
        return NOT_FOUND;
    }
    let a = Box::new(name);
    let k = key(&*a);
    H.lock().unwrap().actives.insert(k, a);
    unsafe { *out = k };
    SUCCESS
}

fn active(h: usize) -> Option<(String, Lobby)> {
    let name = H.lock().unwrap().actives.get(&h).map(|n| (**n).clone())?;
    let l = session_id(&name).and_then(|id| lan()?.lobby(&id))?;
    Some((name, l))
}

/// The registered players of a session as this copy knows them.
fn players_of(l: &Lobby) -> Vec<peerparty_core::PeerId> {
    H.lock().unwrap().registered.get(&l.id).cloned().unwrap_or_else(|| registered(l))
}

unsafe extern "system" fn active_copy_info(h: usize, _o: usize, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let (Some((name, l)), Some(lan)) = (active(h), lan()) else {
        return INVALID_PARAMETERS;
    };
    let (details, settings, mut keep) = hand_out_info(&l);
    struct Block {
        info: ActiveInfo,
        details: DetailsInfo,
        settings: Settings,
    }
    let info = ActiveInfo {
        ApiVersion: 1,
        SessionName: owned_str(&mut keep, &name),
        LocalUserId: ids::puid(lan.me()),
        State: state(&l),
        SessionDetails: std::ptr::null(),
    };
    let p = hand_out(
        Block {
            info,
            details,
            settings,
        },
        keep,
    ) as *mut Block;
    unsafe {
        (*p).details.Settings = &(*p).settings;
        (*p).info.SessionDetails = &(*p).details;
        *out = &(*p).info as *const ActiveInfo as usize;
    }
    SUCCESS
}

unsafe extern "system" fn active_player_count(h: usize, _o: usize) -> u32 {
    active(h).map_or(0, |(_, l)| players_of(&l).len() as u32)
}

unsafe extern "system" fn active_player_by_index(h: usize, o: *const abi::U32Options) -> usize {
    if o.is_null() {
        return 0;
    }
    let i = unsafe { (*o).Value } as usize;
    active(h)
        .and_then(|(_, l)| players_of(&l).get(i).copied())
        .map_or(0, ids::puid)
}

unsafe extern "system" fn active_release(h: usize) {
    H.lock().unwrap().actives.remove(&h);
}

// ---- notifications ----

unsafe extern "system" fn notify_invite_received(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::SessionInviteReceived, f, cd, None)
}

unsafe extern "system" fn notify_invite_accepted(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::SessionInviteAccepted, f, cd, None)
}

unsafe extern "system" fn notify_join_accepted(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::JoinSessionAccepted, f, cd, None)
}

unsafe extern "system" fn notify_never(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::Never, f, cd, None)
}

unsafe extern "system" fn remove_notify(_h: usize, id: u64) {
    remove_listener(id);
}

// ---- LAN events ----

pub fn on_event(_lan: &Lan, ev: &Event) {
    match ev {
        Event::SearchDone { search, results } => {
            let found = H.lock().unwrap().finds.remove(search);
            let Some((handle, cb, cd, specific)) = found else { return };
            {
                let mut h = H.lock().unwrap();
                for l in results {
                    h.ids.insert(l.id.clone());
                }
                if let Some(s) = h.searches.get_mut(&handle) {
                    s.results = results.clone();
                }
            }
            trace("EOS_SessionSearch_Find(done)", || format!("{} results", results.len()));
            let r = if specific && results.is_empty() { NOT_FOUND } else { SUCCESS };
            let info = abi::Completion {
                ResultCode: r,
                ClientData: cd,
            };
            unsafe { call(cb, &info) };
        }
        Event::JoinDone { lobby, result } => {
            let Some((name, presence, cb, cd)) = H.lock().unwrap().joins.remove(lobby) else {
                return;
            };
            trace("EOS_Sessions_JoinSession(done)", || format!("{name} {lobby} {result:?}"));
            let code = match result {
                JoinResult::Ok => {
                    H.lock().unwrap().sessions.insert(
                        name,
                        Session {
                            id: lobby.clone(),
                            presence,
                        },
                    );
                    SUCCESS
                }
                JoinResult::Full => SESSION_TOO_MANY_PLAYERS,
                JoinResult::NotAllowed => SESSION_NOT_ALLOWED,
                JoinResult::NotFound | JoinResult::TimedOut => SESSION_INVALID,
            };
            // As with lobbies: a LAN join beats any game's "joining" screen otherwise.
            later_after(super::lobby::JOIN_LATENCY, move || {
                let info = abi::Completion {
                    ResultCode: code,
                    ClientData: cd,
                };
                unsafe { call(cb, &info) };
            });
        }
        Event::MemberStatus {
            lobby,
            status: peerparty_core::MemberStatus::Closed,
            ..
        } => {
            // The host ended it: the session is gone here too.
            H.lock().unwrap().sessions.retain(|_, s| s.id != *lobby);
        }
        Event::Invite {
            from,
            lobby: Some(lobby),
            connect,
        } if lobby.namespace == NS => receive_invite(*from, connect, lobby.clone()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structs_match_the_sdk() {
        let p = size_of::<usize>();
        // ApiVersion, then pointer-sized fields.
        assert_eq!(std::mem::offset_of!(DetailsInfo, SessionId), p);
        assert_eq!(std::mem::offset_of!(ActiveInfo, SessionDetails), 4 * p);
        assert_eq!(std::mem::offset_of!(JoinAcceptedInfo, UiEventId), (2 * p).max(8));
        assert_eq!(std::mem::offset_of!(UpdateInfo, SessionId), 3 * p);
    }

    #[test]
    fn registered_players_round_trip() {
        let mut l = Lobby::new(NS);
        let a = peerparty_core::PeerId::random();
        let b = peerparty_core::PeerId::random();
        set_registered(&mut l, &[a, b]);
        assert_eq!(registered(&l), vec![a, b]);
        assert!(visible(&l).is_empty(), "hidden from the game");
        assert_eq!(state(&l), STATE_PENDING);
    }
}
