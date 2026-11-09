//! Title Storage and Player Data Storage, answered from disk: every call completes, the
//! way Epic's would (a game can wait on a TitleStorage read before it goes online,
//! and a call that never completes left it on "Connecting" for good).
//!
//! - Player Data Storage (the player's own cloud files) lives in
//!   `%LOCALAPPDATA%\peerparty\eos-storage\<product>\player\<LAN id>\`: writes, reads,
//!   queries, copies of metadata, duplicates and deletes all work there.
//! - Title Storage (files the developer uploads to Epic) is read from
//!   `...\<product>\title\`, empty unless someone puts files there; a missing file is
//!   EOS_NotFound, which games handle as "no such file", not as a hang.
//!
//! Layouts are the EOS SDK's (eos_titlestorage_types.h, eos_playerdatastorage_types.h);
//! read and write callbacks get chunks of the size the game asked for.

use std::ffi::c_char;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use super::abi::result::*;
use super::{call, hand_out, ids, lan, later, owned_str, release, trace};

const CANCELED: i32 = 17;
const REQUEST_IN_PROGRESS: i32 = 39;
const FAILED: i32 = 38; // EOS_InvalidState

/// EOS_TitleStorage_EReadResult / EOS_PlayerDataStorage_EReadResult.
const READ_CONTINUE: i32 = 1;
const READ_CANCEL: i32 = 3;
/// EOS_PlayerDataStorage_EWriteResult.
const WRITE_CONTINUE: i32 = 1;
const WRITE_COMPLETE: i32 = 2;
const WRITE_CANCEL: i32 = 4;

pub fn resolve(name: &str) -> Option<usize> {
    let f: usize = match name {
        "EOS_TitleStorage_QueryFile" | "EOS_PlayerDataStorage_QueryFile" => query_file as *const () as usize,
        "EOS_TitleStorage_QueryFileList" => title_query_list as *const () as usize,
        "EOS_PlayerDataStorage_QueryFileList" => player_query_list as *const () as usize,
        "EOS_TitleStorage_GetFileMetadataCount" => title_count as *const () as usize,
        "EOS_PlayerDataStorage_GetFileMetadataCount" => player_count as *const () as usize,
        "EOS_TitleStorage_CopyFileMetadataAtIndex" => title_copy_at as *const () as usize,
        "EOS_PlayerDataStorage_CopyFileMetadataAtIndex" => player_copy_at as *const () as usize,
        "EOS_TitleStorage_CopyFileMetadataByFilename" => title_copy_by_name as *const () as usize,
        "EOS_PlayerDataStorage_CopyFileMetadataByFilename" => player_copy_by_name as *const () as usize,
        "EOS_TitleStorage_FileMetadata_Release" | "EOS_PlayerDataStorage_FileMetadata_Release" => free as *const () as usize,
        "EOS_TitleStorage_ReadFile" => title_read as *const () as usize,
        "EOS_PlayerDataStorage_ReadFile" => player_read as *const () as usize,
        "EOS_PlayerDataStorage_WriteFile" => player_write as *const () as usize,
        "EOS_PlayerDataStorage_DeleteFile" => player_delete as *const () as usize,
        "EOS_PlayerDataStorage_DuplicateFile" => player_duplicate as *const () as usize,
        "EOS_TitleStorage_DeleteCache" | "EOS_PlayerDataStorage_DeleteCache" => delete_cache as *const () as usize,
        "EOS_TitleStorageFileTransferRequest_GetFileRequestState"
        | "EOS_PlayerDataStorageFileTransferRequest_GetFileRequestState" => request_state as *const () as usize,
        "EOS_TitleStorageFileTransferRequest_GetFilename" | "EOS_PlayerDataStorageFileTransferRequest_GetFilename" => {
            request_filename as *const () as usize
        }
        "EOS_TitleStorageFileTransferRequest_CancelRequest" | "EOS_PlayerDataStorageFileTransferRequest_CancelRequest" => {
            request_cancel as *const () as usize
        }
        "EOS_TitleStorageFileTransferRequest_Release" | "EOS_PlayerDataStorageFileTransferRequest_Release" => {
            request_release as *const () as usize
        }
        _ => return None,
    };
    Some(f)
}

