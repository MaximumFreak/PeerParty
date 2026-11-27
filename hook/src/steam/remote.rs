//! A later player's Steam Remote Storage (Steam Cloud's file calls): in a copy that is
//! player 2–4 (saves/mod.rs), the game's `FileWrite`/`FileRead` and the rest go to that
//! player's own folder, `<data>\saves\<game>\player-N\steam-remote\`, not to the PC's
//! Steam account, whose files are player 1's. Player 1's copy passes every call to
//! steam_api as it is (noting, when watching, that the game uses it).
//!
//! Only the save files are stood in for: workshop and UGC calls, and anything about
//! what's owned, go to steam_api as they are. A file name the game gives with `/` in it
//! is kept as folders. `FileShare` answers no call (k_uAPICallInvalid): sharing a later
//! player's file through player 1's account isn't offered.

use std::collections::HashMap;
use std::ffi::{CString, c_char};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::log;
use crate::steam::obj::call_real;
use crate::sys::vtable::vmethod;

/// Steam's cap on one file (`k_unMaxCloudFileChunkSize`).
const MAX_FILE: usize = 100 << 20;
/// What the quota says: a later player's files are on this PC's disk.
const QUOTA: u64 = 1 << 30;
const RESULT_OK: i32 = 1;
const RESULT_FAIL: i32 = 2;
const RESULT_FILE_NOT_FOUND: i32 = 9;
/// `RemoteStorageFileWriteAsyncComplete_t`, `RemoteStorageFileReadAsyncComplete_t`.
const WRITE_ASYNC_DONE: i32 = 1331;
const READ_ASYNC_DONE: i32 = 1332;

/// The stand-in's methods, for a copy whose saves may be kept apart.
pub fn remote_storage() -> Vec<(&'static str, usize)> {
    let c = crate::config::HookConfig::global();
    if !(crate::saves::own_saves() || crate::saves::watching() || c.multi_instance || c.player_slot.is_some()) {
        return Vec::new();
    }
    vec![
        ("FileWrite", v_write as *const () as usize),
        ("FileRead", v_read as *const () as usize),
        ("FileWriteAsync", v_write_async as *const () as usize),
        ("FileReadAsync", v_read_async as *const () as usize),
        ("FileReadAsyncComplete", v_read_async_complete as *const () as usize),
        ("FileForget", v_forget as *const () as usize),
        ("FileDelete", v_delete as *const () as usize),
        ("FileShare", v_share as *const () as usize),
        ("SetSyncPlatforms", v_set_sync as *const () as usize),
        ("FileWriteStreamOpen", v_stream_open as *const () as usize),
        ("FileWriteStreamWriteChunk", v_stream_chunk as *const () as usize),
        ("FileWriteStreamClose", v_stream_close as *const () as usize),
        ("FileWriteStreamCancel", v_stream_cancel as *const () as usize),
        ("FileExists", v_exists as *const () as usize),
        ("FilePersisted", v_persisted as *const () as usize),
        ("GetFileSize", v_size as *const () as usize),
        ("GetFileTimestamp", v_timestamp as *const () as usize),
        ("GetSyncPlatforms", v_get_sync as *const () as usize),
        ("GetFileCount", v_count as *const () as usize),
        ("GetFileNameAndSize", v_name_and_size as *const () as usize),
        ("GetQuota", v_quota as *const () as usize),
        ("IsCloudEnabledForAccount", v_enabled_account as *const () as usize),
        ("IsCloudEnabledForApp", v_enabled_app as *const () as usize),
        ("SetCloudEnabledForApp", v_set_enabled as *const () as usize),
        ("BeginFileWriteBatch", v_batch as *const () as usize),
        ("EndFileWriteBatch", v_batch as *const () as usize),
        ("GetLocalFileChangeCount", v_change_count as *const () as usize),
    ]
}

