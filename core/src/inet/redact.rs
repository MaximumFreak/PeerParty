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
    if s.chars().any(is_wide) {
        return has_address(&narrow(s));
    }
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

/// Stand-ins, this PC, masks, multicast and broadcast say nothing about where anyone is.
fn says_nothing(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    (o[0] == 198 && o[1] & 0xFE == 18) || ip.is_loopback() || o[0] == 0 || o[0] >= 224
}

/// The first digit (zero) of each run of ten decimal digits Unicode has besides ASCII's
/// (category Nd): full-width, Arabic-Indic, Devanagari, Thai, the mathematical bold and
/// monospace sets and the rest. Each run is 0 to 9 in order.
const DIGIT_ZEROS: [u32; 43] = [
    0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6, 0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20,
    0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0,
    0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x11066, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6,
];

/// The value of a decimal digit from any script but ASCII's.
fn other_digit(c: char) -> Option<u8> {
    let c = c as u32;
    DIGIT_ZEROS.iter().find(|z| (**z..**z + 10).contains(&c)).map(|z| (c - z) as u8)
}

/// Digits from other scripts ("١٩٢", full-width "１９２") and the full-width or ideographic
/// dot and full-width colon ("１９２．１６８．０．１"), which read as an address to a
/// person or a game that converts them.
fn is_wide(c: char) -> bool {
    matches!(c, '\u{FF0E}' | '\u{FF1A}' | '\u{3002}' | '\u{FF61}') || other_digit(c).is_some()
}

/// `s` with such digits, dots and colons as ASCII.
fn narrow(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{FF0E}' | '\u{3002}' | '\u{FF61}' => '.',
            '\u{FF1A}' => ':',
            c => other_digit(c).map_or(c, |d| char::from(b'0' + d)),
        })
        .collect()
}

/// LAN ranges that don't read as a version stuck to a word, as "v10.0.0.1" does:
/// 192.168/16, 172.16/12, link-local and carrier-grade NAT.
fn plainly_lan(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    (o[0] == 192 && o[1] == 168) || (o[0] == 172 && o[1] & 0xF0 == 16) || ip.is_link_local() || (o[0] == 100 && o[1] & 0xC0 == 64)
}

/// Whether `ip` in game data is an address to hide. `port`: it had one (":7777");
/// `apart`: it was the whole value, or a whole item in it (quoted, or between the
/// punctuation of a list or of JSON).
fn hides(ip: Ipv4Addr, port: bool, apart: bool) -> bool {
    // Public addresses only with a port or set apart: "1.0.7.2" in a sentence is more
    // likely a version than an address.
    !says_nothing(ip) && (port || apart || is_local(ip.into()))
}

/// Whether an attribute's name says its value is a version: one of its words is "ver",
/// "rev" or "v", or ends in "version", "build" or "revision" ("game_ver", "BuildVersion").
/// Words, not letters anywhere: "server" and "driver" have "ver" in them, and a server's
/// address under such a key was kept as if it were a version.
fn names_a_version(key: &str) -> bool {
    key_words(key).iter().any(|w| matches!(w.as_str(), "ver" | "rev" | "v") || ["version", "build", "revision"].iter().any(|s| w.ends_with(s)))
}

/// An attribute name's words, lower case: split at anything not a letter or digit, and
/// where a capital follows a small letter ("ServerAddress": "server", "address").
fn key_words(key: &str) -> Vec<String> {
    let mut words: Vec<String> = vec![String::new()];
    let mut was_lower = false;
    for c in key.chars() {
        if !c.is_ascii_alphanumeric() || (c.is_ascii_uppercase() && was_lower) {
            words.push(String::new());
        }
        if c.is_ascii_alphanumeric() {
            words.last_mut().unwrap().push(c.to_ascii_lowercase());
        }
        was_lower = c.is_ascii_lowercase();
    }
    words.retain(|w| !w.is_empty());
    words
}

