//! Photon's payload encryption: Diffie-Hellman over the 768-bit Oakley group 1 prime
//! (RFC 2409, generator 22) to agree a secret, SHA-256 of the secret (big-endian,
//! leading zeros trimmed) as an AES-256 key, and AES-256-CBC with a zero IV and PKCS#7
//! padding over each encrypted message's payload.
//!
//! The scheme is Luxon's `src/ser/ser_encryption.cpp` (BSD-3-Clause, see
//! LICENSE-Luxon). Luxon
//! uses mbedTLS; PeerParty has no crypto dependency, so the three primitives are small
//! textbook implementations here (FIPS 180-4 SHA-256, FIPS 197 AES, Montgomery modular
//! exponentiation), checked against the standards' test vectors in the tests below.
//! None of this needs to resist an attacker on the LAN: it exists because Photon
//! clients insist on the handshake.
//!
//! Unity Relay's DTLS (ugs/dtls.rs) uses the same SHA-256 and AES (with 128-bit keys)
//! plus HMAC-SHA256 (RFC 2104) and AES-GCM (NIST SP 800-38D), also here.

/// RFC 2409 section 6.1, the First Oakley Group (768 bits).
const OAKLEY768: [u8; 96] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC9, 0x0F, 0xDA, 0xA2, 0x21, 0x68, 0xC2, 0x34, 0xC4, 0xC6, 0x62, 0x8B,
    0x80, 0xDC, 0x1C, 0xD1, 0x29, 0x02, 0x4E, 0x08, 0x8A, 0x67, 0xCC, 0x74, 0x02, 0x0B, 0xBE, 0xA6, 0x3B, 0x13, 0x9B, 0x22,
    0x51, 0x4A, 0x08, 0x79, 0x8E, 0x34, 0x04, 0xDD, 0xEF, 0x95, 0x19, 0xB3, 0xCD, 0x3A, 0x43, 0x1B, 0x30, 0x2B, 0x0A, 0x6D,
    0xF2, 0x5F, 0x14, 0x37, 0x4F, 0xE1, 0x35, 0x6D, 0x6D, 0x51, 0xC2, 0x45, 0xE4, 0x85, 0xB5, 0x76, 0x62, 0x5E, 0x7E, 0xC6,
    0xF4, 0x4C, 0x42, 0xE9, 0xA6, 0x3A, 0x36, 0x20, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];
const GENERATOR: u32 = 22;

// ---------------------------------------------------------------- randomness

/// Random bytes from the OS's secure random source.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    peerparty_core::inet::crypto::random(&mut out);
    out
}

// ---------------------------------------------------------------- SHA-256

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01,
    0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
    0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
    0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08,
    0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K256[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------- AES-256

fn sbox() -> &'static ([u8; 256], [u8; 256]) {
    use std::sync::OnceLock;
    static T: OnceLock<([u8; 256], [u8; 256])> = OnceLock::new();
    T.get_or_init(|| {
        // Build the S-box from the multiplicative inverse in GF(2^8) and the affine map.
        let mut s = [0u8; 256];
        let mut inv = [0u8; 256];
        let (mut p, mut q) = (1u8, 1u8);
        loop {
            // p walks the field by multiplying by 3, q by dividing by 3.
            p = p ^ (p << 1) ^ if p & 0x80 != 0 { 0x1B } else { 0 };
            q ^= q << 1;
            q ^= q << 2;
            q ^= q << 4;
            if q & 0x80 != 0 {
                q ^= 0x09;
            }
            let x = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4);
            s[p as usize] = x ^ 0x63;
            if p == 1 {
                break;
            }
        }
        s[0] = 0x63;
        for (i, v) in s.iter().enumerate() {
            inv[*v as usize] = i as u8;
        }
        (s, inv)
    })
}

fn xtime(x: u8) -> u8 {
    (x << 1) ^ if x & 0x80 != 0 { 0x1B } else { 0 }
}

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

/// AES with a 128-, 192- or 256-bit key.
pub struct Aes {
    rk: Vec<[u8; 16]>,
}

pub type Aes256 = Aes;

