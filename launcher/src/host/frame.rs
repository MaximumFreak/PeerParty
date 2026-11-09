//! The overlay's frame for drawing inside the game (exclusive fullscreen, where a window
//! over the game can't show): a named shared-memory block the hook's present.rs reads
//! and draws as one textured quad in the game's own frame.
//!
//! Layout (little-endian): a 256-byte header, then premultiplied BGRA pixels, `w * h`.
//!
//! | offset | field |
//! |---|---|
//! | 0 | magic `LBOV` |
//! | 4 | sequence: odd while being written, even when complete |
//! | 8 | visible (0 or 1) |
//! | 12 | x, 16 y (where the frame's top-left goes, in the game's client pixels) |
//! | 20 | w, 24 h |
//! | 28 | capacity in pixels |
//! | 32 | how many opaque rectangles follow (at most 8) |
//! | 36 | the rectangles: x, y, w, h each (the panel, notifications), for APIs that can
//! only copy, not blend (Vulkan) |

use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::*;

use super::raster::Canvas;

pub const HEADER: usize = 256;
pub const MAX_RECTS: usize = 8;
pub const MAGIC: u32 = u32::from_le_bytes(*b"LBOV");

pub struct Shared {
    pub name: String,
    map: HANDLE,
    view: *mut u8,
    capacity: usize,
    /// The size of the last frame written (a frame of another size is written whole).
    last: (usize, usize),
}

impl Shared {
    /// A block big enough for a 4K frame, named after the game's process.
    pub fn create(pid: u32) -> Option<Shared> {
        let capacity = 3840 * 2400;
        let name = format!("Local\\peerparty-frame-{pid}");
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let size = HEADER + capacity * 4;
        let map = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, (size >> 32) as u32, size as u32, wide.as_ptr())
        };
        if map.is_null() {
            return None;
        }
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            unsafe { CloseHandle(map) };
            return None;
        }
        let s = Shared { name, map, view: view.Value.cast(), capacity, last: (0, 0) };
        s.put(0, MAGIC);
        s.put(28, capacity as u32);
        Some(s)
    }

    fn word(&self, off: usize) -> &AtomicU32 {
        unsafe { &*(self.view.add(off) as *const AtomicU32) }
    }

    fn put(&self, off: usize, v: u32) {
        self.word(off).store(v, Ordering::Release);
    }

    /// Publishes a frame (clipped to the block's capacity), with its opaque parts. Only
    /// the `dirty` rectangles ([x0, y0, x1, y1]) changed since the last frame of the same
    /// size; None: all of it.
    pub fn write(&mut self, c: &Canvas, x: i32, y: i32, visible: bool, rects: &[[u32; 4]], dirty: Option<&[[i32; 4]]>) {
        let n = (c.w * c.h).min(self.capacity);
        let seq = self.word(4).load(Ordering::Acquire);
        self.put(4, seq.wrapping_add(1) | 1);
        let same = self.last == (c.w, c.h);
        match dirty.filter(|_| same) {
            Some(dirty) => {
                for r in dirty {
                    let [x0, y0, x1, y1] = super::raster::clamp_rect(*r, c.w, c.h);
                    for row in y0..y1 {
                        let at = row * c.w + x0;
                        if at + (x1 - x0) > n {
                            break;
                        }
                        unsafe { std::ptr::copy_nonoverlapping(c.px.as_ptr().add(at) as *const u8, self.view.add(HEADER + at * 4), (x1 - x0) * 4) };
                    }
                }
            }
            None => unsafe { std::ptr::copy_nonoverlapping(c.px.as_ptr() as *const u8, self.view.add(HEADER), n * 4) },
        }
        self.last = (c.w, c.h);
        self.put(12, x as u32);
        self.put(16, y as u32);
        self.put(20, c.w as u32);
        self.put(24, (n / c.w.max(1)) as u32);
        let n = rects.len().min(MAX_RECTS);
        self.put(32, n as u32);
        for (i, r) in rects.iter().take(n).enumerate() {
            for (k, v) in r.iter().enumerate() {
                self.put(36 + i * 16 + k * 4, *v);
            }
        }
        self.put(8, u32::from(visible));
        self.put(4, (seq.wrapping_add(2)) & !1);
    }

    pub fn hide(&mut self) {
        if self.word(8).load(Ordering::Acquire) != 0 {
            let seq = self.word(4).load(Ordering::Acquire);
            self.put(8, 0);
            self.put(4, (seq.wrapping_add(2)) & !1);
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            CloseHandle(self.map);
        }
    }
}
