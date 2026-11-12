//! The hook DLLs are embedded in `peerparty.exe` (see build.rs) and written out on
//! demand to `%LOCALAPPDATA%\peerparty\hooks\<content hash>\`. A new build lands in a
//! new folder, so a game still running with the old DLL never blocks it.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

static HOOK64: &[u8] = include_bytes!(env!("PEERPARTY_HOOK64"));
static HOOK32: &[u8] = include_bytes!(env!("PEERPARTY_HOOK32"));

/// Path of the hook DLL for a game of that bitness, written out if needed.
pub fn extract(x86: bool) -> Result<PathBuf, String> {
    let (bytes, name) = if x86 {
        (HOOK32, "peerparty_hook32.dll")
    } else {
        (HOOK64, "peerparty_hook.dll")
    };
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let dir = base
        .join("peerparty")
        .join("hooks")
        .join(format!("{:016x}", fnv1a(bytes)));
    let path = dir.join(name);
    if matches(&path, bytes) {
        return Ok(path);
    }
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Write beside it and rename, so a second launcher never loads a partial file.
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        // Another launcher may have won the race, or a game holds the file open.
        if !matches(&path, bytes) {
            return Err(format!("{}: {e}", path.display()));
        }
    }
    Ok(path)
}

/// How long an earlier build's hook folder, or a log, is kept.
const KEEP_HOOKS: Duration = Duration::from_secs(7 * 24 * 3600);
const KEEP_LOGS: Duration = Duration::from_secs(30 * 24 * 3600);

/// Clears out, on a thread of its own, the hook folders of earlier builds not written in
/// a week (one a running game still has loaded can't be deleted, and stays; another
/// copy of PeerParty of a different build writes its own again when it needs it) and
/// logs in the usual log folder older than a month.
pub fn clean_up() {
    let hooks = env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(env::temp_dir).join("peerparty").join("hooks");
    let current = [format!("{:016x}", fnv1a(HOOK64)), format!("{:016x}", fnv1a(HOOK32))];
    let logs = crate::platform::logs_dir();
    std::thread::spawn(move || {
        let now = SystemTime::now();
        let old = |e: &fs::DirEntry, keep: Duration| {
            e.metadata().and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > keep))
        };
        for e in fs::read_dir(&hooks).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) && name.len() == 16 && !current.contains(&name) && old(&e, KEEP_HOOKS) {
                let _ = fs::remove_dir_all(e.path());
            }
        }
        for e in fs::read_dir(&logs).into_iter().flatten().flatten() {
            if e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("log")) && e.file_type().is_ok_and(|t| t.is_file()) && old(&e, KEEP_LOGS) {
                let _ = fs::remove_file(e.path());
            }
        }
    });
}

fn matches(path: &PathBuf, bytes: &[u8]) -> bool {
    fs::read(path).is_ok_and(|on_disk| on_disk == bytes)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}
