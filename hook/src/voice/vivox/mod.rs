//! Vivox voice chat (vivoxsdk.dll) on PeerParty's LAN voice rooms.
//!
//! The stand-in (x64.rs) builds and reads the SDK's requests, responses and events at
//! the fixed offsets of the 64-bit SDK (`x64::abi`). PeerParty has no 32-bit layout, so
//! an x86 build leaves the game's Vivox to the real SDK and never touches its structs.

#[cfg(target_arch = "x86_64")]
mod x64;
#[cfg(target_arch = "x86_64")]
pub use x64::{attach, looked_up};

#[cfg(not(target_arch = "x86_64"))]
use crate::log;

/// vivoxsdk.dll loaded: nothing of it is taken over in a 32-bit game.
#[cfg(not(target_arch = "x86_64"))]
pub fn attach(_base: usize, module_name: &str, _exports: &[crate::sys::pe::Export]) -> std::collections::HashSet<usize> {
    log!("vivox: {module_name} found; PeerParty knows only the 64-bit SDK's layouts, so the game's own Vivox answers");
    std::collections::HashSet::new()
}

/// A GetProcAddress from the game: nothing to point at PeerParty in a 32-bit game.
#[cfg(not(target_arch = "x86_64"))]
pub fn looked_up(_module: usize) {}
