//! EOS Lobby, LobbySearch, LobbyDetails and LobbyModification on the LAN core's lobbies
//! (namespace "eos.lobby").
//!
//! Handles are PeerParty objects: a details handle is a snapshot of one lobby, a search
//! handle holds a query and its results, a modification handle is an edited copy of a
//! lobby plus the caller's member changes, applied by UpdateLobby.

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::{LazyLock, Mutex};

use peerparty_core::{
    Attr, Event, Filter, JoinResult, Lan, Lobby, LobbyError, MemberStatus, Op, Permission, Query,
    Value,
};

use super::abi::{self, result::*};
use super::ids::{self, intern, text};
use super::{
    Kind, add_listener, call, hand_out, lan, later, later_after, listeners, owned_str, release,
    remove_listener, trace,
};

const NS: &str = "eos.lobby";

/// How long a join takes to report back. A LAN join finishes in a few milliseconds; games
/// tuned to Epic's servers can show a "connecting" screen and then check whether it has
/// finished fading in before they hide it; such a screen sticks when the whole join beats
/// the fade.
pub(super) const JOIN_LATENCY: std::time::Duration = std::time::Duration::from_millis(1000);

struct SearchObj {
    query: Query,
    results: Vec<Lobby>,
    searching: bool,
}

struct ModObj {
    lobby_id: String,
    edited: Lobby,
    lobby_changed: bool,
    member_set: Vec<Attr>,
    member_remove: Vec<String>,
}

#[derive(Default)]
struct Handles {
    details: HashMap<usize, Box<Lobby>>,
    searches: HashMap<usize, Box<SearchObj>>,
    mods: HashMap<usize, Box<ModObj>>,
    /// Lan search id -> (search handle, callback, client data).
    finds: HashMap<u64, (usize, usize, usize)>,
    /// Lobby id -> (callback, client data) for a join in flight.
    joins: HashMap<String, (usize, usize)>,
    /// Invites received: (invite id, lobby, sender), oldest first.
    invites: Vec<(String, Lobby, peerparty_core::PeerId)>,
    /// UI event id -> lobby, for `JoinLobbyAccepted` auto-accepts.
    ui_events: HashMap<u64, Lobby>,
    next_invite: u64,
}

static H: LazyLock<Mutex<Handles>> = LazyLock::new(Mutex::default);

fn key<T>(b: &T) -> usize {
    b as *const T as usize
}

pub fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "EOS_Lobby_CreateLobby" => create_lobby as *const () as usize,
        "EOS_Lobby_DestroyLobby" => destroy_lobby as *const () as usize,
        "EOS_Lobby_JoinLobby" => join_lobby as *const () as usize,
        "EOS_Lobby_JoinLobbyById" => join_lobby_by_id as *const () as usize,
        "EOS_Lobby_LeaveLobby" => leave_lobby as *const () as usize,
        "EOS_Lobby_UpdateLobbyModification" => update_lobby_modification as *const () as usize,
        "EOS_Lobby_UpdateLobby" => update_lobby as *const () as usize,
        "EOS_Lobby_PromoteMember" => promote_member as *const () as usize,
        "EOS_Lobby_KickMember" => kick_member as *const () as usize,
        "EOS_Lobby_CreateLobbySearch" => create_search as *const () as usize,
        "EOS_Lobby_CopyLobbyDetailsHandle" => copy_details_handle as *const () as usize,
        "EOS_Lobby_QueryInvites" => query_invites as *const () as usize,
        "EOS_Lobby_Attribute_Release" => free as *const () as usize,
        "EOS_Lobby_AddNotifyLobbyUpdateReceived" => notify_update as *const () as usize,
        "EOS_Lobby_AddNotifyLobbyMemberUpdateReceived" => {
            notify_member_update as *const () as usize
        }
        "EOS_Lobby_AddNotifyLobbyMemberStatusReceived" => {
            notify_member_status as *const () as usize
        }
        "EOS_Lobby_AddNotifyLobbyInviteReceived" => notify_invite_received as *const () as usize,
        "EOS_Lobby_AddNotifyLobbyInviteAccepted" => notify_invite_accepted as *const () as usize,
        "EOS_Lobby_AddNotifyJoinLobbyAccepted" => notify_join_accepted as *const () as usize,
        "EOS_Lobby_SendInvite" => send_invite as *const () as usize,
        "EOS_Lobby_RejectInvite" => reject_invite as *const () as usize,
        "EOS_Lobby_GetInviteCount" => invite_count as *const () as usize,
        "EOS_Lobby_GetInviteIdByIndex" => invite_id_by_index as *const () as usize,
        "EOS_Lobby_CopyLobbyDetailsHandleByInviteId" => copy_by_invite as *const () as usize,
        "EOS_Lobby_CopyLobbyDetailsHandleByUiEventId" => copy_by_ui_event as *const () as usize,
        "EOS_Lobby_AddNotifyLobbyInviteRejected"
        | "EOS_Lobby_AddNotifySendLobbyNativeInviteRequested"
        | "EOS_Lobby_AddNotifyLeaveLobbyRequested" => notify_never as *const () as usize,
        n if n.starts_with("EOS_Lobby_RemoveNotify") => remove_notify as *const () as usize,

        "EOS_LobbyModification_SetBucketId" => mod_set_bucket as *const () as usize,
        "EOS_LobbyModification_SetPermissionLevel" => mod_set_permission as *const () as usize,
        "EOS_LobbyModification_SetMaxMembers" => mod_set_max_members as *const () as usize,
        "EOS_LobbyModification_SetInvitesAllowed" => mod_set_invites as *const () as usize,
        "EOS_LobbyModification_SetAllowedPlatformIds" => mod_ignore as *const () as usize,
        "EOS_LobbyModification_AddAttribute" => mod_add_attribute as *const () as usize,
        "EOS_LobbyModification_RemoveAttribute" => mod_remove_attribute as *const () as usize,
        "EOS_LobbyModification_AddMemberAttribute" => {
            mod_add_member_attribute as *const () as usize
        }
        "EOS_LobbyModification_RemoveMemberAttribute" => {
            mod_remove_member_attribute as *const () as usize
        }
        "EOS_LobbyModification_Release" => mod_release as *const () as usize,

        "EOS_LobbySearch_Find" => search_find as *const () as usize,
        "EOS_LobbySearch_SetLobbyId" => search_set_lobby_id as *const () as usize,
        "EOS_LobbySearch_SetTargetUserId" => search_set_target as *const () as usize,
        "EOS_LobbySearch_SetParameter" => search_set_parameter as *const () as usize,
        "EOS_LobbySearch_RemoveParameter" => search_remove_parameter as *const () as usize,
        "EOS_LobbySearch_SetMaxResults" => search_set_max as *const () as usize,
        "EOS_LobbySearch_GetSearchResultCount" => search_count as *const () as usize,
        "EOS_LobbySearch_CopySearchResultByIndex" => search_copy_result as *const () as usize,
        "EOS_LobbySearch_Release" => search_release as *const () as usize,

        "EOS_LobbyDetails_GetLobbyOwner" => details_owner as *const () as usize,
        "EOS_LobbyDetails_CopyInfo" => details_copy_info as *const () as usize,
        "EOS_LobbyDetails_GetAttributeCount" => details_attr_count as *const () as usize,
        "EOS_LobbyDetails_CopyAttributeByIndex" => details_attr_by_index as *const () as usize,
        "EOS_LobbyDetails_CopyAttributeByKey" => details_attr_by_key as *const () as usize,
        "EOS_LobbyDetails_GetMemberCount" => details_member_count as *const () as usize,
        "EOS_LobbyDetails_GetMemberByIndex" => details_member_by_index as *const () as usize,
        "EOS_LobbyDetails_GetMemberAttributeCount" => {
            details_member_attr_count as *const () as usize
        }
        "EOS_LobbyDetails_CopyMemberAttributeByIndex" => {
            details_member_attr_by_index as *const () as usize
        }
        "EOS_LobbyDetails_CopyMemberAttributeByKey" => {
            details_member_attr_by_key as *const () as usize
        }
        "EOS_LobbyDetails_CopyMemberInfo" => details_member_info as *const () as usize,
        "EOS_LobbyDetails_Release" => details_release as *const () as usize,
        "EOS_LobbyDetails_Info_Release" => free as *const () as usize,
        "EOS_LobbyDetails_MemberInfo_Release" => free as *const () as usize,
        _ => return None,
    })
}

