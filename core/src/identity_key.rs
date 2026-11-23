//! A player's long-term signing key (Ed25519), the key its `PeerId` is the hash of, and
//! the proofs made with it: the mesh hello's, a relayed connection's, a sign-in ticket's,
//! and generic application signatures (`sign_bytes`). Crate-level so identity, the mesh
//! and the engine use it without going through the internet layer.

use std::fmt;
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::id::PeerId;
use crate::{hex, unhex};

pub const KEY_LEN: usize = 32;
pub const SIG_LEN: usize = 64;
/// What a copy signs to prove its key to the copy it shakes hands with on the mesh.
const HELLO_CONTEXT: &[u8] = b"peerparty mesh hello v3";
const TICKET_CONTEXT: &[u8] = b"peerparty auth ticket v1";
/// What a copy signs to set up a connection through another player (`lan::relay`).
const RELAY_CONTEXT: &[u8] = b"peerparty relay v1\0";
/// What `sign_bytes` puts before a caller's context and message: nothing else PeerParty
/// signs starts with it, so an application signature is never a hello's, a relay's or a
/// ticket's proof.
const APP_CONTEXT: &[u8] = b"peerparty app v1\0";

/// A player's long-term signing key.
pub struct IdentityKey {
    key: SigningKey,
}

impl fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IdentityKey({})", self.fingerprint())
    }
}

impl IdentityKey {
    pub fn generate() -> IdentityKey {
        IdentityKey::from_seed(crate::crypto::random_array())
    }

    pub fn from_seed(seed: [u8; 32]) -> IdentityKey {
        IdentityKey { key: SigningKey::from_bytes(&seed) }
    }

    /// The key in a key file's text (its 32-byte seed in hex), as `load_or_create`
    /// reads it.
    pub fn from_text(text: &str) -> Option<IdentityKey> {
        unhex::<32>(text).map(IdentityKey::from_seed)
    }

    /// Loads the key at `path`, or makes one and saves it there. A key that can't be
    /// saved still works for this run.
    ///
    /// A player's id is this key's hash, and games keep saves under ids made from it, so
    /// a key is never written over while it might still be read: a file that is there but
    /// can't be read now (another program has it open) gets a key for this run only. One
    /// that reads as something else is kept beside it as `.bad`. A new key is written
    /// whole or not at all.
    pub fn load_or_create(path: &Path) -> IdentityKey {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                if let Some(k) = IdentityKey::from_text(&s) {
                    return k;
                }
                let _ = std::fs::rename(path, path.with_extension("bad"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return IdentityKey::generate(),
        }
        let k = IdentityKey::generate();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension(format!("new-{}", std::process::id()));
        if std::fs::write(&tmp, hex(k.key.as_bytes())).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        k
    }

    /// Proof for the mesh's hello that the signer holds this key: its signature over
    /// the game, and over both sides' ids and the numbers each made up for this one
    /// connection (so it is no use on another connection, or to anyone but `to`).
    pub fn sign_hello(&self, game: &str, mine: (PeerId, &[u8; 16]), to: (PeerId, &[u8; 16])) -> [u8; SIG_LEN] {
        self.sign(&hello_transcript(game, mine, to))
    }

    pub fn public(&self) -> [u8; KEY_LEN] {
        self.key.verifying_key().to_bytes()
    }

    /// A game's sign-in ticket's proof (`Lan::auth_ticket`): this key's signature over
    /// the game and a number made up for this ticket.
    pub fn sign_ticket(&self, game: &str, nonce: &[u8; 16]) -> [u8; SIG_LEN] {
        self.sign(&ticket_transcript(game, nonce))
    }

    /// A relayed connection's offer or answer (`lan::relay`): this key's signature over
    /// `t`, under a context nothing else is signed with.
    pub fn sign_relay(&self, t: &[u8]) -> [u8; SIG_LEN] {
        self.sign(&[RELAY_CONTEXT, t].concat())
    }

    /// A signature over `msg` for `context` (a front end's own name for what it signs),
    /// apart from everything PeerParty itself signs (see APP_CONTEXT). `verify_bytes`
    /// checks it.
    pub fn sign_bytes(&self, context: &str, msg: &[u8]) -> [u8; SIG_LEN] {
        self.sign(&app_transcript(context, msg))
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public())
    }

    pub(crate) fn sign(&self, msg: &[u8]) -> [u8; SIG_LEN] {
        self.key.sign(msg).to_bytes()
    }
}

