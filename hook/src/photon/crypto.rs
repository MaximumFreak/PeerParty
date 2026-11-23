//! Photon's payload encryption: Diffie-Hellman over the 768-bit Oakley group 1 prime
//! (RFC 2409, generator 22) to agree a secret, SHA-256 of the secret (big-endian,
//! leading zeros trimmed) as an AES-256 key, and AES-256-CBC with a zero IV and PKCS#7
//! padding over each encrypted message's payload.
//!
//! The scheme is Luxon's `src/ser/ser_encryption.cpp` (BSD-3-Clause, see
//! LICENSE-Luxon). Luxon uses mbedTLS; PeerParty has no crypto dependency, so the
//! primitives are small textbook implementations: SHA-256 and AES are util/crypto.rs's
//! (shared with the other stand-ins), and the Montgomery modular exponentiation for the
//! key exchange is here, checked against independently computed values in the tests
//! below. None of this needs to resist an attacker on the LAN: it exists because Photon
//! clients insist on the handshake.

use crate::util::crypto::{random_bytes, sha256};

/// RFC 2409 section 6.1, the First Oakley Group (768 bits).
const OAKLEY768: [u8; 96] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC9, 0x0F, 0xDA, 0xA2, 0x21, 0x68, 0xC2, 0x34, 0xC4, 0xC6, 0x62, 0x8B,
    0x80, 0xDC, 0x1C, 0xD1, 0x29, 0x02, 0x4E, 0x08, 0x8A, 0x67, 0xCC, 0x74, 0x02, 0x0B, 0xBE, 0xA6, 0x3B, 0x13, 0x9B, 0x22,
    0x51, 0x4A, 0x08, 0x79, 0x8E, 0x34, 0x04, 0xDD, 0xEF, 0x95, 0x19, 0xB3, 0xCD, 0x3A, 0x43, 0x1B, 0x30, 0x2B, 0x0A, 0x6D,
    0xF2, 0x5F, 0x14, 0x37, 0x4F, 0xE1, 0x35, 0x6D, 0x6D, 0x51, 0xC2, 0x45, 0xE4, 0x85, 0xB5, 0x76, 0x62, 0x5E, 0x7E, 0xC6,
    0xF4, 0x4C, 0x42, 0xE9, 0xA6, 0x3A, 0x36, 0x20, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];
const GENERATOR: u32 = 22;

// ---------------------------------------------------------------- big integers

/// Little-endian 32-bit limbs.
type Big = Vec<u32>;

fn from_be(bytes: &[u8], limbs: usize) -> Big {
    let mut out = vec![0u32; limbs];
    for (i, b) in bytes.iter().rev().enumerate() {
        if i / 4 < limbs {
            out[i / 4] |= (*b as u32) << (8 * (i % 4));
        }
    }
    out
}

fn to_be_trimmed(x: &[u32]) -> Vec<u8> {
    let mut out: Vec<u8> = x.iter().rev().flat_map(|l| l.to_be_bytes()).collect();
    let first = out.iter().position(|b| *b != 0).unwrap_or(out.len());
    out.drain(..first);
    if out.is_empty() {
        out.push(0);
    }
    out
}

