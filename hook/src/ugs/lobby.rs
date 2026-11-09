//! Unity Lobby v1 (lobby.services.api.unity.com/v1) on LAN lobbies.
//!
//! A UGS lobby is a LAN lobby in namespace `ugs.lobby`, owned by its host's copy, as
//! pfmp.rs maps PlayFab's lobbies. Its id is the UGS id; its join code is the LAN
//! lobby's bucket (searchable). Lobby attributes:
//! `ugs` (public JSON: name, isPrivate, isLocked, created, and a hash of the password
//! so a joiner checks it before joining) and `d:<key>` per data entry (public when its
//! visibility is public). Member attributes: `p` (public JSON: joined, lastUpdated,
//! profile), `c` (members only: connectionInfo, allocationId) and `pd:<key>` per player
//! data entry. Visibility is applied when a copy renders the lobby for its own player:
//! member data to members, private lobby data to the host, private player data to that
//! player (the LAN is trusted, as everywhere in PeerParty).
//!
//! Versions are this copy's: each time its rendering of a lobby changes the version
//! goes up and the change goes out as Wire patches (wire.rs). The game's SDK only ever
//! talks to its own copy, so GETs and patches agree without the LAN agreeing on a
//! number. Patches are RFC 6902 operations on the paths the SDK's LobbyPatcher
//! understands; removals name old indices, additions new ones, as its
//! ApplyPatchesToLobby applies them (removals from the highest index down, then
//! inserts, then per-player changes).
//!
//! Routes, bodies, success statuses (create 201, delete / heartbeat / remove 204) and
//! error codes (LobbyExceptionReason, in ErrorStatus.code) follow the SDK's
//! LobbyApiClient and models and services.docs.unity.com/lobby/v1. Heartbeats are
//! accepted and change nothing: a LAN lobby lives as long as its host's copy does.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use peerparty_core::{find_attr, remove_attr, set_attr, Attr, Filter, JoinResult, Lobby, LobbyError, Member, MemberStatus, Op, PeerId, Query, Value};

use crate::json::Json;
use crate::log;
use crate::net;

use super::{error_body, now_iso, peer_of, player_id, random_text, wire};

pub const NS: &str = "ugs.lobby";
const SEARCH_WAIT: Duration = Duration::from_secs(2);
const JOIN_WAIT: Duration = Duration::from_secs(8);
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const ID_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

// LobbyExceptionReason values.
const VALIDATION: u32 = 16000;
const NOT_FOUND: u32 = 16001;
const PLAYER_NOT_FOUND: u32 = 16002;
const FULL: u32 = 16004;
const LOCKED: u32 = 16005;
const NO_OPEN: u32 = 16006;
const ALREADY_EXISTS: u32 = 16007;
const BAD_PASSWORD: u32 = 16009;
const FORBIDDEN: u32 = 16403;

type Reply = (u16, String);

fn err(status: u16, code: u32, title: &str, detail: &str) -> Reply {
    (status, error_body(status, code, title, detail))
}

fn not_found() -> Reply {
    err(404, NOT_FOUND, "Lobby Not Found", "no such lobby on the LAN")
}

// ---- attributes ----

fn jattr(attrs: &[Attr], key: &str) -> Json {
    find_attr(attrs, key)
        .and_then(|a| Json::parse(&a.value.text()))
        .unwrap_or(Json::Null)
}

fn attr_json(key: &str, json: &Json, public: bool) -> Attr {
    Attr { key: key.into(), value: Value::Str(json.to_string()), public }
}

fn meta(l: &Lobby) -> Json {
    jattr(&l.attrs, "ugs")
}

fn flag(j: &Json, key: &str) -> bool {
    matches!(j.get(key), Json::Bool(true))
}

fn text(j: &Json, key: &str) -> String {
    match j.get(key) {
        Json::Str(s) => s.clone(),
        Json::Null => String::new(),
        other => other.to_string(),
    }
}

fn pairs(j: &Json) -> &[(String, Json)] {
    match j {
        Json::Obj(p) => p,
        _ => &[],
    }
}

fn set(obj: &mut Json, key: &str, value: Json) {
    if let Json::Obj(p) = obj {
        match p.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value,
            None => p.push((key.into(), value)),
        }
    }
}

