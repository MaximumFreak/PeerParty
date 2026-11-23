//! A split-screen copy's game doesn't see the sound devices of controllers that aren't
//! its own. A DualSense on USB is a sound device too (its speaker and haptics), and a
//! game that plays to it from every copy sounds on one player's controller for all of
//! them. Windows' endpoint enumerator (IMMDeviceEnumerator, mmdevapi.dll) is hooked
//! before the game's first one is made in a copy in a pane (games pick their sound
//! devices at start, before the copy has a controller): mmdevapi's `DllGetClassObject`
//! by export when it loads, then its class factory's `CreateInstance`, whose enumerator
//! gives the methods. `EnumAudioEndpoints`' collection leaves out the endpoints in the
//! same device container as a HID game controller that isn't this copy's (`GetCount`,
//! `Item`), `GetDevice` doesn't find them, and `GetDefaultAudioEndpoint` gives the first
//! endpoint left when the default is one of them. An XInput controller's headset
//! (XInput's own audio calls) is left alone, as its reads are. A controller built into
//! the PC (a handheld's) shares its container with the PC's own speakers and microphone,
//! which are never left out.
//!
//! Before the copy has a controller every controller's sound device is left out, so the
//! one that becomes its own only has its sound if the game looks for devices again.
//! PeerParty's own voice chat sees every endpoint (`unfiltered`): the echo canceller
//! numbers them as Windows does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use retour::GenericDetour;

use super::{device, module, proc_addr};
use crate::log;
use crate::sys::com::{Guid, release, vcall};
use crate::sys::hook::with_lock;

type EnumEndpoints = unsafe extern "system" fn(usize, u32, u32, *mut usize) -> i32;
type GetDefault = unsafe extern "system" fn(usize, u32, u32, *mut usize) -> i32;
type GetDevice = unsafe extern "system" fn(usize, *const u16, *mut usize) -> i32;
type GetCount = unsafe extern "system" fn(usize, *mut u32) -> i32;
type Item = unsafe extern "system" fn(usize, u32, *mut usize) -> i32;
type GetClassObject = unsafe extern "system" fn(*const Guid, *const Guid, *mut usize) -> i32;
type CreateInstance = unsafe extern "system" fn(usize, usize, *const Guid, *mut usize) -> i32;

