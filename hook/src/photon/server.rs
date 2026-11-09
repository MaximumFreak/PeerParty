//! Photon name server, master server and game server in one sans-IO state machine.
//!
//! Port of Luxon Server (BSD-3-Clause, see LICENSE-Luxon): `handler_base.cpp`,
//! `handler_nameserver.cpp`, `authentication.cpp`, `peer_persistence.cpp`,
//! `handler_masterserver.cpp`, `handler_gameserver.cpp`, `game.cpp` and `lobby.cpp`,
//! with the operation, parameter, event and error codes from Luxon's
//! `common_codes.hpp`. Every deliberate difference from Luxon comes from how the real
//! Photon Realtime client behaves; the main ones:
//! - Photon Chat is served by the master server: a connection whose Init names the app
//!   "chat" (what ChatClient sends to its front end) gets Chat's operations (chat.rs).
//! - AuthOnce clients get `EncryptionData` (payload encryption + a shared secret) from
//!   the name server and the master/game servers use that secret, as Photon Cloud does.
//! - Game-list updates go out as event 229 (the client treats 229 and 230 alike), every
//!   second instead of every five, and custom lobby properties trigger them too.
//! - A null property value with DeleteNullProps removes the property (Luxon checks the
//!   key instead), unknown byte-keyed room properties (e.g. 243 MaxPlayers as int) are
//!   kept, and expected users expire after 30 s.
//! - Leave doesn't make the server drop the connection; the client disconnects itself
//!   (avoids racing the Leave response with an ENet disconnect).
//! - CheckUserOnJoin only refuses a duplicate user id from the same IP address, so
//!   two LAN PCs that sign in as the same account can still play together.
//! SQL lobbies' filters (JoinRandomGame and GetGameList, WHERE and ORDER BY) are
//! evaluated by sql.rs over the rooms' lobby properties.
//! Inactive players: in a room with a PlayerTTL, an actor whose client drops (or leaves
//! saying it will come back) stays in the room, inactive, for that long, and the same
//! user takes it back with JoinMode Rejoin (Realtime's ReconnectAndRejoin / RejoinRoom).
//! Not supported: plugins, webhooks,
//! slice caches, datagram encryption, secure WebSockets (wss). TCP and WebSocket clients
//! are served through host.rs, which turns each connection into an ENet peer of its own:
//! here they're connections on socket indexes 6-8 (TCP) and 9-11 (WebSocket) whose
//! redirects name TCP ports, or `ws://` addresses.

use super::crypto::{random_bytes, Aes256, Dh};
use super::enet::{self, Peer, State};
use super::message::{parse_hello, Proto};
use super::sql;
use super::value::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};

// Operation codes (Luxon common_codes.hpp; the Photon Realtime client agrees).
pub mod op {
    pub const JOIN: u8 = 255;
    pub const LEAVE: u8 = 254;
    pub const RAISE_EVENT: u8 = 253;
    pub const SET_PROPERTIES: u8 = 252;
    pub const CHANGE_GROUPS: u8 = 248;
    pub const AUTHENTICATE_ONCE: u8 = 231;
    pub const AUTHENTICATE: u8 = 230;
    pub const JOIN_LOBBY: u8 = 229;
    pub const LEAVE_LOBBY: u8 = 228;
    pub const CREATE_GAME: u8 = 227;
    pub const JOIN_GAME: u8 = 226;
    pub const JOIN_RANDOM_GAME: u8 = 225;
    pub const FIND_FRIENDS: u8 = 222;
    pub const LOBBY_STATS: u8 = 221;
    pub const GET_REGIONS: u8 = 220;
    pub const SETTINGS: u8 = 218;
    pub const GET_GAME_LIST: u8 = 217;
}

pub mod ev {
    pub const JOIN: u8 = 255;
    pub const LEAVE: u8 = 254;
    pub const PROPERTIES_CHANGED: u8 = 253;
    pub const GAME_LIST: u8 = 230;
    pub const GAME_LIST_UPDATE: u8 = 229;
    pub const APP_STATS: u8 = 226;
    pub const LOBBY_STATS: u8 = 224;
}

pub mod key {
    pub const GAME_ID: u8 = 255;
    pub const ACTOR_NR: u8 = 254;
    pub const TARGET_ACTOR: u8 = 253;
    pub const ACTOR_LIST: u8 = 252;
    pub const PROPERTIES: u8 = 251;
    pub const BROADCAST: u8 = 250;
    pub const PLAYER_PROPERTIES: u8 = 249;
    pub const GAME_PROPERTIES: u8 = 248;
    pub const CACHE: u8 = 247;
    pub const RECEIVER_GROUP: u8 = 246;
    pub const DATA: u8 = 245;
    /// An SQL lobby's filter (JoinRandomGame, GetGameList), in the same slot.
    pub const SQL_FILTER: u8 = 245;
    pub const CODE: u8 = 244;
    pub const CLEANUP_CACHE_ON_LEAVE: u8 = 241;
    pub const GROUP: u8 = 240;
    pub const REMOVE: u8 = 239;
    pub const PUBLISH_USER_ID: u8 = 239;
    pub const ADD: u8 = 238;
    pub const SUPPRESS_ROOM_EVENTS: u8 = 237;
    pub const EMPTY_ROOM_TTL: u8 = 236;
    pub const PLAYER_TTL: u8 = 235;
    pub const CHECK_USER_ON_JOIN: u8 = 232;
    pub const EXPECTED_VALUES: u8 = 231;
    pub const ADDRESS: u8 = 230;
    pub const PEER_COUNT: u8 = 229;
    pub const GAME_COUNT: u8 = 228;
    pub const MASTER_PEER_COUNT: u8 = 227;
    pub const USER_ID: u8 = 225;
    pub const APPLICATION_ID: u8 = 224;
    pub const POSITION: u8 = 223;
    pub const MATCHMAKING_TYPE: u8 = 223;
    pub const GAME_LIST: u8 = 222;
    pub const TOKEN: u8 = 221;
    pub const APP_VERSION: u8 = 220;
    pub const JOIN_MODE: u8 = 215;
    pub const LOBBY_NAME: u8 = 213;
    pub const LOBBY_TYPE: u8 = 212;
    pub const LOBBY_STATS: u8 = 211;
    pub const REGION: u8 = 210;
    pub const PLUGINS: u8 = 204;
    pub const MASTER_CLIENT_ID: u8 = 203;
    pub const ENCRYPTION_DATA: u8 = 192;
    pub const GAME_FLAGS: u8 = 191;
    /// Leave: the player will come back; Leave event: the actor is inactive.
    pub const IS_INACTIVE: u8 = 233;
}

pub mod err {
    pub const OK: i16 = 0;
    pub const OPERATION_INVALID: i16 = -2;
    pub const NOT_ALLOWED: i16 = -3;
    pub const INVALID_PARAMETERS: i16 = -6;
    pub const INVALID_AUTHENTICATION: i16 = 32767;
    pub const GAME_ID_ALREADY_EXISTS: i16 = 32766;
    pub const GAME_FULL: i16 = 32765;
    pub const GAME_CLOSED: i16 = 32764;
    pub const SERVER_FORBIDDEN: i16 = 32761;
    pub const NO_RANDOM_MATCH: i16 = 32760;
    pub const GAME_DOES_NOT_EXIST: i16 = 32758;
    pub const TOKEN_EXPIRED: i16 = 32753;
    pub const ALREADY_JOINED: i16 = 32750;
    pub const FOUND_INACTIVE_JOINER: i16 = 32749;
    pub const REJOINER_NOT_FOUND: i16 = 32748;
    pub const FOUND_ACTIVE_JOINER: i16 = 32746;
    pub const ACTOR_LIST_FULL: i16 = 32734;
}

pub mod prop {
    pub const MAX_PLAYERS: u8 = 255;
    pub const IS_VISIBLE: u8 = 254;
    pub const IS_OPEN: u8 = 253;
    pub const PLAYER_COUNT: u8 = 252;
    pub const REMOVED: u8 = 251;
    pub const LOBBY_PROPERTIES: u8 = 250;
    pub const MASTER_CLIENT_ID: u8 = 248;
    pub const PLAYER_TTL: u8 = 246;
    pub const EMPTY_ROOM_TTL: u8 = 245;
    pub const MAX_PLAYERS_INT: u8 = 243;
    pub const ACTOR_USER_ID: u8 = 253;
    /// Actor property: inactive (left with a PlayerTTL, may rejoin).
    pub const ACTOR_INACTIVE: u8 = 254;
}

/// JoinMode Rejoin: take back this user's inactive actor.
const JOIN_MODE_REJOIN: u8 = 3;

pub mod flag {
    pub const CHECK_USER_ON_JOIN: u32 = 1;
    pub const DELETE_CACHE_ON_LEAVE: u32 = 2;
    pub const SUPPRESS_ROOM_EVENTS: u32 = 4;
    pub const PUBLISH_USER_ID: u32 = 8;
    pub const DELETE_NULL_PROPS: u32 = 16;
    pub const BROADCAST_PROPS_TO_ALL: u32 = 32;
    pub const SUPPRESS_PLAYER_INFO: u32 = 64;
}

/// The three servers, in socket order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Name,
    Master,
    Game,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Name, Role::Master, Role::Game];

    pub fn label(self) -> &'static str {
        match self {
            Role::Name => "name server",
            Role::Master => "master server",
            Role::Game => "game server",
        }
    }
}

/// UDP ports: the defaults Photon clients use, and the "alternative" set
/// (`UseAlternativeUdpPorts`).
pub const PORTS: [[u16; 3]; 2] = [[5058, 5055, 5056], [27000, 27001, 27002]];

/// TCP ports (`ConnectionProtocol.Tcp`): the name server's is the one Photon clients
/// dial (4533); master and game servers are named in redirects. Their connections use
/// socket indexes `TCP_BASE..TCP_BASE + 3`.
pub const TCP_PORTS: [u16; 3] = [4533, 4530, 4531];
pub const TCP_BASE: usize = 6;

/// WebSocket ports (`ConnectionProtocol.WebSocket`), as Photon Cloud's: name server
/// 9093 (and 80, where older Realtime clients dial it), master 9090, game 9091. Their
/// connections use socket indexes `WS_BASE..WS_BASE + 3`.
pub const WS_PORTS: [u16; 3] = [9093, 9090, 9091];
pub const WS_NAME_PORT_OLD: u16 = 80;
pub const WS_BASE: usize = 9;

/// Whether a socket index is a WebSocket one.
pub fn is_ws(index: usize) -> bool {
    index >= WS_BASE
}

pub fn socket_index(role: Role, alt: bool) -> usize {
    Role::ALL.iter().position(|r| *r == role).unwrap() + if alt { 3 } else { 0 }
}

pub fn socket_role(index: usize) -> (Role, bool) {
    (Role::ALL[index % 3], (3..TCP_BASE).contains(&index))
}

/// Whether a socket index is a TCP one.
pub fn is_tcp(index: usize) -> bool {
    index >= TCP_BASE
}

/// Region pings are 13 bytes, twelve 0x7D and a sequence byte, to be echoed back
/// unchanged (the client's PingMono; photon-server answers the same way).
pub fn is_region_ping(data: &[u8]) -> bool {
    data.len() == 13 && data[..12].iter().all(|b| *b == 0x7D)
}

