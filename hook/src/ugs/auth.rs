//! Unity Authentication (player-auth.services.api.unity.com) and the player names API
//! (social.services.api.unity.com/v1/names).
//!
//! Every sign-in succeeds as this copy's LAN player: the player id comes from the LAN
//! identity (mod.rs `player_id`), so it stays the same across runs and other copies can
//! work it out. The access token is an unsigned JWT, whose payload Authentication's
//! JwtDecoder reads for its fields: `sub` is the player id,
//! `aud` carries `envId:`, `exp - iat` is an hour so the SDK schedules its refresh
//! (a session-token sign-in) as it does online. Routes, request and response fields
//! follow AuthenticationNetworkClient / SignInResponse / User / AccessToken of the SDK
//! and services.docs.unity.com/player-auth/v1.

use crate::json::Json;
use crate::log;
use crate::net;

use super::{base64url, error_body, my_player_id, peer_of, unix_now};

/// How long an access token lasts, as Unity's do.
const TOKEN_SECONDS: i64 = 3600;
/// The environment id handed out in the token (`aud: envId:...`) and to Lobby.
pub const ENVIRONMENT_ID: &str = "00000000-0000-0000-0000-000070707479";

/// A JWT for `player`, signed by nobody.
pub fn access_token(player: &str, provider: &str) -> String {
    let now = unix_now();
    let header = Json::obj(vec![("alg", Json::str("none")), ("kid", Json::str("peerparty")), ("typ", Json::str("JWT"))]);
    let payload = Json::obj(vec![
        ("aud", Json::Arr(vec![Json::str("idd"), Json::str(format!("envId:{ENVIRONMENT_ID}")), Json::str("upid:peerparty")])),
        ("exp", Json::Num((now + TOKEN_SECONDS) as f64)),
        ("iat", Json::Num(now as f64)),
        ("nbf", Json::Num(now as f64)),
        ("iss", Json::str("https://player-auth.services.api.unity.com")),
        ("jti", Json::str(super::random_text(32, b"0123456789abcdef"))),
        ("project_id", Json::str("peerparty")),
        ("sign_in_provider", Json::str(provider)),
        ("sub", Json::str(player)),
        ("token_type", Json::str("authentication")),
    ]);
    format!(
        "{}.{}.{}",
        base64url(header.to_string().as_bytes()),
        base64url(payload.to_string().as_bytes()),
        base64url(b"peerparty")
    )
}

fn user(player: &str, provider: Option<&str>) -> Json {
    let ids = match provider {
        Some(p) => vec![Json::obj(vec![("providerId", Json::str(p)), ("externalId", Json::str(player))])],
        None => vec![],
    };
    Json::obj(vec![
        ("id", Json::str(player)),
        ("disabled", Json::Bool(false)),
        ("createdAt", Json::str("2026-01-01T00:00:00.000Z")),
        ("externalIds", Json::Arr(ids)),
    ])
}

fn signed_in(provider: &str, external: Option<&str>) -> (u16, String) {
    let player = my_player_id();
    let body = Json::obj(vec![
        ("userId", Json::str(player.clone())),
        ("idToken", Json::str(access_token(&player, provider))),
        ("sessionToken", Json::str(format!("peerparty.{player}"))),
        ("expiresIn", Json::Num(TOKEN_SECONDS as f64)),
        ("user", user(&player, external)),
    ]);
    crate::log::throttled(&format!("ugs signed in {provider}"), 2, |n| {
        log!("ugs signed in ({provider}) as player {player} n={n}")
    });
    (200, body.to_string())
}

fn auth_error(status: u16, title: &str, detail: &str) -> (u16, String) {
    let body = Json::obj(vec![
        ("title", Json::str(title)),
        ("detail", Json::str(detail)),
        ("status", Json::Num(status as f64)),
        ("details", Json::Arr(vec![])),
    ]);
    (status, body.to_string())
}

