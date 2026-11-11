//! ISteamMatchmaking's lobbies on the LAN core's lobbies (namespace "steam.lobby"),
//! following Valve's Steamworks documentation: what a call returns, which callbacks
//! follow it and in what order.
//!
//! A Steam lobby's SteamID (chat account, lobby instance flags) is part of its core id
//! ("steam:<SteamID>"), so every copy names a lobby the same way and a SteamID from an
//! invite or rich presence is enough to join it. Lobby data is all public, as on
//! Steam; member data only travels inside the lobby. PeerParty's own keys
//! ("PeerParty:...", the game server and lobby type) never show up in the game's
//! lobby data.
//!
//! Search filters run here on every lobby the LAN returns, with Steam's comparisons
//! (lobby value against the filter's value).

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use peerparty_core::{Attr, Event, JoinResult, Lan, Lobby, MemberStatus, Permission, Query, Value};

use crate::log;
use crate::net;
use crate::steamlan::{bytes, c_str, peer_of, publish_presence, steam_id};
use crate::steamobj::vmethod;

pub const NS: &str = "steam.lobby";
const CHAT_SOCKET: &str = "steam:chat:";
const HIDDEN: &str = "peerparty:";
const KEY_TYPE: &str = "peerparty:type";
const KEY_JOINABLE: &str = "peerparty:joinable";
const KEY_SERVER: &str = "peerparty:gameserver";

const LOBBY_KICKED: i32 = 512;
const LOBBY_ENTER: i32 = 504;
const LOBBY_DATA_UPDATE: i32 = 505;
const LOBBY_CHAT_UPDATE: i32 = 506;
const LOBBY_CHAT_MSG: i32 = 507;
const LOBBY_GAME_CREATED: i32 = 509;
const LOBBY_MATCH_LIST: i32 = 510;
const LOBBY_CREATED: i32 = 513;

// EChatRoomEnterResponse
const ENTER_SUCCESS: u32 = 1;
const ENTER_DOESNT_EXIST: u32 = 2;
const ENTER_NOT_ALLOWED: u32 = 3;
const ENTER_FULL: u32 = 4;
const ENTER_ERROR: u32 = 5;
// EChatMemberStateChange
const ENTERED: u32 = 0x01;
const LEFT: u32 = 0x02;
const DISCONNECTED: u32 = 0x04;
const KICKED: u32 = 0x08;

#[repr(C)]
struct LobbyEnterT {
    lobby: u64,
    permissions: u32,
    locked: bool,
    response: u32,
}

#[repr(C)]
struct LobbyCreatedT {
    result: i32,
    lobby: u64,
}

#[repr(C)]
struct LobbyMatchListT {
    count: u32,
}

#[repr(C)]
struct LobbyDataUpdateT {
    lobby: u64,
    member: u64,
    success: u8,
}

#[repr(C)]
struct LobbyChatUpdateT {
    lobby: u64,
    changed: u64,
    making_change: u64,
    state: u32,
}

#[repr(C)]
struct LobbyChatMsgT {
    lobby: u64,
    user: u64,
    kind: u8,
    chat_id: u32,
}

#[repr(C)]
struct LobbyGameCreatedT {
    lobby: u64,
    server: u64,
    ip: u32,
    port: u16,
}

#[repr(C)]
struct LobbyKickedT {
    lobby: u64,
    admin: u64,
    disconnected: u8,
}

enum Filter {
    Str(String, String, i32),
    Num(String, i64, i32),
}

#[derive(Default)]
struct State {
    filters: Vec<Filter>,
    near: Vec<(String, i64)>,
    slots: Option<u32>,
    max_results: Option<usize>,
    /// The lobby list search in flight: (core search, our call).
    search: Option<(u64, u64)>,
    results: Vec<u64>,
    /// Lobbies seen in searches and invites.
    known: HashMap<u64, Lobby>,
    /// core lobby id -> our JoinLobby call.
    joins: HashMap<String, u64>,
    /// core search -> lobby, for RequestLobbyData.
    data_requests: HashMap<u64, u64>,
    /// Lobby chat for the game to read: (SteamID, entry type, body) by lobby.
    chat: crate::lobbychat::Log<(u64, u8, Vec<u8>)>,
    /// Chat from players not (yet) known to be members.
    chat_inbox: crate::lobbychat::Inbox,
    /// The game server each lobby last announced, to spot a new one.
    servers: HashMap<u64, String>,
}