// ---- attributes between EOS and the core ----

pub(super) unsafe fn read_attr(data: *const abi::AttributeData, visibility: i32) -> Option<Attr> {
    if data.is_null() {
        return None;
    }
    let d = unsafe { &*data };
    let key = unsafe { text(d.Key) }.filter(|k| !k.is_empty())?;
    let value = unsafe {
        match d.ValueType {
            abi::ATTR_BOOL => Value::Bool(d.Value.AsBool != 0),
            abi::ATTR_INT64 => Value::Int(d.Value.AsInt64),
            abi::ATTR_DOUBLE => Value::Double(d.Value.AsDouble),
            abi::ATTR_STRING => Value::Str(text(d.Value.AsUtf8).unwrap_or_default()),
            _ => return None,
        }
    };
    Some(Attr {
        key,
        value,
        public: visibility == abi::VISIBILITY_PUBLIC,
    })
}

/// An `EOS_Lobby_Attribute` for the game, freed by `EOS_Lobby_Attribute_Release`.
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
    // The data lives inside the same handed-out allocation as the attribute.
    struct Both {
        attr: abi::Attribute,
        data: abi::AttributeData,
    }
    let both = hand_out(
        Both {
            attr: abi::Attribute {
                ApiVersion: abi::LOBBY_ATTRIBUTE_API,
                Data: std::ptr::null_mut(),
                Visibility: if a.public { 0 } else { 1 },
            },
            data: abi::AttributeData {
                ApiVersion: abi::LOBBY_ATTRIBUTEDATA_API,
                Key: key,
                Value: value,
                ValueType: ty,
            },
        },
        keep,
    );
    let both = both as *mut Both;
    unsafe {
        (*both).attr.Data = &mut (*both).data;
        &(*both).attr as *const abi::Attribute as usize
    }
}

pub(super) fn op(code: i32) -> Op {
    match code {
        1 => Op::Ne,
        2 => Op::Gt,
        3 => Op::Ge,
        4 => Op::Lt,
        5 => Op::Le,
        6 => Op::Distance,
        7 | 9 => Op::AnyOf,
        8 | 10 => Op::NotAnyOf,
        11 => Op::Contains,
        _ => Op::Eq,
    }
}

fn join_result(r: JoinResult) -> i32 {
    match r {
        JoinResult::Ok => SUCCESS,
        JoinResult::Full => LOBBY_TOO_MANY_PLAYERS,
        JoinResult::NotFound => NOT_FOUND,
        JoinResult::NotAllowed => LOBBY_NOT_ALLOWED,
        JoinResult::TimedOut => TIMED_OUT,
    }
}

