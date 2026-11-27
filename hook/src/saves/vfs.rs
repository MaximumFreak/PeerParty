//! A later player's own saves (players 2–4), and where player 1's are.
//!
//! In a later player's copy, a path the game uses that falls in a save zone
//! (protocol/src/saves.rs `classify`) is sent to the same path in the player's own folder,
//! `<data>\saves\<game>\player-N\fs\<Zone>\...`, never beside player 1's:
//!
//! - **Windows' data folders, Steam's userdata, folders the player named**: the player's
//!   folder only, nothing read through from player 1's. A folder the game writes into is
//!   made when the game first writes there (a game takes `persistentDataPath` to exist
//!   because player 1's does). With "Start new players from Player 1's saves" (`SAVE_SEED`)
//!   a game's folder (`seed_root`) the player has none of yet starts as a copy of player
//!   1's, the first time the game touches it (the launcher copies the ones it knows of
//!   before the game starts; this is for the rest).
//! - **The game's own folder**: read through. The player reads the game's files there
//!   until it writes one: then the file is copied into its folder first and the write
//!   goes there (and every read after). A file it deletes is noted (`whiteouts.txt`) and
//!   is gone for it, still there for player 1. Folders named for saves (`Saves`,
//!   `Profile`, `SteamData`...) aren't read through: the player's own or nothing. The
//!   game's files are never written.
//!
//! In player 1's copy with `SAVE_WATCH`, nothing is redirected: the zone folders the game
//! writes to are noted in `player-1\discovered.txt`, for the launcher to copy them to
//! other players.
//!
//! An older PeerParty kept a later player's save beside player 1's (`<save>.peerparty-N`):
//! the first time a game's folder is used here with none of its own yet, such a folder
//! is copied in, so that progress carries over.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use peerparty_protocol::saves::{self as layout, Discovered, Roots, Zone};

use crate::log;

/// What the game is doing with a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Reading or looking: opening to read, attributes, a listing.
    Read,
    /// Writing what's there (opened for writing, attributes set): in the game's folder,
    /// the file is copied in first.
    Write,
    /// Making it anew (created, truncated, a copy's or move's destination).
    Create,
    /// Making a folder.
    Mkdir,
    /// Deleting a file.
    Delete,
    /// Removing a folder.
    RemoveDir,
    /// The source of a move: written (copied in first), then gone (`gone`).
    MoveFrom,
}

impl Intent {
    fn writes(self) -> bool {
        !matches!(self, Intent::Read)
    }
}

/// Where the game's call goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// The path as the game gave it.
    Same,
    /// This path instead (full).
    To(String),
    /// Nothing to do, and the call is answered done (a delete of a file in the game's
    /// folder the player never had: it's gone for the player, player 1 keeps it).
    Done,
}

/// A copy's place: which player, and its folders.
pub struct State {
    /// 0: player 1 (watching only); 1 and up: a later player's own.
    pub slot: u32,
    pub roots: Roots,
    pub game: String,
    /// `player-N`, and its `fs`.
    pub player: PathBuf,
    pub fs: PathBuf,
    /// Player 1's folder (`discovered.txt`).
    pub first: PathBuf,
    pub seed: bool,
}

static STATE: RwLock<Option<Arc<State>>> = RwLock::new(None);
/// Seed roots handled this run (lowercase `zone\rel`).
static SEEDED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
/// Paths in the game's folder (lowercase rel) the player has a file or folder of its own
/// for, and the ones it deleted.
static OWN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
static WHITEOUTS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
/// Player 1's lines noted this run (`discovered.txt` holds them all).
static NOTED: LazyLock<Mutex<HashSet<Discovered>>> = LazyLock::new(Mutex::default);
static LISTING_GAP: AtomicBool = AtomicBool::new(false);
/// A file in the game's folder bigger than this isn't copied in on a write (a game
/// opening its archives for writing): the player shares the game's.
const COPY_UP_MAX: u64 = 256 << 20;

