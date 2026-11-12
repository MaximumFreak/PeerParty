//! PeerParty's own EOS layer (on in every game started by peerparty, which sets PEERPARTY_ACTIVE).
//!
//! The game keeps its own EOS SDK loaded and running; PeerParty takes over the
//! multiplayer surface by name (see swap.rs):
//!
//! - Connect, Auth, UserInfo, Friends, ids, Lobby (+ Search / Details / Modification,
//!   invites), Sessions (the same, plus active sessions and registered players),
//!   Presence, CustomInvites, P2P and voice (RTC, RTCAudio, RTCData, the lobby's RTC
//!   room; rtc.rs over voice.rs) run on the shared LAN core; Title and Player Data
//!   Storage are answered from disk (storage.rs). Everything else that could see a
//!   PeerParty id fails fast with the SDK's own "no connection" result (see fail.rs,
//!   table.rs).
//! - Platform, logging and string helpers stay with the real SDK. Its
//!   `EOS_Platform_Create` starts the LAN engine and its `EOS_Platform_Tick` runs
//!   PeerParty's callbacks, on the game's thread, after the SDK's own.

mod abi;
mod custominvites;
mod fail;
mod ids;
mod lobby;
mod p2p;
mod presence;
mod rtc;
mod sessions;
pub(crate) mod storage;
mod table;
mod ui;
mod users;

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use peerparty_core::{Event, Lan, Lobby, PeerId};
use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::net::{self, Front};
use crate::pe::Export;

pub fn enabled() -> bool {
    net::enabled()
}

pub(crate) fn lan() -> Option<&'static Lan> {
    net::lan()
}

// ---- callbacks, always run from EOS_Platform_Tick ----

type Later = Box<dyn FnOnce() + Send>;
static LATER: Mutex<Vec<Later>> = Mutex::new(Vec::new());

/// Runs `f` on the next tick, the way EOS completes every async call.
pub(crate) fn later(f: impl FnOnce() + Send + 'static) {
    LATER.lock().unwrap().push(Box::new(f));
}

static DUE: Mutex<Vec<(Instant, Later)>> = Mutex::new(Vec::new());

/// Runs `f` on the first tick after `wait`. For calls a LAN finishes so much faster than
/// Epic's servers that games racing their own UI against them break.
pub(crate) fn later_after(wait: Duration, f: impl FnOnce() + Send + 'static) {
    DUE.lock().unwrap().push((Instant::now() + wait, Box::new(f)));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Registered but never fired (overlay, expiry, friend requests...).
    Never,
    ConnectLoginStatus,
    AuthLoginStatus,
    LobbyUpdate,
    LobbyMemberUpdate,
    LobbyMemberStatus,
    LobbyInviteReceived,
    LobbyInviteAccepted,
    JoinLobbyAccepted,
    SessionInviteReceived,
    SessionInviteAccepted,
    JoinSessionAccepted,
    PresenceChanged,
    JoinGameAccepted,
    P2PRequest,
    P2PEstablished,
    P2PClosed,
    P2PInterrupted,
    CustomInviteReceived,
    CustomInviteAccepted,
    CustomInviteRejected,
    RequestToJoinReceived,
    RequestToJoinAccepted,
    RequestToJoinRejected,
    RequestToJoinResponse,
    RtcParticipantStatus,
    RtcAudioParticipant,
    RtcBeforeRender,
    RtcData,
    RtcDataParticipant,
    LobbyRtcConnection,
    /// EOS_RTCAudio's AudioDevicesChanged: Windows' sound devices changed.
    RtcAudioDevices,
    /// EOS_UI's display settings: PeerParty's overlay opening and closing.
    DisplaySettings,
}

struct Listener {
    kind: Kind,
    func: usize,
    client_data: usize,
    socket: Option<String>,
}

static LISTENERS: LazyLock<Mutex<BTreeMap<u64, Listener>>> = LazyLock::new(Mutex::default);