fn geq(a: &[u32], b: &[u32]) -> bool {
    for i in (0..a.len().max(b.len())).rev() {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    true
}

fn sub_in_place(a: &mut [u32], b: &[u32]) {
    let mut borrow = 0i64;
    for i in 0..a.len() {
        let v = a[i] as i64 - b.get(i).copied().unwrap_or(0) as i64 - borrow;
        if v < 0 {
            a[i] = (v + (1i64 << 32)) as u32;
            borrow = 1;
        } else {
            a[i] = v as u32;
            borrow = 0;
        }
    }
}

struct Mont {
    n: Big,
    ninv: u32,
    r2: Big,
}

impl Mont {
    fn new(n: Big) -> Self {
        // -n^-1 mod 2^32 by Newton's iteration.
        let mut inv: u32 = 1;
        for _ in 0..5 {
            inv = inv.wrapping_mul(2u32.wrapping_sub(n[0].wrapping_mul(inv)));
        }
        let ninv = inv.wrapping_neg();
        // R^2 mod n, R = 2^(32 * limbs), by doubling 1 that many times.
        let s = n.len();
        let mut x = vec![0u32; s + 1];
        x[0] = 1;
        for _ in 0..(64 * s) {
            let mut carry = 0;
            for l in x.iter_mut() {
                let v = (*l as u64) << 1 | carry;
                *l = v as u32;
                carry = v >> 32;
            }
            if geq(&x, &n) {
                sub_in_place(&mut x, &n);
            }
        }
        x.truncate(s);
        Mont { n, ninv, r2: x }
    }

    fn mul(&self, a: &[u32], b: &[u32]) -> Big {
        let s = self.n.len();
        let n = &self.n;
        let mut t = vec![0u32; s + 2];
        for i in 0..s {
            let mut c = 0u64;
            for j in 0..s {
                let x = t[j] as u64 + a[j] as u64 * b[i] as u64 + c;
                t[j] = x as u32;
                c = x >> 32;
            }
            let x = t[s] as u64 + c;
            t[s] = x as u32;
            t[s + 1] = (x >> 32) as u32;
            let m = t[0].wrapping_mul(self.ninv);
            let x = t[0] as u64 + m as u64 * n[0] as u64;
            let mut c = x >> 32;
            for j in 1..s {
                let x = t[j] as u64 + m as u64 * n[j] as u64 + c;
                t[j - 1] = x as u32;
                c = x >> 32;
            }
            let x = t[s] as u64 + c;
            t[s - 1] = x as u32;
            t[s] = t[s + 1] + (x >> 32) as u32;
            t[s + 1] = 0;
        }
        if geq(&t, n) {
            sub_in_place(&mut t, n);
        }
        t.truncate(s);
        t
    }

    /// base^exp mod n; `base` must be below n.
    fn pow(&self, base: &[u32], exp: &[u8]) -> Big {
        let s = self.n.len();
        let mut one = vec![0u32; s];
        one[0] = 1;
        let bm = self.mul(base, &self.r2);
        let mut acc = self.mul(&one, &self.r2);
        for byte in exp {
            for bit in (0..8).rev() {
                acc = self.mul(&acc, &acc);
                if byte >> bit & 1 == 1 {
                    acc = self.mul(&acc, &bm);
                }
            }
        }
        self.mul(&acc, &one)
    }
}

fn group() -> &'static Mont {
    use std::sync::OnceLock;
    static G: OnceLock<Mont> = OnceLock::new();
    G.get_or_init(|| Mont::new(from_be(&OAKLEY768, 24)))
}

/// One side of the key exchange.
pub struct Dh {
    secret: Vec<u8>,
    pub public: Vec<u8>,
}

impl Dh {
    pub fn new() -> Self {
        // 160 random bits, as the Photon client picks its own.
        let mut secret = random_bytes(20);
        secret[0] |= 0x80;
        Self::with_secret(secret)
    }

    pub fn with_secret(secret: Vec<u8>) -> Self {
        let g = group();
        let mut base = vec![0u32; 24];
        base[0] = GENERATOR;
        let public = to_be_trimmed(&g.pow(&base, &secret));
        Dh { secret, public }
    }

    /// The AES key both sides end up with: SHA-256 of the shared secret's big-endian
    /// bytes with leading zeros trimmed (Luxon `DeriveFromPeerPublicKey`).
    pub fn derive(&self, peer_public: &[u8]) -> Option<[u8; 32]> {
        let g = group();
        if peer_public.is_empty() || peer_public.len() > 96 {
            return None;
        }
        let y = from_be(peer_public, 24);
        // 1 < y < p - 1, or the "shared secret" is predictable.
        let mut p_minus_1 = g.n.clone();
        p_minus_1[0] -= 1;
        let mut two = vec![0u32; 24];
        two[0] = 2;
        if geq(&y, &p_minus_1) || !geq(&y, &two) {
            return None;
        }
        let shared = g.pow(&y, &self.secret);
        Some(sha256(&to_be_trimmed(&shared)))
    }
}

impl Default for Dh {
    fn default() -> Self {
        Self::new()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn modpow_matches_known_values() {
        // 22^5 mod p is just 22^5.
        let d = Dh::with_secret(vec![5]);
        assert_eq!(d.public, to_be_trimmed(&from_be(&(22u64.pow(5)).to_be_bytes(), 3)));
        // 22^(2^200) mod p, computed independently with Python's pow().
        let mut e = vec![1u8];
        e.extend(std::iter::repeat_n(0u8, 25));
        let d = Dh::with_secret(e);
        assert_eq!(hex(&d.public), "e467249761b66261f2df461ed0a39411f92b8ee41322811b2d60ce9460a5f14d039993d278eada5fb20f5acc6b05bef7d598245563bcc07c5ebfd3bacfba87404e2c60532ea16d61c6c55bd6f50bf795c28c551d0789f920760616b0cdc456db");
    }

    #[test]
    fn both_sides_derive_the_same_key() {
        let a = Dh::new();
        let b = Dh::new();
        assert_ne!(a.public, b.public);
        let ka = a.derive(&b.public).unwrap();
        let kb = b.derive(&a.public).unwrap();
        assert_eq!(ka, kb);
        assert!(a.derive(&[1]).is_none());
        assert!(a.derive(&OAKLEY768).is_none());
    }
}
