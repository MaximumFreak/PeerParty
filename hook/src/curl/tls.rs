//! Certificates for a curl that does its own TLS (OpenSSL, mbedTLS and the like, linked
//! into the game or in a libcurl DLL): it never asks crypt32, so the hooks that make the
//! game trust PeerParty's certificate (https/tls.rs) don't reach it, and a request to a
//! host PeerParty answers fails its certificate check.
//!
//! So for a handle whose URL is an `https://` host whose lookup comes to PeerParty's
//! HTTPS server (net/dns.rs `answered_here`), curl is told to accept that server's key
//! and no other: `CURLOPT_PINNEDPUBLICKEY` names PeerParty's key (https/tls.rs
//! `curl_pin`), and curl's chain and name checks are off (`CURLOPT_SSL_VERIFYPEER`,
//! `_VERIFYHOST`, `_VERIFYSTATUS` 0; curl checks a pinned key whatever those say, and
//! aborts before sending anything when it differs). An engine's own check installed
//! through `CURLOPT_SSL_CTX_FUNCTION` (OpenSSL's verify callback) is taken off too. A
//! curl that can't pin (an older one, or a TLS back end without it) is left as it is:
//! its request fails, rather than going to whichever server answers.
//!
//! What the game sets for these options is kept, per handle: while the handle is
//! PeerParty's, curl gets PeerParty's values; when the game points it at another host,
//! the game's (or curl's defaults) go back. A handle copied with `curl_easy_duphandle`
//! keeps the pin without PeerParty knowing it: pointed elsewhere, its requests fail.

use std::collections::HashMap;
use std::ffi::{CStr, CString};

pub const OPT_SSL_VERIFYPEER: u32 = 64;
pub const OPT_SSL_VERIFYHOST: u32 = 81;
pub const OPT_SSL_VERIFYSTATUS: u32 = 232;
pub const OPT_SSL_CTX_FUNCTION: u32 = 20108;
pub const OPT_PINNEDPUBLICKEY: u32 = 10230;

/// Handles remembered at most: a game that never cleans its handles up (or whose
/// `curl_easy_cleanup` isn't hooked) doesn't grow this without end.
const KEEP: usize = 4096;

/// A value curl is given for an option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Num(usize),
    /// A string option; None is NULL.
    Text(Option<CString>),
}

impl Value {
    /// What `curl_easy_setopt` is passed (valid while `self` lives).
    pub fn arg(&self) -> usize {
        match self {
            Value::Num(n) => *n,
            Value::Text(Some(s)) => s.as_ptr() as usize,
            Value::Text(None) => 0,
        }
    }
}

/// What the game set on a handle, of the options PeerParty changes; None is curl's default.
#[derive(Clone, Debug, Default)]
struct Game {
    peer: Option<usize>,
    host: Option<usize>,
    status: Option<usize>,
    ctx: Option<usize>,
    pin: Option<Option<CString>>,
}

#[derive(Debug, Default)]
struct Handle {
    game: Game,
    /// Its URL is a host PeerParty answers, and curl took the pin.
    ours: bool,
}

#[derive(Debug, Default)]
pub struct Handles {
    map: HashMap<usize, Handle>,
}

pub fn is_option(option: u32) -> bool {
    matches!(option, OPT_SSL_VERIFYPEER | OPT_SSL_VERIFYHOST | OPT_SSL_VERIFYSTATUS | OPT_SSL_CTX_FUNCTION | OPT_PINNEDPUBLICKEY)
}

impl Handles {
    fn entry(&mut self, h: usize) -> &mut Handle {
        if self.map.len() >= KEEP && !self.map.contains_key(&h) {
            self.map.clear();
        }
        self.map.entry(h).or_default()
    }