fn password_hash(lobby: &str, password: &str) -> String {
    crate::photon::crypto::sha256(format!("peerparty-ugs:{lobby}:{password}").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A data entry as stored: value as text, visibility, index (lobby data only).
fn data_entry(v: &Json, lobby_data: bool) -> Json {
    let value = match v.get("value") {
        Json::Str(s) => Json::str(s.clone()),
        Json::Null => Json::Null,
        other => Json::str(other.to_string()),
    };
    let vis = v.get("visibility").as_str().unwrap_or("public").to_ascii_lowercase();
    let mut e = vec![("value", value), ("visibility", Json::str(vis))];
    if lobby_data {
        if let Some(i) = v.get("index").as_str() {
            e.push(("index", Json::str(i.to_ascii_uppercase())));
        }
    }
    Json::obj(e)
}

fn visible(vis: &str, member: bool, owner: bool) -> bool {
    match vis {
        "member" => member,
        "private" => owner,
        _ => true,
    }
}

/// A joining or creating player's member attributes, from the request's `player`.
fn player_attrs(player: &Json, joined: &str) -> Vec<Attr> {
    let mut p = vec![("joined", Json::str(joined)), ("lastUpdated", Json::str(joined))];
    if let Json::Obj(_) = player.get("profile") {
        p.push(("profile", player.get("profile").clone()));
    }
    let mut out = vec![attr_json("p", &Json::obj(p), true)];
    let mut c = Vec::new();
    for k in ["connectionInfo", "allocationId"] {
        if let Some(s) = player.get(k).as_str() {
            c.push((k, Json::str(s)));
        }
    }
    if !c.is_empty() {
        out.push(attr_json("c", &Json::obj(c), false));
    }
    for (k, v) in pairs(player.get("data")) {
        if !v.is_null() && !v.get("value").is_null() {
            let e = data_entry(v, false);
            let public = e.get("visibility").as_str() == Some("public");
            out.push(attr_json(&format!("pd:{k}"), &e, public));
        }
    }
    out
}

// ---- rendering ----

fn render_player(m: &Member, viewer_member: bool, is_self: bool) -> Json {
    let p = jattr(&m.attrs, "p");
    let c = jattr(&m.attrs, "c");
    let mut out = vec![("id", Json::str(player_id(m.id)))];
    if let Json::Obj(_) = p.get("profile") {
        out.push(("profile", p.get("profile").clone()));
    }
    if viewer_member {
        for k in ["connectionInfo", "allocationId"] {
            if let Some(s) = c.get(k).as_str() {
                out.push((k, Json::str(s)));
            }
        }
    }
    let mut data = Vec::new();
    for a in &m.attrs {
        if let Some(k) = a.key.strip_prefix("pd:") {
            let e = Json::parse(&a.value.text()).unwrap_or(Json::Null);
            let vis = e.get("visibility").as_str().unwrap_or("public").to_string();
            if visible(&vis, viewer_member, is_self) {
                data.push((k.to_string(), e));
            }
        }
    }
    out.push(("data", Json::Obj(data)));
    let joined = p.get("joined").as_str().unwrap_or("2026-01-01T00:00:00.000Z").to_string();
    let updated = p.get("lastUpdated").as_str().map(str::to_string).unwrap_or_else(|| joined.clone());
    out.push(("joined", Json::str(joined)));
    out.push(("lastUpdated", Json::str(updated)));
    Json::obj(out)
}

/// The lobby as `viewer`'s copy shows it, without `version` and `lastUpdated`.
pub fn render(l: &Lobby, viewer: PeerId) -> Json {
    let member = l.member(viewer).is_some();
    let host = l.owner == viewer;
    let m = meta(l);
    let mut data = Vec::new();
    for a in &l.attrs {
        if let Some(k) = a.key.strip_prefix("d:") {
            let e = Json::parse(&a.value.text()).unwrap_or(Json::Null);
            let vis = e.get("visibility").as_str().unwrap_or("public").to_string();
            if visible(&vis, member, host) {
                data.push((k.to_string(), e));
            }
        }
    }
    let players: Vec<Json> = l.members.iter().map(|mm| render_player(mm, member, mm.id == viewer)).collect();
    let mut out = vec![("id", Json::str(l.id.clone()))];
    if member {
        out.push(("lobbyCode", Json::str(l.bucket.clone())));
    }
    out.extend([
        ("upid", Json::str("peerparty")),
        ("environmentId", Json::str(super::auth::ENVIRONMENT_ID)),
        ("name", Json::str(text(&m, "name"))),
        ("maxPlayers", Json::Num(l.max_members as f64)),
        ("availableSlots", Json::Num(l.open_slots() as f64)),
        ("isPrivate", Json::Bool(flag(&m, "isPrivate"))),
        ("isLocked", Json::Bool(flag(&m, "isLocked"))),
        ("hasPassword", Json::Bool(!text(&m, "pwh").is_empty())),
        ("players", Json::Arr(players)),
        ("data", Json::Obj(data)),
        ("hostId", Json::str(player_id(l.owner))),
        ("created", Json::str(m.get("created").as_str().unwrap_or("2026-01-01T00:00:00.000Z"))),
    ]);
    Json::obj(out)
}

fn with_version(mut view: Json, version: i64, updated: &str) -> Json {
    set(&mut view, "lastUpdated", Json::str(updated));
    set(&mut view, "version", Json::Num(version as f64));
    view
}

// ---- versions and patches ----

struct Tracked {
    version: i64,
    view: Json,
    updated: String,
}

static TRACK: LazyLock<Mutex<HashMap<String, Tracked>>> = LazyLock::new(Mutex::default);
static KICKED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

pub fn channel(id: &str) -> String {
    format!("lobby:{id}")
}

/// Lobbies whose changes this copy follows.
pub fn tracked() -> Vec<String> {
    TRACK.lock().unwrap().keys().cloned().collect()
}

/// A LAN member status change: we may have been kicked.
pub fn member_status(id: &str, member: PeerId, status: MemberStatus) {
    let Some(lan) = net::lan() else { return };
    if !TRACK.lock().unwrap().contains_key(id) {
        return;
    }
    if member == lan.me() && status == MemberStatus::Kicked {
        KICKED.lock().unwrap().insert(id.to_string());
    }
    log!("ugs lobby {id}: player {} {status:?}", player_id(member));
}

/// Stops following a lobby without telling the game it's gone (we left or deleted it).
fn forget(id: &str) {
    TRACK.lock().unwrap().remove(id);
}

/// Our copy's current view of a lobby we're in, with its version; changes since the
/// last look go out over Wire. None if we're not in it (any more).
pub fn refresh(id: &str) -> Option<Json> {
    let lan = net::lan()?;
    match lan.lobby(id) {
        Some(l) if l.namespace == NS => {
            let view = render(&l, lan.me());
            let now = now_iso();
            let mut t = TRACK.lock().unwrap();
            let e = t.entry(id.to_string()).or_insert_with(|| Tracked {
                version: 1,
                view: view.clone(),
                updated: now.clone(),
            });
            if e.view != view {
                let patches = diff(&e.view, &view, &now);
                e.version += 1;
                e.updated = now;
                e.view = view.clone();
                publish(id, e.version, patches);
            }
            Some(with_version(view, e.version, &e.updated))
        }
        Some(_) => None,
        None => {
            let gone = TRACK.lock().unwrap().remove(id);
            if let Some(e) = gone {
                if KICKED.lock().unwrap().remove(id) {
                    log!("ugs lobby {id}: we were removed");
                    wire::kick(&channel(id));
                } else {
                    log!("ugs lobby {id} is gone");
                    publish(id, e.version + 1, vec![patch("remove", "/", None)]);
                }
            }
            None
        }
    }
}

fn version_of(id: &str) -> (i64, String) {
    TRACK
        .lock()
        .unwrap()
        .get(id)
        .map(|t| (t.version, t.updated.clone()))
        .unwrap_or((1, now_iso()))
}

fn publish(id: &str, version: i64, patches: Vec<Json>) {
    let payload = Json::obj(vec![("version", Json::Num(version as f64)), ("patches", Json::Arr(patches))]);
    wire::publish(&channel(id), &payload.to_string());
}

fn patch(op: &str, path: &str, value: Option<Json>) -> Json {
    let mut p = vec![("op", Json::str(op)), ("path", Json::str(path))];
    if let Some(v) = value {
        p.push(("value", v));
    }
    Json::obj(p)
}

fn diff_data(old: &Json, new: &Json, prefix: &str, out: &mut Vec<Json>) {
    let (o, n) = (pairs(old), pairs(new));
    for (k, v) in n {
        match o.iter().find(|(ok, _)| ok == k) {
            None => out.push(patch("add", &format!("{prefix}/data/{k}"), Some(v.clone()))),
            Some((_, ov)) if ov != v => out.push(patch("replace", &format!("{prefix}/data/{k}"), Some(v.clone()))),
            _ => {}
        }
    }
    for (k, _) in o {
        if !n.iter().any(|(nk, _)| nk == k) {
            out.push(patch("remove", &format!("{prefix}/data/{k}"), None));
        }
    }
}

/// The patches that turn `old` into `new` (two renderings), as the SDK applies them.
pub fn diff(old: &Json, new: &Json, updated: &str) -> Vec<Json> {
    let mut out = Vec::new();
    for k in ["name", "isPrivate", "isLocked", "hasPassword", "availableSlots", "maxPlayers", "hostId"] {
        if old.get(k) != new.get(k) && !new.get(k).is_null() {
            out.push(patch("replace", &format!("/{k}"), Some(new.get(k).clone())));
        }
    }
    diff_data(old.get("data"), new.get("data"), "", &mut out);
    let ids = |j: &Json| -> Vec<String> {
        j.get("players").as_arr().iter().map(|p| text(p, "id")).collect()
    };
    let (oid, nid) = (ids(old), ids(new));
    let kept_old: Vec<&String> = oid.iter().filter(|i| nid.contains(i)).collect();
    let kept_new: Vec<&String> = nid.iter().filter(|i| oid.contains(i)).collect();
    let (oldp, newp) = (old.get("players").as_arr(), new.get("players").as_arr());
    if kept_old != kept_new {
        // The roster was reordered: replace it all.
        for i in (0..oid.len()).rev() {
            out.push(patch("remove", &format!("/players/{i}"), None));
        }
        for (j, p) in newp.iter().enumerate() {
            out.push(patch("add", &format!("/players/{j}"), Some(p.clone())));
        }
    } else {
        for (i, id) in oid.iter().enumerate().rev() {
            if !nid.contains(id) {
                out.push(patch("remove", &format!("/players/{i}"), None));
            }
        }
        for (j, id) in nid.iter().enumerate() {
            if !oid.contains(id) {
                out.push(patch("add", &format!("/players/{j}"), Some(newp[j].clone())));
            }
        }
        for (j, id) in nid.iter().enumerate() {
            let Some(i) = oid.iter().position(|o| o == id) else { continue };
            let (po, pn) = (&oldp[i], &newp[j]);
            diff_data(po.get("data"), pn.get("data"), &format!("/players/{j}"), &mut out);
            if po.get("connectionInfo") != pn.get("connectionInfo") {
                match pn.get("connectionInfo") {
                    Json::Null => out.push(patch("remove", &format!("/players/{j}/connectionInfo"), None)),
                    v => out.push(patch("replace", &format!("/players/{j}/connectionInfo"), Some(v.clone()))),
                }
            }
            if po.get("lastUpdated") != pn.get("lastUpdated") {
                out.push(patch("replace", &format!("/players/{j}/lastUpdated"), Some(pn.get("lastUpdated").clone())));
            }
        }
    }
    out.push(patch("replace", "/lastUpdated", Some(Json::str(updated))));
    out
}

// ---- LAN lookups ----

fn find(q: Query) -> Option<Lobby> {
    super::search(q, SEARCH_WAIT).into_iter().find(|l| l.namespace == NS)
}

fn find_by_id(id: &str) -> Option<Lobby> {
    if let Some(l) = net::lan()?.lobby(id).filter(|l| l.namespace == NS) {
        return Some(l);
    }
    let mut q = Query::new(NS);
    q.lobby_id = Some(id.to_string());
    find(q)
}

fn find_by_code(code: &str) -> Option<Lobby> {
    let mut q = Query::new(NS);
    q.filters.push(Filter::Bucket { op: Op::Eq, value: code.to_string() });
    find(q)
}

fn all_lobbies() -> Vec<Lobby> {
    let mut q = Query::new(NS);
    q.max_results = 500;
    let lan = net::lan();
    super::search(q, SEARCH_WAIT)
        .into_iter()
        .filter(|l| l.namespace == NS)
        // Our own copy of a lobby we're in shows its member-only parts.
        .map(|l| lan.and_then(|lan| lan.lobby(&l.id)).unwrap_or(l))
        .collect()
}

// ---- queries ----

fn field_value(view: &Json, field: &str) -> Option<Json> {
    match field {
        "MaxPlayers" => Some(view.get("maxPlayers").clone()),
        "AvailableSlots" => Some(view.get("availableSlots").clone()),
        "Name" => Some(view.get("name").clone()),
        "Created" => Some(view.get("created").clone()),
        "LastUpdated" => Some(view.get("lastUpdated").clone()),
        "IsLocked" => Some(view.get("isLocked").clone()),
        "HasPassword" => Some(view.get("hasPassword").clone()),
        "ID" => Some(view.get("id").clone()),
        f => pairs(view.get("data"))
            .iter()
            .find(|(_, e)| e.get("index").as_str() == Some(f) && e.get("visibility").as_str() == Some("public"))
            .map(|(_, e)| e.get("value").clone()),
    }
}

fn numeric(field: &str) -> bool {
    matches!(field, "MaxPlayers" | "AvailableSlots") || (field.len() == 2 && field.starts_with('N'))
}

/// Lobby's QueryFilter: field, op (CONTAINS, EQ, NE, LT, LE, GT, GE), value.
pub fn passes(view: &Json, filter: &Json) -> bool {
    let field = filter.get("field").as_str().unwrap_or_default();
    let op = filter.get("op").as_str().unwrap_or("EQ").to_ascii_uppercase();
    let want = text(filter, "value");
    let Some(have) = field_value(view, field).filter(|h| !h.is_null()) else {
        return op == "NE";
    };
    let have_text = match &have {
        Json::Str(s) => s.clone(),
        other => other.to_string(),
    };
    use std::cmp::Ordering;
    let ord = if numeric(field) {
        match (have_text.parse::<f64>(), want.parse::<f64>()) {
            (Ok(a), Ok(b)) => a.partial_cmp(&b),
            _ => None,
        }
    } else if matches!(have, Json::Bool(_)) {
        Some(have_text.cmp(&want.to_ascii_lowercase()))
    } else {
        Some(have_text.as_str().cmp(want.as_str()))
    };
    match op.as_str() {
        "CONTAINS" => have_text.to_lowercase().contains(&want.to_lowercase()),
        "EQ" => ord == Some(Ordering::Equal),
        "NE" => ord != Some(Ordering::Equal),
        "LT" => ord == Some(Ordering::Less),
        "LE" => matches!(ord, Some(Ordering::Less | Ordering::Equal)),
        "GT" => ord == Some(Ordering::Greater),
        "GE" => matches!(ord, Some(Ordering::Greater | Ordering::Equal)),
        _ => false,
    }
}

fn sort(views: &mut [Json], order: &[Json]) {
    if order.is_empty() {
        // Oldest first, so every copy lists the LAN's lobbies the same way.
        views.sort_by(|a, b| text(a, "created").cmp(&text(b, "created")));
        return;
    }
    views.sort_by(|a, b| {
        for o in order {
            let field = o.get("field").as_str().unwrap_or("Created");
            let asc = !matches!(o.get("asc"), Json::Bool(false));
            let (x, y) = (field_value(a, field).unwrap_or(Json::Null), field_value(b, field).unwrap_or(Json::Null));
            let c = if numeric(field) {
                x.as_f64().unwrap_or(0.0).partial_cmp(&y.as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                x.to_string().cmp(&y.to_string())
            };
            let c = if asc { c } else { c.reverse() };
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Lobbies a query or quick join may return: public and unlocked.
fn listed(l: &Lobby) -> bool {
    let m = meta(l);
    !flag(&m, "isPrivate") && !flag(&m, "isLocked")
}

fn query(body: &Json) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    let count = body.get("count").as_f64().unwrap_or(10.0).clamp(1.0, 100.0) as usize;
    let skip = body.get("skip").as_f64().unwrap_or(0.0).max(0.0) as usize;
    let filters = body.get("filter").as_arr();
    let mut views: Vec<Json> = all_lobbies()
        .iter()
        .filter(|l| listed(l))
        .map(|l| {
            let (v, u) = version_of(&l.id);
            with_version(render(l, lan.me()), v, &u)
        })
        .filter(|v| filters.iter().all(|f| passes(v, f)))
        .collect();
    sort(&mut views, body.get("order").as_arr());
    let results: Vec<Json> = views.into_iter().skip(skip).take(count).collect();
    log!("ugs lobby query: {} found", results.len());
    (200, Json::obj(vec![("results", Json::Arr(results)), ("continuationToken", Json::Null)]).to_string())
}

// ---- create and join ----

fn create(body: &Json, id: Option<&str>) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    let name = body.get("name").as_str().unwrap_or_default().trim().to_string();
    if name.is_empty() {
        return err(400, VALIDATION, "Validation Error", "a lobby needs a name");
    }
    let max = body.get("maxPlayers").as_f64().unwrap_or(0.0);
    if !(1.0..=100.0).contains(&max) {
        return err(400, VALIDATION, "Validation Error", "maxPlayers must be 1 to 100");
    }
    let password = body.get("password").as_str().filter(|p| !p.is_empty());
    if password.is_some_and(|p| p.len() < 8 || p.len() > 64) {
        return err(400, VALIDATION, "Validation Error", "a password is 8 to 64 characters");
    }
    let now = now_iso();
    let mut l = Lobby::new(NS);
    l.id = id.map(str::to_string).unwrap_or_else(|| random_text(22, ID_ALPHABET));
    l.bucket = random_text(6, CODE_ALPHABET);
    l.max_members = max as u32;
    l.join_by_id = true;
    l.host_migration = true;
    let meta = Json::obj(vec![
        ("name", Json::str(name.clone())),
        ("isPrivate", Json::Bool(flag(body, "isPrivate"))),
        ("isLocked", Json::Bool(flag(body, "isLocked"))),
        ("created", Json::str(now.clone())),
        ("pwh", Json::str(password.map(|p| password_hash(&l.id, p)).unwrap_or_default())),
    ]);
    l.attrs.push(attr_json("ugs", &meta, true));
    for (k, v) in pairs(body.get("data")) {
        if !v.is_null() && !v.get("value").is_null() {
            let e = data_entry(v, true);
            let public = e.get("visibility").as_str() == Some("public");
            l.attrs.push(attr_json(&format!("d:{k}"), &e, public));
        }
    }
    l.members = vec![Member { id: lan.me(), attrs: player_attrs(body.get("player"), &now) }];
    match lan.create_lobby(l) {
        Ok(l) => {
            log!("ugs created lobby {} '{name}' code {} for {}", l.id, l.bucket, l.max_members);
            match refresh(&l.id) {
                Some(view) => (201, view.to_string()),
                None => not_found(),
            }
        }
        Err(_) => err(409, ALREADY_EXISTS, "Lobby Already Exists", "a lobby with this id exists"),
    }
}

fn join_found(target: Lobby, body: &Json) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    let id = target.id.clone();
    if lan.lobby(&id).is_some() {
        return match refresh(&id) {
            Some(v) => (200, v.to_string()),
            None => not_found(),
        };
    }
    let m = meta(&target);
    if flag(&m, "isLocked") {
        return err(409, LOCKED, "Lobby Locked", "the lobby is locked");
    }
    if target.open_slots() == 0 {
        return err(409, FULL, "Lobby Full", "the lobby is full");
    }
    let pwh = text(&m, "pwh");
    if !pwh.is_empty() && password_hash(&id, body.get("password").as_str().unwrap_or_default()) != pwh {
        return err(403, BAD_PASSWORD, "Incorrect Password", "the password is wrong");
    }
    super::clear_join(&id);
    match lan.join(&target) {
        Ok(()) | Err(LobbyError::AlreadyIn) => {}
        Err(_) => return not_found(),
    }
    match super::wait_join(&id, JOIN_WAIT) {
        JoinResult::Ok => {}
        JoinResult::Full => return err(409, FULL, "Lobby Full", "the lobby is full"),
        JoinResult::NotAllowed => return err(409, LOCKED, "Lobby Locked", "the host refused"),
        JoinResult::NotFound => return not_found(),
        JoinResult::TimedOut => {
            let _ = lan.leave(&id);
            return err(404, NOT_FOUND, "Lobby Not Found", "the host didn't answer");
        }
    }
    let attrs = player_attrs(body.get("player"), &now_iso());
    let _ = lan.update(&id, None, &attrs, &[]);
    log!("ugs joined lobby {id} '{}'", text(&m, "name"));
    match refresh(&id) {
        Some(v) => (200, v.to_string()),
        None => not_found(),
    }
}

fn quick_join(body: &Json) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    let filters = body.get("filter").as_arr();
    let mut candidates: Vec<(Json, Lobby)> = all_lobbies()
        .into_iter()
        .filter(|l| listed(l) && l.open_slots() > 0 && text(&meta(l), "pwh").is_empty() && l.member(lan.me()).is_none())
        .map(|l| (with_version(render(&l, lan.me()), 1, &now_iso()), l))
        .filter(|(v, _)| filters.iter().all(|f| passes(v, f)))
        .collect();
    candidates.sort_by(|a, b| text(&a.0, "created").cmp(&text(&b.0, "created")));
    for (_, l) in candidates {
        let r = join_found(l, body);
        if r.0 == 200 {
            return r;
        }
    }
    err(404, NO_OPEN, "No Open Lobbies", "no lobby on the LAN matches")
}

fn create_or_join(id: &str, body: &Json) -> Reply {
    match find_by_id(id) {
        Some(l) => join_found(l, body),
        None => create(body, Some(id)),
    }
}

// ---- changes ----

fn update(id: &str, body: &Json) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    let Some(mut l) = lan.lobby(id).filter(|l| l.namespace == NS) else { return not_found() };
    if !lan.is_owner(id) {
        return err(403, FORBIDDEN, "Forbidden", "only the host changes the lobby");
    }
    let mut m = meta(&l);
    if let Some(n) = body.get("name").as_str().filter(|n| !n.trim().is_empty()) {
        set(&mut m, "name", Json::str(n.trim()));
    }
    for k in ["isPrivate", "isLocked"] {
        if let Json::Bool(b) = body.get(k) {
            set(&mut m, k, Json::Bool(*b));
        }
    }
    if let Some(p) = body.get("password").as_str() {
        set(&mut m, "pwh", Json::str(if p.is_empty() { String::new() } else { password_hash(id, p) }));
    }
    if let Some(n) = body.get("maxPlayers").as_f64() {
        if !(1.0..=100.0).contains(&n) {
            return err(400, VALIDATION, "Validation Error", "maxPlayers must be 1 to 100");
        }
        l.max_members = n as u32;
    }
    set_attr(&mut l.attrs, attr_json("ugs", &m, true));
    for (k, v) in pairs(body.get("data")) {
        let key = format!("d:{k}");
        if v.is_null() || v.get("value").is_null() {
            remove_attr(&mut l.attrs, &key);
        } else {
            let e = data_entry(v, true);
            let public = e.get("visibility").as_str() == Some("public");
            set_attr(&mut l.attrs, attr_json(&key, &e, public));
        }
    }
    if lan.update(id, Some(&l), &[], &[]).is_err() {
        return not_found();
    }
    if let Some(host) = body.get("hostId").as_str().filter(|h| !h.eq_ignore_ascii_case(&player_id(lan.me()))) {
        let Some(peer) = peer_of(host) else {
            return err(404, PLAYER_NOT_FOUND, "Player Not Found", "no such player in the lobby");
        };
        if lan.promote(id, peer).is_err() {
            return err(404, PLAYER_NOT_FOUND, "Player Not Found", "no such player in the lobby");
        }
        log!("ugs lobby {id}: host is now {host}");
    }
    match refresh(id) {
        Some(v) => (200, v.to_string()),
        None => not_found(),
    }
}

fn update_player(id: &str, player: &str, body: &Json) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    let Some(l) = lan.lobby(id).filter(|l| l.namespace == NS) else { return not_found() };
    if !player.eq_ignore_ascii_case(&player_id(lan.me())) {
        return err(403, FORBIDDEN, "Forbidden", "players change only their own data");
    }
    let Some(me) = l.member(lan.me()) else { return not_found() };
    let mut setv = Vec::new();
    let mut remove = Vec::new();
    for (k, v) in pairs(body.get("data")) {
        let key = format!("pd:{k}");
        if v.is_null() || v.get("value").is_null() {
            remove.push(key);
        } else {
            let e = data_entry(v, false);
            let public = e.get("visibility").as_str() == Some("public");
            setv.push(attr_json(&key, &e, public));
        }
    }
    let mut c = jattr(&me.attrs, "c");
    if c.is_null() {
        c = Json::Obj(vec![]);
    }
    let mut c_changed = false;
    for k in ["connectionInfo", "allocationId"] {
        if let Some(s) = body.get(k).as_str() {
            set(&mut c, k, Json::str(s));
            c_changed = true;
        }
    }
    if c_changed {
        setv.push(attr_json("c", &c, false));
    }
    let mut p = jattr(&me.attrs, "p");
    if p.is_null() {
        p = Json::obj(vec![("joined", Json::str(now_iso()))]);
    }
    set(&mut p, "lastUpdated", Json::str(now_iso()));
    setv.push(attr_json("p", &p, true));
    if lan.update(id, None, &setv, &remove).is_err() {
        return not_found();
    }
    match refresh(id) {
        Some(v) => (200, v.to_string()),
        None => not_found(),
    }
}

fn remove_player(id: &str, player: &str) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    if lan.lobby(id).filter(|l| l.namespace == NS).is_none() {
        return not_found();
    }
    if player.eq_ignore_ascii_case(&player_id(lan.me())) {
        forget(id);
        let _ = lan.leave(id);
        log!("ugs left lobby {id}");
        return (204, String::new());
    }
    if !lan.is_owner(id) {
        return err(403, FORBIDDEN, "Forbidden", "only the host removes other players");
    }
    match peer_of(player).map(|p| lan.kick(id, p)) {
        Some(Ok(())) => {
            log!("ugs lobby {id}: removed player {player}");
            refresh(id);
            (204, String::new())
        }
        _ => err(404, PLAYER_NOT_FOUND, "Player Not Found", "no such player in the lobby"),
    }
}

