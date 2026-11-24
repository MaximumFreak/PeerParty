//! Arming one detour: the steps every hooked function goes through, in one place.
//! Each slot is a `OnceLock<GenericDetour<T>>` the detour function calls the original
//! through; it's filled once and never emptied (a hook stays for the process's life).
//! All of it happens under the process-wide hook lock (`hook::hook_lock`), so two
//! layers arming at once never patch the same prologue together.

use std::sync::OnceLock;

use retour::GenericDetour;

use crate::log;
use crate::sys::hook::{frozen, hook_lock};
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
///
/// Generic only where it must be (the slot's type): the lock, the log lines and the
/// freezing are in `enable`, one copy for every hook there is (hundreds of signatures
/// each had a whole copy of all of it).
#[inline]
pub fn try_arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, tag: &str, label: &str, freeze: bool) -> Armed
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    let _g = hook_lock();
    if slot.get().is_some() {
        return untouched(tag, label, None);
    }
    let hook = match unsafe { GenericDetour::new(target, detour) } {
        Ok(h) => h,
        Err(e) => return untouched(tag, label, Some(&e)),
    };
    if slot.set(hook).is_err() {
        return Armed::Untouched;
    }
    let hook = slot.get().unwrap();
    enable(&|| unsafe { hook.enable() }, target.to_ptr() as usize, freeze, tag, label)
}

/// A detour not made: already there (`None`), or retour's reason.
#[inline(never)]
fn untouched(tag: &str, label: &str, why: Option<&retour::Error>) -> Armed {
    match why {
        None => log!("{tag} {label} already hooked"),
        Some(e) => log!("{tag} {label} not hooked: {e}"),
    }
    Armed::Untouched
}

/// Enables a detour just made on the function at `at` (with the other threads paused
/// when `freeze`), under the hook lock the caller holds.
#[inline(never)]
fn enable(on: &dyn Fn() -> retour::Result<()>, at: usize, freeze: bool, tag: &str, label: &str) -> Armed {
    let enabled = if freeze { frozen(at, on) } else { on() };
    match enabled {
        Ok(()) => Armed::Hooked,
        Err(e) => {
            log!("{tag} {label} enable failed: {e}");
            Armed::Failed
        }
    }
}

/// `try_arm` without freezing: whether the detour is in place now.
#[inline]
pub fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, tag: &str, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    try_arm(slot, target, detour, tag, label, false) == Armed::Hooked
}

/// `arm` for a function known by its address.
#[inline]
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

/// Where a group of hooks finds its functions: an export list (read as the module
/// loads, before the game runs in it), or a module already loaded (the game may be
/// running in it: other threads are paused while each prologue is written).
pub enum Source<'a> {
    Exports(&'a [Export]),
    Loaded(&'a dyn Fn(&std::ffi::CStr) -> Option<usize>),
}

impl Source<'_> {
    /// `name`'s address (a missing export is logged).
    #[inline(never)]
    pub fn addr(&self, tag: &str, name: &std::ffi::CStr) -> Option<usize> {
        match self {
            Source::Exports(exports) => export_addr(exports, tag, name.to_str().unwrap_or("")).map(|a| a as usize),
            Source::Loaded(find) => find(name),
        }
    }
}

/// Arms `detour` on `name` from `src` into `slot`: 1 when hooked.
#[inline]
pub fn arm_from<T>(src: &Source, slot: &OnceLock<GenericDetour<T>>, name: &std::ffi::CStr, detour: T, tag: &str) -> usize
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    let Some(a) = src.addr(tag, name) else { return 0 };
    let target: T = unsafe { T::from_ptr(a as *const ()) };
    usize::from(try_arm(slot, target, detour, tag, name.to_str().unwrap_or(""), matches!(src, Source::Loaded(_))) == Armed::Hooked)
}

/// Finds the export `name` and hands its address to `install`: 1 when that hooked it.
#[inline]
pub fn hook_one(exports: &[Export], tag: &str, name: &str, install: impl FnOnce(*const ()) -> bool) -> usize {
    export_addr(exports, tag, name).map_or(0, |a| usize::from(install(a)))
}

/// The export `name`'s address, or a log line saying there's none.
#[inline(never)]
pub fn export_addr(exports: &[Export], tag: &str, name: &str) -> Option<*const ()> {
    let Some(export) = exports.iter().find(|e| e.name == name) else {
        log!("{tag} {name} not exported");
        return None;
    };
    Some(export.addr)
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
