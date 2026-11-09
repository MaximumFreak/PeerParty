//! Lets a second copy of a game start on the same PC, for testing two players on one
//! machine. Unity's "Force Single Instance" builds create a named mutex containing
//! `SingleInstanceMutex` and quit when it already exists. With PEERPARTY_MULTI_INSTANCE
//! set (the launcher's `--multi-instance`), that name gets this process's id appended,
//! so each copy owns its own mutex. Other mutexes are untouched.

use std::sync::OnceLock;

use retour::GenericDetour;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;
use crate::read;

type CreateMutexW = unsafe extern "system" fn(*const core::ffi::c_void, i32, *const u16) -> isize;
type CreateMutexExW =
    unsafe extern "system" fn(*const core::ffi::c_void, *const u16, u32, u32) -> isize;
type OpenMutexW = unsafe extern "system" fn(u32, i32, *const u16) -> isize;

static CREATE: OnceLock<GenericDetour<CreateMutexW>> = OnceLock::new();
static CREATE_EX: OnceLock<GenericDetour<CreateMutexExW>> = OnceLock::new();
static OPEN: OnceLock<GenericDetour<OpenMutexW>> = OnceLock::new();

const MARKER: &str = "SingleInstanceMutex";

pub fn enabled() -> bool {
    std::env::var_os("PEERPARTY_MULTI_INSTANCE").is_some()
}

/// Hooks kernelbase's mutex entry points. kernel32's exports forward here.
pub fn attach_kernelbase(exports: &[Export]) -> usize {
    let mut n = 0;
    n += hook_one(exports, "CreateMutexW", |addr| {
        let target: CreateMutexW = unsafe { std::mem::transmute(addr) };
        arm(&CREATE, target, detour_create, "CreateMutexW")
    });
    n += hook_one(exports, "CreateMutexExW", |addr| {
        let target: CreateMutexExW = unsafe { std::mem::transmute(addr) };
        arm(&CREATE_EX, target, detour_create_ex, "CreateMutexExW")
    });
    n += hook_one(exports, "OpenMutexW", |addr| {
        let target: OpenMutexW = unsafe { std::mem::transmute(addr) };
        arm(&OPEN, target, detour_open, "OpenMutexW")
    });
    n
}

fn hook_one(exports: &[Export], name: &str, install: impl FnOnce(*const ()) -> bool) -> usize {
    let Some(export) = exports.iter().find(|e| e.name == name) else {
        log!("instance {name} not exported");
        return 0;
    };
    usize::from(install(export.addr))
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let hook = match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => h,
            Err(e) => {
                log!("instance {label} not hooked: {e}");
                return false;
            }
        };
        if slot.set(hook).is_err() {
            log!("instance {label} already hooked");
            return false;
        }
        if let Err(e) = unsafe { slot.get().unwrap().enable() } {
            log!("instance {label} enable failed: {e}");
            return false;
        }
        true
    })
}

/// The per-process name for a single-instance mutex, or None to leave `name` alone.
fn rename(name: *const u16, api: &str) -> Option<Vec<u16>> {
    if name.is_null() {
        return None;
    }
    let text = read::wstr(name as usize)?;
    // CreateMutexW may reach CreateMutexExW internally; rename only once.
    if !text.contains(MARKER) || text.contains("-peerparty-") {
        return None;
    }
    let renamed = format!("{text}-peerparty-{}", unsafe { GetCurrentProcessId() });
    log!("instance {api} {text} -> {renamed}");
    Some(renamed.encode_utf16().chain(Some(0)).collect())
}

unsafe extern "system" fn detour_create(
    attrs: *const core::ffi::c_void,
    owner: i32,
    name: *const u16,
) -> isize {
    let Some(hook) = CREATE.get() else { return 0 };
    let renamed = rename(name, "CreateMutexW");
    let name = renamed.as_ref().map_or(name, |w| w.as_ptr());
    unsafe { hook.call(attrs, owner, name) }
}

unsafe extern "system" fn detour_create_ex(
    attrs: *const core::ffi::c_void,
    name: *const u16,
    flags: u32,
    access: u32,
) -> isize {
    let Some(hook) = CREATE_EX.get() else {
        return 0;
    };
    let renamed = rename(name, "CreateMutexExW");
    let name = renamed.as_ref().map_or(name, |w| w.as_ptr());
    unsafe { hook.call(attrs, name, flags, access) }
}

unsafe extern "system" fn detour_open(access: u32, inherit: i32, name: *const u16) -> isize {
    let Some(hook) = OPEN.get() else { return 0 };
    let renamed = rename(name, "OpenMutexW");
    let name = renamed.as_ref().map_or(name, |w| w.as_ptr());
    unsafe { hook.call(access, inherit, name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    #[test]
    fn renames_only_single_instance_mutexes_once() {
        let pid = unsafe { GetCurrentProcessId() };
        let unity = wide("Some Studio-SingleInstanceMutex-SomeGameApp");
        let got = rename(unity.as_ptr(), "test").expect("renamed");
        let got = String::from_utf16(&got[..got.len() - 1]).unwrap();
        assert_eq!(
            got,
            format!("Some Studio-SingleInstanceMutex-SomeGameApp-peerparty-{pid}")
        );
        assert!(rename(wide(&got).as_ptr(), "test").is_none());
        assert!(rename(wide("Local\\SomethingElse").as_ptr(), "test").is_none());
        assert!(rename(std::ptr::null(), "test").is_none());
    }
}
