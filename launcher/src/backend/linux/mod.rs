//! Linux's side (SteamOS and other Linux systems): the games are Windows programs under
//! Proton or Wine, which run PeerParty's Windows hook DLL as they would on Windows. This
//! side is the native program around them:
//!
//! - launch.rs: `peerparty %command%` in a game's Steam launch options. PeerParty runs
//!   Steam's command for the game with its settings in the environment, the overlay's
//!   connection on loopback TCP (`PEERPARTY_IPC`), and waits for it.
//! - control.rs: a socket per running game, for `peerparty --invite` and the window's
//!   Close all.
//! - overlay.rs: the overlay's frame, in a file under `$XDG_RUNTIME_DIR/peerparty` that
//!   this process `mmap`s and the hook maps through Wine (works inside Gamescope).
//! - pads.rs: controllers from evdev (`/dev/input/event*`), into `host::pad`'s buttons.
//! - scan.rs: the game libraries: Steam (its libraries and SD cards), Heroic, Lutris.
//! - http.rs: the update check, through curl.
//!
//! The parsers here (VDF, manifests, input events, paths) are plain functions with tests
//! that run on any system; what opens devices, sockets or memory is Unix-only.

// The parsers are built on Windows too, for their tests; the rest of this module is
// Linux's alone and unused there.
#![cfg_attr(not(unix), allow(dead_code))]

#[cfg(unix)]
pub mod console;
#[cfg(unix)]
pub mod control;
#[cfg(unix)]
pub mod http;
pub mod launch;
#[cfg(unix)]
pub mod overlay;
pub mod pads;
pub mod scan;

use std::path::{Path, PathBuf};

/// The user's home folder (`$HOME`).
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute())
}

/// An XDG base folder: `$<var>` when it's an absolute path, else `$HOME/<fallback>`.
pub fn xdg_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| home().map(|h| h.join(fallback)))
}

/// Where this run's private files go (the overlay's frames, the control sockets):
/// `$XDG_RUNTIME_DIR/peerparty`, only this user's (mode 0700), made if need be. Without
/// a runtime folder, a folder of this user's own in the temp folder.
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute());
    let dir = match base {
        Some(b) => b.join("peerparty"),
        None => std::env::temp_dir().join(format!("peerparty-{}", user_id())),
    };
    make_private_dir(&dir);
    dir
}

/// The user's numeric id (0 off Unix, where nothing asks).
pub fn user_id() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Makes `dir` (mode 0700 on Unix), and takes back a folder of that name another user
/// left open to everyone: only this user's processes may reach what goes in it.
fn make_private_dir(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

/// The program process `pid` runs (`/proc/<pid>/exe`).
pub fn process_exe(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Every running process's id (`/proc`).
pub fn processes() -> Vec<u32> {
    std::fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_folders_fall_back_to_home() {
        // An XDG variable that isn't set (a name no system uses) falls back under $HOME.
        let got = xdg_dir("PEERPARTY_TEST_UNSET_XDG", ".local/share");
        if let Some(h) = home() {
            assert_eq!(got, Some(h.join(".local/share")));
        }
    }
}
