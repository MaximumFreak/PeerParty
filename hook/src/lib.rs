//! Injected into the game before its entry point: watches SDK modules load and gives
//! each one's multiplayer calls PeerParty's backend (watch.rs). A loaded EasyAntiCheat or
//! BattlEye module stops the process, as does an EOS Anti-Cheat session (eos/mod.rs).

mod child;
mod dns;
mod eos;
mod exitlog;
mod galaxy;
mod galaxyvt;
mod hook;
mod instance;
mod invite;
mod ipc;
mod ipfwd;
mod json;
mod log;
mod modes;
mod net;
mod overlay;
mod padblock;
mod party;
mod pe;
mod pfmp;
mod photon;
mod playfab;
mod present;
mod read;
mod saves;
mod sdk;
mod settings;
mod steam;
mod steamgns;
mod steamgs;
mod steamids;
mod steamlan;
mod steammm;
mod steamobj;
mod steamown;
mod steamp2p;
mod steamvt;
mod swap;
mod typing;
mod ugs;
#[cfg(test)]
mod untrusted_tests;
mod voice;
mod watch;
mod xboxlive;
mod xgameui;

use std::panic::AssertUnwindSafe;

use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    module: *mut core::ffi::c_void,
    reason: u32,
    _: *mut core::ffi::c_void,
) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // LoadLibrary of the hook runs on the game's main thread, under the
        // loader lock, before the entry point. Keep going even if init panics
        // so a bug here doesn't fail the load with an empty error.
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| watch::init(module)));
    }
    1
}
