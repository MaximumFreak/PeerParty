//! The little of COM the layers need: GUIDs and calls through an object's vtable.

/// A GUID, laid out as Windows' (Data1, Data2, Data3, Data4).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

impl Guid {
    pub fn same(&self, o: &Guid) -> bool {
        self == o
    }
}

pub const IID_UNKNOWN: Guid = Guid(0, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

/// Calls slot `$slot` of a COM object's vtable (stdcall on x86, the one convention on
/// x64): `vcall!(obj, 2, fn() -> u32)` is `IUnknown::Release`.
macro_rules! vcall {
    ($obj:expr, $slot:expr, fn($($t:ty),*) -> $r:ty $(, $a:expr)*) => {{
        let obj = $obj;
        let f: unsafe extern "system" fn(usize $(, $t)*) -> $r = unsafe { std::mem::transmute(*(*(obj as *const *const usize)).add($slot)) };
        unsafe { f(obj $(, $a)*) }
    }};
}
pub(crate) use vcall;

/// IUnknown::Release, for a pointer that may be null.
pub fn release(obj: usize) {
    if obj != 0 {
        vcall!(obj, 2, fn() -> u32);
    }
}