/// A later player's folder for its files; None in player 1's copy.
fn dir() -> Option<PathBuf> {
    let st = crate::saves::vfs::state().filter(|s| s.slot != 0)?;
    let d = st.player.join(peerparty_protocol::saves::STEAM_REMOTE);
    static TOLD: AtomicBool = AtomicBool::new(false);
    if !TOLD.swap(true, Ordering::Relaxed) {
        log!("steam-remote: player {}'s Steam Remote Storage files are its own, in {}", st.slot + 1, d.display());
    }
    Some(d)
}

/// Player 1 writes through Steam Remote Storage: noted, when watching.
fn watched() {
    static NOTED: AtomicBool = AtomicBool::new(false);
    if let Some(st) = crate::saves::vfs::state().filter(|s| s.slot == 0) {
        if !NOTED.swap(true, Ordering::Relaxed) {
            crate::saves::vfs::note(&st, peerparty_protocol::saves::Discovered::SteamRemote);
        }
    }
}

/// The file for the game's name `name` in `dir`: each `/`-separated part made one plain
/// file name (local_store `file_name`).
fn file_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut p = dir.to_path_buf();
    let mut any = false;
    for part in name.split(['/', '\\']).filter(|s| !s.is_empty()) {
        p.push(crate::util::local_store::file_name(part)?);
        any = true;
    }
    any.then_some(p)
}

fn name_of(p: *const c_char) -> Option<String> {
    if p.is_null() { None } else { crate::sys::read::cstr(p as usize) }
}

/// The player's files, by the game's names (`a/b.sav`), sorted.
fn list(dir: &Path) -> Vec<(String, u64)> {
    fn walk(base: &Path, p: &Path, out: &mut Vec<(String, u64)>) {
        for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
            if out.len() >= 10_000 {
                return;
            }
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                // write_whole's work folder.
                if e.file_name() != ".peerparty-writing" {
                    walk(base, &path, out);
                }
            } else if let Ok(rel) = path.strip_prefix(base) {
                out.push((rel.to_string_lossy().replace('\\', "/"), meta.len()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn write_file(dir: &Path, name: &str, data: &[u8]) -> bool {
    let Some(p) = file_in(dir, name) else { return false };
    let ok = p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && crate::saves::write_whole(&p, data).is_ok();
    crate::log::throttled("steam-remote write", 6, |_| log!("steam-remote: {name} ({} bytes) {}", data.len(), if ok { "written" } else { "couldn't be written" }));
    ok
}

fn read_file(dir: &Path, name: &str) -> Option<Vec<u8>> {
    std::fs::read(file_in(dir, name)?).ok()
}

// ---- the methods ----

vmethod! {
    fn v_write(this: usize, file: *const c_char, data: *const u8, len: i32) -> bool {
        let Some(d) = dir() else {
            watched();
            let ok = call_real!(this, "FileWrite", fn(*const c_char, *const u8, i32) -> bool, false, file, data, len);
            let name = name_of(file).unwrap_or_default();
            if !ok {
                crate::log::throttled("steam-remote p1 write fail", 6, |n| {
                    log!("steam-remote: player 1 FileWrite({name}, {len} bytes) failed (n={n})")
                });
            } else {
                crate::log::throttled("steam-remote p1 write", 6, |_| {
                    log!("steam-remote: player 1 FileWrite({name}, {len} bytes) ok")
                });
            }
            return ok;
        };
        let (Some(name), Ok(n)) = (name_of(file), usize::try_from(len)) else { return false };
        if n > MAX_FILE || (data.is_null() && n > 0) {
            return false;
        }
        let bytes = if n == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(data, n) } };
        write_file(&d, &name, bytes)
    }
}

vmethod! {
    fn v_read(this: usize, file: *const c_char, out: *mut u8, len: i32) -> i32 {
        let Some(d) = dir() else {
            let got = call_real!(this, "FileRead", fn(*const c_char, *mut u8, i32) -> i32, 0, file, out, len);
            if got == 0 && len > 0 {
                let name = name_of(file).unwrap_or_default();
                crate::log::throttled("steam-remote p1 read 0", 6, |n| {
                    log!("steam-remote: player 1 FileRead({name}, {len} bytes) returned 0 (n={n})")
                });
            }
            return got;
        };
        let (Some(name), Ok(n)) = (name_of(file), usize::try_from(len)) else { return 0 };
        let Some(data) = read_file(&d, &name) else {
            if n > 0 {
                crate::log::throttled("steam-remote read 0", 6, |c| {
                    log!("steam-remote: FileRead({name}, {n} bytes) not found (n={c})")
                });
            }
            return 0;
        };
        let k = data.len().min(n);
        if k > 0 && !out.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), out, k) };
        }
        k as i32
    }
}

