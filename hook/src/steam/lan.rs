//! Steam on the LAN: other copies of the game on the LAN are Steam
//! friends, online and playing this game, with their rich presence. A Steam invite
//! (`ActivateGameOverlayInviteDialogConnectString`, `InviteUserToGame`) goes to them,
//! and an invite from one is accepted at once: the game gets Steam's own
//! `GameRichPresenceJoinRequested_t` with the connect string, as if its player had
//! clicked "Join" in the overlay.
//!
//! Steam Input (and the older ISteamController) action reads come back at rest while
//! PeerParty's overlay shows, so a game reading its controller through Steam doesn't
//! move behind it. Steam reads the pad in its own process, so padblock.rs
//! can't hold it back there; the game's `GetDigitalActionData` / `GetAnalogActionData`
//! calls are where it can be.
//!
//! The real ISteamFriends keeps answering for real friends. The flat
//! `SteamAPI_ISteamFriends_*` exports are swapped (by name, see swap.rs), which is how
//! Steamworks.NET, Facepunch and other wrappers call Steam; native games calling the
//! C++ interface get the same functions through a stand-in object (steamobj.rs).
//!
//! SteamIDs: the first copy of the game on a PC is shown a SteamID of its own for this
//! PC (`own_id_on`), unless Valve's Steam client runs the game or the "Steam player ID"
//! mode keeps the game's; steam_api's SteamID is mapped to it both ways (steamown.rs)
//! and saves keyed by SteamID start from a copy of steam_api's (saves.rs). A second
//! copy on the same PC gets a LAN one. Each copy tells the others its SteamID in a
//! `peerparty:steamid` presence key, which the game never sees. Copies that keep one
//! shared SteamID are told apart on the network (steamids.rs).

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, c_char};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use peerparty_core::{Event, PeerId};

use crate::log;
use crate::net::{self, Front};
use crate::sys::pe::Export;
use crate::steam::obj::{call_real, unwrap};
use crate::sys::vtable::vmethod;

const GAME_RICH_PRESENCE_JOIN_REQUESTED: i32 = 337;
const GAME_LOBBY_JOIN_REQUESTED: i32 = 333;
const GAME_OVERLAY_ACTIVATED: i32 = 331;
const LOBBY_INVITE: i32 = 503;
const FRIEND_RICH_PRESENCE_UPDATE: i32 = 336;
const PERSONA_STATE_CHANGE: i32 = 304;
const GET_AUTH_SESSION_TICKET_RESPONSE: i32 = 163;
const GET_TICKET_FOR_WEB_API_RESPONSE: i32 = 168;
const RESULT_OK: u32 = 1;

const FRIEND_FLAG_IMMEDIATE: i32 = 0x04;
const PERSONA_ONLINE: i32 = 1;
const RELATIONSHIP_FRIEND: i32 = 3;
// EPersonaChange
const CHANGE_NAME: i32 = 0x01;
const CHANGE_STATUS: i32 = 0x02;
const CHANGE_COME_ONLINE: i32 = 0x04;
const CHANGE_GONE_OFFLINE: i32 = 0x08;
const CHANGE_GAME_PLAYED: i32 = 0x10;
const CHANGE_RICH_PRESENCE: i32 = 0x1000;

static ON: AtomicBool = AtomicBool::new(false);
/// Written when steam_api loads, read on every swapped call: a read lock, so calls from
/// several game threads never wait on each other.
static REAL: LazyLock<RwLock<HashMap<String, usize>>> = LazyLock::new(RwLock::default);
static MY_PRESENCE: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
/// What each LAN player is shown as, and whether we clash (steamids.rs).
static BOOK: LazyLock<Mutex<crate::steam::ids::Book>> = LazyLock::new(Mutex::default);
static STRINGS: LazyLock<Mutex<HashMap<String, CString>>> = LazyLock::new(Mutex::default);
/// Our SteamID once settled (0 until then).
static ME: AtomicU64 = AtomicU64::new(0);

/// Presence keys PeerParty keeps for itself; the game never sees them.
const KEY_PREFIX: &str = "peerparty:";
const KEY_STEAM_LOBBY: &str = "peerparty:steamlobby";

#[repr(C)]
struct FriendGameInfo {
    game_id: u64,
    game_ip: u32,
    game_port: u16,
    query_port: u16,
    lobby: u64,
}

#[repr(C)]
struct JoinRequested {
    friend: u64,
    connect: [u8; 256],
}

#[repr(C)]
struct RichPresenceUpdate {
    friend: u64,
    app_id: u32,
}

#[repr(C)]
struct PersonaStateChange {
    steam_id: u64,
    change_flags: i32,
}

#[repr(C)]
struct LobbyJoinRequested {
    lobby: u64,
    friend: u64,
}

#[repr(C)]
struct TicketResponse {
    ticket: u32,
    result: u32,
}

#[repr(C)]
struct WebTicketResponse {
    ticket: u32,
    result: u32,
    len: i32,
    data: [u8; 2560],
}

#[repr(C)]
struct LobbyInvite {
    user: u64,
    lobby: u64,
    game: u64,
}

pub fn attach(base: usize, module_name: &str, exports: &[Export]) {
    {
        let mut real = REAL.write().unwrap();
        for e in exports {
            if e.name.starts_with("SteamAPI_ISteamFriends_")
                || STEAM_INPUT_READS.contains(&e.name.as_str())
                || e.name == "SteamAPI_ISteamUser_GetAuthSessionTicket"
                || e.name == "SteamAPI_ISteamUser_GetAuthTicketForWebApi"
                || e.name == "SteamAPI_ISteamUser_GetSteamID"
                || e.name == "SteamAPI_ISteamUser_RequestEncryptedAppTicket"
                || e.name == "SteamAPI_ISteamUser_GetEncryptedAppTicket"
            {
                real.insert(e.name.clone(), e.addr as usize);
            }
        }
    }
    crate::steam::obj::note_flat(exports);
    crate::steam::own::note(exports);
    crate::sys::swap::register(base, module_name, resolve);
    net::subscribe(Front::Steam);
    ON.store(true, Ordering::Relaxed);
    log!("steam-lan on for {module_name}: LAN peers are friends, invites go over the LAN");
}

/// Whether PeerParty's Steam is on in this game (steam_api loaded, the layer on).
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Whether a LAN invite with a Steam connect string reaches this game through Steam.
pub fn takes_joins() -> bool {
    ON.load(Ordering::Relaxed) && crate::steam::delivers(GAME_RICH_PRESENCE_JOIN_REQUESTED)
}

/// Whether a player's presence says they're in a Steam lobby or give a `connect` to join
/// them by: a friend joins them that way, as Steam's friends list does, before their
/// game server (gs.rs `join_server`).
pub fn joined_by_lobby_or_connect(presence: &[(String, String)]) -> bool {
    presence.iter().any(|(k, v)| (k == KEY_STEAM_LOBBY || k == "connect") && !v.is_empty())
}

/// Our rich presence `connect`: where a friend joining us should go.
pub fn my_connect() -> String {
    MY_PRESENCE
        .lock()
        .unwrap()
        .get("connect")
        .cloned()
        .unwrap_or_default()
}

/// Steam Input's and ISteamController's action reads (flat API), held at rest while the
/// overlay shows.
const STEAM_INPUT_READS: [&str; 4] = [
    "SteamAPI_ISteamInput_GetDigitalActionData",
    "SteamAPI_ISteamInput_GetAnalogActionData",
    "SteamAPI_ISteamController_GetDigitalActionData",
    "SteamAPI_ISteamController_GetAnalogActionData",
];

fn resolve(name: &str) -> Option<usize> {
    let f: usize = match name {
        "SteamAPI_ISteamInput_GetDigitalActionData" => input_digital as *const () as usize,
        "SteamAPI_ISteamInput_GetAnalogActionData" => input_analog as *const () as usize,
        "SteamAPI_ISteamController_GetDigitalActionData" => controller_digital as *const () as usize,
        "SteamAPI_ISteamController_GetAnalogActionData" => controller_analog as *const () as usize,
        "SteamAPI_ISteamFriends_GetFriendCount" => get_friend_count as *const () as usize,
        "SteamAPI_ISteamFriends_GetFriendByIndex" => get_friend_by_index as *const () as usize,
        "SteamAPI_ISteamFriends_GetFriendPersonaName" => {
            get_friend_persona_name as *const () as usize
        }
        "SteamAPI_ISteamFriends_GetFriendPersonaState" => {
            get_friend_persona_state as *const () as usize
        }
        "SteamAPI_ISteamFriends_GetFriendRelationship" => {
            get_friend_relationship as *const () as usize
        }
        "SteamAPI_ISteamFriends_HasFriend" => has_friend as *const () as usize,
        "SteamAPI_ISteamFriends_GetFriendGamePlayed" => {
            get_friend_game_played as *const () as usize
        }
        "SteamAPI_ISteamFriends_GetFriendRichPresence" => {
            get_friend_rich_presence as *const () as usize
        }
        "SteamAPI_ISteamFriends_GetFriendRichPresenceKeyCount" => {
            rich_presence_key_count as *const () as usize
        }
        "SteamAPI_ISteamFriends_GetFriendRichPresenceKeyByIndex" => {
            rich_presence_key_by_index as *const () as usize
        }
        "SteamAPI_ISteamFriends_SetRichPresence" => set_rich_presence as *const () as usize,
        "SteamAPI_ISteamFriends_ClearRichPresence" => clear_rich_presence as *const () as usize,
        "SteamAPI_ISteamFriends_ActivateGameOverlayInviteDialogConnectString" => {
            invite_dialog as *const () as usize
        }
        "SteamAPI_ISteamFriends_InviteUserToGame" => invite_user_to_game as *const () as usize,
        "SteamAPI_ISteamFriends_ActivateGameOverlay" => activate_overlay as *const () as usize,
        "SteamAPI_ISteamFriends_ActivateGameOverlayToUser" => activate_overlay_to_user as *const () as usize,
        "SteamAPI_ISteamFriends_RequestUserInformation" => {
            request_user_information as *const () as usize
        }
        "SteamAPI_ISteamFriends_GetSmallFriendAvatar" => avatar_small as *const () as usize,
        "SteamAPI_ISteamFriends_GetMediumFriendAvatar" => avatar_medium as *const () as usize,
        "SteamAPI_ISteamFriends_GetLargeFriendAvatar" => avatar_large as *const () as usize,
        "SteamAPI_ISteamFriends_GetPlayerNickname" => get_player_nickname as *const () as usize,
        "SteamAPI_ISteamFriends_GetFriendSteamLevel" => {
            get_friend_steam_level as *const () as usize
        }
        "SteamAPI_ISteamFriends_GetPersonaName" => get_persona_name as *const () as usize,
        "SteamAPI_ISteamUser_GetAuthSessionTicket" => {
            get_auth_session_ticket as *const () as usize
        }
        "SteamAPI_ISteamUser_GetAuthTicketForWebApi" => {
            get_auth_ticket_for_web_api as *const () as usize
        }
        "SteamAPI_ISteamUser_BLoggedOn" => logged_on as *const () as usize,
        "SteamAPI_ISteamUser_RequestEncryptedAppTicket" => {
            request_encrypted_app_ticket as *const () as usize
        }
        "SteamAPI_ISteamUser_GetEncryptedAppTicket" => get_encrypted_app_ticket as *const () as usize,
        "SteamAPI_ISteamFriends_ActivateGameOverlayInviteDialog" => {
            invite_dialog_lobby as *const () as usize
        }
        _ => return crate::steam::own::resolve(name).or_else(|| crate::steam::obj::flat_swap(name)),
    };
    log::throttled(&format!("steam-lan resolve {name}"), 1, |_| {
        log!("steam-lan {name}: lan")
    });
    Some(f)
}

