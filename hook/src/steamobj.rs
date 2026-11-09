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
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;
use crate::steamvt::TABLES;

/// Defines a C++ virtual method for a stand-in vtable.
macro_rules! vmethod {
    ($(#[$m:meta])* fn $name:ident($($a:ident: $t:ty),* $(,)?) $(-> $r:ty)? $body:block) => {
        $(#[$m])*
        #[cfg(target_arch = "x86")]
        unsafe extern "thiscall" fn $name($($a: $t),*) $(-> $r)? $body
        $(#[$m])*
        #[cfg(not(target_arch = "x86"))]
        unsafe extern "C" fn $name($($a: $t),*) $(-> $r)? $body
    };
}
pub(crate) use vmethod;

/// Calls the real object's method `name` for a stand-in `this`, or gives `default`
/// when this version has no such method.
macro_rules! call_real {
    ($this:expr, $name:literal, fn($($t:ty),*) -> $r:ty, $default:expr $(, $arg:expr)* $(,)?) => {{
        match unsafe { $crate::steamobj::real_method($this, $name) } {
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

/// real object -> stand-in, so a game asking twice gets the same object.
static PROXIES: LazyLock<Mutex<HashMap<usize, usize>>> = LazyLock::new(Mutex::default);
/// stand-in -> real.
static REALS: LazyLock<Mutex<HashMap<usize, usize>>> = LazyLock::new(Mutex::default);
/// Real objects by family ("SteamUser"), newest first seen, for PeerParty's own calls.
static SEEN: LazyLock<Mutex<HashMap<&'static str, (usize, &'static str)>>> =
    LazyLock::new(Mutex::default);

const MAX_SLOTS: usize = 128;
const STUB: usize = 16;

/// Every slot's forwarding stub, in one executable block.
fn stubs() -> Option<usize> {
    static BASE: OnceLock<usize> = OnceLock::new();
    let base = *BASE.get_or_init(|| unsafe { make_stubs() });
    (base != 0).then_some(base)
}

unsafe fn make_stubs() -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
    };
    let size = MAX_SLOTS * STUB;
    let p = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    } as *mut u8;
    if p.is_null() {
        return 0;
    }
    for i in 0..MAX_SLOTS {
        let disp = ((i * size_of::<usize>()) as u32).to_le_bytes();
        #[cfg(target_arch = "x86_64")]
        // mov rcx,[rcx+8]; mov rax,[rcx]; jmp [rax+disp32]
        let code: Vec<u8> = [&[0x48, 0x8B, 0x49, 0x08, 0x48, 0x8B, 0x01, 0xFF, 0xA0][..], &disp].concat();
        #[cfg(target_arch = "x86")]
        // mov ecx,[ecx+4]; mov eax,[ecx]; jmp [eax+disp32]
        let code: Vec<u8> = [&[0x8B, 0x49, 0x04, 0x8B, 0x01, 0xFF, 0xA0][..], &disp].concat();
        unsafe {
            std::ptr::write_bytes(p.add(i * STUB), 0xCC, STUB);
            std::ptr::copy_nonoverlapping(code.as_ptr(), p.add(i * STUB), code.len());
        }
    }
    let mut old = 0;
    unsafe { VirtualProtect(p.cast(), size, PAGE_EXECUTE_READ, &mut old) };
    p as usize
}

/// The stub that forwards vtable slot `i` to the object at `this + pointer size`, for
/// other front ends' stand-ins (galaxy.rs).
pub fn forward_stub(i: usize) -> Option<usize> {
    (i < MAX_SLOTS).then(|| stubs().map(|b| b + i * STUB)).flatten()
}

/// "SteamMatchMaking009" -> "SteamMatchMaking".
fn family(version: &str) -> &str {
    version.trim_end_matches(|c: char| c.is_ascii_digit())
}

fn layout(version: &str) -> Option<(&'static str, &'static [&'static str])> {
    TABLES
        .iter()
        .find(|(v, _)| *v == version)
        .map(|(v, names)| (*v, *names))
}

/// The slot of `name` in `version`, when it has the newest signature.
pub fn slot(version: &str, name: &str) -> Option<usize> {
    layout(version)?.1.iter().position(|n| *n == name)
}

/// PeerParty's methods for an interface version, for a client or the game server.
fn overrides(family: &str, version: &str, server: bool) -> Vec<(&'static str, usize)> {
    match family {
        "SteamNetworking" if server => crate::steamp2p::server_networking(),
        "SteamGameServer" => crate::steamgs::server(),
        "SteamMatchMakingServers" => crate::steamgs::browser(),
        "SteamClient" => client(),
        "SteamUser" => crate::steamp2p::user(),
        "SteamFriends" => crate::steamlan::friends(),
        "SteamMatchMaking" => crate::steammm::matchmaking(),
        "SteamNetworking" => crate::steamp2p::networking(),
        "SteamNetworkingSockets" => crate::steamgns::sockets(version),
        "SteamNetworkingMessages" => crate::steamgns::messages(),
        "SteamNetworkingUtils" => crate::steamgns::utils(),
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
    if REALS.lock().unwrap().contains_key(&real) {
        return real;
    }
    if let Some(p) = PROXIES.lock().unwrap().get(&real) {
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
    let (Some(stubs), true) = (stubs(), names.len() <= MAX_SLOTS) else {
        return real;
    };
    let mut ours = 0;
    let vtable: Vec<usize> = names
        .iter()
        .enumerate()
        .map(|(i, n)| match mine.iter().find(|(m, _)| m == n) {
            Some((_, f)) => {
                ours += 1;
                *f
            }
            None => stubs + i * STUB,
        })
        .collect();
    let vtable = Box::leak(vtable.into_boxed_slice()).as_ptr();
    let proxy = Box::leak(Box::new(Proxy {
        vtable,
        real,
        version,
        server,
    })) as *const Proxy as usize;
    PROXIES.lock().unwrap().insert(real, proxy);
    REALS.lock().unwrap().insert(proxy, real);
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

/// The real object behind a stand-in (anything else comes back as is).
pub fn unwrap(p: usize) -> usize {
    REALS.lock().unwrap().get(&p).copied().unwrap_or(p)
}

/// (real object, real method) for a stand-in's method `name`.
pub unsafe fn real_method(this: usize, name: &str) -> Option<(usize, usize)> {
    if !REALS.lock().unwrap().contains_key(&this) {
        return None;
    }
    let proxy = unsafe { &*(this as *const Proxy) };
    let i = slot(proxy.version, name)?;
    let vtable = unsafe { *(proxy.real as *const *const usize) };
    Some((proxy.real, unsafe { *vtable.add(i) }))
}

/// Whether `this` is one of our stand-ins for a game server's interface (one the game
/// fetched on the game server's pipe, or through the game server's accessors).
pub fn is_server(this: usize) -> bool {
    REALS.lock().unwrap().contains_key(&this) && unsafe { (*(this as *const Proxy)).server }
}

/// A real interface the game has fetched (family "SteamUser"), as its stand-in, for
/// PeerParty's own calls into it.
pub fn seen(family: &str) -> Option<usize> {
    let (real, _) = *SEEN.lock().unwrap().get(family)?;
    PROXIES.lock().unwrap().get(&real).copied()
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
    ]
}

macro_rules! client_getter {
    ($fn:ident, $name:literal) => {
        vmethod! {
            fn $fn(this: usize, user: i32, pipe: i32, version: *const c_char) -> usize {
                let real = call_real!(this, $name, fn(i32, i32, *const c_char) -> usize, 0, user, pipe, version);
                // On the game server's pipe, it's the server's interface.
                let server = crate::steamgs::server_pipe() == Some(pipe) || $name == "GetISteamGameServer";
                wrap_as(real, &crate::read::cstr(version as usize).unwrap_or_default(), server)
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

// ---- steam_api exports that hand out interfaces ----

type CreateFn = unsafe extern "C" fn(*const c_char) -> usize;
type UserFn = unsafe extern "C" fn(i32, *const c_char) -> usize;
type LegacyFn = unsafe extern "C" fn() -> usize;

static CREATE: OnceLock<GenericDetour<CreateFn>> = OnceLock::new();
static FIND_USER: OnceLock<GenericDetour<UserFn>> = OnceLock::new();
static FIND_SERVER: OnceLock<GenericDetour<UserFn>> = OnceLock::new();
/// The old accessors: export, its inline hook (when one could go in), the export's
/// address, the version it hands out, whether it's the server's.
static LEGACY: OnceLock<Vec<(&'static str, Option<GenericDetour<LegacyFn>>, usize, String, bool)>> = OnceLock::new();
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
    wrap(real, &crate::read::cstr(version as usize).unwrap_or_default())
}

unsafe extern "C" fn detour_find_user(user: i32, version: *const c_char) -> usize {
    let real = match FIND_USER.get() {
        Some(h) => unsafe { h.call(user, version) },
        None => match internal("SteamInternal_FindOrCreateUserInterface") {
            Some(f) => unsafe { std::mem::transmute::<usize, UserFn>(f)(user, version) },
            None => return 0,
        },
    };
    wrap(real, &crate::read::cstr(version as usize).unwrap_or_default())
}

unsafe extern "C" fn detour_find_server(user: i32, version: *const c_char) -> usize {
    let real = match FIND_SERVER.get() {
        Some(h) => unsafe { h.call(user, version) },
        None => match internal("SteamInternal_FindOrCreateGameServerInterface") {
            Some(f) => unsafe { std::mem::transmute::<usize, UserFn>(f)(user, version) },
            None => return 0,
        },
    };
    wrap_as(real, &crate::read::cstr(version as usize).unwrap_or_default(), true)
}

macro_rules! legacy {
    ($fn:ident, $export:literal) => {
        unsafe extern "C" fn $fn() -> usize {
            let Some((_, h, addr, version, server)) =
                LEGACY.get().and_then(|l| l.iter().find(|(n, ..)| *n == $export))
            else {
                return 0;
            };
            let real = match h {
                Some(h) => unsafe { h.call() },
                None => unsafe { std::mem::transmute::<usize, LegacyFn>(*addr)() },
            };
            wrap_as(real, version, *server)
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

/// The old accessors' stand-in functions, by export name, for the by-name swap.
static LEGACY_FNS: [(&str, LegacyFn); 8] = [
    ("SteamClient", legacy_client),
    ("SteamUser", legacy_user),
    ("SteamFriends", legacy_friends),
    ("SteamMatchmaking", legacy_matchmaking),
    ("SteamNetworking", legacy_networking),
    ("SteamMatchmakingServers", legacy_matchmaking_servers),
    ("SteamGameServer", legacy_game_server),
    ("SteamGameServerNetworking", legacy_game_server_networking),
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
    let _ = FLAT_REAL.set(real);
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
            | "SteamClient"
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
    if !REALS.lock().unwrap().contains_key(&this) {
        return None;
    }
    let proxy = unsafe { &*(this as *const Proxy) };
    let i = slot(proxy.version, name)?;
    Some(unsafe { *proxy.vtable.add(i) })
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
/// flat function with the real object.
macro_rules! flat_returns_id {
    ($fn:ident, $flat:literal, $method:literal $(, $a:ident: $t:ty)*) => {
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
            let f: unsafe extern "C" fn(usize $(, $t)*) -> u64 = unsafe { std::mem::transmute(real) };
            unsafe { f(unwrap(this) $(, $a)*) }
        }
    };
}

flat_returns_id!(flat_user_steam_id, "SteamAPI_ISteamUser_GetSteamID", "GetSteamID");
flat_returns_id!(flat_server_steam_id, "SteamAPI_ISteamGameServer_GetSteamID", "GetSteamID");
flat_returns_id!(
    flat_unauthenticated,
    "SteamAPI_ISteamGameServer_CreateUnauthenticatedUserConnection",
    "CreateUnauthenticatedUserConnection"
);
flat_returns_id!(flat_lobby_by_index, "SteamAPI_ISteamMatchmaking_GetLobbyByIndex", "GetLobbyByIndex", i: i32);
flat_returns_id!(
    flat_lobby_member,
    "SteamAPI_ISteamMatchmaking_GetLobbyMemberByIndex",
    "GetLobbyMemberByIndex",
    lobby: u64,
    i: i32
);
flat_returns_id!(flat_lobby_owner, "SteamAPI_ISteamMatchmaking_GetLobbyOwner", "GetLobbyOwner", lobby: u64);

fn adapters() -> Vec<(&'static str, usize)> {
    if FLAT_REAL.get().is_none() {
        return Vec::new();
    }
    vec![
        ("SteamAPI_ISteamUser_GetSteamID", flat_user_steam_id as *const () as usize),
        ("SteamAPI_ISteamGameServer_GetSteamID", flat_server_steam_id as *const () as usize),
        ("SteamAPI_ISteamGameServer_CreateUnauthenticatedUserConnection", flat_unauthenticated as *const () as usize),
        ("SteamAPI_ISteamMatchmaking_GetLobbyByIndex", flat_lobby_by_index as *const () as usize),
        ("SteamAPI_ISteamMatchmaking_GetLobbyMemberByIndex", flat_lobby_member as *const () as usize),
        ("SteamAPI_ISteamMatchmaking_GetLobbyOwner", flat_lobby_owner as *const () as usize),
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
    if let Some((_, _, _, _, _)) = LEGACY.get().and_then(|l| l.iter().find(|(n, ..)| *n == name)) {
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

/// `n` pieces of code of at most `each` bytes, padded with int3, in one executable block.
unsafe fn exec_block(n: usize, each: usize, code: impl Fn(usize) -> Vec<u8>) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
    };
    let size = n * each;
    let p = unsafe { VirtualAlloc(std::ptr::null(), size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) } as *mut u8;
    if p.is_null() {
        return 0;
    }
    for i in 0..n {
        let c = code(i);
        debug_assert!(c.len() <= each);
        unsafe {
            std::ptr::write_bytes(p.add(i * each), 0xCC, each);
            std::ptr::copy_nonoverlapping(c.as_ptr(), p.add(i * each), c.len().min(each));
        }
    }
    let mut old = 0;
    unsafe { VirtualProtect(p.cast(), size, PAGE_EXECUTE_READ, &mut old) };
    p as usize
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
    // The old accessors take no version: it's the one this steam_api.dll was built
    // with, the only one of its family named in the image.
    let image = unsafe { image_bytes(base) };
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
    ] {
        let (Some(a), Some(version)) = (find(export), newest_named(image, fam)) else {
            continue;
        };
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
        legacy.push((export, hooked, a, version, server));
    }
    let _ = LEGACY.set(legacy);
    log!("steam-lan interface exports hooked: {}", patched.len());
    patched.extend(crate::steamgs::attach(exports));
    patched
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

/// The mapped image of a module.
unsafe fn image_bytes(base: usize) -> &'static [u8] {
    unsafe {
        let nt = base + *((base + 0x3c) as *const u32) as usize;
        // OptionalHeader.SizeOfImage sits at the same offset in PE32 and PE32+.
        let size = *((nt + 0x18 + 0x38) as *const u32) as usize;
        std::slice::from_raw_parts(base as *const u8, size)
    }
}

/// The highest "<family>NNN" string in `image`, e.g. "SteamFriends015".
fn newest_named(image: &[u8], family: &str) -> Option<String> {
    let pat = family.as_bytes();
    let mut best: Option<String> = None;
    for i in memchr_all(image, pat) {
        let digits = image.get(i + pat.len()..i + pat.len() + 3)?;
        let end = image.get(i + pat.len() + 3).copied();
        if digits.iter().all(u8::is_ascii_digit) && end == Some(0) {
            let v = format!("{family}{}", std::str::from_utf8(digits).ok()?);
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
        assert!(slot("SteamNetworkingSockets004", "ConnectP2P~2").is_some());
        assert!(TABLES.iter().all(|(_, n)| n.len() <= MAX_SLOTS));
    }

    // A fake ISteamNetworking005: each method returns the `this` it was given plus
    // 100 times its slot plus its argument.
    vmethod! { fn fake_slot_20(this: usize, a: u32) -> usize { this.wrapping_add(2000 + a as usize) } }
    vmethod! { fn fake_slot_21(this: usize, a: u32) -> usize { this.wrapping_add(2100 + a as usize) } }

    #[test]
    fn stand_ins_forward_other_methods_to_the_real_object() {
        unsafe { std::env::set_var("PEERPARTY_ACTIVE", "1") };
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

    fn networking() -> Vec<(&'static str, usize)> {
        crate::steamp2p::networking()
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
        assert_eq!(
            got("SteamAPI_SteamGameServerNetworkingMessages_SteamAPI_v002"),
            Some(("SteamNetworkingMessages002".into(), true))
        );
        // Not accessors, or interfaces PeerParty leaves alone.
        assert_eq!(got("SteamAPI_SteamApps_v008"), None);
        assert_eq!(got("SteamAPI_ISteamUser_GetSteamID"), None);
        assert_eq!(got("SteamAPI_SteamUser_vX"), None);
        // Every client accessor names a version PeerParty has a layout for.
        for v in ["SteamUser023", "SteamFriends017", "SteamMatchMaking009", "SteamNetworking006"] {
            assert!(layout(v).is_some(), "{v}");
        }
    }

    #[test]
    fn finds_the_version_an_old_steam_api_was_built_with() {
        let img = b"..SteamFriends014\0..SteamFriends015\0..SteamFriends0155..SteamUser019\0";
        assert_eq!(newest_named(img, "SteamFriends").as_deref(), Some("SteamFriends015"));
        assert_eq!(newest_named(img, "SteamUser").as_deref(), Some("SteamUser019"));
        assert_eq!(newest_named(img, "SteamNetworking"), None);
    }
}