static S: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

fn lan() -> Option<&'static Lan> {
    net::lan()
}

// ---- ids ----

fn core_id(lobby: u64) -> String {
    format!("steam:{lobby}")
}

fn steam_lobby(core: &str) -> Option<u64> {
    core.strip_prefix("steam:")?.parse().ok()
}

fn new_lobby_id() -> u64 {
    let account = u32::from_le_bytes(peerparty_core::PeerId::random().0[..4].try_into().unwrap()) | 1;
    // Public universe, chat account, k_EChatInstanceFlagLobby | k_EChatInstanceFlagMMSLobby.
    (1u64 << 56) | (8u64 << 52) | (0x60000u64 << 32) | account as u64
}

/// Keeps a lobby from an invite or search; returns its SteamID.
pub fn remember(lobby: &Lobby) -> u64 {
    let id = steam_lobby(&lobby.id).unwrap_or(0);
    if id != 0 {
        S.lock().unwrap().known.insert(id, lobby.clone());
    }
    id
}

/// Our latest view of a lobby: as a member, else from a search or invite.
fn view(lobby: u64) -> Option<Lobby> {
    let core = core_id(lobby);
    lan()
        .and_then(|l| l.lobby(&core))
        .or_else(|| S.lock().unwrap().known.get(&lobby).cloned())
}

/// A LAN lobby by SteamID, for invites.
pub fn lobby_for(lobby: u64) -> Option<Lobby> {
    view(lobby)
}

/// The Steam lobby we're in, if any.
pub fn current_lobby() -> Option<Lobby> {
    lan()?.my_lobbies(NS).into_iter().next()
}

pub fn current_lobby_id() -> Option<u64> {
    steam_lobby(&current_lobby()?.id)
}

fn attr<'a>(l: &'a Lobby, key: &str) -> Option<&'a Attr> {
    l.attrs.iter().find(|a| a.key == key)
}

fn text(l: &Lobby, key: &str) -> Option<String> {
    attr(l, key).map(|a| a.value.text())
}

fn visible(attrs: &[Attr]) -> Vec<&Attr> {
    attrs.iter().filter(|a| !a.key.starts_with(HIDDEN)).collect()
}

fn permission(kind: i64, joinable: bool) -> Permission {
    match (kind, joinable) {
        (_, false) | (0, _) => Permission::InviteOnly,
        (1, _) => Permission::Presence,
        _ => Permission::Public,
    }
}

fn s(p: *const c_char) -> String {
    crate::read::cstr(p as usize).unwrap_or_default()
}

fn data_update(lobby: u64, member: u64, ok: bool) {
    crate::steam::inject(
        LOBBY_DATA_UPDATE,
        bytes(&LobbyDataUpdateT {
            lobby,
            member,
            success: ok as u8,
        }),
    );
}

