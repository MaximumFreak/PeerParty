//! Steam game servers and the server browser on the LAN, following Valve's Steamworks
//! documentation and headers for ISteamGameServer and ISteamMatchmakingServers.
//!
//! A game server (`SteamGameServer_Init` plus ISteamGameServer, in a listen server's own
//! process or a dedicated server's) is a LAN lobby in namespace "steam.server" that it
//! owns: its name, map, ports, tags, rules and players are the lobby's attributes,
//! updated as the game sets them. Logging on succeeds at once (`SteamServersConnected_t`,
//! an insecure `GSPolicyResponse_t`) with a LAN game server SteamID, and players on the
//! LAN pass its auth checks. The server's SteamID is in its host's presence
//! ("PeerParty:gs"), so P2P packets and networking-sockets connections to it reach that
//! host.
//!
//! ISteamMatchmakingServers lists those lobbies: the Internet, LAN and Friends lists
//! all give the LAN's servers (Favorites, History and Spectator lists come back empty),
//! with the request's filters applied here. Ping, player and rules queries are
//! answered from the same adverts. A server's address is its host's LAN address and
//! its game port, so the game's own connection goes straight to it.
//!
//! A dedicated server with its own exe meets clients only under the same game key: its
//! exe's name less a "_server" ending does that (mygame_server -> mygame), or start it
//! with `PEERPARTY_GAME=<the client exe's name>`.
//!
//! Its networking-sockets connections (ISteamGameServerNetworkingSockets) are the game
//! server's: their status changes go to the game server's dispatch, they are the game
//! server's SteamID at its end, and a client's connection to it (by its SteamID, or by
//! IP to a port it listens on) sees that SteamID too.

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use peerparty_core::{Attr, Event, Lan, Lobby, Permission, PeerId, Query, Value};
use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::net;
use crate::pe::Export;
use crate::steamlan::{bytes, peer_of, steam_id};
use crate::steamobj::{call_real, vmethod};

pub const NS: &str = "steam.server";
const KEY_GS: &str = "peerparty:gs";

const STEAM_SERVERS_CONNECTED: i32 = 101;
const STEAM_SERVERS_DISCONNECTED: i32 = 103;
const GS_POLICY_RESPONSE: i32 = 115;
const VALIDATE_AUTH_TICKET_RESPONSE: i32 = 143;
const GS_CLIENT_APPROVE: i32 = 201;

/// How often a changed advert goes out, at most.
const ADVERT_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

#[repr(C)]
struct AuthResponse {
    steam_id: u64,
    response: i32,
    owner: u64,
}

#[repr(C, packed)]
struct ClientApprove {
    steam_id: u64,
    owner: u64,
}

#[repr(C)]
struct PolicyResponse {
    secure: u8,
}

#[repr(C)]
struct Disconnected {
    result: i32,
}

/// Steam's gameserveritem_t. CSteamID is byte-packed in Steam's headers, so it follows
/// the tags directly.
#[repr(C)]
#[derive(Clone)]
pub struct ServerItem {
    connection_port: u16,
    query_port: u16,
    ip: u32,
    ping: i32,
    had_response: bool,
    do_not_refresh: bool,
    game_dir: [u8; 32],
    map: [u8; 32],
    description: [u8; 64],
    app_id: u32,
    players: i32,
    max_players: i32,
    bots: i32,
    password: bool,
    secure: bool,
    last_played: u32,
    version: i32,
    name: [u8; 64],
    tags: [u8; 128],
    steam_id: [u8; 8],
    friends_now: i32,
    friends_total: i32,
}

const _: () = assert!(std::mem::offset_of!(ServerItem, app_id) == 144);
const _: () = assert!(std::mem::offset_of!(ServerItem, name) == 172);
const _: () = assert!(std::mem::offset_of!(ServerItem, steam_id) == 364);
const _: () = assert!(size_of::<ServerItem>() == 380);

#[repr(C)]
struct KeyValue {
    key: [u8; 256],
    value: [u8; 256],
}

/// Our game server, from Init and the ISteamGameServer setters.
#[derive(Default, Clone)]
struct Server {
    id: u64,
    ip: u32,
    game_port: u16,
    query_port: u16,
    spectator_port: u16,
    version: String,
    product: String,
    description: String,
    mod_dir: String,
    dedicated: bool,
    max_players: i32,
    bots: i32,
    name: String,
    map: String,
    password: bool,
    spectator_name: String,
    region: String,
    tags: String,
    game_data: String,
    rules: BTreeMap<String, String>,
    /// Steam id -> (name, score).
    players: BTreeMap<u64, (String, u32)>,
    logged_on: bool,
    /// What the game asked for; unset means advertised once logged on.
    advertise: Option<bool>,
}

struct State {
    server: Option<Server>,
    /// The advert's core lobby id while it's up.
    advert: Option<String>,
    dirty: bool,
    last_advert: Option<std::time::Instant>,
    /// Browser requests and server queries by handle.
    lists: HashMap<usize, List>,
    queries: HashMap<i32, ServerQuery>,
    /// Server SteamID -> host, from adverts seen.
    hosts: HashMap<u64, PeerId>,
}

struct List {
    kind: ListKind,
    filters: Vec<(String, String)>,
    response: usize,
    search: Option<u64>,
    items: Vec<Box<ServerItem>>,
    refreshing: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ListKind {
    Internet,
    Lan,
    Friends,
    Favorites,
    History,
    Spectator,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryKind {
    Ping,
    Players,
    Rules,
}

struct ServerQuery {
    kind: QueryKind,
    ip: u32,
    port: u16,
    response: usize,
    search: u64,
}

static S: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        server: None,
        advert: None,
        dirty: false,
        last_advert: None,
        lists: HashMap::new(),
        queries: HashMap::new(),
        hosts: HashMap::new(),
    })
});

static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x5050_0001);
static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);
/// steam_api's SteamGameServer_GetHSteamPipe, for telling the server's dispatch apart.
static GET_PIPE: AtomicUsize = AtomicUsize::new(0);
/// steam_api's SteamAPI_GetHSteamPipe (or the legacy GetHSteamPipe): the game's own pipe.
static GET_CLIENT_PIPE: AtomicUsize = AtomicUsize::new(0);
/// The game is starting or running a game server (SteamGameServer_Init, until Shutdown).
/// Without one there is no server pipe, whatever SteamGameServer_GetHSteamPipe says: some
/// steam_api builds hand back the game's own pipe there, and every
/// client callback PeerParty injected then waited for a server dispatch that never came.
static SERVER_STARTED: AtomicBool = AtomicBool::new(false);
/// Which pipe states have been logged (bit 0 without a game server, bit 1 with one).
static PIPES_LOGGED: AtomicU8 = AtomicU8::new(0);

fn lan() -> Option<&'static Lan> {
    net::lan()
}

fn s(p: *const c_char) -> String {
    crate::read::cstr(p as usize).unwrap_or_default()
}

