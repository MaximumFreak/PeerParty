//! PlayFab's web API, answered locally.
//!
//! Every call gets PlayFab's envelope (`{"code":200,"status":"OK","data":{...}}`), except
//! store calls (purchases, receipts, coupons, currency, consuming items, opening containers,
//! grants, trades, Economy v2 inventory changes, and the whole Admin API), which fail as
//! unavailable (`store_call`).
//! Sign-in always succeeds: the player's PlayFab and entity ids come from their LAN
//! identity, so each copy keeps the same ids across runs and other copies can work out
//! anyone's entity id (Party endpoints carry it). Calls PeerParty has no data for (title
//! data, CloudScript, catalogs, groups) succeed with nothing in them, the way a title
//! with nothing configured answers. A CloudScript function that deletes the player's
//! account (its name says delete and account) deletes nothing and answers
//! `NOT_NEEDED` as its result: there is no PlayFab account to delete.
//!
//! The player's own statistics and user data are kept on disk
//! (`%LOCALAPPDATA%\peerparty\playfab\<game>\<entity id>.json`); leaderboards list the
//! player's own score. LAN players are the friends list.
//!
//! Lobbies made, joined, changed, left and deleted through the web API are the PlayFab
//! Multiplayer library's LAN lobbies (pfmp.rs), so the library and the web API mix.
//!
//! Matchmaking tickets are LAN lobbies (namespace `playfab.ticket`) holding the queue
//! and the ticket's attributes. A ticket is matched with the oldest compatible ticket
//! in its queue when that ticket's oldest compatible ticket is it too, so both copies
//! pick the same pair on their own; the first to notice marks its ticket with the match
//! id and the other adopts it. PlayFab's real rules live on its servers, per title;
//! `compatible` approximates the common ones (see there).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use peerparty_core::{Attr, Event, Lobby, PeerId, Query, Value};

use super::http::Request;
use crate::json::Json;
use crate::log;
use crate::net::{self, Front};

const NS: &str = "playfab.ticket";
const ENTITY_TYPE: &str = "title_player_account";
/// How long a matched ticket stays visible so the other side can adopt the match.
const MATCH_LINGER: Duration = Duration::from_secs(30);
const SEARCH_WAIT: Duration = Duration::from_secs(2);

/// A peer's PlayFab entity id: 16 hex digits, like PlayFab's own.
pub fn entity_of(peer: PeerId) -> String {
    peer.0[..8].iter().map(|b| format!("{b:02X}")).collect()
}

/// Our entity id, or empty before the LAN is up.
pub fn entity_id() -> String {
    net::lan().map(|l| entity_of(l.me())).unwrap_or_default()
}

fn playfab_id() -> String {
    net::lan().map(|l| playfab_of(l.me())).unwrap_or_default()
}

/// A peer's PlayFab (master player) id.
fn playfab_of(peer: PeerId) -> String {
    peer.0[8..].iter().map(|b| format!("{b:02X}")).collect()
}

fn display_name() -> String {
    net::lan().map(|l| l.name().to_string()).unwrap_or_default()
}

struct Ticket {
    queue: String,
    /// Who the ticket is for: us, under whichever entity id the game signed in with.
    entity: String,
    data: String,
    created: SystemTime,
    started: Instant,
    give_up: f64,
    canceled: Option<&'static str>,
    matched: Option<(String, Instant)>,
}

#[derive(Default)]
struct State {
    tickets: HashMap<String, Ticket>,
    matches: HashMap<String, Vec<(String, String)>>,
    avatar_url: String,
    saved: Saved,
}

/// The player's statistics and user data, kept on disk.
#[derive(Default)]
struct Saved {
    loaded: bool,
    stats: BTreeMap<String, f64>,
    user: BTreeMap<String, String>,
    publisher: BTreeMap<String, String>,
    version: f64,
}

/// `PEERPARTY_PLAYFAB_DIR` replaces the folder, for tests.
fn saved_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("PEERPARTY_PLAYFAB_DIR") {
        return Some(PathBuf::from(d).join(format!("{}.json", entity_id())));
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let game = std::env::current_exe().ok()?.file_stem()?.to_string_lossy().to_ascii_lowercase();
    Some(base.join("peerparty").join("playfab").join(game).join(format!("{}.json", entity_id())))
}

fn strings_of(j: &Json) -> BTreeMap<String, String> {
    match j {
        Json::Obj(pairs) => pairs.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect(),
        _ => BTreeMap::new(),
    }
}

fn with_saved<T>(f: impl FnOnce(&mut Saved) -> T) -> T {
    let mut st = STATE.lock().unwrap();
    let s = &mut st.saved;
    if !s.loaded {
        s.loaded = true;
        let path = saved_path();
        let text = path.as_ref().and_then(|p| std::fs::read(p).ok());
        let parsed = text.as_ref().and_then(|t| Json::parse(std::str::from_utf8(t).ok()?));
        if let (Some(p), Some(_), None) = (&path, &text, &parsed) {
            crate::saves::set_aside_unreadable(p, "playfab");
        }
        if let Some(j) = parsed {
            if let Json::Obj(pairs) = j.get("Statistics") {
                s.stats = pairs.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect();
            }
            s.user = strings_of(j.get("UserData"));
            s.publisher = strings_of(j.get("UserPublisherData"));
            s.version = j.get("DataVersion").as_f64().unwrap_or(0.0);
        }
    }
    f(s)
}

