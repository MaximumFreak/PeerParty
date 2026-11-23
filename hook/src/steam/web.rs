//! Steam's Web API host, api.steampowered.com, for games whose Steam is PeerParty's
//! (lan.rs). PeerParty already tells such a game that Steam is signed in (`BLoggedOn`);
//! some games also ask the Web API itself whether Steam is up
//! (`ISteamWebAPIUtil/GetServerInfo`, every few seconds) and drop out of the match the
//! first time no answer comes in time. Over the internet, one slow or refused answer,
//! or a PC without internet, ended a game that runs over the LAN.
//!
//! Lookups of the host resolve to PeerParty's HTTPS server (https/), which answers that
//! question itself, with the time as Steam writes it. Any other Web API request goes on
//! unchanged to the real host (https/client.rs `pass_on`) and its answer comes back;
//! when Steam can't be reached the game gets a 503, as good as the failure it would
//! have had without PeerParty.
//!
//! On by default for Unity games (an exe with its `<name>_Data` folder and
//! UnityPlayer.dll beside it): UnityTls reads the ROOT store, where PeerParty's
//! certificate is listed from crypt32's load on (https/tls.rs). A game with a TLS
//! library of its own and its own list of roots wouldn't trust that certificate, so
//! for other games it takes `PEERPARTY_STEAM_WEB=1`. `PEERPARTY_STEAM_WEB=0`, the Steam
//! layer turned off or Steam's official servers turn it off.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::https::client::{Waits, pass_on};
use crate::https::server::Request;
use crate::log;
use crate::net;

pub const HOST: &str = "api.steampowered.com";
/// A request passed on: to connect, and the whole exchange at most.
const WAITS: Waits = Waits { connect: Duration::from_secs(3), answer: Duration::from_secs(5) };
/// After Steam didn't answer, requests passed on get a 503 at once for this long
/// instead of each waiting out WAITS.
const RETRY_AFTER: Duration = Duration::from_secs(20);

static FAILED_AT: Mutex<Option<(Instant, String)>> = Mutex::new(None);

/// Whether `host` (a lookup or a Host header, port and case aside) is the Web API's.
pub fn is_host(host: &str) -> bool {
    host.split(':').next().unwrap_or_default().trim_end_matches('.').eq_ignore_ascii_case(HOST)
}

/// The Web API on the local HTTPS server.
pub static HTTPS: crate::https::HttpsService = crate::https::HttpsService {
    name: "steam",
    cert_names: || vec![HOST],
    is_host,
    handle,
    upgrade: None,
    // UnityWebRequest's TLS reads the ROOT store itself, before the first lookup.
    reads_root_early: || decided_early() == Some(true),
};

/// Lookups of the Web API's host.
pub static DNS: crate::net::dns::DnsService = crate::net::dns::DnsService { layer: "steam", is_host, redirect };

/// Whether the setting, the Steam layer or the game decide before steam_api loads:
/// Some(false) off, Some(true) on once PeerParty's Steam is, None for not a Unity game
/// (no default).
fn decided_early() -> Option<bool> {
    if !net::enabled() || net::skip("steam") || crate::config::modes::official_now("steam") {
        return Some(false);
    }
    match crate::config::HookConfig::global().steam_web {
        Some(on) => Some(on),
        None => unity_game().then_some(true),
    }
}

/// Whether lookups of the host come here now, and if not, why not (for the log).
fn wanted() -> Result<(), &'static str> {
    if !crate::steam::lan::on() {
        return Err("PeerParty's Steam isn't on in this game");
    }
    match decided_early() {
        Some(true) => Ok(()),
        Some(false) => Err("turned off (PEERPARTY_STEAM_WEB=0, the Steam layer off, or Steam's official servers)"),
        None => Err("not a Unity game, whose TLS may not trust PeerParty's certificate (PEERPARTY_STEAM_WEB=1 answers it anyway)"),
    }
}

/// Whether the exe is a Unity player's: its `<name>_Data` folder and UnityPlayer.dll
/// beside it (checked once).
fn unity_game() -> bool {
    static UNITY: OnceLock<bool> = OnceLock::new();
    *UNITY.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else { return false };
        let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem()) else { return false };
        dir.join(format!("{}_Data", stem.to_string_lossy())).is_dir() && dir.join("UnityPlayer.dll").is_file()
    })
}

/// The address a lookup of the host resolves to instead, or None to leave it alone.
pub fn redirect(host: &str) -> Option<String> {
    if !is_host(host) {
        return None;
    }
    if let Err(why) = wanted() {
        crate::log::throttled("steam-web not answered", 1, |_| {
            log!("steam-web: the game looks up {HOST}, which goes to Steam as it is: {why}. A game that checks Steam is up this way may drop out of its match when Steam is slow to answer or the PC is offline")
        });
        return None;
    }
    let ip = crate::https::server()?;
    crate::log::throttled("steam-web redirect", 2, |n| {
        log!("steam-web {HOST} -> {ip} (PeerParty answers whether Steam is up; other Web API requests go on to Steam) n={n}")
    });
    Some(ip.to_string())
}

/// A request's path without its query: the query may carry the game's Web API key,
/// which stays out of the log.
fn path_of(req: &Request) -> &str {
    req.path.split('?').next().unwrap_or_default()
}

