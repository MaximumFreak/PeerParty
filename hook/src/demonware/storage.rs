//! Demonware's storage service (`bdStorage`, service 10): publisher files and per-player
//! user files kept locally under `%LOCALAPPDATA%\peerparty\demonware` (or
//! `PEERPARTY_DEMONWARE_DIR`).
//!
//! Publisher files (title updates, playlists, messages of the day) are often signed by
//! the publisher, so PeerParty ships none of them and never alters their bytes. A player
//! can place cached publisher files in `<demonware>\publisher\<title_id>\` or
//! `<demonware>\publisher\<game>\`, or keep them in the game's own `LPC\` folder; every
//! copy on this PC reads them verbatim so their signatures stay intact.
//!
//! When the Demonware layer runs in `official` mode, finalized files the game writes into
//! its `LPC\` folder are copied into `<demonware>\publisher\<game>\` automatically, so
//! later LAN or offline runs already have them.
//!
//! User files uploaded through `bdStorage` (tasks 10, 18, 24) are stored per title and
//! account under `<demonware>\user\<title>\<account>\` and returned on reads (tasks 12,
//! 13, 16), keeping each split-screen player's profile separate.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::log;
use crate::util::crypto::sha1;
use crate::util::local_store;

use super::lobby::typed;
use super::mm::{self, Reader};

/// bdStorage's error for a file it doesn't have (`BD_NO_FILE`).
pub const NO_FILE: u32 = 1000;
/// Largest single publisher or user file read or stored (16 MiB).
pub const MAX_FILE_SIZE: usize = 16 << 20;
/// Most entries returned in one file listing or batch request.
const MAX_LIST: usize = 256;

/// Outcome of one `bdStorage` task call, with a short key (`detail`, usually the file
/// name) so the lobby logger throttles per file rather than hiding later file names.
pub enum StorageReply {
    Results { results: Vec<Vec<u8>>, detail: String, note: String },
    Error { code: u32, detail: String, why: String },
    Unhandled,
}

/// Root folder for Demonware storage: `PEERPARTY_DEMONWARE_DIR`, else
/// `%LOCALAPPDATA%\peerparty\demonware`.
pub fn root() -> Option<PathBuf> {
    if let Some(d) = crate::config::HookConfig::global().demonware_dir() {
        return Some(d);
    }
    local_store::sdk_dir("demonware")
}

/// The game's own install folder (`PEERPARTY_GAME_ROOT`, else the executable's folder).
fn game_dir() -> Option<PathBuf> {
    if let Some(r) = crate::config::HookConfig::global().game_root.as_deref() {
        let p = PathBuf::from(r);
        if !r.is_empty() {
            return Some(p);
        }
    }
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Candidate folders searched for publisher files, highest priority first:
/// 1. `<root>\publisher\<title_id>` (when a title id is known),
/// 2. `<root>\publisher\<game_name>` (also where official-mode harvesting writes),
/// 3. `<game_dir>\LPC` (the game's own local persistent cache, if present).
pub fn publisher_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(r) = root() {
        let pub_root = r.join("publisher");
        let title = super::auth::title();
        if title != 0 {
            dirs.push(pub_root.join(title.to_string()));
        }
        if let Some(game) = local_store::game_name() {
            let by_game = pub_root.join(game);
            if !dirs.contains(&by_game) {
                dirs.push(by_game);
            }
        }
    }
    if let Some(g) = game_dir() {
        dirs.push(g.join("LPC"));
    }
    dirs
}

/// The preferred folder a player can place publisher files in for the current title.
pub fn publisher_hint_dir() -> Option<PathBuf> {
    let pub_root = root()?.join("publisher");
    let title = super::auth::title();
    if title != 0 {
        return Some(pub_root.join(title.to_string()));
    }
    Some(pub_root.join(local_store::game_name()?))
}

