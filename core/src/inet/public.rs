//! Public sessions: playing with anyone online who runs the same game through PeerParty,
//! no room code needed (opt-in, `PEERPARTY_PUBLIC=1`).
//!
//! They ride the same tunnel as rooms, in a "room" whose secret everyone knows
//! (`RoomCode::public`), so the room keys keep nobody out. What a public session adds:
//!
//! - Each player has a long-term Ed25519 key (one per identity slot, kept next to the
//!   PeerId). The handshake's CONFIRM carries the player's details, its public key and a
//!   signature over both sides' fresh ephemeral keys and nonces plus those details
//!   (SIGMA-style). So a CONFIRM can't be replayed into another session or altered, and
//!   the details provably come from the key's owner; the session keys still come from
//!   the ephemeral X25519 exchange only (forward secrecy).
//! - A key is pinned to the PeerId it first came with (trust on first use): a different
//!   key claiming a known id is refused.
//! - A local block list of key fingerprints, kept in a text file. A key is free to make,
//!   so a blocked player can come back with a new one: for `NEW_KEY_COOLDOWN` after a
//!   block, keys we've never seen are refused from that player's internet address
//!   (keys we already know, and friends, still get in).
//! - Friends-only (`PEERPARTY_PUBLIC_FRIENDS_ONLY=1`): only keys on the friends list
//!   (`public-friends.txt`, `peerparty --friend FP`) connect.
//! - Rate limits and caps against floods (`RateLimit`, and the caps in `mod.rs`).
//! - Names are cleaned up (`sanitize_name`), and never the Windows user name.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::id::PeerId;

pub const KEY_LEN: usize = 32;
pub const SIG_LEN: usize = 64;
const CONFIRM_CONTEXT: &[u8] = b"peerparty public v1 confirm";
/// Longest name shown for a public player, in characters.
pub const MAX_NAME: usize = 32;
const MAX_BLOCKED: usize = 10_000;
const MAX_PINS: usize = 4096;
/// Keys remembered in memory for this session once the file is full.
const MAX_SESSION_PINS: usize = 65536;
/// What a copy signs to prove its key to the copy it shakes hands with on the mesh.
const HELLO_CONTEXT: &[u8] = b"peerparty mesh hello v3";

fn local_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("peerparty"))
}

/// A player's long-term signing key.
pub struct IdentityKey {
    key: SigningKey,
}

