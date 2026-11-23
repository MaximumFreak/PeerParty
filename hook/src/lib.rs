//! Injected into the game before its entry point: watches SDK modules load and gives
//! each one's multiplayer calls PeerParty's backend (watch.rs). A loaded EasyAntiCheat or
//! BattlEye module stops the process, as does an EOS Anti-Cheat session (eos/mod.rs).

// Shared infrastructure.
mod config;
mod hub;
mod https;
mod log;
mod sys;
mod util;

// The process: module watching, the launcher's pipe, the overlay, the LAN engine.
mod child;
mod instance;
mod invite;
mod ipc;
mod net;
mod overlay;
mod pointer;
mod saves;
mod screen_prefs;
mod voice;
mod vr;
mod watch;

// Input, video, overlapped I/O dispatch, and per-instance window/viewport spoofing.
mod input;
mod overlapped;
mod video;
mod window;

// The SDK layers.
mod demonware;
mod eos;
mod galaxy;
mod photon;
mod playfab;
mod steam;
mod ugs;
mod xboxlive;

#[cfg(test)]
mod untrusted_tests;

/// A test that needs something this PC may not have (an SDK DLL from an installed game,
/// named by a PPTEST_* variable) and returns without it says so on stderr; with
/// `PPTEST_REQUIRE=1` (a run that should have everything) it fails instead, so a missing
/// file can't pass for a test that ran.
#[cfg(test)]
pub(crate) fn skip(why: &str) {
    if std::env::var("PPTEST_REQUIRE").is_ok_and(|v| v == "1") {
        panic!("required but skipped: {why}");
    }
    eprintln!("skipped: {why}");
}

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
        loaded();
    }
    1
}

/// Tells the launcher the hook is in (its event, made before the game ran:
/// launcher/src/main.rs `watch_hook_load`). Nothing when it didn't make one.
fn loaded() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, GetCurrentProcessId, OpenEventW, SetEvent};
    let name: Vec<u16> = format!("Local\\peerparty-loaded-{}", unsafe { GetCurrentProcessId() }).encode_utf16().chain(Some(0)).collect();
    let e = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if !e.is_null() {
        unsafe {
            SetEvent(e);
            CloseHandle(e);
        }
    }
}
