//! The rest of Galaxy's interfaces, answered the way a signed-in Galaxy with no GOG
//! account behind it would: Galaxy's own client isn't signed in under PeerParty, so each of
//! these calls would fail through GetError, which GOG's C# glue throws as an exception
//! (a game may catch its stats one; a failed RequestUserData can make it quit).
//!
//! - IStats: stats and achievements live on disk per game and LAN player
//!   (`%LOCALAPPDATA%\peerparty\galaxy\<game>\<LAN id>\stats.tsv`), written on
//!   `StoreStatsAndAchievements`. Achievements only GOG knows about (names, display
//!   names) are the ones the game touched. Leaderboards fail through their listeners as
//!   if offline. Other LAN players' stats read as zero.
//! - IUtils: no images, no notifications, GOG services "connected"; the overlay is
//!   PeerParty's (visible when it shows, initialized when there's a launcher to show it).
//!   A web page opens in the player's browser.
//! - IApps stays with Galaxy: DLC ownership is never PeerParty's to answer.
//! - IStorage and ICloudStorage: files on disk next to the stats (`storage\`,
//!   `cloud\<container>\`). Shared files fail as if offline.
//! - ITelemetry: events go nowhere and report success.
//! - IChat: a chat room with a LAN player carries messages on LAN channel "galaxy:im".
//! - ICustomNetworking (connections to a game's own servers through GOG): fails at once
//!   through the listener, logged as a gap.
//! - IUser's tokens, tickets and session id, and IFriends' avatars, friend invitations
//!   and FindUser, for LAN players.
//!
//! Every method here has the newest SDK's signature; a slot whose signature differs in
//! an older SDK (`name~args` in galaxyvt.rs) stays with Galaxy.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, c_char};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use super::{
    Gid, SIGNED_IN, by_value, c_str, copy_out, friends_list, galaxy_id, gid, lan, later, my_id, my_name, name_of, ours, peer_of, real_method,
    s, targets,
};
use crate::log;
use crate::steamobj::vmethod;

/// LAN channel socket for IChat messages: room id (8 bytes LE), then the text.
pub const IM: &str = "galaxy:im";

const USER_STATS_AND_ACHIEVEMENTS_RETRIEVE: &str = "USER_STATS_AND_ACHIEVEMENTS_RETRIEVE";
const STATS_AND_ACHIEVEMENTS_STORE: &str = "STATS_AND_ACHIEVEMENTS_STORE";
const ACHIEVEMENT_CHANGE: &str = "ACHIEVEMENT_CHANGE";
const LEADERBOARDS_RETRIEVE: &str = "LEADERBOARDS_RETRIEVE";
const LEADERBOARD_ENTRIES_RETRIEVE: &str = "LEADERBOARD_ENTRIES_RETRIEVE";
const LEADERBOARD_SCORE_UPDATE: &str = "LEADERBOARD_SCORE_UPDATE_LISTENER";
const LEADERBOARD_RETRIEVE: &str = "LEADERBOARD_RETRIEVE";
const USER_TIME_PLAYED_RETRIEVE: &str = "USER_TIME_PLAYED_RETRIEVE";
const OVERLAY_VISIBILITY_CHANGE: &str = "OVERLAY_VISIBILITY_CHANGE";
const FILE_SHARE: &str = "FILE_SHARE";
const SHARED_FILE_DOWNLOAD: &str = "SHARED_FILE_DOWNLOAD";
const CLOUD_STORAGE_GET_FILE_LIST: &str = "CLOUD_STORAGE_GET_FILE_LIST";
const CLOUD_STORAGE_GET_FILE: &str = "CLOUD_STORAGE_GET_FILE";
const CLOUD_STORAGE_PUT_FILE: &str = "CLOUD_STORAGE_PUT_FILE";
const CLOUD_STORAGE_DELETE_FILE: &str = "CLOUD_STORAGE_DELETE_FILE";
const TELEMETRY_EVENT_SEND: &str = "TELEMETRY_EVENT_SEND_LISTENER";
const CHAT_ROOM_WITH_USER_RETRIEVE: &str = "CHAT_ROOM_WITH_USER_RETRIEVE_LISTENER";
const CHAT_ROOM_MESSAGE_SEND: &str = "CHAT_ROOM_MESSAGE_SEND_LISTENER";
const CHAT_ROOM_MESSAGES: &str = "CHAT_ROOM_MESSAGES_LISTENER";
const CHAT_ROOM_MESSAGES_RETRIEVE: &str = "CHAT_ROOM_MESSAGES_RETRIEVE_LISTENER";
const CONNECTION_OPEN: &str = "CUSTOM_NETWORKING_CONNECTION_OPEN";
const CONNECTION_CLOSE: &str = "CUSTOM_NETWORKING_CONNECTION_CLOSE";
const ENCRYPTED_APP_TICKET_RETRIEVE: &str = "ENCRYPTED_APP_TICKET_RETRIEVE";
const OPERATIONAL_STATE_CHANGE: &str = "OPERATIONAL_STATE_CHANGE";
const PLAYFAB_CREATE_OPENID_CONNECTION: &str = "PLAYFAB_CREATE_OPENID_CONNECTION";
const PLAYFAB_LOGIN_WITH_OPENID_CONNECT: &str = "PLAYFAB_LOGIN_WITH_OPENID_CONNECT";
const FRIEND_INVITATION_SEND: &str = "FRIEND_INVITATION_SEND_LISTENER";
const FRIEND_INVITATION_LIST_RETRIEVE: &str = "FRIEND_INVITATION_LIST_RETRIEVE_LISTENER";
const SENT_FRIEND_INVITATION_LIST_RETRIEVE: &str = "SENT_FRIEND_INVITATION_LIST_RETRIEVE_LISTENER";
const FRIEND_INVITATION_RESPOND_TO: &str = "FRIEND_INVITATION_RESPOND_TO_LISTENER";
const FRIEND_DELETE: &str = "FRIEND_DELETE_LISTENER";
const USER_FIND: &str = "USER_FIND_LISTENER";

// Failure reasons (each listener's own enum).
const STATS_CONNECTION_FAILURE: u32 = 1;
const ENTRIES_CONNECTION_FAILURE: u32 = 2;
const SCORE_CONNECTION_FAILURE: u32 = 2;
const CLOUD_NOT_FOUND: u32 = 3;
const CLOUD_BUFFER_TOO_SMALL: u32 = 7;
const CLOUD_WRITE_FUNC_ERROR: u32 = 8;
const CLOUD_READ_FUNC_ERROR: u32 = 6;
const CLOUD_PUT_CONNECTION_FAILURE: u32 = 5;
const CHAT_CONNECTION_FAILURE: u32 = 2;
const CUSTOM_CONNECTION_FAILURE: u32 = 1;
const USER_NOT_FOUND: u32 = 1;

const OVERLAY_STATE_NOT_SUPPORTED: u32 = 1;
const OVERLAY_STATE_INITIALIZED: u32 = 4;
const GOG_SERVICES_CONNECTED: u32 = 1;

/// Calls the listener given with a call and the global ones of its type, on the game's
/// thread from ProcessData.
fn notify(kind: &'static str, listener: usize, f: impl Fn(usize) + Send + 'static) {
    let to = targets(kind, listener);
    later(move || {
        for l in to {
            f(l);
        }
    });
}

/// A C string for a listener call made later.
fn owned(text: &str) -> CString {
    CString::new(text.replace('\0', "")).unwrap_or_default()
}

fn bytes<'a>(p: *const u8, n: u32) -> &'a [u8] {
    if p.is_null() || n == 0 { &[] } else { unsafe { std::slice::from_raw_parts(p, n as usize) } }
}

fn now() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32)
}

// ---- on disk ----

/// `%LOCALAPPDATA%\peerparty\galaxy\<exe>\<LAN id>\` (`PEERPARTY_GALAXY_DIR` replaces
/// it, for tests).
fn root() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("PEERPARTY_GALAXY_DIR") {
        return Some(PathBuf::from(d));
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let game = std::env::current_exe().ok()?.file_stem()?.to_string_lossy().to_ascii_lowercase();
    Some(base.join("peerparty").join("galaxy").join(game).join(lan()?.me().hex()))
}

