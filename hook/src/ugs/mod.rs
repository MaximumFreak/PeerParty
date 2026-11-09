//! A local stand-in for Unity Gaming Services' multiplayer services: Authentication,
//! Lobby (with its Wire events) and Relay, for Unity games played on the LAN.
//!
//! The game's own UGS SDKs keep running. In a game started through PeerParty, lookups of the hosts below
//! resolve to this process's loopback address (127.80.x.y), where the PlayFab
//! service's HTTPS server (playfab/http.rs, playfab/tls.rs) answers. Requests whose
//! `Host` is a Unity one come here instead of PlayFab's API. The certificate is
//! PeerParty's own, trusted inside the game only, two ways (playfab/tls.rs): UnityTls
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use peerparty_core::{Event, JoinResult, Lobby, PeerId};

use crate::json::Json;
use crate::log;
use crate::net::{self, Front};
use crate::playfab::http::Request;

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

/// Whether the stand-in is on for this game (see the module comment).
pub fn wanted() -> bool {
    net::enabled()
        && !net::skip("ugs")
        && match std::env::var("PEERPARTY_UGS").ok().as_deref() {
            Some("0") => false,
            Some("1") => true,
            _ => ships_multiplayer(),
        }
        && !crate::modes::official("ugs")
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
    let ip = crate::playfab::server()?;
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

/// Random text from `alphabet`.
pub fn random_text(len: usize, alphabet: &[u8]) -> String {
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in PeerId::random().0 {
            if out.len() < len {
                out.push(alphabet[b as usize % alphabet.len()] as char);
            }
        }
    }
    out
}

pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        out.extend_from_slice(&PeerId::random().0);
    }
    out.truncate(len);
    out
}

// ---- encodings ----

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 (RFC 4648 section 4), padded.
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Base64url without padding (RFC 4648 section 5), as JWTs use.
pub fn base64url(data: &[u8]) -> String {
    base64(data).trim_end_matches('=').replace('+', "-").replace('/', "_")
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// ---- time ----

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// UTC time as ISO 8601 with milliseconds, the way Unity's services write it.
pub fn iso(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's days_from_civil, inverted).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        d.subsec_millis()
    )
}

pub fn now_iso() -> String {
    iso(SystemTime::now())
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
                    _ => {}
                }
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
    WAITS.0.lock().unwrap().joins.remove(lobby);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (plain, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(plain.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), plain.as_bytes());
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64_decode("-_8").unwrap(), vec![0xfb, 0xff]);
    }

    #[test]
    fn iso_times_are_utc_with_milliseconds() {
        assert_eq!(iso(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_793_437_200_123);
        assert_eq!(iso(t), "2026-10-31T09:00:00.123Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(iso(leap), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn player_ids_are_28_characters_and_found_by_host_name() {
        let id = player_id(PeerId([0xab; 16]));
        assert_eq!(id.len(), 28);
        assert!(is_host("Lobby.Services.Api.Unity.Com:443"));
        assert!(is_host("wire.unity3d.com"));
        assert!(!is_host("qos-discovery.services.api.unity.com"));
        assert!(!is_host("a3cac.playfabapi.com"));
    }

    #[test]
    fn random_text_uses_the_alphabet() {
        let t = random_text(40, b"AB");
        assert_eq!(t.len(), 40);
        assert!(t.bytes().all(|b| b == b'A' || b == b'B'));
    }
}
