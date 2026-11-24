//! Where players 2–4 keep their saves on this PC, as the launcher (which copies, backs up
//! and lists them) and the hook (which sends the game there) both see it.
//!
//! Player 1 keeps the real folders, registry and Steam Cloud, so playing alone and
//! playing as player 1 are the same saves. Every later player has a folder of its own in
//! PeerParty's data folder, and nothing of theirs is ever written beside player 1's:
//!
//! ```text
//! <data>\saves\<game-id>\
//!   manifest.json          the game, written by the launcher
//!   player-1\discovered.txt where player 1's saves were seen (the hook, slot 0)
//!   player-1\backups\      player 1's saves as they were before something replaced them
//!   player-2\fs\<Zone>\... player 2's copy of each folder (`Zone`)
//!   player-2\steam-remote\ player 2's Steam Remote Storage files
//!   player-2\whiteouts.txt files in the game's folder player 2 deleted
//!   player-2\backups\
//! ```
//!
//! A path the game uses falls in a zone (`classify`): one of Windows' folders for
//! programs' data (`AppData\LocalLow`, `Local`, `Roaming`, `Documents`, `Saved Games`),
//! the game's own folder, Steam's `userdata` for one game, or a folder the player named.
//! Below the zone's root (and past the folders every program shares, `EXCLUDED`), a later
//! player's path is the same path in its own `fs\<Zone>` folder. The game's own folder is
//! read through: a later player reads the game's files there until it writes one, and
//! its writes go to its own folder (`GameDir`).
//!
//! Its registry keys under `HKCU\Software\<vendor>` are under `registry_key` instead.

use std::path::{Path, PathBuf};

/// The most players a game's saves are kept for (player 1 included).
pub const MAX_PLAYERS: u32 = 4;
/// The folder of every game's saves, in PeerParty's data folder.
pub const SAVES_DIR: &str = "saves";
/// In a game's folder: what the launcher knows of the game.
pub const MANIFEST: &str = "manifest.json";
/// In player 1's folder: where its saves were seen, one per line (`Discovered`).
pub const DISCOVERED: &str = "discovered.txt";
/// In a later player's folder: paths in the game's folder it deleted, one per line.
pub const WHITEOUTS: &str = "whiteouts.txt";
/// In a later player's folder: its Steam Remote Storage files.
pub const STEAM_REMOTE: &str = "steam-remote";
/// In a player's folder: its earlier saves.
pub const BACKUPS: &str = "backups";
/// The most lines `discovered.txt` keeps (a game writing files all over its folder).
pub const MAX_DISCOVERED: usize = 512;

/// A game's id for its saves: its exe's name, then 8 hex digits of its path (`key`, the
/// launcher's key for the game, or the exe's full path), so two games named alike keep
/// theirs apart.
pub fn game_id(key: &str) -> String {
    let norm = key.trim().replace('/', "\\").to_lowercase();
    let stem = norm.rsplit('\\').next().unwrap_or("");
    let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s);
    let mut slug: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let mut slug: String = slug.trim_matches('-').chars().take(32).collect();
    if slug.is_empty() {
        slug.push_str("game");
    }
    // FNV-1a: the same in every build, and no dependency.
    let mut h: u32 = 0x811c_9dc5;
    for b in norm.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{}-{h:08x}", slug.trim_end_matches('-'))
}

