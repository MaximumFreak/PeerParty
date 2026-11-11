//! Cloud Save and Remote Config, answered from disk. With the stand-in on, the game
//! signs in with PeerParty's token, which Unity's real services would refuse; so the
//! player's saves and the game's remote settings come from here instead.
//!
//! - Cloud Save v1 (cloud-save.services.api.unity.com): player data (default, public
//!   and protected) and custom items, kept per owner in
//!   `%LOCALAPPDATA%\peerparty\ugs\<game>\cloud-save\<owner>\<access>.json`. Items carry
//!   write locks as Unity's do: a write naming an old lock is a conflict. Queries find
//!   nothing; other players' data lives on their PCs. Files (the files API) aren't kept.
//! - Remote Config (config.unity3d.com): the settings in
//!   `%LOCALAPPDATA%\peerparty\ugs\<game>\remote-config.json` (a JSON object of keys
//!   and values, for anyone who wants to give a game its settings), else none, which
//!   leaves the game on its built-in defaults.
//!
//! Routes and bodies per services.docs.unity.com/cloud-save/v1 and the Remote Config
//! runtime's request (`/api/v1/settings`, `configs.settings` in the answer).
//! `PEERPARTY_UGS_DIR` replaces the folder, for tests.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::json::Json;
use crate::log;

use super::{error_body, now_iso, random_text};

type Reply = (u16, String);

const NOT_FOUND: u32 = 7006;
const CONFLICT: u32 = 7008;
const LOCK_ALPHABET: &[u8] = b"0123456789abcdef";

/// One write at a time: a read-modify-write of an owner's file.
static FILES: Mutex<()> = Mutex::new(());

fn root() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("PEERPARTY_UGS_DIR") {
        return Some(PathBuf::from(d));
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let game = std::env::current_exe().ok()?.file_stem()?.to_string_lossy().to_ascii_lowercase();
    Some(base.join("peerparty").join("ugs").join(game))
}

/// A name from the path, kept to one path component.
fn safe(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn store_path(owner: &str, access: &str) -> Option<PathBuf> {
    Some(root()?.join("cloud-save").join(safe(owner)).join(format!("{}.json", safe(access))))
}

/// key -> {value, writeLock, created, modified}, in key order.
fn load(path: &Option<PathBuf>) -> Vec<(String, Json)> {
    match path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| Json::parse(&t)) {
        Some(Json::Obj(items)) => items,
        _ => Vec::new(),
    }
}

fn save(path: &Option<PathBuf>, items: &[(String, Json)]) -> bool {
    let Some(p) = path else { return false };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    std::fs::write(p, Json::Obj(items.to_vec()).to_string()).is_ok()
}

