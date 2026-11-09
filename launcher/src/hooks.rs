//! The hook DLLs are embedded in `peerparty.exe` (see build.rs) and written out on
//! demand to `%LOCALAPPDATA%\peerparty\hooks\<content hash>\`. A new build lands in a
//! new folder, so a game still running with the old DLL never blocks it.

use std::env;
use std::fs;
use std::path::PathBuf;

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

fn matches(path: &PathBuf, bytes: &[u8]) -> bool {
    fs::read(path).is_ok_and(|on_disk| on_disk == bytes)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}