impl fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IdentityKey({})", self.fingerprint())
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, x) in out.iter_mut().enumerate() {
        *x = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl IdentityKey {
    pub fn generate() -> IdentityKey {
        IdentityKey::from_seed(super::crypto::random_array())
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

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public())
    }

    fn sign(&self, msg: &[u8]) -> [u8; SIG_LEN] {
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

/// Whether `sig` is `IdentityKey::sign_hello` by the holder of `key`, as `signer`, for
/// `verifier` (us), and `key` is the one `signer`'s id comes from.
pub fn hello_proves(key: &[u8; KEY_LEN], sig: &[u8; SIG_LEN], game: &str, signer: (PeerId, &[u8; 16]), verifier: (PeerId, &[u8; 16])) -> bool {
    PeerId::of_key(key) == signer.0
        && VerifyingKey::from_bytes(key).is_ok_and(|vk| vk.verify_strict(&hello_transcript(game, signer, verifier), &Signature::from_bytes(sig)).is_ok())
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

fn transcript(room_id: &[u8; 8], sender: &[u8; 48], receiver: &[u8; 48], info: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(CONFIRM_CONTEXT.len() + 8 + 96 + 4 + info.len());
    t.extend_from_slice(CONFIRM_CONTEXT);
    t.extend_from_slice(room_id);
    t.extend_from_slice(sender);
    t.extend_from_slice(receiver);
    t.extend_from_slice(&(info.len() as u32).to_le_bytes());
    t.extend_from_slice(info);
    t
}

/// What a public player seals into its CONFIRM: its details (`info`), its public key, and
/// its signature binding them to this session's two ephemeral blobs (`mine`, then
/// `theirs`).
pub fn advert(key: &IdentityKey, room_id: &[u8; 8], mine: &[u8; 48], theirs: &[u8; 48], info: &[u8]) -> Vec<u8> {
    let info = &info[..info.len().min(u16::MAX as usize)];
    let sig = key.sign(&transcript(room_id, mine, theirs, info));
    let mut out = Vec::with_capacity(2 + info.len() + KEY_LEN + SIG_LEN);
    out.extend_from_slice(&(info.len() as u16).to_le_bytes());
    out.extend_from_slice(info);
    out.extend_from_slice(&key.public());
    out.extend_from_slice(&sig);
    out
}

/// Checks an `advert` from `sender` to `receiver`: its details and public key, or `None`
/// for anything malformed, altered, or signed for another session.
pub fn open_advert<'a>(plain: &'a [u8], room_id: &[u8; 8], sender: &[u8; 48], receiver: &[u8; 48]) -> Option<(&'a [u8], [u8; KEY_LEN])> {
    let len = u16::from_le_bytes(plain.get(..2)?.try_into().ok()?) as usize;
    if plain.len() != 2 + len + KEY_LEN + SIG_LEN {
        return None;
    }
    let info = &plain[2..2 + len];
    let key: [u8; KEY_LEN] = plain[2 + len..2 + len + KEY_LEN].try_into().ok()?;
    let sig: [u8; SIG_LEN] = plain[2 + len + KEY_LEN..].try_into().ok()?;
    let vk = VerifyingKey::from_bytes(&key).ok()?;
    vk.verify_strict(&transcript(room_id, sender, receiver, info), &Signature::from_bytes(&sig)).ok()?;
    Some((info, key))
}

fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}' | '\u{180E}'
            | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFFB}' | '\u{E0000}'..='\u{E0FFF}')
}

/// A public player's name as we show it (and hand to games): no control or invisible
/// characters (no bidi tricks), whitespace collapsed, at most `MAX_NAME` characters,
/// and "Player" for nothing left or for anything that holds an IP address.
pub fn sanitize_name(s: &str) -> String {
    let mut out = String::new();
    let (mut n, mut space) = (0, true);
    for c in s.chars() {
        if invisible(c) && !c.is_whitespace() {
            continue;
        }
        if c.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
                n += 1;
            }
        } else {
            out.push(c);
            space = false;
            n += 1;
        }
        if n >= MAX_NAME {
            break;
        }
    }
    let out = out.trim().to_string();
    if out.is_empty() || super::redact::has_address(&out) { "Player".into() } else { out }
}

/// The name public players see for us: `PEERPARTY_PUBLIC_NAME`, else the name chosen in
/// Settings (`PEERPARTY_NAME`), else "Player-" and the start of our key's fingerprint.
/// Never the Windows user name.
pub fn our_name(chosen: Option<&str>, fp: &str) -> String {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let pick = chosen
        .map(String::from)
        .filter(|v| !v.trim().is_empty())
        .or_else(|| var("PEERPARTY_PUBLIC_NAME"))
        .or_else(|| var("PEERPARTY_NAME"));
    match pick {
        Some(n) => sanitize_name(&n),
        None => format!("Player-{}", &fp[..4.min(fp.len())]),
    }
}

#[derive(Default)]
struct BlockState {
    /// Fingerprint -> a note (the name it had when blocked).
    set: HashMap<String, String>,
    stamp: Option<SystemTime>,
    checked: Option<Instant>,
}

/// Players we never connect to in public sessions, by key fingerprint. Kept in a text
/// file (`%LOCALAPPDATA%\peerparty\blocked-players.txt`, one fingerprint per line, `#`
/// starts a note) that every running copy re-reads when it changes.
pub struct Blocklist {
    path: Option<PathBuf>,
    st: Mutex<BlockState>,
    /// The file's first line (block list or friends list).
    header: &'static str,
}

