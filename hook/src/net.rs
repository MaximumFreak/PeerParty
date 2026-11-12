//! The process's one LAN engine, shared by every front end (EOS, Steam).
//!
//! The game key is the exe's name, not an SDK id: Steam and EOS front ends start the
//! engine in whatever order the game happens to call them, and copies on two PCs must
//! still agree on the key.
//!
//! Each front end drains its own event queue from its own tick, so SDK callbacks only
//! run where that SDK runs its own. Whoever ticks first moves new events from the core
//! into every front end's queue.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, OnceLock};

use peerparty_core::{Event, Lan};

use crate::log;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Front {
    Eos,
    Steam,
    /// PlayFab Party (party.rs), drained from the game's Party polling.
    Party,
    /// The local PlayFab service's matchmaker (playfab/api.rs), drained from its own thread.
    PlayFab,
    /// PlayFab Lobby & Matchmaking (pfmp.rs), drained from the game's lobby polling.
    PfLobby,
    /// GOG Galaxy (galaxy.rs), drained from the game's ProcessData.
    Galaxy,
    /// Unity Gaming Services' Lobby and Relay (ugs/), drained from their own thread.
    Ugs,
    /// Vivox voice chat (vivox.rs), drained from the game's message polling.
    Vivox,
}

impl Front {
    /// The service's name, for the overlay.
    pub fn label(self) -> &'static str {
        match self {
            Front::Eos => "Epic Online Services",
            Front::Steam => "Steam",
            Front::Party => "PlayFab Party",
            Front::PlayFab => "PlayFab matchmaking",
            Front::PfLobby => "PlayFab Lobby",
            Front::Galaxy => "GOG Galaxy",
            Front::Ugs => "Unity Gaming Services",
            Front::Vivox => "Vivox",
        }
    }
}

/// A front end's queue keeps at most this many events; the oldest go first.
const QUEUE_LIMIT: usize = 4096;

static LAN: OnceLock<Option<Lan>> = OnceLock::new();
static QUEUES: LazyLock<Mutex<HashMap<Front, VecDeque<Event>>>> = LazyLock::new(Mutex::default);
/// Each front end's part of our LAN presence ("steam", "eos").
static PRESENCE: Mutex<BTreeMap<&'static str, Vec<(String, String)>>> = Mutex::new(BTreeMap::new());

pub fn enabled() -> bool {
    std::env::var_os("PEERPARTY_ACTIVE").is_some()
}

fn core_log(line: &str) {
    log!("{line}");
}

/// Peers meet peers of the same game: `PEERPARTY_GAME`, or else its exe's name, less a
/// dedicated server's "_server"-style ending (mygame_server.exe meets mygame.exe,
/// MyGameServer-Win64-Shipping.exe meets MyGame-Win64-Shipping.exe).
pub(crate) fn game_key() -> String {
    let stem = exe_stem();
    let env = std::env::var("PEERPARTY_GAME").ok();
    let key = key_for(&stem, env.as_deref());
    if env.is_none_or(|e| e.trim().is_empty()) && key != format!("exe:{}", stem.to_ascii_lowercase()) {
        log!("lan game key {key} (a dedicated server's exe, {stem}, under its client's name; PEERPARTY_GAME overrides)");
    }
    key
}

fn exe_stem() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "game".into())
}

/// The game key for exe stem `stem` and `PEERPARTY_GAME` value `env`.
fn key_for(stem: &str, env: Option<&str>) -> String {
    if let Some(name) = env {
        let name = name.trim().trim_end_matches(".exe").to_ascii_lowercase();
        if !name.is_empty() {
            return format!("exe:{name}");
        }
    }
    let stem = stem.to_ascii_lowercase();
    match server_exe(&stem) {
        ServerExe::Of(client) => format!("exe:{client}"),
        _ => format!("exe:{stem}"),
    }
}

/// What an exe's name says about it being a dedicated server.
#[derive(Debug, PartialEq, Eq)]
enum ServerExe {
    /// Not a server's name.
    No,
    /// A server's, with its client's name: the server word taken out, the build tail
    /// kept (mygame_server -> mygame, MyGameServer-Win64-Shipping ->
    /// mygame-win64-shipping, MyGameServer_x64 -> mygame_x64).
    Of(String),
    /// A server's, with no client name in it (srcds, hlds, DedicatedServer).
    Only,
}

