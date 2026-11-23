//! The first things peerparty.exe does: where DLLs may come from, and the console it
//! was started from.

use std::ptr;

use super::wide;

/// Windows looks for a DLL beside the exe before it looks in System32, and peerparty.exe
/// is run from wherever it was downloaded to: a file named like a system DLL, dropped
/// into Downloads by any web page, would be loaded into it, and into the copy of it
/// that "Fix firewall" starts as administrator. Nothing peerparty.exe needs is beside
/// it (it is one file), so the folder is taken out of the search, first thing.
///
/// That covers what is loaded from here on. What Windows loads before `main` is the
/// linker's part (build.rs): the DLLs the exe names that Windows doesn't always take
/// from System32 are loaded on first use instead, which is after this, and so are the
/// DLLs those bring along. tools/dll-search-check.ps1 shows whether it holds.
pub fn only_system_dlls() {
    use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LOAD_LIBRARY_SEARCH_USER_DIRS, SetDefaultDllDirectories};
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_USER_DIRS) };
}

/// Messages go to the console PeerParty was started from, when it has no output of its
/// own (a GUI-subsystem program gets none); redirected output is left alone. Redirected
/// or not, PeerParty joins that console, so a console program it starts shares it
/// instead of opening a window of its own.
pub fn attach_console() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    unsafe {
        let has = |which| {
            let h = GetStdHandle(which);
            !h.is_null() && h != INVALID_HANDLE_VALUE
        };
        let redirected = has(STD_ERROR_HANDLE);
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 || redirected {
            return;
        }
        let out = CreateFileW(
            wide("CONOUT$").as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        );
        if out != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_ERROR_HANDLE, out);
            if !has(STD_OUTPUT_HANDLE) {
                SetStdHandle(STD_OUTPUT_HANDLE, out);
            }
        }
    }
}