fn lobby_error(e: LobbyError) -> i32 {
    match e {
        LobbyError::NotFound | LobbyError::NotMember => NOT_FOUND,
        LobbyError::AlreadyIn => LOBBY_PRESENCE_LOBBY_EXISTS,
        LobbyError::OwnerUnknown => NOT_FOUND,
        LobbyError::NotOwner => LOBBY_NOT_OWNER,
    }
}

/// Completes `{ResultCode, ClientData, LobbyId}` on the next tick.
fn complete(cb: usize, cd: usize, result: i32, lobby_id: &str) {
    let id = intern(lobby_id) as usize;
    later(move || {
        let info = abi::CompletionId {
            ResultCode: result,
            ClientData: cd,
            Id: id,
        };
        unsafe { call(cb, &info) };
    });
}

// ---- lifecycle ----

unsafe extern "system" fn create_lobby(
    _h: usize,
    o: *const abi::CreateLobbyOptions,
    cd: usize,
    cb: usize,
) {
    let (Some(lan), false) = (lan(), o.is_null()) else {
        return complete(cb, cd, NO_CONNECTION, "");
    };
    let o = unsafe { &*o };
    let v = o.ApiVersion;
    let mut spec = Lobby::new(NS);
    spec.max_members = o.MaxLobbyMembers.max(1);
    spec.permission = Permission::from_u8(o.PermissionLevel as u8);
    spec.allow_invites = v < abi::CREATELOBBY_ALLOW_INVITES || o.bAllowInvites != 0;
    if v >= abi::CREATELOBBY_BUCKET {
        spec.bucket = unsafe { text(o.BucketId) }.unwrap_or_default();
    }
    spec.host_migration = v < abi::CREATELOBBY_DISABLE_MIGRATION || o.bDisableHostMigration == 0;
    if v >= abi::CREATELOBBY_LOBBY_ID {
        spec.id = unsafe { text(o.LobbyId) }.unwrap_or_default();
    }
    spec.join_by_id = v >= abi::CREATELOBBY_JOIN_BY_ID && o.bEnableJoinById != 0;
    // bEnableRTCRoom and LocalRTCOptions follow bDisableHostMigration (eos_lobby_types.h).
    if v >= abi::CREATELOBBY_DISABLE_MIGRATION && o.bEnableRTCRoom != 0 {
        super::rtc::lobby_voice(o.LocalRTCOptions);
    }
    match lan.create_lobby(spec) {
        Ok(l) => {
            trace("EOS_Lobby_CreateLobby", || {
                format!(
                    "v={v} id={} bucket={} max={}",
                    l.id, l.bucket, l.max_members
                )
            });
            complete(cb, cd, SUCCESS, &l.id)
        }
        Err(e) => complete(cb, cd, lobby_error(e), ""),
    }
}

unsafe extern "system" fn destroy_lobby(
    _h: usize,
    o: *const abi::LobbyIdOptions,
    cd: usize,
    cb: usize,
) {
    let id = (!o.is_null())
        .then(|| unsafe { text((*o).LobbyId) })
        .flatten()
        .unwrap_or_default();
    let r = lan().map_or(Err(LobbyError::NotFound), |l| l.destroy(&id));
    complete(cb, cd, r.map_or_else(lobby_error, |_| SUCCESS), &id);
}

unsafe extern "system" fn leave_lobby(
    _h: usize,
    o: *const abi::LobbyIdOptions,
    cd: usize,
    cb: usize,
) {
    let id = (!o.is_null())
        .then(|| unsafe { text((*o).LobbyId) })
        .flatten()
        .unwrap_or_default();
    let r = lan().map_or(Err(LobbyError::NotFound), |l| l.leave(&id));
    trace("EOS_Lobby_LeaveLobby", || format!("{id} -> {r:?}"));
    complete(cb, cd, r.map_or_else(lobby_error, |_| SUCCESS), &id);
}

unsafe extern "system" fn join_lobby(
    _h: usize,
    o: *const abi::JoinLobbyOptions,
    cd: usize,
    cb: usize,
) {
    let target = (!o.is_null())
        .then(|| {
            H.lock()
                .unwrap()
                .details
                .get(&unsafe { (*o).LobbyDetailsHandle })
                .map(|l| (**l).clone())
        })
        .flatten();
    let (Some(lan), Some(target)) = (lan(), target) else {
        return complete(cb, cd, INVALID_PARAMETERS, "");
    };
    trace("EOS_Lobby_JoinLobby", || {
        format!("{} owner={}", target.id, target.owner)
    });
    match lan.join(&target) {
        Ok(()) => {
            H.lock().unwrap().joins.insert(target.id, (cb, cd));
        }
        Err(e) => complete(cb, cd, lobby_error(e), &target.id),
    }
}

unsafe extern "system" fn join_lobby_by_id(
    _h: usize,
    o: *const abi::JoinLobbyByIdOptions,
    cd: usize,
    cb: usize,
) {
    let id = (!o.is_null())
        .then(|| unsafe { text((*o).LobbyId) })
        .flatten();
    let (Some(lan), Some(id)) = (lan(), id) else {
        return complete(cb, cd, INVALID_PARAMETERS, "");
    };
    match lan.join_by_id(NS, &id) {
        Ok(()) => {
            H.lock().unwrap().joins.insert(id, (cb, cd));
        }
        Err(e) => complete(cb, cd, lobby_error(e), &id),
    }
}