/// Build words after a name (Unreal's `-Win64-Shipping`, `-Cmd`, `_x64`,
/// `_nullrenderer`), looked past to find the server word before them.
const BUILD_TAIL: &[&str] =
    &["win64", "win32", "x64", "x86", "shipping", "cmd", "test", "development", "debuggame", "debug", "nullrenderer"];

/// Endings that make a name a server's, longest first.
const SERVER_WORD: &[&str] = &["dedicatedserver", "dedicated_server", "dedicated-server", "dedicated server", "dedicated", "server"];

/// Names that are a dedicated server's whole: Valve's Source and GoldSrc servers, and
/// servers named only for what they are.
const SERVER_ONLY: &[&str] = &["srcds", "hlds", "server", "dedicated", "dedicatedserver", "dedicated_server", "dedicated server"];

const SEPARATORS: [char; 4] = ['_', '-', ' ', '.'];

/// Reads a lowercase exe stem: the build tail is looked past, then a server word (or a
/// whole server name) decides. Too little left before the word (xserver, observer)
/// isn't a client's name, so not a server's either.
fn server_exe(stem: &str) -> ServerExe {
    // Peel build words off the end ("mygameserver-win64-shipping-cmd").
    let mut base = stem;
    while let Some(cut) = base.rfind(SEPARATORS) {
        if !BUILD_TAIL.contains(&&base[cut + 1..]) {
            break;
        }
        base = &base[..cut];
    }
    if SERVER_ONLY.contains(&base) {
        return ServerExe::Only;
    }
    // The tail goes back on the client's name, less a console build's "-cmd".
    let mut tail = String::new();
    let mut rest = &stem[base.len()..];
    while let Some(sep) = rest.chars().next() {
        let word_end = rest[1..].find(SEPARATORS).map_or(rest.len(), |i| i + 1);
        if &rest[1..word_end] != "cmd" {
            tail.push(sep);
            tail.push_str(&rest[1..word_end]);
        }
        rest = &rest[word_end..];
    }
    for word in SERVER_WORD {
        if let Some(before) = base.strip_suffix(word) {
            let client = before.trim_end_matches(SEPARATORS);
            return if client.len() >= 3 { ServerExe::Of(format!("{client}{tail}")) } else { ServerExe::No };
        }
    }
    ServerExe::No
}

/// The presence key a dedicated server says so in ("1"): the overlay shows it as a
/// server, not a player, and doesn't compare its build with a client's.
pub const SERVER_KEY: &str = "peerparty:server";

/// The presence key a game server's host names it in (its advertised name), which the
/// overlay shows in place of the host's player name for a dedicated server.
pub const SERVER_NAME_KEY: &str = "peerparty:server_name";

/// Whether this process is a dedicated server, decided when the LAN starts:
/// `PEERPARTY_DEDICATED` (1 or 0) says so; else it started a Steam game server without
/// starting Steam's client API (what a Steam dedicated server is, whatever its exe is
/// called: some games run their own exe as one, with a switch); else its exe's name does
/// (`server_exe`). A server takes a server identity slot, never a player's (core's
/// `Identity::claim_server`).
pub fn dedicated() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let stem = exe_stem();
        let env = std::env::var("PEERPARTY_DEDICATED").ok();
        let steam_alone = crate::steamgs::server_started() && !crate::steam::client_started();
        let (on, why) = dedicated_for(&stem, env.as_deref(), steam_alone);
        log!("lan: {} ({why})", if on { "a dedicated server" } else { "a player's copy" });
        on
    })
}

fn dedicated_for(stem: &str, env: Option<&str>, steam_alone: bool) -> (bool, &'static str) {
    match env.map(str::trim) {
        Some("1") => return (true, "PEERPARTY_DEDICATED=1"),
        Some("0") => return (false, "PEERPARTY_DEDICATED=0"),
        _ => {}
    }
    if steam_alone {
        return (true, "it started a Steam game server and no Steam client");
    }
    match server_exe(&stem.to_ascii_lowercase()) {
        ServerExe::No => (false, "neither its exe's name nor a Steam game server says server"),
        _ => (true, "its exe's name is a dedicated server's"),
    }
}

