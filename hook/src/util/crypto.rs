//! Hash, cipher and randomness primitives, small textbook implementations checked
//! against the standards' test vectors below (PeerParty takes no crypto dependency):
//! SHA-256 and SHA-1 (FIPS 180-4), MD5 (RFC 1321), AES-128/256 (FIPS 197) with CBC and
//! GCM (NIST SP 800-38D), HMAC-SHA256 (RFC 2104), and the OS's secure random bytes.
//!
//! Shared by the stand-ins: Photon's payload encryption (photon/crypto.rs), Unity
//! Relay's DTLS (ugs/dtls.rs), Demonware's tickets (demonware/crypto.rs), the
//! WebSocket handshake (util/ws.rs) and storage checksums (eos/storage.rs,
//! galaxy/ext.rs). None of it needs to resist an attacker on the LAN.

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

    pub fn decrypt_block(&self, b: &mut [u8; 16]) {
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


// ---------------------------------------------------------------- SHA-1

// One copy for the hook and the core (the DHT's room topics).
pub use peerparty_core::sha1::sha1;

// ---------------------------------------------------------------- MD5

/// MD5, hex (the metadata's MD5Hash).
pub fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11,
        16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    for block in msg.chunks(64) {
        let m: Vec<u32> = block.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0].iter().flat_map(|v| v.to_le_bytes()).map(|b| format!("{b:02x}")).collect()
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
    fn sha1_matches_fips_180_vectors() {
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn md5_matches_rfc_1321() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex(&[b'a'; 1000]), "cabe45dcc9ae5b66ba86600cca6b8ba8");
    }
}
