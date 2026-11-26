//! Steam's C++ interfaces on the LAN. Native games (and steam_api's own flat functions)
//! call Steam through interface objects: a vtable of virtual methods behind
//! `SteamInternal_CreateInterface`, `SteamInternal_FindOrCreateUserInterface` and the old
//! `SteamFriends()`-style exports. In a game started through PeerParty those hand out a stand-in object of
//! PeerParty's instead: its vtable runs PeerParty's code for the multiplayer methods
//! (lobbies, P2P, networking sockets and messages, friends, the local SteamID) and,
//! for every other slot, a four
//! instruction stub that swaps in the real object and jumps to the real method. So the
//! Steam keeps answering everything else, whatever the method's signature.
//!
//! The flat API's accessors (`SteamAPI_SteamUser_v023` and the rest) are swapped by name
//! too: a steam_api whose accessors don't go through
//! `SteamInternal_FindOrCreateUserInterface` would otherwise hand the flat functions its
//! own objects, and the stand-ins would never be in the path.
//!
//! Slot layouts per interface version come from the Steamworks headers (steamvt.rs). A
//! version PeerParty has no layout for is handed out unwrapped.
//!
//! A game server's interfaces (`SteamInternal_FindOrCreateGameServerInterface`, or
//! ISteamClient's getters on the server's pipe) get stand-ins of their own: its
//! ISteamGameServer, and an ISteamNetworking whose packets go to and from the server's
//! SteamID (steamgs.rs, steamp2p.rs).
//!
//! On x86 C++ methods are `thiscall` (`this` in ecx, callee pops); on x64 they're the
//! ordinary convention. MSVC returns a class such as CSteamID from a method through a
//! hidden pointer after `this`, so those methods take `ret: *mut u64` and return it.

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};

use retour::GenericDetour;

use crate::sys::hook::with_lock;
use crate::log;
use crate::sys::pe::Export;
use crate::sys::vtable::{self, exec_block, vmethod, Names};

/// Calls the real object's method `name` for a stand-in `this`, or gives `default`
/// when this version has no such method.
macro_rules! call_real {
    ($this:expr, $name:expr, fn($($t:ty),*) -> $r:ty, $default:expr $(, $arg:expr)* $(,)?) => {{
        match unsafe { $crate::steam::obj::real_method($this, $name) } {
            Some((real, f)) => {
                #[cfg(target_arch = "x86")]
                let f: unsafe extern "thiscall" fn(usize $(, $t)*) -> $r =
                    unsafe { std::mem::transmute(f) };
                #[cfg(not(target_arch = "x86"))]
                let f: unsafe extern "C" fn(usize $(, $t)*) -> $r = unsafe { std::mem::transmute(f) };
                unsafe { f(real $(, $arg)*) }
            }
            None => $default,
        }
    }};
}
pub(crate) use call_real;

/// Calls steam_api's flat function `$f` that returns a CSteamID, as `$class` says this
/// build's flat API does: through a hidden pointer before `self` (`true`), else as a
/// plain number in a register. Gives the SteamID.
macro_rules! call_flat_id {
    ($class:expr, $f:expr, $self:expr $(, $a:expr => $t:ty)* $(,)?) => {{
        let f: usize = $f;
        if $class {
            let f: unsafe extern "C" fn(*mut u64, usize $(, $t)*) -> *mut u64 = unsafe { std::mem::transmute(f) };
            let mut id = 0u64;
            unsafe { f(&mut id, $self $(, $a)*) };
            id
        } else {
            let f: unsafe extern "C" fn(usize $(, $t)*) -> u64 = unsafe { std::mem::transmute(f) };
            unsafe { f($self $(, $a)*) }
        }
    }};
}
pub(crate) use call_flat_id;

/// The stand-in object. `real` sits right after the vtable pointer, where the
/// forwarding stubs read it.
#[repr(C)]
struct Proxy {
    vtable: *const usize,
    real: usize,
    version: &'static str,
    /// A game server's interface.
    server: bool,
}

/// (real object, for the game server) -> stand-in, so a game asking twice gets the same
/// object. The server's and the player's are told apart: a game server on the game's own
/// pipe can be handed the very object the player has.
static PROXIES: LazyLock<Mutex<HashMap<(usize, bool), usize>>> = LazyLock::new(Mutex::default);
/// stand-in -> real.
/// Read on every call through a stand-in: a read lock, so game threads never wait on each
/// other there.
static REALS: LazyLock<RwLock<HashMap<usize, usize>>> = LazyLock::new(RwLock::default);
/// Real objects by family ("SteamUser"), newest first seen, for PeerParty's own calls.
static SEEN: LazyLock<Mutex<HashMap<&'static str, (usize, &'static str)>>> =
    LazyLock::new(Mutex::default);

