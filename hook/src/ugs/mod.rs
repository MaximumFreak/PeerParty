//! A local stand-in for Unity Gaming Services' multiplayer services: Authentication,
//! Lobby (with its Wire events) and Relay, for Unity games played on the LAN.
//!
//! The game's own UGS SDKs keep running. In a game started through PeerParty, lookups of the hosts below
//! resolve to this process's loopback address (127.80.x.y), where PeerParty's HTTPS
//! server (https/) answers. Requests whose `Host` is a Unity one come here
//! (`HTTPS`). The certificate is PeerParty's own, trusted inside the game only, two
//! ways (https/tls.rs): UnityTls
//! reads the Windows ROOT store and verifies the chain itself on MbedTLS
//! (discussions.unity.com/t/836095), so the ROOT store the game opens (crypt32's
//! `CertOpenStore`, hooked from crypt32's load for games that ship Lobby / Relay) comes
//! back with PeerParty's certificate listed too; and crypt32's `CertGetCertificateChain`
//! trusts that one certificate, for any path that asks Windows instead (UnityPlayer.dll
//! imports both). Nothing is written to any Windows store.
//!
//! - auth.rs: player-auth (anonymous, session-token and external-token sign-in; an
//!   unsigned JWT whose `sub` is the player id) and the player names API.
//! - lobby.rs: Lobby v1 on LAN lobbies (namespace `ugs.lobby`).
//! - wire.rs: Wire, the Centrifuge-over-WebSocket channel that carries lobby events.
//! - relay.rs: Relay allocations, and a Unity Relay protocol server in the host's copy.
//! - dtls.rs: DTLS 1.2 with a pre-shared key, for games that make the relay secure.
//! - data.rs: Cloud Save and Remote Config from disk (the real ones would refuse
//!   PeerParty's sign-in).
//!
//! References: Unity's REST API references (services.docs.unity.com/player-auth/v1,
//! /lobby/v1, /relay-allocations/v1); the public package sources of Unity Transport 2.x
//! (Runtime/Relay) for the Relay protocol; Centrifuge's client protocol
//! (github.com/centrifugal/protocol, MIT) for Wire's framing. No code was taken.
//!
//! On when the game ships Unity's Lobby, Relay or Multiplayer SDK
//! (its Managed folder or IL2CPP metadata names them), or with `PEERPARTY_UGS=1`.
//! `PEERPARTY_UGS=0` or `PEERPARTY_SKIP=ugs` turns it off. Games that only use
//! Authentication for other services (Remote Config, Cloud Save) are left alone, as
//! PeerParty's sign-in would make those services refuse the game.

pub mod auth;
pub mod data;
pub mod dtls;
pub mod lobby;
pub mod relay;
pub mod wire;

use std::sync::{Condvar, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use peerparty_core::{Event, JoinResult, Lobby, PeerId};

use crate::util::json::Json;
use crate::log;
use crate::net::{self, Front};
use crate::https::server::Request;

/// The hosts PeerParty answers. Others (QoS, Analytics, Matchmaker, Vivox, Cloud Code,
/// Economy) are left alone.
pub const HOSTS: &[&str] = &[
    "player-auth.services.api.unity.com",
    "social.services.api.unity.com",
    "lobby.services.api.unity.com",
    "relay-allocations.services.api.unity.com",
    "wire.unity3d.com",
    "cloud-save.services.api.unity.com",
    "config.unity3d.com",
];

/// Whether a request's `Host` header is one of Unity's (port and case aside).
pub fn is_host(host: &str) -> bool {
    let h = host.split(':').next().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
    HOSTS.contains(&h.as_str())
}

/// Unity's services on the local HTTPS server; Wire's WebSocket upgrade takes the
/// connection over.
pub static HTTPS: crate::https::HttpsService = crate::https::HttpsService {
    name: "ugs",
    cert_names: || vec!["*.services.api.unity.com", "wire.unity3d.com", "config.unity3d.com"],
    is_host,
    handle,
    upgrade: Some(wire::serve),
    // UnityTls reads the ROOT store itself, maybe long before the first lookup.
    reads_root_early: wanted,
};

/// Lookups of Unity's hosts.
pub static DNS: crate::net::dns::DnsService = crate::net::dns::DnsService { layer: "ugs", is_host, redirect };

/// Whether the stand-in is on for this game (see the module comment).
pub fn wanted() -> bool {
    net::enabled()
        && !net::skip("ugs")
        && match crate::config::HookConfig::global().ugs {
            Some(on) => on,
            None => ships_multiplayer(),
        }
        && !crate::config::modes::official("ugs")
}

/// Names whose presence says the game uses Lobby or Relay.
const MARKERS: &[&[u8]] = &[b"Unity.Services.Lobbies", b"Unity.Services.Relay", b"Unity.Services.Multiplayer"];

/// Whether the game ships Unity's Lobby, Relay or Multiplayer SDK (checked once).
fn ships_multiplayer() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else { return false };
        let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem()) else { return false };
        let data = dir.join(format!("{}_Data", stem.to_string_lossy()));
        let managed = data.join("Managed");
        for dll in ["Unity.Services.Lobbies.dll", "Unity.Services.Relay.dll", "Unity.Services.Multiplayer.dll"] {
            if managed.join(dll).is_file() {
                log!("ugs: the game ships {dll}: Unity's Lobby and Relay come to peerparty");
                return true;
            }
        }
        let meta = data.join("il2cpp_data").join("Metadata").join("global-metadata.dat");
        if let Ok(bytes) = std::fs::read(&meta) {
            if let Some(m) = MARKERS.iter().find(|m| contains(&bytes, m)) {
                log!("ugs: the game's IL2CPP metadata names {}: Unity's Lobby and Relay come to peerparty", String::from_utf8_lossy(m));
                return true;
            }
        }
        false
    })
}