/// Changes our server, if there is one, and marks the advert for an update.
fn with_server(f: impl FnOnce(&mut Server)) {
    let mut st = S.lock().unwrap();
    let st = &mut *st;
    let server = st.server.get_or_insert_with(Server::default);
    f(server);
    st.dirty = true;
}

/// The game server SteamID we're using, 0 before logging on.
pub fn my_server_id() -> u64 {
    S.lock().unwrap().server.as_ref().filter(|s| s.logged_on).map_or(0, |s| s.id)
}

/// The LAN host of game server `id` (us included).
pub fn host_of(id: u64) -> Option<PeerId> {
    if id == 0 {
        return None;
    }
    let lan = lan()?;
    if id == my_server_id() {
        return Some(lan.me());
    }
    if let Some(p) = S.lock().unwrap().hosts.get(&id).copied() {
        return Some(p);
    }
    let found = lan.peers().into_iter().map(|(p, _)| p).find(|p| server_id_of(*p) == Some(id));
    if let Some(p) = found {
        S.lock().unwrap().hosts.insert(id, p);
    }
    found
}

/// The game server a LAN player hosts, from its presence.
pub fn server_id_of(peer: PeerId) -> Option<u64> {
    let lan = lan()?;
    if peer == lan.me() {
        return Some(my_server_id()).filter(|id| *id != 0);
    }
    lan.presence(peer)
        .iter()
        .find(|(k, _)| k == KEY_GS)
        .and_then(|(_, v)| v.parse().ok())
}

/// Whether a SteamID is a game server on the LAN.
pub fn is_server(id: u64) -> bool {
    // Account type 3 (game server) or 4 (anonymous game server).
    matches!((id >> 52) & 0xf, 3 | 4) && host_of(id).is_some()
}

/// The game server's dispatch pipe, once the game has started a game server.
pub fn server_pipe() -> Option<i32> {
    let started = SERVER_STARTED.load(Ordering::Relaxed);
    let bit = if started { 2 } else { 1 };
    let log_now = PIPES_LOGGED.fetch_or(bit, Ordering::Relaxed) & bit == 0;
    if !started && !log_now {
        return None;
    }
    let server = call_pipe(&GET_PIPE);
    let pick = pick_server_pipe(started, server, call_pipe(&GET_CLIENT_PIPE));
    if log_now {
        let client = call_pipe(&GET_CLIENT_PIPE);
        let verdict = match (started, pick) {
            (_, Some(_)) => "the game server's dispatch",
            (false, None) => "no game server started, so all callbacks go to the game",
            (true, None) => "not a separate pipe, so all callbacks go to the game",
        };
        log!("steam-lan pipes: game {client}, game server {server} ({verdict})");
    }
    pick
}

/// Which pipe is the game server's: none until the game starts a game server, and never
/// the game's own pipe (0 = unknown) or 0.
fn pick_server_pipe(started: bool, server: i32, client: i32) -> Option<i32> {
    if !started || server == 0 || server == client {
        return None;
    }
    Some(server)
}

fn call_pipe(slot: &AtomicUsize) -> i32 {
    let f = slot.load(Ordering::Relaxed);
    if f == 0 {
        return 0;
    }
    let f: unsafe extern "C" fn() -> i32 = unsafe { std::mem::transmute(f) };
    unsafe { f() }
}

fn new_server_id(anonymous: bool) -> u64 {
    let account = u32::from_le_bytes(PeerId::random().0[..4].try_into().unwrap()) | 1;
    let kind = if anonymous { 4u64 } else { 3 };
    (1u64 << 56) | (kind << 52) | (1u64 << 32) | account as u64
}

fn log_on(anonymous: bool) {
    let id = {
        let mut st = S.lock().unwrap();
        let st = &mut *st;
        let server = st.server.get_or_insert_with(Server::default);
        if server.id == 0 {
            server.id = new_server_id(anonymous);
        }
        server.logged_on = true;
        st.dirty = true;
        server.id
    };
    net::set_presence("steamgs", vec![(KEY_GS.into(), id.to_string())]);
    log!("steam-lan game server logged on as {id} (LAN)");
    crate::steam::inject_server(STEAM_SERVERS_CONNECTED, vec![0]);
    crate::steam::inject_server(GS_POLICY_RESPONSE, bytes(&PolicyResponse { secure: 0 }));
}

fn log_off() {
    let advert = {
        let mut st = S.lock().unwrap();
        let was = st.server.as_ref().is_some_and(|s| s.logged_on);
        if let Some(server) = st.server.as_mut() {
            server.logged_on = false;
        }
        if !was {
            return;
        }
        st.advert.take()
    };
    if let (Some(id), Some(lan)) = (advert, lan()) {
        let _ = lan.destroy(&id);
    }
    net::set_presence("steamgs", Vec::new());
    log!("steam-lan game server logged off");
    // k_EResultOK: the game asked for it.
    crate::steam::inject_server(STEAM_SERVERS_DISCONNECTED, bytes(&Disconnected { result: 1 }));
}

fn add_player(user: u64) {
    let name = peer_of(user)
        .and_then(|p| lan().and_then(|l| l.peer_name(p)))
        .unwrap_or_default();
    with_server(|s| {
        s.players.entry(user).or_insert((name, 0));
    });
}

// ---- the advert ----

fn attr(key: &str, value: impl ToString) -> Attr {
    Attr {
        key: key.into(),
        value: Value::Str(value.to_string()),
        public: true,
    }
}

fn advert_attrs(s: &Server) -> Vec<Attr> {
    let mut a = vec![
        attr("sid", s.id),
        attr("ip", s.ip),
        attr("port", s.game_port),
        attr("qport", s.query_port),
        attr("sport", s.spectator_port),
        attr("version", &s.version),
        attr("product", &s.product),
        attr("desc", &s.description),
        attr("gamedir", &s.mod_dir),
        attr("dedicated", s.dedicated as u8),
        attr("max", s.max_players),
        attr("bots", s.bots),
        attr("name", &s.name),
        attr("map", &s.map),
        attr("password", s.password as u8),
        attr("sname", &s.spectator_name),
        attr("region", &s.region),
        attr("tags", &s.tags),
        attr("gamedata", &s.game_data),
        attr("appid", crate::steamlan::app_id()),
    ];
    let players: Vec<String> = s
        .players
        .iter()
        .map(|(id, (name, score))| format!("{id}\t{score}\t{}", name.replace(['\t', '\n'], " ")))
        .collect();
    a.push(attr("players", players.join("\n")));
    for (k, v) in &s.rules {
        a.push(attr(&format!("r:{k}"), v));
    }
    a
}