/// Reads a publisher file verbatim from the first candidate folder that holds it.
pub fn read_publisher_file(name: &str) -> Option<(Vec<u8>, PathBuf)> {
    let safe = local_store::file_name(name)?;
    for dir in publisher_dirs() {
        let path = dir.join(&safe);
        if let Some(bytes) = read_bounded(&path) {
            return Some((bytes, path));
        }
    }
    None
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() as usize > MAX_FILE_SIZE {
        return None;
    }
    fs::read(path).ok().filter(|b| b.len() <= MAX_FILE_SIZE)
}

fn title_folder() -> String {
    let title = super::auth::title();
    if title != 0 {
        return title.to_string();
    }
    local_store::game_name().unwrap_or_else(|| "default".to_string())
}

fn account_for(owner: u64) -> u64 {
    if owner != 0 {
        return owner;
    }
    let signed = super::auth::signed_in();
    if signed != 0 {
        return signed;
    }
    crate::hub::accounts::my_steam_id()
}

fn user_dir(owner: u64) -> Option<PathBuf> {
    Some(root()?.join("user").join(title_folder()).join(account_for(owner).to_string()))
}

fn user_file_path(owner: u64, name: &str) -> Option<PathBuf> {
    Some(user_dir(owner)?.join(local_store::file_name(name)?))
}

fn write_user_file(owner: u64, name: &str, data: &[u8]) -> Option<PathBuf> {
    if data.len() > MAX_FILE_SIZE {
        return None;
    }
    let path = user_file_path(owner, name)?;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&path, data).ok()?;
    Some(path)
}

fn read_user_file(owner: u64, name: &str) -> Option<Vec<u8>> {
    read_bounded(&user_file_path(owner, name)?)
}

fn file_id(name: &str) -> u64 {
    let h = sha1(name.as_bytes());
    u64::from_le_bytes(h[..8].try_into().unwrap())
}

fn unix_now() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

fn file_mtime(path: &Path) -> u32 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

/// Serializes `bdFileData`: a single typed blob.
fn encode_file_data(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    mm::blob(&mut out, data);
    out
}

/// Serializes `bdFileInfo`: size, file_id, create_time, modified_time, priv, owner_id, filename.
fn encode_file_info(name: &str, size: u32, created: u32, modified: u32, priv_flag: bool, owner_id: u64) -> Vec<u8> {
    let mut out = Vec::new();
    typed::u32(&mut out, size);
    typed::u64(&mut out, file_id(name));
    typed::u32(&mut out, created);
    typed::u32(&mut out, modified);
    typed::bool(&mut out, priv_flag);
    typed::u64(&mut out, owner_id);
    typed::string(&mut out, name.as_bytes());
    out
}

/// Serializes `bdFileQueryResult`: user_id, platform, filename, error_code, data.
fn encode_query_result(user_id: u64, platform: &[u8], name: &str, err: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 32 + name.len());
    typed::u64(&mut out, user_id);
    typed::string(&mut out, platform);
    typed::string(&mut out, name.as_bytes());
    typed::u32(&mut out, err);
    mm::blob(&mut out, data);
    out
}

/// Serializes `bdContextUserStorageFileInfo`: create_time, modified_time, priv, owner_id, account_type, filename.
fn encode_context_file_info(created: u32, modified: u32, priv_flag: bool, owner_id: u64, platform: &[u8], name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    typed::u32(&mut out, created);
    typed::u32(&mut out, modified);
    typed::bool(&mut out, priv_flag);
    typed::u64(&mut out, owner_id);
    typed::string(&mut out, platform);
    typed::string(&mut out, name.as_bytes());
    out
}

/// Serializes `bdFile2`: 3x u32, priv, owner_id, platform, filename, data.
fn encode_file2(priv_flag: bool, owner_id: u64, platform: &[u8], name: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 48 + name.len());
    typed::u32(&mut out, 0);
    typed::u32(&mut out, 0);
    typed::u32(&mut out, 0);
    typed::bool(&mut out, priv_flag);
    typed::u64(&mut out, owner_id);
    typed::string(&mut out, platform);
    typed::string(&mut out, name.as_bytes());
    mm::blob(&mut out, data);
    out
}

