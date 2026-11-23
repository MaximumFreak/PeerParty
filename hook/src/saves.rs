//! Starting a save from another id's, for games that key their saves by user id.
//! Also `file_name`, for the stores that keep files a game names (EOS, Galaxy).
//!
//! Three cases, one mechanism:
//!
//! - **`--copy-saves`**: a second copy of a game on one PC starts from the first copy's
//!   progress, for testing multiplayer that a fresh save hasn't unlocked yet. A second
//!   copy plays under a LAN identity (a LAN SteamID, EOS id and PlayFab id), so a game
//!   that keys its saves by user id looks in a folder or file named after the LAN id
//!   and finds nothing. The first copy's save is copied whole, every run asked for:
//!   what the second copy had there is moved aside first to `<save>.peerparty-before`
//!   (the run before's to `-2`, then `-3`; older ones go), never merged file by file,
//!   so the second copy can't end up with half of each. Only a run started with
//!   `--copy-saves` copies; the launcher's switch for it resets after each start.
//! - **A SteamID of its own for this PC** (steamlan's `own_id_on`): the game's saves keyed
//!   by SteamID or account id are new ones, under the new id. The first time the game
//!   looks for one, an existing save is *copied* to the new name, once ever: only while
//!   nothing exists under the new name, so the game's progress under PeerParty is its
//!   own from then on. The copy comes from the save under an earlier PeerParty SteamID
//!   for this PC, if there is one (ids used to be random; the core lists them in
//!   `slot-0.previous`), else from steam_api's SteamID. A PC that ran a build which
//!   deleted `slot-0.id` without listing it has no list: there the save's neighbours
//!   named after another PeerParty SteamID (account 2^31 and up) are candidates, except
//!   steam_api's and every slot's now and second copies' earlier ones; the one with the
//!   newest file is taken, each candidate and the choice logged (`guess_earlier`). The original is only read,
//!   never written, renamed or deleted, so a game that rejects the copy (a save that
//!   names its owner inside) can only lose the copy. `PEERPARTY_STEAM_SAVES=0` (the
//!   launcher's "Start from the game's existing saves" switched off) leaves the new
//!   saves empty.
//! - **Earlier ids of the first copy**: saves keyed by its EOS id or PlayFab id start,
//!   the same once-only way, from the ones under the ids it had before (`slot-0.previous`).
//!
//! How: the first time the game touches a path holding one of the ids as a whole path
//! component or a file name token delimited by anything but a letter or digit
//! (`<id>\slot1.sav`, `<id>_save.dat`), the same path with the source id is copied over
//! it, the whole folder or the file, wherever it lives. Saves that aren't keyed by id
//! are shared already and need nothing.
//!
//! kernelbase's file entry points are hooked (kernel32's forward there): CreateFileW/A,
//! CreateFile2, GetFileAttributesW/A/ExW/ExA, FindFirstFileExW/A and CreateDirectoryW/A
//! cover how .NET, Unity and native games open or probe a save. They're only armed when
//! needed: at kernelbase's load for `--copy-saves`, for a SteamID of its own once the
//! game has asked for its SteamID, and when the LAN comes up for a first copy with
//! earlier ids (so a game without Steam, or one that keeps its SteamID, on a PC that
//! never had random ids, has no file hooks). The hooks only look at the path; the
//! game's call goes on unchanged.
//!
//! **A second copy's own saves** (`learn_slot`): a game whose saves aren't keyed by any
//! id (one `savegame.dat` for whoever plays) has every copy on the PC reading and
//! writing the same file, and a second copy, playing under a LAN identity, can take the
//! first copy's save for someone else's and replace it (one game's save names its owner
//! inside and was recreated over the player's). So in a copy in a slot other than the
//! first, the save's folder (or file) gets a twin of the copy's own beside it, started
//! from a copy of the first's the first time: `Saves` -> `Saves.peerparty-2`,
//! `savegame.sav` -> `savegame.peerparty-2.sav`. The save is the deepest folder on the
//! path whose name holds "save", else a file so named (under Windows' own `Saved Games`,
//! the game's folder in it), never the game's own folder or one above it. The same hooks pass the game the
//! twin's path instead (and deleting, moving, replacing, copying and setting attributes
//! too, so a save written to a temporary file and renamed stays in the twin). The first
//! copy's files are only read, to make the twin. `PEERPARTY_SEPARATE_SAVES=0` turns it
//! off.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use crate::log;
use crate::sys::pe::Export;
use crate::sys::read;

/// `--copy-saves`.
pub fn enabled() -> bool {
    crate::config::HookConfig::global().copy_saves
}

/// Whether a game given a SteamID of its own starts from the saves under steam_api's
/// (`PEERPARTY_STEAM_SAVES`, on unless 0).
pub fn adopt_on() -> bool {
    crate::config::HookConfig::global().steam_saves != Some(false)
}

/// A larger save than this (a folder of replays, say) isn't copied: copying runs inside
/// the game's own file call.
const MAX_COPY: u64 = 1 << 30;

/// One id as it appears in paths, and where its save starts from.
#[derive(Clone, Debug, PartialEq)]
struct Pair {
    /// The id the game builds paths from now.
    ours: String,
    /// The id whose save it starts from.
    from: String,
    /// Copy only while nothing exists under `ours` (a SteamID of its own, earlier ids);
    /// otherwise (`--copy-saves`) the source replaces it whole, the one there kept as a
    /// backup (`replace`).
    once: bool,
    /// steam_api's own save, the last thing a SteamID of its own starts from: tried only
    /// once no earlier PeerParty SteamID's save is found (`guess_earlier`).
    fallback: bool,
}

/// A SteamID of its own for this PC (`adopt_steam`): the id, and the SteamIDs whose saves
/// are never taken for an earlier one of its own (steam_api's, this PC's slots' now).
static GUESS: Mutex<Option<(u64, Vec<u64>)>> = Mutex::new(None);

static PAIRS: Mutex<Vec<Pair>> = Mutex::new(Vec::new());
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Roots already handled this run (the side the game uses now).
static DONE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
/// Held while a save is looked at and copied: a second thread touching the same save
/// meanwhile (a loader thread opening the file the main thread's touch is copying)
/// waits for the copy to finish rather than opening it half written.
static COPYING: Mutex<()> = Mutex::new(());

thread_local! {
    /// Set while PeerParty itself is copying, so its own file calls pass straight through.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

fn add(ours: &str, from: &str, once: bool) -> bool {
    add_pair(ours, from, once, false)
}

fn add_pair(ours: &str, from: &str, once: bool, fallback: bool) -> bool {
    if ours.is_empty() || from.is_empty() || ours.eq_ignore_ascii_case(from) {
        return false;
    }
    let mut p = crate::sys::lock(&PAIRS);
    if p.iter().any(|q| q.ours.eq_ignore_ascii_case(ours) && q.from.eq_ignore_ascii_case(from)) {
        return false;
    }
    p.push(Pair { ours: ours.to_string(), from: from.to_string(), once, fallback });
    ACTIVE.store(true, Ordering::Relaxed);
    true
}

/// `--copy-saves`: paths containing `lan` get `main`'s copy first. Several mains for one
/// LAN id are tried in the order learned, the first that exists winning. Either way, a
/// path with `lan` in it is this copy's own already (`keyed_by_own_id`).
pub fn learn(lan: &str, main: &str) {
    own_id(lan);
    if enabled() && add(lan, main, false) {
        log!("saves: paths with {lan} start from {main}'s copy");
    }
}

/// Ids of this copy's own (its LAN ids), as they appear in paths.
static OWN_IDS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `id` is one of this copy's own: a save keyed by it is apart from the first copy's
/// without a twin.
fn own_id(id: &str) {
    let id = id.to_ascii_lowercase();
    let mut ids = crate::sys::lock(&OWN_IDS);
    if !id.is_empty() && !ids.contains(&id) && ids.len() < 16 {
        ids.push(id);
    }
}

/// Whether `full` names a save keyed by one of this copy's own ids.
fn keyed_by_own_id(full: &str) -> bool {
    crate::sys::lock(&OWN_IDS).iter().any(|id| find_token(full, id).is_some())
}

/// The game was shown SteamID `own` in place of steam_api's `game`: saves under its
/// SteamID or account id start, once, from the ones under an earlier SteamID of its own
/// for this PC (`earlier`, newest first) or else steam_api's (module docs).
pub fn adopt_steam(own: u64, game: u64, earlier: &[u64]) {
    if own == game || game == 0 {
        return;
    }
    if !adopt_on() {
        log!("saves: the game's saves under SteamID {own} start empty (PEERPARTY_STEAM_SAVES=0)");
        return;
    }
    let mut n = 0;
    for old in earlier.iter().filter(|o| **o != own && **o != game) {
        let mut m = 0;
        for (ours, from) in steam_names(own, *old) {
            m += usize::from(add(&ours, &from, true));
        }
        if m > 0 {
            log!("saves: saves under SteamID {own} start from a copy of {old}'s (account {}), PeerParty's earlier one for this PC, if it has any", *old as u32);
        }
        n += m;
    }
    for (ours, from) in steam_names(own, game) {
        n += usize::from(add_pair(&ours, &from, true, true));
    }
    // With no earlier SteamID listed (a PC whose slot-0.id was deleted before ids were
    // kept), one is looked for among the save's neighbours before steam_api's is taken.
    // A listed one is exact: then a game with no save under it never had one.
    if earlier.is_empty() {
        let mut never = vec![game];
        never.extend(other_slot_steam_ids());
        *crate::sys::lock(&GUESS) = Some((own, never));
    }
    if n > 0 {
        log!(
            "saves: saves under SteamID {own} (account {}) start from a copy of {game}'s (account {}) the first time; the originals are only read",
            own as u32,
            game as u32
        );
        arm_now();
    }
}

/// The LAN came up as the first copy: saves keyed by its EOS id or PlayFab id start,
/// once, from the ones under the ids the slot had before (module docs).
pub fn adopt_earlier_ids(lan: &peerparty_core::Lan) {
    if lan.slot() != 0 || lan.server() {
        return;
    }
    let earlier = peerparty_core::Identity::previous_ids(&peerparty_core::Identity::default_dir(), 0);
    let mut n = 0;
    for old in earlier.iter().filter(|o| **o != lan.me()) {
        n += usize::from(add(&lan.me().hex(), &old.hex(), true));
        n += usize::from(add(&crate::playfab::entity_of(lan.me()), &crate::playfab::entity_of(*old), true));
    }
    if n > 0 {
        log!("saves: saves under this copy's ids start from a copy of the ones under its earlier ids ({}) the first time", earlier.iter().map(|i| i.hex()).collect::<Vec<_>>().join(", "));
        // Called while the LAN is being set up: hooking takes the hook lock, which a
        // thread loading a module may hold while it waits for the LAN, so not from here.
        std::thread::spawn(arm_now);
    }
}

/// SteamIDs that are never the first copy's earlier one: every slot's now (from their
/// keys), and the other slots' earlier ones (second copies' saves).
fn other_slot_steam_ids() -> Vec<u64> {
    let dir = peerparty_core::Identity::default_dir();
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        let Some(rest) = name.strip_prefix("slot-") else { continue };
        if rest.ends_with(".ed25519") {
            if let Some(id) = std::fs::read_to_string(e.path()).ok().and_then(|t| id_of_key_file(&t)) {
                out.push(crate::steam::ids::lan_steam_id(id));
            }
        } else if let Some(slot) = rest.strip_suffix(".previous").and_then(|n| n.parse::<u32>().ok()).filter(|n| *n != 0) {
            out.extend(peerparty_core::Identity::previous_ids(&dir, slot).into_iter().map(crate::steam::ids::lan_steam_id));
        }
    }
    out
}