/// A name the game gave, kept to one path component.
fn safe(name: &str) -> Option<String> {
    let n = name.replace(['/', '\\', ':'], "_");
    (!n.is_empty() && n != "." && n != "..").then_some(n)
}

fn storage_path(name: &str) -> Option<PathBuf> {
    Some(root()?.join("storage").join(safe(name)?))
}

fn cloud_path(container: &str, name: &str) -> Option<PathBuf> {
    Some(root()?.join("cloud").join(safe(container)?).join(safe(name)?))
}

/// Files in a folder: (name, size, modified), sorted by name.
fn listing(dir: Option<PathBuf>) -> Vec<(String, u32, u32)> {
    let Some(dir) = dir else { return Vec::new() };
    let mut v: Vec<(String, u32, u32)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok().filter(|m| m.is_file())?;
            let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as u32;
            Some((e.file_name().to_string_lossy().into_owned(), m.len() as u32, t))
        })
        .collect();
    v.sort();
    v
}

fn write_file(path: Option<PathBuf>, data: &[u8]) -> bool {
    let Some(p) = path else { return false };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    std::fs::write(p, data).is_ok()
}

// ---- state ----

#[derive(Default)]
struct Stats {
    loaded: bool,
    ints: BTreeMap<String, i32>,
    floats: BTreeMap<String, f32>,
    /// Average-rate stats: (count so far, session length so far).
    rates: BTreeMap<String, (f64, f64)>,
    /// Achievements the game touched: unlock time (0: locked).
    achievements: BTreeMap<String, u32>,
    /// Unlocked since the last store: the store tells the achievement listeners.
    unlocked: Vec<String>,
}

#[derive(Default)]
struct Ext {
    stats: Stats,
    cloud_list: Vec<(String, u32, u32, String)>,
    /// Chat rooms with LAN players: room id -> the other player's GalaxyID.
    rooms: HashMap<u64, u64>,
    /// Each room's messages: (message id, sender, send time, text).
    history: HashMap<u64, Vec<(u64, u64, u32, String)>>,
    unread: HashMap<u64, u32>,
    /// The messages GetChatRoomMessageByIndex reads: the last retrieved or received.
    current: Vec<(u64, u64, u32, String)>,
    visit: String,
}

static X: LazyLock<Mutex<Ext>> = LazyLock::new(Mutex::default);
static NEXT_INDEX: AtomicU32 = AtomicU32::new(0);
static NEXT_MESSAGE: AtomicU64 = AtomicU64::new(1);
static SESSION: LazyLock<u64> = LazyLock::new(|| {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    (t ^ (std::process::id() as u64).rotate_left(32)) | 1
});

fn with_stats<T>(f: impl FnOnce(&mut Stats) -> T) -> T {
    let mut x = X.lock().unwrap();
    if !x.stats.loaded {
        x.stats.loaded = true;
        load(&mut x.stats);
    }
    f(&mut x.stats)
}

fn load(st: &mut Stats) {
    let Some(text) = root().and_then(|r| std::fs::read_to_string(r.join("stats.tsv")).ok()) else { return };
    for line in text.lines() {
        let mut it = line.splitn(3, '\t');
        let (Some(kind), Some(name), Some(value)) = (it.next(), it.next(), it.next()) else { continue };
        let name = name.to_string();
        match kind {
            "i" => {
                st.ints.insert(name, value.parse().unwrap_or(0));
            }
            "f" => {
                st.floats.insert(name, value.parse().unwrap_or(0.0));
            }
            "r" => {
                let (c, l) = value.split_once('\t').unwrap_or((value, "0"));
                st.rates.insert(name, (c.parse().unwrap_or(0.0), l.parse().unwrap_or(0.0)));
            }
            "a" => {
                st.achievements.insert(name, value.parse().unwrap_or(0));
            }
            _ => {}
        }
    }
}

fn save(st: &Stats) -> bool {
    let mut out = String::new();
    let clean = |n: &str| n.replace(['\t', '\n', '\r'], " ");
    for (k, v) in &st.ints {
        out += &format!("i\t{}\t{v}\n", clean(k));
    }
    for (k, v) in &st.floats {
        out += &format!("f\t{}\t{v}\n", clean(k));
    }
    for (k, (c, l)) in &st.rates {
        out += &format!("r\t{}\t{c}\t{l}\n", clean(k));
    }
    for (k, v) in &st.achievements {
        out += &format!("a\t{}\t{v}\n", clean(k));
    }
    write_file(root().map(|r| r.join("stats.tsv")), out.as_bytes())
}

/// This player (GalaxyID() is 0) or another one.
fn is_me(user: Gid) -> bool {
    let id = gid(user);
    id == 0 || id == my_id()
}

// ---- IStats ----

pub fn stats() -> Vec<(&'static str, usize)> {
    vec![
        ("RequestUserStatsAndAchievements", request_stats as *const () as usize),
        ("GetStatInt", get_stat_int as *const () as usize),
        ("GetStatFloat", get_stat_float as *const () as usize),
        ("SetStatInt", set_stat_int as *const () as usize),
        ("SetStatFloat", set_stat_float as *const () as usize),
        ("UpdateAvgRateStat", update_avg_rate as *const () as usize),
        ("GetAchievementsNumber", achievements_number as *const () as usize),
        ("GetAchievementName", achievement_name as *const () as usize),
        ("GetAchievementNameCopy", achievement_name_copy as *const () as usize),
        ("GetAchievement", get_achievement as *const () as usize),
        ("SetAchievement", set_achievement as *const () as usize),
        ("ClearAchievement", clear_achievement as *const () as usize),
        ("StoreStatsAndAchievements", store_stats as *const () as usize),
        ("ResetStatsAndAchievements", reset_stats as *const () as usize),
        ("GetAchievementDisplayName", same_name as *const () as usize),
        ("GetAchievementDisplayNameCopy", same_name_copy as *const () as usize),
        ("GetAchievementDescription", empty_for_name as *const () as usize),
        ("GetAchievementDescriptionCopy", empty_for_name_copy as *const () as usize),
        ("IsAchievementVisible", yes_for_name as *const () as usize),
        ("IsAchievementVisibleWhileLocked", yes_for_name as *const () as usize),
        ("RequestLeaderboards", request_leaderboards as *const () as usize),
        ("GetLeaderboardDisplayName", same_name as *const () as usize),
        ("GetLeaderboardDisplayNameCopy", same_name_copy as *const () as usize),
        ("GetLeaderboardSortMethod", zero_for_name as *const () as usize),
        ("GetLeaderboardDisplayType", zero_for_name as *const () as usize),
        ("RequestLeaderboardEntriesGlobal", entries_global as *const () as usize),
        ("RequestLeaderboardEntriesAroundUser", entries_around as *const () as usize),
        ("RequestLeaderboardEntriesForUsers", entries_for_users as *const () as usize),
        ("GetRequestedLeaderboardEntry", requested_entry as *const () as usize),
        ("GetRequestedLeaderboardEntryWithDetails", requested_entry_details as *const () as usize),
        ("SetLeaderboardScore", set_score as *const () as usize),
        ("SetLeaderboardScoreWithDetails", set_score_details as *const () as usize),
        ("GetLeaderboardEntryCount", zero_for_name as *const () as usize),
        ("FindLeaderboard", find_leaderboard as *const () as usize),
        ("FindOrCreateLeaderboard", find_or_create_leaderboard as *const () as usize),
        ("RequestUserTimePlayed", request_time_played as *const () as usize),
        ("GetUserTimePlayed", time_played as *const () as usize),
    ]
}

vmethod! {
    fn request_stats(_this: usize, user: Gid, listener: usize) {
        ours();
        let id = if is_me(user) { my_id() } else { gid(user) };
        with_stats(|_| ());
        notify(USER_STATS_AND_ACHIEVEMENTS_RETRIEVE, listener, move |l| virt!(l, 1, fn(Gid), by_value(&id)));
    }
}

vmethod! {
    fn get_stat_int(_this: usize, name: *const c_char, user: Gid) -> i32 {
        ours();
        if !is_me(user) {
            return 0;
        }
        let n = s(name);
        with_stats(|st| st.ints.get(&n).copied().unwrap_or(0))
    }
}