fn delete(id: &str) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    if lan.lobby(id).filter(|l| l.namespace == NS).is_none() {
        return not_found();
    }
    if !lan.is_owner(id) {
        return err(403, FORBIDDEN, "Forbidden", "only the host deletes the lobby");
    }
    forget(id);
    let _ = lan.destroy(id);
    log!("ugs deleted lobby {id}");
    (204, String::new())
}

fn get(id: &str) -> Reply {
    if let Some(v) = refresh(id) {
        return (200, v.to_string());
    }
    let Some(lan) = net::lan() else { return not_found() };
    match find_by_id(id) {
        Some(l) => (200, with_version(render(&l, lan.me()), 1, &now_iso()).to_string()),
        None => not_found(),
    }
}

fn joined(hosted: bool) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    let ids: Vec<Json> = lan
        .my_lobbies(NS)
        .into_iter()
        .filter(|l| !hosted || l.owner == lan.me())
        .map(|l| Json::str(l.id))
        .collect();
    (200, Json::Arr(ids).to_string())
}

fn tokens(id: &str, body: &Json) -> Reply {
    let Some(lan) = net::lan() else { return not_found() };
    if lan.lobby(id).filter(|l| l.namespace == NS).is_none() {
        return not_found();
    }
    let wanted: Vec<String> = match body {
        Json::Arr(a) => a.iter().filter_map(|t| t.get("tokenType").as_str().map(str::to_string)).collect(),
        other => other.get("tokenType").as_str().map(|s| vec![s.to_string()]).unwrap_or_default(),
    };
    let mut out = Vec::new();
    for t in if wanted.is_empty() { vec!["wireJoin".to_string()] } else { wanted } {
        let uri = if t == "vivoxJoin" { format!("sip:confctl-g-peerparty.{id}@peerparty.invalid") } else { channel(id) };
        out.push((t, Json::obj(vec![("tokenValue", Json::str("peerparty")), ("uri", Json::str(uri))])));
    }
    (200, Json::Obj(out).to_string())
}

