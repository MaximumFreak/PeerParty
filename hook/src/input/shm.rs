//! Shared-memory reader for `protocol::input_shm::InputHeader`.
//!
//! Maps either a named Win32 file mapping (`ShmBacking::WinMapping`) or a shared file
//! (`ShmBacking::FileMmap`, used by a Linux host with a game under Wine) read-only and
//! reads lock-free seqlock snapshots for `input::policy()`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use peerparty_protocol::input_shm::{CURSOR_CHANNEL_OFFSET, CURSOR_CHANNEL_SIZE, CursorChannel, INPUT_BLOCK_SIZE, InputHeader, InputMode, InputSnapshot};
use peerparty_protocol::transport::ShmBacking;

use crate::log;

struct View {
    base: usize,
    size: usize,
    writable: bool,
}

/// Mapped block: never unmapped during the process lifetime so input threads reading
/// without a lock never race with unmapping.
static VIEW: AtomicPtr<View> = AtomicPtr::new(std::ptr::null_mut());
static VIEW_NAME: Mutex<String> = Mutex::new(String::new());
static ATTACHED: AtomicBool = AtomicBool::new(false);
/// Last complete snapshot read, returned if the writer is mid-publish when a reader polls.
static LAST: Mutex<Option<InputSnapshot>> = Mutex::new(None);

thread_local! {
    /// This thread's last whole read: the block (its address: views are never unmapped,
    /// so a new block has another), its sequence and the snapshot.
    static SEEN: std::cell::Cell<Option<(usize, u32, InputSnapshot)>> = const { std::cell::Cell::new(None) };
}

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
        Some((base, size, writable)) => {
            VIEW.store(Box::into_raw(Box::new(View { base, size, writable })), Ordering::Release);
            *known = name.to_string();
            ATTACHED.store(true, Ordering::Release);
            log!("input: mapped virtual input block {name} ({size} bytes{})", if writable { ", rw" } else { ", ro" });
            super::cursor::on_shm_attached();
            true
        }
        None => {
            log!("input: couldn't map virtual input block {name}{}", crate::sys::wine::tag());
            false
        }
    }
}

/// The bidirectional cursor & coalesced-move channel (offset 256 of the 4 KiB block),
/// when the block is mapped read-write and large enough.
pub fn cursor_channel() -> Option<&'static CursorChannel> {
    if !attached() {
        return None;
    }
    let v = VIEW.load(Ordering::Acquire);
    if v.is_null() {
        return None;
    }
    let (base, size, writable) = unsafe { ((*v).base, (*v).size, (*v).writable) };
    if !writable || size < CURSOR_CHANNEL_OFFSET + CURSOR_CHANNEL_SIZE {
        return None;
    }
    // SAFETY: `open` validated the mapping and views stay mapped for the process lifetime.
    unsafe { CursorChannel::from_mapping(base as *const u8, size) }
}

/// The session compositor's HWND from the input block's pane projection (`0` when none).
pub fn compositor_hwnd() -> u64 {
    if !attached() {
        return 0;
    }
    let v = VIEW.load(Ordering::Acquire);
    if v.is_null() {
        return 0;
    }
    let (base, size) = unsafe { ((*v).base, (*v).size) };
    let Some(h) = (unsafe { InputHeader::from_raw(base as *const u8, size) }) else { return 0 };
    u64::from(h.pane.compositor_hwnd_lo.load(Ordering::Acquire)) | (u64::from(h.pane.compositor_hwnd_hi.load(Ordering::Acquire)) << 32)
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

/// How many times a read tries for a whole snapshot while the session writes one, and
/// after how many it lets other threads run (the writer may be waiting for this core).
const TRIES: u32 = 64;
const SPIN_FIRST: u32 = 16;

/// Reads a snapshot from `header`. While the session is writing one, the read waits a
/// moment for it; failing that, the last whole one read stands in. With none read yet
/// the copy's controllers are held still for that one call (`BlockAll`): never a made-up
/// snapshot with no controller, which every input layer would take as the copy's seat
/// being empty (and unplug its own controller for good).
pub(super) fn read_from(header: &InputHeader) -> Option<InputSnapshot> {
    match header.mode() {
        InputMode::PassThrough => Some(InputSnapshot { mode: InputMode::PassThrough, ..Default::default() }),
        InputMode::BlockAll => Some(InputSnapshot { mode: InputMode::BlockAll, ..Default::default() }),
        InputMode::Virtual => {
            // The session publishes at most a few hundred times a second; the game asks
            // far more often (every key, every API, every thread). The same sequence as
            // this thread's last whole read is that snapshot: one atomic load, no lock.
            let at = header as *const InputHeader as usize;
            let seq = header.seq.load(Ordering::Acquire);
            if seq & 1 == 0
                && let Some((h, s, snap)) = SEEN.try_with(std::cell::Cell::get).ok().flatten()
                && h == at
                && s == seq
            {
                return Some(snap);
            }
            for i in 0..TRIES {
                if let Some(snap) = header.read_stable() {
                    let _ = SEEN.try_with(|c| c.set(Some((at, snap.seq, snap))));
                    *crate::sys::lock(&LAST) = Some(snap);
                    return Some(snap);
                }
                if i < SPIN_FIRST {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
            let last = *crate::sys::lock(&LAST);
            crate::log::throttled("input: torn read", 1, |n| {
                log!("input: the session was writing this copy's input for {TRIES} tries ({n} time(s) so far): {}", if last.is_some() { "the last whole snapshot stands in" } else { "none read yet, its controllers held still for that read" });
            });
            Some(last.unwrap_or(InputSnapshot { mode: InputMode::BlockAll, ..Default::default() }))
        }
    }
}

/// Maps an input SHM block (read-write when allowed, so the hook can write `CursorChannel`
/// feedback, falling back to read-only) and verifies its size and `PPIN` magic.
fn open(name: &str) -> Option<(usize, usize, bool)> {
    use windows_sys::Win32::System::Memory::{
        FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION, MapViewOfFile, OpenFileMappingW, VirtualQuery,
    };
    let (base, file_size, writable) = match ShmBacking::parse(name) {
        Some(ShmBacking::FileMmap { path }) => map_file(&path)?,
        Some(ShmBacking::WinMapping { name: mapping }) => open_mapping(&mapping)?,
        None => {
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let (map, access, writable) = {
                let rw = unsafe { OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, 0, wide.as_ptr()) };
                if !rw.is_null() {
                    (rw, FILE_MAP_READ | FILE_MAP_WRITE, true)
                } else {
                    (unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) }, FILE_MAP_READ, false)
                }
            };
            if map.is_null() {
                return None;
            }
            let view = unsafe { MapViewOfFile(map, access, 0, 0, 0) };
            unsafe { windows_sys::Win32::Foundation::CloseHandle(map) };
            (view.Value as usize, usize::MAX, writable)
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
    Some((base, size, writable))
}

fn open_mapping(name: &str) -> Option<(usize, usize, bool)> {
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, OpenFileMappingW};
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let (map, access, writable) = {
        let rw = unsafe { OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, 0, wide.as_ptr()) };
        if !rw.is_null() {
            (rw, FILE_MAP_READ | FILE_MAP_WRITE, true)
        } else {
            (unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) }, FILE_MAP_READ, false)
        }
    };
    if map.is_null() {
        return None;
    }
    let view = unsafe { MapViewOfFile(map, access, 0, 0, 0) };
    unsafe { windows_sys::Win32::Foundation::CloseHandle(map) };
    Some((view.Value as usize, usize::MAX, writable))
}

