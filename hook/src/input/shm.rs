//! Shared-memory reader for `protocol::input_shm::InputHeader`.
//!
//! Maps either a named Win32 file mapping (`ShmBacking::WinMapping`) or a shared file
//! (`ShmBacking::FileMmap`, used by a Linux host with a game under Wine) read-only and
//! reads lock-free seqlock snapshots for `input::policy()`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use peerparty_protocol::input_shm::{INPUT_BLOCK_SIZE, InputHeader, InputMode, InputSnapshot};
use peerparty_protocol::transport::ShmBacking;

use crate::log;

struct View {
    base: usize,
    size: usize,
}

/// Mapped block: never unmapped during the process lifetime so input threads reading
/// without a lock never race with unmapping.
static VIEW: AtomicPtr<View> = AtomicPtr::new(std::ptr::null_mut());
static VIEW_NAME: Mutex<String> = Mutex::new(String::new());
static ATTACHED: AtomicBool = AtomicBool::new(false);
/// Last complete snapshot read, returned if the writer is mid-publish when a reader polls.
static LAST: Mutex<Option<InputSnapshot>> = Mutex::new(None);

/// Fast lock-free check: whether an input SHM block is mapped.
pub fn attached() -> bool {
    ATTACHED.load(Ordering::Acquire)
}

/// Opens `PEERPARTY_INPUT_SHM` if set in the process environment.
pub fn init_from_env() -> bool {
    let Ok(name) = std::env::var(peerparty_protocol::env::INPUT_SHM) else {
        return false;
    };
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    attach(name)
}

/// Opens and maps `name` (a `ShmBacking` string). Returns true if attached.
pub fn attach(name: &str) -> bool {
    let mut known = crate::sys::lock(&VIEW_NAME);
    if *known == name && ATTACHED.load(Ordering::Acquire) {
        return true;
    }
    match open(name) {
        Some((base, size)) => {
            VIEW.store(Box::into_raw(Box::new(View { base, size })), Ordering::Release);
            *known = name.to_string();
            ATTACHED.store(true, Ordering::Release);
            log!("input: mapped virtual input block {name} ({size} bytes)");
            true
        }
        None => {
            log!("input: couldn't map virtual input block {name}{}", crate::sys::wine::tag());
            false
        }
    }
}

/// Reads the current snapshot from the mapped input block, if attached.
/// In `PassThrough` or `BlockAll`, completes in a single atomic mode load.
pub fn read() -> Option<InputSnapshot> {
    if !attached() {
        return None;
    }
    let v = VIEW.load(Ordering::Acquire);
    if v.is_null() {
        return None;
    }
    let (base, size) = unsafe { ((*v).base, (*v).size) };
    // SAFETY: `open` validated alignment, size, and magic, and views stay mapped.
    let header = unsafe { InputHeader::from_raw(base as *const u8, size) }?;
    read_from(header)
}

/// Reads a snapshot from `header`, using `LAST` as a fallback if the seqlock is mid-write.
pub(super) fn read_from(header: &InputHeader) -> Option<InputSnapshot> {
    match header.mode() {
        InputMode::PassThrough => Some(InputSnapshot { mode: InputMode::PassThrough, ..Default::default() }),
        InputMode::BlockAll => Some(InputSnapshot { mode: InputMode::BlockAll, ..Default::default() }),
        InputMode::Virtual => {
            for _ in 0..3 {
                if let Some(snap) = header.read_stable() {
                    if let Ok(mut last) = LAST.try_lock() {
                        *last = Some(snap);
                    }
                    return Some(snap);
                }
                std::hint::spin_loop();
            }
            // Writer mid-update: keep the last complete virtual snapshot, or default Virtual.
            let fallback = LAST
                .try_lock()
                .ok()
                .and_then(|g| *g)
                .unwrap_or(InputSnapshot { mode: InputMode::Virtual, ..Default::default() });
            Some(fallback)
        }
    }
}

