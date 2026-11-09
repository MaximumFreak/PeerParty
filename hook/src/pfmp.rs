//! PeerParty's PlayFab Lobby & Matchmaking layer (on in every game started through PeerParty): the game's PlayFab
//! lobbies and matchmaking tickets live on the LAN instead of PlayFab's services.
//!
//! Every `PF*` export of the game's PlayFab Multiplayer DLL is swapped by name
//! (swap.rs); the real library is never initialized. A PlayFab lobby is a LAN lobby
//! (namespace `playfab.lobby`) and its connection string is `LBL-<lobby id>`. Search
//! properties are public lobby attributes (`s:<key>`), lobby properties private ones
//! (`l:<key>`); each member carries its entity id (`pf`, `pft`) and its properties
//! (`m:<key>`). `PFMultiplayerFindLobbies` searches the LAN and applies PlayFab's
//! OData-style filter and sort strings itself.
//!
//! Matchmaking tickets go through the local PlayFab service's matchmaker (playfab/api.rs),
//! so both libraries pair tickets the same way. A match's lobby arrangement string is
//! `LBA-<creator entity>-<match id>`: the member whose ticket sorts first creates the
//! arranged lobby and the others join it.
//!
//! State changes queue up and reach the game through
//! `PFMultiplayerStartProcessing{Lobby,Matchmaking}StateChanges`, in the order the
//! library's header documents. Struct layouts follow PFLobby.h and PFMatchmaking.h
//! (github.com/PlayFab/PlayFabMultiplayerUnreal), checked against the C# interop of
//! the 1.7.2 Unity package. Game-server functions report E_NOTIMPL.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, c_char};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use peerparty_core::{Attr, Event, JoinResult, Lan, Lobby, Member, MemberStatus, PeerId, Permission, Query, Value};

use crate::log;
use crate::net::{self, Front};
use crate::pe::Export;
use crate::playfab::api::{self, Poll};

pub const NS: &str = "playfab.lobby";
pub const CONNECT_PREFIX: &str = "LBL-";
const ARRANGED_PREFIX: &str = "LBA-";
const ENTITY_TYPE: &str = "title_player_account";
/// How long a member of a match keeps trying to join the lobby its creator makes.
const ARRANGED_WAIT: Duration = Duration::from_secs(30);
/// How long the other members give the creator before their first try: the creator
/// hears of the match up to a poll later, and a join that beats it waits out a timeout.
const ARRANGED_HEAD_START: Duration = Duration::from_secs(2);

const S_OK: i32 = 0;
const E_NOTIMPL: i32 = 0x8000_4001_u32 as i32;
const E_ABORT: i32 = 0x8000_4004_u32 as i32;
const E_FAIL: i32 = 0x8000_4005_u32 as i32;
const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const E_TIMEOUT: i32 = 0x8007_05B4_u32 as i32;
const E_LOBBY_NOT_FOUND: i32 = 0x8007_0490_u32 as i32;
const E_LOBBY_FULL: i32 = 0x8007_0091_u32 as i32;

// PFLobbyStateChangeType
const CREATE_AND_JOIN_COMPLETED: u32 = 0;
const JOIN_COMPLETED: u32 = 1;
const MEMBER_ADDED: u32 = 2;
const MEMBER_REMOVED: u32 = 4;
const FORCE_REMOVE_COMPLETED: u32 = 5;
const LEAVE_COMPLETED: u32 = 6;
const UPDATED: u32 = 7;
const POST_UPDATE_COMPLETED: u32 = 8;
const DISCONNECTING: u32 = 9;
const DISCONNECTED: u32 = 10;
const JOIN_ARRANGED_COMPLETED: u32 = 11;
const FIND_LOBBIES_COMPLETED: u32 = 12;
const INVITE_RECEIVED: u32 = 13;
const INVITE_LISTENER_CHANGED: u32 = 14;
const SEND_INVITE_COMPLETED: u32 = 15;

// PFLobbyMemberRemovedReason
const LOCAL_LEFT: u32 = 0;
const LOCAL_REMOVED: u32 = 1;
const REMOTE_LEFT: u32 = 2;

// PFLobbyDisconnectingReason
const NO_LOCAL_MEMBERS: u32 = 0;
const LOBBY_DELETED: u32 = 1;

// PFLobbyAccessPolicy
const ACCESS_PRIVATE: u32 = 2;

// PFMatchmakingTicketStatus and PFMatchmakingStateChangeType
const TICKET_CREATING: u32 = 0;
const TICKET_WAITING: u32 = 3;
const TICKET_MATCHED: u32 = 4;
const TICKET_CANCELED: u32 = 5;
const TICKET_FAILED: u32 = 6;
const TICKET_STATUS_CHANGED: u32 = 0;
const TICKET_COMPLETED: u32 = 1;

// ---- the library's structs ----

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EntityKey {
    id: usize,
    ty: usize,
}

#[repr(C)]
struct CreateConfig {
    max_members: u32,
    migration: u32,
    access: u32,
    search_count: u32,
    search_keys: usize,
    search_values: usize,
    lobby_count: u32,
    lobby_keys: usize,
    lobby_values: usize,
}

#[repr(C)]
struct ArrangedConfig {
    max_members: u32,
    migration: u32,
    access: u32,
    member_count: u32,
    member_keys: usize,
    member_values: usize,
}

#[repr(C)]
struct JoinConfig {
    member_count: u32,
    member_keys: usize,
    member_values: usize,
}

#[repr(C)]
struct DataUpdate {
    new_owner: usize,
    max_members: usize,
    access: usize,
    lock: usize,
    search_count: u32,
    search_keys: usize,
    search_values: usize,
    lobby_count: u32,
    lobby_keys: usize,
    lobby_values: usize,
}

#[repr(C)]
struct MemberDataUpdate {
    count: u32,
    keys: usize,
    values: usize,
}

#[repr(C)]
struct SearchConfig {
    friends: usize,
    filter: usize,
    sort: usize,
    count: usize,
}

#[repr(C)]
struct SearchResult {
    lobby_id: usize,
    connection: usize,
    owner: usize,
    max_members: u32,
    members: u32,
    search_count: u32,
    search_keys: usize,
    search_values: usize,
    friend_count: u32,
    friends: usize,
    lock: u32,
}

#[repr(C)]
struct MemberUpdateSummary {
    member: EntityKey,
    connection_updated: bool,
    count: u32,
    keys: usize,
}

#[repr(C)]
struct TicketConfig {
    timeout: u32,
    queue: usize,
    members_count: u32,
    members: usize,
}

#[repr(C)]
struct MatchMember {
    entity: EntityKey,
    team: usize,
    attributes: usize,
}

#[repr(C)]
struct MatchDetails {
    match_id: usize,
    members: usize,
    member_count: u32,
    regions: usize,
    region_count: u32,
    arrangement: usize,
    server: usize,
}

// ---- state changes ----

#[repr(C)]
struct LobbyCompleted {
    ty: u32,
    result: i32,
    ctx: usize,
    lobby: usize,
}

/// JoinLobbyCompleted and JoinArrangedLobbyCompleted.
#[repr(C)]
struct JoinCompleted {
    ty: u32,
    result: i32,
    member: EntityKey,
    ctx: usize,
    lobby: usize,
}

#[repr(C)]
struct MemberAdded {
    ty: u32,
    lobby: usize,
    member: EntityKey,
}

#[repr(C)]
struct MemberRemoved {
    ty: u32,
    lobby: usize,
    member: EntityKey,
    reason: u32,
}

/// ForceRemoveMemberCompleted and PostUpdateCompleted.
#[repr(C)]
struct MemberOpCompleted {
    ty: u32,
    result: i32,
    lobby: usize,
    member: EntityKey,
    ctx: usize,
}

#[repr(C)]
struct LeaveCompleted {
    ty: u32,
    lobby: usize,
    local_user: usize,
    ctx: usize,
}

#[repr(C)]
struct Updated {
    ty: u32,
    lobby: usize,
    owner: bool,
    max_members: bool,
    access: bool,
    lock: bool,
    search_count: u32,
    search_keys: usize,
    lobby_count: u32,
    lobby_keys: usize,
    member_count: u32,
    members: usize,
    server: bool,
    server_count: u32,
    server_keys: usize,
    server_connection: bool,
}

#[repr(C)]
struct Disconnecting {
    ty: u32,
    lobby: usize,
    reason: u32,
}

#[repr(C)]
struct Disconnected {
    ty: u32,
    lobby: usize,
}

#[repr(C)]
struct FindCompleted {
    ty: u32,
    result: i32,
    searcher: EntityKey,
    ctx: usize,
    count: u32,
    results: usize,
}

#[repr(C)]
struct InviteReceived {
    ty: u32,
    listener: EntityKey,
    inviter: EntityKey,
    connection: usize,
    lobby_id: usize,
}

#[repr(C)]
struct ListenerChanged {
    ty: u32,
    listener: EntityKey,
}

#[repr(C)]
struct SendInviteCompleted {
    ty: u32,
    result: i32,
    lobby: usize,
    sender: EntityKey,
    invitee: EntityKey,
    ctx: usize,
}

#[repr(C)]
struct TicketChanged {
    ty: u32,
    ticket: usize,
}

#[repr(C)]
struct TicketCompleted {
    ty: u32,
    result: i32,
    ticket: usize,
    ctx: usize,
}

// ---- entity keys ----

struct KeyStore {
    _id: CString,
    _ty: CString,
    key: EntityKey,
}

/// Entity keys handed to the game, kept for the life of the process (there are few).
static KEYS: LazyLock<Mutex<HashMap<(String, String), &'static KeyStore>>> = LazyLock::new(Mutex::default);

fn key_ref(id: &str, ty: &str) -> &'static EntityKey {
    let mut keys = KEYS.lock().unwrap();
    let store = keys.entry((id.to_string(), ty.to_string())).or_insert_with(|| {
        let id_c = cstring(id);
        let ty_c = cstring(ty);
        let key = EntityKey {
            id: id_c.as_ptr() as usize,
            ty: ty_c.as_ptr() as usize,
        };
        Box::leak(Box::new(KeyStore {
            _id: id_c,
            _ty: ty_c,
            key,
        }))
    });
    &store.key
}

fn key(id: &str, ty: &str) -> EntityKey {
    *key_ref(id, ty)
}

unsafe fn entity_in(p: *const EntityKey) -> Option<(String, String)> {
    if p.is_null() {
        return None;
    }
    let k = unsafe { p.read_unaligned() };
    let id = unsafe { text(k.id as *const c_char) };
    let ty = unsafe { text(k.ty as *const c_char) };
    (!id.is_empty()).then(|| (id, if ty.is_empty() { ENTITY_TYPE.into() } else { ty }))
}

// ---- helpers ----

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

unsafe fn text(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        crate::read::cstr(p as usize).unwrap_or_default()
    }
}

unsafe fn put<T>(out: *mut T, v: T) {
    if !out.is_null() {
        unsafe { out.write_unaligned(v) };
    }
}