pub(crate) fn add_listener(
    kind: Kind,
    func: usize,
    client_data: usize,
    socket: Option<String>,
) -> u64 {
    if func == 0 {
        return 0;
    }
    let id = fail::NEXT_NOTIFY.fetch_add(1, Ordering::Relaxed);
    LISTENERS.lock().unwrap().insert(
        id,
        Listener {
            kind,
            func,
            client_data,
            socket,
        },
    );
    id
}

pub(crate) fn remove_listener(id: u64) {
    LISTENERS.lock().unwrap().remove(&id);
}

/// (function, client data) for each listener of `kind` that wants `socket`.
pub(crate) fn listeners(kind: Kind, socket: Option<&str>) -> Vec<(usize, usize)> {
    LISTENERS
        .lock()
        .unwrap()
        .values()
        .filter(|l| l.kind == kind)
        .filter(|l| match (&l.socket, socket) {
            (Some(want), Some(have)) => want == have,
            _ => true,
        })
        .map(|l| (l.func, l.client_data))
        .collect()
}

/// Calls an SDK-style callback with one info struct.
pub(crate) unsafe fn call<T>(func: usize, info: &T) {
    if func != 0 {
        let f: unsafe extern "system" fn(*const T) = unsafe { std::mem::transmute(func) };
        unsafe { f(info) };
    }
}

// ---- structs handed out by Copy* functions, owned until their Release ----

struct Owned {
    /// Bytes from the start the value covers.
    len: usize,
    _value: Box<dyn std::any::Any>,
    _strings: Vec<std::ffi::CString>,
}

// The values are plain C structs that only this registry touches.
unsafe impl Send for Owned {}

/// Handed-out values by start address.
static OWNED: LazyLock<Mutex<BTreeMap<usize, Owned>>> = LazyLock::new(Mutex::default);

/// Keeps `value` (and the strings it points into) alive until `release`. The game may
/// be given a field inside it (an attribute with its data beside it), so `release`
/// takes any address within the value.
pub(crate) fn hand_out<T: 'static>(value: T, strings: Vec<std::ffi::CString>) -> usize {
    let b = Box::new(value);
    let p = &*b as *const T as usize;
    OWNED.lock().unwrap().insert(
        p,
        Owned {
            len: std::mem::size_of::<T>().max(1),
            _value: b,
            _strings: strings,
        },
    );
    p
}

/// Frees the handed-out value `p` points into (the game's `*_Release`).
pub(crate) fn release(p: usize) {
    let mut owned = OWNED.lock().unwrap();
    let start = owned.range(..=p).next_back().filter(|(s, o)| p < **s + o.len).map(|(s, _)| *s);
    if let Some(s) = start {
        owned.remove(&s);
    }
}

