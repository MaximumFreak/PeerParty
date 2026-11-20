//! The lobby service's crypto: HMAC-SHA1 (RFC 2104) over ugs/wire.rs's SHA-1, the key
//! expansion Demonware's lobby handshake uses, and AES-128-CBC with an explicit IV and
//! no padding (the sender pads to 16 bytes itself), over photon/crypto.rs's AES.

use crate::photon::crypto::Aes;
use crate::ugs::wire::sha1;

pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + data.len());
    inner.extend(k.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(data);
    let ih = sha1(&inner);
    let mut outer = Vec::with_capacity(84);
    outer.extend(k.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&ih);
    sha1(&outer)
}

/// `len` bytes expanded from `secret` and `label`: HMAC-SHA1(secret, label || 1), then
/// HMAC-SHA1(secret, previous || label || n) for n = 2, 3, ...
pub fn expand(secret: &[u8], label: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 20);
    let mut prev: Vec<u8> = Vec::new();
    let mut n = 1u8;
    while out.len() < len {
        let mut msg = prev.clone();
        msg.extend_from_slice(label);
        msg.push(n);
        let h = hmac_sha1(secret, &msg);
        out.extend_from_slice(&h);
        prev = h.to_vec();
        n = n.wrapping_add(1);
    }
    out.truncate(len);
    out
}

/// The keys both ends derive once the handshake is done.
#[derive(Clone, Debug, PartialEq)]
pub struct Keys {
    /// What the server sends back to prove it has the session key.
    pub response: [u8; 8],
    /// Signs each encrypted message (the first 8 bytes of HMAC-SHA1).
    pub hmac: [u8; 20],
    /// Decrypts what the client sends.
    pub client_to_server: [u8; 16],
    /// Encrypts what the server sends.
    pub server_to_client: [u8; 16],
}

/// Keys from the session key and every handshake byte so far (`transcript`).
pub fn derive(session_key: &[u8], transcript: &[u8]) -> Keys {
    let digest = sha1(transcript);
    let secret = hmac_sha1(&digest, session_key);
    let chal = expand(&secret, b"CLIENTCHAL", 16);
    let data = expand(&secret, b"BDDATA", 72);
    let mut k = Keys { response: [0; 8], hmac: [0; 20], client_to_server: [0; 16], server_to_client: [0; 16] };
    k.response.copy_from_slice(&chal[8..16]);
    k.hmac.copy_from_slice(&data[20..40]);
    k.client_to_server.copy_from_slice(&data[40..56]);
    k.server_to_client.copy_from_slice(&data[56..72]);
    k
}

/// AES-128-CBC, `data` a whole number of blocks.
pub fn cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let aes = Aes::new(key);
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut b = [0u8; 16];
        b[..chunk.len()].copy_from_slice(chunk);
        for (x, p) in b.iter_mut().zip(prev) {
            *x ^= p;
        }
        aes.encrypt_block(&mut b);
        out.extend_from_slice(&b);
        prev = b;
    }
    out
}

/// AES-128-CBC decryption; None unless `data` is a whole number of blocks.
pub fn cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() % 16 != 0 {
        return None;
    }
    let aes = Aes::new(key);
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let c: [u8; 16] = chunk.try_into().ok()?;
        let mut b = c;
        aes.decrypt_block(&mut b);
        for (x, p) in b.iter_mut().zip(prev) {
            *x ^= p;
        }
        out.extend_from_slice(&b);
        prev = c;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn hmac_sha1_matches_rfc_2202() {
        assert_eq!(hmac_sha1(&[0x0b; 20], b"Hi There").to_vec(), unhex("b617318655057264e28bc0b6fb378c8ef146be00"));
        assert_eq!(
            hmac_sha1(b"Jefe", b"what do ya want for nothing?").to_vec(),
            unhex("effcdf6ae5eb2fa2d27416d5f184df9c259a7c79")
        );
        // A key longer than a block is hashed first.
        assert_eq!(
            hmac_sha1(&[0xaa; 80], b"Test Using Larger Than Block-Size Key - Hash Key First").to_vec(),
            unhex("aa4ae5e15272d00e95705637ce8a3b55ed402112")
        );
    }

    #[test]
    fn aes_128_cbc_matches_sp_800_38a() {
        let key: [u8; 16] = unhex("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
        let iv: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let plain = unhex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        let cipher = unhex("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2");
        assert_eq!(cbc_encrypt(&key, &iv, &plain), cipher);
        assert_eq!(cbc_decrypt(&key, &iv, &cipher).unwrap(), plain);
        assert!(cbc_decrypt(&key, &iv, &cipher[..20]).is_none());
    }

    #[test]
    fn expansion_chains_each_block_on_the_last() {
        let secret = [7u8; 20];
        let out = expand(&secret, b"BDDATA", 72);
        assert_eq!(out.len(), 72);
        assert_eq!(out[..20], hmac_sha1(&secret, b"BDDATA\x01"));
        let mut second = out[..20].to_vec();
        second.extend_from_slice(b"BDDATA\x02");
        assert_eq!(out[20..40], hmac_sha1(&secret, &second));
        // The keys come from fixed places in the expansion.
        let k = derive(&[0x13; 24], b"transcript");
        let secret = hmac_sha1(&sha1(b"transcript"), &[0x13; 24]);
        assert_eq!(k.response[..], expand(&secret, b"CLIENTCHAL", 16)[8..16]);
        assert_eq!(k.server_to_client[..], expand(&secret, b"BDDATA", 72)[56..72]);
    }
}
