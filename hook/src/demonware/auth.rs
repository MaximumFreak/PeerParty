//! Demonware's JSON sign-in ("auth3"): `POST /auth/` on `<title>-pc-auth3.<env>.demonware.net`.
//!
//! The request carries `auth_task`, `iv_seed`, `title_id` and `extra_data`, a JSON text
//! whose `token` is the base64 of the game's Steam encrypted app ticket. The game asked
//! Steam for that ticket with 0x58 bytes of its own inside (a random 24-byte key, then
//! the persona name); steamlan.rs answers it with `app_ticket`, which carries them in the
//! clear.
//!
//! The answer (`code` 700, `auth_task` one more than the request's) carries a client ticket (bdAuthTicket, 128 bytes) and a
//! server ticket (128 bytes) the game later hands the lobby service. The client ticket
//! normally comes encrypted with the key from the app ticket; Demonware's client takes
//! it as it is when it starts with the ticket's magic number, so it's sent in the clear.
//! Its session key is PeerParty's: lobby.rs reads it back out of the server ticket.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::util::json::Json;
use crate::log;
use crate::https::server::Request;


static TITLE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// The SteamID the last sign-in was for: the account the umbrella service's tokens are.
static SIGNED_IN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The title id the game signed in with (0 before it has).
pub fn title() -> u32 {
    TITLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// The account id the game signed in as (0 before it has).
pub fn signed_in() -> u64 {
    SIGNED_IN.load(std::sync::atomic::Ordering::Relaxed)
}

/// bdAuthTicket's magic number.
pub const TICKET_MAGIC: u32 = 0xEFBD_ADDE;

/// The start of an encrypted app ticket PeerParty made (steamlan.rs).
pub const APP_TICKET_MAGIC: &[u8; 4] = b"PPDW";

/// Marks a server ticket PeerParty made; the session key is the 24 bytes before it.
pub const SERVER_TICKET_MARK: &[u8; 4] = b"PPST";

/// The Steam encrypted app ticket PeerParty hands a game whose Demonware is answered
/// here: its magic, the player's SteamID, then the data the game asked to include.
pub fn app_ticket(steam_id: u64, included: &[u8]) -> Vec<u8> {
    let mut t = APP_TICKET_MAGIC.to_vec();
    t.extend_from_slice(&steam_id.to_le_bytes());
    t.extend_from_slice(included);
    t
}

/// What the sign-in learns from the game's app ticket.
#[derive(Debug, Default, PartialEq)]
pub struct Player {
    pub steam_id: u64,
    pub name: String,
    /// The key the game put in its ticket (the client ticket's encryption key).
    pub key: Vec<u8>,
}

/// Reads an app ticket from `app_ticket`. Another kind (a real Steam ticket, which only
/// the publisher can decrypt) gives what's known of this player instead.
pub fn read_app_ticket(ticket: &[u8]) -> Player {
    if ticket.len() >= 12 && &ticket[..4] == APP_TICKET_MAGIC {
        let steam_id = u64::from_le_bytes(ticket[4..12].try_into().unwrap());
        let included = &ticket[12..];
        let key = included.get(..24).unwrap_or_default().to_vec();
        let name_bytes = included.get(24..).unwrap_or_default();
        let end = name_bytes.iter().position(|b| *b == 0).unwrap_or(name_bytes.len()).min(64);
        let mut name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
        // A game that sends no data of its own with the ticket has no name in it.
        if name.is_empty() {
            name = lan_name();
        }
        return Player { steam_id, name, key };
    }
    Player { steam_id: crate::hub::accounts::my_steam_id(), name: lan_name(), key: Vec::new() }
}

fn lan_name() -> String {
    crate::net::lan().map(|l| l.name().to_string()).unwrap_or_default()
}

/// The reply's `auth_task`: the request's plus one (Steam sign-in asks with 28 and
/// waits for 29; Demonware's client checks it before anything else in the reply).
pub fn reply_task(request: u64) -> u64 {
    request.wrapping_add(1)
}

/// A number the game sent as a JSON string or number.
fn num(v: &Json) -> Option<u64> {
    match v {
        Json::Str(s) => s.trim().parse().ok(),
        // Exact for an integer; a fraction is cut, as before integers were kept apart.
        _ => v.as_u64().or_else(|| v.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64)),
    }
}

/// The 128-byte bdAuthTicket, packed: magic, type, title id, issued, expires, licence
/// id, user id, user name (64), session key (24), then 7 bytes of hash fields left 0.
pub fn client_ticket(title_id: u32, player: &Player, session_key: &[u8; 24], now: u32) -> [u8; 128] {
    let mut t = [0u8; 128];
    t[0..4].copy_from_slice(&TICKET_MAGIC.to_le_bytes());
    t[4] = 0;
    t[5..9].copy_from_slice(&title_id.to_le_bytes());
    t[9..13].copy_from_slice(&now.to_le_bytes());
    t[13..17].copy_from_slice(&now.wrapping_add(30_000).to_le_bytes());
    // 17..25 licence id 0
    t[25..33].copy_from_slice(&player.steam_id.to_le_bytes());
    let name = player.name.as_bytes();
    let n = name.len().min(63);
    t[33..33 + n].copy_from_slice(&name[..n]);
    t[97..121].copy_from_slice(session_key);
    t
}

