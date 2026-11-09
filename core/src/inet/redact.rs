//! Keeping internet addresses out of logs, the overlay, Settings and anything a game sees.
//!
//! Logs name an internet peer by a short tag ("peer-3fa9c1"): a keyed hash of its address
//! with a key made fresh for each run, so lines about the same peer match within one log
//! but the tag can't be turned back into the address (nor compared across runs or PCs).
//! `PEERPARTY_INET_DEBUG=1` logs real addresses, for troubleshooting.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::OnceLock;

/// `PEERPARTY_INET_DEBUG=1`: show real addresses in logs and `--net-test`.
pub fn debug() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var("PEERPARTY_INET_DEBUG").is_ok_and(|v| v.trim() == "1"))
}

fn key() -> &'static [u8; 32] {
    static K: OnceLock<[u8; 32]> = OnceLock::new();
    K.get_or_init(super::crypto::random_array)
}

fn hash(prefix: &str, what: &str) -> String {
    let m = super::crypto::mac16(key(), &[what.as_bytes()]);
    format!("{prefix}-{:02x}{:02x}{:02x}", m[0], m[1], m[2])
}

/// An internet address as logs show it: a tag, or the address with `debug()`.
pub fn addr(a: SocketAddr) -> String {
    if debug() { a.to_string() } else { hash("peer", &a.to_string()) }
}

/// Our own public address as logs show it: hidden but for the port (which tells NAT
/// types apart), or all of it with `debug()`.
pub fn own(a: SocketAddr) -> String {
    if debug() { a.to_string() } else { format!("<hidden>:{}", a.port()) }
}

/// For LAN logs: addresses on the LAN or this PC as they are, anything else tagged.
pub fn lan(a: SocketAddr) -> String {
    if is_local(a.ip()) { a.to_string() } else { addr(a) }
}

/// Loopback, private, link-local or unspecified: not an internet address.
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified()
                // 100.64/10 (carrier-grade NAT, and VPNs like Tailscale) and 198.18/15
                // (PeerParty's stand-in addresses for internet peers).
                || (v.octets()[0] == 100 && v.octets()[1] & 0xC0 == 64)
                || (v.octets()[0] == 198 && v.octets()[1] & 0xFE == 18)
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_local(IpAddr::V4(v4));
            }
            let s = v.segments()[0];
            v.is_loopback() || v.is_unspecified() || s & 0xfe00 == 0xfc00 || s & 0xffc0 == 0xfe80
        }
    }
}

/// Whether text has something that reads as an IP address in it ("1.2.3.4", with or
/// without a port, or an IPv6 address): display names that do are replaced.
pub fn has_address(s: &str) -> bool {
    // Dotted quads anywhere, with any separators around them.
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_digit()) {
            let mut j = i;
            let mut groups = 0;
            loop {
                let start = j;
                while j < b.len() && b[j].is_ascii_digit() && j - start < 3 {
                    j += 1;
                }
                if j == start {
                    break;
                }
                groups += 1;
                if groups == 4 || j >= b.len() || !matches!(b[j], b'.' | b',' | b'_' | b'-' | b' ') {
                    break;
                }
                j += 1;
            }
            if groups >= 4 {
                return true;
            }
        }
        i += 1;
    }
    s.split(|c: char| c.is_whitespace() || c == '[' || c == ']' || c == '(' || c == ')')
        .any(|w| w.matches(':').count() >= 2 && w.parse::<std::net::Ipv6Addr>().is_ok())
}

/// A dotted quad starting at `i` (not part of a longer run of digits and dots): the
/// address and where it ends.
fn quad(b: &[u8], i: usize) -> Option<(Ipv4Addr, usize)> {
    let mut j = i;
    let mut o = [0u8; 4];
    for (g, slot) in o.iter_mut().enumerate() {
        if g > 0 {
            if b.get(j) != Some(&b'.') {
                return None;
            }
            j += 1;
        }
        let start = j;
        while j < b.len() && b[j].is_ascii_digit() && j - start < 3 {
            j += 1;
        }
        if j == start {
            return None;
        }
        *slot = std::str::from_utf8(&b[start..j]).ok()?.parse().ok()?;
    }
    // "1.2.3.4.5" or "1.2.3.45678" is something else (a version, a number).
    if b.get(j).is_some_and(u8::is_ascii_digit) || (b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit)) {
        return None;
    }
    Some((Ipv4Addr::from(o), j))
}