/// Applies an owner's edit to a lobby we host.
fn edit(lobby: u64, f: impl FnOnce(&mut Lobby)) -> bool {
    let core = core_id(lobby);
    let Some(lan) = lan() else { return false };
    if !lan.is_owner(&core) {
        return false;
    }
    let Some(mut l) = lan.lobby(&core) else {
        return false;
    };
    f(&mut l);
    let joinable = text(&l, KEY_JOINABLE).is_none_or(|v| v != "0");
    let kind = text(&l, KEY_TYPE).and_then(|v| v.parse().ok()).unwrap_or(2);
    l.permission = permission(kind, joinable);
    let ok = lan.update(&core, Some(&l), &[], &[]).is_ok();
    if ok {
        data_update(lobby, lobby, true);
    }
    ok
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

// ---- search ----

fn lobby_int(l: &Lobby, key: &str) -> Option<i64> {
    text(l, key).and_then(|v| v.trim().parse().ok())
}

/// Steam's ELobbyComparison: how the lobby's value compares with the filter's.
fn compare(ord: std::cmp::Ordering, cmp: i32) -> bool {
    use std::cmp::Ordering::*;
    match cmp {
        -2 => ord != Greater,
        -1 => ord == Less,
        0 => ord == Equal,
        1 => ord == Greater,
        2 => ord != Less,
        3 => ord != Equal,
        _ => true,
    }
}

fn passes(l: &Lobby, f: &Filter) -> bool {
    match f {
        Filter::Str(k, v, cmp) => match text(l, k) {
            Some(have) => compare(have.as_str().cmp(v.as_str()), *cmp),
            None => *cmp == 3,
        },
        Filter::Num(k, v, cmp) => match lobby_int(l, k) {
            Some(have) => compare(have.cmp(v), *cmp),
            None => *cmp == 3,
        },
    }
}

fn finish_search(results: &[Lobby]) -> (u64, u32) {
    let mut st = S.lock().unwrap();
    let Some((_, call)) = st.search.take() else {
        return (0, 0);
    };
    let mut hits: Vec<Lobby> = results
        .iter()
        .filter(|l| st.filters.iter().all(|f| passes(l, f)))
        .filter(|l| st.slots.is_none_or(|n| l.open_slots() >= n))
        .cloned()
        .collect();
    for (k, target) in &st.near {
        hits.sort_by_key(|l| lobby_int(l, k).map_or(i64::MAX, |v| (v - target).abs()));
    }
    hits.truncate(st.max_results.unwrap_or(50));
    st.results = hits.iter().filter_map(|l| steam_lobby(&l.id)).collect();
    for l in hits {
        if let Some(id) = steam_lobby(&l.id) {
            st.known.insert(id, l);
        }
    }
    st.filters.clear();
    st.near.clear();
    st.slots = None;
    st.max_results = None;
    (call, st.results.len() as u32)
}

// ---- ISteamMatchmaking ----

pub fn matchmaking() -> Vec<(&'static str, usize)> {
    vec![
        ("RequestLobbyList", request_lobby_list as *const () as usize),
        ("AddRequestLobbyListStringFilter", add_string_filter as *const () as usize),
        ("AddRequestLobbyListNumericalFilter", add_numerical_filter as *const () as usize),
        ("AddRequestLobbyListNearValueFilter", add_near_filter as *const () as usize),
        ("AddRequestLobbyListFilterSlotsAvailable", add_slots_filter as *const () as usize),
        ("AddRequestLobbyListDistanceFilter", ignore_i32 as *const () as usize),
        ("AddRequestLobbyListResultCountFilter", add_count_filter as *const () as usize),
        ("AddRequestLobbyListCompatibleMembersFilter", ignore_u64 as *const () as usize),
        ("GetLobbyByIndex", get_lobby_by_index as *const () as usize),
        ("CreateLobby", create_lobby as *const () as usize),
        ("JoinLobby", join_lobby as *const () as usize),
        ("LeaveLobby", leave_lobby as *const () as usize),
        ("InviteUserToLobby", invite_user_to_lobby as *const () as usize),
        ("GetNumLobbyMembers", get_num_lobby_members as *const () as usize),
        ("GetLobbyMemberByIndex", get_lobby_member_by_index as *const () as usize),
        ("GetLobbyData", get_lobby_data as *const () as usize),
        ("SetLobbyData", set_lobby_data as *const () as usize),
        ("GetLobbyDataCount", get_lobby_data_count as *const () as usize),
        ("GetLobbyDataByIndex", get_lobby_data_by_index as *const () as usize),
        ("DeleteLobbyData", delete_lobby_data as *const () as usize),
        ("GetLobbyMemberData", get_lobby_member_data as *const () as usize),
        ("SetLobbyMemberData", set_lobby_member_data as *const () as usize),
        ("SendLobbyChatMsg", send_lobby_chat_msg as *const () as usize),
        ("GetLobbyChatEntry", get_lobby_chat_entry as *const () as usize),
        ("RequestLobbyData", request_lobby_data as *const () as usize),
        ("SetLobbyGameServer", set_lobby_game_server as *const () as usize),
        ("GetLobbyGameServer", get_lobby_game_server as *const () as usize),
        ("SetLobbyMemberLimit", set_lobby_member_limit as *const () as usize),
        ("GetLobbyMemberLimit", get_lobby_member_limit as *const () as usize),
        ("SetLobbyType", set_lobby_type as *const () as usize),
        ("SetLobbyJoinable", set_lobby_joinable as *const () as usize),
        ("GetLobbyOwner", get_lobby_owner as *const () as usize),
        ("SetLobbyOwner", set_lobby_owner as *const () as usize),
        ("SetLinkedLobby", set_linked_lobby as *const () as usize),
    ]
}

vmethod! {
    fn request_lobby_list(_this: usize) -> u64 {
        let call = crate::steam::new_call();
        let Some(lan) = lan() else { return 0 };
        let mut q = Query::new(NS);
        q.max_results = 500;
        let search = lan.search(q);
        S.lock().unwrap().search = Some((search, call));
        log!("steam-lan RequestLobbyList call={call:#x}");
        call
    }
}

vmethod! {
    fn add_string_filter(_this: usize, key: *const c_char, value: *const c_char, cmp: i32) {
        S.lock().unwrap().filters.push(Filter::Str(s(key), s(value), cmp));
    }
}

vmethod! {
    fn add_numerical_filter(_this: usize, key: *const c_char, value: i32, cmp: i32) {
        S.lock().unwrap().filters.push(Filter::Num(s(key), value as i64, cmp));
    }
}

vmethod! {
    fn add_near_filter(_this: usize, key: *const c_char, value: i32) {
        S.lock().unwrap().near.push((s(key), value as i64));
    }
}

vmethod! {
    fn add_slots_filter(_this: usize, n: i32) {
        S.lock().unwrap().slots = Some(n.max(0) as u32);
    }
}

vmethod! {
    fn add_count_filter(_this: usize, n: i32) {
        S.lock().unwrap().max_results = Some(n.max(0) as usize);
    }
}

vmethod! { fn ignore_i32(_this: usize, _v: i32) {} }
vmethod! { fn ignore_u64(_this: usize, _v: u64) {} }

vmethod! {
    fn get_lobby_by_index(_this: usize, ret: *mut u64, index: i32) -> *mut u64 {
        let id = S.lock().unwrap().results.get(index.max(0) as usize).copied().unwrap_or(0);
        unsafe { *ret = id };
        ret
    }
}

vmethod! {
    fn create_lobby(_this: usize, kind: i32, max_members: i32) -> u64 {
        let call = crate::steam::new_call();
        let Some(lan) = lan() else { return 0 };
        let id = new_lobby_id();
        let mut l = Lobby::new(NS);
        l.id = core_id(id);
        l.max_members = max_members.clamp(1, 250) as u32;
        l.permission = permission(kind as i64, true);
        l.join_by_id = true;
        set_hidden(&mut l, KEY_TYPE, kind.to_string());
        let created = lan.create_lobby(l).is_ok();
        log!("steam-lan CreateLobby type={kind} max={max_members} -> {id} ({created})");
        let result = LobbyCreatedT {
            result: if created { 1 } else { 2 },
            lobby: if created { id } else { 0 },
        };
        crate::steam::complete(call, LOBBY_CREATED, bytes(&result));
        crate::steam::inject(LOBBY_CREATED, bytes(&result));
        if created {
            crate::steam::inject(
                LOBBY_ENTER,
                bytes(&LobbyEnterT {
                    lobby: id,
                    permissions: 0,
                    locked: kind == 0,
                    response: ENTER_SUCCESS,
                }),
            );
            data_update(id, id, true);
            publish_presence();
        }
        call
    }
}

fn enter_result(lobby: u64, call: u64, response: u32) {
    let e = LobbyEnterT {
        lobby,
        permissions: 0,
        locked: false,
        response,
    };
    if call != 0 {
        crate::steam::complete(call, LOBBY_ENTER, bytes(&e));
    }
    crate::steam::inject(LOBBY_ENTER, bytes(&e));
    if response == ENTER_SUCCESS {
        data_update(lobby, lobby, true);
        publish_presence();
    }
}

vmethod! {
    fn join_lobby(_this: usize, lobby: u64) -> u64 {
        let call = crate::steam::new_call();
        let Some(lan) = lan() else { return 0 };
        let core = core_id(lobby);
        if lan.lobby(&core).is_some() {
            enter_result(lobby, call, ENTER_SUCCESS);
            return call;
        }
        let known = S.lock().unwrap().known.get(&lobby).cloned();
        let r = match &known {
            Some(l) => lan.join(l).or_else(|_| lan.join_by_id(NS, &core)),
            None => lan.join_by_id(NS, &core),
        };
        log!("steam-lan JoinLobby {lobby} -> {r:?}");
        match r {
            Ok(()) => {
                S.lock().unwrap().joins.insert(core, call);
            }
            Err(_) => enter_result(lobby, call, ENTER_DOESNT_EXIST),
        }
        call
    }
}

vmethod! {
    fn leave_lobby(_this: usize, lobby: u64) {
        if let Some(lan) = lan() {
            let r = lan.leave(&core_id(lobby));
            log!("steam-lan LeaveLobby {lobby} -> {r:?}");
        }
        S.lock().unwrap().chat.forget(lobby);
        publish_presence();
    }
}

vmethod! {
    fn invite_user_to_lobby(_this: usize, lobby: u64, user: u64) -> bool {
        match (view(lobby), peer_of(user)) {
            (Some(l), Some(p)) => crate::invite::send_lobby(Some(p), l) > 0,
            _ => false,
        }
    }
}

vmethod! {
    fn get_num_lobby_members(_this: usize, lobby: u64) -> i32 {
        view(lobby).map_or(0, |l| l.members.len() as i32)
    }
}

vmethod! {
    fn get_lobby_member_by_index(_this: usize, ret: *mut u64, lobby: u64, index: i32) -> *mut u64 {
        let id = view(lobby)
            .and_then(|l| l.members.get(index.max(0) as usize).map(|m| steam_id(m.id)))
            .unwrap_or(0);
        crate::steamlan::dump_answer(&format!("member #{index}"), || id.to_string());
        unsafe { *ret = id };
        ret
    }
}

vmethod! {
    fn get_lobby_data(_this: usize, lobby: u64, key: *const c_char) -> *const c_char {
        let k = s(key);
        let v = view(lobby)
            .and_then(|l| {
                let v = l.attrs.iter().find(|a| a.key.eq_ignore_ascii_case(&k) && !a.key.starts_with(HIDDEN))?.value.text();
                Some(crate::steamlan::ids_in_text(l.owner, v))
            })
            .unwrap_or_default();
        c_str(&v)
    }
}

vmethod! {
    fn set_lobby_data(_this: usize, lobby: u64, key: *const c_char, value: *const c_char) -> bool {
        let (k, v) = (s(key), s(value));
        crate::steamlan::dump("set-lobby-data", None, format!("{k}={v}").as_bytes());
        if k.is_empty() || k.starts_with(HIDDEN) {
            return false;
        }
        edit(lobby, |l| {
            peerparty_core::set_attr(&mut l.attrs, Attr { key: k, value: Value::Str(v), public: true })
        })
    }
}

vmethod! {
    fn get_lobby_data_count(_this: usize, lobby: u64) -> i32 {
        view(lobby).map_or(0, |l| visible(&l.attrs).len() as i32)
    }
}

/// Copies `src` into a C buffer of `cap` bytes, always terminated.
fn copy_out(dst: *mut c_char, cap: i32, src: &str) {
    if dst.is_null() || cap <= 0 {
        return;
    }
    let n = src.len().min(cap as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, n);
        *dst.add(n) = 0;
    }
}

