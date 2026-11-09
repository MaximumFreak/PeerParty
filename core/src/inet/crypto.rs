//! The tunnel's cryptography: an X25519 key exchange keyed with the room's pre-shared key
//! (in the spirit of Noise's NNpsk0), ChaCha20-Poly1305 for every packet after it, and a
//! replay window.
//!
//! Each side of a session picks a fresh ephemeral key pair and a 16-byte nonce. The
//! session keys are HKDF-SHA256 over the X25519 result, salted with the PSK and bound to
//! both sides' public keys and nonces, one key per direction. Without the room code
//! nobody can finish the handshake, read a packet or forge one; a fresh pair per session
//! means a later leak of the room code doesn't open recorded sessions without also
//! breaking X25519.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

/// Fills `buf` from the OS's secure random source.
pub fn random(buf: &mut [u8]) {
    getrandom::getrandom(buf).expect("the OS random source failed");
}

pub fn random_array<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    random(&mut b);
    b
}

/// HMAC-SHA256 over `parts`, cut to 16 bytes.
pub fn mac16(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 16] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("any key length works");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes()[..16].try_into().unwrap()
}

/// Checks a `mac16` in constant time.
pub fn mac16_ok(key: &[u8; 32], parts: &[&[u8]], tag: &[u8]) -> bool {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("any key length works");
    for p in parts {
        m.update(p);
    }
    tag.len() == 16 && m.verify_truncated_left(tag).is_ok()
}

/// One side's half of a handshake: an ephemeral key pair and a nonce.
pub struct Ephemeral {
    secret: StaticSecret,
    /// The public key followed by the nonce: what the other side sees of us.
    pub blob: [u8; 48],
}

impl Ephemeral {
    pub fn new() -> Ephemeral {
        let secret = StaticSecret::from(random_array::<32>());
        let mut blob = [0u8; 48];
        blob[..32].copy_from_slice(PublicKey::from(&secret).as_bytes());
        random(&mut blob[32..]);
        Ephemeral { secret, blob }
    }
}

impl Default for Ephemeral {
    fn default() -> Self {
        Ephemeral::new()
    }
}

/// A session's keys: one to seal what we send, one to open what we get.
pub struct Keys {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
}

/// A nonce counter no data packet reaches: the handshake's confirmation uses it.
pub const CONFIRM_COUNTER: u64 = u64::MAX;

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(n)
}

impl Keys {
    /// The session keys between `mine` and the peer's `blob`, or `None` for a key that
    /// isn't a real one (a low-order point) or our own blob reflected back.
    pub fn derive(psk: &[u8; 32], mine: &Ephemeral, peer: &[u8; 48]) -> Option<Keys> {
        if mine.blob == *peer {
            return None;
        }
        let peer_pub: [u8; 32] = peer[..32].try_into().ok()?;
        let dh = mine.secret.diffie_hellman(&PublicKey::from(peer_pub));
        if !dh.was_contributory() {
            return None;
        }
        let (lo, hi) = if mine.blob < *peer { (&mine.blob, peer) } else { (peer, &mine.blob) };
        // The HKDF state holds the PRK and doesn't wipe it on drop: it's wiped here (it
        // is plain arrays, so all zeros is still a valid value, and it is never used or
        // dropped again).
        let mut hk = std::mem::ManuallyDrop::new(Hkdf::<Sha256>::new(Some(psk), dh.as_bytes()));
        let mut okm = [0u8; 64];
        let expanded = hk.expand_multi_info(&[b"peerparty inet v1 keys", lo, hi], &mut okm);
        wipe(&mut *hk);
        if expanded.is_err() {
            okm.zeroize();
            return None;
        }
        let a = ChaCha20Poly1305::new(Key::from_slice(&okm[..32]));
        let b = ChaCha20Poly1305::new(Key::from_slice(&okm[32..]));
        // The ciphers hold (and on drop wipe) their own copies; `dh` wipes itself too.
        okm.zeroize();
        Some(if mine.blob < *peer {
            Keys { send: a, recv: b }
        } else {
            Keys { send: b, recv: a }
        })
    }