#[derive(Clone, Copy, PartialEq)]
enum Store {
    Title,
    Player,
}

fn dir(store: Store) -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let product = super::product_id();
    let product = if product.is_empty() { "unknown".to_string() } else { product };
    let root = base.join("peerparty").join("eos-storage").join(product);
    Some(match store {
        Store::Title => root.join("title"),
        Store::Player => root.join("player").join(lan()?.me().hex()),
    })
}

/// A file name the game gave, kept to one path component.
fn safe_name(name: &str) -> Option<String> {
    let n = name.replace(['/', '\\'], "_");
    (!n.is_empty() && n != "." && n != ".." && !n.contains(':')).then_some(n)
}

fn path(store: Store, name: &str) -> Option<PathBuf> {
    Some(dir(store)?.join(safe_name(name)?))
}

/// The store's files, sorted by name.
fn files(store: Store) -> Vec<String> {
    let Some(d) = dir(store) else { return Vec::new() };
    let mut v: Vec<String> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    v.sort();
    v
}

fn me_id() -> usize {
    lan().map_or(0, |l| ids::puid(l.me()))
}

unsafe fn text(p: *const c_char) -> String {
    unsafe { ids::text(p) }.unwrap_or_default()
}

// ---- options and callback infos ----

#[repr(C)]
struct NameOptions {
    api: i32,
    local: usize,
    filename: *const c_char,
}

#[repr(C)]
struct IndexOptions {
    api: i32,
    local: usize,
    index: u32,
}

#[repr(C)]
struct DuplicateOptions {
    api: i32,
    local: usize,
    source: *const c_char,
    destination: *const c_char,
}

#[repr(C)]
struct TransferOptions {
    api: i32,
    local: usize,
    filename: *const c_char,
    chunk: u32,
    data_callback: usize,
    progress_callback: usize,
}

#[repr(C)]
struct Done {
    result: i32,
    client_data: usize,
    local: usize,
}

#[repr(C)]
struct ListDone {
    result: i32,
    client_data: usize,
    local: usize,
    count: u32,
}

#[repr(C)]
struct FileDone {
    result: i32,
    client_data: usize,
    local: usize,
    filename: *const c_char,
}

#[repr(C)]
struct ReadData {
    client_data: usize,
    local: usize,
    filename: *const c_char,
    total: u32,
    last: i32,
    length: u32,
    data: *const u8,
}

#[repr(C)]
struct WriteData {
    client_data: usize,
    local: usize,
    filename: *const c_char,
    buffer_length: u32,
}

#[repr(C)]
struct Progress {
    client_data: usize,
    local: usize,
    filename: *const c_char,
    transferred: u32,
    total: u32,
}

#[repr(C)]
struct TitleMetadata {
    api: i32,
    size: u32,
    md5: *const c_char,
    filename: *const c_char,
    unencrypted: u32,
}

#[repr(C)]
struct PlayerMetadata {
    api: i32,
    size: u32,
    md5: *const c_char,
    filename: *const c_char,
    last_modified: i64,
    unencrypted: u32,
}

/// A file transfer handle: its name, and whether it has finished or was canceled.
struct Request {
    filename: String,
    done: AtomicBool,
    canceled: AtomicBool,
}

fn complete(cb: usize, info: Done) {
    later(move || unsafe { call(cb, &info) });
}

// ---- queries and metadata ----

unsafe extern "system" fn query_file(_h: usize, o: *const NameOptions, cd: usize, cb: usize) {
    // Both stores: the one the game means is told by which function it called, but a
    // file present in either answers the same (Title Storage is normally empty here).
    let name = if o.is_null() { String::new() } else { unsafe { text((*o).filename) } };
    let found = [Store::Player, Store::Title].iter().any(|s| path(*s, &name).is_some_and(|p| p.is_file()));
    trace("EOS_*Storage_QueryFile", || format!("{name} found={found}"));
    complete(cb, Done { result: if found { SUCCESS } else { NOT_FOUND }, client_data: cd, local: me_id() });
}