vmethod! {
    fn get_lobby_data_by_index(
        _this: usize,
        lobby: u64,
        index: i32,
        key: *mut c_char,
        key_size: i32,
        value: *mut c_char,
        value_size: i32,
    ) -> bool {
        let Some(l) = view(lobby) else { return false };
        let attrs = visible(&l.attrs);
        let Some(a) = attrs.get(index.max(0) as usize) else { return false };
        copy_out(key, key_size, &a.key);
        copy_out(value, value_size, &crate::steamlan::ids_in_text(l.owner, a.value.text()));
        true
    }
}

vmethod! {
    fn delete_lobby_data(_this: usize, lobby: u64, key: *const c_char) -> bool {
        let k = s(key);
        if k.starts_with(HIDDEN) {
            return false;
        }
        edit(lobby, |l| {
            peerparty_core::remove_attr(&mut l.attrs, &k);
        })
    }
}

vmethod! {
    fn get_lobby_member_data(_this: usize, lobby: u64, user: u64, key: *const c_char) -> *const c_char {
        let k = s(key);
        let v = (|| {
            let l = view(lobby)?;
            let p = peer_of(user)?;
            let a = l.member(p)?.attrs.iter().find(|a| a.key.eq_ignore_ascii_case(&k))?;
            Some(crate::steamlan::ids_in_text(p, a.value.text()))
        })()
        .unwrap_or_default();
        c_str(&v)
    }
}

