//! The real privacy service, asked with the game's own request.
//!
//! PeerParty answers privacy.xboxlive.com for the LAN's players (api.rs), but only Xbox
//! Live knows the player's real block list and mute list and the limits on their account
//! (a child account's communication and multiplayer settings, privacy settings). Each
//! privacy request the game makes carries its own Xbox Live token and signature (XSAPI
//! signs the method, path, token and body; not the host), so the same request goes on
//! unchanged to the real service and its answer is merged with PeerParty's
//! (`api::merge_privacy`): the stricter wins. Nothing new leaves the PC: it's the request
//! the game would have sent without PeerParty.
//!
//! The lookup skips PeerParty's own redirect (dns.rs: it runs on a thread of its own that
//! bypasses it), and the TLS client (playfab/tls.rs) checks the real certificate and
//! refuses PeerParty's. Unreachable (offline, signed out, refused), the LAN's answers
//! stand alone as before, and asking again waits RETRY_AFTER.
//!
//! What the real service said is kept (`learn`) for PlayFab Party's Xbox helper
//! (party/xbl.rs), which has no request of its own to pass on: a blocked player gets no
//! chat, a muted one isn't heard, and the voice and text permissions the game asked
//! about apply to Party chat too. A LAN player without a real XUID (a stand-in) gets
//! what the service said about cross-network players.
//!
//! `PEERPARTY_XBL_REAL=0` turns it off (LAN answers only).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::json::Json;
use crate::log;
use crate::playfab::http::Request;
use crate::playfab::tls::TlsStream;

const HOST: &str = "privacy.xboxlive.com";
const CONNECT_WAIT: Duration = Duration::from_secs(3);
/// The whole exchange, at most; the game's request waits this long.
const ANSWER_WAIT: Duration = Duration::from_secs(5);
/// After a failure the LAN answers alone for this long.
const RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_ANSWER: usize = 1 << 20;

/// Headers not passed on: about this connection, or ours to set.
const HOP: &[&str] = &["host", "connection", "content-length", "transfer-encoding", "accept-encoding", "keep-alive", "te", "upgrade", "proxy-connection", "proxy-authorization"];

static FAILED_AT: Mutex<Option<Instant>> = Mutex::new(None);

fn wanted() -> bool {
    std::env::var("PEERPARTY_XBL_REAL").map_or(true, |v| v.trim() != "0")
}

/// The real service's answer to the game's request: (status, body). None when it can't
/// be asked (no token in the request, off, a recent failure) or doesn't answer.
pub fn ask(req: &Request) -> Option<(u16, String)> {
    if !wanted() || req.header("Authorization").is_none_or(|a| a.trim().is_empty()) {
        return None;
    }
    if FAILED_AT.lock().unwrap().is_some_and(|t| t.elapsed() < RETRY_AFTER) {
        return None;
    }
    let raw = request_bytes(req);
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("peerparty-xbl-privacy".into()).spawn(move || {
        crate::dns::bypass_this_thread();
        let _ = tx.send(exchange(&raw));
    });
    let answer = match spawned {
        Ok(_) => rx.recv_timeout(ANSWER_WAIT).unwrap_or_else(|_| Err("no answer in time".into())),
        Err(e) => Err(e.to_string()),
    };
    match answer {
        Ok(a) => {
            *FAILED_AT.lock().unwrap() = None;
            Some(a)
        }
        Err(why) => {
            *FAILED_AT.lock().unwrap() = Some(Instant::now());
            crate::log::throttled("xboxlive real privacy", 1, |_| {
                log!("xboxlive: the real privacy service didn't answer ({why}): the LAN's answers stand alone for now (real block lists and account limits unknown)")
            });
            None
        }
    }
}

