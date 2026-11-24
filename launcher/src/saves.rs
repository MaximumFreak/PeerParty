//! The save manager: every player's saves for a game, and copying, swapping, resetting
//! and restoring them between any two players while the game isn't running.
//!
//! Player 1's saves are where the game keeps them (the folders its copy was seen writing,
//! `discovered.txt`, and its registry keys); players 2–4's are in PeerParty's data folder
//! (protocol/src/saves.rs has the layout, the hook sends their game there). Everything a
//! player has is taken as one snapshot:
//!
//! ```text
//! fs\<Zone>\<rel>\...   files, by zone (player 1's real folders are copied in)
//! registry.json         the game's keys under HKCU\Software, as data
//! steam-remote\         Steam Remote Storage files
//! stores\<sdk>\<name>   PeerParty's stand-ins' stores of this player (EOS, Galaxy,
//!                       PlayFab, Unity Gaming Services), by the player's ids
//! snapshot.txt          what it holds and, for player 1's, where each came from
//! ```
//!
//! A copy makes the source's snapshot in a staging folder, renames the ids in it (a save
//! keyed by player 1's SteamID becomes player 2's), backs up the destination's own as a
//! snapshot in `player-N\backups\<time>-<why>` (the newest `KEEP_BACKUPS` are kept), and
//! only then puts it in place: a copy that fails part-way leaves the destination as it
//! was. Links and junctions inside saves are never followed, and a save over `MAX_COPY`
//! isn't copied.
//!
//! Player 1's Steam Cloud files are Steam's: they are copied to other players, never
//! written.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use peerparty_protocol::saves::{self as layout, Discovered, Roots, Zone};

/// Backups kept per player.
pub const KEEP_BACKUPS: usize = 5;
/// A player's saves bigger than this aren't copied.
pub const MAX_COPY: u64 = 1 << 30;
/// PeerParty's stand-ins' stores that keep a player's data by its ids.
const STORES: &[(&str, SourceKind)] = &[("eos-storage", SourceKind::Eos), ("galaxy", SourceKind::Galaxy), ("playfab", SourceKind::PlayFab), ("ugs", SourceKind::Ugs)];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    /// A copy of the game is running (as this player, 1-based).
    Running(u32),
    Same,
    BadPlayer(u32),
    TooBig { what: String, mb: u64 },
    NoBackup(String),
    Io(String),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Running(p) => write!(f, "the game is running as player {p}: close it to manage saves"),
            SaveError::Same => write!(f, "that's the same player"),
            SaveError::BadPlayer(p) => write!(f, "no player {p} (players are 1 to {})", layout::MAX_PLAYERS),
            SaveError::TooBig { what, mb } => write!(f, "{what} is over {mb} MB; not copied"),
            SaveError::NoBackup(id) => write!(f, "no backup {id:?}"),
            SaveError::Io(e) => write!(f, "{e}"),
        }
    }
}

fn io(what: &str, p: &Path, e: std::io::Error) -> SaveError {
    SaveError::Io(format!("{what} {}: {e}", p.display()))
}

/// What a player's saves are made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Files,
    Prefs,
    Steam,
    Eos,
    Galaxy,
    PlayFab,
    Ugs,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Files => "Files",
            SourceKind::Prefs => "Prefs",
            SourceKind::Steam => "Steam",
            SourceKind::Eos => "EOS",
            SourceKind::Galaxy => "Galaxy",
            SourceKind::PlayFab => "PlayFab",
            SourceKind::Ugs => "UGS",
        }
    }
}

/// One backup of a player's saves.
#[derive(Clone, Debug, PartialEq)]
pub struct Backup {
    /// Its folder's name, `<yyyymmdd-hhmmss>-<why>`.
    pub id: String,
    pub path: PathBuf,
    pub when: Option<SystemTime>,
}

/// One player's saves, as the manager lists them.
#[derive(Clone, Debug, Default)]
pub struct PlayerSaves {
    /// 0 for player 1.
    pub slot: u32,
    pub files: usize,
    pub bytes: u64,
    pub newest: Option<SystemTime>,
    pub kinds: Vec<SourceKind>,
    pub backups: Vec<Backup>,
    /// Its folder (player 1's: the first of its saves' folders, if known).
    pub folder: Option<PathBuf>,
    pub running: bool,
}

impl PlayerSaves {
    pub fn has_saves(&self) -> bool {
        !self.kinds.is_empty()
    }
}

/// What a copy, swap, reset or restore did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub files: usize,
    pub keys: usize,
    /// The backup made of what was replaced.
    pub backup: Option<String>,
    /// What was left out, and why.
    pub notes: Vec<String>,
}

/// One game's saves.
pub struct GameSaves {
    pub id: String,
    pub data: PathBuf,
    pub roots: Roots,
    /// Where the game's identity keys are (`Identity::default_dir`).
    pub identity: PathBuf,
    /// The game's exe (for Steam's app id).
    pub exe: Option<PathBuf>,
}

impl GameSaves {
    /// The saves of the game the launcher knows by `key` (its listed exe, lowercase:
    /// `store::key_of`), in `game_dir` (its folder, else the exe's).
    pub fn for_game(key: &str, exe: Option<&Path>, game_dir: Option<&Path>) -> GameSaves {
        let id = layout::game_id(key);
        let data = crate::platform::data_dir();
        let mut roots = known_roots();
        roots.game_dir = game_dir.map(|d| d.to_string_lossy().into_owned()).or_else(|| exe.and_then(|e| e.parent()).map(|d| d.to_string_lossy().into_owned()));
        roots.shared = vec![data.to_string_lossy().into_owned(), std::env::temp_dir().to_string_lossy().into_owned()];
        // The launcher's own record of where the game was last started from wins.
        if let Some(m) = Manifest::read(&layout::game_dir(&data, &id)) {
            if roots.game_dir.is_none() || m.game_root.is_some() {
                roots.game_dir = m.game_root.or(roots.game_dir);
            }
            roots.extra = m.save_paths;
        }
        GameSaves { id, data, roots, identity: peerparty_core::Identity::default_dir(), exe: exe.map(Path::to_path_buf) }
    }

    fn game_dir(&self) -> PathBuf {
        layout::game_dir(&self.data, &self.id)
    }

    fn player_dir(&self, slot: u32) -> PathBuf {
        layout::player_dir(&self.data, &self.id, slot)
    }

    /// Where player 1's saves were seen (logs and caches in the game's folder left
    /// out), and where a Unity game keeps them before it was ever seen there.
    pub fn discovered(&self) -> Vec<Discovered> {
        let text = std::fs::read_to_string(self.player_dir(0).join(layout::DISCOVERED)).unwrap_or_default();
        let mut all: Vec<Discovered> = Discovered::parse_all(&text)
            .into_iter()
            .filter(|d| !matches!(d, Discovered::Files { zone: Zone::GameDir, rel, .. } if layout::noise_in_game_dir(rel)))
            .collect();
        for guess in self.unity_saves() {
            let same = |d: &Discovered| match (d, &guess) {
                (Discovered::Files { zone: a, rel: r, .. }, Discovered::Files { zone: b, rel: s, .. }) => a == b && r.eq_ignore_ascii_case(s),
                (Discovered::Registry(a), Discovered::Registry(b)) => a.eq_ignore_ascii_case(b),
                _ => false,
            };
            if !all.iter().any(same) {
                all.push(guess);
            }
        }
        all
    }