/// Whether `path` asks the Web API's own server time, the question games ask to see that
/// Steam is up (`/ISteamWebAPIUtil/GetServerInfo/v1/`, `v0001` the same).
fn is_server_info(path: &str) -> bool {
    let mut parts = path.trim_matches('/').split('/');
    let (Some(iface), Some(method), version) = (parts.next(), parts.next(), parts.next()) else { return false };
    iface.eq_ignore_ascii_case("ISteamWebAPIUtil")
        && method.eq_ignore_ascii_case("GetServerInfo")
        && version.is_none_or(|v| matches!(v.to_ascii_lowercase().as_str(), "v1" | "v0001" | ""))
        && parts.next().is_none_or(str::is_empty)
}

/// `GetServerInfo`'s answer for the time `now` (seconds since 1970): what the real one
/// gives.
fn server_info(now: i64) -> String {
    use crate::util::json::Json;
    Json::obj(vec![("servertime", Json::int(now)), ("servertimestring", Json::str(crate::util::time::ctime(now)))]).to_string()
}

fn handle(req: &Request) -> (u16, String) {
    let path = path_of(req);
    if is_server_info(path) {
        crate::log::throttled("steam-web server info", 2, |n| {
            log!("steam-web {} {path}: answered here, Steam is up (as BLoggedOn says) n={n}", req.method)
        });
        return (200, server_info(crate::util::time::unix_now()));
    }
    pass_to_steam(req, path)
}

/// The request as Steam answers it, or a 503 when Steam can't be reached.
fn pass_to_steam(req: &Request, path: &str) -> (u16, String) {
    let unavailable = |why: &str| (503, format!(r#"{{"error":"PeerParty couldn't reach Steam's Web API: {}"}}"#, why.replace(['"', '\\'], "'")));
    if let Some((at, why)) = crate::sys::lock(&FAILED_AT).clone()
        && at.elapsed() < RETRY_AFTER
    {
        crate::log::throttled(&format!("steam-web not passed {path}"), 1, |n| {
            log!("steam-web {} {path}: 503 at once, Steam didn't answer {} s ago ({why}) n={n}", req.method, at.elapsed().as_secs())
        });
        return unavailable(&why);
    }
    let started = Instant::now();
    match pass_on(HOST, req, WAITS) {
        Ok((status, body)) => {
            *crate::sys::lock(&FAILED_AT) = None;
            crate::log::throttled(&format!("steam-web passed {path}"), 2, |n| {
                log!("steam-web {} {path}: passed on to Steam: {status}, {} bytes in {} ms n={n}", req.method, body.len(), started.elapsed().as_millis())
            });
            (status, body)
        }
        Err(why) => {
            log!("steam-web {} {path}: Steam didn't answer in {} ms ({why}): 503, as it would have failed without PeerParty", req.method, started.elapsed().as_millis());
            *crate::sys::lock(&FAILED_AT) = Some((Instant::now(), why.clone()));
            unavailable(&why)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> Request {
        Request { method: "GET".into(), path: path.into(), headers: vec![("Host".into(), HOST.into())], body: Vec::new() }
    }

    #[test]
    fn the_server_time_question_is_known_in_its_spellings() {
        for p in ["/ISteamWebAPIUtil/GetServerInfo/v1/", "/ISteamWebAPIUtil/GetServerInfo/v1", "/isteamwebapiutil/getserverinfo/v0001/", "/ISteamWebAPIUtil/GetServerInfo/"] {
            assert!(is_server_info(p), "{p}");
        }
        for p in ["/ISteamWebAPIUtil/GetSupportedAPIList/v1/", "/ISteamUser/GetPlayerSummaries/v2/", "/ISteamWebAPIUtil/GetServerInfo/v2/", "/ISteamWebAPIUtil/GetServerInfo/v1/x", "/"] {
            assert!(!is_server_info(p), "{p}");
        }
        assert_eq!(path_of(&get("/ISteamUser/GetPlayerSummaries/v2/?key=SECRET&steamids=1")), "/ISteamUser/GetPlayerSummaries/v2/");
    }

    #[test]
    fn the_server_time_is_answered_as_steam_writes_it() {
        assert_eq!(server_info(1_793_437_200), r#"{"servertime":1793437200,"servertimestring":"Sat Oct 31 09:00:00 2026"}"#);
        let (status, body) = handle(&get("/ISteamWebAPIUtil/GetServerInfo/v1/"));
        assert_eq!(status, 200);
        let j = crate::util::json::Json::parse(&body).unwrap();
        assert!(j.get("servertime").as_i64().is_some_and(|t| (t - crate::util::time::unix_now()).abs() < 5), "{body}");
    }

    #[test]
    fn the_host_is_the_web_apis_alone() {
        assert!(is_host("api.steampowered.com") && is_host("API.SteamPowered.com:443") && is_host("api.steampowered.com."));
        assert!(!is_host("store.steampowered.com") && !is_host("partner.steam-api.com") && !is_host("steampowered.com"));
        // Not a lookup this service answers: left alone.
        assert_eq!(redirect("store.steampowered.com"), None);
    }
}