/// Simple case-insensitive `*` / `?` glob match for `listPublisherFiles`.
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    let p: Vec<u8> = pattern.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let t: Vec<u8> = text.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut match_ti) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            match_ti = ti;
        } else if let Some(sp) = star {
            pi = sp + 1;
            match_ti += 1;
            ti = match_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn is_ignored_cache_file(name: &str) -> bool {
    name.is_empty()
        || name.starts_with('.')
        || name.to_ascii_lowercase().ends_with(".tmp")
}

/// Lists matching publisher files across candidate folders (deduplicated by lowercase name).
fn list_publisher_files(pattern: &str, max_results: usize, offset: usize) -> Vec<Vec<u8>> {
    let cap = if max_results == 0 { MAX_LIST } else { max_results.min(MAX_LIST) };
    // Exact name (no wildcards): check directly.
    if !pattern.is_empty() && !pattern.contains('*') && !pattern.contains('?') {
        if offset > 0 {
            return Vec::new();
        }
        return match read_publisher_file(pattern) {
            Some((bytes, path)) => {
                let ts = file_mtime(&path);
                vec![encode_file_info(pattern, bytes.len() as u32, ts, ts, false, 0)]
            }
            None => Vec::new(),
        };
    }
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for dir in publisher_dirs() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() || meta.len() as usize > MAX_FILE_SIZE {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_ignored_cache_file(&name) || !glob_match(pattern, &name) {
                continue;
            }
            if !seen.insert(name.to_ascii_lowercase()) {
                continue;
            }
            let ts = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as u32).unwrap_or(0);
            entries.push((name, meta.len() as u32, ts));
        }
    }
    entries.sort_by(|a, b| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()));
    entries
        .into_iter()
        .skip(offset)
        .take(cap)
        .map(|(name, size, ts)| encode_file_info(&name, size, ts, ts, false, 0))
        .collect()
}

fn log_missing_publisher_hint() {
    let key = format!("demonware publisher hint {}", title_folder());
    crate::log::throttled(&key, 1, |_| {
        if let Some(dir) = publisher_hint_dir() {
            log!(
                "demonware storage: publisher files aren't bundled with PeerParty; place cached files in {} (or run once with Demonware set to official so PeerParty caches them from the game's LPC folder)",
                dir.display()
            );
        }
    });
}