vmethod! {
    fn v_write_async(this: usize, file: *const c_char, data: *const u8, len: u32) -> u64 {
        let Some(d) = dir() else {
            watched();
            let call = call_real!(this, "FileWriteAsync", fn(*const c_char, *const u8, u32) -> u64, 0, file, data, len);
            if call == 0 {
                let name = name_of(file).unwrap_or_default();
                crate::log::throttled("steam-remote p1 write_async fail", 6, |n| {
                    log!("steam-remote: player 1 FileWriteAsync({name}, {len} bytes) returned 0 (n={n})")
                });
            }
            return call;
        };
        let n = len as usize;
        let ok = match name_of(file) {
            Some(name) if n <= MAX_FILE && !(data.is_null() && n > 0) => {
                let bytes = if n == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(data, n) } };
                write_file(&d, &name, bytes)
            }
            _ => false,
        };
        let call = crate::steam::new_call();
        crate::steam::complete(call, WRITE_ASYNC_DONE, (if ok { RESULT_OK } else { RESULT_FAIL }).to_le_bytes().to_vec());
        call
    }
}

/// Async reads done, waiting for `FileReadAsyncComplete`: call -> the bytes read.
static READS: LazyLock<Mutex<HashMap<u64, Vec<u8>>>> = LazyLock::new(Mutex::default);

vmethod! {
    fn v_read_async(this: usize, file: *const c_char, offset: u32, len: u32) -> u64 {
        let Some(d) = dir() else {
            return call_real!(this, "FileReadAsync", fn(*const c_char, u32, u32) -> u64, 0, file, offset, len);
        };
        let call = crate::steam::new_call();
        let data = name_of(file).and_then(|n| read_file(&d, &n));
        let (result, got) = match &data {
            Some(all) if (offset as usize) <= all.len() => {
                let end = (offset as usize).saturating_add(len as usize).min(all.len());
                (RESULT_OK, all[offset as usize..end].to_vec())
            }
            Some(_) => (RESULT_FAIL, Vec::new()),
            None => (RESULT_FILE_NOT_FOUND, Vec::new()),
        };
        // RemoteStorageFileReadAsyncComplete_t (8-byte packing): the call, the result,
        // the offset, the bytes read.
        let mut r = Vec::with_capacity(24);
        r.extend_from_slice(&call.to_le_bytes());
        r.extend_from_slice(&result.to_le_bytes());
        r.extend_from_slice(&offset.to_le_bytes());
        r.extend_from_slice(&(got.len() as u32).to_le_bytes());
        r.extend_from_slice(&[0u8; 4]);
        {
            let mut reads = crate::sys::lock(&READS);
            // A game that never collects its reads mustn't keep them all.
            if reads.len() >= 64 {
                if let Some(old) = reads.keys().min().copied() {
                    reads.remove(&old);
                }
            }
            reads.insert(call, got);
        }
        crate::steam::complete(call, READ_ASYNC_DONE, r);
        call
    }
}