/// `s` cut to at most `max` bytes at a character boundary: what another player sent,
/// held to the SDK's documented limit before a game, which sized its buffers by that
/// limit, gets it.
pub(crate) fn clamp(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A C string for a struct we hand out, kept in `keep` until the struct is released.
pub(crate) fn owned_str(keep: &mut Vec<std::ffi::CString>, s: &str) -> *const std::ffi::c_char {
    let c = std::ffi::CString::new(s.replace('\0', "")).unwrap();
    let p = c.as_ptr();
    keep.push(c);
    p
}

/// Throttled per-name call logging for PeerParty's own EOS functions.
pub(crate) fn trace(name: &str, detail: impl FnOnce() -> String) {
    crate::log::throttled(name, 4, |n| log!("eos-lan call {name} n={n} {}", detail()));
}

static TICKS: AtomicU32 = AtomicU32::new(0);

fn pump() {
    if TICKS.fetch_add(1, Ordering::Relaxed) % 256 == 0 {
        crate::swap::patch_imports();
    }
    run_later();
    if let Some(lan) = lan() {
        for ev in net::take(Front::Eos) {
            dispatch(lan, ev);
        }
        rtc::pump(lan);
    }
    run_later();
}

fn run_later() {
    {
        let now = Instant::now();
        let mut due = DUE.lock().unwrap();
        let mut ready = LATER.lock().unwrap();
        let mut i = 0;
        while i < due.len() {
            if due[i].0 <= now {
                ready.push(due.remove(i).1);
            } else {
                i += 1;
            }
        }
    }
    // A callback may queue more; bound the rounds so a loop can't stall the frame.
    for _ in 0..8 {
        let batch = std::mem::take(&mut *LATER.lock().unwrap());
        if batch.is_empty() {
            return;
        }
        for f in batch {
            f();
        }
    }
}

fn dispatch(lan: &Lan, ev: Event) {
    match &ev {
        Event::PeerJoined(p) => log!("eos-lan peer joined {p}"),
        Event::PeerLeft(p) => log!("eos-lan peer left {p}"),
        _ => {}
    }
    lobby::on_event(lan, &ev);
    sessions::on_event(lan, &ev);
    presence::on_event(lan, &ev);
    custominvites::on_event(lan, &ev);
    rtc::on_event(lan, &ev);
    p2p::on_event(lan, &ev);
}

/// The EOS lobby or session we're in, for "invite everyone to where I am".
pub fn current_lobby() -> Option<Lobby> {
    lan()?
        .my_lobbies("eos.lobby")
        .into_iter()
        .next()
        .or_else(sessions::current)
}

/// Our EOS presence join info, for invites from games that join through presence.
pub fn join_info() -> String {
    presence::my_join_info()
}

/// Whether a LAN invite is an EOS custom invite or request to join (custominvites.rs).
pub fn is_custom_invite(connect: &str) -> bool {
    custominvites::is_ours(connect)
}

/// Whether a LAN invite waits for the player's answer: everything but the answer to a
/// request to join (which is a reply, not an invite).
pub fn needs_answer(connect: &str) -> bool {
    !custominvites::is_response(connect)
}

/// Whether a LAN invite is an EOS request to join us.
pub fn is_request_to_join(connect: &str) -> bool {
    custominvites::is_request(connect)
}

/// The player declined a request to join in the overlay.
pub fn reject_request_to_join(from: PeerId) {
    custominvites::decline_request(from);
}

/// PeerParty's overlay opened or closed: games listening for EOS's display settings
/// hear it as Epic's overlay.
pub fn overlay_visible(on: bool) {
    if enabled() {
        ui::overlay_visible(on);
    }
}

/// Whether the game takes a "join game" through EOS presence.
pub fn takes_presence_joins() -> bool {
    presence::takes_joins()
}

static PRODUCT: OnceLock<String> = OnceLock::new();

/// The EOS product id the game created its platform with.
pub(crate) fn product_id() -> String {
    PRODUCT.get().cloned().unwrap_or_default()
}

// ---- which function answers a name ----

/// `EOS_AntiCheatClient_BeginSession` / `EOS_AntiCheatServer_BeginSession` (handle,
/// options): never returns.
unsafe extern "system" fn anticheat_begin_session(_h: usize, _o: usize) -> i32 {
    crate::watch::stop("the game started an EOS Anti-Cheat session")
}

/// PeerParty's function for an EOS export, or `None` to let the real one answer.
pub(crate) fn resolve(name: &str) -> Option<usize> {
    // Unity also probes native plugins for its own entry points; those aren't ours.
    if !name.starts_with("EOS_") {
        return None;
    }
    // A game that starts an EOS Anti-Cheat session uses anti-cheat: it is stopped, as when
    // an EasyAntiCheat module loads. The SDK exports these functions in every game, so
    // only the session start counts, not the export or the interface.
    if matches!(name, "EOS_AntiCheatClient_BeginSession" | "EOS_AntiCheatServer_BeginSession") {
        note(name, "stops the game (anti-cheat)");
        return Some(anticheat_begin_session as *const () as usize);
    }
    // rtc.rs answers the lobby's RTC room functions, so it comes before lobby.rs.
    if let Some(f) = users::resolve(name)
        .or_else(|| rtc::resolve(name))
        .or_else(|| lobby::resolve(name))
        .or_else(|| sessions::resolve(name))
        .or_else(|| presence::resolve(name))
        .or_else(|| custominvites::resolve(name))
        .or_else(|| ui::resolve(name))
        .or_else(|| p2p::resolve(name))
        .or_else(|| storage::resolve(name))
    {
        note(name, "lan");
        return Some(f);
    }
    let entry = table::TABLE
        .binary_search_by(|(n, _, _)| (*n).cmp(name))
        .ok()
        .map(|i| table::TABLE[i]);
    let (class, slots) = match entry {
        Some((_, class, slots)) => (class, slots),
        // Newer than the table: guess from the name, else let the SDK answer.
        None if name.contains("_AddNotify") => (fail::Class::AddNotify, 4),
        None if name.contains("_RemoveNotify") => (fail::Class::Void, 3),
        None if name.ends_with("_Release") => (fail::Class::Void, 1),
        None => {
            note(name, "unknown, passed to the SDK");
            return None;
        }
    };
    let f = fail::handler(class, slots).map(|h| fail::noted(name, h));
    if f.is_some() {
        note(name, &format!("fails fast ({class:?})"));
    }
    f
}

static NOTED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

fn note(name: &str, what: &str) {
    if NOTED.lock().unwrap().insert(name.to_string()) {
        log!("eos-lan {name}: {what}");
    }
}

// ---- hooks on the real SDK ----

type PlatformCreateFn = unsafe extern "system" fn(*const abi::PlatformOptions) -> usize;
type PlatformTickFn = unsafe extern "system" fn(usize);

static CREATE: OnceLock<GenericDetour<PlatformCreateFn>> = OnceLock::new();
static TICK: OnceLock<GenericDetour<PlatformTickFn>> = OnceLock::new();

type RestartCheckFn = unsafe extern "system" fn(usize) -> i32;
static RESTART_CHECK: OnceLock<GenericDetour<RestartCheckFn>> = OnceLock::new();

/// EOS_Platform_CheckForLauncherAndRestart: a game asks whether Epic's launcher started
/// it, and if not, the SDK starts it again through the launcher and the game quits. One
/// that PeerParty started would leave PeerParty that way (and run once, not twice), so
/// the answer there is "carry on" (EOS_NoChange), in every mode. Returns the hooked
/// function, for the call logging to leave alone.
pub fn answer_restart_check(exports: &[Export]) -> Option<usize> {
    let e = exports.iter().find(|e| e.name == "EOS_Platform_CheckForLauncherAndRestart")?;
    let target: RestartCheckFn = unsafe { std::mem::transmute(e.addr) };
    arm(&RESTART_CHECK, target, detour_restart_check, "EOS_Platform_CheckForLauncherAndRestart").then_some(e.addr as usize)
}

unsafe extern "system" fn detour_restart_check(_platform: usize) -> i32 {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| log!("eos: the game asked to restart through Epic's launcher; told to carry on (PeerParty started it)"));
    abi::result::NO_CHANGE
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let hook = match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => h,
            Err(e) => {
                log!("eos-lan {label} not hooked: {e}");
                return false;
            }
        };
        if slot.set(hook).is_err() {
            return false;
        }
        if let Err(e) = unsafe { slot.get().unwrap().enable() } {
            log!("eos-lan {label} enable failed: {e}");
            return false;
        }
        true
    })
}