/// Dispatches one `bdStorage` (service 10) task.
pub fn handle(task: u8, args: &[u8]) -> StorageReply {
    let mut r = Reader::new(args);
    match task {
        // setUserFile(context, filename, priv, data, [owner])
        10 => {
            let _context = r.string();
            let Some(name_b) = r.string() else { return StorageReply::Unhandled };
            let name = String::from_utf8_lossy(name_b).into_owned();
            let priv_flag = r.bool().unwrap_or(false);
            let Some(data) = r.blob() else { return StorageReply::Unhandled };
            let owner = r.u64().unwrap_or(0);
            let now = unix_now();
            let note = match write_user_file(owner, &name, data) {
                Some(_) => format!("user file {name:?}: saved ({} bytes)", data.len()),
                None => format!("user file {name:?}: not saved ({} bytes)", data.len()),
            };
            let info = encode_file_info(&name, data.len() as u32, now, now, priv_flag, account_for(owner));
            StorageReply::Results { results: vec![info], detail: name, note }
        }
        // getUserFile(context, filename, [owner, platform])
        12 => {
            let _context = r.string();
            let Some(name_b) = r.string() else { return StorageReply::Unhandled };
            let name = String::from_utf8_lossy(name_b).into_owned();
            let owner = r.u64().unwrap_or(0);
            match read_user_file(owner, &name) {
                Some(bytes) => {
                    let note = format!("user file {name:?}: served ({} bytes)", bytes.len());
                    StorageReply::Results { results: vec![encode_file_data(&bytes)], detail: name, note }
                }
                None => StorageReply::Error {
                    code: NO_FILE,
                    detail: name.clone(),
                    why: format!("user file {name:?}: not here (BD_NO_FILE)"),
                },
            }
        }
        // getUserFileList(context, [owner, ...])
        13 => {
            let _context = r.string();
            let owner = r.u64().unwrap_or(0);
            let mut results = Vec::new();
            if let Some(dir) = user_dir(owner) {
                if let Ok(rd) = fs::read_dir(dir) {
                    let mut list: Vec<(String, u32, u32)> = rd
                        .flatten()
                        .filter_map(|e| {
                            let meta = e.metadata().ok()?;
                            if !meta.is_file() || meta.len() as usize > MAX_FILE_SIZE {
                                return None;
                            }
                            let name = e.file_name().to_string_lossy().into_owned();
                            let ts = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as u32).unwrap_or(0);
                            Some((name, meta.len() as u32, ts))
                        })
                        .collect();
                    list.sort_by(|a, b| a.0.cmp(&b.0));
                    for (name, size, ts) in list.into_iter().take(MAX_LIST) {
                        results.push(encode_file_info(&name, size, ts, ts, false, account_for(owner)));
                    }
                }
            }
            let note = format!("user file list: {} file(s)", results.len());
            StorageReply::Results { results, detail: String::new(), note }
        }
        // getFiles(context, user_count, [(user_id, platform)...], file_count, [filename...])
        16 => {
            let _context = r.string();
            let Some(user_count) = r.u32() else { return StorageReply::Unhandled };
            let mut users = Vec::new();
            for _ in 0..(user_count as usize).min(MAX_LIST) {
                let (Some(uid), Some(plat)) = (r.u64(), r.string()) else { break };
                users.push((uid, plat.to_vec()));
            }
            let Some(file_count) = r.u32() else { return StorageReply::Unhandled };
            let mut results = Vec::new();
            let mut names = Vec::new();
            for i in 0..(file_count as usize).min(MAX_LIST) {
                let Some(name_b) = r.string() else { break };
                let name = String::from_utf8_lossy(name_b).into_owned();
                let (uid, plat) = users.get(i).or_else(|| users.first()).map(|(u, p)| (*u, p.as_slice())).unwrap_or((0, b""));
                match read_user_file(uid, &name) {
                    Some(bytes) => results.push(encode_query_result(uid, plat, &name, 0, &bytes)),
                    None => results.push(encode_query_result(uid, plat, &name, NO_FILE, &[])),
                }
                names.push(name);
            }
            let detail = names.join(",");
            let note = format!("batch user files ({}): {} entry(ies)", detail, results.len());
            StorageReply::Results { results, detail, note }
        }
        // uploadFilesNew(context, owner, platform, count, [(filename, data, version, priv)...])
        // uploadFiles(context, owner, platform, count, [(filename, data, unk, priv)...])
        18 | 24 => {
            let _context = r.string();
            let Some(owner) = r.u64() else { return StorageReply::Unhandled };
            let Some(platform) = r.string() else { return StorageReply::Unhandled };
            let Some(count) = r.u32() else { return StorageReply::Unhandled };
            let now = unix_now();
            let mut results = Vec::new();
            let mut names = Vec::new();
            for _ in 0..(count as usize).min(MAX_LIST) {
                let Some(name_b) = r.string() else { break };
                let Some(data) = r.blob() else { break };
                let _ver = r.u32();
                let priv_flag = r.bool().unwrap_or(false);
                let name = String::from_utf8_lossy(name_b).into_owned();
                let _ = write_user_file(owner, &name, data);
                if task == 18 {
                    results.push(encode_context_file_info(now, now, priv_flag, account_for(owner), platform, &name));
                } else {
                    results.push(encode_file2(priv_flag, account_for(owner), platform, &name, data));
                }
                names.push(name);
            }
            let detail = names.join(",");
            let note = format!("uploaded {} user file(s): {detail}", results.len());
            StorageReply::Results { results, detail, note }
        }
        // listPublisherFiles(context, date, num_results, offset, filename)
        20 => {
            let _context = r.string();
            let _date = r.u32();
            let num_results = r.u16().unwrap_or(0) as usize;
            let offset = r.u16().unwrap_or(0) as usize;
            let pattern = r.string().map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
            let results = list_publisher_files(&pattern, num_results, offset);
            let note = format!("list publisher files {pattern:?}: {} match(es)", results.len());
            StorageReply::Results { results, detail: pattern, note }
        }
        // getPublisherFile(context, filename)
        21 => {
            let _context = r.string();
            let name = r.string().map(|n| String::from_utf8_lossy(n).into_owned()).unwrap_or_default();
            match read_publisher_file(&name) {
                Some((bytes, path)) => {
                    let note = format!("publisher file {name:?}: served {} bytes from {}", bytes.len(), path.display());
                    StorageReply::Results { results: vec![encode_file_data(&bytes)], detail: name, note }
                }
                None => {
                    log_missing_publisher_hint();
                    StorageReply::Error {
                        code: NO_FILE,
                        detail: name.clone(),
                        why: format!("publisher file {name:?}: not here (BD_NO_FILE)"),
                    }
                }
            }
        }
        _ => StorageReply::Unhandled,
    }
}