/// Key/value arrays from the game. A null value means "delete this key".
unsafe fn pairs(count: u32, keys: usize, values: usize) -> Vec<(String, Option<String>)> {
    if keys == 0 || count == 0 {
        return Vec::new();
    }
    (0..count as usize)
        .map(|i| unsafe {
            let k = *(keys as *const usize).add(i);
            let v = if values == 0 { 0 } else { *(values as *const usize).add(i) };
            (
                text(k as *const c_char),
                (v != 0).then(|| text(v as *const c_char)),
            )
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn str_attr(key: String, value: &str, public: bool) -> Attr {
    Attr {
        key,
        value: Value::Str(value.to_string()),
        public,
    }
}

fn attr_text(attrs: &[Attr], key: &str) -> Option<String> {
    peerparty_core::find_attr(attrs, key).map(|a| a.value.text())
}

fn new_guid() -> String {
    let h = PeerId::random().hex();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Text kept alive with a queued state change, plus a pointer array over it.
#[derive(Default)]
struct Strings {
    owned: Vec<CString>,
}

impl Strings {
    fn add(&mut self, s: &str) -> usize {
        let c = cstring(s);
        let p = c.as_ptr() as usize;
        self.owned.push(c);
        p
    }

    fn array(&mut self, items: &[String]) -> Vec<usize> {
        items.iter().map(|s| self.add(s)).collect()
    }
}

fn ptr_of<T>(v: &[T]) -> usize {
    if v.is_empty() { 0 } else { v.as_ptr() as usize }
}

// ---- a lobby as the game sees it ----

struct ViewMember {
    peer: PeerId,
    entity: String,
    ty: String,
    props: Vec<(CString, CString)>,
    prop_keys: Vec<usize>,
}

/// A snapshot of a lobby. The game reads it through the PFLobbyGet* functions, so a
/// replaced snapshot stays alive until the next state-change batch.
#[derive(Default)]
struct View {
    id: CString,
    connection: CString,
    owner: Option<(String, String)>,
    max_members: u32,
    access: u32,
    lock: u32,
    migration: u32,
    search: Vec<(CString, CString)>,
    search_keys: Vec<usize>,
    props: Vec<(CString, CString)>,
    prop_keys: Vec<usize>,
    members: Vec<ViewMember>,
    member_keys: Vec<EntityKey>,
}

fn prefixed(attrs: &[Attr], prefix: &str) -> Vec<(CString, CString)> {
    attrs
        .iter()
        .filter_map(|a| a.key.strip_prefix(prefix).map(|k| (cstring(k), cstring(&a.value.text()))))
        .collect()
}

fn keys_of(props: &[(CString, CString)]) -> Vec<usize> {
    props.iter().map(|(k, _)| k.as_ptr() as usize).collect()
}

fn find_prop<'a>(props: &'a [(CString, CString)], key: &str) -> Option<&'a CString> {
    props.iter().find(|(k, _)| k.to_bytes() == key.as_bytes()).map(|(_, v)| v)
}

/// The entity behind a LAN member, once its attributes have arrived.
fn member_entity(m: &Member) -> Option<(String, String)> {
    let id = attr_text(&m.attrs, "pf")?;
    let ty = attr_text(&m.attrs, "pft").unwrap_or_else(|| ENTITY_TYPE.into());
    Some((id, ty))
}

fn num_attr(l: &Lobby, key: &str) -> u32 {
    l.attr(key).and_then(|a| a.value.text().parse().ok()).unwrap_or(0)
}

impl View {
    fn placeholder(id: &str) -> View {
        View {
            id: cstring(id),
            connection: cstring(&format!("{CONNECT_PREFIX}{id}")),
            ..Default::default()
        }
    }

    fn of(l: &Lobby) -> View {
        let members: Vec<ViewMember> = l
            .members
            .iter()
            .filter_map(|m| {
                let (entity, ty) = member_entity(m)?;
                let props = prefixed(&m.attrs, "m:");
                let prop_keys = keys_of(&props);
                Some(ViewMember {
                    peer: m.id,
                    entity,
                    ty,
                    props,
                    prop_keys,
                })
            })
            .collect();
        let member_keys = members.iter().map(|m| key(&m.entity, &m.ty)).collect();
        let owner = members
            .iter()
            .find(|m| m.peer == l.owner)
            .map(|m| (m.entity.clone(), m.ty.clone()));
        let search = prefixed(&l.attrs, "s:");
        let props = prefixed(&l.attrs, "l:");
        View {
            id: cstring(&l.id),
            connection: cstring(&format!("{CONNECT_PREFIX}{}", l.id)),
            owner,
            max_members: l.max_members,
            access: num_attr(l, "acc"),
            lock: num_attr(l, "lock"),
            migration: num_attr(l, "mig"),
            search_keys: keys_of(&search),
            search,
            prop_keys: keys_of(&props),
            props,
            members,
            member_keys,
        }
    }

    fn member(&self, entity: &str) -> Option<&ViewMember> {
        self.members.iter().find(|m| m.entity == entity)
    }
}

/// Keys added, changed or removed between two property lists.
fn changed_keys(old: &[(CString, CString)], new: &[(CString, CString)]) -> Vec<String> {
    let mut out: Vec<String> = new
        .iter()
        .filter(|(k, v)| old.iter().find(|(ok, _)| ok == k).is_none_or(|(_, ov)| ov != v))
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    out.extend(
        old.iter()
            .filter(|(k, _)| !new.iter().any(|(nk, _)| nk == k))
            .map(|(k, _)| k.to_string_lossy().into_owned()),
    );
    out
}

#[derive(Default, Debug, PartialEq)]
struct Diff {
    added: Vec<(String, String)>,
    removed: Vec<(String, String)>,
    owner: bool,
    max_members: bool,
    access: bool,
    lock: bool,
    search: Vec<String>,
    props: Vec<String>,
    /// Members in both snapshots whose properties changed.
    members: Vec<(String, String, Vec<String>)>,
}

impl Diff {
    fn updated(&self) -> bool {
        self.owner
            || self.max_members
            || self.access
            || self.lock
            || !self.search.is_empty()
            || !self.props.is_empty()
            || !self.members.is_empty()
    }
}

fn diff(old: &View, new: &View) -> Diff {
    let mut d = Diff {
        owner: old.owner != new.owner,
        max_members: old.max_members != new.max_members,
        access: old.access != new.access,
        lock: old.lock != new.lock,
        search: changed_keys(&old.search, &new.search),
        props: changed_keys(&old.props, &new.props),
        ..Default::default()
    };
    for m in &new.members {
        match old.member(&m.entity) {
            None => d.added.push((m.entity.clone(), m.ty.clone())),
            Some(o) => {
                let keys = changed_keys(&o.props, &m.props);
                if !keys.is_empty() {
                    d.members.push((m.entity.clone(), m.ty.clone(), keys));
                }
            }
        }
    }
    for o in &old.members {
        if new.member(&o.entity).is_none() {
            d.removed.push((o.entity.clone(), o.ty.clone()));
        }
    }
    d
}

// ---- PeerParty's objects ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum JoinKind {
    Join,
    Arranged,
}

enum Phase {
    Joining {
        kind: JoinKind,
        ctx: usize,
        /// For an arranged lobby: when to stop retrying, and the lobby to create if
        /// nobody has (None while someone else is expected to create it).
        until: Instant,
        create: Option<Lobby>,
        /// Set while the first try waits for the creator.
        first_try: Option<Instant>,
    },
    Joined,
    Gone,
}

struct LobbyObj {
    id: String,
    entity: String,
    ty: String,
    member_attrs: Vec<Attr>,
    phase: Phase,
    view: Box<View>,
    dirty: bool,
}

struct Search {
    entity: (String, String),
    ctx: usize,
    filter: Vec<Clause>,
    sort: Vec<SortKey>,
    count: usize,
    friends: bool,
}

/// What a match's details point into.
struct MatchMem {
    _strings: Strings,
    _members: Vec<MatchMember>,
    _regions: Vec<usize>,
    details: MatchDetails,
}

struct TicketObj {
    id: String,
    id_c: CString,
    ctx: usize,
    status: u32,
    /// Set by the ticket's watcher thread; the next batch tells the game.
    outcome: Option<Poll>,
    done: bool,
    details: Option<Box<MatchMem>>,
}

/// One queued state change and the memory it points into.
struct Change {
    ptr: usize,
    _c: Box<dyn Any + Send>,
    _keep: Vec<Box<dyn Any + Send>>,
}

#[derive(Default)]
struct Batches {
    queue: Vec<Change>,
    in_flight: Vec<Change>,
    in_flight_ptrs: Vec<usize>,
    /// Handles to forget once the game has seen the changes ending them.
    forget: Vec<usize>,
    forget_in_flight: Vec<usize>,
}

impl Batches {
    fn push<T: Any + Send>(&mut self, c: T, keep: Vec<Box<dyn Any + Send>>) {
        let b = Box::new(c);
        let ptr = &*b as *const T as usize;
        self.queue.push(Change {
            ptr,
            _c: b,
            _keep: keep,
        });
    }

    fn start(&mut self) -> (u32, usize) {
        let batch = std::mem::take(&mut self.queue);
        self.forget_in_flight = std::mem::take(&mut self.forget);
        self.in_flight_ptrs = batch.iter().map(|c| c.ptr).collect();
        self.in_flight = batch;
        (self.in_flight_ptrs.len() as u32, ptr_of(&self.in_flight_ptrs))
    }

    fn finish(&mut self) -> Vec<usize> {
        self.in_flight.clear();
        self.in_flight_ptrs.clear();
        std::mem::take(&mut self.forget_in_flight)
    }

    fn kinds(&self) -> Vec<u32> {
        self.in_flight.iter().map(|c| unsafe { *(c.ptr as *const u32) }).collect()
    }
}

#[derive(Default)]
struct State {
    initialized: bool,
    lobbies: HashMap<usize, LobbyObj>,
    tickets: HashMap<usize, TicketObj>,
    contexts: HashMap<usize, usize>,
    searches: HashMap<u64, Search>,
    listening: HashSet<(String, String)>,
    lobby_changes: Batches,
    ticket_changes: Batches,
    /// Snapshots replaced since the last batch began.
    retired: Vec<Box<View>>,
}

// Only plain data and handles; raw pointers are kept as usize.
unsafe impl Send for State {}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x504D_0000);
static LOADED: AtomicBool = AtomicBool::new(false);
const MP_HANDLE: usize = 0x504D_5048;

fn handle() -> usize {
    NEXT_HANDLE.fetch_add(0x10, Ordering::Relaxed)
}

/// Whether the game has a PlayFab Multiplayer library loaded (and PeerParty took it over).
pub fn loaded() -> bool {
    LOADED.load(Ordering::Relaxed)
}

pub fn attach(base: usize, module_name: &str, exports: &[Export]) -> HashSet<usize> {
    crate::swap::register(base, module_name, resolve);
    net::subscribe(Front::PfLobby);
    LOADED.store(true, Ordering::Relaxed);
    let ours = exports.iter().filter(|e| e.name.starts_with("PF")).count();
    log!("pfmp-lan on for {module_name}: {ours} PlayFab Lobby & Matchmaking functions are PeerParty's");
    HashSet::new()
}

/// The connection string of a lobby we're in, for "invite everyone".
///
/// `net::take` asks while delivering an invite, and `pump` calls it holding STATE: then
/// the copy `pump` took just before answers (locking again would hang the game).
pub fn current_connect() -> Option<String> {
    match STATE.try_lock() {
        Ok(s) => joined_connect(&s),
        Err(_) => JOINED.lock().unwrap().clone(),
    }
}

/// The connection string `current_connect` answers with while STATE is held.
static JOINED: Mutex<Option<String>> = Mutex::new(None);

fn joined_connect(s: &State) -> Option<String> {
    let c = s
        .lobbies
        .values()
        .find(|l| matches!(l.phase, Phase::Joined))
        .map(|l| format!("{CONNECT_PREFIX}{}", l.id));
    *JOINED.lock().unwrap() = c.clone();
    c
}

fn trace(name: &str, detail: impl FnOnce() -> String) {
    crate::log::throttled(name, 4, |n| log!("pfmp-lan call {name} n={n} {}", detail()));
}

// ---- lobby events ----

impl State {
    fn lobby_of(&self, id: &str) -> Option<usize> {
        self.lobbies
            .iter()
            .find(|(_, l)| l.id == id && !matches!(l.phase, Phase::Gone))
            .map(|(h, _)| *h)
    }

    fn set_view(&mut self, h: usize, view: View) {
        if let Some(l) = self.lobbies.get_mut(&h) {
            let old = std::mem::replace(&mut l.view, Box::new(view));
            self.retired.push(old);
        }
    }

    /// We're in: our attributes go up, the game hears MemberAdded for us and then the
    /// completion.
    fn joined(&mut self, lan: &Lan, h: usize, kind: JoinKind, ctx: usize) {
        let Some(l) = self.lobbies.get_mut(&h) else { return };
        l.phase = Phase::Joined;
        let _ = lan.update(&l.id, None, &l.member_attrs, &[]);
        let (id, entity, ty) = (l.id.clone(), l.entity.clone(), l.ty.clone());
        if let Some(lobby) = lan.lobby(&id) {
            self.set_view(h, View::of(&lobby));
        }
        let me = key(&entity, &ty);
        self.lobby_changes.push(
            MemberAdded {
                ty: MEMBER_ADDED,
                lobby: h,
                member: me,
            },
            Vec::new(),
        );
        self.lobby_changes.push(
            JoinCompleted {
                ty: if kind == JoinKind::Arranged { JOIN_ARRANGED_COMPLETED } else { JOIN_COMPLETED },
                result: S_OK,
                member: me,
                ctx,
                lobby: h,
            },
            Vec::new(),
        );
        log!("pfmp-lan joined lobby {id} as {entity}");
    }

    fn join_failed(&mut self, h: usize, kind: JoinKind, ctx: usize, result: i32) {
        let Some(l) = self.lobbies.get_mut(&h) else { return };
        l.phase = Phase::Gone;
        let me = key(&l.entity, &l.ty);
        log!("pfmp-lan could not join lobby {}: {:#x}", l.id, result);
        self.lobby_changes.push(
            JoinCompleted {
                ty: if kind == JoinKind::Arranged { JOIN_ARRANGED_COMPLETED } else { JOIN_COMPLETED },
                result,
                member: me,
                ctx,
                lobby: h,
            },
            Vec::new(),
        );
        self.lobby_changes.forget.push(h);
    }