pub(crate) fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// The address a lookup of `host` resolves to instead, or None to leave it alone.
pub fn redirect(host: &str) -> Option<String> {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if !HOSTS.contains(&h.as_str()) || !wanted() {
        return None;
    }
    let ip = crate::https::server()?;
    start_pump();
    crate::log::throttled(&format!("ugs redirect {h}"), 2, |n| {
        log!("ugs {h} -> {ip} (PeerParty's Unity Gaming Services) n={n}")
    });
    Some(ip.to_string())
}

/// Answers one request to a Unity host: (HTTP status, JSON body; empty for 204).
pub fn handle(req: &Request) -> (u16, String) {
    let host = req.header("Host").unwrap_or_default().split(':').next().unwrap_or_default().to_ascii_lowercase();
    let path = req.path.split('?').next().unwrap_or_default().to_string();
    crate::log::throttled(&format!("ugs {} {host}{path}", req.method), 3, |n| {
        log!("ugs {} {host}{path} n={n}", req.method)
    });
    if net::lan().is_none() {
        return (503, error_body(503, 0, "Service Unavailable", "PeerParty's LAN isn't running"));
    }
    start_pump();
    let body = Json::parse(&String::from_utf8_lossy(&req.body)).unwrap_or(Json::Null);
    match host.as_str() {
        "player-auth.services.api.unity.com" => auth::handle(&req.method, &path, &body),
        "social.services.api.unity.com" => auth::names(&req.method, &path, &body),
        "lobby.services.api.unity.com" => lobby::handle(&req.method, &path, &body),
        "relay-allocations.services.api.unity.com" => relay::handle(&req.method, &path, &body),
        "cloud-save.services.api.unity.com" => data::cloud_save(&req.method, &req.path, &body),
        "config.unity3d.com" => data::remote_config(&req.method, &req.path),
        _ => {
            crate::log::gap("ugs", &format!("{} {host}{path} (not a peerparty service, 404)", req.method));
            (404, error_body(404, 0, "Not Found", "not a peerparty service"))
        }
    }
}

/// Unity's error body (Lobby's ErrorStatus, Relay's ErrorResponseBody, Auth's
/// AuthenticationErrorResponse share these fields).
pub fn error_body(status: u16, code: u32, title: &str, detail: &str) -> String {
    Json::obj(vec![
        ("type", Json::str("https://services.docs.unity.com/docs/errors")),
        ("status", Json::Num(status as f64)),
        ("title", Json::str(title)),
        ("detail", Json::str(detail)),
        ("code", Json::Num(code as f64)),
        ("details", Json::Arr(vec![])),
    ])
    .to_string()
}

// ---- identities ----