/// The game's request as it goes to the real host.
fn request_bytes(req: &Request) -> Vec<u8> {
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {HOST}\r\n", req.method, req.path);
    for (k, v) in &req.headers {
        if !HOP.iter().any(|h| k.eq_ignore_ascii_case(h)) && !k.contains(['\r', '\n']) && !v.contains(['\r', '\n']) {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    head.push_str(&format!("Content-Length: {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n", req.body.len()));
    let mut out = head.into_bytes();
    out.extend_from_slice(&req.body);
    out
}

fn exchange(raw: &[u8]) -> Result<(u16, String), String> {
    let addr = (HOST, 443).to_socket_addrs().map_err(|e| format!("lookup: {e}"))?.find(|a| a.is_ipv4()).ok_or("no IPv4 address")?;
    if addr.ip().is_loopback() {
        return Err("the name points at this PC".into());
    }
    let sock = TcpStream::connect_timeout(&addr, CONNECT_WAIT).map_err(|e| format!("connect: {e}"))?;
    let _ = sock.set_read_timeout(Some(ANSWER_WAIT));
    let _ = sock.set_write_timeout(Some(ANSWER_WAIT));
    let mut tls = TlsStream::connect(sock, HOST).map_err(|e| format!("TLS: {e}"))?;
    tls.write_all(raw).map_err(|e| format!("send: {e}"))?;
    let mut got = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        match tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&chunk[..n]),
            Err(e) if got.is_empty() => return Err(format!("receive: {e}")),
            Err(_) => break,
        }
        if got.len() > MAX_ANSWER {
            return Err("answer too long".into());
        }
        if parse_response(&got).is_some_and(|(_, _, complete)| complete) {
            break;
        }
    }
    let (status, body, _) = parse_response(&got).ok_or("not an HTTP answer")?;
    Ok((status, body))
}

/// (status, body, whether the whole body is here) of an HTTP/1.1 answer read so far.
fn parse_response(got: &[u8]) -> Option<(u16, String, bool)> {
    let end = got.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&got[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let header = |name: &str| {
        head.split("\r\n").skip(1).filter_map(|l| l.split_once(':')).find(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string())
    };
    let rest = &got[end + 4..];
    if header("Transfer-Encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        let mut body = Vec::new();
        let mut at = 0;
        loop {
            let Some(nl) = rest[at..].windows(2).position(|w| w == b"\r\n") else { return Some((status, String::from_utf8_lossy(&body).into(), false)) };
            let size = usize::from_str_radix(std::str::from_utf8(&rest[at..at + nl]).ok()?.split(';').next()?.trim(), 16).ok()?;
            at += nl + 2;
            if size == 0 {
                return Some((status, String::from_utf8_lossy(&body).into(), true));
            }
            // The size is the server's: one past anything read so far is "not yet" (the
            // read stops at MAX_ANSWER), never a sum that wraps.
            if rest.len().saturating_sub(at) < size.saturating_add(2) {
                return Some((status, String::from_utf8_lossy(&body).into(), false));
            }
            body.extend_from_slice(&rest[at..at + size]);
            at += size + 2;
        }
    }
    match header("Content-Length").and_then(|n| n.parse::<usize>().ok()) {
        Some(n) => Some((status, String::from_utf8_lossy(&rest[..rest.len().min(n)]).into(), rest.len() >= n)),
        None => Some((status, String::from_utf8_lossy(rest).into(), false)),
    }
}

// ---- what the real service said, for Party chat ----

/// A refusal: the first reason the service gave.
type Verdict = Option<String>;

#[derive(Default, PartialEq)]
struct Learned {
    blocked: HashSet<u64>,
    muted: HashSet<u64>,
    /// (target, permission lowercased) -> None allowed, Some(reason) refused. Target 0
    /// is the service's "cross-network user" (a LAN player without a real XUID).
    permissions: HashMap<(u64, String), Verdict>,
}

static LEARNED: Mutex<Option<Learned>> = Mutex::new(None);
/// Goes up each time what's learned changes (party/xbl.rs tells the game).
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn xuid_of(v: &Json) -> Option<u64> {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64().map(|n| n as u64))
}

fn first_reason(p: &Json) -> String {
    p.get("reasons").as_arr().first().and_then(|r| r.get("reason").as_str()).unwrap_or("NotAllowed").to_string()
}