/// Where `token` sits in `name` as a whole token (nothing alphanumeric either side).
fn find_token(name: &str, token: &str) -> Option<usize> {
    let lower = name.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(token).map(|i| i + from) {
        let end = i + token.len();
        let edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
        if edge(lower[..i].chars().next_back()) && edge(lower[end..].chars().next()) {
            return Some(i);
        }
        from = end;
    }
    None
}

/// The newest modified time of any file under `p` (looking at no more than 10,000
/// entries); None for a folder without files.
fn newest_in(p: &Path) -> Option<std::time::SystemTime> {
    fn walk(p: &Path, seen: &mut usize, best: &mut Option<std::time::SystemTime>) {
        let Ok(meta) = std::fs::symlink_metadata(p) else { return };
        *seen += 1;
        if *seen > 10_000 {
            return;
        }
        if meta.is_dir() {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                walk(&e.path(), seen, best);
            }
        } else if let Ok(t) = meta.modified() {
            *best = Some(best.map_or(t, |b| b.max(t)));
        }
    }
    let mut best = None;
    walk(p, &mut 0, &mut best);
    best
}

/// The saves beside `root` (the save under SteamID `own`, by its SteamID or account id)
/// that may be an earlier PeerParty SteamID's: the same name with another PeerParty
/// SteamID in it (account at 2^31 and up, where no real one is: steamids.rs), none of
/// `never`'s. Each with its newest file's time; None when it holds no file.
fn earlier_candidates(root: &Path, own: u64, never: &[u64]) -> Vec<(PathBuf, Option<std::time::SystemTime>)> {
    let (Some(parent), Some(name)) = (root.parent(), root.file_name().and_then(|n| n.to_str())) else { return Vec::new() };
    let forms = [(own.to_string(), true), ((own as u32).to_string(), false)];
    let Some((token, wide, at)) = forms.iter().find_map(|(t, wide)| find_token(name, t).map(|at| (t.as_str(), *wide, at))) else { return Vec::new() };
    let (prefix, suffix) = (name[..at].to_ascii_lowercase(), name[at + token.len()..].to_ascii_lowercase());
    let mut out = Vec::new();
    for e in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().to_ascii_lowercase();
        if n.len() <= prefix.len() + suffix.len() || !n.starts_with(&prefix) || !n.ends_with(&suffix) {
            continue;
        }
        let mid = &n[prefix.len()..n.len() - suffix.len()];
        if !mid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(v) = mid.parse::<u64>() else { continue };
        let id = if wide {
            if v >> 32 != own >> 32 {
                continue;
            }
            v
        } else {
            match u32::try_from(v) {
                Ok(a) => (own & !0xffff_ffff) | a as u64,
                Err(_) => continue,
            }
        };
        if (id as u32) < 0x8000_0000 || id as u32 == own as u32 || never.iter().any(|x| *x as u32 == id as u32) {
            continue;
        }
        out.push((e.path(), newest_in(&e.path())));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The candidate whose newest file is the newest; one without files never wins.
fn pick_newest(candidates: &[(PathBuf, Option<std::time::SystemTime>)]) -> Option<PathBuf> {
    candidates.iter().filter_map(|(p, t)| t.map(|t| (p, t))).max_by_key(|(_, t)| *t).map(|(p, _)| p.clone())
}

/// For `root`, a SteamID of its own's save with no earlier SteamID listed: the save under
/// an earlier PeerParty SteamID beside it, if one can be told (module docs). Every
/// candidate is logged, and why the one taken was.
fn guess_earlier(root: &Path) -> Option<PathBuf> {
    let (own, never) = crate::sys::lock(&GUESS).clone()?;
    let candidates = earlier_candidates(root, own, &never);
    if candidates.is_empty() {
        return None;
    }
    let ago = |t: &Option<std::time::SystemTime>| match t {
        Some(t) => format!("newest file {} s old", t.elapsed().map_or(0, |d| d.as_secs())),
        None => "no files".to_string(),
    };
    for (p, t) in &candidates {
        log!("saves: earlier SteamID of this PC's? candidate {} ({})", p.display(), ago(t));
    }
    let chosen = pick_newest(&candidates);
    match &chosen {
        Some(p) => {
            let with_files = candidates.iter().filter(|c| c.1.is_some()).count();
            let why = if with_files == 1 { "the only one with files".to_string() } else { format!("the newest files of {with_files}") };
            log!("saves: {} taken as this PC's earlier SteamID's save for {}: {why}", p.display(), root.display());
        }
        None => log!("saves: no candidate for {} holds files; steam_api's save is next", root.display()),
    }
    chosen
}

/// The SteamIDs of its own this PC's first copy had before (its earlier peer ids').
pub fn earlier_own_steam_ids() -> Vec<u64> {
    peerparty_core::Identity::previous_ids(&peerparty_core::Identity::default_dir(), 0).into_iter().map(crate::steam::ids::lan_steam_id).collect()
}

/// How a SteamID shows up in a path: decimal, its account id, hex padded or not.
fn steam_names(own: u64, game: u64) -> Vec<(String, String)> {
    let mut v = vec![
        (own.to_string(), game.to_string()),
        (format!("{own:016x}"), format!("{game:016x}")),
        (format!("{own:x}"), format!("{game:x}")),
    ];
    if own as u32 != game as u32 {
        v.push(((own as u32).to_string(), (game as u32).to_string()));
    }
    v.dedup();
    v
}

/// The LAN came up: a copy in a slot other than the first maps its ids (EOS and
/// PlayFab, from its peer id) to the first slot's. A dedicated server keeps its own.
pub fn learn_lan(lan: &peerparty_core::Lan) {
    if lan.slot() == 0 || lan.slot() == u32::MAX || lan.server() {
        return;
    }
    own_id(&lan.me().hex());
    own_id(&crate::playfab::entity_of(lan.me()));
    if !enabled() {
        return;
    }
    let Some(main) = first_slot() else { return };
    learn(&lan.me().hex(), &main.hex());
    learn(&crate::playfab::entity_of(lan.me()), &crate::playfab::entity_of(main));
}

/// The first copy's LAN identity on this PC: the id of the first slot's signing key
/// (core's `Identity::claim`). Only read: a missing key isn't made here.
pub fn first_slot() -> Option<peerparty_core::PeerId> {
    let first = peerparty_core::Identity::default_dir().join("slot-0.ed25519");
    let id = std::fs::read_to_string(&first).ok().and_then(|s| id_of_key_file(&s));
    if id.is_none() {
        log!("saves: no first-slot identity at {}", first.display());
    }
    id
}

/// The peer id for a slot's key file, read the way the core reads it.
fn id_of_key_file(text: &str) -> Option<peerparty_core::PeerId> {
    let key = peerparty_core::inet::public::IdentityKey::from_text(text)?;
    Some(peerparty_core::PeerId::of_key(&key.public()))
}

/// Where `path`'s save comes from, in the order to try: (root, source, once, fallback),
/// the root being the path up to the end of the component holding the id.
fn sources_of(path: &str, pairs: &[Pair]) -> Vec<(String, String, bool, bool)> {
    let lower = path.to_ascii_lowercase();
    let mut out = Vec::new();
    for pair in pairs {
        let needle = pair.ours.to_ascii_lowercase();
        let mut from = 0;
        while let Some(i) = lower[from..].find(&needle).map(|i| i + from) {
            let end = i + needle.len();
            let before = lower[..i].chars().next_back();
            let after = lower[end..].chars().next();
            let edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
            if edge(before) && edge(after) {
                let next = |at: usize| path[at..].find(['\\', '/']).map_or(path.len(), |j| at + j);
                let mut stop = next(end);
                // Steam's folder for an account holds every game's files: under it, only
                // this game's folder (`userdata\<account>\<app id>`) is the save.
                if lower[..i].trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next() == Some("userdata") && stop < path.len() {
                    stop = next(stop + 1);
                }
                let root = path[..stop].to_string();
                let source = format!("{}{}{}", &path[..i], pair.from, &path[end..stop]);
                out.push((root, source, pair.once, pair.fallback));
                break;
            }
            from = end;
        }
    }
    out
}

/// Copies what is newer under `from` to `to`. A link or junction inside the save is
/// left out: following one would copy what `size_of_tree` didn't count (it doesn't
/// follow them), and one pointing back up would never end. `from` itself is followed.
fn copy_newer(from: &Path, to: &Path) -> std::io::Result<usize> {
    let meta = std::fs::metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        let mut n = 0;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                log!("saves: {} is a link; not copied", entry.path().display());
                continue;
            }
            n += copy_newer(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(n);
    }
    let newer = match std::fs::metadata(to) {
        Ok(dest) => match (meta.modified(), dest.modified()) {
            (Ok(a), Ok(b)) => a > b,
            _ => false,
        },
        Err(_) => true,
    };
    if !newer {
        return Ok(0);
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, to)?;
    Ok(1)
}