vmethod! {
    fn v_read_async_complete(this: usize, call: u64, out: *mut u8, len: u32) -> bool {
        if dir().is_none() {
            return call_real!(this, "FileReadAsyncComplete", fn(u64, *mut u8, u32) -> bool, false, call, out, len);
        }
        let Some(data) = crate::sys::lock(&READS).remove(&call) else { return false };
        let k = data.len().min(len as usize);
        if k > 0 && !out.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), out, k) };
        }
        true
    }
}

vmethod! {
    fn v_forget(this: usize, file: *const c_char) -> bool {
        if dir().is_none() {
            return call_real!(this, "FileForget", fn(*const c_char) -> bool, false, file);
        }
        // Kept on this PC, not uploaded: there is no upload.
        true
    }
}

vmethod! {
    fn v_delete(this: usize, file: *const c_char) -> bool {
        let Some(d) = dir() else {
            return call_real!(this, "FileDelete", fn(*const c_char) -> bool, false, file);
        };
        name_of(file).and_then(|n| file_in(&d, &n)).is_some_and(|p| std::fs::remove_file(p).is_ok())
    }
}

vmethod! {
    fn v_share(this: usize, file: *const c_char) -> u64 {
        if dir().is_none() {
            return call_real!(this, "FileShare", fn(*const c_char) -> u64, 0, file);
        }
        crate::log::throttled("steam-remote share", 2, |_| log!("gap: steam-remote: FileShare from a later player isn't offered (no call)"));
        0
    }
}

vmethod! {
    fn v_set_sync(this: usize, file: *const c_char, platforms: i32) -> bool {
        if dir().is_none() {
            return call_real!(this, "SetSyncPlatforms", fn(*const c_char, i32) -> bool, false, file, platforms);
        }
        true
    }
}

/// Write streams open: handle -> (the game's name, the bytes so far).
static STREAMS: LazyLock<Mutex<HashMap<u64, (String, Vec<u8>)>>> = LazyLock::new(Mutex::default);
static NEXT_STREAM: AtomicU64 = AtomicU64::new(0x5050_0000_0000_0001);

vmethod! {
    fn v_stream_open(this: usize, file: *const c_char) -> u64 {
        if dir().is_none() {
            watched();
            return call_real!(this, "FileWriteStreamOpen", fn(*const c_char) -> u64, u64::MAX, file);
        }
        let Some(name) = name_of(file) else { return u64::MAX };
        let mut s = crate::sys::lock(&STREAMS);
        if s.len() >= 16 {
            return u64::MAX;
        }
        let h = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
        s.insert(h, (name, Vec::new()));
        h
    }
}

vmethod! {
    fn v_stream_chunk(this: usize, handle: u64, data: *const u8, len: i32) -> bool {
        if dir().is_none() {
            return call_real!(this, "FileWriteStreamWriteChunk", fn(u64, *const u8, i32) -> bool, false, handle, data, len);
        }
        let Ok(n) = usize::try_from(len) else { return false };
        let mut s = crate::sys::lock(&STREAMS);
        let Some((_, buf)) = s.get_mut(&handle) else { return false };
        if buf.len() + n > MAX_FILE || (data.is_null() && n > 0) {
            return false;
        }
        if n > 0 {
            buf.extend_from_slice(unsafe { std::slice::from_raw_parts(data, n) });
        }
        true
    }
}

vmethod! {
    fn v_stream_close(this: usize, handle: u64) -> bool {
        let Some(d) = dir() else {
            return call_real!(this, "FileWriteStreamClose", fn(u64) -> bool, false, handle);
        };
        let Some((name, buf)) = crate::sys::lock(&STREAMS).remove(&handle) else { return false };
        write_file(&d, &name, &buf)
    }
}

vmethod! {
    fn v_stream_cancel(this: usize, handle: u64) -> bool {
        if dir().is_none() {
            return call_real!(this, "FileWriteStreamCancel", fn(u64) -> bool, false, handle);
        }
        crate::sys::lock(&STREAMS).remove(&handle).is_some()
    }
}