vmethod! {
    fn get_stat_float(_this: usize, name: *const c_char, user: Gid) -> f32 {
        ours();
        if !is_me(user) {
            return 0.0;
        }
        let n = s(name);
        with_stats(|st| {
            st.floats.get(&n).copied().or_else(|| st.rates.get(&n).map(|(c, l)| if *l > 0.0 { (c / l) as f32 } else { 0.0 })).unwrap_or(0.0)
        })
    }
}

vmethod! {
    fn set_stat_int(_this: usize, name: *const c_char, value: i32) {
        ours();
        let n = s(name);
        with_stats(|st| st.ints.insert(n, value));
    }
}

vmethod! {
    fn set_stat_float(_this: usize, name: *const c_char, value: f32) {
        ours();
        let n = s(name);
        with_stats(|st| st.floats.insert(n, value));
    }
}

vmethod! {
    fn update_avg_rate(_this: usize, name: *const c_char, count: f32, length: f64) {
        ours();
        let n = s(name);
        with_stats(|st| {
            let r = st.rates.entry(n).or_insert((0.0, 0.0));
            r.0 += count as f64;
            r.1 += length;
        });
    }
}

vmethod! { fn achievements_number(_this: usize) -> u32 { ours(); with_stats(|st| st.achievements.len() as u32) } }

fn achievement_at(index: u32) -> String {
    with_stats(|st| st.achievements.keys().nth(index as usize).cloned().unwrap_or_default())
}

vmethod! { fn achievement_name(_this: usize, index: u32) -> *const c_char { ours(); c_str(&achievement_at(index)) } }

vmethod! {
    fn achievement_name_copy(_this: usize, index: u32, buf: *mut c_char, len: u32) {
        ours();
        copy_out(&achievement_at(index), buf, len);
    }
}

vmethod! {
    fn get_achievement(_this: usize, name: *const c_char, unlocked: *mut bool, time: *mut u32, user: Gid) {
        ours();
        let n = s(name);
        let t = if is_me(user) { with_stats(|st| *st.achievements.entry(n).or_insert(0)) } else { 0 };
        unsafe {
            if !unlocked.is_null() {
                *unlocked = t != 0;
            }
            if !time.is_null() {
                *time = t;
            }
        }
    }
}

vmethod! {
    fn set_achievement(_this: usize, name: *const c_char) {
        ours();
        let n = s(name);
        with_stats(|st| {
            let t = st.achievements.entry(n.clone()).or_insert(0);
            if *t == 0 {
                *t = now().max(1);
                st.unlocked.push(n);
            }
        });
    }
}

vmethod! {
    fn clear_achievement(_this: usize, name: *const c_char) {
        ours();
        let n = s(name);
        with_stats(|st| {
            st.achievements.insert(n.clone(), 0);
            st.unlocked.retain(|u| *u != n);
        });
    }
}

fn stored(listener: usize, reset: bool) {
    let (ok, unlocked) = with_stats(|st| {
        if reset {
            let keep: Vec<String> = st.achievements.keys().cloned().collect();
            *st = Stats { loaded: true, ..Default::default() };
            for k in keep {
                st.achievements.insert(k, 0);
            }
        }
        (save(st), std::mem::take(&mut st.unlocked))
    });
    if !ok {
        log!("galaxy stats: couldn't write them to disk");
    }
    for a in &unlocked {
        log!("galaxy achievement unlocked: {a}");
        crate::ipc::note("Achievement unlocked", a);
    }
    notify(STATS_AND_ACHIEVEMENTS_STORE, listener, |l| virt!(l, 1, fn()));
    if !unlocked.is_empty() {
        let names: Vec<CString> = unlocked.iter().map(|a| owned(a)).collect();
        notify(ACHIEVEMENT_CHANGE, 0, move |l| {
            for n in &names {
                virt!(l, 1, fn(*const c_char), n.as_ptr());
            }
        });
    }
}

vmethod! { fn store_stats(_this: usize, listener: usize) { ours(); stored(listener, false) } }
vmethod! { fn reset_stats(_this: usize, listener: usize) { ours(); stored(listener, true) } }
vmethod! { fn same_name(_this: usize, name: *const c_char) -> *const c_char { ours(); c_str(&s(name)) } }
vmethod! { fn same_name_copy(_this: usize, name: *const c_char, buf: *mut c_char, len: u32) { ours(); copy_out(&s(name), buf, len) } }
vmethod! { fn empty_for_name(_this: usize, _name: *const c_char) -> *const c_char { ours(); c_str("") } }
vmethod! { fn empty_for_name_copy(_this: usize, _name: *const c_char, buf: *mut c_char, len: u32) { ours(); copy_out("", buf, len) } }
vmethod! { fn yes_for_name(_this: usize, _name: *const c_char) -> bool { ours(); true } }
vmethod! { fn zero_for_name(_this: usize, _name: *const c_char) -> u32 { ours(); 0 } }

// Leaderboards live on GOG's servers: every request fails as if offline.

fn leaderboard_gap() {
    crate::log::gap("galaxy", "leaderboards (answered as offline)");
}

vmethod! {
    fn request_leaderboards(_this: usize, listener: usize) {
        ours();
        leaderboard_gap();
        notify(LEADERBOARDS_RETRIEVE, listener, |l| virt!(l, 2, fn(u32), STATS_CONNECTION_FAILURE));
    }
}

fn entries_failed(name: *const c_char, listener: usize) {
    ours();
    leaderboard_gap();
    let n = owned(&s(name));
    notify(LEADERBOARD_ENTRIES_RETRIEVE, listener, move |l| virt!(l, 2, fn(*const c_char, u32), n.as_ptr(), ENTRIES_CONNECTION_FAILURE));
}

vmethod! { fn entries_global(_this: usize, name: *const c_char, _start: u32, _end: u32, listener: usize) { entries_failed(name, listener) } }
vmethod! { fn entries_around(_this: usize, name: *const c_char, _before: u32, _after: u32, _user: Gid, listener: usize) { entries_failed(name, listener) } }
vmethod! { fn entries_for_users(_this: usize, name: *const c_char, _users: *const u64, _count: u32, listener: usize) { entries_failed(name, listener) } }

vmethod! {
    fn requested_entry(_this: usize, _index: u32, rank: *mut u32, score: *mut i32, user: *mut u64) {
        ours();
        unsafe {
            for p in [rank as *mut u32, score as *mut u32] {
                if !p.is_null() {
                    *p = 0;
                }
            }
            if !user.is_null() {
                *user = 0;
            }
        }
    }
}

vmethod! {
    fn requested_entry_details(_this: usize, index: u32, rank: *mut u32, score: *mut i32, _details: *mut u8, _size: u32, out: *mut u32, user: *mut u64) {
        unsafe { requested_entry(_this, index, rank, score, user) };
        if !out.is_null() {
            unsafe { *out = 0 };
        }
    }
}

fn score_failed(name: *const c_char, score: i32, listener: usize) {
    ours();
    leaderboard_gap();
    let n = owned(&s(name));
    notify(LEADERBOARD_SCORE_UPDATE, listener, move |l| virt!(l, 2, fn(*const c_char, i32, u32), n.as_ptr(), score, SCORE_CONNECTION_FAILURE));
}

vmethod! { fn set_score(_this: usize, name: *const c_char, score: i32, _force: bool, listener: usize) { score_failed(name, score, listener) } }
vmethod! {
    fn set_score_details(_this: usize, name: *const c_char, score: i32, _details: *const u8, _size: u32, _force: bool, listener: usize) {
        score_failed(name, score, listener)
    }
}

fn leaderboard_failed(name: *const c_char, listener: usize) {
    ours();
    leaderboard_gap();
    let n = owned(&s(name));
    notify(LEADERBOARD_RETRIEVE, listener, move |l| virt!(l, 2, fn(*const c_char, u32), n.as_ptr(), STATS_CONNECTION_FAILURE));
}

vmethod! { fn find_leaderboard(_this: usize, name: *const c_char, listener: usize) { leaderboard_failed(name, listener) } }
vmethod! {
    fn find_or_create_leaderboard(_this: usize, name: *const c_char, _display: *const c_char, _sort: *const u32, _kind: *const u32, listener: usize) {
        leaderboard_failed(name, listener)
    }
}

vmethod! {
    fn request_time_played(_this: usize, user: Gid, listener: usize) {
        ours();
        let id = if is_me(user) { my_id() } else { gid(user) };
        notify(USER_TIME_PLAYED_RETRIEVE, listener, move |l| virt!(l, 1, fn(Gid), by_value(&id)));
    }
}