/// The copy's state, once `start` ran.
pub fn state() -> Option<Arc<State>> {
    STATE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// This game's id for its saves: the launcher's, else one from the exe's path.
pub fn game_id() -> String {
    crate::config::HookConfig::global().save_game.clone().unwrap_or_else(|| {
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        layout::game_id(&exe)
    })
}

/// The real folders the zones start at, in this process.
pub fn roots() -> Roots {
    let c = crate::config::HookConfig::global();
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let profile = env("USERPROFILE");
    let under_profile = |rest: &str| profile.as_ref().map(|p| format!("{}\\{rest}", p.trim_end_matches('\\')));
    let mut shared: Vec<String> = ["TEMP", "TMP"].iter().filter_map(|k| env(k)).collect();
    shared.extend(crate::util::local_store::peerparty_dir().map(|d| d.to_string_lossy().into_owned()));
    shared.extend([&c.galaxy_dir, &c.playfab_dir, &c.ugs_dir, &c.demonware_dir, &c.log_dir].into_iter().flatten().map(|d| d.to_string_lossy().into_owned()));
    shared.extend(c.shared_save_paths.iter().cloned());
    let game_dir = c.game_root.clone().or_else(|| std::env::current_exe().ok()?.parent().map(|p| p.to_string_lossy().into_owned()));
    Roots {
        local_low: shell_folder("{A520A1A4-1780-4FF6-BD18-167343C5AF16}").or_else(|| under_profile(r"AppData\LocalLow")),
        local: env("LOCALAPPDATA").or_else(|| shell_folder("Local AppData")),
        roaming: env("APPDATA").or_else(|| shell_folder("AppData")),
        documents: shell_folder("Personal").or_else(|| under_profile("Documents")),
        saved_games: shell_folder("{4C5C32FF-BB9D-43B0-B5B4-2D72E54EAAA4}").or_else(|| under_profile("Saved Games")),
        game_dir,
        extra: c.save_paths.clone(),
        shared,
    }
}

/// One of the user's folders as Explorer has it (`User Shell Folders`, expanded): the
/// Documents folder may be moved (to another drive, a synced folder) and still be the one
/// games use.
fn shell_folder(name: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW};
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let key = w(r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders");
    let value = w(name);
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) };
    if rc != 0 {
        return None;
    }
    let n = (len as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]).trim_end_matches('\0').to_string();
    (!s.is_empty() && !s.contains('%')).then_some(s)
}

/// Sets this copy up as `slot`'s (0: player 1, watching when asked). Again with another
/// slot (the LAN gave it another), the folders follow.
pub fn start(slot: u32) {
    let _busy = Busy::enter();
    let Some(data) = crate::util::local_store::peerparty_dir() else {
        log!("saves: no folder for PeerParty's data; player {}'s saves aren't kept apart", slot + 1);
        return;
    };
    let c = crate::config::HookConfig::global();
    let game = game_id();
    let roots = roots();
    let st = State {
        slot,
        player: layout::player_dir(&data, &game, slot),
        fs: layout::fs_dir(&data, &game, slot),
        first: layout::player_dir(&data, &game, 0),
        roots,
        game,
        seed: c.save_seed || c.copy_saves,
    };
    let show = |o: &Option<String>| o.clone().unwrap_or_else(|| "unknown".into());
    if slot == 0 {
        log!("saves: player 1 keeps the real folders; where the game writes its saves is noted in {}", st.first.join(layout::DISCOVERED).display());
    } else {
        log!(
            "saves: player {}'s saves are its own, in {} (game {}): LocalLow {}, Local {}, Roaming {}, Documents {}, Saved Games {}; the game's folder {} is read through; {}",
            slot + 1,
            st.fs.display(),
            st.game,
            show(&st.roots.local_low),
            show(&st.roots.local),
            show(&st.roots.roaming),
            show(&st.roots.documents),
            show(&st.roots.saved_games),
            show(&st.roots.game_dir),
            if st.seed { "a game folder it has none of yet starts as a copy of player 1's" } else { "it starts with none of player 1's (fresh)" }
        );
        if !st.roots.extra.is_empty() {
            log!("saves: player {}'s folders named for the game: {}", slot + 1, st.roots.extra.join("; "));
        }
        load_game_dir_files(&st);
    }
    *STATE.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(st));
}