unsafe fn member_action(
    o: *const abi::MemberActionOptions,
    cd: usize,
    cb: usize,
    act: fn(&Lan, &str, peerparty_core::PeerId) -> Result<(), LobbyError>,
) {
    let (id, target) = if o.is_null() {
        (String::new(), None)
    } else {
        unsafe {
            (
                text((*o).LobbyId).unwrap_or_default(),
                ids::peer_of_puid((*o).TargetUserId),
            )
        }
    };
    let r = match (lan(), target) {
        (Some(l), Some(t)) => act(l, &id, t),
        _ => Err(LobbyError::NotMember),
    };
    complete(cb, cd, r.map_or_else(lobby_error, |_| SUCCESS), &id);
}

unsafe extern "system" fn promote_member(
    _h: usize,
    o: *const abi::MemberActionOptions,
    cd: usize,
    cb: usize,
) {
    unsafe { member_action(o, cd, cb, |l, id, t| l.promote(id, t)) }
}

unsafe extern "system" fn kick_member(
    _h: usize,
    o: *const abi::MemberActionOptions,
    cd: usize,
    cb: usize,
) {
    unsafe { member_action(o, cd, cb, |l, id, t| l.kick(id, t)) }
}

/// No invites on the LAN; the query succeeds with none.
unsafe extern "system" fn query_invites(
    _h: usize,
    o: *const abi::LocalUserOptions,
    cd: usize,
    cb: usize,
) {
    let local = if o.is_null() {
        0
    } else {
        unsafe { (*o).LocalUserId }
    };
    later(move || {
        let info = abi::CompletionId {
            ResultCode: SUCCESS,
            ClientData: cd,
            Id: local,
        };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn free(p: usize) {
    release(p);
}

// ---- notifications ----

unsafe extern "system" fn notify_update(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::LobbyUpdate, f, cd, None)
}

unsafe extern "system" fn notify_member_update(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::LobbyMemberUpdate, f, cd, None)
}

unsafe extern "system" fn notify_member_status(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::LobbyMemberStatus, f, cd, None)
}

unsafe extern "system" fn notify_never(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::Never, f, cd, None)
}

unsafe extern "system" fn remove_notify(_h: usize, id: u64) {
    remove_listener(id);
}

/// Lobby events for another front end's lobbies (Steam's are "steam:<id>").
fn not_ours(ev: &Event) -> bool {
    match ev {
        Event::JoinDone { lobby, .. }
        | Event::LobbyUpdated { lobby }
        | Event::MemberUpdated { lobby, .. }
        | Event::MemberStatus { lobby, .. } => {
            lobby.contains(':') || super::sessions::is_session(lobby)
        }
        _ => false,
    }
}

pub fn on_event(_lan: &Lan, ev: &Event) {
    if not_ours(ev) {
        return;
    }
    match ev {
        Event::SearchDone { search, results } => {
            let done = {
                let mut h = H.lock().unwrap();
                let Some((handle, cb, cd)) = h.finds.remove(search) else {
                    return;
                };
                if let Some(s) = h.searches.get_mut(&handle) {
                    s.results = results.clone();
                    s.searching = false;
                }
                (cb, cd)
            };
            trace("EOS_LobbySearch_Find(done)", || {
                format!("{} results", results.len())
            });
            let info = abi::Completion {
                ResultCode: SUCCESS,
                ClientData: done.1,
            };
            unsafe { call(done.0, &info) };
        }
        Event::JoinDone { lobby, result } => {
            let Some((cb, cd)) = H.lock().unwrap().joins.remove(lobby) else {
                return;
            };
            trace("EOS_Lobby_JoinLobby(done)", || {
                format!("{lobby} {result:?}")
            });
            let (code, id) = (join_result(*result), intern(lobby) as usize);
            later_after(JOIN_LATENCY, move || {
                let info = abi::CompletionId {
                    ResultCode: code,
                    ClientData: cd,
                    Id: id,
                };
                unsafe { call(cb, &info) };
            });
        }
        Event::LobbyUpdated { lobby } => {
            for (f, cd) in listeners(Kind::LobbyUpdate, None) {
                let info = abi::LobbyUpdateInfo {
                    ClientData: cd,
                    LobbyId: intern(lobby),
                };
                unsafe { call(f, &info) };
            }
        }
        Event::MemberUpdated { lobby, member } => {
            for (f, cd) in listeners(Kind::LobbyMemberUpdate, None) {
                let info = abi::LobbyMemberUpdateInfo {
                    ClientData: cd,
                    LobbyId: intern(lobby),
                    TargetUserId: ids::puid(*member),
                };
                unsafe { call(f, &info) };
            }
        }
        Event::MemberStatus {
            lobby,
            member,
            status,
        } => {
            let code = match status {
                MemberStatus::Joined => 0,
                MemberStatus::Left => 1,
                MemberStatus::Disconnected => 2,
                MemberStatus::Kicked => 3,
                MemberStatus::Promoted => 4,
                MemberStatus::Closed => 5,
            };
            trace("LobbyMemberStatus", || {
                format!("{lobby} {member} {status:?}")
            });
            for (f, cd) in listeners(Kind::LobbyMemberStatus, None) {
                let info = abi::LobbyMemberStatusInfo {
                    ClientData: cd,
                    LobbyId: intern(lobby),
                    TargetUserId: ids::puid(*member),
                    CurrentStatus: code,
                };
                unsafe { call(f, &info) };
            }
        }
        Event::Invite {
            from,
            lobby: Some(lobby),
            connect,
        } if lobby.namespace == NS => receive_invite(*from, connect, lobby.clone()),
        _ => {}
    }
}

// ---- invites ----

