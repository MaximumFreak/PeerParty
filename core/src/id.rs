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
//! Slots are held per game (`claim_for`: the lock and owner files carry a tag of the
//! game's key): the player is the same player in two different games running at once
//! (their keys are the slot's), and a second copy of the same game takes the next slot.
//!
//! A dedicated server takes slots of its own (`server-N.*`, `claim_server`), so it
//! never holds a player's slot: a server started before the game would otherwise be
//! the PC's first copy, with the player's name and the PC's own ids, and push the
//! player to a second slot.
//!
//! A game started again straight after quitting can find its slot still held by the
//! run that is quitting (a game can take seconds to close after its window has gone).
//! The next slot would make it a different player: another id, a LAN SteamID, other
//! saves. So a slot's holder says who it is (`slot-N.owner`: its process id and
//! program), and a slot held by an earlier run of the same program that has no window
//! left is waited for (EXITING_WAIT). The hook also lets the slot go as soon as the
//! game starts exiting (`Lan::release_slot`).
//!
//! A starter that waited long enough to take itself for the game (a menu before the
//! game starts, with internet play set up) holds the first slot when the game it
//! starts comes up. It lets the slot go once the game says it is the game, and the game
//! waits for a slot its starter holds (`take_slot_from`), so the game is still this
//! PC's first player.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        Self::claim_as(dir, false, None)
    }

    /// Takes the first player slot in `dir` that no other copy of `game` holds: copies
    /// of other games don't count (the player is the same one in each).
    pub fn claim_for(dir: &Path, game: &str) -> Identity {
        Self::claim_as(dir, false, Some(game))
    }

    /// Takes the first free server slot in `dir` (a dedicated server's), named
    /// "<player>'s server", or "<player>'s server (N)" after the first.
    pub fn claim_server(dir: &Path) -> Identity {
        Self::claim_as(dir, true, None)
    }

    /// As `claim_server`, held per game.
    pub fn claim_server_for(dir: &Path, game: &str) -> Identity {
        Self::claim_as(dir, true, Some(game))
    }

    fn claim_as(dir: &Path, server: bool, game: Option<&str>) -> Identity {
        let tag = game.map(|g| format!(".{}", game_tag(g))).unwrap_or_default();
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
                let path = dir.join(format!("{prefix}-{slot}{tag}.lock"));
                let owner = dir.join(format!("{prefix}-{slot}{tag}.owner"));
                let mut lock = open_lock(&path);
                // Held by an earlier run of this program on its way out: wait for it,
                // for as long as it stays without a window (players' slots). Held by
                // this game's starter: wait for it to let the slot go.
                let until = Instant::now() + EXITING_WAIT;
                while lock.is_err() && !server && Instant::now() < until && (leaving(&owner) || held_by_starter(&owner)) {
                    std::thread::sleep(Duration::from_millis(100));
                    lock = open_lock(&path);
                }
                let Ok(lock) = lock else { continue };
                let _ = fs::write(&owner, format!("{}\r\n{}\r\n", std::process::id(), own_exe().unwrap_or_default()));
                let key = Arc::new(IdentityKey::load_or_create(&dir.join(format!("{prefix}-{slot}.ed25519"))));
                let id = PeerId::of_key(&key.public());
                // The id used to be random bytes in a file of its own: listed among the
                // slot's previous ids first, then the file goes (players' slots only:
                // server slots came later).
                if !server {
                    retire_old_id(dir, slot, id);
                }
                let name = copy_name(&base, slot, if server { None } else { other_name() });
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

}

/// The name the second and later player copies on this PC go by (the launcher's
/// "Second player's name", `PEERPARTY_OTHER_NAME`), when one is set.
fn other_name() -> Option<String> {
    std::env::var("PEERPARTY_OTHER_NAME")
        .ok()
        .map(|n| n.trim().chars().take(32).collect::<String>())
        .filter(|n| !n.is_empty())
}

/// The name of the copy in `slot` on this PC: the first is `base`; later ones are
/// `other` when one is set (the third onwards numbered, "Sam (3)"), else "base (2)".
fn copy_name(base: &str, slot: u32, other: Option<String>) -> String {
    match (slot, other) {
        (0, _) => base.to_string(),
        (1, Some(o)) => o,
        (n, Some(o)) => format!("{o} ({})", n + 1),
        (n, None) => format!("{base} ({})", n + 1),
    }
}