fn query_list(store: Store, cd: usize, cb: usize) {
    let count = files(store).len() as u32;
    later(move || {
        let info = ListDone { result: SUCCESS, client_data: cd, local: me_id(), count };
        unsafe { call(cb, &info) };
    });
}

unsafe extern "system" fn title_query_list(_h: usize, _o: usize, cd: usize, cb: usize) {
    query_list(Store::Title, cd, cb);
}

unsafe extern "system" fn player_query_list(_h: usize, _o: usize, cd: usize, cb: usize) {
    query_list(Store::Player, cd, cb);
}

/// TitleStorage returns the count itself.
unsafe extern "system" fn title_count(_h: usize, _o: usize) -> u32 {
    files(Store::Title).len() as u32
}

/// PlayerDataStorage returns a result and writes the count.
unsafe extern "system" fn player_count(_h: usize, _o: usize, out: *mut i32) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = files(Store::Player).len() as i32 };
    SUCCESS
}

fn metadata(store: Store, name: &str) -> Option<usize> {
    let p = path(store, name)?;
    let data = std::fs::read(&p).ok()?;
    let mut keep = Vec::new();
    let md5 = owned_str(&mut keep, &md5_hex(&data));
    let filename = owned_str(&mut keep, name);
    let size = data.len() as u32;
    Some(match store {
        Store::Title => hand_out(TitleMetadata { api: 2, size, md5, filename, unencrypted: size }, keep),
        Store::Player => {
            let modified = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(-1, |d| d.as_secs() as i64);
            hand_out(PlayerMetadata { api: 3, size, md5, filename, last_modified: modified, unencrypted: size }, keep)
        }
    })
}

fn copy_at(store: Store, o: *const IndexOptions, out: *mut usize) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    let Some(name) = files(store).into_iter().nth(unsafe { (*o).index } as usize) else { return NOT_FOUND };
    match metadata(store, &name) {
        Some(m) => {
            unsafe { *out = m };
            SUCCESS
        }
        None => NOT_FOUND,
    }
}

fn copy_by_name(store: Store, o: *const NameOptions, out: *mut usize) -> i32 {
    if out.is_null() || o.is_null() {
        return INVALID_PARAMETERS;
    }
    unsafe { *out = 0 };
    match metadata(store, &unsafe { text((*o).filename) }) {
        Some(m) => {
            unsafe { *out = m };
            SUCCESS
        }
        None => NOT_FOUND,
    }
}

unsafe extern "system" fn title_copy_at(_h: usize, o: *const IndexOptions, out: *mut usize) -> i32 {
    copy_at(Store::Title, o, out)
}

unsafe extern "system" fn player_copy_at(_h: usize, o: *const IndexOptions, out: *mut usize) -> i32 {
    copy_at(Store::Player, o, out)
}

unsafe extern "system" fn title_copy_by_name(_h: usize, o: *const NameOptions, out: *mut usize) -> i32 {
    copy_by_name(Store::Title, o, out)
}

unsafe extern "system" fn player_copy_by_name(_h: usize, o: *const NameOptions, out: *mut usize) -> i32 {
    copy_by_name(Store::Player, o, out)
}

unsafe extern "system" fn free(p: usize) {
    release(p);
}

unsafe extern "system" fn delete_cache(_h: usize, _o: usize, cd: usize, cb: usize) -> i32 {
    complete(cb, Done { result: SUCCESS, client_data: cd, local: me_id() });
    SUCCESS
}

// ---- transfers ----

fn new_request(filename: &str) -> usize {
    hand_out(Request { filename: filename.to_string(), done: AtomicBool::new(false), canceled: AtomicBool::new(false) }, Vec::new())
}

fn request<'a>(h: usize) -> Option<&'a Request> {
    (h != 0).then(|| unsafe { &*(h as *const Request) })
}

