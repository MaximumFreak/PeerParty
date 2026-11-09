//! A local stand-in for the Xbox Live web services behind a Microsoft Store (GDK) game's
//! friends screen: social (the friends list), profile (gamertags), userpresence (who is
//! online in which title) and privacy (mute / block lists, permission checks). Under
//! PeerParty the friends are the LAN's players, each online in this title, so a game's
//! "friends online" screen lists them instead of the player's real Xbox friends.
//!
//! The game's own XSAPI keeps running: it signs its requests with the real Xbox user's
//! token (Gaming Services fetches it; nothing here touches it) and sends them through
//! libHttpClient, over XCurl.dll or WinHTTP. Lookups of the four hosts resolve to the
//! PlayFab service's loopback HTTPS server (playfab/mod.rs; dns.rs), whose certificate
//! lists their names (playfab/tls.rs) and is trusted inside the game only: WinHTTP's
//! chain check goes through crypt32's hooked `CertGetCertificateChain`, and the GDK's
//! own pin check, `XNetworkingVerifyServerCertificate`, which XCurl and libHttpClient
//! call for every request, passes for that one certificate (xgameui.rs). Every other
//! Xbox Live host (achievements, stats, RTA, session directory, title storage) still
//! goes to Xbox Live.
//!
//! - api.rs: the answers, from a directory of who is on the LAN.
//! - curl.rs, winhttp.rs: with `PEERPARTY_XBL_TRACE=1`, a trace of every Xbox Live
//!   call the game makes (URLs and bodies), through XCurl or WinHTTP.
//!
//! XUIDs: each copy learns its player's real XUID from the game's own requests to these
//! hosts (the requester is in the path, `/users/xuid(X)/...`; presence and privacy
//! calls tend to come before the friends screen) and publishes it in LAN presence as
//! `peerparty:xuid` (as party/xbl.rs does for Party games). A LAN player is listed under
//! that XUID; one who hasn't published one yet, or who is signed in to the same Xbox
//! account as this copy (two copies on one PC), gets a stand-in XUID made from their LAN
//! id (`0x0009FFFF` in the top half, out of real XUIDs' range), which only PeerParty's
//! answers (and the EOS layer's account mapping, eos/users.rs) know.
//!
//! On for games that carry a MicrosoftGame.config (GDK), or with
//! `PEERPARTY_XBL=1`; `PEERPARTY_XBL=0` or `PEERPARTY_SKIP=xboxlive` turns it off.

pub mod api;
pub mod curl;
pub mod winhttp;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use peerparty_core::PeerId;

use crate::json::Json;
use crate::log;
use crate::net;
use crate::playfab::http::Request;

/// The hosts PeerParty answers, and the service name api.rs routes on.
pub const HOSTS: &[(&str, &str)] = &[
    ("social.xboxlive.com", "social"),
    ("profile.xboxlive.com", "profile"),
    ("userpresence.xboxlive.com", "userpresence"),
    ("privacy.xboxlive.com", "privacy"),
];

/// The service a host (or a `Host` header, port and case aside) is, if it's one of ours.
pub fn service_of(host: &str) -> Option<&'static str> {
    let h = host.split(':').next().unwrap_or_default().trim_end_matches('.').to_ascii_lowercase();
    HOSTS.iter().find(|(name, _)| *name == h).map(|(_, s)| *s)
}

/// Whether the stand-in is on for this game (see the module comment).
pub fn wanted() -> bool {
    net::enabled()
        && !net::skip("xboxlive")
        && match std::env::var("PEERPARTY_XBL").ok().as_deref() {
            Some("0") => false,
            Some("1") => true,
            _ => title().is_some(),
        }
}

/// The address a lookup of `host` resolves to instead, or None to leave it alone.
pub fn redirect(host: &str) -> Option<String> {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    service_of(&h)?;
    if !wanted() {
        return None;
    }
    let ip = crate::playfab::server()?;
    crate::log::throttled(&format!("xboxlive redirect {h}"), 2, |n| {
        log!("xboxlive {h} -> {ip} (PeerParty's Xbox Live friends, from the LAN) n={n}")
    });
    Some(ip.to_string())
}

/// The title, from the package's MicrosoftGame.config.
#[derive(Clone, Debug, PartialEq)]
pub struct Title {
    pub id: u32,
    pub name: String,
}

/// This game's title (checked once): the MicrosoftGame.config in the exe's folder or one
/// of its parents (a packaged game's root).
pub fn title() -> Option<&'static Title> {
    static TITLE: OnceLock<Option<Title>> = OnceLock::new();
    TITLE
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            let config = exe.ancestors().skip(1).take(6).map(|d| d.join("MicrosoftGame.config")).find(|p| p.is_file())?;
            let t = parse_config(&std::fs::read_to_string(&config).ok()?)?;
            log!("xboxlive: {} is title {:08X} ({})", config.display(), t.id, t.name);
            Some(t)
        })
        .as_ref()
}

