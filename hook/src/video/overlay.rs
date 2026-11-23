//! Inbound overlay frame block (`protocol::shm::FrameHeader`): maps the launcher's
//! shared-memory overlay pixels and reads lock-free complete frames for the graphics
//! backends to composite onto the game's back buffer.

use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

use peerparty_protocol::shm::{FrameHeader, HEADER_SIZE, fits};
use peerparty_protocol::transport::ShmBacking;

use crate::log;

/// The mapped frame block, null until `attach`: its base and size together, so a reader
/// never pairs one block's base with another's size. Replaced blocks stay mapped so
/// concurrent present threads never dangle.
static VIEW: AtomicPtr<View> = AtomicPtr::new(std::ptr::null_mut());
static VIEW_NAME: Mutex<String> = Mutex::new(String::new());

struct View {
    base: usize,
    size: usize,
}

fn view() -> (usize, usize) {
    let v = VIEW.load(Ordering::Acquire);
    if v.is_null() { (0, 0) } else { unsafe { ((*v).base, (*v).size) } }
}

/// Opens and maps `name` (a `ShmBacking` string or Win32 mapping name). Returns true if
/// mapped (or already mapped under `name`).
pub fn attach(name: &str) -> bool {
    let mut known = VIEW_NAME.lock().unwrap();
    if *known == name && !VIEW.load(Ordering::Acquire).is_null() {
        return true;
    }
    match open(name) {
        Some((base, size)) => {
            VIEW.store(Box::into_raw(Box::new(View { base, size })), Ordering::Release);
            *known = name.to_string();
            true
        }
        None => {
            log!("video: couldn't open the overlay's frame {name}{}", crate::sys::wine::tag());
            false
        }
    }
}

/// Maps a frame block, a named mapping or a file (`ShmBacking`): its base and its size as
/// mapped, so a frame can't be read past the block's end whatever its header says.
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
    let got = unsafe { VirtualQuery(base as *const _, &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
    let size = if got == 0 { 0 } else { info.RegionSize.min(file_size) };
    // SAFETY: mapped and readable for `size` bytes, and never unmapped (see VIEW).
    let header = unsafe { FrameHeader::from_raw(base as *const u8, size) };
    if !header.is_some_and(FrameHeader::valid_magic) {
        return None;
    }
    log!("video: overlay frame {name} mapped ({size} bytes)");
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
    let path: &str = &path;
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
    let sized = unsafe { GetFileSizeEx(file, &mut len) } != 0 && len >= HEADER_SIZE as i64;
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

fn header() -> Option<(&'static FrameHeader, usize)> {
    let (base, size) = view();
    let h = unsafe { FrameHeader::from_raw(base as *const u8, size) }?;
    Some((h, size))
}

/// A complete frame to draw: its sequence number, size and pixels (premultiplied BGRA).
pub struct Frame {
    pub seq: u32,
    pub w: u32,
    pub h: u32,
    pub px: *const u32,
    /// The block it is in (None: pixels that aren't in shared memory, a test's).
    pub(super) header: Option<&'static FrameHeader>,
}

/// The current frame if the overlay (or a notification) is showing, else None.
pub fn frame() -> Option<Frame> {
    let (header, size) = header()?;
    if !header.is_visible() {
        return None;
    }
    let info = header.read();
    if !fits(info.w, info.h, size) {
        return None;
    }
    let px = (header as *const FrameHeader as usize + HEADER_SIZE) as *const u32;
    Some(Frame { seq: info.seq, w: info.w, h: info.h, px, header: Some(header) })
}

impl Frame {
    /// Whether the launcher is writing this frame right now (its sequence is odd).
    pub fn writing(&self) -> bool {
        self.seq & 1 == 1
    }

    /// Copies the frame out of shared memory and checks afterwards that the launcher
    /// didn't rewrite it meanwhile.
    pub fn stable(&self) -> Option<std::sync::MutexGuard<'static, Vec<u32>>> {
        static COPY: Mutex<Vec<u32>> = Mutex::new(Vec::new());
        if self.writing() {
            return None;
        }
        let n = self.w as usize * self.h as usize;
        let mut copy = COPY.lock().unwrap_or_else(|e| e.into_inner());
        copy.clear();
        copy.reserve(n);
        unsafe {
            std::ptr::copy_nonoverlapping(self.px, copy.as_mut_ptr(), n);
            copy.set_len(n);
        }
        let unchanged = self.header.is_none_or(|h| {
            std::sync::atomic::fence(Ordering::Acquire);
            h.seq.load(Ordering::Acquire) == self.seq
        });
        unchanged.then_some(copy)
    }

    /// Copies the frame into `dst` (rows `pitch` bytes apart) if read whole (`stable`).
    ///
    /// # Safety
    /// `dst` must hold `h` rows of `pitch` bytes, `pitch >= w * 4`.
    pub unsafe fn copy_to(&self, dst: *mut u8, pitch: usize) -> bool {
        let Some(px) = self.stable() else { return false };
        unsafe { self.write_rows(&px, dst, pitch) };
        true
    }

    /// Writes a `stable` copy of this frame into `dst` (rows `pitch` bytes apart). For a
    /// texture mapped with DISCARD, take the copy first: a mapping left unwritten holds
    /// garbage, which would be drawn.
    ///
    /// # Safety
    /// As `copy_to`; `px` must be this frame's `stable` copy.
    pub unsafe fn write_rows(&self, px: &[u32], dst: *mut u8, pitch: usize) {
        let row = self.w as usize * 4;
        if px.len() < self.w as usize * self.h as usize {
            return;
        }
        for y in 0..self.h as usize {
            unsafe { std::ptr::copy_nonoverlapping((px.as_ptr() as *const u8).add(y * row), dst.add(y * pitch), row) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::shm::{FrameWrite, block_size};
    use peerparty_protocol::transport::FileShm;

    #[test]
    fn a_file_block_maps_at_its_size() {
        let path = FileShm::path_in(&std::env::temp_dir(), std::process::id(), &[0x5a, 0x11, 0x77, 0x02]);
        let _ = std::fs::remove_file(&path);
        let (w, h) = (4u32, 2u32);
        let size = block_size((w * h) as usize).unwrap();
        let shm = FileShm::create(&path, size as u64).unwrap();
        let writer = FrameHeader::new();
        writer.init(w * h);
        writer.publish(&FrameWrite { visible: true, w, h, ..Default::default() }, || {});
        let header: Vec<u8> = (0..HEADER_SIZE / 4)
            .flat_map(|i| unsafe { (*(&writer as *const FrameHeader as *const u32).add(i)).to_le_bytes() })
            .collect();
        {
            use std::io::Write;
            (&shm.file).write_all(&header).unwrap();
        }
        let name = ShmBacking::FileMmap { path: path.display().to_string() }.to_env_string();
        let (base, mapped) = open(&name).expect("the file block maps");
        assert_eq!(mapped, size, "the file's bytes, not the view's whole pages");
        let read = unsafe { FrameHeader::from_raw(base as *const u8, mapped) }.unwrap().read();
        assert!(read.visible && !read.writing());
        assert_eq!((read.w, read.h), (w, h));
        assert!(fits(read.w, read.h, mapped) && !fits(w, h + 1, mapped));
        assert_eq!(open(&format!("file:{}.missing", path.display())), None);
        unsafe {
            windows_sys::Win32::System::Memory::UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: base as *mut _ },
            )
        };
        shm.remove().unwrap();
    }
}
