//! The overlay's frame for drawing inside the game (exclusive fullscreen, where a window
//! over the game can't show): a shared-memory block the hook's present.rs reads and
//! draws as one textured quad in the game's own frame.
//!
//! Its layout and sequence lock are `peerparty_protocol::shm`'s (a 256-byte
//! `FrameHeader`, then premultiplied BGRA pixels). Where it is (`ShmBacking`): a named
//! mapping (`create`), or a file both sides map (`create_file`), which a Linux launcher
//! and a game under Wine can share. The hook gets the backing's string in the `frame`
//! command. On Linux only the file is there (`mmap`ed shared, backend/linux/overlay.rs):
//! its name is the Unix path, which the hook turns into Wine's.

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::path::Path;

#[cfg(windows)]
use peerparty_protocol::shm::block_size;
use peerparty_protocol::shm::{FrameHeader, FrameWrite, HEADER_SIZE, clamp_capacity};
#[cfg(windows)]
use peerparty_protocol::transport::frame_mapping_name;
use peerparty_protocol::transport::{FileShm, ShmBacking};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::System::Memory::*;

use super::raster::Canvas;

pub struct Shared {
    /// Where the hook finds it: a `ShmBacking` string.
    pub name: String,
    #[cfg(windows)]
    map: HANDLE,
    /// The mapping itself (its view is `view`), held for as long as this is.
    #[cfg(unix)]
    #[allow(dead_code)]
    map: crate::backend::linux::overlay::Mmap,
    view: *mut u8,
    capacity: usize,
    /// The size of the last frame written (a frame of another size is written whole).
    last: (usize, usize),
    /// The file behind a `create_file` block, deleted with it.
    file: Option<FileShm>,
}