/// "SteamMatchMaking009" -> "SteamMatchMaking".
fn family(version: &str) -> &str {
    version.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// Every version PeerParty has a layout for.
pub(crate) fn tables() -> impl Iterator<Item = (&'static str, Names)> {
    crate::steam::vt::TABLES.iter().map(|(v, ix)| (*v, Names::new(crate::steam::vt::NAMES, ix)))
}

pub(crate) fn layout(version: &str) -> Option<(&'static str, Names)> {
    tables().find(|(v, _)| *v == version)
}

/// The slot of `name` in `version`, when it has the newest signature. An overloaded
/// method's slots share a name: `name@N` is the Nth of them (from 0, in vtable order).
pub fn slot(version: &str, name: &str) -> Option<usize> {
    let names = layout(version)?.1;
    match name.rsplit_once('@').and_then(|(n, k)| Some((n, k.parse::<usize>().ok()?))) {
        Some((n, k)) => names.iter().enumerate().filter(|(_, x)| **x == n).nth(k).map(|(i, _)| i),
        None => names.iter().position(|n| *n == name).or_else(|| {
            let alt = SAME_CALL.iter().find(|(n, _)| *n == name)?.1;
            names.iter().position(|n| *n == alt)
        }),
    }
}

/// Older shapes that differ from the newest only in how a header spelled a type (the
/// friend flags as `EFriendFlags`, now `int`): the same call, so the newest name finds
/// them too.
const SAME_CALL: &[(&str, &str)] = &[("GetFriendCount", "GetFriendCount~1"), ("HasFriend", "HasFriend~2")];

/// PeerParty's methods for an interface version, for a client or the game server.
fn overrides(family: &str, version: &str, server: bool) -> Vec<(&'static str, usize)> {
    match family {
        "SteamNetworking" if server => crate::steam::p2p::server_networking(),
        "SteamGameServer" => crate::steam::gs::server(),
        "SteamMatchMakingServers" => crate::steam::gs::browser(),
        "SteamClient" => client(),
        "SteamUser" => crate::steam::p2p::user(),
        "SteamFriends" => crate::steam::lan::friends(),
        "SteamMatchMaking" => crate::steam::mm::matchmaking(),
        "SteamNetworking" => crate::steam::p2p::networking(),
        "SteamNetworkingSockets" => crate::steam::gns::sockets(version),
        "SteamNetworkingMessages" => crate::steam::gns::messages(),
        "SteamNetworkingUtils" => crate::steam::gns::utils(),
        "SteamUtils" => crate::steam::utils(),
        "SteamInput" | "SteamController" => crate::steam::lan::input(),
        "STEAMUSERSTATS_INTERFACE_VERSION" if !server => crate::steam::own::user_stats(),
        "STEAMAPPS_INTERFACE_VERSION" if !server => crate::steam::own::apps(),
        "STEAMUGC_INTERFACE_VERSION" if !server => crate::steam::own::ugc(),
        "STEAMREMOTESTORAGE_INTERFACE_VERSION" if !server => crate::steam::remote::remote_storage(),
        _ => Vec::new(),
    }
}

/// The stand-in for `real`, or `real` itself for an interface PeerParty leaves alone.
pub fn wrap(real: usize, version: &str) -> usize {
    wrap_as(real, version, false)
}

/// `wrap` for a client's interface or (`server`) the game server's.
pub fn wrap_as(real: usize, version: &str, server: bool) -> usize {
    if real == 0 || !crate::net::enabled() {
        return real;
    }
    if REALS.read().unwrap().contains_key(&real) {
        return real;
    }
    if let Some(p) = crate::sys::lock(&PROXIES).get(&(real, server)) {
        return *p;
    }
    let fam = family(version);
    let mine = overrides(fam, version, server);
    if mine.is_empty() {
        return real;
    }
    let Some((version, names)) = layout(version) else {
        log::throttled(&format!("steam-lan unknown {version}"), 1, |_| {
            log!("steam-lan {version}: no layout known, left as is")
        });
        return real;
    };
    // How many slots of each name came before: an overload's `name@N` (see `slot`).
    let mut before: HashMap<&str, usize> = HashMap::new();
    let Some((vtable, ours)) = vtable::build(names.len(), |i| {
        let n = *names.get(i)?;
        let k = before.entry(n).or_insert(0);
        let nth = format!("{n}@{k}");
        *k += 1;
        mine.iter().find(|(m, _)| *m == nth).or_else(|| mine.iter().find(|(m, _)| *m == n)).map(|(_, f)| *f)
    }) else {
        return real;
    };
    let proxy = Box::leak(Box::new(Proxy {
        vtable,
        real,
        version,
        server,
    })) as *const Proxy as usize;
    {
        // Another thread may have wrapped the same object meanwhile: the game must get
        // one stand-in per object, so the first one in stays (this one goes unused).
        let mut proxies = crate::sys::lock(&PROXIES);
        if let Some(p) = proxies.get(&(real, server)) {
            return *p;
        }
        proxies.insert((real, server), proxy);
        REALS.write().unwrap().insert(proxy, real);
    }
    if !server {
        SEEN.lock()
            .unwrap()
            .entry(family(version))
            .or_insert((real, version));
    }
    log!(
        "steam-lan {version}{}: peerparty answers {ours} of its {} methods",
        if server { " (game server)" } else { "" },
        names.len()
    );
    proxy
}

/// Whether `p` is one of PeerParty's stand-ins.
pub fn is_stand_in(p: usize) -> bool {
    REALS.read().unwrap().contains_key(&p)
}

/// The interface versions PeerParty has handed out stand-ins for, sorted, for the log
/// when there's nothing to invite to.
pub fn stand_ins() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = REALS
        .read()
        .unwrap()
        .keys()
        .map(|p| unsafe { (*(*p as *const Proxy)).version })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// What the log says about Steam when there's nothing to invite to: the stand-ins the
/// game was handed, and whether matchmaking (where lobbies are made) is among them.
pub fn invite_diagnosis() -> String {
    let have = stand_ins();
    let lobbies = have.iter().any(|v| family(v) == "SteamMatchMaking");
    format!(
        "steam stand-ins handed out: {}{}",
        if have.is_empty() { "none".to_string() } else { have.join(", ") },
        if lobbies { "" } else { " (none for matchmaking: a lobby the game makes doesn't reach PeerParty)" }
    )
}

/// The real object behind a stand-in (anything else comes back as is).
pub fn unwrap(p: usize) -> usize {
    REALS.read().unwrap().get(&p).copied().unwrap_or(p)
}

/// (real object, real method) for a stand-in's method `name`.
pub unsafe fn real_method(this: usize, name: &str) -> Option<(usize, usize)> {
    if !REALS.read().unwrap().contains_key(&this) {
        return None;
    }
    let proxy = unsafe { &*(this as *const Proxy) };
    let i = stand_in_slot(proxy.version, name)?;
    let vtable = unsafe { *(proxy.real as *const *const usize) };
    Some((proxy.real, unsafe { *vtable.add(i) }))
}

/// Whether `this` is one of our stand-ins for a game server's interface (one the game
/// fetched on the game server's pipe, or through the game server's accessors).
pub fn is_server(this: usize) -> bool {
    REALS.read().unwrap().contains_key(&this) && unsafe { (*(this as *const Proxy)).server }
}

/// A real interface the game has fetched (family "SteamUser"), as its stand-in, for
/// PeerParty's own calls into it.
pub fn seen(family: &str) -> Option<usize> {
    let (real, _) = *crate::sys::lock(&SEEN).get(family)?;
    crate::sys::lock(&PROXIES).get(&(real, false)).copied()
}

// ---- ISteamClient: hands out stand-ins ----

fn client() -> Vec<(&'static str, usize)> {
    vec![
        ("GetISteamUser", get_user as *const () as usize),
        ("GetISteamFriends", get_friends as *const () as usize),
        ("GetISteamMatchmaking", get_matchmaking as *const () as usize),
        ("GetISteamMatchmakingServers", get_matchmaking_servers as *const () as usize),
        ("GetISteamNetworking", get_networking as *const () as usize),
        ("GetISteamGameServer", get_game_server as *const () as usize),
        ("GetISteamGenericInterface", get_generic as *const () as usize),
        ("GetISteamUtils", get_utils as *const () as usize),
        ("GetISteamUserStats", get_user_stats as *const () as usize),
        ("GetISteamApps", get_apps as *const () as usize),
        ("GetISteamUGC", get_ugc as *const () as usize),
        ("GetISteamRemoteStorage", get_remote_storage as *const () as usize),
        ("GetISteamController", get_controller as *const () as usize),
        ("GetISteamInput", get_input as *const () as usize),
    ]
}

macro_rules! client_getter {
    ($fn:ident, $name:literal) => {
        vmethod! {
            fn $fn(this: usize, user: i32, pipe: i32, version: *const c_char) -> usize {
                let real = call_real!(this, $name, fn(i32, i32, *const c_char) -> usize, 0, user, pipe, version);
                // On the game server's pipe, it's the server's interface.
                let server = crate::steam::gs::for_server(pipe, $name) || $name == "GetISteamGameServer";
                wrap_as(real, &crate::sys::read::cstr(version as usize).unwrap_or_default(), server)
            }
        }
    };
}

client_getter!(get_user, "GetISteamUser");
client_getter!(get_friends, "GetISteamFriends");
client_getter!(get_matchmaking, "GetISteamMatchmaking");
client_getter!(get_matchmaking_servers, "GetISteamMatchmakingServers");
client_getter!(get_networking, "GetISteamNetworking");
client_getter!(get_game_server, "GetISteamGameServer");
client_getter!(get_generic, "GetISteamGenericInterface");
client_getter!(get_user_stats, "GetISteamUserStats");
client_getter!(get_apps, "GetISteamApps");
client_getter!(get_ugc, "GetISteamUGC");
client_getter!(get_remote_storage, "GetISteamRemoteStorage");
client_getter!(get_controller, "GetISteamController");
client_getter!(get_input, "GetISteamInput");

vmethod! {
    /// ISteamUtils takes only the pipe: the game server's pipe gets the server's.
    fn get_utils(this: usize, pipe: i32, version: *const c_char) -> usize {
        let real = call_real!(this, "GetISteamUtils", fn(i32, *const c_char) -> usize, 0, pipe, version);
        let server = crate::steam::gs::for_server(pipe, "GetISteamUtils");
        wrap_as(real, &crate::sys::read::cstr(version as usize).unwrap_or_default(), server)
    }
}

// ---- steam_api exports that hand out interfaces ----

type CreateFn = unsafe extern "C" fn(*const c_char) -> usize;
type UserFn = unsafe extern "C" fn(i32, *const c_char) -> usize;
type LegacyFn = unsafe extern "C" fn() -> usize;

static CREATE: OnceLock<GenericDetour<CreateFn>> = OnceLock::new();
static FIND_USER: OnceLock<GenericDetour<UserFn>> = OnceLock::new();
static FIND_SERVER: OnceLock<GenericDetour<UserFn>> = OnceLock::new();
/// An old accessor (`SteamMatchmaking()` and the rest): they take no version, so it's
/// the one steam_api names for its family, or else the one its object turns out to be.
struct Legacy {
    export: &'static str,
    /// Its inline hook, when one could go in.
    hook: Option<GenericDetour<LegacyFn>>,
    addr: usize,
    /// "SteamMatchMaking", as versions are named.
    family: &'static str,
    /// The version steam_api names, if it names one.
    named: Option<String>,
    /// Otherwise the version its object's vtable matches, worked out on the first call.
    seen: OnceLock<Option<&'static str>>,
    server: bool,
}

static LEGACY: OnceLock<Vec<Legacy>> = OnceLock::new();
/// steam_api's flat methods of every interface, by name: a steam_api that names no
/// interface versions can still be told apart by them (`version_from_vtable`).
static FLAT_ALL: OnceLock<HashMap<String, usize>> = OnceLock::new();
/// The `SteamInternal_*` accessors' own addresses, for calls that reach the detours
/// through the by-name swap when the inline hook couldn't go in (an export too short to
/// patch).
static INTERNAL: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
const INTERNAL_NAMES: [&str; 3] = [
    "SteamInternal_CreateInterface",
    "SteamInternal_FindOrCreateUserInterface",
    "SteamInternal_FindOrCreateGameServerInterface",
];

fn internal(name: &str) -> Option<usize> {
    INTERNAL.get()?.get(name).copied()
}

unsafe extern "C" fn detour_create(version: *const c_char) -> usize {
    let real = match CREATE.get() {
        Some(h) => unsafe { h.call(version) },
        None => match internal("SteamInternal_CreateInterface") {
            Some(f) => unsafe { std::mem::transmute::<usize, CreateFn>(f)(version) },
            None => return 0,
        },
    };
    wrap(real, &crate::sys::read::cstr(version as usize).unwrap_or_default())
}

unsafe extern "C" fn detour_find_user(user: i32, version: *const c_char) -> usize {
    let real = match FIND_USER.get() {
        Some(h) => unsafe { h.call(user, version) },
        None => match internal("SteamInternal_FindOrCreateUserInterface") {
            Some(f) => unsafe { std::mem::transmute::<usize, UserFn>(f)(user, version) },
            None => return 0,
        },
    };
    wrap(real, &crate::sys::read::cstr(version as usize).unwrap_or_default())
}

unsafe extern "C" fn detour_find_server(user: i32, version: *const c_char) -> usize {
    let real = match FIND_SERVER.get() {
        Some(h) => unsafe { h.call(user, version) },
        None => match internal("SteamInternal_FindOrCreateGameServerInterface") {
            Some(f) => unsafe { std::mem::transmute::<usize, UserFn>(f)(user, version) },
            None => return 0,
        },
    };
    wrap_as(real, &crate::sys::read::cstr(version as usize).unwrap_or_default(), true)
}

macro_rules! legacy {
    ($fn:ident, $export:literal) => {
        unsafe extern "C" fn $fn() -> usize {
            let Some(l) = LEGACY.get().and_then(|l| l.iter().find(|l| l.export == $export)) else {
                return 0;
            };
            let real = match &l.hook {
                Some(h) => unsafe { h.call() },
                None => unsafe { std::mem::transmute::<usize, LegacyFn>(l.addr)() },
            };
            match legacy_version(l, real) {
                Some(version) => wrap_as(real, version, l.server),
                None => real,
            }
        }
    };
}

legacy!(legacy_client, "SteamClient");
legacy!(legacy_user, "SteamUser");
legacy!(legacy_friends, "SteamFriends");
legacy!(legacy_matchmaking, "SteamMatchmaking");
legacy!(legacy_networking, "SteamNetworking");
legacy!(legacy_matchmaking_servers, "SteamMatchmakingServers");
legacy!(legacy_game_server, "SteamGameServer");
legacy!(legacy_game_server_networking, "SteamGameServerNetworking");
legacy!(legacy_utils, "SteamUtils");
legacy!(legacy_game_server_utils, "SteamGameServerUtils");
legacy!(legacy_user_stats, "SteamUserStats");
legacy!(legacy_apps, "SteamApps");
legacy!(legacy_ugc, "SteamUGC");
legacy!(legacy_remote_storage, "SteamRemoteStorage");
legacy!(legacy_controller, "SteamController");

/// The old accessors' stand-in functions, by export name, for the by-name swap.
static LEGACY_FNS: [(&str, LegacyFn); 15] = [
    ("SteamClient", legacy_client),
    ("SteamUser", legacy_user),
    ("SteamFriends", legacy_friends),
    ("SteamMatchmaking", legacy_matchmaking),
    ("SteamNetworking", legacy_networking),
    ("SteamMatchmakingServers", legacy_matchmaking_servers),
    ("SteamGameServer", legacy_game_server),
    ("SteamGameServerNetworking", legacy_game_server_networking),
    ("SteamUtils", legacy_utils),
    ("SteamGameServerUtils", legacy_game_server_utils),
    ("SteamUserStats", legacy_user_stats),
    ("SteamApps", legacy_apps),
    ("SteamUGC", legacy_ugc),
    ("SteamRemoteStorage", legacy_remote_storage),
    ("SteamController", legacy_controller),
];

// ---- the flat API's accessors ----

/// The flat accessors steam_api exports (some builds export every historical version
/// of each): name, real function, the version it hands out, whether it's the game
/// server's; and the block of stubs, one per accessor, that the by-name swap hands out
/// (steamlan.rs's resolver). Where the accessor already hands out a stand-in, `wrap_as`
/// leaves it as is.
static FLAT: OnceLock<(Vec<(String, usize, String, bool)>, usize)> = OnceLock::new();
const ACCESSOR_STUB: usize = 32;

/// "SteamAPI_SteamUser_v023" -> ("SteamUser023", client),
/// "SteamAPI_SteamGameServerNetworkingSockets_SteamAPI_v012" -> ("SteamNetworkingSockets012", server).
fn flat_accessor(name: &str) -> Option<(String, bool)> {
    let rest = name.strip_prefix("SteamAPI_Steam")?;
    let (base, digits) = rest.rsplit_once("_v")?;
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (family, server) = match base.strip_suffix("_SteamAPI").unwrap_or(base) {
        "User" => ("SteamUser", false),
        "Friends" => ("SteamFriends", false),
        "Matchmaking" => ("SteamMatchMaking", false),
        "MatchmakingServers" => ("SteamMatchMakingServers", false),
        "Networking" => ("SteamNetworking", false),
        "NetworkingSockets" => ("SteamNetworkingSockets", false),
        "NetworkingMessages" => ("SteamNetworkingMessages", false),
        "NetworkingUtils" => ("SteamNetworkingUtils", false),
        "Utils" => ("SteamUtils", false),
        "GameServerUtils" => ("SteamUtils", true),
        "GameServer" => ("SteamGameServer", true),
        "GameServerNetworking" => ("SteamNetworking", true),
        "GameServerNetworkingSockets" => ("SteamNetworkingSockets", true),
        "GameServerNetworkingMessages" => ("SteamNetworkingMessages", true),
        _ => return None,
    };
    Some((format!("{family}{digits:0>3}"), server))
}

/// Notes steam_api's flat accessors and methods, before the swap first resolves one.
/// A flat function takes its arguments as the C++ method it stands for does, `self` as
/// `this`, so the flat layer can be served by the stand-ins' own vtables: on x64 the
/// conventions are the same; on x86 a trampoline turns the cdecl call into a thiscall.
pub fn note_flat(exports: &[Export]) {
    let _ = INTERNAL.set(
        INTERNAL_NAMES
            .iter()
            .filter_map(|n| exports.iter().find(|e| e.name == *n).map(|e| (*n, e.addr as usize)))
            .collect(),
    );
    let found: Vec<_> = exports
        .iter()
        .filter_map(|e| flat_accessor(&e.name).map(|(v, s)| (e.name.clone(), e.addr as usize, v, s)))
        .filter(|(_, _, v, _)| layout(v).is_some())
        .collect();
    let base = unsafe { make_accessor_stubs(found.len()) };
    if base != 0 {
        log!("steam-lan flat accessors handing out stand-ins: {}", found.len());
        let _ = FLAT.set((found, base));
    }
    let real: HashMap<String, usize> = exports
        .iter()
        .filter(|e| flat_interface(&e.name).is_some())
        .map(|e| (e.name.clone(), e.addr as usize))
        .collect();
    let methods: Vec<_> = exports
        .iter()
        .filter_map(|e| flat_method(&e.name).map(|m| (e.name.clone(), e.addr as usize, m.to_string())))
        .collect();
    let base = unsafe { make_flat_thunks(methods.len()) };
    if base != 0 {
        log!("steam-lan flat methods served by the stand-ins: {}", methods.len());
        let _ = FLAT_METHODS.set((methods, base));
    }
    let class = returns_class_flat(exports.iter().map(|e| e.name.as_str()));
    FLAT_CLASS.store(class, std::sync::atomic::Ordering::Relaxed);
    if class && exports.iter().any(|e| flat_interface(&e.name).is_some()) {
        log!("steam-lan flat API without versioned accessors: its CSteamID returns come through a hidden pointer before self");
    }
    let _ = FLAT_REAL.set(real);
}

/// Whether steam_api's flat functions that return a CSteamID declare it as the class:
/// MSVC hands a class back through a hidden pointer before `self`. The flat API that
/// added the versioned accessors (`SteamAPI_SteamUser_v020`) declares a plain number,
/// returned in a register; the one before it the class.
static FLAT_CLASS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn flat_returns_class() -> bool {
    FLAT_CLASS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether these export names are the older flat API's: none is a versioned accessor of
/// any interface.
fn returns_class_flat<'a>(mut names: impl Iterator<Item = &'a str>) -> bool {
    !names.any(|n| {
        n.strip_prefix("SteamAPI_Steam")
            .and_then(|rest| rest.rsplit_once("_v"))
            .is_some_and(|(_, d)| (1..=3).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// PeerParty's function for a flat accessor or method `name`, for the by-name swap.
pub fn flat_swap(name: &str) -> Option<usize> {
    if let Some(f) = accessor_swap(name) {
        log::throttled(&format!("steam-lan resolve {name}"), 1, |_| {
            log!("steam-lan {name}: hands out PeerParty's stand-in")
        });
        return Some(f);
    }
    if let Some((_, f)) = adapters().into_iter().find(|(n, _)| *n == name) {
        return Some(f);
    }
    let (methods, base) = FLAT_METHODS.get()?;
    let i = methods.iter().position(|(n, ..)| n == name)?;
    Some(base + i * FLAT_THUNK)
}

// ---- the flat API's methods ----

/// The flat methods of the interfaces PeerParty stands in for: name, steam_api's own
/// function, the C++ method's name; and the block of trampolines, one per method.
static FLAT_METHODS: OnceLock<(Vec<(String, usize, String)>, usize)> = OnceLock::new();
/// Every flat method of those interfaces, by name, for the adapters' fallbacks.
static FLAT_REAL: OnceLock<HashMap<String, usize>> = OnceLock::new();
const FLAT_THUNK: usize = 112;

/// Methods that return a class (a CSteamID, a SteamIPAddress_t). The C++ method returns
/// it through a hidden pointer after `this`; the flat function in a register, or through
/// a hidden pointer before `self`. A trampoline can't bridge that: PeerParty's own get
/// adapters (`adapters`), the rest stay with steam_api's flat function.
const RETURNS_CLASS: &[&str] = &[
    "GetSteamID", "GetFriendByIndex", "GetClanByIndex", "GetClanOwner", "GetClanOfficerByIndex",
    "GetChatMemberByIndex", "GetCoplayFriend", "GetFriendFromSourceByIndex", "GetLobbyByIndex",
    "GetLobbyMemberByIndex", "GetLobbyOwner", "CreateUnauthenticatedUserConnection", "GetPublicIP",
];

/// "SteamAPI_ISteamUser_GetSteamID" -> ("SteamUser", "GetSteamID"), for the interfaces
/// PeerParty stands in for.
fn flat_interface(name: &str) -> Option<(&str, &str)> {
    let (iface, method) = name.strip_prefix("SteamAPI_I")?.split_once('_')?;
    let ours = matches!(
        iface,
        "SteamUser" | "SteamFriends" | "SteamMatchmaking" | "SteamMatchmakingServers" | "SteamNetworking"
            | "SteamNetworkingSockets" | "SteamNetworkingMessages" | "SteamNetworkingUtils" | "SteamGameServer"
            | "SteamClient" | "SteamUtils"
    );
    (ours && !method.is_empty()).then_some((iface, method))
}

/// The C++ method a flat function can be trampolined to.
fn flat_method(name: &str) -> Option<&str> {
    flat_interface(name).map(|(_, m)| m).filter(|m| !RETURNS_CLASS.contains(m))
}

/// The stand-in's own method `name` (PeerParty's, or the stub to the real one), when
/// `this` is a stand-in whose version has that method.
unsafe fn member(this: usize, name: &str) -> Option<usize> {
    if !REALS.read().unwrap().contains_key(&this) {
        return None;
    }
    let proxy = unsafe { &*(this as *const Proxy) };
    let i = stand_in_slot(proxy.version, name)?;
    Some(unsafe { *proxy.vtable.add(i) })
}

/// `slot` for a stand-in's version (one of vt.rs's own strings), remembered: every flat
/// API call through a stand-in asks, and `slot` searches the tables by name.
fn stand_in_slot(version: &'static str, name: &str) -> Option<usize> {
    static SLOTS: LazyLock<RwLock<HashMap<usize, HashMap<String, Option<usize>>>>> = LazyLock::new(RwLock::default);
    let key = version.as_ptr() as usize;
    if let Some(&i) = SLOTS.read().unwrap_or_else(|e| e.into_inner()).get(&key).and_then(|m| m.get(name)) {
        return i;
    }
    let i = slot(version, name);
    SLOTS.write().unwrap_or_else(|e| e.into_inner()).entry(key).or_default().insert(name.to_string(), i);
    i
}

/// Where flat method `i` goes for `self`: the stand-in's own method, as Valve's flat
/// functions call it; else steam_api's own flat function, with the real object written
/// back over the `self` the trampoline saved.
#[cfg(not(target_arch = "x86"))]
extern "C" fn flat_target(this: usize, i: usize, saved_this: *mut usize) -> usize {
    let Some((methods, _)) = FLAT_METHODS.get() else { return 0 };
    let (_, real, method) = &methods[i];
    if let Some(f) = unsafe { member(this, method) } {
        return f;
    }
    unsafe { *saved_this = unwrap(this) };
    *real
}

/// x86: as `flat_target`, with whether the target is the stand-in's (thiscall) method
/// in the high half, else steam_api's own (cdecl) flat function.
#[cfg(target_arch = "x86")]
extern "C" fn flat_target32(this: usize, i: usize, saved_this: *mut usize) -> u64 {
    let Some((methods, _)) = FLAT_METHODS.get() else { return 0 };
    let (_, real, method) = &methods[i];
    if let Some(f) = unsafe { member(this, method) } {
        return 1 << 32 | f as u64;
    }
    unsafe { *saved_this = unwrap(this) };
    *real as u64
}

/// x86: a flat function is cdecl (`self` and the arguments on the stack, the caller pops)
/// and the C++ method thiscall (`this` in ecx, the method pops its arguments). The
/// trampoline asks `flat_target32(self, i, &self)`; for steam_api's own function it
/// jumps there with the real object in `self`; for the stand-in's method it copies the
/// stack arguments (as many as any Steam method has), calls it with `this` in ecx and
/// puts the stack back itself, however much the method popped.
#[cfg(target_arch = "x86")]
unsafe fn make_flat_thunks(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    const ARGS: u8 = 24; // dwords copied
    unsafe { exec_block(n, FLAT_THUNK, |i| {
        // push ebp; mov ebp,esp; lea eax,[ebp+8]; push eax; push i; push dword [ebp+8]
        let mut c: Vec<u8> = vec![0x55, 0x89, 0xE5, 0x8D, 0x45, 0x08, 0x50, 0x68];
        c.extend((i as u32).to_le_bytes());
        c.extend([0xFF, 0x75, 0x08]);
        // mov eax, flat_target32; call eax; add esp,12; test edx,edx; jnz member
        c.push(0xB8);
        c.extend((flat_target32 as *const () as usize as u32).to_le_bytes());
        c.extend([0xFF, 0xD0, 0x83, 0xC4, 0x0C, 0x85, 0xD2, 0x75, 0x05]);
        // steam_api's own: mov esp,ebp; pop ebp; jmp eax
        c.extend([0x89, 0xEC, 0x5D, 0xFF, 0xE0]);
        // member: push esi; push edi; sub esp,ARGS*4; lea esi,[ebp+12]; mov edi,esp;
        // mov ecx,ARGS; rep movsd; mov ecx,[ebp+8]; call eax
        c.extend([0x56, 0x57, 0x83, 0xEC, ARGS * 4, 0x8D, 0x75, 0x0C, 0x89, 0xE7, 0xB9]);
        c.extend((ARGS as u32).to_le_bytes());
        c.extend([0xF3, 0xA5, 0x8B, 0x4D, 0x08, 0xFF, 0xD0]);
        // lea esp,[ebp-8]; pop edi; pop esi; pop ebp; ret (eax, edx and st0 kept)
        c.extend([0x8D, 0x65, 0xF8, 0x5F, 0x5E, 0x5D, 0xC3]);
        c
    }) }
}

/// One trampoline per flat method: keep the argument registers (rcx, rdx, r8, r9,
/// xmm0-3), ask `flat_target(self, i, &saved self)` where to go, restore, jump there.
/// Stack arguments and the caller's shadow space are untouched.
#[cfg(not(target_arch = "x86"))]
unsafe fn make_flat_thunks(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let xmm = [(0x44u8, 0x20u8), (0x4C, 0x30), (0x54, 0x40), (0x5C, 0x50)];
    unsafe { exec_block(n, FLAT_THUNK, |i| {
        // push rcx; push rdx; push r8; push r9; sub rsp,0x68 (aligned for the call)
        let mut c: Vec<u8> = vec![0x51, 0x52, 0x41, 0x50, 0x41, 0x51, 0x48, 0x83, 0xEC, 0x68];
        // movdqu [rsp+0x20..0x50], xmm0-3
        for (r, off) in xmm {
            c.extend([0xF3, 0x0F, 0x7F, r, 0x24, off]);
        }
        // mov rdx, i; lea r8, [rsp+0x80] (the saved rcx); mov rax, flat_target; call rax
        c.extend([0x48, 0xBA]);
        c.extend((i as u64).to_le_bytes());
        c.extend([0x4C, 0x8D, 0x84, 0x24, 0x80, 0x00, 0x00, 0x00]);
        c.extend([0x48, 0xB8]);
        c.extend((flat_target as *const () as usize as u64).to_le_bytes());
        c.extend([0xFF, 0xD0]);
        for (r, off) in xmm {
            c.extend([0xF3, 0x0F, 0x6F, r, 0x24, off]);
        }
        // add rsp,0x68; pop r9; pop r8; pop rdx; pop rcx; jmp rax
        c.extend([0x48, 0x83, 0xC4, 0x68, 0x41, 0x59, 0x41, 0x58, 0x5A, 0x59, 0xFF, 0xE0]);
        c
    }) }
}

/// A flat method PeerParty answers that returns a CSteamID: the stand-in's method with
/// its hidden return pointer, the value handed back in a register; else steam_api's own
/// flat function with the real object. `$old` is the same for the older flat API, whose
/// callers pass a hidden return pointer before `self`.
macro_rules! flat_returns_id {
    ($fn:ident, $old:ident, $flat:literal, $method:literal $(, $a:ident: $t:ty)*) => {
        unsafe extern "C" fn $old(out: *mut u64, this: usize $(, $a: $t)*) -> *mut u64 {
            let id = unsafe { $fn(this $(, $a)*) };
            if !out.is_null() {
                unsafe { *out = id };
            }
            out
        }

        unsafe extern "C" fn $fn(this: usize $(, $a: $t)*) -> u64 {
            if let Some(f) = unsafe { member(this, $method) } {
                #[cfg(target_arch = "x86")]
                let f: unsafe extern "thiscall" fn(usize, *mut u64 $(, $t)*) -> *mut u64 = unsafe { std::mem::transmute(f) };
                #[cfg(not(target_arch = "x86"))]
                let f: unsafe extern "C" fn(usize, *mut u64 $(, $t)*) -> *mut u64 = unsafe { std::mem::transmute(f) };
                let mut id = 0u64;
                unsafe { f(this, &mut id $(, $a)*) };
                return id;
            }
            let Some(real) = FLAT_REAL.get().and_then(|r| r.get($flat).copied()) else { return 0 };
            call_flat_id!(flat_returns_class(), real, unwrap(this) $(, $a => $t)*)
        }
    };
}

flat_returns_id!(flat_user_steam_id, flat_user_steam_id_old, "SteamAPI_ISteamUser_GetSteamID", "GetSteamID");
flat_returns_id!(flat_server_steam_id, flat_server_steam_id_old, "SteamAPI_ISteamGameServer_GetSteamID", "GetSteamID");
flat_returns_id!(
    flat_unauthenticated,
    flat_unauthenticated_old,
    "SteamAPI_ISteamGameServer_CreateUnauthenticatedUserConnection",
    "CreateUnauthenticatedUserConnection"
);
flat_returns_id!(
    flat_lobby_by_index,
    flat_lobby_by_index_old,
    "SteamAPI_ISteamMatchmaking_GetLobbyByIndex",
    "GetLobbyByIndex",
    i: i32
);
flat_returns_id!(
    flat_lobby_member,
    flat_lobby_member_old,
    "SteamAPI_ISteamMatchmaking_GetLobbyMemberByIndex",
    "GetLobbyMemberByIndex",
    lobby: u64,
    i: i32
);
flat_returns_id!(
    flat_lobby_owner,
    flat_lobby_owner_old,
    "SteamAPI_ISteamMatchmaking_GetLobbyOwner",
    "GetLobbyOwner",
    lobby: u64
);

// ---- ISteamNetworkingUtils's inline helpers ----
//
// `SetGlobalConfigValueInt32`, `SetGlobalCallback_SteamNetConnectionStatusChanged` and the
// rest are inline in the header: each is one SetConfigValue call. steam_api's flat
// exports of them run that body on the object they're given, and a flat function
// PeerParty doesn't answer gets the real object, so the call would go to Steam's
// SetConfigValue and never reach the stand-in's (steamgns.rs keeps the game's status
// callbacks there). These run the same bodies on the stand-in.

const CFG_GLOBAL: i32 = 1;
const CFG_CONNECTION: i32 = 4;
const CFG_INT32: i32 = 1;
const CFG_FLOAT: i32 = 3;
const CFG_STRING: i32 = 4;
const CFG_PTR: i32 = 5;

/// `SetConfigValue` on the stand-in (PeerParty's), else steam_api's flat one with the
/// real object.
fn utils_set_config(this: usize, value: i32, scope: i32, obj: isize, kind: i32, arg: *const u8) -> bool {
    if let Some(f) = unsafe { member(this, "SetConfigValue") } {
        #[cfg(target_arch = "x86")]
        let f: unsafe extern "thiscall" fn(usize, i32, i32, isize, i32, *const u8) -> bool =
            unsafe { std::mem::transmute(f) };
        #[cfg(not(target_arch = "x86"))]
        let f: unsafe extern "C" fn(usize, i32, i32, isize, i32, *const u8) -> bool = unsafe { std::mem::transmute(f) };
        return unsafe { f(this, value, scope, obj, kind, arg) };
    }
    let Some(real) = FLAT_REAL.get().and_then(|r| r.get("SteamAPI_ISteamNetworkingUtils_SetConfigValue").copied()) else {
        return false;
    };
    let f: unsafe extern "C" fn(usize, i32, i32, isize, i32, *const u8) -> bool = unsafe { std::mem::transmute(real) };
    unsafe { f(unwrap(this), value, scope, obj, kind, arg) }
}

unsafe extern "C" fn cfg_global_i32(this: usize, value: i32, v: i32) -> bool {
    utils_set_config(this, value, CFG_GLOBAL, 0, CFG_INT32, &v as *const i32 as *const u8)
}
unsafe extern "C" fn cfg_global_f32(this: usize, value: i32, v: f32) -> bool {
    utils_set_config(this, value, CFG_GLOBAL, 0, CFG_FLOAT, &v as *const f32 as *const u8)
}
unsafe extern "C" fn cfg_global_str(this: usize, value: i32, v: *const c_char) -> bool {
    utils_set_config(this, value, CFG_GLOBAL, 0, CFG_STRING, v as *const u8)
}
/// A pointer value is passed as a pointer to the pointer.
unsafe extern "C" fn cfg_global_ptr(this: usize, value: i32, v: usize) -> bool {
    utils_set_config(this, value, CFG_GLOBAL, 0, CFG_PTR, &v as *const usize as *const u8)
}
unsafe extern "C" fn cfg_conn_i32(this: usize, conn: u32, value: i32, v: i32) -> bool {
    utils_set_config(this, value, CFG_CONNECTION, conn as isize, CFG_INT32, &v as *const i32 as *const u8)
}
unsafe extern "C" fn cfg_conn_f32(this: usize, conn: u32, value: i32, v: f32) -> bool {
    utils_set_config(this, value, CFG_CONNECTION, conn as isize, CFG_FLOAT, &v as *const f32 as *const u8)
}
unsafe extern "C" fn cfg_conn_str(this: usize, conn: u32, value: i32, v: *const c_char) -> bool {
    utils_set_config(this, value, CFG_CONNECTION, conn as isize, CFG_STRING, v as *const u8)
}

/// `SteamNetworkingConfigValue_t`: the option, its type, and the value (a string's is
/// its pointer).
#[repr(C)]
struct ConfigValue {
    value: i32,
    kind: i32,
    val: u64,
}

unsafe extern "C" fn cfg_struct(this: usize, opt: *const ConfigValue, scope: i32, obj: isize) -> bool {
    if opt.is_null() {
        return false;
    }
    let o = unsafe { &*opt };
    let arg = if o.kind == CFG_STRING { o.val as usize as *const u8 } else { &o.val as *const u64 as *const u8 };
    utils_set_config(this, o.value, scope, obj, o.kind, arg)
}

macro_rules! global_callback {
    ($fn:ident, $value:expr) => {
        unsafe extern "C" fn $fn(this: usize, f: usize) -> bool {
            unsafe { cfg_global_ptr(this, $value, f) }
        }
    };
}

global_callback!(cb_status, 201);
global_callback!(cb_auth, 202);
global_callback!(cb_relay, 203);
global_callback!(cb_session_request, 204);
global_callback!(cb_session_failed, 205);
global_callback!(cb_fake_ip, 207);

fn adapters() -> Vec<(&'static str, usize)> {
    if FLAT_REAL.get().is_none() {
        return Vec::new();
    }
    let id = |new: usize, old: usize| if flat_returns_class() { old } else { new };
    vec![
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalConfigValueInt32", cfg_global_i32 as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalConfigValueFloat", cfg_global_f32 as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalConfigValueString", cfg_global_str as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalConfigValuePtr", cfg_global_ptr as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetConnectionConfigValueInt32", cfg_conn_i32 as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetConnectionConfigValueFloat", cfg_conn_f32 as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetConnectionConfigValueString", cfg_conn_str as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetConfigValueStruct", cfg_struct as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalCallback_SteamNetConnectionStatusChanged", cb_status as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalCallback_SteamNetAuthenticationStatusChanged", cb_auth as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalCallback_SteamRelayNetworkStatusChanged", cb_relay as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalCallback_MessagesSessionRequest", cb_session_request as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalCallback_MessagesSessionFailed", cb_session_failed as *const () as usize),
        ("SteamAPI_ISteamNetworkingUtils_SetGlobalCallback_FakeIPResult", cb_fake_ip as *const () as usize),
        (
            "SteamAPI_ISteamUser_GetSteamID",
            id(flat_user_steam_id as *const () as usize, flat_user_steam_id_old as *const () as usize),
        ),
        (
            "SteamAPI_ISteamGameServer_GetSteamID",
            id(flat_server_steam_id as *const () as usize, flat_server_steam_id_old as *const () as usize),
        ),
        (
            "SteamAPI_ISteamGameServer_CreateUnauthenticatedUserConnection",
            id(flat_unauthenticated as *const () as usize, flat_unauthenticated_old as *const () as usize),
        ),
        (
            "SteamAPI_ISteamMatchmaking_GetLobbyByIndex",
            id(flat_lobby_by_index as *const () as usize, flat_lobby_by_index_old as *const () as usize),
        ),
        (
            "SteamAPI_ISteamMatchmaking_GetLobbyMemberByIndex",
            id(flat_lobby_member as *const () as usize, flat_lobby_member_old as *const () as usize),
        ),
        (
            "SteamAPI_ISteamMatchmaking_GetLobbyOwner",
            id(flat_lobby_owner as *const () as usize, flat_lobby_owner_old as *const () as usize),
        ),
    ]
}

/// Every accessor PeerParty swaps by name: the versioned flat ones (x64), the old
/// `SteamUser()`-style ones and the `SteamInternal_*` ones. The last two are inline-hooked
/// too; the swap covers a build whose export was too short to patch.
fn accessor_swap(name: &str) -> Option<usize> {
    let internal: Option<usize> = match name {
        "SteamInternal_CreateInterface" => Some(detour_create as *const () as usize),
        "SteamInternal_FindOrCreateUserInterface" => Some(detour_find_user as *const () as usize),
        "SteamInternal_FindOrCreateGameServerInterface" => Some(detour_find_server as *const () as usize),
        _ => None,
    };
    if let Some(f) = internal {
        return self::internal(name).map(|_| f);
    }
    if LEGACY.get().is_some_and(|l| l.iter().any(|l| l.export == name)) {
        return LEGACY_FNS.iter().find(|(n, _)| *n == name).map(|(_, f)| *f as usize);
    }
    let (found, base) = FLAT.get()?;
    let i = found.iter().position(|(n, ..)| n == name)?;
    Some(base + i * ACCESSOR_STUB)
}

/// Where accessor stub `i` goes: the real accessor, its object wrapped.
extern "C" fn flat_call(i: usize) -> usize {
    let Some((_, real, version, server)) = FLAT.get().and_then(|(f, _)| f.get(i)) else {
        return 0;
    };
    let f: LegacyFn = unsafe { std::mem::transmute(*real) };
    wrap_as(unsafe { f() }, version, *server)
}

/// One stub per flat accessor: `mov rcx, i; mov rax, flat_call; jmp rax` (x64), or
/// `push i; call flat_call; add esp, 4; ret` (x86, cdecl). Accessors take no arguments,
/// so rcx is free on x64, and flat_call returns straight to the caller.
unsafe fn make_accessor_stubs(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    unsafe {
        exec_block(n, ACCESSOR_STUB, |i| {
            let call = flat_call as *const () as usize;
            if cfg!(target_arch = "x86") {
                let mut c = vec![0x68];
                c.extend((i as u32).to_le_bytes());
                c.push(0xB8);
                c.extend((call as u32).to_le_bytes());
                c.extend([0xFF, 0xD0, 0x83, 0xC4, 0x04, 0xC3]);
                return c;
            }
            let mut c = vec![0x48, 0xB9];
            c.extend((i as u64).to_le_bytes());
            c.extend([0x48, 0xB8]);
            c.extend((call as u64).to_le_bytes());
            c.extend([0xFF, 0xE0]);
            c
        })
    }
}

/// Hooks the interface-handing exports of steam_api. Returns the addresses it patched,
/// for the logging pass to leave alone.
pub fn attach(base: usize, exports: &[Export]) -> Vec<usize> {
    let mut patched = Vec::new();
    let find = |n: &str| exports.iter().find(|e| e.name == n).map(|e| e.addr as usize);
    if let Some(a) = find("SteamInternal_CreateInterface") {
        if arm(&CREATE, a, detour_create as CreateFn) {
            patched.push(a);
        }
    }
    if let Some(a) = find("SteamInternal_FindOrCreateUserInterface") {
        if arm(&FIND_USER, a, detour_find_user as UserFn) {
            patched.push(a);
        }
    }
    if let Some(a) = find("SteamInternal_FindOrCreateGameServerInterface") {
        if arm(&FIND_SERVER, a, detour_find_server as UserFn) {
            patched.push(a);
        }
    }
    let _ = FLAT_ALL.set(
        exports
            .iter()
            .filter(|e| e.name.starts_with("SteamAPI_ISteam"))
            .map(|e| (e.name.clone(), e.addr as usize))
            .collect(),
    );
    // The old accessors take no version: it's the one this steam_api.dll was built
    // with, the only one of its family named in the image. A steam_api that names none
    // (one that keeps its version names some other way) still gets the hook: its
    // object's version is told from its vtable on the first call.
    let image = unsafe { data_sections(base) };
    let mut legacy = Vec::new();
    for (export, fam, f, server) in [
        ("SteamClient", "SteamClient", legacy_client as LegacyFn, false),
        ("SteamUser", "SteamUser", legacy_user as LegacyFn, false),
        ("SteamFriends", "SteamFriends", legacy_friends as LegacyFn, false),
        ("SteamMatchmaking", "SteamMatchMaking", legacy_matchmaking as LegacyFn, false),
        ("SteamNetworking", "SteamNetworking", legacy_networking as LegacyFn, false),
        ("SteamMatchmakingServers", "SteamMatchMakingServers", legacy_matchmaking_servers as LegacyFn, false),
        ("SteamGameServer", "SteamGameServer", legacy_game_server as LegacyFn, true),
        ("SteamGameServerNetworking", "SteamNetworking", legacy_game_server_networking as LegacyFn, true),
        ("SteamUtils", "SteamUtils", legacy_utils as LegacyFn, false),
        ("SteamGameServerUtils", "SteamUtils", legacy_game_server_utils as LegacyFn, true),
        ("SteamUserStats", "STEAMUSERSTATS_INTERFACE_VERSION", legacy_user_stats as LegacyFn, false),
        ("SteamApps", "STEAMAPPS_INTERFACE_VERSION", legacy_apps as LegacyFn, false),
        ("SteamUGC", "STEAMUGC_INTERFACE_VERSION", legacy_ugc as LegacyFn, false),
        ("SteamRemoteStorage", "STEAMREMOTESTORAGE_INTERFACE_VERSION", legacy_remote_storage as LegacyFn, false),
        ("SteamController", "SteamController", legacy_controller as LegacyFn, false),
    ] {
        let Some(a) = find(export) else { continue };
        let named = image.iter().filter_map(|s| newest_named(s, fam)).max();
        let target: LegacyFn = unsafe { std::mem::transmute(a) };
        let hooked = with_lock(|| unsafe {
            let h = GenericDetour::new(target, f).ok()?;
            h.enable().ok()?;
            Some(h)
        });
        if hooked.is_some() {
            patched.push(a);
        }
        // Not hooked (too short to patch): the by-name swap still reaches it.
        legacy.push(Legacy {
            export,
            hook: hooked,
            addr: a,
            family: fam,
            named,
            seen: OnceLock::new(),
            server,
        });
    }
    // Which old accessors there are and how each is reached: a game that fetches its
    // interfaces through one PeerParty can't wrap plays on steam_api's own objects.
    if !legacy.is_empty() {
        let each: Vec<String> = legacy
            .iter()
            .map(|l| {
                let version = l.named.clone().unwrap_or_else(|| "version told from its object".into());
                let how = if l.hook.is_some() { "" } else { ", not hooked (too short): only by name" };
                format!("{} ({version}{how})", l.export)
            })
            .collect();
        log!("steam-lan old accessors: {}", each.join(", "));
    }
    let _ = LEGACY.set(legacy);
    log!("steam-lan interface exports hooked: {}", patched.len());
    patched.extend(crate::steam::gs::attach(exports));
    patched
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, addr: usize, detour: T) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    crate::sys::detour::arm_at(slot, addr, detour, "steam-lan", "detour")
}

/// A module's readable, initialized, non-code sections (where its strings are): not the
/// code, and not the gaps between sections.
unsafe fn data_sections(base: usize) -> Vec<&'static [u8]> {
    const CODE: u32 = 0x20; // IMAGE_SCN_CNT_CODE
    const INITIALIZED: u32 = 0x40; // IMAGE_SCN_CNT_INITIALIZED_DATA
    const EXECUTE: u32 = 0x2000_0000; // IMAGE_SCN_MEM_EXECUTE
    const READ: u32 = 0x4000_0000; // IMAGE_SCN_MEM_READ
    unsafe {
        let nt = base + *((base + 0x3c) as *const u32) as usize;
        let count = *((nt + 6) as *const u16) as usize;
        let optional = *((nt + 20) as *const u16) as usize;
        let first = nt + 24 + optional;
        (0..count)
            .filter_map(|i| {
                let h = first + i * 40;
                let size = *((h + 8) as *const u32) as usize; // VirtualSize
                let rva = *((h + 12) as *const u32) as usize;
                let raw = *((h + 16) as *const u32) as usize; // SizeOfRawData
                let flags = *((h + 36) as *const u32);
                let data = flags & INITIALIZED != 0 && flags & READ != 0 && flags & (CODE | EXECUTE) == 0;
                // Past SizeOfRawData a section is zeros: nothing to find there.
                let len = size.min(raw);
                (data && len > 0).then(|| std::slice::from_raw_parts((base + rva) as *const u8, len))
            })
            .collect()
    }
}

/// The version an old accessor's object `real` is: the one steam_api names, else the
/// one its vtable matches (once, logged).
fn legacy_version(l: &Legacy, real: usize) -> Option<&str> {
    if real == 0 {
        return l.named.as_deref();
    }
    *l.seen.get_or_init(|| {
        // The object's own class says it best: a steam_api that holds several versions'
        // names, or none, still builds each version as a class of its own.
        let rtti = version_from_rtti(real, l.family);
        if let Ok(v) = rtti {
            log!("steam-lan {}: its object's class is {v}'s, so it's taken as {v}", l.export);
            return Some(v);
        }
        if let Some(v) = l.named.as_deref().and_then(layout).map(|(v, _)| v) {
            return Some(v);
        }
        let flat = FLAT_ALL.get();
        let v = flat.and_then(|flat| {
            version_from_vtable(l.family, flat, |i| unsafe {
                let vt = *(real as *const usize);
                let p = vt + i * size_of::<usize>();
                crate::sys::read::readable(p, size_of::<usize>()).then(|| *(p as *const usize))
            })
        });
        if let Some(v) = v {
            log!("steam-lan {}: its object's methods are steam_api's own in {v}'s order, so it's taken as {v}", l.export);
            return Some(v);
        }
        let why = rtti.err().unwrap_or_default();
        log!("steam-lan {}: can't tell which {} version its object is ({why}), so it's left as is", l.export, l.family);
        None
    })
}

/// The family a class name's stem stands for: "SteamMatchmaking" (from
/// "SteamMatchmaking9", "ISteamMatchmaking009", "CSteamMatchmaking009").
fn family_of_class(stem: &str) -> Option<&'static str> {
    let stem = stem.strip_prefix('I').or_else(|| stem.strip_prefix('C')).filter(|s| s.starts_with("Steam")).unwrap_or(stem);
    Some(match stem.to_ascii_lowercase().as_str() {
        "steamclient" => "SteamClient",
        "steamuser" => "SteamUser",
        "steamfriends" => "SteamFriends",
        "steammatchmaking" => "SteamMatchMaking",
        "steammatchmakingservers" => "SteamMatchMakingServers",
        "steamnetworking" => "SteamNetworking",
        "steamgameserver" => "SteamGameServer",
        "steamutils" => "SteamUtils",
        "steamuserstats" => "STEAMUSERSTATS_INTERFACE_VERSION",
        "steamapps" => "STEAMAPPS_INTERFACE_VERSION",
        "steamugc" => "STEAMUGC_INTERFACE_VERSION",
        "steamremotestorage" => "STEAMREMOTESTORAGE_INTERFACE_VERSION",
        "steamcontroller" => "SteamController",
        _ => return None,
    })
}

/// The interface version a decorated MSVC class name (".?AVSteamMatchmaking9@@") stands
/// for, when it names one of PeerParty's layouts.
fn version_of_class(decorated: &str) -> Option<&'static str> {
    let name = decorated.strip_prefix(".?AV").or_else(|| decorated.strip_prefix(".?AU"))?;
    let name = name.split('@').next()?;
    let digits_at = name.find(|c: char| c.is_ascii_digit())?;
    let (stem, digits) = name.split_at(digits_at);
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let family = family_of_class(stem)?;
    layout(&format!("{family}{digits:0>3}")).map(|(v, _)| v)
}

/// The version of `family` an MSVC-built object says it is: its class's name, or a base
/// class's, from the RTTI beside its vtable ("SteamMatchmaking9" is SteamMatchMaking009).
/// Taken only when exactly one version of `family` is named, and, where the vtable's end
/// can be seen (the next vtable's RTTI), when its length is that version's.
fn version_from_rtti(real: usize, family: &str) -> Result<&'static str, String> {
    let names = unsafe { rtti::class_names(real) }.ok_or("no MSVC RTTI behind its vtable")?;
    let mut found: Vec<&'static str> = names.iter().filter_map(|n| version_of_class(n)).filter(|v| self::family(v) == family).collect();
    found.sort_unstable();
    found.dedup();
    let v = match found.as_slice() {
        [v] => *v,
        [] => return Err(format!("its class ({}) names no {family} version", names.first().map_or("?", |n| n.as_str()))),
        many => return Err(format!("its classes name {}", many.join(", "))),
    };
    let want = layout(v).map_or(0, |(_, n)| n.len());
    match unsafe { rtti::vtable_len(real) } {
        Some(n) if n != want => Err(format!("its class is {v}'s, but its vtable has {n} methods, not {want}")),
        _ => Ok(v),
    }
}