/// Puts up, updates or takes down the advert. Runs from the game's dispatch.
pub fn tick() {
    let Some(lan) = lan() else { return };
    let (want, server, advert) = {
        let mut st = S.lock().unwrap();
        let Some(server) = st.server.clone() else { return };
        let want = server.logged_on && server.advertise != Some(false);
        let due = st.last_advert.is_none_or(|t| t.elapsed() >= ADVERT_EVERY);
        if want && st.advert.is_some() && !(st.dirty && due) {
            return;
        }
        if !want && st.advert.is_none() {
            return;
        }
        st.dirty = false;
        st.last_advert = Some(std::time::Instant::now());
        (want, server, st.advert.clone())
    };
    match (want, advert) {
        (true, None) => {
            let mut l = Lobby::new(NS);
            l.id = format!("steamgs:{}", server.id);
            l.max_members = 1;
            l.permission = Permission::Public;
            l.attrs = advert_attrs(&server);
            match lan.create_lobby(l) {
                Ok(l) => {
                    log!(
                        "steam-lan game server advertised: \"{}\" map={} port={} players={}/{}",
                        server.name,
                        server.map,
                        server.game_port,
                        server.players.len(),
                        server.max_players
                    );
                    S.lock().unwrap().advert = Some(l.id);
                }
                Err(e) => log!("steam-lan game server advert failed: {e:?}"),
            }
        }
        (true, Some(id)) => {
            if let Some(mut l) = lan.lobby(&id) {
                l.attrs = advert_attrs(&server);
                let _ = lan.update(&id, Some(&l), &[], &[]);
            }
        }
        (false, Some(id)) => {
            let _ = lan.destroy(&id);
            S.lock().unwrap().advert = None;
            log!("steam-lan game server no longer advertised");
        }
        (false, None) => {}
    }
}

fn text(l: &Lobby, key: &str) -> String {
    l.attrs
        .iter()
        .find(|a| a.key == key)
        .map(|a| a.value.text())
        .unwrap_or_default()
}

fn num<T: std::str::FromStr + Default>(l: &Lobby, key: &str) -> T {
    text(l, key).trim().parse().unwrap_or_default()
}

fn copy(dst: &mut [u8], src: &str) {
    let n = src.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&src.as_bytes()[..n]);
    dst[n] = 0;
}

fn players_of(l: &Lobby) -> Vec<(u64, u32, String)> {
    text(l, "players")
        .lines()
        .filter_map(|line| {
            let mut it = line.splitn(3, '\t');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next().unwrap_or("").to_string()))
        })
        .collect()
}

/// The address a client reaches an advertised server at: its host's LAN address.
fn address_of(l: &Lobby) -> u32 {
    let advertised: u32 = num(l, "ip");
    let host = lan().and_then(|lan| {
        if l.owner == lan.me() {
            return None;
        }
        lan.game_address(l.owner)
    });
    match host {
        Some(std::net::IpAddr::V4(v4)) => u32::from(v4),
        _ if advertised != 0 => advertised,
        _ => u32::from(std::net::Ipv4Addr::LOCALHOST),
    }
}

fn item_of(l: &Lobby) -> ServerItem {
    let players = players_of(l);
    let bots: i32 = num(l, "bots");
    let sid: u64 = num(l, "sid");
    let mut item = ServerItem {
        connection_port: num(l, "port"),
        query_port: num(l, "qport"),
        ip: address_of(l),
        ping: 1,
        had_response: true,
        do_not_refresh: false,
        game_dir: [0; 32],
        map: [0; 32],
        description: [0; 64],
        app_id: num(l, "appid"),
        players: players.len() as i32 + bots,
        max_players: num(l, "max"),
        bots,
        password: text(l, "password") == "1",
        secure: false,
        last_played: 0,
        version: version_number(&text(l, "version")),
        name: [0; 64],
        tags: [0; 128],
        steam_id: sid.to_le_bytes(),
        friends_now: 0,
        friends_total: 0,
    };
    copy(&mut item.game_dir, &text(l, "gamedir"));
    copy(&mut item.map, &text(l, "map"));
    copy(&mut item.description, &text(l, "desc"));
    let name = text(l, "name");
    copy(&mut item.name, if name.is_empty() { &l.id } else { &name });
    copy(&mut item.tags, &text(l, "tags"));
    if item.query_port == 0 {
        item.query_port = item.connection_port;
    }
    item
}

/// Steam reports a version string "1.2.3.4" as the number 1234.
fn version_number(v: &str) -> i32 {
    v.chars().filter(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)
}

/// Steam's `*` wildcards, case-insensitive.
fn wildcard(pattern: &str, text: &str) -> bool {
    let p = pattern.to_ascii_lowercase();
    let t = text.to_ascii_lowercase();
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p == t;
    }
    let mut rest = t.as_str();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else { return false };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            let Some(at) = rest.find(part) else { return false };
            rest = &rest[at + part.len()..];
        }
    }
    true
}

fn tag_list(v: &str) -> Vec<String> {
    v.split(',').map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()).collect()
}