    /// We're out of a lobby we were in. `removed` is why, when it's about us (we left,
    /// or were removed); None when the lobby itself went away.
    fn gone(&mut self, h: usize, removed: Option<u32>, leave_ctx: Option<(usize, usize)>) {
        let Some(l) = self.lobbies.get_mut(&h) else { return };
        let was_in = matches!(l.phase, Phase::Joined);
        l.phase = Phase::Gone;
        let me = key(&l.entity, &l.ty);
        log!("pfmp-lan out of lobby {} ({removed:?})", l.id);
        if let (true, Some(reason)) = (was_in, removed) {
            self.lobby_changes.push(
                MemberRemoved {
                    ty: MEMBER_REMOVED,
                    lobby: h,
                    member: me,
                    reason,
                },
                Vec::new(),
            );
        }
        if let Some((user, ctx)) = leave_ctx {
            self.lobby_changes.push(
                LeaveCompleted {
                    ty: LEAVE_COMPLETED,
                    lobby: h,
                    local_user: user,
                    ctx,
                },
                Vec::new(),
            );
        }
        let why = if removed == Some(LOCAL_LEFT) { NO_LOCAL_MEMBERS } else { LOBBY_DELETED };
        self.lobby_changes.push(
            Disconnecting {
                ty: DISCONNECTING,
                lobby: h,
                reason: why,
            },
            Vec::new(),
        );
        self.lobby_changes.push(
            Disconnected {
                ty: DISCONNECTED,
                lobby: h,
            },
            Vec::new(),
        );
        self.lobby_changes.forget.push(h);
    }

    /// Brings the game's snapshot of a lobby in line with the LAN's.
    fn reconcile(&mut self, lan: &Lan, h: usize) {
        let Some(l) = self.lobbies.get_mut(&h) else { return };
        l.dirty = false;
        if !matches!(l.phase, Phase::Joined) {
            return;
        }
        let Some(lobby) = lan.lobby(&l.id) else { return };
        let me = l.entity.clone();
        let new = View::of(&lobby);
        let d = diff(&l.view, &new);
        for (entity, ty) in d.added.iter().filter(|(e, _)| *e != me) {
            log!("pfmp-lan {entity} joined lobby {}", l.id);
            self.lobby_changes.push(
                MemberAdded {
                    ty: MEMBER_ADDED,
                    lobby: h,
                    member: key(entity, ty),
                },
                Vec::new(),
            );
        }
        let id = self.lobbies[&h].id.clone();
        for (entity, ty) in d.removed.iter().filter(|(e, _)| *e != me) {
            log!("pfmp-lan {entity} left lobby {id}");
            self.lobby_changes.push(
                MemberRemoved {
                    ty: MEMBER_REMOVED,
                    lobby: h,
                    member: key(entity, ty),
                    reason: REMOTE_LEFT,
                },
                Vec::new(),
            );
        }
        if d.updated() {
            let mut text = Strings::default();
            let search = text.array(&d.search);
            let props = text.array(&d.props);
            let member_keys: Vec<Vec<usize>> = d.members.iter().map(|(_, _, k)| text.array(k)).collect();
            let summaries: Vec<MemberUpdateSummary> = d
                .members
                .iter()
                .zip(&member_keys)
                .map(|((e, t, _), k)| MemberUpdateSummary {
                    member: key(e, t),
                    connection_updated: false,
                    count: k.len() as u32,
                    keys: ptr_of(k),
                })
                .collect();
            let c = Updated {
                ty: UPDATED,
                lobby: h,
                owner: d.owner,
                max_members: d.max_members,
                access: d.access,
                lock: d.lock,
                search_count: search.len() as u32,
                search_keys: ptr_of(&search),
                lobby_count: props.len() as u32,
                lobby_keys: ptr_of(&props),
                member_count: summaries.len() as u32,
                members: ptr_of(&summaries),
                server: false,
                server_count: 0,
                server_keys: 0,
                server_connection: false,
            };
            trace("lobby updated", || format!("{id} {d:?}"));
            self.lobby_changes.push(
                c,
                vec![
                    Box::new(text.owned),
                    Box::new(search),
                    Box::new(props),
                    Box::new(member_keys),
                    Box::new(summaries),
                ],
            );
        }
        self.set_view(h, new);
    }
}

fn pump(s: &mut State) {
    let Some(lan) = net::lan() else { return };
    joined_connect(s);
    for ev in net::take(Front::PfLobby) {
        match ev {
            Event::JoinDone { lobby, result } => on_join_done(s, lan, &lobby, result),
            Event::SearchDone { search, results } => on_search_done(s, lan, search, results),
            Event::MemberStatus { lobby, member, status } => {
                let Some(h) = s.lobby_of(&lobby) else { continue };
                if member == lan.me() && matches!(status, MemberStatus::Closed | MemberStatus::Kicked) {
                    let removed = (status == MemberStatus::Kicked).then_some(LOCAL_REMOVED);
                    if matches!(s.lobbies[&h].phase, Phase::Joined) {
                        s.gone(h, removed, None);
                    }
                } else {
                    s.lobbies.get_mut(&h).unwrap().dirty = true;
                }
            }
            Event::LobbyUpdated { lobby } | Event::MemberUpdated { lobby, .. } => {
                if let Some(h) = s.lobby_of(&lobby) {
                    s.lobbies.get_mut(&h).unwrap().dirty = true;
                }
            }
            Event::PeerLeft(_) => {
                for l in s.lobbies.values_mut() {
                    l.dirty = true;
                }
            }
            Event::Invite { from, connect, .. } if connect.starts_with(CONNECT_PREFIX) => {
                on_invite(s, from, &connect);
            }
            _ => {}
        }
    }
    let now = Instant::now();
    for l in s.lobbies.values_mut() {
        if let Phase::Joining { first_try, .. } = &mut l.phase {
            if first_try.is_some_and(|at| at <= now) {
                *first_try = None;
                let _ = lan.join_by_id(NS, &l.id);
            }
        }
    }
    let dirty: Vec<usize> = s.lobbies.iter().filter(|(_, l)| l.dirty).map(|(h, _)| *h).collect();
    for h in dirty {
        s.reconcile(lan, h);
    }
}

fn on_join_done(s: &mut State, lan: &Lan, lobby: &str, result: JoinResult) {
    let Some(h) = s
        .lobbies
        .iter()
        .find(|(_, l)| l.id == lobby && matches!(l.phase, Phase::Joining { .. }))
        .map(|(h, _)| *h)
    else {
        return;
    };
    let Phase::Joining { kind, ctx, until, create, .. } = &mut s.lobbies.get_mut(&h).unwrap().phase else {
        return;
    };
    let (kind, ctx, until) = (*kind, *ctx, *until);
    if result == JoinResult::Ok {
        s.joined(lan, h, kind, ctx);
        return;
    }
    let missing = matches!(result, JoinResult::TimedOut | JoinResult::NotFound);
    if kind == JoinKind::Arranged && missing {
        if let Some(lobby) = create.take() {
            // Nobody made the arranged lobby: we do.
            match lan.create_lobby(lobby) {
                Ok(_) => s.joined(lan, h, kind, ctx),
                Err(e) => {
                    log!("pfmp-lan could not create arranged lobby: {e:?}");
                    s.join_failed(h, kind, ctx, E_FAIL);
                }
            }
            return;
        }
        if Instant::now() < until {
            let _ = lan.join_by_id(NS, lobby);
            return;
        }
    }
    let code = match result {
        JoinResult::Full => E_LOBBY_FULL,
        JoinResult::NotAllowed => E_ACCESSDENIED,
        _ => E_LOBBY_NOT_FOUND,
    };
    s.join_failed(h, kind, ctx, code);
}

fn on_invite(s: &mut State, from: PeerId, connect: &str) {
    let listener = s.listening.iter().next().cloned();
    let Some((lid, lty)) = listener else {
        log!("pfmp-lan invite to {connect} arrived, but the game isn't listening for lobby invites");
        return;
    };
    let mut text = Strings::default();
    let connection = text.add(connect);
    let lobby_id = text.add(connect.strip_prefix(CONNECT_PREFIX).unwrap_or(connect));
    log!("pfmp-lan invite from {} to {connect}", api::entity_of(from));
    s.lobby_changes.push(
        InviteReceived {
            ty: INVITE_RECEIVED,
            listener: key(&lid, &lty),
            inviter: key(&api::entity_of(from), ENTITY_TYPE),
            connection,
            lobby_id,
        },
        vec![Box::new(text.owned)],
    );
}

// ---- searching ----

#[derive(Clone, Debug, PartialEq)]
enum Operand {
    Num(f64),
    Text(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CmpOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Clone, Debug, PartialEq)]
struct Clause {
    key: String,
    op: CmpOp,
    value: Operand,
}

#[derive(Clone, Debug, PartialEq)]
struct SortKey {
    key: String,
    /// `distance{key=value}`: closest to `value` first.
    near: Option<f64>,
    descending: bool,
}

fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || c == '(' || c == ')' {
            chars.next();
        } else if c == '\'' {
            chars.next();
            let mut t = String::from("'");
            while let Some(c) = chars.next() {
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                        t.push('\'');
                    } else {
                        break;
                    }
                } else {
                    t.push(c);
                }
            }
            out.push(t);
        } else {
            let mut t = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '(' || c == ')' {
                    break;
                }
                t.push(c);
                chars.next();
            }
            out.push(t);
        }
    }
    out
}

/// PlayFab's lobby filter: `key op value` clauses joined by `and`.
fn parse_filter(s: &str) -> Vec<Clause> {
    let t = tokens(s);
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < t.len() {
        let op = match t[i + 1].to_ascii_lowercase().as_str() {
            "eq" => CmpOp::Eq,
            "ne" => CmpOp::Ne,
            "gt" => CmpOp::Gt,
            "ge" => CmpOp::Ge,
            "lt" => CmpOp::Lt,
            "le" => CmpOp::Le,
            _ => {
                i += 1;
                continue;
            }
        };
        let raw = &t[i + 2];
        let value = match raw.strip_prefix('\'') {
            Some(text) => Operand::Text(text.to_string()),
            None => match raw.parse::<f64>() {
                Ok(n) => Operand::Num(n),
                Err(_) => Operand::Text(raw.to_ascii_lowercase()),
            },
        };
        out.push(Clause {
            key: t[i].clone(),
            op,
            value,
        });
        i += 3;
        if t.get(i).is_some_and(|w| w.eq_ignore_ascii_case("and")) {
            i += 1;
        }
    }
    out
}

fn parse_sort(s: &str) -> Vec<SortKey> {
    s.split(',')
        .filter_map(|part| {
            let mut words = part.split_whitespace();
            let first = words.next()?;
            let descending = words.next().is_some_and(|w| w.eq_ignore_ascii_case("desc"));
            if let Some(inner) = first.strip_prefix("distance{").and_then(|r| r.strip_suffix('}')) {
                let (k, v) = inner.split_once('=')?;
                return Some(SortKey {
                    key: k.trim().to_string(),
                    near: v.trim().parse().ok(),
                    descending,
                });
            }
            Some(SortKey {
                key: first.to_string(),
                near: None,
                descending,
            })
        })
        .collect()
}

/// A search key's value in a lobby, as `me` sees it.
fn search_value(l: &Lobby, me: PeerId, key: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    Some(match lower.as_str() {
        "lobby/membercount" => l.members.len().to_string(),
        "lobby/membercountremaining" => l.open_slots().to_string(),
        "lobby/ammember" => l.member(me).is_some().to_string(),
        "lobby/amowner" => (l.owner == me).to_string(),
        "lobby/membershiplock" => {
            if num_attr(l, "lock") == 1 { "Locked" } else { "Unlocked" }.to_string()
        }
        _ => l.attr(&format!("s:{key}"))?.value.text(),
    })
}

fn clause_holds(l: &Lobby, me: PeerId, c: &Clause) -> bool {
    use std::cmp::Ordering;
    let Some(have) = search_value(l, me, &c.key) else {
        return c.op == CmpOp::Ne;
    };
    let ord = match &c.value {
        Operand::Num(n) => match have.parse::<f64>() {
            Ok(h) => h.partial_cmp(n),
            Err(_) => return c.op == CmpOp::Ne,
        },
        Operand::Text(t) if have == "true" || have == "false" => Some(have.as_str().cmp(&t.to_ascii_lowercase())),
        Operand::Text(t) => Some(have.as_str().cmp(t.as_str())),
    };
    let Some(ord) = ord else { return false };
    match c.op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Ge => ord != Ordering::Less,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Le => ord != Ordering::Greater,
    }
}

