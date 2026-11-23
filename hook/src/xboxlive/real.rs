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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::https::client::{Waits, pass_on};
use crate::https::server::Request;
use crate::log;
use crate::util::json::Json;

const HOST: &str = "privacy.xboxlive.com";
/// To connect, and the whole exchange at most; the game's request waits this long.
const WAITS: Waits = Waits { connect: Duration::from_secs(3), answer: Duration::from_secs(5) };
/// After a failure the LAN answers alone for this long.
const RETRY_AFTER: Duration = Duration::from_secs(60);

static FAILED_AT: Mutex<Option<Instant>> = Mutex::new(None);

fn wanted() -> bool {
    crate::config::HookConfig::global().xbl_real != Some(false)
}

/// The real service's answer to the game's request: (status, body). None when it can't
/// be asked (no token in the request, off, a recent failure) or doesn't answer.
pub fn ask(req: &Request) -> Option<(u16, String)> {
    if !wanted() || req.header("Authorization").is_none_or(|a| a.trim().is_empty()) {
        return None;
    }
    if crate::sys::lock(&FAILED_AT).is_some_and(|t| t.elapsed() < RETRY_AFTER) {
        return None;
    }
    match pass_on(HOST, req, WAITS) {
        Ok(a) => {
            *crate::sys::lock(&FAILED_AT) = None;
            Some(a)
        }
        Err(why) => {
            *crate::sys::lock(&FAILED_AT) = Some(Instant::now());
            crate::log::throttled("xboxlive real privacy", 1, |_| {
                log!("xboxlive: the real privacy service didn't answer ({why}): the LAN's answers stand alone for now (real block lists and account limits unknown)")
            });
            None
        }
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
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64())
}

fn first_reason(p: &Json) -> String {
    p.get("reasons").as_arr().first().and_then(|r| r.get("reason").as_str()).unwrap_or("NotAllowed").to_string()
}

/// Keeps what a 200 answer of the real service says about who may do what.
pub fn learn(method: &str, path_and_query: &str, request: &Json, answer: &str) {
    let Some(a) = Json::parse(answer) else { return };
    let mut guard = crate::sys::lock(&LEARNED);
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
    let guard = crate::sys::lock(&LEARNED);
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
    fn a_request_without_a_token_isnt_passed_on() {
        assert!(ask(&req("GET", "/users/xuid(1)/people/avoid", &[], "")).is_none());
    }

    /// Run by hand (`--ignored`): the TLS client against the real host, which turns a
    /// made-up token away.
    #[test]
    #[ignore = "talks to the real privacy.xboxlive.com"]
    fn the_real_host_answers() {
        let r = req("GET", "/users/xuid(2533274800000001)/people/avoid", &[("Authorization", "XBL3.0 x=0;none"), ("x-xbl-contract-version", "4")], "");
        let (status, body) = pass_on(HOST, &r, WAITS).unwrap();
        assert!(matches!(status, 400 | 401 | 403), "{status} {body}");
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
