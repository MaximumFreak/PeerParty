//! Room codes: the shared secret two players type in to find each other on the internet.
//!
//! A code is 120 random bits, written in Crockford base32 with a 10-bit typo check:
//! `PP1-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-CC`. Everything the internet layer needs comes from
//! it through HKDF-SHA256: the pre-shared key that authenticates the handshake, the key
//! that hides which DHT topic a room meets on, and a short room id. Codes are always
//! generated (never a typed passphrase): the DHT topic is public, and a guessable secret
//! could be found from it offline.

use std::fmt;

use hkdf::Hkdf;
use sha1::Sha1;
use sha2::{Digest, Sha256};

const SECRET_LEN: usize = 15;
const DATA_CHARS: usize = SECRET_LEN * 8 / 5; // 24
const CHECK_CHARS: usize = 2;
/// Crockford's base32: no I, L, O or U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const PREFIX: &str = "PP1";

#[derive(Clone, PartialEq, Eq)]
pub struct RoomCode {
    secret: [u8; SECRET_LEN],
}

impl Drop for RoomCode {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.secret);
    }
}

impl fmt::Debug for RoomCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the secret itself: logs only ever show the fingerprint.
        write!(f, "RoomCode({})", self.fingerprint())
    }
}

impl fmt::Display for RoomCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut chars = encode(&self.secret);
        chars.extend_from_slice(&check(&self.secret));
        f.write_str(PREFIX)?;
        for group in chars.chunks(4) {
            f.write_str("-")?;
            f.write_str(std::str::from_utf8(group).unwrap_or("?"))?;
        }
        Ok(())
    }
}

fn decode_char(c: u8) -> Option<u8> {
    let c = match c.to_ascii_uppercase() {
        b'O' => b'0',
        b'I' | b'L' => b'1',
        c => c,
    };
    ALPHABET.iter().position(|&a| a == c).map(|p| p as u8)
}

fn encode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize]);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize]);
    }
    out
}

/// The two check characters: the first 10 bits of a hash of the secret.
fn check(secret: &[u8; SECRET_LEN]) -> [u8; CHECK_CHARS] {
    let h = Sha256::new()
        .chain_update(b"peerparty room code check")
        .chain_update(secret)
        .finalize();
    let v = ((h[0] as u32) << 2) | (h[1] as u32 >> 6);
    [ALPHABET[(v >> 5) as usize & 31], ALPHABET[v as usize & 31]]
}

impl RoomCode {
    pub fn generate() -> RoomCode {
        let mut secret = [0u8; SECRET_LEN];
        super::crypto::random(&mut secret);
        RoomCode { secret }
    }