fn sort_lobbies(lobbies: &mut [Lobby], me: PeerId, keys: &[SortKey]) {
    lobbies.sort_by(|a, b| {
        for k in keys {
            let (va, vb) = (search_value(a, me, &k.key), search_value(b, me, &k.key));
            let ord = match (va.as_deref().map(str::parse::<f64>), vb.as_deref().map(str::parse::<f64>)) {
                (Some(Ok(x)), Some(Ok(y))) => match k.near {
                    Some(n) => (x - n).abs().total_cmp(&(y - n).abs()),
                    None => x.total_cmp(&y),
                },
                _ => va.cmp(&vb),
            };
            let ord = if k.descending { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// A lobby as PlayFab's REST `FindLobbies` lists it (playfab/api.rs).
pub(crate) struct Summary {
    pub id: String,
    pub connection: String,
    pub owner: Option<(String, String)>,
    pub max_members: u32,
    pub members: u32,
    pub locked: bool,
    pub search: Vec<(String, String)>,
    /// Lobby data and members (entity, type, member data), for `GetLobby`.
    pub props: Vec<(String, String)>,
    pub member_list: Vec<(String, String, Vec<(String, String)>)>,
}

fn texts(v: &[(CString, CString)]) -> Vec<(String, String)> {
    v.iter().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect()
}

fn summary(l: &Lobby) -> Summary {
    let view = View::of(l);
    Summary {
        id: l.id.clone(),
        connection: format!("{CONNECT_PREFIX}{}", l.id),
        owner: view.owner.clone(),
        max_members: l.max_members,
        members: l.members.len() as u32,
        locked: view.lock == 1,
        search: texts(&view.search),
        props: texts(&view.props),
        member_list: view.members.iter().map(|m| (m.entity.clone(), m.ty.clone(), texts(&m.props))).collect(),
    }
}

/// The REST API's `GetLobby`: a lobby this copy is in, or None.
pub(crate) fn rest_lobby(id: &str) -> Option<Summary> {
    net::lan()?.lobby(id).filter(|l| l.namespace == NS).map(|l| summary(&l))
}

/// Lobbies found on the LAN, filtered, sorted and cut to `count` the way
/// `PFMultiplayerFindLobbies` does, for the REST API's `FindLobbies`.
pub(crate) fn summaries(results: Vec<Lobby>, me: PeerId, filter: &str, sort: &str, count: usize) -> Vec<Summary> {
    let filter = parse_filter(filter);
    let mut lobbies: Vec<Lobby> = results.into_iter().filter(|l| filter.iter().all(|c| clause_holds(l, me, c))).collect();
    sort_lobbies(&mut lobbies, me, &parse_sort(sort));
    lobbies.truncate(count);
    lobbies.iter().map(summary).collect()
}

/// Search results and everything they point into.
struct Found {
    _text: Strings,
    _arrays: Vec<Vec<usize>>,
    _friends: Vec<Vec<EntityKey>>,
    _results: Vec<SearchResult>,
}

fn on_search_done(s: &mut State, lan: &Lan, id: u64, results: Vec<Lobby>) {
    let Some(search) = s.searches.remove(&id) else { return };
    let me = lan.me();
    let mut lobbies: Vec<Lobby> = results
        .into_iter()
        .filter(|l| search.filter.iter().all(|c| clause_holds(l, me, c)))
        .collect();
    sort_lobbies(&mut lobbies, me, &search.sort);
    lobbies.truncate(search.count);
    let mut text = Strings::default();
    let mut arrays = Vec::new();
    let mut friends_all = Vec::new();
    let mut out = Vec::new();
    for l in &lobbies {
        let view = View::of(l);
        let skeys: Vec<String> = view.search.iter().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
        let svals: Vec<String> = view.search.iter().map(|(_, v)| v.to_string_lossy().into_owned()).collect();
        let keys = text.array(&skeys);
        let vals = text.array(&svals);
        let friends: Vec<EntityKey> = if search.friends {
            view.members.iter().filter(|m| m.peer != me).map(|m| key(&m.entity, &m.ty)).collect()
        } else {
            Vec::new()
        };
        out.push(SearchResult {
            lobby_id: text.add(&l.id),
            connection: text.add(&format!("{CONNECT_PREFIX}{}", l.id)),
            owner: view.owner.as_ref().map_or(0, |(e, t)| key_ref(e, t) as *const EntityKey as usize),
            max_members: l.max_members,
            members: l.members.len() as u32,
            search_count: keys.len() as u32,
            search_keys: ptr_of(&keys),
            search_values: ptr_of(&vals),
            friend_count: friends.len() as u32,
            friends: ptr_of(&friends),
            lock: view.lock,
        });
        arrays.push(keys);
        arrays.push(vals);
        friends_all.push(friends);
    }
    log!("pfmp-lan search found {} lobbies", out.len());
    let c = FindCompleted {
        ty: FIND_LOBBIES_COMPLETED,
        result: S_OK,
        searcher: key(&search.entity.0, &search.entity.1),
        ctx: search.ctx,
        count: out.len() as u32,
        results: ptr_of(&out),
    };
    s.lobby_changes.push(
        c,
        vec![Box::new(Found {
            _text: text,
            _arrays: arrays,
            _friends: friends_all,
            _results: out,
        })],
    );
}

// ---- matchmaking ----

/// Polls a ticket on its own thread (a poll waits on a LAN search).
fn watch_ticket(h: usize, id: String) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(750));
            let active = STATE
                .lock()
                .unwrap()
                .tickets
                .get(&h)
                .is_some_and(|t| !t.done && t.outcome.is_none());
            if !active {
                return;
            }
            let poll = api::poll_ticket(&id).unwrap_or(Poll::Ended("Lost"));
            if matches!(poll, Poll::Waiting) {
                continue;
            }
            if let Some(t) = STATE.lock().unwrap().tickets.get_mut(&h) {
                if t.outcome.is_none() {
                    t.outcome = Some(poll);
                }
            }
            return;
        }
    });
}

fn match_mem(match_id: &str, members: &[(String, String)]) -> Box<MatchMem> {
    let mut text = Strings::default();
    let creator = members.first().map(|(e, _)| e.clone()).unwrap_or_default();
    let mm: Vec<MatchMember> = members
        .iter()
        .map(|(e, data)| MatchMember {
            entity: key(e, ENTITY_TYPE),
            team: text.add(""),
            attributes: text.add(data),
        })
        .collect();
    let regions = vec![text.add("LAN")];
    let details = MatchDetails {
        match_id: text.add(match_id),
        members: ptr_of(&mm),
        member_count: mm.len() as u32,
        regions: ptr_of(&regions),
        region_count: regions.len() as u32,
        arrangement: text.add(&format!("{ARRANGED_PREFIX}{creator}-{match_id}")),
        server: 0,
    };
    Box::new(MatchMem {
        _strings: text,
        _members: mm,
        _regions: regions,
        details,
    })
}

fn pump_tickets(s: &mut State) {
    let hs: Vec<usize> = s.tickets.keys().copied().collect();
    for h in hs {
        let t = s.tickets.get_mut(&h).unwrap();
        if t.done {
            continue;
        }
        if t.status == TICKET_CREATING {
            t.status = TICKET_WAITING;
            s.ticket_changes.push(
                TicketChanged {
                    ty: TICKET_STATUS_CHANGED,
                    ticket: h,
                },
                Vec::new(),
            );
            continue;
        }
        let Some(outcome) = t.outcome.take() else { continue };
        let result = match &outcome {
            Poll::Matched(match_id, members) => {
                log!("pfmp-lan ticket {} matched: {match_id} ({} members)", t.id, members.len());
                t.details = Some(match_mem(match_id, members));
                t.status = TICKET_MATCHED;
                S_OK
            }
            Poll::Ended(reason) => {
                log!("pfmp-lan ticket {} ended: {reason}", t.id);
                if *reason == "Requested" {
                    t.status = TICKET_CANCELED;
                    E_ABORT
                } else {
                    t.status = TICKET_FAILED;
                    E_TIMEOUT
                }
            }
            Poll::Waiting => continue,
        };
        t.done = true;
        let ctx = t.ctx;
        s.ticket_changes.push(
            TicketChanged {
                ty: TICKET_STATUS_CHANGED,
                ticket: h,
            },
            Vec::new(),
        );
        s.ticket_changes.push(
            TicketCompleted {
                ty: TICKET_COMPLETED,
                result,
                ticket: h,
                ctx,
            },
            Vec::new(),
        );
    }
}

// ---- the exports ----

fn resolve(name: &str) -> Option<usize> {
    if !name.starts_with("PF") {
        return None;
    }
    let f: usize = match name {
        "PFMultiplayerInitialize" => mp_initialize as *const () as usize,
        "PFMultiplayerUninitialize" => mp_uninitialize as *const () as usize,
        "PFMultiplayerGetErrorMessage" => mp_error_message as *const () as usize,
        "PFMultiplayerSetEntityToken" => mp_set_entity_token as *const () as usize,
        "PFMultiplayerSetMemoryCallbacks" => ok2 as *const () as usize,
        "PFMultiplayerSetThreadAffinityMask" => ok2 as *const () as usize,
        "PFMultiplayerStartProcessingLobbyStateChanges" => mp_start_lobby as *const () as usize,
        "PFMultiplayerFinishProcessingLobbyStateChanges" => mp_finish_lobby as *const () as usize,
        "PFMultiplayerStartProcessingMatchmakingStateChanges" => mp_start_tickets as *const () as usize,
        "PFMultiplayerFinishProcessingMatchmakingStateChanges" => mp_finish_tickets as *const () as usize,
        "PFMultiplayerCreateAndJoinLobby" => mp_create_and_join as *const () as usize,
        "PFMultiplayerJoinLobby" => mp_join as *const () as usize,
        "PFMultiplayerJoinArrangedLobby" => mp_join_arranged as *const () as usize,
        "PFMultiplayerFindLobbies" => mp_find_lobbies as *const () as usize,
        "PFMultiplayerStartListeningForLobbyInvites" => mp_start_listening as *const () as usize,
        "PFMultiplayerStopListeningForLobbyInvites" => mp_stop_listening as *const () as usize,
        "PFMultiplayerGetLobbyInviteListenerStatus" => mp_listener_status as *const () as usize,
        "PFMultiplayerCreateMatchmakingTicket" => mp_create_ticket as *const () as usize,
        "PFMultiplayerDestroyMatchmakingTicket" => mp_destroy_ticket as *const () as usize,
        "PFMatchmakingTicketGetStatus" => ticket_status as *const () as usize,
        "PFMatchmakingTicketCancel" => ticket_cancel as *const () as usize,
        "PFMatchmakingTicketGetTicketId" => ticket_id as *const () as usize,
        "PFMatchmakingTicketGetMatch" => ticket_match as *const () as usize,
        "PFLobbyGetLobbyId" => lobby_id as *const () as usize,
        "PFLobbyGetConnectionString" => lobby_connection as *const () as usize,
        "PFLobbyGetMaxMemberCount" => lobby_max_members as *const () as usize,
        "PFLobbyGetOwner" => lobby_owner as *const () as usize,
        "PFLobbyGetOwnerMigrationPolicy" => lobby_migration as *const () as usize,
        "PFLobbyGetAccessPolicy" => lobby_access as *const () as usize,
        "PFLobbyGetMembershipLock" => lobby_lock as *const () as usize,
        "PFLobbyGetMembers" => lobby_members as *const () as usize,
        "PFLobbyGetSearchPropertyKeys" => lobby_search_keys as *const () as usize,
        "PFLobbyGetSearchProperty" => lobby_search_property as *const () as usize,
        "PFLobbyGetLobbyPropertyKeys" => lobby_property_keys as *const () as usize,
        "PFLobbyGetLobbyProperty" => lobby_property as *const () as usize,
        "PFLobbyGetMemberPropertyKeys" => lobby_member_keys as *const () as usize,
        "PFLobbyGetMemberProperty" => lobby_member_property as *const () as usize,
        "PFLobbyGetMemberConnectionStatus" => lobby_member_connection as *const () as usize,
        "PFLobbyGetServer" => null_out as *const () as usize,
        "PFLobbyGetServerPropertyKeys" => no_items as *const () as usize,
        "PFLobbyGetServerProperty" => no_property as *const () as usize,
        "PFLobbyGetServerConnectionStatus" => zero_out as *const () as usize,
        "PFLobbyLeave" => lobby_leave as *const () as usize,
        "PFLobbyPostUpdate" => lobby_post_update as *const () as usize,
        "PFLobbyForceRemoveMember" => lobby_force_remove as *const () as usize,
        "PFLobbySendInvite" => lobby_send_invite as *const () as usize,
        n if n.ends_with("GetCustomContext") => get_custom_context as *const () as usize,
        n if n.ends_with("SetCustomContext") => set_custom_context as *const () as usize,
        _ => {
            note(name);
            unsupported as *const () as usize
        }
    };
    Some(f)
}

static NOTED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

fn note(name: &str) {
    if NOTED.lock().unwrap().insert(name.to_string()) {
        crate::log::gap("pfmp", &format!("{name} (not carried on the LAN, reports E_NOTIMPL)"));
    }
}

extern "system" fn unsupported() -> i32 {
    E_NOTIMPL
}

extern "system" fn ok2(_: usize, _: usize) -> i32 {
    S_OK
}

unsafe extern "system" fn null_out(_: usize, out: *mut usize) -> i32 {
    unsafe { put(out, 0) };
    S_OK
}

unsafe extern "system" fn zero_out(_: usize, out: *mut u32) -> i32 {
    unsafe { put(out, 0) };
    S_OK
}