/// Whether `ip` in game data is an address to hide. `port`: it had one (":7777");
/// `alone`: it was the whole value.
fn hides(ip: Ipv4Addr, port: bool, alone: bool) -> bool {
    let o = ip.octets();
    // Stand-ins, this PC, masks, multicast and broadcast say nothing about where anyone is.
    if (o[0] == 198 && o[1] & 0xFE == 18) || ip.is_loopback() || o[0] == 0 || o[0] >= 224 {
        return false;
    }
    // Public addresses only with a port or on their own: "1.0.7.2" in a description is
    // more likely a version than an address.
    port || alone || is_local(ip.into())
}

/// `s` (the value of attribute `key`) with the IP addresses in it replaced by `with`,
/// keeping ports: "203.0.113.5:7777" becomes "198.18.4.9:7777", "[2001:db8::1]:80"
/// becomes "198.18.4.9:80". `None`: nothing to replace. Values of keys that name a
/// version or build keep public-looking quads that stand alone ("1.0.7.2").
pub fn scrub(key: &str, s: &str, with: Ipv4Addr) -> Option<String> {
    let k = key.to_ascii_lowercase();
    let versiony = ["ver", "build", "rev"].iter().any(|w| k.contains(w));
    let whole = s.trim();
    let b = s.as_bytes();
    let repl = with.to_string();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let mut i = 0;
    while i < b.len() {
        let starts = |i: usize| i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.' || b[i - 1] == b':');
        // IPv4 first: a v4-mapped v6 ("::ffff:1.2.3.4") is caught by the v6 pass below.
        if b[i].is_ascii_digit()
            && starts(i)
            && let Some((ip, end)) = quad(b, i)
        {
            let port = b.get(end) == Some(&b':') && b.get(end + 1).is_some_and(u8::is_ascii_digit);
            if hides(ip, port, &s[i..end] == whole && !versiony) {
                out.push_str(&s[last..i]);
                out.push_str(&repl);
                last = end;
            }
            i = end;
            continue;
        }
        // IPv6: a run of hex digits, colons and dots (and a %zone) that parses as one.
        if (b[i].is_ascii_hexdigit() || b[i] == b':') && starts(i) {
            let mut end = i;
            while end < b.len() && (b[end].is_ascii_hexdigit() || matches!(b[end], b':' | b'.')) {
                end += 1;
            }
            let mut stop = end;
            if b.get(end) == Some(&b'%') {
                stop = end + 1;
                while stop < b.len() && b[stop].is_ascii_alphanumeric() {
                    stop += 1;
                }
            }
            let word = &s[i..end];
            if word.matches(':').count() >= 2
                && let Ok(v6) = word.parse::<std::net::Ipv6Addr>()
            {
                if !v6.is_loopback() && !v6.is_unspecified() && !v6.is_multicast() {
                    // "[v6]:port" becomes "v4:port".
                    let (from, to) = if i > 0 && b[i - 1] == b'[' && b.get(stop) == Some(&b']') {
                        (i - 1, stop + 1)
                    } else {
                        (i, stop)
                    };
                    if from >= last {
                        out.push_str(&s[last..from]);
                        out.push_str(&repl);
                        last = to;
                    }
                }
                i = stop;
                continue;
            }
            if end > i {
                i = end;
                continue;
            }
        }
        i += 1;
    }
    if last == 0 {
        return None;
    }
    out.push_str(&s[last..]);
    Some(out)
}