impl Identity {
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

/// How long a new copy waits for its slot's holder to finish quitting (see the top).
const EXITING_WAIT: Duration = Duration::from_secs(8);

/// The process (the game's starter) whose slot this one takes over: 0 for none.
static STARTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// This process is the game that `pid` (its starter) started: a slot `pid` holds is
/// waited for, as a quitting run's is, since the starter lets it go once this process
/// says it is the game.
pub fn take_slot_from(pid: u32) {
    STARTER.store(pid, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a held slot's owner (from its owner file) is the running starter set by
/// `take_slot_from`.
fn held_by_starter(owner: &Path) -> bool {
    let starter = STARTER.load(std::sync::atomic::Ordering::Relaxed);
    if starter == 0 || starter == std::process::id() {
        return false;
    }
    let Ok(text) = fs::read_to_string(owner) else { return false };
    let Some(pid) = text.lines().next().and_then(|p| p.trim().parse::<u32>().ok()) else { return false };
    let Some(exe) = text.lines().nth(1) else { return false };
    // The owner file names a process id; that id may since belong to another program.
    pid == starter && process_exe(pid).is_some_and(|e| e.eq_ignore_ascii_case(exe.trim()))
}

/// A short, file-name-safe tag for a game's key (its slot locks are per game).
pub fn game_tag(game: &str) -> String {
    Sha256::digest(game.as_bytes())[..6].iter().map(|b| format!("{b:02x}")).collect()
}

fn open_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).write(true).truncate(false).share_mode(0).open(path)
}

/// This process's program, as `QueryFullProcessImageNameW` gives it.
fn own_exe() -> Option<String> {
    std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned())
}

/// Whether a held slot's owner (from its owner file) is an earlier run of this program
/// that is quitting: still running, the same program as us, and with no visible window.
fn leaving(owner: &Path) -> bool {
    let Ok(text) = fs::read_to_string(owner) else { return false };
    let mut lines = text.lines();
    let (Some(pid), Some(exe)) = (lines.next().and_then(|p| p.trim().parse::<u32>().ok()), lines.next()) else {
        return false;
    };
    let Some(ours) = own_exe() else { return false };
    if pid == std::process::id() || !exe.trim().eq_ignore_ascii_case(&ours) {
        return false;
    }
    let Some(theirs) = process_exe(pid) else { return false };
    // The owner file names a process id; that id may since belong to another program.
    theirs.eq_ignore_ascii_case(&ours) && !has_visible_window(pid)
}