unsafe extern "system" fn no_items(_: usize, count: *mut u32, items: *mut usize) -> i32 {
    unsafe {
        put(count, 0);
        put(items, 0);
    }
    S_OK
}

unsafe extern "system" fn no_property(_: usize, _: *const c_char, value: *mut usize) -> i32 {
    unsafe { put(value, 0) };
    S_OK
}

unsafe extern "system" fn get_custom_context(obj: usize, out: *mut usize) -> i32 {
    let v = STATE.lock().unwrap().contexts.get(&obj).copied().unwrap_or(0);
    unsafe { put(out, v) };
    S_OK
}

extern "system" fn set_custom_context(obj: usize, value: usize) -> i32 {
    STATE.lock().unwrap().contexts.insert(obj, value);
    S_OK
}

unsafe extern "system" fn mp_initialize(title: *const c_char, out: *mut usize) -> i32 {
    let title = unsafe { text(title) };
    {
        let mut s = STATE.lock().unwrap();
        s.initialized = true;
    }
    net::subscribe(Front::PfLobby);
    unsafe { put(out, MP_HANDLE) };
    let lan = net::lan().is_some();
    log!("pfmp-lan PFMultiplayerInitialize title={title} lan={lan}");
    S_OK
}

extern "system" fn mp_uninitialize(_: usize) -> i32 {
    let mut s = STATE.lock().unwrap();
    if let Some(lan) = net::lan() {
        for l in s.lobbies.values() {
            if !matches!(l.phase, Phase::Gone) {
                let _ = lan.leave(&l.id);
            }
        }
    }
    for t in s.tickets.values().filter(|t| !t.done) {
        api::cancel_ticket(&t.id);
    }
    *s = State::default();
    log!("pfmp-lan PFMultiplayerUninitialize");
    S_OK
}

extern "system" fn mp_error_message(err: i32) -> *const c_char {
    let msg: &'static std::ffi::CStr = match err {
        S_OK => c"Success",
        E_NOTIMPL => c"Not carried on PeerParty's LAN (game-server lobbies and party tickets)",
        E_ABORT => c"The matchmaking ticket was canceled",
        E_TIMEOUT => c"Matchmaking timed out: no other player on the LAN matched",
        E_ACCESSDENIED => c"Only the lobby's owner can do that",
        E_LOBBY_NOT_FOUND => c"The lobby wasn't found on the LAN",
        E_LOBBY_FULL => c"The lobby is full",
        E_INVALIDARG => c"Invalid argument",
        _ => c"The operation failed",
    };
    msg.as_ptr()
}

unsafe extern "system" fn mp_set_entity_token(_: usize, entity: *const EntityKey, _token: *const c_char) -> i32 {
    let who = unsafe { entity_in(entity) };
    trace("PFMultiplayerSetEntityToken", || format!("{who:?}"));
    S_OK
}

static TICKS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn mp_start_lobby(_: usize, count: *mut u32, changes: *mut usize) -> i32 {
    let _ = std::panic::catch_unwind(|| {
        if net::lan().is_some() && TICKS.fetch_add(1, Ordering::Relaxed) % 256 == 0 {
            crate::swap::patch_imports();
        }
    });
    let mut s = STATE.lock().unwrap();
    s.retired.clear();
    pump(&mut s);
    let (n, p) = s.lobby_changes.start();
    if n > 0 {
        let kinds = s.lobby_changes.kinds();
        trace("lobby state changes", || format!("{kinds:?}"));
    }
    unsafe {
        put(count, n);
        put(changes, p);
    }
    S_OK
}

extern "system" fn mp_finish_lobby(_: usize, _: u32, _: usize) -> i32 {
    let mut s = STATE.lock().unwrap();
    for h in s.lobby_changes.finish() {
        if let Some(l) = s.lobbies.remove(&h) {
            s.retired.push(l.view);
        }
        s.contexts.remove(&h);
    }
    S_OK
}

unsafe extern "system" fn mp_start_tickets(_: usize, count: *mut u32, changes: *mut usize) -> i32 {
    let mut s = STATE.lock().unwrap();
    pump_tickets(&mut s);
    let (n, p) = s.ticket_changes.start();
    if n > 0 {
        let kinds = s.ticket_changes.kinds();
        trace("matchmaking state changes", || format!("{kinds:?}"));
    }
    unsafe {
        put(count, n);
        put(changes, p);
    }
    S_OK
}

extern "system" fn mp_finish_tickets(_: usize, _: u32, _: usize) -> i32 {
    let mut s = STATE.lock().unwrap();
    for h in s.ticket_changes.finish() {
        s.tickets.remove(&h);
        s.contexts.remove(&h);
    }
    S_OK
}

/// Our member attributes: our entity, then the game's member properties.
fn member_attrs(entity: &(String, String), props: &[(String, Option<String>)]) -> Vec<Attr> {
    let mut out = vec![
        str_attr("pf".into(), &entity.0, true),
        str_attr("pft".into(), &entity.1, true),
    ];
    for (k, v) in props {
        if let Some(v) = v {
            out.push(str_attr(format!("m:{k}"), v, false));
        }
    }
    out
}

/// A new LAN lobby holding a PlayFab lobby's settings, with us as its only member.
fn new_lobby(
    id: &str,
    lan: &Lan,
    max: u32,
    migration: u32,
    access: u32,
    search: &[(String, Option<String>)],
    props: &[(String, Option<String>)],
    mine: Vec<Attr>,
) -> Lobby {
    let mut l = Lobby::new(NS);
    l.id = id.to_string();
    l.max_members = max.clamp(2, 128);
    l.permission = if access == ACCESS_PRIVATE { Permission::InviteOnly } else { Permission::Public };
    l.host_migration = true;
    l.join_by_id = true;
    l.attrs = vec![
        str_attr("acc".into(), &access.to_string(), true),
        str_attr("mig".into(), &migration.to_string(), true),
        str_attr("lock".into(), "0", true),
    ];
    for (k, v) in search {
        if let Some(v) = v {
            l.attrs.push(str_attr(format!("s:{k}"), v, true));
        }
    }
    for (k, v) in props {
        if let Some(v) = v {
            l.attrs.push(str_attr(format!("l:{k}"), v, false));
        }
    }
    l.members = vec![Member { id: lan.me(), attrs: mine }];
    l
}

unsafe extern "system" fn mp_create_and_join(
    _: usize,
    creator: *const EntityKey,
    create: *const CreateConfig,
    join: *const JoinConfig,
    ctx: usize,
    out: *mut usize,
) -> i32 {
    let Some(lan) = net::lan() else { return E_FAIL };
    let Some(entity) = (unsafe { entity_in(creator) }) else { return E_INVALIDARG };
    if create.is_null() {
        return E_INVALIDARG;
    }
    let c = unsafe { &*create };
    let search = unsafe { pairs(c.search_count, c.search_keys, c.search_values) };
    let props = unsafe { pairs(c.lobby_count, c.lobby_keys, c.lobby_values) };
    let mprops = if join.is_null() {
        Vec::new()
    } else {
        let j = unsafe { &*join };
        unsafe { pairs(j.member_count, j.member_keys, j.member_values) }
    };
    let mine = member_attrs(&entity, &mprops);
    let id = new_guid();
    let lobby = new_lobby(&id, lan, c.max_members, c.migration, c.access, &search, &props, mine.clone());
    log!(
        "pfmp-lan create lobby {id} max={} access={} search={search:?} props={props:?} member={mprops:?}",
        c.max_members,
        c.access
    );
    let h = handle();
    let mut s = STATE.lock().unwrap();
    s.lobbies.insert(
        h,
        LobbyObj {
            id: id.clone(),
            entity: entity.0.clone(),
            ty: entity.1.clone(),
            member_attrs: mine,
            phase: Phase::Joining {
                kind: JoinKind::Join,
                ctx,
                until: Instant::now(),
                create: None,
                first_try: None,
            },
            view: Box::new(View::placeholder(&id)),
            dirty: false,
        },
    );
    match lan.create_lobby(lobby) {
        Ok(l) => {
            s.lobbies.get_mut(&h).unwrap().phase = Phase::Joined;
            s.set_view(h, View::of(&l));
            let me = key(&entity.0, &entity.1);
            s.lobby_changes.push(
                MemberAdded {
                    ty: MEMBER_ADDED,
                    lobby: h,
                    member: me,
                },
                Vec::new(),
            );
            s.lobby_changes.push(
                LobbyCompleted {
                    ty: CREATE_AND_JOIN_COMPLETED,
                    result: S_OK,
                    ctx,
                    lobby: h,
                },
                Vec::new(),
            );
        }
        Err(e) => {
            log!("pfmp-lan could not create lobby: {e:?}");
            s.lobbies.get_mut(&h).unwrap().phase = Phase::Gone;
            s.lobby_changes.push(
                LobbyCompleted {
                    ty: CREATE_AND_JOIN_COMPLETED,
                    result: E_FAIL,
                    ctx,
                    lobby: h,
                },
                Vec::new(),
            );
            s.lobby_changes.forget.push(h);
        }
    }
    unsafe { put(out, h) };
    S_OK
}

/// A lobby object for a lobby we're joining.
fn insert_joining(
    s: &mut State,
    id: &str,
    entity: &(String, String),
    mine: Vec<Attr>,
    kind: JoinKind,
    ctx: usize,
    create: Option<Lobby>,
) -> usize {
    let h = handle();
    s.lobbies.insert(
        h,
        LobbyObj {
            id: id.to_string(),
            entity: entity.0.clone(),
            ty: entity.1.clone(),
            member_attrs: mine,
            phase: Phase::Joining {
                kind,
                ctx,
                until: Instant::now() + ARRANGED_WAIT,
                create,
                first_try: None,
            },
            view: Box::new(View::placeholder(id)),
            dirty: false,
        },
    );
    h
}

/// Starts joining a lobby by id; JoinDone finishes it.
fn begin_join(
    s: &mut State,
    lan: &Lan,
    id: &str,
    entity: &(String, String),
    mine: Vec<Attr>,
    kind: JoinKind,
    ctx: usize,
    create: Option<Lobby>,
) -> usize {
    let h = insert_joining(s, id, entity, mine, kind, ctx, create);
    match lan.join_by_id(NS, id) {
        Ok(()) => {}
        // Already in it (say, a second join of the same lobby): done at once.
        Err(peerparty_core::LobbyError::AlreadyIn) => s.joined(lan, h, kind, ctx),
        Err(e) => {
            log!("pfmp-lan join {id}: {e:?}");
            s.join_failed(h, kind, ctx, E_LOBBY_NOT_FOUND);
        }
    }
    h
}

unsafe extern "system" fn mp_join(
    _: usize,
    member: *const EntityKey,
    connection: *const c_char,
    config: *const JoinConfig,
    ctx: usize,
    out: *mut usize,
) -> i32 {
    let Some(lan) = net::lan() else { return E_FAIL };
    let Some(entity) = (unsafe { entity_in(member) }) else { return E_INVALIDARG };
    let connection = unsafe { text(connection) };
    let id = connection.strip_prefix(CONNECT_PREFIX).unwrap_or(&connection).to_string();
    if id.is_empty() {
        return E_INVALIDARG;
    }
    let props = if config.is_null() {
        Vec::new()
    } else {
        let j = unsafe { &*config };
        unsafe { pairs(j.member_count, j.member_keys, j.member_values) }
    };
    log!("pfmp-lan join lobby {id} as {} member={props:?}", entity.0);
    let mine = member_attrs(&entity, &props);
    let mut s = STATE.lock().unwrap();
    let h = begin_join(&mut s, lan, &id, &entity, mine, JoinKind::Join, ctx, None);
    unsafe { put(out, h) };
    S_OK
}

unsafe extern "system" fn mp_join_arranged(
    _: usize,
    member: *const EntityKey,
    arrangement: *const c_char,
    config: *const ArrangedConfig,
    ctx: usize,
    out: *mut usize,
) -> i32 {
    let Some(lan) = net::lan() else { return E_FAIL };
    let Some(entity) = (unsafe { entity_in(member) }) else { return E_INVALIDARG };
    if config.is_null() {
        return E_INVALIDARG;
    }
    let arrangement = unsafe { text(arrangement) };
    let c = unsafe { &*config };
    let props = unsafe { pairs(c.member_count, c.member_keys, c.member_values) };
    let mine = member_attrs(&entity, &props);
    // `LBA-<creator>-<match>` names who creates it; any other string, whoever finds
    // nobody there first.
    let (creator, id) = match arrangement.strip_prefix(ARRANGED_PREFIX).and_then(|r| r.split_once('-')) {
        Some((creator, m)) => (Some(creator.to_string()), format!("arranged-{m}")),
        None => (None, format!("arranged-{arrangement}")),
    };
    let lobby = new_lobby(&id, lan, c.max_members, c.migration, c.access, &[], &[], mine.clone());
    let we_create = creator.as_deref() == Some(entity.0.as_str());
    log!(
        "pfmp-lan join arranged lobby {id} as {} (creator {}) member={props:?}",
        entity.0,
        creator.as_deref().unwrap_or("whoever is first")
    );
    let mut s = STATE.lock().unwrap();
    let h = if we_create {
        let h = insert_joining(&mut s, &id, &entity, mine, JoinKind::Arranged, ctx, None);
        match lan.create_lobby(lobby) {
            Ok(_) | Err(peerparty_core::LobbyError::AlreadyIn) => s.joined(lan, h, JoinKind::Arranged, ctx),
            Err(e) => {
                log!("pfmp-lan could not create arranged lobby: {e:?}");
                s.join_failed(h, JoinKind::Arranged, ctx, E_FAIL);
            }
        }
        h
    } else if creator.is_some() {
        // The creator is named: give it a head start, then join (pump makes the try).
        let h = insert_joining(&mut s, &id, &entity, mine, JoinKind::Arranged, ctx, None);
        if let Some(Phase::Joining { first_try, .. }) = s.lobbies.get_mut(&h).map(|l| &mut l.phase) {
            *first_try = Some(Instant::now() + ARRANGED_HEAD_START);
        }
        h
    } else {
        begin_join(&mut s, lan, &id, &entity, mine, JoinKind::Arranged, ctx, Some(lobby))
    };
    unsafe { put(out, h) };
    S_OK
}

