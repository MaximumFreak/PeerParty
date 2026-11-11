//! Who this copy of the game is on the LAN.
//!
//! A player is a signing key, kept per machine *slot* under
//! `%LOCALAPPDATA%\peerparty\identity`, and a peer id is 128 bits of that key's hash:
//! saying "I am this id" means proving the key, which every way of connecting asks for
//! (the mesh's hello, a room's and a public session's handshake), so an id can't be
//! claimed by anyone but its player. Each running copy holds an exclusive lock on the
//! slot it took, so two copies on one PC get two identities and the same copy keeps its
//! id across runs (games that key saves or "last host" by user id keep working).
//!
//! A slot's id used to be random bytes in `slot-N.id`. Those ids aren't dropped: each
//! one a slot had is listed in `slot-N.previous` (newest first, one hex id a line), so
//! saves a game keyed by the old id can be carried over to the new one (the hook's
//! saves.rs). `slot-N.id` is only deleted once its id is listed.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::hash::BuildHasher;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::inet::public::IdentityKey;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct PeerId(pub [u8; 16]);

impl PeerId {
    /// The id of the player with this public key.
    pub fn of_key(key: &[u8; 32]) -> Self {
        let h = Sha256::new().chain_update(b"peerparty peer id").chain_update(key).finalize();
        PeerId(h[..16].try_into().expect("16 of 32 bytes"))
    }

    /// Random bytes in an id's shape (lobby ids and the like; a player's id is `of_key`).
    /// RandomState is seeded from the OS generator, which is all this needs.
    pub fn random() -> Self {
        let mut out = [0u8; 16];
        for (i, chunk) in out.chunks_mut(8).enumerate() {
            let v = RandomState::new().hash_one(i as u64 ^ std::process::id() as u64);
            chunk.copy_from_slice(&v.to_le_bytes());
        }
        PeerId(out)
    }

    /// 32 lowercase hex digits, which is also the format of an EOS ProductUserId.
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = [0u8; 16];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(PeerId(out))
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({})", &self.hex()[..8])
    }
}

pub struct Identity {
    /// `PeerId::of_key` of `key`.
    pub id: PeerId,
    pub name: String,
    pub slot: u32,
    /// What proves the id is ours.
    pub key: Arc<IdentityKey>,
    _lock: Option<File>,
}

impl Identity {
    /// The default location: `%LOCALAPPDATA%\peerparty\identity`.
    pub fn default_dir() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("peerparty")
            .join("identity")
    }

    /// Takes the first free slot in `dir`. Falls back to a throwaway id if the folder
    /// can't be used, so a read-only profile still gets onto the LAN.
    pub fn claim(dir: &Path) -> Identity {
        // The player's chosen name (the launcher's Settings), else the Windows user name.
        let user = std::env::var("PEERPARTY_NAME")
            .ok()
            .map(|n| n.trim().chars().take(32).collect::<String>())
            .filter(|n| !n.is_empty())
            .or_else(|| std::env::var("USERNAME").ok())
            .unwrap_or_else(|| "Player".into());
        if fs::create_dir_all(dir).is_ok() {
            for slot in 0..32u32 {
                let lock = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(false)
                    .share_mode(0)
                    .open(dir.join(format!("slot-{slot}.lock")));
                let Ok(lock) = lock else { continue };
                let key = Arc::new(IdentityKey::load_or_create(&dir.join(format!("slot-{slot}.ed25519"))));
                let id = PeerId::of_key(&key.public());
                // The id used to be random bytes in a file of its own: listed among the
                // slot's previous ids first, then the file goes.
                retire_old_id(dir, slot, id);
                let name = if slot == 0 {
                    user
                } else {
                    format!("{user} ({})", slot + 1)
                };
                return Identity {
                    id,
                    name,
                    slot,
                    key,
                    _lock: Some(lock),
                };
            }
        }
        Identity::ephemeral(&user)
    }

    /// The ids `slot` had before its current one, newest first (`slot-N.previous`).
    pub fn previous_ids(dir: &Path, slot: u32) -> Vec<PeerId> {
        fs::read_to_string(dir.join(format!("slot-{slot}.previous")))
            .map(|t| t.lines().filter_map(PeerId::from_hex).collect())
            .unwrap_or_default()
    }

    /// Lists `old` among `slot`'s previous ids (newest first), unless it is `now` or
    /// listed already. False if the list couldn't be written.
    pub fn remember_previous(dir: &Path, slot: u32, old: PeerId, now: PeerId) -> bool {
        let mut ids = Self::previous_ids(dir, slot);
        if old == now || ids.contains(&old) {
            return true;
        }
        ids.insert(0, old);
        let text: String = ids.iter().map(|i| format!("{}\r\n", i.hex())).collect();
        let path = dir.join(format!("slot-{slot}.previous"));
        let tmp = dir.join(format!("slot-{slot}.previous.tmp"));
        fs::write(&tmp, text).is_ok() && fs::rename(&tmp, &path).is_ok()
    }

    /// A throwaway identity: a fresh key, nothing on disk.
    pub fn ephemeral(name: &str) -> Identity {
        let key = Arc::new(IdentityKey::generate());
        Identity {
            id: PeerId::of_key(&key.public()),
            name: name.into(),
            slot: u32::MAX,
            key,
            _lock: None,
        }
    }
}