fn save(s: &Saved) {
    let Some(p) = saved_path() else { return };
    let map = |m: &BTreeMap<String, String>| Json::Obj(m.iter().map(|(k, v)| (k.clone(), Json::str(v.clone()))).collect());
    let j = Json::obj(vec![
        ("Statistics", Json::Obj(s.stats.iter().map(|(k, v)| (k.clone(), Json::Num(*v))).collect())),
        ("UserData", map(&s.user)),
        ("UserPublisherData", map(&s.publisher)),
        ("DataVersion", Json::Num(s.version)),
    ]);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if crate::saves::write_whole(&p, j.to_string().as_bytes()).is_err() {
        log!("playfab: couldn't save the player's data to {}", p.display());
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
/// Matchmaking runs one request at a time (searches share one event queue).
static MATCHING: Mutex<()> = Mutex::new(());

fn ok(data: Json) -> (u16, String) {
    let body = Json::obj(vec![
        ("code", Json::Num(200.0)),
        ("status", Json::str("OK")),
        ("data", data),
    ]);
    (200, body.to_string())
}

fn error(code: u16, name: &str, number: u32, message: &str) -> (u16, String) {
    let body = Json::obj(vec![
        ("code", Json::Num(code as f64)),
        ("status", Json::str("BadRequest")),
        ("error", Json::str(name)),
        ("errorCode", Json::Num(number as f64)),
        ("errorMessage", Json::str(message)),
    ]);
    (code, body.to_string())
}

fn entity_key(id: &str) -> Json {
    Json::obj(vec![
        ("Id", Json::str(id)),
        ("Type", Json::str(ENTITY_TYPE)),
        ("TypeString", Json::str(ENTITY_TYPE)),
    ])
}

fn entity_token() -> Json {
    Json::obj(vec![
        ("EntityToken", Json::str(format!("peerparty-{}", entity_id()))),
        ("TokenExpiration", Json::str("2099-12-31T00:00:00.000Z")),
        ("Entity", entity_key(&entity_id())),
    ])
}

/// Calls that buy, grant, redeem, trade or spend: purchases, receipts and refunds, coupons,
/// virtual currency, grants, trades, consuming, moving, revoking or changing items, opening
/// containers, and Economy v2's inventory changes, in the Client and Server APIs; and the
/// whole Admin API (title management, which PeerParty doesn't run). They are the store's
/// business, so they fail as unavailable, as they would offline, rather than getting the
/// empty success other unimplemented calls get. Names are matched without regard to case.
fn store_call(path: &str) -> bool {
    const VERBS: &[&str] = &[
        "purchaseitem", "startpurchase", "payforpurchase", "confirmpurchase", "getpurchase", "getpaymenttoken",
        "restoreiospurchases", "refundpurchase", "resolvepurchasedispute", "redeem", "unlockcontainer",
        "consume", "validate", "grant", "opentrade", "accepttrade", "canceltrade", "modifyitemuses", "moveitemto",
        "revokeinventoryitem", "updateuserinventoryitem",
    ];
    let path = path.to_ascii_lowercase();
    let Some((service, name)) = path.trim_start_matches('/').split_once('/') else { return false };
    match service {
        "client" | "server" => {
            // The legacy matchmaker's ticket, not a store item.
            name != "redeemmatchmakerticket" && (VERBS.iter().any(|p| name.starts_with(p)) || name.ends_with("virtualcurrency"))
        }
        "admin" => true,
        "inventory" => !name.starts_with("get"),
        _ => false,
    }
}

/// Answers one request: (HTTP status, JSON body).
pub fn handle(req: &Request) -> (u16, String) {
    let path = req.path.split('?').next().unwrap_or_default().to_string();
    if store_call(&path) {
        crate::log::throttled(&format!("playfab store {path}"), 3, |n| log!("playfab {path} n={n}: a store call, answered as unavailable"));
        return error(503, "ServiceUnavailable", 1123, "The store isn't reachable");
    }
    let body = Json::parse(&String::from_utf8_lossy(&req.body)).unwrap_or(Json::Null);
    if net::lan().is_none() {
        return error(503, "ServiceUnavailable", 1123, "PeerParty's LAN isn't running");
    }
    crate::log::throttled(&format!("playfab {path}"), 3, |n| log!("playfab {path} n={n}"));
    let now = iso(SystemTime::now());
    match path.as_str() {
        p if p.starts_with("/Client/LoginWith") => ok(Json::obj(vec![
            ("PlayFabId", Json::str(playfab_id())),
            ("SessionTicket", Json::str(format!("{}-peerparty", playfab_id()))),
            ("NewlyCreated", Json::Bool(false)),
            ("LastLoginTime", Json::str(now)),
            ("EntityToken", entity_token()),
            (
                "SettingsForUser",
                Json::obj(vec![
                    ("NeedsAttribution", Json::Bool(false)),
                    ("GatherDeviceInfo", Json::Bool(false)),
                    ("GatherFocusInfo", Json::Bool(false)),
                ]),
            ),
            (
                "TreatmentAssignment",
                Json::obj(vec![("Variants", Json::Arr(vec![])), ("Variables", Json::Arr(vec![]))]),
            ),
        ])),
        "/Authentication/GetEntityToken" => ok(entity_token()),
        "/Authentication/ValidateEntityToken" => ok(Json::obj(vec![("Entity", entity_key(&entity_id()))])),
        "/Client/GetUserData" => user_data(&body, false, &now),
        "/Client/GetUserPublisherData" => user_data(&body, true, &now),
        "/Client/UpdateUserData" => update_user_data(&body, false),
        "/Client/UpdateUserPublisherData" => update_user_data(&body, true),
        "/Client/GetTitleData" | "/Client/GetUserReadOnlyData" | "/Client/GetTitleNews"
        | "/Client/GetUserPublisherReadOnlyData" | "/Client/GetTitleInternalData" => ok(Json::obj(vec![
            ("Data", Json::Obj(vec![])),
            ("DataVersion", Json::Num(0.0)),
            ("News", Json::Arr(vec![])),
        ])),
        "/Client/GetPlayerProfile" => ok(Json::obj(vec![("PlayerProfile", player_profile())])),
        "/Client/GetAccountInfo" => ok(Json::obj(vec![(
            "AccountInfo",
            Json::obj(vec![
                ("PlayFabId", Json::str(playfab_id())),
                ("Created", Json::str("2020-01-01T00:00:00.000Z")),
                (
                    "TitleInfo",
                    Json::obj(vec![
                        ("DisplayName", Json::str(display_name())),
                        ("TitlePlayerAccount", entity_key(&entity_id())),
                    ]),
                ),
            ]),
        )])),
        "/Client/UpdateAvatarUrl" => {
            if let Some(url) = body.get("ImageUrl").as_str() {
                STATE.lock().unwrap().avatar_url = url.to_string();
            }
            ok(Json::Obj(vec![]))
        }
        "/Client/UpdateUserTitleDisplayName" => ok(Json::obj(vec![(
            "DisplayName",
            Json::str(body.get("DisplayName").as_str().unwrap_or_default()),
        )])),
        "/Client/ExecuteCloudScript" | "/CloudScript/ExecuteEntityCloudScript" => ok(Json::obj(vec![
            ("FunctionName", body.get("FunctionName").clone()),
            ("Revision", Json::Num(0.0)),
            ("FunctionResult", script_result(body.get("FunctionName").as_str().unwrap_or_default())),
            ("Logs", Json::Arr(vec![])),
            ("ExecutionTimeSeconds", Json::Num(0.0)),
            ("ProcessorTimeSeconds", Json::Num(0.0)),
            ("MemoryConsumedBytes", Json::Num(0.0)),
            ("APIRequestsIssued", Json::Num(0.0)),
            ("HttpRequestsIssued", Json::Num(0.0)),
        ])),
        "/CloudScript/ExecuteFunction" => ok(Json::obj(vec![
            ("FunctionName", body.get("FunctionName").clone()),
            ("FunctionResult", script_result(body.get("FunctionName").as_str().unwrap_or_default())),
            ("ExecutionTimeMilliseconds", Json::Num(0.0)),
        ])),
        "/Client/GetLeaderboard" | "/Client/GetFriendLeaderboard" => leaderboard(&body, true),
        "/Client/GetLeaderboardAroundPlayer" | "/Client/GetFriendLeaderboardAroundPlayer" => leaderboard(&body, false),
        "/Client/UpdatePlayerStatistics" => {
            with_saved(|s| {
                for st in body.get("Statistics").as_arr() {
                    if let (Some(name), Some(value)) = (st.get("StatisticName").as_str(), st.get("Value").as_f64()) {
                        s.stats.insert(name.to_string(), value);
                    }
                }
                save(s);
            });
            ok(Json::Obj(vec![]))
        }
        "/Client/GetPlayerStatistics" => {
            let want: Vec<&str> = body.get("StatisticNames").as_arr().iter().filter_map(|n| n.as_str()).collect();
            let stats: Vec<Json> = with_saved(|s| {
                s.stats
                    .iter()
                    .filter(|(k, _)| want.is_empty() || want.contains(&k.as_str()))
                    .map(|(k, v)| Json::obj(vec![("StatisticName", Json::str(k.clone())), ("Value", Json::Num(*v)), ("Version", Json::Num(0.0))]))
                    .collect()
            });
            ok(Json::obj(vec![("Statistics", Json::Arr(stats))]))
        }
        "/Client/GetFriendsList" => ok(Json::obj(vec![("Friends", friends())])),
        "/Client/GetPlayerCombinedInfo" => {
            let stats: Vec<Json> = with_saved(|s| {
                s.stats.iter().map(|(k, v)| Json::obj(vec![("StatisticName", Json::str(k.clone())), ("Value", Json::Num(*v)), ("Version", Json::Num(0.0))])).collect()
            });
            let data = with_saved(|s| data_json(&s.user, &now));
            ok(Json::obj(vec![
                ("PlayFabId", Json::str(playfab_id())),
                (
                    "InfoResultPayload",
                    Json::obj(vec![
                        ("PlayerProfile", player_profile()),
                        ("PlayerStatistics", Json::Arr(stats)),
                        ("UserData", data),
                        ("UserDataVersion", Json::Num(with_saved(|s| s.version))),
                        ("UserReadOnlyData", Json::Obj(vec![])),
                        ("TitleData", Json::Obj(vec![])),
                        ("UserInventory", Json::Arr(vec![])),
                        ("UserVirtualCurrency", Json::Obj(vec![])),
                        ("UserVirtualCurrencyRechargeTimes", Json::Obj(vec![])),
                        ("CharacterList", Json::Arr(vec![])),
                    ]),
                ),
            ]))
        }
        // No catalog, store, inventory or groups on the LAN: a title with none set up.
        "/Client/GetUserInventory" => ok(Json::obj(vec![
            ("Inventory", Json::Arr(vec![])),
            ("VirtualCurrency", Json::Obj(vec![])),
            ("VirtualCurrencyRechargeTimes", Json::Obj(vec![])),
        ])),
        "/Client/GetCatalogItems" => ok(Json::obj(vec![("Catalog", Json::Arr(vec![]))])),
        "/Client/GetStoreItems" => ok(Json::obj(vec![
            ("Store", Json::Arr(vec![])),
            ("StoreId", body.get("StoreId").clone()),
            ("CatalogVersion", body.get("CatalogVersion").clone()),
        ])),
        "/Client/GetCharacterInventory" => ok(Json::obj(vec![
            ("Inventory", Json::Arr(vec![])),
            ("VirtualCurrency", Json::Obj(vec![])),
            ("CharacterId", body.get("CharacterId").clone()),
        ])),
        "/Client/GetAllUsersCharacters" => ok(Json::obj(vec![("Characters", Json::Arr(vec![]))])),
        "/Inventory/GetInventoryItems" | "/Inventory/GetInventoryCollectionIds" => ok(Json::obj(vec![
            ("Items", Json::Arr(vec![])),
            ("CollectionIds", Json::Arr(vec![])),
            ("ETag", Json::str("1")),
        ])),
        "/Catalog/SearchItems" | "/Catalog/GetItems" => ok(Json::obj(vec![("Items", Json::Arr(vec![]))])),
        "/Group/ListMembership" | "/Group/ListMembershipOpportunities" => {
            ok(Json::obj(vec![("Groups", Json::Arr(vec![])), ("Invitations", Json::Arr(vec![])), ("Applications", Json::Arr(vec![]))]))
        }
        "/Client/WritePlayerEvent" | "/Client/WriteTitleEvent" => ok(Json::Obj(vec![])),
        "/Profile/GetProfile" => ok(Json::obj(vec![(
            "Profile",
            Json::obj(vec![
                ("Entity", entity_key(&entity_id())),
                ("DisplayName", Json::str(display_name())),
                ("Created", Json::str("2020-01-01T00:00:00.000Z")),
                ("Lineage", Json::obj(vec![("TitlePlayerAccountId", Json::str(entity_id())), ("MasterPlayerAccountId", Json::str(playfab_id()))])),
                ("VersionNumber", Json::Num(1.0)),
            ]),
        )])),
        "/Event/WriteTelemetryEvents" | "/Event/WriteEvents" => {
            ok(Json::obj(vec![("AssignedEventIds", Json::Arr(vec![]))]))
        }
        "/MultiplayerServer/ListQosServersForTitle" | "/MultiplayerServer/ListQosServers" => {
            ok(Json::obj(vec![("QosServers", Json::Arr(vec![])), ("PageSize", Json::Num(0.0))]))
        }
        // A reachability check (some games ask for the bare host).
        "/" => ok(Json::Obj(vec![])),
        // Every LAN player is a friend: friends' lobbies are all of them.
        "/Lobby/FindLobbies" | "/Lobby/FindFriendLobbies" => find_lobbies(&body),
        "/Lobby/GetLobby" => get_lobby(&body),
        // Lobby changes reach the library through the LAN, not PlayFab's PubSub.
        "/Lobby/SubscribeToLobbyResource" => ok(Json::obj(vec![("Topic", Json::str(format!(
            "peerparty/{}",
            body.get("ResourceId").as_str().unwrap_or_default()
        )))])),
        "/Lobby/UnsubscribeFromLobbyResource" => ok(Json::Obj(vec![])),
        "/Lobby/UpdateLobby" => update_lobby(&body),
        "/Lobby/CreateLobby" => create_lobby(&body),
        "/Lobby/JoinLobby" => {
            let (member, data) = member_of(&body);
            let connection = body.get("ConnectionString").as_str().unwrap_or_default();
            lobby_result(crate::pfmp::rest_join(connection, member, data).map(|id| Json::obj(vec![("LobbyId", Json::str(id))])))
        }
        "/Lobby/JoinArrangedLobby" => {
            let arrangement = body.get("ArrangementString").as_str().unwrap_or_default().to_string();
            let c = create_settings(&body);
            lobby_result(crate::pfmp::rest_join_arranged(&arrangement, c).map(|id| Json::obj(vec![("LobbyId", Json::str(id))])))
        }
        "/Lobby/DeleteLobby" => lobby_result(crate::pfmp::rest_delete(body.get("LobbyId").as_str().unwrap_or_default()).map(|()| Json::Obj(vec![]))),
        "/Lobby/RemoveMember" => lobby_result(
            crate::pfmp::rest_remove(
                body.get("LobbyId").as_str().unwrap_or_default(),
                body.get("MemberEntity").get("Id").as_str().unwrap_or_default(),
            )
            .map(|()| Json::Obj(vec![])),
        ),
        "/Lobby/InviteToLobby" => lobby_result(
            crate::pfmp::rest_invite(
                body.get("LobbyId").as_str().unwrap_or_default(),
                body.get("InviteeEntity").get("Id").as_str().unwrap_or_default(),
            )
            .map(|()| Json::Obj(vec![])),
        ),
        "/Lobby/LeaveLobby" => {
            crate::pfmp::rest_leave(body.get("LobbyId").as_str().unwrap_or_default());
            ok(Json::Obj(vec![]))
        }
        "/Match/CreateMatchmakingTicket" => create_ticket(&body),
        "/Match/GetMatchmakingTicket" => get_ticket(&body),
        "/Match/GetMatch" => get_match(&body),
        "/Match/CancelMatchmakingTicket" => {
            let id = body.get("TicketId").as_str().unwrap_or_default().to_string();
            cancel(|tid, _| tid == id, "Requested");
            ok(Json::Obj(vec![]))
        }
        "/Match/CancelAllMatchmakingTicketsForPlayer" => {
            let queue = body.get("QueueName").as_str().unwrap_or_default().to_string();
            cancel(|_, t| queue.is_empty() || t.queue == queue, "Requested");
            ok(Json::Obj(vec![]))
        }
        "/Match/ListMatchmakingTicketsForPlayer" => {
            let queue = body.get("QueueName").as_str().unwrap_or_default();
            let ids: Vec<Json> = STATE
                .lock()
                .unwrap()
                .tickets
                .iter()
                .filter(|(_, t)| t.queue == queue && t.canceled.is_none() && t.matched.is_none())
                .map(|(id, _)| Json::str(id.clone()))
                .collect();
            ok(Json::obj(vec![("TicketIds", Json::Arr(ids))]))
        }
        other => {
            crate::log::gap("playfab", &format!("{other} (no stand-in, answered with an empty success)"));
            ok(Json::Obj(vec![]))
        }
    }
}

fn player_profile() -> Json {
    let avatar = STATE.lock().unwrap().avatar_url.clone();
    Json::obj(vec![
        ("PlayerId", Json::str(playfab_id())),
        ("TitleId", Json::str("")),
        ("DisplayName", Json::str(display_name())),
        ("AvatarUrl", if avatar.is_empty() { Json::Null } else { Json::str(avatar) }),
        ("Origination", Json::str("Steam")),
        ("Created", Json::str("2020-01-01T00:00:00.000Z")),
    ])
}

// ---- matchmaking ----

fn new_ticket_id() -> String {
    let h = PeerId::random().hex();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn millis(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn create_ticket(body: &Json) -> (u16, String) {
    let attrs = body.get("Creator").get("Attributes");
    let data = match attrs.get("EscapedDataObject").as_str() {
        Some(s) => s.to_string(),
        None if !attrs.get("DataObject").is_null() => attrs.get("DataObject").to_string(),
        None => "{}".to_string(),
    };
    let queue = body.get("QueueName").as_str().unwrap_or_default();
    let give_up = body.get("GiveUpAfterSeconds").as_f64().unwrap_or(120.0);
    match open_ticket(queue, &entity_id(), &data, give_up) {
        Ok(id) => ok(Json::obj(vec![("TicketId", Json::str(id))])),
        Err(e) => error(400, "MatchmakingTicketCreationFailed", 2000, &e),
    }
}

/// A ticket's progress, for the PlayFab Multiplayer library's matchmaking (pfmp.rs).
pub(crate) enum Poll {
    Waiting,
    /// The match id and its members' (entity id, attributes), the same order everywhere.
    Matched(String, Vec<(String, String)>),
    /// Canceled: "Requested" or "Timeout".
    Ended(&'static str),
}

/// Puts a ticket on the LAN. `data` is its attributes, a JSON object.
pub(crate) fn open_ticket(queue: &str, entity: &str, data: &str, give_up: f64) -> Result<String, String> {
    let Some(lan) = net::lan() else {
        return Err("no LAN".into());
    };
    net::subscribe(Front::PlayFab);
    let data = if data.trim().is_empty() { "{}".to_string() } else { data.to_string() };
    let queue = queue.to_string();
    let id = new_ticket_id();
    let created = SystemTime::now();
    let mut lobby = Lobby::new(NS);
    lobby.id = id.clone();
    lobby.max_members = 1;
    lobby.host_migration = false;
    for (k, v) in [
        ("queue", Value::Str(queue.clone())),
        ("entity", Value::Str(entity.to_string())),
        ("data", Value::Str(data.clone())),
        ("created", Value::Int(millis(created))),
        ("match", Value::Str(String::new())),
    ] {
        lobby.attrs.push(Attr {
            key: k.into(),
            value: v,
            public: true,
        });
    }
    lan.create_lobby(lobby).map_err(|e| format!("{e:?}"))?;
    log!("playfab ticket {id} queue={queue} data={data}");
    STATE.lock().unwrap().tickets.insert(
        id.clone(),
        Ticket {
            queue,
            entity: entity.to_string(),
            data,
            created,
            started: Instant::now(),
            give_up,
            canceled: None,
            matched: None,
        },
    );
    Ok(id)
}

pub(crate) fn cancel_ticket(id: &str) {
    cancel(|tid, _| tid == id, "Requested");
}

/// Moves a ticket on (it may match) and says where it stands. Blocks for a LAN search.
pub(crate) fn poll_ticket(id: &str) -> Option<Poll> {
    let lan = net::lan()?;
    if !advance(lan, id) {
        return None;
    }
    let s = STATE.lock().unwrap();
    let t = s.tickets.get(id)?;
    Some(match (&t.matched, t.canceled) {
        (Some((m, _)), _) => Poll::Matched(m.clone(), s.matches.get(m).cloned().unwrap_or_default()),
        (None, Some(reason)) => Poll::Ended(reason),
        (None, None) => Poll::Waiting,
    })
}

fn cancel(which: impl Fn(&str, &Ticket) -> bool, reason: &'static str) {
    let lan = net::lan();
    let mut s = STATE.lock().unwrap();
    for (id, t) in s.tickets.iter_mut() {
        if t.canceled.is_none() && t.matched.is_none() && which(id, t) {
            t.canceled = Some(reason);
            if let Some(lan) = lan {
                let _ = lan.destroy(id);
            }
            log!("playfab ticket {id} canceled ({reason})");
        }
    }
}

/// Matched tickets leave the LAN once the other side has had time to adopt the match.
fn tidy(lan: &peerparty_core::Lan) {
    let s = STATE.lock().unwrap();
    for (id, t) in &s.tickets {
        if t.matched.as_ref().is_some_and(|(_, at)| at.elapsed() > MATCH_LINGER) {
            let _ = lan.destroy(id);
        }
    }
}

#[derive(Clone)]
struct View {
    id: String,
    entity: String,
    data: String,
    created: i64,
    matched: String,
}

// ---- player data, statistics, friends ----

fn data_json(m: &BTreeMap<String, String>, now: &str) -> Json {
    Json::Obj(
        m.iter()
            .map(|(k, v)| {
                (k.clone(), Json::obj(vec![("Value", Json::str(v.clone())), ("LastUpdated", Json::str(now)), ("Permission", Json::str("Private"))]))
            })
            .collect(),
    )
}

/// `GetUserData` / `GetUserPublisherData`: this player's, from disk; anyone else's is
/// empty.
fn user_data(body: &Json, publisher: bool, now: &str) -> (u16, String) {
    let other = body.get("PlayFabId").as_str().is_some_and(|id| !id.is_empty() && id != playfab_id());
    let keys: Vec<&str> = body.get("Keys").as_arr().iter().filter_map(|k| k.as_str()).collect();
    let (data, version) = with_saved(|s| {
        let m = if publisher { &s.publisher } else { &s.user };
        let picked: BTreeMap<String, String> = if other {
            BTreeMap::new()
        } else {
            m.iter().filter(|(k, _)| keys.is_empty() || keys.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        (data_json(&picked, now), s.version)
    });
    ok(Json::obj(vec![("Data", data), ("DataVersion", Json::Num(version))]))
}

fn update_user_data(body: &Json, publisher: bool) -> (u16, String) {
    let version = with_saved(|s| {
        let m = if publisher { &mut s.publisher } else { &mut s.user };
        for (k, v) in strings_of(body.get("Data")) {
            m.insert(k, v);
        }
        // A key set to null is removed too.
        if let Json::Obj(pairs) = body.get("Data") {
            for (k, v) in pairs {
                if matches!(v, Json::Null) {
                    m.remove(k);
                }
            }
        }
        for k in body.get("KeysToRemove").as_arr().iter().filter_map(|k| k.as_str()) {
            m.remove(k);
        }
        s.version += 1.0;
        save(s);
        s.version
    });
    ok(Json::obj(vec![("DataVersion", Json::Num(version))]))
}

/// A leaderboard of this player's own score (other players' scores live on PlayFab).
fn leaderboard(body: &Json, from_top: bool) -> (u16, String) {
    let name = body.get("StatisticName").as_str().unwrap_or_default().to_string();
    let start = body.get("StartPosition").as_f64().unwrap_or(0.0);
    let value = with_saved(|s| s.stats.get(&name).copied());
    let entries = match value {
        Some(v) if !from_top || start <= 0.0 => vec![Json::obj(vec![
            ("PlayFabId", Json::str(playfab_id())),
            ("DisplayName", Json::str(display_name())),
            ("StatValue", Json::Num(v)),
            ("Position", Json::Num(0.0)),
            ("Profile", player_profile()),
        ])],
        _ => Vec::new(),
    };
    ok(Json::obj(vec![("Leaderboard", Json::Arr(entries)), ("Version", Json::Num(0.0))]))
}

/// LAN players, as PlayFab friends.
fn friends() -> Json {
    let Some(lan) = net::lan() else { return Json::Arr(vec![]) };
    Json::Arr(
        lan.peers()
            .into_iter()
            .map(|(p, name)| {
                Json::obj(vec![
                    ("FriendPlayFabId", Json::str(playfab_of(p))),
                    ("Username", Json::str(name.clone())),
                    ("TitleDisplayName", Json::str(name.clone())),
                    (
                        "Profile",
                        Json::obj(vec![
                            ("PlayerId", Json::str(playfab_of(p))),
                            ("DisplayName", Json::str(name)),
                            ("TitleId", Json::Null),
                        ]),
                    ),
                    ("Tags", Json::Arr(vec![])),
                ])
            })
            .collect(),
    )
}

// ---- lobbies through the web API ----

/// The member a lobby call is for (`MemberEntity`, or the first of `Members`) and its
/// member data. This player when the call doesn't say.
fn member_of(body: &Json) -> ((String, String), Vec<(String, Option<String>)>) {
    let m = if matches!(body.get("MemberEntity"), Json::Null) { body.get("Members").as_arr().first().cloned().unwrap_or(Json::Null) } else { body.clone() };
    let e = m.get("MemberEntity");
    let id = e.get("Id").as_str().filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(entity_id);
    let ty = e.get("Type").as_str().filter(|s| !s.is_empty()).unwrap_or(ENTITY_TYPE).to_string();
    let data = changes(m.get("MemberData"), &Json::Null);
    ((id, ty), data)
}

fn create_settings(body: &Json) -> crate::pfmp::RestCreate {
    let (member, member_data) = member_of(body);
    crate::pfmp::RestCreate {
        member,
        max: body.get("MaxPlayers").as_f64().map_or(8, |n| n as u32),
        access: body.get("AccessPolicy").as_str().unwrap_or("Public").to_string(),
        migration: body.get("OwnerMigrationPolicy").as_str().unwrap_or("Automatic").to_string(),
        search: changes(body.get("SearchData"), &Json::Null),
        props: changes(body.get("LobbyData"), &Json::Null),
        member_data,
    }
}

fn create_lobby(body: &Json) -> (u16, String) {
    lobby_result(crate::pfmp::rest_create(create_settings(body)).map(|id| {
        Json::obj(vec![("LobbyId", Json::str(id.clone())), ("ConnectionString", Json::str(format!("{}{id}", crate::pfmp::CONNECT_PREFIX)))])
    }))
}

/// A web API lobby call's answer: its data, or PlayFab's error by name.
fn lobby_result(r: Result<Json, &'static str>) -> (u16, String) {
    match r {
        Ok(data) => ok(data),
        Err(name) => {
            let code = if name.ends_with("NotFound") { 404 } else { 400 };
            error(code, name, 0, name)
        }
    }
}

fn view(l: &Lobby) -> View {
    let text = |k: &str| l.attr(k).map(|a| a.value.text()).unwrap_or_default();
    View {
        id: l.id.clone(),
        entity: text("entity"),
        data: text("data"),
        created: text("created").parse().unwrap_or(0),
        matched: text("match"),
    }
}

/// Every ticket on the LAN in `queue`, ours included.
fn search(lan: &peerparty_core::Lan, queue: &str) -> Vec<View> {
    lan_lobbies(lan, NS)
        .iter()
        .filter(|l| l.attr("queue").is_some_and(|a| a.value.text() == queue))
        .map(view)
        .collect()
}

/// Every lobby on the LAN in namespace `ns`, ours included (waits for the answers).
fn lan_lobbies(lan: &peerparty_core::Lan, ns: &str) -> Vec<Lobby> {
    net::subscribe(Front::PlayFab);
    let mut q = Query::new(ns);
    q.max_results = 200;
    let id = lan.search(q);
    let deadline = Instant::now() + SEARCH_WAIT;
    while Instant::now() < deadline {
        // Other requests' searches and joins wait in the same queue (net::take_one).
        let mine = |e: &Event| matches!(e, Event::SearchDone { search, .. } if *search == id);
        if let Some(Event::SearchDone { results, .. }) = net::take_one(Front::PlayFab, mine, waited_for) {
            return results;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    log!("playfab search of {ns} timed out");
    Vec::new()
}

/// The address a player's PlayFab lobby advertises for game port `port`: a search
/// value of theirs that reads as an `ip:port` with that port (a dedicated server's
/// public address, which a game looks the server up by). Blocks for a LAN search.
pub(crate) fn lobby_address(owner: PeerId, port: u16) -> Option<String> {
    let lan = net::lan()?;
    let lobbies = lan_lobbies(lan, crate::pfmp::NS);
    let found = address_in(lobbies.iter().filter(|l| l.owner == owner), port);
    log!("playfab lobby address of {owner} for port {port}: {}", found.as_deref().unwrap_or("none"));
    found
}

fn address_in<'a>(lobbies: impl Iterator<Item = &'a Lobby>, port: u16) -> Option<String> {
    lobbies
        .flat_map(|l| l.attrs.iter())
        .filter(|a| a.key.starts_with("s:"))
        .map(|a| a.value.text())
        .find(|v| v.trim().parse::<std::net::SocketAddrV4>().is_ok_and(|a| a.port() == port))
        .map(|v| v.trim().to_string())
}

/// The events requests wait for in the PlayFab queue (here and pfmp.rs's REST joins).
pub(crate) fn waited_for(e: &Event) -> bool {
    matches!(e, Event::SearchDone { .. } | Event::JoinDone { .. })
}

// ---- lobbies ----

/// A JSON object of strings and a list of keys to delete, as (key, Some(value) / None).
fn changes(set: &Json, delete: &Json) -> Vec<(String, Option<String>)> {
    let mut v: Vec<(String, Option<String>)> = match set {
        Json::Obj(pairs) => pairs.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string))).collect(),
        _ => Vec::new(),
    };
    v.extend(delete.as_arr().iter().filter_map(|k| k.as_str()).map(|k| (k.to_string(), None)));
    v
}

fn owner_json(owner: &Option<(String, String)>) -> Json {
    match owner {
        Some((id, ty)) => Json::obj(vec![("Id", Json::str(id.clone())), ("Type", Json::str(ty.clone()))]),
        None => Json::Null,
    }
}

fn string_map(pairs: Vec<(String, String)>) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k, Json::Str(v))).collect())
}

/// `/Lobby/GetLobby`: a LAN lobby this copy is in, as PlayFab's `Lobby` model.
fn get_lobby(body: &Json) -> (u16, String) {
    let id = body.get("LobbyId").as_str().unwrap_or_default();
    let Some(l) = crate::pfmp::rest_lobby(id) else {
        log!("playfab GetLobby {id}: not a lobby this copy is in");
        return error(404, "LobbyNotFound", 0, "Lobby not found.");
    };
    let members: Vec<Json> = l
        .member_list
        .into_iter()
        .map(|(id, ty, data)| {
            Json::obj(vec![
                ("MemberEntity", Json::obj(vec![("Id", Json::str(id)), ("Type", Json::str(ty))])),
                ("MemberData", string_map(data)),
            ])
        })
        .collect();
    ok(Json::obj(vec![(
        "Lobby",
        Json::obj(vec![
            ("LobbyId", Json::str(l.id)),
            ("ConnectionString", Json::str(l.connection)),
            ("MaxPlayers", Json::Num(l.max_members as f64)),
            ("MembershipLock", Json::str(if l.locked { "Locked" } else { "Unlocked" })),
            ("AccessPolicy", Json::str("Public")),
            ("OwnerMigrationPolicy", Json::str("Automatic")),
            ("UseConnections", Json::Bool(true)),
            ("ChangeNumber", Json::Num(1.0)),
            ("Owner", owner_json(&l.owner)),
            ("LobbyData", string_map(l.props)),
            ("SearchData", string_map(l.search)),
            ("Members", Json::Arr(members)),
        ]),
    )]))
}

/// `/Lobby/UpdateLobby`: changes a LAN lobby of the PlayFab Multiplayer library's.
fn update_lobby(body: &Json) -> (u16, String) {
    let id = body.get("LobbyId").as_str().unwrap_or_default();
    let u = crate::pfmp::RestUpdate {
        max: body.get("MaxPlayers").as_f64().map(|n| n as u32),
        locked: body.get("MembershipLock").as_str().map(|l| l == "Locked"),
        search: changes(body.get("SearchData"), body.get("SearchDataToDelete")),
        props: changes(body.get("LobbyData"), body.get("LobbyDataToDelete")),
        member: changes(body.get("MemberData"), body.get("MemberDataToDelete")),
        owner: body.get("Owner").get("Id").as_str().map(str::to_string),
    };
    match crate::pfmp::rest_update(id, u) {
        Ok(()) => ok(Json::Obj(vec![])),
        Err(name) => {
            log!("playfab UpdateLobby {id}: {name}");
            error(400, name, 0, name)
        }
    }
}

/// `/Lobby/FindLobbies`: the PlayFab Multiplayer library's LAN lobbies (pfmp.rs), for
/// games that search through the REST API and join through the library.
fn find_lobbies(body: &Json) -> (u16, String) {
    let Some(lan) = net::lan() else {
        return error(503, "ServiceUnavailable", 1123, "no LAN");
    };
    let filter = body.get("Filter").as_str().unwrap_or_default();
    let sort = body.get("OrderBy").as_str().unwrap_or_default();
    let count = body.get("Pagination").get("PageSizeRequested").as_f64().map_or(50, |n| n as usize).clamp(1, 200);
    let found = crate::pfmp::summaries(lan_lobbies(lan, crate::pfmp::NS), lan.me(), filter, sort, count);
    log!("playfab FindLobbies filter={filter:?} sort={sort:?}: {} on the LAN", found.len());
    let lobbies: Vec<Json> = found
        .into_iter()
        .map(|l| {
            Json::obj(vec![
                ("ConnectionString", Json::str(l.connection)),
                ("CurrentPlayers", Json::Num(l.members as f64)),
                ("LobbyId", Json::str(l.id)),
                ("MaxPlayers", Json::Num(l.max_members as f64)),
                ("MembershipLock", Json::str(if l.locked { "Locked" } else { "Unlocked" })),
                ("Owner", owner_json(&l.owner)),
                ("SearchData", string_map(l.search)),
            ])
        })
        .collect();
    let total = lobbies.len();
    ok(Json::obj(vec![
        ("Lobbies", Json::Arr(lobbies)),
        ("Pagination", Json::obj(vec![("TotalMatchedLobbyCount", Json::Num(total as f64))])),
    ]))
}

fn get_ticket(body: &Json) -> (u16, String) {
    let Some(lan) = net::lan() else {
        return error(503, "ServiceUnavailable", 1123, "no LAN");
    };
    let id = body.get("TicketId").as_str().unwrap_or_default().to_string();
    if !advance(lan, &id) {
        return error(400, "MatchmakingTicketNotFound", 2053, "Ticket not found.");
    }
    let s = STATE.lock().unwrap();
    ok(ticket_json(&id, &s.tickets[&id]))
}

/// Matches a waiting ticket if its pair is on the LAN, or cancels it once it has waited
/// too long. False if there's no such ticket.
fn advance(lan: &peerparty_core::Lan, id: &str) -> bool {
    let _one = MATCHING.lock().unwrap();
    tidy(lan);
    let id = id.to_string();
    let (queue, entity, data, created, expired) = {
        let s = STATE.lock().unwrap();
        let Some(t) = s.tickets.get(&id) else {
            return false;
        };
        if t.canceled.is_some() || t.matched.is_some() {
            return true;
        }
        (
            t.queue.clone(),
            t.entity.clone(),
            t.data.clone(),
            t.created,
            t.started.elapsed().as_secs_f64() > t.give_up,
        )
    };
    if expired {
        cancel(|tid, _| tid == id, "Timeout");
    } else {
        let all = search(lan, &queue);
        let me = View {
            id: id.clone(),
            entity,
            data,
            created: millis(created),
            matched: String::new(),
        };
        if let Some((match_id, members)) = find_match(&me, &all) {
            log!("playfab ticket {id} matched: {match_id}");
            if let Some(mut l) = lan.lobby(&id) {
                if let Some(a) = l.attrs.iter_mut().find(|a| a.key == "match") {
                    a.value = Value::Str(match_id.clone());
                }
                let _ = lan.update(&id, Some(&l), &[], &[]);
            }
            let mut s = STATE.lock().unwrap();
            s.matches.insert(
                match_id.clone(),
                members.into_iter().map(|v| (v.entity, v.data)).collect(),
            );
            if let Some(t) = s.tickets.get_mut(&id) {
                t.matched = Some((match_id, Instant::now()));
            }
        }
    }
    true
}

/// The match `me` belongs to, from every ticket in its queue: one another ticket
/// already named (it noticed first), else the mutual-oldest compatible pair.
fn find_match(me: &View, all: &[View]) -> Option<(String, Vec<View>)> {
    let others: Vec<&View> = all.iter().filter(|v| v.id != me.id).collect();
    if let Some(named) = others.iter().find(|v| names_me(v, me)) {
        let mut members = vec![me.clone(), (*named).clone()];
        members.sort_by(|a, b| a.id.cmp(&b.id));
        return Some((named.matched.clone(), members));
    }
    let mut open: Vec<&View> = others.into_iter().filter(|v| v.matched.is_empty()).collect();
    open.push(me);
    let oldest_for = |x: &View| -> Option<View> {
        open.iter()
            .filter(|y| y.id != x.id && compatible(&x.data, &y.data))
            .min_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)))
            .map(|v| (*v).clone())
    };
    let pick = oldest_for(me)?;
    if oldest_for(&pick)?.id != me.id {
        return None;
    }
    let mut ids = [me.id.clone(), pick.id.clone()];
    ids.sort();
    let mut members = vec![me.clone(), pick];
    members.sort_by(|a, b| a.id.cmp(&b.id));
    Some((ids.join("+"), members))
}