/// The 128-byte server ticket the game passes to the lobby service unread: the session
/// key, PeerParty's mark, the user id and name.
pub fn server_ticket(player: &Player, session_key: &[u8; 24]) -> [u8; 128] {
    let mut t = [0u8; 128];
    t[0..24].copy_from_slice(session_key);
    t[24..28].copy_from_slice(SERVER_TICKET_MARK);
    t[28..36].copy_from_slice(&player.steam_id.to_le_bytes());
    let name = player.name.as_bytes();
    let n = name.len().min(63);
    t[36..36 + n].copy_from_slice(&name[..n]);
    t
}

/// The umbrella service's token for a signed-in player (`POST /v1.0/tokens/lsg/`, with
/// the sign-in's server ticket, its seed and the title): an access token, the token to
/// renew it, and the player's account. Nothing checks them but this stand-in.
fn umbrella_tokens(req: &Request) -> (u16, String) {
    let body = Json::parse(&String::from_utf8_lossy(&req.body)).unwrap_or(Json::Null);
    let has_ticket = body.get("ticket").as_str().is_some_and(|t| !t.is_empty());
    let account = SIGNED_IN.load(std::sync::atomic::Ordering::Relaxed);
    let account = if account != 0 { account } else { crate::hub::accounts::my_steam_id() };
    let token = |n| crate::util::encoding::base64(&crate::util::crypto::random_bytes(n));
    log!(
        "demonware umbrella: tokens for account {account} (title {}, {}): handed out",
        num(body.get("titleID")).unwrap_or(0),
        if has_ticket { "with the sign-in's ticket" } else { "no ticket in the request" }
    );
    let reply = Json::obj(vec![
        ("accessToken", Json::str(token(32))),
        ("refreshToken", Json::str(token(32))),
        ("accountID", Json::U64(account)),
    ]);
    (200, reply.to_string())
}