vmethod! { fn time_played(_this: usize, _user: Gid) -> u32 { ours(); 0 } }

// ---- IUtils ----

pub fn utils() -> Vec<(&'static str, usize)> {
    vec![
        ("GetImageSize", image_size as *const () as usize),
        ("GetImageRGBA", image_rgba as *const () as usize),
        ("RegisterForNotification", nothing_for_name as *const () as usize),
        ("GetNotification", get_notification as *const () as usize),
        ("ShowOverlayWithWebPage", show_web_page as *const () as usize),
        ("IsOverlayVisible", overlay_visible as *const () as usize),
        ("GetOverlayState", overlay_state as *const () as usize),
        ("DisableOverlayPopups", nothing_for_name as *const () as usize),
        ("GetGogServicesConnectionState", services_state as *const () as usize),
    ]
}

vmethod! {
    fn image_size(_this: usize, _id: u32, w: *mut i32, h: *mut i32) {
        ours();
        unsafe {
            if !w.is_null() {
                *w = 0;
            }
            if !h.is_null() {
                *h = 0;
            }
        }
    }
}

vmethod! {
    fn image_rgba(_this: usize, _id: u32, buf: *mut u8, len: u32) {
        ours();
        if !buf.is_null() {
            unsafe { std::ptr::write_bytes(buf, 0, len as usize) };
        }
    }
}

vmethod! { fn nothing_for_name(_this: usize, _name: *const c_char) { ours(); } }

vmethod! {
    fn get_notification(_this: usize, _id: u64, consumable: *mut bool, kind: *mut c_char, klen: u32, _content: *mut u8, _size: u32) -> u32 {
        ours();
        if !consumable.is_null() {
            unsafe { *consumable = false };
        }
        copy_out("", kind, klen);
        0
    }
}

vmethod! {
    fn show_web_page(_this: usize, url: *const c_char) {
        ours();
        let u = s(url);
        if u.starts_with("https://") || u.starts_with("http://") {
            log!("galaxy: the game opened {u}, in the player's browser");
            let _ = std::process::Command::new("explorer.exe").arg(&u).spawn();
        } else {
            log!("galaxy: the game asked the overlay for {u:?}, not a web page; ignored");
        }
    }
}

vmethod! { fn overlay_visible(_this: usize) -> bool { ours(); crate::overlay::visible() } }

vmethod! {
    fn overlay_state(_this: usize) -> u32 {
        ours();
        if crate::ipc::enabled() { OVERLAY_STATE_INITIALIZED } else { OVERLAY_STATE_NOT_SUPPORTED }
    }
}

vmethod! { fn services_state(_this: usize) -> u32 { ours(); GOG_SERVICES_CONNECTED } }

/// PeerParty's overlay showed or hid: Galaxy's overlay visibility listeners hear it.
pub fn overlay_changed(on: bool) {
    if SIGNED_IN.load(Ordering::Relaxed) {
        notify(OVERLAY_VISIBILITY_CHANGE, 0, move |l| virt!(l, 1, fn(bool), on));
    }
}

// ---- IApps ----

/// Nothing: DLC ownership is Galaxy's to answer.
pub fn apps() -> Vec<(&'static str, usize)> {
    Vec::new()
}

// ---- IStorage ----

pub fn storage() -> Vec<(&'static str, usize)> {
    vec![
        ("FileWrite", file_write as *const () as usize),
        ("FileRead", file_read as *const () as usize),
        ("FileDelete", file_delete as *const () as usize),
        ("FileExists", file_exists as *const () as usize),
        ("GetFileSize", file_size as *const () as usize),
        ("GetFileTimestamp", file_timestamp as *const () as usize),
        ("GetFileCount", file_count as *const () as usize),
        ("GetFileNameByIndex", file_name_at as *const () as usize),
        ("GetFileNameCopyByIndex", file_name_copy_at as *const () as usize),
        ("FileShare", file_share as *const () as usize),
        ("DownloadSharedFile", download_shared as *const () as usize),
        ("GetSharedFileName", shared_name as *const () as usize),
        ("GetSharedFileNameCopy", shared_name_copy as *const () as usize),
        ("GetSharedFileSize", shared_size as *const () as usize),
        ("GetSharedFileOwner", shared_owner as *const () as usize),
        ("SharedFileRead", shared_read as *const () as usize),
        ("SharedFileClose", shared_close as *const () as usize),
        ("GetDownloadedSharedFileCount", downloaded_count as *const () as usize),
        ("GetDownloadedSharedFileByIndex", downloaded_at as *const () as usize),
    ]
}

fn storage_files() -> Vec<(String, u32, u32)> {
    listing(root().map(|r| r.join("storage")))
}

vmethod! {
    fn file_write(_this: usize, name: *const c_char, data: *const u8, size: u32) {
        ours();
        let n = s(name);
        if !write_file(storage_path(&n), bytes(data, size)) {
            log!("galaxy FileWrite {n}: couldn't write it");
        }
    }
}

vmethod! {
    fn file_read(_this: usize, name: *const c_char, data: *mut u8, size: u32) -> u32 {
        ours();
        let Some(got) = storage_path(&s(name)).and_then(|p| std::fs::read(p).ok()) else { return 0 };
        let n = got.len().min(size as usize);
        if !data.is_null() && n > 0 {
            unsafe { std::ptr::copy_nonoverlapping(got.as_ptr(), data, n) };
        }
        n as u32
    }
}

vmethod! {
    fn file_delete(_this: usize, name: *const c_char) {
        ours();
        if let Some(p) = storage_path(&s(name)) {
            let _ = std::fs::remove_file(p);
        }
    }
}

vmethod! { fn file_exists(_this: usize, name: *const c_char) -> bool { ours(); storage_path(&s(name)).is_some_and(|p| p.is_file()) } }

fn storage_entry(name: *const c_char) -> Option<(String, u32, u32)> {
    let n = safe(&s(name))?;
    storage_files().into_iter().find(|(f, ..)| *f == n)
}

vmethod! { fn file_size(_this: usize, name: *const c_char) -> u32 { ours(); storage_entry(name).map_or(0, |e| e.1) } }
vmethod! { fn file_timestamp(_this: usize, name: *const c_char) -> u32 { ours(); storage_entry(name).map_or(0, |e| e.2) } }
vmethod! { fn file_count(_this: usize) -> u32 { ours(); storage_files().len() as u32 } }

fn storage_name_at(index: u32) -> String {
    storage_files().into_iter().nth(index as usize).map(|e| e.0).unwrap_or_default()
}

vmethod! { fn file_name_at(_this: usize, index: u32) -> *const c_char { ours(); c_str(&storage_name_at(index)) } }
vmethod! { fn file_name_copy_at(_this: usize, index: u32, buf: *mut c_char, len: u32) { ours(); copy_out(&storage_name_at(index), buf, len) } }

vmethod! {
    fn file_share(_this: usize, name: *const c_char, listener: usize) {
        ours();
        crate::log::gap("galaxy", "FileShare (shared files answered as offline)");
        let n = owned(&s(name));
        notify(FILE_SHARE, listener, move |l| virt!(l, 2, fn(*const c_char, u32), n.as_ptr(), STATS_CONNECTION_FAILURE));
    }
}

vmethod! {
    fn download_shared(_this: usize, id: u64, listener: usize) {
        ours();
        crate::log::gap("galaxy", "DownloadSharedFile (shared files answered as offline)");
        notify(SHARED_FILE_DOWNLOAD, listener, move |l| virt!(l, 2, fn(u64, u32), id, STATS_CONNECTION_FAILURE));
    }
}

vmethod! { fn shared_name(_this: usize, _id: u64) -> *const c_char { ours(); c_str("") } }
vmethod! { fn shared_name_copy(_this: usize, _id: u64, buf: *mut c_char, len: u32) { ours(); copy_out("", buf, len) } }
vmethod! { fn shared_size(_this: usize, _id: u64) -> u32 { ours(); 0 } }

vmethod! {
    fn shared_owner(_this: usize, ret: *mut u64, _id: u64) -> *mut u64 {
        ours();
        unsafe { *ret = 0 };
        ret
    }
}