/// Whether `id` is one `game_id` could make (a folder name, from the command line too).
pub fn valid_game_id(id: &str) -> bool {
    let Some((slug, hash)) = id.rsplit_once('-') else { return false };
    !slug.is_empty() && slug.len() <= 32 && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && hash.len() == 8 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `<data>\saves\<id>`.
pub fn game_dir(data: &Path, id: &str) -> PathBuf {
    data.join(SAVES_DIR).join(id)
}

/// `<data>\saves\<id>\player-<slot + 1>`.
pub fn player_dir(data: &Path, id: &str, slot: u32) -> PathBuf {
    game_dir(data, id).join(format!("player-{}", slot + 1))
}

/// A later player's copies of the zones' folders.
pub fn fs_dir(data: &Path, id: &str, slot: u32) -> PathBuf {
    player_dir(data, id, slot).join("fs")
}

/// Where a later player's `HKCU\Software\<vendor>\...` keys are, under `HKCU`.
pub fn registry_key(id: &str, slot: u32) -> String {
    format!("Software\\PeerParty\\Saves\\{id}\\p{}", slot + 1)
}

/// One kind of place saves are kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Zone {
    /// `%USERPROFILE%\AppData\LocalLow` (Unity's `persistentDataPath`).
    LocalLow,
    /// `%LOCALAPPDATA%` (Unreal's `Saved`).
    Local,
    /// `%APPDATA%` (Godot's `app_userdata`).
    Roaming,
    /// The Documents folder, wherever it was moved (`My Games\<game>`).
    Documents,
    /// `%USERPROFILE%\Saved Games`.
    SavedGames,
    /// The game's own folder: read through until written.
    GameDir,
    /// Steam's `userdata\<account>\<app id>`.
    SteamUserdata,
    /// A folder the player named for the game.
    Other,
}

impl Zone {
    pub const ALL: [Zone; 8] = [Zone::LocalLow, Zone::Local, Zone::Roaming, Zone::Documents, Zone::SavedGames, Zone::GameDir, Zone::SteamUserdata, Zone::Other];

    /// Its folder's name in a player's `fs` folder.
    pub fn dir_name(self) -> &'static str {
        match self {
            Zone::LocalLow => "LocalLow",
            Zone::Local => "Local",
            Zone::Roaming => "Roaming",
            Zone::Documents => "Documents",
            Zone::SavedGames => "SavedGames",
            Zone::GameDir => "GameDir",
            Zone::SteamUserdata => "SteamUserdata",
            Zone::Other => "Other",
        }
    }

    pub fn from_dir_name(name: &str) -> Option<Zone> {
        Zone::ALL.into_iter().find(|z| z.dir_name().eq_ignore_ascii_case(name))
    }

    /// How many components below the zone's root make one game's saves (what player 1's
    /// saves are copied as, and recorded as): `Company\Product` for Unity's LocalLow,
    /// `My Games\<game>` in Documents, `<account>\<app id>` in Steam's userdata, else the
    /// first folder.
    pub fn seed_depth(self, rel: &str) -> usize {
        match self {
            Zone::LocalLow | Zone::SteamUserdata => 2,
            Zone::Documents if first(rel).is_some_and(|f| f.eq_ignore_ascii_case("my games")) => 2,
            Zone::Other => 2,
            _ => 1,
        }
    }
}

/// A path's first component.
fn first(rel: &str) -> Option<&str> {
    rel.split('\\').find(|c| !c.is_empty())
}

/// The real folders zones start at, as each side finds them (the hook in the game, the
/// launcher for copying); all full paths, any case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roots {
    pub local_low: Option<String>,
    pub local: Option<String>,
    pub roaming: Option<String>,
    pub documents: Option<String>,
    pub saved_games: Option<String>,
    /// The game's folder (`PEERPARTY_GAME_ROOT`, else the exe's).
    pub game_dir: Option<String>,
    /// Folders the player named as the game's saves (`PEERPARTY_SAVE_PATHS`).
    pub extra: Vec<String>,
    /// Folders every player shares (`PEERPARTY_SHARED_SAVE_PATHS`, PeerParty's own
    /// folder, the temp folder): never anyone's own.
    pub shared: Vec<String>,
}