/// What the player has in the game's folder already, and what it deleted there.
fn load_game_dir_files(st: &State) {
    let base = st.fs.join(Zone::GameDir.dir_name());
    let mut own = crate::sys::lock(&OWN);
    own.clear();
    fn walk(base: &Path, p: &Path, own: &mut HashSet<String>, left: &mut usize) {
        for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
            if *left == 0 {
                return;
            }
            *left -= 1;
            let path = e.path();
            if let Ok(rel) = path.strip_prefix(base) {
                own.insert(rel.to_string_lossy().to_lowercase());
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(base, &path, own, left);
            }
        }
    }
    walk(&base, &base, &mut own, &mut 20_000);
    let mut w = crate::sys::lock(&WHITEOUTS);
    w.clear();
    if let Ok(text) = std::fs::read_to_string(st.player.join(layout::WHITEOUTS)) {
        w.extend(text.lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).take(20_000));
    }
    if !own.is_empty() || !w.is_empty() {
        log!("saves: player {} has {} files of its own in the game's folder, and deleted {} there", st.slot + 1, own.len(), w.len());
    }
}

fn save_whiteouts(st: &State) {
    let text: String = {
        let w = crate::sys::lock(&WHITEOUTS);
        let mut v: Vec<&String> = w.iter().collect();
        v.sort();
        v.into_iter().map(|l| format!("{l}\r\n")).collect()
    };
    let path = st.player.join(layout::WHITEOUTS);
    if let Err(e) = std::fs::create_dir_all(&st.player).and_then(|()| super::write_whole(&path, text.as_bytes())) {
        log!("saves: {} couldn't be written ({e})", path.display());
    }
}

/// The full path of `path` (relative to the current folder, `..` resolved), any length.
pub fn full_path(path: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;
    let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe { GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(String::from_utf16_lossy(&buf[..n]));
        }
        if n > 1 << 16 {
            return None;
        }
        buf = vec![0u16; n + 1];
    }
}

/// A sandbox path as the game gets it: long ones with the Win32 long-path prefix, so a
/// path that fit for player 1 still opens for player 2 under a deeper folder.
fn as_game_path(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if s.len() >= 248 && !s.starts_with(r"\\") { format!(r"\\?\{s}") } else { s }
}

fn ensure_parent(p: &Path) {
    if let Some(parent) = p.parent() {
        if !parent.is_dir() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log!("saves: {} couldn't be made ({e})", parent.display());
            }
        }
    }
}

/// Marks this thread's file calls as PeerParty's own while it lives (saves/mod.rs
/// `BUSY`): the copies and folders made here pass straight through the hooks.
struct Busy(bool);

impl Busy {
    fn enter() -> Busy {
        Busy(super::BUSY.with(|b| b.replace(true)))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let was = self.0;
        super::BUSY.with(|b| b.set(was));
    }
}

/// Where the game's call with `path` goes (module comment). The file calls made here
/// pass straight through (`Busy`).
pub fn route(path: &str, intent: Intent) -> Route {
    let Some(st) = state() else { return Route::Same };
    let _busy = Busy::enter();
    if st.slot == 0 {
        if intent.writes() {
            watch(&st, path);
        }
        return Route::Same;
    }
    let Some(full) = full_path(path) else { return Route::Same };
    let Some((zone, rel)) = layout::classify(&st.roots, &full) else { return Route::Same };
    let sandbox = layout::sandbox_path(&st.fs, zone, &rel);
    if zone != Zone::GameDir {
        first_use(&st, zone, &rel, &full);
        if matches!(intent, Intent::Write | Intent::Create | Intent::Mkdir | Intent::MoveFrom) {
            ensure_parent(&sandbox);
        }
        return Route::To(as_game_path(&sandbox));
    }
    game_dir_route(&st, &rel, &full, &sandbox, intent)
}