/// Whether `named` claims a match with `me` that this side would have made too: its
/// "match" is exactly the two ids, sorted and joined by `+`, and the two tickets are
/// compatible. Any copy can write its own ticket's "match", so a claim naming other
/// tickets as well, or pairing tickets the rules keep apart (a different password, two
/// hosts), is ignored and `me` goes on matching as usual.
fn names_me(named: &View, me: &View) -> bool {
    let mut ids = [me.id.as_str(), named.id.as_str()];
    ids.sort();
    named.id != me.id && named.matched == ids.join("+") && compatible(&me.data, &named.data)
}

/// Keys that describe a player rather than what they're looking for.
const NOT_CRITERIA: &[&str] = &[
    "NAME", "LOBBYID", "REGIONS", "ISHOST", "ISJOINER", "ROLE", "ROLES", "FACTION", "PLAT",
    "PCNT", "LEVEL", "UPGRADE", "SEED", "MAPS", "INVADER",
];

fn is_host(d: &Json) -> bool {
    d.get("ISHOST").as_f64().unwrap_or(0.0) > 0.0
        && !d.get("LOBBYID").is_null()
        && d.get("ISJOINER").as_f64().unwrap_or(0.0) <= 0.0
}

fn strings(v: &Json) -> Vec<&str> {
    v.as_arr().iter().filter_map(|x| x.as_str()).collect()
}

