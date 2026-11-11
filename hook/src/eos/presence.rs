//! EOS Presence on the LAN: each copy's status, rich text, data records and join info
//! travel in its LAN presence (keys "PeerParty:eos:..."; Steam's rich presence shares
//! the same presence and never sees these). Presence is about Epic accounts, so ids
//! here are EpicAccountIds.
//!
//! A LAN invite carrying our join info reaches a game that listens for
//! `JoinGameAccepted` as that callback, the way a friend's "Join game" from the Epic
//! overlay would (see net.rs and invite.rs for which SDK takes an invite).

use std::collections::BTreeMap;
use std::ffi::c_char;
use std::sync::{LazyLock, Mutex};

use peerparty_core::{Event, Lan, PeerId};

use super::abi::result::*;
use super::ids::{self, intern, text};
use super::{Kind, add_listener, call, hand_out, lan, later, listeners, owned_str, release, remove_listener, trace};

const PREFIX: &str = "peerparty:eos:";
const KEY_STATUS: &str = "peerparty:eos:status";
const KEY_RICH: &str = "peerparty:eos:richtext";
const KEY_JOIN: &str = "peerparty:eos:joininfo";
const DATA: &str = "peerparty:eos:data:";

// EOS_Presence_EStatus
const ONLINE: i32 = 1;