/// Whether a server passes a request's filters. "or", "nand" and "nor" groups are
/// skipped rather than half-evaluated, so they never hide a server.
fn passes(l: &Lobby, filters: &[(String, String)]) -> bool {
    let item = item_of(l);
    let mut skip = 0usize;
    for (k, v) in filters {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        let key = k.to_ascii_lowercase();
        let ok = match key.as_str() {
            "and" => true,
            "or" | "nand" | "nor" => {
                skip = v.trim().parse().unwrap_or(0);
                true
            }
            "gamedir" => text(l, "gamedir").eq_ignore_ascii_case(v),
            "map" => text(l, "map").eq_ignore_ascii_case(v),
            "dedicated" => text(l, "dedicated") == "1",
            "notfull" => item.max_players == 0 || item.players < item.max_players,
            "hasplayers" => item.players > 0,
            "noplayers" => item.players == 0,
            "password" => (text(l, "password") == "1") == (v.trim() == "1"),
            "name_match" => wildcard(v, &text(l, "name")),
            "version_match" => wildcard(v, &text(l, "version")),
            "gametagsand" => {
                let have = tag_list(&text(l, "tags"));
                tag_list(v).iter().all(|t| have.contains(t))
            }
            "gametagsnor" => {
                let have = tag_list(&text(l, "tags"));
                !tag_list(v).iter().any(|t| have.contains(t))
            }
            "gamedataand" => {
                let have = tag_list(&text(l, "gamedata"));
                tag_list(v).iter().all(|t| have.contains(t))
            }
            "gamedataor" => {
                let have = tag_list(&text(l, "gamedata"));
                tag_list(v).iter().any(|t| have.contains(t))
            }
            // Secure (VAC), region, app id, white/black lists and the rest don't apply
            // to LAN servers.
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

// ---- ISteamGameServer ----

pub fn server() -> Vec<(&'static str, usize)> {
    vec![
        ("SetProduct", set_product as *const () as usize),
        ("SetGameDescription", set_game_description as *const () as usize),
        ("SetModDir", set_mod_dir as *const () as usize),
        ("SetDedicatedServer", set_dedicated as *const () as usize),
        ("LogOn", log_on_token as *const () as usize),
        ("LogOn~0", log_on_old as *const () as usize),
        ("LogOnAnonymous", log_on_anonymous as *const () as usize),
        ("LogOff", log_off_method as *const () as usize),
        ("BLoggedOn", logged_on as *const () as usize),
        ("BSecure", secure as *const () as usize),
        ("GetSteamID", get_steam_id as *const () as usize),
        ("SetMaxPlayerCount", set_max_players as *const () as usize),
        ("SetBotPlayerCount", set_bots as *const () as usize),
        ("SetServerName", set_server_name as *const () as usize),
        ("SetMapName", set_map_name as *const () as usize),
        ("SetPasswordProtected", set_password as *const () as usize),
        ("SetSpectatorPort", set_spectator_port as *const () as usize),
        ("UpdateSpectatorPort~1", set_spectator_port as *const () as usize),
        ("SetSpectatorServerName", set_spectator_name as *const () as usize),
        ("ClearAllKeyValues", clear_key_values as *const () as usize),
        ("SetKeyValue", set_key_value as *const () as usize),
        ("SetGameTags", set_game_tags as *const () as usize),
        ("SetGameType~1", set_game_tags as *const () as usize),
        ("SetGameData", set_game_data as *const () as usize),
        ("SetRegion", set_region as *const () as usize),
        ("SetAdvertiseServerActive", set_advertise as *const () as usize),
        ("EnableHeartbeats~1", set_advertise as *const () as usize),
        ("BeginAuthSession", begin_auth_session as *const () as usize),
        ("EndAuthSession", end_auth_session as *const () as usize),
        ("UserHasLicenseForApp", user_has_license as *const () as usize),
        ("SendUserConnectAndAuthenticate", send_user_connect as *const () as usize),
        ("CreateUnauthenticatedUserConnection", create_unauthenticated as *const () as usize),
        ("SendUserDisconnect", send_user_disconnect as *const () as usize),
        ("BUpdateUserData", update_user_data as *const () as usize),
        ("BSetServerType~8", set_server_type_old as *const () as usize),
        ("UpdateServerStatus~6", update_server_status_old as *const () as usize),
    ]
}

/// A setter: records the value, then lets the real server have it too.
macro_rules! setter_str {
    ($fn:ident, $name:literal, $field:ident) => {
        vmethod! {
            fn $fn(this: usize, v: *const c_char) {
                let text = s(v);
                with_server(|s| s.$field = text);
                call_real!(this, $name, fn(*const c_char) -> (), (), v);
            }
        }
    };
}

setter_str!(set_product, "SetProduct", product);
setter_str!(set_game_description, "SetGameDescription", description);
setter_str!(set_mod_dir, "SetModDir", mod_dir);
setter_str!(set_server_name, "SetServerName", name);
setter_str!(set_map_name, "SetMapName", map);
setter_str!(set_spectator_name, "SetSpectatorServerName", spectator_name);
setter_str!(set_game_data, "SetGameData", game_data);
setter_str!(set_region, "SetRegion", region);

vmethod! {
    fn set_game_tags(this: usize, v: *const c_char) {
        let text = s(v);
        with_server(|s| s.tags = text);
        // SetGameType~1 on old interfaces has no "SetGameTags" slot, so this skips it.
        call_real!(this, "SetGameTags", fn(*const c_char) -> (), (), v);
    }
}

vmethod! {
    fn set_dedicated(this: usize, v: bool) {
        with_server(|s| s.dedicated = v);
        call_real!(this, "SetDedicatedServer", fn(bool) -> (), (), v);
    }
}

vmethod! {
    fn set_max_players(this: usize, v: i32) {
        with_server(|s| s.max_players = v);
        call_real!(this, "SetMaxPlayerCount", fn(i32) -> (), (), v);
    }
}

vmethod! {
    fn set_bots(this: usize, v: i32) {
        with_server(|s| s.bots = v);
        call_real!(this, "SetBotPlayerCount", fn(i32) -> (), (), v);
    }
}

vmethod! {
    fn set_password(this: usize, v: bool) {
        with_server(|s| s.password = v);
        call_real!(this, "SetPasswordProtected", fn(bool) -> (), (), v);
    }
}

vmethod! {
    fn set_spectator_port(_this: usize, v: u16) {
        with_server(|s| s.spectator_port = v);
    }
}

vmethod! {
    fn clear_key_values(this: usize) {
        with_server(|s| s.rules.clear());
        call_real!(this, "ClearAllKeyValues", fn() -> (), ());
    }
}

vmethod! {
    fn set_key_value(this: usize, k: *const c_char, v: *const c_char) {
        let (key, value) = (s(k), s(v));
        if !key.is_empty() {
            with_server(|s| {
                s.rules.insert(key, value);
            });
        }
        call_real!(this, "SetKeyValue", fn(*const c_char, *const c_char) -> (), (), k, v);
    }
}

vmethod! {
    fn set_advertise(_this: usize, v: bool) {
        with_server(|s| s.advertise = Some(v));
        log!("steam-lan game server advertising {}", if v { "on" } else { "off" });
    }
}

vmethod! { fn log_on_token(_this: usize, _token: *const c_char) { log_on(false) } }
vmethod! { fn log_on_old(_this: usize) { log_on(false) } }
vmethod! { fn log_on_anonymous(_this: usize) { log_on(true) } }
vmethod! { fn log_off_method(_this: usize) { log_off() } }

vmethod! {
    fn logged_on(_this: usize) -> bool {
        my_server_id() != 0
    }
}

vmethod! { fn secure(_this: usize) -> bool { false } }

vmethod! {
    /// Returns a CSteamID through MSVC's hidden pointer.
    fn get_steam_id(this: usize, ret: *mut u64) -> *mut u64 {
        let mut id = my_server_id();
        if id == 0 {
            let _ = call_real!(this, "GetSteamID", fn(*mut u64) -> *mut u64, std::ptr::null_mut(), &mut id);
        }
        unsafe { *ret = id };
        ret
    }
}

vmethod! {
    fn begin_auth_session(this: usize, ticket: *const u8, len: i32, user: u64) -> i32 {
        if peer_of(user).is_none() {
            return call_real!(this, "BeginAuthSession", fn(*const u8, i32, u64) -> i32, 1, ticket, len, user);
        }
        add_player(user);
        log!("steam-lan game server: LAN player {user} authenticated");
        // k_EBeginAuthSessionResultOK, then k_EAuthSessionResponseOK.
        // Sign-in between PeerParty players (LEGAL.md, "Sign-in is answered locally"): OK
        // for the player at that address, named as their own owner. Steam isn't asked.
        crate::steam::inject_server(
            VALIDATE_AUTH_TICKET_RESPONSE,
            bytes(&AuthResponse { steam_id: user, response: 0, owner: user }),
        );
        0
    }
}

vmethod! {
    fn end_auth_session(this: usize, user: u64) {
        with_server(|s| {
            s.players.remove(&user);
        });
        if peer_of(user).is_none() {
            call_real!(this, "EndAuthSession", fn(u64) -> (), (), user);
        }
    }
}

vmethod! {
    /// steam_api answers; PeerParty never says "has a licence" itself.
    /// With nothing to ask, or an answer that isn't one of EUserHasLicenseForAppResult's
    /// three, it's k_EUserHasLicenseResultNoAuth (2): can't verify.
    fn user_has_license(this: usize, user: u64, app: u32) -> i32 {
        let r = call_real!(this, "UserHasLicenseForApp", fn(u64, u32) -> i32, 2, user, app);
        if (0..=2).contains(&r) { r } else { 2 }
    }
}

vmethod! {
    /// The old auth: the ticket's owner is the LAN player at that address.
    fn send_user_connect(this: usize, ip: u32, data: *const u8, len: u32, out: *mut u64) -> bool {
        let user = player_at(ip);
        let Some(user) = user else {
            return call_real!(this, "SendUserConnectAndAuthenticate", fn(u32, *const u8, u32, *mut u64) -> bool, false, ip, data, len, out);
        };
        if !out.is_null() {
            unsafe { *out = user };
        }
        add_player(user);
        log!("steam-lan game server: LAN player {user} connected");
        // Sign-in between PeerParty players again (see the auth reply above).
        crate::steam::inject_server(GS_CLIENT_APPROVE, bytes(&ClientApprove { steam_id: user, owner: user }));
        true
    }
}

vmethod! {
    fn create_unauthenticated(_this: usize, ret: *mut u64) -> *mut u64 {
        // An anonymous user (a bot): account type 10, a fresh account id.
        let account = u32::from_le_bytes(PeerId::random().0[..4].try_into().unwrap()) | 1;
        let id = (1u64 << 56) | (10u64 << 52) | account as u64;
        with_server(|s| {
            s.players.insert(id, (String::new(), 0));
        });
        unsafe { *ret = id };
        ret
    }
}

vmethod! {
    fn send_user_disconnect(this: usize, user: u64) {
        with_server(|s| {
            s.players.remove(&user);
        });
        if peer_of(user).is_none() {
            call_real!(this, "SendUserDisconnect", fn(u64) -> (), (), user);
        }
    }
}

vmethod! {
    fn update_user_data(_this: usize, user: u64, name: *const c_char, score: u32) -> bool {
        let name = s(name);
        with_server(|s| {
            s.players.insert(user, (name, score));
        });
        true
    }
}

vmethod! {
    /// Old interfaces (004 to 010): flags, address, ports, game dir, version, LAN mode.
    fn set_server_type_old(
        _this: usize,
        _flags: u32,
        ip: u32,
        game_port: u16,
        spectator_port: u16,
        query_port: u16,
        game_dir: *const c_char,
        version: *const c_char,
        _lan: bool,
    ) -> bool {
        let (dir, ver) = (s(game_dir), s(version));
        with_server(|s| {
            s.ip = ip;
            s.game_port = game_port;
            s.spectator_port = spectator_port;
            s.query_port = query_port;
            s.mod_dir = dir;
            s.version = ver;
        });
        true
    }
}

vmethod! {
    fn update_server_status_old(
        _this: usize,
        _players: i32,
        max: i32,
        bots: i32,
        name: *const c_char,
        spectator_name: *const c_char,
        map: *const c_char,
    ) {
        let (n, sn, m) = (s(name), s(spectator_name), s(map));
        with_server(|s| {
            s.max_players = max;
            s.bots = bots;
            s.name = n;
            s.spectator_name = sn;
            s.map = m;
        });
    }
}

/// The LAN player at a client address the game gives (host byte order).
fn player_at(ip: u32) -> Option<u64> {
    let lan = lan()?;
    let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::from(ip));
    let known: Vec<u64> = S.lock().unwrap().server.as_ref().map(|s| s.players.keys().copied().collect()).unwrap_or_default();
    let mut at: Vec<u64> = lan
        .peers()
        .into_iter()
        .filter(|(p, _)| lan.peer_has_ip(*p, addr) || (addr.is_loopback() && lan.peer_ip(*p).is_some_and(|a| a.is_loopback())))
        .map(|(p, _)| steam_id(p))
        .collect();
    // Two players behind one address: the one not on the server yet.
    at.sort_by_key(|id| known.contains(id));
    at.into_iter().next()
}

// ---- server init and dispatch exports ----

type InitV2Fn = unsafe extern "C" fn(u32, u16, u16, u32, *const c_char, *const c_char, *mut c_char) -> i32;
type InitFn = unsafe extern "C" fn(u32, u16, u16, u16, u32, *const c_char) -> bool;
type VoidFn = unsafe extern "C" fn();
type IdFn = unsafe extern "C" fn() -> u64;
type BoolFn = unsafe extern "C" fn() -> bool;

static INIT_V2: OnceLock<GenericDetour<InitV2Fn>> = OnceLock::new();
static INIT: OnceLock<GenericDetour<InitFn>> = OnceLock::new();
static SHUTDOWN: OnceLock<GenericDetour<VoidFn>> = OnceLock::new();
static RUN_CALLBACKS: OnceLock<GenericDetour<VoidFn>> = OnceLock::new();
static GET_ID: OnceLock<GenericDetour<IdFn>> = OnceLock::new();
static BSECURE: OnceLock<GenericDetour<BoolFn>> = OnceLock::new();

fn started(ip: u32, game_port: u16, query_port: u16, version: &str) {
    let version = version.to_string();
    with_server(|s| {
        s.ip = ip;
        s.game_port = game_port;
        s.query_port = query_port;
        s.version = version.clone();
    });
    log!("steam-lan game server started: ip={ip:#x} port={game_port} query={query_port} version={version}");
}

unsafe extern "C" fn detour_init_v2(
    ip: u32,
    game_port: u16,
    query_port: u16,
    mode: u32,
    version: *const c_char,
    interfaces: *const c_char,
    err: *mut c_char,
) -> i32 {
    let Some(h) = INIT_V2.get() else { return 1 };
    // Set first: steam_api fetches the server's interfaces on its new pipe during Init.
    SERVER_STARTED.store(true, Ordering::Relaxed);
    let rc = unsafe { h.call(ip, game_port, query_port, mode, version, interfaces, err) };
    SERVER_STARTED.store(rc == 0, Ordering::Relaxed);
    if rc == 0 {
        started(ip, game_port, query_port, &s(version));
    } else {
        log!("steam-lan game server init failed ({rc}): {}", crate::read::cstr(err as usize).unwrap_or_default());
    }
    rc
}

unsafe extern "C" fn detour_init(ip: u32, steam_port: u16, game_port: u16, query_port: u16, mode: u32, version: *const c_char) -> bool {
    let Some(h) = INIT.get() else { return false };
    SERVER_STARTED.store(true, Ordering::Relaxed);
    let ok = unsafe { h.call(ip, steam_port, game_port, query_port, mode, version) };
    SERVER_STARTED.store(ok, Ordering::Relaxed);
    if ok {
        started(ip, game_port, query_port, &s(version));
    } else {
        log!("steam-lan game server init failed");
    }
    ok
}

unsafe extern "C" fn detour_shutdown() {
    log_off();
    S.lock().unwrap().server = None;
    if let Some(h) = SHUTDOWN.get() {
        unsafe { h.call() };
    }
    SERVER_STARTED.store(false, Ordering::Relaxed);
}

unsafe extern "C" fn detour_run_callbacks() {
    if let Some(h) = RUN_CALLBACKS.get() {
        unsafe { h.call() };
    }
    let _ = std::panic::catch_unwind(crate::steamlan::pump);
    crate::steam::deliver_server();
}

unsafe extern "C" fn detour_get_id() -> u64 {
    let id = my_server_id();
    if id != 0 {
        return id;
    }
    GET_ID.get().map_or(0, |h| unsafe { h.call() })
}

unsafe extern "C" fn detour_bsecure() -> bool {
    if my_server_id() != 0 {
        return false;
    }
    BSECURE.get().is_some_and(|h| unsafe { h.call() })
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, addr: usize, detour: T) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let target: T = unsafe { std::mem::transmute_copy(&addr) };
        let Ok(h) = (unsafe { GenericDetour::new(target, detour) }) else {
            return false;
        };
        if slot.set(h).is_err() {
            return false;
        }
        unsafe { slot.get().unwrap().enable() }.is_ok()
    })
}