/// First components below Windows' data folders that every program shares, never one
/// game's saves: Windows' and runtimes' own, graphics drivers' shader caches, stores'
/// and overlays' folders, and PeerParty's.
pub const EXCLUDED: &[&str] = &[
    // Windows, its runtimes and its caches.
    "microsoft", "windows", "temp", "tmp", "crashdumps", "packages", "connecteddevicesplatform", "comms", "peerdistrepub", "diagnostics", "fontcache",
    "programs", "virtualstore", "d3dscache", "squirreltemp", ".dotnet", "nuget", "assembly", "desktop.ini", "thumbs.db", "elevateddiagnostics",
    // Graphics drivers and APIs: shader caches every game shares.
    "nvidia", "nvidia corporation", "amd", "ati", "intel", "vulkan", "khronos", "mesa", "mesa_shader_cache", "shadercache", "openal",
    // Engines' shared caches.
    "unrealengine", "unrealenginelauncher",
    // Stores, launchers and overlays (their own data, not this game's).
    "steam", "valve", "epicgameslauncher", "epic", "epic games","gog.com", "galaxyclient", "origin", "electronic arts", "ea desktop", "ubisoft game launcher",
    "battle.net", "blizzard entertainment", "discord", "overwolf", "rivatuner", "razer", "logitech", "lghub", "corsair",
    // PeerParty's own.
    "peerparty", "peerparty-data",
];
/// First components of Documents that are the player's own files.
const EXCLUDED_DOCUMENTS: &[&str] = &["my music", "my pictures", "my videos", "music", "pictures", "videos", "downloads", "windowspowershell", "powershell"];
/// Folders of the game's own folder whose files are a player's saves: never read
/// through to player 1's.
pub const STRICT_GAME_DIR: &[&str] = &["save", "saves", "savegame", "savegames", "save games", "savedata", "saved", "savedgames", "profile", "profiles", "userdata", "steamdata"];