    /// The game sets one of the options (`is_option`) to `value` (`text`: a string
    /// option's text, read from the game's pointer). What curl gets instead, if the
    /// handle is PeerParty's. `pin` is only asked when this handle is ours and the
    /// option is `OPT_PINNEDPUBLICKEY`, so non-PeerParty curl calls never trigger
    /// certificate creation.
    pub fn set<'a>(&mut self, h: usize, option: u32, value: usize, text: Option<CString>, pin: impl FnOnce() -> Option<&'a CStr>) -> Option<Value> {
        let handle = self.entry(h);
        let g = &mut handle.game;
        match option {
            OPT_SSL_VERIFYPEER => g.peer = Some(value),
            OPT_SSL_VERIFYHOST => g.host = Some(value),
            OPT_SSL_VERIFYSTATUS => g.status = Some(value),
            OPT_SSL_CTX_FUNCTION => g.ctx = Some(value),
            OPT_PINNEDPUBLICKEY => g.pin = Some(text),
            _ => return None,
        }
        if !handle.ours {
            return None;
        }
        match option {
            OPT_PINNEDPUBLICKEY => pin().map(|p| Value::Text(Some(p.to_owned()))),
            _ => Some(Value::Num(0)),
        }
    }

    /// The handle's URL is set: `local` when its host is answered here. The options to
    /// set before it, in order; when the handle becomes PeerParty's, the first is the
    /// pin, and if curl refuses it the caller calls `unpinned` and sets none of the rest.
    pub fn url(&mut self, h: usize, local: bool, pin: &CStr) -> Vec<(u32, Value)> {
        let handle = self.entry(h);
        match (handle.ours, local) {
            (false, true) => {
                handle.ours = true;
                [OPT_PINNEDPUBLICKEY, OPT_SSL_VERIFYPEER, OPT_SSL_VERIFYHOST, OPT_SSL_VERIFYSTATUS, OPT_SSL_CTX_FUNCTION]
                    .into_iter()
                    .map(|o| (o, ours(o, pin)))
                    .collect()
            }
            (true, false) => {
                handle.ours = false;
                let g = handle.game.clone();
                vec![
                    (OPT_PINNEDPUBLICKEY, Value::Text(g.pin.flatten())),
                    (OPT_SSL_VERIFYPEER, Value::Num(g.peer.unwrap_or(1))),
                    (OPT_SSL_VERIFYHOST, Value::Num(g.host.unwrap_or(2))),
                    (OPT_SSL_VERIFYSTATUS, Value::Num(g.status.unwrap_or(0))),
                    (OPT_SSL_CTX_FUNCTION, Value::Num(g.ctx.unwrap_or(0))),
                ]
            }
            _ => Vec::new(),
        }
    }

    /// curl refused the pin: the handle stays the game's.
    pub fn unpinned(&mut self, h: usize) {
        if let Some(handle) = self.map.get_mut(&h) {
            handle.ours = false;
        }
    }

    /// The handle is gone (`curl_easy_cleanup`) or back to curl's defaults
    /// (`curl_easy_reset`): nothing is kept for it.
    pub fn forget(&mut self, h: usize) {
        self.map.remove(&h);
    }

    pub fn is_ours(&self, h: usize) -> bool {
        self.map.get(&h).is_some_and(|x| x.ours)
    }
}

fn ours(option: u32, pin: &CStr) -> Value {
    match option {
        OPT_PINNEDPUBLICKEY => Value::Text(Some(pin.to_owned())),
        _ => Value::Num(0),
    }
}

