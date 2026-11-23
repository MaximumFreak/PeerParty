//! What a split-screen session shares with its copies' hooks for their frames, whichever
//! way the frames come (`peerparty_protocol::frame_ring`):
//!
//! - `RingBlock`: each copy's ring block, which says which of its frames is newest and
//!   which the session's window is drawing. A named mapping on Windows; a file both sides
//!   map (`ShmBacking::FileMmap`) when the frames come as pixels, the only kind a game
//!   under Wine can reach a native Linux window with.
//! - `PixelFrames`: a copy's frames as pixels, in the file its hook writes them into
//!   (`SurfaceMsg::pixels`), mapped here to read.

use std::sync::Arc;

use peerparty_protocol::frame_ring::{BLOCK_SIZE, RingHeader, SLOTS, bytes_per_pixel, pixels_size, slot_bytes};
use peerparty_protocol::ipc::SurfaceMsg;
use peerparty_protocol::transport::{FileShm, ShmBacking};

/// Whether a session's copies send their frames as pixels, by `PEERPARTY_COMPOSITOR`
/// (`compositor`, the environment's or the game's setting): always off Windows (a native
/// window can't open Windows' shared textures), and on Windows when it is `pixels` (to try
/// that path there).
pub fn pixel_frames(compositor: Option<&str>) -> bool {
    cfg!(unix) || compositor.is_some_and(|v| v.trim().eq_ignore_ascii_case("pixels"))
}

/// A random suffix for the names nothing should guess.
fn token() -> String {
    peerparty_core::inet::crypto::random_array::<8>().iter().map(|b| format!("{b:02x}")).collect()
}

/// What keeps the block mapped (and its file, removed with it).
#[allow(dead_code)]
enum Keep {
    #[cfg(windows)]
    Mapping(windows_sys::Win32::Foundation::HANDLE),
    #[cfg(windows)]
    File(Option<FileShm>, windows_sys::Win32::Foundation::HANDLE),
    #[cfg(unix)]
    File(Option<FileShm>, crate::backend::linux::overlay::Mmap),
}

/// One copy's ring block.
pub struct RingBlock {
    /// What the hook is told (`HostMsg::Capture`'s `ring`).
    pub name: String,
    view: *mut u8,
    keep: Keep,
}

unsafe impl Send for RingBlock {}
unsafe impl Sync for RingBlock {}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

impl RingBlock {
    /// A fresh block for the copy with process id `pid`, in a named mapping.
    #[cfg(windows)]
    pub fn create(pid: u32) -> Option<Arc<RingBlock>> {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE};
        let name = format!(r"Local\peerparty-ring-{pid}-{}", token());
        let map = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, 4096, wide(&name).as_ptr()) };
        if map.is_null() {
            return None;
        }
        // ERROR_ALREADY_EXISTS: someone else's block of that name.
        if unsafe { GetLastError() } == 183 {
            unsafe { CloseHandle(map) };
            return None;
        }
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, 4096) }.Value as *mut u8;
        if view.is_null() {
            unsafe { CloseHandle(map) };
            return None;
        }
        let b = RingBlock { name, view, keep: Keep::Mapping(map) };
        b.header().init();
        Some(Arc::new(b))
    }

    /// A fresh block in a file of its own in `dir` (where the hook makes its pixel files
    /// too), for the copy with process id `pid`.
    pub fn create_file(dir: &std::path::Path, pid: u32) -> Option<Arc<RingBlock>> {
        let file = FileShm::create(&dir.join(format!("peerparty-ring-{pid}-{}", token())), 4096).ok()?;
        let name = ShmBacking::FileMmap { path: file.path.to_string_lossy().into_owned() }.to_env_string();
        #[cfg(windows)]
        let (view, keep) = {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE};
            let map = unsafe { CreateFileMappingW(file.file.as_raw_handle(), std::ptr::null(), PAGE_READWRITE, 0, 0, std::ptr::null()) };
            if map.is_null() {
                let _ = file.remove();
                return None;
            }
            let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, 4096) }.Value as *mut u8;
            if view.is_null() {
                unsafe { CloseHandle(map) };
                let _ = file.remove();
                return None;
            }
            (view, Keep::File(Some(file), map))
        };
        #[cfg(unix)]
        let (view, keep) = {
            let Ok(m) = crate::backend::linux::overlay::Mmap::shared(&file.file, 4096) else {
                let _ = file.remove();
                return None;
            };
            (m.as_ptr(), Keep::File(Some(file), m))
        };
        let b = RingBlock { name, view, keep };
        b.header().init();
        Some(Arc::new(b))
    }

    pub fn header(&self) -> &RingHeader {
        unsafe { RingHeader::from_raw(self.view, BLOCK_SIZE) }.expect("a page-aligned view")
    }

    /// Whether it's a file (the frames come as pixels).
    pub fn is_file(&self) -> bool {
        matches!(self.keep, Keep::File(..))
    }
}

impl Drop for RingBlock {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() }) };
            match &mut self.keep {
                Keep::Mapping(map) => unsafe {
                    CloseHandle(*map);
                },
                Keep::File(file, map) => {
                    unsafe { CloseHandle(*map) };
                    if let Some(f) = file.take() {
                        let _ = f.remove();
                    }
                }
            }
        }
        #[cfg(unix)]
        {
            let Keep::File(file, _) = &mut self.keep;
            if let Some(f) = file.take() {
                let _ = f.remove();
            }
        }
    }
}

