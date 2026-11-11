//! The answers: Xbox Live's social, profile, presence and privacy JSON, built from a
//! `Directory` of who is on the LAN (mod.rs fills it in). Nothing here touches the
//! network or global state, so the shapes are unit-tested below.
//!
//! Shapes: XSAPI's request builders and deserializers (github.com/microsoft/xbox-live-api,
//! Source/Services/Social/social_service.cpp, social_relationship_result.cpp,
//! profile_service.cpp, Presence/presence_service.cpp, presence_user_batch_request.cpp,
//! presence_record.cpp, presence_device_record.cpp, Privacy/privacy_service.cpp,
//! permission_check_result.cpp). XUIDs travel as decimal strings.

use crate::json::Json;

/// A player the game may ask about.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Player {
    pub xuid: u64,
    pub name: String,
    /// A public player while voice and chat with strangers is off (the launcher's
    /// setting): the game is told not to carry speech or text between them and us.
    pub quiet: bool,
}

/// Everyone PeerParty can answer for: this copy's player and the LAN's.
#[derive(Clone, Debug, Default)]
pub struct Directory {
    /// This copy's player (xuid 0 while it isn't known yet).
    pub me: Player,
    pub peers: Vec<Player>,
    pub title_id: u32,
    pub title_name: String,
    /// ISO 8601, for presence's `lastModified`.
    pub now: String,
}

impl Directory {
    fn find(&self, xuid: u64) -> Option<&Player> {
        if xuid != 0 && xuid == self.me.xuid {
            return Some(&self.me);
        }
        self.peers.iter().find(|p| p.xuid == xuid)
    }

    /// Everyone but `who`: the friends list from `who`'s point of view.
    fn friends_of(&self, who: u64) -> Vec<&Player> {
        std::iter::once(&self.me).chain(self.peers.iter()).filter(|p| p.xuid != 0 && p.xuid != who).collect()
    }
}

/// `xuid(123)` -> 123.
pub fn parse_xuid(seg: &str) -> Option<u64> {
    seg.strip_prefix("xuid(")?.strip_suffix(')')?.parse().ok()
}

/// The user a `/users/<who>/...` path is about: `me` is this copy's player.
fn who(seg: &str, dir: &Directory) -> Option<u64> {
    if seg.eq_ignore_ascii_case("me") { Some(dir.me.xuid) } else { parse_xuid(seg) }
}

fn query<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

fn xuid_json(x: u64) -> Json {
    Json::str(x.to_string())
}

/// XUIDs in a request body, as strings or numbers.
fn xuids(v: &Json) -> Vec<u64> {
    v.as_arr()
        .iter()
        .filter_map(|x| x.as_str().and_then(|s| s.parse().ok()).or_else(|| x.as_f64().map(|n| n as u64)))
        .collect()
}

fn not_found() -> (u16, String) {
    (404, String::new())
}