/// The program a running process was started from; None when it has ended.
fn process_exe(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
        WaitForSingleObject,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return None;
        }
        let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        (alive && ok).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn has_visible_window(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};
    struct Find {
        pid: u32,
        found: bool,
    }
    unsafe extern "system" fn each(hwnd: HWND, l: LPARAM) -> BOOL {
        let f = unsafe { &mut *(l as *mut Find) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == f.pid && unsafe { IsWindowVisible(hwnd) } != 0 {
            f.found = true;
            return 0;
        }
        1
    }
    let mut f = Find { pid, found: false };
    unsafe { EnumWindows(Some(each), &mut f as *mut Find as LPARAM) };
    f.found
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

    /// The second player's name goes to copy 2 only; copy 1 keeps the player's name, and
    /// with none set, copy 2 is "name (2)" as before.
    #[test]
    fn the_second_players_name_is_for_later_copies_only() {
        assert_eq!(copy_name("Kim", 0, Some("Sam".into())), "Kim");
        assert_eq!(copy_name("Kim", 1, Some("Sam".into())), "Sam");
        assert_eq!(copy_name("Kim", 2, Some("Sam".into())), "Sam (3)");
        assert_eq!(copy_name("Kim", 1, None), "Kim (2)");
    }

    /// Slots are held per game: the player is the same in two games at once, and a
    /// second copy of one game is another player.
    #[test]
    fn slots_are_held_per_game() {
        let dir = std::env::temp_dir().join(format!("peerparty-id-game-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let a = Identity::claim_for(&dir, "steam:480");
        let other_game = Identity::claim_for(&dir, "eos:abc");
        assert_eq!((a.slot, other_game.slot), (0, 0));
        assert_eq!(a.id, other_game.id, "one player in both games");
        let second = Identity::claim_for(&dir, "steam:480");
        assert_eq!(second.slot, 1);
        assert_ne!(second.id, a.id);
        assert_ne!(game_tag("steam:480"), game_tag("eos:abc"));
        drop((a, other_game, second));
        let _ = fs::remove_dir_all(&dir);
    }

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

    /// A slot's holder says who it is; a holder that is this very process, or no longer
    /// runs, or is another program, or still has its window, isn't waited for.
    #[test]
    fn a_slot_is_waited_for_only_while_an_earlier_run_quits() {
        let dir = std::env::temp_dir().join(format!("peerparty-id-owner-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let me = Identity::claim(&dir);
        let owner = dir.join("slot-0.owner");
        let text = fs::read_to_string(&owner).unwrap();
        assert_eq!(text.lines().next(), Some(std::process::id().to_string().as_str()));
        assert!(!leaving(&owner), "this process isn't an earlier run");
        // Another process of another program (this PC's System process can't be opened;
        // explorer or anything else runs under its own program).
        fs::write(&owner, format!("4\r\n{}\r\n", own_exe().unwrap())).unwrap();
        assert!(!leaving(&owner), "not the same program");
        fs::write(&owner, format!("{}\r\nC:\\elsewhere\\game.exe\r\n", u32::MAX - 3)).unwrap();
        assert!(!leaving(&owner), "no such process");
        fs::write(&owner, "junk").unwrap();
        assert!(!leaving(&owner));
        let t = Instant::now();
        let second = Identity::claim(&dir);
        assert_eq!(second.slot, 1);
        assert!(t.elapsed() < Duration::from_secs(2), "a running copy isn't waited for");
        drop((me, second));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A slot held by the starter this game took it over from is waited for; one held
    /// by any other process, or a pid now another program's, isn't.
    #[test]
    fn a_slot_held_by_the_games_starter_is_waited_for() {
        use std::os::windows::process::CommandExt;
        let dir = std::env::temp_dir().join(format!("peerparty-id-starter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut other = std::process::Command::new("cmd")
            .args(["/c", "ping -n 4 127.0.0.1 >nul"])
            .stdout(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let pid = other.id();
        let exe = process_exe(pid).unwrap();
        let owner = dir.join("slot-0.owner");
        fs::write(&owner, format!("{pid}\r\n{exe}\r\n")).unwrap();
        assert!(!held_by_starter(&owner), "no starter named yet");
        take_slot_from(pid);
        assert!(held_by_starter(&owner));
        assert!(!leaving(&owner), "another program, so only as the starter");
        fs::write(&owner, format!("{pid}\r\nC:\\elsewhere\\game.exe\r\n")).unwrap();
        assert!(!held_by_starter(&owner), "the pid is another program's now");
        take_slot_from(0);
        let _ = other.kill();
        let _ = other.wait();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Run by the next test in a process of its own: holds slot 0 a moment, windowless,
    /// as a game that is quitting does.
    #[test]
    #[ignore]
    fn slot_holder() {
        let Some(dir) = std::env::var_os("PPTEST_HOLD_SLOT") else { return };
        let me = Identity::claim(Path::new(&dir));
        assert_eq!(me.slot, 0);
        std::thread::sleep(Duration::from_millis(1500));
        drop(me);
    }

    /// An earlier run of the same program still holding the slot while it quits is
    /// waited for: the new run is the same player, not the next slot's.
    #[test]
    fn a_quitting_earlier_run_is_waited_for() {
        use std::os::windows::process::CommandExt;
        let dir = std::env::temp_dir().join(format!("peerparty-id-quitting-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "id::tests::slot_holder", "--test-threads", "1"])
            .env("PPTEST_HOLD_SLOT", &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let owner = dir.join("slot-0.owner");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fs::read_to_string(&owner).is_ok_and(|t| t.starts_with(&child.id().to_string())) {
            assert!(Instant::now() < deadline, "the holder never took the slot");
            std::thread::sleep(Duration::from_millis(20));
        }
        let t = Instant::now();
        let me = Identity::claim(&dir);
        assert_eq!(me.slot, 0, "waited for the quitting run");
        assert!(t.elapsed() >= Duration::from_millis(500), "{:?}", t.elapsed());
        assert!(child.wait().unwrap().success());
        drop(me);
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