    /// A Unity game's saves before player 1 ran it under PeerParty: its
    /// `persistentDataPath` (`LocalLow\<company>\<product>`) and its PlayerPrefs key
    /// (`HKCU\Software\<company>\<product>`), named by `<exe>_Data\app.info`, when they're
    /// there. Without it the card said player 1 had no saves until the first run.
    fn unity_saves(&self) -> Vec<Discovered> {
        let Some(exe) = self.exe.as_deref() else { return Vec::new() };
        let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem()) else { return Vec::new() };
        let mut data = stem.to_os_string();
        data.push("_Data");
        let info = dir.join(data).join("app.info");
        // Two short lines, company then product; anything else isn't Unity's.
        if std::fs::metadata(&info).map_or(true, |m| m.len() > 4096) {
            return Vec::new();
        }
        let text = std::fs::read_to_string(&info).unwrap_or_default();
        let mut lines = text.lines().map(str::trim);
        let ok = |s: &&str| !s.is_empty() && s.len() <= 128 && *s != "." && *s != ".." && !s.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']);
        let (Some(company), Some(product)) = (lines.next().filter(ok), lines.next().filter(ok)) else { return Vec::new() };
        let rel = format!("{company}\\{product}");
        let mut out = Vec::new();
        if let Some(real) = layout::real_path(&self.roots, Zone::LocalLow, &rel).filter(|r| Path::new(r).is_dir()) {
            out.push(Discovered::Files { zone: Zone::LocalLow, rel: rel.clone(), real });
        }
        let key = format!("Software\\{rel}");
        if layout::game_registry_key(&key).is_some() && reg::exists(&key) {
            out.push(Discovered::Registry(key));
        }
        out
    }

    /// Where player 1's saves are best shown: a game's folder in Windows' data folders
    /// before one in its own folder, a folder of saves there before any other file.
    fn best_place(&self) -> Option<(Zone, String, String)> {
        let rank = |zone: Zone, rel: &str| match zone {
            Zone::GameDir if layout::strict_in_game_dir(rel) => 1,
            Zone::GameDir => 2,
            _ => 0,
        };
        self.discovered()
            .into_iter()
            .filter_map(|d| match d {
                Discovered::Files { zone, rel, real } if Path::new(&real).exists() => Some((zone, rel, real)),
                _ => None,
            })
            .min_by_key(|(zone, rel, _)| rank(*zone, rel))
    }

    /// Notes where player 1's saves are by hand (a folder the player picked).
    pub fn add_discovered_folder(&self, folder: &Path) -> Result<(), SaveError> {
        let full = folder.to_string_lossy().into_owned();
        let Some((zone, rel)) = layout::classify(&self.roots, &full) else {
            return Err(SaveError::Io(format!("{full} isn't a folder of the game's saves (Windows' data folders, Saved Games, Documents or the game's folder)")));
        };
        let mut all = self.discovered();
        let line = Discovered::Files { zone, rel, real: full };
        if !all.contains(&line) {
            all.push(line);
        }
        let dir = self.player_dir(0);
        std::fs::create_dir_all(&dir).map_err(|e| io("making", &dir, e))?;
        let text: String = all.iter().map(|d| d.to_line() + "\r\n").collect();
        std::fs::write(dir.join(layout::DISCOVERED), text).map_err(|e| io("writing", &dir, e))
    }

    // ---- who's running ----

    /// Holds player `slot` for a run of the game: another start as that player is
    /// refused while it's held (`PlayerLock`).
    pub fn lock_player(&self, slot: u32) -> Result<PlayerLock, SaveError> {
        if slot >= layout::MAX_PLAYERS {
            return Err(SaveError::BadPlayer(slot + 1));
        }
        let dir = self.player_dir(slot);
        std::fs::create_dir_all(&dir).map_err(|e| io("making", &dir, e))?;
        PlayerLock::take(&dir.join("running.lock")).ok_or(SaveError::Running(slot + 1))
    }

    /// The first player no run holds, from `from` up, held for this run.
    pub fn lock_first_free(&self, from: u32) -> Option<(u32, PlayerLock)> {
        (from..layout::MAX_PLAYERS).find_map(|s| self.lock_player(s).ok().map(|l| (s, l)))
    }

    /// The players a run of the game holds now.
    pub fn running(&self) -> Vec<u32> {
        (0..layout::MAX_PLAYERS)
            .filter(|s| {
                let p = self.player_dir(*s).join("running.lock");
                p.exists() && PlayerLock::take(&p).is_none()
            })
            .collect()
    }

    fn not_running(&self) -> Result<(), SaveError> {
        match self.running().first() {
            Some(s) => Err(SaveError::Running(s + 1)),
            None => Ok(()),
        }
    }

    // ---- listing ----

    /// Every player's saves (players 1 to 4).
    pub fn inspect(&self) -> Vec<PlayerSaves> {
        let running = self.running();
        let best = self.best_place();
        (0..layout::MAX_PLAYERS)
            .map(|slot| {
                let mut p = PlayerSaves { slot, running: running.contains(&slot), backups: self.backups(slot), ..PlayerSaves::default() };
                let add = |path: &Path, kind: SourceKind, p: &mut PlayerSaves| {
                    let (n, bytes, newest) = tree_stats(path);
                    if n > 0 {
                        p.files += n;
                        p.bytes += bytes;
                        p.newest = p.newest.max(newest);
                        if !p.kinds.contains(&kind) {
                            p.kinds.push(kind);
                        }
                    }
                };
                if slot == 0 {
                    for d in self.discovered() {
                        if let Discovered::Files { real, .. } = &d {
                            add(Path::new(real), SourceKind::Files, &mut p);
                        }
                        if let Discovered::Registry(k) = &d {
                            if reg::exists(k) && !p.kinds.contains(&SourceKind::Prefs) {
                                p.kinds.push(SourceKind::Prefs);
                            }
                        }
                    }
                    p.folder = best.as_ref().map(|(.., real)| PathBuf::from(real));
                } else {
                    let dir = self.player_dir(slot);
                    add(&dir.join("fs"), SourceKind::Files, &mut p);
                    add(&dir.join(layout::STEAM_REMOTE), SourceKind::Steam, &mut p);
                    if reg::exists(&layout::registry_key(&self.id, slot)) {
                        p.kinds.push(SourceKind::Prefs);
                    }
                    // The same game folder as player 1's, when this player has it.
                    let fs = dir.join("fs");
                    p.folder = best.as_ref().map(|(zone, rel, _)| layout::sandbox_path(&fs, *zone, rel)).filter(|f| f.exists()).or_else(|| Some(fs).filter(|f| f.exists()));
                }
                for (path, kind) in self.store_entries(slot) {
                    add(&path, kind, &mut p);
                }
                p
            })
            .collect()
    }

    /// A player's backups, newest first.
    pub fn backups(&self, slot: u32) -> Vec<Backup> {
        let dir = self.player_dir(slot).join(layout::BACKUPS);
        let mut v: Vec<Backup> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| Backup { id: e.file_name().to_string_lossy().into_owned(), path: e.path(), when: e.metadata().ok().and_then(|m| m.created().or_else(|_| m.modified()).ok()) })
            .collect();
        // Newest first: by the id's second, then by when the folder was made. By id alone,
        // two made in one second went by their reasons' names ("before-restore" ahead of
        // a later "before-reset"), and pruning to the newest five could drop the newest.
        let second = |b: &Backup| b.id.get(..15).map(str::to_string).unwrap_or_default();
        v.sort_by(|a, b| second(b).cmp(&second(a)).then(b.when.cmp(&a.when)).then(b.id.cmp(&a.id)));
        v
    }

    // ---- ids ----

    /// The tokens player `slot`'s ids are in save names as; player 1's include the
    /// game's own Steam account's, which its saves may be keyed by.
    fn tokens(&self, slot: u32) -> Vec<Vec<String>> {
        let Some(peer) = peerparty_core::Identity::ensure_slot(&self.identity, slot) else { return Vec::new() };
        let mut v = Vec::new();
        if slot == 0 {
            for d in self.discovered() {
                if let Discovered::SteamId(real) = d {
                    v.push(layout::id_tokens(&peer.0, real));
                }
            }
        }
        v.push(layout::id_tokens(&peer.0, layout::lan_steam_id(&peer.0)));
        v
    }

    /// (from, to) token lists for renaming `from`'s ids to `to`'s.
    fn remap_pairs(&self, from: u32, to: u32) -> (Vec<String>, Vec<String>) {
        let to_set = self.tokens(to).into_iter().next().unwrap_or_default();
        let (mut f, mut t) = (Vec::new(), Vec::new());
        for set in self.tokens(from) {
            for (a, b) in set.into_iter().zip(to_set.iter()) {
                f.push(a);
                t.push(b.clone());
            }
        }
        (f, t)
    }

    /// PeerParty's stores' entries for player `slot` (named by one of its ids).
    fn store_entries(&self, slot: u32) -> Vec<(PathBuf, SourceKind)> {
        let tokens: Vec<String> = self.tokens(slot).into_iter().flatten().collect();
        let Some(game) = self.exe.as_deref().and_then(|e| e.file_stem()).map(|s| s.to_string_lossy().to_ascii_lowercase()) else { return Vec::new() };
        let mut out = Vec::new();
        for (sdk, kind) in STORES {
            let base = self.data.join(sdk).join(&game);
            // Entries keyed by id are at most two folders down (`<game>\<id>`,
            // `<game>\<scope>\<id>.json`).
            for e in std::fs::read_dir(&base).into_iter().flatten().flatten().chain(
                std::fs::read_dir(&base).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten()),
            ) {
                let name = e.file_name().to_string_lossy().into_owned();
                if tokens.iter().any(|t| layout::remap_name(&name, std::slice::from_ref(t), &["\u{1}".to_string()]).is_some()) {
                    out.push((e.path(), *kind));
                }
            }
        }
        out
    }

    // ---- snapshots ----

    /// Player `slot`'s saves, copied into `into` (a new folder).
    fn capture(&self, slot: u32, into: &Path, report: &mut Report) -> Result<(), SaveError> {
        std::fs::create_dir_all(into).map_err(|e| io("making", into, e))?;
        let mut lines: Vec<String> = Vec::new();
        let mut budget = MAX_COPY;
        if slot == 0 {
            for d in self.discovered() {
                match d {
                    Discovered::Files { zone, rel, real } => {
                        let from = Path::new(&real);
                        if !from.exists() {
                            continue;
                        }
                        let to = layout::sandbox_path(&into.join("fs"), zone, &rel);
                        report.files += copy_tree(from, &to, &mut budget, &real)?;
                        lines.push(Discovered::Files { zone, rel, real }.to_line());
                    }
                    Discovered::Registry(k) => {
                        if reg::exists(&k) {
                            lines.push(Discovered::Registry(k).to_line());
                        }
                    }
                    Discovered::SteamId(_) | Discovered::SteamRemote => {}
                }
            }
            let keys: Vec<String> = lines.iter().filter_map(|l| match Discovered::from_line(l) {
                Some(Discovered::Registry(k)) => Some(k),
                _ => None,
            }).collect();
            report.keys += reg::export(&keys, "Software\\", &into.join("registry.json"))?;
            if let Some(remote) = self.steam_cloud_folder() {
                report.files += copy_tree(&remote, &into.join(layout::STEAM_REMOTE), &mut budget, "player 1's Steam Cloud files")?;
            }
        } else {
            let dir = self.player_dir(slot);
            for part in ["fs", layout::STEAM_REMOTE] {
                if dir.join(part).exists() {
                    report.files += copy_tree(&dir.join(part), &into.join(part), &mut budget, &format!("player {}'s saves", slot + 1))?;
                }
            }
            if dir.join(layout::WHITEOUTS).exists() {
                std::fs::copy(dir.join(layout::WHITEOUTS), into.join(layout::WHITEOUTS)).map_err(|e| io("copying", &dir, e))?;
            }
            report.keys += reg::export(&[layout::registry_key(&self.id, slot)], &format!("{}\\", layout::registry_key(&self.id, slot)), &into.join("registry.json"))?;
        }
        for (path, _) in self.store_entries(slot) {
            let Some(sdk) = path.strip_prefix(&self.data).ok().and_then(|r| r.components().next()).map(|c| c.as_os_str().to_owned()) else { continue };
            let rel = path.strip_prefix(self.data.join(&sdk)).unwrap_or(&path).to_path_buf();
            report.files += copy_tree(&path, &into.join("stores").join(&sdk).join(rel), &mut budget, "PeerParty's stores")?;
        }
        lines.insert(0, format!("player\t{}", slot + 1));
        std::fs::write(into.join("snapshot.txt"), lines.join("\r\n") + "\r\n").map_err(|e| io("writing", into, e))
    }

    /// Renames `from`'s ids to `to`'s in a snapshot's file and folder names.
    fn remap(&self, snap: &Path, from: u32, to: u32) -> Result<usize, SaveError> {
        let (f, t) = self.remap_pairs(from, to);
        if f.is_empty() {
            return Ok(0);
        }
        let mut n = 0;
        for part in ["fs", layout::STEAM_REMOTE, "stores"] {
            n += rename_tokens(&snap.join(part), &f, &t)?;
        }
        Ok(n)
    }

    /// Puts snapshot `snap` in place as player `slot`'s saves, its own replaced.
    fn install(&self, snap: &Path, slot: u32, report: &mut Report) -> Result<(), SaveError> {
        let reg_file = snap.join("registry.json");
        if slot == 0 {
            // Each game folder of the snapshot goes where player 1's game keeps it.
            let known = self.discovered();
            for (zone, rel) in seed_entries(&snap.join("fs")) {
                let real = known.iter().find_map(|d| match d {
                    Discovered::Files { zone: z, rel: r, real } if *z == zone && r.eq_ignore_ascii_case(&rel) => Some(real.clone()),
                    _ => None,
                });
                let real = match (zone, real) {
                    (_, Some(r)) => r,
                    // The game's own folder: only its folders of saves.
                    (Zone::GameDir, None) if layout::strict_in_game_dir(&rel) => match layout::real_path(&self.roots, zone, &rel) {
                        Some(r) => r,
                        None => continue,
                    },
                    (Zone::GameDir | Zone::SteamUserdata, None) => {
                        report.notes.push(format!("{}\\{rel} left out: player 1's game was never seen using it", zone.dir_name()));
                        continue;
                    }
                    (_, None) => match layout::real_path(&self.roots, zone, &rel) {
                        Some(r) => r,
                        None => continue,
                    },
                };
                let from = layout::sandbox_path(&snap.join("fs"), zone, &rel);
                let to = PathBuf::from(&real);
                remove_any(&to).map_err(|e| io("replacing", &to, e))?;
                report.files += copy_tree(&from, &to, &mut u64::MAX.clone(), &real)?;
            }
            // Keys: each of the snapshot's under Software.
            report.keys += reg::import(&reg_file, "Software\\", true)?;
            if snap.join(layout::STEAM_REMOTE).exists() {
                report.notes.push("Steam Cloud files are left as Steam has them for player 1".into());
            }
        } else {
            let dir = self.player_dir(slot);
            std::fs::create_dir_all(&dir).map_err(|e| io("making", &dir, e))?;
            for part in ["fs", layout::STEAM_REMOTE] {
                remove_any(&dir.join(part)).map_err(|e| io("replacing", &dir.join(part), e))?;
                if snap.join(part).exists() {
                    report.files += copy_tree(&snap.join(part), &dir.join(part), &mut u64::MAX.clone(), &format!("player {}'s saves", slot + 1))?;
                }
            }
            let _ = std::fs::remove_file(dir.join(layout::WHITEOUTS));
            if snap.join(layout::WHITEOUTS).exists() {
                std::fs::copy(snap.join(layout::WHITEOUTS), dir.join(layout::WHITEOUTS)).map_err(|e| io("copying", &dir, e))?;
            }
            let key = layout::registry_key(&self.id, slot);
            reg::delete(&key);
            report.keys += reg::import(&reg_file, &format!("{key}\\"), false)?;
        }
        // PeerParty's stores: this player's entries replaced by the snapshot's.
        for (path, _) in self.store_entries(slot) {
            remove_any(&path).map_err(|e| io("replacing", &path, e))?;
        }
        let stores = snap.join("stores");
        if stores.exists() {
            report.files += copy_tree(&stores, &self.data, &mut u64::MAX.clone(), "PeerParty's stores")?;
        }
        Ok(())
    }

    /// Clears player `slot`'s saves (after a backup).
    fn clear(&self, slot: u32) -> Result<(), SaveError> {
        if slot == 0 {
            for d in self.discovered() {
                match d {
                    Discovered::Files { real, .. } => remove_any(Path::new(&real)).map_err(|e| io("removing", Path::new(&real), e))?,
                    Discovered::Registry(k) => reg::delete(&k),
                    _ => {}
                }
            }
        } else {
            let dir = self.player_dir(slot);
            for part in ["fs", layout::STEAM_REMOTE, layout::WHITEOUTS] {
                remove_any(&dir.join(part)).map_err(|e| io("removing", &dir.join(part), e))?;
            }
            reg::delete(&layout::registry_key(&self.id, slot));
        }
        for (path, _) in self.store_entries(slot) {
            remove_any(&path).map_err(|e| io("removing", &path, e))?;
        }
        Ok(())
    }

    /// Backs up player `slot`'s saves as `why`; the oldest beyond `KEEP_BACKUPS` go.
    fn backup(&self, slot: u32, why: &str) -> Result<String, SaveError> {
        let [y, mo, d, h, mi, s] = crate::platform::local_time();
        let mut id = format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{why}");
        let dir = self.player_dir(slot).join(layout::BACKUPS);
        let mut n = 2;
        while dir.join(&id).exists() {
            id = format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{why}-{n}");
            n += 1;
        }
        let into = dir.join(&id);
        let mut r = Report::default();
        if let Err(e) = self.capture(slot, &into, &mut r) {
            let _ = std::fs::remove_dir_all(&into);
            return Err(e);
        }
        for old in self.backups(slot).into_iter().skip(KEEP_BACKUPS) {
            let _ = std::fs::remove_dir_all(&old.path);
        }
        Ok(id)
    }

    fn staging(&self, what: &str) -> PathBuf {
        let p = self.game_dir().join(format!(".staging-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn check(&self, slots: &[u32]) -> Result<(), SaveError> {
        for s in slots {
            if *s >= layout::MAX_PLAYERS {
                return Err(SaveError::BadPlayer(s + 1));
            }
        }
        self.not_running()
    }

    // ---- what the player asks for ----

    /// Player `from`'s saves become player `to`'s too (0 is player 1); `to`'s own are
    /// backed up first.
    pub fn copy(&self, from: u32, to: u32) -> Result<Report, SaveError> {
        if from == to {
            return Err(SaveError::Same);
        }
        self.check(&[from, to])?;
        let stage = self.staging("copy");
        let mut report = Report::default();
        let staged = self.capture(from, &stage, &mut report).and_then(|()| self.remap(&stage, from, to));
        if let Err(e) = staged {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
        let backup = match self.backup(to, &format!("before-copy-from-p{}", from + 1)) {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&stage);
                return Err(e);
            }
        };
        report.backup = Some(backup.clone());
        let mut put = Report::default();
        let done = self.install(&stage, to, &mut put);
        let _ = std::fs::remove_dir_all(&stage);
        report.notes.extend(put.notes);
        if let Err(e) = done {
            // What was there comes back.
            let _ = self.restore_from(to, &self.player_dir(to).join(layout::BACKUPS).join(&backup));
            return Err(e);
        }
        Ok(report)
    }

    /// Players `a` and `b` swap saves (both backed up first).
    pub fn swap(&self, a: u32, b: u32) -> Result<Report, SaveError> {
        if a == b {
            return Err(SaveError::Same);
        }
        self.check(&[a, b])?;
        let (sa, sb) = (self.staging("swap-a"), self.staging("swap-b"));
        let mut report = Report::default();
        let staged = self
            .capture(a, &sa, &mut report)
            .and_then(|()| self.capture(b, &sb, &mut Report::default()))
            .and_then(|()| self.remap(&sa, a, b))
            .and_then(|_| self.remap(&sb, b, a));
        let cleanup = || {
            let _ = std::fs::remove_dir_all(&sa);
            let _ = std::fs::remove_dir_all(&sb);
        };
        if let Err(e) = staged {
            cleanup();
            return Err(e);
        }
        let ba = self.backup(a, &format!("before-swap-with-p{}", b + 1));
        let bb = self.backup(b, &format!("before-swap-with-p{}", a + 1));
        let (Ok(ba), Ok(bb)) = (ba, bb) else {
            cleanup();
            return Err(SaveError::Io("the players' saves couldn't be backed up; nothing was swapped".into()));
        };
        let mut put = Report::default();
        let done = self.install(&sa, b, &mut put).and_then(|()| self.install(&sb, a, &mut put));
        cleanup();
        report.notes.extend(put.notes);
        report.backup = Some(format!("{ba}, {bb}"));
        if let Err(e) = done {
            let _ = self.restore_from(a, &self.player_dir(a).join(layout::BACKUPS).join(&ba));
            let _ = self.restore_from(b, &self.player_dir(b).join(layout::BACKUPS).join(&bb));
            return Err(e);
        }
        Ok(report)
    }

    /// Player `slot` starts fresh; what it had is a backup.
    pub fn reset(&self, slot: u32) -> Result<Report, SaveError> {
        self.check(&[slot])?;
        let backup = self.backup(slot, "before-reset")?;
        self.clear(slot)?;
        Ok(Report { backup: Some(backup), ..Report::default() })
    }

    /// Player `slot`'s saves become backup `id` again; what it has now is a backup.
    pub fn restore(&self, slot: u32, id: &str) -> Result<Report, SaveError> {
        self.check(&[slot])?;
        let src = self.player_dir(slot).join(layout::BACKUPS).join(id);
        if id.contains(['/', '\\']) || id.starts_with('.') || !src.join("snapshot.txt").exists() {
            return Err(SaveError::NoBackup(id.to_string()));
        }
        // Copied out first: the backup made next may rotate this one away.
        let stage = self.staging("restore");
        let mut report = Report::default();
        if let Err(e) = copy_tree(&src, &stage, &mut u64::MAX.clone(), id) {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
        let backup = self.backup(slot, "before-restore")?;
        let done = self.install(&stage, slot, &mut report);
        let _ = std::fs::remove_dir_all(&stage);
        report.backup = Some(backup);
        done.map(|()| report)
    }

    fn restore_from(&self, slot: u32, backup: &Path) -> Result<(), SaveError> {
        self.clear(slot)?;
        self.install(backup, slot, &mut Report::default())
    }

    /// The backups of every player beyond the newest one, removed.
    pub fn clean_backups(&self, slot: u32) -> usize {
        let mut n = 0;
        for old in self.backups(slot).into_iter().skip(1) {
            if std::fs::remove_dir_all(&old.path).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Player 1's Steam Cloud files on this PC (`<Steam>\userdata\<account>\<app>\remote`),
    /// when the game was seen using them.
    fn steam_cloud_folder(&self) -> Option<PathBuf> {
        let found = self.discovered();
        if !found.contains(&Discovered::SteamRemote) {
            return None;
        }
        let account = found.iter().find_map(|d| if let Discovered::SteamId(id) = d { Some(*id as u32) } else { None })?;
        let exe = self.exe.as_deref()?;
        let app = crate::steamid::Game::look(exe, self.roots.game_dir.as_deref().map(Path::new)).app_id(exe)?;
        let dir = reg::steam_path()?.join("userdata").join(account.to_string()).join(app).join("remote");
        dir.is_dir().then_some(dir)
    }

    /// Notes the game for the save manager (`manifest.json`), at each start.
    pub fn note_start(&self, exe: &Path, game_root: Option<&Path>, save_paths: &[String]) {
        let m = Manifest { exe: exe.to_string_lossy().into_owned(), game_root: game_root.map(|r| r.to_string_lossy().into_owned()), save_paths: save_paths.to_vec() };
        m.write(&self.game_dir());
    }

    /// Whether any later player has saves of its own here.
    pub fn later_players_have_saves(&self) -> bool {
        (1..layout::MAX_PLAYERS).any(|s| self.player_dir(s).join("fs").exists() || self.player_dir(s).join(layout::STEAM_REMOTE).exists())
    }
}

/// The launcher's note of a game (`manifest.json`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Manifest {
    exe: String,
    game_root: Option<String>,
    save_paths: Vec<String>,
}

impl Manifest {
    fn read(dir: &Path) -> Option<Manifest> {
        serde_json::from_str(&std::fs::read_to_string(dir.join(layout::MANIFEST)).ok()?).ok()
    }

    fn write(&self, dir: &Path) {
        if std::fs::create_dir_all(dir).is_ok() {
            if let Ok(text) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(dir.join(layout::MANIFEST), text);
            }
        }
    }
}

/// A run's hold on a player: an open lock file no one else may open.
pub struct PlayerLock {
    _file: std::fs::File,
}

impl PlayerLock {
    fn take(path: &Path) -> Option<PlayerLock> {
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            o.share_mode(0);
        }
        let f = o.open(path).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return None;
            }
        }
        Some(PlayerLock { _file: f })
    }
}