/// Answers one request to `service` (social, profile, userpresence, privacy).
pub fn route(service: &str, method: &str, path_and_query: &str, body: &Json, dir: &Directory) -> (u16, String) {
    let (path, q) = path_and_query.split_once('?').unwrap_or((path_and_query, ""));
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let get = method.eq_ignore_ascii_case("GET");
    let post = method.eq_ignore_ascii_case("POST");
    let ok = |j: Json| (200, j.to_string());
    match (service, segs.as_slice()) {
        // GET /users/xuid(X)/people?view=FriendList&startIndex=0&maxItems=100
        ("social", ["users", w, "people"]) if get => {
            let Some(w) = who(w, dir) else { return not_found() };
            ok(people(dir, w, q))
        }
        // GET /users/xuid(X)/people/xuid(Y)
        ("social", ["users", _, "people", y]) if get => match parse_xuid(y).and_then(|y| dir.find(y)) {
            Some(p) => ok(person(p)),
            None => not_found(),
        },
        // POST /users/batch/profile/settings {"userIds":[..],"settings":[..]}
        ("profile", ["users", "batch", "profile", "settings"]) if post => {
            let settings: Vec<&str> = body.get("settings").as_arr().iter().filter_map(Json::as_str).collect();
            ok(profiles(dir, &xuids(body.get("userIds")), &settings))
        }
        // GET /users/xuid(X)/profile/settings?settings=Gamertag,...
        ("profile", ["users", w, "profile", "settings"]) if get => {
            let Some(w) = who(w, dir) else { return not_found() };
            let settings: Vec<&str> = query(q, "settings").map(|s| s.split(',').collect()).unwrap_or_default();
            ok(profiles(dir, &[w], &settings))
        }
        // GET /users/me/profile/settings/people/People?settings=...
        ("profile", ["users", w, "profile", "settings", "people", _]) if get => {
            let Some(w) = who(w, dir) else { return not_found() };
            let settings: Vec<&str> = query(q, "settings").map(|s| s.split(',').collect()).unwrap_or_default();
            let ids: Vec<u64> = dir.friends_of(w).iter().map(|p| p.xuid).collect();
            ok(profiles(dir, &ids, &settings))
        }
        // POST /users/batch {"users":[..]} or {"group":"People","groupXuid":".."}
        ("userpresence", ["users", "batch"]) if post => ok(presence_batch(dir, body)),
        // GET /users/xuid(X)?level=all
        ("userpresence", ["users", w]) if get => {
            let Some(w) = who(w, dir) else { return not_found() };
            ok(presence(dir, w))
        }
        // GET /users/xuid(X)/groups/People?level=all (older XSAPI)
        ("userpresence", ["users", w, "groups", _]) if get => {
            let Some(w) = who(w, dir) else { return not_found() };
            ok(Json::Arr(dir.friends_of(w).iter().map(|p| presence(dir, p.xuid)).collect()))
        }
        // The title's own presence (POST, heartbeat) and its removal (DELETE).
        ("userpresence", ["users", _, "devices", "current", "titles", "current"]) => (200, String::new()),
        // GET /users/xuid(X)/people/avoid | mute. Nobody is avoided; the muted are the
        // strangers the player chose not to talk with.
        ("privacy", ["users", _, "people", list @ ("avoid" | "mute")]) if get => {
            let muted = dir.peers.iter().filter(|p| *list == "mute" && p.quiet).map(|p| Json::obj(vec![("xuid", xuid_json(p.xuid))])).collect();
            ok(Json::obj(vec![("users", Json::Arr(muted))]))
        }
        // GET /users/xuid(X)/permission/validate?setting=..&target=xuid(Y)
        ("privacy", ["users", _, "permission", "validate"]) if get => {
            let target = query(q, "target").and_then(parse_xuid).unwrap_or(0);
            ok(allowed(dir, target, query(q, "setting").unwrap_or_default(), true))
        }
        // POST /users/xuid(X)/permission/validate {"users":[{"xuid":..}],"permissions":[..]}
        ("privacy", ["users", _, "permission", "validate"]) if post => ok(permissions(dir, body)),
        _ => not_found(),
    }
}

/// The friends list: every LAN player, as Xbox Live friends, a page of it.
fn people(dir: &Directory, who: u64, q: &str) -> Json {
    let all = dir.friends_of(who);
    // Nobody is a favourite.
    let all = if query(q, "view").is_some_and(|v| v.eq_ignore_ascii_case("Favorite")) { Vec::new() } else { all };
    let start: usize = query(q, "startIndex").and_then(|s| s.parse().ok()).unwrap_or(0);
    let max: usize = query(q, "maxItems").and_then(|s| s.parse().ok()).filter(|n| *n > 0).unwrap_or(usize::MAX);
    let page: Vec<Json> = all.iter().skip(start).take(max).map(|p| person(p)).collect();
    Json::obj(vec![("people", Json::Arr(page)), ("totalCount", Json::Num(all.len() as f64))])
}

fn person(p: &Player) -> Json {
    Json::obj(vec![
        ("xuid", xuid_json(p.xuid)),
        ("isFavorite", Json::Bool(false)),
        // Contract 3 reads isFriend; older XSAPI reads isFollowingCaller.
        ("isFriend", Json::Bool(true)),
        ("isFollowingCaller", Json::Bool(true)),
        ("isFollowedByCaller", Json::Bool(true)),
        ("socialNetworks", Json::Arr(vec![Json::str("LegacyXboxLive")])),
    ])
}