// ---- ids ----

/// A LAN SteamID for a copy without a real one (see steamids.rs).
fn lan_steam_id(peer: PeerId) -> u64 {
    crate::steam::ids::lan_steam_id(peer)
}

/// The SteamID book, worked out again when presence may have changed.
fn book() -> std::sync::MutexGuard<'static, crate::steam::ids::Book> {
    let mut b = crate::sys::lock(&BOOK);
    if b.stale() {
        if let Some(lan) = net::lan() {
            b.mine = ME.load(Ordering::Relaxed);
            if b.refresh(lan).clashed_now {
                drop(b);
                publish_presence();
                b = crate::sys::lock(&BOOK);
            }
        }
    }
    b
}

/// Presence or players changed: work the SteamIDs out again on the next look.
pub fn ids_changed() {
    crate::sys::lock(&BOOK).dirty = true;
}

/// A peer's SteamID as this game knows it: the one it goes by on the network, or its
/// LAN one (no SteamID told, or one that clashes). Ours for ourselves.
pub fn steam_id(peer: PeerId) -> u64 {
    let lan = net::lan();
    if lan.is_some_and(|l| l.me() == peer) {
        return my_steam_id();
    }
    let mut b = book();
    if let Some(id) = b.shown(peer) {
        return id;
    }
    let id = lan_steam_id(peer);
    b.note(id, peer);
    id
}

/// Whether account IDs (a SteamID's low 32 bits) are swapped in packets too while a
/// clash is on: only with `PEERPARTY_STEAM_ACCOUNTS=1`. Off by default: an account id
/// that two players share is usually small (a few hex digits), and as 4 bytes it turns
/// up in ordinary game data, which swapping would corrupt.
fn accounts_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = crate::config::HookConfig::global().steam_accounts == Some(true);
        if on {
            log!("steam-lan account IDs in packets are swapped too on a SteamID clash (PEERPARTY_STEAM_ACCOUNTS=1)");
        }
        on
    })
}

/// `PEERPARTY_STEAM_DUMP=1` (or a count): log game packets, lobby chat and lobby data
/// in hex, with every SteamID or account ID found in them and how it's encoded. A
/// diagnostic for games whose players end up mixed up; off by default.
fn dump_max() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        let v = crate::config::HookConfig::global().steam_dump.clone();
        let n = match v.trim() {
            "" | "0" => 0,
            "1" => 400,
            x => x.parse().unwrap_or(400),
        };
        if n > 0 {
            log!("steam-lan dump on (PEERPARTY_STEAM_DUMP): up to {n} packets each way, IDs flagged");
        }
        n
    })
}

/// Logs `data` for the dump: the first 60 each way in full, after that only ones
/// holding an ID, up to `dump_max`.
pub fn dump(what: &str, peer: Option<PeerId>, data: &[u8]) {
    let max = dump_max();
    if max == 0 {
        return;
    }
    static SEEN: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);
    let ids = book().all_ids();
    let mut all = ids.clone();
    all.push(my_steam_id());
    all.sort_unstable();
    all.dedup();
    let found = crate::steam::ids::find_ids(data, &all);
    let k = {
        let mut g = crate::sys::lock(&SEEN);
        let c = g.get_or_insert_with(HashMap::new).entry(what.to_string()).or_insert(0);
        if *c >= max || (*c >= 60 && found.is_empty()) {
            return;
        }
        *c += 1;
        *c
    };
    let who = peer.map(|p| format!(" {}", p)).unwrap_or_default();
    let found = if found.is_empty() { String::new() } else { format!(" ids=[{}]", found.join(", ")) };
    log!("steam-lan dump {what}{who} #{k} len={}{found}: {}", data.len(), crate::steam::ids::hex_head(data, 160));
}

/// For the dump: what the game was told for `what` (a lobby's members in order, its
/// owner), logged when it changes, up to `dump_max` times each.
pub fn dump_answer(what: &str, answer: impl FnOnce() -> String) {
    let max = dump_max();
    if max == 0 {
        return;
    }
    static LAST: Mutex<Option<HashMap<String, (String, usize)>>> = Mutex::new(None);
    let answer = answer();
    let mut g = crate::sys::lock(&LAST);
    let e = g.get_or_insert_with(HashMap::new).entry(what.to_string()).or_insert((String::new(), 0));
    if e.0 == answer || e.1 >= max {
        return;
    }
    e.0 = answer.clone();
    e.1 += 1;
    drop(g);
    log!("steam-lan dump {what}: {answer} (we're {})", my_steam_id());
}

fn ids_in_with(from: PeerId, data: &mut [u8], accounts: bool) {
    dump(if accounts { "in" } else { "text-in" }, Some(from), data);
    let n = crate::steam::ids::swapped_in(&book(), from, data, accounts);
    if n.any() {
        let (ids, accounts) = (n.ids, n.accounts);
        crate::log::throttled("steam-lan ids in", 3, |k| {
            log!("steam-lan SteamID clash: swapped {ids} SteamID(s) and {accounts} account ID(s) in what a LAN player sent (n={k})")
        });
    }
}

/// What `from` sent (a packet), with SteamIDs and account IDs as this game knows them
/// (only while a clash is on).
pub fn ids_in(from: PeerId, data: &mut [u8]) {
    ids_in_with(from, data, accounts_on());
}

/// `data` for `to`, with SteamIDs and account IDs as it knows them (only for copies
/// that don't handle clashes themselves).
pub fn ids_out(to: PeerId, data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    dump("out", Some(to), data);
    crate::steam::ids::swapped_out(&book(), to, data, accounts_on())
}

/// Text from `from` (lobby data, rich presence), with SteamIDs as this game knows them
/// (not account IDs: they're short numbers in text).
pub fn ids_in_text(from: PeerId, text: String) -> String {
    let mut b = text.into_bytes();
    ids_in_with(from, &mut b, false);
    String::from_utf8(b).unwrap_or_default()
}

/// The LAN peer (not us) with SteamID `id`.
fn lan_peer(id: u64) -> Option<PeerId> {
    if let Some(p) = book().peer(id) {
        return Some(p);
    }
    // A SteamID the game kept from before we mapped it.
    lan_friends()
        .into_iter()
        .map(|(p, _)| p)
        .find(|p| steam_id(*p) == id)
}

/// The LAN player (us included) with SteamID `id`, or the host of game server `id`.
pub fn peer_of(id: u64) -> Option<PeerId> {
    if id != 0 && id == my_steam_id() {
        return net::lan().map(|l| l.me());
    }
    // Our network SteamID, should the game have picked it up.
    if let Some(lan) = net::lan().filter(|l| id == lan_steam_id(l.me()) && crate::sys::lock(&BOOK).clashed) {
        return Some(lan.me());
    }
    lan_peer(id).or_else(|| crate::steam::gs::host_of(id))
}