impl Aes {
    /// `key` is 16, 24 or 32 bytes.
    pub fn new(key: &[u8]) -> Self {
        assert!(matches!(key.len(), 16 | 24 | 32), "AES key length");
        let (s, _) = sbox();
        let nk = key.len() / 4;
        let rounds = nk + 6;
        let words = 4 * (rounds + 1);
        let mut w = vec![[0u8; 4]; words];
        for i in 0..nk {
            w[i].copy_from_slice(&key[i * 4..i * 4 + 4]);
        }
        let mut rcon = 1u8;
        for i in nk..words {
            let mut t = w[i - 1];
            if i % nk == 0 {
                t = [s[t[1] as usize] ^ rcon, s[t[2] as usize], s[t[3] as usize], s[t[0] as usize]];
                rcon = xtime(rcon);
            } else if nk > 6 && i % nk == 4 {
                t = t.map(|b| s[b as usize]);
            }
            for j in 0..4 {
                w[i][j] = w[i - nk][j] ^ t[j];
            }
        }
        let mut rk = vec![[0u8; 16]; rounds + 1];
        for (r, k) in rk.iter_mut().enumerate() {
            for c in 0..4 {
                k[c * 4..c * 4 + 4].copy_from_slice(&w[r * 4 + c]);
            }
        }
        Aes { rk }
    }

    pub fn encrypt_block(&self, b: &mut [u8; 16]) {
        let (s, _) = sbox();
        let last = self.rk.len() - 1;
        xor16(b, &self.rk[0]);
        for round in 1..=last {
            for x in b.iter_mut() {
                *x = s[*x as usize];
            }
            shift_rows(b);
            if round != last {
                for c in 0..4 {
                    let col = [b[c * 4], b[c * 4 + 1], b[c * 4 + 2], b[c * 4 + 3]];
                    let all = col[0] ^ col[1] ^ col[2] ^ col[3];
                    for r in 0..4 {
                        b[c * 4 + r] = col[r] ^ all ^ xtime(col[r] ^ col[(r + 1) % 4]);
                    }
                }
            }
            xor16(b, &self.rk[round]);
        }
    }

    fn decrypt_block(&self, b: &mut [u8; 16]) {
        let (_, inv) = sbox();
        let last = self.rk.len() - 1;
        xor16(b, &self.rk[last]);
        for round in (0..last).rev() {
            inv_shift_rows(b);
            for x in b.iter_mut() {
                *x = inv[*x as usize];
            }
            xor16(b, &self.rk[round]);
            if round != 0 {
                for c in 0..4 {
                    let col = [b[c * 4], b[c * 4 + 1], b[c * 4 + 2], b[c * 4 + 3]];
                    for r in 0..4 {
                        b[c * 4 + r] = gmul(col[r], 14)
                            ^ gmul(col[(r + 1) % 4], 11)
                            ^ gmul(col[(r + 2) % 4], 13)
                            ^ gmul(col[(r + 3) % 4], 9);
                    }
                }
            }
        }
    }

    /// AES-256-CBC, zero IV, PKCS#7 padding: Photon's payload encryption.
    pub fn encrypt_cbc(&self, data: &[u8]) -> Vec<u8> {
        let pad = 16 - data.len() % 16;
        let mut buf = data.to_vec();
        buf.extend(std::iter::repeat_n(pad as u8, pad));
        let mut prev = [0u8; 16];
        for chunk in buf.chunks_mut(16) {
            let mut b: [u8; 16] = chunk.try_into().unwrap();
            xor16(&mut b, &prev);
            self.encrypt_block(&mut b);
            chunk.copy_from_slice(&b);
            prev = b;
        }
        buf
    }

    pub fn decrypt_cbc(&self, data: &[u8]) -> Option<Vec<u8>> {
        if data.is_empty() || data.len() % 16 != 0 {
            return None;
        }
        let mut out = Vec::with_capacity(data.len());
        let mut prev = [0u8; 16];
        for chunk in data.chunks(16) {
            let c: [u8; 16] = chunk.try_into().unwrap();
            let mut b = c;
            self.decrypt_block(&mut b);
            xor16(&mut b, &prev);
            out.extend_from_slice(&b);
            prev = c;
        }
        let pad = *out.last()? as usize;
        if pad == 0 || pad > 16 || out[out.len() - pad..].iter().any(|b| *b as usize != pad) {
            return None;
        }
        out.truncate(out.len() - pad);
        Some(out)
    }