unsafe extern "system" fn mp_find_lobbies(
    _: usize,
    searcher: *const EntityKey,
    config: *const SearchConfig,
    ctx: usize,
) -> i32 {
    let Some(lan) = net::lan() else { return E_FAIL };
    let Some(entity) = (unsafe { entity_in(searcher) }) else { return E_INVALIDARG };
    let (filter, sort, count, friends) = if config.is_null() {
        (String::new(), String::new(), 50, false)
    } else {
        let c = unsafe { &*config };
        let count = if c.count == 0 { 50 } else { (unsafe { *(c.count as *const u32) }) as usize };
        unsafe { (text(c.filter as *const c_char), text(c.sort as *const c_char), count, c.friends != 0) }
    };
    log!("pfmp-lan find lobbies filter={filter:?} sort={sort:?} count={count} friends={friends}");
    let mut q = Query::new(NS);
    q.max_results = 200;
    let id = lan.search(q);
    STATE.lock().unwrap().searches.insert(
        id,
        Search {
            entity,
            ctx,
            filter: parse_filter(&filter),
            sort: parse_sort(&sort),
            count: count.clamp(1, 200),
            friends,
        },
    );
    S_OK
}

unsafe extern "system" fn mp_start_listening(_: usize, entity: *const EntityKey) -> i32 {
    let Some(entity) = (unsafe { entity_in(entity) }) else { return E_INVALIDARG };
    let mut s = STATE.lock().unwrap();
    if s.listening.insert(entity.clone()) {
        log!("pfmp-lan listening for lobby invites as {}", entity.0);
        s.lobby_changes.push(
            ListenerChanged {
                ty: INVITE_LISTENER_CHANGED,
                listener: key(&entity.0, &entity.1),
            },
            Vec::new(),
        );
    }
    S_OK
}

unsafe extern "system" fn mp_stop_listening(_: usize, entity: *const EntityKey) -> i32 {
    let Some(entity) = (unsafe { entity_in(entity) }) else { return E_INVALIDARG };
    let mut s = STATE.lock().unwrap();
    if s.listening.remove(&entity) {
        s.lobby_changes.push(
            ListenerChanged {
                ty: INVITE_LISTENER_CHANGED,
                listener: key(&entity.0, &entity.1),
            },
            Vec::new(),
        );
    }
    S_OK
}

unsafe extern "system" fn mp_listener_status(_: usize, entity: *const EntityKey, out: *mut u32) -> i32 {
    let Some(entity) = (unsafe { entity_in(entity) }) else { return E_INVALIDARG };
    let listening = STATE.lock().unwrap().listening.contains(&entity);
    unsafe { put(out, u32::from(listening)) };
    S_OK
}

unsafe extern "system" fn mp_create_ticket(
    _: usize,
    user_count: u32,
    users: *const EntityKey,
    attributes: *const usize,
    config: *const TicketConfig,
    ctx: usize,
    out: *mut usize,
) -> i32 {
    if user_count == 0 || users.is_null() || config.is_null() {
        return E_INVALIDARG;
    }
    if user_count > 1 {
        log!("pfmp-lan ticket for {user_count} local users: only the first is matched on the LAN");
    }
    let Some(entity) = (unsafe { entity_in(users) }) else { return E_INVALIDARG };
    let data = if attributes.is_null() { String::new() } else { unsafe { text(*attributes as *const c_char) } };
    let c = unsafe { &*config };
    let queue = unsafe { text(c.queue as *const c_char) };
    let timeout = if c.timeout == 0 { 120.0 } else { f64::from(c.timeout) };
    let id = match api::open_ticket(&queue, &entity.0, &data, timeout) {
        Ok(id) => id,
        Err(e) => {
            log!("pfmp-lan ticket not created: {e}");
            return E_FAIL;
        }
    };
    log!("pfmp-lan ticket {id} queue={queue} entity={} attributes={data}", entity.0);
    let h = handle();
    STATE.lock().unwrap().tickets.insert(
        h,
        TicketObj {
            id_c: cstring(&id),
            id: id.clone(),
            ctx,
            status: TICKET_CREATING,
            outcome: None,
            done: false,
            details: None,
        },
    );
    watch_ticket(h, id);
    unsafe { put(out, h) };
    S_OK
}

extern "system" fn mp_destroy_ticket(_: usize, ticket: usize) -> i32 {
    let mut s = STATE.lock().unwrap();
    let Some(t) = s.tickets.get_mut(&ticket) else { return E_INVALIDARG };
    if !t.done {
        t.done = true;
        api::cancel_ticket(&t.id);
    }
    // The game destroys tickets while handling their completion; they last the batch.
    if !s.ticket_changes.forget_in_flight.contains(&ticket) {
        s.ticket_changes.forget.push(ticket);
    }
    S_OK
}

unsafe extern "system" fn ticket_status(ticket: usize, out: *mut u32) -> i32 {
    let s = STATE.lock().unwrap();
    let Some(t) = s.tickets.get(&ticket) else { return E_INVALIDARG };
    unsafe { put(out, t.status) };
    S_OK
}

extern "system" fn ticket_cancel(ticket: usize) -> i32 {
    let id = match STATE.lock().unwrap().tickets.get(&ticket) {
        Some(t) if !t.done => t.id.clone(),
        Some(_) => return S_OK,
        None => return E_INVALIDARG,
    };
    // The watcher sees the cancellation on its next poll.
    api::cancel_ticket(&id);
    S_OK
}

unsafe extern "system" fn ticket_id(ticket: usize, out: *mut usize) -> i32 {
    let s = STATE.lock().unwrap();
    let Some(t) = s.tickets.get(&ticket) else { return E_INVALIDARG };
    unsafe { put(out, t.id_c.as_ptr() as usize) };
    S_OK
}

unsafe extern "system" fn ticket_match(ticket: usize, out: *mut usize) -> i32 {
    let s = STATE.lock().unwrap();
    let Some(t) = s.tickets.get(&ticket) else { return E_INVALIDARG };
    let Some(d) = &t.details else { return E_INVALIDARG };
    unsafe { put(out, &d.details as *const MatchDetails as usize) };
    S_OK
}

// ---- lobby getters ----

fn with_view<R>(lobby: usize, f: impl FnOnce(&View) -> R) -> Option<R> {
    let s = STATE.lock().unwrap();
    s.lobbies.get(&lobby).map(|l| f(&l.view))
}

unsafe extern "system" fn lobby_id(lobby: usize, out: *mut usize) -> i32 {
    match with_view(lobby, |v| v.id.as_ptr() as usize) {
        Some(p) => {
            unsafe { put(out, p) };
            S_OK
        }
        None => E_INVALIDARG,
    }
}

unsafe extern "system" fn lobby_connection(lobby: usize, out: *mut usize) -> i32 {
    match with_view(lobby, |v| v.connection.as_ptr() as usize) {
        Some(p) => {
            unsafe { put(out, p) };
            S_OK
        }
        None => E_INVALIDARG,
    }
}

unsafe fn put_u32(lobby: usize, out: *mut u32, f: impl FnOnce(&View) -> u32) -> i32 {
    match with_view(lobby, f) {
        Some(v) => {
            unsafe { put(out, v) };
            S_OK
        }
        None => E_INVALIDARG,
    }
}

unsafe extern "system" fn lobby_max_members(lobby: usize, out: *mut u32) -> i32 {
    unsafe { put_u32(lobby, out, |v| v.max_members) }
}

unsafe extern "system" fn lobby_migration(lobby: usize, out: *mut u32) -> i32 {
    unsafe { put_u32(lobby, out, |v| v.migration) }
}

unsafe extern "system" fn lobby_access(lobby: usize, out: *mut u32) -> i32 {
    unsafe { put_u32(lobby, out, |v| v.access) }
}

unsafe extern "system" fn lobby_lock(lobby: usize, out: *mut u32) -> i32 {
    unsafe { put_u32(lobby, out, |v| v.lock) }
}

unsafe extern "system" fn lobby_owner(lobby: usize, out: *mut usize) -> i32 {
    let owner = with_view(lobby, |v| {
        v.owner.as_ref().map_or(0, |(e, t)| key_ref(e, t) as *const EntityKey as usize)
    });
    match owner {
        Some(p) => {
            unsafe { put(out, p) };
            S_OK
        }
        None => E_INVALIDARG,
    }
}

unsafe extern "system" fn lobby_members(lobby: usize, count: *mut u32, out: *mut usize) -> i32 {
    match with_view(lobby, |v| (v.member_keys.len() as u32, ptr_of(&v.member_keys))) {
        Some((n, p)) => {
            unsafe {
                put(count, n);
                put(out, p);
            }
            S_OK
        }
        None => E_INVALIDARG,
    }
}