/// Our SteamID. The first copy on this PC: one of its own for this PC (derived from
/// PeerParty's identity), so every PC's game sees a different SteamID and account ID
/// even when steam_api gives every player the same one; or, with the game's SteamID
/// kept (`steamid` mode, `own_id_on`), the one steam_api gives. A LAN one for a second
/// copy, and for a dedicated server (which never holds the PC's first slot).
pub fn my_steam_id() -> u64 {
    let known = ME.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }
    let Some(lan) = net::lan() else { return 0 };
    let first = lan.slot() == 0 && !lan.server();
    let real = if first { real_steam_id() } else { None };
    let own = real.is_some_and(|r| own_id_on() && r != lan_steam_id(lan.me()));
    let id = if own { lan_steam_id(lan.me()) } else { real.unwrap_or_else(|| lan_steam_id(lan.me())) };
    if let (true, Some(game)) = (own, real) {
        // steam_api still knows the player by its own SteamID: what goes to it and comes
        // back from it is mapped (steamown.rs). Saves keyed by SteamID start from a copy
        // of an earlier one of its own for this PC, else steam_api's (saves.rs).
        crate::steam::own::set(id, game);
        crate::saves::adopt_steam(id, game, &crate::saves::earlier_own_steam_ids());
    }
    // The first copy waits for the game to fetch its ISteamUser before settling.
    if real.is_some() || !first {
        ME.store(id, Ordering::Relaxed);
        {
            let mut b = crate::sys::lock(&BOOK);
            b.mine = id;
            b.dirty = true;
        }
        if !first && !lan.server() {
            // --copy-saves: saves under this LAN SteamID start from the first copy's: under
            // its own SteamID for this PC when it has one, else (or when that has no
            // save yet) steam_api's.
            let mut mains = Vec::new();
            if own_id_on() {
                mains.extend(crate::saves::first_slot().map(lan_steam_id));
            }
            mains.extend(real_steam_id());
            for main in mains {
                crate::saves::learn(&id.to_string(), &main.to_string());
                crate::saves::learn(&(id as u32).to_string(), &(main as u32).to_string());
            }
        }
        log!(
            "steam-lan our SteamID is {id} ({})",
            if own {
                format!("this PC's own, account {}; steam_api's is {}", id as u32, real.unwrap_or(0))
            } else if real.is_some() {
                "the game's own from steam_api".to_string()
            } else if lan.server() {
                "a LAN one: this is a dedicated server".to_string()
            } else {
                "a LAN one: another copy has this PC's".to_string()
            }
        );
        publish_presence();
    }
    id
}

/// Whether the first copy shows the game a SteamID of its own for this PC:
/// `PEERPARTY_MODE_STEAMID` (the launcher's "Steam player ID" setting, global or per
/// game) `lan` always, `official` never, `auto` (the default) unless the Steam
/// client is in the game, whose SteamIDs are each player's own already.
pub(crate) fn own_id_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let mode = crate::config::HookConfig::global().mode_text("steamid");
        let (on, why) = match mode.as_str() {
            "lan" | "on" | "own" | "1" => (true, "steamid mode lan"),
            "official" | "off" | "game" | "0" => (false, "steamid mode official"),
            _ if steam_client_in_game() => (false, "auto: Steam runs the game"),
            _ => (true, "auto: Steam isn't running the game"),
        };
        log!("steam-lan {} ({why})", if on { "the game gets a SteamID of its own for this PC" } else { "the game keeps steam_api's SteamID" });
        on
    })
}

/// Whether a steamclient module in the game was loaded from a Steam install (a steam.exe
/// beside it).
pub(crate) fn steam_client_in_game() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
    ["steamclient64.dll", "steamclient.dll"].iter().any(|n| {
        let w: Vec<u16> = n.encode_utf16().chain(Some(0)).collect();
        let m = unsafe { GetModuleHandleW(w.as_ptr()) };
        if m.is_null() {
            return false;
        }
        let mut buf = [0u16; 1024];
        let len = unsafe { GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let path = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len.min(buf.len())]));
        path.parent().is_some_and(|d| d.join("steam.exe").is_file())
    })
}

fn real_steam_id() -> Option<u64> {
    let user = crate::steam::obj::seen("SteamUser")?;
    // The flat function where there is one: it returns the SteamID as a plain number,
    // while the C++ method returns a class, whose hidden return pointer not every
    // steam_api build expects in the same place.
    let out = match real("SteamAPI_ISteamUser_GetSteamID") {
        Some(f) => {
            let f: unsafe extern "C" fn(usize) -> u64 = unsafe { std::mem::transmute(f) };
            unsafe { f(crate::steam::obj::unwrap(user)) }
        }
        None => {
            let mut out = 0u64;
            let _ = call_real!(
                user,
                "GetSteamID",
                fn(*mut u64) -> *mut u64,
                std::ptr::null_mut(),
                &mut out
            );
            out
        }
    };
    plausible_steam_id(out).then_some(out)
}

/// An individual account's SteamID: universe 1-4, account type 1, an account id.
fn plausible_steam_id(id: u64) -> bool {
    (1..=4).contains(&(id >> 56)) && (id >> 52) & 0xF == 1 && id as u32 != 0
}

/// Whether we're a second copy on this PC (with a LAN SteamID and name).
/// LAN friends in a stable order.
fn lan_friends() -> Vec<(PeerId, String)> {
    let mut v = net::lan().map(|l| l.peers()).unwrap_or_default();
    v.sort_by_key(|(id, _)| *id);
    v
}


/// The game's Steam app id: `SteamAppId` (the launcher sets it), else `SteamGameId`'s low
/// 24 bits (Steam sets both for a game it starts), else a `steam_appid.txt` in the
/// working folder or beside the exe (where steam_api itself looks). Found once.
pub fn app_id() -> u32 {
    static ID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        let env = |k: &str| std::env::var(k).ok().and_then(|s| s.trim().parse::<u64>().ok());
        if let Some(id) = env("SteamAppId").filter(|&id| id != 0) {
            return id as u32;
        }
        if let Some(id) = env("SteamGameId").map(|g| (g & 0xFF_FFFF) as u32).filter(|&id| id != 0) {
            return id;
        }
        let mut dirs = Vec::new();
        dirs.extend(std::env::current_dir().ok());
        dirs.extend(std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())));
        dirs.iter().find_map(|d| appid_in(&std::fs::read_to_string(d.join("steam_appid.txt")).ok()?)).unwrap_or(0)
    })
}

/// The id in a steam_appid.txt's text: its first number.
fn appid_in(text: &str) -> Option<u32> {
    let digits: String = text.trim_start_matches('\u{feff}').trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok().filter(|&id| id != 0)
}

/// A C string that stays valid (Steam's own strings stay valid until the next call).
pub fn c_str(s: &str) -> *const c_char {
    let mut m = crate::sys::lock(&STRINGS);
    // Most are asked for again and again: no new key then.
    if let Some(c) = m.get(s) {
        return c.as_ptr();
    }
    m.entry(s.to_string())
        .or_insert_with(|| CString::new(s.replace('\0', "")).unwrap())
        .as_ptr()
}

fn real(name: &str) -> Option<usize> {
    REAL.read().unwrap().get(name).copied()
}

/// Calls the real method. For a stand-in (steamobj.rs) whose interface version has the
/// method with the newest signature, that's the real object's own C++ method of the
/// same name: steam_api's flat function calls the slot of the version the flat API was
/// built for, and a game fetching an older version through an old accessor holds an
/// object whose slots differ (one build's flat RequestUserInformation landed on the
/// older object's SetRichPresence, with the SteamID for its strings, and crashed).
/// Otherwise the flat function, with the real object (a stand-in's virtual call would
/// come straight back here). Not for a method that returns a class (a CSteamID): the
/// C++ method returns it through a hidden pointer (`real_friend_by_index`).
macro_rules! real_call {
    ($name:literal, fn(usize $(, $t:ty)*) -> $r:ty, $default:expr, $this:expr $(, $arg:expr)*) => {
        if unsafe { crate::steam::obj::real_method($this, method_of($name)) }.is_some() {
            call_real!($this, method_of($name), fn($($t),*) -> $r, $default $(, $arg)*)
        } else {
            match real($name) {
                Some(f) => {
                    let f: unsafe extern "C" fn(usize $(, $t)*) -> $r = unsafe { std::mem::transmute(f) };
                    unsafe { f(unwrap($this) $(, $arg)*) }
                }
                None => $default,
            }
        }
    };
}

/// "SteamAPI_ISteamFriends_GetFriendCount" -> "GetFriendCount".
fn method_of(flat: &str) -> &str {
    flat.rsplit('_').next().unwrap_or(flat)
}

/// The real ISteamFriends's friend `index`: the flat function where there is one, else
/// the C++ method, which returns the CSteamID through a hidden pointer after `this`.
fn real_friend_by_index(this: usize, index: i32, flags: i32) -> u64 {
    if let Some(f) = real("SteamAPI_ISteamFriends_GetFriendByIndex") {
        let f: unsafe extern "C" fn(usize, i32, i32) -> u64 = unsafe { std::mem::transmute(f) };
        return unsafe { f(unwrap(this), index, flags) };
    }
    let mut out = 0u64;
    let _ = call_real!(
        this,
        "GetFriendByIndex",
        fn(*mut u64, i32, i32) -> *mut u64,
        std::ptr::null_mut(),
        &mut out,
        index,
        flags
    );
    out
}

/// LAN friends who aren't already Steam friends (who'd be listed twice).
fn extra_friends(this: usize) -> Vec<(PeerId, String)> {
    let real = real_friends(this);
    lan_friends().into_iter().filter(|(p, _)| !real.contains(&steam_id(*p))).collect()
}

/// The real friend list (immediate friends), read through GetFriendByIndex and kept for
/// a moment: games ask for the count every frame. Not HasFriend: some steam_api builds
/// answer it yes for every SteamID, and one build's crashes on any call.
fn real_friends(this: usize) -> Vec<u64> {
    static CACHE: Mutex<Option<(usize, Instant, Vec<u64>)>> = Mutex::new(None);
    let n = real_call!(
        "SteamAPI_ISteamFriends_GetFriendCount",
        fn(usize, i32) -> i32,
        0,
        this,
        FRIEND_FLAG_IMMEDIATE
    )
    .max(0);
    let mut cache = crate::sys::lock(&CACHE);
    if let Some((at, when, ids)) = cache.as_ref() {
        if *at == this && ids.len() == n as usize && when.elapsed() < Duration::from_secs(2) {
            return ids.clone();
        }
    }
    let ids: Vec<u64> = (0..n).map(|i| real_friend_by_index(this, i, FRIEND_FLAG_IMMEDIATE)).collect();
    *cache = Some((this, Instant::now(), ids.clone()));
    ids
}