/// Strips a Win32 long-path prefix (`\\?\C:\`, not a UNC one's) and turns `/` into `\`.
pub fn normalize(path: &str) -> String {
    let p = path.replace('/', "\\");
    match p.strip_prefix(r"\\?\") {
        Some(rest) if !rest.get(..4).is_some_and(|u| u.eq_ignore_ascii_case(r"UNC\")) => rest.to_string(),
        _ => p,
    }
}

/// `path` relative to `root` (both full), when it's inside it; "" for the root itself.
fn under<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let root = root.trim_end_matches('\\');
    if root.is_empty() || path.len() < root.len() || !path.is_char_boundary(root.len()) || !path[..root.len()].eq_ignore_ascii_case(root) {
        return None;
    }
    match &path[root.len()..] {
        "" => Some(""),
        rest => rest.strip_prefix('\\').map(|r| r.trim_end_matches('\\')),
    }
}

/// Whether a folder is no game's own: a drive's root, or one holding the user's
/// folders (a game "folder" found as the user's profile).
fn plausible_game_dir(dir: &str, roots: &Roots) -> bool {
    let d = dir.trim_end_matches('\\');
    let parts = d.split('\\').filter(|c| !c.is_empty()).count();
    parts >= 2 && [&roots.local_low, &roots.local, &roots.roaming, &roots.documents, &roots.saved_games].into_iter().flatten().all(|r| under(r, d).is_none())
}

/// The zone `path` (a full path, as the game passed it or `normalize`d) falls in, and the
/// part of it below the zone's root (its own case kept); None for a path every player
/// shares. The zone's root itself is shared too (only what's inside it is a player's).
pub fn classify(roots: &Roots, path: &str) -> Option<(Zone, String)> {
    let p = normalize(path);
    if p.starts_with(r"\\") {
        return None;
    }
    if roots.shared.iter().any(|s| under(&p, &normalize(s)).is_some()) {
        return None;
    }
    // The game's own folder first: a game kept inside AppData is still its folder.
    if let Some(g) = roots.game_dir.as_deref().map(normalize).filter(|g| plausible_game_dir(g, roots)) {
        if let Some(rel) = under(&p, &g) {
            return (!rel.is_empty()).then(|| (Zone::GameDir, rel.to_string()));
        }
    }
    for (i, extra) in roots.extra.iter().enumerate() {
        if let Some(rel) = under(&p, &normalize(extra)) {
            return Some((Zone::Other, if rel.is_empty() { i.to_string() } else { format!("{i}\\{rel}") }));
        }
    }
    if let Some(rel) = steam_userdata(&p) {
        return Some((Zone::SteamUserdata, rel));
    }
    // The longest root that holds it (Documents may sit inside another).
    let mut best: Option<(Zone, &str, usize)> = None;
    for (zone, root) in [(Zone::LocalLow, &roots.local_low), (Zone::Local, &roots.local), (Zone::Roaming, &roots.roaming), (Zone::Documents, &roots.documents), (Zone::SavedGames, &roots.saved_games)] {
        let Some(root) = root.as_deref() else { continue };
        let root = normalize(root);
        if let Some(rel) = under(&p, &root) {
            if best.is_none_or(|b| root.len() > b.2) {
                best = Some((zone, rel, root.len()));
            }
        }
    }
    let (zone, rel, _) = best?;
    let comps: Vec<&str> = rel.split('\\').filter(|c| !c.is_empty()).collect();
    let head = comps.first()?.to_ascii_lowercase();
    // A listing of the zone's root (`LocalLow\*`) is of every program's folders.
    let wild = |c: &str| c.contains(['*', '?']);
    if EXCLUDED.contains(&head.as_str()) || head.starts_with("ntuser.") || wild(&head) {
        return None;
    }
    if zone == Zone::Documents {
        if EXCLUDED_DOCUMENTS.contains(&head.as_str()) {
            return None;
        }
        // `My Games` holds every game's: only a game's folder in it is its own.
        if head == "my games" && (comps.len() < 2 || wild(comps[1])) {
            return None;
        }
    }
    Some((zone, comps.join("\\")))
}

/// `...\userdata\<account>\<app id>\...` (Steam's folder for one game's files of one
/// account): `<account>\<app id>\...`. The account's folder and its `config` are shared.
fn steam_userdata(p: &str) -> Option<String> {
    let comps: Vec<&str> = p.split('\\').collect();
    let at = comps.iter().rposition(|c| c.eq_ignore_ascii_case("userdata"))?;
    let digits = |c: &&str| !c.is_empty() && c.len() <= 20 && c.bytes().all(|b| b.is_ascii_digit());
    let (acct, app) = (comps.get(at + 1)?, comps.get(at + 2)?);
    // Steam's own folder holds `steam.exe`'s neighbours; any `userdata` with two numbers
    // under it is taken as Steam's.
    (digits(acct) && digits(app) && at >= 1).then(|| comps[at + 1..].iter().filter(|c| !c.is_empty()).copied().collect::<Vec<_>>().join("\\"))
}

/// The real path for `rel` in `zone` (None for Steam's userdata, whose folder only a
/// recorded path knows, and a zone whose root isn't known).
pub fn real_path(roots: &Roots, zone: Zone, rel: &str) -> Option<String> {
    let root = match zone {
        Zone::LocalLow => roots.local_low.clone(),
        Zone::Local => roots.local.clone(),
        Zone::Roaming => roots.roaming.clone(),
        Zone::Documents => roots.documents.clone(),
        Zone::SavedGames => roots.saved_games.clone(),
        Zone::GameDir => roots.game_dir.clone(),
        Zone::Other => {
            let (i, rest) = rel.split_once('\\').unwrap_or((rel, ""));
            let base = normalize(roots.extra.get(i.parse::<usize>().ok()?)?);
            return Some(if rest.is_empty() { base } else { format!("{}\\{rest}", base.trim_end_matches('\\')) });
        }
        Zone::SteamUserdata => None,
    }?;
    let root = normalize(&root);
    Some(if rel.is_empty() { root } else { format!("{}\\{rel}", root.trim_end_matches('\\')) })
}

/// A later player's path for `rel` in `zone`.
pub fn sandbox_path(fs: &Path, zone: Zone, rel: &str) -> PathBuf {
    let mut p = fs.join(zone.dir_name());
    for c in rel.split('\\').filter(|c| !c.is_empty() && *c != "." && *c != "..") {
        p.push(c);
    }
    p
}

/// The part of `rel` that is one game's saves (`Zone::seed_depth`), None when `rel` is
/// above that (the vendor's folder alone).
pub fn seed_root(zone: Zone, rel: &str) -> Option<String> {
    let comps: Vec<&str> = rel.split('\\').filter(|c| !c.is_empty()).collect();
    let n = zone.seed_depth(rel);
    (comps.len() >= n).then(|| comps[..n].join("\\"))
}

/// Whether a path in the game's folder is in a folder of saves (`STRICT_GAME_DIR`): a
/// later player never reads player 1's there.
pub fn strict_in_game_dir(rel: &str) -> bool {
    let comps: Vec<&str> = rel.split('\\').filter(|c| !c.is_empty()).collect();
    comps.iter().take(comps.len().saturating_sub(1)).any(|c| STRICT_GAME_DIR.contains(&c.to_ascii_lowercase().as_str()))
}

/// One line of `discovered.txt`: where player 1's saves were seen.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Discovered {
    /// A game's saves in a zone (`seed_root`), and the real path it was seen at.
    Files { zone: Zone, rel: String, real: String },
    /// A registry key under `HKCU`, `Software\<vendor>\<product>`.
    Registry(String),
    /// The game's Steam account (steam_api's SteamID), for its saves keyed by it.
    SteamId(u64),
    /// The game used Steam Remote Storage.
    SteamRemote,
}