/// Hooks steam_api's game server exports. Returns the addresses it patched.
pub fn attach(exports: &[Export]) -> Vec<usize> {
    let find = |n: &str| exports.iter().find(|e| e.name == n).map(|e| e.addr as usize);
    if let Some(a) = find("SteamGameServer_GetHSteamPipe") {
        GET_PIPE.store(a, Ordering::Relaxed);
    }
    if let Some(a) = find("SteamAPI_GetHSteamPipe").or_else(|| find("GetHSteamPipe")) {
        GET_CLIENT_PIPE.store(a, Ordering::Relaxed);
    }
    let mut patched = Vec::new();
    let mut try_arm = |name: &str, ok: &dyn Fn(usize) -> bool| {
        if let Some(a) = find(name) {
            if ok(a) {
                patched.push(a);
            }
        }
    };
    try_arm("SteamInternal_GameServer_Init_V2", &|a| arm(&INIT_V2, a, detour_init_v2 as InitV2Fn));
    try_arm("SteamInternal_GameServer_Init", &|a| arm(&INIT, a, detour_init as InitFn));
    try_arm("SteamGameServer_Shutdown", &|a| arm(&SHUTDOWN, a, detour_shutdown as VoidFn));
    try_arm("SteamGameServer_RunCallbacks", &|a| arm(&RUN_CALLBACKS, a, detour_run_callbacks as VoidFn));
    try_arm("SteamGameServer_GetSteamID", &|a| arm(&GET_ID, a, detour_get_id as IdFn));
    try_arm("SteamGameServer_BSecure", &|a| arm(&BSECURE, a, detour_bsecure as BoolFn));
    if !patched.is_empty() {
        log!("steam-lan game server exports hooked: {}", patched.len());
    }
    patched
}