vmethod! {
    fn set_lobby_member_data(_this: usize, lobby: u64, key: *const c_char, value: *const c_char) {
        let (k, v) = (s(key), s(value));
        crate::steamlan::dump("set-member-data", None, format!("{k}={v}").as_bytes());
        let Some(lan) = lan() else { return };
        let a = Attr { key: k, value: Value::Str(v), public: false };
        if lan.update(&core_id(lobby), None, &[a], &[]).is_ok() {
            data_update(lobby, crate::steamlan::my_steam_id(), true);
        }
    }
}

fn chat_socket(lobby: u64) -> String {
    format!("{CHAT_SOCKET}{lobby}")
}

/// The longest lobby chat message Steam sends (and so the longest we take).
const CHAT_MAX: usize = 4096;

fn add_chat(lobby: u64, user: u64, data: Vec<u8>) {
    let chat_id = S.lock().unwrap().chat.push(lobby, (user, 1, data));
    crate::steam::inject(
        LOBBY_CHAT_MSG,
        bytes(&LobbyChatMsgT {
            lobby,
            user,
            kind: 1,
            chat_id,
        }),
    );
}

vmethod! {
    fn send_lobby_chat_msg(_this: usize, lobby: u64, body: *const u8, len: i32) -> bool {
        let (Some(lan), Some(l)) = (lan(), view(lobby)) else { return false };
        if body.is_null() || len <= 0 || len as usize > CHAT_MAX {
            return false;
        }
        let data = unsafe { std::slice::from_raw_parts(body, len as usize) }.to_vec();
        let me = lan.me();
        for m in l.members.iter().filter(|m| m.id != me) {
            lan.send(m.id, &chat_socket(lobby), 0, &crate::steamlan::ids_out(m.id, &data), true, true, true);
        }
        add_chat(lobby, crate::steamlan::my_steam_id(), data);
        true
    }
}