/// The host of an `https://` URL (no port, user or brackets), lowercase; None for any
/// other scheme, or an IP literal in brackets.
pub fn https_host(url: &str) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    if host.starts_with('[') {
        return None;
    }
    let host = host.split(':').next()?.trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin() -> CString {
        CString::new("sha256//AAAA").unwrap()
    }

    #[test]
    fn hosts_come_from_https_urls_only() {
        assert_eq!(https_host("https://ABCD.PlayFabAPI.com/Client/Login?x=1").as_deref(), Some("abcd.playfabapi.com"));
        assert_eq!(https_host("HTTPS://user:pw@host.example:8443/").as_deref(), Some("host.example"));
        assert_eq!(https_host("https://host.example.?q").as_deref(), Some("host.example"));
        assert_eq!(https_host("https://host.example#frag").as_deref(), Some("host.example"));
        assert_eq!(https_host("http://host.example/"), None);
        assert_eq!(https_host("host.example/path"), None);
        assert_eq!(https_host("https://[::1]:443/"), None);
        assert_eq!(https_host("https:///path"), None);
    }

    #[test]
    fn a_handle_for_a_local_host_takes_the_pin_and_gives_the_games_settings_back() {
        let pin = pin();
        let no_pin = || -> Option<&CStr> { panic!("pin must not be asked") };
        let mut t = Handles::default();
        // The game's own settings before the URL: passed on as they are, without asking for the pin.
        assert_eq!(t.set(1, OPT_SSL_VERIFYPEER, 1, None, no_pin), None);
        assert_eq!(t.set(1, OPT_SSL_CTX_FUNCTION, 0x5000, None, no_pin), None);
        assert_eq!(t.set(1, OPT_PINNEDPUBLICKEY, 0x6000, Some(CString::new("sha256//GAME").unwrap()), no_pin), None);
        // A local host: the pin first, then the checks off.
        let plan = t.url(1, true, &pin);
        assert_eq!(plan[0], (OPT_PINNEDPUBLICKEY, Value::Text(Some(pin.clone()))));
        assert!(plan[1..].iter().all(|(_, v)| *v == Value::Num(0)));
        assert_eq!(plan.len(), 5);
        assert!(t.is_ours(1));
        // Another local URL changes nothing; the game's later settings are kept, not used.
        assert!(t.url(1, true, &pin).is_empty());
        assert_eq!(t.set(1, OPT_SSL_VERIFYHOST, 2, None, no_pin), Some(Value::Num(0)));
        assert_eq!(t.set(1, OPT_PINNEDPUBLICKEY, 0, None, || Some(pin.as_c_str())), Some(Value::Text(Some(pin.clone()))));
        // Another host: the game's values (or curl's defaults) go back.
        let back = t.url(1, false, &pin);
        assert_eq!(
            back,
            vec![
                (OPT_PINNEDPUBLICKEY, Value::Text(None)),
                (OPT_SSL_VERIFYPEER, Value::Num(1)),
                (OPT_SSL_VERIFYHOST, Value::Num(2)),
                (OPT_SSL_VERIFYSTATUS, Value::Num(0)),
                (OPT_SSL_CTX_FUNCTION, Value::Num(0x5000)),
            ]
        );
        assert!(!t.is_ours(1));
        assert_eq!(t.set(1, OPT_SSL_VERIFYPEER, 0, None, no_pin), None, "the game's own again");
    }

    #[test]
    fn defaults_come_back_when_the_game_set_nothing() {
        let pin = pin();
        let mut t = Handles::default();
        t.url(7, true, &pin);
        let back = t.url(7, false, &pin);
        assert_eq!(back[0], (OPT_PINNEDPUBLICKEY, Value::Text(None)));
        assert_eq!(back[1], (OPT_SSL_VERIFYPEER, Value::Num(1)));
        assert_eq!(back[2], (OPT_SSL_VERIFYHOST, Value::Num(2)));
    }

    #[test]
    fn a_refused_pin_or_a_forgotten_handle_leaves_the_game_alone() {
        let pin = pin();
        let no_pin = || -> Option<&CStr> { panic!("pin must not be asked") };
        let mut t = Handles::default();
        t.url(2, true, &pin);
        t.unpinned(2);
        assert!(!t.is_ours(2));
        assert_eq!(t.set(2, OPT_SSL_VERIFYPEER, 1, None, no_pin), None);
        assert!(t.url(2, false, &pin).is_empty(), "nothing to give back");
        t.url(3, true, &pin);
        t.forget(3);
        assert!(!t.is_ours(3));
        assert!(t.url(3, false, &pin).is_empty());
        // Non-TLS options are none of this.
        assert_eq!(t.set(3, 10002, 1, None, no_pin), None);
        assert!(is_option(OPT_PINNEDPUBLICKEY) && !is_option(10002));
    }

    #[test]
    fn values_pass_as_curl_takes_them() {
        assert_eq!(Value::Num(5).arg(), 5);
        assert_eq!(Value::Text(None).arg(), 0);
        let v = Value::Text(Some(pin()));
        let p = v.arg() as *const std::ffi::c_char;
        assert_eq!(unsafe { CStr::from_ptr(p) }.to_str().unwrap(), "sha256//AAAA");
    }
}