/// Whether two tickets' attributes can be matched. PlayFab's rules are per title and
/// only on its servers; this covers what games put in tickets:
/// - a host (ISHOST with a LOBBYID) pairs with a joiner whose ROLE is one of the host's
///   open ROLES; two non-hosts pair when their ROLES overlap; two hosts never pair
/// - platform lists (PLAT) must overlap when both have one
/// - every other text attribute both have (a password, a mode) must be equal; a game
///   may mark a joiner's copy of a value with a trailing `j`, so that's ignored
/// Numbers (levels, counts) aren't compared: on a LAN anyone may play anyone.
pub(crate) fn compatible(a: &str, b: &str) -> bool {
    let (Some(a), Some(b)) = (Json::parse(a), Json::parse(b)) else {
        return false;
    };
    let (ha, hb) = (is_host(&a), is_host(&b));
    let roles_ok = match (ha, hb) {
        (true, true) => false,
        (true, false) | (false, true) => {
            let (host, joiner) = if ha { (&a, &b) } else { (&b, &a) };
            let open = strings(host.get("ROLES"));
            match joiner.get("ROLE").as_str() {
                Some(role) => open.is_empty() || open.contains(&role),
                None => {
                    let want = strings(joiner.get("ROLES"));
                    open.is_empty() || want.is_empty() || want.iter().any(|r| open.contains(r))
                }
            }
        }
        (false, false) => {
            let (ra, rb) = (strings(a.get("ROLES")), strings(b.get("ROLES")));
            ra.is_empty() || rb.is_empty() || ra.iter().any(|r| rb.contains(r))
        }
    };
    if !roles_ok {
        return false;
    }
    let (pa, pb) = (strings(a.get("PLAT")), strings(b.get("PLAT")));
    if !pa.is_empty() && !pb.is_empty() && !pa.iter().any(|p| pb.contains(p)) {
        return false;
    }
    let Json::Obj(pairs) = &a else { return true };
    pairs.iter().all(|(k, va)| {
        if NOT_CRITERIA.contains(&k.as_str()) {
            return true;
        }
        match (va.as_str(), b.get(k).as_str()) {
            (Some(x), Some(y)) => x.trim_end_matches('j') == y.trim_end_matches('j'),
            _ => true,
        }
    })
}