// --- Official-mode LPC harvester ------------------------------------------------------------

/// Previously copied LPC files: lowercase filename -> (len, mtime secs).
static HARVESTED: LazyLock<Mutex<HashMap<String, (u64, u32)>>> = LazyLock::new(Mutex::default);
static SWEEP_PENDING: AtomicBool = AtomicBool::new(false);

/// Whether the Demonware layer is running against official servers and should snapshot
/// verified files the game writes into its `LPC\` folder into PeerParty's publisher cache.
pub fn harvest_wanted() -> bool {
    crate::net::enabled() && !crate::net::skip("demonware") && crate::config::modes::official("demonware")
}

/// Whether `path` is a file directly inside `<game_dir>\LPC\`.
fn lpc_file_in_game_dir(path: &str) -> Option<(PathBuf, String)> {
    let norm = path.trim().trim_matches('"').replace('/', "\\");
    let lower = norm.to_ascii_lowercase();
    let idx = lower.rfind("\\lpc\\")?;
    let name = &norm[idx + 5..];
    if name.is_empty() || name.contains('\\') {
        return None;
    }
    let parent = Path::new(&norm[..idx]);
    if let Some(g) = game_dir() {
        let g_norm = g.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
        let p_norm = parent.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase();
        let p_norm = p_norm.strip_prefix(r"\\?\").unwrap_or(&p_norm);
        let g_norm = g_norm.strip_prefix(r"\\?\").unwrap_or(&g_norm);
        if p_norm != g_norm {
            return None;
        }
    }
    Some((PathBuf::from(&norm[..idx + 4]), name.to_string()))
}

/// Copies one finalized file from `lpc_dir\name` into `<root>\publisher\<game_name>\name`.
fn copy_lpc_file(lpc_dir: &Path, name: &str) -> bool {
    if is_ignored_cache_file(name) {
        return false;
    }
    let Some(safe) = local_store::file_name(name) else { return false };
    let src = lpc_dir.join(name);
    let Ok(meta) = fs::metadata(&src) else { return false };
    if !meta.is_file() || meta.len() == 0 || meta.len() as usize > MAX_FILE_SIZE {
        return false;
    }
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as u32).unwrap_or(0);
    let stamp = (meta.len(), mtime);
    let key = safe.to_ascii_lowercase();
    if crate::sys::lock(&HARVESTED).get(&key) == Some(&stamp) {
        return false;
    }
    let Some(dst_dir) = root().and_then(|r| Some(r.join("publisher").join(local_store::game_name()?))) else {
        return false;
    };
    let Ok(bytes) = fs::read(&src) else { return false };
    if bytes.is_empty() || bytes.len() > MAX_FILE_SIZE {
        return false;
    }
    let _ = fs::create_dir_all(&dst_dir);
    let dst = dst_dir.join(&safe);
    let tmp = dst_dir.join(format!(".tmp-{safe}-{}", std::process::id()));
    if fs::write(&tmp, &bytes).is_err() {
        let _ = fs::remove_file(&tmp);
        return false;
    }
    if fs::rename(&tmp, &dst).is_err() {
        let _ = fs::copy(&tmp, &dst);
        let _ = fs::remove_file(&tmp);
    }
    crate::sys::lock(&HARVESTED).insert(key, stamp);
    log!("demonware storage: cached publisher file {safe} ({} bytes) from official session into {}", bytes.len(), dst.display());
    true
}

/// Scans an `LPC\` folder and caches any finalized publisher files not yet saved.
pub fn harvest_lpc_dir(lpc_dir: &Path) -> usize {
    let Ok(rd) = fs::read_dir(lpc_dir) else { return 0 };
    let mut n = 0;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if copy_lpc_file(lpc_dir, &name) {
            n += 1;
        }
    }
    n
}