/// Windows' folders for programs' data, as Explorer has them.
pub fn known_roots() -> Roots {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, FOLDERID_LocalAppData, FOLDERID_LocalAppDataLow, FOLDERID_RoamingAppData, FOLDERID_SavedGames, SHGetKnownFolderPath};
        let get = |id: &windows_sys::core::GUID| -> Option<String> {
            let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
            let ok = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) } >= 0 && !p.is_null();
            let s = ok.then(|| {
                let mut n = 0;
                while unsafe { *p.add(n) } != 0 {
                    n += 1;
                }
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
            });
            if !p.is_null() {
                unsafe { windows_sys::Win32::System::Com::CoTaskMemFree(p.cast()) };
            }
            s
        };
        Roots {
            local_low: get(&FOLDERID_LocalAppDataLow),
            local: get(&FOLDERID_LocalAppData),
            roaming: get(&FOLDERID_RoamingAppData),
            documents: get(&FOLDERID_Documents),
            saved_games: get(&FOLDERID_SavedGames),
            ..Roots::default()
        }
    }
    #[cfg(not(windows))]
    {
        Roots::default()
    }
}

/// The game folders (`seed_root`s) a snapshot's `fs` holds, by zone.
fn seed_entries(fs: &Path) -> Vec<(Zone, String)> {
    let mut out = Vec::new();
    for zone in Zone::ALL {
        let base = fs.join(zone.dir_name());
        // Down to the seed depth (the game's folder in the zone); in the game's own
        // folder, each file.
        fn walk(base: &Path, p: &Path, zone: Zone, out: &mut Vec<(Zone, String)>) {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                let path = e.path();
                let rel = path.strip_prefix(base).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                let deep_enough = if zone == Zone::GameDir { !is_dir } else { rel.split('\\').count() >= zone.seed_depth(&rel) };
                if deep_enough || !is_dir {
                    out.push((zone, rel));
                } else {
                    walk(base, &path, zone, out);
                }
            }
        }
        walk(&base, &base, zone, &mut out);
    }
    out
}