/// Whether a LAN peer is a dedicated server (its presence says so).
pub fn is_server_peer(peer: peerparty_core::PeerId) -> bool {
    lan_if_started().is_some_and(|l| l.presence(peer).iter().any(|(k, v)| k == SERVER_KEY && v == "1"))
}

/// Whether `PEERPARTY_SKIP` (comma-separated: swap, invite, eos, steam, party, pfmp,
/// playfab, photon, ugs, galaxy, dns, ipfwd, calls) turns off one of PeerParty's layers, for a game that objects to it.
pub fn skip(layer: &str) -> bool {
    static SKIP: OnceLock<Vec<String>> = OnceLock::new();
    SKIP.get_or_init(|| {
        let list: Vec<String> = std::env::var("PEERPARTY_SKIP")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if !list.is_empty() {
            log!("skipping peerparty layers: {}", list.join(", "));
        }
        list
    })
    .iter()
    .any(|s| s == layer)
}

/// The engine, started on first use (only in a game started through PeerParty).
/// The game is exiting: deletes the router port mappings internet play made (their
/// leases would end them anyway). Starts nothing that isn't running.
pub fn on_exit() {
    if let Some(Some(l)) = LAN.get() {
        l.release_port_maps();
    }
}

pub fn lan() -> Option<&'static Lan> {
    if !enabled() {
        return None;
    }
    LAN.get_or_init(|| match Lan::global(&game_key(), dedicated(), core_log) {
        Ok(l) => {
            if l.server() {
                log!("lan: this copy takes a server slot of its own, not a player's");
                PRESENCE.lock().unwrap().insert("server", vec![(SERVER_KEY.into(), "1".into())]);
            }
            crate::saves::learn_lan(&l);
            crate::saves::adopt_earlier_ids(&l);
            let build = build_id();
            log!("lan: this copy's game build is {build}");
            PRESENCE.lock().unwrap().insert("build", vec![(BUILD_KEY.into(), build)]);
            let version = version();
            log!("lan: this copy is peerparty {version}");
            PRESENCE.lock().unwrap().insert("version", vec![(VERSION_KEY.into(), version)]);
            l.set_presence(PRESENCE.lock().unwrap().values().flatten().cloned().collect());
            std::thread::spawn(|| {
                for wait in [0, 3, 10] {
                    std::thread::sleep(std::time::Duration::from_secs(wait));
                    if connect_patched_by().is_some() {
                        break;
                    }
                }
            });
            Some(l)
        }
        Err(e) => {
            log!("lan could not start: {e}");
            None
        }
    })
    .as_ref()
}

/// Another module in the game that has patched Winsock's `connect` (some builds block
/// or redirect the game's network that way): PeerParty's own LAN connections go
/// through it too and may fail, so it's named once as a gap.
fn connect_patched_by() -> Option<String> {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
        GetModuleHandleExW, GetModuleHandleW, GetProcAddress,
    };
    let ws2: Vec<u16> = "ws2_32.dll\0".encode_utf16().collect();
    let module = unsafe { GetModuleHandleW(ws2.as_ptr()) };
    if module.is_null() {
        return None;
    }
    let f = unsafe { GetProcAddress(module, c"connect".as_ptr().cast()) }? as usize;
    // A hotpatch prologue is often skipped (mov edi,edi): the jump is at the start.
    let code = unsafe { std::slice::from_raw_parts(f as *const u8, 6) };
    let target = match code[0] {
        0xE9 => f.wrapping_add(5).wrapping_add(i32::from_le_bytes(code[1..5].try_into().ok()?) as isize as usize),
        _ => return None,
    };
    let owner = |addr: usize| {
        let mut m = std::ptr::null_mut();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        (unsafe { GetModuleHandleExW(flags, addr as *const u16, &mut m) } != 0).then_some(m as usize)
    };
    let by = owner(target);
    let ours = by.is_some() && by == owner(connect_patched_by as *const () as usize);
    if crate::ipfwd::hooks_connect() || ours || by == Some(module as usize) {
        return None;
    }
    // The patch's code is often in memory of its own, outside any module.
    let name = match by {
        Some(by) => {
            let mut buf = [0u16; 260];
            let len = unsafe { GetModuleFileNameW(by as _, buf.as_mut_ptr(), buf.len() as u32) } as usize;
            let path = String::from_utf16_lossy(&buf[..len]);
            path.rsplit('\\').next().unwrap_or(&path).to_string()
        }
        None => "code outside any module".to_string(),
    };
    crate::log::gap(
        "network",
        &format!("Winsock's connect is patched by {name}: PeerParty's LAN connections may fail while it blocks or redirects them"),
    );
    Some(name)
}