/// MSVC's run-time type information, read with every pointer checked: the Complete
/// Object Locator in the slot before a vtable leads to the class's name and its bases'.
mod rtti {
    use crate::sys::read::{cstr, readable};

    const P: usize = size_of::<usize>();

    unsafe fn word(p: usize) -> Option<usize> {
        readable(p, P).then(|| unsafe { *(p as *const usize) })
    }

    unsafe fn u32_at(p: usize) -> Option<u32> {
        readable(p, 4).then(|| unsafe { *(p as *const u32) })
    }

    /// x64 locators hold image-relative offsets (signature 1, with the locator's own
    /// offset to find the image); x86 ones hold pointers (signature 0).
    unsafe fn at(p: usize, field: usize, image: usize) -> Option<usize> {
        let v = unsafe { u32_at(p + field) }? as usize;
        Some(if cfg!(target_pointer_width = "64") { image + v } else { v })
    }

    /// The image a locator at `col` belongs to, when `col` is one.
    unsafe fn locator(col: usize) -> Option<usize> {
        let sig = unsafe { u32_at(col) }?;
        if cfg!(target_pointer_width = "64") {
            let own = unsafe { u32_at(col + 20) }? as usize;
            let image = col.checked_sub(own)?;
            (sig == 1 && readable(image, 2) && unsafe { *(image as *const u16) } == 0x5A4D).then_some(image)
        } else {
            (sig == 0).then_some(0)
        }
    }