fn read(store: Store, o: *const TransferOptions, cd: usize, cb: usize) -> usize {
    if o.is_null() {
        return 0;
    }
    let o = unsafe { &*o };
    let name = unsafe { text(o.filename) };
    let (chunk, data_cb, progress_cb) = (if o.chunk == 0 { 4096 } else { o.chunk } as usize, o.data_callback, o.progress_callback);
    let req = new_request(&name);
    let data = path(store, &name).and_then(|p| std::fs::read(p).ok());
    trace(if store == Store::Title { "EOS_TitleStorage_ReadFile" } else { "EOS_PlayerDataStorage_ReadFile" }, || {
        format!("{name}: {}", data.as_ref().map_or("not found".to_string(), |d| format!("{} bytes", d.len())))
    });
    later(move || {
        let c_name = std::ffi::CString::new(name.replace('\0', "")).unwrap_or_default();
        let local = me_id();
        let canceled = || request(req).is_some_and(|r| r.canceled.load(Ordering::Relaxed));
        let result = match &data {
            None => NOT_FOUND,
            Some(d) => {
                let total = d.len() as u32;
                let mut at = 0usize;
                let mut result = SUCCESS;
                loop {
                    if canceled() {
                        result = CANCELED;
                        break;
                    }
                    let end = (at + chunk).min(d.len());
                    let info = ReadData {
                        client_data: cd,
                        local,
                        filename: c_name.as_ptr(),
                        total,
                        last: (end == d.len()) as i32,
                        length: (end - at) as u32,
                        data: d[at..].as_ptr(),
                    };
                    let answer = if data_cb == 0 {
                        READ_CONTINUE
                    } else {
                        let f: unsafe extern "system" fn(*const ReadData) -> i32 = unsafe { std::mem::transmute(data_cb) };
                        unsafe { f(&info) }
                    };
                    if progress_cb != 0 {
                        let p = Progress { client_data: cd, local, filename: c_name.as_ptr(), transferred: end as u32, total };
                        unsafe { call(progress_cb, &p) };
                    }
                    if answer != READ_CONTINUE {
                        result = if answer == READ_CANCEL { CANCELED } else { FAILED };
                        break;
                    }
                    at = end;
                    if at >= d.len() {
                        break;
                    }
                }
                result
            }
        };
        if let Some(r) = request(req) {
            r.done.store(true, Ordering::Relaxed);
        }
        let info = FileDone { result, client_data: cd, local, filename: c_name.as_ptr() };
        unsafe { call(cb, &info) };
    });
    req
}

unsafe extern "system" fn title_read(_h: usize, o: *const TransferOptions, cd: usize, cb: usize) -> usize {
    read(Store::Title, o, cd, cb)
}

unsafe extern "system" fn player_read(_h: usize, o: *const TransferOptions, cd: usize, cb: usize) -> usize {
    read(Store::Player, o, cd, cb)
}