/// Files, bytes and the newest file's time under `p` (looking at no more than 50,000).
fn tree_stats(p: &Path) -> (usize, u64, Option<SystemTime>) {
    fn walk(p: &Path, acc: &mut (usize, u64, Option<SystemTime>), seen: &mut usize) {
        let Ok(meta) = std::fs::symlink_metadata(p) else { return };
        *seen += 1;
        if *seen > 50_000 || meta.file_type().is_symlink() {
            return;
        }
        if meta.is_dir() {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                walk(&e.path(), acc, seen);
            }
        } else {
            acc.0 += 1;
            acc.1 += meta.len();
            acc.2 = acc.2.max(meta.modified().ok());
        }
    }
    let mut acc = (0, 0, None);
    walk(p, &mut acc, &mut 0);
    acc
}

/// Copies `from` (a file or a folder) to `to`, links and junctions left out; `budget` is
/// what may still be copied. The files copied.
fn copy_tree(from: &Path, to: &Path, budget: &mut u64, what: &str) -> Result<usize, SaveError> {
    let meta = std::fs::symlink_metadata(from).map_err(|e| io("reading", from, e))?;
    if meta.file_type().is_symlink() || is_junction(from) {
        return Ok(0);
    }
    if meta.is_dir() {
        std::fs::create_dir_all(to).map_err(|e| io("making", to, e))?;
        let mut n = 0;
        for e in std::fs::read_dir(from).map_err(|e| io("reading", from, e))?.flatten() {
            n += copy_tree(&e.path(), &to.join(e.file_name()), budget, what)?;
        }
        return Ok(n);
    }
    if meta.len() > *budget {
        return Err(SaveError::TooBig { what: what.to_string(), mb: MAX_COPY >> 20 });
    }
    *budget -= meta.len();
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io("making", parent, e))?;
    }
    std::fs::copy(from, to).map_err(|e| io("copying", from, e))?;
    Ok(1)
}