// ---- ISteamUser ----

/// Whether the Steam client is in the process. Without it nothing answers an auth
/// ticket request.
fn real_steam_client() -> bool {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    ["steamclient64.dll", "steamclient.dll"].iter().any(|n| {
        let w: Vec<u16> = n.encode_utf16().chain(Some(0)).collect();
        !unsafe { GetModuleHandleW(w.as_ptr()) }.is_null()
    })
}

/// A game asked for an auth ticket. Steam answers with
/// `GetAuthSessionTicketResponse_t`; when no steamclient module is in the process,
/// PeerParty does, since some games wait for it before signing in to their services.
///
/// A real Steam client's failure (handle 0) is passed on as it is; with no client, 0 is
/// what an unanswered call gives, and PeerParty hands out a handle of its own.
pub fn auth_ticket_issued(ticket: u32) -> u32 {
    if real_steam_client() {
        return ticket;
    }
    let ticket = if ticket == 0 { 0x5050_0001 } else { ticket };
    log!("steam-lan auth ticket {ticket:#x}: nothing else answers it, answering GetAuthSessionTicketResponse_t");
    crate::steam::inject(GET_AUTH_SESSION_TICKET_RESPONSE, bytes(&TicketResponse { ticket, result: RESULT_OK }));
    ticket
}

/// Presence that says this copy's auth tickets carry PeerParty's proof (no real Steam
/// client signs them): a ticket for this player without one isn't this player's.
pub const KEY_TICKETS: &str = "peerparty:tickets";

/// A ticket steam_api just wrote (`ticket`, `*len` bytes, room for `max`): with no real
/// Steam client to sign it, PeerParty's proof goes on the end (`Lan::auth_ticket`), so
/// the PC it goes to can tell whose it is.
///
/// # Safety
/// `ticket` holds `max` bytes and `len` points at the length written.
pub unsafe fn add_ticket_proof(ticket: *mut u8, max: i32, len: *mut u32) {
    if real_steam_client() || ticket.is_null() || len.is_null() {
        return;
    }
    let Some(proof) = net::lan().and_then(|l| l.auth_ticket()) else { return };
    let n = unsafe { *len } as usize;
    if n + proof.len() > max.max(0) as usize {
        return;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(proof.as_ptr(), ticket.add(n), proof.len());
        *len = (n + proof.len()) as u32;
    }
}

/// BeginAuthSession for a LAN player's SteamID `user` (`peer`): 0 (OK) or 1
/// (k_EBeginAuthSessionResultInvalidTicket). A ticket with PeerParty's proof must be
/// that player's; one without is that player's only if their copy doesn't put proofs
/// on its tickets (it has a real Steam client, whose tickets can't be checked here).
pub fn auth_verdict(ticket: *const u8, len: i32, user: u64, peer: PeerId) -> i32 {
    let Some(lan) = net::lan() else { return 0 };
    let bytes = if ticket.is_null() || len <= 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(ticket, len as usize) } };
    let proves = lan.check_auth_ticket(bytes, peer);
    let wants_proof = lan.presence(peer).iter().any(|(k, v)| k == KEY_TICKETS && v == "1");
    match proves {
        Some(true) => 0,
        Some(false) => {
            log!("steam-lan auth: a ticket for {user} isn't that player's (signed by someone else): refused");
            1
        }
        None if wants_proof => {
            log!("steam-lan auth: a ticket for {user} without that player's proof: refused");
            1
        }
        None => 0,
    }
}

/// The web API flavour (`GetTicketForWebApiResponse_t`), which newer games and EOS
/// sign-in through Steam wait for.
pub fn web_ticket_issued(ticket: u32) -> u32 {
    if real_steam_client() {
        return ticket;
    }
    let ticket = if ticket == 0 { 0x5050_0002 } else { ticket };
    log!("steam-lan web API ticket {ticket:#x}: nothing else answers it, answering GetTicketForWebApiResponse_t");
    let mut r = Box::new(WebTicketResponse { ticket, result: RESULT_OK, len: 16, data: [0; 2560] });
    r.data[..8].copy_from_slice(&my_steam_id().to_le_bytes());
    r.data[8..16].copy_from_slice(b"peerpart");
    crate::steam::inject(GET_TICKET_FOR_WEB_API_RESPONSE, bytes(&*r));
    ticket
}

/// Reports signed in to Steam: on a LAN, Steam's servers being out of reach (Steam's
/// offline mode) mustn't hide multiplayer.
unsafe extern "C" fn logged_on(_this: usize) -> bool {
    true
}

unsafe extern "C" fn get_auth_ticket_for_web_api(this: usize, identity: *const c_char) -> u32 {
    let h = real_call!(
        "SteamAPI_ISteamUser_GetAuthTicketForWebApi",
        fn(usize, *const c_char) -> u32,
        0,
        this,
        identity
    );
    web_ticket_issued(h)
}

unsafe extern "C" fn get_auth_session_ticket(
    this: usize,
    ticket: *mut u8,
    max: i32,
    len: *mut u32,
    identity: *const core::ffi::c_void,
) -> u32 {
    let h = real_call!(
        "SteamAPI_ISteamUser_GetAuthSessionTicket",
        fn(usize, *mut u8, i32, *mut u32, *const core::ffi::c_void) -> u32,
        0,
        this,
        ticket,
        max,
        len,
        identity
    );
    if h != 0 {
        unsafe { add_ticket_proof(ticket, max, len) };
    }
    auth_ticket_issued(h)
}

/// `EncryptedAppTicketResponse_t` (k_iSteamUserCallbacks + 54): its EResult.
const ENCRYPTED_APP_TICKET_RESPONSE: i32 = 154;

/// The data the game asked to put in its encrypted app ticket (demonware/auth.rs).
static TICKET_DATA: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// A game whose Demonware PeerParty answers (demonware/) asks for an encrypted app
/// ticket to sign in with. Only the publisher can read a real one, so PeerParty makes
/// its own, which carries the data the game put in it for PeerParty's sign-in to read:
/// Some(the call's handle). None for any other game, whose request goes to Steam. Both
/// the flat function and the C++ stand-in (steamp2p.rs) come here.
pub fn encrypted_ticket_requested(data: *const u8, len: i32) -> Option<u64> {
    if !crate::demonware::active() {
        return None;
    }
    let included = if data.is_null() || len <= 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data, len.min(1024) as usize) }.to_vec()
    };
    log!("steam-lan encrypted app ticket requested ({} bytes of the game's inside): PeerParty's, for its Demonware service", included.len());
    *crate::sys::lock(&TICKET_DATA) = included;
    let call = crate::steam::new_call();
    crate::steam::complete(call, ENCRYPTED_APP_TICKET_RESPONSE, RESULT_OK.to_le_bytes().to_vec());
    Some(call)
}

/// The ticket `encrypted_ticket_requested` made, written to `ticket`: Some(whether it
/// fit), or None when the request went to Steam.
///
/// # Safety
/// `ticket` holds `max` bytes; `len` is null or writable.
pub unsafe fn encrypted_ticket(ticket: *mut u8, max: i32, len: *mut u32) -> Option<bool> {
    if !crate::demonware::active() {
        return None;
    }
    let t = crate::demonware::auth::app_ticket(my_steam_id(), &crate::sys::lock(&TICKET_DATA));
    if ticket.is_null() || max < 0 || t.len() > max as usize {
        log!("steam-lan encrypted app ticket: {} bytes don't fit the game's {max}", t.len());
        return Some(false);
    }
    unsafe {
        std::ptr::copy_nonoverlapping(t.as_ptr(), ticket, t.len());
        if !len.is_null() {
            *len = t.len() as u32;
        }
    }
    crate::log::throttled("steam-lan encrypted app ticket read", 2, |n| {
        log!("steam-lan encrypted app ticket handed to the game ({} bytes) n={n}", t.len())
    });
    Some(true)
}

unsafe extern "C" fn request_encrypted_app_ticket(this: usize, data: *const u8, len: i32) -> u64 {
    if let Some(call) = encrypted_ticket_requested(data, len) {
        return call;
    }
    real_call!(
        "SteamAPI_ISteamUser_RequestEncryptedAppTicket",
        fn(usize, *const u8, i32) -> u64,
        0,
        this,
        data,
        len
    )
}

unsafe extern "C" fn get_encrypted_app_ticket(this: usize, ticket: *mut u8, max: i32, len: *mut u32) -> bool {
    if let Some(ok) = unsafe { encrypted_ticket(ticket, max, len) } {
        return ok;
    }
    real_call!(
        "SteamAPI_ISteamUser_GetEncryptedAppTicket",
        fn(usize, *mut u8, i32, *mut u32) -> bool,
        false,
        this,
        ticket,
        max,
        len
    )
}

// ---- ISteamFriends ----

unsafe extern "C" fn get_friend_count(this: usize, flags: i32) -> i32 {
    let real = real_call!(
        "SteamAPI_ISteamFriends_GetFriendCount",
        fn(usize, i32) -> i32,
        0,
        this,
        flags
    );
    if flags & FRIEND_FLAG_IMMEDIATE != 0 {
        real.max(0) + extra_friends(this).len() as i32
    } else {
        real
    }
}

unsafe extern "C" fn get_friend_by_index(this: usize, index: i32, flags: i32) -> u64 {
    let n_real = real_call!(
        "SteamAPI_ISteamFriends_GetFriendCount",
        fn(usize, i32) -> i32,
        0,
        this,
        flags
    )
    .max(0);
    if index < n_real {
        return real_friend_by_index(this, index, flags);
    }
    if flags & FRIEND_FLAG_IMMEDIATE == 0 {
        return 0;
    }
    extra_friends(this)
        .get((index - n_real) as usize)
        .map_or(0, |(p, _)| steam_id(*p))
}