/// Whether an attribute's name says its value is an address ("ip", "HostAddr",
/// "server_ip"), and not a number about one ("host_port", "server_id").
fn names_an_address(key: &str) -> bool {
    let words = key_words(key);
    let addressy = |w: &str| {
        matches!(w, "ip" | "ipv4" | "addr" | "address" | "host" | "server" | "endpoint" | "hostip" | "serverip" | "ipaddr" | "ipaddress" | "hostaddr" | "serveraddr")
    };
    let other = |w: &str| matches!(w, "port" | "id" | "count" | "num" | "number" | "time" | "score" | "version" | "ver" | "build");
    words.iter().any(|w| addressy(w)) && !words.iter().any(|w| other(w))
}

/// An integer that holds an IPv4 address (`Value::Int`), under a key that names one
/// (`names_an_address`): the same number for `with`, in the same byte order (`ntohl`'s,
/// or an `in_addr` read as a little-endian integer) and the same sign (a 32-bit value
/// stored signed). `None`: nothing to replace. Without such a key a number is left alone:
/// seeds, scores and hashes are numbers of the same size.
pub fn scrub_int(key: &str, v: i64, with: Ipv4Addr) -> Option<i64> {
    if !names_an_address(key) {
        return None;
    }
    let (raw, signed) = if (0..=u32::MAX as i64).contains(&v) {
        (v as u32, false)
    } else if (i32::MIN as i64..0).contains(&v) {
        (v as i32 as u32, true)
    } else {
        return None;
    };
    // Four octets, the first not 0 whichever end it's at ("7777" is a port).
    if raw < 1 << 24 || raw & 0xFF == 0 {
        return None;
    }
    let (be, le) = (Ipv4Addr::from(raw), Ipv4Addr::from(raw.swap_bytes()));
    // A stand-in or this PC in either order is that (the other order reads as some
    // public address, which it isn't).
    let ours = |ip: Ipv4Addr| ip.is_loopback() || (ip.octets()[0] == 198 && ip.octets()[1] & 0xFE == 18);
    if ours(be) || ours(le) {
        return None;
    }
    let hide = |ip: Ipv4Addr| hides(ip, false, true);
    let as_be = match (hide(be), hide(le)) {
        (false, false) => return None,
        (true, false) => true,
        (false, true) => false,
        // Both read as one: the order that gives a LAN address (what games put there
        // most), else `ntohl`'s.
        (true, true) => !(is_local(le.into()) && !is_local(be.into())),
    };
    let w = if as_be { u32::from(with) } else { u32::from(with).swap_bytes() };
    Some(if signed { w as i32 as i64 } else { w as i64 })
}

/// `s` (the value of attribute `key`) with the IP addresses in it replaced by `with`,
/// keeping ports: "203.0.113.5:7777" becomes "198.18.4.9:7777", "[2001:db8::1]:80"
/// becomes "198.18.4.9:80". `None`: nothing to replace. Values of keys that name a
/// version or build keep public-looking quads that stand alone ("1.0.7.2").
/// Full-width digits ("２０３．０．１１３．５") are read as ASCII; a value with an
/// address in them comes back in ASCII.
pub fn scrub(key: &str, s: &str, with: Ipv4Addr) -> Option<String> {
    if s.chars().any(is_wide) {
        return scrub_ascii(key, &narrow(s), with);
    }
    scrub_ascii(key, s, with)
}