unsafe fn put_keys(lobby: usize, count: *mut u32, out: *mut usize, f: impl FnOnce(&View) -> Option<&Vec<usize>>) -> i32 {
    match with_view(lobby, |v| f(v).map(|k| (k.len() as u32, ptr_of(k)))) {
        Some(Some((n, p))) => {
            unsafe {
                put(count, n);
                put(out, p);
            }
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

unsafe fn put_value(
    lobby: usize,
    out: *mut usize,
    f: impl FnOnce(&View) -> Option<Option<usize>>,
) -> i32 {
    match with_view(lobby, f) {
        Some(Some(v)) => {
            unsafe { put(out, v.unwrap_or(0)) };
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

unsafe extern "system" fn lobby_search_keys(lobby: usize, count: *mut u32, out: *mut usize) -> i32 {
    unsafe { put_keys(lobby, count, out, |v| Some(&v.search_keys)) }
}

unsafe extern "system" fn lobby_search_property(lobby: usize, k: *const c_char, out: *mut usize) -> i32 {
    let k = unsafe { text(k) };
    unsafe { put_value(lobby, out, |v| Some(find_prop(&v.search, &k).map(|c| c.as_ptr() as usize))) }
}

unsafe extern "system" fn lobby_property_keys(lobby: usize, count: *mut u32, out: *mut usize) -> i32 {
    unsafe { put_keys(lobby, count, out, |v| Some(&v.prop_keys)) }
}

unsafe extern "system" fn lobby_property(lobby: usize, k: *const c_char, out: *mut usize) -> i32 {
    let k = unsafe { text(k) };
    unsafe { put_value(lobby, out, |v| Some(find_prop(&v.props, &k).map(|c| c.as_ptr() as usize))) }
}

unsafe extern "system" fn lobby_member_keys(
    lobby: usize,
    member: *const EntityKey,
    count: *mut u32,
    out: *mut usize,
) -> i32 {
    let Some((entity, _)) = (unsafe { entity_in(member) }) else { return E_INVALIDARG };
    unsafe { put_keys(lobby, count, out, |v| v.member(&entity).map(|m| &m.prop_keys)) }
}

unsafe extern "system" fn lobby_member_property(
    lobby: usize,
    member: *const EntityKey,
    k: *const c_char,
    out: *mut usize,
) -> i32 {
    let Some((entity, _)) = (unsafe { entity_in(member) }) else { return E_INVALIDARG };
    let k = unsafe { text(k) };
    unsafe {
        put_value(lobby, out, |v| {
            v.member(&entity)
                .map(|m| find_prop(&m.props, &k).map(|c| c.as_ptr() as usize))
        })
    }
}

unsafe extern "system" fn lobby_member_connection(lobby: usize, member: *const EntityKey, out: *mut u32) -> i32 {
    let Some((entity, _)) = (unsafe { entity_in(member) }) else { return E_INVALIDARG };
    match with_view(lobby, |v| v.member(&entity).is_some()) {
        Some(present) => {
            unsafe { put(out, u32::from(present)) };
            S_OK
        }
        None => E_INVALIDARG,
    }
}

// ---- lobby operations ----

unsafe extern "system" fn lobby_leave(lobby: usize, user: *const EntityKey, ctx: usize) -> i32 {
    let who = unsafe { entity_in(user) };
    let mut s = STATE.lock().unwrap();
    let Some(l) = s.lobbies.get(&lobby) else { return E_INVALIDARG };
    if matches!(l.phase, Phase::Gone) {
        return S_OK;
    }
    if let Some(lan) = net::lan() {
        let _ = lan.leave(&l.id);
    }
    let user_ptr = who.map_or(0, |(e, t)| key_ref(&e, &t) as *const EntityKey as usize);
    log!("pfmp-lan leave lobby {}", l.id);
    s.gone(lobby, Some(LOCAL_LEFT), Some((user_ptr, ctx)));
    S_OK
}

/// What the REST API's `UpdateLobby` asks (playfab/api.rs). A None value deletes the key.
#[derive(Default)]
pub(crate) struct RestUpdate {
    pub max: Option<u32>,
    pub locked: Option<bool>,
    pub search: Vec<(String, Option<String>)>,
    pub props: Vec<(String, Option<String>)>,
    pub member: Vec<(String, Option<String>)>,
    pub owner: Option<String>,
}

/// The REST API's `UpdateLobby` on a lobby this copy is in: what `PFLobbyPostUpdate`
/// does, minus the completion (the HTTP answer is the result). Err: PlayFab's error name.
pub(crate) fn rest_update(id: &str, u: RestUpdate) -> Result<(), &'static str> {
    let lan = net::lan().ok_or("LobbyNotFound")?;
    let mut s = STATE.lock().unwrap();
    // A lobby the library holds, or one the web API made or joined (no library object).
    let h = s.lobby_of(id);
    if h.is_none() && !lan.lobby(id).is_some_and(|l| l.namespace == NS) {
        return Err("LobbyNotFound");
    }
    let mut edited = None;
    let mut promote = None;
    if u.max.is_some() || u.locked.is_some() || !u.search.is_empty() || !u.props.is_empty() || u.owner.is_some() {
        if !lan.is_owner(id) {
            return Err("LobbyPlayerNotOwner");
        }
        let mut now = lan.lobby(id).ok_or("LobbyNotFound")?;
        if let Some(m) = u.max {
            now.max_members = m.clamp(2, 128);
        }
        if let Some(locked) = u.locked {
            peerparty_core::set_attr(&mut now.attrs, str_attr("lock".into(), if locked { "1" } else { "0" }, true));
        }
        apply(&mut now.attrs, "s:", true, &u.search);
        apply(&mut now.attrs, "l:", false, &u.props);
        if let Some(owner) = &u.owner {
            promote = View::of(&now).members.iter().find(|m| &m.entity == owner).map(|m| m.peer);
        }
        trace("UpdateLobby (web API)", || format!("{id} search={:?} props={:?}", u.search, u.props));
        edited = Some(now);
    }
    let set: Vec<Attr> = u.member.iter().filter_map(|(k, v)| v.as_ref().map(|v| str_attr(format!("m:{k}"), v, false))).collect();
    let remove: Vec<String> = u.member.iter().filter(|(_, v)| v.is_none()).map(|(k, _)| format!("m:{k}")).collect();
    if let Some(obj) = h.and_then(|h| s.lobbies.get_mut(&h)) {
        for a in &set {
            peerparty_core::set_attr(&mut obj.member_attrs, a.clone());
        }
        for k in &remove {
            peerparty_core::remove_attr(&mut obj.member_attrs, k);
        }
        obj.dirty = true;
    }
    lan.update(id, edited.as_ref(), &set, &remove).map_err(|_| "LobbyNotFound")?;
    if let Some(peer) = promote {
        let _ = lan.promote(id, peer);
    }
    Ok(())
}

// ---- the web API's lobby calls (playfab/api.rs) ----
//
// Games that make and join lobbies through PlayFab's REST API get the same LAN lobbies
// as the library's, so one copy can use the library and another the web API (or one
// game both). There's no library object for these: GetLobby, UpdateLobby, LeaveLobby
// and the rest work on the LAN lobby itself.

/// PFLobbyAccessPolicy from the web API's name.
fn access_of(name: &str) -> u32 {
    match name {
        "Friends" => 1,
        "Private" => ACCESS_PRIVATE,
        _ => 0,
    }
}

/// PFLobbyOwnerMigrationPolicy from the web API's name.
fn migration_of(name: &str) -> u32 {
    match name {
        "Manual" => 1,
        "None" => 2,
        "Server" => 3,
        _ => 0,
    }
}

/// The settings of a lobby the web API makes or joins.
pub(crate) struct RestCreate {
    pub member: (String, String),
    pub max: u32,
    pub access: String,
    pub migration: String,
    pub search: Vec<(String, Option<String>)>,
    pub props: Vec<(String, Option<String>)>,
    pub member_data: Vec<(String, Option<String>)>,
}

/// `CreateLobby`: a new LAN lobby with the creator in it. Its id.
pub(crate) fn rest_create(c: RestCreate) -> Result<String, &'static str> {
    let lan = net::lan().ok_or("ServiceUnavailable")?;
    let id = new_guid();
    let mine = member_attrs(&c.member, &c.member_data);
    let lobby = new_lobby(&id, lan, c.max, migration_of(&c.migration), access_of(&c.access), &c.search, &c.props, mine);
    match lan.create_lobby(lobby) {
        Ok(_) => {
            log!("pfmp-lan create lobby {id} through the web API max={} access={} search={:?}", c.max, c.access, c.search);
            Ok(id)
        }
        Err(e) => {
            log!("pfmp-lan could not create lobby through the web API: {e:?}");
            Err("LobbyCreateFailed")
        }
    }
}

/// Waits until this copy is in lobby `id`, or its join failed.
fn wait_joined(lan: &Lan, id: &str, wait: Duration) -> Result<(), &'static str> {
    net::subscribe(net::Front::PlayFab);
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if lan.lobby(id).is_some_and(|l| l.members.iter().any(|m| m.id == lan.me())) {
            return Ok(());
        }
        for ev in net::take(net::Front::PlayFab) {
            if let Event::JoinDone { lobby, result } = ev {
                if lobby == id && result != JoinResult::Ok {
                    return Err(match result {
                        JoinResult::Full => "LobbyFull",
                        JoinResult::NotAllowed => "LobbyNotJoinable",
                        _ => "LobbyNotFound",
                    });
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err("LobbyNotFound")
}

/// `JoinLobby`: joins the LAN lobby a connection string names. Its id.
pub(crate) fn rest_join(connection: &str, member: (String, String), data: Vec<(String, Option<String>)>) -> Result<String, &'static str> {
    let lan = net::lan().ok_or("ServiceUnavailable")?;
    let id = connection.strip_prefix(CONNECT_PREFIX).unwrap_or(connection).to_string();
    if id.is_empty() {
        return Err("InvalidParams");
    }
    match lan.join_by_id(NS, &id) {
        Ok(()) | Err(peerparty_core::LobbyError::AlreadyIn) => {}
        Err(e) => {
            log!("pfmp-lan join {id} through the web API: {e:?}");
            return Err("LobbyNotFound");
        }
    }
    wait_joined(lan, &id, Duration::from_secs(10))?;
    let _ = lan.update(&id, None, &member_attrs(&member, &data), &[]);
    log!("pfmp-lan joined lobby {id} through the web API as {}", member.0);
    Ok(id)
}

/// `JoinArrangedLobby`: the lobby an arrangement string names, made by whoever the
/// string names (`LBA-<creator>-<match>`, as PeerParty's matches give) or by the first
/// copy to find nobody there.
pub(crate) fn rest_join_arranged(arrangement: &str, c: RestCreate) -> Result<String, &'static str> {
    let lan = net::lan().ok_or("ServiceUnavailable")?;
    let (creator, id) = match arrangement.strip_prefix(ARRANGED_PREFIX).and_then(|r| r.split_once('-')) {
        Some((creator, m)) => (Some(creator.to_string()), format!("arranged-{m}")),
        None => (None, format!("arranged-{arrangement}")),
    };
    let mine = member_attrs(&c.member, &c.member_data);
    let create = || {
        let lobby = new_lobby(&id, lan, c.max, migration_of(&c.migration), access_of(&c.access), &[], &[], mine.clone());
        match lan.create_lobby(lobby) {
            Ok(_) | Err(peerparty_core::LobbyError::AlreadyIn) => Ok(()),
            Err(_) => Err("LobbyCreateFailed"),
        }
    };
    log!("pfmp-lan join arranged lobby {id} through the web API (creator {})", creator.as_deref().unwrap_or("whoever is first"));
    if creator.as_deref() == Some(c.member.0.as_str()) {
        create()?;
        return Ok(id);
    }
    let deadline = Instant::now() + if creator.is_some() { ARRANGED_WAIT } else { ARRANGED_HEAD_START };
    loop {
        if lan.join_by_id(NS, &id).is_ok() && wait_joined(lan, &id, Duration::from_secs(1)).is_ok() {
            let _ = lan.update(&id, None, &mine, &[]);
            return Ok(id);
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if creator.is_some() {
        return Err("LobbyNotFound");
    }
    create()?;
    Ok(id)
}

/// `DeleteLobby`: the owner ends the lobby for everyone.
pub(crate) fn rest_delete(id: &str) -> Result<(), &'static str> {
    let lan = net::lan().ok_or("ServiceUnavailable")?;
    if !lan.is_owner(id) {
        return Err("LobbyPlayerNotOwner");
    }
    lan.destroy(id).map_err(|_| "LobbyNotFound")?;
    let mut s = STATE.lock().unwrap();
    if let Some(h) = s.lobby_of(id) {
        s.gone(h, Some(LOCAL_LEFT), None);
    }
    log!("pfmp-lan deleted lobby {id} through the web API");
    Ok(())
}

/// `RemoveMember`: the owner removes a member (a member removing itself leaves).
pub(crate) fn rest_remove(id: &str, entity: &str) -> Result<(), &'static str> {
    let lan = net::lan().ok_or("ServiceUnavailable")?;
    let l = lan.lobby(id).ok_or("LobbyNotFound")?;
    let target = l
        .members
        .iter()
        .find(|m| member_entity(m).is_some_and(|(e, _)| e == entity))
        .map(|m| m.id)
        .ok_or("LobbyMemberNotFound")?;
    if target == lan.me() {
        rest_leave(id);
        return Ok(());
    }
    if !lan.is_owner(id) {
        return Err("LobbyPlayerNotOwner");
    }
    lan.kick(id, target).map_err(|_| "LobbyMemberNotFound")?;
    log!("pfmp-lan removed {entity} from lobby {id} through the web API");
    Ok(())
}

/// `InviteToLobby`: a LAN invite to the lobby for the player with that entity id.
pub(crate) fn rest_invite(id: &str, invitee: &str) -> Result<(), &'static str> {
    let lan = net::lan().ok_or("ServiceUnavailable")?;
    let peer = lan.peers().into_iter().map(|(p, _)| p).find(|p| api::entity_of(*p) == invitee).ok_or("InvalidParams")?;
    let connect = format!("{CONNECT_PREFIX}{id}");
    lan.invite(Some(peer), &connect, None);
    log!("pfmp-lan invite {invitee} to {connect} through the web API");
    Ok(())
}

/// The REST API's `LeaveLobby`: this copy leaves the LAN lobby, and the library hears it
/// left (member removed, then disconnected), as PlayFab's own notifications would say.
pub(crate) fn rest_leave(id: &str) {
    if let Some(lan) = net::lan() {
        let _ = lan.leave(id);
    }
    let mut s = STATE.lock().unwrap();
    if let Some(h) = s.lobby_of(id) {
        log!("pfmp-lan left lobby {id} through the web API");
        s.gone(h, Some(LOCAL_LEFT), None);
    }
}

fn apply(attrs: &mut Vec<Attr>, prefix: &str, public: bool, changes: &[(String, Option<String>)]) {
    for (k, v) in changes {
        let key = format!("{prefix}{k}");
        match v {
            Some(v) => peerparty_core::set_attr(attrs, str_attr(key, v, public)),
            None => {
                peerparty_core::remove_attr(attrs, &key);
            }
        }
    }
}

unsafe extern "system" fn lobby_post_update(
    lobby: usize,
    user: *const EntityKey,
    update: *const DataUpdate,
    member_update: *const MemberDataUpdate,
    ctx: usize,
) -> i32 {
    let Some(lan) = net::lan() else { return E_FAIL };
    let who = unsafe { entity_in(user) };
    let mut s = STATE.lock().unwrap();
    let Some(l) = s.lobbies.get(&lobby) else { return E_INVALIDARG };
    let id = l.id.clone();
    let me = who.clone().unwrap_or((l.entity.clone(), l.ty.clone()));
    let member_changes = if member_update.is_null() {
        Vec::new()
    } else {
        let m = unsafe { &*member_update };
        unsafe { pairs(m.count, m.keys, m.values) }
    };
    let mut result = S_OK;
    let mut edited = None;
    let mut promote = None;
    if !update.is_null() {
        let u = unsafe { &*update };
        if !lan.is_owner(&id) {
            result = E_ACCESSDENIED;
        } else if let Some(mut lobby_now) = lan.lobby(&id) {
            if u.max_members != 0 {
                lobby_now.max_members = unsafe { *(u.max_members as *const u32) }.clamp(2, 128);
            }
            if u.access != 0 {
                let access = unsafe { *(u.access as *const u32) };
                lobby_now.permission =
                    if access == ACCESS_PRIVATE { Permission::InviteOnly } else { Permission::Public };
                peerparty_core::set_attr(&mut lobby_now.attrs, str_attr("acc".into(), &access.to_string(), true));
            }
            if u.lock != 0 {
                let lock = unsafe { *(u.lock as *const u32) };
                peerparty_core::set_attr(&mut lobby_now.attrs, str_attr("lock".into(), &lock.to_string(), true));
            }
            let search = unsafe { pairs(u.search_count, u.search_keys, u.search_values) };
            let props = unsafe { pairs(u.lobby_count, u.lobby_keys, u.lobby_values) };
            apply(&mut lobby_now.attrs, "s:", true, &search);
            apply(&mut lobby_now.attrs, "l:", false, &props);
            if let Some((owner, _)) = unsafe { entity_in(u.new_owner as *const EntityKey) } {
                promote = l.view.members.iter().find(|m| m.entity == owner).map(|m| m.peer);
            }
            trace("PFLobbyPostUpdate lobby", || format!("{id} search={search:?} props={props:?}"));
            edited = Some(lobby_now);
        }
    }
    if result == S_OK {
        let set: Vec<Attr> = member_changes
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| str_attr(format!("m:{k}"), v, false)))
            .collect();
        let remove: Vec<String> = member_changes
            .iter()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| format!("m:{k}"))
            .collect();
        if !member_changes.is_empty() {
            trace("PFLobbyPostUpdate member", || format!("{id} {member_changes:?}"));
        }
        if let Some(obj) = s.lobbies.get_mut(&lobby) {
            for a in &set {
                peerparty_core::set_attr(&mut obj.member_attrs, a.clone());
            }
            for k in &remove {
                peerparty_core::remove_attr(&mut obj.member_attrs, k);
            }
        }
        if lan.update(&id, edited.as_ref(), &set, &remove).is_err() {
            result = E_LOBBY_NOT_FOUND;
        }
        if let Some(peer) = promote {
            let _ = lan.promote(&id, peer);
        }
    }
    s.lobby_changes.push(
        MemberOpCompleted {
            ty: POST_UPDATE_COMPLETED,
            result,
            lobby,
            member: key(&me.0, &me.1),
            ctx,
        },
        Vec::new(),
    );
    // The owner's own edit raises no LAN event; members' do, but checking is cheap.
    if let Some(obj) = s.lobbies.get_mut(&lobby) {
        obj.dirty = true;
    }
    S_OK
}

