//! Windows' side of starting and hosting a game: the injected start (launch.rs, with
//! wow64.rs for 32-bit games and packaged.rs for Microsoft Store ones), the programs the
//! game starts (tree.rs), .bat files that start a game (wrapper.rs), the invite events
//! (invite.rs), the console PeerParty was started from (console.rs), WinHTTP for the
//! update check (http.rs) and the game libraries in the registry (scan.rs).
//!
//! The Win32 helpers more than one of those needs are here, once: UTF-16 strings, a
//! process's program, an owner-only security descriptor for pipes, and COM's GUIDs and
//! method calls.

pub mod console;
pub mod http;
pub mod invite;
pub mod launch;
pub mod packaged;
pub mod scan;
pub mod tree;
pub mod wow64;
pub mod wrapper;

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// A string as Windows takes it: UTF-16, ending in a zero.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// `wide`'s units as bytes, for writing into another process.
pub fn wide_bytes(s: &OsStr) -> Vec<u8> {
    wide(s).iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// "`what` failed: " and Windows' last error.
pub fn os_error(what: &str) -> String {
    format!("{what} failed: {}", std::io::Error::last_os_error())
}

/// The exe a running process was started from.
pub fn process_exe(pid: u32) -> Option<PathBuf> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) } != 0;
    unsafe { CloseHandle(h) };
    ok.then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

/// A security descriptor (freed on drop).
pub struct Descriptor(*mut core::ffi::c_void);

impl Descriptor {
    /// Security attributes that hand this descriptor to a new object (not inherited).
    /// They point into `self`, so they live no longer than it.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: self.0, bInheritHandle: 0 }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

/// A DACL letting only this process's user (and the system) at a pipe: the default one
/// lets every user on the PC read it.
pub fn owner_only() -> Option<Descriptor> {
    owner_and(&[])
}

/// `owner_only`, and the accounts or groups `others` names (SDDL's: `BA` is the
/// Administrators group).
pub fn owner_and(others: &[&str]) -> Option<Descriptor> {
    let sid = user_sid()?;
    let more: String = others.iter().map(|o| format!("(A;;GA;;;{o})")).collect();
    let sddl = wide(format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY){more}"));
    let mut sd = std::ptr::null_mut();
    let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) };
    (ok != 0 && !sd.is_null()).then_some(Descriptor(sd))
}

/// This process's user, as an SID string ("S-1-5-21-...").
pub fn user_sid() -> Option<String> {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut buf = vec![0u64; 64];
        let mut len = 0u32;
        let got = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len);
        CloseHandle(token);
        if got == 0 {
            return None;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return None;
        }
        let n = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
        LocalFree(text.cast());
        Some(sid)
    }
}

/// A COM class or interface id, laid out as Windows' `GUID`.
#[repr(C)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

/// Calls method `slot` of a COM object (its address as a usize).
macro_rules! vcall {
    ($obj:expr, $slot:expr, fn($($t:ty),*) -> $r:ty $(, $arg:expr)*) => {{
        let vt = unsafe { *($obj as *const *const usize) };
        let f: unsafe extern "system" fn(usize $(, $t)*) -> $r = unsafe { std::mem::transmute(*vt.add($slot)) };
        unsafe { f($obj $(, $arg)*) }
    }};
}
pub(crate) use vcall;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_and_the_owner_descriptor() {
        assert_eq!(wide("ab"), [0x61, 0x62, 0]);
        assert_eq!(wide_bytes(OsStr::new("a")), [0x61, 0, 0, 0]);
        assert!(user_sid().is_some_and(|s| s.starts_with("S-1-")), "{:?}", user_sid());
        let sd = owner_only().expect("a descriptor");
        assert!(!sd.attributes().lpSecurityDescriptor.is_null());
        let me = process_exe(std::process::id()).expect("our own program");
        assert_eq!(me.file_name(), std::env::current_exe().unwrap().file_name());
    }
}