    pub fn seal(&self, counter: u64, ad: &[u8], plain: &[u8]) -> Vec<u8> {
        self.send
            .encrypt(&nonce(counter), Payload { msg: plain, aad: ad })
            .expect("ChaCha20-Poly1305 sealing can't fail")
    }

    pub fn open(&self, counter: u64, ad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        self.recv
            .decrypt(&nonce(counter), Payload { msg: sealed, aad: ad })
            .ok()
    }
}

/// Overwrites `v`'s bytes with zeros. Only for values with no heap parts or Drop of
/// their own, that are never read again.
fn wipe<T>(v: &mut T) {
    let bytes = unsafe { std::slice::from_raw_parts_mut((v as *mut T).cast::<u8>(), std::mem::size_of::<T>()) };
    bytes.zeroize();
}

/// Which packet counters we've seen: the newest and a window of 128 below it.
#[derive(Default)]
pub struct Replay {
    newest: Option<u64>,
    seen: u128,
}

impl Replay {
    /// Whether `c` is new. Doesn't record it: call `commit` once the packet opens.
    pub fn fresh(&self, c: u64) -> bool {
        match self.newest {
            None => true,
            Some(n) if c > n => true,
            Some(n) => {
                let back = n - c;
                back < 128 && self.seen & (1u128 << back) == 0
            }
        }
    }

    pub fn commit(&mut self, c: u64) {
        match self.newest {
            None => {
                self.newest = Some(c);
                self.seen = 1;
            }
            Some(n) if c > n => {
                let shift = c - n;
                self.seen = if shift >= 128 { 0 } else { self.seen << shift };
                self.seen |= 1;
                self.newest = Some(c);
            }
            Some(n) => {
                let back = n - c;
                if back < 128 {
                    self.seen |= 1u128 << back;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_agree_and_strangers_cannot() {
        let psk = random_array::<32>();
        let (a, b) = (Ephemeral::new(), Ephemeral::new());
        let ka = Keys::derive(&psk, &a, &b.blob).unwrap();
        let kb = Keys::derive(&psk, &b, &a.blob).unwrap();
        let sealed = ka.seal(7, b"ad", b"hello");
        assert_eq!(kb.open(7, b"ad", &sealed).as_deref(), Some(&b"hello"[..]));
        assert!(kb.open(8, b"ad", &sealed).is_none(), "wrong counter");
        assert!(kb.open(7, b"AD", &sealed).is_none(), "wrong ad");
        assert!(ka.open(7, b"ad", &sealed).is_none(), "keys are per direction");
        let mut bent = sealed.clone();
        bent[0] ^= 1;
        assert!(kb.open(7, b"ad", &bent).is_none(), "tampered");
        // Someone without the PSK gets other keys from the same exchange.
        let kx = Keys::derive(&random_array::<32>(), &b, &a.blob).unwrap();
        assert!(kx.open(7, b"ad", &sealed).is_none());
        // A reflected blob or a zero key is refused.
        assert!(Keys::derive(&psk, &a, &a.blob).is_none());
        assert!(Keys::derive(&psk, &a, &[0u8; 48]).is_none());
    }

    #[test]
    fn macs() {
        let k = random_array::<32>();
        let t = mac16(&k, &[b"a", b"bc"]);
        assert!(mac16_ok(&k, &[b"ab", b"c"], &t));
        assert!(!mac16_ok(&k, &[b"abd"], &t));
        assert!(!mac16_ok(&random_array::<32>(), &[b"abc"], &t));
        assert!(!mac16_ok(&k, &[b"abc"], &t[..15]));
    }

    #[test]
    fn replay_window() {
        let mut r = Replay::default();
        for c in [5u64, 3, 9, 200, 100] {
            assert!(r.fresh(c), "{c}");
            r.commit(c);
            assert!(!r.fresh(c), "{c} twice");
        }
        assert!(!r.fresh(9), "below the window");
        assert!(r.fresh(150));
        assert!(r.fresh(201));
    }
}