fn ticket_json(id: &str, t: &Ticket) -> Json {
    let status = if t.matched.is_some() {
        "Matched"
    } else if t.canceled.is_some() {
        "Canceled"
    } else {
        "WaitingForMatch"
    };
    let mut pairs = vec![
        ("TicketId", Json::str(id)),
        ("QueueName", Json::str(t.queue.clone())),
        ("Status", Json::str(status)),
        ("Created", Json::str(iso(t.created))),
        ("GiveUpAfterSeconds", Json::Num(t.give_up)),
        ("Creator", member_json(&t.entity, &t.data, true)),
        ("Members", Json::Arr(vec![member_json(&t.entity, &t.data, true)])),
    ];
    if let Some((m, _)) = &t.matched {
        pairs.push(("MatchId", Json::str(m.clone())));
    }
    if let Some(r) = t.canceled {
        pairs.push(("CancellationReasonString", Json::str(r)));
    }
    Json::obj(pairs)
}

fn member_json(entity: &str, data: &str, escaped: bool) -> Json {
    let attrs = if escaped {
        Json::obj(vec![("EscapedDataObject", Json::str(data))])
    } else {
        Json::obj(vec![("DataObject", Json::parse(data).unwrap_or(Json::Obj(vec![])))])
    };
    Json::obj(vec![
        ("Entity", Json::obj(vec![("Id", Json::str(entity)), ("Type", Json::str(ENTITY_TYPE))])),
        ("Attributes", attrs),
    ])
}