impl Discovered {
    pub fn to_line(&self) -> String {
        let clean = |s: &str| s.replace(['\t', '\r', '\n'], " ");
        match self {
            Discovered::Files { zone, rel, real } => format!("files\t{}\t{}\t{}", zone.dir_name(), clean(rel), clean(real)),
            Discovered::Registry(k) => format!("registry\t{}", clean(k)),
            Discovered::SteamId(id) => format!("steamid\t{id}"),
            Discovered::SteamRemote => "steam-remote".to_string(),
        }
    }

    pub fn from_line(line: &str) -> Option<Discovered> {
        let mut f = line.trim_end_matches(['\r', '\n']).split('\t');
        Some(match f.next()? {
            "files" => {
                let zone = Zone::from_dir_name(f.next()?)?;
                let rel = f.next()?.to_string();
                let real = f.next()?.to_string();
                if rel.is_empty() || rel.split('\\').any(|c| c == "..") || real.is_empty() {
                    return None;
                }
                Discovered::Files { zone, rel, real }
            }
            "registry" => {
                let k = f.next()?.to_string();
                if !k.to_ascii_lowercase().starts_with("software\\") || k.split('\\').count() < 2 {
                    return None;
                }
                Discovered::Registry(k)
            }
            "steamid" => Discovered::SteamId(f.next()?.parse().ok().filter(|v| *v != 0)?),
            "steam-remote" => Discovered::SteamRemote,
            _ => return None,
        })
    }

    /// `discovered.txt`'s lines, each once, at most `MAX_DISCOVERED`.
    pub fn parse_all(text: &str) -> Vec<Discovered> {
        let mut out: Vec<Discovered> = Vec::new();
        for d in text.lines().filter_map(Discovered::from_line) {
            if !out.contains(&d) && out.len() < MAX_DISCOVERED {
                out.push(d);
            }
        }
        out
    }
}

/// `HKCU\Software\<vendor>\...` that's a game's own (not Windows', a driver's or a
/// store's): `<vendor>\<rest>` with the vendor's case kept. None for anything else.
pub fn game_registry_key(subkey: &str) -> Option<&str> {
    let s = subkey.trim_start_matches('\\');
    let rest = s.get(..9).filter(|h| h.eq_ignore_ascii_case("software\\")).map(|_| &s[9..])?;
    let vendor = rest.split('\\').next().filter(|v| !v.is_empty())?;
    const SHARED_VENDORS: &[&str] = &[
        "microsoft", "classes", "policies", "registeredapplications", "clients", "odbc", "valve", "epic games", "gog.com", "nvidia corporation", "nvidia", "amd", "ati",
        "ati technologies", "intel", "khronos", "vulkan", "directx", "wine", "peerparty", "wow6432node", "google", "mozilla", "discord", "overwolf", "razer", "logitech",
    ];
    (!SHARED_VENDORS.contains(&vendor.to_ascii_lowercase().as_str())).then_some(rest)
}

/// The tokens one player's ids appear as in save paths, from its 16-byte peer id and its
/// Steam id: the peer id's hex, the PlayFab entity id the hook gives it, the SteamID in
/// decimal, 16-digit hex and unpadded hex, and its account id.
pub fn id_tokens(peer: &[u8; 16], steam: u64) -> Vec<String> {
    let hex: String = peer.iter().map(|b| format!("{b:02x}")).collect();
    let entity: String = peer[..8].iter().map(|b| format!("{b:02X}")).collect();
    let mut v = vec![hex, entity, steam.to_string(), format!("{steam:016x}"), format!("{steam:x}"), (steam as u32).to_string()];
    v.dedup();
    v
}