fn bulk(id: &str, body: &Json) -> Reply {
    let mut last = get(id);
    if let Json::Obj(_) = body.get("lobbyUpdate") {
        last = update(id, body.get("lobbyUpdate"));
        if last.0 != 200 {
            return last;
        }
    }
    for (player, upd) in pairs(body.get("playerUpdates")) {
        last = update_player(id, player, upd);
    }
    for p in body.get("playersToRemove").as_arr() {
        if let Some(p) = p.as_str().map(str::to_string).or_else(|| p.get("id").as_str().map(str::to_string)) {
            remove_player(id, &p);
        }
    }
    if !body.get("playersToAdd").as_arr().is_empty() {
        log!("ugs lobby {id}: bulk update's playersToAdd isn't supported (players join themselves on the LAN)");
    }
    match refresh(id) {
        Some(v) => (200, v.to_string()),
        None => last,
    }
}

fn heartbeat(id: &str) -> Reply {
    match net::lan().and_then(|l| l.lobby(id)) {
        Some(_) => (204, String::new()),
        None => not_found(),
    }
}

/// Lobby's routes (the SDK's base path ends in /v1).
pub fn handle(method: &str, path: &str, body: &Json) -> Reply {
    let p = path.strip_prefix("/v1").unwrap_or(path);
    let segs: Vec<&str> = p.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (method, segs.as_slice()) {
        ("POST", ["create"]) => create(body, None),
        ("POST", ["query"]) => query(body),
        ("POST", ["quickjoin"]) => quick_join(body),
        ("POST", ["joinbycode"]) => {
            let code = body.get("lobbyCode").as_str().unwrap_or_default().trim().to_ascii_uppercase();
            if code.is_empty() {
                return err(400, VALIDATION, "Validation Error", "a join code is needed");
            }
            match find_by_code(&code) {
                Some(l) => join_found(l, body),
                None => not_found(),
            }
        }
        ("GET", ["joined"]) => joined(false),
        ("GET", ["hosted"]) => joined(true),
        ("POST", [id, "join"]) => match find_by_id(id) {
            Some(l) => join_found(l, body),
            None => not_found(),
        },
        ("POST", [id, "createorjoin"]) => create_or_join(id, body),
        ("POST", [id, "heartbeat"]) => heartbeat(id),
        ("POST", [id, "reconnect"]) => get(id),
        ("POST", [id, "tokens"]) => tokens(id, body),
        ("POST", [id, "bulkupdate"]) => bulk(id, body),
        ("POST", [id, "players", player]) => update_player(id, player, body),
        ("DELETE", [id, "players", player]) => remove_player(id, player),
        ("GET", [id]) => get(id),
        ("POST", [id]) => update(id, body),
        ("DELETE", [id]) => delete(id),
        _ => err(404, 16404, "Not Found", "PeerParty doesn't answer this Lobby call"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(b: u8) -> PeerId {
        PeerId([b; 16])
    }

    fn member(b: u8, data: &[(&str, &str, &str)]) -> Member {
        let mut p = Json::obj(vec![]);
        let mut d = Vec::new();
        for (k, v, vis) in data {
            d.push((k.to_string(), Json::obj(vec![("value", Json::str(*v)), ("visibility", Json::str(*vis))])));
        }
        if let Json::Obj(o) = &mut p {
            o.push(("data".into(), Json::Obj(d)));
        }
        Member { id: peer(b), attrs: player_attrs(&p, "2026-10-31T00:00:00.000Z") }
    }

    fn lobby() -> Lobby {
        let mut l = Lobby::new(NS);
        l.id = "L1".into();
        l.bucket = "ABC123".into();
        l.owner = peer(1);
        l.max_members = 4;
        let meta = Json::obj(vec![("name", Json::str("Room")), ("isPrivate", Json::Bool(false)), ("isLocked", Json::Bool(false)), ("created", Json::str("2026-10-31T00:00:00.000Z")), ("pwh", Json::str(""))]);
        l.attrs.push(attr_json("ugs", &meta, true));
        for (k, v, vis, idx) in [("map", "forest", "public", Some("S1")), ("secret", "x", "private", None), ("team", "red", "member", None), ("level", "7", "public", Some("N1"))] {
            let mut e = vec![("value", Json::str(v)), ("visibility", Json::str(vis))];
            if let Some(i) = idx {
                e.push(("index", Json::str(i)));
            }
            l.attrs.push(attr_json(&format!("d:{k}"), &data_entry(&Json::obj(e), true), vis == "public"));
        }
        l.members = vec![member(1, &[("ready", "1", "public")]), member(2, &[("ready", "0", "public"), ("hidden", "h", "private")])];
        l
    }

    #[test]
    fn rendering_applies_visibility_per_viewer() {
        let l = lobby();
        let host = render(&l, peer(1));
        assert_eq!(pairs(host.get("data")).len(), 4, "the host sees private lobby data");
        assert_eq!(host.get("lobbyCode").as_str(), Some("ABC123"));
        let guest = render(&l, peer(2));
        assert!(pairs(guest.get("data")).iter().all(|(k, _)| k != "secret"));
        assert!(pairs(guest.get("data")).iter().any(|(k, _)| k == "team"));
        // Private player data: only its owner sees it.
        let p2_for_2 = &guest.get("players").as_arr()[1];
        assert!(pairs(p2_for_2.get("data")).iter().any(|(k, _)| k == "hidden"));
        let p2_for_1 = &host.get("players").as_arr()[1];
        assert!(pairs(p2_for_1.get("data")).iter().all(|(k, _)| k != "hidden"));
        // Outsiders: public data only, no code.
        let out = render(&l.public_view(), peer(9));
        assert_eq!(pairs(out.get("data")).len(), 2);
        assert!(out.get("lobbyCode").is_null());
        assert_eq!(out.get("hostId").as_str(), Some(player_id(peer(1)).as_str()));
        assert_eq!(out.get("availableSlots").as_f64(), Some(2.0));
    }

    #[test]
    fn query_filters_follow_lobbys_fields_and_ops() {
        let v = with_version(render(&lobby(), peer(9)), 1, "2026-10-31T00:00:00.000Z");
        let f = |field: &str, op: &str, value: &str| Json::obj(vec![("field", Json::str(field)), ("op", Json::str(op)), ("value", Json::str(value))]);
        assert!(passes(&v, &f("AvailableSlots", "GT", "0")));
        assert!(!passes(&v, &f("AvailableSlots", "GT", "2")));
        assert!(passes(&v, &f("MaxPlayers", "EQ", "4")));
        assert!(passes(&v, &f("S1", "EQ", "forest")));
        assert!(!passes(&v, &f("S1", "EQ", "desert")));
        assert!(passes(&v, &f("N1", "GE", "7")) && passes(&v, &f("N1", "LT", "10")));
        // Numbers compare as numbers: 7 < 10 although "7" > "10".
        assert!(passes(&v, &f("N1", "LT", "10")));
        assert!(passes(&v, &f("Name", "CONTAINS", "roo")));
        assert!(passes(&v, &f("IsLocked", "EQ", "false")));
        assert!(passes(&v, &f("HasPassword", "EQ", "false")));
        // A field the lobby doesn't have only passes NE.
        assert!(!passes(&v, &f("S2", "EQ", "x")) && passes(&v, &f("S2", "NE", "x")));
    }

    /// The SDK's LobbyPatcher + ApplyPatchesToLobby, restated: what a patch list does to
    /// a cached lobby. The test checks diff's patches turn the old view into the new.
    fn apply(view: &Json, patches: &[Json]) -> Json {
        let mut v = view.clone();
        let mut left: Vec<usize> = Vec::new();
        let mut joined: Vec<(usize, Json)> = Vec::new();
        let mut player_ops: Vec<(usize, String, String, Json)> = Vec::new();
        for p in patches {
            let op = p.get("op").as_str().unwrap().to_string();
            let path = p.get("path").as_str().unwrap().to_string();
            let value = p.get("value").clone();
            if let Some(rest) = path.strip_prefix("/players/") {
                let mut parts = rest.splitn(2, '/');
                let i: usize = parts.next().unwrap().parse().unwrap();
                match parts.next() {
                    None if op == "remove" => left.push(i),
                    None => joined.push((i, value)),
                    Some(sub) => player_ops.push((i, op, format!("/{sub}"), value)),
                }
            } else if let Some(k) = path.strip_prefix("/data/") {
                let mut data = pairs(v.get("data")).to_vec();
                data.retain(|(dk, _)| dk != k);
                if op != "remove" {
                    data.push((k.to_string(), value));
                }
                set(&mut v, "data", Json::Obj(data));
            } else if path == "/" {
                return Json::Null;
            } else {
                set(&mut v, path.trim_start_matches('/'), value);
            }
        }
        let mut players = v.get("players").as_arr().to_vec();
        left.sort_by(|a, b| b.cmp(a));
        for i in left {
            players.remove(i);
        }
        for (i, p) in joined {
            players.insert(i, p);
        }
        for (i, op, sub, value) in player_ops {
            let pl = &mut players[i];
            if let Some(k) = sub.strip_prefix("/data/") {
                let mut data = pairs(pl.get("data")).to_vec();
                data.retain(|(dk, _)| dk != k);
                if op != "remove" {
                    data.push((k.to_string(), value));
                }
                set(pl, "data", Json::Obj(data));
            } else if op == "remove" {
                if let Json::Obj(o) = pl {
                    o.retain(|(k, _)| format!("/{k}") != sub);
                }
            } else {
                set(pl, sub.trim_start_matches('/'), value);
            }
        }
        set(&mut v, "players", Json::Arr(players));
        v
    }

    /// Order-insensitive comparison of two renderings (data maps may reorder).
    fn same(a: &Json, b: &Json) -> bool {
        fn norm(j: &Json) -> Json {
            match j {
                Json::Obj(p) => {
                    let mut p: Vec<(String, Json)> = p.iter().map(|(k, v)| (k.clone(), norm(v))).collect();
                    p.sort_by(|x, y| x.0.cmp(&y.0));
                    Json::Obj(p)
                }
                Json::Arr(a) => Json::Arr(a.iter().map(norm).collect()),
                o => o.clone(),
            }
        }
        norm(a) == norm(b)
    }

    #[test]
    fn patches_replay_to_the_new_view() {
        let viewer = peer(1);
        let before = lobby();
        let mut after = lobby();
        // Lobby changes: name, a data entry changed, one removed, one added; a player
        // left, two joined, the remaining one changed data and connection info.
        let mut m = meta(&after);
        set(&mut m, "name", Json::str("Renamed"));
        set_attr(&mut after.attrs, attr_json("ugs", &m, true));
        set_attr(&mut after.attrs, attr_json("d:map", &data_entry(&Json::obj(vec![("value", Json::str("cave")), ("visibility", Json::str("public")), ("index", Json::str("S1"))]), true), true));
        remove_attr(&mut after.attrs, "d:team");
        after.attrs.push(attr_json("d:mode", &data_entry(&Json::obj(vec![("value", Json::str("ctf")), ("visibility", Json::str("public"))]), true), true));
        after.members.remove(1);
        after.members.push(member(3, &[("ready", "1", "public")]));
        after.members.push(member(4, &[]));
        let me = after.member_mut(viewer).unwrap();
        set_attr(&mut me.attrs, attr_json("pd:ready", &data_entry(&Json::obj(vec![("value", Json::str("0")), ("visibility", Json::str("public"))]), false), true));
        set_attr(&mut me.attrs, attr_json("c", &Json::obj(vec![("connectionInfo", Json::str("udp://x"))]), false));
        set_attr(&mut me.attrs, attr_json("pd:new", &data_entry(&Json::obj(vec![("value", Json::str("n")), ("visibility", Json::str("member"))]), false), false));
        let (old, new) = (render(&before, viewer), render(&after, viewer));
        let patches = diff(&old, &new, "2026-10-31T01:00:00.000Z");
        let replayed = apply(&old, &patches);
        let mut want = new.clone();
        set(&mut want, "lastUpdated", Json::str("2026-10-31T01:00:00.000Z"));
        assert!(same(&replayed, &want), "replayed {replayed}\nwant {want}\npatches {}", Json::Arr(patches.clone()));
        // Every path is one the SDK's LobbyPatcher parses.
        for p in &patches {
            let path = p.get("path").as_str().unwrap();
            let ok = ["/name", "/lastUpdated", "/availableSlots", "/hostId"].contains(&path)
                || path.starts_with("/data/")
                || (path.starts_with("/players/") && {
                    let rest: Vec<&str> = path.split('/').collect();
                    rest.len() == 3 || rest[3] == "data" || rest[3] == "connectionInfo" || rest[3] == "lastUpdated"
                });
            assert!(ok, "LobbyPatcher doesn't know {path}");
        }
        // A reordered roster is replaced whole and still replays.
        let mut swapped = before.clone();
        swapped.members.swap(0, 1);
        let p2 = diff(&old, &render(&swapped, viewer), "t");
        let mut want2 = render(&swapped, viewer);
        set(&mut want2, "lastUpdated", Json::str("t"));
        assert!(same(&apply(&old, &p2), &want2));
    }

    #[test]
    fn unchanged_views_need_only_the_timestamp() {
        let v = render(&lobby(), peer(1));
        assert_eq!(diff(&v, &v, "t").len(), 1);
    }

    #[test]
    fn routes_without_a_lan_are_not_found() {
        // Tests run outside a game started through PeerParty: every route answers in Lobby's error shape.
        let (status, body) = handle("GET", "/v1/abc", &Json::Null);
        assert_eq!(status, 404);
        assert_eq!(Json::parse(&body).unwrap().get("code").as_f64(), Some(NOT_FOUND as f64));
        assert_eq!(handle("POST", "/v1/nope/nope/nope/nope", &Json::Null).0, 404);
    }

    #[test]
    fn password_hashes_depend_on_the_lobby() {
        assert_ne!(password_hash("a", "password1"), password_hash("b", "password1"));
        assert_eq!(password_hash("a", "password1").len(), 64);
    }
}