unsafe extern "system" fn lobby_force_remove(lobby: usize, target: *const EntityKey, _prevent_rejoin: u8, ctx: usize) -> i32 {
    let Some(lan) = net::lan() else { return E_FAIL };
    let Some(target) = (unsafe { entity_in(target) }) else { return E_INVALIDARG };
    let mut s = STATE.lock().unwrap();
    let Some(l) = s.lobbies.get(&lobby) else { return E_INVALIDARG };
    let peer = l.view.member(&target.0).map(|m| m.peer);
    let result = match peer {
        Some(p) if lan.kick(&l.id, p).is_ok() => S_OK,
        Some(_) => E_ACCESSDENIED,
        None => E_INVALIDARG,
    };
    log!("pfmp-lan remove {} from lobby {}: {result:#x}", target.0, l.id);
    s.lobby_changes.push(
        MemberOpCompleted {
            ty: FORCE_REMOVE_COMPLETED,
            result,
            lobby,
            member: key(&target.0, &target.1),
            ctx,
        },
        Vec::new(),
    );
    if let Some(obj) = s.lobbies.get_mut(&lobby) {
        obj.dirty = true;
    }
    S_OK
}

unsafe extern "system" fn lobby_send_invite(
    lobby: usize,
    sender: *const EntityKey,
    invitee: *const EntityKey,
    ctx: usize,
) -> i32 {
    let Some(lan) = net::lan() else { return E_FAIL };
    let (Some(from), Some(to)) = (unsafe { entity_in(sender) }, unsafe { entity_in(invitee) }) else {
        return E_INVALIDARG;
    };
    let mut s = STATE.lock().unwrap();
    let Some(l) = s.lobbies.get(&lobby) else { return E_INVALIDARG };
    let connect = format!("{CONNECT_PREFIX}{}", l.id);
    let peer = lan.peers().into_iter().map(|(p, _)| p).find(|p| api::entity_of(*p) == to.0);
    let result = match peer {
        Some(p) => {
            lan.invite(Some(p), &connect, None);
            S_OK
        }
        None => E_INVALIDARG,
    };
    log!("pfmp-lan invite {} to {connect}: {result:#x}", to.0);
    s.lobby_changes.push(
        SendInviteCompleted {
            ty: SEND_INVITE_COMPLETED,
            result,
            lobby,
            sender: key(&from.0, &from.1),
            invitee: key(&to.0, &to.1),
            ctx,
        },
        Vec::new(),
    );
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn layouts_match_the_1_7_2_interop() {
        // Offsets from PlayFab's generated C# interop (FieldOffset attributes).
        assert_eq!(offset_of!(Updated, lobby), 8);
        assert_eq!(offset_of!(Updated, owner), 16);
        assert_eq!(offset_of!(Updated, search_count), 20);
        assert_eq!(offset_of!(Updated, search_keys), 24);
        assert_eq!(offset_of!(Updated, lobby_keys), 40);
        assert_eq!(offset_of!(Updated, members), 56);
        assert_eq!(offset_of!(Updated, server), 64);
        assert_eq!(offset_of!(Updated, server_count), 68);
        assert_eq!(offset_of!(Updated, server_keys), 72);
        assert_eq!(offset_of!(Updated, server_connection), 80);
        assert_eq!(size_of::<SearchResult>(), 80);
        assert_eq!(offset_of!(SearchResult, friends), 64);
        assert_eq!(offset_of!(SearchResult, lock), 72);
        assert_eq!(size_of::<MemberUpdateSummary>(), 32);
        assert_eq!(offset_of!(JoinCompleted, member), 8);
        assert_eq!(offset_of!(JoinCompleted, ctx), 24);
        assert_eq!(offset_of!(JoinCompleted, lobby), 32);
        assert_eq!(offset_of!(MemberRemoved, reason), 32);
        assert_eq!(offset_of!(FindCompleted, ctx), 24);
        assert_eq!(offset_of!(FindCompleted, results), 40);
        assert_eq!(offset_of!(InviteReceived, connection), 40);
        assert_eq!(offset_of!(SendInviteCompleted, ctx), 48);
        assert_eq!(offset_of!(CreateConfig, lobby_values), 48);
        assert_eq!(offset_of!(DataUpdate, lobby_values), 72);
        assert_eq!(offset_of!(MatchDetails, arrangement), 40);
        assert_eq!(offset_of!(MatchDetails, server), 48);
        assert_eq!(size_of::<MatchMember>(), 32);
        assert_eq!(offset_of!(TicketCompleted, ctx), 16);
    }

    fn lobby(id: &str, members: usize, attrs: &[(&str, &str)]) -> Lobby {
        let mut l = Lobby::new(NS);
        l.id = id.into();
        l.max_members = 4;
        for i in 0..members {
            let mut p = PeerId::default();
            p.0[0] = i as u8 + 1;
            l.members.push(Member {
                id: p,
                attrs: vec![str_attr("pf".into(), &format!("E{i}"), true)],
            });
        }
        l.owner = l.members.first().map(|m| m.id).unwrap_or_default();
        for (k, v) in attrs {
            l.attrs.push(str_attr((*k).into(), v, true));
        }
        l
    }

    #[test]
    fn filters_and_sorts_like_playfab() {
        let f = parse_filter(
            "lobby/membershipLock eq 'Unlocked' and string_key1 eq 'Capture The Flag' and number_key10 gt 50 and lobby/memberCount lt 3",
        );
        assert_eq!(f.len(), 4);
        assert_eq!(f[1].value, Operand::Text("Capture The Flag".into()));
        let me = PeerId::random();
        let good = lobby("a", 2, &[("s:string_key1", "Capture The Flag"), ("s:number_key10", "60"), ("lock", "0")]);
        let locked = lobby("b", 2, &[("s:string_key1", "Capture The Flag"), ("s:number_key10", "60"), ("lock", "1")]);
        let low = lobby("c", 1, &[("s:string_key1", "Capture The Flag"), ("s:number_key10", "7")]);
        let full = lobby("d", 3, &[("s:string_key1", "Capture The Flag"), ("s:number_key10", "99")]);
        let holds = |l: &Lobby| f.iter().all(|c| clause_holds(l, me, c));
        assert!(holds(&good));
        assert!(!holds(&locked));
        assert!(!holds(&low));
        assert!(!holds(&full));
        assert!(parse_filter("lobby/amMember eq false").iter().all(|c| clause_holds(&good, me, c)));
        assert!(parse_filter("string_key2 ne 'x'").iter().all(|c| clause_holds(&good, me, c)));
        assert!(!parse_filter("string_key2 eq 'x'").iter().all(|c| clause_holds(&good, me, c)));
        assert!(parse_filter("").is_empty());

        let mut ls = vec![
            lobby("x", 1, &[("s:number_key1", "9")]),
            lobby("y", 3, &[("s:number_key1", "4")]),
            lobby("z", 2, &[("s:number_key1", "6")]),
        ];
        sort_lobbies(&mut ls, me, &parse_sort("distance{number_key1=5} asc"));
        assert_eq!(ls.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["y", "z", "x"]);
        sort_lobbies(&mut ls, me, &parse_sort("lobby/memberCount desc"));
        assert_eq!(ls.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["y", "z", "x"]);
        sort_lobbies(&mut ls, me, &parse_sort("number_key1 asc"));
        assert_eq!(ls.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["y", "z", "x"]);
        sort_lobbies(&mut ls, me, &parse_sort("number_key1 desc"));
        assert_eq!(ls.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["x", "z", "y"]);
    }

    #[test]
    fn current_connect_answers_while_state_is_held() {
        // An invite delivered from pump (STATE held) asks current_connect: it mustn't lock again.
        let s = STATE.lock().unwrap();
        joined_connect(&s);
        assert_eq!(current_connect(), None);
        drop(s);
    }

    #[test]
    fn rest_find_lobbies_filters_like_the_library() {
        let me = PeerId::random();
        let ls = vec![
            lobby("a", 1, &[("s:string_key5", "CA3"), ("s:string_key4", "GoG")]),
            lobby("b", 1, &[("s:string_key5", "N9W"), ("s:string_key4", "GoG")]),
        ];
        let found = summaries(ls, me, "string_key5 eq 'CA3' and string_key4 eq 'GoG'", "", 50);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].connection, "LBL-a");
        assert!(found[0].search.contains(&("string_key5".into(), "CA3".into())));
    }

    #[test]
    fn diffs_name_what_changed() {
        let a = lobby("a", 1, &[("s:mode", "race"), ("l:seed", "1")]);
        let mut b = lobby("a", 2, &[("s:mode", "battle"), ("l:seed", "1"), ("l:map", "3")]);
        b.members[0].attrs.push(str_attr("m:ready".into(), "1", false));
        let d = diff(&View::of(&a), &View::of(&b));
        assert_eq!(d.added, vec![("E1".to_string(), ENTITY_TYPE.to_string())]);
        assert!(d.removed.is_empty());
        assert_eq!(d.search, vec!["mode".to_string()]);
        assert_eq!(d.props, vec!["map".to_string()]);
        assert_eq!(d.members, vec![("E0".to_string(), ENTITY_TYPE.to_string(), vec!["ready".to_string()])]);
        assert!(!d.owner);
        let back = diff(&View::of(&b), &View::of(&a));
        assert_eq!(back.removed.len(), 1);
        assert_eq!(back.props, vec!["map".to_string()]);
    }

    #[test]
    fn arrangement_names_the_creator() {
        let m = match_mem("t1+t2", &[("AAAA".into(), "{}".into()), ("BBBB".into(), "{}".into())]);
        let s = unsafe { text(m.details.arrangement as *const c_char) };
        assert_eq!(s, "LBA-AAAA-t1+t2");
        let (creator, rest) = s.strip_prefix(ARRANGED_PREFIX).unwrap().split_once('-').unwrap();
        assert_eq!((creator, rest), ("AAAA", "t1+t2"));
        assert_eq!(m.details.member_count, 2);
    }
}