/// A LAN invite reached us through EOS. The game hears it was received; once the player
/// accepts it (at once with auto-accept on; see invite.rs) it is
/// accepted through whichever event the game listens for: an accepted invite, else a
/// join accepted from the overlay.
fn receive_invite(from: peerparty_core::PeerId, connect: &str, lobby: Lobby) {
    let (invite_id, ui_event) = {
        let mut h = H.lock().unwrap();
        h.next_invite += 1;
        let n = h.next_invite;
        let id = format!("peerparty-invite-{n}");
        h.invites.push((id.clone(), lobby.clone(), from));
        h.ui_events.insert(n, lobby.clone());
        (id, n)
    };
    let Some(me) = lan().map(|l| l.me()) else {
        return;
    };
    let (local, sender) = (ids::puid(me), ids::puid(from));
    trace("LobbyInvite", || {
        format!("{invite_id} lobby={} from={from}", lobby.id)
    });
    let decision = crate::invite::decision(from, connect, Some(&lobby));
    for (f, cd) in listeners(Kind::LobbyInviteReceived, None).into_iter().filter(|_| decision.received()) {
        let info = abi::LobbyInviteReceivedInfo {
            ClientData: cd,
            InviteId: intern(&invite_id),
            LocalUserId: local,
            TargetUserId: sender,
        };
        unsafe { call(f, &info) };
    }
    if !decision.accept() {
        return;
    }
    let accepted = listeners(Kind::LobbyInviteAccepted, None);
    if !accepted.is_empty() {
        for (f, cd) in accepted {
            let info = abi::LobbyInviteAcceptedInfo {
                ClientData: cd,
                InviteId: intern(&invite_id),
                LocalUserId: local,
                TargetUserId: sender,
                LobbyId: intern(&lobby.id),
            };
            unsafe { call(f, &info) };
        }
        return;
    }
    for (f, cd) in listeners(Kind::JoinLobbyAccepted, None) {
        let info = abi::JoinLobbyAcceptedInfo {
            ClientData: cd,
            LocalUserId: local,
            UiEventId: ui_event,
        };
        unsafe { call(f, &info) };
    }
}

unsafe extern "system" fn notify_invite_received(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::LobbyInviteReceived, f, cd, None)
}

unsafe extern "system" fn notify_invite_accepted(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::LobbyInviteAccepted, f, cd, None)
}

unsafe extern "system" fn notify_join_accepted(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::JoinLobbyAccepted, f, cd, None)
}

unsafe extern "system" fn send_invite(
    _h: usize,
    o: *const abi::SendInviteOptions,
    cd: usize,
    cb: usize,
) {
    let (id, target) = if o.is_null() {
        (String::new(), None)
    } else {
        unsafe {
            (
                text((*o).LobbyId).unwrap_or_default(),
                ids::peer_of_puid((*o).TargetUserId),
            )
        }
    };
    let lobby = lan().and_then(|l| l.lobby(&id));
    let r = match (lan(), lobby, target) {
        (Some(l), Some(lobby), Some(t)) => {
            let connect = crate::steamlan::my_connect();
            if l.invite(Some(t), &connect, Some(&lobby)) > 0 {
                SUCCESS
            } else {
                NOT_FOUND
            }
        }
        (_, None, _) => NOT_FOUND,
        _ => INVALID_USER,
    };
    trace("EOS_Lobby_SendInvite", || format!("{id} -> {target:?} {r}"));
    complete(cb, cd, r, &id);
}