#[allow(non_snake_case)]
#[repr(C)]
struct UsersOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    TargetUserId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct QueryInfo {
    ResultCode: i32,
    ClientData: usize,
    LocalUserId: usize,
    TargetUserId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct SetPresenceOptions {
    ApiVersion: i32,
    LocalUserId: usize,
    Modification: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct SetPresenceInfo {
    ResultCode: i32,
    ClientData: usize,
    LocalUserId: usize,
    RichPresenceResultCode: i32,
}

#[allow(non_snake_case)]
#[repr(C)]
struct DataRecord {
    ApiVersion: i32,
    Key: *const c_char,
    Value: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct RecordsOptions {
    ApiVersion: i32,
    RecordsCount: i32,
    Records: *const DataRecord,
}

#[allow(non_snake_case)]
#[repr(C)]
struct Info {
    ApiVersion: i32,
    Status: i32,
    UserId: usize,
    ProductId: *const c_char,
    ProductVersion: *const c_char,
    Platform: *const c_char,
    RichText: *const c_char,
    RecordsCount: i32,
    Records: *const DataRecord,
    ProductName: *const c_char,
    IntegratedPlatform: *const c_char,
}

#[allow(non_snake_case)]
#[repr(C)]
struct ChangedInfo {
    ClientData: usize,
    LocalUserId: usize,
    PresenceUserId: usize,
}

#[allow(non_snake_case)]
#[repr(C)]
struct JoinGameAcceptedInfo {
    ClientData: usize,
    JoinInfo: *const c_char,
    LocalUserId: usize,
    TargetUserId: usize,
    UiEventId: u64,
}

/// Our presence: what the game last set.
#[derive(Clone, Default)]
struct Mine {
    status: Option<i32>,
    rich: Option<String>,
    data: BTreeMap<String, String>,
    join: Option<String>,
}

static MINE: LazyLock<Mutex<Mine>> = LazyLock::new(Mutex::default);
static EDITS: LazyLock<Mutex<std::collections::HashMap<usize, Box<Mine>>>> =
    LazyLock::new(Mutex::default);
static NEXT_UI_EVENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn resolve(name: &str) -> Option<usize> {
    Some(match name {
        "EOS_Presence_QueryPresence" => query_presence as *const () as usize,
        "EOS_Presence_HasPresence" => has_presence as *const () as usize,
        "EOS_Presence_CopyPresence" => copy_presence as *const () as usize,
        "EOS_Presence_Info_Release" => free as *const () as usize,
        "EOS_Presence_CreatePresenceModification" => create_modification as *const () as usize,
        "EOS_Presence_SetPresence" => set_presence as *const () as usize,
        "EOS_Presence_GetJoinInfo" => get_join_info as *const () as usize,
        "EOS_Presence_AddNotifyOnPresenceChanged" => notify_changed as *const () as usize,
        "EOS_Presence_AddNotifyJoinGameAccepted" => notify_join_accepted as *const () as usize,
        n if n.starts_with("EOS_Presence_RemoveNotify") => remove_notify as *const () as usize,
        "EOS_PresenceModification_SetStatus" => mod_set_status as *const () as usize,
        "EOS_PresenceModification_SetRawRichText" => mod_set_rich_text as *const () as usize,
        "EOS_PresenceModification_SetData" => mod_set_data as *const () as usize,
        "EOS_PresenceModification_DeleteData" => mod_delete_data as *const () as usize,
        "EOS_PresenceModification_SetJoinInfo" => mod_set_join_info as *const () as usize,
        "EOS_PresenceModification_SetTemplateId" | "EOS_PresenceModification_SetTemplateData" => {
            mod_ignore as *const () as usize
        }
        "EOS_PresenceModification_Release" => mod_release as *const () as usize,
        _ => return None,
    })
}

/// Our EOS presence, as LAN presence keys.
fn to_keys(m: &Mine) -> Vec<(String, String)> {
    let mut kv = Vec::new();
    if let Some(s) = m.status {
        kv.push((KEY_STATUS.into(), s.to_string()));
    }
    if let Some(r) = &m.rich {
        kv.push((KEY_RICH.into(), r.clone()));
    }
    if let Some(j) = &m.join {
        kv.push((KEY_JOIN.into(), j.clone()));
    }
    for (k, v) in &m.data {
        kv.push((format!("{DATA}{k}"), v.clone()));
    }
    kv
}

/// A peer's EOS presence from its LAN presence keys.
/// EOS_PRESENCE_DATA_MAX_KEYS, _MAX_KEY_LENGTH and _MAX_VALUE_LENGTH;
/// EOS_PRESENCE_RICH_TEXT_MAX_VALUE_LENGTH and the join info's limit are the last.
const MAX_DATA_KEYS: usize = 32;
const MAX_KEY_LEN: usize = 64;
const MAX_VALUE_LEN: usize = 255;

/// A player's presence from their LAN keys, held to the SDK's limits (another copy
/// enforces them when it sets them, but what arrives is only what a peer sent).
fn from_keys(kv: &[(String, String)]) -> Mine {
    let mut m = Mine::default();
    for (k, v) in kv.iter().filter(|(k, _)| k.starts_with(PREFIX)) {
        let v = super::clamp(v, MAX_VALUE_LEN).to_string();
        if let Some(d) = k.strip_prefix(DATA) {
            if d.len() <= MAX_KEY_LEN && (m.data.len() < MAX_DATA_KEYS || m.data.contains_key(d)) {
                m.data.insert(d.into(), v);
            }
        } else if k == KEY_STATUS {
            m.status = v.parse().ok();
        } else if k == KEY_RICH {
            m.rich = Some(v);
        } else if k == KEY_JOIN {
            m.join = Some(v);
        }
    }
    m
}

/// Our join info, for invites from games that join through presence.
pub fn my_join_info() -> String {
    MINE.lock().unwrap().join.clone().unwrap_or_default()
}

/// Whether the game takes a "join game" from presence.
pub fn takes_joins() -> bool {
    !listeners(Kind::JoinGameAccepted, None).is_empty()
}

fn presence_of(peer: PeerId) -> Option<Mine> {
    let lan = lan()?;
    if peer == lan.me() {
        return Some(MINE.lock().unwrap().clone());
    }
    lan.peer_name(peer)?;
    Some(from_keys(&lan.presence(peer)))
}

fn target(o: *const UsersOptions) -> Option<PeerId> {
    if o.is_null() {
        return None;
    }
    ids::peer_of_epic(unsafe { (*o).TargetUserId })
}

unsafe extern "system" fn query_presence(_h: usize, o: *const UsersOptions, cd: usize, cb: usize) {
    let (local, remote) = if o.is_null() { (0, 0) } else { unsafe { ((*o).LocalUserId, (*o).TargetUserId) } };
    let r = match target(o) {
        Some(p) if presence_of(p).is_some() => SUCCESS,
        Some(_) => NOT_FOUND,
        None => INVALID_PARAMETERS,
    };
    later(move || {
        let info = QueryInfo {
            ResultCode: r,
            ClientData: cd,
            LocalUserId: local,
            TargetUserId: remote,
        };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn has_presence(_h: usize, o: *const UsersOptions) -> i32 {
    target(o).and_then(presence_of).is_some() as i32
}

unsafe extern "system" fn copy_presence(_h: usize, o: *const UsersOptions, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(peer) = target(o) else {
        return INVALID_PARAMETERS;
    };
    let Some(m) = presence_of(peer) else {
        return NOT_FOUND;
    };
    let mut keep = Vec::new();
    let records: Vec<DataRecord> = m
        .data
        .iter()
        .map(|(k, v)| DataRecord {
            ApiVersion: 1,
            Key: owned_str(&mut keep, k),
            Value: owned_str(&mut keep, v),
        })
        .collect();
    let info = Info {
        ApiVersion: 3,
        Status: m.status.unwrap_or(ONLINE),
        UserId: ids::epic(peer),
        ProductId: owned_str(&mut keep, &super::product_id()),
        ProductVersion: owned_str(&mut keep, ""),
        Platform: owned_str(&mut keep, "WIN"),
        RichText: owned_str(&mut keep, m.rich.as_deref().unwrap_or("")),
        RecordsCount: records.len() as i32,
        Records: std::ptr::null(),
        ProductName: owned_str(&mut keep, ""),
        IntegratedPlatform: owned_str(&mut keep, ""),
    };
    struct Block {
        info: Info,
        records: Vec<DataRecord>,
    }
    let p = hand_out(Block { info, records }, keep) as *mut Block;
    unsafe {
        (*p).info.Records = (*p).records.as_ptr();
        *out = &(*p).info as *const Info as usize;
    }
    SUCCESS
}

unsafe extern "system" fn free(p: usize) {
    release(p);
}

unsafe extern "system" fn create_modification(_h: usize, _o: usize, out: *mut usize) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    // Starts from what we have: a modification only changes what it sets.
    let m = Box::new(MINE.lock().unwrap().clone());
    let k = &*m as *const Mine as usize;
    EDITS.lock().unwrap().insert(k, m);
    unsafe { *out = k };
    SUCCESS
}

fn with_edit(h: usize, f: impl FnOnce(&mut Mine) -> i32) -> i32 {
    match EDITS.lock().unwrap().get_mut(&h) {
        Some(m) => f(m),
        None => INVALID_PARAMETERS,
    }
}

unsafe extern "system" fn mod_set_status(h: usize, o: *const super::abi::I32Options) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let s = unsafe { (*o).Value };
    if !(0..=4).contains(&s) {
        return 3008; // EOS_Presence_StatusInvalid
    }
    with_edit(h, |m| {
        m.status = Some(s);
        SUCCESS
    })
}

unsafe extern "system" fn mod_set_rich_text(h: usize, o: *const super::abi::PtrOptions) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let t = unsafe { text((*o).Value as *const c_char) }.unwrap_or_default();
    with_edit(h, |m| {
        m.rich = Some(t);
        SUCCESS
    })
}

unsafe extern "system" fn mod_set_join_info(h: usize, o: *const super::abi::PtrOptions) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let j = unsafe { text((*o).Value as *const c_char) }.filter(|s| !s.is_empty());
    with_edit(h, |m| {
        m.join = j;
        SUCCESS
    })
}

unsafe fn records(o: *const RecordsOptions) -> Option<Vec<(String, Option<String>)>> {
    if o.is_null() {
        return None;
    }
    let o = unsafe { &*o };
    if o.RecordsCount < 0 || (o.RecordsCount > 0 && o.Records.is_null()) {
        return None;
    }
    let rs = unsafe { std::slice::from_raw_parts(o.Records, o.RecordsCount as usize) };
    Some(
        rs.iter()
            .filter_map(|r| {
                let k = unsafe { text(r.Key) }?;
                Some((k, unsafe { text(r.Value) }))
            })
            .collect(),
    )
}

unsafe extern "system" fn mod_set_data(h: usize, o: *const RecordsOptions) -> i32 {
    let Some(rs) = (unsafe { records(o) }) else {
        return INVALID_PARAMETERS;
    };
    with_edit(h, |m| {
        for (k, v) in rs {
            m.data.insert(k, v.unwrap_or_default());
        }
        SUCCESS
    })
}

/// DeleteData's records are `{ApiVersion, Key}`: the same start as a data record.
unsafe extern "system" fn mod_delete_data(h: usize, o: *const RecordsOptions) -> i32 {
    if o.is_null() {
        return INVALID_PARAMETERS;
    }
    let o = unsafe { &*o };
    if o.RecordsCount < 0 || (o.RecordsCount > 0 && o.Records.is_null()) {
        return INVALID_PARAMETERS;
    }
    #[allow(non_snake_case)]
    #[repr(C)]
    struct RecordId {
        ApiVersion: i32,
        Key: *const c_char,
    }
    let ids = unsafe { std::slice::from_raw_parts(o.Records as *const RecordId, o.RecordsCount as usize) };
    let keys: Vec<String> = ids.iter().filter_map(|r| unsafe { text(r.Key) }).collect();
    with_edit(h, |m| {
        for k in keys {
            m.data.remove(&k);
        }
        SUCCESS
    })
}

unsafe extern "system" fn mod_ignore(h: usize, _o: usize) -> i32 {
    with_edit(h, |_| SUCCESS)
}

unsafe extern "system" fn mod_release(h: usize) {
    EDITS.lock().unwrap().remove(&h);
}

unsafe extern "system" fn set_presence(_h: usize, o: *const SetPresenceOptions, cd: usize, cb: usize) {
    let (local, edit) = if o.is_null() { (0, 0) } else { unsafe { ((*o).LocalUserId, (*o).Modification) } };
    let new = EDITS.lock().unwrap().get(&edit).map(|m| (**m).clone());
    let r = match new {
        Some(m) => {
            trace("EOS_Presence_SetPresence", || {
                format!("status={:?} rich={:?} join={:?} data={}", m.status, m.rich, m.join, m.data.len())
            });
            crate::net::set_presence("eos", to_keys(&m));
            *MINE.lock().unwrap() = m;
            SUCCESS
        }
        None => INVALID_PARAMETERS,
    };
    later(move || {
        let info = SetPresenceInfo {
            ResultCode: r,
            ClientData: cd,
            LocalUserId: local,
            RichPresenceResultCode: r,
        };
        unsafe { call(cb, &info) };
    });
    // Our own presence changed too.
    if r == SUCCESS {
        if let Some(me) = lan().map(|l| l.me()) {
            later(move || changed(me));
        }
    }
}

unsafe extern "system" fn get_join_info(_h: usize, o: *const UsersOptions, buf: *mut c_char, len: *mut i32) -> i32 {
    if len.is_null() {
        return INVALID_PARAMETERS;
    }
    let Some(peer) = target(o) else {
        return INVALID_PARAMETERS;
    };
    let Some(join) = presence_of(peer).and_then(|m| m.join) else {
        return NOT_FOUND;
    };
    let need = join.len() as i32 + 1;
    unsafe {
        if buf.is_null() || *len < need {
            *len = need;
            return LIMIT_EXCEEDED;
        }
        std::ptr::copy_nonoverlapping(join.as_ptr(), buf as *mut u8, join.len());
        *buf.add(join.len()) = 0;
        *len = need;
    }
    SUCCESS
}

unsafe extern "system" fn notify_changed(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::PresenceChanged, f, cd, None)
}

unsafe extern "system" fn notify_join_accepted(_h: usize, _o: usize, cd: usize, f: usize) -> u64 {
    add_listener(Kind::JoinGameAccepted, f, cd, None)
}

unsafe extern "system" fn remove_notify(_h: usize, id: u64) {
    remove_listener(id);
}

fn changed(peer: PeerId) {
    let Some(me) = lan().map(|l| l.me()) else { return };
    for (f, cd) in listeners(Kind::PresenceChanged, None) {
        let info = ChangedInfo {
            ClientData: cd,
            LocalUserId: ids::epic(me),
            PresenceUserId: ids::epic(peer),
        };
        unsafe { call(f, &info) };
    }
}

pub fn on_event(lan: &Lan, ev: &Event) {
    match ev {
        Event::PeerJoined(p) | Event::PresenceChanged(p) => changed(*p),
        // A LAN invite with join info and no lobby: the overlay's "Join game". Custom
        // invites ride the same messages and are custominvites.rs's.
        Event::Invite {
            from,
            connect,
            lobby: None,
        } if !connect.is_empty() && !super::custominvites::is_ours(connect) => {
            // "Accepted" is what the game hears, and it joins on hearing it: so only
            // when the player has accepted, here in the overlay or by letting invites
            // through (never a stranger's). It used to fire for every invite, whoever
            // sent it, and again when the player did accept.
            if !crate::invite::decision(*from, connect, None).accept() {
                trace("JoinGameAccepted", || format!("from {from}: waits for the player"));
                return;
            }
            let ui = NEXT_UI_EVENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            trace("JoinGameAccepted", || format!("from {from}: {connect}"));
            for (f, cd) in listeners(Kind::JoinGameAccepted, None) {
                let info = JoinGameAcceptedInfo {
                    ClientData: cd,
                    JoinInfo: intern(connect),
                    LocalUserId: ids::epic(lan.me()),
                    TargetUserId: ids::epic(*from),
                    UiEventId: ui,
                };
                unsafe { call(f, &info) };
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_round_trips_through_lan_keys() {
        let mut m = Mine {
            status: Some(2),
            rich: Some("In the lobby".into()),
            join: Some("lobby=abc".into()),
            ..Default::default()
        };
        m.data.insert("level".into(), "3".into());
        let kv = to_keys(&m);
        assert!(kv.iter().all(|(k, _)| k.starts_with(PREFIX)), "Steam never sees these");
        let back = from_keys(&kv);
        assert_eq!(back.status, Some(2));
        assert_eq!(back.rich.as_deref(), Some("In the lobby"));
        assert_eq!(back.join.as_deref(), Some("lobby=abc"));
        assert_eq!(back.data.get("level").map(String::as_str), Some("3"));
    }

    /// What another player sent is held to the SDK's limits before the game sees it.
    #[test]
    fn received_presence_keeps_to_the_sdks_limits() {
        let mut kv: Vec<(String, String)> = (0..100).map(|n| (format!("{DATA}k{n:03}"), "v".to_string())).collect();
        kv.push((format!("{DATA}{}", "x".repeat(65)), "long key".into()));
        kv.push((KEY_RICH.into(), "é".repeat(200)));
        kv.push((KEY_JOIN.into(), "j".repeat(5000)));
        let m = from_keys(&kv);
        assert_eq!(m.data.len(), MAX_DATA_KEYS);
        assert!(m.data.keys().all(|k| k.len() <= MAX_KEY_LEN));
        let rich = m.rich.unwrap();
        assert!(rich.len() <= MAX_VALUE_LEN && rich.chars().all(|c| c == 'é'), "cut at a character");
        assert_eq!(m.join.unwrap().len(), MAX_VALUE_LEN);
    }

    #[test]
    fn info_matches_the_sdk() {
        let p = size_of::<usize>();
        assert_eq!(std::mem::offset_of!(Info, UserId), 8);
        assert_eq!(std::mem::offset_of!(Info, ProductId), 8 + p);
        assert_eq!(std::mem::offset_of!(JoinGameAcceptedInfo, UiEventId), (4 * p).max(16));
    }
}
