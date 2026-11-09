//! Who this copy of the game is on the LAN.
//!
//! A peer id is 128 random bits, kept per machine *slot* under
//! `%LOCALAPPDATA%\peerparty\identity`. Each running copy holds an exclusive lock on the
//! slot it took, so two copies on one PC get two identities and the same copy keeps its
//! id across runs (games that key saves or "last host" by user id keep working).

use std::collections::hash_map::RandomState;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::hash::BuildHasher;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct PeerId(pub [u8; 16]);

impl PeerId {
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
    pub id: PeerId,
    pub name: String,
    pub slot: u32,
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
                let id_path = dir.join(format!("slot-{slot}.id"));
                let id = fs::read_to_string(&id_path)
                    .ok()
                    .and_then(|s| PeerId::from_hex(&s))
                    .unwrap_or_else(|| {
                        let id = PeerId::random();
                        let _ = fs::write(&id_path, id.hex());
                        id
                    });
                let name = if slot == 0 {
                    user
                } else {
                    format!("{user} ({})", slot + 1)
                };
                return Identity {
                    id,
                    name,
                    slot,
                    _lock: Some(lock),
                };
            }
        }
        Identity::ephemeral(&user)
    }

    pub fn ephemeral(name: &str) -> Identity {
        Identity {
            id: PeerId::random(),
            name: name.into(),
            slot: u32::MAX,
            _lock: None,
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
        drop((b, again));
        let _ = fs::remove_dir_all(&dir);
    }
}