unsafe extern "system" fn reject_invite(
    _h: usize,
    o: *const abi::RejectInviteOptions,
    cd: usize,
    cb: usize,
) {
    let id = (!o.is_null())
        .then(|| unsafe { text((*o).InviteId) })
        .flatten()
        .unwrap_or_default();
    let mut h = H.lock().unwrap();
    let before = h.invites.len();
    h.invites.retain(|(i, _, _)| *i != id);
    let r = if h.invites.len() < before {
        SUCCESS
    } else {
        NOT_FOUND
    };
    drop(h);
    complete(cb, cd, r, &id);
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
    let Some(id) = H
        .lock()
        .unwrap()
        .invites
        .get(i)
        .map(|(id, _, _)| id.clone())
    else {
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

fn hand_out_details(lobby: Lobby, out: *mut usize) -> i32 {
    let d = Box::new(lobby);
    let k = key(&*d);
    H.lock().unwrap().details.insert(k, d);
    unsafe { *out = k };
    SUCCESS
}

unsafe extern "system" fn copy_by_invite(
    _h: usize,
    o: *const abi::PtrOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let id = unsafe { text((*o).Value as *const c_char) }.unwrap_or_default();
    let lobby = H
        .lock()
        .unwrap()
        .invites
        .iter()
        .find(|(i, _, _)| *i == id)
        .map(|(_, l, _)| l.clone());
    match lobby {
        Some(l) => hand_out_details(l, out),
        None => NOT_FOUND,
    }
}

unsafe extern "system" fn copy_by_ui_event(
    _h: usize,
    o: *const abi::UiEventIdOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let ev = unsafe { (*o).UiEventId };
    let lobby = H.lock().unwrap().ui_events.get(&ev).cloned();
    match lobby {
        Some(l) => hand_out_details(l, out),
        None => NOT_FOUND,
    }
}

// ---- modification ----

unsafe extern "system" fn update_lobby_modification(
    _h: usize,
    o: *const abi::LobbyIdOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let id = unsafe { text((*o).LobbyId) }.unwrap_or_default();
    let Some(lobby) = lan().and_then(|l| l.lobby(&id)) else {
        return NOT_FOUND;
    };
    let m = Box::new(ModObj {
        lobby_id: id,
        edited: lobby,
        lobby_changed: false,
        member_set: Vec::new(),
        member_remove: Vec::new(),
    });
    let k = key(&*m);
    H.lock().unwrap().mods.insert(k, m);
    unsafe { *out = k };
    SUCCESS
}

fn with_mod(handle: usize, f: impl FnOnce(&mut ModObj) -> i32) -> i32 {
    match H.lock().unwrap().mods.get_mut(&handle) {
        Some(m) => f(m),
        None => INVALID_PARAMETERS,
    }
}

unsafe extern "system" fn mod_set_bucket(h: usize, o: *const abi::PtrOptions) -> i32 {
    let Some(b) = (!o.is_null())
        .then(|| unsafe { text((*o).Value as *const c_char) })
        .flatten()
    else {
        return INVALID_PARAMETERS;
    };
    with_mod(h, |m| {
        m.edited.bucket = b;
        m.lobby_changed = true;
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
        m.lobby_changed = true;
        SUCCESS
    })
}

unsafe extern "system" fn mod_set_max_members(h: usize, o: *const abi::U32Options) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let n = unsafe { (*o).Value };
    with_mod(h, |m| {
        m.edited.max_members = n.max(1);
        m.lobby_changed = true;
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
        m.lobby_changed = true;
        SUCCESS
    })
}

unsafe extern "system" fn mod_ignore(h: usize, _o: usize) -> i32 {
    with_mod(h, |_| SUCCESS)
}

unsafe extern "system" fn mod_add_attribute(h: usize, o: *const abi::AddAttributeOptions) -> i32 {
    let Some(a) = (!o.is_null())
        .then(|| unsafe { read_attr((*o).Attribute, (*o).Visibility) })
        .flatten()
    else {
        return INVALID_PARAMETERS;
    };
    crate::log!(
        "eos-lan lobby attribute {} = {:?} ({})",
        a.key,
        a.value,
        if a.public { "public" } else { "private" }
    );
    with_mod(h, |m| {
        peerparty_core::set_attr(&mut m.edited.attrs, a);
        m.lobby_changed = true;
        SUCCESS
    })
}

unsafe extern "system" fn mod_remove_attribute(h: usize, o: *const abi::KeyOptions) -> i32 {
    let Some(k) = (!o.is_null()).then(|| unsafe { text((*o).Key) }).flatten() else {
        return INVALID_PARAMETERS;
    };
    with_mod(h, |m| {
        peerparty_core::remove_attr(&mut m.edited.attrs, &k);
        m.lobby_changed = true;
        SUCCESS
    })
}

unsafe extern "system" fn mod_add_member_attribute(
    h: usize,
    o: *const abi::AddAttributeOptions,
) -> i32 {
    let Some(a) = (!o.is_null())
        .then(|| unsafe { read_attr((*o).Attribute, (*o).Visibility) })
        .flatten()
    else {
        return INVALID_PARAMETERS;
    };
    with_mod(h, |m| {
        m.member_remove.retain(|k| !k.eq_ignore_ascii_case(&a.key));
        peerparty_core::set_attr(&mut m.member_set, a);
        SUCCESS
    })
}

unsafe extern "system" fn mod_remove_member_attribute(h: usize, o: *const abi::KeyOptions) -> i32 {
    let Some(k) = (!o.is_null()).then(|| unsafe { text((*o).Key) }).flatten() else {
        return INVALID_PARAMETERS;
    };
    with_mod(h, |m| {
        peerparty_core::remove_attr(&mut m.member_set, &k);
        m.member_remove.push(k);
        SUCCESS
    })
}

unsafe extern "system" fn mod_release(h: usize) {
    H.lock().unwrap().mods.remove(&h);
}

unsafe extern "system" fn update_lobby(
    _h: usize,
    o: *const abi::UpdateLobbyOptions,
    cd: usize,
    cb: usize,
) {
    let m = (!o.is_null())
        .then(|| {
            let h = H.lock().unwrap();
            h.mods
                .get(&unsafe { (*o).LobbyModificationHandle })
                .map(|m| {
                    (
                        m.lobby_id.clone(),
                        m.lobby_changed.then(|| m.edited.clone()),
                        m.member_set.clone(),
                        m.member_remove.clone(),
                    )
                })
        })
        .flatten();
    let Some((id, edited, set, remove)) = m else {
        return complete(cb, cd, INVALID_PARAMETERS, "");
    };
    let r = lan().map_or(Err(LobbyError::NotFound), |l| {
        l.update(&id, edited.as_ref(), &set, &remove)
    });
    trace("EOS_Lobby_UpdateLobby", || {
        format!(
            "{id} lobby={} member_set={} -> {r:?}",
            edited.is_some(),
            set.len()
        )
    });
    complete(cb, cd, r.map_or_else(lobby_error, |_| SUCCESS), &id);
}

// ---- search ----

unsafe extern "system" fn create_search(
    _h: usize,
    o: *const abi::U32Options,
    out: *mut usize,
) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    let mut query = Query::new(NS);
    if !o.is_null() {
        let max = unsafe { (*o).Value };
        if max > 0 {
            query.max_results = max.min(200);
        }
    }
    let s = Box::new(SearchObj {
        query,
        results: Vec::new(),
        searching: false,
    });
    let k = key(&*s);
    H.lock().unwrap().searches.insert(k, s);
    unsafe { *out = k };
    SUCCESS
}

fn with_search(handle: usize, f: impl FnOnce(&mut SearchObj) -> i32) -> i32 {
    match H.lock().unwrap().searches.get_mut(&handle) {
        Some(s) => f(s),
        None => INVALID_PARAMETERS,
    }
}