fn query_param(path: &str, name: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            // Bytes, not a slice of the text: the two after a '%' can end inside a
            // character, and slicing there panics (with the store's lock held).
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn item_json(key: &str, item: &Json, with_value: bool) -> Json {
    let mut v = vec![("key", Json::str(key))];
    if with_value {
        v.push(("value", item.get("value").clone()));
    }
    v.push(("writeLock", item.get("writeLock").clone()));
    v.push(("modified", Json::obj(vec![("date", item.get("modified").clone())])));
    v.push(("created", Json::obj(vec![("date", item.get("created").clone())])));
    Json::obj(v)
}

fn listing(items: &[(String, Json)], keys: &[String], with_value: bool) -> Reply {
    let results: Vec<Json> = items.iter().filter(|(k, _)| keys.is_empty() || keys.contains(k)).map(|(k, i)| item_json(k, i, with_value)).collect();
    (200, Json::obj(vec![("results", Json::Arr(results)), ("links", Json::obj(vec![("next", Json::Null)]))]).to_string())
}

/// Sets one item; Err(key) on a write-lock conflict.
fn set(items: &mut Vec<(String, Json)>, key: &str, value: Json, lock: Option<&str>) -> Result<String, String> {
    let now = now_iso();
    let fresh = random_text(32, LOCK_ALPHABET);
    match items.iter_mut().find(|(k, _)| k == key) {
        Some((_, item)) => {
            if lock.is_some_and(|l| !l.is_empty() && Some(l) != item.get("writeLock").as_str()) {
                return Err(key.to_string());
            }
            let created = item.get("created").clone();
            *item = Json::obj(vec![("value", value), ("writeLock", Json::str(fresh.clone())), ("created", created), ("modified", Json::str(now))]);
        }
        None => {
            if lock.is_some_and(|l| !l.is_empty()) {
                return Err(key.to_string());
            }
            items.push((
                key.to_string(),
                Json::obj(vec![("value", value), ("writeLock", Json::str(fresh.clone())), ("created", Json::str(now.clone())), ("modified", Json::str(now))]),
            ));
            items.sort_by(|a, b| a.0.cmp(&b.0));
        }
    }
    Ok(fresh)
}

fn conflict(key: &str) -> Reply {
    (409, error_body(409, CONFLICT, "Conflict", &format!("the write lock for {key} doesn't match")))
}

/// Cloud Save: `path` with its query.
pub fn cloud_save(method: &str, path: &str, body: &Json) -> Reply {
    let route = path.split('?').next().unwrap_or_default();
    let parts: Vec<&str> = route.trim_matches('/').split('/').collect();
    // v1 / data / projects / <project> / ...
    if parts.len() < 5 || parts[0] != "v1" || parts[1] != "data" || parts[2] != "projects" {
        crate::log::gap("ugs", &format!("{method} cloud-save{route} (not carried, 404)"));
        return (404, error_body(404, NOT_FOUND, "Not Found", "not a peerparty Cloud Save route"));
    }
    let rest = &parts[4..];
    // players/<id>[/public|/protected|/default]/<op...> or custom/<id>/<op...>
    let (owner, access, op): (String, &str, &[&str]) = match rest {
        ["players", "public", "query"] | ["custom", "query"] => {
            return (200, Json::obj(vec![("results", Json::Arr(vec![]))]).to_string());
        }
        ["players", id, kind @ ("public" | "protected" | "default"), op @ ..] => (id.to_string(), kind, op),
        ["players", id, op @ ..] => (id.to_string(), "default", op),
        ["custom", id, op @ ..] => (format!("custom-{id}"), "custom", op),
        _ => {
            crate::log::gap("ugs", &format!("{method} cloud-save{route} (not carried, 404)"));
            return (404, error_body(404, NOT_FOUND, "Not Found", "not a peerparty Cloud Save route"));
        }
    };
    let path_buf = store_path(&owner, access);
    let _guard = FILES.lock().unwrap();
    let mut items = load(&path_buf);
    let keys: Vec<String> = query_param(path, "keys").map(|k| k.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect()).unwrap_or_default();
    let writable = access != "protected" && access != "custom";
    let reply = match (method, op) {
        ("GET", ["items"]) => listing(&items, &keys, true),
        ("GET", ["keys"]) => listing(&items, &keys, false),
        ("POST", ["items"]) if writable => {
            let key = body.get("key").as_str().unwrap_or_default().to_string();
            match set(&mut items, &key, body.get("value").clone(), body.get("writeLock").as_str()) {
                Ok(lock) => {
                    save(&path_buf, &items);
                    (200, Json::obj(vec![("writeLock", Json::str(lock))]).to_string())
                }
                Err(k) => conflict(&k),
            }
        }
        ("POST", ["item-batch"]) if writable => {
            let mut results = Vec::new();
            let mut next = items.clone();
            for d in body.get("data").as_arr() {
                let key = d.get("key").as_str().unwrap_or_default().to_string();
                match set(&mut next, &key, d.get("value").clone(), d.get("writeLock").as_str()) {
                    Ok(lock) => results.push(Json::obj(vec![("key", Json::str(key)), ("writeLock", Json::str(lock))])),
                    Err(k) => return conflict(&k),
                }
            }
            items = next;
            save(&path_buf, &items);
            (200, Json::obj(vec![("results", Json::Arr(results))]).to_string())
        }
        ("DELETE", ["items", key]) if writable => {
            let key = percent_decode(key);
            let lock = query_param(path, "writeLock");
            match items.iter().position(|(k, _)| *k == key) {
                Some(i) if lock.as_deref().is_some_and(|l| !l.is_empty() && Some(l) != items[i].1.get("writeLock").as_str()) => conflict(&key),
                Some(i) => {
                    items.remove(i);
                    save(&path_buf, &items);
                    (204, String::new())
                }
                None => (404, error_body(404, NOT_FOUND, "Not Found", &format!("no item {key}"))),
            }
        }
        ("DELETE", ["items"]) if writable => {
            items.clear();
            save(&path_buf, &items);
            (204, String::new())
        }
        _ => {
            crate::log::gap("ugs", &format!("{method} cloud-save{route} (not carried, 404)"));
            (404, error_body(404, NOT_FOUND, "Not Found", "not a peerparty Cloud Save route"))
        }
    };
    crate::log::throttled(&format!("ugs cloud-save {method} {access}"), 3, |n| log!("ugs cloud-save {method} {access} {} n={n} -> {}", op.join("/"), reply.0));
    reply
}

/// Remote Config: the game's settings, from disk.
pub fn remote_config(method: &str, path: &str) -> Reply {
    let route = path.split('?').next().unwrap_or_default();
    if !route.contains("settings") {
        crate::log::gap("ugs", &format!("{method} config.unity3d.com{route} (not carried, 404)"));
        return (404, error_body(404, 0, "Not Found", "not a peerparty Remote Config route"));
    }
    let file = root().map(|r| r.join("remote-config.json"));
    let settings = match file.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| Json::parse(&t)) {
        Some(j @ Json::Obj(_)) => j,
        _ => Json::Obj(vec![]),
    };
    let n = if let Json::Obj(v) = &settings { v.len() } else { 0 };
    crate::log::throttled("ugs remote-config", 2, |c| log!("ugs remote config n={c}: {n} settings from disk"));
    let metadata = Json::obj(vec![
        ("assignmentId", Json::str("peerparty")),
        ("environmentId", Json::str("peerparty")),
        ("configAssignmentHash", Json::str(format!("peerparty-{n}"))),
    ]);
    let body = Json::obj(vec![
        ("configs", Json::obj(vec![("settings", settings.clone())])),
        // Older runtimes read the settings at the top.
        ("settings", settings),
        ("metadata", metadata),
    ]);
    (200, body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percent_before_part_of_a_character_is_kept() {
        let e = char::from_u32(0xe9).unwrap();
        assert_eq!(percent_decode(&format!("a%41+%e{e}%{e}{e}%4")), format!("aA %e{e}%{e}{e}%4"));
        assert_eq!(query_param(&format!("/x?keys=%{e}&k=1"), "keys"), Some(format!("%{e}")));
    }

    fn with_dir<T>(f: impl FnOnce() -> T) -> T {
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("peerparty-ugs-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::set_var("PEERPARTY_UGS_DIR", &dir) };
        let r = f();
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    fn json(r: &Reply) -> Json {
        Json::parse(&r.1).unwrap_or(Json::Null)
    }

    #[test]
    fn cloud_save_keeps_items_with_write_locks() {
        with_dir(|| {
            let base = "/v1/data/projects/p1/players/abc";
            let r = cloud_save("POST", &format!("{base}/items"), &Json::parse(r#"{"key":"level","value":{"n":3}}"#).unwrap());
            assert_eq!(r.0, 200);
            let lock = json(&r).get("writeLock").as_str().unwrap().to_string();
            let got = json(&cloud_save("GET", &format!("{base}/items?keys=level"), &Json::Null));
            assert_eq!(got.get("results").as_arr()[0].get("value").get("n").as_f64(), Some(3.0));
            // A stale lock is a conflict; the current one works.
            let stale = Json::parse(r#"{"key":"level","value":4,"writeLock":"old"}"#).unwrap();
            assert_eq!(cloud_save("POST", &format!("{base}/items"), &stale).0, 409);
            let good = Json::parse(&format!(r#"{{"data":[{{"key":"level","value":4,"writeLock":"{lock}"}},{{"key":"gold","value":"9"}}]}}"#)).unwrap();
            assert_eq!(cloud_save("POST", &format!("{base}/item-batch"), &good).0, 200);
            let keys = json(&cloud_save("GET", &format!("{base}/keys"), &Json::Null));
            assert_eq!(keys.get("results").as_arr().len(), 2);
            assert_eq!(cloud_save("DELETE", &format!("{base}/items/gold"), &Json::Null).0, 204);
            assert_eq!(cloud_save("DELETE", &format!("{base}/items/gold"), &Json::Null).0, 404);
            // Public data is kept apart; protected data can't be written by the game.
            assert_eq!(json(&cloud_save("GET", &format!("{base}/public/items"), &Json::Null)).get("results").as_arr().len(), 0);
            assert_eq!(cloud_save("POST", &format!("{base}/protected/items"), &stale).0, 404);
        });
    }

    #[test]
    fn remote_config_reads_its_file() {
        with_dir(|| {
            let r = json(&remote_config("POST", "/api/v1/settings"));
            assert!(matches!(r.get("configs").get("settings"), Json::Obj(v) if v.is_empty()));
            let dir = root().unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("remote-config.json"), r#"{"difficulty":2,"motd":"hi"}"#).unwrap();
            let r = json(&remote_config("POST", "/api/v1/settings"));
            assert_eq!(r.get("configs").get("settings").get("difficulty").as_f64(), Some(2.0));
            assert_eq!(r.get("settings").get("motd").as_str(), Some("hi"));
        });
    }

    #[test]
    fn query_params_decode() {
        assert_eq!(query_param("/x?keys=a%2Cb&y=1", "keys").as_deref(), Some("a,b"));
        assert_eq!(query_param("/x?keys=a", "y"), None);
    }
}