    unsafe fn type_name(td: usize) -> Option<String> {
        let name = cstr(td + 2 * P)?;
        name.starts_with(".?A").then_some(name)
    }

    /// The decorated names of the object's class and its bases (".?AVSteamUser16@@").
    pub unsafe fn class_names(obj: usize) -> Option<Vec<String>> {
        unsafe {
            let vt = word(obj)?;
            let col = word(vt.checked_sub(P)?)?;
            let image = locator(col)?;
            let mut out = vec![type_name(at(col, 12, image)?)?];
            let hierarchy = at(col, 16, image)?;
            let n = u32_at(hierarchy + 8)?.min(64) as usize;
            let bases = at(hierarchy, 12, image)?;
            for i in 0..n {
                let entry = at(bases, 4 * i, image)?;
                if let Some(name) = at(entry, 0, image).and_then(|td| type_name(td)) {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
            }
            Some(out)
        }
    }

    /// How many methods the object's vtable has: up to the next vtable's locator. None
    /// when that can't be seen (no locator within reach), and on x86, whose locators
    /// (signature 0, no offset to themselves) can't be told from other data.
    pub unsafe fn vtable_len(obj: usize) -> Option<usize> {
        if cfg!(target_pointer_width = "32") {
            return None;
        }
        unsafe {
            let vt = word(obj)?;
            for i in 0..256 {
                let e = word(vt + i * P)?;
                if readable(e, 24) && locator(e).is_some() {
                    return Some(i);
                }
            }
            None
        }
    }
}

/// steam_api's flat-function prefix for a version family's methods.
fn flat_iface(family: &str) -> Option<&'static str> {
    Some(match family {
        "SteamClient" => "ISteamClient",
        "SteamUser" => "ISteamUser",
        "SteamFriends" => "ISteamFriends",
        "SteamMatchMaking" => "ISteamMatchmaking",
        "SteamMatchMakingServers" => "ISteamMatchmakingServers",
        "SteamNetworking" => "ISteamNetworking",
        "SteamGameServer" => "ISteamGameServer",
        "SteamUtils" => "ISteamUtils",
        "STEAMUSERSTATS_INTERFACE_VERSION" => "ISteamUserStats",
        "STEAMAPPS_INTERFACE_VERSION" => "ISteamApps",
        "STEAMUGC_INTERFACE_VERSION" => "ISteamUGC",
        "STEAMREMOTESTORAGE_INTERFACE_VERSION" => "ISteamRemoteStorage",
        "SteamController" => "ISteamController",
        _ => return None,
    })
}