vmethod! { fn shared_read(_this: usize, _id: u64, _data: *mut u8, _size: u32, _offset: u32) -> u32 { ours(); 0 } }
vmethod! { fn shared_close(_this: usize, _id: u64) { ours(); } }
vmethod! { fn downloaded_count(_this: usize) -> u32 { ours(); 0 } }
vmethod! { fn downloaded_at(_this: usize, _index: u32) -> u64 { ours(); 0 } }

// ---- ICloudStorage ----

pub fn cloud_storage() -> Vec<(&'static str, usize)> {
    vec![
        ("GetFileList", cloud_list as *const () as usize),
        ("GetFileNameByIndex", cloud_name_at as *const () as usize),
        ("GetFileSizeByIndex", cloud_size_at as *const () as usize),
        ("GetFileTimestampByIndex", cloud_time_at as *const () as usize),
        ("GetFileHashByIndex", cloud_hash_at as *const () as usize),
        // MSVC puts overloads in reverse order: the buffer forms come first.
        ("GetFile~5@4#1", cloud_get_buffer as *const () as usize),
        ("GetFile~5@4#2", cloud_get_func as *const () as usize),
        ("GetFileMetadata", cloud_metadata as *const () as usize),
        ("PutFile~8@4", cloud_put_buffer as *const () as usize),
        ("PutFile~9@5", cloud_put_func as *const () as usize),
        ("CalculateHash~4", hash_buffer as *const () as usize),
        ("CalculateHash~5", hash_func as *const () as usize),
        ("DeleteFile", cloud_delete as *const () as usize),
        ("OpenSavegame", nothing as *const () as usize),
        ("CloseSavegame", nothing as *const () as usize),
    ]
}

type WriteFunc = unsafe extern "C" fn(usize, *const u8, i32) -> i32;
type ReadFunc = unsafe extern "C" fn(usize, *mut u8, i32) -> i32;
type RewindFunc = unsafe extern "C" fn(usize, u32) -> i32;
/// ICloudStorage::ReadPhase.
const UPLOADING: u32 = 1;

vmethod! { fn nothing(_this: usize) { ours(); } }

vmethod! {
    fn cloud_list(_this: usize, container: *const c_char, listener: usize) {
        ours();
        let c = s(container);
        let files: Vec<(String, u32, u32, String)> = listing(root().zip(safe(&c)).map(|(r, c)| r.join("cloud").join(c)))
            .into_iter()
            .map(|(n, size, t)| {
                let hash = cloud_path(&c, &n).and_then(|p| std::fs::read(p).ok()).map(|d| crate::eos::storage::md5_hex(&d)).unwrap_or_default();
                (n, size, t, hash)
            })
            .collect();
        let count = files.len() as u32;
        let used: u32 = files.iter().map(|f| f.1).sum();
        X.lock().unwrap().cloud_list = files;
        // GOG's default quota is 200 MB a game.
        notify(CLOUD_STORAGE_GET_FILE_LIST, listener, move |l| virt!(l, 1, fn(u32, u32, u32), count, 200 << 20, used));
    }
}

fn cloud_at<T>(index: u32, f: impl FnOnce(&(String, u32, u32, String)) -> T) -> Option<T> {
    X.lock().unwrap().cloud_list.get(index as usize).map(f)
}

vmethod! { fn cloud_name_at(_this: usize, index: u32) -> *const c_char { ours(); c_str(&cloud_at(index, |e| e.0.clone()).unwrap_or_default()) } }
vmethod! { fn cloud_size_at(_this: usize, index: u32) -> u32 { ours(); cloud_at(index, |e| e.1).unwrap_or(0) } }
vmethod! { fn cloud_time_at(_this: usize, index: u32) -> u32 { ours(); cloud_at(index, |e| e.2).unwrap_or(0) } }
vmethod! { fn cloud_hash_at(_this: usize, index: u32) -> *const c_char { ours(); c_str(&cloud_at(index, |e| e.3.clone()).unwrap_or_default()) } }

fn get_succeeded(container: &str, name: &str, size: u32, listener: usize) {
    let (c, n) = (owned(container), owned(name));
    let id = owned("");
    notify(CLOUD_STORAGE_GET_FILE, listener, move |l| {
        virt!(l, 1, fn(*const c_char, *const c_char, u32, u32, *const c_char), c.as_ptr(), n.as_ptr(), size, 0, id.as_ptr())
    });
}

fn get_failed(container: &str, name: &str, reason: u32, listener: usize) {
    let (c, n) = (owned(container), owned(name));
    notify(CLOUD_STORAGE_GET_FILE, listener, move |l| virt!(l, 2, fn(*const c_char, *const c_char, u32), c.as_ptr(), n.as_ptr(), reason));
}

fn cloud_read(container: &str, name: &str) -> Option<Vec<u8>> {
    std::fs::read(cloud_path(container, name)?).ok()
}

vmethod! {
    fn cloud_get_buffer(_this: usize, container: *const c_char, name: *const c_char, buf: *mut u8, len: u32, listener: usize) {
        ours();
        let (c, n) = (s(container), s(name));
        match cloud_read(&c, &n) {
            None => get_failed(&c, &n, CLOUD_NOT_FOUND, listener),
            Some(d) if d.len() > len as usize => get_failed(&c, &n, CLOUD_BUFFER_TOO_SMALL, listener),
            Some(d) => {
                if !buf.is_null() && !d.is_empty() {
                    unsafe { std::ptr::copy_nonoverlapping(d.as_ptr(), buf, d.len()) };
                }
                get_succeeded(&c, &n, d.len() as u32, listener);
            }
        }
    }
}

vmethod! {
    fn cloud_get_func(_this: usize, container: *const c_char, name: *const c_char, param: usize, write: usize, listener: usize) {
        ours();
        let (c, n) = (s(container), s(name));
        let Some(d) = cloud_read(&c, &n) else { return get_failed(&c, &n, CLOUD_NOT_FOUND, listener) };
        if write != 0 {
            let write: WriteFunc = unsafe { std::mem::transmute(write) };
            for chunk in d.chunks(64 << 10) {
                if unsafe { write(param, chunk.as_ptr(), chunk.len() as i32) } < 0 {
                    return get_failed(&c, &n, CLOUD_WRITE_FUNC_ERROR, listener);
                }
            }
        }
        get_succeeded(&c, &n, d.len() as u32, listener);
    }
}

vmethod! {
    fn cloud_metadata(_this: usize, container: *const c_char, name: *const c_char, listener: usize) {
        ours();
        let (c, n) = (s(container), s(name));
        match cloud_path(&c, &n).and_then(|p| std::fs::metadata(p).ok()) {
            Some(m) => get_succeeded(&c, &n, m.len() as u32, listener),
            None => get_failed(&c, &n, CLOUD_NOT_FOUND, listener),
        }
    }
}

fn put_done(container: &str, name: &str, data: Option<&[u8]>, listener: usize) {
    let ok = data.is_some_and(|d| write_file(cloud_path(container, name), d));
    let (c, n) = (owned(container), owned(name));
    let reason = if data.is_some() { CLOUD_PUT_CONNECTION_FAILURE } else { CLOUD_READ_FUNC_ERROR };
    notify(CLOUD_STORAGE_PUT_FILE, listener, move |l| {
        if ok {
            virt!(l, 1, fn(*const c_char, *const c_char), c.as_ptr(), n.as_ptr())
        } else {
            virt!(l, 2, fn(*const c_char, *const c_char, u32), c.as_ptr(), n.as_ptr(), reason)
        }
    });
}

vmethod! {
    fn cloud_put_buffer(_this: usize, container: *const c_char, name: *const c_char, buf: *const u8, len: u32, listener: usize, _kind: u32, _time: u32, _hash: *const c_char) {
        ours();
        put_done(&s(container), &s(name), Some(bytes(buf, len)), listener);
    }
}

/// Everything `read` hands out, or None when it fails.
fn read_all(param: usize, read: usize) -> Option<Vec<u8>> {
    if read == 0 {
        return Some(Vec::new());
    }
    let read: ReadFunc = unsafe { std::mem::transmute(read) };
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let n = unsafe { read(param, chunk.as_mut_ptr(), chunk.len() as i32) };
        if n < 0 {
            return None;
        }
        if n == 0 {
            return Some(out);
        }
        out.extend_from_slice(&chunk[..(n as usize).min(chunk.len())]);
    }
}