/// The game's EOS SDK just loaded. Returns the exports hooked here, which the generic
/// logging pass must leave alone.
pub fn attach(base: usize, module_name: &str, exports: &[Export]) -> HashSet<usize> {
    crate::swap::register(base, module_name, resolve);
    net::subscribe(Front::Eos);
    let mut skip = HashSet::new();
    if let Some(e) = exports.iter().find(|e| e.name == "EOS_Platform_Create") {
        let target: PlatformCreateFn = unsafe { std::mem::transmute(e.addr) };
        if arm(&CREATE, target, detour_create, "EOS_Platform_Create") {
            skip.insert(e.addr as usize);
        }
    }
    if let Some(e) = exports.iter().find(|e| e.name == "EOS_Platform_Tick") {
        let target: PlatformTickFn = unsafe { std::mem::transmute(e.addr) };
        if arm(&TICK, target, detour_tick, "EOS_Platform_Tick") {
            skip.insert(e.addr as usize);
        }
    }
    log!(
        "eos-lan on for {module_name}: {} functions are PeerParty's, the rest of the multiplayer surface fails fast",
        exports
            .iter()
            .filter(|e| users::resolve(&e.name)
                .or_else(|| rtc::resolve(&e.name))
                .or_else(|| lobby::resolve(&e.name))
                .or_else(|| p2p::resolve(&e.name))
                .or_else(|| storage::resolve(&e.name))
                .is_some())
            .count()
    );
    skip
}

