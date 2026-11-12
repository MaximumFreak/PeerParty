//! The little of COM that the sound devices need (aec.rs, endpoints.rs): GUIDs, calls
//! through a vtable, and Windows' audio endpoint enumerator (MMDevice API).

use std::ffi::c_void;

pub type Hresult = i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

impl Guid {
    pub fn same(&self, o: &Guid) -> bool {
        self.0 == o.0 && self.1 == o.1 && self.2 == o.2 && self.3 == o.3
    }
}

pub const IID_UNKNOWN: Guid = Guid(0, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
const CLSID_MM_ENUMERATOR: Guid = Guid(0xBCDE0395, 0xE52F, 0x467C, [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E]);
const IID_MM_ENUMERATOR: Guid = Guid(0xA95664D2, 0x9614, 0x4F35, [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6]);

/// EDataFlow.
pub const RENDER: u32 = 0;
pub const CAPTURE: u32 = 1;
/// ERole.
pub const CONSOLE: u32 = 0;
pub const COMMUNICATIONS: u32 = 2;

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *const c_void, flags: u32) -> Hresult;
    pub fn CoCreateInstance(clsid: *const Guid, outer: *const c_void, ctx: u32, iid: *const Guid, out: *mut usize) -> Hresult;
    pub fn CoTaskMemFree(p: *const c_void);
    fn PropVariantClear(v: *mut PropVariant) -> Hresult;
}

/// Calls slot `$slot` of a COM object's vtable.
macro_rules! vcall {
    ($obj:expr, $slot:expr, fn($($t:ty),*) -> $r:ty $(, $a:expr)*) => {{
        let obj = $obj;
        let f: unsafe extern "system" fn(usize $(, $t)*) -> $r = unsafe { std::mem::transmute(*(*(obj as *const *const usize)).add($slot)) };
        unsafe { f(obj $(, $a)*) }
    }};
}
pub(crate) use vcall;

pub fn release(obj: usize) {
    if obj != 0 {
        vcall!(obj, 2, fn() -> u32);
    }
}

#[repr(C)]
pub struct PropVariant {
    pub vt: u16,
    pub reserved: [u16; 3],
    pub value: [u64; 2],
}

/// The calling thread in the multithreaded apartment (if it has none yet), and the
/// endpoint enumerator. Release it when done.
pub fn enumerator() -> Option<usize> {
    unsafe { CoInitializeEx(std::ptr::null(), 0) }; // COINIT_MULTITHREADED; S_FALSE if already
    let mut e = 0usize;
    let hr = unsafe { CoCreateInstance(&CLSID_MM_ENUMERATOR, std::ptr::null(), 1, &IID_MM_ENUMERATOR, &mut e) };
    (hr >= 0 && e != 0).then_some(e)
}

/// A string COM allocated, freed.
pub fn take_wide(p: *mut u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) });
    unsafe { CoTaskMemFree(p as *const c_void) };
    Some(s)
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// IMMDevice::GetId.
pub fn device_id(dev: usize) -> Option<String> {
    let mut p: *mut u16 = std::ptr::null_mut();
    if vcall!(dev, 5, fn(*mut *mut u16) -> Hresult, &mut p) < 0 {
        return None;
    }
    take_wide(p)
}

/// The device's name as Sound settings shows it (PKEY_Device_FriendlyName).
pub fn device_name(dev: usize) -> Option<String> {
    #[repr(C)]
    struct Key(Guid, u32);
    const FRIENDLY_NAME: Key = Key(Guid(0xA45C254E, 0xDF1C, 0x4EFD, [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0]), 14);
    let mut store = 0usize;
    if vcall!(dev, 4, fn(u32, *mut usize) -> Hresult, 0, &mut store) < 0 || store == 0 {
        return None;
    }
    let mut v = PropVariant { vt: 0, reserved: [0; 3], value: [0; 2] };
    let ok = vcall!(store, 5, fn(*const Key, *mut PropVariant) -> Hresult, &FRIENDLY_NAME, &mut v) >= 0;
    release(store);
    // VT_LPWSTR
    let name = if ok && v.vt == 31 && v.value[0] != 0 {
        let p = v.value[0] as usize as *const u16;
        let mut n = 0;
        while unsafe { *p.add(n) } != 0 {
            n += 1;
        }
        Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) }))
    } else {
        None
    };
    unsafe { PropVariantClear(&mut v) };
    name
}

/// The default endpoint of `flow` for `role`.
pub fn default_device(enumerator: usize, flow: u32, role: u32) -> Option<usize> {
    let mut def = 0usize;
    (vcall!(enumerator, 4, fn(u32, u32, *mut usize) -> Hresult, flow, role, &mut def) >= 0 && def != 0).then_some(def)
}

/// The active endpoints of `flow`, in the enumerator's order (the order the Voice Capture
/// DSP numbers them in). Release each.
pub fn active_devices(enumerator: usize, flow: u32) -> Vec<usize> {
    let mut coll = 0usize;
    // DEVICE_STATE_ACTIVE
    if vcall!(enumerator, 3, fn(u32, u32, *mut usize) -> Hresult, flow, 1, &mut coll) < 0 || coll == 0 {
        return Vec::new();
    }
    let mut count = 0u32;
    vcall!(coll, 3, fn(*mut u32) -> Hresult, &mut count);
    let mut out = Vec::new();
    for i in 0..count {
        let mut dev = 0usize;
        if vcall!(coll, 4, fn(u32, *mut usize) -> Hresult, i, &mut dev) >= 0 && dev != 0 {
            out.push(dev);
        }
    }
    release(coll);
    out
}

/// IMMDeviceEnumerator::GetDevice. Release it.
pub fn device_by_id(enumerator: usize, id: &str) -> Option<usize> {
    let w = wide(id);
    let mut dev = 0usize;
    (vcall!(enumerator, 5, fn(*const u16, *mut usize) -> Hresult, w.as_ptr(), &mut dev) >= 0 && dev != 0).then_some(dev)
}
