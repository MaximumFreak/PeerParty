//! EOS's opaque ids and handles, minted by PeerParty.
//!
//! An `EOS_ProductUserId` or `EOS_EpicAccountId` is a pointer to one of these, one per
//! LAN peer per kind, never freed (EOS ids are valid for the process's lifetime). Both
//! kinds carry the peer's `PeerId`, whose hex is also the string form. The real SDK
//! never sees them: every function that takes one is PeerParty's or fails fast.

use std::collections::HashMap;
use std::ffi::{CString, c_char};
use std::sync::{LazyLock, Mutex};

use peerparty_core::PeerId;

const MAGIC: u32 = 0x5050_4944; // "PPID"

#[repr(C)]
struct IdObj {
    magic: u32,
    kind: u32,
    peer: PeerId,
    text: CString,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Product = 1,
    Epic = 2,
}

#[derive(Default)]
struct Registry {
    by_peer: HashMap<(Kind, PeerId), usize>,
    by_ptr: HashMap<usize, (Kind, PeerId)>,
    strings: HashMap<String, usize>,
}

static REG: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

/// The handle for a peer's id of that kind.
pub fn handle(kind: Kind, peer: PeerId) -> usize {
    let mut r = REG.lock().unwrap();
    if let Some(p) = r.by_peer.get(&(kind, peer)) {
        return *p;
    }
    let obj = Box::leak(Box::new(IdObj {
        magic: MAGIC,
        kind: kind as u32,
        peer,
        text: CString::new(peer.hex()).unwrap(),
    }));
    let p = obj as *const IdObj as usize;
    r.by_peer.insert((kind, peer), p);
    r.by_ptr.insert(p, (kind, peer));
    p
}

pub fn puid(peer: PeerId) -> usize {
    handle(Kind::Product, peer)
}

pub fn epic(peer: PeerId) -> usize {
    handle(Kind::Epic, peer)
}

/// The peer behind a handle we minted, if it is one and of that kind.
pub fn peer(kind: Kind, handle: usize) -> Option<PeerId> {
    let r = REG.lock().unwrap();
    r.by_ptr
        .get(&handle)
        .filter(|(k, _)| *k == kind)
        .map(|(_, p)| *p)
}

pub fn peer_of_puid(handle: usize) -> Option<PeerId> {
    peer(Kind::Product, handle)
}

pub fn peer_of_epic(handle: usize) -> Option<PeerId> {
    peer(Kind::Epic, handle)
}

/// A NUL-terminated copy that lives for the rest of the process. For strings handed to
/// callbacks (lobby ids), which a game may keep.
pub fn intern(s: &str) -> *const c_char {
    let mut r = REG.lock().unwrap();
    if let Some(p) = r.strings.get(s) {
        return *p as *const c_char;
    }
    let c = CString::new(s.replace('\0', "")).unwrap();
    let p = c.into_raw() as usize;
    r.strings.insert(s.to_string(), p);
    p as *const c_char
}

/// A C string argument, if present and valid UTF-8 (lossy beyond that).
pub unsafe fn text(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { std::ffi::CStr::from_ptr(p) };
    Some(s.to_string_lossy().into_owned())
}

/// `EOS_*_ToString`: writes the id's hex into the caller's buffer.
pub unsafe fn to_string(kind: Kind, id: usize, buf: *mut c_char, len: *mut i32) -> i32 {
    use super::abi::result::*;
    let Some(peer) = peer(kind, id) else {
        return INVALID_PARAMETERS;
    };
    if len.is_null() {
        return INVALID_PARAMETERS;
    }
    let text = peer.hex();
    let need = text.len() as i32 + 1;
    unsafe {
        if buf.is_null() || *len < need {
            *len = need;
            return LIMIT_EXCEEDED;
        }
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, text.len());
        *buf.add(text.len()) = 0;
        *len = need;
    }
    SUCCESS
}

/// `EOS_*_FromString`.
pub unsafe fn from_string(kind: Kind, s: *const c_char) -> usize {
    unsafe { text(s) }
        .and_then(|s| PeerId::from_hex(&s))
        .map_or(0, |p| handle(kind, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_stable_and_typed() {
        let p = PeerId([9; 16]);
        let a = puid(p);
        assert_eq!(a, puid(p));
        assert_ne!(a, epic(p));
        assert_eq!(peer_of_puid(a), Some(p));
        assert_eq!(peer_of_epic(a), None);
        assert_eq!(peer_of_puid(12345), None);

        let mut buf = [0 as c_char; 40];
        let mut len = 10;
        assert_eq!(
            unsafe { to_string(Kind::Product, a, buf.as_mut_ptr(), &mut len) },
            super::super::abi::result::LIMIT_EXCEEDED
        );
        assert_eq!(len, 33);
        assert_eq!(
            unsafe { to_string(Kind::Product, a, buf.as_mut_ptr(), &mut len) },
            0
        );
        assert_eq!(unsafe { from_string(Kind::Product, buf.as_ptr()) }, a);
        assert_eq!(intern("lobby"), intern("lobby"));
    }
}