impl fmt::Debug for Blocklist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blocklist({} blocked)", self.list().len())
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Blocklist {
    pub fn default_path() -> Option<PathBuf> {
        local_dir().map(|d| d.join("blocked-players.txt"))
    }

    pub fn open(path: Option<PathBuf>) -> Blocklist {
        Blocklist::open_with(path, "# peerparty: players blocked in public sessions, one key fingerprint per line")
    }

    fn open_with(path: Option<PathBuf>, header: &'static str) -> Blocklist {
        let b = Blocklist { path, st: Mutex::new(BlockState::default()), header };
        b.reload(&mut b.st.lock().unwrap());
        b
    }

    /// The friends list: the same kind of file (`public-friends.txt`), for friends-only
    /// public sessions.
    pub fn friends(path: Option<PathBuf>) -> Blocklist {
        Blocklist::open_with(path, "# peerparty: friends for friends-only public sessions, one key fingerprint per line")
    }

    pub fn friends_path() -> Option<PathBuf> {
        local_dir().map(|d| d.join("public-friends.txt"))
    }

    pub fn in_memory() -> Blocklist {
        Blocklist::open(None)
    }

    fn reload(&self, st: &mut BlockState) {
        let Some(p) = &self.path else { return };
        st.stamp = mtime(p);
        st.set.clear();
        let Ok(text) = std::fs::read_to_string(p) else { return };
        for line in text.lines() {
            let (fp, note) = line.split_once('#').unwrap_or((line, ""));
            if let Some(fp) = normalize_fingerprint(fp)
                && st.set.len() < MAX_BLOCKED
            {
                st.set.insert(fp, note.trim().to_string());
            }
        }
    }

    /// Re-reads the file if it changed (looked at every 2 s at most). True when it did.
    pub fn refresh(&self) -> bool {
        let Some(p) = &self.path else { return false };
        let mut st = self.st.lock().unwrap();
        if st.checked.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return false;
        }
        st.checked = Some(Instant::now());
        if mtime(p) == st.stamp {
            return false;
        }
        self.reload(&mut st);
        true
    }

    pub fn is_blocked(&self, fp: &str) -> bool {
        self.st.lock().unwrap().set.contains_key(fp)
    }

    fn save(&self, st: &mut BlockState) -> io::Result<()> {
        let Some(p) = &self.path else { return Ok(()) };
        let mut text = format!("{}\n", self.header);
        let mut all: Vec<_> = st.set.iter().collect();
        all.sort();
        for (fp, note) in all {
            let note: String = note.chars().filter(|c| !c.is_control()).collect();
            text.push_str(&if note.is_empty() { format!("{fp}\n") } else { format!("{fp}  # {note}\n") });
        }
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(p, text)?;
        st.stamp = mtime(p);
        Ok(())
    }

    /// Blocks a fingerprint (`note`: who it was). False if it already was.
    pub fn add(&self, fp: &str, note: &str) -> io::Result<bool> {
        let fp = normalize_fingerprint(fp).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a player fingerprint"))?;
        let mut st = self.st.lock().unwrap();
        if self.path.is_some() {
            self.reload(&mut st);
        }
        if st.set.contains_key(&fp) {
            return Ok(false);
        }
        if st.set.len() >= MAX_BLOCKED {
            return Err(io::Error::other("the block list is full"));
        }
        st.set.insert(fp, sanitize_name(note));
        self.save(&mut st)?;
        Ok(true)
    }

    pub fn remove(&self, fp: &str) -> io::Result<bool> {
        let Some(fp) = normalize_fingerprint(fp) else { return Ok(false) };
        let mut st = self.st.lock().unwrap();
        if self.path.is_some() {
            self.reload(&mut st);
        }
        if st.set.remove(&fp).is_none() {
            return Ok(false);
        }
        self.save(&mut st)?;
        Ok(true)
    }

    pub fn len(&self) -> usize {
        self.st.lock().unwrap().set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every blocked fingerprint with its note, sorted.
    pub fn list(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self.st.lock().unwrap().set.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
        v.sort();
        v
    }
}