vmethod! {
    fn get_lobby_chat_entry(
        _this: usize,
        lobby: u64,
        chat_id: i32,
        user: *mut u64,
        data: *mut u8,
        cap: i32,
        kind: *mut i32,
    ) -> i32 {
        let st = S.lock().unwrap();
        let Some((from, k, body)) = (chat_id >= 0).then(|| st.chat.get(lobby, chat_id as u32)).flatten() else {
            return 0;
        };
        let n = body.len().min(cap.max(0) as usize);
        unsafe {
            if !user.is_null() {
                *user = *from;
            }
            if !kind.is_null() {
                *kind = *k as i32;
            }
            if !data.is_null() {
                std::ptr::copy_nonoverlapping(body.as_ptr(), data, n);
            }
        }
        n as i32
    }
}

vmethod! {
    fn request_lobby_data(_this: usize, lobby: u64) -> bool {
        let Some(lan) = lan() else { return false };
        if lan.lobby(&core_id(lobby)).is_some() {
            data_update(lobby, lobby, true);
            return true;
        }
        let mut q = Query::new(NS);
        q.lobby_id = Some(core_id(lobby));
        q.max_results = 1;
        let search = lan.search(q);
        S.lock().unwrap().data_requests.insert(search, lobby);
        true
    }
}

fn server_text(ip: u32, port: u16, server: u64) -> String {
    format!("{ip}:{port}:{server}")
}