vmethod! {
    fn v_exists(this: usize, file: *const c_char) -> bool {
        let Some(d) = dir() else {
            return call_real!(this, "FileExists", fn(*const c_char) -> bool, false, file);
        };
        name_of(file).and_then(|n| file_in(&d, &n)).is_some_and(|p| p.is_file())
    }
}

vmethod! {
    fn v_persisted(this: usize, file: *const c_char) -> bool {
        let Some(d) = dir() else {
            return call_real!(this, "FilePersisted", fn(*const c_char) -> bool, false, file);
        };
        name_of(file).and_then(|n| file_in(&d, &n)).is_some_and(|p| p.is_file())
    }
}

vmethod! {
    fn v_size(this: usize, file: *const c_char) -> i32 {
        let Some(d) = dir() else {
            return call_real!(this, "GetFileSize", fn(*const c_char) -> i32, 0, file);
        };
        name_of(file).and_then(|n| file_in(&d, &n)).and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| i32::try_from(m.len()).unwrap_or(i32::MAX))
    }
}

vmethod! {
    fn v_timestamp(this: usize, file: *const c_char) -> i64 {
        let Some(d) = dir() else {
            return call_real!(this, "GetFileTimestamp", fn(*const c_char) -> i64, 0, file);
        };
        name_of(file)
            .and_then(|n| file_in(&d, &n))
            .and_then(|p| std::fs::metadata(p).ok()?.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |t| t.as_secs() as i64)
    }
}

vmethod! {
    fn v_get_sync(this: usize, file: *const c_char) -> i32 {
        if dir().is_none() {
            return call_real!(this, "GetSyncPlatforms", fn(*const c_char) -> i32, 0, file);
        }
        // k_ERemoteStoragePlatformWindows.
        1
    }
}

vmethod! {
    fn v_count(this: usize) -> i32 {
        let n = match dir() {
            Some(d) => list(&d).len() as i32,
            None => call_real!(this, "GetFileCount", fn() -> i32, 0),
        };
        crate::log::throttled(&format!("steam-remote count {n}"), 2, |_| {
            log!("steam-remote: GetFileCount -> {n}")
        });
        n
    }
}

/// Names handed out by `GetFileNameAndSize`: the game reads them after the call.
static NAMES: LazyLock<Mutex<HashMap<String, CString>>> = LazyLock::new(Mutex::default);

vmethod! {
    fn v_name_and_size(this: usize, i: i32, size: *mut i32) -> *const c_char {
        let Some(d) = dir() else {
            return call_real!(this, "GetFileNameAndSize", fn(i32, *mut i32) -> *const c_char, std::ptr::null(), i, size);
        };
        let files = list(&d);
        let Some((name, len)) = usize::try_from(i).ok().and_then(|i| files.get(i)) else {
            if !size.is_null() {
                unsafe { *size = 0 };
            }
            return c"".as_ptr();
        };
        if !size.is_null() {
            unsafe { *size = i32::try_from(*len).unwrap_or(i32::MAX) };
        }
        let mut names = crate::sys::lock(&NAMES);
        if names.len() > 20_000 {
            // Pointers handed out long ago: a game doesn't keep them.
            names.clear();
        }
        let c = names.entry(name.clone()).or_insert_with(|| CString::new(name.as_str()).unwrap_or_default());
        c.as_ptr()
    }
}

vmethod! {
    fn v_quota(this: usize, total: *mut u64, available: *mut u64) -> bool {
        let Some(d) = dir() else {
            return call_real!(this, "GetQuota", fn(*mut u64, *mut u64) -> bool, false, total, available);
        };
        let used: u64 = list(&d).iter().map(|f| f.1).sum();
        if !total.is_null() {
            unsafe { *total = QUOTA };
        }
        if !available.is_null() {
            unsafe { *available = QUOTA.saturating_sub(used) };
        }
        true
    }
}