/// What the client gets in GetRegions: the usual Photon Cloud region codes, all
/// pointing at this server, so games with a region whitelist still find one.
const REGIONS: [&str; 14] = ["eu", "us", "usw", "asia", "jp", "au", "sa", "cae", "kr", "in", "za", "uae", "tr", "hk"];

const TOKEN_GRACE: i64 = 30_000;
const PENDING_GAME_TTL: i64 = 30_000;
const LIST_UPDATE_EVERY: i64 = 1_000;
const APP_STATS_EVERY: i64 = 5_000;
const MAX_LIST: usize = 500;

type AppKey = (String, String);
type LobbyKey = (AppKey, String, u8);
type GameKey = (AppKey, String);

struct Persist {
    app: AppKey,
    user_id: String,
    secret: Option<[u8; 32]>,
    invite: Option<String>,
    refs: u32,
    released: i64,
}

struct Conn {
    sock: usize,
    addr: SocketAddr,
    enet: Peer,
    proto: Option<Proto>,
    init_app: String,
    aes: Option<Aes256>,
    token: Option<String>,
    user_id: String,
    app: Option<AppKey>,
    lobby: Option<LobbyKey>,
    pending_list: BTreeMap<String, Option<Table>>,
    game: Option<GameKey>,
    actor: i32,
    left: bool,
    kill_at: Option<i64>,
}

impl Conn {
    fn role(&self) -> Role {
        socket_role(self.sock).0
    }
}

struct Actor {
    conn: u64,
    nr: i32,
    props: Table,
    groups: [u64; 4],
    user_id: String,
    ip: IpAddr,
    /// Inactive until then (its client left with a PlayerTTL); `conn` is 0 meanwhile.
    inactive_until: Option<i64>,
}

impl Actor {
    fn in_group(&self, g: u8) -> bool {
        g == 0 || self.groups[(g / 64) as usize] & (1 << (g % 64)) != 0
    }
}

#[derive(Clone)]
struct Cached {
    code: u8,
    sender: i32,
    data: Option<Value>,
    channel: u8,
    reliable: bool,
}

struct Game {
    lobby: LobbyKey,
    created: bool,
    open: bool,
    visible: bool,
    max_players: u8,
    player_ttl: i32,
    empty_ttl: i32,
    master: i32,
    last_actor: i32,
    flags: u32,
    custom: Table,
    lobby_props: Vec<String>,
    actors: Vec<Actor>,
    expected: HashMap<String, i64>,
    cache: Vec<Cached>,
    keep_until: i64,
    /// Rooms that asked for Photon's Fusion plugin get PeerParty's (fusion.rs).
    fusion: Option<super::fusion::Plugin>,
}

impl Game {
    fn new(lobby: LobbyKey, now: i64) -> Game {
        Game {
            lobby,
            created: false,
            open: true,
            visible: true,
            max_players: 0,
            player_ttl: 0,
            empty_ttl: 0,
            master: 1,
            last_actor: 0,
            flags: flag::CHECK_USER_ON_JOIN | flag::DELETE_CACHE_ON_LEAVE,
            custom: Vec::new(),
            lobby_props: Vec::new(),
            actors: Vec::new(),
            expected: HashMap::new(),
            cache: Vec::new(),
            keep_until: now + PENDING_GAME_TTL,
            fusion: None,
        }
    }

    fn count(&self) -> u8 {
        self.actors.len().min(255) as u8
    }

    fn actor(&self, nr: i32) -> Option<&Actor> {
        self.actors.iter().find(|a| a.nr == nr)
    }

    fn actor_ids(&self) -> Vec<i32> {
        self.actors.iter().map(|a| a.nr).collect()
    }

    /// Luxon `Game::get_game_props`.
    fn props(&self) -> Table {
        let mut t = self.custom.clone();
        table_set(&mut t, Value::Byte(prop::MAX_PLAYERS), Value::Byte(self.max_players));
        table_set(&mut t, Value::Byte(prop::IS_VISIBLE), Value::Bool(self.visible));
        table_set(&mut t, Value::Byte(prop::IS_OPEN), Value::Bool(self.open));
        table_set(&mut t, Value::Byte(prop::PLAYER_TTL), Value::Int(self.player_ttl));
        table_set(&mut t, Value::Byte(prop::EMPTY_ROOM_TTL), Value::Int(self.empty_ttl));
        table_set(&mut t, Value::Byte(prop::MASTER_CLIENT_ID), Value::Int(self.master));
        table_set(&mut t, Value::Byte(prop::LOBBY_PROPERTIES), Value::Strs(self.lobby_props.clone()));
        table_set(&mut t, Value::Byte(prop::PLAYER_COUNT), Value::Byte(self.count()));
        t
    }

    /// Luxon `Game::get_lobby_game_props`, plus 243 (int MaxPlayers) when known.
    fn lobby_view(&self) -> Table {
        let mut t = vec![
            (Value::Byte(prop::PLAYER_COUNT), Value::Byte(self.count())),
            (Value::Byte(prop::IS_OPEN), Value::Bool(self.open)),
            (Value::Byte(prop::MAX_PLAYERS), Value::Byte(self.max_players)),
        ];
        if let Some(v) = table_get(&self.custom, &Value::Byte(prop::MAX_PLAYERS_INT)) {
            t.push((Value::Byte(prop::MAX_PLAYERS_INT), v.clone()));
        }
        for k in &self.lobby_props {
            let key = Value::Str(k.clone());
            if let Some(v) = table_get(&self.custom, &key) {
                t.push((key, v.clone()));
            }
        }
        t
    }

    fn prop(&self, k: &Value) -> Option<Value> {
        if let Value::Byte(b) = k {
            let v = match *b {
                prop::MAX_PLAYERS => Value::Byte(self.max_players),
                prop::IS_VISIBLE => Value::Bool(self.visible),
                prop::IS_OPEN => Value::Bool(self.open),
                prop::PLAYER_TTL => Value::Int(self.player_ttl),
                prop::EMPTY_ROOM_TTL => Value::Int(self.empty_ttl),
                prop::MASTER_CLIENT_ID => Value::Int(self.master),
                prop::LOBBY_PROPERTIES => Value::Strs(self.lobby_props.clone()),
                prop::PLAYER_COUNT => Value::Byte(self.count()),
                _ => return table_get(&self.custom, k).cloned(),
            };
            return Some(v);
        }
        table_get(&self.custom, k).cloned()
    }

    /// Luxon `Game::expect_game_props`: every expected value must match (a missing
    /// property matches null).
    /// A lobby property by name, for SQL filters.
    fn sql_prop(&self, name: &str) -> Option<Value> {
        self.custom.iter().find(|(k, _)| matches!(k, Value::Str(s) if s.eq_ignore_ascii_case(name))).map(|(_, v)| v.clone())
    }

    fn matches(&self, expected: &Table) -> bool {
        expected.iter().all(|(k, v)| match self.prop(k) {
            Some(have) => same(&have, v),
            None => v.is_null(),
        })
    }

    /// Luxon `Game::insert_game_props`. Returns (lobby list changed, became invisible).
    fn apply(&mut self, update: &Table) -> (bool, bool) {
        let delete_null = self.flags & flag::DELETE_NULL_PROPS != 0;
        let (mut list, mut hidden) = (false, false);
        for (k, v) in update {
            match (k, v) {
                // A byte from older clients; Photon Realtime 4.1.6+ (`RoomOptions.MaxPlayers`
                // is an int there; Fusion 1.1's) sends an int.
                (Value::Byte(prop::MAX_PLAYERS), v) if v.as_i64().is_some() => {
                    let n = v.as_i64().unwrap().clamp(0, 255) as u8;
                    list |= self.max_players != n;
                    self.max_players = n;
                }
                (Value::Byte(prop::IS_VISIBLE), Value::Bool(b)) => {
                    hidden |= self.visible && !*b;
                    list |= !self.visible && *b;
                    self.visible = *b;
                }
                (Value::Byte(prop::IS_OPEN), Value::Bool(b)) => {
                    list |= self.open != *b;
                    self.open = *b;
                }
                (Value::Byte(prop::PLAYER_TTL), v) => {
                    if let Some(n) = v.as_i32() {
                        self.player_ttl = n;
                    }
                }
                (Value::Byte(prop::EMPTY_ROOM_TTL), v) => {
                    if let Some(n) = v.as_i32() {
                        self.empty_ttl = n;
                    }
                }
                (Value::Byte(prop::MASTER_CLIENT_ID), v) => {
                    if let Some(n) = v.as_i32() {
                        if self.actor(n).is_some() {
                            self.master = n;
                        }
                    }
                }
                (Value::Byte(prop::LOBBY_PROPERTIES), Value::Strs(l)) => {
                    self.lobby_props = l.clone();
                    list = true;
                }
                (Value::Byte(prop::PLAYER_COUNT | prop::REMOVED), _) => {}
                (Value::Byte(prop::MAX_PLAYERS | prop::IS_VISIBLE | prop::IS_OPEN | prop::LOBBY_PROPERTIES), _) => {}
                (Value::Null, _) => {}
                (k, v) => {
                    if let Value::Str(s) = k {
                        list |= self.lobby_props.contains(s);
                    }
                    if v.is_null() && delete_null {
                        table_remove(&mut self.custom, k);
                    } else {
                        table_set(&mut self.custom, k.clone(), v.clone());
                    }
                }
            }
        }
        (list, hidden)
    }

    /// Luxon `Game::validate_join`, with expected users that expire and the
    /// CheckUserOnJoin relaxation for different IP addresses (see the module docs).
    fn validate_join(&self, user: &str, ip: IpAddr, now: i64) -> Result<(), (i16, &'static str)> {
        if !self.created {
            return Err((err::GAME_DOES_NOT_EXIST, "Game does not exist"));
        }
        if !self.open {
            return Err((err::GAME_CLOSED, "Game is closed"));
        }
        let reserved = self.expected.iter().filter(|(u, t)| **t > now && u.as_str() != user).count();
        if self.max_players > 0 && self.actors.len() + reserved + 1 > self.max_players as usize {
            return Err((err::GAME_FULL, "Game is full"));
        }
        if self.actors.len() + 1 > 254 {
            return Err((err::ACTOR_LIST_FULL, "Game is full"));
        }
        if self.flags & flag::CHECK_USER_ON_JOIN != 0 && self.actors.iter().any(|a| a.user_id == user && a.ip == ip && a.inactive_until.is_none()) {
            return Err((err::ALREADY_JOINED, "Game already joined"));
        }
        if !user.is_empty() && self.actors.iter().any(|a| a.user_id == user && a.inactive_until.is_some()) {
            return Err((err::FOUND_INACTIVE_JOINER, "Found inactive UserId"));
        }
        Ok(())
    }

    /// The inactive actor `user` can take back.
    fn validate_rejoin(&self, user: &str) -> Result<i32, (i16, &'static str)> {
        if !self.created {
            return Err((err::GAME_DOES_NOT_EXIST, "Game does not exist"));
        }
        match self.actors.iter().find(|a| !user.is_empty() && a.user_id == user) {
            Some(a) if a.inactive_until.is_some() => Ok(a.nr),
            Some(_) => Err((err::FOUND_ACTIVE_JOINER, "Found active UserId")),
            None => Err((err::REJOINER_NOT_FOUND, "UserId not found")),
        }
    }

