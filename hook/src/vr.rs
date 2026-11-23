//! VR kept out of split screen. A game with VR support asks the VR runtime at start-up
//! whether a headset is there, and with one plugged in (or a streaming app that looks
//! like one) it starts in VR and starts the VR runtime too. Split-screen copies share
//! one screen and can't share one headset, so they're told there's none: the game
//! starts on the flat screen as it does on a PC without a headset, and no VR runtime
//! is started.
//!
//! Two runtime interfaces are answered, found by their export names whatever the DLL
//! is called:
//! - OpenVR (`openvr_api.dll`): `VR_IsHmdPresent` and `VR_IsRuntimeInstalled` say no,
//!   and `VR_InitInternal` / `VR_InitInternal2` / `VR_GetGenericInterface` fail with
//!   "no headset found", without loading the runtime's client DLL.
//! - OpenXR (the Khronos loader, `openxr_loader.dll`): `xrCreateInstance` and
//!   `xrEnumerateInstanceExtensionProperties` fail with "runtime unavailable", without
//!   loading the runtime, and so does `xrGetInstanceProcAddr` handing them out.
//!
//! - Every runtime, whichever library or plugin looks for it (Oculus's OVRPlugin, which
//!   carries its own OpenXR loader and LibOVR client; an OpenXR loader built into the
//!   game; an `openvr_api` whose exports weren't hooked): a runtime DLL is reached by
//!   looking up its entry point by name once it's loaded, and in split screen that
//!   lookup finds nothing (`refused`, from PeerParty's `GetProcAddress`, sys/swap.rs).
//!   The library then reports no runtime, as it does when the runtime is missing or
//!   broken. The names: `xrNegotiateLoaderRuntimeInterface` (every OpenXR runtime),
//!   `VRClientCoreFactory` (OpenVR's runtime client) and `ovr_Initialize` (the Oculus
//!   runtime; LibOVR's client gives up when any of its functions is missing).
//!
//! A copy that is alone on the screen (no split screen, or the other players left) gets
//! the runtime's real answers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use retour::GenericDetour;

use crate::log;
use crate::sys::pe::Export;

/// `EVRInitError::VRInitError_Init_HmdNotFound`.
const VR_INIT_HMD_NOT_FOUND: i32 = 108;
/// `XR_ERROR_RUNTIME_UNAVAILABLE`.
const XR_ERROR_RUNTIME_UNAVAILABLE: i32 = -51;
const XR_SUCCESS: i32 = 0;

// OpenVR's calls are cdecl, OpenXR's stdcall (the same thing on x64).
type VrBool = unsafe extern "C" fn() -> bool;
type VrInit = unsafe extern "C" fn(*mut i32, i32) -> u32;
type VrInit2 = unsafe extern "C" fn(*mut i32, i32, *const u8) -> u32;
type VrGetInterface = unsafe extern "C" fn(*const u8, *mut i32) -> usize;
type XrCreateInstance = unsafe extern "system" fn(usize, *mut u64) -> i32;
type XrEnumExtensions = unsafe extern "system" fn(*const u8, u32, *mut u32, usize) -> i32;
type XrGetProc = unsafe extern "system" fn(u64, *const u8, *mut usize) -> i32;

static HMD_PRESENT: OnceLock<GenericDetour<VrBool>> = OnceLock::new();
static RUNTIME_INSTALLED: OnceLock<GenericDetour<VrBool>> = OnceLock::new();
static INIT: OnceLock<GenericDetour<VrInit>> = OnceLock::new();
static INIT2: OnceLock<GenericDetour<VrInit2>> = OnceLock::new();
static GET_INTERFACE: OnceLock<GenericDetour<VrGetInterface>> = OnceLock::new();
static XR_CREATE: OnceLock<GenericDetour<XrCreateInstance>> = OnceLock::new();
static XR_EXTENSIONS: OnceLock<GenericDetour<XrEnumExtensions>> = OnceLock::new();
static XR_GET_PROC: OnceLock<GenericDetour<XrGetProc>> = OnceLock::new();

static TOLD_OPENVR: AtomicBool = AtomicBool::new(false);
static TOLD_OPENXR: AtomicBool = AtomicBool::new(false);
static TOLD_LOOKUP: AtomicBool = AtomicBool::new(false);