static ENUM: OnceLock<GenericDetour<EnumEndpoints>> = OnceLock::new();
static DEFAULT: OnceLock<GenericDetour<GetDefault>> = OnceLock::new();
static GET_DEVICE: OnceLock<GenericDetour<GetDevice>> = OnceLock::new();
static COUNT: OnceLock<GenericDetour<GetCount>> = OnceLock::new();
static ITEM: OnceLock<GenericDetour<Item>> = OnceLock::new();
static CLASS_OBJECT: OnceLock<GenericDetour<GetClassObject>> = OnceLock::new();
static CREATE: OnceLock<GenericDetour<CreateInstance>> = OnceLock::new();
static BUSY: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// PeerParty's own calls on this thread (voice chat): every endpoint.
    static UNFILTERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` with this copy's sound devices as Windows has them, none left out:
/// PeerParty's own voice chat, whose echo canceller numbers endpoints as Windows does.
pub(crate) fn unfiltered<R>(f: impl FnOnce() -> R) -> R {
    let was = UNFILTERED.with(|u| u.replace(true));
    let r = f();
    UNFILTERED.with(|u| u.set(was));
    r
}

fn bypassed() -> bool {
    UNFILTERED.with(std::cell::Cell::get)
}

/// Collections handed to the game with endpoints left out: the indices kept.
static FILTERED: Mutex<Vec<(usize, Vec<u32>)>> = Mutex::new(Vec::new());

const E_NOTFOUND: i32 = 0x8007_0490_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

const CLSID_MM_ENUMERATOR: Guid = Guid(0xBCDE0395, 0xE52F, 0x467C, [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E]);
const IID_MM_ENUMERATOR: Guid = Guid(0xA95664D2, 0x9614, 0x4F35, [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6]);

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *const std::ffi::c_void, flags: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(clsid: *const Guid, outer: *const std::ffi::c_void, ctx: u32, iid: *const Guid, out: *mut usize) -> i32;
    fn PropVariantClear(v: *mut PropVariant) -> i32;
}

#[repr(C)]
struct PropVariant {
    vt: u16,
    reserved: [u16; 3],
    value: [u64; 2],
}

/// `PROPERTYKEY` / `DEVPROPKEY`: a format id and a property id.
#[repr(C)]
struct Key(Guid, u32);

/// PKEY_Device_ContainerId (DEVPKEY_Device_ContainerId): which physical device it's in.
const CONTAINER_ID: Key = Key(Guid(0x8C7ED206, 0x3F8A, 0x4827, [0xB3, 0xAB, 0xAE, 0x9E, 0x1F, 0xAE, 0xFC, 0x6C]), 2);
/// DEVPKEY_Device_InstanceId.
const INSTANCE_ID: Key = Key(Guid(0x78C34FC8, 0x104A, 0x4ACA, [0x9E, 0xA4, 0x52, 0x4D, 0x52, 0x99, 0x6E, 0x57]), 256);
/// PKEY_Device_FriendlyName, for the log.
const FRIENDLY_NAME: Key = Key(Guid(0xA45C254E, 0xDF1C, 0x4EFD, [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0]), 14);

/// Hooks the endpoint enumerator: mmdevapi.dll's `DllGetClassObject` at once (by export:
/// nothing in it is called, and mmdevapi.dll may just be loading), so the game's first
/// enumerator already has them; and an enumerator of the hook's own, off the calling
/// thread, for one the game made before. Once.
pub fn install() {
    if let Some(m) = module("mmdevapi.dll")
        && let Some(a) = proc_addr(m, c"DllGetClassObject".as_ptr().cast())
        && arm(&CLASS_OBJECT, a, class_object as GetClassObject, "mmdevapi DllGetClassObject") > 0
    {
        log!("input: sound devices: mmdevapi's class object hooked (the game's first enumerator leaves out other players' controllers' sound devices)");
    }
    if ENUM.get().is_some() || BUSY.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new().name("peerparty-audio".into()).spawn(|| {
        let n = unsafe { install_on_own_thread() };
        if n > 0 {
            log!("input: sound devices: {n} enumerator calls hooked (other players' controllers' sound devices are left out)");
        }
        BUSY.store(false, Ordering::Relaxed);
    });
    if spawned.is_err() {
        BUSY.store(false, Ordering::Relaxed);
    }
}

unsafe fn install_on_own_thread() -> usize {
    // S_OK or S_FALSE: this call's to undo.
    let inited = unsafe { CoInitializeEx(std::ptr::null(), 0) } >= 0;
    let mut e = 0usize;
    let n = if unsafe { CoCreateInstance(&CLSID_MM_ENUMERATOR, std::ptr::null(), 1, &IID_MM_ENUMERATOR, &mut e) } >= 0 && e != 0 {
        let n = hook_enumerator(e);
        release(e);
        n
    } else {
        0
    };
    if inited {
        unsafe { CoUninitialize() };
    }
    n
}

/// Hooks the methods of enumerator `e` (EnumAudioEndpoints 3, GetDefaultAudioEndpoint 4,
/// GetDevice 5) and of its collections (GetCount 3, Item 4).
fn hook_enumerator(e: usize) -> usize {
    let slot = |obj: usize, i: usize| unsafe { *(*(obj as *const *const usize)).add(i) };
    let mut n = 0;
    n += arm(&ENUM, slot(e, 3), enum_endpoints as EnumEndpoints, "EnumAudioEndpoints");
    n += arm(&DEFAULT, slot(e, 4), default_endpoint as GetDefault, "GetDefaultAudioEndpoint");
    n += arm(&GET_DEVICE, slot(e, 5), get_device as GetDevice, "GetDevice");
    if COUNT.get().is_none() {
        // A collection, for its class's methods: every endpoint, any state (through the
        // real call: the hooked one may leave some out).
        let mut coll = 0usize;
        match ENUM.get() {
            Some(h) => unsafe { h.call(e, 2, 0xF, &mut coll) },
            None => vcall!(e, 3, fn(u32, u32, *mut usize) -> i32, 2, 0xF, &mut coll),
        };
        if coll != 0 {
            n += arm(&COUNT, slot(coll, 3), get_count as GetCount, "IMMDeviceCollection::GetCount");
            n += arm(&ITEM, slot(coll, 4), item as Item, "IMMDeviceCollection::Item");
            release(coll);
        }
    }
    n
}

/// mmdevapi's class object, asked for by COM when the game makes an enumerator: the
/// factory's `CreateInstance` is hooked, which hooks the enumerator it makes. COM calls
/// this once the DLL has loaded, outside the loader lock.
unsafe extern "system" fn class_object(clsid: *const Guid, iid: *const Guid, out: *mut usize) -> i32 {
    let Some(h) = CLASS_OBJECT.get() else { return E_NOTFOUND };
    let hr = unsafe { h.call(clsid, iid, out) };
    if hr >= 0 && !clsid.is_null() && !out.is_null() && unsafe { *out } != 0 && unsafe { &*clsid }.same(&CLSID_MM_ENUMERATOR) && CREATE.get().is_none() {
        let factory = unsafe { *out };
        // IClassFactory::CreateInstance is slot 3.
        let at = unsafe { *(*(factory as *const *const usize)).add(3) };
        arm(&CREATE, at, create_instance as CreateInstance, "mmdevapi IClassFactory::CreateInstance");
    }
    hr
}

unsafe extern "system" fn create_instance(this: usize, outer: usize, iid: *const Guid, out: *mut usize) -> i32 {
    let Some(h) = CREATE.get() else { return E_NOTFOUND };
    let hr = unsafe { h.call(this, outer, iid, out) };
    if hr >= 0 && !out.is_null() && unsafe { *out } != 0 && ENUM.get().is_none() && crate::window::in_pane() {
        let made = unsafe { *out };
        let mut e = 0usize;
        // The enumerator interface, whichever the game asked for.
        if vcall!(made, 0, fn(*const Guid, *mut usize) -> i32, &IID_MM_ENUMERATOR, &mut e) >= 0 && e != 0 {
            let n = hook_enumerator(e);
            release(e);
            log!("input: sound devices: the game's first enumerator: {n} of its calls hooked (other players' controllers' sound devices are left out)");
        }
    }
    hr
}

/// Controllers changed: which devices are other players' is looked at again.
pub(super) fn devices_changed() {
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn arm<T: retour::Function + Copy>(cell: &OnceLock<GenericDetour<T>>, addr: usize, detour: T, label: &str) -> usize {
    with_lock(|| {
        if cell.get().is_some() || addr == 0 {
            return 0;
        }
        let target: T = unsafe { T::from_ptr(addr as *const ()) };
        match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => {
                let _ = cell.set(h);
                usize::from(crate::sys::hook::frozen(addr, || unsafe { cell.get().unwrap().enable() }).is_ok())
            }
            Err(e) => {
                log!("input: {label} not hooked: {e}");
                0
            }
        }
    })
}

/// The device containers looked up last, for a moment for the same seat (looking means
/// opening every HID device).
type Cached = Option<(std::time::Instant, (bool, u64), Vec<[u8; 16]>)>;
static CACHE: Mutex<Cached> = Mutex::new(None);

/// The container of devices built into the PC (`GUID_CONTAINER_ID_BASE`,
/// {00000000-0000-0000-FFFF-FFFFFFFFFFFF}): a handheld's own controller is in it, and so
/// are its speakers and microphone.
const BUILT_IN: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

/// Whether endpoints in container `c` may be left out: not one no device names
/// (GUID_NULL), nor the PC's own.
fn separable(c: &[u8; 16]) -> bool {
    *c != [0; 16] && *c != BUILT_IN
}

/// The device containers of the HID game controllers that aren't this copy's (none
/// outside split screen; all of them before the copy has a controller). XInput
/// controllers' HID sides are left out (see the module's note).
fn others_containers() -> Vec<[u8; 16]> {
    let seat = match super::topology() {
        super::Policy::Virtual(snap) => Some(snap),
        // Not told its seat yet: a copy in a pane has none.
        _ if !super::shm::attached() && crate::window::in_pane() => None,
        _ => return Vec::new(),
    };
    let who = seat.as_ref().map_or((false, 0), |s| (s.pad.connected, s.pad.hid_key));
    if let Some((at, w, c)) = CACHE.lock().unwrap().as_ref()
        && *w == who
        && at.elapsed() < std::time::Duration::from_secs(2)
    {
        return c.clone();
    }
    let found: Vec<[u8; 16]> = device::hid_interface_paths()
        .into_iter()
        .filter(|p| !device::is_xinput_path(p))
        .filter_map(|p| {
            let d = device::of_path(&p)?;
            let own = seat.as_ref().is_some_and(|s| device::is_own(&d, s));
            (d.controller && !own).then(|| container_of_interface(&p)).flatten()
        })
        .filter(|c| {
            let keep = separable(c);
            if !keep {
                crate::log::throttled("input: audio built-in", 1, |_| {
                    log!("input: sound devices: a controller is built into the PC (or names no container): the PC's own sound devices are never left out");
                });
            }
            keep
        })
        .collect();
    *CACHE.lock().unwrap() = Some((std::time::Instant::now(), who, found.clone()));
    found
}

/// The device container of the device interface `path` (NUL-terminated), from
/// cfgmgr32.dll.
fn container_of_interface(path: &[u16]) -> Option<[u8; 16]> {
    type IfaceProp = unsafe extern "system" fn(*const u16, *const Key, *mut u32, *mut u8, *mut u32, u32) -> u32;
    type Locate = unsafe extern "system" fn(*mut u32, *const u16, u32) -> u32;
    type NodeProp = unsafe extern "system" fn(u32, *const Key, *mut u32, *mut u8, *mut u32, u32) -> u32;
    let m = module("cfgmgr32.dll")?;
    let iface: IfaceProp = unsafe { std::mem::transmute(proc_addr(m, c"CM_Get_Device_Interface_PropertyW".as_ptr().cast())?) };
    let locate: Locate = unsafe { std::mem::transmute(proc_addr(m, c"CM_Locate_DevNodeW".as_ptr().cast())?) };
    let node: NodeProp = unsafe { std::mem::transmute(proc_addr(m, c"CM_Get_DevNode_PropertyW".as_ptr().cast())?) };
    let mut id = [0u16; 512];
    let (mut ty, mut size) = (0u32, (id.len() * 2) as u32);
    if unsafe { iface(path.as_ptr(), &INSTANCE_ID, &mut ty, id.as_mut_ptr().cast(), &mut size, 0) } != 0 {
        return None;
    }
    let mut inst = 0u32;
    if unsafe { locate(&mut inst, id.as_ptr(), 0) } != 0 {
        return None;
    }
    let mut guid = [0u8; 16];
    let mut size = 16u32;
    (unsafe { node(inst, &CONTAINER_ID, &mut ty, guid.as_mut_ptr(), &mut size, 0) } == 0 && size == 16).then_some(guid)
}

/// A property of endpoint `dev` (IMMDevice::OpenPropertyStore, IPropertyStore::GetValue):
/// a GUID (VT_CLSID) or a string (VT_LPWSTR).
fn endpoint_prop(dev: usize, key: &Key) -> (Option<[u8; 16]>, Option<String>) {
    let mut store = 0usize;
    if vcall!(dev, 4, fn(u32, *mut usize) -> i32, 0, &mut store) < 0 || store == 0 {
        return (None, None);
    }
    let mut v = PropVariant { vt: 0, reserved: [0; 3], value: [0; 2] };
    let ok = vcall!(store, 5, fn(*const Key, *mut PropVariant) -> i32, key, &mut v) >= 0;
    release(store);
    let p = v.value[0] as usize;
    let out = match (ok, v.vt) {
        (true, 72) if p != 0 => (Some(unsafe { *(p as *const [u8; 16]) }), None),
        (true, 31) if p != 0 => {
            let s = p as *const u16;
            let mut n = 0;
            while unsafe { *s.add(n) } != 0 && n < 512 {
                n += 1;
            }
            (None, Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) })))
        }
        _ => (None, None),
    };
    unsafe { PropVariantClear(&mut v) };
    out
}

/// Whether endpoint `dev` is in one of `containers`.
fn hidden(dev: usize, containers: &[[u8; 16]]) -> bool {
    !containers.is_empty() && endpoint_prop(dev, &CONTAINER_ID).0.is_some_and(|c| containers.contains(&c))
}

fn left_out(dev: usize) {
    let name = endpoint_prop(dev, &FRIENDLY_NAME).1.unwrap_or_else(|| "a sound device".into());
    crate::log::throttled(&format!("input: audio left out {name}"), 1, |_| {
        log!("input: sound devices: \"{name}\" is another player's controller's (or a controller while this copy has none): left out for this copy's game");
    });
}

unsafe extern "system" fn enum_endpoints(this: usize, flow: u32, mask: u32, out: *mut usize) -> i32 {
    let hr = unsafe { ENUM.get().unwrap().call(this, flow, mask, out) };
    if hr < 0 || out.is_null() || bypassed() {
        return hr;
    }
    let coll = unsafe { *out };
    let mut filtered = FILTERED.lock().unwrap();
    filtered.retain(|(c, _)| *c != coll);
    drop(filtered);
    let (Some(count), Some(item)) = (COUNT.get(), ITEM.get()) else { return hr };
    let containers = others_containers();
    if coll == 0 || containers.is_empty() {
        return hr;
    }
    let mut n = 0u32;
    unsafe { count.call(coll, &mut n) };
    let mut kept = Vec::with_capacity(n as usize);
    for i in 0..n {
        let mut dev = 0usize;
        if unsafe { item.call(coll, i, &mut dev) } < 0 || dev == 0 {
            kept.push(i);
            continue;
        }
        if hidden(dev, &containers) {
            left_out(dev);
        } else {
            kept.push(i);
        }
        release(dev);
    }
    if kept.len() < n as usize {
        let mut filtered = FILTERED.lock().unwrap();
        if filtered.len() >= 32 {
            filtered.remove(0);
        }
        filtered.push((coll, kept));
    }
    hr
}

/// The default endpoint, unless it's another player's controller's: then the first
/// endpoint of its flow left for this copy (none: not found).
unsafe extern "system" fn default_endpoint(this: usize, flow: u32, role: u32, out: *mut usize) -> i32 {
    let hr = unsafe { DEFAULT.get().unwrap().call(this, flow, role, out) };
    if hr < 0 || out.is_null() || unsafe { *out } == 0 || bypassed() {
        return hr;
    }
    let dev = unsafe { *out };
    let containers = others_containers();
    if !hidden(dev, &containers) {
        return hr;
    }
    left_out(dev);
    release(dev);
    unsafe { *out = 0 };
    // The active endpoints of `flow` (DEVICE_STATE_ACTIVE), through the hooks: the
    // first one left.
    let mut coll = 0usize;
    if unsafe { enum_endpoints(this, flow, 1, &mut coll) } < 0 || coll == 0 {
        return E_NOTFOUND;
    }
    let mut first = 0usize;
    let mut n = 0u32;
    unsafe { get_count(coll, &mut n) };
    if n > 0 {
        unsafe { item(coll, 0, &mut first) };
    }
    FILTERED.lock().unwrap().retain(|(c, _)| *c != coll);
    release(coll);
    if first == 0 {
        log!("input: sound devices: the default one is another player's controller's, and no other is there: none");
        return E_NOTFOUND;
    }
    let name = endpoint_prop(first, &FRIENDLY_NAME).1.unwrap_or_else(|| "another".into());
    crate::log::throttled(&format!("input: audio default {name}"), 1, |_| {
        log!("input: sound devices: the default one is another player's controller's: \"{name}\" instead");
    });
    unsafe { *out = first };
    0
}

unsafe extern "system" fn get_device(this: usize, id: *const u16, out: *mut usize) -> i32 {
    let hr = unsafe { GET_DEVICE.get().unwrap().call(this, id, out) };
    if hr < 0 || out.is_null() || unsafe { *out } == 0 || bypassed() {
        return hr;
    }
    let dev = unsafe { *out };
    let containers = others_containers();
    if hidden(dev, &containers) {
        left_out(dev);
        release(dev);
        unsafe { *out = 0 };
        return E_NOTFOUND;
    }
    hr
}

unsafe extern "system" fn get_count(coll: usize, n: *mut u32) -> i32 {
    let hr = unsafe { COUNT.get().unwrap().call(coll, n) };
    if hr >= 0
        && !n.is_null()
        && let Some(k) = FILTERED.lock().unwrap().iter().find(|(c, _)| *c == coll).map(|(_, k)| k.len())
    {
        unsafe { *n = k as u32 };
    }
    hr
}

unsafe extern "system" fn item(coll: usize, i: u32, out: *mut usize) -> i32 {
    let real = ITEM.get().unwrap();
    let mapped = FILTERED.lock().unwrap().iter().find(|(c, _)| *c == coll).map(|(_, k)| k.get(i as usize).copied());
    match mapped {
        None => unsafe { real.call(coll, i, out) },
        Some(Some(j)) => unsafe { real.call(coll, j, out) },
        Some(None) => {
            if !out.is_null() {
                unsafe { *out = 0 };
            }
            E_INVALIDARG
        }
    }
}