/// The SteamID the hook gives a LAN player with peer id `peer` (hook steam/ids.rs).
pub fn lan_steam_id(peer: &[u8; 16]) -> u64 {
    let account = u32::from_le_bytes([peer[0], peer[1], peer[2], peer[3]]) | 0x8000_0000;
    (1u64 << 56) | (1u64 << 52) | (1u64 << 32) | u64::from(account)
}

/// `name` with each of `from`'s tokens that stands as a whole token (no letter or digit
/// either side) replaced by `to`'s at the same place; None when nothing matched.
pub fn remap_name(name: &str, from: &[String], to: &[String]) -> Option<String> {
    let mut out = name.to_string();
    let mut changed = false;
    for (f, t) in from.iter().zip(to) {
        if f.is_empty() || f.eq_ignore_ascii_case(t) {
            continue;
        }
        let mut at = 0;
        while let Some(i) = out.to_ascii_lowercase()[at..].find(&f.to_ascii_lowercase()).map(|i| i + at) {
            let end = i + f.len();
            let edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
            if edge(out[..i].chars().next_back()) && edge(out[end..].chars().next()) {
                out.replace_range(i..end, t);
                changed = true;
                at = i + t.len();
            } else {
                at = end;
            }
        }
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Roots {
        Roots {
            local_low: Some(r"C:\Users\a\AppData\LocalLow".into()),
            local: Some(r"C:\Users\a\AppData\Local".into()),
            roaming: Some(r"C:\Users\a\AppData\Roaming".into()),
            documents: Some(r"D:\Docs".into()),
            saved_games: Some(r"C:\Users\a\Saved Games".into()),
            game_dir: Some(r"D:\Games\Some Game".into()),
            extra: vec![r"E:\MySaves".into()],
            shared: vec![r"C:\Users\a\Tmp".into(), r"C:\Users\a\AppData\Local\peerparty".into()],
        }
    }

    #[test]
    fn game_ids_are_stable_and_safe() {
        let a = game_id(r"c:\games\some game\Some Game.exe");
        assert_eq!(a, game_id("C:/Games/Some Game/some game.EXE"), "case and slashes aside");
        assert!(a.starts_with("some-game-"), "{a}");
        assert!(valid_game_id(&a), "{a}");
        assert_ne!(a, game_id(r"d:\other\some game.exe"));
        assert!(valid_game_id(&game_id("")));
        assert!(valid_game_id(&game_id(r"c:\x\---.exe")));
        assert!(valid_game_id(&game_id(r"c:\x\日本語.exe")));
        for bad in ["", "abc", "x-1234567", "../x-12345678", "A-12345678", "x-1234567g"] {
            assert!(!valid_game_id(bad), "{bad}");
        }
    }

    /// Windows' data folders: a game's folder below them is a later player's own; the
    /// folders every program shares, and the roots themselves, are nobody's.
    #[test]
    fn paths_fall_in_their_zones() {
        let r = roots();
        let c = |p: &str| classify(&r, p);
        assert_eq!(c(r"C:\Users\a\AppData\LocalLow\Co\Game\data_full_-99.data"), Some((Zone::LocalLow, r"Co\Game\data_full_-99.data".into())));
        assert_eq!(c(r"\\?\C:\USERS\A\APPDATA\LOCALLOW\Co\Game"), Some((Zone::LocalLow, r"Co\Game".into())), "long paths and any case");
        assert_eq!(c("C:/Users/a/AppData/Local/Foo/Saved/SaveGames/slot1.sav"), Some((Zone::Local, r"Foo\Saved\SaveGames\slot1.sav".into())));
        assert_eq!(c(r"C:\Users\a\AppData\Roaming\Godot\app_userdata\G\save.tres"), Some((Zone::Roaming, r"Godot\app_userdata\G\save.tres".into())));
        assert_eq!(c(r"D:\Docs\My Games\Foo\slot0.sav"), Some((Zone::Documents, r"My Games\Foo\slot0.sav".into())));
        assert_eq!(c(r"C:\Users\a\Saved Games\Foo\profile.bin"), Some((Zone::SavedGames, r"Foo\profile.bin".into())));
        assert_eq!(c(r"C:\Users\a\AppData\LocalLow\Co\Game\"), Some((Zone::LocalLow, r"Co\Game".into())), "a trailing slash");
        for shared in [
            r"C:\Users\a\AppData\LocalLow",
            r"C:\Users\a\AppData\Local\NVIDIA\DXCache\x.bin",
            r"C:\Users\a\AppData\Local\D3DSCache\x",
            r"C:\Users\a\Tmp\x.tmp",
            r"C:\Users\a\AppData\Local\peerparty\saves\x",
            r"C:\Users\a\AppData\Roaming\Microsoft\Crypto\x",
            r"C:\Users\a\AppData\LocalLow\Microsoft\CryptnetUrlCache\x",
            r"C:\Users\a\AppData\Local\UnrealEngine\Common\DerivedDataCache\x",
            r"C:\Users\a\AppData\Local\Steam\htmlcache\x",
            r"D:\Docs\My Games",
            r"D:\Docs\My Games\*",
            r"C:\Users\a\AppData\LocalLow\*",
            r"D:\Docs\My Pictures\x.png",
            r"C:\Users\a\NTUSER.DAT",
            r"C:\Windows\System32\kernel32.dll",
            r"\\.\pipe\x",
            r"\\server\share\Saves\x",
        ] {
            assert_eq!(c(shared), None, "{shared}");
        }
        assert_eq!(c(r"C:\Users\a\AppData\Local\Microsoftish\x"), Some((Zone::Local, r"Microsoftish\x".into())), "a whole component only");
    }

    #[test]
    fn the_games_folder_steams_userdata_and_named_folders() {
        let r = roots();
        let c = |p: &str| classify(&r, p);
        assert_eq!(c(r"D:\Games\Some Game\SteamData\user_stats.ini"), Some((Zone::GameDir, r"SteamData\user_stats.ini".into())));
        assert_eq!(c(r"D:\Games\Some Game"), None, "the folder itself");
        assert_eq!(c(r"D:\Games\Some Gamer\x"), None);
        assert_eq!(c(r"C:\Program Files (x86)\Steam\userdata\4660\123450\remote\save.dat"), Some((Zone::SteamUserdata, r"4660\123450\remote\save.dat".into())));
        assert_eq!(c(r"C:\Program Files (x86)\Steam\userdata\4660\config\localconfig.vdf"), None, "the account's own files");
        assert_eq!(c(r"C:\Program Files (x86)\Steam\userdata\4660"), None);
        assert_eq!(c(r"E:\MySaves\slot1.dat"), Some((Zone::Other, r"0\slot1.dat".into())));
        assert_eq!(real_path(&r, Zone::Other, r"0\slot1.dat").as_deref(), Some(r"E:\MySaves\slot1.dat"));
        assert_eq!(real_path(&r, Zone::LocalLow, r"Co\Game").as_deref(), Some(r"C:\Users\a\AppData\LocalLow\Co\Game"));
        // A game kept in AppData is its folder, read through, not a save zone.
        let mut inside = r.clone();
        inside.game_dir = Some(r"C:\Users\a\AppData\Local\Programs\Game".into());
        assert_eq!(classify(&inside, r"C:\Users\a\AppData\Local\Programs\Game\data.pak"), Some((Zone::GameDir, "data.pak".into())));
        // A "game folder" that is the profile or a drive is none.
        for bad in [r"C:\Users\a", r"D:\", "D:"] {
            let mut g = r.clone();
            g.game_dir = Some(bad.into());
            assert_eq!(classify(&g, r"C:\Users\a\AppData\LocalLow\Co\Game\x"), Some((Zone::LocalLow, r"Co\Game\x".into())), "{bad}");
            assert_eq!(classify(&g, r"D:\x\y"), None, "{bad}");
        }
    }

    #[test]
    fn sandbox_paths_and_seed_roots() {
        let fs = Path::new(r"C:\pp\saves\g-00000000\player-2\fs");
        assert_eq!(sandbox_path(fs, Zone::LocalLow, r"Co\Game\x.data"), Path::new(r"C:\pp\saves\g-00000000\player-2\fs\LocalLow\Co\Game\x.data"));
        assert_eq!(sandbox_path(fs, Zone::Local, r"..\..\x"), Path::new(r"C:\pp\saves\g-00000000\player-2\fs\Local\x"), "never out of it");
        assert_eq!(seed_root(Zone::LocalLow, r"Co\Game\x.data").as_deref(), Some(r"Co\Game"));
        assert_eq!(seed_root(Zone::LocalLow, "Co"), None);
        assert_eq!(seed_root(Zone::Roaming, r"Godot\app_userdata\G").as_deref(), Some("Godot"));
        assert_eq!(seed_root(Zone::Documents, r"My Games\Foo\slot0.sav").as_deref(), Some(r"My Games\Foo"));
        assert_eq!(seed_root(Zone::Documents, r"Foo\slot0.sav").as_deref(), Some("Foo"));
        assert_eq!(seed_root(Zone::SteamUserdata, r"4660\123450\remote\a").as_deref(), Some(r"4660\123450"));
        assert!(strict_in_game_dir(r"Saves\slot1.dat"));
        assert!(strict_in_game_dir(r"data\Profile\p.bin"));
        assert!(!strict_in_game_dir(r"SteamData"), "the folder alone");
        assert!(!strict_in_game_dir(r"BepInEx\LogOutput.log"));
        assert_eq!(registry_key("g-00000000", 1), r"Software\PeerParty\Saves\g-00000000\p2");
    }

    #[test]
    fn discovered_lines_round_trip_and_read_forgivingly() {
        let all = [
            Discovered::Files { zone: Zone::LocalLow, rel: r"Co\Game".into(), real: r"C:\Users\a\AppData\LocalLow\Co\Game".into() },
            Discovered::Registry(r"Software\Co\Game".into()),
            Discovered::SteamId(76561197960270388),
            Discovered::SteamRemote,
        ];
        let text: String = all.iter().map(|d| d.to_line() + "\r\n").collect();
        assert_eq!(Discovered::parse_all(&format!("{text}{text}junk\nfiles\tNowhere\tx\ty\nfiles\tLocal\t..\\x\ty\nregistry\tSYSTEM\\x\nsteamid\t0\n")), all);
        let many: String = (0..MAX_DISCOVERED + 10).map(|i| format!("registry\tSoftware\\V{i}\n")).collect();
        assert_eq!(Discovered::parse_all(&many).len(), MAX_DISCOVERED);
    }

    #[test]
    fn a_games_registry_keys_and_everyone_elses() {
        assert_eq!(game_registry_key(r"Software\Co\Game"), Some(r"Co\Game"));
        assert_eq!(game_registry_key(r"SOFTWARE\Co"), Some("Co"));
        for shared in [r"Software\Microsoft\Windows\CurrentVersion", r"Software\Valve\Steam", r"Software\Classes\x", r"Software\PeerParty\Saves\x", r"Software", r"Software\", r"System\x", "Co\\Game"] {
            assert_eq!(game_registry_key(shared), None, "{shared}");
        }
    }

    #[test]
    fn ids_are_remapped_as_whole_tokens() {
        let a = [0x11u8; 16];
        let b = [0x22u8; 16];
        let (sa, sb) = (lan_steam_id(&a), lan_steam_id(&b));
        let (from, to) = (id_tokens(&a, sa), id_tokens(&b, sb));
        assert_eq!(from.len(), to.len());
        assert_eq!(remap_name(&format!("{sa}"), &from, &to), Some(sb.to_string()));
        assert_eq!(remap_name(&format!("profile_{}.sav", sa as u32), &from, &to), Some(format!("profile_{}.sav", sb as u32)));
        assert_eq!(remap_name(&"11".repeat(16).to_uppercase(), &from, &to), Some("22".repeat(16)), "any case");
        assert_eq!(remap_name(&format!("1{sa}"), &from, &to), None, "inside a longer number");
        assert_eq!(remap_name("savegame.dat", &from, &to), None);
    }
}