/// Keeps what a 200 answer of the real service says about who may do what.
pub fn learn(method: &str, path_and_query: &str, request: &Json, answer: &str) {
    let Some(a) = Json::parse(answer) else { return };
    let mut guard = LEARNED.lock().unwrap();
    let l = guard.get_or_insert_with(Learned::default);
    let before = (l.blocked.clone(), l.muted.clone(), l.permissions.clone());
    note(l, method, path_and_query, request, &a);
    if (&l.blocked, &l.muted, &l.permissions) != (&before.0, &before.1, &before.2) {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

fn note(l: &mut Learned, method: &str, path_and_query: &str, request: &Json, a: &Json) {
    let (path, q) = path_and_query.split_once('?').unwrap_or((path_and_query, ""));
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let query = |key: &str| q.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.to_string());
    match segs.as_slice() {
        ["users", _, "people", list @ ("avoid" | "mute")] => {
            let set: HashSet<u64> = a.get("users").as_arr().iter().filter_map(|u| xuid_of(u.get("xuid"))).collect();
            if *list == "avoid" { l.blocked = set } else { l.muted = set }
        }
        ["users", _, "permission", "validate"] if method.eq_ignore_ascii_case("GET") => {
            let (Some(target), Some(setting)) = (query("target").and_then(|t| super::api::parse_xuid(&t)), query("setting")) else { return };
            let verdict = (a.get("isAllowed") == &Json::Bool(false)).then(|| first_reason(a));
            l.permissions.insert((target, setting.to_ascii_lowercase()), verdict);
        }
        ["users", _, "permission", "validate"] => {
            let asked: Vec<String> = request.get("permissions").as_arr().iter().filter_map(|p| p.as_str().map(str::to_string)).collect();
            for r in a.get("responses").as_arr() {
                let u = r.get("user");
                let target = match (xuid_of(u.get("xuid")), u.get("anonymousUser").as_str()) {
                    (Some(x), _) => x,
                    (None, Some(kind)) if kind.eq_ignore_ascii_case("crossNetworkUser") => 0,
                    _ => continue,
                };
                for (j, p) in r.get("permissions").as_arr().iter().enumerate() {
                    let Some(name) = p.get("permissionRequested").as_str().map(str::to_string).or_else(|| asked.get(j).cloned()) else { continue };
                    let verdict = (p.get("isAllowed") == &Json::Bool(false)).then(|| first_reason(p));
                    l.permissions.insert((target, name.to_ascii_lowercase()), verdict);
                }
            }
        }
        _ => {}
    }
}

// PARTY_CHAT_PERMISSION_OPTIONS and PARTY_XBL_CHAT_PERMISSION_MASK_REASON
// (PartyXboxLive_c.h).
const SEND_AUDIO: u32 = 0x3;
const RECEIVE_AUDIO: u32 = 0xC;
const RECEIVE_TEXT: u32 = 0x10;
pub const ALL_CHAT: u32 = 0x1F;
const REASON_NONE: u32 = 0;
const REASON_PRIVILEGE: u32 = 2;
const REASON_PRIVACY: u32 = 3;

/// The chat the real service allows with `target` (a XUID; a stand-in counts as a
/// cross-network player): (PARTY_CHAT_PERMISSION_OPTIONS, mask reason).
pub fn chat_permission(target: u64) -> (u32, u32) {
    let guard = LEARNED.lock().unwrap();
    let Some(l) = guard.as_ref() else { return (ALL_CHAT, REASON_NONE) };
    chat_from(l, target)
}

fn chat_from(l: &Learned, target: u64) -> (u32, u32) {
    let key = if target >> 32 == super::STAND_IN_HIGH >> 32 { 0 } else { target };
    if key != 0 && l.blocked.contains(&key) {
        return (0, REASON_PRIVACY);
    }
    let mut mask = ALL_CHAT;
    let mut reason = REASON_NONE;
    let mut refuse = |bits: u32, why: &str| {
        mask &= !bits;
        // A child account's or the player's own communication setting is a privilege;
        // what they set about others is privacy.
        let r = if why.contains("Privilege") || why == "Restricted" { REASON_PRIVILEGE } else { REASON_PRIVACY };
        if reason == REASON_NONE || r == REASON_PRIVILEGE {
            reason = r;
        }
    };
    if let Some(Some(why)) = l.permissions.get(&(key, "communicateusingvoice".into())) {
        refuse(SEND_AUDIO | RECEIVE_AUDIO, why);
    }
    if let Some(Some(why)) = l.permissions.get(&(key, "communicateusingtext".into())) {
        refuse(RECEIVE_TEXT, why);
    }
    if key != 0 && l.muted.contains(&key) {
        refuse(RECEIVE_AUDIO, "MuteListRestrictsTarget");
    }
    (mask, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Request {
        Request { method: method.into(), path: path.into(), headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: body.as_bytes().to_vec() }
    }

    #[test]
    fn the_games_request_goes_on_as_it_came_signed() {
        let r = req(
            "POST",
            "/users/xuid(1)/permission/validate",
            &[("Host", "privacy.xboxlive.com"), ("Authorization", "XBL3.0 x=1;tok"), ("Signature", "AAAA"), ("x-xbl-contract-version", "3"), ("Accept-Encoding", "gzip"), ("Connection", "keep-alive"), ("Content-Length", "2")],
            "{}",
        );
        let text = String::from_utf8(request_bytes(&r)).unwrap();
        assert!(text.starts_with("POST /users/xuid(1)/permission/validate HTTP/1.1\r\nHost: privacy.xboxlive.com\r\n"), "{text}");
        for kept in ["Authorization: XBL3.0 x=1;tok\r\n", "Signature: AAAA\r\n", "x-xbl-contract-version: 3\r\n", "Content-Length: 2\r\n", "Accept-Encoding: identity\r\n", "Connection: close\r\n"] {
            assert!(text.contains(kept), "{kept} in {text}");
        }
        assert!(!text.contains("gzip") && !text.contains("keep-alive"));
        assert!(text.ends_with("\r\n\r\n{}"));
        // No token, nothing to pass on.
        assert!(ask(&req("GET", "/users/xuid(1)/people/avoid", &[], "")).is_none());
    }

    /// Run by hand (`--ignored`): the TLS client against the real host, which turns a
    /// made-up token away.
    #[test]
    #[ignore = "talks to the real privacy.xboxlive.com"]
    fn the_real_host_answers() {
        let raw = request_bytes(&req("GET", "/users/xuid(2533274800000001)/people/avoid", &[("Authorization", "XBL3.0 x=0;none"), ("x-xbl-contract-version", "4")], ""));
        let (status, body) = exchange(&raw).unwrap();
        assert!(matches!(status, 400 | 401 | 403), "{status} {body}");
    }

    #[test]
    fn answers_are_read_whole_plain_or_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_response(plain), Some((200, "hello".into(), true)));
        assert_eq!(parse_response(&plain[..plain.len() - 2]).map(|r| r.2), Some(false));
        let chunked = b"HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked), Some((403, "abcde".into(), true)));
        assert_eq!(parse_response(&chunked[..60]).map(|r| r.2), Some(false));
        assert_eq!(parse_response(&chunked[..30]), None, "headers not all here yet");
        assert_eq!(parse_response(b"garbage"), None);
    }

    #[test]
    fn what_the_service_says_limits_party_chat() {
        let mut l = Learned::default();
        let me = "/users/xuid(2533274800000001)";
        note(&mut l, "GET", &format!("{me}/people/avoid"), &Json::Null, &Json::parse(r#"{"users":[{"xuid":"2533274800000009"}]}"#).unwrap());
        note(&mut l, "GET", &format!("{me}/people/mute"), &Json::Null, &Json::parse(r#"{"users":[{"xuid":"2533274800000008"}]}"#).unwrap());
        let body = Json::parse(r#"{"users":[{"xuid":"2533274800000002"},{"anonymousUser":"crossNetworkUser"}],"permissions":["CommunicateUsingVoice","CommunicateUsingText"]}"#).unwrap();
        let answer = r#"{"responses":[
            {"user":{"xuid":"2533274800000002"},"permissions":[{"isAllowed":true},{"isAllowed":false,"reasons":[{"reason":"PrivacySettingsRestrictsTarget"}]}]},
            {"user":{"anonymousUser":"crossNetworkUser"},"permissions":[{"isAllowed":false,"reasons":[{"reason":"PrivilegeRestrictsTarget"}]},{"isAllowed":true}]}]}"#;
        note(&mut l, "POST", &format!("{me}/permission/validate"), &body, &Json::parse(answer).unwrap());
        assert_eq!(chat_from(&l, 2533274800000009), (0, REASON_PRIVACY), "blocked: no chat at all");
        assert_eq!(chat_from(&l, 2533274800000008), (ALL_CHAT & !RECEIVE_AUDIO, REASON_PRIVACY), "muted: not heard");
        assert_eq!(chat_from(&l, 2533274800000002), (ALL_CHAT & !RECEIVE_TEXT, REASON_PRIVACY), "no text by their settings");
        // A LAN player without a real XUID is a cross-network player: no voice for this
        // (child) account.
        assert_eq!(chat_from(&l, super::super::STAND_IN_HIGH | 7), (RECEIVE_TEXT, REASON_PRIVILEGE));
        assert_eq!(chat_from(&l, 2533274800000005), (ALL_CHAT, REASON_NONE), "nothing said: everything");
        // A single check by GET counts too.
        note(&mut l, "GET", &format!("{me}/permission/validate?setting=CommunicateUsingVoice&target=xuid(2533274800000005)"), &Json::Null, &Json::parse(r#"{"isAllowed":false,"reasons":[{"reason":"BlockListRestrictsTarget"}]}"#).unwrap());
        assert_eq!(chat_from(&l, 2533274800000005), (RECEIVE_TEXT, REASON_PRIVACY));
    }
}