#[cfg(windows)]
fn is_junction(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const REPARSE: u32 = 0x400;
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_attributes() & REPARSE != 0)
}

#[cfg(not(windows))]
fn is_junction(_: &Path) -> bool {
    false
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() && !is_junction(p) => std::fs::remove_dir_all(p),
        Ok(m) if m.is_dir() || is_junction(p) => std::fs::remove_dir(p),
        Ok(_) => std::fs::remove_file(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Renames entries under `dir` whose names hold one of `from`'s tokens to `to`'s
/// (deepest first). The entries renamed.
fn rename_tokens(dir: &Path, from: &[String], to: &[String]) -> Result<usize, SaveError> {
    let mut n = 0;
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(0) };
    for e in entries.flatten() {
        let path = e.path();
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            n += rename_tokens(&path, from, to)?;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(new) = layout::remap_name(&name, from, to) {
            let dest = path.with_file_name(&new);
            remove_any(&dest).map_err(|e| io("replacing", &dest, e))?;
            std::fs::rename(&path, &dest).map_err(|e| io("renaming", &path, e))?;
            n += 1;
        }
    }
    Ok(n)
}

/// The game's registry keys as data (`registry.json`): a key's values and subkeys.
mod reg {
    use super::*;

    #[cfg(windows)]
    use windows_sys::Win32::System::Registry::*;

    /// How deep a key's subkeys are followed, and how many keys at most.
    const MAX_DEPTH: usize = 16;
    const MAX_KEYS: usize = 20_000;

    #[cfg(windows)]
    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    #[cfg(windows)]
    fn open(path: &str, write: bool) -> Option<HKEY> {
        let mut h: HKEY = std::ptr::null_mut();
        let rc = if write {
            unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, w(path).as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_ALL_ACCESS, std::ptr::null(), &mut h, std::ptr::null_mut()) }
        } else {
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, w(path).as_ptr(), 0, KEY_READ, &mut h) }
        };
        (rc == 0).then_some(h)
    }

    pub fn exists(path: &str) -> bool {
        #[cfg(windows)]
        {
            open(path, false).map(|h| unsafe { RegCloseKey(h) }).is_some()
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            false
        }
    }

    pub fn delete(path: &str) {
        #[cfg(windows)]
        unsafe {
            RegDeleteTreeW(HKEY_CURRENT_USER, w(path).as_ptr());
            RegDeleteKeyW(HKEY_CURRENT_USER, w(path).as_ptr());
        }
        #[cfg(not(windows))]
        let _ = path;
    }

    #[cfg(windows)]
    fn dump(h: HKEY, depth: usize, count: &mut usize) -> serde_json::Value {
        let mut values = Vec::new();
        let mut keys = serde_json::Map::new();
        *count += 1;
        let mut i = 0;
        loop {
            let mut name = vec![0u16; 16384];
            let mut name_len = name.len() as u32;
            let mut kind = 0u32;
            let mut len = 0u32;
            let rc = unsafe { RegEnumValueW(h, i, name.as_mut_ptr(), &mut name_len, std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut len) };
            if rc != 0 {
                break;
            }
            let mut data = vec![0u8; len as usize];
            name_len = name.len() as u32;
            let rc = unsafe { RegEnumValueW(h, i, name.as_mut_ptr(), &mut name_len, std::ptr::null(), &mut kind, data.as_mut_ptr(), &mut len) };
            if rc == 0 {
                data.truncate(len as usize);
                let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
                values.push(serde_json::json!({"name": String::from_utf16_lossy(&name[..name_len as usize]), "type": kind, "data": hex}));
            }
            i += 1;
        }
        if depth < MAX_DEPTH {
            let mut j = 0;
            loop {
                let mut name = vec![0u16; 256];
                let mut name_len = name.len() as u32;
                let rc = unsafe { RegEnumKeyExW(h, j, name.as_mut_ptr(), &mut name_len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
                if rc != 0 || *count >= MAX_KEYS {
                    break;
                }
                let sub = String::from_utf16_lossy(&name[..name_len as usize]);
                let mut sh: HKEY = std::ptr::null_mut();
                if unsafe { RegOpenKeyExW(h, w(&sub).as_ptr(), 0, KEY_READ, &mut sh) } == 0 {
                    keys.insert(sub, dump(sh, depth + 1, count));
                    unsafe { RegCloseKey(sh) };
                }
                j += 1;
            }
        }
        serde_json::json!({"values": values, "keys": keys})
    }

    /// Writes keys `paths` (under HKCU) to `file`, each by its path after `strip`. The
    /// keys written.
    pub fn export(paths: &[String], strip: &str, file: &Path) -> Result<usize, SaveError> {
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut out = serde_json::Map::new();
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut count = 0;
        #[cfg(windows)]
        for p in paths {
            let Some(h) = open(p, false) else { continue };
            let tree = dump(h, 0, &mut count);
            unsafe { RegCloseKey(h) };
            let rel = p.get(strip.len()..).filter(|_| p.to_ascii_lowercase().starts_with(&strip.to_ascii_lowercase())).unwrap_or("").to_string();
            out.insert(rel, tree);
        }
        #[cfg(not(windows))]
        let _ = (paths, strip);
        if out.is_empty() {
            return Ok(0);
        }
        let text = serde_json::to_string_pretty(&serde_json::Value::Object(out)).map_err(|e| SaveError::Io(e.to_string()))?;
        std::fs::write(file, text).map_err(|e| io("writing", file, e))?;
        Ok(count)
    }

    #[cfg(windows)]
    fn load(h: HKEY, tree: &serde_json::Value, depth: usize) -> usize {
        let mut n = 1;
        for v in tree["values"].as_array().into_iter().flatten() {
            let (Some(name), Some(kind), Some(hex)) = (v["name"].as_str(), v["type"].as_u64(), v["data"].as_str()) else { continue };
            let data: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()).collect();
            unsafe { RegSetValueExW(h, w(name).as_ptr(), 0, kind as u32, data.as_ptr(), data.len() as u32) };
        }
        if depth < MAX_DEPTH {
            for (name, sub) in tree["keys"].as_object().into_iter().flatten() {
                let mut sh: HKEY = std::ptr::null_mut();
                if unsafe { RegCreateKeyExW(h, w(name).as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_ALL_ACCESS, std::ptr::null(), &mut sh, std::ptr::null_mut()) } == 0 {
                    n += load(sh, sub, depth + 1);
                    unsafe { RegCloseKey(sh) };
                }
            }
        }
        n
    }

    /// Puts `file`'s keys under `under` (each replaced first when `replace`). The keys
    /// written.
    pub fn import(file: &Path, under: &str, replace: bool) -> Result<usize, SaveError> {
        let Ok(text) = std::fs::read_to_string(file) else { return Ok(0) };
        let all: serde_json::Value = serde_json::from_str(&text).map_err(|e| SaveError::Io(format!("{}: {e}", file.display())))?;
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut n = 0;
        #[cfg(windows)]
        for (rel, tree) in all.as_object().into_iter().flatten() {
            let path = if rel.is_empty() { under.trim_end_matches('\\').to_string() } else { format!("{under}{rel}") };
            // Only a game's own keys, never Windows' (a backup that names one is wrong).
            if layout::game_registry_key(&path).is_none() && !path.to_ascii_lowercase().starts_with("software\\peerparty\\saves\\") {
                continue;
            }
            if replace {
                delete(&path);
            }
            if let Some(h) = open(&path, true) {
                n += load(h, tree, 0);
                unsafe { RegCloseKey(h) };
            }
        }
        #[cfg(not(windows))]
        let _ = (all, under, replace);
        Ok(n)
    }

    /// Steam's folder (`HKCU\Software\Valve\Steam\SteamPath`).
    pub fn steam_path() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            let mut buf = vec![0u16; 1024];
            let mut len = (buf.len() * 2) as u32;
            let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, w(r"Software\Valve\Steam").as_ptr(), w("SteamPath").as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) };
            (rc == 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]).replace('/', "\\")))
        }
        #[cfg(not(windows))]
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(base: &Path) -> GameSaves {
        let r = |n: &str| Some(base.join("real").join(n).to_string_lossy().into_owned());
        GameSaves {
            id: format!("test-game-{:08x}", std::process::id()),
            data: base.join("data"),
            roots: Roots {
                local_low: r("LocalLow"),
                local: r("Local"),
                roaming: r("Roaming"),
                documents: r("Documents"),
                saved_games: r("Saved Games"),
                game_dir: r(r"Games\Test Game"),
                extra: Vec::new(),
                shared: vec![base.join("data").to_string_lossy().into_owned()],
            },
            identity: base.join("identity"),
            exe: Some(base.join(r"real\Games\Test Game\testgame.exe")),
        }
    }

    fn base(name: &str) -> PathBuf {
        let b = std::env::temp_dir().join(format!("peerparty-savemgr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&b);
        b
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    /// Player 1 to 2, 2 to 1, 2 to 3, and a swap: each destination's own is backed up,
    /// player 1's real folder gets player 2's, and saves keyed by an id are renamed.
    #[test]
    fn any_player_to_any_player() {
        let b = base("any");
        let g = game(&b);
        let real = b.join(r"real\LocalLow\Co\Game");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("save.dat"), "p1").unwrap();
        let p1 = peerparty_core::Identity::ensure_slot(&g.identity, 0).unwrap();
        let p2 = peerparty_core::Identity::ensure_slot(&g.identity, 1).unwrap();
        // A save keyed by player 1's SteamID (the game's account).
        let steam = 76561197960270388u64;
        std::fs::create_dir_all(real.join(steam.to_string())).unwrap();
        std::fs::write(real.join(steam.to_string()).join("slot.sav"), "p1 keyed").unwrap();
        g.add_discovered_folder(&real).unwrap();
        let dir = g.player_dir(0);
        std::fs::write(dir.join(layout::DISCOVERED), format!("{}\r\nsteamid\t{steam}\r\n", read(&dir.join(layout::DISCOVERED)).trim_end())).unwrap();
        // 1 -> 2.
        let r = g.copy(0, 1).unwrap();
        assert_eq!(r.files, 2);
        let mine = layout::sandbox_path(&g.player_dir(1).join("fs"), Zone::LocalLow, r"Co\Game");
        assert_eq!(read(&mine.join("save.dat")), "p1");
        let p2_steam = layout::lan_steam_id(&p2.0);
        assert_eq!(read(&mine.join(p2_steam.to_string()).join("slot.sav")), "p1 keyed", "renamed to player 2's id");
        assert!(!mine.join(steam.to_string()).exists());
        // Player 2 plays; 2 -> 1 puts it in player 1's real folder, keyed back.
        std::fs::write(mine.join("save.dat"), "p2").unwrap();
        let r = g.copy(1, 0).unwrap();
        assert!(r.backup.as_deref().unwrap().contains("before-copy-from-p2"));
        assert_eq!(read(&real.join("save.dat")), "p2");
        assert_eq!(read(&real.join(steam.to_string()).join("slot.sav")), "p1 keyed", "back under the game's account");
        assert!(!real.join(p2_steam.to_string()).exists());
        let backup = &g.backups(0)[0];
        assert_eq!(read(&backup.path.join(r"fs\LocalLow\Co\Game\save.dat")), "p1", "player 1's own was kept");
        // 2 -> 3, then a swap of 2 and 3, then a restore.
        g.copy(1, 2).unwrap();
        let p3 = layout::sandbox_path(&g.player_dir(2).join("fs"), Zone::LocalLow, r"Co\Game");
        assert_eq!(read(&p3.join("save.dat")), "p2");
        std::fs::write(p3.join("save.dat"), "p3").unwrap();
        g.swap(1, 2).unwrap();
        assert_eq!(read(&mine.join("save.dat")), "p3");
        assert_eq!(read(&p3.join("save.dat")), "p2");
        let id = g.backups(1)[0].id.clone();
        assert!(id.contains("before-swap-with-p3"), "{id}");
        g.restore(1, &id).unwrap();
        assert_eq!(read(&mine.join("save.dat")), "p2", "restored");
        // Reset: fresh, and undoable.
        g.reset(1).unwrap();
        assert!(!g.player_dir(1).join("fs").exists());
        let id = g.backups(1)[0].id.clone();
        g.restore(1, &id).unwrap();
        assert_eq!(read(&mine.join("save.dat")), "p2");
        let _ = p1;
        let _ = std::fs::remove_dir_all(&b);
    }

    /// Backups keep the newest five; a running game refuses; a copy that can't finish
    /// leaves the destination as it was.
    #[test]
    fn backups_rotate_and_a_running_game_or_a_failed_copy_changes_nothing() {
        let b = base("guards");
        let g = game(&b);
        let real = b.join(r"real\Roaming\Foo");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("a.sav"), "p1").unwrap();
        g.add_discovered_folder(&real).unwrap();
        for _ in 0..7 {
            g.copy(0, 1).unwrap();
        }
        assert_eq!(g.backups(1).len(), KEEP_BACKUPS);
        // Running as player 3: everything waits.
        let lock = g.lock_player(2).unwrap();
        assert!(g.lock_player(2).is_err(), "player 3 is taken");
        assert_eq!(g.running(), [2]);
        assert_eq!(g.copy(0, 1), Err(SaveError::Running(3)));
        drop(lock);
        assert!(g.running().is_empty());
        // A source over the cap: nothing copied, player 2's stays.
        let mine = layout::sandbox_path(&g.player_dir(1).join("fs"), Zone::Roaming, "Foo");
        std::fs::write(mine.join("a.sav"), "p2").unwrap();
        let mut budget = 1;
        assert!(matches!(copy_tree(&real, &b.join("x"), &mut budget, "x"), Err(SaveError::TooBig { .. })));
        assert_eq!(g.copy(1, 1), Err(SaveError::Same));
        assert_eq!(g.copy(0, 4), Err(SaveError::BadPlayer(5)));
        assert_eq!(read(&mine.join("a.sav")), "p2");
        assert!(g.restore(1, r"..\x").is_err());
        let _ = std::fs::remove_dir_all(&b);
    }

    /// Player 1's folder before the game ran (a Unity game's, from `app.info`), a
    /// mod loader's log and cache never counted as saves, and "Open folder" going to the
    /// game's folder in LocalLow over a file it wrote in its own folder.
    #[test]
    fn player_ones_folder_is_found_and_shown_first() {
        let b = base("place");
        let g = game(&b);
        let exe = g.exe.clone().unwrap();
        let low = b.join(r"real\LocalLow\Made Up Co\Made Up Game");
        std::fs::create_dir_all(&low).unwrap();
        std::fs::write(low.join("slot0.data"), "x").unwrap();
        assert!(g.discovered().is_empty(), "no app.info: not a Unity game");
        let info = exe.parent().unwrap().join(r"testgame_Data\app.info");
        std::fs::create_dir_all(info.parent().unwrap()).unwrap();
        std::fs::write(&info, "Made Up Co\nMade Up Game").unwrap();
        let found = g.discovered();
        assert!(found.contains(&Discovered::Files { zone: Zone::LocalLow, rel: r"Made Up Co\Made Up Game".into(), real: low.to_string_lossy().into_owned() }), "{found:?}");
        let p1 = &g.inspect()[0];
        assert!(p1.has_saves());
        assert_eq!(p1.folder.as_deref(), Some(low.as_path()));

        // The hook's notes: a loader's config, log and cache first, then the game's folder.
        let dir = g.player_dir(0);
        std::fs::create_dir_all(&dir).unwrap();
        let game_dir = exe.parent().unwrap();
        let mut notes = String::new();
        for rel in [r"Loader\config\Loader.cfg", r"Loader\LogOutput.log", r"Loader\cache\types.dat"] {
            let f = game_dir.join(rel);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, "x").unwrap();
            notes += &(Discovered::Files { zone: Zone::GameDir, rel: rel.into(), real: f.to_string_lossy().into_owned() }.to_line() + "\r\n");
        }
        notes += &(Discovered::Files { zone: Zone::LocalLow, rel: r"Made Up Co\Made Up Game".into(), real: low.to_string_lossy().into_owned() }.to_line() + "\r\n");
        std::fs::write(dir.join(layout::DISCOVERED), notes).unwrap();
        let found = g.discovered();
        assert_eq!(found.len(), 2, "the log and cache left out, the guess not doubled: {found:?}");
        assert_eq!(g.inspect()[0].folder.as_deref(), Some(low.as_path()), "LocalLow before the game's folder");

        // Player 2's own copy of that folder is what its "Open folder" shows.
        let mine = layout::sandbox_path(&g.player_dir(1).join("fs"), Zone::LocalLow, r"Made Up Co\Made Up Game");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("slot0.data"), "y").unwrap();
        assert_eq!(g.inspect()[1].folder.as_deref(), Some(mine.as_path()));
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn snapshots_list_their_game_folders() {
        let b = base("seeds");
        let fs = b.join("fs");
        for p in [r"LocalLow\Co\Game\a.dat", r"Roaming\Foo\b.dat", r"Documents\My Games\Bar\c.sav", r"GameDir\Saves\d.sav", r"GameDir\config.ini"] {
            let f = fs.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        let mut got = seed_entries(&fs);
        got.sort();
        assert_eq!(
            got,
            [
                (Zone::LocalLow, r"Co\Game".to_string()),
                (Zone::Roaming, "Foo".to_string()),
                (Zone::Documents, r"My Games\Bar".to_string()),
                (Zone::GameDir, r"Saves\d.sav".to_string()),
                (Zone::GameDir, "config.ini".to_string()),
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&b);
    }

    /// A junction inside a save isn't followed (one back up to the save would never end).
    #[test]
    fn links_inside_saves_are_left_out() {
        use std::os::windows::process::CommandExt;
        let b = base("links");
        let save = b.join("save");
        std::fs::create_dir_all(&save).unwrap();
        std::fs::write(save.join("a.sav"), "x").unwrap();
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(save.join("loop")).arg(&save).creation_flags(0x0800_0000).output().unwrap();
        assert!(made.status.success());
        assert_eq!(copy_tree(&save, &b.join("copy"), &mut MAX_COPY.clone(), "x").unwrap(), 1);
        assert!(!b.join(r"copy\loop").exists());
        remove_any(&save.join("loop")).unwrap();
        assert!(save.join("a.sav").exists());
        let _ = std::fs::remove_dir_all(&b);
    }

    /// Registry keys go into a snapshot and back.
    #[test]
    fn registry_keys_round_trip() {
        let b = base("reg");
        std::fs::create_dir_all(&b).unwrap();
        let key = format!(r"Software\PeerPartyTest{}\Game", std::process::id());
        reg::delete(&key);
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Registry::*;
            let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
            let mut h: HKEY = std::ptr::null_mut();
            RegCreateKeyExW(HKEY_CURRENT_USER, w(&format!(r"{key}\Sub")).as_ptr(), 0, std::ptr::null(), 0, KEY_ALL_ACCESS, std::ptr::null(), &mut h, std::ptr::null_mut());
            let v = 7u32;
            RegSetValueExW(h, w("Progress").as_ptr(), 0, REG_DWORD, (&v as *const u32).cast(), 4);
            RegCloseKey(h);
        }
        let file = b.join("registry.json");
        assert_eq!(reg::export(std::slice::from_ref(&key), "Software\\", &file).unwrap(), 2);
        reg::delete(&key);
        assert!(!reg::exists(&key));
        assert_eq!(reg::import(&file, "Software\\", true).unwrap(), 2);
        assert!(reg::exists(&format!(r"{key}\Sub")));
        reg::delete(&key.rsplit_once('\\').unwrap().0.to_string());
        let _ = std::fs::remove_dir_all(&b);
    }
}
