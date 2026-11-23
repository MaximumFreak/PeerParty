//! PeerParty launcher. One x64 exe for every game: both hook DLLs are embedded (see
//! build.rs and hooks.rs). Started with no arguments (double-clicked) it opens the
//! launcher window (src/ui); with a game on its command line it starts that game
//! (`cli.rs` + `launch.rs`).
//!
//! It's a GUI-subsystem program, so double-clicking it opens no console. From a
//! console it attaches to that console for its messages; a .bat file and Steam still
//! wait for it and get the game's exit code.
//!
//! Starting the game with the hook and waiting for it is the system's part (`backend`):
//! on Windows the game starts suspended with the hook queued to load before its entry
//! point (backend/windows/launch.rs); on Linux `peerparty` wraps the command Steam runs
//! a Proton game with (backend/linux/launch.rs). Either way PeerParty waits for the game
//! and returns its exit code, so Steam sees the game running for as long as it runs.
//!
//! As a Steam launch option: `"…\peerparty.exe" %command%`.

#![windows_subsystem = "windows"]

// The Windows exe injects 32-bit games from its own 64-bit process (wow64.rs).
#[cfg(all(windows, not(target_arch = "x86_64")))]
compile_error!("peerparty.exe is x64 only; it injects 32-bit games itself");
#[cfg(not(any(windows, unix)))]
compile_error!("peerparty builds for Windows and for Linux (and other Unix systems)");

mod anticheat;
mod backend;
mod cli;
mod firewall;
mod gameroot;
mod hooks;
mod host;
mod i18n;
mod icon_art;
mod launch;
mod platform;
mod portable;
mod release;
mod steamid;
mod ui;
mod update;

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

pub use backend::signal_invite;
pub use launch::crt_quote;

fn main() -> ExitCode {
    backend::only_system_dlls();
    // Before any thread: a `peerparty-data` folder beside the exe is PeerParty's folder.
    portable::init();
    // No arguments: the window. `--updated`: the window, started by the update that
    // put this exe in place (update.rs); `--moved`: by the window that moved
    // PeerParty's files (portable.rs).
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let restarted = |flag: &str| args.len() == 1 && args[0] == flag;
    if args.is_empty() || restarted("--updated") || restarted("--moved") {
        update::clean_up();
        hooks::clean_up();
        return match ui::run(!args.is_empty(), restarted("--moved")) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        };
    }
    backend::attach_console();
    cli::execute(args)
}
