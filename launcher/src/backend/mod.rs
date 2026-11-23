//! Starting a game with PeerParty and hosting it, per operating system. `main.rs`, the
//! window (`ui`) and the overlay's host (`host`) call the functions re-exported here,
//! which every system has under the same names:
//!
//! - `windows/`: the injected start (early-bird APC, the WOW64 path, packaged games),
//!   the job that follows the game's programs, `.bat` files, the invite and close events,
//!   the console, WinHTTP and the registry's game libraries.
//! - `linux/`: SteamOS and other Linux systems, where the games are Windows programs
//!   under Proton (or Wine): `peerparty %command%` wraps Steam's command, the hook reaches
//!   this process over loopback TCP and draws the overlay from a shared file
//!   (`$XDG_RUNTIME_DIR/peerparty`, `Z:\...` to the game), controllers come from evdev,
//!   and the libraries are Steam's, Heroic's and Lutris's.
//!
//! `platform` is the window's side of the same split (folders, fonts, icons, the single
//! window); it uses these backends where they overlap.

// Linux's parsers (libraries, input events, paths) are built for every system's tests.
#[cfg(any(unix, test))]
pub mod linux;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub use linux::{
    console::{attach_console, only_system_dlls},
    control::signal_invite,
    http,
    launch::{attach, resolve, start, start_session},
};
#[cfg(windows)]
pub use windows::{
    console::{attach_console, only_system_dlls},
    http,
    invite::signal_invite,
    launch::{attach, resolve, start, start_session},
};