/// Bytes under `p`, stopping once past `cap`.
fn size_of_tree(p: &Path, cap: u64) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    for entry in std::fs::read_dir(p).into_iter().flatten().flatten() {
        total += size_of_tree(&entry.path(), cap.saturating_sub(total));
        if total > cap {
            break;
        }
    }
    total
}

/// Copies `from` to `to`, which doesn't exist: into a temporary name beside it first, so
/// a copy cut short (the game closed mid-way) never passes for a finished one.
fn copy_fresh(from: &Path, to: &Path) -> std::io::Result<usize> {
    let (tmp, n) = stage_copy(from, to)?;
    if let Err(e) = std::fs::rename(&tmp, to) {
        drop_staged(&tmp);
        return Err(e);
    }
    Ok(n)
}

/// All of `from` copied to `<to>.peerparty-copy`, for a rename onto `to` once it's
/// whole: (that path, the files copied).
fn stage_copy(from: &Path, to: &Path) -> std::io::Result<(PathBuf, usize)> {
    let mut tmp = to.as_os_str().to_owned();
    tmp.push(".peerparty-copy");
    let tmp = PathBuf::from(tmp);
    if tmp.is_dir() {
        std::fs::remove_dir_all(&tmp)?;
    } else if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    match copy_newer(from, &tmp) {
        Ok(n) => Ok((tmp, n)),
        Err(e) => {
            drop_staged(&tmp);
            Err(e)
        }
    }
}

fn drop_staged(tmp: &Path) {
    let _ = if tmp.is_dir() { std::fs::remove_dir_all(tmp) } else { std::fs::remove_file(tmp) };
}

/// How many of a save's earlier versions `replace` keeps beside it.
const BACKUPS: usize = 3;

