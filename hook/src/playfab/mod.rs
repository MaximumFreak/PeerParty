//! A local stand-in for PlayFab's web API, for games whose PlayFab servers are offline
//! or gone. On for games that use PlayFab Party or PlayFab Multiplayer, and
//! for any other game whenever PlayFab can't be reached (no internet, or the title's
//! servers are gone); a game that only uses PlayFab's web API keeps the real one while
//! it answers.
//!
//! The game's own PlayFab SDK keeps running. Lookups of `<title>.playfabapi.com` resolve
//! to a loopback address of this process (127.80.x.y, from the process id), where
//! PeerParty answers HTTPS (https/) with PlayFab's JSON (api.rs): sign-in, entity
//! tokens, title data, profiles, CloudScript and telemetry succeed locally, and
//! matchmaking tickets are matched between copies of the game on the LAN.
//!
//! References: PlayFab's REST API documentation and its open-source C# SDK
//! (github.com/PlayFab/CSharpSDK), whose request and result models a game ships.
//!
//! PlayFab's newer C SDK (`PlayFabCore.*.dll`, github.com/PlayFab/PlayFabCSdk) signs in
//! through this service too: it is flat C over libHttpClient,
//! whose Win32 build sends the same REST calls through WinHTTP, so its own code runs
//! unchanged and only the host and the certificate check are PeerParty's. Its login
//! result needs `PlayFabId`, `NewlyCreated` and an `EntityToken` object
//! (Source/PlayFabCore/Source/Authentication/CombinedLoginResult.cpp,
//! Generated/AuthenticationTypes.cpp `LoginResult::FromJson`), which api.rs sends. Loading
//! PlayFabCore turns the service on, as Party does.
//!
//! The same HTTPS server (https/) answers the other web stand-ins (ugs/, demonware/,
//! xboxlive/); requests are routed by their Host header, and PlayFab's API takes any
//! request no other service claims.
//!
//! - api.rs: the web API.
//! - party/: PlayFab Party (networks, chat, Xbox Live chat permissions).
//! - mp.rs: PlayFab Multiplayer's Lobby & Matchmaking.

pub(crate) mod api;
pub(crate) mod mp;
pub(crate) mod party;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::net::ToSocketAddrs;
use std::sync::OnceLock;

use crate::log;

pub use api::{entity_id, entity_of};

pub const HOST_SUFFIX: &str = ".playfabapi.com";

/// Whether `host` (a lookup or a Host header, port and case aside) is PlayFab's.
pub fn is_host(host: &str) -> bool {
    let h = host.split(':').next().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
    h.ends_with(HOST_SUFFIX) || h == "playfabapi.com"
}

/// PlayFab's web API on the local HTTPS server: the last service, so it answers any
/// request no other one claims (as PlayFab's SDKs always name its host, that's theirs).
pub static HTTPS: crate::https::HttpsService = crate::https::HttpsService {
    name: "playfab",
    cert_names: || vec!["*.playfabapi.com", "playfabapi.com"],
    is_host,
    handle: api::handle,
    upgrade: None,
    // PlayFab's Unity SDK calls through UnityWebRequest: UnityTls reads the ROOT store
    // itself, maybe long before the first lookup.
    reads_root_early: || game_wants() == Some(true),
};

/// Lookups of PlayFab's hosts.
pub static DNS: crate::net::dns::DnsService = crate::net::dns::DnsService { layer: "playfab", is_host, redirect };

/// Whether lookups of `host` come here. `PEERPARTY_PLAYFAB=0` turns it off, `=1` on
/// for any game; otherwise it's on when the game ships or has loaded PlayFab Party,
/// Lobby & Matchmaking or the C SDK, or when the real host doesn't resolve.
pub fn wanted(host: &str) -> bool {
    game_wants().unwrap_or_else(|| unreachable(host))
}

/// Whether the setting or the game's PlayFab SDKs decide that lookups come here; None
/// leaves it to whether the real host answers. `PEERPARTY_SKIP=playfab` turns it off.
fn game_wants() -> Option<bool> {
    if !crate::net::enabled() || crate::net::skip("playfab") || crate::config::modes::official("playfab") {
        return Some(false);
    }
    match crate::config::HookConfig::global().playfab {
        Some(on) => Some(on),
        None => (crate::playfab::party::loaded() || crate::playfab::mp::loaded() || CORE.load(Ordering::Relaxed) || ships_sdk()).then_some(true),
    }
}

/// Whether the game's folder holds PlayFab Party, Lobby & Matchmaking or the C SDK
/// (checked once). Games sign in before loading them: a sign-in at the real PlayFab
/// leaves the game searching the real lobbies for LAN ones.
fn ships_sdk() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else { return false };
        let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem()) else { return false };
        let plugins = dir.join(format!("{}_Data", stem.to_string_lossy())).join("Plugins");
        for d in [dir.to_path_buf(), plugins.join("x86_64"), plugins.join("x86"), plugins] {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                let sdk = name.ends_with(".dll")
                    && (name.starts_with("party") || name.starts_with("playfabmultiplayer") || name.starts_with("playfabcore"));
                if sdk {
                    log!("playfab: the game ships {}: PlayFab's web API comes to peerparty", e.file_name().to_string_lossy());
                    return true;
                }
            }
        }
        false
    })
}

static CORE: AtomicBool = AtomicBool::new(false);

/// The game loaded PlayFab's C SDK (PlayFabCore): its sign-in comes to the service.
pub fn core_loaded(module: &str) {
    if !CORE.swap(true, Ordering::Relaxed) {
        log!("playfab {module}: PlayFab's C SDK signs in through PeerParty's PlayFab service");
    }
}

thread_local! {
    /// Set while PeerParty looks up the real host itself, so that lookup isn't redirected.
    static PROBING: Cell<bool> = const { Cell::new(false) };
}

/// Whether PlayFab's real host can't be found (checked once a run).
fn unreachable(host: &str) -> bool {
    static GONE: OnceLock<bool> = OnceLock::new();
    *GONE.get_or_init(|| {
        PROBING.with(|p| p.set(true));
        let found = (host, 443).to_socket_addrs().is_ok_and(|mut a| a.next().is_some());
        PROBING.with(|p| p.set(false));
        if !found {
            log!("playfab {host} doesn't resolve: answering PlayFab locally");
        }
        !found
    })
}

/// The address a lookup of `host` should resolve to instead, as text, or None to
/// leave the lookup alone.
pub fn redirect(host: &str) -> Option<String> {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if !is_host(&h)
        || PROBING.with(|p| p.get())
        || !wanted(&h)
    {
        return None;
    }
    let ip = crate::https::server()?;
    crate::log::throttled(&format!("playfab redirect {h}"), 2, |n| {
        log!("playfab {h} -> {ip} (PeerParty's PlayFab service) n={n}")
    });
    Some(ip.to_string())
}

