//! The Windows and hooking plumbing every layer shares: no SDK knowledge here.
//!
//! - detour.rs: arming one retour detour into its slot (`arm`, `arm_at`, `hook_one`).
//! - hook.rs: the generic logging hooks on every export, and the process-wide hook lock.
//! - pe.rs: a module's export table, read from memory.
//! - read.rs: safe reads of pointers a game hands over (strings, bytes).
//! - scan.rs: finding a function in a module by its bytes (a pattern with wildcards).
//! - win.rs: wide strings, module and export lookups, memory queries.
//! - com.rs: GUIDs and calls through a COM vtable.
//! - vtable.rs: C++ stand-in objects: `vmethod!` and the slot-forwarding stubs.
//! - swap.rs: swapping an SDK's functions by name (GetProcAddress and import tables).
//! - exitlog.rs: why a game ended.
//! - wine.rs: whether the game runs under Wine / Proton.

pub(crate) mod com;
pub(crate) mod detour;
pub(crate) mod exitlog;
pub(crate) mod hook;
pub(crate) mod pe;
pub(crate) mod read;
pub(crate) mod scan;
pub(crate) mod swap;
pub(crate) mod vtable;
pub(crate) mod win;
pub(crate) mod wine;

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `m`, whatever happened to the last holder. Detours and SDK callbacks run on
/// the game's threads, where a panic can't unwind (it would end the game): a lock some
/// other thread poisoned by panicking mid-update is taken as it is, rather than turning
/// every later call through that hook into another panic.
pub(crate) fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_poisoned_lock_is_still_taken() {
        static M: std::sync::Mutex<u32> = std::sync::Mutex::new(1);
        let _ = std::thread::spawn(|| {
            let _g = M.lock().unwrap();
            panic!("poisons it");
        })
        .join();
        assert!(M.is_poisoned());
        *super::lock(&M) += 1;
        assert_eq!(*super::lock(&M), 2);
    }
}
