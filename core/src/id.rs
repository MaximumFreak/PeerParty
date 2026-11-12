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
//!
//! A dedicated server takes slots of its own (`server-N.*`, `claim_server`), so it
//! never holds a player's slot: a server started before the game would otherwise be
//! the PC's first copy, with the player's name and the PC's own ids, and push the
//! player to a second slot.

use std::fmt;
use std::fs::{self, File, OpenOptions};
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

    /// Random bytes in an id's shape (lobby ids and the like; a player's id is `of_key`),
    /// from the OS's secure random source.
    pub fn random() -> Self {
        PeerId(crate::inet::crypto::random_array())
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
    /// A dedicated server's identity (`claim_server`): its slots are apart from players'.
    pub server: bool,
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

    /// Takes the first free player slot in `dir`. Falls back to a throwaway id if the
    /// folder can't be used, so a read-only profile still gets onto the LAN.
    pub fn claim(dir: &Path) -> Identity {
        Self::claim_as(dir, false)
    }

    /// Takes the first free server slot in `dir` (a dedicated server's), named
    /// "<player>'s server", or "<player>'s server (N)" after the first.
    pub fn claim_server(dir: &Path) -> Identity {
        Self::claim_as(dir, true)
    }

    fn claim_as(dir: &Path, server: bool) -> Identity {
        // The player's chosen name (the launcher's Settings), else the Windows user name.
        let user = std::env::var("PEERPARTY_NAME")
            .ok()
            .map(|n| n.trim().chars().take(32).collect::<String>())
            .filter(|n| !n.is_empty())
            .or_else(|| std::env::var("USERNAME").ok())
            .unwrap_or_else(|| "Player".into());
        let (prefix, base) = if server { ("server", format!("{user}'s server")) } else { ("slot", user) };
        if fs::create_dir_all(dir).is_ok() {
            for slot in 0..32u32 {
                let lock = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(false)
                    .share_mode(0)
                    .open(dir.join(format!("{prefix}-{slot}.lock")));
                let Ok(lock) = lock else { continue };
                let key = Arc::new(IdentityKey::load_or_create(&dir.join(format!("{prefix}-{slot}.ed25519"))));
                let id = PeerId::of_key(&key.public());
                // The id used to be random bytes in a file of its own: listed among the
                // slot's previous ids first, then the file goes (players' slots only:
                // server slots came later).
                if !server {
                    retire_old_id(dir, slot, id);
                }
                let name = if slot == 0 { base } else { format!("{base} ({})", slot + 1) };
                return Identity {
                    id,
                    name,
                    slot,
                    server,
                    key,
                    _lock: Some(lock),
                };
            }
        }
        let mut me = Identity::ephemeral(&base);
        me.server = server;
        me
    }

    /// The ids of every identity slot on this PC (players' and servers'), from their
    /// key files in `dir`: a peer with one of these is another copy on this PC.
    pub fn ids_on_this_pc(dir: &Path) -> Vec<PeerId> {
        let mut out = Vec::new();
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            let slot_file = (name.starts_with("slot-") || name.starts_with("server-")) && name.ends_with(".ed25519");
            if !slot_file {
                continue;
            }
            let key = fs::read_to_string(e.path()).ok().and_then(|t| IdentityKey::from_text(&t));
            if let Some(key) = key {
                out.push(PeerId::of_key(&key.public()));
            }
        }
        out
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
            server: false,
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

    /// A dedicated server's slots are apart from players': a server running first leaves
    /// the player the first slot, its name and its id.
    #[test]
    fn a_server_never_takes_a_players_slot() {
        let dir = std::env::temp_dir().join(format!("peerparty-id-server-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let server = Identity::claim_server(&dir);
        let player = Identity::claim(&dir);
        let second_server = Identity::claim_server(&dir);
        assert_eq!((player.slot, player.server), (0, false));
        assert!(!player.name.contains('('), "the player keeps the first slot's plain name");
        assert_eq!((server.slot, server.server), (0, true));
        assert_eq!(server.name, format!("{}'s server", player.name));
        assert_eq!((second_server.slot, second_server.name.clone()), (1, format!("{}'s server (2)", player.name)));
        assert_ne!(server.id, player.id);
        assert!(dir.join("server-0.ed25519").exists() && dir.join("slot-0.ed25519").exists());
        let mut here = Identity::ids_on_this_pc(&dir);
        here.sort();
        let mut want = vec![server.id, player.id, second_server.id];
        want.sort();
        assert_eq!(here, want, "every slot's id, players' and servers'");
        let server_id = server.id;
        drop(server);
        assert_eq!(Identity::claim_server(&dir).id, server_id, "a server keeps its id across runs");
        drop((player, second_server));
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