/// Maps an input SHM block read-only and verifies its size and `PPIN` magic.
fn open(name: &str) -> Option<(usize, usize)> {
    use windows_sys::Win32::System::Memory::{
        FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MapViewOfFile, OpenFileMappingW, VirtualQuery,
    };
    let (base, file_size) = match ShmBacking::parse(name) {
        Some(ShmBacking::FileMmap { path }) => map_file(&path)?,
        Some(ShmBacking::WinMapping { name: mapping }) => open_mapping(&mapping)?,
        None => {
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let map = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) };
            if map.is_null() {
                return None;
            }
            let view = unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0) };
            unsafe { windows_sys::Win32::Foundation::CloseHandle(map) };
            (view.Value as usize, usize::MAX)
        }
    };
    if base == 0 {
        return None;
    }
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let got = unsafe {
        VirtualQuery(base as *const _, &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>())
    };
    let size = if got == 0 { 0 } else { info.RegionSize.min(file_size) };
    // SAFETY: mapped and readable for `size` bytes.
    let header = unsafe { InputHeader::from_raw(base as *const u8, size) };
    if !header.is_some_and(InputHeader::valid_magic) {
        unsafe {
            windows_sys::Win32::System::Memory::UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: base as *mut _ },
            );
        }
        return None;
    }
    Some((base, size))
}

fn open_mapping(name: &str) -> Option<(usize, usize)> {
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, MapViewOfFile, OpenFileMappingW};
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let map = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) };
    if map.is_null() {
        return None;
    }
    let view = unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0) };
    unsafe { windows_sys::Win32::Foundation::CloseHandle(map) };
    Some((view.Value as usize, usize::MAX))
}

fn map_file(path: &str) -> Option<(usize, usize)> {
    let path = crate::sys::wine::windows_path(path);
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileSizeEx, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY};
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let file = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut len = 0i64;
    let sized = unsafe { GetFileSizeEx(file, &mut len) } != 0 && len >= INPUT_BLOCK_SIZE as i64;
    let map = if sized {
        unsafe { CreateFileMappingW(file, std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null()) }
    } else {
        std::ptr::null_mut()
    };
    unsafe { CloseHandle(file) };
    if map.is_null() {
        return None;
    }
    let view = unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0) };
    unsafe { CloseHandle(map) };
    Some((view.Value as usize, usize::try_from(len).unwrap_or(usize::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::input_shm::VirtualPad;
    use peerparty_protocol::transport::FileShm;

    #[test]
    fn file_input_block_maps_and_reads_snapshots() {
        let path = FileShm::path_in(&std::env::temp_dir(), std::process::id(), &[0x44, 0x22, 0x99, 0x01]);
        let _ = std::fs::remove_file(&path);
        let shm = FileShm::create(&path, INPUT_BLOCK_SIZE as u64).unwrap();
        let writer = InputHeader::new();
        writer.init();
        let want = InputSnapshot {
            mode: InputMode::Virtual,
            pad: VirtualPad {
                connected: true,
                buttons: 0x1000,
                left_trigger: 128,
                right_trigger: 255,
                thumb_lx: 12345,
                thumb_ly: -23456,
                thumb_rx: 500,
                thumb_ry: -500,
                packet_number: 7,
                ..Default::default()
            },
            ..Default::default()
        };
        writer.publish(&want);
        let bytes: Vec<u8> = (0..INPUT_BLOCK_SIZE / 4)
            .flat_map(|i| unsafe { (*(&writer as *const InputHeader as *const u32).add(i)).to_le_bytes() })
            .collect();
        {
            use std::io::Write;
            (&shm.file).write_all(&bytes).unwrap();
        }
        let backing = ShmBacking::FileMmap { path: path.display().to_string() }.to_env_string();
        let (base, size) = open(&backing).expect("file input block maps");
        assert_eq!(size, INPUT_BLOCK_SIZE);
        let reader = unsafe { InputHeader::from_raw(base as *const u8, size) }.unwrap();
        let got = read_from(reader).unwrap();
        assert_eq!(got.mode, InputMode::Virtual);
        assert_eq!(got.pad, want.pad);
        unsafe {
            windows_sys::Win32::System::Memory::UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: base as *mut _ },
            );
        }
        shm.remove().unwrap();
    }
}