// ---- ISteamMatchmakingServers ----

pub fn browser() -> Vec<(&'static str, usize)> {
    vec![
        ("RequestInternetServerList", request_internet as *const () as usize),
        ("RequestLANServerList", request_lan as *const () as usize),
        ("RequestFriendsServerList", request_friends as *const () as usize),
        ("RequestFavoritesServerList", request_favorites as *const () as usize),
        ("RequestHistoryServerList", request_history as *const () as usize),
        ("RequestSpectatorServerList", request_spectator as *const () as usize),
        ("ReleaseRequest", release_request as *const () as usize),
        ("GetServerDetails", get_server_details as *const () as usize),
        ("CancelQuery", cancel_query as *const () as usize),
        ("RefreshQuery", refresh_query as *const () as usize),
        ("IsRefreshing", is_refreshing as *const () as usize),
        ("GetServerCount", get_server_count as *const () as usize),
        ("RefreshServer", refresh_server as *const () as usize),
        ("PingServer", ping_server as *const () as usize),
        ("PlayerDetails", player_details as *const () as usize),
        ("ServerRules", server_rules as *const () as usize),
        ("CancelServerQuery", cancel_server_query as *const () as usize),
    ]
}

unsafe fn read_filters(filters: *const *const KeyValue, n: u32) -> Vec<(String, String)> {
    if filters.is_null() {
        return Vec::new();
    }
    let cut = |b: &[u8]| String::from_utf8_lossy(&b[..b.iter().position(|c| *c == 0).unwrap_or(b.len())]).into_owned();
    (0..n.min(64) as usize)
        .filter_map(|i| {
            // Steam's own headers pass a pointer to one array of pairs.
            let base = unsafe { *filters };
            if base.is_null() {
                return None;
            }
            let kv = unsafe { &*base.add(i) };
            Some((cut(&kv.key), cut(&kv.value)))
        })
        .collect()
}

fn start_list(kind: ListKind, filters: Vec<(String, String)>, response: usize) -> usize {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let search = searches(kind).then(|| lan().map(|l| l.search(Query::new(NS)))).flatten();
    log!("steam-lan server list {kind:?} request={handle:#x} filters={filters:?}");
    S.lock().unwrap().lists.insert(
        handle,
        List {
            kind,
            filters,
            response,
            search,
            items: Vec::new(),
            refreshing: true,
        },
    );
    if search.is_none() {
        // Nothing to look for: complete on the next dispatch.
        PENDING_EMPTY.lock().unwrap().push(handle);
    }
    handle
}

/// Which lists hold LAN servers.
fn searches(kind: ListKind) -> bool {
    matches!(kind, ListKind::Internet | ListKind::Lan | ListKind::Friends)
}

static PENDING_EMPTY: Mutex<Vec<usize>> = Mutex::new(Vec::new());

vmethod! {
    fn request_internet(_this: usize, _app: u32, filters: *const *const KeyValue, n: u32, response: usize) -> usize {
        start_list(ListKind::Internet, unsafe { read_filters(filters, n) }, response)
    }
}
vmethod! {
    fn request_lan(_this: usize, _app: u32, response: usize) -> usize {
        start_list(ListKind::Lan, Vec::new(), response)
    }
}
vmethod! {
    fn request_friends(_this: usize, _app: u32, filters: *const *const KeyValue, n: u32, response: usize) -> usize {
        start_list(ListKind::Friends, unsafe { read_filters(filters, n) }, response)
    }
}
vmethod! {
    fn request_favorites(_this: usize, _app: u32, filters: *const *const KeyValue, n: u32, response: usize) -> usize {
        start_list(ListKind::Favorites, unsafe { read_filters(filters, n) }, response)
    }
}
vmethod! {
    fn request_history(_this: usize, _app: u32, filters: *const *const KeyValue, n: u32, response: usize) -> usize {
        start_list(ListKind::History, unsafe { read_filters(filters, n) }, response)
    }
}
vmethod! {
    fn request_spectator(_this: usize, _app: u32, filters: *const *const KeyValue, n: u32, response: usize) -> usize {
        start_list(ListKind::Spectator, unsafe { read_filters(filters, n) }, response)
    }
}