/// Our name is PeerParty's in every copy, the first included: LAN players see the same
/// name in every service (the launcher's Settings).
unsafe extern "C" fn get_persona_name(this: usize) -> *const c_char {
    if let Some(l) = net::lan() {
        return c_str(l.name());
    }
    real_call!(
        "SteamAPI_ISteamFriends_GetPersonaName",
        fn(usize) -> *const c_char,
        c_str(""),
        this
    )
}

unsafe extern "C" fn get_friend_persona_name(this: usize, id: u64) -> *const c_char {
    if id != 0 && id == my_steam_id() {
        if let Some(l) = net::lan() {
            return c_str(l.name());
        }
    }
    match lan_peer(id) {
        Some(p) => c_str(
            &net::lan()
                .and_then(|l| l.peer_name(p))
                .unwrap_or_else(|| "LAN player".into()),
        ),
        None => real_call!(
            "SteamAPI_ISteamFriends_GetFriendPersonaName",
            fn(usize, u64) -> *const c_char,
            c_str(""),
            this,
            crate::steam::own::to_steam(id)
        ),
    }
}

unsafe extern "C" fn get_friend_persona_state(this: usize, id: u64) -> i32 {
    match lan_peer(id) {
        Some(_) => PERSONA_ONLINE,
        None => real_call!(
            "SteamAPI_ISteamFriends_GetFriendPersonaState",
            fn(usize, u64) -> i32,
            0,
            this,
            crate::steam::own::to_steam(id)
        ),
    }
}

unsafe extern "C" fn get_friend_relationship(this: usize, id: u64) -> i32 {
    match lan_peer(id) {
        Some(_) => RELATIONSHIP_FRIEND,
        None => real_call!(
            "SteamAPI_ISteamFriends_GetFriendRelationship",
            fn(usize, u64) -> i32,
            0,
            this,
            crate::steam::own::to_steam(id)
        ),
    }
}

unsafe extern "C" fn has_friend(this: usize, id: u64, flags: i32) -> bool {
    match lan_peer(id) {
        Some(_) => true,
        None => real_call!(
            "SteamAPI_ISteamFriends_HasFriend",
            fn(usize, u64, i32) -> bool,
            false,
            this,
            crate::steam::own::to_steam(id),
            flags
        ),
    }
}

unsafe extern "C" fn get_friend_game_played(
    this: usize,
    id: u64,
    out: *mut FriendGameInfo,
) -> bool {
    if lan_peer(id).is_none() {
        return real_call!(
            "SteamAPI_ISteamFriends_GetFriendGamePlayed",
            fn(usize, u64, *mut FriendGameInfo) -> bool,
            false,
            this,
            crate::steam::own::to_steam(id),
            out
        );
    }
    if !out.is_null() {
        let lobby = lan_peer(id)
            .and_then(|p| {
                net::lan()?
                    .presence(p)
                    .into_iter()
                    .find(|(k, _)| k == KEY_STEAM_LOBBY)?
                    .1
                    .parse::<u64>()
                    .ok()
            })
            .unwrap_or(0);
        unsafe {
            out.write(FriendGameInfo {
                game_id: app_id() as u64,
                game_ip: 0,
                game_port: 0,
                query_port: 0,
                lobby,
            })
        };
    }
    true
}

/// A LAN friend's rich presence, without PeerParty's own keys.
fn presence_of(id: u64) -> Option<Vec<(String, String)>> {
    let peer = lan_peer(id)?;
    let mut kv = net::lan().map(|l| l.presence(peer)).unwrap_or_default();
    kv.retain(|(k, _)| !k.starts_with(KEY_PREFIX));
    Some(kv.into_iter().map(|(k, v)| (k, ids_in_text(peer, v))).collect())
}

unsafe extern "C" fn get_friend_rich_presence(
    this: usize,
    id: u64,
    key: *const c_char,
) -> *const c_char {
    match presence_of(id) {
        Some(kv) => {
            let k = crate::sys::read::cstr(key as usize).unwrap_or_default();
            let v = kv
                .iter()
                .find(|(x, _)| x.eq_ignore_ascii_case(&k))
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            c_str(v)
        }
        None => real_call!(
            "SteamAPI_ISteamFriends_GetFriendRichPresence",
            fn(usize, u64, *const c_char) -> *const c_char,
            c_str(""),
            this,
            crate::steam::own::to_steam(id),
            key
        ),
    }
}

unsafe extern "C" fn rich_presence_key_count(this: usize, id: u64) -> i32 {
    match presence_of(id) {
        Some(kv) => kv.len() as i32,
        None => real_call!(
            "SteamAPI_ISteamFriends_GetFriendRichPresenceKeyCount",
            fn(usize, u64) -> i32,
            0,
            this,
            crate::steam::own::to_steam(id)
        ),
    }
}

unsafe extern "C" fn rich_presence_key_by_index(this: usize, id: u64, index: i32) -> *const c_char {
    match presence_of(id) {
        Some(kv) => c_str(
            kv.get(index.max(0) as usize)
                .map_or("", |(k, _)| k.as_str()),
        ),
        None => real_call!(
            "SteamAPI_ISteamFriends_GetFriendRichPresenceKeyByIndex",
            fn(usize, u64, i32) -> *const c_char,
            c_str(""),
            this,
            crate::steam::own::to_steam(id),
            index
        ),
    }
}

/// Tells the LAN our rich presence, our SteamID and the Steam lobby we're in.
pub fn publish_presence() {
    let mut kv: Vec<(String, String)> = MY_PRESENCE
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    {
        let mut b = crate::sys::lock(&BOOK);
        b.mine = ME.load(Ordering::Relaxed);
        kv.extend(b.presence());
    }
    if let Some(lobby) = crate::steam::mm::current_lobby_id() {
        kv.push((KEY_STEAM_LOBBY.into(), lobby.to_string()));
    }
    if !real_steam_client() {
        kv.push((KEY_TICKETS.into(), "1".into()));
    }
    net::set_presence("steam", kv);
}

unsafe extern "C" fn set_rich_presence(
    this: usize,
    key: *const c_char,
    value: *const c_char,
) -> bool {
    if let Some(k) = crate::sys::read::cstr(key as usize) {
        let v = crate::sys::read::cstr(value as usize).unwrap_or_default();
        log!("steam-lan rich presence {k} = {v}");
        {
            let mut p = crate::sys::lock(&MY_PRESENCE);
            if v.is_empty() {
                p.remove(&k);
            } else {
                p.insert(k, v);
            }
        }
        publish_presence();
    }
    real_call!(
        "SteamAPI_ISteamFriends_SetRichPresence",
        fn(usize, *const c_char, *const c_char) -> bool,
        true,
        this,
        key,
        value
    )
}

unsafe extern "C" fn clear_rich_presence(this: usize) {
    crate::sys::lock(&MY_PRESENCE).clear();
    publish_presence();
    real_call!(
        "SteamAPI_ISteamFriends_ClearRichPresence",
        fn(usize) -> (),
        (),
        this
    );
}

/// Steam Input's XInput slot for controller `input` (`GetGamepadIndexForController`,
/// -1 for one that isn't an XInput controller): a stand-in's real object's own method,
/// else steam_api's flat function, found beside the hooked read `sibling`.
fn gamepad_index(this: usize, sibling: &str, input: u64) -> Option<i32> {
    if unsafe { crate::steam::obj::real_method(this, "GetGamepadIndexForController") }.is_some() {
        return Some(call_real!(this, "GetGamepadIndexForController", fn(u64) -> i32, -1, input));
    }
    let flat = format!("{}GetGamepadIndexForController", &sibling[..sibling.len() - method_of(sibling).len()]);
    let f = flat_beside(sibling, &flat)?;
    let f: unsafe extern "C" fn(usize, u64) -> i32 = unsafe { std::mem::transmute(f) };
    Some(unsafe { f(unwrap(this), input) })
}

/// steam_api's flat function `want`, from the module the real `sibling` is in.
fn flat_beside(sibling: &str, want: &str) -> Option<usize> {
    static FOUND: Mutex<Vec<(String, Option<usize>)>> = Mutex::new(Vec::new());
    if let Some((_, f)) = crate::sys::lock(&FOUND).iter().find(|(n, _)| n == want) {
        return *f;
    }
    use windows_sys::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW, GetProcAddress};
    let f = real(sibling).and_then(|at| {
        let mut m = std::ptr::null_mut();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if unsafe { GetModuleHandleExW(flags, at as *const u16, &mut m) } == 0 {
            return None;
        }
        let name = CString::new(want).ok()?;
        unsafe { GetProcAddress(m, name.as_ptr().cast()) }.map(|f| f as usize)
    });
    let mut found = crate::sys::lock(&FOUND);
    if found.len() < 8 {
        found.push((want.to_string(), f));
    }
    f
}

/// Whether controller `input`'s actions read at rest now (`input::steam_input_held`).
fn held(this: usize, sibling: &str, input: u64) -> bool {
    crate::input::steam_input_held(input, |h| gamepad_index(this, sibling, h))
}

/// `InputDigitalActionData_t { bool bState; bool bActive; }`: two bytes, returned in a
/// register. While the overlay shows, the button reads released (still active); in a
/// split-screen copy, so does every controller's but its own.
fn digital(name: &str, this: usize, input: u64, action: u64) -> u16 {
    let Some(f) = real(name) else { return 0 };
    let f: unsafe extern "C" fn(usize, u64, u64) -> u16 = unsafe { std::mem::transmute(f) };
    // The real object: a stand-in's methods would gate the read a second time.
    let r = unsafe { f(unwrap(this), input, action) };
    if held(this, name, input) { r & 0xFF00 } else { r }
}