/// The version of `family` whose slot order an object's vtable follows, for a
/// steam_api whose objects' vtables hold the very functions it exports flat (slot i is
/// `SteamAPI_ISteamMatchmaking_CreateLobby` where the version has CreateLobby). `slot(i)`
/// reads the object's slot i (None past readable memory). A version counts when most of
/// the slots it has a flat function for hold that function; the best one wins, and of
/// equals the oldest (whose changed signatures are forwarded, not served). None when
/// none counts: a steam_api whose flat functions only call into its objects (Steam's)
/// leaves the object as is.
fn version_from_vtable(family: &str, flat: &HashMap<String, usize>, slot: impl Fn(usize) -> Option<usize>) -> Option<&'static str> {
    let iface = flat_iface(family)?;
    let mut best: Option<(i64, &'static str)> = None;
    for (v, names) in tables().filter(|(v, _)| self::family(v) == family) {
        let (mut hit, mut miss) = (0i64, 0i64);
        for (i, n) in names.iter().enumerate() {
            let method = n.split('~').next().unwrap_or(n);
            let Some(&f) = flat.get(&format!("SteamAPI_{iface}_{method}")) else {
                continue;
            };
            if slot(i) == Some(f) {
                hit += 1;
            } else {
                miss += 1;
            }
        }
        if hit >= 3 && hit > miss && best.is_none_or(|(s, _)| hit - miss > s) {
            best = Some((hit - miss, v));
        }
    }
    best.map(|(_, v)| v)
}