/// A peer's UGS player id: 28 characters, like Unity's (lowercase hex here).
pub fn player_id(peer: PeerId) -> String {
    peer.0[..14].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn my_player_id() -> String {
    net::lan().map(|l| player_id(l.me())).unwrap_or_default()
}

/// The LAN peer (us included) whose player id this is.
pub fn peer_of(player: &str) -> Option<PeerId> {
    let lan = net::lan()?;
    std::iter::once(lan.me())
        .chain(lan.peers().into_iter().map(|(p, _)| p))
        .find(|p| player_id(*p).eq_ignore_ascii_case(player))
}

// ---- the LAN's events ----

/// Results of LAN searches and joins, handed from the pump to the waiting request.
#[derive(Default)]
struct Waits {
    searches: std::collections::HashMap<u64, Vec<Lobby>>,
    joins: std::collections::HashMap<String, JoinResult>,
}

static WAITS: LazyLock<(Mutex<Waits>, Condvar)> = LazyLock::new(Default::default);

/// Starts (once) the thread that drains the LAN's events for UGS: search and join
/// results go to the waiting requests, lobby changes to lobby.rs (Wire events).
pub fn start_pump() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        net::subscribe(Front::Ugs);
        std::thread::spawn(|| loop {
            let events = net::take(Front::Ugs);
            let mut touched = Vec::new();
            for ev in events {
                match ev {
                    Event::SearchDone { search, results } => {
                        let (m, cv) = &*WAITS;
                        m.lock().unwrap().searches.insert(search, results);
                        cv.notify_all();
                    }
                    Event::JoinDone { lobby, result } => {
                        let (m, cv) = &*WAITS;
                        m.lock().unwrap().joins.insert(lobby.clone(), result);
                        cv.notify_all();
                        touched.push(lobby);
                    }
                    Event::LobbyUpdated { lobby } | Event::MemberUpdated { lobby, .. } => touched.push(lobby),
                    Event::MemberStatus { lobby: id, member, status } => {
                        lobby::member_status(&id, member, status);
                        touched.push(id);
                    }
                    Event::PeerLeft(_) => touched.extend(lobby::tracked()),
                    // A player asking this copy's relay for an allocation (relay.rs).
                    Event::ChanRequest { peer, socket } if socket == relay::SOCKET => {
                        if let Some(lan) = net::lan() {
                            if lan.knows(peer) {
                                lan.accept(peer, relay::SOCKET);
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let Some(lan) = net::lan() {
                relay::serve(lan);
            }
            touched.sort();
            touched.dedup();
            for id in touched {
                lobby::refresh(&id);
            }
            relay::tick();
            std::thread::sleep(Duration::from_millis(50));
        });
    });
}

/// Runs a LAN search and waits for its results (at most `wait`).
pub fn search(query: peerparty_core::Query, wait: Duration) -> Vec<Lobby> {
    let Some(lan) = net::lan() else { return Vec::new() };
    start_pump();
    let id = lan.search(query);
    let (m, cv) = &*WAITS;
    let deadline = Instant::now() + wait;
    let mut w = m.lock().unwrap();
    loop {
        if let Some(r) = w.searches.remove(&id) {
            return r;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            log!("ugs: LAN search timed out");
            return Vec::new();
        }
        w = cv.wait_timeout(w, left).unwrap().0;
    }
}

/// Waits for a join the LAN engine is running to finish.
pub fn wait_join(lobby: &str, wait: Duration) -> JoinResult {
    let (m, cv) = &*WAITS;
    let deadline = Instant::now() + wait;
    let mut w = m.lock().unwrap();
    loop {
        if let Some(r) = w.joins.remove(lobby) {
            return r;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return JoinResult::TimedOut;
        }
        w = cv.wait_timeout(w, left).unwrap().0;
    }
}

/// Forgets a stale join result before a new join (a retried join must not see it).
pub fn clear_join(lobby: &str) {
    crate::sys::lock(&WAITS.0).joins.remove(lobby);
}

#[cfg(test)]
mod tests {
    use super::*;



    #[test]
    fn player_ids_are_28_characters_and_found_by_host_name() {
        let id = player_id(PeerId([0xab; 16]));
        assert_eq!(id.len(), 28);
        assert!(is_host("Lobby.Services.Api.Unity.Com:443"));
        assert!(is_host("wire.unity3d.com"));
        assert!(!is_host("qos-discovery.services.api.unity.com"));
        assert!(!is_host("a3cac.playfabapi.com"));
    }

}