    fn is_active(&self, nr: i32) -> bool {
        self.actor(nr).is_some_and(|a| a.inactive_until.is_none())
    }
}

/// Value equality for property matching: numbers compare by value whatever their
/// width (clients send MaxPlayers as byte or int).
fn same(a: &Value, b: &Value) -> bool {
    let num = |v: &Value| if matches!(v, Value::Bool(_)) { None } else { v.as_i64() };
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

#[derive(Default)]
struct Lobby {
    members: BTreeSet<u64>,
    games: BTreeSet<String>,
}

enum Receivers {
    Others,
    All,
    Master,
    Actors(Vec<i32>),
}

/// A datagram to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Out {
    pub sock: usize,
    pub to: SocketAddr,
    pub data: Vec<u8>,
}

pub struct Config {
    /// The UDP ports actually bound, per socket index (normally [`PORTS`]).
    pub ports: [[u16; 3]; 2],
    /// The TCP ports listened on, per role (0 where none).
    pub tcp_ports: [u16; 3],
    /// The WebSocket ports listened on, per role (0 where none).
    pub ws_ports: [u16; 3],
    /// The host part of server addresses handed to a client at `ip`.
    /// The address of this PC to give a client at this address (its other end).
    pub advertise: Box<dyn Fn(std::net::SocketAddr) -> String + Send>,
    pub log: Box<dyn Fn(&str) + Send>,
}

/// `PEERPARTY_PHOTON_TRACE=1`: game server operations, events and Fusion packets go to
/// the log (for finding why a game leaves a room).
fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PEERPARTY_PHOTON_TRACE").is_ok_and(|v| v == "1"))
}

/// Fusion's packets are logged without the variable too, the first 80 of a run: a
/// Shared mode session that fails, fails in its first few (one seen left 7 ms after its
/// connection was accepted).
fn fusion_trace() -> bool {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    trace() || N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 80
}

/// A packet's length and first 160 bytes, in hex.
fn head(b: &[u8]) -> String {
    let hex: Vec<String> = b.iter().take(160).map(|x| format!("{x:02x}")).collect();
    format!("{} bytes {}{}", b.len(), hex.join(" "), if b.len() > 160 { " ..." } else { "" })
}

fn resp(code: u8, ret: i16, debug: Option<&str>, params: Params) -> Response {
    Response { code, ret, debug: debug.map(str::to_owned), params }
}

fn random_string(n: usize, charset: &[u8]) -> String {
    random_bytes(n).iter().map(|b| charset[*b as usize % charset.len()] as char).collect()
}