fn get_match(body: &Json) -> (u16, String) {
    let id = body.get("MatchId").as_str().unwrap_or_default();
    let escaped = matches!(body.get("EscapeObject"), Json::Bool(true));
    let with_attrs = !matches!(body.get("ReturnMemberAttributes"), Json::Bool(false));
    let s = STATE.lock().unwrap();
    let Some(members) = s.matches.get(id) else {
        return error(400, "MatchNotFound", 2056, "Match not found.");
    };
    let members: Vec<Json> = members
        .iter()
        .map(|(e, d)| {
            if with_attrs {
                member_json(e, d, escaped)
            } else {
                Json::obj(vec![("Entity", Json::obj(vec![("Id", Json::str(e.clone())), ("Type", Json::str(ENTITY_TYPE))]))])
            }
        })
        .collect();
    ok(Json::obj(vec![
        ("MatchId", Json::str(id)),
        ("Members", Json::Arr(members)),
        ("RegionPreferences", Json::Arr(vec![Json::str("LAN")])),
    ]))
}

/// `2026-10-30T09:15:00.000Z`.
pub(crate) fn iso(t: SystemTime) -> String {
    let ms = millis(t);
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        ms.rem_euclid(1000)
    )
}

/// What a game is told when it asks a CloudScript function to delete the player's
/// account: in English, as games show a script's text as it comes.
pub const NOT_NEEDED: &str = "Not needed while using PeerParty";