/// player-auth's routes.
pub fn handle(method: &str, path: &str, body: &Json) -> (u16, String) {
    match (method, path) {
        ("POST", "/v1/authentication/anonymous") => signed_in("anonymous", None),
        ("POST", "/v1/authentication/session-token") => signed_in("anonymous", None),
        ("POST", p) if p.starts_with("/v1/authentication/external-token/") => {
            let provider = p.rsplit('/').next().unwrap_or("external");
            signed_in(provider, Some(provider))
        }
        ("POST", "/v1/authentication/usernamepassword/sign-in" | "/v1/authentication/usernamepassword/sign-up") => {
            signed_in("username", Some("username"))
        }
        ("POST", "/v1/authentication/code-link/sign-in") => signed_in("anonymous", None),
        ("POST", p) if p.starts_with("/v1/authentication/link") || p.starts_with("/v1/authentication/unlink") => {
            let provider = body.get("idProvider").as_str().unwrap_or("external").to_string();
            let body = Json::obj(vec![("user", user(&my_player_id(), Some(&provider)))]);
            (200, body.to_string())
        }
        ("GET", p) if p.starts_with("/v1/users/") && p.ends_with("/notifications") => {
            (200, Json::obj(vec![("notifications", Json::Arr(vec![]))]).to_string())
        }
        ("GET", p) if p.starts_with("/v1/users/") => {
            let id = p.trim_start_matches("/v1/users/");
            (200, user(id, None).to_string())
        }
        ("DELETE", p) if p.starts_with("/v1/users/") => (204, String::new()),
        _ => {
            crate::log::gap("ugs", &format!("{method} {path} (Authentication call not answered, 404)"));
            auth_error(404, "Not Found", "PeerParty doesn't answer this Authentication call")
        }
    }
}

/// A player's display name: their LAN name and a four-digit tag, as Unity's look
/// ("Name#1234"), the tag from the player id so every copy agrees.
pub fn name_of(player: &str) -> String {
    let peer = peer_of(player);
    let lan = net::lan();
    let base = match (peer, lan) {
        (Some(p), Some(l)) if p == l.me() => l.name().to_string(),
        (Some(p), Some(l)) => l.peer_name(p).unwrap_or_else(|| "Player".into()),
        _ => "Player".into(),
    };
    let base: String = base.chars().filter(|c| !c.is_whitespace() && *c != '#').take(50).collect();
    let tag = u16::from_str_radix(player.get(..4).unwrap_or("0"), 16).unwrap_or(0) % 10_000;
    format!("{}#{tag:04}", if base.is_empty() { "Player" } else { &base })
}

/// social.services.api.unity.com/v1/names/{playerId} (Authentication's PlayerNamesApi).
pub fn names(method: &str, path: &str, body: &Json) -> (u16, String) {
    let Some(player) = path.strip_prefix("/v1/names/") else {
        return (404, error_body(404, 0, "Not Found", "PeerParty doesn't answer this call"));
    };
    let name = match method {
        // A game may set a name; it keeps its own text with our tag, for this run.
        "POST" | "PUT" => match body.get("name").as_str() {
            Some(n) if !n.trim().is_empty() => {
                let base: String = n.split('#').next().unwrap_or(n).chars().filter(|c| !c.is_whitespace()).take(50).collect();
                let tag = name_of(player).rsplit('#').next().unwrap_or("0000").to_string();
                format!("{base}#{tag}")
            }
            _ => name_of(player),
        },
        _ => name_of(player),
    };
    (200, Json::obj(vec![("id", Json::str(player)), ("name", Json::str(name))]).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(token: &str) -> Json {
        let part = token.split('.').nth(1).unwrap();
        Json::parse(&String::from_utf8(super::super::base64_decode(part).unwrap()).unwrap()).unwrap()
    }

    #[test]
    fn tokens_decode_the_way_the_sdk_reads_them() {
        let t = access_token("abc", "anonymous");
        assert_eq!(t.split('.').count(), 3, "JwtDecoder wants three parts");
        let p = payload(&t);
        assert_eq!(p.get("sub").as_str(), Some("abc"));
        let exp = p.get("exp").as_f64().unwrap();
        let iat = p.get("iat").as_f64().unwrap();
        assert_eq!(exp - iat, TOKEN_SECONDS as f64);
        assert!(p.get("aud").as_arr().iter().any(|a| a.as_str().is_some_and(|s| s.starts_with("envId:"))));
        // base64url: no padding or '+' '/' that would trip Base64UrlDecode.
        assert!(!t.contains('=') && !t.contains('+') && !t.contains('/'));
    }

    #[test]
    fn unknown_calls_get_an_error_body_the_sdk_parses() {
        let (status, body) = handle("POST", "/v1/nope", &Json::Null);
        assert_eq!(status, 404);
        let b = Json::parse(&body).unwrap();
        assert_eq!(b.get("status").as_f64(), Some(404.0));
        assert!(b.get("title").as_str().is_some());
    }
}