/// The game's own folder: read through until written (module comment).
fn game_dir_route(st: &State, rel: &str, real: &str, sandbox: &Path, intent: Intent) -> Route {
    let key = rel.to_lowercase();
    let strict = layout::strict_in_game_dir(rel) || crate::sys::lock(&NOTED_GAME_DIR).contains(&key);
    let own = crate::sys::lock(&OWN).contains(&key);
    let deleted = crate::sys::lock(&WHITEOUTS).contains(&key);
    let to = || Route::To(as_game_path(sandbox));
    let mine = |st: &State| {
        crate::sys::lock(&OWN).insert(key.clone());
        if crate::sys::lock(&WHITEOUTS).remove(&key) {
            save_whiteouts(st);
        }
    };
    match intent {
        Intent::Read => {
            if own || deleted || strict {
                to()
            } else {
                Route::Same
            }
        }
        Intent::Write | Intent::MoveFrom => {
            if own || strict || deleted {
                ensure_parent(sandbox);
                mine(st);
                return to();
            }
            match std::fs::metadata(real) {
                Ok(m) if m.is_dir() => {
                    let _ = std::fs::create_dir_all(sandbox);
                    mine(st);
                    to()
                }
                Ok(m) if m.len() > COPY_UP_MAX => {
                    crate::log::throttled("saves big game file", 8, |_| {
                        log!("gap: saves: {real} is {} MB, too big to copy for player {}: it writes the game's own", m.len() >> 20, st.slot + 1)
                    });
                    Route::Same
                }
                Ok(_) => {
                    ensure_parent(sandbox);
                    match std::fs::copy(real, sandbox) {
                        Ok(_) => {
                            log!("saves: player {} writes {real}: copied into its own folder first ({})", st.slot + 1, sandbox.display());
                            mine(st);
                            to()
                        }
                        Err(e) => {
                            log!("gap: saves: {real} couldn't be copied for player {} ({e}); it starts as a new file there", st.slot + 1);
                            mine(st);
                            to()
                        }
                    }
                }
                Err(_) => {
                    ensure_parent(sandbox);
                    mine(st);
                    to()
                }
            }
        }
        Intent::Create | Intent::Mkdir => {
            ensure_parent(sandbox);
            mine(st);
            to()
        }
        Intent::Delete | Intent::RemoveDir => {
            let there = Path::new(real).exists();
            if own {
                crate::sys::lock(&OWN).remove(&key);
            }
            if there && !strict {
                if crate::sys::lock(&WHITEOUTS).insert(key.clone()) {
                    log!("saves: player {} deleted {real}: gone for it, kept for player 1", st.slot + 1);
                    save_whiteouts(st);
                }
                if !own {
                    return Route::Done;
                }
            }
            to()
        }
    }
}

/// A move's source is gone once the move is done: in the game's folder, the game's file
/// is now gone for the player too.
pub fn gone(path: &str) {
    let _busy = Busy::enter();
    let Some(st) = state().filter(|s| s.slot != 0) else { return };
    let Some(full) = full_path(path) else { return };
    if let Some((Zone::GameDir, rel)) = layout::classify(&st.roots, &full) {
        let key = rel.to_lowercase();
        crate::sys::lock(&OWN).remove(&key);
        if Path::new(&full).exists() && crate::sys::lock(&WHITEOUTS).insert(key) {
            save_whiteouts(&st);
        }
    }
}

/// Folders in the game's folder player 1 wrote saves into (from `discovered.txt`): a
/// later player doesn't read through to them.
static NOTED_GAME_DIR: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// The first time a game's folder (`seed_root`) is used this run: an older PeerParty's
/// twin of it is copied in, or (`seed`) player 1's, if the player has none of its own.
fn first_use(st: &State, zone: Zone, rel: &str, full: &str) {
    let Some(seed) = layout::seed_root(zone, rel) else { return };
    let key = format!("{}\\{}", zone.dir_name(), seed).to_lowercase();
    if crate::sys::lock(&SEEDED).contains(&key) {
        return;
    }
    let _copying = crate::sys::lock(&super::COPYING);
    if !crate::sys::lock(&SEEDED).insert(key) {
        return;
    }
    let real_root = real_root(&layout::normalize(full), rel, &seed);
    let mine = layout::sandbox_path(&st.fs, zone, &seed);
    if mine.exists() {
        log!("saves: player {}: {} is its own already ({})", st.slot + 1, real_root.as_deref().unwrap_or(&seed), mine.display());
        return;
    }
    let Some(real_root) = real_root else { return };
    if let Some(old) = legacy_twin(&real_root, st.slot) {
        match super::copy_fresh(&old, &mine) {
            Ok(n) => log!("saves: player {}: an older PeerParty's {} is now its own ({n} files copied to {})", st.slot + 1, old.display(), mine.display()),
            Err(e) => log!("saves: player {}: copying an older PeerParty's {} failed ({e})", st.slot + 1, old.display()),
        }
        return;
    }
    if !st.seed {
        log!("saves: player {}: {real_root} starts empty for it ({})", st.slot + 1, mine.display());
        return;
    }
    let from = PathBuf::from(&real_root);
    if !from.exists() {
        log!("saves: player {}: player 1 has no {real_root} to start from", st.slot + 1);
        return;
    }
    if super::size_of_tree(&from, super::MAX_COPY) > super::MAX_COPY {
        log!("saves: player {}: player 1's {real_root} is over {} MB; player {} starts without it", st.slot + 1, super::MAX_COPY >> 20, st.slot + 1);
        return;
    }
    match super::copy_fresh(&from, &mine) {
        Ok(n) => log!("saves: player {}: {real_root} starts as a copy of player 1's ({n} files copied to {})", st.slot + 1, mine.display()),
        Err(e) => log!("saves: player {}: copying player 1's {real_root} failed ({e}); it starts empty", st.slot + 1),
    }
}