/// A CloudScript function's result: none (a title with no scripts), except for one
/// that deletes the player's account, which deletes nothing and says it isn't needed.
fn script_result(name: &str) -> Json {
    if deletes_account(name) {
        log!("playfab CloudScript {name}: nothing deleted, answered \"{NOT_NEEDED}\"");
        Json::str(NOT_NEEDED)
    } else {
        Json::Null
    }
}

fn deletes_account(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("delete") && n.contains("account")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lobby_advertises_the_address_for_its_game_port() {
        let mut l = Lobby::new(crate::pfmp::NS);
        let s = |k: &str, v: &str| Attr { key: format!("s:{k}"), value: Value::Str(v.into()), public: true };
        l.attrs = vec![s("string_key5", "My World"), s("string_key9", "1.2.3.4:99"), s("string_key10", "203.0.113.5:2456")];
        assert_eq!(address_in([&l].into_iter(), 2456).as_deref(), Some("203.0.113.5:2456"));
        assert_eq!(address_in([&l].into_iter(), 2457), None);
    }

    #[test]
    fn an_account_deleting_script_deletes_nothing_and_says_so() {
        // The result is a plain string, which a game shows as the reason.
        assert!(matches!(script_result("deleteMyAccount"), Json::Str(s) if s == NOT_NEEDED));
        assert!(deletes_account("DeleteAccount") && deletes_account("delete_user_account"));
        assert!(!deletes_account("deletePlayerSave") && !deletes_account("reportUser"));
        assert!(matches!(script_result("reportUser"), Json::Null));
    }

    #[test]
    fn store_calls_fail_as_unavailable() {
        for path in [
            "/Client/PurchaseItem",
            "/Client/StartPurchase",
            "/Client/PayForPurchase",
            "/Client/ConfirmPurchase",
            "/Client/AddUserVirtualCurrency",
            "/Client/SubtractUserVirtualCurrency",
            "/Client/RedeemCoupon",
            "/Client/UnlockContainerInstance",
            "/Client/ConsumeItem",
            "/Client/ConsumeXboxEntitlements",
            "/Client/ValidateWindowsStoreReceipt",
            "/Client/RestoreIOSPurchases",
            "/Client/AcceptTrade",
            "/Client/OpenTrade",
            "/Server/GrantItemsToUser",
            "/Server/GrantItemsToCharacter",
            "/Server/AddUserVirtualCurrency",
            "/Server/ConsumeItem",
            "/Server/UnlockContainerItem",
            "/Server/RevokeInventoryItems",
            "/Server/ModifyItemUses",
            "/Server/MoveItemToCharacterFromUser",
            "/Admin/GrantItemsToUsers",
            "/Admin/AddUserVirtualCurrency",
            "/Admin/RefundPurchase",
            "/Admin/UpdateCatalogItems",
            "/Admin/AddVirtualCurrencyTypes",
            "/Client/RedeemAppleAppStoreReceipt",
            "/client/startpurchase",
            "/Inventory/PurchaseInventoryItems",
            "/Inventory/AddInventoryItems",
            "/Inventory/RedeemSteamInventoryItems",
            "/Inventory/ExecuteInventoryOperations",
        ] {
            assert!(store_call(path), "{path}");
            let req = Request { method: "POST".into(), path: format!("{path}?sdk=x"), headers: vec![], body: b"{}".to_vec() };
            let (status, body) = handle(&req);
            assert_eq!(status, 503, "{path}");
            assert!(body.contains("ServiceUnavailable") && !body.contains("\"data\""), "{path}: {body}");
        }
        for path in [
            "/Client/GetUserInventory",
            "/Client/GetCatalogItems",
            "/Client/GetStoreItems",
            "/Inventory/GetInventoryItems",
            "/Authentication/ValidateEntityToken",
            "/Client/LoginWithCustomID",
            "/Client/GetPlayerProfile",
            "/Server/GetUserInventory",
            "/Server/RevokeBans",
            "/Server/RedeemMatchmakerTicket",
            "/Client/UpdateUserData",
        ] {
            assert!(!store_call(path), "{path}");
        }
    }

    #[test]
    fn update_lobby_reads_sets_and_deletes() {
        let body = Json::parse(r#"{"SearchData":{"string_key2":"False"},"SearchDataToDelete":["string_key5"]}"#).unwrap();
        assert_eq!(
            changes(body.get("SearchData"), body.get("SearchDataToDelete")),
            vec![("string_key2".to_string(), Some("False".to_string())), ("string_key5".to_string(), None)]
        );
        assert!(changes(body.get("LobbyData"), body.get("LobbyDataToDelete")).is_empty());
    }

    fn v(id: &str, created: i64, data: &str) -> View {
        View {
            id: id.into(),
            entity: format!("E{id}"),
            data: data.into(),
            created,
            matched: String::new(),
        }
    }

    const HOST: &str = r#"{"ISHOST":1,"LOBBYID":"inv|PP1-x","ROLES":["ALLY"],"PASS":"AAAAA","HOPPER":"coop","PLAT":["pc"]}"#;
    const JOINER: &str = r#"{"FACTION":1,"ROLE":"ALLY","ISJOINER":1,"ROLES":["ALLY"],"PASS":"AAAAA","HOPPER":"coopj","PLAT":["pc","xbox"]}"#;
    const WRONG_PASS: &str = r#"{"FACTION":1,"ROLE":"ALLY","ISJOINER":1,"ROLES":["ALLY"],"PASS":"BBBBB","HOPPER":"coopj","PLAT":["pc"]}"#;
    const INVADER: &str = r#"{"FACTION":2,"ROLE":"INVADER","ISJOINER":1,"ROLES":["INVADER"],"PASS":"AAAAA","PLAT":["pc"]}"#;

    #[test]
    fn compatible_follows_roles_passwords_and_platforms() {
        assert!(compatible(HOST, JOINER));
        assert!(compatible(JOINER, HOST));
        assert!(!compatible(HOST, WRONG_PASS));
        assert!(!compatible(HOST, INVADER));
        assert!(!compatible(HOST, HOST));
        let console = JOINER.replace(r#"["pc","xbox"]"#, r#"["xbox"]"#);
        assert!(!compatible(HOST, &console));
    }

    #[test]
    fn both_sides_pick_the_same_pair() {
        let host = v("a", 1, HOST);
        let joiner = v("b", 2, JOINER);
        let late = v("c", 3, JOINER);
        let all = vec![host.clone(), joiner.clone(), late.clone()];
        let (m1, members) = find_match(&host, &all).expect("host matched");
        let (m2, _) = find_match(&joiner, &all).expect("joiner matched");
        assert_eq!(m1, "a+b");
        assert_eq!(m1, m2);
        assert_eq!(members.len(), 2);
        assert!(find_match(&late, &all).is_none(), "the host is taken");
        // The joiner adopts a match the host already marked.
        let mut marked = host.clone();
        marked.matched = "a+b".into();
        let (m3, members) = find_match(&joiner, &[marked, joiner.clone()]).unwrap();
        assert_eq!(m3, "a+b");
        assert_eq!(members.iter().filter(|m| m.id == "a").count(), 1);
    }

    /// A ticket that names ours is adopted only when the claim is one this side would
    /// make: a stranger can't pull a ticket into a match its rules keep it out of.
    #[test]
    fn a_named_match_must_be_one_we_would_make() {
        let joiner = v("b", 2, JOINER);
        let claim = |id: &str, data: &str, matched: &str| {
            let mut c = v(id, 1, data);
            c.matched = matched.into();
            c
        };
        let honest = claim("a", HOST, "a+b");
        assert_eq!(find_match(&joiner, &[honest.clone(), joiner.clone()]).map(|m| m.0), Some("a+b".into()));
        // A different password, a different role, or a second host.
        for data in [WRONG_PASS, INVADER] {
            let bad = claim("a", &data.replace("\"ISJOINER\":1", "\"ISHOST\":1,\"LOBBYID\":\"x\""), "a+b");
            assert!(find_match(&joiner, &[bad, joiner.clone()]).is_none(), "{data}");
        }
        let host = v("h", 1, HOST);
        assert!(find_match(&host, &[claim("a", HOST, "a+h"), host.clone()]).is_none(), "two hosts");
        // Not exactly the two ids, sorted: other tickets named too, the order swapped,
        // ours named twice, or only ours.
        for matched in ["a+b+c", "b+a", "a+b+b", "b", "a+b+", "+a+b", "a++b"] {
            let bad = claim("a", HOST, matched);
            assert!(find_match(&joiner, &[bad, joiner.clone()]).is_none(), "{matched}");
        }
        // A refused claim doesn't stop the usual matching: an honest open host still
        // pairs with us.
        let squatter = claim("0", WRONG_PASS, "0+b");
        let open = v("c", 3, HOST);
        let (m, members) = find_match(&joiner, &[squatter, open, joiner.clone()]).expect("matched");
        assert_eq!(m, "b+c");
        assert_eq!(members.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["b", "c"]);
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso(UNIX_EPOCH + Duration::from_millis(1_793_352_900_123)),
            "2026-10-30T09:35:00.123Z"
        );
    }
}