unsafe extern "system" fn search_set_lobby_id(h: usize, o: *const abi::PtrOptions) -> i32 {
    let id = (!o.is_null())
        .then(|| unsafe { text((*o).Value as *const c_char) })
        .flatten();
    with_search(h, |s| {
        s.query.lobby_id = id;
        SUCCESS
    })
}

unsafe extern "system" fn search_set_target(h: usize, o: *const abi::PtrOptions) -> i32 {
    let target = (!o.is_null())
        .then(|| ids::peer_of_puid(unsafe { (*o).Value }))
        .flatten();
    if target.is_none() {
        return INVALID_PARAMETERS;
    }
    with_search(h, |s| {
        s.query.member = target;
        SUCCESS
    })
}

fn filter_key(f: &Filter) -> Option<&str> {
    match f {
        Filter::Attr { key, .. } => Some(key),
        Filter::Bucket { .. } => Some("bucket"),
        Filter::MinMembers(_) => Some("mincurrentmembers"),
        Filter::MinSlots(_) => Some("minslotsavailable"),
    }
}

unsafe extern "system" fn search_set_parameter(
    h: usize,
    o: *const abi::SetParameterOptions,
) -> i32 {
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
        _ => 0,
    };
    let filter = match a.key.to_ascii_lowercase().as_str() {
        "bucket" => Filter::Bucket {
            op,
            value: a.value.text(),
        },
        "mincurrentmembers" => Filter::MinMembers(as_int(&a.value)),
        "minslotsavailable" => Filter::MinSlots(as_int(&a.value)),
        _ => Filter::Attr {
            key: a.key.clone(),
            op,
            value: a.value.clone(),
        },
    };
    with_search(h, |s| {
        let k = filter_key(&filter).map(|k| k.to_ascii_lowercase());
        s.query
            .filters
            .retain(|f| filter_key(f).map(|x| x.to_ascii_lowercase()) != k);
        s.query.filters.push(filter);
        SUCCESS
    })
}

unsafe extern "system" fn search_remove_parameter(
    h: usize,
    o: *const abi::RemoveParameterOptions,
) -> i32 {
    let Some(k) = (!o.is_null()).then(|| unsafe { text((*o).Key) }).flatten() else {
        return INVALID_PARAMETERS;
    };
    with_search(h, |s| {
        let before = s.query.filters.len();
        s.query
            .filters
            .retain(|f| !filter_key(f).is_some_and(|x| x.eq_ignore_ascii_case(&k)));
        if s.query.filters.len() == before {
            NOT_FOUND
        } else {
            SUCCESS
        }
    })
}

unsafe extern "system" fn search_set_max(h: usize, o: *const abi::U32Options) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let n = unsafe { (*o).Value };
    if n == 0 || n > 200 {
        return INVALID_PARAMETERS;
    }
    with_search(h, |s| {
        s.query.max_results = n;
        SUCCESS
    })
}

unsafe extern "system" fn search_find(h: usize, _o: usize, cd: usize, cb: usize) {
    let Some(lan) = lan() else {
        return later(move || unsafe {
            call(
                cb,
                &abi::Completion {
                    ResultCode: NO_CONNECTION,
                    ClientData: cd,
                },
            )
        });
    };
    let query = {
        let mut hs = H.lock().unwrap();
        match hs.searches.get_mut(&h) {
            Some(s) if s.searching => Err(ALREADY_PENDING),
            Some(s) => {
                // Exactly one of: a lobby id, a member, or attribute parameters.
                let modes = s.query.lobby_id.is_some() as u8
                    + s.query.member.is_some() as u8
                    + (!s.query.filters.is_empty()) as u8;
                if modes != 1 {
                    Err(INVALID_PARAMETERS)
                } else {
                    s.searching = true;
                    s.results.clear();
                    Ok(s.query.clone())
                }
            }
            None => Err(INVALID_PARAMETERS),
        }
    };
    match query {
        Ok(q) => {
            trace("EOS_LobbySearch_Find", || format!("{q:?}"));
            let id = lan.search(q);
            H.lock().unwrap().finds.insert(id, (h, cb, cd));
        }
        Err(r) => later(move || unsafe {
            call(
                cb,
                &abi::Completion {
                    ResultCode: r,
                    ClientData: cd,
                },
            )
        }),
    }
}

unsafe extern "system" fn search_count(h: usize, _o: usize) -> u32 {
    H.lock()
        .unwrap()
        .searches
        .get(&h)
        .filter(|s| !s.searching)
        .map_or(0, |s| s.results.len() as u32)
}

unsafe extern "system" fn search_copy_result(
    h: usize,
    o: *const abi::U32Options,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let i = unsafe { (*o).Value } as usize;
    let mut hs = H.lock().unwrap();
    let Some(lobby) = hs
        .searches
        .get(&h)
        .filter(|s| !s.searching)
        .and_then(|s| s.results.get(i).cloned())
    else {
        return INVALID_PARAMETERS;
    };
    let d = Box::new(lobby);
    let k = key(&*d);
    hs.details.insert(k, d);
    unsafe { *out = k };
    SUCCESS
}

unsafe extern "system" fn search_release(h: usize) {
    let mut hs = H.lock().unwrap();
    hs.searches.remove(&h);
    hs.finds.retain(|_, (handle, _, _)| *handle != h);
}

unsafe extern "system" fn copy_details_handle(
    _h: usize,
    o: *const abi::PtrOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let id = unsafe { text((*o).Value as *const c_char) }.unwrap_or_default();
    let Some(lobby) = lan().and_then(|l| l.lobby(&id)) else {
        return NOT_FOUND;
    };
    let d = Box::new(lobby);
    let k = key(&*d);
    H.lock().unwrap().details.insert(k, d);
    unsafe { *out = k };
    SUCCESS
}