/// The keys of public players met before, to tell a returning player from a key made
/// a minute ago (a blocked player coming back as someone new is held off for a while,
/// see `Shared::new_keys_cooling`). `%LOCALAPPDATA%\peerparty\public-keys.txt`.
///
/// It was once what tied an id to its key (the first key seen for an id kept it). An id
/// is its key's hash now, which nobody has to remember, and which a stranger can't get
/// to first.
pub struct Pins {
    path: Option<PathBuf>,
    map: Mutex<Option<HashMap<PeerId, [u8; KEY_LEN]>>>,
}

impl fmt::Debug for Pins {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pins")
    }
}

impl Pins {
    pub fn default_path() -> Option<PathBuf> {
        local_dir().map(|d| d.join("public-keys.txt"))
    }

    pub fn open(path: Option<PathBuf>) -> Pins {
        Pins { path, map: Mutex::new(None) }
    }

    pub fn in_memory() -> Pins {
        Pins::open(None)
    }

    /// Whether we've seen `key` before (pinned for some id).
    pub fn knows_key(&self, key: &[u8; KEY_LEN]) -> bool {
        let g = self.map.lock().unwrap();
        g.as_ref().is_some_and(|m| m.values().any(|k| k == key)) || self.path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).is_some_and(|t| {
            let h = hex(key);
            t.lines().take(MAX_PINS).any(|l| l.trim().split_once(' ').is_some_and(|(_, k)| k == h))
        })
    }

    /// Remembers `key` (in the file for the first `MAX_PINS`, then in memory for this
    /// session, then not at all: a list that is full never keeps a player out).
    pub fn remember(&self, id: PeerId, key: &[u8; KEY_LEN]) {
        let mut g = self.map.lock().unwrap();
        let map = g.get_or_insert_with(|| {
            let mut m = HashMap::new();
            if let Some(text) = self.path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
                for l in text.lines().take(MAX_PINS) {
                    if let Some((i, k)) = l.trim().split_once(' ')
                        && let (Some(i), Some(k)) = (PeerId::from_hex(i), unhex::<KEY_LEN>(k))
                    {
                        m.entry(i).or_insert(k);
                    }
                }
            }
            m
        });
        if map.contains_key(&id) || map.len() >= MAX_SESSION_PINS {
            return;
        }
        map.insert(id, *key);
        if map.len() <= MAX_PINS {
            if let Some(p) = &self.path {
                use std::io::Write;
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                    let _ = writeln!(f, "{} {}", id.hex(), hex(key));
                }
            }
        }
    }
}

/// A token bucket per source address, for the handshakes that cost us work. Holds at
/// most `MAX_TRACKED` addresses: in a flood from more than that, new ones wait.
pub struct RateLimit {
    map: HashMap<IpAddr, (f32, Instant)>,
    burst: f32,
    per_sec: f32,
}

const MAX_TRACKED: usize = 4096;

impl RateLimit {
    pub fn new(burst: f32, per_sec: f32) -> RateLimit {
        RateLimit { map: HashMap::new(), burst, per_sec }
    }

    pub fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        if !self.map.contains_key(&ip) && self.map.len() >= MAX_TRACKED {
            // Forget addresses whose bucket has refilled anyway.
            let full = Duration::from_secs_f32(self.burst / self.per_sec);
            self.map.retain(|_, (_, t)| now.duration_since(*t) < full);
            if self.map.len() >= MAX_TRACKED {
                return false;
            }
        }
        let (burst, rate) = (self.burst, self.per_sec);
        let e = self.map.entry(ip).or_insert((burst, now));
        e.0 = (e.0 + now.duration_since(e.1).as_secs_f32() * rate).min(burst);
        e.1 = now;
        if e.0 >= 1.0 {
            e.0 -= 1.0;
            true
        } else {
            false
        }
    }

    pub fn tracked(&self) -> usize {
        self.map.len()
    }
}