fn map_file(path: &str) -> Option<(usize, usize, bool)> {
    let path = crate::sys::wine::windows_path(path);
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileSizeEx, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READONLY, PAGE_READWRITE};
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let (file, writable) = {
        let rw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if rw != INVALID_HANDLE_VALUE {
            (rw, true)
        } else {
            let ro = unsafe {
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
            (ro, false)
        }
    };
    if file == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut len = 0i64;
    let sized = unsafe { GetFileSizeEx(file, &mut len) } != 0 && len >= INPUT_BLOCK_SIZE as i64;
    let map = if sized {
        unsafe { CreateFileMappingW(file, std::ptr::null(), if writable { PAGE_READWRITE } else { PAGE_READONLY }, 0, 0, std::ptr::null()) }
    } else {
        std::ptr::null_mut()
    };
    unsafe { CloseHandle(file) };
    if map.is_null() {
        return None;
    }
    let view = unsafe { MapViewOfFile(map, if writable { FILE_MAP_READ | FILE_MAP_WRITE } else { FILE_MAP_READ }, 0, 0, 0) };
    unsafe { CloseHandle(map) };
    Some((view.Value as usize, usize::try_from(len).unwrap_or(usize::MAX), writable))
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
        let (base, size, _writable) = open(&backing).expect("file input block maps");
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

    /// While the session is writing (or keeps rewriting) a copy's input, a read is the
    /// last whole snapshot or controllers held still, never a Virtual one read torn: a
    /// half-written pad reads as unplugged, and the game would drop its player.
    #[test]
    fn contention_never_reads_a_torn_unplugged_pad() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let whole = |s: &InputSnapshot| s.mode == InputMode::BlockAll || (s.mode == InputMode::Virtual && s.pad.connected);
        // Stuck mid-write: the session's writer stopped between its two sequence bumps.
        let stuck = InputHeader::new();
        stuck.init();
        stuck.publish(&InputSnapshot { mode: InputMode::Virtual, pad: VirtualPad { connected: true, ..Default::default() }, ..Default::default() });
        stuck.seq.fetch_add(1, Ordering::AcqRel);
        for _ in 0..8 {
            let got = read_from(&stuck).unwrap();
            assert!(whole(&got), "stuck writer: {got:?}");
        }
        // A writer that never stops: every read is whole.
        let busy: &'static InputHeader = Box::leak(Box::new(InputHeader::new()));
        busy.init();
        busy.publish(&InputSnapshot { mode: InputMode::Virtual, pad: VirtualPad { connected: true, ..Default::default() }, ..Default::default() });
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let writer = std::thread::spawn(move || {
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                n = n.wrapping_add(1);
                busy.publish(&InputSnapshot { mode: InputMode::Virtual, pad: VirtualPad { connected: true, buttons: n, packet_number: n, ..Default::default() }, ..Default::default() });
            }
        });
        let until = std::time::Instant::now() + std::time::Duration::from_millis(300);
        let mut reads = 0;
        while std::time::Instant::now() < until {
            let got = read_from(busy).unwrap();
            assert!(whole(&got), "busy writer: {got:?}");
            reads += 1;
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert!(reads > 0);
    }
}