fn rewind(param: usize, f: usize) {
    if f != 0 {
        let f: RewindFunc = unsafe { std::mem::transmute(f) };
        unsafe { f(param, UPLOADING) };
    }
}

vmethod! {
    fn cloud_put_func(_this: usize, container: *const c_char, name: *const c_char, param: usize, read: usize, rewind_f: usize, listener: usize, _kind: u32, _time: u32, _hash: *const c_char) {
        ours();
        rewind(param, rewind_f);
        let data = read_all(param, read);
        put_done(&s(container), &s(name), data.as_deref(), listener);
    }
}

vmethod! {
    fn hash_buffer(_this: usize, buf: *const u8, len: u32, out: *mut c_char, size: u32) {
        ours();
        copy_out(&crate::eos::storage::md5_hex(bytes(buf, len)), out, size);
    }
}

vmethod! {
    fn hash_func(_this: usize, param: usize, read: usize, rewind_f: usize, out: *mut c_char, size: u32) {
        ours();
        let data = read_all(param, read).unwrap_or_default();
        rewind(param, rewind_f);
        copy_out(&crate::eos::storage::md5_hex(&data), out, size);
    }
}

vmethod! {
    fn cloud_delete(_this: usize, container: *const c_char, name: *const c_char, listener: usize, _hash: *const c_char) {
        ours();
        let (c, n) = (s(container), s(name));
        let ok = cloud_path(&c, &n).is_some_and(|p| std::fs::remove_file(p).is_ok());
        let (cc, nn) = (owned(&c), owned(&n));
        notify(CLOUD_STORAGE_DELETE_FILE, listener, move |l| {
            if ok {
                virt!(l, 1, fn(*const c_char, *const c_char), cc.as_ptr(), nn.as_ptr())
            } else {
                virt!(l, 2, fn(*const c_char, *const c_char, u32), cc.as_ptr(), nn.as_ptr(), CLOUD_NOT_FOUND)
            }
        });
    }
}

// ---- ITelemetry ----

pub fn telemetry() -> Vec<(&'static str, usize)> {
    vec![
        ("AddStringParam", param_2 as *const () as usize),
        ("AddIntParam", param_int as *const () as usize),
        ("AddFloatParam", param_float as *const () as usize),
        ("AddBoolParam", param_bool as *const () as usize),
        ("AddObjectParam", nothing_for_name as *const () as usize),
        ("AddArrayParam", nothing_for_name as *const () as usize),
        ("CloseParam", nothing as *const () as usize),
        ("ClearParams", nothing as *const () as usize),
        ("SetSamplingClass", nothing_for_name as *const () as usize),
        ("SendTelemetryEvent", send_event as *const () as usize),
        ("SendAnonymousTelemetryEvent", send_event as *const () as usize),
        ("GetVisitID", visit_id as *const () as usize),
        ("GetVisitIDCopy", visit_id_copy as *const () as usize),
        ("ResetVisitID", reset_visit as *const () as usize),
    ]
}

vmethod! { fn param_2(_this: usize, _name: *const c_char, _value: *const c_char) { ours(); } }
vmethod! { fn param_int(_this: usize, _name: *const c_char, _value: i32) { ours(); } }
vmethod! { fn param_float(_this: usize, _name: *const c_char, _value: f64) { ours(); } }
vmethod! { fn param_bool(_this: usize, _name: *const c_char, _value: bool) { ours(); } }

vmethod! {
    fn send_event(_this: usize, kind: *const c_char, listener: usize) -> u32 {
        ours();
        let index = NEXT_INDEX.fetch_add(1, Ordering::Relaxed);
        let k = owned(&s(kind));
        notify(TELEMETRY_EVENT_SEND, listener, move |l| virt!(l, 1, fn(*const c_char, u32), k.as_ptr(), index));
        index
    }
}

fn visit() -> String {
    let mut x = X.lock().unwrap();
    if x.visit.is_empty() {
        x.visit = format!("{:016x}", *SESSION ^ now() as u64);
    }
    x.visit.clone()
}

vmethod! { fn visit_id(_this: usize) -> *const c_char { ours(); c_str(&visit()) } }
vmethod! { fn visit_id_copy(_this: usize, buf: *mut c_char, len: u32) { ours(); copy_out(&visit(), buf, len) } }
vmethod! { fn reset_visit(_this: usize) { ours(); X.lock().unwrap().visit.clear(); } }

// ---- IChat ----

pub fn chat() -> Vec<(&'static str, usize)> {
    vec![
        ("RequestChatRoomWithUser", chat_room_with as *const () as usize),
        ("RequestChatRoomMessages", chat_messages as *const () as usize),
        ("SendChatRoomMessage", chat_send as *const () as usize),
        ("GetChatRoomMessageByIndex", chat_message_at as *const () as usize),
        ("GetChatRoomMemberCount", chat_member_count as *const () as usize),
        ("GetChatRoomMemberUserIDByIndex", chat_member_at as *const () as usize),
        ("GetChatRoomUnreadMessageCount", chat_unread as *const () as usize),
        ("MarkChatRoomAsRead", chat_read as *const () as usize),
    ]
}

/// The same room id on both sides: from the pair of GalaxyIDs, in order.
pub fn room_of(a: u64, b: u64) -> u64 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for byte in lo.to_le_bytes().into_iter().chain(hi.to_le_bytes()) {
        h = (h ^ byte as u64).wrapping_mul(0x100_0000_01b3);
    }
    h | 1
}

vmethod! {
    fn chat_room_with(_this: usize, user: Gid, listener: usize) {
        ours();
        let id = gid(user);
        if peer_of(id).is_none() || id == my_id() {
            notify(CHAT_ROOM_WITH_USER_RETRIEVE, listener, move |l| virt!(l, 2, fn(Gid, u32), by_value(&id), CHAT_CONNECTION_FAILURE));
            return;
        }
        let room = room_of(my_id(), id);
        X.lock().unwrap().rooms.insert(room, id);
        notify(CHAT_ROOM_WITH_USER_RETRIEVE, listener, move |l| virt!(l, 1, fn(Gid, u64), by_value(&id), room));
    }
}

fn longest(v: &[(u64, u64, u32, String)]) -> u32 {
    v.iter().map(|m| m.3.len() as u32 + 1).max().unwrap_or(0)
}

vmethod! {
    fn chat_messages(_this: usize, room: u64, limit: u32, before: u64, listener: usize) {
        ours();
        let (count, long) = {
            let mut x = X.lock().unwrap();
            let all = x.history.get(&room).cloned().unwrap_or_default();
            let upto = if before == 0 { all.len() } else { all.iter().position(|m| m.0 == before).unwrap_or(all.len()) };
            let from = upto.saturating_sub(if limit == 0 { upto } else { limit as usize });
            x.current = all[from..upto].to_vec();
            (x.current.len() as u32, longest(&x.current))
        };
        notify(CHAT_ROOM_MESSAGES_RETRIEVE, listener, move |l| virt!(l, 1, fn(u64, u32, u32), room, count, long));
    }
}

vmethod! {
    fn chat_send(_this: usize, room: u64, msg: *const c_char, listener: usize) -> u32 {
        ours();
        let index = NEXT_INDEX.fetch_add(1, Ordering::Relaxed);
        let text = s(msg);
        let to = X.lock().unwrap().rooms.get(&room).copied();
        let sent = match (lan(), to.and_then(peer_of)) {
            (Some(lan), Some(peer)) => {
                let mut packet = room.to_le_bytes().to_vec();
                packet.extend_from_slice(text.as_bytes());
                let r = lan.send(peer, IM, 0, &packet, true, true, true);
                crate::log::throttled("galaxy chat send", 4, |n| log!("galaxy chat message n={n} to {} -> {r:?}", to.unwrap_or(0)));
                !matches!(r, peerparty_core::SendResult::Dropped | peerparty_core::SendResult::NoChannel)
            }
            _ => false,
        };
        if !sent {
            notify(CHAT_ROOM_MESSAGE_SEND, listener, move |l| virt!(l, 2, fn(u64, u32, u32), room, index, CHAT_CONNECTION_FAILURE));
            return index;
        }
        let id = NEXT_MESSAGE.fetch_add(1, Ordering::Relaxed);
        let t = now();
        X.lock().unwrap().history.entry(room).or_default().push((id, my_id(), t, text));
        notify(CHAT_ROOM_MESSAGE_SEND, listener, move |l| virt!(l, 1, fn(u64, u32, u64, u32), room, index, id, t));
        index
    }
}