/// The real seed root: the path the game gave (`norm`, normalized), up to the end of
/// `seed`, `rel`'s first folders. `rel` comes without trailing backslashes (`classify`)
/// and the game's path may have them (a folder named as `...\Game\`): cut where `rel`
/// starts without them, or the cut lands that many characters late (`...\Local\GGame`).
fn real_root(norm: &str, rel: &str, seed: &str) -> Option<String> {
    let at = norm.trim_end_matches('\\').len().checked_sub(rel.len())?;
    Some(format!("{}{seed}", norm.get(..at)?))
}

/// An older PeerParty's twin of `real` for `slot`: `<real>.peerparty-<N>`.
fn legacy_twin(real: &str, slot: u32) -> Option<PathBuf> {
    let mut name = std::ffi::OsString::from(real);
    name.push(format!(".peerparty-{}", slot + 1));
    let p = PathBuf::from(name);
    p.is_dir().then_some(p)
}

/// Player 1 wrote `path`: notes the game's folder for it in `discovered.txt`.
fn watch(st: &State, path: &str) {
    let Some(full) = full_path(path) else { return };
    let Some((zone, rel)) = layout::classify(&st.roots, &full) else { return };
    let norm = layout::normalize(&full);
    let line = if zone == Zone::GameDir {
        // Logs and caches aren't saves: noted, the save manager would list a mod
        // loader's log as player 1's saves and open its folder.
        if layout::noise_in_game_dir(&rel) {
            crate::log::throttled("saves noise", 4, |_| log!("saves: player 1 wrote {norm} in the game's folder: a log or cache, not noted as saves"));
            return;
        }
        // A file the game wrote in its folder, as it is (a whole folder there could be
        // the game's data).
        Discovered::Files { zone, rel: rel.clone(), real: norm }
    } else {
        let Some(seed) = layout::seed_root(zone, &rel) else { return };
        let real = real_root(&norm, &rel, &seed).unwrap_or_default();
        Discovered::Files { zone, rel: seed, real }
    };
    note(st, line);
}

/// Notes one line in player 1's `discovered.txt`, once.
pub fn note(st: &State, line: Discovered) {
    let _busy = Busy::enter();
    {
        let mut n = crate::sys::lock(&NOTED);
        if n.contains(&line) || n.len() >= layout::MAX_DISCOVERED {
            return;
        }
        n.insert(line.clone());
    }
    let path = st.first.join(layout::DISCOVERED);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut all = Discovered::parse_all(&text);
    if all.contains(&line) {
        return;
    }
    if all.len() >= layout::MAX_DISCOVERED {
        return;
    }
    all.push(line.clone());
    let out: String = all.iter().map(|d| d.to_line() + "\r\n").collect();
    match std::fs::create_dir_all(&st.first).and_then(|()| super::write_whole(&path, out.as_bytes())) {
        Ok(()) => log!("saves: player 1's saves seen at {}", line.to_line().replace('\t', " ")),
        Err(e) => log!("saves: {} couldn't be written ({e})", path.display()),
    }
}

/// Registry keys player 1's game was seen writing (`Software\<vendor>\<product>`,
/// lowercase): a later player never reads them through (registry.rs).
static NOTED_REGISTRY: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// Player 1's notes, read once by a later player (its game-folder saves and the keys its
/// game writes aren't read through).
pub fn load_noted() {
    let Some(st) = state() else { return };
    let text = std::fs::read_to_string(st.first.join(layout::DISCOVERED)).unwrap_or_default();
    let mut g = crate::sys::lock(&NOTED_GAME_DIR);
    let mut r = crate::sys::lock(&NOTED_REGISTRY);
    for d in Discovered::parse_all(&text) {
        match d {
            Discovered::Files { zone: Zone::GameDir, rel, .. } => {
                g.insert(rel.to_lowercase());
            }
            Discovered::Registry(k) => {
                r.insert(k.to_lowercase());
            }
            _ => {}
        }
    }
}