    /// Reads a code as typed: any case, with or without the prefix, dashes or spaces, and
    /// O for 0 or I and L for 1. `None` for anything else, including a failed typo check.
    pub fn parse(text: &str) -> Option<RoomCode> {
        let chars: Vec<u8> = text.bytes().filter(|c| !matches!(c, b'-' | b' ' | b'_' | b'\t' | b'\r' | b'\n')).collect();
        // The "PP1" prefix is optional, and its 1 may be typed as l or I too. An earlier
        // prefix, "LB1", still reads.
        let chars = match chars.as_slice() {
            [l, b, one, rest @ ..]
                if rest.len() == DATA_CHARS + CHECK_CHARS
                    && matches!(
                        (l.to_ascii_uppercase(), b.to_ascii_uppercase()),
                        (b'P', b'P') | (b'L', b'B')
                    )
                    && matches!(one.to_ascii_uppercase(), b'1' | b'L' | b'I') =>
            {
                rest
            }
            all => all,
        };
        let mut vals = Vec::with_capacity(DATA_CHARS + CHECK_CHARS);
        for &c in chars {
            vals.push(decode_char(c)?);
        }
        if vals.len() != DATA_CHARS + CHECK_CHARS {
            return None;
        }
        let mut secret = [0u8; SECRET_LEN];
        let (mut acc, mut bits, mut i) = (0u32, 0u32, 0usize);
        for &v in &vals[..DATA_CHARS] {
            acc = (acc << 5) | v as u32;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                secret[i] = (acc >> bits) as u8;
                i += 1;
            }
        }
        let want = check(&secret);
        let got = [
            ALPHABET[vals[DATA_CHARS] as usize],
            ALPHABET[vals[DATA_CHARS + 1] as usize],
        ];
        (want == got).then_some(RoomCode { secret })
    }

    fn key(&self, label: &[u8]) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(b"peerparty/inet/v1"), &self.secret);
        let mut out = [0u8; 32];
        hk.expand(label, &mut out)
            .expect("32 bytes is a valid HKDF length");
        out
    }

    /// The pre-shared key the handshake is keyed with.
    pub fn psk(&self) -> [u8; 32] {
        self.key(b"psk")
    }

    /// The key a handshake's first packet is MACed with, so strangers get no reply.
    pub fn init_key(&self) -> [u8; 32] {
        self.key(b"init-mac")
    }

    /// A short public id in every handshake packet, so a socket can tell rooms apart.
    pub fn room_id(&self) -> [u8; 8] {
        let k = self.key(b"room-id");
        k[..8].try_into().unwrap()
    }

    /// For logs: identifies a room without giving away its code.
    pub fn fingerprint(&self) -> String {
        if self.is_public() {
            return "public".into();
        }
        self.room_id()[..4].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The everyone-is-welcome "room" of public sessions. Its secret is well known (a hash
    /// of a fixed string), so its keys keep nobody out: there, each player is
    /// authenticated by a signing key of their own instead (see `public`). A different
    /// room id, so copies that don't know public sessions just ignore its packets.
    pub fn public() -> RoomCode {
        let h = Sha256::digest(b"peerparty public sessions v1");
        RoomCode { secret: h[..SECRET_LEN].try_into().unwrap() }
    }

    pub fn is_public(&self) -> bool {
        let h = Sha256::digest(b"peerparty public sessions v1");
        self.secret[..] == h[..SECRET_LEN]
    }

    /// The DHT info-hash a room's copies of `game` meet on in hour `slot`. Only someone
    /// with the code can work it out, and it changes every hour.
    pub fn topic(&self, game: &str, slot: u64) -> [u8; 20] {
        let mut h = Sha1::new();
        h.update(b"peerparty/1/room/");
        h.update(self.key(b"topic"));
        h.update([0]);
        h.update(game.as_bytes());
        h.update([0]);
        h.update(slot.to_be_bytes());
        h.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_forgives_typing() {
        for _ in 0..50 {
            let c = RoomCode::generate();
            let s = c.to_string();
            assert!(s.starts_with("PP1-"), "{s}");
            assert_eq!(s.len(), 4 + 26 + 6, "{s}");
            assert_eq!(RoomCode::parse(&s), Some(c.clone()));
            let typed = s
                .to_ascii_lowercase()
                .replace('-', " ")
                .replace('0', "o")
                .replace('1', "l");
            assert_eq!(RoomCode::parse(&typed), Some(c.clone()), "{typed}");
            assert_eq!(RoomCode::parse(&s[4..].replace('-', "")), Some(c));
        }
    }

    #[test]
    fn rejects_typos_and_junk() {
        let c = RoomCode::generate();
        let s = c.to_string();
        let mut misses = 0;
        // Change each data character in turn: the check catches nearly all of them.
        for i in 4..s.len() - 3 {
            let b = s.as_bytes()[i];
            if b == b'-' {
                continue;
            }
            let mut t = s.clone().into_bytes();
            let p = ALPHABET.iter().position(|&a| a == b).unwrap();
            t[i] = ALPHABET[(p + 1) % 32];
            if RoomCode::parse(std::str::from_utf8(&t).unwrap()).is_some() {
                misses += 1;
            }
        }
        assert!(misses <= 1, "{misses} typos got through");
        assert!(RoomCode::parse(&s.replacen("PP1", "LB1", 1)).is_some(), "the earlier prefix");
        let short = &s[..s.len() - 1];
        for junk in ["", "PP1", "hello world", "PP1-UUUU-UUUU-UUUU-UUUU-UUUU-UUUU-UU", short] {
            assert_eq!(RoomCode::parse(junk), None, "{junk}");
        }
    }

    #[test]
    fn keys_differ_per_room_and_label() {
        let (a, b) = (RoomCode::generate(), RoomCode::generate());
        assert_ne!(a.psk(), b.psk());
        assert_ne!(a.psk(), a.init_key());
        assert_ne!(a.topic("g", 1), a.topic("g", 2));
        assert_ne!(a.topic("g", 1), a.topic("h", 1));
        assert_ne!(a.topic("g", 1), b.topic("g", 1));
        assert_eq!(a.topic("g", 1), a.clone().topic("g", 1));
        assert!(!format!("{a:?}").contains(&a.to_string()[4..8]));
    }
}