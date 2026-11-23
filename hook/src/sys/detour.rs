//! Arming one detour: the steps every hooked function goes through, in one place.
//! Each slot is a `OnceLock<GenericDetour<T>>` the detour function calls the original
//! through; it's filled once and never emptied (a hook stays for the process's life).
//! All of it happens under the process-wide hook lock (`hook::with_lock`), so two
//! layers arming at once never patch the same prologue together.

use std::sync::OnceLock;

use retour::GenericDetour;

use crate::log;
use crate::sys::hook::{frozen, with_lock};
use crate::sys::pe::Export;

/// What arming did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Armed {
    /// The detour is in place.
    Hooked,
    /// Nothing was patched (retour couldn't make the detour, or the slot was taken):
    /// the function runs as it was, and the generic logging pass may still hook it.
    Untouched,
    /// Enabling failed part-way: the prologue may have been written, so nothing else
    /// may patch it again.
    Failed,
}

/// Arms `detour` on `target` into `slot` (once). `tag` and `label` name it in the log
/// ("dns", "getaddrinfo"). `freeze` stops the process's other threads while the
/// prologue is written, for a function they may be running (one found after start-up).
pub fn try_arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, tag: &str, label: &str, freeze: bool) -> Armed
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        if slot.get().is_some() {
            log!("{tag} {label} already hooked");
            return Armed::Untouched;
        }
        let hook = match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => h,
            Err(e) => {
                log!("{tag} {label} not hooked: {e}");
                return Armed::Untouched;
            }
        };
        if slot.set(hook).is_err() {
            return Armed::Untouched;
        }
        let hook = slot.get().unwrap();
        let enabled = if freeze { frozen(target.to_ptr() as usize, || unsafe { hook.enable() }) } else { unsafe { hook.enable() } };
        match enabled {
            Ok(()) => Armed::Hooked,
            Err(e) => {
                log!("{tag} {label} enable failed: {e}");
                Armed::Failed
            }
        }
    })
}

/// `try_arm` without freezing: whether the detour is in place now.
pub fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, tag: &str, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    try_arm(slot, target, detour, tag, label, false) == Armed::Hooked
}

/// `arm` for a function known by its address.
pub fn arm_at<T>(slot: &OnceLock<GenericDetour<T>>, addr: usize, detour: T, tag: &str, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    if addr == 0 {
        return false;
    }
    let target: T = unsafe { T::from_ptr(addr as *const ()) };
    arm(slot, target, detour, tag, label)
}

/// Finds the export `name` and hands its address to `install`: 1 when that hooked it.
pub fn hook_one(exports: &[Export], tag: &str, name: &str, install: impl FnOnce(*const ()) -> bool) -> usize {
    let Some(export) = exports.iter().find(|e| e.name == name) else {
        log!("{tag} {name} not exported");
        return 0;
    };
    usize::from(install(export.addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    type F = extern "system" fn(u64, u64, u64) -> u64;

    /// Long enough a prologue for a detour, with no jump back into it.
    #[inline(never)]
    extern "system" fn target(a: u64, b: u64, c: u64) -> u64 {
        let (a, b, c) = (std::hint::black_box(a), std::hint::black_box(b), std::hint::black_box(c));
        a.wrapping_mul(1_000_003).wrapping_add(b.rotate_left(7)) ^ c.wrapping_mul(31)
    }

    static SLOT: OnceLock<GenericDetour<F>> = OnceLock::new();

    extern "system" fn doubled(a: u64, b: u64, c: u64) -> u64 {
        SLOT.get().unwrap().call(a, b, c).wrapping_mul(2)
    }

    #[test]
    fn a_slot_is_armed_once() {
        let f = target as F;
        let want = target(3, 4, 5);
        let first = try_arm(&SLOT, f, doubled as F, "test", "target", false);
        assert_ne!(first, Armed::Failed);
        if first == Armed::Hooked {
            assert_eq!(std::hint::black_box(f)(3, 4, 5), want.wrapping_mul(2));
            assert_eq!(try_arm(&SLOT, f, doubled as F, "test", "target", true), Armed::Untouched, "taken");
        }
        assert!(!arm_at(&SLOT, 0, doubled as F, "test", "nothing"));
        assert_eq!(hook_one(&[], "test", "missing", |_| true), 0);
    }
}