/// The registry keys player 1's game was seen writing (`software\<vendor>\<product>`).
pub fn noted_registry() -> Vec<String> {
    crate::sys::lock(&NOTED_REGISTRY).iter().cloned().collect()
}

/// Whether player 1's game was seen writing registry key `path` (`Software\...`) or one
/// above or below it.
pub fn written_by_first(path: &str) -> bool {
    let p = path.trim_matches('\\').to_lowercase();
    crate::sys::lock(&NOTED_REGISTRY).iter().any(|k| p == *k || p.starts_with(&format!("{k}\\")) || k.starts_with(&format!("{p}\\")))
}

/// A listing in the game's folder shows the game's files, not a later player's own
/// there (said once).
pub fn listing_gap(path: &str) {
    if LISTING_GAP.swap(true, Ordering::Relaxed) {
        return;
    }
    log!("gap: saves: a listing of {path} in the game's folder shows the game's files; files player 2+ made there are left out of it");
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A folder the game names with a trailing backslash has the same real root as one
    /// named without: player 1's saves are copied from where they are.
    #[test]
    fn a_seed_root_named_with_a_trailing_backslash_is_found() {
        let (rel, seed) = ("Some Game", "Some Game");
        assert_eq!(real_root(r"C:\Users\p\AppData\Local\Some Game", rel, seed).as_deref(), Some(r"C:\Users\p\AppData\Local\Some Game"));
        assert_eq!(real_root(r"C:\Users\p\AppData\Local\Some Game\", rel, seed).as_deref(), Some(r"C:\Users\p\AppData\Local\Some Game"));
        assert_eq!(real_root(r"C:\Users\p\AppData\Local\Some Game\\", rel, seed).as_deref(), Some(r"C:\Users\p\AppData\Local\Some Game"));
        assert_eq!(
            real_root(r"C:\Users\p\AppData\Local\Some Game\Saves\slot1.sav", r"Some Game\Saves\slot1.sav", seed).as_deref(),
            Some(r"C:\Users\p\AppData\Local\Some Game")
        );
        assert_eq!(real_root("short", "a longer rel", seed), None);
    }

    /// A later player's state over folders of a test's own.
    pub fn test_state(base: &Path, slot: u32, seed: bool) -> State {
        let r = |n: &str| Some(base.join("real").join(n).to_string_lossy().into_owned());
        let data = base.join("data");
        State {
            slot,
            roots: Roots {
                local_low: r("LocalLow"),
                local: r("Local"),
                roaming: r("Roaming"),
                documents: r("Documents"),
                saved_games: r("Saved Games"),
                game_dir: r(r"Games\Test Game"),
                extra: Vec::new(),
                shared: vec![data.to_string_lossy().into_owned()],
            },
            game: "test-game-00000000".into(),
            player: layout::player_dir(&data, "test-game-00000000", slot),
            fs: layout::fs_dir(&data, "test-game-00000000", slot),
            first: layout::player_dir(&data, "test-game-00000000", 0),
            seed,
        }
    }

    /// Serializes tests that set the process's save state.
    pub static TEST_LOCK: Mutex<()> = Mutex::new(());

    pub fn install(st: State) {
        crate::sys::lock(&SEEDED).clear();
        crate::sys::lock(&OWN).clear();
        crate::sys::lock(&WHITEOUTS).clear();
        crate::sys::lock(&NOTED).clear();
        crate::sys::lock(&NOTED_GAME_DIR).clear();
        crate::sys::lock(&NOTED_REGISTRY).clear();
        *STATE.write().unwrap() = Some(Arc::new(st));
    }

    pub fn uninstall() {
        *STATE.write().unwrap() = None;
    }

    fn base(name: &str) -> PathBuf {
        let b = std::env::temp_dir().join(format!("peerparty-vfs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&b);
        b
    }

    fn to(r: Route) -> PathBuf {
        match r {
            Route::To(p) => PathBuf::from(p.strip_prefix(r"\\?\").unwrap_or(&p)),
            other => panic!("{other:?}"),
        }
    }

    /// Windows' data folders: the player's own only, made when it first writes there;
    /// player 1's are never read or written.
    #[test]
    fn data_folders_are_the_players_own() {
        let _g = crate::sys::lock(&TEST_LOCK);
        // The test's own file calls see the disk as it is (the hooks may be in).
        let _quiet = Busy::enter();
        let b = base("zones");
        let real = b.join(r"real\LocalLow\Co\Game");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("data_full_-99.data"), "player 1").unwrap();
        install(test_state(&b, 1, false));
        let p = real.join("data_full_-99.data").to_string_lossy().into_owned();
        let read = to(route(&p, Intent::Read));
        assert_eq!(read, b.join(r"data\saves\test-game-00000000\player-2\fs\LocalLow\Co\Game\data_full_-99.data"));
        assert!(!read.exists(), "nothing of player 1's read through");
        assert!(!read.parent().unwrap().exists(), "a read makes no folder");
        let write = to(route(&p, Intent::Create));
        assert!(write.parent().unwrap().is_dir(), "a write's folder is made");
        std::fs::write(&write, "player 2").unwrap();
        assert_eq!(std::fs::read_to_string(real.join("data_full_-99.data")).unwrap(), "player 1");
        // Shared folders and the zone's root pass through.
        assert_eq!(route(&b.join(r"real\LocalLow").to_string_lossy(), Intent::Read), Route::Same);
        assert_eq!(route(&b.join(r"real\Local\NVIDIA\x").to_string_lossy(), Intent::Create), Route::Same);
        assert_eq!(route(&b.join(r"data\saves\x").to_string_lossy(), Intent::Create), Route::Same, "its own folder isn't redirected again");
        uninstall();
        let _ = std::fs::remove_dir_all(&b);
    }

    /// Seeding: a game folder the player has none of starts as a copy of player 1's,
    /// once; one it has stays; an older PeerParty's twin is taken over.
    #[test]
    fn a_new_players_folder_starts_from_player_1s_when_asked() {
        let _g = crate::sys::lock(&TEST_LOCK);
        // The test's own file calls see the disk as it is (the hooks may be in).
        let _quiet = Busy::enter();
        let b = base("seed");
        let real = b.join(r"real\LocalLow\Co\Game");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("save.dat"), "player 1").unwrap();
        install(test_state(&b, 1, true));
        let got = to(route(&real.join("save.dat").to_string_lossy(), Intent::Read));
        assert_eq!(std::fs::read_to_string(&got).unwrap(), "player 1");
        std::fs::write(&got, "player 2").unwrap();
        install(test_state(&b, 1, true));
        let again = to(route(&real.join("save.dat").to_string_lossy(), Intent::Read));
        assert_eq!(std::fs::read_to_string(again).unwrap(), "player 2", "its own stays");
        // An older build's twin beside player 1's save.
        let docs = b.join(r"real\Documents\My Games\Foo");
        std::fs::create_dir_all(&docs).unwrap();
        let twin = b.join(r"real\Documents\My Games\Foo.peerparty-3");
        std::fs::create_dir_all(&twin).unwrap();
        std::fs::write(twin.join("slot0.sav"), "old player 3").unwrap();
        install(test_state(&b, 2, false));
        let got = to(route(&docs.join("slot0.sav").to_string_lossy(), Intent::Read));
        assert_eq!(std::fs::read_to_string(got).unwrap(), "old player 3");
        uninstall();
        let _ = std::fs::remove_dir_all(&b);
    }

    /// The game's folder: read through until written; a write copies the file in first,
    /// a delete hides it; folders of saves aren't read through. The game's files stay.
    #[test]
    fn the_games_folder_is_copy_on_write() {
        let _g = crate::sys::lock(&TEST_LOCK);
        // The test's own file calls see the disk as it is (the hooks may be in).
        let _quiet = Busy::enter();
        let b = base("gamedir");
        let game = b.join(r"real\Games\Test Game");
        std::fs::create_dir_all(game.join("SteamData")).unwrap();
        std::fs::create_dir_all(game.join("Saves")).unwrap();
        std::fs::write(game.join("data.pak"), "asset").unwrap();
        std::fs::write(game.join("config.ini"), "p1 config").unwrap();
        std::fs::write(game.join(r"SteamData\user_stats.ini"), "p1 stats").unwrap();
        std::fs::write(game.join(r"Saves\slot1.dat"), "p1 save").unwrap();
        install(test_state(&b, 1, false));
        let s = |p: &Path| p.to_string_lossy().into_owned();
        assert_eq!(route(&s(&game.join("data.pak")), Intent::Read), Route::Same, "assets read through");
        let cfg = to(route(&s(&game.join("config.ini")), Intent::Write));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "p1 config", "copied in first");
        std::fs::write(&cfg, "p2 config").unwrap();
        assert_eq!(to(route(&s(&game.join("config.ini")), Intent::Read)), cfg, "read from its own now");
        assert_eq!(std::fs::read_to_string(game.join("config.ini")).unwrap(), "p1 config");
        // Saves folders: its own or nothing.
        let slot = to(route(&s(&game.join(r"Saves\slot1.dat")), Intent::Read));
        assert!(!slot.exists());
        let stats = to(route(&s(&game.join(r"SteamData\user_stats.ini")), Intent::Write));
        assert!(!stats.exists(), "not copied from player 1's");
        // Delete: gone for player 2, kept for player 1, and remembered.
        assert_eq!(route(&s(&game.join("data.pak")), Intent::Delete), Route::Done);
        assert!(game.join("data.pak").exists());
        assert!(!to(route(&s(&game.join("data.pak")), Intent::Read)).exists());
        assert!(std::fs::read_to_string(b.join(r"data\saves\test-game-00000000\player-2\whiteouts.txt")).unwrap().contains("data.pak"));
        install(test_state(&b, 1, false));
        load_game_dir_files(&state().unwrap());
        assert!(!to(route(&s(&game.join("data.pak")), Intent::Read)).exists(), "still gone next run");
        assert_eq!(to(route(&s(&game.join("config.ini")), Intent::Read)), cfg, "its own still found next run");
        // Made anew: the whiteout goes.
        let again = to(route(&s(&game.join("data.pak")), Intent::Create));
        std::fs::write(&again, "p2 pak").unwrap();
        assert_eq!(to(route(&s(&game.join("data.pak")), Intent::Read)), again);
        assert_eq!(std::fs::read_to_string(game.join("data.pak")).unwrap(), "asset");
        uninstall();
        let _ = std::fs::remove_dir_all(&b);
    }

    /// Player 1 is never redirected; with watching on, where it writes is noted.
    #[test]
    fn player_1_is_watched_not_moved() {
        let _g = crate::sys::lock(&TEST_LOCK);
        // The test's own file calls see the disk as it is (the hooks may be in).
        let _quiet = Busy::enter();
        let b = base("watch");
        let real = b.join(r"real\LocalLow\Co\Game");
        std::fs::create_dir_all(&real).unwrap();
        install(test_state(&b, 0, false));
        let p = real.join("save.dat").to_string_lossy().into_owned();
        assert_eq!(route(&p, Intent::Read), Route::Same);
        assert_eq!(route(&p, Intent::Create), Route::Same);
        // Trailing backslash on a directory create must not shift the cut past the zone root.
        let dir_slash = format!("{}\\", real.display());
        assert_eq!(route(&dir_slash, Intent::Create), Route::Same);
        assert_eq!(route(&b.join(r"real\Games\Test Game\Saves\a.sav").to_string_lossy(), Intent::Create), Route::Same);
        let text = std::fs::read_to_string(b.join(r"data\saves\test-game-00000000\player-1\discovered.txt")).unwrap();
        let seen = Discovered::parse_all(&text);
        assert!(seen.contains(&Discovered::Files { zone: Zone::LocalLow, rel: r"Co\Game".into(), real: real.to_string_lossy().into_owned() }), "{text}");
        assert_eq!(seen.iter().filter(|d| matches!(d, Discovered::Files { zone: Zone::LocalLow, .. })).count(), 1, "{text}");
        assert!(seen.iter().any(|d| matches!(d, Discovered::Files { zone: Zone::GameDir, rel, .. } if rel == r"Saves\a.sav")), "{text}");
        // A later player doesn't read through to player 1's noted game-folder saves.
        install(test_state(&b, 1, false));
        load_noted();
        let game = b.join(r"real\Games\Test Game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("progress.bin"), "p1").unwrap();
        assert_eq!(route(&game.join("progress.bin").to_string_lossy(), Intent::Read), Route::Same);
        uninstall();
        let _ = std::fs::remove_dir_all(&b);
    }
}
