//! What core asks of the operating system, one module per OS family with the same
//! functions in each:
//!
//! - where PeerParty keeps its files (`data_dir`) and the player's account name
//! - an exclusive lock on a file that lasts as long as its `File` (identity slots), and
//!   the same-PC peer board's files, which say while their copy runs
//! - whether a process still runs and what program it is (`process_exe`)
//! - this PC's IPv4 networks, its default gateway, and the sockets a process has open
//! - TCP keepalive timings and a UDP port every copy on this PC can share
//!
//! `procfs` reads the text of Linux's `/proc` files; it is built everywhere for its
//! tests.

use std::path::PathBuf;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(any(unix, test))]
mod procfs;

/// PeerParty's folder: `PEERPARTY_DATA_DIR` when it names one (tests: a folder per run),
/// else the OS's place for it (`os_data_dir`).
pub(crate) fn data_dir() -> Option<PathBuf> {
    data_dir_override().or_else(os_data_dir)
}

/// `PEERPARTY_DATA_DIR`, when it's set to an absolute path.
pub(crate) fn data_dir_override() -> Option<PathBuf> {
    std::env::var_os("PEERPARTY_DATA_DIR").map(PathBuf::from).filter(|p| p.is_absolute())
}

/// PeerParty's folder (`data_dir`), else one in the temp folder.
pub(crate) fn data_dir_or_temp() -> PathBuf {
    data_dir().unwrap_or_else(|| std::env::temp_dir().join("peerparty"))
}