/// The highest "<family>NNN" string in `image`, e.g. "SteamFriends015". A match that
/// isn't one (too near the end, more digits) is skipped, not the end of the search.
fn newest_named(image: &[u8], family: &str) -> Option<String> {
    let pat = family.as_bytes();
    let mut best: Option<String> = None;
    for i in memchr_all(image, pat) {
        let Some(digits) = image.get(i + pat.len()..i + pat.len() + 3) else {
            continue;
        };
        let end = image.get(i + pat.len() + 3).copied();
        if digits.iter().all(u8::is_ascii_digit) && end == Some(0) {
            let v = format!("{family}{}", String::from_utf8_lossy(digits));
            if layout(&v).is_some() && best.as_ref().is_none_or(|b| v > *b) {
                best = Some(v);
            }
        }
    }
    best
}

fn memchr_all<'a>(hay: &'a [u8], needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    hay.windows(needle.len())
        .enumerate()
        .filter(move |(_, w)| *w == needle)
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A steam_api whose flat functions are its objects' methods: each method name gets
    /// an address, and a version's vtable holds those in its order.
    fn flat_for(family_iface: &str, versions: &[&str]) -> HashMap<String, usize> {
        let mut flat = HashMap::new();
        for v in versions {
            for n in layout(v).unwrap().1 {
                let method = n.split('~').next().unwrap();
                let next = 0x1000 + flat.len() * 16;
                flat.entry(format!("SteamAPI_{family_iface}_{method}")).or_insert(next);
            }
        }
        flat
    }

    fn vtable_of(version: &str, iface: &str, flat: &HashMap<String, usize>) -> Vec<usize> {
        layout(version)
            .unwrap()
            .1
            .iter()
            .map(|n| flat[&format!("SteamAPI_{iface}_{}", n.split('~').next().unwrap())])
            .collect()
    }

    #[test]
    fn an_unnamed_version_is_told_from_the_vtable() {
        let all: Vec<&str> = tables().map(|(v, _)| v).filter(|v| family(v) == "SteamMatchMaking").collect();
        let flat = flat_for("ISteamMatchmaking", &all);
        for v in ["SteamMatchMaking009", "SteamMatchMaking008", "SteamMatchMaking006"] {
            let vt = vtable_of(v, "ISteamMatchmaking", &flat);
            assert_eq!(version_from_vtable("SteamMatchMaking", &flat, |i| vt.get(i).copied()), Some(v), "{v}");
        }
        let users: Vec<&str> = tables().map(|(v, _)| v).filter(|v| family(v) == "SteamUser").collect();
        let flat = flat_for("ISteamUser", &users);
        let vt = vtable_of("SteamUser019", "ISteamUser", &flat);
        let got = version_from_vtable("SteamUser", &flat, |i| vt.get(i).copied()).unwrap();
        // Versions that differ only in a changed signature look alike; the oldest is
        // taken, so the changed method is forwarded rather than served.
        assert_eq!(layout(got).unwrap().1.len(), layout("SteamUser019").unwrap().1.len());
        assert!(got <= "SteamUser019");
    }

    #[test]
    fn a_class_name_says_its_version() {
        assert_eq!(version_of_class(".?AVSteamMatchmaking9@@"), Some("SteamMatchMaking009"));
        assert_eq!(version_of_class(".?AVISteamFriends015@@"), Some("SteamFriends015"));
        assert_eq!(version_of_class(".?AVSteamUserStats11@@"), Some("STEAMUSERSTATS_INTERFACE_VERSION011"));
        assert_eq!(version_of_class(".?AVSteamApps5@@"), Some("STEAMAPPS_INTERFACE_VERSION005"));
        assert_eq!(version_of_class(".?AVCSteamUser16@ns@@"), Some("SteamUser016"));
        // Steam's own classes carry no version; a version PeerParty has no layout for
        // isn't one; other classes aren't Steam's.
        assert_eq!(version_of_class(".?AVCSteamMatchmaking@@"), None);
        assert_eq!(version_of_class(".?AVSteamUser99@@"), None);
        assert_eq!(version_of_class(".?AVPlayer2@@"), None);
        assert_eq!(version_of_class("SteamUser16"), None);
    }

    #[test]
    fn a_vtable_of_other_functions_is_left_alone() {
        let flat = flat_for("ISteamMatchmaking", &["SteamMatchMaking009"]);
        // Steam's own objects: the flat functions only call into them.
        assert_eq!(version_from_vtable("SteamMatchMaking", &flat, |i| Some(0x9000_0000 + i * 8)), None);
        assert_eq!(version_from_vtable("SteamMatchMaking", &flat, |_| None), None);
        assert_eq!(version_from_vtable("SteamUnknown", &flat, |_| None), None);
    }

    #[test]
    fn layouts_have_the_lobby_and_p2p_methods_where_steam_puts_them() {
        assert_eq!(slot("SteamMatchMaking009", "CreateLobby"), Some(13));
        assert_eq!(slot("SteamMatchMaking009", "GetLobbyOwner"), Some(35));
        assert_eq!(slot("SteamNetworking005", "SendP2PPacket"), Some(0));
        assert_eq!(slot("SteamNetworking005", "ReadP2PPacket"), Some(2));
        assert_eq!(slot("SteamUser019", "GetSteamID"), Some(2));
        assert_eq!(slot("SteamClient017", "GetISteamMatchmaking"), Some(10));
        assert_eq!(slot("SteamFriends015", "ActivateGameOverlayInviteDialog"), Some(33));
        assert_eq!(slot("SteamNetworkingSockets013", "ConnectP2P"), Some(3));
        assert_eq!(slot("SteamNetworkingMessages002", "SendMessageToUser"), Some(0));
        assert!(slot("SteamNetworkingUtils004", "SetConfigValue").is_some());
        // A changed signature is left alone, or taken by its older shape's name.
        assert_eq!(slot("SteamUser019", "GetAuthSessionTicket"), None);
        assert!(slot("SteamNetworkingSockets008", "SendMessages~3").is_some());
        // ISteamUtils's call results: slots 11-13 until GetCSERIPPort went in 011.
        assert_eq!(slot("SteamUtils005", "IsAPICallCompleted"), Some(11));
        assert_eq!(slot("SteamUtils005", "GetAPICallResult"), Some(13));
        assert_eq!(slot("SteamUtils010", "GetAPICallFailureReason"), Some(12));
        assert_eq!(slot("SteamUtils011", "IsAPICallCompleted"), Some(10));
        assert_eq!(slot("SteamClient010", "GetISteamUtils"), Some(9));
        assert!(slot("SteamNetworkingSockets004", "ConnectP2P~2").is_some());
        assert!(tables().all(|(_, n)| n.len() <= vtable::MAX_SLOTS));
        // The C++ interfaces steamown and Steam Input's gating stand in front of.
        for v in ["SteamInput001", "SteamInput006", "SteamController003", "SteamController008"] {
            assert!(slot(v, "GetDigitalActionData").is_some() && slot(v, "GetAnalogActionData").is_some(), "{v}");
        }
        assert!(slot("STEAMAPPS_INTERFACE_VERSION008", "GetAppOwner").is_some());
        assert!(slot("STEAMUGC_INTERFACE_VERSION016", "CreateQueryUserUGCRequest").is_some());
        // GetUserStat's two overloads: two slots of one name, told apart by `@N`.
        let a = slot("STEAMUSERSTATS_INTERFACE_VERSION012", "GetUserStat~3@0").unwrap();
        let b = slot("STEAMUSERSTATS_INTERFACE_VERSION012", "GetUserStat~3@1").unwrap();
        assert_eq!(b, a + 1);
        assert_eq!(slot("STEAMUSERSTATS_INTERFACE_VERSION012", "GetUserStat~3"), Some(a));
        assert_eq!(slot("STEAMUSERSTATS_INTERFACE_VERSION012", "GetUserStat~3@2"), None);
        assert!(slot("STEAMUSERSTATS_INTERFACE_VERSION012", "GetUserAchievementAndUnlockTime").is_some());
    }

    // A fake ISteamNetworking005: each method returns the `this` it was given plus
    // 100 times its slot plus its argument.
    vmethod! { fn fake_slot_20(this: usize, a: u32) -> usize { this.wrapping_add(2000 + a as usize) } }
    vmethod! { fn fake_slot_21(this: usize, a: u32) -> usize { this.wrapping_add(2100 + a as usize) } }

    /// A game server on the game's own pipe can be handed the object the player has:
    /// the server gets a stand-in of its own (its P2P sessions are the server's).
    #[test]
    fn one_real_object_gets_a_players_and_a_servers_stand_in() {
        unsafe { std::env::set_var("PEERPARTY_ACTIVE", "1") };
        crate::config::reload();
        let vt = vec![0usize; layout("SteamNetworking006").unwrap().1.len()];
        let real = Box::leak(Box::new(Box::leak(vt.into_boxed_slice()).as_ptr() as usize)) as *const usize as usize;
        let player = wrap_as(real, "SteamNetworking006", false);
        let server = wrap_as(real, "SteamNetworking006", true);
        assert!(player != real && server != real && player != server);
        assert!(!is_server(player) && is_server(server));
        assert_eq!(wrap_as(real, "SteamNetworking006", true), server, "same server stand-in again");
        assert_eq!((unwrap(player), unwrap(server)), (real, real));
        let first = |p: usize| unsafe { **(p as *const *const usize) };
        assert_eq!(first(player), networking()[0].1, "the player's SendP2PPacket");
        assert_eq!(first(server), crate::steam::p2p::server_networking()[0].1, "the server's SendP2PPacket");
    }

    #[test]
    fn stand_ins_forward_other_methods_to_the_real_object() {
        unsafe { std::env::set_var("PEERPARTY_ACTIVE", "1") };
        crate::config::reload();
        let n = layout("SteamNetworking005").unwrap().1.len();
        let mut vt = vec![0usize; n];
        vt[20] = fake_slot_20 as *const () as usize;
        vt[21] = fake_slot_21 as *const () as usize;
        // The real object: just a vtable pointer. Use a small address-like key for `this`.
        let real = Box::leak(Box::new(vt.as_ptr() as usize)) as *const usize as usize;
        let proxy = wrap(real, "SteamNetworking005");
        assert_ne!(proxy, real, "wrapped");
        assert_eq!(wrap(real, "SteamNetworking005"), proxy, "same stand-in again");
        assert_eq!(unwrap(proxy), real);
        let pvt = unsafe { *(proxy as *const *const usize) };
        // Slot 0 is PeerParty's SendP2PPacket, not the real one.
        assert_eq!(unsafe { *pvt }, networking()[0].1);
        #[cfg(target_arch = "x86")]
        type F = unsafe extern "thiscall" fn(usize, u32) -> usize;
        #[cfg(not(target_arch = "x86"))]
        type F = unsafe extern "C" fn(usize, u32) -> usize;
        for slot in [20usize, 21] {
            let f: F = unsafe { std::mem::transmute(*pvt.add(slot)) };
            let got = unsafe { f(proxy, 7) };
            assert_eq!(got, real.wrapping_add(slot * 100 + 7), "slot {slot} reached the real method with the real this");
        }
        std::mem::forget(vt);
    }

    /// Every ISteamUser's auth ticket goes through PeerParty, whichever shape it has:
    /// one that didn't went out without PeerParty's proof, and the host refused it.
    #[test]
    fn every_steam_user_version_has_its_ticket_answered() {
        let mine = crate::steam::p2p::user();
        for (version, names) in tables().filter(|(v, _)| v.starts_with("SteamUser")) {
            let ticket = names.iter().find(|n| n.starts_with("GetAuthSessionTicket"));
            if let Some(ticket) = ticket {
                assert!(mine.iter().any(|(m, _)| m == ticket), "{version}'s {ticket} isn't answered");
            }
        }
    }

    fn networking() -> Vec<(&'static str, usize)> {
        crate::steam::p2p::networking()
    }

    // A fake ISteamInput006's GetDigitalActionData (C++: the struct through a pointer
    // after `this`) and two GetUserStat overloads that say which slot they are.
    vmethod! { fn fake_digital(_this: usize, out: *mut u8, _i: u64, _a: u64) -> *mut u8 { unsafe { *out = 1; *out.add(1) = 1 }; out } }
    vmethod! { fn fake_stat_0(_this: usize, _id: u64, _n: *const c_char, out: *mut u8) -> bool { unsafe { *out = 10 }; true } }
    vmethod! { fn fake_stat_1(_this: usize, _id: u64, _n: *const c_char, out: *mut u8) -> bool { unsafe { *out = 11 }; true } }

    #[test]
    fn cpp_steam_input_reads_rest_and_overloads_keep_their_slots() {
        let _turn = crate::overlay::test_turn();
        unsafe { std::env::set_var("PEERPARTY_ACTIVE", "1") };
        crate::config::reload();
        #[cfg(target_arch = "x86")]
        type Digital = unsafe extern "thiscall" fn(usize, *mut u8, u64, u64) -> *mut u8;
        #[cfg(not(target_arch = "x86"))]
        type Digital = unsafe extern "C" fn(usize, *mut u8, u64, u64) -> *mut u8;
        #[cfg(target_arch = "x86")]
        type Stat = unsafe extern "thiscall" fn(usize, u64, *const c_char, *mut u8) -> bool;
        #[cfg(not(target_arch = "x86"))]
        type Stat = unsafe extern "C" fn(usize, u64, *const c_char, *mut u8) -> bool;

        let v = "SteamInput006";
        let mut vt = vec![0usize; layout(v).unwrap().1.len()];
        let at = slot(v, "GetDigitalActionData").unwrap();
        vt[at] = fake_digital as *const () as usize;
        let real = Box::leak(Box::new(vt.as_ptr() as usize)) as *const usize as usize;
        let proxy = wrap(real, v);
        assert_ne!(proxy, real, "ISteamInput is a stand-in");
        let pvt = unsafe { *(proxy as *const *const usize) };
        let f: Digital = unsafe { std::mem::transmute(*pvt.add(at)) };
        let mut d = [0u8; 2];
        unsafe { f(proxy, d.as_mut_ptr(), 1, 2) };
        assert_eq!(d, [1, 1]);
        crate::overlay::set_visible_for_test(true);
        unsafe { f(proxy, d.as_mut_ptr(), 1, 2) };
        crate::overlay::set_visible_for_test(false);
        assert_eq!(d, [0, 1], "released while the overlay shows, still active");

        let v = "STEAMUSERSTATS_INTERFACE_VERSION012";
        let mut vt = vec![0usize; layout(v).unwrap().1.len()];
        let (a, b) = (slot(v, "GetUserStat~3@0").unwrap(), slot(v, "GetUserStat~3@1").unwrap());
        vt[a] = fake_stat_0 as *const () as usize;
        vt[b] = fake_stat_1 as *const () as usize;
        let real = Box::leak(Box::new(vt.as_ptr() as usize)) as *const usize as usize;
        let proxy = wrap(real, v);
        assert_ne!(proxy, real, "ISteamUserStats is a stand-in");
        let pvt = unsafe { *(proxy as *const *const usize) };
        for (i, want) in [(a, 10u8), (b, 11)] {
            let f: Stat = unsafe { std::mem::transmute(*pvt.add(i)) };
            assert_ne!(unsafe { *pvt.add(i) }, crate::sys::vtable::forward_stub(i).unwrap(), "PeerParty's method in slot {i}");
            let mut out = 0u8;
            assert!(unsafe { f(proxy, 5, c"x".as_ptr(), &mut out) });
            assert_eq!(out, want, "slot {i} reached its own real overload");
        }
        std::mem::forget(vt);
    }

    #[test]
    fn the_older_flat_api_is_the_one_without_versioned_accessors() {
        let class = |names: &[&str]| returns_class_flat(names.iter().copied());
        assert!(class(&["SteamAPI_Init", "SteamAPI_ISteamUser_GetSteamID", "SteamAPI_ISteamClient_GetISteamUser"]));
        assert!(!class(&["SteamAPI_ISteamUser_GetSteamID", "SteamAPI_SteamUser_v020"]));
        // Any interface's accessor counts, ones PeerParty leaves alone too.
        assert!(!class(&["SteamAPI_SteamApps_v008"]));
        assert!(!class(&["SteamAPI_SteamNetworkingSockets_SteamAPI_v012"]));
        assert!(class(&["SteamAPI_SteamUser_vX", "SteamAPI_SteamUser_v"]));
    }

    unsafe extern "C" fn old_flat_id(out: *mut u64, this: usize, i: i32) -> *mut u64 {
        unsafe { *out = this as u64 * 10 + i as u64 };
        out
    }

    unsafe extern "C" fn new_flat_id(this: usize, i: i32) -> u64 {
        this as u64 * 10 + i as u64
    }

    #[test]
    fn flat_ids_are_read_the_way_each_flat_api_returns_them() {
        let old = old_flat_id as *const () as usize;
        let new = new_flat_id as *const () as usize;
        assert_eq!(call_flat_id!(true, old, 7, 3 => i32), 73);
        assert_eq!(call_flat_id!(false, new, 7, 3 => i32), 73);
        // The game's own call into the older flat API gets the ID at its pointer.
        let mut out = 99u64;
        let p: *mut u64 = &mut out;
        assert_eq!(unsafe { flat_lobby_by_index_old(p, 0, 2) }, p);
        assert_eq!(out, 0);
    }

    #[test]
    fn flat_accessors_name_their_interface_version() {
        let got = |n: &str| flat_accessor(n);
        assert_eq!(got("SteamAPI_SteamUser_v023"), Some(("SteamUser023".into(), false)));
        assert_eq!(got("SteamAPI_SteamMatchmaking_v009"), Some(("SteamMatchMaking009".into(), false)));
        assert_eq!(got("SteamAPI_SteamNetworkingSockets_SteamAPI_v012"), Some(("SteamNetworkingSockets012".into(), false)));
        assert_eq!(got("SteamAPI_SteamNetworkingSockets_v008"), Some(("SteamNetworkingSockets008".into(), false)));
        assert_eq!(got("SteamAPI_SteamGameServer_v015"), Some(("SteamGameServer015".into(), true)));
        assert_eq!(got("SteamAPI_SteamGameServerNetworking_v006"), Some(("SteamNetworking006".into(), true)));
        assert_eq!(got("SteamAPI_SteamUtils_v010"), Some(("SteamUtils010".into(), false)));
        assert_eq!(got("SteamAPI_SteamGameServerUtils_v010"), Some(("SteamUtils010".into(), true)));
        assert_eq!(
            got("SteamAPI_SteamGameServerNetworkingMessages_SteamAPI_v002"),
            Some(("SteamNetworkingMessages002".into(), true))
        );
        // Not accessors, or interfaces PeerParty leaves alone.
        assert_eq!(got("SteamAPI_SteamApps_v008"), None);
        assert_eq!(got("SteamAPI_ISteamUser_GetSteamID"), None);
        assert_eq!(got("SteamAPI_SteamUser_vX"), None);
        // Every client accessor names a version PeerParty has a layout for.
        for v in ["SteamUser023", "SteamFriends017", "SteamMatchMaking009", "SteamNetworking006", "SteamUtils010"] {
            assert!(layout(v).is_some(), "{v}");
        }
    }

    #[test]
    fn finds_the_version_an_old_steam_api_was_built_with() {
        let img = b"..SteamFriends014\0..SteamFriends015\0..SteamFriends0155..SteamUser019\0";
        assert_eq!(newest_named(img, "SteamFriends").as_deref(), Some("SteamFriends015"));
        assert_eq!(newest_named(img, "SteamUser").as_deref(), Some("SteamUser019"));
        assert_eq!(newest_named(img, "SteamNetworking"), None);
        // A family name too near the end doesn't hide the versions before it.
        let img = b"..SteamUser019\0..SteamUser01";
        assert_eq!(newest_named(img, "SteamUser").as_deref(), Some("SteamUser019"));
        let img = b"..SteamUser019\0..SteamUser";
        assert_eq!(newest_named(img, "SteamUser").as_deref(), Some("SteamUser019"));
    }

    #[test]
    fn this_modules_data_sections_hold_its_strings() {
        unsafe extern "system" {
            fn GetModuleHandleW(name: *const u16) -> usize;
        }
        let base = unsafe { GetModuleHandleW(std::ptr::null()) };
        let s = unsafe { data_sections(base) };
        assert!(!s.is_empty());
        let marker = std::hint::black_box("SteamFriends015\0");
        assert!(s.iter().any(|d| newest_named(d, "SteamFriends").is_some()), "{marker}");
    }
}