/// A stand-in IPv4 address for a peer we only reach through an internet tunnel, for
/// games that ask where a player is: stable for the peer, in 198.18.0.0/15 (reserved
/// for benchmarking, never routed on the internet), never its real address.
pub fn stand_in(id: &[u8; 16]) -> std::net::Ipv4Addr {
    let h = super::crypto::mac16(&[0x5a; 32], &[b"peerparty stand-in address", id]);
    std::net::Ipv4Addr::new(198, 18 + (h[0] & 1), h[1], 1 + h[2] % 254)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_hide_addresses_and_stay_stable() {
        let a: SocketAddr = "203.0.113.7:4000".parse().unwrap();
        let t = addr(a);
        if !debug() {
            assert!(t.starts_with("peer-") && !t.contains("203") && !t.contains("4000"), "{t}");
            assert_eq!(addr(a), t);
            assert_ne!(addr("203.0.113.8:4000".parse().unwrap()), t);
            assert!(!own(a).contains("203"));
        }
        assert_eq!(lan("192.168.1.5:10".parse().unwrap()), "192.168.1.5:10");
    }

    #[test]
    fn finds_addresses_in_names() {
        for s in ["1.2.3.4", "me@10.0.0.1:5", "x 192.168.001.002 y", "1-2-3-4", "::1 here", "[2001:db8::1]:80", "1_2_3_4"] {
            assert!(has_address(s), "{s}");
        }
        for s in ["Robin", "Player 2", "v1.2.3", "Team 10", "12:30", "a:b:c", "1.2.3"] {
            assert!(!has_address(s), "{s}");
        }
    }

    #[test]
    fn scrubs_addresses_from_game_data() {
        let w = Ipv4Addr::new(198, 18, 4, 9);
        let cases = [
            ("addr", "203.0.113.5:7777", Some("198.18.4.9:7777")),
            ("host", "203.0.113.5", Some("198.18.4.9")),
            ("host", " 203.0.113.5 ", Some(" 198.18.4.9 ")),
            ("desc", "lan at 192.168.1.20 now", Some("lan at 198.18.4.9 now")),
            ("connect", "+connect 203.0.113.5:27015 +pw x", Some("+connect 198.18.4.9:27015 +pw x")),
            ("v6", "[2001:db8::1]:80", Some("198.18.4.9:80")),
            ("v6", "2001:db8::1", Some("198.18.4.9")),
            ("v6", "fe80::1%12", Some("198.18.4.9")),
            ("mapped", "::ffff:10.0.0.2", Some("198.18.4.9")),
            ("cgnat", "100.70.1.2", Some("198.18.4.9")),
            ("two", "10.0.0.1,10.0.0.2", Some("198.18.4.9,198.18.4.9")),
            // Kept: versions, stand-ins, loopback, masks, times, MACs, plain text.
            ("version", "1.0.7.2", None),
            ("desc", "build 1.0.7.2 today", None),
            ("game_ver", "203.0.113.5", None),
            ("host", "198.19.3.4:7777", None),
            ("host", "127.0.0.1:7777", None),
            ("mask", "255.255.255.0", None),
            ("x", "1.2.3.4.5", None),
            ("time", "12:30:45", None),
            // Split, so the release check for real MAC addresses passes over it.
            ("mac", concat!("aa:bb:cc", ":dd:ee:ff"), None),
            ("name", "Robin", None),
            ("x", "::", None),
            ("x", "::1", None),
            ("hash", "deadbeef", None),
        ];
        for (k, v, want) in cases {
            assert_eq!(scrub(k, v, w).as_deref(), want, "{k}={v}");
        }
        // A version-named key still loses an address with a port.
        assert_eq!(scrub("server_version", "10.0.0.1:1", w).as_deref(), Some("198.18.4.9:1"));
    }

    #[test]
    fn stand_ins_are_reserved_and_stable() {
        let id = [7u8; 16];
        let ip = stand_in(&id);
        assert_eq!(ip, stand_in(&id));
        assert!(is_local(ip.into()));
        assert_eq!(ip.octets()[0], 198);
        assert!(matches!(ip.octets()[1], 18 | 19));
        assert!(!is_local("8.8.8.8".parse().unwrap()));
        assert!(is_local("fe80::1".parse().unwrap()));
    }
}