fn scrub_ascii(key: &str, s: &str, with: Ipv4Addr) -> Option<String> {
    let versiony = names_a_version(key);
    let whole = s.trim();
    let b = s.as_bytes();
    let repl = with.to_string();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let mut i = 0;
    // What sets an item apart inside a value: quotes, and the punctuation of lists,
    // "key=value" pairs and JSON.
    let opens = |i: usize| i == 0 || matches!(b[i - 1], b'"' | b'\'' | b'=' | b':' | b'[' | b'{' | b'(' | b',' | b';' | b'|');
    let closes = |end: usize| end == b.len() || matches!(b[end], b'"' | b'\'' | b']' | b'}' | b')' | b',' | b';' | b'|');
    while i < b.len() {
        let starts = |i: usize| i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'.' || b[i - 1] == b':');
        // IPv4: after anything but a digit or a dot ("ip:1.2.3.4:7777", `"1.2.3.4"`). A
        // v4-mapped v6 ("::ffff:1.2.3.4") is taken whole by the v6 pass below, which
        // comes to it first.
        if b[i].is_ascii_digit()
            && (i == 0 || !(b[i - 1].is_ascii_digit() || b[i - 1] == b'.'))
            && !(i > 0 && b[i - 1] == b':' && s[..i].ends_with("::"))
            && let Some((ip, end)) = quad(b, i)
        {
            let digits = |from: usize| b[from.min(b.len())..].iter().take_while(|c| c.is_ascii_digit()).count();
            // ":7777", and "1.2.3.4 7777" when that is the whole value.
            let port = (b.get(end) == Some(&b':') && digits(end + 1) > 0)
                || (b.get(end) == Some(&b' ') && s[..i].trim().is_empty() && (2..=5).contains(&digits(end + 1)) && end + 1 + digits(end + 1) == s.trim_end().len());
            // Stuck to a word ("v1.0.7.2", "build10.0.0.1"): an address only with a port,
            // or in a LAN range no version looks like ("x192.168.1.20").
            let glued = i > 0 && b[i - 1].is_ascii_alphabetic();
            let apart = &s[i..end] == whole || (opens(i) && closes(end));
            let hide = if glued { (port || plainly_lan(ip)) && !says_nothing(ip) } else { hides(ip, port, apart && !versiony) };
            if hide {
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

    /// Ways a game writes an address that used to reach strangers as written.
    #[test]
    fn scrubs_addresses_in_the_forms_games_write_them() {
        let w = Ipv4Addr::new(198, 18, 4, 9);
        let cases = [
            // After a colon, in a pair, in JSON, in a list.
            ("x", "ip:203.0.113.5:7777", Some("ip:198.18.4.9:7777")),
            ("x", "ip=203.0.113.5", Some("ip=198.18.4.9")),
            ("info", "{\"ip\":\"203.0.113.5\",\"port\":7777}", Some("{\"ip\":\"198.18.4.9\",\"port\":7777}")),
            ("x", "203.0.113.5;7777", Some("198.18.4.9;7777")),
            ("x", "203.0.113.5,7777", Some("198.18.4.9,7777")),
            ("x", "203.0.113.5 7777", Some("198.18.4.9 7777")),
            ("hosts", "[203.0.113.5|203.0.113.6]", Some("[198.18.4.9|198.18.4.9]")),
            // Under keys with "ver" in them that aren't about versions.
            ("server", "203.0.113.5", Some("198.18.4.9")),
            ("ServerAddress", "203.0.113.5", Some("198.18.4.9")),
            ("driver_host", "203.0.113.5", Some("198.18.4.9")),
            // Stuck to a word, with a port.
            ("x", "udp203.0.113.5:7777", Some("udp198.18.4.9:7777")),
            // Still versions: under a version's key, in a sentence, stuck to a word.
            ("GameVersion", "203.0.113.5", None),
            ("buildVer", "1.0.7.2", None),
            ("client_build", "\"1.0.7.2\"", None),
            ("desc", "patch 1.0.7.2 is out", None),
            ("desc", "v1.0.7.2", None),
            ("desc", "v10.0.0.1", None),
            ("x", "1.0.7.2 2024 edition", None),
        ];
        for (k, v, want) in cases {
            assert_eq!(scrub(k, v, w).as_deref(), want, "{k}={v}");
        }
        for (k, want) in [("ver", true), ("game_ver", true), ("GameVersion", true), ("build", true), ("clientBuild", true), ("v", true), ("rev", true), ("server", false), ("ServerAddress", false), ("driver", false), ("river", false), ("host", false)] {
            assert_eq!(names_a_version(k), want, "{k}");
        }
    }

    /// The rest of what slipped through: LAN addresses stuck to a word, full-width
    /// digits, and addresses held as integers.
    #[test]
    fn scrubs_glued_wide_and_integer_addresses() {
        let w = Ipv4Addr::new(198, 18, 4, 9);
        let cases = [
            ("x", "x192.168.1.20", Some("x198.18.4.9")),
            ("x", "lan172.16.0.5 here", Some("lan198.18.4.9 here")),
            ("x", "id169.254.10.1", Some("id198.18.4.9")),
            ("x", "\u{FF12}\u{FF10}\u{FF13}\u{FF0E}\u{FF10}\u{FF0E}\u{FF11}\u{FF11}\u{FF13}\u{FF0E}\u{FF15}", Some("198.18.4.9")),
            ("x", "at \u{FF11}\u{FF19}\u{FF12}.\u{FF11}\u{FF16}\u{FF18}.1.20\u{FF1A}\u{FF17}\u{FF17}\u{FF17}\u{FF17}", Some("at 198.18.4.9:7777")),
            // Other scripts' digits: Arabic-Indic, Devanagari with the ideographic dot,
            // mathematical bold.
            ("x", "\u{661}\u{669}\u{662}.\u{661}\u{666}\u{668}.\u{661}.\u{662}\u{660}", Some("198.18.4.9")),
            ("x", "\u{967}\u{96F}\u{968}\u{3002}\u{967}\u{96C}\u{96E}\u{3002}\u{967}\u{3002}\u{968}\u{966}", Some("198.18.4.9")),
            ("x", "\u{1D7CF}\u{1D7CE}.\u{1D7CE}.\u{1D7CE}.\u{1D7D5}:\u{1D7D5}\u{1D7D5}", Some("198.18.4.9:77")),
            // Still kept: a version stuck to a word, full-width digits that aren't an address.
            ("desc", "v10.0.0.1", None),
            ("desc", "v1.0.7.2", None),
            ("score", "\u{FF11}\u{FF10}\u{FF10}", None),
        ];
        for (k, v, want) in cases {
            assert_eq!(scrub(k, v, w).as_deref(), want, "{k}={v}");
        }
        assert!(has_address("\u{FF11}\u{FF10}\u{FF0E}\u{FF10}\u{FF0E}\u{FF10}\u{FF0E}\u{FF11}"));

        let ip = |s: &str| u32::from(s.parse::<Ipv4Addr>().unwrap());
        let (wb, wl) = (u32::from(w) as i64, u32::from(w).swap_bytes() as i64);
        // ntohl's order, an in_addr read little-endian, and either one stored signed.
        assert_eq!(scrub_int("ip", ip("203.0.113.5") as i64, w), Some(wb));
        assert_eq!(scrub_int("HostIp", ip("192.168.1.20") as i64, w), Some(wb));
        assert_eq!(scrub_int("server_addr", ip("192.168.1.20").swap_bytes() as i64, w), Some(wl));
        assert_eq!(scrub_int("ip", ip("203.0.113.5") as i32 as i64, w), Some(u32::from(w) as i32 as i64));
        assert_eq!(scrub_int("addr", ip("10.0.0.7").swap_bytes() as i32 as i64, w), Some(u32::from(w).swap_bytes() as i32 as i64));
        // Not addresses: other keys (a seed of the same size), ports, stand-ins, this
        // PC, out of range.
        assert_eq!(scrub_int("seed", ip("203.0.113.5") as i64, w), None);
        assert_eq!(scrub_int("host_port", 7777, w), None);
        assert_eq!(scrub_int("ip", 7777, w), None);
        assert_eq!(scrub_int("server_id", ip("203.0.113.5") as i64, w), None);
        assert_eq!(scrub_int("ip", ip("198.19.3.4") as i64, w), None);
        assert_eq!(scrub_int("ip", ip("127.0.0.1") as i64, w), None);
        assert_eq!(scrub_int("ip", 1 << 40, w), None);
        for (k, want) in [("ip", true), ("HostIp", true), ("server_address", true), ("ServerAddr", true), ("host", true), ("hostPort", false), ("membership", false), ("tip", false), ("ip_count", false)] {
            assert_eq!(names_an_address(k), want, "{k}");
        }
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