/// `InputAnalogActionData_t { EInputSourceMode eMode; float x; float y; bool bActive; }`:
/// bigger than a register, so returned through a hidden pointer (first, before `this`).
/// While the overlay shows, x and y read 0 (mode and active kept).
fn analog(name: &str, out: *mut u8, this: usize, input: u64, action: u64) -> *mut u8 {
    let Some(f) = real(name) else {
        // No real one to ask: at rest, as the C++ path answers (`v_input_analog`).
        if !out.is_null() {
            unsafe { std::ptr::write_bytes(out, 0, 16) };
        }
        return out;
    };
    let f: unsafe extern "C" fn(*mut u8, usize, u64, u64) -> *mut u8 = unsafe { std::mem::transmute(f) };
    let r = unsafe { f(out, unwrap(this), input, action) };
    if !r.is_null() && held(this, name, input) {
        unsafe { std::ptr::write_bytes(r.add(4), 0, 8) };
    }
    r
}

/// ISteamInput's and ISteamController's C++ action reads, held at rest while the
/// overlay shows, as the flat ones are (`digital`, `analog`). A C++ method returns a
/// struct through a hidden pointer after `this`, whatever its size.
pub fn input() -> Vec<(&'static str, usize)> {
    vec![
        ("GetDigitalActionData", v_input_digital as *const () as usize),
        ("GetAnalogActionData", v_input_analog as *const () as usize),
    ]
}

vmethod! {
    fn v_input_digital(this: usize, out: *mut u8, input: u64, action: u64) -> *mut u8 {
        let r = call_real!(this, "GetDigitalActionData", fn(*mut u8, u64, u64) -> *mut u8, std::ptr::null_mut(), out, input, action);
        let r = if r.is_null() && !out.is_null() {
            // No such method: nothing pressed, not active.
            unsafe { std::ptr::write_bytes(out, 0, 2) };
            out
        } else {
            r
        };
        // bState, then bActive (kept).
        if !r.is_null() && held(this, "SteamAPI_ISteamInput_GetDigitalActionData", input) {
            unsafe { *r = 0 };
        }
        r
    }
}

vmethod! {
    fn v_input_analog(this: usize, out: *mut u8, input: u64, action: u64) -> *mut u8 {
        let r = call_real!(this, "GetAnalogActionData", fn(*mut u8, u64, u64) -> *mut u8, std::ptr::null_mut(), out, input, action);
        let r = if r.is_null() && !out.is_null() {
            unsafe { std::ptr::write_bytes(out, 0, 16) };
            out
        } else {
            r
        };
        if !r.is_null() && held(this, "SteamAPI_ISteamInput_GetAnalogActionData", input) {
            unsafe { std::ptr::write_bytes(r.add(4), 0, 8) };
        }
        r
    }
}

unsafe extern "C" fn input_digital(this: usize, input: u64, action: u64) -> u16 {
    digital("SteamAPI_ISteamInput_GetDigitalActionData", this, input, action)
}

unsafe extern "C" fn input_analog(out: *mut u8, this: usize, input: u64, action: u64) -> *mut u8 {
    analog("SteamAPI_ISteamInput_GetAnalogActionData", out, this, input, action)
}

unsafe extern "C" fn controller_digital(this: usize, input: u64, action: u64) -> u16 {
    digital("SteamAPI_ISteamController_GetDigitalActionData", this, input, action)
}

unsafe extern "C" fn controller_analog(out: *mut u8, this: usize, input: u64, action: u64) -> *mut u8 {
    analog("SteamAPI_ISteamController_GetAnalogActionData", out, this, input, action)
}

/// The overlay's "invite friends" dialog: PeerParty's overlay opens on its invite page
/// (inviting to this connect string); without an overlay every LAN friend is invited.
unsafe extern "C" fn invite_dialog(_this: usize, connect: *const c_char) {
    let connect = crate::sys::read::cstr(connect as usize).unwrap_or_default();
    if crate::overlay::invite_dialog(&connect, None) {
        log!("steam-lan the game's invite dialog (connect {connect:?}): PeerParty's overlay opens on its invite page");
        return;
    }
    log!("steam-lan the game's invite dialog (connect {connect:?}): no overlay, so every LAN friend is invited");
    crate::invite::send(None, &connect);
}

/// The overlay's "invite friends to this lobby" dialog: PeerParty's overlay opens on its
/// invite page; without an overlay every LAN friend is invited.
unsafe extern "C" fn invite_dialog_lobby(_this: usize, lobby: u64) {
    match crate::steam::mm::lobby_for(lobby) {
        Some(l) => {
            if crate::overlay::invite_dialog(&my_connect(), Some(l.clone())) {
                log!("steam-lan the game's invite dialog for lobby {lobby}: PeerParty's overlay opens on its invite page");
                return;
            }
            log!("steam-lan the game's invite dialog for lobby {lobby}: no overlay, so every LAN friend is invited");
            crate::invite::send_lobby(None, l);
        }
        None => log!("steam-lan invite dialog for lobby {lobby}, which isn't a LAN lobby"),
    }
}

/// `ActivateGameOverlay("Friends" / "Players" / "Settings" / ...)`: PeerParty's overlay
/// opens on that page; without one, Steam's (if there is one) gets the call.
unsafe extern "C" fn activate_overlay(this: usize, dialog: *const c_char) {
    let d = crate::sys::read::cstr(dialog as usize).unwrap_or_default();
    if crate::overlay::steam_dialog(&d, None) {
        log!("steam-lan ActivateGameOverlay({d:?}): PeerParty's overlay opens");
        return;
    }
    log!("steam-lan ActivateGameOverlay({d:?}): no PeerParty overlay, so Steam's gets it");
    real_call!("SteamAPI_ISteamFriends_ActivateGameOverlay", fn(usize, *const c_char) -> (), (), this, dialog);
}

/// `ActivateGameOverlayToUser("steamid" / "chat" / ..., player)`: a LAN player's page
/// in PeerParty's overlay; anyone else goes to Steam's.
unsafe extern "C" fn activate_overlay_to_user(this: usize, dialog: *const c_char, id: u64) {
    let d = crate::sys::read::cstr(dialog as usize).unwrap_or_default();
    if let Some(p) = lan_peer(id) {
        if crate::overlay::steam_dialog(&d, Some(p)) {
            return;
        }
    }
    real_call!(
        "SteamAPI_ISteamFriends_ActivateGameOverlayToUser",
        fn(usize, *const c_char, u64) -> (),
        (),
        this,
        dialog,
        crate::steam::own::to_steam(id)
    );
}

/// The game hears PeerParty's overlay open and close as Steam's (`GameOverlayActivated_t`),
/// so it pauses or frees the cursor the way it does for Steam's.
pub fn overlay_activated(active: bool) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    #[repr(C)]
    struct GameOverlayActivated {
        active: u8,
        user_initiated: u8,
        app_id: u32,
        overlay_pid: u32,
    }
    crate::steam::inject(
        GAME_OVERLAY_ACTIVATED,
        bytes(&GameOverlayActivated {
            active: u8::from(active),
            user_initiated: 1,
            app_id: app_id(),
            overlay_pid: std::process::id(),
        }),
    );
}

unsafe extern "C" fn invite_user_to_game(this: usize, id: u64, connect: *const c_char) -> bool {
    match lan_peer(id) {
        Some(p) => {
            let c = crate::sys::read::cstr(connect as usize).unwrap_or_default();
            crate::invite::send(Some(p), &c) > 0
        }
        None => real_call!(
            "SteamAPI_ISteamFriends_InviteUserToGame",
            fn(usize, u64, *const c_char) -> bool,
            false,
            this,
            crate::steam::own::to_steam(id),
            connect
        ),
    }
}

/// LAN friends' details are already here: no download to wait for.
unsafe extern "C" fn request_user_information(this: usize, id: u64, name_only: bool) -> bool {
    match lan_peer(id) {
        Some(_) => false,
        None => real_call!(
            "SteamAPI_ISteamFriends_RequestUserInformation",
            fn(usize, u64, bool) -> bool,
            false,
            this,
            crate::steam::own::to_steam(id),
            name_only
        ),
    }
}

unsafe extern "C" fn avatar_small(this: usize, id: u64) -> i32 {
    match lan_peer(id) {
        Some(_) => 0,
        None => real_call!(
            "SteamAPI_ISteamFriends_GetSmallFriendAvatar",
            fn(usize, u64) -> i32,
            0,
            this,
            crate::steam::own::to_steam(id)
        ),
    }
}

unsafe extern "C" fn avatar_medium(this: usize, id: u64) -> i32 {
    match lan_peer(id) {
        Some(_) => 0,
        None => real_call!(
            "SteamAPI_ISteamFriends_GetMediumFriendAvatar",
            fn(usize, u64) -> i32,
            0,
            this,
            crate::steam::own::to_steam(id)
        ),
    }
}

unsafe extern "C" fn avatar_large(this: usize, id: u64) -> i32 {
    match lan_peer(id) {
        Some(_) => 0,
        None => real_call!(
            "SteamAPI_ISteamFriends_GetLargeFriendAvatar",
            fn(usize, u64) -> i32,
            0,
            this,
            crate::steam::own::to_steam(id)
        ),
    }
}

unsafe extern "C" fn get_player_nickname(this: usize, id: u64) -> *const c_char {
    match lan_peer(id) {
        Some(_) => std::ptr::null(),
        None => real_call!(
            "SteamAPI_ISteamFriends_GetPlayerNickname",
            fn(usize, u64) -> *const c_char,
            std::ptr::null(),
            this,
            crate::steam::own::to_steam(id)
        ),
    }
}