    /// AES-GCM with a 96-bit nonce: the ciphertext followed by the 16-byte tag.
    pub fn gcm_seal(&self, nonce: &[u8; 12], aad: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        self.gcm_ctr(nonce, &mut out);
        let tag = self.gcm_tag(nonce, aad, &out);
        out.extend_from_slice(&tag);
        out
    }

    /// Opens what `gcm_seal` made; None if the tag doesn't match.
    pub fn gcm_open(&self, nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        let n = sealed.len().checked_sub(16)?;
        let (ct, tag) = sealed.split_at(n);
        let want = self.gcm_tag(nonce, aad, ct);
        // Constant time isn't needed on the LAN, but it costs nothing.
        if want.iter().zip(tag).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
            return None;
        }
        let mut out = ct.to_vec();
        self.gcm_ctr(nonce, &mut out);
        Some(out)
    }

    /// Counter mode from inc32(J0).
    fn gcm_ctr(&self, nonce: &[u8; 12], data: &mut [u8]) {
        for (i, chunk) in data.chunks_mut(16).enumerate() {
            let mut ks = [0u8; 16];
            ks[..12].copy_from_slice(nonce);
            ks[12..].copy_from_slice(&(i as u32 + 2).to_be_bytes());
            self.encrypt_block(&mut ks);
            for (d, k) in chunk.iter_mut().zip(ks) {
                *d ^= k;
            }
        }
    }

    fn gcm_tag(&self, nonce: &[u8; 12], aad: &[u8], ct: &[u8]) -> [u8; 16] {
        let mut h = [0u8; 16];
        self.encrypt_block(&mut h);
        let h = u128::from_be_bytes(h);
        let mut y = 0u128;
        for part in [aad, ct] {
            for chunk in part.chunks(16) {
                let mut b = [0u8; 16];
                b[..chunk.len()].copy_from_slice(chunk);
                y = gf128_mul(y ^ u128::from_be_bytes(b), h);
            }
        }
        let lens = ((aad.len() as u128 * 8) << 64) | (ct.len() as u128 * 8);
        y = gf128_mul(y ^ lens, h);
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(nonce);
        j0[15] = 1;
        self.encrypt_block(&mut j0);
        (y ^ u128::from_be_bytes(j0)).to_be_bytes()
    }
}

/// Multiplication in GCM's GF(2^128) (SP 800-38D, algorithm 1; bit 0 is the MSB).
fn gf128_mul(x: u128, y: u128) -> u128 {
    let mut z = 0u128;
    let mut v = y;
    for i in 0..128 {
        if (x >> (127 - i)) & 1 == 1 {
            z ^= v;
        }
        let lsb = v & 1;
        v >>= 1;
        if lsb == 1 {
            v ^= 0xE1 << 120;
        }
    }
    z
}

// ---------------------------------------------------------------- HMAC-SHA256

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    inner.extend_from_slice(data);
    let mut outer: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    outer.extend_from_slice(&sha256(&inner));
    sha256(&outer)
}

fn xor16(a: &mut [u8; 16], b: &[u8; 16]) {
    for i in 0..16 {
        a[i] ^= b[i];
    }
}

// State is column-major: byte (row r, column c) is b[c * 4 + r].
fn shift_rows(b: &mut [u8; 16]) {
    let t = *b;
    for c in 0..4 {
        for r in 0..4 {
            b[c * 4 + r] = t[((c + r) % 4) * 4 + r];
        }
    }
}