// ---- details ----

fn with_details<R>(h: usize, missing: R, f: impl FnOnce(&Lobby) -> R) -> R {
    match H.lock().unwrap().details.get(&h) {
        Some(l) => f(l),
        None => missing,
    }
}

unsafe extern "system" fn details_owner(h: usize, _o: usize) -> usize {
    with_details(h, 0, |l| ids::puid(l.owner))
}

unsafe extern "system" fn details_copy_info(h: usize, _o: usize, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(l) = with_details(h, None, |l| Some(l.clone())) else {
        return INVALID_PARAMETERS;
    };
    let mut keep = Vec::new();
    let info = abi::LobbyDetailsInfo {
        ApiVersion: abi::LOBBYDETAILS_INFO_API,
        LobbyId: owned_str(&mut keep, &l.id),
        LobbyOwnerUserId: ids::puid(l.owner),
        PermissionLevel: l.permission as i32,
        AvailableSlots: l.open_slots(),
        MaxMembers: l.max_members,
        bAllowInvites: l.allow_invites as i32,
        BucketId: owned_str(&mut keep, &l.bucket),
        bAllowHostMigration: l.host_migration as i32,
        bRTCRoomEnabled: super::rtc::lobby_rooms_on() as i32,
        bAllowJoinById: l.join_by_id as i32,
        bRejoinAfterKickRequiresInvite: 0,
        bPresenceEnabled: 0,
        AllowedPlatformIds: 0,
        AllowedPlatformIdsCount: 0,
    };
    unsafe { *out = hand_out(info, keep) };
    SUCCESS
}

unsafe extern "system" fn details_attr_count(h: usize, _o: usize) -> u32 {
    with_details(h, 0, |l| l.attrs.len() as u32)
}

unsafe extern "system" fn details_attr_by_index(
    h: usize,
    o: *const abi::U32Options,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let i = unsafe { (*o).Value } as usize;
    match with_details(h, None, |l| l.attrs.get(i).cloned()) {
        Some(a) => {
            unsafe { *out = hand_out_attr(&a) };
            SUCCESS
        }
        None => INVALID_PARAMETERS,
    }
}

unsafe extern "system" fn details_attr_by_key(
    h: usize,
    o: *const abi::PtrOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let k = unsafe { text((*o).Value as *const c_char) }.unwrap_or_default();
    match with_details(h, None, |l| l.attr(&k).cloned()) {
        Some(a) => {
            unsafe { *out = hand_out_attr(&a) };
            SUCCESS
        }
        None => {
            let have = with_details(h, String::new(), |l| {
                l.attrs.iter().map(|a| a.key.as_str()).collect::<Vec<_>>().join(",")
            });
            crate::log!("eos-lan LobbyDetails attribute {k} not found (has: {have})");
            NOT_FOUND
        }
    }
}

unsafe extern "system" fn details_member_count(h: usize, _o: usize) -> u32 {
    with_details(h, 0, |l| l.members.len() as u32)
}

unsafe extern "system" fn details_member_by_index(h: usize, o: *const abi::U32Options) -> usize {
    if o.is_null() {
        return 0;
    }
    let i = unsafe { (*o).Value } as usize;
    with_details(h, 0, |l| l.members.get(i).map_or(0, |m| ids::puid(m.id)))
}

fn member_attrs(h: usize, target: usize) -> Option<Vec<Attr>> {
    let peer = ids::peer_of_puid(target)?;
    with_details(h, None, |l| l.member(peer).map(|m| m.attrs.clone()))
}

unsafe extern "system" fn details_member_attr_count(h: usize, o: *const abi::PtrOptions) -> u32 {
    if o.is_null() {
        return 0;
    }
    member_attrs(h, unsafe { (*o).Value }).map_or(0, |a| a.len() as u32)
}

unsafe extern "system" fn details_member_attr_by_index(
    h: usize,
    o: *const abi::TargetAttrIndexOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let (target, i) = unsafe { ((*o).TargetUserId, (*o).AttrIndex as usize) };
    match member_attrs(h, target).and_then(|a| a.get(i).cloned()) {
        Some(a) => {
            unsafe { *out = hand_out_attr(&a) };
            SUCCESS
        }
        None => INVALID_PARAMETERS,
    }
}

unsafe extern "system" fn details_member_attr_by_key(
    h: usize,
    o: *const abi::TargetAttrKeyOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let (target, k) = unsafe { ((*o).TargetUserId, text((*o).AttrKey).unwrap_or_default()) };
    match member_attrs(h, target).and_then(|a| peerparty_core::find_attr(&a, &k).cloned()) {
        Some(a) => {
            unsafe { *out = hand_out_attr(&a) };
            SUCCESS
        }
        None => NOT_FOUND,
    }
}

unsafe extern "system" fn details_member_info(
    h: usize,
    o: *const abi::PtrOptions,
    out: *mut usize,
) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let target = unsafe { (*o).Value };
    if member_attrs(h, target).is_none() {
        return NOT_FOUND;
    }
    let info = abi::LobbyMemberInfo {
        ApiVersion: abi::LOBBYDETAILS_MEMBERINFO_API,
        UserId: target,
        // EOS_OPT_Unknown; every LAN peer is on this platform.
        Platform: 0,
        bAllowsCrossplay: 1,
    };
    unsafe { *out = hand_out(info, Vec::new()) };
    SUCCESS
}

unsafe extern "system" fn details_release(h: usize) {
    H.lock().unwrap().details.remove(&h);
}