unsafe extern "C" fn get_friend_steam_level(this: usize, id: u64) -> i32 {
    match lan_peer(id) {
        Some(_) => 0,
        None => real_call!(
            "SteamAPI_ISteamFriends_GetFriendSteamLevel",
            fn(usize, u64) -> i32,
            0,
            this,
            crate::steam::own::to_steam(id)
        ),
    }
}

// ---- the same methods for C++ callers (steamobj.rs) ----

pub fn friends() -> Vec<(&'static str, usize)> {
    vec![
        ("GetPersonaName", v_get_persona_name as *const () as usize),
        ("GetFriendCount", v_get_friend_count as *const () as usize),
        ("GetFriendByIndex", v_get_friend_by_index as *const () as usize),
        ("GetFriendRelationship", v_get_friend_relationship as *const () as usize),
        ("GetFriendPersonaState", v_get_friend_persona_state as *const () as usize),
        ("GetFriendPersonaName", v_get_friend_persona_name as *const () as usize),
        ("GetFriendGamePlayed", v_get_friend_game_played as *const () as usize),
        ("GetFriendSteamLevel", v_get_friend_steam_level as *const () as usize),
        ("GetPlayerNickname", v_get_player_nickname as *const () as usize),
        ("HasFriend", v_has_friend as *const () as usize),
        // SteamFriends005-008: the same calls, the flags spelled EFriendFlags
        // (steamobj::SAME_CALL).
        ("GetFriendCount~1", v_get_friend_count as *const () as usize),
        ("HasFriend~2", v_has_friend as *const () as usize),
        ("ActivateGameOverlayInviteDialog", v_invite_dialog_lobby as *const () as usize),
        (
            "ActivateGameOverlayInviteDialogConnectString",
            v_invite_dialog as *const () as usize,
        ),
        ("GetSmallFriendAvatar", v_avatar_small as *const () as usize),
        ("GetMediumFriendAvatar", v_avatar_medium as *const () as usize),
        ("GetLargeFriendAvatar", v_avatar_large as *const () as usize),
        ("RequestUserInformation", v_request_user_information as *const () as usize),
        ("SetRichPresence", v_set_rich_presence as *const () as usize),
        ("ClearRichPresence", v_clear_rich_presence as *const () as usize),
        ("GetFriendRichPresence", v_get_friend_rich_presence as *const () as usize),
        ("GetFriendRichPresenceKeyCount", v_rich_presence_key_count as *const () as usize),
        ("GetFriendRichPresenceKeyByIndex", v_rich_presence_key_by_index as *const () as usize),
        ("InviteUserToGame", v_invite_user_to_game as *const () as usize),
        ("ActivateGameOverlay", v_activate_overlay as *const () as usize),
        ("ActivateGameOverlayToUser", v_activate_overlay_to_user as *const () as usize),
    ]
}

vmethod! { fn v_get_persona_name(this: usize) -> *const c_char { unsafe { get_persona_name(this) } } }
vmethod! { fn v_get_friend_count(this: usize, flags: i32) -> i32 { unsafe { get_friend_count(this, flags) } } }
vmethod! {
    /// Returns a CSteamID: through the hidden pointer MSVC passes after `this`.
    fn v_get_friend_by_index(this: usize, ret: *mut u64, index: i32, flags: i32) -> *mut u64 {
        unsafe { *ret = get_friend_by_index(this, index, flags) };
        ret
    }
}
vmethod! { fn v_get_friend_relationship(this: usize, id: u64) -> i32 { unsafe { get_friend_relationship(this, id) } } }
vmethod! { fn v_get_friend_persona_state(this: usize, id: u64) -> i32 { unsafe { get_friend_persona_state(this, id) } } }
vmethod! { fn v_get_friend_persona_name(this: usize, id: u64) -> *const c_char { unsafe { get_friend_persona_name(this, id) } } }
vmethod! {
    fn v_get_friend_game_played(this: usize, id: u64, out: *mut FriendGameInfo) -> bool {
        unsafe { get_friend_game_played(this, id, out) }
    }
}
vmethod! { fn v_get_friend_steam_level(this: usize, id: u64) -> i32 { unsafe { get_friend_steam_level(this, id) } } }
vmethod! { fn v_get_player_nickname(this: usize, id: u64) -> *const c_char { unsafe { get_player_nickname(this, id) } } }
vmethod! { fn v_has_friend(this: usize, id: u64, flags: i32) -> bool { unsafe { has_friend(this, id, flags) } } }
vmethod! { fn v_invite_dialog_lobby(this: usize, lobby: u64) { unsafe { invite_dialog_lobby(this, lobby) } } }
vmethod! { fn v_invite_dialog(this: usize, connect: *const c_char) { unsafe { invite_dialog(this, connect) } } }
vmethod! { fn v_activate_overlay(this: usize, dialog: *const c_char) { unsafe { activate_overlay(this, dialog) } } }
vmethod! {
    fn v_activate_overlay_to_user(this: usize, dialog: *const c_char, id: u64) {
        unsafe { activate_overlay_to_user(this, dialog, id) }
    }
}
vmethod! { fn v_avatar_small(this: usize, id: u64) -> i32 { unsafe { avatar_small(this, id) } } }
vmethod! { fn v_avatar_medium(this: usize, id: u64) -> i32 { unsafe { avatar_medium(this, id) } } }
vmethod! { fn v_avatar_large(this: usize, id: u64) -> i32 { unsafe { avatar_large(this, id) } } }
vmethod! {
    fn v_request_user_information(this: usize, id: u64, name_only: bool) -> bool {
        unsafe { request_user_information(this, id, name_only) }
    }
}
vmethod! {
    fn v_set_rich_presence(this: usize, key: *const c_char, value: *const c_char) -> bool {
        unsafe { set_rich_presence(this, key, value) }
    }
}
vmethod! { fn v_clear_rich_presence(this: usize) { unsafe { clear_rich_presence(this) } } }
vmethod! {
    fn v_get_friend_rich_presence(this: usize, id: u64, key: *const c_char) -> *const c_char {
        unsafe { get_friend_rich_presence(this, id, key) }
    }
}
vmethod! { fn v_rich_presence_key_count(this: usize, id: u64) -> i32 { unsafe { rich_presence_key_count(this, id) } } }
vmethod! {
    fn v_rich_presence_key_by_index(this: usize, id: u64, index: i32) -> *const c_char {
        unsafe { rich_presence_key_by_index(this, id, index) }
    }
}
vmethod! {
    fn v_invite_user_to_game(this: usize, id: u64, connect: *const c_char) -> bool {
        unsafe { invite_user_to_game(this, id, connect) }
    }
}

// ---- events -> Steam callbacks ----

pub fn bytes<T>(v: &T) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, size_of::<T>()) }.to_vec()
}