fn hello_transcript(game: &str, signer: (PeerId, &[u8; 16]), verifier: (PeerId, &[u8; 16])) -> Vec<u8> {
    let mut t = Vec::with_capacity(HELLO_CONTEXT.len() + 4 + game.len() + 64);
    t.extend_from_slice(HELLO_CONTEXT);
    t.extend_from_slice(&(game.len() as u32).to_le_bytes());
    t.extend_from_slice(game.as_bytes());
    for (id, nonce) in [signer, verifier] {
        t.extend_from_slice(&id.0);
        t.extend_from_slice(nonce);
    }
    t
}

fn ticket_transcript(game: &str, nonce: &[u8; 16]) -> Vec<u8> {
    let mut t = Vec::with_capacity(TICKET_CONTEXT.len() + 4 + game.len() + 16);
    t.extend_from_slice(TICKET_CONTEXT);
    t.extend_from_slice(&(game.len() as u32).to_le_bytes());
    t.extend_from_slice(game.as_bytes());
    t.extend_from_slice(nonce);
    t
}

/// Whether `sig` is `IdentityKey::sign_ticket` by the holder of `key`; the id it proves
/// is `PeerId::of_key(key)`.
pub fn ticket_proves(key: &[u8; KEY_LEN], sig: &[u8; SIG_LEN], game: &str, nonce: &[u8; 16]) -> bool {
    VerifyingKey::from_bytes(key).is_ok_and(|vk| vk.verify_strict(&ticket_transcript(game, nonce), &Signature::from_bytes(sig)).is_ok())
}

/// Whether `sig` is `IdentityKey::sign_hello` by the holder of `key`, as `signer`, for
/// `verifier` (us), and `key` is the one `signer`'s id comes from.
pub fn hello_proves(key: &[u8; KEY_LEN], sig: &[u8; SIG_LEN], game: &str, signer: (PeerId, &[u8; 16]), verifier: (PeerId, &[u8; 16])) -> bool {
    PeerId::of_key(key) == signer.0
        && VerifyingKey::from_bytes(key).is_ok_and(|vk| vk.verify_strict(&hello_transcript(game, signer, verifier), &Signature::from_bytes(sig)).is_ok())
}

/// Whether `sig` is `IdentityKey::sign_relay` of `t` by the holder of `key`.
pub fn relay_proves(key: &[u8; KEY_LEN], sig: &[u8; SIG_LEN], t: &[u8]) -> bool {
    VerifyingKey::from_bytes(key).is_ok_and(|vk| vk.verify_strict(&[RELAY_CONTEXT, t].concat(), &Signature::from_bytes(sig)).is_ok())
}

fn app_transcript(context: &str, msg: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(APP_CONTEXT.len() + 4 + context.len() + msg.len());
    t.extend_from_slice(APP_CONTEXT);
    t.extend_from_slice(&(context.len() as u32).to_le_bytes());
    t.extend_from_slice(context.as_bytes());
    t.extend_from_slice(msg);
    t
}

/// Whether `sig` is `IdentityKey::sign_bytes(context, msg)` by the holder of `key`.
pub fn verify_bytes(key: &[u8; KEY_LEN], context: &str, msg: &[u8], sig: &[u8; SIG_LEN]) -> bool {
    VerifyingKey::from_bytes(key).is_ok_and(|vk| vk.verify_strict(&app_transcript(context, msg), &Signature::from_bytes(sig)).is_ok())
}

/// A public key's short, shareable form: "3fa9-c1d2-0b7e-55aa" (64 bits of a hash).
pub fn fingerprint(key: &[u8; KEY_LEN]) -> String {
    let h = Sha256::new().chain_update(b"peerparty public id").chain_update(key).finalize();
    let s = hex(&h[..8]);
    format!("{}-{}-{}-{}", &s[..4], &s[4..8], &s[8..12], &s[12..16])
}

/// A fingerprint as typed (any case, with or without dashes), in its usual form.
pub fn normalize_fingerprint(s: &str) -> Option<String> {
    let h: String = s.chars().filter(|c| !matches!(c, '-' | ' ' | ':')).collect::<String>().to_ascii_lowercase();
    if h.len() != 16 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{}-{}-{}-{}", &h[..4], &h[4..8], &h[8..12], &h[12..16]))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_signatures_hold_only_for_their_context_and_message() {
        let (k, other) = (IdentityKey::generate(), IdentityKey::generate());
        let sig = k.sign_bytes("ctx", b"msg");
        assert!(verify_bytes(&k.public(), "ctx", b"msg", &sig));
        assert!(!verify_bytes(&other.public(), "ctx", b"msg", &sig));
        assert!(!verify_bytes(&k.public(), "ctx2", b"msg", &sig));
        assert!(!verify_bytes(&k.public(), "ctx", b"msh", &sig));
        // Never one of PeerParty's own proofs.
        let nonce = [7u8; 16];
        assert!(!ticket_proves(&k.public(), &k.sign_bytes("", &nonce), "", &nonce));
    }
}