/// Public sessions' settings: our key (from the identity slot when `None`), the block
/// list, the key pins and the name to show (`None`: see `our_name`).
#[derive(Clone)]
pub struct PublicCfg {
    pub key: Option<Arc<IdentityKey>>,
    pub blocked: Arc<Blocklist>,
    pub pins: Arc<Pins>,
    pub name: Option<String>,
    /// Fingerprints let in when `friends_only` (and never held back by a block cooldown).
    pub friends: Arc<Blocklist>,
    pub friends_only: bool,
}

impl fmt::Debug for PublicCfg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicCfg").field("key", &self.key).field("blocked", &self.blocked).finish()
    }
}

impl PublicCfg {
    /// The files under `%LOCALAPPDATA%\peerparty`.
    pub fn standard() -> PublicCfg {
        PublicCfg {
            key: None,
            blocked: Arc::new(Blocklist::open(Blocklist::default_path())),
            pins: Arc::new(Pins::open(Pins::default_path())),
            name: None,
            friends: Arc::new(Blocklist::friends(Blocklist::friends_path())),
            friends_only: std::env::var("PEERPARTY_PUBLIC_FRIENDS_ONLY")
                .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "on" | "yes" | "true")),
        }
    }

    /// Nothing on disk: a fresh key, an empty block list (tests).
    pub fn ephemeral(name: &str) -> PublicCfg {
        PublicCfg {
            key: Some(Arc::new(IdentityKey::generate())),
            blocked: Arc::new(Blocklist::in_memory()),
            pins: Arc::new(Pins::in_memory()),
            name: Some(name.into()),
            friends: Arc::new(Blocklist::friends(None)),
            friends_only: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blobs() -> ([u8; 8], [u8; 48], [u8; 48]) {
        (super::super::crypto::random_array(), super::super::crypto::random_array(), super::super::crypto::random_array())
    }

    #[test]
    fn adverts_verify_and_tampering_fails() {
        let k = IdentityKey::generate();
        let (room, a, b) = blobs();
        let ad = advert(&k, &room, &a, &b, b"details");
        let (info, key) = open_advert(&ad, &room, &a, &b).expect("verifies");
        assert_eq!((info, key), (&b"details"[..], k.public()));
        // Any flipped bit anywhere fails.
        for i in 0..ad.len() {
            for bit in [0x01u8, 0x80] {
                let mut t = ad.clone();
                t[i] ^= bit;
                assert!(open_advert(&t, &room, &a, &b).is_none(), "byte {i} bit {bit:#x}");
            }
        }
        // Another session (either blob), another room, the sides swapped, a trailing or
        // missing byte: refused.
        let (room2, a2, b2) = blobs();
        assert!(open_advert(&ad, &room, &a2, &b).is_none());
        assert!(open_advert(&ad, &room, &a, &b2).is_none());
        assert!(open_advert(&ad, &room2, &a, &b).is_none());
        assert!(open_advert(&ad, &room, &b, &a).is_none());
        let mut long = ad.clone();
        long.push(0);
        assert!(open_advert(&long, &room, &a, &b).is_none());
        assert!(open_advert(&ad[..ad.len() - 1], &room, &a, &b).is_none());
        // Someone else's key in place of the signer's.
        let other = IdentityKey::generate().public();
        let mut swapped = ad.clone();
        let at = 2 + b"details".len();
        swapped[at..at + KEY_LEN].copy_from_slice(&other);
        assert!(open_advert(&swapped, &room, &a, &b).is_none());
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(sanitize_name("  Robin \t the\nGreat "), "Robin the Great");
        assert_eq!(sanitize_name("a\u{202E}b\u{200B}c\u{0007}"), "abc");
        assert_eq!(sanitize_name(&"x".repeat(100)).chars().count(), MAX_NAME);
        assert_eq!(sanitize_name("203.0.113.9"), "Player");
        assert_eq!(sanitize_name("host 10.0.0.1:27015"), "Player");
        assert_eq!(sanitize_name("\u{200B}\u{200B}"), "Player");
        assert_eq!(sanitize_name("Ünïcødé ✓"), "Ünïcødé ✓");
    }

    #[test]
    fn our_name_is_never_the_user_name() {
        let n = our_name(None, "abcd-ef01-2345-6789");
        if std::env::var_os("PEERPARTY_PUBLIC_NAME").is_none() && std::env::var_os("PEERPARTY_NAME").is_none() {
            assert_eq!(n, "Player-abcd");
        }
        if let Ok(u) = std::env::var("USERNAME")
            && std::env::var_os("PEERPARTY_NAME").is_none()
            && std::env::var_os("PEERPARTY_PUBLIC_NAME").is_none()
        {
            assert_ne!(n, u);
        }
        assert_eq!(our_name(Some("1.2.3.4"), "abcd"), "Player");
    }

    #[test]
    fn fingerprints_normalize() {
        let k = IdentityKey::generate();
        let fp = k.fingerprint();
        assert_eq!(fp.len(), 19);
        assert_eq!(normalize_fingerprint(&fp.to_uppercase().replace('-', "")), Some(fp.clone()));
        assert_eq!(normalize_fingerprint("xyz"), None);
        assert_eq!(normalize_fingerprint("0123456789abcdeg"), None);
    }

    #[test]
    fn keys_persist() {
        let dir = std::env::temp_dir().join(format!("pp-pubkey-{}", std::process::id()));
        let p = dir.join("k.ed25519");
        let a = IdentityKey::load_or_create(&p);
        let b = IdentityKey::load_or_create(&p);
        assert_eq!(a.public(), b.public());
        assert_eq!(IdentityKey::from_text(&format!("{}\r\n", std::fs::read_to_string(&p).unwrap())).map(|k| k.public()), Some(a.public()));
        // A file that reads as something else is kept aside, and a new key takes its place.
        std::fs::write(&p, "not a key").unwrap();
        let c = IdentityKey::load_or_create(&p);
        assert_ne!(c.public(), a.public());
        assert_eq!(std::fs::read_to_string(dir.join("k.bad")).unwrap(), "not a key");
        assert_eq!(IdentityKey::load_or_create(&p).public(), c.public());
        // One that is there but can't be read (here a folder) isn't written over.
        let locked = dir.join("locked.ed25519");
        std::fs::create_dir_all(&locked).unwrap();
        let d = IdentityKey::load_or_create(&locked);
        assert!(locked.is_dir(), "left as it was");
        assert_ne!(IdentityKey::load_or_create(&locked).public(), d.public(), "a key for one run each time");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3, "no temporary files left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_list_file_round_trips_and_is_shared() {
        let dir = std::env::temp_dir().join(format!("pp-blocks-{}", std::process::id()));
        let p = dir.join("blocked.txt");
        let a = Blocklist::open(Some(p.clone()));
        let fp = IdentityKey::generate().fingerprint();
        assert!(a.add(&fp, "Griefer\n#x").unwrap());
        assert!(!a.add(&fp.to_uppercase(), "again").unwrap());
        assert!(a.is_blocked(&fp));
        // Another copy reads the same file.
        let b = Blocklist::open(Some(p.clone()));
        assert!(b.is_blocked(&fp));
        assert_eq!(b.list()[0].0, fp);
        assert!(a.add("not a fingerprint", "").is_err());
        assert!(a.remove(&fp).unwrap());
        assert!(!a.is_blocked(&fp));
        std::thread::sleep(Duration::from_millis(50));
        let _ = std::fs::write(&p, format!("garbage\n{fp} # hand-edited\n"));
        std::thread::sleep(Duration::from_millis(2100));
        b.refresh();
        assert!(b.is_blocked(&fp), "{:?}", b.list());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_met_before_are_remembered() {
        let p = Pins::in_memory();
        let (k1, k2) = (IdentityKey::generate().public(), IdentityKey::generate().public());
        p.remember(PeerId::of_key(&k1), &k1);
        p.remember(PeerId::of_key(&k1), &k1);
        p.remember(PeerId::of_key(&k2), &k2);
        assert!(p.knows_key(&k1) && p.knows_key(&k2));
        assert!(!p.knows_key(&IdentityKey::generate().public()));
    }

    #[test]
    fn the_key_file_stops_growing_and_a_full_list_keeps_nobody_out() {
        let dir = std::env::temp_dir().join(format!("pp-pins-{}", std::process::id()));
        let path = dir.join("keys.txt");
        let p = Pins::open(Some(path.clone()));
        let k = IdentityKey::generate().public();
        for _ in 0..MAX_PINS {
            p.remember(PeerId::random(), &k);
        }
        let late = IdentityKey::generate().public();
        p.remember(PeerId::of_key(&late), &late);
        assert!(p.knows_key(&late), "past the file's limit a key is still remembered for the session");
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(lines, MAX_PINS, "the file stops growing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mesh's hello: an id is proved by the key it is the hash of, for one
    /// connection and one other side.
    #[test]
    fn a_hello_is_proved_only_by_its_own_key_for_this_connection() {
        let (ann, ben, eve) = (IdentityKey::generate(), IdentityKey::generate(), IdentityKey::generate());
        let id = |k: &IdentityKey| PeerId::of_key(&k.public());
        let (na, nb) = ([1u8; 16], [2u8; 16]);
        let sig = ann.sign_hello("game", (id(&ann), &na), (id(&ben), &nb));
        let ok = |key: &IdentityKey, sig: &[u8; SIG_LEN], game: &str, signer: (PeerId, &[u8; 16]), verifier: (PeerId, &[u8; 16])| hello_proves(&key.public(), sig, game, signer, verifier);
        assert!(ok(&ann, &sig, "game", (id(&ann), &na), (id(&ben), &nb)));
        // Another connection (either number differs), another game, another listener.
        assert!(!ok(&ann, &sig, "game", (id(&ann), &[9; 16]), (id(&ben), &nb)));
        assert!(!ok(&ann, &sig, "game", (id(&ann), &na), (id(&ben), &[9; 16])));
        assert!(!ok(&ann, &sig, "other", (id(&ann), &na), (id(&ben), &nb)));
        assert!(!ok(&ann, &sig, "game", (id(&ann), &na), (id(&eve), &nb)));
        // Eve with her own key, saying she is Ann: the id isn't her key's.
        let forged = eve.sign_hello("game", (id(&ann), &na), (id(&ben), &nb));
        assert!(!ok(&eve, &forged, "game", (id(&ann), &na), (id(&ben), &nb)));
        // Eve with Ann's public key and her own signature.
        assert!(!ok(&ann, &forged, "game", (id(&ann), &na), (id(&ben), &nb)));
    }

    #[test]
    fn rate_limit_buckets() {
        let mut r = RateLimit::new(5.0, 2.0);
        let t = Instant::now();
        let ip: IpAddr = "203.0.113.1".parse().unwrap();
        assert_eq!((0..10).filter(|_| r.allow(ip, t)).count(), 5);
        assert!(r.allow("203.0.113.2".parse().unwrap(), t));
        assert!(r.allow(ip, t + Duration::from_millis(600)));
        assert!(!r.allow(ip, t + Duration::from_millis(600)));
        // Flooded with addresses, the table stays bounded.
        for i in 0..20_000u32 {
            r.allow(IpAddr::V4(i.into()), t);
        }
        assert!(r.tracked() <= MAX_TRACKED);
    }
}