vmethod! {
    fn v_enabled_account(this: usize) -> bool {
        if dir().is_none() {
            return call_real!(this, "IsCloudEnabledForAccount", fn() -> bool, false);
        }
        true
    }
}

vmethod! {
    fn v_enabled_app(this: usize) -> bool {
        if dir().is_none() {
            return call_real!(this, "IsCloudEnabledForApp", fn() -> bool, false);
        }
        true
    }
}

vmethod! {
    fn v_set_enabled(this: usize, on: bool) {
        if dir().is_none() {
            call_real!(this, "SetCloudEnabledForApp", fn(bool) -> (), (), on);
        }
    }
}

vmethod! {
    fn v_batch(this: usize) -> bool {
        let _ = this;
        // Each write is whole on its own here; player 1's batches are steam_api's, and
        // a batch only groups uploads, so answering true never loses a write.
        true
    }
}

vmethod! {
    fn v_change_count(this: usize) -> i32 {
        if dir().is_none() {
            return call_real!(this, "GetLocalFileChangeCount", fn() -> i32, 0);
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_players_files_are_its_own() {
        let _g = crate::sys::lock(&crate::saves::vfs::tests::TEST_LOCK);
        let base = std::env::temp_dir().join(format!("peerparty-steam-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        crate::saves::vfs::tests::install(crate::saves::vfs::tests::test_state(&base, 1, false));
        let d = dir().unwrap();
        assert!(d.ends_with(r"player-2\steam-remote"));
        assert!(write_file(&d, "saves/slot1.sav", b"one"));
        assert!(write_file(&d, "settings.cfg", b"two!"));
        assert_eq!(read_file(&d, "saves/slot1.sav").as_deref(), Some(&b"one"[..]));
        assert_eq!(list(&d), [("saves/slot1.sav".to_string(), 3), ("settings.cfg".to_string(), 4)]);
        assert!(file_in(&d, "../../x").is_none(), "never outside its folder");
        assert!(file_in(&d, "a/./b").is_none());
        assert!(file_in(&d, "/").is_none());
        // Async: the read completes as a call result with the struct's layout.
        let call = unsafe { v_read_async(0, c"saves/slot1.sav".as_ptr(), 1, 10) };
        let (_, _, data) = crate::steam::injected().into_iter().find(|p| p.0 == READ_ASYNC_DONE).unwrap();
        assert_eq!(data.len(), 24);
        assert_eq!(u64::from_le_bytes(data[0..8].try_into().unwrap()), call);
        assert_eq!(i32::from_le_bytes(data[8..12].try_into().unwrap()), RESULT_OK);
        assert_eq!(u32::from_le_bytes(data[16..20].try_into().unwrap()), 2, "bytes read from offset 1");
        let mut buf = [0u8; 8];
        assert!(unsafe { v_read_async_complete(0, call, buf.as_mut_ptr(), 8) } != 0);
        assert_eq!(&buf[..2], b"ne");
        // Streams.
        let h = unsafe { v_stream_open(0, c"big.bin".as_ptr()) };
        assert!(unsafe { v_stream_chunk(0, h, b"ab".as_ptr(), 2) } != 0);
        assert!(unsafe { v_stream_chunk(0, h, b"cd".as_ptr(), 2) } != 0);
        assert!(unsafe { v_stream_close(0, h) } != 0);
        assert_eq!(read_file(&d, "big.bin").as_deref(), Some(&b"abcd"[..]));
        assert_eq!(unsafe { v_count(0) }, 3);
        let mut size = 0;
        let name = unsafe { v_name_and_size(0, 0, &mut size) };
        assert_eq!((crate::sys::read::cstr(name as usize).as_deref(), size), (Some("big.bin"), 4));
        assert!(unsafe { v_delete(0, c"big.bin".as_ptr()) } != 0);
        assert!(unsafe { v_exists(0, c"big.bin".as_ptr()) } == 0);
        crate::saves::vfs::tests::uninstall();
        let _ = std::fs::remove_dir_all(&base);
    }
}