unsafe extern "system" fn player_write(_h: usize, o: *const TransferOptions, cd: usize, cb: usize) -> usize {
    if o.is_null() {
        return 0;
    }
    let o = unsafe { &*o };
    let name = unsafe { text(o.filename) };
    let (chunk, data_cb, progress_cb) = (if o.chunk == 0 { 4096 } else { o.chunk } as usize, o.data_callback, o.progress_callback);
    let req = new_request(&name);
    trace("EOS_PlayerDataStorage_WriteFile", || name.clone());
    later(move || {
        let c_name = std::ffi::CString::new(name.replace('\0', "")).unwrap_or_default();
        let local = me_id();
        let mut contents = Vec::new();
        let mut buf = vec![0u8; chunk];
        let mut result = SUCCESS;
        loop {
            if request(req).is_some_and(|r| r.canceled.load(Ordering::Relaxed)) {
                result = CANCELED;
                break;
            }
            let info = WriteData { client_data: cd, local, filename: c_name.as_ptr(), buffer_length: chunk as u32 };
            let mut written = 0u32;
            let answer = if data_cb == 0 {
                WRITE_COMPLETE
            } else {
                let f: unsafe extern "system" fn(*const WriteData, *mut u8, *mut u32) -> i32 = unsafe { std::mem::transmute(data_cb) };
                unsafe { f(&info, buf.as_mut_ptr(), &mut written) }
            };
            contents.extend_from_slice(&buf[..(written as usize).min(chunk)]);
            if progress_cb != 0 {
                let p = Progress { client_data: cd, local, filename: c_name.as_ptr(), transferred: contents.len() as u32, total: contents.len() as u32 };
                unsafe { call(progress_cb, &p) };
            }
            match answer {
                WRITE_CONTINUE => continue,
                WRITE_COMPLETE => break,
                WRITE_CANCEL => {
                    result = CANCELED;
                    break;
                }
                _ => {
                    result = FAILED;
                    break;
                }
            }
        }
        if result == SUCCESS {
            let saved = path(Store::Player, &name).and_then(|p| {
                std::fs::create_dir_all(p.parent()?).ok()?;
                std::fs::write(&p, &contents).ok()
            });
            if saved.is_none() {
                result = FAILED;
            }
        }
        if let Some(r) = request(req) {
            r.done.store(true, Ordering::Relaxed);
        }
        let info = FileDone { result, client_data: cd, local, filename: c_name.as_ptr() };
        unsafe { call(cb, &info) };
    });
    req
}

unsafe extern "system" fn player_delete(_h: usize, o: *const NameOptions, cd: usize, cb: usize) {
    let name = if o.is_null() { String::new() } else { unsafe { text((*o).filename) } };
    let ok = path(Store::Player, &name).is_some_and(|p| std::fs::remove_file(p).is_ok());
    complete(cb, Done { result: if ok { SUCCESS } else { NOT_FOUND }, client_data: cd, local: me_id() });
}

unsafe extern "system" fn player_duplicate(_h: usize, o: *const DuplicateOptions, cd: usize, cb: usize) {
    let ok = !o.is_null() && {
        let (src, dst) = unsafe { (text((*o).source), text((*o).destination)) };
        matches!((path(Store::Player, &src), path(Store::Player, &dst)), (Some(s), Some(d)) if std::fs::copy(&s, &d).is_ok())
    };
    complete(cb, Done { result: if ok { SUCCESS } else { NOT_FOUND }, client_data: cd, local: me_id() });
}

unsafe extern "system" fn request_state(h: usize) -> i32 {
    match request(h) {
        Some(r) if r.canceled.load(Ordering::Relaxed) => CANCELED,
        Some(r) if r.done.load(Ordering::Relaxed) => SUCCESS,
        Some(_) => REQUEST_IN_PROGRESS,
        None => INVALID_PARAMETERS,
    }
}

unsafe extern "system" fn request_filename(h: usize, size: u32, out: *mut c_char, out_len: *mut i32) -> i32 {
    let Some(r) = request(h) else { return INVALID_PARAMETERS };
    let bytes = r.filename.as_bytes();
    if !out_len.is_null() {
        unsafe { *out_len = bytes.len() as i32 + 1 };
    }
    if out.is_null() || (size as usize) < bytes.len() + 1 {
        return LIMIT_EXCEEDED;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
        *out.add(bytes.len()) = 0;
    }
    SUCCESS
}

unsafe extern "system" fn request_cancel(h: usize) -> i32 {
    match request(h) {
        Some(r) => {
            r.canceled.store(true, Ordering::Relaxed);
            SUCCESS
        }
        None => INVALID_PARAMETERS,
    }
}

unsafe extern "system" fn request_release(h: usize) {
    release(h);
}

/// MD5, hex (the metadata's MD5Hash).
pub(crate) fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11,
        16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    for block in msg.chunks(64) {
        let m: Vec<u32> = block.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0].iter().flat_map(|v| v.to_le_bytes()).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn md5_matches_rfc_1321() {
        assert_eq!(super::md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(super::md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(super::md5_hex(&[b'a'; 1000]), "cabe45dcc9ae5b66ba86600cca6b8ba8");
    }
}
