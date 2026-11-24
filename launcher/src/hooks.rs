//! The hook DLLs are embedded in `peerparty.exe`, compressed (see build.rs), and written
//! out on demand to `hooks\<content hash>\` in PeerParty's folder (`platform::data_dir`).
//! A new build lands in a new folder, so a game still running with the old DLL never
//! blocks it.

use std::env;
use std::fs;
#[cfg(windows)]
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Each blob: an 8-byte tag, the DLL's length and FNV-1a hash (both u64 little-endian,
/// worked out by build.rs), then the DLL as a raw deflate stream.
static HOOK64: &[u8] = include_bytes!(env!("PEERPARTY_HOOK64"));
static HOOK32: &[u8] = include_bytes!(env!("PEERPARTY_HOOK32"));

const HEADER: usize = 24;

/// A blob's DLL length and hash, from its header.
fn header(blob: &[u8]) -> (u64, u64) {
    let word = |at: usize| u64::from_le_bytes(blob[at..at + 8].try_into().unwrap());
    (word(8), word(16))
}

/// The folder name for a blob's DLL: its hash.
fn folder(blob: &[u8]) -> String {
    format!("{:016x}", header(blob).1)
}

/// Path of the hook DLL for a game of that bitness, written out if needed. (On Linux the
/// Windows peerparty.exe that starts the game inside Proton writes out its own.)
#[cfg(windows)]
pub fn extract(x86: bool) -> Result<PathBuf, String> {
    let (blob, name) = if x86 {
        (HOOK32, "peerparty_hook32.dll")
    } else {
        (HOOK64, "peerparty_hook.dll")
    };
    let (len, hash) = header(blob);
    let dir = crate::platform::data_dir().join("hooks").join(folder(blob));
    let path = dir.join(name);
    // Only a whole file is ever renamed into place, and the folder is named by the
    // content's hash: one of the right length is this DLL.
    if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == len) {
        return Ok(path);
    }
    let bytes = inflate(&blob[HEADER..], len as usize)?;
    if fnv1a(&bytes) != hash {
        return Err(format!("the embedded {name} is damaged (its hash doesn't match)"));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Write beside it and rename, so a second launcher never loads a partial file.
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        // Another launcher may have won the race, or a game holds the file open.
        if !fs::read(&path).is_ok_and(|on_disk| on_disk == bytes) {
            return Err(format!("{}: {e}", path.display()));
        }
    }
    Ok(path)
}

/// A raw deflate stream of `len` bytes, inflated.
fn inflate(z: &[u8], len: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::with_capacity(len);
    flate2::read::DeflateDecoder::new(z).take(len as u64 + 1).read_to_end(&mut out).map_err(|e| format!("inflating a hook: {e}"))?;
    if out.len() != len {
        return Err(format!("inflating a hook: {} bytes, not {len}", out.len()));
    }
    Ok(out)
}

/// How long an earlier build's hook folder, or a log, is kept.
const KEEP_HOOKS: Duration = Duration::from_secs(7 * 24 * 3600);
const KEEP_LOGS: Duration = Duration::from_secs(30 * 24 * 3600);

/// Clears out, on a thread of its own, the hook folders of earlier builds not written in
/// a week (one a running game still has loaded can't be deleted, and stays; another
/// copy of PeerParty of a different build writes its own again when it needs it) and
/// logs in the usual log folder older than a month.
pub fn clean_up() {
    let hooks = crate::platform::data_dir().join("hooks");
    let current = [folder(HOOK64), folder(HOOK32)];
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

/// The same hash as build.rs's, which names each hook's folder.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both embedded blobs inflate to the length and hash their headers give (an empty
    /// stub included).
    #[test]
    fn embedded_hooks_inflate_to_their_header() {
        for blob in [HOOK64, HOOK32] {
            assert!(blob.len() >= HEADER);
            assert!(blob.starts_with(b"PPHK"));
            let (len, hash) = header(blob);
            let dll = inflate(&blob[HEADER..], len as usize).unwrap();
            assert_eq!(fnv1a(&dll), hash);
            assert!(dll.is_empty() || dll.starts_with(b"MZ"));
        }
    }
}