/// Runs from the game's Steam dispatch (RunFrame / RunCallbacks).
pub fn pump() {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let _ = my_steam_id();
    crate::steam::mm::tick();
    crate::steam::gns::tick();
    crate::steam::gs::tick();
    crate::steam::gs::finish_empty();
    for ev in net::take(Front::Steam) {
        if matches!(ev, Event::PeerJoined(_) | Event::PeerLeft(_) | Event::PresenceChanged(_)) {
            ids_changed();
        }
        crate::steam::mm::on_event(&ev);
        crate::steam::gs::on_event(&ev);
        crate::steam::p2p::on_event(&ev);
        crate::steam::gns::on_event(&ev);
        match ev {
            Event::PeerJoined(p) => crate::steam::inject(
                PERSONA_STATE_CHANGE,
                bytes(&PersonaStateChange {
                    steam_id: steam_id(p),
                    change_flags: CHANGE_COME_ONLINE
                        | CHANGE_NAME
                        | CHANGE_STATUS
                        | CHANGE_GAME_PLAYED,
                }),
            ),
            Event::PeerLeft(p) => crate::steam::inject(
                PERSONA_STATE_CHANGE,
                bytes(&PersonaStateChange {
                    steam_id: steam_id(p),
                    change_flags: CHANGE_GONE_OFFLINE,
                }),
            ),
            Event::PresenceChanged(p) => {
                let id = steam_id(p);
                crate::steam::inject(
                    FRIEND_RICH_PRESENCE_UPDATE,
                    bytes(&RichPresenceUpdate {
                        friend: id,
                        app_id: app_id(),
                    }),
                );
                crate::steam::inject(
                    PERSONA_STATE_CHANGE,
                    bytes(&PersonaStateChange {
                        steam_id: id,
                        change_flags: CHANGE_RICH_PRESENCE,
                    }),
                );
            }
            Event::Invite {
                from,
                lobby: Some(lobby),
                connect,
            } if lobby.namespace == crate::steam::mm::NS
                && (connect.is_empty() || crate::steam::delivers(GAME_LOBBY_JOIN_REQUESTED)) =>
            {
                let lobby_id = crate::steam::mm::remember(&lobby);
                let friend = steam_id(from);
                let decision = crate::invite::decision(from, &connect, Some(&lobby));
                log!(
                    "steam-lan invite from {from} to lobby {lobby_id} {}",
                    if decision.accept() { "accepted" } else { "passed to the game; waiting for the player" }
                );
                if decision == crate::invite::Decision::Forced {
                    // The game heard of it already (LobbyInvite_t); now it's accepted.
                } else {
                    crate::steam::inject(
                        LOBBY_INVITE,
                        bytes(&LobbyInvite {
                            user: friend,
                            lobby: lobby_id,
                            game: app_id() as u64,
                        }),
                    );
                }
                if decision.accept() {
                    crate::steam::inject(
                        GAME_LOBBY_JOIN_REQUESTED,
                        bytes(&LobbyJoinRequested {
                            lobby: lobby_id,
                            friend,
                        }),
                    );
                }
            }
            Event::Invite { from, connect, lobby } if !connect.is_empty() => {
                // Steam has no "invite received" for a rich presence join: it waits for
                // the player in the overlay.
                if !crate::invite::decision(from, &connect, lobby.as_ref()).accept() {
                    log!("steam-lan invite from {from} waits for the player: connect={connect}");
                    continue;
                }
                log!("steam-lan invite from {from} accepted: connect={connect}");
                let mut msg = JoinRequested {
                    friend: steam_id(from),
                    connect: [0; 256],
                };
                let n = connect.len().min(255);
                msg.connect[..n].copy_from_slice(&connect.as_bytes()[..n]);
                crate::steam::inject(GAME_RICH_PRESENCE_JOIN_REQUESTED, bytes(&msg));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host in a Steam lobby, or with a rich presence `connect`, is joined that way;
    /// one with only a game server through the server.
    #[test]
    fn a_lobby_or_connect_in_presence_is_joined_before_a_server() {
        let kv = |v: &[(&str, &str)]| v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        assert!(joined_by_lobby_or_connect(&kv(&[("peerparty:gs", "1"), (KEY_STEAM_LOBBY, "109775241000000001")])));
        assert!(joined_by_lobby_or_connect(&kv(&[("connect", "+connect_lobby 1")])));
        assert!(!joined_by_lobby_or_connect(&kv(&[("peerparty:gs", "1"), ("peerparty:gsport", "8766")])));
        assert!(!joined_by_lobby_or_connect(&kv(&[("connect", "")])));
    }

    #[test]
    fn reads_the_id_in_a_steam_appid_txt() {
        assert_eq!(appid_in("480\n"), Some(480));
        assert_eq!(appid_in("\u{feff} 1245620 \r\n"), Some(1245620));
        assert_eq!(appid_in("0"), None);
        assert_eq!(appid_in("abc"), None);
    }

    #[test]
    fn only_individual_accounts_count_as_a_real_steam_id() {
        assert!(plausible_steam_id(0x0110_0001_0BED_6549));
        assert!(plausible_steam_id(lan_steam_id(PeerId([7; 16]))));
        // What a build with another return convention hands back: small numbers, pointers.
        assert!(!plausible_steam_id(7));
        assert!(!plausible_steam_id(0x7FF8_0A73_1FE0));
        assert!(!plausible_steam_id(0x0110_0001_0000_0000));
        // A lobby (chat account type 8) isn't a player.
        assert!(!plausible_steam_id(0x0186_0000_0000_1234));
    }

    /// Steam Input reads pass through, and come back at rest while the overlay shows.
    #[test]
    fn steam_input_reads_rest_while_the_overlay_shows() {
        let _turn = crate::overlay::test_turn();
        unsafe extern "C" fn fake_digital(_: usize, _: u64, _: u64) -> u16 {
            0x0101 // pressed, active
        }
        unsafe extern "C" fn fake_analog(out: *mut u8, _: usize, _: u64, _: u64) -> *mut u8 {
            let v: [u8; 16] = [3, 0, 0, 0, 0, 0, 0x80, 0x3F, 0, 0, 0x80, 0xBF, 1, 0, 0, 0];
            unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), out, 16) };
            out
        }
        {
            let mut r = REAL.write().unwrap();
            r.insert("SteamAPI_ISteamInput_GetDigitalActionData".into(), fake_digital as *const () as usize);
            r.insert("SteamAPI_ISteamInput_GetAnalogActionData".into(), fake_analog as *const () as usize);
        }
        let read = || {
            let mut a = [0u8; 16];
            let d = unsafe { input_digital(1, 2, 3) };
            unsafe { input_analog(a.as_mut_ptr(), 1, 2, 3) };
            (d, a)
        };
        let (d, a) = read();
        assert_eq!(d, 0x0101);
        assert_eq!(&a[4..12], &[0, 0, 0x80, 0x3F, 0, 0, 0x80, 0xBF]);
        crate::overlay::set_visible_for_test(true);
        let (d, a) = read();
        crate::overlay::set_visible_for_test(false);
        assert_eq!(d, 0x0100, "released, still active");
        assert_eq!((a[0], &a[4..12], a[12]), (3, &[0u8; 8][..], 1), "stick centred, mode and active kept");
    }

    // A fake older ISteamFriends: its RequestUserInformation notes the SteamID it got;
    // every other slot (SetRichPresence among them) notes that it was reached instead.
    // All take RequestUserInformation's arguments, so a wrong slot can't unbalance an
    // x86 thiscall's stack.
    static GOT: AtomicU64 = AtomicU64::new(0);
    static WRONG: AtomicBool = AtomicBool::new(false);
    vmethod! { fn fake_request_user_information(_this: usize, id: u64, _name_only: bool) -> bool { GOT.store(id, Ordering::SeqCst); true } }
    vmethod! { fn fake_other(_this: usize, _id: u64, _name_only: bool) -> bool { WRONG.store(true, Ordering::SeqCst); false } }

    /// A steam_api whose flat functions call the slot of the version the flat API was
    /// built for (SteamFriends015), as one 32-bit build's do whatever object they get.
    unsafe extern "C" fn flat_for_015(obj: usize, id: u64, name_only: bool) -> bool {
        let i = crate::steam::obj::slot("SteamFriends015", "RequestUserInformation").unwrap();
        let vt = unsafe { *(obj as *const *const usize) };
        #[cfg(target_arch = "x86")]
        let f: unsafe extern "thiscall" fn(usize, u64, bool) -> bool = unsafe { std::mem::transmute(*vt.add(i)) };
        #[cfg(not(target_arch = "x86"))]
        let f: unsafe extern "C" fn(usize, u64, bool) -> bool = unsafe { std::mem::transmute(*vt.add(i)) };
        unsafe { f(obj, id, name_only) }
    }

    /// A game that fetched an older ISteamFriends through the old `SteamFriends()`
    /// accessor holds an object whose slots aren't the flat API's: PeerParty's calls on
    /// to the real object go to that version's own method, never through steam_api's
    /// flat function (which landed on SetRichPresence with the SteamID for its strings,
    /// and the game crashed opening its multiplayer menu).
    #[test]
    fn calls_on_to_an_older_interface_use_its_own_slots() {
        unsafe { std::env::set_var("PEERPARTY_ACTIVE", "1") };
        crate::config::reload();
        let v = "SteamFriends014";
        let at = crate::steam::obj::slot(v, "RequestUserInformation").unwrap();
        // The case this guards: the flat API's slot is another method in this version.
        let flat_slot = crate::steam::obj::slot("SteamFriends015", "RequestUserInformation").unwrap();
        assert_ne!(at, flat_slot);
        assert_eq!(crate::steam::obj::slot(v, "SetRichPresence"), Some(flat_slot));

        let mut vt = vec![fake_other as *const () as usize; crate::steam::obj::layout(v).unwrap().1.len()];
        vt[at] = fake_request_user_information as *const () as usize;
        let real = Box::leak(Box::new(vt.as_ptr() as usize)) as *const usize as usize;
        let proxy = crate::steam::obj::wrap(real, v);
        assert_ne!(proxy, real, "ISteamFriends is a stand-in");
        REAL.write()
            .unwrap()
            .insert("SteamAPI_ISteamFriends_RequestUserInformation".into(), flat_for_015 as *const () as usize);

        let id = 0x0110_0001_0BED_6549;
        let ok = real_call!(
            "SteamAPI_ISteamFriends_RequestUserInformation",
            fn(usize, u64, bool) -> bool,
            false,
            proxy,
            id,
            false
        );
        assert!(!WRONG.load(Ordering::SeqCst), "another method of the older version was called");
        assert!(ok);
        assert_eq!(GOT.load(Ordering::SeqCst), id, "the older version's own RequestUserInformation got the SteamID");
        std::mem::forget(vt);
    }

    /// Once the game uses Demonware, Steam's encrypted app ticket is PeerParty's: the
    /// request completes with k_EResultOK and the ticket carries the game's data.
    #[test]
    fn steams_encrypted_ticket_is_peerpartys_once_demonware_is_in_use() {
        crate::demonware::set_active();
        let mut included = vec![0x31u8; 24];
        included.extend_from_slice(b"Player\0");
        let call = encrypted_ticket_requested(included.as_ptr(), included.len() as i32).unwrap();
        assert_ne!(call, 0);
        assert!(crate::steam::injected().iter().any(|(id, _, data)| *id == 154 && data[..] == 1u32.to_le_bytes()));
        let mut buf = [0u8; 0x200];
        let mut len = 0u32;
        assert_eq!(unsafe { encrypted_ticket(buf.as_mut_ptr(), buf.len() as i32, &mut len) }, Some(true));
        let p = crate::demonware::auth::read_app_ticket(&buf[..len as usize]);
        assert_eq!(p.key, vec![0x31u8; 24]);
        assert_eq!(p.name, "Player");
        // Too small a buffer is refused, not overrun.
        assert_eq!(unsafe { encrypted_ticket(buf.as_mut_ptr(), 8, &mut len) }, Some(false));
    }

    #[test]
    fn lan_steam_ids_are_valid_individuals_and_map_back() {
        let p = PeerId([0xab; 16]);
        let id = steam_id(p);
        assert_eq!(id, lan_steam_id(p));
        assert_eq!(id >> 56, 1, "public universe");
        assert_eq!((id >> 52) & 0xf, 1, "individual");
        assert_eq!((id >> 32) & 0xfffff, 1, "instance: the desktop one, which games accept");
        assert!(id as u32 >= 0x8000_0000, "account above every real one");
        assert_eq!(lan_peer(id), Some(p));
        assert_eq!(size_of::<JoinRequested>(), 264);
        assert_eq!(size_of::<FriendGameInfo>(), 24);
        assert_eq!(size_of::<PersonaStateChange>(), 16);
    }
}