vmethod! {
    fn release_request(_this: usize, handle: usize) {
        S.lock().unwrap().lists.remove(&handle);
    }
}

vmethod! {
    fn get_server_details(_this: usize, handle: usize, index: i32) -> *mut ServerItem {
        let mut st = S.lock().unwrap();
        st.lists
            .get_mut(&handle)
            .and_then(|l| l.items.get_mut(index.max(0) as usize))
            .map_or(std::ptr::null_mut(), |b| &mut **b as *mut ServerItem)
    }
}

vmethod! {
    fn cancel_query(_this: usize, handle: usize) {
        if let Some(l) = S.lock().unwrap().lists.get_mut(&handle) {
            l.search = None;
            l.refreshing = false;
            l.response = 0;
        }
    }
}

vmethod! {
    fn refresh_query(_this: usize, handle: usize) {
        let mut st = S.lock().unwrap();
        let Some(l) = st.lists.get_mut(&handle) else { return };
        l.items.clear();
        l.refreshing = true;
        l.search = searches(l.kind).then(|| lan().map(|lan| lan.search(Query::new(NS)))).flatten();
        if l.search.is_none() {
            PENDING_EMPTY.lock().unwrap().push(handle);
        }
    }
}

vmethod! {
    fn is_refreshing(_this: usize, handle: usize) -> bool {
        S.lock().unwrap().lists.get(&handle).is_some_and(|l| l.refreshing)
    }
}

vmethod! {
    fn get_server_count(_this: usize, handle: usize) -> i32 {
        S.lock().unwrap().lists.get(&handle).map_or(0, |l| l.items.len() as i32)
    }
}

vmethod! { fn refresh_server(_this: usize, _handle: usize, _index: i32) {} }

fn start_query(kind: QueryKind, ip: u32, port: u16, response: usize) -> i32 {
    let Some(lan) = lan() else { return 0 };
    let id = NEXT_QUERY.fetch_add(1, Ordering::Relaxed) as i32;
    let search = lan.search(Query::new(NS));
    S.lock().unwrap().queries.insert(id, ServerQuery { kind, ip, port, response, search });
    id
}

vmethod! {
    fn ping_server(_this: usize, ip: u32, port: u16, response: usize) -> i32 {
        start_query(QueryKind::Ping, ip, port, response)
    }
}
vmethod! {
    fn player_details(_this: usize, ip: u32, port: u16, response: usize) -> i32 {
        start_query(QueryKind::Players, ip, port, response)
    }
}
vmethod! {
    fn server_rules(_this: usize, ip: u32, port: u16, response: usize) -> i32 {
        start_query(QueryKind::Rules, ip, port, response)
    }
}
vmethod! {
    fn cancel_server_query(_this: usize, query: i32) {
        S.lock().unwrap().queries.remove(&query);
    }
}

// ---- calling the game's response objects ----

/// Calls virtual method `slot` of a game object (thiscall on x86).
macro_rules! virt {
    ($obj:expr, $slot:expr, fn($($t:ty),*) $(, $arg:expr)*) => {{
        let obj: usize = $obj;
        unsafe {
            let f = *(*(obj as *const *const usize)).add($slot);
            #[cfg(target_arch = "x86")]
            let f: unsafe extern "thiscall" fn(usize $(, $t)*) = std::mem::transmute(f);
            #[cfg(not(target_arch = "x86"))]
            let f: unsafe extern "C" fn(usize $(, $t)*) = std::mem::transmute(f);
            f(obj $(, $arg)*)
        }
    }};
}

enum Call {
    Responded(usize, usize, i32),
    Complete(usize, usize, i32),
    Ping(usize, Option<Box<ServerItem>>),
    Player(usize, String, i32),
    PlayersDone(usize, bool),
    Rule(usize, String, String),
    RulesDone(usize, bool),
}

fn run(calls: Vec<Call>) {
    for c in calls {
        match c {
            Call::Responded(resp, h, i) => virt!(resp, 0, fn(usize, i32), h, i),
            Call::Complete(resp, h, r) => virt!(resp, 2, fn(usize, i32), h, r),
            Call::Ping(resp, Some(mut item)) => virt!(resp, 0, fn(*mut ServerItem), &mut *item as *mut ServerItem),
            Call::Ping(resp, None) => virt!(resp, 1, fn()),
            Call::Player(resp, name, score) => {
                let name = std::ffi::CString::new(name).unwrap_or_default();
                virt!(resp, 0, fn(*const c_char, i32, f32), name.as_ptr(), score, 0.0f32)
            }
            Call::PlayersDone(resp, true) => virt!(resp, 2, fn()),
            Call::PlayersDone(resp, false) => virt!(resp, 1, fn()),
            Call::Rule(resp, k, v) => {
                let k = std::ffi::CString::new(k).unwrap_or_default();
                let v = std::ffi::CString::new(v).unwrap_or_default();
                virt!(resp, 0, fn(*const c_char, *const c_char), k.as_ptr(), v.as_ptr())
            }
            Call::RulesDone(resp, true) => virt!(resp, 2, fn()),
            Call::RulesDone(resp, false) => virt!(resp, 1, fn()),
        }
    }
}

fn matches_address(l: &Lobby, ip: u32, port: u16) -> bool {
    let item = item_of(l);
    let port_ok = port == item.connection_port || port == item.query_port;
    let ip_ok = ip == item.ip || ip == num::<u32>(l, "ip") || std::net::Ipv4Addr::from(ip).is_loopback();
    port_ok && ip_ok
}