/// The engine if something already started it (never starts it).
pub fn lan_if_started() -> Option<&'static Lan> {
    LAN.get().and_then(|l| l.as_ref())
}

/// The front ends that have subscribed: the SDKs the game uses through PeerParty.
pub fn fronts() -> Vec<Front> {
    let mut f: Vec<Front> = QUEUES.lock().unwrap().keys().copied().collect();
    f.sort_by_key(|f| format!("{f:?}"));
    f
}

/// The presence key carrying each copy's game build (`build_id`).
pub const BUILD_KEY: &str = "peerparty:build";

/// The presence key carrying each copy's PeerParty version (`version`). Only logged:
/// the wire protocol's own version (the handshake) decides who can talk.
pub const VERSION_KEY: &str = "peerparty:version";

/// Which PeerParty this is: the launcher's version and commit (`PEERPARTY_VERSION`,
/// which it sets for the game), else the hook's package version.
pub fn version() -> String {
    std::env::var("PEERPARTY_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Logs whether a peer runs the same PeerParty as us, so a log shows mismatched copies:
/// when it first says, and the few times it says something else (a version doesn't
/// change while a copy runs; a peer that keeps changing it gets no more lines).
fn check_version(lan: &Lan, peer: peerparty_core::PeerId) {
    static SEEN: Mutex<Vec<Said>> = Mutex::new(Vec::new());
    let theirs = lan.presence(peer).into_iter().find(|(k, _)| k == VERSION_KEY).map(|(_, v)| v);
    let here = lan.peers().iter().any(|(p, _)| *p == peer);
    if !says_version(here, theirs.as_deref()) {
        return;
    }
    if !note_version(&mut SEEN.lock().unwrap(), peer, &theirs) {
        return;
    }
    let mine = version();
    match theirs {
        Some(t) if t == mine => log!("lan: {peer} runs the same peerparty ({t})"),
        Some(t) => log!("lan: {peer} runs a different peerparty: {t} (this copy: {mine})"),
        None => log!("lan: {peer} doesn't say its peerparty version (older than this copy: {mine})"),
    }
}

/// What a peer last said its version was, and how many different things it has said.
struct Said {
    peer: peerparty_core::PeerId,
    version: Option<String>,
    times: u32,
}

/// Versions logged per peer, and peers remembered (the oldest are forgotten).
const VERSION_LINES: u32 = 4;
const VERSION_PEERS: usize = 1024;

/// Records what `peer` says its version is. Whether to log it: it's new for that peer,
/// and the peer hasn't used up its lines.
fn note_version(seen: &mut Vec<Said>, peer: peerparty_core::PeerId, theirs: &Option<String>) -> bool {
    if let Some(s) = seen.iter_mut().find(|s| s.peer == peer) {
        if s.version == *theirs {
            return false;
        }
        s.version = theirs.clone();
        s.times = s.times.saturating_add(1);
        return s.times <= VERSION_LINES;
    }
    if seen.len() >= VERSION_PEERS {
        seen.remove(0);
    }
    seen.push(Said { peer, version: theirs.clone(), times: 1 });
    true
}

/// Whether a presence change tells us anything about a peer's version. A peer that left
/// has its presence cleared (a `PresenceChanged` comes just before `PeerLeft`), which
/// isn't the peer saying no version: only one still connected can be "older".
fn says_version(connected: bool, theirs: Option<&str>) -> bool {
    connected || theirs.is_some()
}

/// A fingerprint of the game's version: the sizes of the files that change between
/// versions (the exe, and for Unity games GameAssembly.dll / Assembly-CSharp.dll, whose
/// player exe never changes). Sizes, not dates, so copies of one install agree.
pub fn build_id() -> String {
    let Ok(exe) = std::env::current_exe() else { return String::new() };
    let dir = exe.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let files = [
        exe.clone(),
        dir.join("GameAssembly.dll"),
        dir.join(format!("{stem}_Data")).join("Managed").join("Assembly-CSharp.dll"),
    ];
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for f in files {
        let len = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        for b in len.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// Whether a peer runs the same game build (unknown counts as the same: an older
/// PeerParty doesn't say). A dedicated server and its clients are different programs,
/// so their builds are never compared.
pub fn same_build(peer: peerparty_core::PeerId) -> bool {
    let Some(lan) = lan_if_started() else { return true };
    if lan.server() || is_server_peer(peer) {
        return true;
    }
    let theirs = lan.presence(peer).into_iter().find(|(k, _)| k == BUILD_KEY).map(|(_, v)| v);
    let mine = PRESENCE.lock().unwrap().get("build").and_then(|v| v.first().map(|(_, b)| b.clone()));
    match (theirs, mine) {
        (Some(t), Some(m)) => t == m,
        _ => true,
    }
}

/// Replaces one front end's part of our LAN presence; the LAN gets all of them.
pub fn set_presence(source: &'static str, kv: Vec<(String, String)>) {
    let all: Vec<(String, String)> = {
        let mut p = PRESENCE.lock().unwrap();
        p.insert(source, kv);
        p.values().flatten().cloned().collect()
    };
    if let Some(lan) = lan() {
        lan.set_presence(all);
    }
}

/// Front ends that want events. Events reach only the queues that exist.
pub fn subscribe(front: Front) {
    QUEUES.lock().unwrap().entry(front).or_default();
}

/// Ticks the engine and hands `front` its events, oldest first.
pub fn take(front: Front) -> Vec<Event> {
    let Some(lan) = LAN.get().and_then(|l| l.as_ref()) else {
        return Vec::new();
    };
    let mut q = QUEUES.lock().unwrap();
    tick_into(lan, &mut q);
    q.get_mut(&front)
        .map(|q| q.drain(..).collect())
        .unwrap_or_default()
}

/// Ticks the engine and takes from `front`'s queue the first event `want` picks,
/// leaving the rest in place for whoever waits on them. For a queue several waiters
/// share, each on its own thread (PlayFab's web API serves each request on one), where
/// `take` would hand one waiter, and so lose, the others' answers. Events neither
/// `want` nor `others` cares for are dropped; of those `others` keeps, the newest
/// `SHARED_KEEP` stay (an answer nobody waits for any more goes in time).
pub fn take_one(front: Front, want: impl Fn(&Event) -> bool, others: impl Fn(&Event) -> bool) -> Option<Event> {
    let lan = LAN.get().and_then(|l| l.as_ref())?;
    let mut q = QUEUES.lock().unwrap();
    tick_into(lan, &mut q);
    pick(q.get_mut(&front)?, want, others)
}

const SHARED_KEEP: usize = 64;

fn pick(q: &mut VecDeque<Event>, want: impl Fn(&Event) -> bool, others: impl Fn(&Event) -> bool) -> Option<Event> {
    let found = q.iter().position(&want).and_then(|i| q.remove(i));
    q.retain(|e| others(e));
    while q.len() > SHARED_KEEP {
        q.pop_front();
    }
    found
}

/// Ticks the engine and hands its events to the front ends, without being one (the
/// typed-IP forwarder, ipfwd.rs, runs a game with no SDK this way).
pub fn pump() {
    let Some(lan) = LAN.get().and_then(|l| l.as_ref()) else {
        return;
    };
    let mut q = QUEUES.lock().unwrap();
    tick_into(lan, &mut q);
}

fn tick_into(lan: &Lan, q: &mut HashMap<Front, VecDeque<Event>>) {
    lan.tick();
    let fresh = lan.poll();
    for ev in fresh {
        crate::ipfwd::event(&ev);
        if let Event::PresenceChanged(peer) = &ev {
            check_version(lan, *peer);
        }
        if let Event::Invite { from, connect, lobby } = &ev {
            if connect == crate::invite::JOIN_REQUEST {
                crate::invite::join_requested(*from);
                continue;
            }
            if let Some(code) = connect.strip_prefix(crate::invite::CODE_PREFIX) {
                crate::invite::code_arrived(*from, code);
                continue;
            }
            if already_there(connect, lobby.as_ref()) {
                continue;
            }
            crate::invite::arrived(&ev);
        }
        distribute(q, ev);
    }
}

fn distribute(q: &mut HashMap<Front, VecDeque<Event>>, ev: Event) {
    let fronts: Vec<Front> = q.keys().copied().collect();
    for f in &fronts {
        if wants(*f, &ev, &fronts) {
            let queue = q.get_mut(f).unwrap();
            if queue.len() >= QUEUE_LIMIT {
                queue.pop_front();
            }
            queue.push_back(ev.clone());
        }
    }
}

/// Hands an event to the front ends again: an invite the player accepted in the
/// overlay (invite.rs). They pick it up on their next tick.
pub fn redeliver(ev: Event) {
    let mut q = QUEUES.lock().unwrap();
    distribute(&mut q, ev);
}

/// An invite to a lobby we're already in, or to the place our own rich presence
/// already points at, is dropped: games tend to leave and rejoin on it.
fn already_there(connect: &str, lobby: Option<&peerparty_core::Lobby>) -> bool {
    let Some(lan) = LAN.get().and_then(|l| l.as_ref()) else {
        return false;
    };
    let mut mine = lan.my_lobbies("eos.lobby");
    mine.extend(lan.my_lobbies("eos.session"));
    mine.extend(lan.my_lobbies(crate::steammm::NS));
    mine.extend(lan.my_lobbies(crate::pfmp::NS));
    mine.extend(lan.my_lobbies(crate::galaxy::NS));
    let in_lobby = lobby.is_some_and(|l| mine.iter().any(|m| m.id == l.id));
    let in_connect = !connect.is_empty()
        && (crate::steamlan::my_connect() == connect
            || mine.iter().any(|m| m.id == connect)
            || crate::pfmp::current_connect().is_some_and(|c| c == connect)
            || crate::galaxy::my_connect().is_some_and(|c| c == connect));
    if in_lobby || in_connect {
        log!("invite ignored: already there ({connect})");
    }
    in_lobby || in_connect
}

/// An invite is accepted through one SDK only, or the game would join twice: Steam
/// for a Steam lobby, or a Steam connect string the game listens for, else EOS: its
/// lobby or session invite, or a presence "join game" with the join info.
fn wants(front: Front, ev: &Event, fronts: &[Front]) -> bool {
    let Event::Invite { connect, lobby, .. } = ev else {
        return true;
    };
    if already_there(connect, lobby.as_ref()) {
        return false;
    }
    // EOS custom invites and requests to join are the EOS front end's alone.
    if crate::eos::is_custom_invite(connect) {
        return front == Front::Eos;
    }
    // A PlayFab lobby's connection string belongs to the PlayFab Multiplayer library.
    if connect.starts_with(crate::pfmp::CONNECT_PREFIX) && fronts.contains(&Front::PfLobby) {
        return front == Front::PfLobby;
    }
    let ns = lobby.as_ref().map(|l| l.namespace.as_str());
    let steam = fronts.contains(&Front::Steam)
        && (ns == Some(crate::steammm::NS)
            || (!connect.is_empty() && crate::steamlan::takes_joins()));
    // A connection string goes to Galaxy when Steam doesn't take it and the game
    // listens for Galaxy's join requests.
    let galaxy = !steam && fronts.contains(&Front::Galaxy) && ns.is_none() && !connect.is_empty() && crate::galaxy::takes_joins();
    match front {
        Front::Steam => steam,
        Front::Galaxy => galaxy,
        Front::Eos => {
            !steam
                && !galaxy
                && match ns {
                    Some(n) => n == "eos.lobby" || n == "eos.session",
                    None => !connect.is_empty() && crate::eos::takes_presence_joins(),
                }
        }
        Front::Party | Front::PlayFab | Front::PfLobby | Front::Ugs | Front::Vivox => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Said, ServerExe, VERSION_LINES, VERSION_PEERS, dedicated_for, key_for, note_version, says_version, server_exe};

    /// Two waiters on one queue: each takes its own answer and leaves the other's.
    #[test]
    fn a_shared_queue_keeps_the_other_waiters_answers() {
        use peerparty_core::{Event, JoinResult, PeerId};
        use std::collections::VecDeque;
        let search = |n: u64| Event::SearchDone { search: n, results: Vec::new() };
        let join = |l: &str| Event::JoinDone { lobby: l.into(), result: JoinResult::Ok };
        let others = |e: &Event| matches!(e, Event::SearchDone { .. } | Event::JoinDone { .. });
        let mut q: VecDeque<Event> = VecDeque::from([search(1), Event::PresenceChanged(PeerId([1; 16])), join("x"), search(2)]);
        let mine = super::pick(&mut q, |e| matches!(e, Event::SearchDone { search: 2, .. }), others);
        assert!(matches!(mine, Some(Event::SearchDone { search: 2, .. })));
        assert_eq!(q.len(), 2, "the other search's answer and the join's stay; the rest goes");
        let theirs = super::pick(&mut q, |e| matches!(e, Event::JoinDone { lobby, .. } if lobby == "x"), others);
        assert!(theirs.is_some());
        assert!(super::pick(&mut q, |e| matches!(e, Event::JoinDone { .. }), others).is_none());
        // Answers nobody takes don't pile up.
        for n in 0..1000 {
            q.push_back(search(100 + n));
        }
        assert!(super::pick(&mut q, |_| false, others).is_none());
        assert_eq!(q.len(), super::SHARED_KEEP);
        assert!(matches!(q.back(), Some(Event::SearchDone { search: 1099, .. })));
    }

    #[test]
    fn version_lines_are_bounded() {
        let mut seen: Vec<Said> = Vec::new();
        let p = peerparty_core::PeerId([1; 16]);
        let v = |s: &str| Some(s.to_string());
        assert!(note_version(&mut seen, p, &v("1")));
        assert!(!note_version(&mut seen, p, &v("1")), "the same again says nothing new");
        assert!(note_version(&mut seen, p, &v("2")));
        // A peer that keeps changing it stops being logged, and takes no more room.
        let logged = (0..1000).filter(|i| note_version(&mut seen, p, &v(&format!("x{i}")))).count();
        assert_eq!(logged as u32, VERSION_LINES - 2);
        assert_eq!(seen.len(), 1);
        // Many peers: the oldest are forgotten.
        for i in 0..(VERSION_PEERS as u32 + 10) {
            let mut id = [2u8; 16];
            id[..4].copy_from_slice(&i.to_le_bytes());
            note_version(&mut seen, peerparty_core::PeerId(id), &None);
        }
        assert_eq!(seen.len(), VERSION_PEERS);
    }

    #[test]
    fn a_dedicated_servers_exe_meets_its_clients() {
        assert_eq!(key_for("mygame_server", None), "exe:mygame");
        assert_eq!(key_for("MyGame", None), "exe:mygame");
        assert_eq!(key_for("OtherGameServer", None), "exe:othergame");
        assert_eq!(key_for("MyGame-Server", None), "exe:mygame");
        assert_eq!(key_for("Game_DedicatedServer", None), "exe:game");
        // Too little left to be a client's name: kept whole.
        assert_eq!(key_for("server", None), "exe:server");
        assert_eq!(key_for("xserver", None), "exe:xserver");
        assert_eq!(key_for("SomeLongGameName", None), "exe:somelonggamename");
        // A build tail stays on the client's name, so Unreal and 64-bit builds meet.
        assert_eq!(key_for("HammerfallServer-Win64-Shipping", None), "exe:hammerfall-win64-shipping");
        assert_eq!(key_for("Hammerfall-Win64-Shipping", None), "exe:hammerfall-win64-shipping");
        assert_eq!(key_for("DuskvaleServer_x64", None), "exe:duskvale_x64");
        assert_eq!(key_for("orbit3server_x64", None), "exe:orbit3_x64");
        assert_eq!(key_for("ForgeServer-Win64-Shipping-Cmd", None), "exe:forge-win64-shipping");
        // A game's own build tail with no server word is left alone.
        assert_eq!(key_for("Game-Win64-Shipping", None), "exe:game-win64-shipping");
        // PEERPARTY_GAME wins, as before; blank is unset.
        assert_eq!(key_for("mygame_server", Some("MyGame.exe")), "exe:mygame");
        assert_eq!(key_for("mygame_server", Some("other ")), "exe:other");
        assert_eq!(key_for("mygame_server", Some("  ")), "exe:mygame");
    }

    /// The shapes Windows dedicated servers' exes are named in (made-up games): a
    /// "server" or "dedicated" word before any build tail, launchers and their -Cmd
    /// builds, Valve's servers; and clients that must not read as servers.
    #[test]
    fn dedicated_server_exe_shapes_read_as_servers() {
        let servers = [
            "mygame_server", "ember_server", "hollow_server", "trucker_server", "trucker2_server",
            "SkyGameServer", "SkyAscendedServer", "NightfallServer", "9DaysServer", "TerraformServer",
            "CaveServer", "VoidServer", "EchoServer", "OutpostServer", "QRXServer", "FjordServer",
            "TeamGameServer", "PostwarServer", "OrbitReforgerServer", "XYZGameServer", "orbit3server",
            "orbit3server_x64", "DuskvaleServer_x64", "IronDedicated", "StarEngineersDedicated",
            "CastleEngineersDedicated", "EmpireDedicated", "TheGroveDedicatedServer",
            "shipyard_DedicatedServer", "dontsleep_dedicated_server_nullrenderer_x64",
            "PalaceServer", "PalaceServer-Win64-Shipping-Cmd", "PalaceServer-Win64-Test-Cmd",
            "SandboxServer-Win64-Shipping", "ForgeServer-Win64-Shipping-Cmd", "AstralServer-Win64-Shipping",
            "MinesServer-Win64-Shipping", "HammerfallServer-Win64-Shipping", "SiegeServer-Win64-Shipping",
            "LabFactorServer-Win64-Shipping", "WingsServer-Win64-Shipping", "IsleServer-Win64-Shipping",
            "TinylandServer-Win64-Shipping", "FogServer-Win64-Shipping", "FieldOpsServer-Win64-Shipping",
            "PathOfBeastsServer-Win64-Shipping", "srcds", "srcds_win64", "srcds_x64", "hlds", "DedicatedServer",
            "Server",
        ];
        for s in servers {
            assert_ne!(server_exe(&s.to_ascii_lowercase()), ServerExe::No, "{s}");
        }
        let clients = [
            "mygame", "SkyGame", "IronClient", "Palace-Win64-Shipping", "ForgeGame-Win64-Shipping",
            "game2", "hlgame", "game_win64", "Wanderer", "Duskvale_x64", "orbit3_x64", "Observer", "xserver",
            "StarEngineers", "Game-Win64-Test", "LateShift",
        ];
        for c in clients {
            assert_eq!(server_exe(&c.to_ascii_lowercase()), ServerExe::No, "{c}");
        }
    }

    #[test]
    fn a_dedicated_server_is_told_by_steam_its_exe_or_the_variable() {
        assert!(dedicated_for("mygame_server", None, false).0);
        assert!(!dedicated_for("mygame", None, false).0);
        // A Steam game server with no Steam client: a server, whatever the exe's name.
        assert!(dedicated_for("Wanderer", None, true).0);
        assert!(dedicated_for("game2", None, true).0);
        // PEERPARTY_DEDICATED says either way; anything else leaves it to the rest.
        assert!(dedicated_for("srv", Some("1"), false).0);
        assert!(!dedicated_for("mygame_server", Some(" 0 "), true).0);
        assert!(dedicated_for("mygame_server", Some("yes"), false).0);
    }

    #[test]
    fn a_peer_leaving_isnt_an_old_version() {
        // Leaving clears presence: no version, but not connected either.
        assert!(!says_version(false, None));
        // Connected and silent: a genuinely older copy.
        assert!(says_version(true, None));
        assert!(says_version(true, Some("0.1.0.8")));
        assert!(says_version(false, Some("0.1.0.8")));
    }
}