/// `slot-N.id`, from before ids were their key's: its id goes on the slot's previous
/// list, and only once it's there is the file deleted. A file that doesn't read as an id
/// is left alone.
fn retire_old_id(dir: &Path, slot: u32, now: PeerId) {
    let path = dir.join(format!("slot-{slot}.id"));
    let Ok(text) = fs::read_to_string(&path) else { return };
    if let Some(old) = PeerId::from_hex(&text) {
        if Identity::remember_previous(dir, slot, old, now) {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_rejects_junk() {
        let id = PeerId::random();
        assert_eq!(PeerId::from_hex(&id.hex()), Some(id));
        assert_eq!(id.hex().len(), 32);
        assert_eq!(PeerId::from_hex("xyz"), None);
        assert_ne!(PeerId::random(), PeerId::random());
    }

    #[test]
    fn two_claims_in_one_folder_get_two_slots() {
        let dir = std::env::temp_dir().join(format!("peerparty-id-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let a = Identity::claim(&dir);
        let b = Identity::claim(&dir);
        assert_ne!(a.slot, b.slot);
        assert_ne!(a.id, b.id);
        let a_id = a.id;
        let a_slot = a.slot;
        drop(a);
        let again = Identity::claim(&dir);
        assert_eq!(
            (again.slot, again.id),
            (a_slot, a_id),
            "a freed slot keeps its id"
        );
        assert_eq!(again.id, PeerId::of_key(&again.key.public()), "which is its key's");
        assert_ne!(PeerId::of_key(&[1; 32]), PeerId::of_key(&[2; 32]));
        drop((b, again));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A slot's id from before ids were their key's is kept on its previous list, newest
    /// first, and its old file only goes once it is listed.
    #[test]
    fn an_old_slot_id_is_remembered_before_its_file_goes() {
        let dir = std::env::temp_dir().join(format!("peerparty-id-previous-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let old = PeerId([0x10; 16]);
        fs::write(dir.join("slot-0.id"), old.hex()).unwrap();
        let me = Identity::claim(&dir);
        assert_eq!(me.slot, 0);
        assert_ne!(me.id, old);
        assert_eq!(Identity::previous_ids(&dir, 0), [old]);
        assert!(!dir.join("slot-0.id").exists(), "listed, so its file went");
        // Listed once; a later one goes first; the slot's own id is never listed.
        let older = PeerId([0x20; 16]);
        assert!(Identity::remember_previous(&dir, 0, old, me.id));
        assert!(Identity::remember_previous(&dir, 0, me.id, me.id));
        assert!(Identity::remember_previous(&dir, 0, older, me.id));
        assert_eq!(Identity::previous_ids(&dir, 0), [older, old]);
        drop(me);
        // A file that isn't an id stays where it is.
        fs::write(dir.join("slot-1.id"), "not an id").unwrap();
        let me = Identity::claim(&dir);
        assert_eq!(me.slot, 0);
        let other = Identity::claim(&dir);
        assert_eq!(other.slot, 1);
        assert!(dir.join("slot-1.id").exists());
        assert!(Identity::previous_ids(&dir, 1).is_empty());
        assert!(Identity::previous_ids(&dir, 7).is_empty(), "no list, no ids");
        drop((me, other));
        let _ = fs::remove_dir_all(&dir);
    }
}