/// Called from the file hooks after a write or rename touches a path while Demonware is
/// in `official` mode: snapshots finalized `LPC\` files into PeerParty's publisher cache.
pub fn note_lpc_write(path: &str, finalized: bool) {
    if !harvest_wanted() {
        return;
    }
    let Some((lpc_dir, name)) = lpc_file_in_game_dir(path) else {
        return;
    };
    if finalized && !is_ignored_cache_file(&name) {
        copy_lpc_file(&lpc_dir, &name);
        return;
    }
    // Direct writes (without rename) finish after CreateFile returns: sweep shortly after.
    if !SWEEP_PENDING.swap(true, Ordering::AcqRel) {
        let _ = std::thread::Builder::new().name("peerparty-dw-harvest".into()).spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            SWEEP_PENDING.store(false, Ordering::Release);
            let _ = harvest_lpc_dir(&lpc_dir);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TempEnv {
        dir: PathBuf,
        prev_dw: Option<String>,
        prev_root: Option<String>,
    }

    impl TempEnv {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("peerparty-dw-storage-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let prev_dw = std::env::var(peerparty_protocol::env::DEMONWARE_DIR).ok();
            let prev_root = std::env::var(peerparty_protocol::env::GAME_ROOT).ok();
            unsafe {
                std::env::set_var(peerparty_protocol::env::DEMONWARE_DIR, dir.join("dw"));
                std::env::set_var(peerparty_protocol::env::GAME_ROOT, dir.join("game"));
            }
            crate::config::reload();
            Self { dir, prev_dw, prev_root }
        }
    }

    impl Drop for TempEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_dw {
                    Some(v) => std::env::set_var(peerparty_protocol::env::DEMONWARE_DIR, v),
                    None => std::env::remove_var(peerparty_protocol::env::DEMONWARE_DIR),
                }
                match &self.prev_root {
                    Some(v) => std::env::set_var(peerparty_protocol::env::GAME_ROOT, v),
                    None => std::env::remove_var(peerparty_protocol::env::GAME_ROOT),
                }
            }
            crate::config::reload();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn glob_matches_publisher_patterns_case_insensitively() {
        assert!(glob_match("*", "playlists_tu23.gz"));
        assert!(glob_match("*.gz", "PLAYLISTS_TU23.GZ"));
        assert!(glob_match("motd-*.gz", "motd-english_tu23.gz"));
        assert!(glob_match("file_?.bin", "file_a.bin"));
        assert!(!glob_match("file_?.bin", "file_ab.bin"));
        assert!(!glob_match("*.ff", "playlists_tu23.gz"));
    }

    #[test]
    fn publisher_files_serve_verbatim_and_fall_back_to_lpc() {
        let _g = crate::sys::lock(&TEST_LOCK);
        let env = TempEnv::new("pub");
        let lpc = env.dir.join("game").join("LPC");
        fs::create_dir_all(&lpc).unwrap();
        let lpc_bytes = b"SIGNED-LPC-BYTES\x00\x01\x02";
        fs::write(lpc.join("playlists.gz"), lpc_bytes).unwrap();

        // Served directly from LPC when not yet in peerparty's vault.
        let mut ask = Vec::new();
        typed::string(&mut ask, b"");
        typed::string(&mut ask, b"playlists.gz");
        match handle(21, &ask) {
            StorageReply::Results { results, detail, .. } => {
                assert_eq!(detail, "playlists.gz");
                assert_eq!(results.len(), 1);
                let mut r = Reader::new(&results[0]);
                assert_eq!(r.blob().unwrap(), lpc_bytes);
            }
            _ => panic!("expected publisher file from LPC"),
        }

        // Vault file takes priority over LPC.
        let game = local_store::game_name().unwrap();
        let vault = env.dir.join("dw").join("publisher").join(&game);
        fs::create_dir_all(&vault).unwrap();
        let vault_bytes = b"VAULT-OVERRIDE-SIGNED";
        fs::write(vault.join("playlists.gz"), vault_bytes).unwrap();
        match handle(21, &ask) {
            StorageReply::Results { results, .. } => {
                let mut r = Reader::new(&results[0]);
                assert_eq!(r.blob().unwrap(), vault_bytes);
            }
            _ => panic!("expected publisher file from vault"),
        }

        // Missing file returns BD_NO_FILE.
        let mut missing = Vec::new();
        typed::string(&mut missing, b"");
        typed::string(&mut missing, b"missing.ff");
        match handle(21, &missing) {
            StorageReply::Error { code, detail, .. } => {
                assert_eq!(code, NO_FILE);
                assert_eq!(detail, "missing.ff");
            }
            _ => panic!("expected BD_NO_FILE"),
        }

        // listPublisherFiles (task 20) deduplicates across vault and LPC and skips dotfiles.
        fs::write(lpc.join(".manifest"), b"\x03\x00\x00\x00\x00\x00\x00\x00").unwrap();
        fs::write(lpc.join("core_ffotd.ff"), b"FFOTD").unwrap();
        let mut list_args = Vec::new();
        typed::string(&mut list_args, b"");
        typed::u32(&mut list_args, 0);
        typed::u16(&mut list_args, 10);
        typed::u16(&mut list_args, 0);
        typed::string(&mut list_args, b"*");
        match handle(20, &list_args) {
            StorageReply::Results { results, .. } => {
                assert_eq!(results.len(), 2);
                let names: Vec<String> = results
                    .iter()
                    .map(|res| {
                        let mut r = Reader::new(res);
                        let _sz = r.u32().unwrap();
                        let _id = r.u64().unwrap();
                        let _ct = r.u32().unwrap();
                        let _mt = r.u32().unwrap();
                        let _pr = r.bool().unwrap();
                        let _ow = r.u64().unwrap();
                        String::from_utf8_lossy(r.string().unwrap()).into_owned()
                    })
                    .collect();
                assert_eq!(names, ["core_ffotd.ff", "playlists.gz"]);
            }
            _ => panic!("expected list results"),
        }
    }

    #[test]
    fn user_files_persist_and_stay_isolated_per_owner() {
        let _g = crate::sys::lock(&TEST_LOCK);
        let _env = TempEnv::new("user");

        // Task 10: upload user file for owner 111.
        let mut up = Vec::new();
        typed::string(&mut up, b"");
        typed::string(&mut up, b"statsarchive00");
        typed::bool(&mut up, false);
        mm::blob(&mut up, b"player-one-stats");
        typed::u64(&mut up, 111);
        match handle(10, &up) {
            StorageReply::Results { results, detail, .. } => {
                assert_eq!(detail, "statsarchive00");
                assert_eq!(results.len(), 1);
                let mut r = Reader::new(&results[0]);
                assert_eq!(r.u32(), Some(16));
                let _id = r.u64().unwrap();
                let _ct = r.u32().unwrap();
                let _mt = r.u32().unwrap();
                assert_eq!(r.bool(), Some(false));
                assert_eq!(r.u64(), Some(111));
                assert_eq!(r.string(), Some(&b"statsarchive00"[..]));
            }
            _ => panic!("expected upload info"),
        }

        // Task 12: owner 111 reads it back; owner 222 gets BD_NO_FILE.
        let read_for = |owner: u64| {
            let mut args = Vec::new();
            typed::string(&mut args, b"");
            typed::string(&mut args, b"statsarchive00");
            typed::u64(&mut args, owner);
            typed::string(&mut args, b"steam");
            handle(12, &args)
        };
        match read_for(111) {
            StorageReply::Results { results, .. } => {
                let mut r = Reader::new(&results[0]);
                assert_eq!(r.blob().unwrap(), b"player-one-stats");
            }
            _ => panic!("expected owner 111 file"),
        }
        match read_for(222) {
            StorageReply::Error { code, .. } => assert_eq!(code, NO_FILE),
            _ => panic!("owner 222 must not read owner 111's file"),
        }

        // Task 16: batch get returns data for existing file and NO_FILE for missing one.
        let mut batch = Vec::new();
        typed::string(&mut batch, b"");
        typed::u32(&mut batch, 2);
        typed::u64(&mut batch, 111);
        typed::string(&mut batch, b"steam");
        typed::u64(&mut batch, 111);
        typed::string(&mut batch, b"steam");
        typed::u32(&mut batch, 2);
        typed::string(&mut batch, b"statsarchive00");
        typed::string(&mut batch, b"metplayers");
        match handle(16, &batch) {
            StorageReply::Results { results, .. } => {
                assert_eq!(results.len(), 2);
                let mut r0 = Reader::new(&results[0]);
                assert_eq!(r0.u64(), Some(111));
                assert_eq!(r0.string(), Some(&b"steam"[..]));
                assert_eq!(r0.string(), Some(&b"statsarchive00"[..]));
                assert_eq!(r0.u32(), Some(0));
                assert_eq!(r0.blob().unwrap(), b"player-one-stats");
                let mut r1 = Reader::new(&results[1]);
                assert_eq!(r1.u64(), Some(111));
                let _ = r1.string();
                assert_eq!(r1.string(), Some(&b"metplayers"[..]));
                assert_eq!(r1.u32(), Some(NO_FILE));
                assert_eq!(r1.blob().unwrap(), b"");
            }
            _ => panic!("expected batch results"),
        }
    }

    #[test]
    fn lpc_harvester_copies_finalized_files_and_skips_temps() {
        let _g = crate::sys::lock(&TEST_LOCK);
        let env = TempEnv::new("harvest");
        crate::sys::lock(&HARVESTED).clear();
        let lpc = env.dir.join("game").join("LPC");
        fs::create_dir_all(&lpc).unwrap();
        fs::write(lpc.join(".manifest"), b"\x03\x00\x00\x00\x00\x00\x00\x00").unwrap();
        fs::write(lpc.join(".tmp-playlists.gz-123"), b"partial").unwrap();
        fs::write(lpc.join("playlists_tu23.gz"), b"SIGNED-PLAYLIST").unwrap();

        assert_eq!(harvest_lpc_dir(&lpc), 1);
        // Second pass skips unchanged files.
        assert_eq!(harvest_lpc_dir(&lpc), 0);
        let game = local_store::game_name().unwrap();
        let cached = env.dir.join("dw").join("publisher").join(&game).join("playlists_tu23.gz");
        assert_eq!(fs::read(&cached).unwrap(), b"SIGNED-PLAYLIST");
        assert!(!env.dir.join("dw").join("publisher").join(&game).join(".manifest").exists());
    }
}