/// Answers browser requests whose LAN search finished, and empty lists.
pub fn on_event(ev: &Event) {
    let Event::SearchDone { search, results } = ev else { return };
    {
        let mut st = S.lock().unwrap();
        for l in results {
            let sid: u64 = num(l, "sid");
            if sid != 0 {
                st.hosts.insert(sid, l.owner);
            }
        }
    }
    let mut calls = Vec::new();
    {
        let mut st = S.lock().unwrap();
        for (h, list) in st.lists.iter_mut().filter(|(_, l)| l.search == Some(*search)) {
            list.search = None;
            list.refreshing = false;
            list.items = results
                .iter()
                .filter(|l| l.namespace == NS && passes(l, &list.filters))
                .map(|l| Box::new(item_of(l)))
                .collect();
            log!("steam-lan server list {:?} request={h:#x}: {} servers", list.kind, list.items.len());
            if list.response == 0 {
                continue;
            }
            for i in 0..list.items.len() {
                calls.push(Call::Responded(list.response, *h, i as i32));
            }
            // eServerResponded, or eNoServersListedOnMasterServer.
            let r = if list.items.is_empty() { 2 } else { 0 };
            calls.push(Call::Complete(list.response, *h, r));
        }
        let done: Vec<i32> = st.queries.iter().filter(|(_, q)| q.search == *search).map(|(id, _)| *id).collect();
        for id in done {
            let q = st.queries.remove(&id).unwrap();
            let found = results.iter().find(|l| l.namespace == NS && matches_address(l, q.ip, q.port));
            match q.kind {
                QueryKind::Ping => calls.push(Call::Ping(q.response, found.map(|l| Box::new(item_of(l))))),
                QueryKind::Players => {
                    if let Some(l) = found {
                        for (_, score, name) in players_of(l) {
                            calls.push(Call::Player(q.response, name, score as i32));
                        }
                    }
                    calls.push(Call::PlayersDone(q.response, found.is_some()));
                }
                QueryKind::Rules => {
                    if let Some(l) = found {
                        for a in l.attrs.iter().filter(|a| a.key.starts_with("r:")) {
                            calls.push(Call::Rule(q.response, a.key[2..].to_string(), a.value.text()));
                        }
                    }
                    calls.push(Call::RulesDone(q.response, found.is_some()));
                }
            }
        }
    }
    run(calls);
}

/// Completes lists that have nothing to search. Runs from the game's dispatch.
pub fn finish_empty() {
    let handles: Vec<usize> = std::mem::take(&mut *PENDING_EMPTY.lock().unwrap());
    let mut calls = Vec::new();
    {
        let mut st = S.lock().unwrap();
        for h in handles {
            if let Some(l) = st.lists.get_mut(&h) {
                l.refreshing = false;
                if l.response != 0 {
                    calls.push(Call::Complete(l.response, h, 2));
                }
            }
        }
    }
    run(calls);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advert(kv: &[(&str, &str)]) -> Lobby {
        let mut l = Lobby::new(NS);
        l.id = "steamgs:1".into();
        l.attrs = kv.iter().map(|(k, v)| attr(k, v)).collect();
        l
    }

    #[test]
    fn no_server_pipe_without_a_game_server() {
        // A game with no SteamGameServer_Init, whose steam_api's
        // SteamGameServer_GetHSteamPipe gives the game's own pipe. Its callbacks (the
        // auth ticket's, 163) must go to the game's dispatch.
        assert_eq!(pick_server_pipe(false, 1, 1), None);
        assert_eq!(pick_server_pipe(false, 2, 1), None);
        assert_eq!(pick_server_pipe(false, 0, 1), None);
        // A server pipe equal to the game's is never the server's own.
        assert_eq!(pick_server_pipe(true, 1, 1), None);
        assert_eq!(pick_server_pipe(true, 0, 1), None);
        // A started game server with a pipe of its own gets its dispatch.
        assert_eq!(pick_server_pipe(true, 2, 1), Some(2));
        // The game's pipe unknown (no getter export): the server pipe still counts.
        assert_eq!(pick_server_pipe(true, 2, 0), Some(2));
    }

    #[test]
    fn server_pipe_is_none_before_a_game_server_starts() {
        // No steam_api hooked and no game server: nothing is the server's.
        assert!(!SERVER_STARTED.load(Ordering::Relaxed));
        assert_eq!(server_pipe(), None);
        assert_eq!(server_pipe(), None);
    }

    #[test]
    fn items_carry_steams_layout_and_values() {
        let l = advert(&[
            ("sid", "90071992547409921"),
            ("ip", "3232235777"),
            ("port", "27015"),
            ("qport", "27016"),
            ("name", "LAN game"),
            ("map", "de_lan"),
            ("gamedir", "mygame"),
            ("max", "8"),
            ("bots", "1"),
            ("players", "76561197960287930\t5\tAlice\n76561197960287931\t2\tBob"),
            ("version", "1.0.2.3"),
            ("tags", "coop,hard"),
        ]);
        let item = item_of(&l);
        assert_eq!(item.connection_port, 27015);
        assert_eq!(item.query_port, 27016);
        assert_eq!(item.players, 3, "players include bots");
        assert_eq!(item.max_players, 8);
        assert_eq!(item.version, 1023);
        assert_eq!(&item.map[..7], b"de_lan\0");
        assert_eq!(u64::from_le_bytes(item.steam_id), 90071992547409921);
        let bytes = unsafe { std::slice::from_raw_parts(&item as *const ServerItem as *const u8, 380) };
        assert_eq!(&bytes[172..181], b"LAN game\0", "name at Steam's offset");
        assert_eq!(&bytes[0..2], &27015u16.to_le_bytes(), "connection port first");
    }

    #[test]
    fn filters_work_like_the_master_server() {
        let l = advert(&[
            ("gamedir", "mygame"),
            ("map", "Forest"),
            ("tags", "coop, hard"),
            ("max", "4"),
            ("players", "1\t0\tA"),
            ("name", "Kim's server"),
            ("dedicated", "0"),
        ]);
        let f = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
        assert!(passes(&l, &f(&[("gamedir", "MyGame"), ("map", "forest")])));
        assert!(!passes(&l, &f(&[("gamedir", "other")])));
        assert!(passes(&l, &f(&[("gametagsand", "coop,hard"), ("notfull", "")])));
        assert!(!passes(&l, &f(&[("gametagsand", "coop,pvp")])));
        assert!(!passes(&l, &f(&[("gametagsnor", "hard")])));
        assert!(passes(&l, &f(&[("hasplayers", ""), ("name_match", "*server")])));
        assert!(!passes(&l, &f(&[("noplayers", "")])));
        assert!(!passes(&l, &f(&[("dedicated", "1")])));
        // An "or" group is skipped, not half-applied.
        assert!(passes(&l, &f(&[("or", "2"), ("map", "Desert"), ("map", "Snow"), ("gamedir", "mygame")])));
    }

    #[test]
    fn server_ids_are_game_servers() {
        let anon = new_server_id(true);
        assert_eq!(anon >> 56, 1, "public universe");
        assert_eq!((anon >> 52) & 0xf, 4, "anonymous game server");
        assert_ne!(anon as u32, 0, "account id");
        assert_eq!((new_server_id(false) >> 52) & 0xf, 3, "game server");
        assert_eq!(size_of::<ClientApprove>(), 16);
        assert_eq!(size_of::<KeyValue>(), 512);
    }

    #[test]
    fn wildcards_match_like_steam() {
        assert!(wildcard("*lan*", "My LAN party"));
        assert!(wildcard("my*", "My server"));
        assert!(!wildcard("my*", "Your server"));
        assert!(wildcard("exact", "EXACT"));
    }
}