/// `<TitleId>66CF167C</TitleId>` and `<ShellVisuals DefaultDisplayName="...">`.
fn parse_config(xml: &str) -> Option<Title> {
    let id = xml.split("<TitleId>").nth(1)?.split("</TitleId>").next()?.trim();
    let id = u32::from_str_radix(id, 16).ok()?;
    let name = xml
        .split("DefaultDisplayName=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .filter(|s| !s.starts_with("ms-resource:"))
        .unwrap_or_default()
        .to_string();
    Some(Title { id, name })
}

// ---- XUIDs ----

const XUID_KEY: &str = "peerparty:xuid";
const STAND_IN_HIGH: u64 = 0x0009_FFFF_0000_0000;

static ME: AtomicU64 = AtomicU64::new(0);

/// This copy's player's XUID, once a request has shown it.
pub fn my_xuid() -> Option<u64> {
    Some(ME.load(Ordering::Relaxed)).filter(|x| *x != 0)
}

/// Notes the player's XUID (the first one seen sticks) and publishes it on the LAN.
pub fn learn(xuid: u64) {
    if xuid == 0 || ME.compare_exchange(0, xuid, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    log!("xboxlive: this copy's Xbox user is xuid {xuid}; it goes out in LAN presence");
    net::set_presence("xboxlive", vec![(XUID_KEY.into(), xuid.to_string())]);
}

/// The requester's XUID in a request to `host`, for the requests whose path names the
/// player making them (their own friends, presence, privacy lists, achievements).
pub fn own_xuid_in(host: &str, path: &str) -> Option<u64> {
    let path = path.split('?').next().unwrap_or_default();
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let host = host.split(':').next().unwrap_or_default().to_ascii_lowercase();
    let mine = match (host.as_str(), segs.as_slice()) {
        ("social.xboxlive.com", ["users", _, "people"]) => true,
        ("userpresence.xboxlive.com", ["users", _, "devices", "current", ..]) => true,
        ("privacy.xboxlive.com", ["users", _, "people", "avoid" | "mute"] | ["users", _, "permission", "validate"]) => true,
        ("achievements.xboxlive.com", ["users", _, "achievements", ..]) => true,
        _ => false,
    };
    if mine { api::parse_xuid(segs[1]) } else { None }
}

/// A stand-in XUID for a LAN player, from their LAN id.
fn stand_in(peer: PeerId) -> u64 {
    STAND_IN_HIGH | u64::from(u32::from_le_bytes([peer.0[0], peer.0[1], peer.0[2], peer.0[3]]))
}

/// The XUID a LAN player is listed under (see the module comment): their stand-in when
/// they publish none, ours, or one another LAN player also publishes (two copies signed
/// in to one account), so no two players ever share a XUID here.
fn xuid_given(peer: PeerId, published: Option<u64>, mine: Option<u64>, shared: bool) -> u64 {
    match published {
        Some(x) if x != 0 && Some(x) != mine && !shared => x,
        _ => stand_in(peer),
    }
}

fn published_xuid(lan: &peerparty_core::Lan, peer: PeerId) -> Option<u64> {
    lan.presence(peer).iter().find(|(k, _)| k == XUID_KEY).and_then(|(_, v)| v.parse().ok())
}

pub fn xuid_of(peer: PeerId) -> u64 {
    let lan = net::lan();
    if lan.is_some_and(|l| l.me() == peer) {
        return my_xuid().unwrap_or_else(|| stand_in(peer));
    }
    let published = lan.and_then(|l| published_xuid(l, peer));
    let shared = published.is_some_and(|x| {
        lan.is_some_and(|l| l.peers().into_iter().any(|(q, _)| q != peer && published_xuid(l, q) == Some(x)))
    });
    let mine = my_xuid();
    if published.is_some_and(|x| shared || Some(x) == mine) {
        crate::log::throttled(&format!("xboxlive clash {peer:?}"), 1, |_| {
            log!(
                "xboxlive: XUID clash: a LAN player publishes xuid {}, which {} also uses; listed under stand-in {} here",
                published.unwrap_or(0),
                if shared { "another LAN player" } else { "this copy" },
                stand_in(peer)
            )
        });
    }
    xuid_given(peer, published, mine, shared)
}

/// The LAN player (or this copy) listed under `xuid`.
pub fn peer_of(xuid: u64) -> Option<PeerId> {
    let lan = net::lan()?;
    if my_xuid() == Some(xuid) {
        return Some(lan.me());
    }
    lan.peers().into_iter().map(|(p, _)| p).find(|p| xuid_of(*p) == xuid)
}

fn directory() -> api::Directory {
    let lan = net::lan();
    let me = api::Player {
        xuid: my_xuid().unwrap_or(0),
        name: lan.map(|l| l.name().to_string()).unwrap_or_default(),
    };
    let peers = lan
        .map(|l| l.peers().into_iter().map(|(p, name)| api::Player { xuid: xuid_of(p), name }).collect())
        .unwrap_or_default();
    let (title_id, title_name) = title().map(|t| (t.id, t.name.clone())).unwrap_or_default();
    api::Directory { me, peers, title_id, title_name, now: crate::playfab::api::iso(SystemTime::now()) }
}

/// A path with its digits folded, for throttling log lines per kind of request.
pub(crate) fn shape(path: &str) -> String {
    let path = path.split('?').next().unwrap_or_default();
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_digit() {
            if !out.ends_with('#') {
                out.push('#');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Answers one request to an Xbox Live host: (HTTP status, JSON body).
pub fn handle(req: &Request) -> (u16, String) {
    let host = req.header("Host").unwrap_or_default().to_ascii_lowercase();
    let Some(service) = service_of(&host) else { return (404, String::new()) };
    if net::lan().is_none() {
        return (503, String::new());
    }
    if let Some(x) = own_xuid_in(&host, &req.path) {
        learn(x);
    }
    let body = Json::parse(&String::from_utf8_lossy(&req.body)).unwrap_or(Json::Null);
    let (status, out) = api::route(service, &req.method, &req.path, &body, &directory());
    let shape = shape(&req.path);
    crate::log::throttled(&format!("xboxlive {} {host}{shape} {status}", req.method), 3, |n| {
        log!("xboxlive {} {host}{shape} -> {status} n={n}", req.method);
    });
    if status == 404 {
        crate::log::throttled(&format!("xboxlive unanswered {host}{shape}"), 1, |_| {
            log!("xboxlive: no LAN answer for {} {host}{} (body {} bytes); the game gets 404", req.method, req.path, req.body.len());
        });
    }
    (status, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_only_the_friends_screens() {
        assert_eq!(service_of("social.xboxlive.com"), Some("social"));
        assert_eq!(service_of("Profile.XboxLive.com:443"), Some("profile"));
        assert_eq!(service_of("userpresence.xboxlive.com."), Some("userpresence"));
        assert_eq!(service_of("privacy.xboxlive.com"), Some("privacy"));
        for real in ["achievements.xboxlive.com", "userstats.xboxlive.com", "rta.xboxlive.com", "sessiondirectory.xboxlive.com", "xboxlive.com"] {
            assert_eq!(service_of(real), None, "{real} stays on Xbox Live");
        }
    }

    #[test]
    fn learns_the_requester_from_their_own_requests() {
        assert_eq!(own_xuid_in("social.xboxlive.com", "/users/xuid(2533274812345678)/people?view=FriendList"), Some(2533274812345678));
        assert_eq!(own_xuid_in("userpresence.xboxlive.com", "/users/xuid(7)/devices/current/titles/current"), Some(7));
        assert_eq!(own_xuid_in("privacy.xboxlive.com", "/users/xuid(7)/permission/validate"), Some(7));
        assert_eq!(own_xuid_in("achievements.xboxlive.com", "/users/xuid(7)/achievements?titleId=1&maxItems=100"), Some(7));
        // Someone else's presence, a batch, or another host: no.
        assert_eq!(own_xuid_in("userpresence.xboxlive.com", "/users/xuid(7)?level=all"), None);
        assert_eq!(own_xuid_in("profile.xboxlive.com", "/users/batch/profile/settings"), None);
        assert_eq!(own_xuid_in("social.xboxlive.com", "/users/me/people"), None);
    }

    #[test]
    fn players_keep_their_real_xuid_unless_it_is_ours() {
        let peer = PeerId([0x78, 0x56, 0x34, 0x12, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9]);
        assert_eq!(xuid_given(peer, Some(2533274800000002), Some(2533274800000001), false), 2533274800000002);
        assert_eq!(xuid_given(peer, Some(2533274800000002), Some(2533274800000001), true), 0x0009_FFFF_1234_5678, "shared with another LAN player");
        // Same Xbox account on both copies, or nothing published yet: a stand-in.
        assert_eq!(xuid_given(peer, Some(2533274800000001), Some(2533274800000001), false), 0x0009_FFFF_1234_5678);
        assert_eq!(xuid_given(peer, None, None, false), 0x0009_FFFF_1234_5678);
    }

    #[test]
    fn reads_the_title_from_microsoftgame_config() {
        let xml = r#"<Game><ShellVisuals DefaultDisplayName="Sample Game" PublisherDisplayName="Contoso" /><TitleId>12345678</TitleId></Game>"#;
        assert_eq!(parse_config(xml), Some(Title { id: 0x12345678, name: "Sample Game".into() }));
        assert_eq!(parse_config("<Game/>"), None);
        assert_eq!(shape("/users/xuid(2533274812345678)/people?view=All"), "/users/xuid(#)/people");
    }
}