fn parse_server(v: &str) -> Option<(u32, u16, u64)> {
    let mut it = v.split(':');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

vmethod! {
    fn set_lobby_game_server(_this: usize, lobby: u64, ip: u32, port: u16, server: u64) {
        let v = server_text(ip, port, server);
        if edit(lobby, |l| set_hidden(l, KEY_SERVER, v.clone())) {
            S.lock().unwrap().servers.insert(lobby, v);
            crate::steam::inject(
                LOBBY_GAME_CREATED,
                bytes(&LobbyGameCreatedT { lobby, server, ip, port }),
            );
        }
    }
}

vmethod! {
    fn get_lobby_game_server(_this: usize, lobby: u64, ip: *mut u32, port: *mut u16, server: *mut u64) -> bool {
        let Some((a, b, c)) = view(lobby).and_then(|l| text(&l, KEY_SERVER)).and_then(|v| parse_server(&v)) else {
            return false;
        };
        unsafe {
            if !ip.is_null() {
                *ip = a;
            }
            if !port.is_null() {
                *port = b;
            }
            if !server.is_null() {
                *server = c;
            }
        }
        true
    }
}

vmethod! {
    fn set_lobby_member_limit(_this: usize, lobby: u64, max: i32) -> bool {
        edit(lobby, |l| l.max_members = max.clamp(1, 250) as u32)
    }
}

vmethod! {
    fn get_lobby_member_limit(_this: usize, lobby: u64) -> i32 {
        view(lobby).map_or(0, |l| l.max_members as i32)
    }
}

vmethod! {
    fn set_lobby_type(_this: usize, lobby: u64, kind: i32) -> bool {
        edit(lobby, |l| set_hidden(l, KEY_TYPE, kind.to_string()))
    }
}

vmethod! {
    fn set_lobby_joinable(_this: usize, lobby: u64, joinable: bool) -> bool {
        edit(lobby, |l| set_hidden(l, KEY_JOINABLE, (joinable as u8).to_string()))
    }
}

vmethod! {
    fn get_lobby_owner(_this: usize, ret: *mut u64, lobby: u64) -> *mut u64 {
        let id = view(lobby).map_or(0, |l| steam_id(l.owner));
        crate::steamlan::dump_answer("owner", || id.to_string());
        unsafe { *ret = id };
        ret
    }
}

vmethod! {
    fn set_lobby_owner(_this: usize, lobby: u64, owner: u64) -> bool {
        match (lan(), peer_of(owner)) {
            (Some(lan), Some(p)) => lan.promote(&core_id(lobby), p).is_ok(),
            _ => false,
        }
    }
}

vmethod! { fn set_linked_lobby(_this: usize, _lobby: u64, _dependent: u64) -> bool { false } }

// ---- LAN events -> Steam callbacks ----

fn chat_update(lobby: u64, changed: u64, making_change: u64, state: u32) {
    crate::steam::inject(
        LOBBY_CHAT_UPDATE,
        bytes(&LobbyChatUpdateT {
            lobby,
            changed,
            making_change,
            state,
        }),
    );
}

/// Runs from the game's Steam dispatch before events: lobby chat that arrived, from
/// members of lobbies we're in (lobbychat.rs).
pub fn tick() {
    let Some(lan) = lan() else { return };
    let mut fresh = Vec::new();
    while let Some(mut p) = lan.receive_where(None, |s| s.starts_with(CHAT_SOCKET)) {
        if let Some(lobby) = p.socket[CHAT_SOCKET.len()..].parse::<u64>().ok() {
            crate::steamlan::ids_in(p.peer, &mut p.data);
            fresh.push((lobby, p.peer, p.data));
        }
    }
    let mut inbox = std::mem::take(&mut S.lock().unwrap().chat_inbox);
    let me = lan.me();
    let heard = inbox.sort(fresh, Instant::now(), CHAT_MAX, |lobby, who| {
        lan.lobby(&core_id(lobby)).is_some_and(|l| l.member(me).is_some() && l.member(who).is_some())
    });
    S.lock().unwrap().chat_inbox = inbox;
    for (lobby, who, data) in heard {
        add_chat(lobby, steam_id(who), data);
    }
}

pub fn on_event(ev: &Event) {
    let Some(lan) = lan() else { return };
    match ev {
        Event::ChanRequest { peer, socket } if socket.starts_with(CHAT_SOCKET) => {
            lan.accept(*peer, socket);
        }
        Event::SearchDone { search, results } => {
            let mine = S.lock().unwrap().search.is_some_and(|(s, _)| s == *search);
            if mine {
                let (call, count) = finish_search(results);
                log!("steam-lan RequestLobbyList found {count}");
                let r = bytes(&LobbyMatchListT { count });
                crate::steam::complete(call, LOBBY_MATCH_LIST, r.clone());
                crate::steam::inject(LOBBY_MATCH_LIST, r);
                return;
            }
            let wanted = S.lock().unwrap().data_requests.remove(search);
            if let Some(lobby) = wanted {
                let found = results.iter().find(|l| steam_lobby(&l.id) == Some(lobby));
                if let Some(l) = found {
                    remember(l);
                }
                data_update(lobby, lobby, found.is_some());
            }
        }
        Event::JoinDone { lobby: core, result } => {
            let Some(lobby) = steam_lobby(core) else { return };
            let call = S.lock().unwrap().joins.remove(core).unwrap_or(0);
            let response = match result {
                JoinResult::Ok => ENTER_SUCCESS,
                JoinResult::Full => ENTER_FULL,
                JoinResult::NotFound | JoinResult::TimedOut => ENTER_DOESNT_EXIST,
                JoinResult::NotAllowed => ENTER_NOT_ALLOWED,
                #[allow(unreachable_patterns)]
                _ => ENTER_ERROR,
            };
            log!("steam-lan JoinLobby {lobby} done: {result:?}");
            enter_result(lobby, call, response);
        }
        Event::LobbyUpdated { lobby: core } => {
            let Some(lobby) = steam_lobby(core) else { return };
            data_update(lobby, lobby, true);
            // A game server the owner announced since we last looked.
            let Some(l) = lan.lobby(core) else { return };
            if let Some(v) = text(&l, KEY_SERVER) {
                let new = S.lock().unwrap().servers.insert(lobby, v.clone()).as_ref() != Some(&v);
                if let (true, Some((ip, port, server))) = (new, parse_server(&v)) {
                    crate::steam::inject(
                        LOBBY_GAME_CREATED,
                        bytes(&LobbyGameCreatedT { lobby, server, ip, port }),
                    );
                }
            }
        }
        Event::MemberUpdated { lobby: core, member } => {
            let Some(lobby) = steam_lobby(core) else { return };
            data_update(lobby, steam_id(*member), true);
        }
        Event::MemberStatus { lobby: core, member, status } => {
            let Some(lobby) = steam_lobby(core) else { return };
            let who = steam_id(*member);
            let owner = lan.lobby(core).map_or(who, |l| steam_id(l.owner));
            match status {
                MemberStatus::Joined => chat_update(lobby, who, who, ENTERED),
                MemberStatus::Left => chat_update(lobby, who, who, LEFT),
                MemberStatus::Disconnected => chat_update(lobby, who, who, DISCONNECTED),
                MemberStatus::Kicked if *member == lan.me() => {
                    crate::steam::inject(
                        LOBBY_KICKED,
                        bytes(&LobbyKickedT { lobby, admin: owner, disconnected: 0 }),
                    );
                    publish_presence();
                }
                MemberStatus::Kicked => chat_update(lobby, who, owner, KICKED),
                MemberStatus::Promoted => {}
                MemberStatus::Closed => {
                    crate::steam::inject(
                        LOBBY_KICKED,
                        bytes(&LobbyKickedT { lobby, admin: 0, disconnected: 1 }),
                    );
                    publish_presence();
                }
            }
            data_update(lobby, lobby, true);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_structs_have_steams_layout() {
        assert_eq!(size_of::<LobbyEnterT>(), 24);
        assert_eq!(size_of::<LobbyCreatedT>(), 16);
        assert_eq!(size_of::<LobbyDataUpdateT>(), 24);
        assert_eq!(size_of::<LobbyChatUpdateT>(), 32);
        assert_eq!(size_of::<LobbyChatMsgT>(), 24);
        assert_eq!(size_of::<LobbyGameCreatedT>(), 24);
        assert_eq!(size_of::<LobbyKickedT>(), 24);
    }

    #[test]
    fn lobby_ids_are_lobby_chat_ids_and_round_trip() {
        let id = new_lobby_id();
        assert_eq!(id >> 56, 1);
        assert_eq!((id >> 52) & 0xf, 8, "chat account");
        assert_eq!((id >> 32) & 0xfffff, 0x60000, "lobby instance flags");
        assert_eq!(steam_lobby(&core_id(id)), Some(id));
        assert_eq!(steam_lobby("0123abcd"), None, "EOS lobby ids aren't Steam's");
    }

    #[test]
    fn filters_compare_the_lobby_value_with_the_filters() {
        let mut l = Lobby::new(NS);
        l.attrs.push(Attr { key: "level".into(), value: Value::Str("5".into()), public: true });
        l.attrs.push(Attr { key: "mode".into(), value: Value::Str("coop".into()), public: true });
        let num = |v, c| passes(&l, &Filter::Num("level".into(), v, c));
        assert!(num(5, 0) && num(6, -1) && num(5, -2) && num(4, 1) && num(5, 2) && num(4, 3));
        assert!(!num(5, -1) && !num(5, 1) && !num(5, 3));
        assert!(passes(&l, &Filter::Str("mode".into(), "coop".into(), 0)));
        assert!(!passes(&l, &Filter::Str("mode".into(), "pvp".into(), 0)));
        assert!(!passes(&l, &Filter::Str("missing".into(), "x".into(), 0)));
        assert!(passes(&l, &Filter::Str("missing".into(), "x".into(), 3)));
    }

    #[test]
    fn game_server_round_trips() {
        assert_eq!(parse_server(&server_text(0x0a000001, 27015, 42)), Some((0x0a000001, 27015, 42)));
    }
}