impl Shared {
    /// A block for frames of up to `pixels` (within MIN_CAPACITY..MAX_CAPACITY), named
    /// after the game's process and random digits, so nothing can open it by guessing
    /// or make it first: one that already exists is refused.
    #[cfg(windows)]
    pub fn create(pid: u32, pixels: usize) -> Option<Shared> {
        let capacity = clamp_capacity(pixels);
        let name = frame_mapping_name(pid, &peerparty_core::inet::crypto::random_array::<8>());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let size = block_size(capacity)?;
        let map = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, (size >> 32) as u32, size as u32, wide.as_ptr())
        };
        if map.is_null() {
            return None;
        }
        // ERROR_ALREADY_EXISTS: someone else's block of that name.
        if unsafe { windows_sys::Win32::Foundation::GetLastError() } == 183 {
            unsafe { CloseHandle(map) };
            return None;
        }
        Shared::map(ShmBacking::WinMapping { name }, map, size, capacity, None)
    }

    /// The same block in a file of its own in `dir` (`FileMmap`), named after the game's
    /// process and random digits; deleted when this is dropped.
    #[cfg(windows)]
    pub fn create_file(dir: &Path, pid: u32, pixels: usize) -> Option<Shared> {
        let capacity = clamp_capacity(pixels);
        let size = block_size(capacity)?;
        let file = FileShm::create(&FileShm::path_in(dir, pid, &peerparty_core::inet::crypto::random_array::<8>()), size as u64).ok()?;
        let map = unsafe { CreateFileMappingW(file.file.as_raw_handle(), std::ptr::null(), PAGE_READWRITE, 0, 0, std::ptr::null()) };
        let backing = ShmBacking::FileMmap { path: file.path.to_string_lossy().into_owned() };
        if map.is_null() {
            let _ = file.remove();
            return None;
        }
        Shared::map(backing, map, size, capacity, Some(file))
    }

    /// The same, mapped with `mmap` (the file is all there is off Windows).
    #[cfg(unix)]
    pub fn create_file(dir: &Path, pid: u32, pixels: usize) -> Option<Shared> {
        let capacity = clamp_capacity(pixels);
        let size = peerparty_protocol::shm::block_size(capacity)?;
        let file = FileShm::create(&FileShm::path_in(dir, pid, &peerparty_core::inet::crypto::random_array::<8>()), size as u64).ok()?;
        let map = match crate::backend::linux::overlay::Mmap::shared(&file.file, size) {
            Ok(m) => m,
            Err(_) => {
                let _ = file.remove();
                return None;
            }
        };
        let name = ShmBacking::FileMmap { path: file.path.to_string_lossy().into_owned() }.to_env_string();
        let s = Shared { name, view: map.as_ptr(), map, capacity, last: (0, 0), file: Some(file) };
        s.header().init(capacity as u32);
        Some(s)
    }

    #[cfg(windows)]
    fn map(backing: ShmBacking, map: HANDLE, size: usize, capacity: usize, file: Option<FileShm>) -> Option<Shared> {
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            unsafe { CloseHandle(map) };
            if let Some(f) = file {
                let _ = f.remove();
            }
            return None;
        }
        let s = Shared { name: backing.to_env_string(), map, view: view.Value.cast(), capacity, last: (0, 0), file };
        s.header().init(capacity as u32);
        Some(s)
    }

    /// The block's header. A view starts on a page, so it's aligned.
    fn header(&self) -> &FrameHeader {
        // SAFETY: the view is mapped (read and write) for as long as self, and holds
        // HEADER_SIZE bytes and more; the hook touches the header only through its words.
        unsafe { FrameHeader::from_raw(self.view, HEADER_SIZE) }.expect("a mapped view is aligned and long enough")
    }

    /// Whether a frame of `pixels` fits.
    pub fn fits(&self, pixels: usize) -> bool {
        pixels <= self.capacity
    }

    /// Publishes a frame (clipped to the block's capacity), with its opaque parts. Only
    /// the `dirty` rectangles ([x0, y0, x1, y1]) changed since the last frame of the same
    /// size; None: all of it.
    pub fn write(&mut self, c: &Canvas, x: i32, y: i32, visible: bool, rects: &[[u32; 4]], dirty: Option<&[[i32; 4]]>) {
        let n = (c.w * c.h).min(self.capacity);
        let same = self.last == (c.w, c.h);
        let view = self.view;
        let frame = FrameWrite { visible, x, y, w: c.w as u32, h: (n / c.w.max(1)) as u32, rects, flags: 0 };
        self.header().publish(&frame, || match dirty.filter(|_| same) {
            Some(dirty) => {
                for r in dirty {
                    let [x0, y0, x1, y1] = super::raster::clamp_rect(*r, c.w, c.h);
                    for row in y0..y1 {
                        let at = row * c.w + x0;
                        if at + (x1 - x0) > n {
                            break;
                        }
                        unsafe { std::ptr::copy_nonoverlapping(c.px.as_ptr().add(at) as *const u8, view.add(HEADER_SIZE + at * 4), (x1 - x0) * 4) };
                    }
                }
            }
            None => unsafe { std::ptr::copy_nonoverlapping(c.px.as_ptr() as *const u8, view.add(HEADER_SIZE), n * 4) },
        });
        self.last = (c.w, c.h);
    }

    pub fn hide(&mut self) {
        self.header().hide();
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Off Windows the mapping goes with `map`, after this.
        #[cfg(windows)]
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            CloseHandle(self.map);
        }
        if let Some(f) = self.file.take() {
            let _ = f.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::shm::{MAGIC, MAX_RECTS, MIN_CAPACITY, block_size};

    fn canvas(w: usize, h: usize) -> Canvas {
        let mut c = Canvas::new(w, h);
        for (i, p) in c.px.iter_mut().enumerate() {
            *p = 0xff00_0000 | i as u32;
        }
        c
    }

    /// A frame written is the one the hook reads: header words and pixels, whole frames
    /// only, and hiding is one step.
    fn round_trip(s: &mut Shared) {
        let h = s.header();
        assert!(h.valid_magic());
        assert_eq!(h.read().capacity, MIN_CAPACITY as u32);
        assert!(s.fits(MIN_CAPACITY) && !s.fits(MIN_CAPACITY + 1));
        let c = canvas(64, 32);
        let rects: Vec<[u32; 4]> = (0..10).map(|i| [i, i, 4, 4]).collect();
        s.write(&c, -3, 9, true, &rects, None);
        let f = s.header().read_stable(|f| *f).expect("a whole frame");
        assert_eq!((f.visible, f.x, f.y, f.w, f.h, f.seq), (true, -3, 9, 64, 32, 2));
        assert_eq!(f.opaque().len(), MAX_RECTS);
        let px = unsafe { std::slice::from_raw_parts(s.view.add(HEADER_SIZE) as *const u32, 64 * 32) };
        assert_eq!(px, &c.px[..]);
        // A dirty rectangle of the same size: only it is copied.
        let mut c2 = canvas(64, 32);
        c2.px.iter_mut().for_each(|p| *p = 7);
        s.write(&c2, 0, 0, true, &[], Some(&[[0, 0, 2, 1]]));
        let px = unsafe { std::slice::from_raw_parts(s.view.add(HEADER_SIZE) as *const u32, 64 * 32) };
        assert_eq!((px[0], px[1], px[2]), (7, 7, c.px[2]));
        assert_eq!(s.header().read().opaque().len(), 0);
        s.hide();
        let f = s.header().read();
        assert_eq!((f.visible, f.seq), (false, 6));
    }

    #[cfg(windows)]
    #[test]
    fn a_named_block_round_trips() {
        let mut s = Shared::create(std::process::id(), 10).expect("a block");
        assert!(matches!(ShmBacking::parse(&s.name), Some(ShmBacking::WinMapping { .. })), "{}", s.name);
        round_trip(&mut s);
    }

    /// The file a Linux launcher would share with a game under Wine: the hook reads the
    /// same bytes from it, and it goes when the block does.
    #[test]
    fn a_file_block_round_trips_and_goes() {
        let dir = std::env::temp_dir();
        let mut s = Shared::create_file(&dir, std::process::id(), 10).expect("a file block");
        let Some(ShmBacking::FileMmap { path }) = ShmBacking::parse(&s.name) else { panic!("{}", s.name) };
        round_trip(&mut s);
        let opened = FileShm::open(Path::new(&path), false).unwrap();
        assert_eq!(opened.size, block_size(MIN_CAPACITY).unwrap() as u64);
        drop(opened);
        let mut head = [0u8; 4];
        std::io::Read::read_exact(&mut std::fs::File::open(&path).unwrap(), &mut head).unwrap();
        assert_eq!(u32::from_le_bytes(head), MAGIC);
        drop(s);
        assert!(!Path::new(&path).exists());
    }
}