vmethod! {
    fn chat_message_at(_this: usize, index: u32, id: *mut u64, kind: *mut u32, sender: *mut u64, time: *mut u32, buf: *mut c_char, len: u32) -> u32 {
        ours();
        let Some(m) = X.lock().unwrap().current.get(index as usize).cloned() else { return 0 };
        unsafe {
            if !id.is_null() {
                *id = m.0;
            }
            if !kind.is_null() {
                *kind = 1; // CHAT_MESSAGE_TYPE_CHAT_MESSAGE
            }
            if !sender.is_null() {
                *sender = m.1;
            }
            if !time.is_null() {
                *time = m.2;
            }
        }
        copy_out(&m.3, buf, len);
        (m.3.len() as u32).min(len.saturating_sub(1))
    }
}

vmethod! { fn chat_member_count(_this: usize, room: u64) -> u32 { ours(); if X.lock().unwrap().rooms.contains_key(&room) { 2 } else { 0 } } }

vmethod! {
    fn chat_member_at(_this: usize, ret: *mut u64, room: u64, index: u32) -> *mut u64 {
        ours();
        let other = X.lock().unwrap().rooms.get(&room).copied();
        unsafe {
            *ret = match (index, other) {
                (0, Some(_)) => my_id(),
                (1, Some(o)) => o,
                _ => 0,
            }
        };
        ret
    }
}

vmethod! { fn chat_unread(_this: usize, room: u64) -> u32 { ours(); X.lock().unwrap().unread.get(&room).copied().unwrap_or(0) } }
vmethod! { fn chat_read(_this: usize, room: u64) { ours(); X.lock().unwrap().unread.remove(&room); } }

/// From ProcessData: chat messages LAN players sent us.
pub fn pump() {
    let Some(lan) = lan() else { return };
    while let Some(p) = lan.receive_where(None, |s| s == IM) {
        if p.data.len() < 8 {
            continue;
        }
        let from = galaxy_id(p.peer);
        let room = u64::from_le_bytes(p.data[..8].try_into().unwrap());
        if room != room_of(my_id(), from) {
            log!("galaxy chat message from {from} for a room that isn't ours; dropped");
            continue;
        }
        crate::log::throttled("galaxy chat receive", 4, |n| log!("galaxy chat message n={n} from {from}"));
        let text = String::from_utf8_lossy(&p.data[8..]).into_owned();
        let m = (NEXT_MESSAGE.fetch_add(1, Ordering::Relaxed), from, now(), text);
        let long = {
            let mut x = X.lock().unwrap();
            x.rooms.insert(room, from);
            x.history.entry(room).or_default().push(m.clone());
            *x.unread.entry(room).or_default() += 1;
            x.current = vec![m];
            longest(&x.current)
        };
        notify(CHAT_ROOM_MESSAGES, 0, move |l| virt!(l, 1, fn(u64, u32, u32), room, 1, long));
    }
}

// ---- ICustomNetworking ----

pub fn custom_networking() -> Vec<(&'static str, usize)> {
    vec![
        ("OpenConnection", open_connection as *const () as usize),
        ("CloseConnection", close_connection as *const () as usize),
        ("SendData", send_data as *const () as usize),
        ("GetAvailableDataSize", available_data as *const () as usize),
        ("PeekData", read_data as *const () as usize),
        ("ReadData", read_data as *const () as usize),
        ("PopData", pop_data as *const () as usize),
    ]
}

vmethod! {
    fn open_connection(_this: usize, connect: *const c_char, listener: usize) {
        ours();
        let c = s(connect);
        crate::log::gap("galaxy", &format!("OpenConnection {c} (custom networking through GOG isn't carried; it fails)"));
        let cs = owned(&c);
        notify(CONNECTION_OPEN, listener, move |l| virt!(l, 2, fn(*const c_char, u32), cs.as_ptr(), CUSTOM_CONNECTION_FAILURE));
    }
}

vmethod! {
    fn close_connection(_this: usize, id: u64, listener: usize) {
        ours();
        notify(CONNECTION_CLOSE, listener, move |l| virt!(l, 1, fn(u64, u32), id, 0));
    }
}

vmethod! { fn send_data(_this: usize, _id: u64, _data: *const u8, _size: u32) { ours(); } }
vmethod! { fn available_data(_this: usize, _id: u64) -> u32 { ours(); 0 } }
vmethod! { fn read_data(_this: usize, _id: u64, _dest: *mut u8, _size: u32) { ours(); } }
vmethod! { fn pop_data(_this: usize, _id: u64, _size: u32) { ours(); } }

// ---- IUser: tokens, tickets, session ----

pub fn user() -> Vec<(&'static str, usize)> {
    vec![
        ("SignOut", sign_out as *const () as usize),
        ("RequestEncryptedAppTicket", request_ticket as *const () as usize),
        ("GetEncryptedAppTicket", get_ticket as *const () as usize),
        ("CreateOpenIDConnection", create_openid as *const () as usize),
        ("LoginWithOpenIDConnect", login_openid as *const () as usize),
        ("GetSessionID", session_id as *const () as usize),
        ("GetAccessToken", access_token as *const () as usize),
        ("GetAccessTokenCopy", access_token_copy as *const () as usize),
        ("GetRefreshToken", refresh_token as *const () as usize),
        ("GetRefreshTokenCopy", refresh_token_copy as *const () as usize),
        ("GetIDToken", id_token as *const () as usize),
        ("GetIDTokenCopy", id_token_copy as *const () as usize),
        ("ReportInvalidAccessToken", report_invalid_token as *const () as usize),
    ]
}

vmethod! {
    fn sign_out(_this: usize) {
        ours();
        if SIGNED_IN.swap(false, Ordering::Relaxed) {
            log!("galaxy signed out");
        }
        notify(OPERATIONAL_STATE_CHANGE, 0, |l| virt!(l, 1, fn(u32), 0));
    }
}

/// PeerParty's ticket: a local identity string, not a GOG ticket.
fn ticket() -> Vec<u8> {
    format!("peerparty-galaxy-ticket:{}", my_id()).into_bytes()
}

vmethod! {
    fn request_ticket(_this: usize, _data: *const u8, _size: u32, listener: usize) {
        ours();
        notify(ENCRYPTED_APP_TICKET_RETRIEVE, listener, |l| virt!(l, 1, fn()));
    }
}

vmethod! {
    fn get_ticket(_this: usize, buf: *mut u8, max: u32, current: *mut u32) {
        ours();
        let t = ticket();
        let n = t.len().min(max as usize);
        if !buf.is_null() && n > 0 {
            unsafe { std::ptr::copy_nonoverlapping(t.as_ptr(), buf, n) };
        }
        if !current.is_null() {
            unsafe { *current = n as u32 };
        }
    }
}

vmethod! {
    fn create_openid(_this: usize, _secret: *const c_char, _title: *const c_char, _connection: *const c_char, _ignore_nonce: bool, listener: usize) {
        ours();
        notify(PLAYFAB_CREATE_OPENID_CONNECTION, listener, |l| virt!(l, 1, fn(bool), true));
    }
}

vmethod! {
    fn login_openid(_this: usize, _title: *const c_char, _connection: *const c_char, _token: *const c_char, _create: bool, _request: *const c_char, _secret: *const c_char, listener: usize) {
        ours();
        notify(PLAYFAB_LOGIN_WITH_OPENID_CONNECT, listener, |l| virt!(l, 1, fn()));
    }
}

vmethod! { fn session_id(_this: usize) -> u64 { ours(); *SESSION } }