/// A read-only mapping of a whole file.
struct ReadMap {
    ptr: *const u8,
    len: usize,
    #[cfg(windows)]
    map: windows_sys::Win32::Foundation::HANDLE,
}

impl ReadMap {
    fn open(path: &std::path::Path) -> Option<ReadMap> {
        // Opened sharing reads, writes and deletes: the hook writes it and deletes it
        // when it's done with it.
        let f = FileShm::open(path, false).ok()?;
        let len = usize::try_from(f.size).ok().filter(|l| *l > 0)?;
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY};
            let map = unsafe { CreateFileMappingW(f.file.as_raw_handle(), std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null()) };
            if map.is_null() {
                return None;
            }
            let ptr = unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, len) }.Value as *const u8;
            if ptr.is_null() {
                unsafe { CloseHandle(map) };
                return None;
            }
            Some(ReadMap { ptr, len, map })
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.file.as_raw_fd(), 0) };
            (ptr != libc::MAP_FAILED).then(|| ReadMap { ptr: ptr as *const u8, len })
        }
    }
}

impl Drop for ReadMap {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr as *mut _ });
            windows_sys::Win32::Foundation::CloseHandle(self.map);
        }
        #[cfg(unix)]
        unsafe {
            libc::munmap(self.ptr as *mut _, self.len);
        }
    }
}

/// One generation of a copy's frames as pixels.
pub struct PixelFrames {
    #[cfg_attr(windows, allow(dead_code))]
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    /// A typed `DXGI_FORMAT` (`frame_ring::bytes_per_pixel` knows it).
    pub format: u32,
    #[cfg_attr(windows, allow(dead_code))]
    pub color_space: u32,
    pub stride: u32,
    slot: usize,
    map: ReadMap,
}

unsafe impl Send for PixelFrames {}
unsafe impl Sync for PixelFrames {}

impl PixelFrames {
    /// The file a `surface` of pixels names, mapped. None when it isn't one, is gone, or
    /// is smaller than its frames.
    pub fn open(m: &SurfaceMsg) -> Option<PixelFrames> {
        let path = m.pixels.as_ref()?;
        let bpp = bytes_per_pixel(m.format)?;
        if m.width == 0 || m.height == 0 || u64::from(m.stride) < u64::from(m.width) * u64::from(bpp) {
            eprintln!("peerparty: session: a copy's pixel frames ({}x{}, stride {}) don't add up; its pane stays on its window", m.width, m.height, m.stride);
            return None;
        }
        let (Some(size), Some(slot)) = (pixels_size(m.stride, m.height), slot_bytes(m.stride, m.height)) else { return None };
        let map = ReadMap::open(std::path::Path::new(path))?;
        if map.len < size {
            eprintln!("peerparty: session: a copy's pixel file is {} bytes, smaller than its {size} bytes of frames", map.len);
            return None;
        }
        Some(PixelFrames {
            generation: m.generation,
            width: m.width,
            height: m.height,
            format: m.format,
            color_space: m.color_space,
            stride: m.stride,
            slot,
            map,
        })
    }

    /// Slot `i`'s rows (`stride` bytes each, `height` of them).
    pub fn slot(&self, i: usize) -> &[u8] {
        let i = i % SLOTS;
        unsafe { std::slice::from_raw_parts(self.map.ptr.add(i * self.slot), self.stride as usize * self.height as usize) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pixel file as a hook writes one (three slots, each a frame's rows), mapped
    /// through its `surface`; a ring block in a file the hook would map by its name.
    #[test]
    fn pixel_files_and_file_rings_map() {
        let dir = std::env::temp_dir().join(format!("peerparty-ring-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h, stride) = (4u32, 3u32, 16u32);
        let path = dir.join("pixels-1-2");
        let mut bytes = vec![0u8; pixels_size(stride, h).unwrap()];
        bytes[slot_bytes(stride, h).unwrap()] = 0xab;
        std::fs::write(&path, &bytes).unwrap();
        let m = SurfaceMsg {
            pid: 1,
            generation: 2,
            textures: Vec::new(),
            content: 0,
            release: 0,
            width: w,
            height: h,
            format: 87,
            api: "vulkan".into(),
            color_space: 0,
            pixels: Some(path.to_string_lossy().into_owned()),
            stride,
        };
        let p = PixelFrames::open(&m).expect("mapped");
        assert_eq!(p.slot(1)[0], 0xab);
        assert_eq!(p.slot(0).len(), (stride * h) as usize);
        assert!(PixelFrames::open(&SurfaceMsg { height: 4000, ..m.clone() }).is_none(), "a file too small for its frames");
        assert!(PixelFrames::open(&SurfaceMsg { width: 0, ..m.clone() }).is_none(), "an empty frame");
        assert!(PixelFrames::open(&SurfaceMsg { height: 0, ..m.clone() }).is_none(), "an empty frame");
        assert!(PixelFrames::open(&SurfaceMsg { pixels: None, ..m.clone() }).is_none());
        drop(p);

        let ring = RingBlock::create_file(&dir, 1).expect("a ring file");
        assert!(ring.is_file() && ring.header().valid());
        let Some(ShmBacking::FileMmap { path: rp }) = ShmBacking::parse(&ring.name) else { panic!("{}", ring.name) };
        assert!(std::path::Path::new(&rp).is_file());
        drop(ring);
        assert!(!std::path::Path::new(&rp).exists(), "removed with the block");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