/// The longest prefix of `s` that fits `max` bytes of UTF-8 (XSAPI copies settings into
/// fixed buffers: a legacy gamertag holds 15 bytes).
fn fit(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// XSAPI's profile settings.
const SETTINGS: &[&str] = &[
    "AppDisplayName",
    "AppDisplayPicRaw",
    "GameDisplayName",
    "GameDisplayPicRaw",
    "Gamerscore",
    "Gamertag",
    "ModernGamertag",
    "ModernGamertagSuffix",
    "UniqueModernGamertag",
];

fn setting(p: &Player, id: &str) -> Option<String> {
    let name = if p.name.trim().is_empty() { format!("Player {}", p.xuid % 10_000) } else { p.name.trim().to_string() };
    Some(match id {
        "Gamertag" => fit(&name, 15),
        "GameDisplayName" | "AppDisplayName" | "ModernGamertag" | "UniqueModernGamertag" | "RealName" | "PublicGamerpic" => {
            fit(&name, 60)
        }
        "ModernGamertagSuffix" | "AppDisplayPicRaw" | "GameDisplayPicRaw" | "Bio" | "Location" => String::new(),
        "Gamerscore" => "0".into(),
        "AccountTier" => "Gold".into(),
        "XboxOneRep" => "GoodPlayer".into(),
        _ => return None,
    })
}

/// `profileUsers` for the XUIDs PeerParty knows; others are left out.
fn profiles(dir: &Directory, ids: &[u64], settings: &[&str]) -> Json {
    let settings: Vec<&str> = if settings.is_empty() { SETTINGS.to_vec() } else { settings.to_vec() };
    let users: Vec<Json> = ids
        .iter()
        .filter_map(|x| dir.find(*x))
        .map(|p| {
            let values: Vec<Json> = settings
                .iter()
                .filter_map(|id| setting(p, id).map(|v| Json::obj(vec![("id", Json::str(*id)), ("value", Json::str(v))])))
                .collect();
            Json::obj(vec![
                ("id", xuid_json(p.xuid)),
                ("hostId", xuid_json(p.xuid)),
                ("settings", Json::Arr(values)),
                ("isSponsoredUser", Json::Bool(false)),
            ])
        })
        .collect();
    Json::obj(vec![("profileUsers", Json::Arr(users))])
}

/// One presence record: LAN players (and this copy's) are online in this title, on PC;
/// anyone else is offline.
fn presence(dir: &Directory, xuid: u64) -> Json {
    if dir.find(xuid).is_none() {
        return Json::obj(vec![("xuid", xuid_json(xuid)), ("state", Json::str("Offline"))]);
    }
    let title = Json::obj(vec![
        ("id", Json::str(dir.title_id.to_string())),
        ("name", Json::str(dir.title_name.clone())),
        ("placement", Json::str("Full")),
        ("state", Json::str("Active")),
        ("lastModified", Json::str(dir.now.clone())),
        ("activity", Json::obj(vec![("richPresence", Json::str("Playing on the LAN"))])),
    ]);
    Json::obj(vec![
        ("xuid", xuid_json(xuid)),
        ("state", Json::str("Online")),
        ("devices", Json::Arr(vec![Json::obj(vec![("type", Json::str("WindowsOneCore")), ("titles", Json::Arr(vec![title]))])])),
    ])
}

fn presence_batch(dir: &Directory, body: &Json) -> Json {
    let ids: Vec<u64> = if body.get("users").as_arr().is_empty() {
        // A social group ("People", "Favorites") of groupXuid or this copy's player.
        let owner = body.get("groupXuid").as_str().and_then(|s| s.parse().ok()).unwrap_or(dir.me.xuid);
        dir.friends_of(owner).iter().map(|p| p.xuid).collect()
    } else {
        xuids(body.get("users"))
    };
    let online_only = matches!(body.get("onlineOnly"), Json::Bool(true));
    Json::Arr(
        ids.into_iter()
            .filter(|x| !online_only || dir.find(*x).is_some())
            .map(|x| presence(dir, x))
            .collect(),
    )
}

/// One permission check's answer. There is no Xbox Live to ask, so everything is
/// allowed, but for one thing PeerParty knows: speech and text (the `CommunicateUsing*`
/// permissions) with a public player, while the player has voice and chat with
/// strangers off. A game that carries its own chat asks here whether it may.
fn allowed(dir: &Directory, target: u64, permission: &str, with_reasons: bool) -> Json {
    let quiet = permission.to_ascii_lowercase().starts_with("communicateusing") && dir.peers.iter().any(|p| p.xuid == target && p.quiet);
    let mut out = vec![("isAllowed", Json::Bool(!quiet))];
    if quiet || with_reasons {
        let reasons = if quiet { vec![Json::obj(vec![("reason", Json::str("MuteListRestrictsTarget"))])] } else { vec![] };
        out.push(("reasons", Json::Arr(reasons)));
    }
    Json::obj(out)
}

/// Each permission for each user asked about, in the order asked (see `allowed`).
fn permissions(dir: &Directory, body: &Json) -> Json {
    let asked: Vec<&str> = body.get("permissions").as_arr().iter().map(|p| p.as_str().unwrap_or_default()).collect();
    let responses: Vec<Json> = body
        .get("users")
        .as_arr()
        .iter()
        .map(|u| {
            let target = xuids(&Json::Arr(vec![u.get("xuid").clone()])).first().copied().unwrap_or(0);
            Json::obj(vec![("user", u.clone()), ("permissions", Json::Arr(asked.iter().map(|p| allowed(dir, target, p, false)).collect()))])
        })
        .collect();
    Json::obj(vec![("responses", Json::Arr(responses))])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> Directory {
        Directory {
            me: Player { xuid: 2533274800000001, name: "Timber".into(), quiet: false },
            peers: vec![
                Player { xuid: 2533274800000002, name: "Timber (2)".into(), quiet: false },
                Player { xuid: 2533274800000003, name: "A very long LAN player name".into(), quiet: false },
            ],
            title_id: 0x12345678,
            title_name: "Sample Game".into(),
            now: "2026-11-01T18:00:00.000Z".into(),
        }
    }

    fn call(service: &str, method: &str, path: &str, body: &str) -> (u16, Json) {
        let body = Json::parse(body).unwrap_or(Json::Null);
        let (status, text) = route(service, method, path, &body, &dir());
        (status, Json::parse(&text).unwrap_or(Json::Null))
    }

    #[test]
    fn friends_list_is_the_lan() {
        let (status, j) = call("social", "GET", "/users/xuid(2533274800000001)/people?view=FriendList&maxItems=100", "");
        assert_eq!(status, 200);
        assert_eq!(j.get("totalCount"), &Json::Num(2.0));
        let people = j.get("people").as_arr();
        assert_eq!(people.len(), 2);
        // XSAPI's XblSocialRelationshipResult::Deserialize: a string xuid, isFriend, isFavorite.
        assert_eq!(people[0].get("xuid").as_str(), Some("2533274800000002"));
        assert_eq!(people[0].get("isFriend"), &Json::Bool(true));
        assert_eq!(people[0].get("isFollowingCaller"), &Json::Bool(true));
        assert_eq!(people[0].get("isFavorite"), &Json::Bool(false));
        assert_eq!(people[0].get("socialNetworks").as_arr().len(), 1);
        // The requester isn't their own friend; `me` works as the requester too.
        let (_, j) = call("social", "GET", "/users/me/people", "");
        assert!(j.get("people").as_arr().iter().all(|p| p.get("xuid").as_str() != Some("2533274800000001")));
        // Paging, and no favourites.
        let (_, j) = call("social", "GET", "/users/xuid(2533274800000001)/people?view=FriendList&startIndex=1&maxItems=5", "");
        assert_eq!(j.get("people").as_arr().len(), 1);
        assert_eq!(j.get("totalCount"), &Json::Num(2.0));
        let (_, j) = call("social", "GET", "/users/xuid(2533274800000001)/people?view=Favorite", "");
        assert!(j.get("people").as_arr().is_empty());
        // A friend's friends include this copy's player.
        let (_, j) = call("social", "GET", "/users/xuid(2533274800000002)/people", "");
        assert!(j.get("people").as_arr().iter().any(|p| p.get("xuid").as_str() == Some("2533274800000001")));
    }

    #[test]
    fn profiles_carry_lan_names() {
        let body = r#"{"userIds":["2533274800000002","2533274800000003","2533274899999999"],"settings":["AppDisplayName","AppDisplayPicRaw","GameDisplayName","GameDisplayPicRaw","Gamerscore","Gamertag","ModernGamertag","ModernGamertagSuffix","UniqueModernGamertag"]}"#;
        let (status, j) = call("profile", "POST", "/users/batch/profile/settings", body);
        assert_eq!(status, 200);
        let users = j.get("profileUsers").as_arr();
        assert_eq!(users.len(), 2, "unknown XUIDs are left out");
        assert_eq!(users[0].get("id").as_str(), Some("2533274800000002"));
        let value = |u: &Json, id: &str| {
            u.get("settings").as_arr().iter().find(|s| s.get("id").as_str() == Some(id)).and_then(|s| s.get("value").as_str()).map(str::to_string)
        };
        assert_eq!(users[0].get("settings").as_arr().len(), 9, "every setting asked for, each with id and value");
        assert_eq!(value(&users[0], "Gamertag").as_deref(), Some("Timber (2)"));
        assert_eq!(value(&users[0], "GameDisplayName").as_deref(), Some("Timber (2)"));
        assert_eq!(value(&users[0], "ModernGamertagSuffix").as_deref(), Some(""));
        // A legacy gamertag fits XSAPI's 16-byte buffer.
        assert_eq!(value(&users[1], "Gamertag").as_deref(), Some("A very long LAN"));
        assert_eq!(value(&users[1], "ModernGamertag").as_deref(), Some("A very long LAN player name"));
        // One user by path, and the requester's own profile.
        let (_, j) = call("profile", "GET", "/users/me/profile/settings?settings=Gamertag", "");
        assert_eq!(j.get("profileUsers").as_arr()[0].get("settings").as_arr().len(), 1);
        assert_eq!(value(&j.get("profileUsers").as_arr()[0], "Gamertag").as_deref(), Some("Timber"));
        let (_, j) = call("profile", "GET", "/users/me/profile/settings/people/People?settings=Gamertag,GameDisplayName", "");
        assert_eq!(j.get("profileUsers").as_arr().len(), 2);
    }

    #[test]
    fn presence_batch_says_online_in_this_title() {
        let body = r#"{"users":["2533274800000002","2533274899999999"],"level":"all","onlineOnly":false,"broadcastingOnly":false}"#;
        let (status, j) = call("userpresence", "POST", "/users/batch", body);
        assert_eq!(status, 200);
        let records = j.as_arr();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].get("xuid").as_str(), Some("2533274800000002"));
        assert_eq!(records[0].get("state").as_str(), Some("Online"));
        let device = &records[0].get("devices").as_arr()[0];
        assert_eq!(device.get("type").as_str(), Some("WindowsOneCore"));
        let title = &device.get("titles").as_arr()[0];
        // XblPresenceRecord::IsUserPlayingTitle: the decimal title id, state "active".
        assert_eq!(title.get("id").as_str(), Some("305419896"));
        assert_eq!(title.get("state").as_str(), Some("Active"));
        assert_eq!(title.get("name").as_str(), Some("Sample Game"));
        assert!(title.get("lastModified").as_str().is_some());
        assert_eq!(title.get("placement").as_str(), Some("Full"));
        assert!(title.get("activity").get("richPresence").as_str().is_some());
        assert_eq!(records[1].get("state").as_str(), Some("Offline"));
        // onlineOnly drops strangers; a social group lists the LAN.
        let (_, j) = call("userpresence", "POST", "/users/batch", r#"{"users":["2533274800000002","2533274899999999"],"onlineOnly":true}"#);
        assert_eq!(j.as_arr().len(), 1);
        let (_, j) = call("userpresence", "POST", "/users/batch", r#"{"group":"People","groupXuid":"2533274800000001","level":"all","onlineOnly":true}"#);
        assert_eq!(j.as_arr().len(), 2);
        // One user, and the title's own presence writes.
        let (status, j) = call("userpresence", "GET", "/users/xuid(2533274800000003)?level=all", "");
        assert_eq!((status, j.get("state").as_str()), (200, Some("Online")));
        let (status, _) = call("userpresence", "POST", "/users/xuid(2533274800000001)/devices/current/titles/current", r#"{"state":"active"}"#);
        assert_eq!(status, 200);
    }

    #[test]
    fn privacy_allows_everything() {
        let (status, j) = call("privacy", "GET", "/users/xuid(2533274800000001)/people/avoid", "");
        assert_eq!(status, 200);
        assert!(j.get("users").as_arr().is_empty());
        let (_, j) = call("privacy", "GET", "/users/xuid(2533274800000001)/permission/validate?setting=PlayMultiplayer&target=xuid(2533274800000002)", "");
        assert_eq!(j.get("isAllowed"), &Json::Bool(true));
        let body = r#"{"users":[{"xuid":"2533274800000002"},{"anonymousUser":"crossNetworkUser"}],"permissions":["PlayMultiplayer","CommunicateUsingVoice"]}"#;
        let (_, j) = call("privacy", "POST", "/users/xuid(2533274800000001)/permission/validate", body);
        let responses = j.get("responses").as_arr();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].get("user").get("xuid").as_str(), Some("2533274800000002"));
        // BatchDeserialize needs one result per permission asked.
        assert_eq!(responses[1].get("permissions").as_arr().len(), 2);
        assert_eq!(responses[1].get("permissions").as_arr()[0].get("isAllowed"), &Json::Bool(true));
    }

    /// With voice and chat with strangers off, a game that asks whether it may carry
    /// speech or text to a public player is told no, and nothing else changes.
    #[test]
    fn strangers_are_muted_while_chat_with_them_is_off() {
        let mut d = dir();
        d.peers[1].quiet = true;
        let ask = |method: &str, path: &str, body: &str| {
            let (status, text) = route("privacy", method, path, &Json::parse(body).unwrap_or(Json::Null), &d);
            assert_eq!(status, 200);
            Json::parse(&text).unwrap()
        };
        let me = "/users/xuid(2533274800000001)";
        let muted = ask("GET", &format!("{me}/people/mute"), "");
        assert_eq!(muted.get("users").as_arr().len(), 1);
        assert_eq!(muted.get("users").as_arr()[0].get("xuid").as_str(), Some("2533274800000003"));
        assert!(ask("GET", &format!("{me}/people/avoid"), "").get("users").as_arr().is_empty());
        for (setting, target, want) in [
            ("CommunicateUsingVoice", "2533274800000003", false),
            ("CommunicateUsingText", "2533274800000003", false),
            ("PlayMultiplayer", "2533274800000003", true),
            ("CommunicateUsingVoice", "2533274800000002", true),
        ] {
            let j = ask("GET", &format!("{me}/permission/validate?setting={setting}&target=xuid({target})"), "");
            assert_eq!(j.get("isAllowed"), &Json::Bool(want), "{setting} with {target}");
            assert_eq!(j.get("reasons").as_arr().len(), usize::from(!want));
        }
        let body = r#"{"users":[{"xuid":"2533274800000003"},{"xuid":"2533274800000002"}],"permissions":["PlayMultiplayer","CommunicateUsingVoice"]}"#;
        let j = ask("POST", &format!("{me}/permission/validate"), body);
        let answers: Vec<Vec<bool>> = j
            .get("responses")
            .as_arr()
            .iter()
            .map(|r| r.get("permissions").as_arr().iter().map(|p| p.get("isAllowed") == &Json::Bool(true)).collect())
            .collect();
        assert_eq!(answers, [[true, false], [true, true]]);
    }

    #[test]
    fn unknown_paths_are_not_found() {
        assert_eq!(call("social", "GET", "/users/xuid(1)/summary", "").0, 404);
        assert_eq!(call("profile", "DELETE", "/users/batch/profile/settings", "").0, 404);
        assert_eq!(parse_xuid("xuid(42)"), Some(42));
        assert_eq!(parse_xuid("gt(42)"), None);
    }
}