fn inv_shift_rows(b: &mut [u8; 16]) {
    let t = *b;
    for c in 0..4 {
        for r in 0..4 {
            b[((c + r) % 4) * 4 + r] = t[c * 4 + r];
        }
    }
}

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

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn sha256_matches_fips_180_vectors() {
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn aes256_matches_fips_197_vector() {
        let key: [u8; 32] = unhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f").try_into().unwrap();
        let aes = Aes256::new(&key);
        let mut b: [u8; 16] = unhex("00112233445566778899aabbccddeeff").try_into().unwrap();
        aes.encrypt_block(&mut b);
        assert_eq!(hex(&b), "8ea2b7ca516745bfeafc49904b496089");
        aes.decrypt_block(&mut b);
        assert_eq!(hex(&b), "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn aes128_matches_fips_197_vector() {
        let aes = Aes::new(&unhex("000102030405060708090a0b0c0d0e0f"));
        let mut b: [u8; 16] = unhex("00112233445566778899aabbccddeeff").try_into().unwrap();
        aes.encrypt_block(&mut b);
        assert_eq!(hex(&b), "69c4e0d86a7b0430d8cdb78070b4c55a");
        aes.decrypt_block(&mut b);
        assert_eq!(hex(&b), "00112233445566778899aabbccddeeff");
        // FIPS 197 C.2 (AES-192) too, for the key schedule's middle case.
        let mut b: [u8; 16] = unhex("00112233445566778899aabbccddeeff").try_into().unwrap();
        Aes::new(&unhex("000102030405060708090a0b0c0d0e0f1011121314151617")).encrypt_block(&mut b);
        assert_eq!(hex(&b), "dda97ca4864cdfe06eaf70a0ec0d7191");
    }

    #[test]
    fn gcm_matches_the_gcm_specs_test_cases() {
        // McGrew & Viega, "The Galois/Counter Mode of Operation", test cases 1, 2 and 4.
        let zero = Aes::new(&[0u8; 16]);
        assert_eq!(hex(&zero.gcm_seal(&[0; 12], &[], &[])), "58e2fccefa7e3061367f1d57a4e7455a");
        assert_eq!(
            hex(&zero.gcm_seal(&[0; 12], &[], &[0; 16])),
            "0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf"
        );
        let aes = Aes::new(&unhex("feffe9928665731c6d6a8f9467308308"));
        let iv: [u8; 12] = unhex("cafebabefacedbaddecaf888").try_into().unwrap();
        let pt = unhex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39",
        );
        let aad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let sealed = aes.gcm_seal(&iv, &aad, &pt);
        assert_eq!(
            hex(&sealed),
            "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e0915bc94fbc3221a5db94fae95ae7121a47"
        );
        assert_eq!(aes.gcm_open(&iv, &aad, &sealed).unwrap(), pt);
        let mut bad = sealed.clone();
        bad[3] ^= 1;
        assert!(aes.gcm_open(&iv, &aad, &bad).is_none());
        assert!(aes.gcm_open(&iv, b"other", &sealed).is_none());
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block is hashed first.
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn aes256_cbc_matches_sp800_38a_vector() {
        // NIST SP 800-38A F.2.5 (CBC-AES256), with the IV folded into the first block
        // since Photon's IV is zero.
        let key: [u8; 32] = unhex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4").try_into().unwrap();
        let iv = unhex("000102030405060708090a0b0c0d0e0f");
        let mut pt = unhex("6bc1bee22e409f96e93d7e117393172a");
        for i in 0..16 {
            pt[i] ^= iv[i];
        }
        let ct = Aes256::new(&key).encrypt_cbc(&pt);
        assert_eq!(hex(&ct[..16]), "f58c4c04d6e5f1ba779eabfb5f7bfbd6");
        assert_eq!(Aes256::new(&key).decrypt_cbc(&ct).unwrap(), pt);
    }

    #[test]
    fn cbc_round_trips_every_length_and_rejects_bad_padding() {
        let key = [7u8; 32];
        let aes = Aes256::new(&key);
        for n in 0..70 {
            let data: Vec<u8> = (0..n as u8).collect();
            let ct = aes.encrypt_cbc(&data);
            assert_eq!(ct.len(), (n / 16 + 1) * 16);
            assert_eq!(aes.decrypt_cbc(&ct).unwrap(), data);
        }
        assert!(aes.decrypt_cbc(&[0u8; 15]).is_none());
        assert!(Aes256::new(&[8u8; 32]).decrypt_cbc(&aes.encrypt_cbc(b"hello")).is_none_or(|v| v != b"hello"));
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