fn b64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Tokens name the player; the ID token is a JWT (unsigned) for games that read its
/// claims.
fn token(kind: &str) -> String {
    let id = my_id();
    match kind {
        "id" => {
            let header = b64url(br#"{"alg":"none","typ":"JWT"}"#);
            let name = my_name().replace(['"', '\\'], "");
            let t = now() as u64;
            let body = b64url(format!(r#"{{"iss":"peerparty","sub":"{id}","name":"{name}","iat":{t},"exp":{}}}"#, t + 86400).as_bytes());
            format!("{header}.{body}.")
        }
        k => format!("peerparty-{k}-{id}"),
    }
}

vmethod! { fn access_token(_this: usize) -> *const c_char { ours(); c_str(&token("access")) } }
vmethod! { fn access_token_copy(_this: usize, buf: *mut c_char, len: u32) { ours(); copy_out(&token("access"), buf, len) } }
vmethod! { fn refresh_token(_this: usize) -> *const c_char { ours(); c_str(&token("refresh")) } }
vmethod! { fn refresh_token_copy(_this: usize, buf: *mut c_char, len: u32) { ours(); copy_out(&token("refresh"), buf, len) } }
vmethod! { fn id_token(_this: usize) -> *const c_char { ours(); c_str(&token("id")) } }
vmethod! { fn id_token_copy(_this: usize, buf: *mut c_char, len: u32) { ours(); copy_out(&token("id"), buf, len) } }
vmethod! { fn report_invalid_token(_this: usize, _token: *const c_char, _info: *const c_char) -> bool { ours(); false } }

// ---- IFriends: avatars, friend invitations, FindUser ----

pub fn friends() -> Vec<(&'static str, usize)> {
    vec![
        ("GetDefaultAvatarCriteria", default_avatar_criteria as *const () as usize),
        ("SetDefaultAvatarCriteria", set_default_avatar_criteria as *const () as usize),
        ("GetFriendAvatarUrl", avatar_url as *const () as usize),
        ("GetFriendAvatarUrlCopy", avatar_url_copy as *const () as usize),
        ("GetFriendAvatarImageRGBA", avatar_rgba as *const () as usize),
        ("SendFriendInvitation", send_friend_invitation as *const () as usize),
        ("RequestFriendInvitationList", request_invitation_list as *const () as usize),
        ("RequestSentFriendInvitationList", request_sent_invitation_list as *const () as usize),
        ("GetFriendInvitationCount", invitation_count as *const () as usize),
        ("GetFriendInvitationByIndex", invitation_at as *const () as usize),
        ("RespondToFriendInvitation", respond_to_invitation as *const () as usize),
        ("DeleteFriend", delete_friend as *const () as usize),
        ("FindUser", find_user as *const () as usize),
    ]
}

static AVATAR_CRITERIA: AtomicU32 = AtomicU32::new(0);

vmethod! { fn default_avatar_criteria(_this: usize) -> u32 { ours(); AVATAR_CRITERIA.load(Ordering::Relaxed) } }
vmethod! { fn set_default_avatar_criteria(_this: usize, c: u32) { ours(); AVATAR_CRITERIA.store(c, Ordering::Relaxed) } }

vmethod! {
    fn avatar_url(this: usize, user: Gid, kind: u32) -> *const c_char {
        if peer_of(gid(user)).is_some() {
            ours();
            return c_str("");
        }
        real!(this, "GetFriendAvatarUrl", fn(Gid, u32) -> *const c_char, c_str(""), user, kind)
    }
}

vmethod! {
    fn avatar_url_copy(this: usize, user: Gid, kind: u32, buf: *mut c_char, len: u32) {
        if peer_of(gid(user)).is_some() {
            ours();
            return copy_out("", buf, len);
        }
        real!(this, "GetFriendAvatarUrlCopy", fn(Gid, u32, *mut c_char, u32) -> (), (), user, kind, buf, len)
    }
}

vmethod! {
    fn avatar_rgba(this: usize, user: Gid, kind: u32, buf: *mut u8, len: u32) {
        if peer_of(gid(user)).is_some() {
            ours();
            if !buf.is_null() {
                unsafe { std::ptr::write_bytes(buf, 0, len as usize) };
            }
            return;
        }
        real!(this, "GetFriendAvatarImageRGBA", fn(Gid, u32, *mut u8, u32) -> (), (), user, kind, buf, len)
    }
}

// LAN players are friends already; GOG friend invitations have nothing to do.

vmethod! {
    fn send_friend_invitation(_this: usize, user: Gid, listener: usize) {
        ours();
        let id = gid(user);
        notify(FRIEND_INVITATION_SEND, listener, move |l| virt!(l, 1, fn(Gid), by_value(&id)));
    }
}

vmethod! { fn request_invitation_list(_this: usize, listener: usize) { ours(); notify(FRIEND_INVITATION_LIST_RETRIEVE, listener, |l| virt!(l, 1, fn())) } }
vmethod! { fn request_sent_invitation_list(_this: usize, listener: usize) { ours(); notify(SENT_FRIEND_INVITATION_LIST_RETRIEVE, listener, |l| virt!(l, 1, fn())) } }
vmethod! { fn invitation_count(_this: usize) -> u32 { ours(); 0 } }

vmethod! {
    fn invitation_at(_this: usize, _index: u32, user: *mut u64, time: *mut u32) {
        ours();
        unsafe {
            if !user.is_null() {
                *user = 0;
            }
            if !time.is_null() {
                *time = 0;
            }
        }
    }
}

vmethod! {
    fn respond_to_invitation(_this: usize, user: Gid, accept: bool, listener: usize) {
        ours();
        let id = gid(user);
        notify(FRIEND_INVITATION_RESPOND_TO, listener, move |l| virt!(l, 1, fn(Gid, bool), by_value(&id), accept));
    }
}

vmethod! {
    fn delete_friend(_this: usize, user: Gid, listener: usize) {
        ours();
        let id = gid(user);
        notify(FRIEND_DELETE, listener, move |l| virt!(l, 1, fn(Gid), by_value(&id)));
    }
}

/// A LAN player by name (any case) or GalaxyID.
fn find(spec: &str) -> Option<u64> {
    let spec = spec.trim();
    if let Ok(id) = spec.parse::<u64>() {
        if peer_of(id).is_some() {
            return Some(id);
        }
    }
    if spec.eq_ignore_ascii_case(&my_name()) {
        return Some(my_id());
    }
    friends_list().into_iter().map(|(p, _)| galaxy_id(p)).find(|id| name_of(*id).is_some_and(|n| n.eq_ignore_ascii_case(spec)))
}

vmethod! {
    fn find_user(_this: usize, spec: *const c_char, listener: usize) {
        ours();
        let sp = s(spec);
        let found = find(&sp);
        let cs = owned(&sp);
        notify(USER_FIND, listener, move |l| match found {
            Some(id) => virt!(l, 1, fn(*const c_char, Gid), cs.as_ptr(), by_value(&id)),
            None => virt!(l, 2, fn(*const c_char, u32), cs.as_ptr(), USER_NOT_FOUND),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_rooms_are_the_same_on_both_sides() {
        assert_eq!(room_of(5, 9), room_of(9, 5));
        assert_ne!(room_of(5, 9), room_of(5, 10));
        assert_eq!(room_of(1, 2) & 1, 1);
    }

    #[test]
    fn id_tokens_are_jwts() {
        assert_eq!(b64url(b"Man"), "TWFu");
        assert_eq!(b64url(b"Ma"), "TWE");
        assert_eq!(b64url(b"M"), "TQ");
        let t = token("id");
        assert_eq!(t.matches('.').count(), 2);
    }

    #[test]
    fn stats_round_trip_through_their_file() {
        let dir = std::env::temp_dir().join(format!("peerparty-galaxy-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::set_var("PEERPARTY_GALAXY_DIR", &dir) };
        let mut st = Stats { loaded: true, ..Default::default() };
        st.ints.insert("kills".into(), 7);
        st.floats.insert("distance".into(), 1.5);
        st.rates.insert("rate".into(), (3.0, 2.0));
        st.achievements.insert("first blood".into(), 1234);
        st.achievements.insert("locked".into(), 0);
        assert!(save(&st));
        let mut back = Stats::default();
        load(&mut back);
        assert_eq!(back.ints, st.ints);
        assert_eq!(back.floats, st.floats);
        assert_eq!(back.rates, st.rates);
        assert_eq!(back.achievements, st.achievements);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_stay_one_path_component() {
        assert_eq!(safe("../save.dat").as_deref(), Some(".._save.dat"));
        assert_eq!(safe("a\\b:c").as_deref(), Some("a_b_c"));
        assert_eq!(safe(".."), None);
        assert_eq!(safe(""), None);
    }
}
