//! Calling COM objects through their vtables by slot number (windows-sys has the
//! functions but not the interfaces), plus what the present hooks share: GUIDs, a
//! hidden window for throwaway devices, and the HLSL compiler.

use std::ffi::c_void;

/// Calls method `slot` of COM object `obj` (stdcall, `this` first, on x86 and x64).
macro_rules! vcall {
    ($obj:expr, $slot:expr, fn($($t:ty),*) -> $r:ty $(, $arg:expr)* $(,)?) => {{
        let obj = $obj as usize;
        let vt = unsafe { *(obj as *const *const usize) };
        let f: unsafe extern "system" fn(usize $(, $t)*) -> $r = unsafe { std::mem::transmute(*vt.add($slot)) };
        unsafe { f(obj $(, $arg)*) }
    }};
}
pub(crate) use vcall;

/// The function at `slot` of `obj`'s vtable.
pub fn slot(obj: usize, slot: usize) -> usize {
    unsafe { *(*(obj as *const *const usize)).add(slot) }
}

pub fn release(obj: usize) {
    if obj != 0 {
        vcall!(obj, 2, fn() -> u32);
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

pub const IID_ID3D11DEVICE: Guid = Guid(0xdb6f6ddb, 0xac77, 0x4e88, [0x82, 0x53, 0x81, 0x9d, 0xf9, 0xbb, 0xf1, 0x40]);
pub const IID_ID3D11TEXTURE2D: Guid = Guid(0x6f15aaf2, 0xd208, 0x4e89, [0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c]);
pub const IID_ID3D11ON12DEVICE: Guid = Guid(0x85611e73, 0x70a9, 0x490e, [0x96, 0x14, 0xa9, 0xe3, 0x02, 0x77, 0x79, 0x04]);
pub const IID_ID3D12DEVICE: Guid = Guid(0x189819f1, 0x1db6, 0x4b57, [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7]);
pub const IID_ID3D12RESOURCE: Guid = Guid(0x696442be, 0xa72e, 0x4059, [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad]);
pub const IID_ID3D12COMMANDQUEUE: Guid = Guid(0x0ec870a6, 0x5d7e, 0x4c22, [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed]);
pub const IID_IDXGISWAPCHAIN3: Guid = Guid(0x94d99bdb, 0xf1f8, 0x4ab0, [0xb2, 0x36, 0x7d, 0xa0, 0x17, 0x0e, 0xda, 0xb1]);

/// QueryInterface; 0 when the object doesn't have that interface.
pub fn query(obj: usize, iid: &Guid) -> usize {
    let mut out = 0usize;
    let hr = vcall!(obj, 0, fn(*const Guid, *mut usize) -> i32, iid, &mut out);
    if hr < 0 { 0 } else { out }
}

pub fn module(name: &str) -> windows_sys::Win32::Foundation::HMODULE {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe { GetModuleHandleW(w.as_ptr()) }
}

pub fn load(name: &str) -> windows_sys::Win32::Foundation::HMODULE {
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
    let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe { LoadLibraryW(w.as_ptr()) }
}

pub fn proc(m: windows_sys::Win32::Foundation::HMODULE, name: &std::ffi::CStr) -> Option<usize> {
    if m.is_null() {
        return None;
    }
    unsafe { windows_sys::Win32::System::LibraryLoader::GetProcAddress(m, name.as_ptr().cast()) }.map(|f| f as usize)
}

/// A hidden window for a throwaway device (never shown); destroyed on drop.
pub struct HiddenWindow(pub windows_sys::Win32::Foundation::HWND);

impl HiddenWindow {
    pub fn new() -> Option<HiddenWindow> {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
        let h = unsafe {
            CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 64, 64, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null())
        };
        (!h.is_null()).then_some(HiddenWindow(h))
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0) };
    }
}

/// Compiles HLSL with Windows' own d3dcompiler_47.dll. Returns the bytecode.
pub fn compile(src: &str, entry: &std::ffi::CStr, target: &std::ffi::CStr) -> Option<Vec<u8>> {
    type D3DCompile = unsafe extern "system" fn(
        *const c_void,
        usize,
        *const i8,
        *const c_void,
        *const c_void,
        *const i8,
        *const i8,
        u32,
        u32,
        *mut usize,
        *mut usize,
    ) -> i32;
    let m = load("d3dcompiler_47.dll");
    let f: D3DCompile = unsafe { std::mem::transmute(proc(m, c"D3DCompile")?) };
    let (mut code, mut errors) = (0usize, 0usize);
    let hr = unsafe {
        f(src.as_ptr().cast(), src.len(), std::ptr::null(), std::ptr::null(), std::ptr::null(), entry.as_ptr(), target.as_ptr(), 0, 0, &mut code, &mut errors)
    };
    release(errors);
    if hr < 0 || code == 0 {
        crate::log!("present: shader {entry:?} didn't compile ({hr:#x})");
        return None;
    }
    // ID3DBlob: GetBufferPointer (3), GetBufferSize (4).
    let p = vcall!(code, 3, fn() -> *const u8);
    let n = vcall!(code, 4, fn() -> usize);
    let out = unsafe { std::slice::from_raw_parts(p, n) }.to_vec();
    release(code);
    Some(out)
}