fn new_token() -> String {
    random_string(32, b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz")
}

pub struct Server {
    cfg: Config,
    conns: HashMap<u64, Conn>,
    by_addr: HashMap<(usize, SocketAddr), u64>,
    next_conn: u64,
    next_peer_id: [i16; WS_BASE + 3],
    tokens: HashMap<String, Persist>,
    games: HashMap<GameKey, Game>,
    lobbies: HashMap<LobbyKey, Lobby>,
    chat: super::chat::Chat,
    last_list: i64,
    last_stats: i64,
    out: Vec<Out>,
}

impl Server {
    pub fn new(cfg: Config) -> Server {
        Server {
            cfg,
            conns: HashMap::new(),
            by_addr: HashMap::new(),
            next_conn: 1,
            next_peer_id: [0; WS_BASE + 3],
            tokens: HashMap::new(),
            games: HashMap::new(),
            lobbies: HashMap::new(),
            chat: super::chat::Chat::default(),
            last_list: 0,
            last_stats: 0,
            out: Vec::new(),
        }
    }

    fn log(&self, s: &str) {
        (self.cfg.log)(s)
    }

    /// Tells the overlay who is in this room now (an empty room is gone).
    fn publish_room(&self, gk: &GameKey) {
        let Some(g) = self.games.get(gk) else { return };
        let addrs: Vec<SocketAddr> = g
            .actors
            .iter()
            .filter(|a| a.inactive_until.is_none())
            .map(|a| self.conns.get(&a.conn).map_or(SocketAddr::new(a.ip, 0), |c| c.addr))
            .collect();
        super::room_changed(&gk.1, addrs, g.max_players);
    }

    pub fn connection_count(&self) -> usize {
        self.conns.len()
    }

    fn address(&self, c: &Conn, role: Role) -> String {
        let alt = socket_role(c.sock).1;
        if is_ws(c.sock) {
            return format!("ws://{}:{}", (self.cfg.advertise)(c.addr), self.cfg.ws_ports[socket_index(role, false)]);
        }
        let port = if is_tcp(c.sock) {
            self.cfg.tcp_ports[socket_index(role, false)]
        } else {
            self.cfg.ports[alt as usize][socket_index(role, false)]
        };
        format!("{}:{}", (self.cfg.advertise)(c.addr), port)
    }

    /// One datagram that arrived on socket `sock`. Returns what to send now.
    pub fn on_datagram(&mut self, sock: usize, from: SocketAddr, data: &[u8], now: i64) -> Vec<Out> {
        let Ok((hdr, cmds)) = enet::parse(data) else { return Vec::new() };
        let id = match self.by_addr.get(&(sock, from)) {
            Some(id) => *id,
            None => {
                let n = &mut self.next_peer_id[sock];
                *n = if *n >= 32000 { 1 } else { *n + 1 };
                let Some(peer) = Peer::accept(&hdr, &cmds, *n, now) else { return Vec::new() };
                let id = self.next_conn;
                self.next_conn += 1;
                self.conns.insert(
                    id,
                    Conn {
                        sock,
                        addr: from,
                        enet: peer,
                        proto: None,
                        init_app: String::new(),
                        aes: None,
                        token: None,
                        user_id: String::new(),
                        app: None,
                        lobby: None,
                        pending_list: BTreeMap::new(),
                        game: None,
                        actor: 0,
                        left: false,
                        kill_at: None,
                    },
                );
                self.by_addr.insert((sock, from), id);
                id
            }
        };
        if let Some(c) = self.conns.get_mut(&id) {
            c.enet.handle(&hdr, cmds, now);
        }
        while let Some(d) = self.conns.get_mut(&id).and_then(|c| c.enet.dispatch()) {
            self.on_message(id, d, now);
        }
        self.flush(now)
    }

    /// Timers: resends, pings, timeouts, list updates, stats, expiry.
    pub fn tick(&mut self, now: i64) -> Vec<Out> {
        let due: Vec<u64> =
            self.conns.iter().filter(|(_, c)| c.kill_at.is_some_and(|t| now >= t)).map(|(i, _)| *i).collect();
        for id in due {
            if let Some(c) = self.conns.get_mut(&id) {
                let (sock, to) = (c.sock, c.addr);
                for d in c.enet.disconnect(now) {
                    self.out.push(Out { sock, to, data: d });
                }
            }
        }
        if now - self.last_list >= LIST_UPDATE_EVERY {
            self.last_list = now;
            self.send_list_updates();
        }
        if now - self.last_stats >= APP_STATS_EVERY {
            self.last_stats = now;
            self.send_app_stats();
        }
        self.expire(now);
        let keys: Vec<GameKey> = self.games.iter().filter(|(_, g)| g.fusion.is_some()).map(|(k, _)| k.clone()).collect();
        for gk in keys {
            let mut lines = Vec::new();
            let out = self.games.get_mut(&gk).and_then(|g| g.fusion.as_mut()).map(|f| f.tick(now, &mut lines)).unwrap_or_default();
            for l in lines {
                self.log(&l);
            }
            self.send_relay(&gk, out);
        }
        self.flush(now)
    }

    /// Fusion Shared server packets, as event 101 from actor 0 (unreliable).
    fn send_relay(&mut self, gk: &GameKey, out: super::fusion::Out) {
        for (to, packet) in out {
            if fusion_trace() {
                self.log(&format!("photon-trace relay 0 -> {to}: {}", head(&packet)));
            }
            // The sender (actor 0) is said outright: Fusion takes a packet's address from it.
            let params = vec![(key::ACTOR_NR, Value::Int(0)), (key::DATA, Value::Bytes(packet))];
            let e = EventMsg { code: super::fusion::RELAY_EVENT, params };
            self.deliver(gk, 0, Receivers::Actors(vec![to]), &e, 0, false);
        }
    }

    /// When `tick` next has work.
    pub fn next_tick(&self, now: i64) -> i64 {
        let mut t = now + 250;
        for c in self.conns.values() {
            t = t.min(c.enet.next_deadline(now));
            if let Some(k) = c.kill_at {
                t = t.min(k);
            }
        }
        for g in self.games.values() {
            if let Some(n) = g.fusion.as_ref().and_then(|f| f.next_tick(now)) {
                t = t.min(n);
            }
        }
        t.max(now + 1)
    }

    fn flush(&mut self, now: i64) -> Vec<Out> {
        let mut dead = Vec::new();
        for (id, c) in self.conns.iter_mut() {
            let (sock, to) = (c.sock, c.addr);
            for d in c.enet.flush(now) {
                self.out.push(Out { sock, to, data: d });
            }
            if matches!(c.enet.state, State::Disconnected | State::TimedOut) {
                dead.push(*id);
            }
        }
        for id in dead {
            self.drop_conn(id, now);
        }
        // Leave events from dropped players.
        for c in self.conns.values_mut() {
            let (sock, to) = (c.sock, c.addr);
            if c.enet.has_outgoing() {
                for d in c.enet.flush(now) {
                    self.out.push(Out { sock, to, data: d });
                }
            }
        }
        std::mem::take(&mut self.out)
    }

    fn drop_conn(&mut self, id: u64, now: i64) {
        let Some(mut c) = self.conns.remove(&id) else { return };
        self.by_addr.remove(&(c.sock, c.addr));
        let why = if c.enet.state == State::TimedOut { "timed out" } else { "disconnected" };
        self.log(&format!("photon: {} client {} {why}", c.role().label(), c.addr));
        if let Some(t) = c.token.as_ref().and_then(|t| self.tokens.get_mut(t)) {
            t.refs = t.refs.saturating_sub(1);
            t.released = now;
        }
        if let Some(l) = c.lobby.take().and_then(|k| self.lobbies.get_mut(&k)) {
            l.members.remove(&id);
        }
        for (to, e) in self.chat.gone(id) {
            self.send(to, &Msg::Event(e), 0, true, false);
        }
        if let Some(g) = c.game.clone() {
            if c.actor != 0 && !c.left {
                self.actor_gone(&g, c.actor, now, true);
            }
        }
    }

    fn expire(&mut self, now: i64) {
        self.tokens.retain(|_, p| p.refs > 0 || now - p.released < TOKEN_GRACE);
        // Inactive actors whose PlayerTTL ran out leave for good.
        let lapsed: Vec<(GameKey, i32)> = self
            .games
            .iter()
            .flat_map(|(k, g)| g.actors.iter().filter(|a| a.inactive_until.is_some_and(|t| now >= t)).map(move |a| (k.clone(), a.nr)))
            .collect();
        for (k, nr) in lapsed {
            self.remove_actor(&k, nr, now);
        }
        let mut gone = Vec::new();
        for (k, g) in self.games.iter_mut() {
            g.expected.retain(|_, t| *t > now);
            if g.actors.is_empty() && now >= g.keep_until {
                gone.push(k.clone());
            }
        }
        for k in gone {
            let g = self.games.remove(&k).unwrap();
            if g.created {
                self.log(&format!("photon: room '{}' closed", k.1));
            }
            if let Some(l) = self.lobbies.get_mut(&g.lobby) {
                l.games.remove(&k.1);
            }
            if g.created && g.visible {
                self.queue_list_change(&g.lobby, &k.1, None);
            }
        }
        self.lobbies.retain(|_, l| !l.members.is_empty() || !l.games.is_empty());
    }

    // ------------------------------------------------------------ sending

    fn send(&mut self, id: u64, msg: &Msg, channel: u8, reliable: bool, encrypt: bool) {
        let Some(c) = self.conns.get_mut(&id) else { return };
        let Some(p) = c.proto else { return };
        let key = if encrypt { c.aes.as_ref() } else { None };
        match p.encode(msg, key) {
            Ok(bytes) => {
                c.enet.send(channel, &bytes, reliable, false);
            }
            Err(e) => {
                let s = format!("photon: couldn't encode a message for {}: {e}", c.addr);
                self.log(&s);
            }
        }
    }

    fn respond(&mut self, id: u64, r: Response, channel: u8, encrypt: bool) {
        self.send(id, &Msg::Response(r), channel, true, encrypt);
    }

    /// Kick after the response has had time to arrive.
    fn kill_later(&mut self, id: u64, now: i64) {
        if let Some(c) = self.conns.get_mut(&id) {
            c.kill_at = Some(now + 500);
        }
    }

    // ------------------------------------------------------------ messages

    fn on_message(&mut self, id: u64, d: enet::Delivery, now: i64) {
        let Some(c) = self.conns.get(&id) else { return };
        if c.kill_at.is_some() {
            return;
        }
        let Some(proto) = c.proto else {
            return self.on_hello(id, &d, now);
        };
        let (msg, encrypted) = match proto.decode(&d.payload, c.aes.as_ref()) {
            Ok(m) => m,
            Err(e) => {
                let s = format!("photon: unreadable message from {}: {e}", c.addr);
                return self.log(&s);
            }
        };
        match msg {
            Msg::Request(req) => self.on_op(id, req, d.channel, d.reliable, encrypted, now),
            Msg::InternalRequest(req) => self.on_internal(id, req, d.channel),
            _ => {}
        }
    }

    fn on_hello(&mut self, id: u64, d: &enet::Delivery, now: i64) {
        let addr = self.conns[&id].addr;
        let hello = match parse_hello(&d.payload) {
            Ok(h) => h,
            Err(e) => {
                self.log(&format!("photon: {addr} didn't start with an Init ({e})"));
                self.conns.get_mut(&id).unwrap().kill_at = Some(now);
                return;
            }
        };
        let Some(proto) = Proto::from_version(hello.init.proto_major, hello.init.proto_minor) else {
            self.log(&format!(
                "photon: {addr} wants protocol {}.{}, which isn't supported",
                hello.init.proto_major, hello.init.proto_minor
            ));
            self.conns.get_mut(&id).unwrap().kill_at = Some(now);
            return;
        };
        let c = self.conns.get_mut(&id).unwrap();
        c.proto = Some(proto);
        c.init_app = hello.init.app_id.clone();
        let v = hello.init.version;
        let s = format!(
            "photon: {} client {addr} connected (app '{}', {}, sdk {} v{}.{}.{}.{}{})",
            c.role().label(),
            hello.init.app_id,
            proto.name(),
            hello.init.sdk,
            v[0],
            v[1],
            v[2],
            v[3],
            if hello.token.is_some() { ", token init" } else { "" }
        );
        self.log(&s);
        self.send(id, &Msg::InitResponse, d.channel, true, false);
        if let Some(token) = hello.token {
            // The AuthOnce modes carry the token in the Init; answer as if it came
            // in an AuthenticateOnce request (Luxon `HandleHTTPRequest`).
            let req = Request { code: op::AUTHENTICATE_ONCE, params: vec![(key::TOKEN, Value::Str(token))] };
            self.on_op(id, req, 0, true, false, now);
        }
    }

    /// Luxon `HandleInternalOperationRequest`: key exchange (0) and ping (1).
    fn on_internal(&mut self, id: u64, req: Request, channel: u8) {
        if channel != 0 {
            return;
        }
        let r = match req.code {
            0 => match param(&req.params, 1).and_then(Value::as_bytes) {
                None => resp(0, 2, Some("missing client public key param 1"), vec![]),
                Some(client_pub) => {
                    let dh = Dh::new();
                    match dh.derive(client_pub) {
                        None => resp(0, 3, Some("bad client public key"), vec![]),
                        Some(key) => {
                            let c = self.conns.get_mut(&id).unwrap();
                            let r = resp(0, 0, None, vec![(1, Value::Bytes(dh.public.clone()))]);
                            let addr = c.addr;
                            let p = c.proto.unwrap();
                            // Answer in the clear, then switch the key on.
                            if let Ok(b) = p.encode(&Msg::InternalResponse(r), None) {
                                c.enet.send(0, &b, true, false);
                            }
                            c.aes = Some(Aes256::new(&key));
                            self.log(&format!("photon: encryption established with {addr}"));
                            return;
                        }
                    }
                }
            },
            1 => {
                let ts = param(&req.params, 1).cloned().unwrap_or(Value::Int(0));
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                resp(1, 0, None, vec![(1, ts), (2, Value::Int(ms as i32))])
            }
            other => resp(other, err::OPERATION_INVALID, Some(&format!("Unsupported internal operation {other}")), vec![]),
        };
        self.send(id, &Msg::InternalResponse(r), 0, true, false);
    }

    fn on_op(&mut self, id: u64, req: Request, channel: u8, reliable: bool, encrypted: bool, now: i64) {
        let Some(c) = self.conns.get(&id) else { return };
        let role = c.role();
        if trace() && req.code != op::RAISE_EVENT {
            let keys: Vec<String> = req.params.iter().map(|(k, _)| k.to_string()).collect();
            let addr = c.addr;
            self.log(&format!("photon-trace {} {addr} op {} channel {channel} keys [{}]", role.label(), req.code, keys.join(",")));
        }
        let authed = c.token.is_some();
        if !authed {
            if channel == 0 {
                match (req.code, role) {
                    (op::AUTHENTICATE | op::AUTHENTICATE_ONCE, _) => return self.on_auth(id, req, encrypted, now),
                    (op::GET_REGIONS, Role::Name) => return self.on_get_regions(id),
                    _ => {}
                }
            }
        } else if role == Role::Game && req.code == op::RAISE_EVENT {
            return self.on_raise_event(id, req, channel, reliable, now);
        } else if self.chat.is_chat(id) && req.code != op::AUTHENTICATE && req.code != op::AUTHENTICATE_ONCE {
            return self.on_chat(id, &req, channel);
        } else if channel == 0 {
            match (role, req.code) {
                (Role::Master, op::JOIN_LOBBY) => return self.on_join_lobby(id, &req),
                (Role::Master, op::LEAVE_LOBBY) => return self.on_leave_lobby(id),
                (Role::Master, op::LOBBY_STATS) => return self.on_lobby_stats(id, &req),
                (Role::Master, op::CREATE_GAME | op::JOIN_GAME | op::JOIN_RANDOM_GAME) => {
                    return self.on_matchmaking(id, &req, now);
                }
                (Role::Master, op::FIND_FRIENDS) => return self.on_find_friends(id, &req),
                (Role::Master, op::GET_GAME_LIST) => return self.on_get_game_list(id, &req),
                (Role::Master | Role::Game, op::SETTINGS) => {
                    return self.respond(id, resp(op::SETTINGS, err::OK, None, vec![]), 0, false);
                }
                (Role::Game, op::JOIN | op::CREATE_GAME | op::JOIN_GAME) => return self.on_gs_join(id, &req, now),
                (Role::Game, op::LEAVE) => return self.on_leave(id, &req, now),
                (Role::Game, op::SET_PROPERTIES) => return self.on_set_properties(id, &req),
                (Role::Game, op::CHANGE_GROUPS) => return self.on_change_groups(id, &req),
                _ => {}
            }
        }
        // Luxon `HandlerBase::HandleOperationRequest`: only channel 0 gets answers.
        if channel != 0 {
            return;
        }
        let r = if authed && (req.code == op::AUTHENTICATE || req.code == op::AUTHENTICATE_ONCE) {
            resp(req.code, err::NOT_ALLOWED, Some("Already authenticated"), vec![])
        } else {
            let addr = self.conns[&id].addr;
            self.log(&format!("photon: {} {addr} sent unsupported operation {}", role.label(), req.code));
            resp(req.code, err::OPERATION_INVALID, Some(&format!("Unsupported operation {}", req.code)), vec![])
        };
        self.respond(id, r, 0, false);
    }

    /// A Photon Chat operation (chat.rs): its events, then the response.
    fn on_chat(&mut self, id: u64, req: &Request, channel: u8) {
        let (ret, events) = self.chat.op(id, req);
        for (to, e) in events {
            self.send(to, &Msg::Event(e), 0, true, false);
        }
        if ret == err::OPERATION_INVALID {
            let addr = self.conns[&id].addr;
            self.log(&format!("photon: chat client {addr} sent unsupported operation {}", req.code));
        }
        if channel == 0 {
            self.respond(id, resp(req.code, ret, None, vec![]), 0, false);
        }
    }

    fn on_get_regions(&mut self, id: u64) {
        let addr = self.address(&self.conns[&id], Role::Master);
        let r = resp(
            op::GET_REGIONS,
            err::OK,
            None,
            vec![
                (key::REGION, Value::Strs(REGIONS.iter().map(|s| s.to_string()).collect())),
                (key::ADDRESS, Value::Strs(vec![addr; REGIONS.len()])),
            ],
        );
        self.respond(id, r, 0, false);
    }

    fn refuse(&mut self, id: u64, code: u8, ret: i16, why: &str, encrypted: bool, now: i64) {
        self.respond(id, resp(code, ret, Some(why), vec![]), 0, encrypted);
        self.kill_later(id, now);
    }

    /// Luxon `authenticate`, accepting every client, with the user id they asked for or a
    /// random one. Tokens carry the user and room between servers.
    fn on_auth(&mut self, id: u64, req: Request, encrypted: bool, now: i64) {
        let c = &self.conns[&id];
        let (role, addr, code) = (c.role(), c.addr, req.code);
        let chat = role == Role::Master && c.init_app.eq_ignore_ascii_case("chat");
        if role == Role::Game && c.init_app.eq_ignore_ascii_case("chat") {
            return self.refuse(id, code, err::INVALID_AUTHENTICATION, "Photon Chat is served by the master server", encrypted, now);
        }
        let token = match param(&req.params, key::TOKEN) {
            Some(Value::Str(t)) if self.tokens.contains_key(t) => t.clone(),
            Some(Value::Str(_)) => {
                self.log(&format!("photon: {} {addr} used an unknown or expired token", role.label()));
                return self.refuse(id, code, err::TOKEN_EXPIRED, "Authentication token expired", encrypted, now);
            }
            Some(_) => return self.refuse(id, code, err::INVALID_PARAMETERS, "Token must be a string", encrypted, now),
            None => {
                let Some(app_id) = param(&req.params, key::APPLICATION_ID).and_then(Value::as_str) else {
                    return self.refuse(id, code, err::INVALID_AUTHENTICATION, "Invalid app id", encrypted, now);
                };
                let version = param(&req.params, key::APP_VERSION).and_then(Value::as_str).unwrap_or("").to_owned();
                let user_id = match param(&req.params, key::USER_ID).and_then(Value::as_str) {
                    Some(u) if !u.is_empty() => u.to_owned(),
                    _ => random_string(32, b"0123456789ABCDEF"),
                };
                let t = new_token();
                let secret = (code == op::AUTHENTICATE_ONCE).then(|| random_bytes(32).try_into().unwrap());
                let p = Persist { app: (app_id.to_owned(), version), user_id, secret, invite: None, refs: 0, released: now };
                self.tokens.insert(t.clone(), p);
                t
            }
        };
        let p = self.tokens.get_mut(&token).unwrap();
        p.refs += 1;
        let (app, user_id, secret, invite) = (p.app.clone(), p.user_id.clone(), p.secret, p.invite.clone());
        let c = self.conns.get_mut(&id).unwrap();
        c.token = Some(token.clone());
        c.user_id = user_id.clone();
        c.app = Some(app.clone());
        if role != Role::Name {
            if let Some(s) = secret {
                // AuthOnce: payload encryption with the secret the name server handed
                // out (Photon Cloud behaviour; Luxon leaves this out).
                c.aes = Some(Aes256::new(&s));
            }
        }
        let mut params = vec![];
        match role {
            Role::Name => {
                let master = self.address(&self.conns[&id], Role::Master);
                params.push((key::USER_ID, Value::Str(user_id.clone())));
                params.push((key::ADDRESS, Value::Str(master)));
                params.push((key::TOKEN, Value::Str(token)));
                if let Some(s) = secret {
                    let data = vec![(Value::Byte(0), Value::Byte(0)), (Value::Byte(1), Value::Bytes(s.to_vec()))];
                    params.push((key::ENCRYPTION_DATA, Value::Dict(DictTy { key: Ty::Byte, val: Ty::Any }, data)));
                }
            }
            Role::Master => {
                params.push((key::POSITION, Value::Int(0)));
                params.push((key::TOKEN, Value::Str(token)));
                params.push((key::USER_ID, Value::Str(user_id.clone())));
            }
            Role::Game => {
                params.push((key::POSITION, Value::Int(0)));
                match invite.map(|g| (app.clone(), g)).filter(|k| self.games.contains_key(k)) {
                    Some(k) => self.conns.get_mut(&id).unwrap().game = Some(k),
                    None => {
                        self.log(&format!("photon: game server client {addr} isn't invited to a room"));
                        return self.refuse(id, code, err::SERVER_FORBIDDEN, "Not invited to any game server", encrypted, now);
                    }
                }
            }
        }
        if chat {
            self.chat.signed_in(id, &app.0, &user_id);
        }
        self.log(&format!(
            "photon: {} {addr} signed in as '{user_id}' (app '{}' version '{}'){}",
            if chat { "chat front end" } else { role.label() },
            app.0,
            app.1,
            if chat { ", Photon Chat" } else { "" }
        ));
        self.respond(id, resp(code, err::OK, None, params), 0, encrypted);
        if role == Role::Master && param(&req.params, key::LOBBY_STATS).and_then(Value::as_bool) == Some(true) {
            let stats = self.lobby_stats(&app, |_| true);
            self.send(id, &Msg::Event(EventMsg { code: ev::LOBBY_STATS, params: stats }), 0, false, false);
        }
    }

    // ------------------------------------------------------------ master server

    fn lobby_key(&self, id: u64, params: &Params) -> LobbyKey {
        let c = &self.conns[&id];
        let app = c.app.clone().unwrap_or_default();
        match param(params, key::LOBBY_NAME).and_then(Value::as_str) {
            Some(name) => {
                let ty = param(params, key::LOBBY_TYPE).and_then(Value::as_u8).unwrap_or(0);
                (app, name.to_owned(), ty)
            }
            // No lobby named: the one the client is in, else the default lobby
            // (Photon's rule for CreateRoom / JoinRandom).
            None => c.lobby.clone().filter(|_| param(params, key::LOBBY_TYPE).is_none()).unwrap_or((app, String::new(), 0)),
        }
    }

    /// The lobby's visible rooms, joinable first, then full, then closed (Luxon
    /// `Lobby::get_game_list`), at most 500.
    fn game_list(&self, lk: &LobbyKey) -> Table {
        self.game_list_where(lk, None)
    }

    /// The lobby's rooms, those an SQL filter passes when there is one (in its order).
    fn game_list_where(&self, lk: &LobbyKey, filter: Option<&sql::Filter>) -> Table {
        let Some(l) = self.lobbies.get(lk) else { return Vec::new() };
        let mut rows: Vec<(u8, &String, &Game)> = l
            .games
            .iter()
            .filter_map(|n| self.games.get(&(lk.0.clone(), n.clone())).map(|g| (n, g)))
            .filter(|(_, g)| g.created && g.visible)
            .filter(|(_, g)| filter.is_none_or(|f| f.matches(&|c| g.sql_prop(c))))
            .map(|(n, g)| {
                let full = g.max_players > 0 && g.actors.len() >= g.max_players as usize;
                (if !g.open { 2 } else if full { 1 } else { 0 }, n, g)
            })
            .collect();
        match filter.filter(|f| f.has_order()) {
            Some(f) => rows.sort_by(|a, b| f.compare(&|c| a.2.sql_prop(c), &|c| b.2.sql_prop(c))),
            None => rows.sort_by_key(|r| r.0),
        }
        rows.into_iter().take(MAX_LIST).map(|(_, n, g)| (Value::Str(n.clone()), Value::Hashtable(g.lobby_view()))).collect()
    }

    fn on_join_lobby(&mut self, id: u64, req: &Request) {
        let app = self.conns[&id].app.clone().unwrap_or_default();
        let name = param(&req.params, key::LOBBY_NAME).and_then(Value::as_str).unwrap_or("").to_owned();
        let ty = param(&req.params, key::LOBBY_TYPE).and_then(Value::as_u8).unwrap_or(0);
        let lk = (app, name, ty);
        if let Some(old) = self.conns.get_mut(&id).unwrap().lobby.take() {
            if let Some(l) = self.lobbies.get_mut(&old) {
                l.members.remove(&id);
            }
        }
        self.lobbies.entry(lk.clone()).or_default().members.insert(id);
        let c = self.conns.get_mut(&id).unwrap();
        c.lobby = Some(lk.clone());
        c.pending_list.clear();
        let addr = c.addr;
        self.log(&format!("photon: {addr} joined lobby '{}' (type {})", lk.1, lk.2));
        self.respond(id, resp(op::JOIN_LOBBY, err::OK, None, vec![]), 0, false);
        // Only the default lobby type pushes room lists (SQL lobbies use GetGameList,
        // async-random lobbies have none).
        if ty == 0 {
            let list = self.game_list(&lk);
            let e = EventMsg { code: ev::GAME_LIST, params: vec![(key::GAME_LIST, Value::Hashtable(list))] };
            self.send(id, &Msg::Event(e), 0, true, false);
        }
    }

    fn on_leave_lobby(&mut self, id: u64) {
        if let Some(old) = self.conns.get_mut(&id).unwrap().lobby.take() {
            if let Some(l) = self.lobbies.get_mut(&old) {
                l.members.remove(&id);
            }
        }
        self.respond(id, resp(op::LEAVE_LOBBY, err::OK, None, vec![]), 0, false);
    }

    fn on_get_game_list(&mut self, id: u64, req: &Request) {
        let lk = self.lobby_key(id, &req.params);
        let filter = match param(&req.params, key::SQL_FILTER).and_then(Value::as_str).map(sql::Filter::parse) {
            Some(Err(why)) => {
                self.log(&format!("photon: GetGameList SQL filter refused: {why}"));
                return self.respond(id, resp(op::GET_GAME_LIST, err::INVALID_PARAMETERS, Some("Invalid SQL filter"), vec![]), 0, false);
            }
            Some(Ok(f)) => Some(f),
            None => None,
        };
        let list = self.game_list_where(&lk, filter.as_ref());
        self.respond(id, resp(op::GET_GAME_LIST, err::OK, None, vec![(key::GAME_LIST, Value::Hashtable(list))]), 0, false);
    }

    /// Luxon `MasterServer::get_lobby_stats`: player and room counts per lobby.
    fn lobby_stats(&self, app: &AppKey, want: impl Fn(&(String, u8)) -> bool) -> Params {
        let mut keys: Vec<&LobbyKey> = self.lobbies.keys().filter(|k| &k.0 == app && want(&(k.1.clone(), k.2))).collect();
        keys.sort_by(|a, b| (&a.1, a.2).cmp(&(&b.1, b.2)));
        let (mut names, mut types, mut peers, mut counts) = (vec![], vec![], vec![], vec![]);
        for k in keys {
            let l = &self.lobbies[k];
            let games: Vec<&Game> = l.games.iter().filter_map(|n| self.games.get(&(app.clone(), n.clone()))).collect();
            names.push(k.1.clone());
            types.push(k.2);
            peers.push((l.members.len() + games.iter().map(|g| g.actors.len()).sum::<usize>()) as i32);
            counts.push(games.iter().filter(|g| g.created).count() as i32);
        }
        vec![
            (key::PEER_COUNT, Value::Ints(peers)),
            (key::GAME_COUNT, Value::Ints(counts)),
            (key::LOBBY_TYPE, Value::Bytes(types)),
            (key::LOBBY_NAME, Value::Strs(names)),
        ]
    }

    fn on_lobby_stats(&mut self, id: u64, req: &Request) {
        let app = self.conns[&id].app.clone().unwrap_or_default();
        let names = param(&req.params, key::LOBBY_NAME).and_then(Value::as_strs);
        let types = param(&req.params, key::LOBBY_TYPE).and_then(Value::as_bytes).map(<[u8]>::to_vec);
        let params = match (names, types) {
            (Some(n), Some(t)) => {
                let want: HashSet<(String, u8)> = n.into_iter().zip(t).collect();
                self.lobby_stats(&app, |k| want.contains(k))
            }
            _ => self.lobby_stats(&app, |_| true),
        };
        self.respond(id, resp(op::LOBBY_STATS, err::OK, None, params), 0, false);
    }

    fn on_find_friends(&mut self, id: u64, req: &Request) {
        let users = param(&req.params, 1).and_then(Value::as_strs).unwrap_or_default();
        let (mut online, mut rooms) = (vec![], vec![]);
        for u in &users {
            let conns: Vec<&Conn> = self.conns.values().filter(|c| &c.user_id == u && c.kill_at.is_none()).collect();
            online.push(!conns.is_empty());
            let room = conns.iter().find(|c| c.actor != 0 && !c.left).and_then(|c| c.game.as_ref()).map(|g| g.1.clone());
            rooms.push(room.unwrap_or_default());
        }
        let r = resp(op::FIND_FRIENDS, err::OK, None, vec![(1, Value::Bools(online)), (2, Value::Strs(rooms))]);
        self.respond(id, r, 0, false);
    }

    /// Luxon `MasterServer::handle_create_game` / `handle_join_game` /
    /// `handle_join_random_game`: pick or reserve the room, then send the client to
    /// the game server with a token that carries the invitation.
    fn on_matchmaking(&mut self, id: u64, req: &Request, now: i64) {
        let code = req.code;
        let (app, user, ip) = {
            let c = &self.conns[&id];
            (c.app.clone().unwrap_or_default(), c.user_id.clone(), c.addr.ip())
        };
        let mode = param(&req.params, key::JOIN_MODE).and_then(Value::as_u8).unwrap_or(0);
        let lk = self.lobby_key(id, &req.params);
        let requested = param(&req.params, key::GAME_ID).and_then(Value::as_str).map(str::to_owned);
        let expected_users = param(&req.params, key::ADD).and_then(Value::as_strs).unwrap_or_default();
        let fail = |s: &mut Server, ret: i16, why: &str| {
            let addr = s.conns[&id].addr;
            s.log(&format!("photon: {addr} matchmaking op {code} failed: {why}"));
            s.respond(id, resp(code, ret, Some(why), vec![]), 0, false);
        };
        let name = match code {
            op::CREATE_GAME => {
                let name = requested.unwrap_or_else(|| uuid_like());
                if self.games.contains_key(&(app.clone(), name.clone())) {
                    return fail(self, err::GAME_ID_ALREADY_EXISTS, "A game with the specified id already exist.");
                }
                self.reserve(&lk, &name, now);
                name
            }
            op::JOIN_GAME => {
                let Some(name) = requested else { return fail(self, err::INVALID_PARAMETERS, "Missing game id") };
                match self.games.get(&(app.clone(), name.clone())) {
                    Some(g) if g.created && mode == JOIN_MODE_REJOIN => {
                        if let Err((ret, why)) = g.validate_rejoin(&user) {
                            return fail(self, ret, why);
                        }
                    }
                    Some(g) if g.created => {
                        if let Err((ret, why)) = g.validate_join(&user, ip, now) {
                            return fail(self, ret, why);
                        }
                    }
                    // Being created right now: let the game server sort it out.
                    Some(_) if mode == 1 => {}
                    Some(_) => return fail(self, err::GAME_DOES_NOT_EXIST, "Game does not exist"),
                    None if mode == 1 => self.reserve(&lk, &name, now),
                    None => return fail(self, err::GAME_DOES_NOT_EXIST, "Game does not exist"),
                }
                name
            }
            _ => {
                let expected = param(&req.params, key::GAME_PROPERTIES).and_then(Value::as_table).cloned().unwrap_or_default();
                // SQL lobbies (type 2) filter with SQL instead of expected properties.
                let filter = match param(&req.params, key::SQL_FILTER).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(sql::Filter::parse) {
                    Some(Err(why)) => return fail(self, err::INVALID_PARAMETERS, &format!("Invalid SQL filter: {why}")),
                    Some(Ok(f)) => Some(f),
                    None => None,
                };
                let mm = param(&req.params, key::MATCHMAKING_TYPE).and_then(Value::as_u8).unwrap_or(0);
                let mut found: Vec<(&String, &Game)> = self
                    .lobbies
                    .get(&lk)
                    .map(|l| {
                        l.games
                            .iter()
                            .filter_map(|n| self.games.get(&(app.clone(), n.clone())).map(|g| (n, g)))
                            .filter(|(_, g)| g.visible && g.matches(&expected) && g.validate_join(&user, ip, now).is_ok())
                            .filter(|(_, g)| filter.as_ref().is_none_or(|f| f.matches(&|c| g.sql_prop(c))))
                            .collect()
                    })
                    .unwrap_or_default();
                // 0 FillRoom: fullest first; 1 SerialMatching: oldest first (list
                // order); 2 RandomMatching.
                match mm {
                    _ if filter.as_ref().is_some_and(|f| f.has_order()) => {
                        let f = filter.as_ref().unwrap();
                        found.sort_by(|a, b| f.compare(&|c| a.1.sql_prop(c), &|c| b.1.sql_prop(c)));
                    }
                    0 => found.sort_by_key(|(_, g)| std::cmp::Reverse(g.actors.len())),
                    2 if !found.is_empty() => {
                        let i = random_bytes(1)[0] as usize % found.len();
                        found.swap(0, i);
                    }
                    _ => {}
                }
                match found.first().map(|(n, _)| (*n).clone()) {
                    Some(n) => n,
                    None if mode == 1 => {
                        let name = requested.unwrap_or_else(|| uuid_like());
                        if self.games.contains_key(&(app.clone(), name.clone())) {
                            return fail(self, err::GAME_ID_ALREADY_EXISTS, "A game with the specified id already exist.");
                        }
                        self.reserve(&lk, &name, now);
                        name
                    }
                    None => return fail(self, err::NO_RANDOM_MATCH, "No match found"),
                }
            }
        };
        let gk = (app.clone(), name.clone());
        if let Some(g) = self.games.get_mut(&gk) {
            for u in expected_users {
                g.expected.insert(u, now + PENDING_GAME_TTL);
            }
        }
        let token = self.conns[&id].token.clone().unwrap();
        if let Some(p) = self.tokens.get_mut(&token) {
            p.invite = Some(name.clone());
        }
        let gs = self.address(&self.conns[&id], Role::Game);
        let addr = self.conns[&id].addr;
        let verb = match code {
            op::CREATE_GAME => "create",
            op::JOIN_GAME => "join",
            _ => "join random",
        };
        self.log(&format!("photon: {addr} ({user}) {verb} room '{name}' -> game server {gs}"));
        let r = resp(
            code,
            err::OK,
            None,
            vec![(key::ADDRESS, Value::Str(gs)), (key::TOKEN, Value::Str(token)), (key::GAME_ID, Value::Str(name))],
        );
        self.respond(id, r, 0, false);
    }

    /// A room the master handed out but no game server player has made yet.
    fn reserve(&mut self, lk: &LobbyKey, name: &str, now: i64) {
        self.games.insert((lk.0.clone(), name.to_owned()), Game::new(lk.clone(), now));
        self.lobbies.entry(lk.clone()).or_default().games.insert(name.to_owned());
    }

    fn queue_list_change(&mut self, lk: &LobbyKey, name: &str, view: Option<Table>) {
        if lk.2 != 0 {
            return;
        }
        let Some(l) = self.lobbies.get(lk) else { return };
        for m in l.members.clone() {
            if let Some(c) = self.conns.get_mut(&m) {
                c.pending_list.insert(name.to_owned(), view.clone());
            }
        }
    }

    fn list_changed(&mut self, gk: &GameKey) {
        let Some(g) = self.games.get(gk) else { return };
        if !g.created {
            return;
        }
        let (lk, view) = (g.lobby.clone(), g.visible.then(|| g.lobby_view()));
        self.queue_list_change(&lk, &gk.1, view);
    }

    /// Event 229 (GameListUpdate) to lobby members with changes; a removed room is
    /// `{251: true}` (Luxon `Lobby::send_game_list_updates`, sent as 229 because the
    /// client handles 229 and 230 the same way).
    fn send_list_updates(&mut self) {
        let ids: Vec<u64> = self.conns.iter().filter(|(_, c)| !c.pending_list.is_empty()).map(|(i, _)| *i).collect();
        for id in ids {
            let pending = std::mem::take(&mut self.conns.get_mut(&id).unwrap().pending_list);
            let list: Table = pending
                .into_iter()
                .map(|(n, v)| {
                    let v = v.unwrap_or_else(|| vec![(Value::Byte(prop::REMOVED), Value::Bool(true))]);
                    (Value::Str(n), Value::Hashtable(v))
                })
                .collect();
            let e = EventMsg { code: ev::GAME_LIST_UPDATE, params: vec![(key::GAME_LIST, Value::Hashtable(list))] };
            self.send(id, &Msg::Event(e), 0, true, false);
        }
    }

    /// Event 226 (AppStats) to master clients, unreliable (Luxon sends it every 5 s).
    fn send_app_stats(&mut self) {
        let mut apps: HashMap<AppKey, (i32, i32, i32)> = HashMap::new();
        for c in self.conns.values() {
            if let Some(a) = &c.app {
                let e = apps.entry(a.clone()).or_default();
                match c.role() {
                    Role::Master => e.2 += 1,
                    Role::Game if c.actor != 0 && !c.left => e.0 += 1,
                    _ => {}
                }
            }
        }
        for (k, g) in &self.games {
            if g.created {
                apps.entry(k.0.clone()).or_default().1 += 1;
            }
        }
        let ids: Vec<(u64, AppKey)> = self
            .conns
            .iter()
            .filter(|(_, c)| c.role() == Role::Master && c.app.is_some())
            .map(|(i, c)| (*i, c.app.clone().unwrap()))
            .collect();
        for (id, app) in ids {
            let (players, games, masters) = apps.get(&app).copied().unwrap_or_default();
            let e = EventMsg {
                code: ev::APP_STATS,
                params: vec![
                    (key::PEER_COUNT, Value::Int(players)),
                    (key::GAME_COUNT, Value::Int(games)),
                    (key::MASTER_PEER_COUNT, Value::Int(masters)),
                ],
            };
            self.send(id, &Msg::Event(e), 0, false, false);
        }
    }

    // ------------------------------------------------------------ game server

    fn gs_fail(&mut self, id: u64, code: u8, ret: i16, why: &str) {
        let addr = self.conns[&id].addr;
        self.log(&format!("photon: game server {addr} op {code} failed: {why}"));
        self.respond(id, resp(code, ret, Some(why), vec![]), 0, false);
    }

    /// Luxon `GameServer::handle_create_game` / `handle_join_game`.
    fn on_gs_join(&mut self, id: u64, req: &Request, now: i64) {
        let code = req.code;
        let (invited, user, ip, addr, cur_actor) = {
            let c = &self.conns[&id];
            (c.game.clone(), c.user_id.clone(), c.addr.ip(), c.addr, if c.left { 0 } else { c.actor })
        };
        if cur_actor != 0 {
            return self.gs_fail(id, code, err::OPERATION_INVALID, "Already in a game");
        }
        let Some(mut gk) = invited else {
            return self.gs_fail(id, code, err::SERVER_FORBIDDEN, "Not invited to any game server");
        };
        if let Some(n) = param(&req.params, key::GAME_ID).and_then(Value::as_str) {
            if n != gk.1 {
                let other = (gk.0.clone(), n.to_owned());
                if !self.games.contains_key(&other) {
                    return self.gs_fail(id, code, err::GAME_DOES_NOT_EXIST, "Game does not exist");
                }
                gk = other;
            }
        }
        let mode = param(&req.params, key::JOIN_MODE).and_then(Value::as_u8).unwrap_or(0);
        let Some(g) = self.games.get_mut(&gk) else {
            return self.gs_fail(id, code, err::GAME_DOES_NOT_EXIST, "Game does not exist");
        };
        let create = !g.created && (code == op::CREATE_GAME || code == op::JOIN || mode == 1);
        if g.created && code == op::CREATE_GAME {
            return self.gs_fail(id, code, err::GAME_ID_ALREADY_EXISTS, "A game with the specified id already exist.");
        }
        let rejoin = g.created && mode == JOIN_MODE_REJOIN;
        let mut joined_props = param(&req.params, key::PLAYER_PROPERTIES).and_then(Value::as_table).cloned().unwrap_or_default();
        let nr = if rejoin {
            let nr = match g.validate_rejoin(&user) {
                Ok(nr) => nr,
                Err((ret, why)) => return self.gs_fail(id, code, ret, why),
            };
            // The same actor, active again, with what it had plus anything sent now.
            let a = g.actors.iter_mut().find(|a| a.nr == nr).unwrap();
            a.conn = id;
            a.ip = ip;
            a.inactive_until = None;
            for (k, v) in joined_props.drain(..) {
                table_set(&mut a.props, k, v);
            }
            table_remove(&mut a.props, &Value::Byte(prop::ACTOR_INACTIVE));
            nr
        } else {
            if create {
                g.created = true;
                apply_room_options(g, &req.params);
            } else if let Err((ret, why)) = g.validate_join(&user, ip, now) {
                let why = why.to_owned();
                return self.gs_fail(id, code, ret, &why);
            }
            // Next free actor number, 1..=254, wrapping (Luxon `Game::add_actor`).
            let mut nr = g.last_actor;
            loop {
                nr = nr % 254 + 1;
                if g.actor(nr).is_none() {
                    break;
                }
            }
            g.last_actor = nr;
            if g.flags & flag::PUBLISH_USER_ID != 0 {
                table_set(&mut joined_props, Value::Byte(prop::ACTOR_USER_ID), Value::Str(user.clone()));
            }
            g.actors.push(Actor { conn: id, nr, props: joined_props, groups: [1, 0, 0, 0], user_id: user.clone(), ip, inactive_until: None });
            nr
        };
        let props = g.actor(nr).map(|a| a.props.clone()).unwrap_or_default();
        if create || !g.is_active(g.master) {
            g.master = nr;
        }
        g.expected.remove(&user);
        let ids = g.actor_ids();
        let suppress_info = g.flags & flag::SUPPRESS_PLAYER_INFO != 0;
        let others: Table = g
            .actors
            .iter()
            .filter(|a| a.nr != nr && !suppress_info)
            .map(|a| {
                let mut p = a.props.clone();
                if a.inactive_until.is_some() {
                    table_set(&mut p, Value::Byte(prop::ACTOR_INACTIVE), Value::Bool(true));
                }
                (Value::Int(a.nr), Value::Hashtable(p))
            })
            .collect();
        let r = resp(
            code,
            err::OK,
            None,
            vec![
                (key::ACTOR_NR, Value::Int(nr)),
                (key::ACTOR_LIST, Value::Ints(ids.clone())),
                (key::GAME_PROPERTIES, Value::Hashtable(g.props())),
                (key::PLAYER_PROPERTIES, Value::Hashtable(others)),
                (key::GAME_FLAGS, Value::Int(g.flags as i32)),
            ],
        );
        let suppress = g.flags & flag::SUPPRESS_ROOM_EVENTS != 0;
        let cache = g.cache.clone();
        let (count, max) = (g.actors.len(), g.max_players);
        let c = self.conns.get_mut(&id).unwrap();
        c.game = Some(gk.clone());
        c.actor = nr;
        c.left = false;
        if addr.ip().is_loopback() {
            super::joined_locally(&gk.1, nr);
        }
        self.publish_room(&gk);
        self.log(&format!(
            "photon: {addr} ({user}) {} room '{}' as actor {nr} ({count}/{} players)",
            if create { "created" } else if rejoin { "rejoined" } else { "joined" },
            gk.1,
            if max == 0 { "any".to_string() } else { max.to_string() }
        ));
        self.respond(id, r, 0, false);
        if !suppress {
            let mut p = vec![(key::ACTOR_NR, Value::Int(nr)), (key::ACTOR_LIST, Value::Ints(ids))];
            if !suppress_info {
                p.push((key::PLAYER_PROPERTIES, Value::Hashtable(props)));
            }
            self.deliver(&gk, 0, Receivers::All, &EventMsg { code: ev::JOIN, params: p }, 0, true);
        }
        for e in cache {
            let mut p = vec![];
            if e.sender != 0 {
                p.push((key::ACTOR_NR, Value::Int(e.sender)));
            }
            if let Some(d) = e.data {
                p.push((key::DATA, d));
            }
            self.send(id, &Msg::Event(EventMsg { code: e.code, params: p }), e.channel, e.reliable, false);
        }
        self.list_changed(&gk);
    }

    fn on_leave(&mut self, id: u64, req: &Request, now: i64) {
        let c = self.conns.get_mut(&id).unwrap();
        let (gk, nr) = (c.game.clone(), c.actor);
        if nr == 0 || c.left {
            return self.gs_fail(id, op::LEAVE, err::OPERATION_INVALID, "Not in a game");
        }
        c.left = true;
        c.actor = 0;
        let comes_back = matches!(param(&req.params, key::IS_INACTIVE), Some(Value::Bool(true)));
        self.respond(id, resp(op::LEAVE, err::OK, None, vec![]), 0, false);
        if let Some(gk) = gk {
            self.actor_gone(&gk, nr, now, comes_back);
        }
    }

    /// An actor's client went: inactive when the room keeps players (PlayerTTL) and
    /// `may_return`, else out of the room.
    fn actor_gone(&mut self, gk: &GameKey, nr: i32, now: i64, may_return: bool) {
        let Some(g) = self.games.get_mut(gk) else { return };
        if !may_return || g.player_ttl == 0 {
            return self.remove_actor(gk, nr, now);
        }
        let ttl = g.player_ttl;
        let Some(a) = g.actors.iter_mut().find(|a| a.nr == nr) else { return };
        a.conn = 0;
        a.inactive_until = Some(if ttl < 0 { i64::MAX } else { now + ttl as i64 });
        if let Some(f) = g.fusion.as_mut() {
            f.left(nr);
        }
        (self.cfg.log)(&format!(
            "photon: actor {nr} in room '{}' is inactive ({})",
            gk.1,
            if ttl < 0 { "kept until it rejoins".to_string() } else { format!("may rejoin for {} s", ttl / 1000) }
        ));
        let g = self.games.get_mut(gk).unwrap();
        let mut p = vec![(key::ACTOR_NR, Value::Int(nr)), (key::ACTOR_LIST, Value::Ints(g.actor_ids())), (key::IS_INACTIVE, Value::Bool(true))];
        let master_changed = g.master == nr;
        if master_changed {
            if let Some(m) = g.actors.iter().filter(|a| a.inactive_until.is_none()).map(|a| a.nr).min() {
                g.master = m;
                p.push((key::MASTER_CLIENT_ID, Value::Int(m)));
            }
        }
        if g.flags & flag::SUPPRESS_ROOM_EVENTS == 0 || master_changed {
            self.deliver(gk, 0, Receivers::All, &EventMsg { code: ev::LEAVE, params: p }, 0, true);
        }
        self.list_changed(gk);
    }

    /// Luxon `Game::remove_actor` (+ the cache cleanup Luxon gates on the wrong flag).
    fn remove_actor(&mut self, gk: &GameKey, nr: i32, now: i64) {
        let Some(g) = self.games.get_mut(gk) else { return };
        let Some(pos) = g.actors.iter().position(|a| a.nr == nr) else { return };
        g.actors.remove(pos);
        if let Some(f) = g.fusion.as_mut() {
            f.left(nr);
        }
        if g.flags & flag::DELETE_CACHE_ON_LEAVE != 0 {
            g.cache.retain(|e| e.sender != nr);
        }
        let left = g.actors.len();
        super::left(&gk.1, nr);
        (self.cfg.log)(&format!("photon: actor {nr} left room '{}' ({left} left)", gk.1));
        self.publish_room(gk);
        let g = self.games.get_mut(gk).unwrap();
        if g.actors.is_empty() {
            g.keep_until = now + g.empty_ttl.max(0) as i64;
            self.list_changed(gk);
            return;
        }
        let mut p = vec![(key::ACTOR_NR, Value::Int(nr)), (key::ACTOR_LIST, Value::Ints(g.actor_ids()))];
        let master_changed = g.master == nr;
        if master_changed {
            let active = g.actors.iter().filter(|a| a.inactive_until.is_none()).map(|a| a.nr).min();
            g.master = active.unwrap_or_else(|| g.actors.iter().map(|a| a.nr).min().unwrap());
            p.push((key::MASTER_CLIENT_ID, Value::Int(g.master)));
        }
        if g.flags & flag::SUPPRESS_ROOM_EVENTS == 0 || master_changed {
            self.deliver(gk, 0, Receivers::All, &EventMsg { code: ev::LEAVE, params: p }, 0, true);
        }
        self.list_changed(gk);
    }

    /// Luxon `GameServer::handle_set_properties`.
    fn on_set_properties(&mut self, id: u64, req: &Request) {
        let (gk, me) = {
            let c = &self.conns[&id];
            (c.game.clone(), if c.left { 0 } else { c.actor })
        };
        let code = op::SET_PROPERTIES;
        let Some(gk) = gk.filter(|_| me != 0) else { return self.gs_fail(id, code, err::OPERATION_INVALID, "Not in a game") };
        let Some(update) = param(&req.params, key::PROPERTIES).and_then(Value::as_table).cloned() else {
            return self.gs_fail(id, code, err::INVALID_PARAMETERS, "Missing properties");
        };
        let expected = param(&req.params, key::EXPECTED_VALUES).and_then(Value::as_table).cloned();
        let target = param(&req.params, key::ACTOR_NR).and_then(Value::as_i32).unwrap_or(0);
        let g = self.games.get_mut(&gk).unwrap();
        let mut list = false;
        if target != 0 {
            let delete_null = g.flags & flag::DELETE_NULL_PROPS != 0;
            let Some(a) = g.actors.iter_mut().find(|a| a.nr == target) else {
                return self.gs_fail(id, code, err::INVALID_PARAMETERS, "No such actor");
            };
            if let Some(exp) = &expected {
                let ok = exp.iter().all(|(k, v)| table_get(&a.props, k).map_or(v.is_null(), |h| same(h, v)));
                if !ok {
                    return self.gs_fail(id, code, err::INVALID_PARAMETERS, "Expected properties don't match");
                }
            }
            for (k, v) in &update {
                if v.is_null() && delete_null {
                    table_remove(&mut a.props, k);
                } else {
                    table_set(&mut a.props, k.clone(), v.clone());
                }
            }
        } else {
            if let Some(exp) = &expected {
                if !g.matches(exp) {
                    return self.gs_fail(id, code, err::INVALID_PARAMETERS, "Expected properties don't match");
                }
            }
            let (changed, hidden) = g.apply(&update);
            list = changed || hidden;
        }
        let to_all = g.flags & flag::BROADCAST_PROPS_TO_ALL != 0 || expected.is_some();
        self.respond(id, resp(code, err::OK, None, vec![]), 0, false);
        if list {
            self.list_changed(&gk);
        }
        let e = EventMsg {
            code: ev::PROPERTIES_CHANGED,
            params: vec![
                (key::TARGET_ACTOR, Value::Int(target)),
                (key::PROPERTIES, Value::Hashtable(update)),
                (key::ACTOR_NR, Value::Int(me)),
            ],
        };
        self.deliver(&gk, me, if to_all { Receivers::All } else { Receivers::Others }, &e, 0, true);
    }

    /// Luxon `GameServer::handle_change_groups`: an empty array means every group.
    fn on_change_groups(&mut self, id: u64, req: &Request) {
        let (gk, me) = {
            let c = &self.conns[&id];
            (c.game.clone(), if c.left { 0 } else { c.actor })
        };
        if let Some(a) = gk
            .filter(|_| me != 0)
            .and_then(|gk| self.games.get_mut(&gk))
            .and_then(|g| g.actors.iter_mut().find(|a| a.nr == me))
        {
            match param(&req.params, key::REMOVE).and_then(Value::as_bytes) {
                Some([]) => a.groups = [1, 0, 0, 0],
                Some(list) => {
                    for g in list.iter().filter(|g| **g != 0) {
                        a.groups[(*g / 64) as usize] &= !(1u64 << (*g % 64));
                    }
                }
                None => {}
            }
            match param(&req.params, key::ADD).and_then(Value::as_bytes) {
                Some([]) => a.groups = [u64::MAX; 4],
                Some(list) => {
                    for g in list {
                        a.groups[(*g / 64) as usize] |= 1u64 << (*g % 64);
                    }
                }
                None => {}
            }
        }
        self.respond(id, resp(op::CHANGE_GROUPS, err::OK, None, vec![]), 0, false);
    }

    /// Luxon `GameServer::handle_raise_event` and its event-cache rules.
    fn on_raise_event(&mut self, id: u64, req: Request, channel: u8, reliable: bool, now: i64) {
        let (gk, me) = {
            let c = &self.conns[&id];
            (c.game.clone(), if c.left { 0 } else { c.actor })
        };
        let Some(gk) = gk.filter(|_| me != 0) else { return };
        let code = param(&req.params, key::CODE).and_then(Value::as_u8).unwrap_or(0);
        let fusion = code == super::fusion::RELAY_EVENT || code == super::fusion::PROTOCOL_EVENT;
        if trace() || (fusion && fusion_trace()) {
            let to = param(&req.params, key::ACTOR_LIST).and_then(Value::as_ints);
            let data = match param(&req.params, key::DATA) {
                Some(Value::Bytes(b)) => head(b),
                Some(v) => format!("{v:?}").chars().take(160).collect(),
                None => "-".into(),
            };
            self.log(&format!("photon-trace event {code} from {me} to {to:?}: {data}"));
        }
        if code > 220 {
            if channel == 0 {
                self.gs_fail(id, op::RAISE_EVENT, err::OPERATION_INVALID, "Event codes above 220 are reserved");
            }
            return;
        }
        let data = param(&req.params, key::DATA).cloned().filter(|d| !d.is_null());
        let cache_op = param(&req.params, key::CACHE).and_then(Value::as_u8).unwrap_or(0);
        let targets = param(&req.params, key::ACTOR_LIST).and_then(Value::as_ints);
        let group = param(&req.params, key::GROUP).and_then(Value::as_u8).unwrap_or(0);
        let receivers = param(&req.params, key::RECEIVER_GROUP).and_then(Value::as_u8).unwrap_or(0);
        // Fusion Shared mode: relayed connection packets to the plugin (actor 0) go to
        // PeerParty's Shared server.
        let mut targets = targets;
        if code == super::fusion::RELAY_EVENT && targets.as_ref().is_some_and(|t| t.contains(&0)) {
            let g = self.games.get_mut(&gk).unwrap();
            let (players, master) = (g.max_players as usize, g.master);
            if let (Some(plugin), Some(Value::Bytes(bytes))) = (g.fusion.as_mut(), data.as_ref()) {
                let mut lines = Vec::new();
                let out = plugin.relay(me, bytes, now, players, master, &mut lines);
                for l in lines {
                    self.log(&l);
                }
                self.send_relay(&gk, out);
                let rest: Vec<i32> = targets.take().unwrap_or_default().into_iter().filter(|a| *a != 0).collect();
                if rest.is_empty() {
                    return;
                }
                targets = Some(rest);
            }
        }
        // Fusion's protocol messages to its plugin (actor 0) are answered here.
        if code == super::fusion::PROTOCOL_EVENT && targets.as_ref().is_some_and(|t| t.contains(&0)) {
            let g = self.games.get_mut(&gk).unwrap();
            if let (Some(plugin), Some(Value::Bytes(bytes))) = (g.fusion.as_mut(), data.as_ref()) {
                let mut lines = Vec::new();
                let unique = || {
                    let mut id = [0u8; 8];
                    id.copy_from_slice(&random_bytes(8));
                    id[7] |= 1;
                    id
                };
                let out = plugin.handle(me, bytes, unique, &mut lines);
                for l in lines {
                    self.log(&l);
                }
                for (to, msg) in out {
                    if fusion_trace() {
                        self.log(&format!("photon-trace protocol 0 -> {to}: {}", head(&msg)));
                    }
                    let e = EventMsg { code: super::fusion::PROTOCOL_EVENT, params: vec![(key::DATA, Value::Bytes(msg))] };
                    self.deliver(&gk, 0, Receivers::Actors(vec![to]), &e, 1, true);
                }
                let rest: Vec<i32> = targets.take().unwrap_or_default().into_iter().filter(|a| *a != 0).collect();
                if rest.is_empty() {
                    return;
                }
                targets = Some(rest);
            }
        }
        let g = self.games.get_mut(&gk).unwrap();
        match cache_op {
            6 => {
                // RemoveFromRoomCache: by code (0 = any), sender list and data subset.
                g.cache.retain(|e| {
                    let code_hit = code == 0 || e.code == code;
                    let sender_hit = targets.as_ref().is_none_or(|t| t.contains(&e.sender));
                    let data_hit = match (&data, &e.data) {
                        (None, _) => true,
                        (Some(Value::Hashtable(want)), Some(Value::Hashtable(have))) => {
                            want.iter().all(|(k, v)| table_get(have, k).is_some_and(|h| same(h, v)))
                        }
                        (Some(want), Some(have)) => same(want, have),
                        (Some(_), None) => false,
                    };
                    !(code_hit && sender_hit && data_hit)
                });
                return;
            }
            7 => {
                let ids = g.actor_ids();
                g.cache.retain(|e| e.sender == 0 || ids.contains(&e.sender));
                return;
            }
            10..=13 => {
                self.log(&format!("photon: event cache slice op {cache_op} ignored"));
                return;
            }
            4 | 5 if targets.is_none() && receivers != 2 && group == 0 => {
                g.cache.push(Cached {
                    code,
                    sender: if cache_op == 5 { 0 } else { me },
                    data: data.clone(),
                    channel,
                    reliable: true,
                });
            }
            _ => {}
        }
        let mut params = vec![(key::ACTOR_NR, Value::Int(me))];
        if let Some(d) = data {
            params.push((key::DATA, d));
        }
        let rcv = match (targets, receivers) {
            (Some(t), _) => Receivers::Actors(t),
            (None, 1) => Receivers::All,
            (None, 2) => Receivers::Master,
            _ => Receivers::Others,
        };
        let e = EventMsg { code, params };
        if group != 0 {
            let in_group: Vec<i32> = g
                .actors
                .iter()
                .filter(|a| a.in_group(group) && (a.nr != me || matches!(rcv, Receivers::All)))
                .map(|a| a.nr)
                .collect();
            return self.deliver(&gk, me, Receivers::Actors(in_group), &e, channel, reliable);
        }
        self.deliver(&gk, me, rcv, &e, channel, reliable);
    }

    /// Send one event to room members, encoding it once per protocol version.
    fn deliver(&mut self, gk: &GameKey, sender: i32, to: Receivers, e: &EventMsg, channel: u8, reliable: bool) {
        let Some(g) = self.games.get(gk) else { return };
        let conns: Vec<u64> = g
            .actors
            .iter()
            .filter(|a| match &to {
                Receivers::All => true,
                Receivers::Others => a.nr != sender,
                Receivers::Master => a.nr == g.master,
                Receivers::Actors(l) => l.contains(&a.nr),
            })
            .map(|a| a.conn)
            .collect();
        let msg = Msg::Event(e.clone());
        let mut encoded: HashMap<Proto, Vec<u8>> = HashMap::new();
        for id in conns {
            let Some(c) = self.conns.get_mut(&id) else { continue };
            let Some(p) = c.proto else { continue };
            if !encoded.contains_key(&p) {
                match p.encode(&msg, None) {
                    Ok(b) => {
                        encoded.insert(p, b);
                    }
                    Err(err) => {
                        (self.cfg.log)(&format!("photon: couldn't encode event {}: {err}", e.code));
                        return;
                    }
                }
            }
            c.enet.send(channel, &encoded[&p], reliable, false);
        }
    }
}

/// Room options the creating client sends (LoadBalancingPeer `RoomOptionsToOpParameters`).
fn apply_room_options(g: &mut Game, p: &Params) {
    let mut flags = flag::CHECK_USER_ON_JOIN | flag::DELETE_CACHE_ON_LEAVE;
    if let Some(false) = param(p, key::CLEANUP_CACHE_ON_LEAVE).and_then(Value::as_bool) {
        flags &= !flag::DELETE_CACHE_ON_LEAVE;
    }
    if let Some(b) = param(p, key::CHECK_USER_ON_JOIN).and_then(Value::as_bool) {
        if !b {
            flags &= !flag::CHECK_USER_ON_JOIN;
        }
    }
    if param(p, key::SUPPRESS_ROOM_EVENTS).and_then(Value::as_bool) == Some(true) {
        flags |= flag::SUPPRESS_ROOM_EVENTS;
    }
    if param(p, key::PUBLISH_USER_ID).and_then(Value::as_bool) == Some(true) {
        flags |= flag::PUBLISH_USER_ID;
    }
    if let Some(f) = param(p, key::GAME_FLAGS).and_then(Value::as_i32) {
        flags = f as u32;
    }
    g.flags = flags;
    if param(p, key::PLUGINS).and_then(Value::as_strs).is_some_and(|l| l.iter().any(|n| n == super::fusion::PLUGIN_NAME)) {
        g.fusion = Some(super::fusion::Plugin::default());
    }
    if let Some(n) = param(p, key::PLAYER_TTL).and_then(Value::as_i32) {
        g.player_ttl = n;
    }
    if let Some(n) = param(p, key::EMPTY_ROOM_TTL).and_then(Value::as_i32) {
        g.empty_ttl = n;
    }
    if let Some(t) = param(p, key::GAME_PROPERTIES).and_then(Value::as_table) {
        let mut t = t.clone();
        // `249 = false` inside the room props is the old "don't clean up" marker.
        if let Some(pos) = t.iter().position(|(k, _)| *k == Value::Byte(249)) {
            if t[pos].1 == Value::Bool(false) {
                g.flags &= !flag::DELETE_CACHE_ON_LEAVE;
            }
            t.remove(pos);
        }
        g.apply(&t);
        if g.max_players == 0 {
            if let Some(n) = table_get(&t, &Value::Byte(prop::MAX_PLAYERS_INT)).and_then(Value::as_i32) {
                g.max_players = n.clamp(0, 255) as u8;
            }
        }
    }
}

/// A room name for rooms created without one (Photon uses a GUID string).
fn uuid_like() -> String {
    let b = random_bytes(16);
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}