/// The entry points by which a VR library reaches a runtime it has loaded.
const RUNTIME_ENTRIES: [&str; 3] = ["xrNegotiateLoaderRuntimeInterface", "VRClientCoreFactory", "ovr_Initialize"];

/// Whether a `GetProcAddress` lookup of `name` is answered "not found": a VR runtime's
/// entry point, in a split-screen copy (the caller sets `ERROR_PROC_NOT_FOUND`).
pub fn refused(name: *const u8) -> bool {
    if (name as usize) <= 0xffff || !blocked() {
        return false;
    }
    let Some(name) = crate::sys::read::cstr(name as usize) else { return false };
    if !is_runtime_entry(&name) {
        return false;
    }
    if !TOLD_LOOKUP.swap(true, Ordering::Relaxed) {
        log!("vr: the game looks for a VR runtime ({name}): not found, as this copy shares the screen (it starts on the flat screen)");
    }
    true
}

fn is_runtime_entry(name: &str) -> bool {
    RUNTIME_ENTRIES.contains(&name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runtime {
    OpenVr,
    OpenXr,
}

/// Which VR runtime interface a module's exports are, if any.
pub fn identify<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<Runtime> {
    let mut openvr = false;
    let mut openxr = false;
    for name in names {
        match name {
            "VR_InitInternal" | "VR_InitInternal2" | "VR_IsHmdPresent" => openvr = true,
            "xrCreateInstance" | "xrGetInstanceProcAddr" => openxr = true,
            _ => {}
        }
    }
    if openvr {
        Some(Runtime::OpenVr)
    } else if openxr {
        Some(Runtime::OpenXr)
    } else {
        None
    }
}

/// Whether this copy is told there's no headset: it shares the screen with others.
fn blocked() -> bool {
    crate::window::focus_spoofed()
}

/// A module exporting a VR runtime interface loaded (watch.rs). It's hooked only in a
/// split-screen copy; the hooks pass through again if the copy ends up alone.
pub fn attach(full: &str, kind: Runtime, exports: &[Export]) {
    if !blocked() {
        return;
    }
    let find = |name: &str| exports.iter().find(|e| e.name == name).map(|e| e.addr as usize).unwrap_or(0);
    let arm = |hooked: bool| usize::from(hooked);
    use crate::sys::detour::arm_at;
    let n = match kind {
        Runtime::OpenVr => {
            arm(arm_at(&HMD_PRESENT, find("VR_IsHmdPresent"), hmd_present as VrBool, "vr", "VR_IsHmdPresent"))
                + arm(arm_at(&RUNTIME_INSTALLED, find("VR_IsRuntimeInstalled"), runtime_installed as VrBool, "vr", "VR_IsRuntimeInstalled"))
                + arm(arm_at(&INIT, find("VR_InitInternal"), init as VrInit, "vr", "VR_InitInternal"))
                + arm(arm_at(&INIT2, find("VR_InitInternal2"), init2 as VrInit2, "vr", "VR_InitInternal2"))
                + arm(arm_at(&GET_INTERFACE, find("VR_GetGenericInterface"), get_interface as VrGetInterface, "vr", "VR_GetGenericInterface"))
        }
        Runtime::OpenXr => {
            arm(arm_at(&XR_CREATE, find("xrCreateInstance"), xr_create as XrCreateInstance, "vr", "xrCreateInstance"))
                + arm(arm_at(&XR_EXTENSIONS, find("xrEnumerateInstanceExtensionProperties"), xr_extensions as XrEnumExtensions, "vr", "xrEnumerateInstanceExtensionProperties"))
                + arm(arm_at(&XR_GET_PROC, find("xrGetInstanceProcAddr"), xr_get_proc as XrGetProc, "vr", "xrGetInstanceProcAddr"))
        }
    };
    let what = match kind {
        Runtime::OpenVr => "OpenVR",
        Runtime::OpenXr => "OpenXR",
    };
    log!("vr: {full} is {what}: hooked {n}, so this split-screen copy is told there's no headset");
}

fn told(flag: &AtomicBool, call: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        log!("vr: the game asks for VR ({call}): no headset, as this copy shares the screen (it starts on the flat screen)");
    }
}

unsafe extern "C" fn hmd_present() -> bool {
    if !blocked() {
        return unsafe { HMD_PRESENT.get().unwrap().call() };
    }
    told(&TOLD_OPENVR, "VR_IsHmdPresent");
    false
}

unsafe extern "C" fn runtime_installed() -> bool {
    if !blocked() {
        return unsafe { RUNTIME_INSTALLED.get().unwrap().call() };
    }
    told(&TOLD_OPENVR, "VR_IsRuntimeInstalled");
    false
}

unsafe fn no_headset(error: *mut i32) {
    if !error.is_null() {
        unsafe { error.write(VR_INIT_HMD_NOT_FOUND) };
    }
}

unsafe extern "C" fn init(error: *mut i32, app: i32) -> u32 {
    if !blocked() {
        return unsafe { INIT.get().unwrap().call(error, app) };
    }
    told(&TOLD_OPENVR, "VR_InitInternal");
    unsafe { no_headset(error) };
    0
}

unsafe extern "C" fn init2(error: *mut i32, app: i32, info: *const u8) -> u32 {
    if !blocked() {
        return unsafe { INIT2.get().unwrap().call(error, app, info) };
    }
    told(&TOLD_OPENVR, "VR_InitInternal2");
    unsafe { no_headset(error) };
    0
}

unsafe extern "C" fn get_interface(version: *const u8, error: *mut i32) -> usize {
    if !blocked() {
        return unsafe { GET_INTERFACE.get().unwrap().call(version, error) };
    }
    told(&TOLD_OPENVR, "VR_GetGenericInterface");
    unsafe { no_headset(error) };
    0
}

unsafe extern "system" fn xr_create(info: usize, instance: *mut u64) -> i32 {
    if !blocked() {
        return unsafe { XR_CREATE.get().unwrap().call(info, instance) };
    }
    told(&TOLD_OPENXR, "xrCreateInstance");
    XR_ERROR_RUNTIME_UNAVAILABLE
}

unsafe extern "system" fn xr_extensions(layer: *const u8, capacity: u32, count: *mut u32, props: usize) -> i32 {
    if !blocked() {
        return unsafe { XR_EXTENSIONS.get().unwrap().call(layer, capacity, count, props) };
    }
    told(&TOLD_OPENXR, "xrEnumerateInstanceExtensionProperties");
    if !count.is_null() {
        unsafe { count.write(0) };
    }
    XR_ERROR_RUNTIME_UNAVAILABLE
}

/// The loader hands out its pre-instance functions here too (Unity's OpenXR plugin
/// finds them this way): those come to the stand-ins above.
unsafe extern "system" fn xr_get_proc(instance: u64, name: *const u8, out: *mut usize) -> i32 {
    if blocked() && instance == 0 && !out.is_null() {
        let ours = match crate::sys::read::cstr(name as usize).as_deref() {
            Some("xrCreateInstance") => Some(xr_create as XrCreateInstance as usize),
            Some("xrEnumerateInstanceExtensionProperties") => Some(xr_extensions as XrEnumExtensions as usize),
            _ => None,
        };
        if let Some(f) = ours {
            unsafe { out.write(f) };
            return XR_SUCCESS;
        }
    }
    unsafe { XR_GET_PROC.get().unwrap().call(instance, name, out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vr_runtimes_are_known_by_their_exports() {
        assert_eq!(identify(["VR_GetGenericInterface", "VR_InitInternal2", "VR_IsHmdPresent"]), Some(Runtime::OpenVr));
        assert_eq!(identify(["xrCreateInstance", "xrGetInstanceProcAddr", "xrEnumerateApiLayerProperties"]), Some(Runtime::OpenXr));
        assert_eq!(identify(["SteamAPI_Init", "XInputGetState"]), None);
    }

    #[test]
    fn only_runtime_entry_points_are_refused() {
        for name in RUNTIME_ENTRIES {
            assert!(is_runtime_entry(name));
        }
        // An OpenXR API layer's entry, and LibOVR functions other than the first, still resolve.
        for name in ["xrNegotiateLoaderApiLayerInterface", "ovr_InitializeRenderingShim", "ovr_Shutdown", "GetProcAddress", ""] {
            assert!(!is_runtime_entry(name), "{name}");
        }
    }
}