/// Answers one request to a Demonware host over HTTPS: (HTTP status, JSON body).
pub fn handle(req: &Request) -> (u16, String) {
    let host = req.header("Host").unwrap_or_default().split(':').next().unwrap_or_default().to_ascii_lowercase();
    let path = req.path.split('?').next().unwrap_or_default().to_string();
    crate::log::throttled(&format!("demonware https {} {host}{path}", req.method), 3, |n| {
        log!("demonware {} https://{host}{path} n={n}", req.method)
    });
    if req.method == "POST" && path.trim_end_matches('/') == "/v1.0/tokens/lsg" {
        return umbrella_tokens(req);
    }
    if !(req.method == "POST" && path.trim_end_matches('/') == "/auth") {
        log!("demonware: no answer for {} {host}{path} (body {} bytes)", req.method, req.body.len());
        // What it asked, once per path: what an answer would have to look like.
        crate::log::throttled(&format!("demonware unanswered {host}{path}"), 1, |_| {
            let headers: Vec<String> = req.headers.iter().map(|(k, v)| format!("{k}: {}", v.chars().take(80).collect::<String>())).collect();
            log!("demonware: that request's headers: {}", headers.join(" | "));
            log!("demonware: that request's body: {}", String::from_utf8_lossy(&req.body[..req.body.len().min(600)]).replace(['\r', '\n'], " "));
        });
        return (404, "{}".to_string());
    }
    let Some(body) = Json::parse(&String::from_utf8_lossy(&req.body)) else {
        log!("demonware auth: the request isn't JSON: {}", String::from_utf8_lossy(&req.body));
        return (400, "{}".to_string());
    };
    let task = num(body.get("auth_task")).unwrap_or(0);
    let iv_seed = num(body.get("iv_seed")).unwrap_or(0);
    let title_id = num(body.get("title_id")).unwrap_or(0) as u32;
    let extra = body.get("extra_data").as_str().and_then(Json::parse).unwrap_or(Json::Null);
    let token = extra
        .get("token")
        .as_str()
        .and_then(crate::util::encoding::base64_decode)
        .unwrap_or_default();
    let player = read_app_ticket(&token);
    TITLE.store(title_id, std::sync::atomic::Ordering::Relaxed);
    SIGNED_IN.store(player.steam_id, std::sync::atomic::Ordering::Relaxed);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);
    let session_key: [u8; 24] = crate::util::crypto::random_bytes(24).try_into().unwrap();
    let client = client_ticket(title_id, &player, &session_key, now);
    let server = server_ticket(&player, &session_key);
    super::lobby::remember_session_key(session_key);
    log!(
        "demonware auth: task {task}, title {title_id}, player {} ({}), app ticket {} bytes{}: signed in",
        player.name,
        player.steam_id,
        token.len(),
        if token.starts_with(APP_TICKET_MAGIC) { " (PeerParty's)" } else { "" }
    );
    let extra_out = Json::obj(vec![
        ("username", Json::str(player.name.clone())),
        ("time_to_live", Json::Num(9999.0)),
        ("extended_data", Json::str(crate::util::encoding::base64(b"peerparty"))),
    ]);
    let reply = Json::obj(vec![
        ("auth_task", Json::str(reply_task(task).to_string())),
        ("code", Json::str("700")),
        ("iv_seed", Json::str(iv_seed.to_string())),
        ("client_ticket", Json::str(crate::util::encoding::base64(&client))),
        ("server_ticket", Json::str(crate::util::encoding::base64(&server))),
        ("client_id", Json::str("")),
        ("account_type", Json::str("steam")),
        ("crossplay_enabled", Json::Bool(false)),
        ("loginqueue_eanbled", Json::Bool(false)),
        ("identity", Json::str(body.get("identity").as_str().unwrap_or_default())),
        ("extra_data", Json::str(extra_out.to_string())),
        ("lsg_endpoint", Json::Null),
    ]);
    (200, reply.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_umbrella_service_hands_out_tokens() {
        let req = Request {
            method: "POST".into(),
            path: "/v1.0/tokens/lsg/?client=title-pc".into(),
            headers: vec![("Host".into(), "prod.umbrella.demonware.net".into())],
            body: br#"{"ticket": "AAAA", "initialVectorSeed": "MTIz", "titleID": 1234}"#.to_vec(),
        };
        let (status, body) = handle(&req);
        assert_eq!(status, 200);
        let j = Json::parse(&body).unwrap();
        assert!(j.get("accessToken").as_str().is_some_and(|t| t.len() > 20), "{body}");
        assert!(j.get("refreshToken").as_str().is_some(), "{body}");
        assert!(j.get("accountID").as_u64().is_some(), "{body}");
    }

    #[test]
    fn app_tickets_carry_the_games_key_and_name() {
        let mut included = vec![9u8; 24];
        included.extend_from_slice(b"Player\0\0\0");
        let t = app_ticket(76561198000001234, &included);
        let p = read_app_ticket(&t);
        assert_eq!(p.steam_id, 76561198000001234);
        assert_eq!(p.name, "Player");
        assert_eq!(p.key, vec![9u8; 24]);
    }

    #[test]
    fn the_client_ticket_has_bdauthtickets_layout() {
        let p = Player { steam_id: 0x0110_0001_0000_1234, name: "Player".into(), key: vec![] };
        let t = client_ticket(4321, &p, &[0x42; 24], 1000);
        assert_eq!(u32::from_le_bytes(t[0..4].try_into().unwrap()), TICKET_MAGIC);
        assert_eq!(u32::from_le_bytes(t[5..9].try_into().unwrap()), 4321);
        assert_eq!(u32::from_le_bytes(t[9..13].try_into().unwrap()), 1000);
        assert_eq!(u64::from_le_bytes(t[25..33].try_into().unwrap()), p.steam_id);
        assert_eq!(&t[33..40], b"Player\0");
        assert_eq!(t[97..121], [0x42; 24]);
        let s = server_ticket(&p, &[0x42; 24]);
        assert_eq!(s[..24], [0x42; 24]);
        assert_eq!(&s[24..28], SERVER_TICKET_MARK);
    }

    #[test]
    fn sign_in_answers_code_700_with_both_tickets() {
        let token = crate::util::encoding::base64(&app_ticket(7, &[1u8; 24]));
        let extra = Json::obj(vec![("token", Json::str(token))]).to_string();
        let body = Json::obj(vec![
            ("auth_task", Json::Num(28.0)),
            ("iv_seed", Json::Num(12345.0)),
            ("title_id", Json::Num(4321.0)),
            ("extra_data", Json::str(extra)),
        ])
        .to_string();
        let req = Request {
            method: "POST".into(),
            path: "/auth/".into(),
            headers: vec![("Host".into(), "title-pc-auth3.prod.demonware.net:443".into())],
            body: body.into_bytes(),
        };
        let (status, out) = handle(&req);
        assert_eq!(status, 200);
        let j = Json::parse(&out).unwrap();
        // The answer to task 28 is task 29.
        assert_eq!(j.get("auth_task").as_str(), Some("29"));
        assert_eq!(j.get("code").as_str(), Some("700"));
        assert_eq!(j.get("iv_seed").as_str(), Some("12345"));
        let client = crate::util::encoding::base64_decode(j.get("client_ticket").as_str().unwrap()).unwrap();
        let server = crate::util::encoding::base64_decode(j.get("server_ticket").as_str().unwrap()).unwrap();
        assert_eq!(client.len(), 128);
        assert_eq!(server.len(), 128);
        assert_eq!(u64::from_le_bytes(client[25..33].try_into().unwrap()), 7);
        // Both tickets carry the same session key.
        assert_eq!(client[97..121], server[..24]);
    }
}