/// `<save>.peerparty-before`, then `-2`, `-3`: the save `replace` moved aside, newest
/// first.
fn backup_name(save: &Path, n: usize) -> PathBuf {
    let mut name = save.as_os_str().to_owned();
    name.push(if n <= 1 { ".peerparty-before".to_string() } else { format!(".peerparty-before-{n}") });
    PathBuf::from(name)
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        // A link to a folder (a junction too) goes as a folder; what it points to stays.
        Ok(m) if m.is_symlink() && std::fs::metadata(p).is_ok_and(|t| t.is_dir()) => std::fs::remove_dir(p),
        Ok(_) => std::fs::remove_file(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `--copy-saves`: `to` becomes a copy of all of `from`. What was at `to` is moved
/// aside first (`backup_name`, the older backups shifted along and the oldest dropped);
/// if it can't be, nothing is copied and `to` stays as it was. Returns the files copied
/// and the backup made, if there was anything to back up.
fn replace(from: &Path, to: &Path) -> std::io::Result<(usize, Option<PathBuf>)> {
    // The copy is made whole first: one that fails (a full disk, a locked file) leaves
    // the save and its backups as they were, none shifted along or dropped.
    let (tmp, n) = stage_copy(from, to)?;
    let mut backup = None;
    if std::fs::symlink_metadata(to).is_ok() {
        let rotate = || -> std::io::Result<PathBuf> {
            remove_any(&backup_name(to, BACKUPS))?;
            for n in (1..BACKUPS).rev() {
                let older = backup_name(to, n);
                if std::fs::symlink_metadata(&older).is_ok() {
                    std::fs::rename(&older, backup_name(to, n + 1))?;
                }
            }
            let first = backup_name(to, 1);
            std::fs::rename(to, &first)?;
            Ok(first)
        };
        match rotate() {
            Ok(first) => backup = Some(first),
            Err(e) => {
                drop_staged(&tmp);
                return Err(e);
            }
        }
    }
    if let Err(e) = std::fs::rename(&tmp, to) {
        // What was there goes back.
        drop_staged(&tmp);
        if let Some(b) = &backup {
            let _ = remove_any(to);
            let _ = std::fs::rename(b, to);
        }
        return Err(e);
    }
    Ok((n, backup))
}

/// Whether a root `sources_of` found can be a save to copy. Not a device path (a named
/// pipe can carry an id, and even looking at one opens it), not Steam's own folder for
/// an account (`userdata\<account id>`: every game's cloud files and settings, not
/// this game's save), not PeerParty's own peer board or identity slots (a copy's
/// `.peer` file carries its hex peer id), and not under the temp folder.
fn worth_copying(root: &str, temp: Option<&str>) -> bool {
    let lower = root.replace('/', "\\").to_ascii_lowercase();
    if lower.starts_with(r"\\.\") || lower.starts_with(r"\\?\globalroot") || lower.starts_with(r"\\?\pipe\") || lower.starts_with(r"\\?\unc\.\") {
        return false;
    }
    let parts: Vec<&str> = lower.trim_end_matches('\\').split('\\').collect();
    if parts.len() >= 2 && parts[parts.len() - 2] == "userdata" {
        return false;
    }
    if parts.windows(2).any(|w| w[0] == "peerparty" && (w[1] == "peers" || w[1] == "identity")) {
        return false;
    }
    let temp = temp.map(|t| t.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()).filter(|t| !t.is_empty());
    !temp.is_some_and(|t| lower.starts_with(&format!("{t}\\")))
}

/// A path the game opens or probes: copy its save in first, if it has one to start from.
fn seen(path: &str) {
    // One spelling per save ("C:/x", "x" from the game's folder, "\\?\C:\x"), so DONE
    // knows a save it has copied whichever way the game names it next (under
    // --copy-saves a second copy would replace what this run has saved since).
    let full = full_path(path).unwrap_or_else(|| path.to_string());
    let path = match full.strip_prefix(r"\\?\") {
        Some(rest) if !rest.to_ascii_uppercase().starts_with(r"UNC\") => rest,
        _ => full.as_str(),
    };
    let pairs = crate::sys::lock(&PAIRS).clone();
    let found = sources_of(path, &pairs);
    if found.first().is_some_and(|(root, ..)| !worth_copying(root, std::env::var("TEMP").ok().as_deref())) {
        return;
    }
    let Some((root, _, once, _)) = found.first().cloned() else {
        // Say which save-like paths the game uses that carry none of the ids, so a game
        // that names its saves some other way shows up in the log.
        static SHOWN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let lower = path.to_ascii_lowercase();
        if (lower.contains("savegame") || lower.contains("my games")) && SHOWN.fetch_add(1, Ordering::Relaxed) < 40 {
            log!("saves: the game's save path {path} holds none of the ids {}", pairs.iter().map(|p| p.ours.as_str()).collect::<Vec<_>>().join(", "));
        }
        return;
    };
    let _copying = crate::sys::lock(&COPYING);
    if !crate::sys::lock(&DONE).insert(root.to_ascii_lowercase()) {
        return;
    }
    let root_p = PathBuf::from(&root);
    // Never over a save the new id has; an empty folder is no save yet.
    if once && root_p.exists() && !is_empty_dir(&root_p) {
        log!("saves: {root} is the game's own already; nothing copied");
        return;
    }
    let tried: Vec<&(String, String, bool, bool)> = found.iter().filter(|f| f.0.eq_ignore_ascii_case(&root)).collect();
    // The order: the save under an id listed as an earlier one, one told from its
    // neighbours (a SteamID of its own only), then steam_api's.
    let source = tried.iter().find(|f| !f.3 && Path::new(&f.1).exists()).map(|f| PathBuf::from(&f.1));
    let source = source.or_else(|| if once && tried.iter().any(|f| f.3) { guess_earlier(&root_p) } else { None });
    let source = source.or_else(|| tried.iter().find(|f| f.3 && Path::new(&f.1).exists()).map(|f| PathBuf::from(&f.1)));
    let Some(source_p) = source else {
        log!("saves: {root} has nothing to start from ({})", found.iter().map(|f| f.1.as_str()).collect::<Vec<_>>().join(", "));
        return;
    };
    let source = source_p.display().to_string();
    // Either way the copy runs inside the game's file call.
    if size_of_tree(&source_p, MAX_COPY) > MAX_COPY {
        log!("saves: {source} is over {} MB; not copied to {root}", MAX_COPY >> 20);
        return;
    }
    if once {
        if root_p.is_dir() {
            // Empty (above): copy_fresh puts the copy in its place.
            let _ = std::fs::remove_dir(&root_p);
        }
        match copy_fresh(&source_p, &root_p) {
            Ok(n) => log!("saves: {source} -> {root} ({n} files copied, once)"),
            Err(e) => log!("saves: copying {source} -> {root} failed: {e}"),
        }
        return;
    }
    match replace(&source_p, &root_p) {
        Ok((n, Some(b))) => log!("saves: {source} -> {root} ({n} files copied; what was there is now {})", b.display()),
        Ok((n, None)) => log!("saves: {source} -> {root} ({n} files copied)"),
        Err(e) => log!("saves: replacing {root} with {source} failed ({e}); {root} is as it was"),
    }
}

fn is_empty_dir(p: &Path) -> bool {
    std::fs::read_dir(p).is_ok_and(|mut d| d.next().is_none())
}

/// Called with every path the game opens or probes.
fn touch(name: *const u16) {
    if !ACTIVE.load(Ordering::Relaxed) || name.is_null() || BUSY.with(|b| b.get()) {
        return;
    }
    BUSY.with(|b| b.set(true));
    let _ = std::panic::catch_unwind(|| {
        if let Some(path) = read::wstr(name as usize) {
            seen(&path);
        }
    });
    BUSY.with(|b| b.set(false));
}

/// As `touch`, for an ANSI path (the system code page).
fn touch_a(name: *const u8) {
    if !ACTIVE.load(Ordering::Relaxed) || name.is_null() || BUSY.with(|b| b.get()) {
        return;
    }
    BUSY.with(|b| b.set(true));
    let _ = std::panic::catch_unwind(|| {
        if let Some(path) = ansi(name) {
            seen(&path);
        }
    });
    BUSY.with(|b| b.set(false));
}

fn ansi(p: *const u8) -> Option<String> {
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_ACP};
    let mut len = 0usize;
    while len < 32768 && unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    if len == 0 || len == 32768 {
        return None;
    }
    let n = unsafe { MultiByteToWideChar(CP_ACP, 0, p, len as i32, std::ptr::null_mut(), 0) };
    if n <= 0 {
        return None;
    }
    let mut w = vec![0u16; n as usize];
    let n = unsafe { MultiByteToWideChar(CP_ACP, 0, p, len as i32, w.as_mut_ptr(), n) };
    (n > 0).then(|| String::from_utf16_lossy(&w[..n as usize]))
}

// ---- a second copy's own saves ----

/// The slot of a copy whose saves are its own (1 and up); 0 while off.
static OWN_SLOT: AtomicU32 = AtomicU32::new(0);

/// Whether this copy's saves are its own (a later player's), known already.
pub fn own_saves() -> bool {
    OWN_SLOT.load(Ordering::Relaxed) != 0
}

/// A split-screen session's later player (`slot` from its `InstanceSpec`, read at the
/// hook's start): its saves are its own before the game first reads them. A game that
/// reads its save before its LAN comes up would otherwise read, and keep writing, the
/// first player's. The hooks go in at kernelbase (watch.rs).
pub fn own_slot_from_session(slot: u32) {
    if slot == 0 || slot == u32::MAX || crate::config::HookConfig::global().separate_saves == Some(false) {
        return;
    }
    OWN_SLOT.store(slot, Ordering::Relaxed);
    log!("saves: split-screen player {}: saves shared by every copy are this player's own from the start (a twin beside each, `.peerparty-{}`)", slot + 1, slot + 1);
}

/// The LAN came up: a player's copy in a slot other than the first keeps its own saves
/// (module comment).
pub fn learn_slot(lan: &peerparty_core::Lan) {
    let slot = lan.slot();
    if slot == 0 || slot == u32::MAX || lan.server() {
        return;
    }
    if crate::config::HookConfig::global().separate_saves == Some(false) {
        return;
    }
    let before = OWN_SLOT.swap(slot, Ordering::Relaxed);
    if before == slot {
        return;
    }
    if before != 0 {
        log!("saves: the session made this player {}, but the LAN gave it copy {}'s slot: its saves are copy {}'s from now on", before + 1, slot + 1, slot + 1);
    } else {
        log!("saves: this copy is the PC's copy {}: saves shared by every copy are its own (a twin beside each, `.peerparty-{}`)", slot + 1, slot + 1);
    }
    // Called while the LAN is being set up: not from here (adopt_earlier_ids).
    std::thread::spawn(arm_now);
}

/// A path's Win32 long-path prefix (`\\?\`, not a UNC one's), and the path without it.
fn split_verbatim(path: &str) -> (&str, &str) {
    match path.strip_prefix(r"\\?\") {
        Some(rest) if !rest.get(..4).is_some_and(|p| p.eq_ignore_ascii_case(r"UNC\")) => (r"\\?\", rest),
        _ => ("", path),
    }
}

/// For a full path the game uses, in a copy whose saves are its own: (the save's root,
/// its twin, the rest of the path after the root). None for a path outside any save.
pub fn twin_of(full: &str, exe_dir: &str, slot: u32) -> Option<(String, String, String)> {
    // A long path (`\\?\C:\...`) is the same save: its twin keeps the prefix.
    let (prefix, full) = split_verbatim(full);
    let (_, exe_dir) = split_verbatim(exe_dir);
    let norm = full.replace('/', "\\");
    if norm.starts_with("\\\\") || norm.to_ascii_lowercase().contains(".peerparty-") {
        return None;
    }
    // Components, by where each starts and ends.
    let mut comps: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    for (i, c) in norm.char_indices() {
        if c == '\\' {
            if i > start {
                comps.push((start, i));
            }
            start = i + 1;
        }
    }
    if norm.len() > start {
        comps.push((start, norm.len()));
    }
    let lower = norm.to_ascii_lowercase();
    // The deepest folder named for saves, past the drive; else the file itself (or the
    // folder the path names).
    let named = |i: usize| lower[comps[i].0..comps[i].1].contains("save");
    let last = comps.len().checked_sub(1)?;
    let mut k = (1..last).rev().find(|&i| named(i)).or_else(|| (last >= 1 && named(last)).then_some(last))?;
    // Windows' Saved Games holds every game's: its game folder is this game's save.
    if &lower[comps[k].0..comps[k].1] == "saved games" {
        k += 1;
        if k >= comps.len() {
            return None;
        }
    }
    let (rs, re) = comps[k];
    let root = &norm[..re];
    let root_l = &lower[..re];
    let exe = exe_dir.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
    if exe == root_l || exe.starts_with(&format!("{root_l}\\")) {
        return None;
    }
    let mark = format!(".peerparty-{}", slot + 1);
    let is_last = k + 1 == comps.len();
    let name = &norm[rs..re];
    let twin = match name.rfind('.').filter(|&d| d > 0 && is_last) {
        // A file: before its extension, so the game still knows it.
        Some(d) => format!("{}{}{}{}", &norm[..rs], &name[..d], mark, &name[d..]),
        None => format!("{root}{mark}"),
    };
    Some((format!("{prefix}{root}"), format!("{prefix}{twin}"), norm[re..].to_string()))
}

/// Roots whose twin this run has made (or found), and that twin.
static TWINS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Mutex::default);

/// Makes `twin` from `root` the first time this run sees it (a twin already there is
/// the copy's own and stays, unless `--copy-saves` asked for the first copy's again).
/// The root's twin: the one made first, so a root reached two ways ("Saves v1.0"
/// itself, a file in it) keeps one twin. Another thread asking meanwhile waits for the
/// copy (`COPYING`): the twin is listed only once it's whole, so no thread is handed a
/// path to a save still being copied (and a game finding none there doesn't start a
/// new one that the copy then can't be renamed onto).
fn make_twin(root: &str, twin: &str) -> String {
    let key = root.to_ascii_lowercase();
    if let Some(t) = crate::sys::lock(&TWINS).get(&key) {
        return t.clone();
    }
    let _copying = crate::sys::lock(&COPYING);
    if let Some(t) = crate::sys::lock(&TWINS).get(&key) {
        return t.clone();
    }
    let (from, to) = (Path::new(root), Path::new(twin));
    let too_big = || from.exists() && size_of_tree(from, MAX_COPY) >= MAX_COPY;
    if to.exists() && enabled() && from.exists() && !too_big() {
        // `--copy-saves`: the first copy's save over this one's, as for saves keyed by
        // an id (`replace`); this one's is kept as a backup.
        match replace(from, to) {
            Ok((n, backup)) => log!(
                "saves: {root} is {twin} in this copy, replaced by the first copy's for --copy-saves ({n} files copied; this copy's is kept as {})",
                backup.map_or("nothing".into(), |b| b.display().to_string())
            ),
            Err(e) => log!("saves: {root} is {twin} in this copy; copying the first copy's over it for --copy-saves failed ({e}), so it stays as it was"),
        }
    } else if to.exists() {
        log!("saves: {root} is {twin} in this copy (its own, kept from before)");
    } else if too_big() {
        log!("saves: {root} is {twin} in this copy; the first copy's is too big to copy here, so it starts empty");
    } else if from.exists() {
        match copy_fresh(from, to) {
            Ok(n) => log!("saves: {root} is {twin} in this copy, started from the first copy's ({n} files copied)"),
            Err(e) => log!("saves: {root} is {twin} in this copy; copying the first copy's failed ({e}), so it starts empty"),
        }
    } else {
        log!("saves: {root} is {twin} in this copy (nothing there yet)");
    }
    crate::sys::lock(&TWINS).insert(key, twin.to_string());
    twin.to_string()
}

/// The path the game gets instead of `path` in a copy whose saves are its own; the
/// twin is made from the first copy's the first time.
fn own_path(path: &str) -> Option<String> {
    let slot = OWN_SLOT.load(Ordering::Relaxed);
    if slot == 0 {
        return None;
    }
    let full = full_path(path)?;
    if under_any(&full, &not_saves()) {
        return None;
    }
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_string_lossy().into_owned();
    let (root, mut twin, rest) = twin_of(&full, &exe_dir, slot)?;
    // A save keyed by an id (the other way above, or one of this copy's own) is this
    // copy's already.
    let pairs = crate::sys::lock(&PAIRS).clone();
    if !sources_of(&full, &pairs).is_empty() || keyed_by_own_id(&full) {
        return None;
    }
    // A folder with a dot in its name ("MyGame v1.0") is a folder's twin, as it is
    // when a file in it is opened, not a file's (BUSY is set: this passes through).
    let folder = format!("{root}.peerparty-{}", slot + 1);
    if twin != folder && Path::new(&root).is_dir() {
        twin = folder;
    }
    let twin = make_twin(&root, &twin);
    Some(format!("{twin}{rest}"))
}

/// Folders whose files are never a game's shared saves: PeerParty's own (its stores for
/// services' cloud saves are kept per identity already, and a container a game named
/// "saves" there isn't the game's folder), the folders tests point those stores at, and
/// the temp folder.
fn not_saves() -> Vec<String> {
    let c = crate::config::HookConfig::global();
    let mut v: Vec<String> = [&c.galaxy_dir, &c.playfab_dir, &c.ugs_dir, &c.log_dir]
        .into_iter()
        .flatten()
        .map(|d| d.to_string_lossy().into_owned())
        .chain(["TEMP", "TMP"].iter().filter_map(|k| std::env::var(k).ok()))
        .collect();
    if let Some(d) = crate::util::local_store::peerparty_dir() {
        v.push(d.to_string_lossy().into_owned());
    }
    v
}

/// Whether `full` is one of `roots` or inside one (case and slashes aside).
fn under_any(full: &str, roots: &[String]) -> bool {
    let f = full.replace('/', "\\").to_ascii_lowercase();
    roots.iter().any(|r| {
        let r = r.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
        !r.is_empty() && (f == r || f.starts_with(&format!("{r}\\")))
    })
}

fn full_path(path: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;
    let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// `own_path` for a wide path the game passed: the twin's, NUL-terminated.
fn own_w(name: *const u16) -> Option<Vec<u16>> {
    if name.is_null() || OWN_SLOT.load(Ordering::Relaxed) == 0 || BUSY.with(|b| b.get()) {
        return None;
    }
    BUSY.with(|b| b.set(true));
    let r = std::panic::catch_unwind(|| read::wstr(name as usize).and_then(|p| own_path(&p))).ok().flatten();
    BUSY.with(|b| b.set(false));
    Some(r?.encode_utf16().chain(Some(0)).collect())
}

/// `own_w` for an ANSI path: the twin's in the system code page, NUL-terminated.
fn own_a(name: *const u8) -> Option<Vec<u8>> {
    use windows_sys::Win32::Globalization::{CP_ACP, WideCharToMultiByte};
    if name.is_null() || OWN_SLOT.load(Ordering::Relaxed) == 0 || BUSY.with(|b| b.get()) {
        return None;
    }
    BUSY.with(|b| b.set(true));
    let r = std::panic::catch_unwind(|| ansi(name).and_then(|p| own_path(&p))).ok().flatten();
    BUSY.with(|b| b.set(false));
    let w: Vec<u16> = r?.encode_utf16().collect();
    let n = unsafe { WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, std::ptr::null_mut(), 0, std::ptr::null(), std::ptr::null_mut()) };
    if n <= 0 {
        return None;
    }
    let mut out = vec![0u8; n as usize + 1];
    unsafe { WideCharToMultiByte(CP_ACP, 0, w.as_ptr(), w.len() as i32, out.as_mut_ptr(), n, std::ptr::null(), std::ptr::null_mut()) };
    Some(out)
}

/// Rebinds `$name` to the twin's path when there is one (kept alive in `$keep`).
macro_rules! own {
    ($f:ident, $name:ident, $keep:ident) => {
        let $keep = $f($name);
        let $name = $keep.as_ref().map_or($name, |v| v.as_ptr());
    };
}

// ---- kernelbase hooks ----

type CreateFileW = unsafe extern "system" fn(*const u16, u32, u32, usize, u32, u32, isize) -> isize;
type CreateFileA = unsafe extern "system" fn(*const u8, u32, u32, usize, u32, u32, isize) -> isize;
type CreateFile2 = unsafe extern "system" fn(*const u16, u32, u32, u32, usize) -> isize;
type GetFileAttributesW = unsafe extern "system" fn(*const u16) -> u32;
type GetFileAttributesA = unsafe extern "system" fn(*const u8) -> u32;
type GetFileAttributesExW = unsafe extern "system" fn(*const u16, i32, usize) -> i32;
type GetFileAttributesExA = unsafe extern "system" fn(*const u8, i32, usize) -> i32;
type FindFirstFileExW = unsafe extern "system" fn(*const u16, i32, usize, i32, usize, u32) -> isize;
type FindFirstFileExA = unsafe extern "system" fn(*const u8, i32, usize, i32, usize, u32) -> isize;
type CreateDirectoryW = unsafe extern "system" fn(*const u16, usize) -> i32;
type CreateDirectoryA = unsafe extern "system" fn(*const u8, usize) -> i32;
type OnePathW = unsafe extern "system" fn(*const u16) -> i32;
type OnePathA = unsafe extern "system" fn(*const u8) -> i32;
type SetAttrsW = unsafe extern "system" fn(*const u16, u32) -> i32;
type SetAttrsA = unsafe extern "system" fn(*const u8, u32) -> i32;
type MoveExW = unsafe extern "system" fn(*const u16, *const u16, u32) -> i32;
type MoveExA = unsafe extern "system" fn(*const u8, *const u8, u32) -> i32;
type MoveProgressW = unsafe extern "system" fn(*const u16, *const u16, usize, usize, u32) -> i32;
type ReplaceW = unsafe extern "system" fn(*const u16, *const u16, *const u16, u32, usize, usize) -> i32;
type CopyExW = unsafe extern "system" fn(*const u16, *const u16, usize, usize, usize, u32) -> i32;

static CREATE_FILE: OnceLock<GenericDetour<CreateFileW>> = OnceLock::new();
static CREATE_FILE_A: OnceLock<GenericDetour<CreateFileA>> = OnceLock::new();
static CREATE_FILE2: OnceLock<GenericDetour<CreateFile2>> = OnceLock::new();
static ATTRS: OnceLock<GenericDetour<GetFileAttributesW>> = OnceLock::new();
static ATTRS_A: OnceLock<GenericDetour<GetFileAttributesA>> = OnceLock::new();
static ATTRS_EX: OnceLock<GenericDetour<GetFileAttributesExW>> = OnceLock::new();
static ATTRS_EX_A: OnceLock<GenericDetour<GetFileAttributesExA>> = OnceLock::new();
static FIND: OnceLock<GenericDetour<FindFirstFileExW>> = OnceLock::new();
static FIND_A: OnceLock<GenericDetour<FindFirstFileExA>> = OnceLock::new();
static MKDIR: OnceLock<GenericDetour<CreateDirectoryW>> = OnceLock::new();
static MKDIR_A: OnceLock<GenericDetour<CreateDirectoryA>> = OnceLock::new();
static DELETE: OnceLock<GenericDetour<OnePathW>> = OnceLock::new();
static DELETE_A: OnceLock<GenericDetour<OnePathA>> = OnceLock::new();
static RMDIR: OnceLock<GenericDetour<OnePathW>> = OnceLock::new();
static RMDIR_A: OnceLock<GenericDetour<OnePathA>> = OnceLock::new();
static SET_ATTRS: OnceLock<GenericDetour<SetAttrsW>> = OnceLock::new();
static SET_ATTRS_A: OnceLock<GenericDetour<SetAttrsA>> = OnceLock::new();
static MOVE_EX: OnceLock<GenericDetour<MoveExW>> = OnceLock::new();
static MOVE_EX_A: OnceLock<GenericDetour<MoveExA>> = OnceLock::new();
static MOVE_PROGRESS: OnceLock<GenericDetour<MoveProgressW>> = OnceLock::new();
static REPLACE: OnceLock<GenericDetour<ReplaceW>> = OnceLock::new();
static COPY_EX: OnceLock<GenericDetour<CopyExW>> = OnceLock::new();

static ARMED: AtomicBool = AtomicBool::new(false);

fn arm<T>(exports: &[Export], name: &str, slot: &OnceLock<GenericDetour<T>>, detour: T) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    if slot.get().is_some() {
        return 0;
    }
    let Some(e) = exports.iter().find(|e| e.name == name) else {
        return 0;
    };
    // `arm_now` hooks these while the game runs (its threads may be in them): the
    // others are paused while each prologue is written (not at the hook's own start,
    // before the game has threads: `sys::hook::frozen`).
    let target: T = unsafe { T::from_ptr(e.addr as *const ()) };
    usize::from(crate::sys::detour::try_arm(slot, target, detour, "saves", name, true) == crate::sys::detour::Armed::Hooked)
}

/// Arms the file hooks in kernelbase (once).
pub fn attach_kernelbase(exports: &[Export]) -> usize {
    if ARMED.swap(true, Ordering::SeqCst) {
        return 0;
    }
    arm(exports, "CreateFileW", &CREATE_FILE, detour_create_file as CreateFileW)
        + arm(exports, "CreateFileA", &CREATE_FILE_A, detour_create_file_a as CreateFileA)
        + arm(exports, "CreateFile2", &CREATE_FILE2, detour_create_file2 as CreateFile2)
        + arm(exports, "GetFileAttributesW", &ATTRS, detour_attrs as GetFileAttributesW)
        + arm(exports, "GetFileAttributesA", &ATTRS_A, detour_attrs_a as GetFileAttributesA)
        + arm(exports, "GetFileAttributesExW", &ATTRS_EX, detour_attrs_ex as GetFileAttributesExW)
        + arm(exports, "GetFileAttributesExA", &ATTRS_EX_A, detour_attrs_ex_a as GetFileAttributesExA)
        + arm(exports, "FindFirstFileExW", &FIND, detour_find as FindFirstFileExW)
        + arm(exports, "FindFirstFileExA", &FIND_A, detour_find_a as FindFirstFileExA)
        + arm(exports, "CreateDirectoryW", &MKDIR, detour_mkdir as CreateDirectoryW)
        + arm(exports, "CreateDirectoryA", &MKDIR_A, detour_mkdir_a as CreateDirectoryA)
        + arm(exports, "DeleteFileW", &DELETE, detour_delete as OnePathW)
        + arm(exports, "DeleteFileA", &DELETE_A, detour_delete_a as OnePathA)
        + arm(exports, "RemoveDirectoryW", &RMDIR, detour_rmdir as OnePathW)
        + arm(exports, "RemoveDirectoryA", &RMDIR_A, detour_rmdir_a as OnePathA)
        + arm(exports, "SetFileAttributesW", &SET_ATTRS, detour_set_attrs as SetAttrsW)
        + arm(exports, "SetFileAttributesA", &SET_ATTRS_A, detour_set_attrs_a as SetAttrsA)
        + arm(exports, "MoveFileExW", &MOVE_EX, detour_move_ex as MoveExW)
        + arm(exports, "MoveFileExA", &MOVE_EX_A, detour_move_ex_a as MoveExA)
        + arm(exports, "MoveFileWithProgressW", &MOVE_PROGRESS, detour_move_progress as MoveProgressW)
        + arm(exports, "ReplaceFileW", &REPLACE, detour_replace as ReplaceW)
        + arm(exports, "CopyFileExW", &COPY_EX, detour_copy_ex as CopyExW)
}

/// Arms the file hooks now, from kernelbase as it's loaded (also for a split-screen
/// copy without saves of its own to keep apart: its CreateFile keeps other players'
/// controllers from it, input/device.rs).
pub fn arm_now() {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    if ARMED.load(Ordering::SeqCst) {
        return;
    }
    let name: Vec<u16> = "kernelbase.dll".encode_utf16().chain(Some(0)).collect();
    let base = unsafe { GetModuleHandleW(name.as_ptr()) };
    if base.is_null() {
        log!("saves: kernelbase not found; saves aren't copied");
        return;
    }
    match unsafe { crate::sys::pe::exports(base.cast()) } {
        Ok(t) => {
            let n = attach_kernelbase(&t.code);
            log!("saves file hooks {n}");
        }
        Err(e) => log!("saves: kernelbase's exports unreadable ({e}); saves aren't copied"),
    }
}

/// A split-screen copy can't open another player's game controller (input/device.rs).
fn not_there() -> isize {
    unsafe { windows_sys::Win32::Foundation::SetLastError(windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND) };
    -1
}

unsafe extern "system" fn detour_create_file(name: *const u16, access: u32, share: u32, sa: usize, disposition: u32, flags: u32, template: isize) -> isize {
    if crate::input::refuse_open_w(name) {
        return not_there();
    }
    touch(name);
    let asked = name;
    own!(own_w, name, own_name);
    let h = unsafe { CREATE_FILE.get().unwrap().call(name, access, share, sa, disposition, flags, template) };
    crate::input::opened_w(asked, h);
    h
}

unsafe extern "system" fn detour_create_file_a(name: *const u8, access: u32, share: u32, sa: usize, disposition: u32, flags: u32, template: isize) -> isize {
    if crate::input::refuse_open_a(name) {
        return not_there();
    }
    touch_a(name);
    let asked = name;
    own!(own_a, name, own_name);
    let h = unsafe { CREATE_FILE_A.get().unwrap().call(name, access, share, sa, disposition, flags, template) };
    crate::input::opened_a(asked, h);
    h
}

unsafe extern "system" fn detour_create_file2(name: *const u16, access: u32, share: u32, disposition: u32, params: usize) -> isize {
    if crate::input::refuse_open_w(name) {
        return not_there();
    }
    touch(name);
    let asked = name;
    own!(own_w, name, own_name);
    let h = unsafe { CREATE_FILE2.get().unwrap().call(name, access, share, disposition, params) };
    crate::input::opened_w(asked, h);
    h
}

unsafe extern "system" fn detour_attrs(name: *const u16) -> u32 {
    touch(name);
    own!(own_w, name, own_name);
    unsafe { ATTRS.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_attrs_a(name: *const u8) -> u32 {
    touch_a(name);
    own!(own_a, name, own_name);
    unsafe { ATTRS_A.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_attrs_ex(name: *const u16, level: i32, info: usize) -> i32 {
    touch(name);
    own!(own_w, name, own_name);
    unsafe { ATTRS_EX.get().unwrap().call(name, level, info) }
}

unsafe extern "system" fn detour_attrs_ex_a(name: *const u8, level: i32, info: usize) -> i32 {
    touch_a(name);
    own!(own_a, name, own_name);
    unsafe { ATTRS_EX_A.get().unwrap().call(name, level, info) }
}

unsafe extern "system" fn detour_find(name: *const u16, level: i32, data: usize, op: i32, filter: usize, flags: u32) -> isize {
    touch(name);
    own!(own_w, name, own_name);
    unsafe { FIND.get().unwrap().call(name, level, data, op, filter, flags) }
}

unsafe extern "system" fn detour_find_a(name: *const u8, level: i32, data: usize, op: i32, filter: usize, flags: u32) -> isize {
    touch_a(name);
    own!(own_a, name, own_name);
    unsafe { FIND_A.get().unwrap().call(name, level, data, op, filter, flags) }
}

unsafe extern "system" fn detour_mkdir(name: *const u16, sa: usize) -> i32 {
    touch(name);
    own!(own_w, name, own_name);
    unsafe { MKDIR.get().unwrap().call(name, sa) }
}

unsafe extern "system" fn detour_mkdir_a(name: *const u8, sa: usize) -> i32 {
    touch_a(name);
    own!(own_a, name, own_name);
    unsafe { MKDIR_A.get().unwrap().call(name, sa) }
}

// Calls that only change files: the twin's path for a copy whose saves are its own
// (no `touch`: that copies an id-keyed save in before the game reads it).

unsafe extern "system" fn detour_delete(name: *const u16) -> i32 {
    own!(own_w, name, own_name);
    unsafe { DELETE.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_delete_a(name: *const u8) -> i32 {
    own!(own_a, name, own_name);
    unsafe { DELETE_A.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_rmdir(name: *const u16) -> i32 {
    own!(own_w, name, own_name);
    unsafe { RMDIR.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_rmdir_a(name: *const u8) -> i32 {
    own!(own_a, name, own_name);
    unsafe { RMDIR_A.get().unwrap().call(name) }
}

unsafe extern "system" fn detour_set_attrs(name: *const u16, attrs: u32) -> i32 {
    own!(own_w, name, own_name);
    unsafe { SET_ATTRS.get().unwrap().call(name, attrs) }
}

unsafe extern "system" fn detour_set_attrs_a(name: *const u8, attrs: u32) -> i32 {
    own!(own_a, name, own_name);
    unsafe { SET_ATTRS_A.get().unwrap().call(name, attrs) }
}

unsafe extern "system" fn detour_move_ex(from: *const u16, to: *const u16, flags: u32) -> i32 {
    own!(own_w, from, own_from);
    own!(own_w, to, own_to);
    unsafe { MOVE_EX.get().unwrap().call(from, to, flags) }
}

unsafe extern "system" fn detour_move_ex_a(from: *const u8, to: *const u8, flags: u32) -> i32 {
    own!(own_a, from, own_from);
    own!(own_a, to, own_to);
    unsafe { MOVE_EX_A.get().unwrap().call(from, to, flags) }
}

unsafe extern "system" fn detour_move_progress(from: *const u16, to: *const u16, progress: usize, data: usize, flags: u32) -> i32 {
    own!(own_w, from, own_from);
    own!(own_w, to, own_to);
    unsafe { MOVE_PROGRESS.get().unwrap().call(from, to, progress, data, flags) }
}

unsafe extern "system" fn detour_replace(replaced: *const u16, replacement: *const u16, backup: *const u16, flags: u32, a: usize, b: usize) -> i32 {
    own!(own_w, replaced, own_replaced);
    own!(own_w, replacement, own_replacement);
    own!(own_w, backup, own_backup);
    unsafe { REPLACE.get().unwrap().call(replaced, replacement, backup, flags, a, b) }
}

unsafe extern "system" fn detour_copy_ex(from: *const u16, to: *const u16, progress: usize, data: usize, cancel: usize, flags: u32) -> i32 {
    own!(own_w, from, own_from);
    own!(own_w, to, own_to);
    unsafe { COPY_EX.get().unwrap().call(from, to, progress, data, cancel, flags) }
}

/// A store file that is there but can't be read as one (damaged, or not ours) is moved
/// aside to `<name>.unreadable-<seconds>`, so the empty store that replaces it doesn't
/// overwrite what may still be recovered by hand. Logged with `what`.
pub fn set_aside_unreadable(path: &Path, what: &str) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".unreadable-{secs}"));
    let aside = path.with_file_name(name);
    match std::fs::rename(path, &aside) {
        Ok(()) => log!("{what}: {} couldn't be read; kept as {} and started empty", path.display(), aside.display()),
        Err(e) => log!("{what}: {} couldn't be read, nor moved aside ({e})", path.display()),
    }
}

/// Writes a whole file (a save store) so that a crash, a power cut or a full disk
/// mid-write leaves the old one whole: the data goes to a file in a `.peerparty-writing`
/// folder beside it, is flushed to the disk, then takes the old one's place in one step
/// (a rename within the volume). Listings of the store see files only, not that folder.
pub fn write_whole(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    let name = path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
    let work = dir.join(".peerparty-writing");
    std::fs::create_dir_all(&work)?;
    let mut tmp_name = std::ffi::OsString::from(format!("{}-{}-", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    tmp_name.push(name);
    let tmp = work.join(tmp_name);
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    let done = written.and_then(|()| std::fs::rename(&tmp, path));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let _ = std::fs::remove_dir(&work);
    done
}

#[cfg(test)]
mod tests {

    /// A second copy's twin of a save shared by every copy: the deepest part named for
    /// saves, never the game's own folder, and Windows' Saved Games only through the
    /// game's folder in it.
    #[test]
    fn a_second_copy_gets_a_twin_of_shared_saves() {
        let exe = r"C:\Games\Some Game";
        let t = twin_of(r"C:\Games\Some Game\Profile\Player\Saves\savegame.dat", exe, 1).unwrap();
        assert_eq!(t.0, r"C:\Games\Some Game\Profile\Player\Saves");
        assert_eq!(t.1, r"C:\Games\Some Game\Profile\Player\Saves.peerparty-2");
        assert_eq!(t.2, r"\savegame.dat");
        // A search pattern inside the folder goes to the twin's.
        assert_eq!(twin_of(r"C:\Games\Some Game\Profile\Player\Saves\*", exe, 2).unwrap().1, r"C:\Games\Some Game\Profile\Player\Saves.peerparty-3");
        // A save file on its own keeps its extension.
        let f = twin_of(r"C:\Users\a\Documents\My Games\Foo\savegame.sav", r"C:\Games\Foo", 1).unwrap();
        assert_eq!(f.1, r"C:\Users\a\Documents\My Games\Foo\savegame.peerparty-2.sav");
        assert_eq!(f.2, "");
        // Unreal: SaveGames, not Saved.
        let u = twin_of(r"C:\Users\a\AppData\Local\Foo\Saved\SaveGames\slot1.sav", r"C:\Games\Foo", 1).unwrap();
        assert_eq!(u.1, r"C:\Users\a\AppData\Local\Foo\Saved\SaveGames.peerparty-2");
        // Saved Games: the game's folder in it.
        let g = twin_of(r"C:\Users\a\Saved Games\Foo\profile.bin", r"C:\Games\Foo", 1).unwrap();
        assert_eq!(g.1, r"C:\Users\a\Saved Games\Foo.peerparty-2");
        // The game's own folder (or one above it) is never a save.
        assert_eq!(twin_of(r"C:\Games\Save The Date\data\x.pak", r"C:\Games\Save The Date", 1), None);
        assert_eq!(twin_of(r"D:\Saves\Foo\game\x.pak", r"D:\Saves\Foo\game", 1), None);
        // Paths with nothing named for saves, twins already, and device paths.
        assert_eq!(twin_of(r"C:\Games\Foo\data\x.pak", r"C:\Games\Foo", 1), None);
        assert_eq!(twin_of(r"C:\x\Saves.peerparty-2\a.dat", r"C:\Games\Foo", 1), None);
        assert_eq!(twin_of(r"\\.\pipe\savepipe", r"C:\Games\Foo", 1), None);
        // A long path is the same save, its twin a long path too; a UNC one isn't.
        let l = twin_of(r"\\?\C:\Games\Some Game\Profile\Player\Saves\savegame.dat", exe, 1).unwrap();
        assert_eq!(l.0, r"\\?\C:\Games\Some Game\Profile\Player\Saves");
        assert_eq!(l.1, r"\\?\C:\Games\Some Game\Profile\Player\Saves.peerparty-2");
        assert_eq!(l.2, r"\savegame.dat");
        assert_eq!(twin_of(r"\\?\C:\Games\Some Game\data\Saves\x", r"\\?\C:\Games\Some Game\data\Saves", 1), None);
        assert_eq!(twin_of(r"\\?\UNC\server\share\Saves\a.dat", exe, 1), None);
    }

    /// Threads asking for the same twin at once all get it whole: none is handed the
    /// twin's path while the first copy's save is still being copied into it.
    #[test]
    fn threads_asking_for_a_twin_at_once_wait_for_it() {
        let base = std::env::temp_dir().join(format!("peerparty-twin-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let saves = base.join("Saves");
        std::fs::create_dir_all(&saves).unwrap();
        for i in 0..200 {
            std::fs::write(saves.join(format!("part{i}.dat")), vec![i as u8; 4096]).unwrap();
        }
        let twin = base.join("Saves.peerparty-2");
        let (root, to) = (saves.to_string_lossy().into_owned(), twin.to_string_lossy().into_owned());
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let (root, to) = (root.clone(), to.clone());
                std::thread::spawn(move || {
                    let got = make_twin(&root, &to);
                    // What the thread was handed is all there.
                    let n = std::fs::read_dir(&got).map(|d| d.count()).unwrap_or(0);
                    (got, n)
                })
            })
            .collect();
        for t in threads {
            let (got, n) = t.join().unwrap();
            assert_eq!(got, to);
            assert_eq!(n, 200, "a thread was handed a twin still being copied");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The first time, the twin starts as a copy of the first copy's save, which is
    /// left as it was; a twin already there is kept.
    #[test]
    fn the_twin_starts_from_the_first_copys_save_once() {
        let base = std::env::temp_dir().join(format!("peerparty-twin-{}", std::process::id()));
        let saves = base.join("Saves");
        std::fs::create_dir_all(&saves).unwrap();
        std::fs::write(saves.join("savegame.dat"), b"first").unwrap();
        let twin = base.join("Saves.peerparty-2");
        make_twin(&saves.to_string_lossy(), &twin.to_string_lossy());
        assert_eq!(std::fs::read(twin.join("savegame.dat")).unwrap(), b"first");
        std::fs::write(twin.join("savegame.dat"), b"second").unwrap();
        // Not copied again (this run or, the twin existing, the next).
        crate::sys::lock(&TWINS).clear();
        make_twin(&saves.to_string_lossy(), &twin.to_string_lossy());
        assert_eq!(std::fs::read(twin.join("savegame.dat")).unwrap(), b"second");
        assert_eq!(std::fs::read(saves.join("savegame.dat")).unwrap(), b"first");
        let _ = std::fs::remove_dir_all(&base);
    }

    use super::*;

    /// A whole write replaces the file and leaves nothing beside it; one that can't
    /// finish leaves the old file as it was.
    #[test]
    fn a_whole_write_replaces_the_file_in_one_step() {
        let dir = std::env::temp_dir().join(format!("peerparty-write-whole-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("store.json");
        write_whole(&file, b"one").unwrap();
        write_whole(&file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["store.json"], "nothing left beside it");
        // A file open without delete sharing can't be replaced: the old one stays.
        let held = std::fs::File::open(&file).unwrap();
        let blocked = write_whole(&file, b"three");
        drop(held);
        if blocked.is_err() {
            assert_eq!(std::fs::read(&file).unwrap(), b"two");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn pair(ours: &str, from: &str, once: bool) -> Pair {
        Pair { ours: ours.into(), from: from.into(), once, fallback: false }
    }

    #[test]
    fn finds_the_id_component_and_its_main_twin() {
        let pairs = vec![
            pair("76561202255233024", "76561198000000001", false),
            pair("abcdef0123456789abcdef0123456789", "00112233445566778899aabbccddeeff", false),
        ];
        let got = sources_of(r"C:\Users\x\AppData\LocalLow\Co\Game\76561202255233024\slot1\save.dat", &pairs);
        assert_eq!(
            got,
            vec![(
                r"C:\Users\x\AppData\LocalLow\Co\Game\76561202255233024".into(),
                r"C:\Users\x\AppData\LocalLow\Co\Game\76561198000000001".into(),
                false,
                false
            )]
        );
        let got = sources_of(r"D:\Saves\profile_76561202255233024.sav", &pairs);
        assert_eq!(got[0].1, r"D:\Saves\profile_76561198000000001.sav");
        // Hex ids match whatever their case; ids inside longer numbers don't.
        assert!(!sources_of(r"C:\s\ABCDEF0123456789ABCDEF0123456789\a", &pairs).is_empty());
        assert!(sources_of(r"C:\s\176561202255233024\a", &pairs).is_empty());
        assert!(sources_of(r"C:\s\nothing\here", &pairs).is_empty());
    }

    #[test]
    fn under_steams_account_folder_only_the_games_folder_is_copied() {
        let pairs = vec![pair("2147483714", "4660", true)];
        let got = sources_of(r"C:\Steam\userdata\2147483714\123450\remote\save.dat", &pairs);
        assert_eq!(got[0].0, r"C:\Steam\userdata\2147483714\123450");
        assert_eq!(got[0].1, r"C:\Steam\userdata\4660\123450");
        // The account folder itself is no save (`worth_copying` turns it down).
        let got = sources_of(r"C:\Steam\userdata\2147483714", &pairs);
        assert!(!worth_copying(&got[0].0, None));
    }

    #[test]
    fn several_sources_for_one_id_in_order() {
        let pairs = vec![pair("222", "111", false), pair("222", "000", false)];
        let got = sources_of(r"C:\s\222\a.sav", &pairs);
        assert_eq!(got.iter().map(|g| g.1.as_str()).collect::<Vec<_>>(), [r"C:\s\111", r"C:\s\000"]);
    }

    #[test]
    fn a_steamid_shows_up_four_ways() {
        let own = 0x0110_0001_8000_0042u64;
        let game = 0x0110_0001_0000_1234u64;
        let names = steam_names(own, game);
        assert!(names.contains(&(own.to_string(), game.to_string())));
        assert!(names.contains(&("2147483714".into(), "4660".into())));
        assert!(names.contains(&("0110000180000042".into(), "0110000100001234".into())));
        assert!(names.contains(&("110000180000042".into(), "110000100001234".into())));
    }

    #[test]
    fn copies_folders_keeping_newer_files() {
        let base = std::env::temp_dir().join(format!("peerparty-saves-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (main, lan) = (base.join("111"), base.join("222"));
        std::fs::create_dir_all(main.join("sub")).unwrap();
        std::fs::write(main.join("a.sav"), "main").unwrap();
        std::fs::write(main.join("sub").join("b.sav"), "main").unwrap();
        assert_eq!(copy_newer(&main, &lan).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(lan.join("sub").join("b.sav")).unwrap(), "main");
        // A file the LAN copy wrote since is newer and stays.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(lan.join("a.sav"), "lan").unwrap();
        assert_eq!(copy_newer(&main, &lan).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(lan.join("a.sav")).unwrap(), "lan");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_junction_inside_a_save_is_not_followed() {
        use std::os::windows::process::CommandExt;
        let base = std::env::temp_dir().join(format!("peerparty-junction-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (main, lan) = (base.join("111"), base.join("222"));
        std::fs::create_dir_all(&main).unwrap();
        std::fs::write(main.join("a.sav"), "main").unwrap();
        // A junction back up to the save itself: followed, the copy would never end.
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(main.join("loop"))
            .arg(&main)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J: {made:?}");
        assert!(size_of_tree(&main, MAX_COPY) < 100);
        assert_eq!(copy_newer(&main, &lan).unwrap(), 1);
        assert!(!lan.join("loop").exists());
        remove_any(&main.join("loop")).unwrap();
        assert!(main.join("a.sav").exists(), "removing the junction leaves what it pointed to");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_fresh_copy_leaves_nothing_half_done_and_the_original_alone() {
        let base = std::env::temp_dir().join(format!("peerparty-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (old, new) = (base.join("4660"), base.join("2147483714"));
        std::fs::create_dir_all(old.join("sub")).unwrap();
        std::fs::write(old.join("a.sav"), "old").unwrap();
        std::fs::write(old.join("sub").join("b.sav"), "old").unwrap();
        // Left over from a copy cut short.
        std::fs::create_dir_all(base.join("2147483714.peerparty-copy")).unwrap();
        assert_eq!(copy_fresh(&old, &new).unwrap(), 2);
        assert!(!base.join("2147483714.peerparty-copy").exists());
        assert_eq!(std::fs::read_to_string(new.join("sub").join("b.sav")).unwrap(), "old");
        std::fs::write(new.join("a.sav"), "new").unwrap();
        assert_eq!(std::fs::read_to_string(old.join("a.sav")).unwrap(), "old");
        // A file save, too.
        std::fs::write(base.join("4660_save.dat"), "old").unwrap();
        assert_eq!(copy_fresh(&base.join("4660_save.dat"), &base.join("2147483714_save.dat")).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(base.join("2147483714_save.dat")).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `--copy-saves` makes the second copy's save a copy of all of the first's: what it
    /// had is moved aside whole, never merged file by file (a fresh save written since
    /// would otherwise win over the real progress, file by file). Three backups are kept.
    #[test]
    fn copy_saves_replaces_the_save_whole_and_keeps_backups() {
        let base = std::env::temp_dir().join(format!("peerparty-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (main, lan) = (base.join("111"), base.join("222"));
        std::fs::create_dir_all(main.join("sub")).unwrap();
        std::fs::write(main.join("a.sav"), "main").unwrap();
        std::fs::write(main.join("sub").join("b.sav"), "main").unwrap();
        // Nothing there yet: a plain copy, no backup.
        assert_eq!(replace(&main, &lan).unwrap(), (2, None));
        // The second copy's own save, written after the first's: still replaced.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(lan.join("a.sav"), "lan").unwrap();
        std::fs::write(lan.join("c.sav"), "lan only").unwrap();
        let (n, backup) = replace(&main, &lan).unwrap();
        assert_eq!((n, backup.as_deref()), (2, Some(base.join("222.peerparty-before").as_path())));
        assert_eq!(std::fs::read_to_string(lan.join("a.sav")).unwrap(), "main");
        assert!(!lan.join("c.sav").exists(), "nothing of the old save is mixed in");
        assert_eq!(std::fs::read_to_string(base.join("222.peerparty-before").join("c.sav")).unwrap(), "lan only");
        assert_eq!(std::fs::read_to_string(base.join("222.peerparty-before").join("a.sav")).unwrap(), "lan");
        // Each run moves the backups along: -2, -3, and the oldest goes.
        for run in 0..3 {
            std::fs::write(lan.join("run.txt"), format!("{run}")).unwrap();
            replace(&main, &lan).unwrap();
        }
        assert_eq!(std::fs::read_to_string(base.join("222.peerparty-before").join("run.txt")).unwrap(), "2");
        assert_eq!(std::fs::read_to_string(base.join("222.peerparty-before-2").join("run.txt")).unwrap(), "1");
        assert_eq!(std::fs::read_to_string(base.join("222.peerparty-before-3").join("run.txt")).unwrap(), "0");
        assert!(!base.join("222.peerparty-before-4").exists());
        assert_eq!(std::fs::read_to_string(main.join("a.sav")).unwrap(), "main", "the first copy's save is only read");
        // A save that is one file, too.
        std::fs::write(base.join("111_save.dat"), "main").unwrap();
        std::fs::write(base.join("222_save.dat"), "lan").unwrap();
        replace(&base.join("111_save.dat"), &base.join("222_save.dat")).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("222_save.dat")).unwrap(), "main");
        assert_eq!(std::fs::read_to_string(base.join("222_save.dat.peerparty-before")).unwrap(), "lan");
        // A source that can't be copied leaves the save where it was.
        std::fs::write(lan.join("keep.txt"), "kept").unwrap();
        assert!(replace(&base.join("missing"), &lan).is_err());
        assert_eq!(std::fs::read_to_string(lan.join("keep.txt")).unwrap(), "kept");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A SteamID of its own starts from the save under an earlier one of its own when
    /// there is one, else steam_api's; and never over a save already there.
    #[test]
    fn an_earlier_own_steamid_comes_before_steam_apis() {
        let own = crate::steam::ids::lan_steam_id(peerparty_core::PeerId([0xa1; 16]));
        let old = crate::steam::ids::lan_steam_id(peerparty_core::PeerId([0x10; 16]));
        let game = 76561197960270388u64;
        let mut pairs = Vec::new();
        for (ours, from) in steam_names(own, old) {
            pairs.push(pair(&ours, &from, true));
        }
        for (ours, from) in steam_names(own, game) {
            pairs.push(Pair { fallback: true, ..pair(&ours, &from, true) });
        }
        let base = std::env::temp_dir().join(format!("peerparty-earlier-{}", std::process::id()));
        let path = base.join((own as u32).to_string()).join("save.bin");
        let got = sources_of(&path.to_string_lossy(), &pairs);
        let sources: Vec<_> = got.iter().map(|g| g.1.clone()).collect();
        assert_eq!(
            sources,
            [base.join((old as u32).to_string()).to_string_lossy().into_owned(), base.join((game as u32).to_string()).to_string_lossy().into_owned()],
            "the earlier own id's first"
        );
        assert!(got.iter().all(|g| g.2), "once only: an existing save for the new id is never touched");
        assert_eq!(got.iter().map(|g| g.3).collect::<Vec<_>>(), [false, true], "steam_api's is the fallback");
    }

    /// With no earlier SteamID listed, the save beside the new one under another
    /// PeerParty SteamID is taken: the one with the newest file when there are several.
    /// steam_api's, this PC's slots' now, real accounts' and empty ones never are.
    #[test]
    fn an_unlisted_earlier_steamid_is_the_newest_neighbour() {
        use std::time::{Duration, SystemTime};
        let base = std::env::temp_dir().join(format!("peerparty-guess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let own = crate::steam::ids::lan_steam_id(peerparty_core::PeerId([0xa1; 16]));
        let slot1 = crate::steam::ids::lan_steam_id(peerparty_core::PeerId([0x2b; 16]));
        let game = 76561197960270388u64;
        let at = |days: u64| SystemTime::now() - Duration::from_secs(days * 86_400);
        let save = |name: &str, when: Option<SystemTime>| {
            let d = base.join(name);
            std::fs::create_dir_all(&d).unwrap();
            if let Some(when) = when {
                let f = std::fs::File::create(d.join("save.bin")).unwrap();
                f.set_modified(when).unwrap();
            }
        };
        let older = crate::steam::ids::lan_steam_id(peerparty_core::PeerId([0x10; 16]));
        let newer = crate::steam::ids::lan_steam_id(peerparty_core::PeerId([0x20; 16]));
        let empty = crate::steam::ids::lan_steam_id(peerparty_core::PeerId([0x30; 16]));
        save(&(older as u32).to_string(), Some(at(20)));
        save(&(newer as u32).to_string(), Some(at(5)));
        save(&(empty as u32).to_string(), None);
        // Newer still, and never taken: steam_api's, a slot's now, a real account's.
        save(&(game as u32).to_string(), Some(at(1)));
        save(&(slot1 as u32).to_string(), Some(at(1)));
        save("12345", Some(at(1)));
        save("notanid", Some(at(1)));
        let root = base.join((own as u32).to_string());
        let found = earlier_candidates(&root, own, &[game, slot1]);
        let names: Vec<String> = found.iter().map(|c| c.0.file_name().unwrap().to_string_lossy().into_owned()).collect();
        let mut want = vec![(older as u32).to_string(), (newer as u32).to_string(), (empty as u32).to_string()];
        want.sort();
        assert_eq!(names, want);
        assert_eq!(pick_newest(&found), Some(base.join((newer as u32).to_string())), "the newest files win");
        // The SteamID64 form too, and a lone candidate wins as it is.
        let wide = base.join("wide");
        std::fs::create_dir_all(wide.join(format!("{older}"))).unwrap();
        std::fs::write(wide.join(format!("{older}")).join("a"), "x").unwrap();
        let found = earlier_candidates(&wide.join(own.to_string()), own, &[game]);
        assert_eq!(pick_newest(&found), Some(wide.join(older.to_string())));
        // Only empty ones: none.
        assert_eq!(pick_newest(&[(base.join("x"), None)]), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn peerparty_and_temp_folders_never_get_a_twin() {
        let roots = vec![r"C:\Users\x\AppData\Local\peerparty".to_string(), r"C:\Users\x\Tmp\".to_string()];
        assert!(under_any(r"C:\Users\x\AppData\Local\PeerParty\galaxy\g\ab\cloud\saves\cloud.sav", &roots));
        assert!(under_any(r"c:/users/x/tmp/work/saves/a.sav", &roots));
        assert!(!under_any(r"C:\Users\x\Tmp2\saves\a.sav", &roots));
        assert!(!under_any(r"C:\Games\G\Saves\a.sav", &roots));
    }

    #[test]
    fn devices_steams_account_folders_and_temp_files_are_not_saves() {
        let temp = Some(r"C:\Users\x\Tmp");
        for ok in [
            r"C:\Users\x\AppData\LocalLow\Co\Game\76561202255233024",
            r"C:\Users\x\AppData\Local\Co\Game\Saved\SaveGames\4660",
            r"D:\Games\Game\Saves\profile_4660.sav",
            r"\\?\C:\Users\x\Saved Games\Co\4660",
            r"\\nas\share\saves\4660",
        ] {
            assert!(worth_copying(ok, temp), "{ok}");
        }
        for not in [
            r"\\.\pipe\peerparty-4660",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\Device\4660",
            r"\\?\pipe\x_4660",
            r"C:\Program Files (x86)\Steam\userdata\4660",
            r"C:\Program Files (x86)\Steam\UserData\4660\",
            r"C:\Users\x\Tmp\4660",
            r"c:/users/x/tmp/game/4660",
        ] {
            assert!(!worth_copying(not, temp), "{not}");
        }
        assert!(worth_copying(r"C:\Users\x\Tmp2\4660", temp), "only under the temp folder itself");
        assert!(worth_copying(r"C:\Users\x\Tmp\4660", None));
    }

    /// A second copy's `.peer` file holds its hex peer id: never one of player 1's
    /// copied over it (seen in a real two-copy start).
    #[test]
    fn peerpartys_own_peer_board_and_identity_are_not_saves() {
        for not in [
            r"C:\Users\x\AppData\Local\peerparty\peers\2b905cc8ae6b74b26cba924d66b5139e.peer",
            r"C:\Users\x\AppData\Local\PeerParty\Identity\2b905cc8ae6b74b26cba924d66b5139e",
        ] {
            assert!(!worth_copying(not, None), "{not}");
        }
        // PeerParty's stand-in cloud saves are a player's save like any other.
        assert!(worth_copying(r"C:\Users\x\AppData\Local\peerparty\galaxy\game\2b905cc8ae6b74b26cba924d66b5139e", None));
        assert!(worth_copying(r"C:\Users\x\AppData\Local\Co\peers\2b905cc8ae6b74b26cba924d66b5139e", None));
    }

    /// The first slot's id is read from its key file, as the core makes it from the key.
    #[test]
    fn the_first_slot_is_its_keys_id() {
        use peerparty_core::inet::public::IdentityKey;
        use peerparty_core::{Identity, PeerId};
        let dir = std::env::temp_dir().join(format!("peerparty-slot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let me = Identity::claim(&dir);
        assert_eq!(me.slot, 0);
        let text = std::fs::read_to_string(dir.join("slot-0.ed25519")).unwrap();
        assert_eq!(id_of_key_file(&text), Some(me.id));
        let seed = [7u8; 32];
        let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(id_of_key_file(&format!("{hex}\r\n")), Some(PeerId::of_key(&IdentityKey::from_seed(seed).public())));
        for bad in ["", "abc", &hex[..63], &format!("{}zz", &hex[..62])] {
            assert_eq!(id_of_key_file(bad), None, "{bad:?}");
        }
        drop(me);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tree_is_sized_up_to_the_cap() {
        let base = std::env::temp_dir().join(format!("peerparty-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("d")).unwrap();
        std::fs::write(base.join("a"), [0u8; 100]).unwrap();
        std::fs::write(base.join("d").join("b"), [0u8; 50]).unwrap();
        assert_eq!(size_of_tree(&base, 1000), 150);
        assert!(size_of_tree(&base, 10) > 10);
        let _ = std::fs::remove_dir_all(&base);
    }
}