unsafe extern "system" fn detour_create(options: *const abi::PlatformOptions) -> usize {
    let product = if options.is_null() {
        None
    } else {
        unsafe { ids::text((*options).ProductId) }
    };
    if let Some(p) = &product {
        let _ = PRODUCT.set(p.clone());
    }
    lan();
    crate::swap::patch_imports();
    let Some(hook) = CREATE.get() else { return 0 };
    let platform = unsafe { hook.call(options) };
    log!(
        "eos-lan EOS_Platform_Create product={} -> {platform:#x}",
        product.unwrap_or_default()
    );
    platform
}

unsafe extern "system" fn detour_tick(platform: usize) {
    if let Some(hook) = TICK.get() {
        unsafe { hook.call(platform) };
    }
    let _ = std::panic::catch_unwind(pump);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_restart_check_says_carry_on() {
        assert_eq!(unsafe { detour_restart_check(0) }, abi::result::NO_CHANGE);
    }

    #[test]
    fn clamping_keeps_whole_characters() {
        assert_eq!(clamp("abc", 5), "abc");
        assert_eq!(clamp("abcdef", 3), "abc");
        assert_eq!(clamp("aé", 2), "a", "é is two bytes");
        assert_eq!(clamp("€€", 4), "€");
        assert_eq!(clamp("€", 0), "");
    }

    /// A value handed out is freed by its release, whichever field the game was given;
    /// an address outside every value frees nothing.
    #[test]
    fn release_takes_a_field_inside_the_value() {
        #[repr(C)]
        struct Pair {
            data: [u64; 4],
            info: u32,
        }
        let held = |p: usize| OWNED.lock().unwrap().contains_key(&p);
        let p = hand_out(Pair { data: [0; 4], info: 1 }, Vec::new());
        let info = unsafe { &(*(p as *const Pair)).info } as *const u32 as usize;
        assert_ne!(info, p);
        let q = hand_out(7u64, Vec::new());
        release(usize::MAX);
        release(0);
        assert!(held(p) && held(q));
        release(info);
        assert!(!held(p), "released through its inner field");
        assert!(held(q));
        release(q);
        assert!(!held(q));
    }

    #[test]
    fn only_an_anti_cheat_session_start_stops_the_game() {
        let stop = anticheat_begin_session as *const () as usize;
        assert_eq!(resolve("EOS_AntiCheatClient_BeginSession"), Some(stop));
        assert_eq!(resolve("EOS_AntiCheatServer_BeginSession"), Some(stop));
        // The rest of the anti-cheat API is exported by every SDK build: not a reason.
        for name in ["EOS_AntiCheatClient_AddNotifyMessageToServer", "EOS_AntiCheatClient_EndSession", "EOS_Platform_GetAntiCheatClientInterface"] {
            assert_ne!(resolve(name), Some(stop), "{name}");
        }
    }
}
