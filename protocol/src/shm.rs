//! The overlay's frame block: the launcher draws the overlay into it, and the hook's
//! present hooks (hook/src/present) draw it inside the game's own frames.
//!
//! Layout (little-endian): a 256-byte `FrameHeader`, then premultiplied BGRA pixels,
//! `w * h` of them, rows `w` pixels apart.
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
//! | 36 | the rectangles: x, y, w, h each (the panel, notifications), for APIs that can only copy, not blend (Vulkan) |
//! | 164 | flags (`FLAG_*`) |
//! | 168 | reserved, zero |
//!
//! Each word is read and written atomically. The writer makes the sequence odd, writes
//! the pixels and the words, then makes it even again; a reader that saw the same even
//! sequence before and after its copy has a whole frame.

use std::sync::atomic::{AtomicU32, Ordering, fence};

pub const HEADER_SIZE: usize = 256;
pub const MAX_RECTS: usize = 8;
pub const MAGIC: u32 = u32::from_le_bytes(*b"LBOV");
/// The least a block holds (a 1920x1200 frame); a bigger frame gets a bigger block.
pub const MIN_CAPACITY: usize = 1920 * 1200;
/// The most: 8K (7680x4320).
pub const MAX_CAPACITY: usize = 7680 * 4320;
/// Reserved for frames going the other way (a game's frame for a compositor).
pub const FLAG_COMPOSITOR_OUT: u32 = 1 << 0;

/// The block's first 256 bytes, as both sides see them.
#[repr(C)]
pub struct FrameHeader {
    pub magic: AtomicU32,
    pub seq: AtomicU32,
    pub visible: AtomicU32,
    /// An `i32`, as its bits.
    pub x: AtomicU32,
    pub y: AtomicU32,
    pub w: AtomicU32,
    pub h: AtomicU32,
    pub capacity: AtomicU32,
    pub rect_count: AtomicU32,
    pub rects: [[AtomicU32; 4]; MAX_RECTS],
    pub flags: AtomicU32,
    pub reserved: [AtomicU32; 22],
}

const _: () = assert!(std::mem::size_of::<FrameHeader>() == HEADER_SIZE);
const _: () = assert!(std::mem::align_of::<FrameHeader>() == 4);
const _: () = assert!(std::mem::offset_of!(FrameHeader, rects) == 36);
const _: () = assert!(std::mem::offset_of!(FrameHeader, flags) == 164);

/// A block's size in bytes for frames of up to `capacity` pixels (None if that
/// overflows).
pub fn block_size(capacity: usize) -> Option<usize> {
    capacity.checked_mul(4)?.checked_add(HEADER_SIZE)
}

/// A frame's size in pixels clamped to what a block may hold.
pub fn clamp_capacity(pixels: usize) -> usize {
    pixels.clamp(MIN_CAPACITY, MAX_CAPACITY)
}

/// Whether a `w` by `h` frame fits a block mapped at `mapped` bytes: the reader's
/// check, whatever the header says.
pub fn fits(w: u32, h: u32, mapped: usize) -> bool {
    let room = mapped.saturating_sub(HEADER_SIZE) as u64 / 4;
    w != 0 && h != 0 && u64::from(w) * u64::from(h) <= room
}

/// The header's words at one moment (one `FrameHeader::read`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FrameInfo {
    pub seq: u32,
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub capacity: u32,
    pub rect_count: u32,
    pub rects: [[u32; 4]; MAX_RECTS],
    pub flags: u32,
}

impl FrameInfo {
    /// Whether the writer was halfway through a frame (the sequence is odd).
    pub fn writing(&self) -> bool {
        self.seq & 1 == 1
    }

    /// The opaque rectangles, at most `MAX_RECTS`.
    pub fn opaque(&self) -> &[[u32; 4]] {
        &self.rects[..(self.rect_count as usize).min(MAX_RECTS)]
    }
}

/// What the writer publishes with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FrameWrite<'a> {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Opaque rectangles (x, y, w, h): only the first `MAX_RECTS` are kept.
    pub rects: &'a [[u32; 4]],
    pub flags: u32,
}

impl FrameHeader {
    /// A zeroed header, for a block that isn't shared memory (tests, one process).
    pub fn new() -> FrameHeader {
        // SAFETY: every field is an AtomicU32, for which all-zero bytes are valid.
        unsafe { std::mem::zeroed() }
    }

    /// The header at the start of a mapped block.
    ///
    /// # Safety
    /// `base` must point to at least `len` bytes that stay mapped, readable and (for a
    /// writer) writable for `'a`, and that other processes touch only through atomic
    /// word accesses (this layout). None when `len` is too short or `base` misaligned.
    pub unsafe fn from_raw<'a>(base: *const u8, len: usize) -> Option<&'a FrameHeader> {
        if base.is_null() || len < HEADER_SIZE || base.align_offset(std::mem::align_of::<FrameHeader>()) != 0 {
            return None;
        }
        // SAFETY: the caller's promise, and the checks above.
        Some(unsafe { &*(base as *const FrameHeader) })
    }

    /// The writer's start on a new block: its magic and capacity.
    pub fn init(&self, capacity: u32) {
        self.capacity.store(capacity, Ordering::Release);
        self.magic.store(MAGIC, Ordering::Release);
    }

    pub fn valid_magic(&self) -> bool {
        self.magic.load(Ordering::Acquire) == MAGIC
    }

    /// Whether the overlay shows: the one load a present makes while it doesn't.
    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire) != 0
    }

    /// Marks a frame as being written; returns the sequence to give `end_write`.
    pub fn begin_write(&self) -> u32 {
        let seq = self.seq.load(Ordering::Acquire);
        self.seq.store(seq.wrapping_add(1) | 1, Ordering::Release);
        fence(Ordering::Release);
        seq
    }

    /// Marks the frame begun at `seq` as complete.
    pub fn end_write(&self, seq: u32) {
        self.seq.store(seq.wrapping_add(2) & !1, Ordering::Release);
    }

    /// Writes the header's words for a frame. Call between `begin_write` and
    /// `end_write`, after the pixels.
    pub fn set_frame(&self, f: &FrameWrite<'_>) {
        self.x.store(f.x as u32, Ordering::Release);
        self.y.store(f.y as u32, Ordering::Release);
        self.w.store(f.w, Ordering::Release);
        self.h.store(f.h, Ordering::Release);
        let n = f.rects.len().min(MAX_RECTS);
        self.rect_count.store(n as u32, Ordering::Release);
        for (slot, r) in self.rects.iter().zip(f.rects.iter().take(n)) {
            for (word, v) in slot.iter().zip(r) {
                word.store(*v, Ordering::Release);
            }
        }
        self.flags.store(f.flags, Ordering::Release);
        self.visible.store(u32::from(f.visible), Ordering::Release);
    }

    /// Publishes a frame whose pixels `pixels` writes (it runs while the sequence is
    /// odd, so no reader takes a half-written frame).
    pub fn publish(&self, f: &FrameWrite<'_>, pixels: impl FnOnce()) {
        let seq = self.begin_write();
        pixels();
        self.set_frame(f);
        self.end_write(seq);
    }

    /// Hides the frame (nothing to write when it's hidden already).
    pub fn hide(&self) {
        if self.is_visible() {
            let seq = self.seq.load(Ordering::Acquire);
            self.visible.store(0, Ordering::Release);
            self.end_write(seq);
        }
    }

    /// The header's words now (`writing` says whether they're mid-change).
    pub fn read(&self) -> FrameInfo {
        let seq = self.seq.load(Ordering::Acquire);
        let mut rects = [[0u32; 4]; MAX_RECTS];
        for (out, slot) in rects.iter_mut().zip(&self.rects) {
            for (v, word) in out.iter_mut().zip(slot) {
                *v = word.load(Ordering::Acquire);
            }
        }
        FrameInfo {
            seq,
            visible: self.visible.load(Ordering::Acquire) != 0,
            x: self.x.load(Ordering::Acquire) as i32,
            y: self.y.load(Ordering::Acquire) as i32,
            w: self.w.load(Ordering::Acquire),
            h: self.h.load(Ordering::Acquire),
            capacity: self.capacity.load(Ordering::Acquire),
            rect_count: self.rect_count.load(Ordering::Acquire),
            rects,
            flags: self.flags.load(Ordering::Acquire),
        }
    }

    /// Whether nothing was written since `info` was read: a copy taken in between is
    /// whole.
    pub fn unchanged_since(&self, info: &FrameInfo) -> bool {
        fence(Ordering::Acquire);
        !info.writing() && self.seq.load(Ordering::Acquire) == info.seq
    }

    /// Reads a complete frame: `copy` takes its header and copies the pixels out; the
    /// result counts only if the writer didn't touch the block meanwhile. None while
    /// it's being written, or when it was rewritten during the copy (keep the last).
    pub fn read_stable<R>(&self, copy: impl FnOnce(&FrameInfo) -> R) -> Option<R> {
        let info = self.read();
        if info.writing() {
            return None;
        }
        let out = copy(&info);
        self.unchanged_since(&info).then_some(out)
    }
}

impl Default for FrameHeader {
    fn default() -> FrameHeader {
        FrameHeader::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_is_the_one_both_sides_used() {
        assert_eq!(std::mem::size_of::<FrameHeader>(), 256);
        let h = FrameHeader::new();
        let base = &h as *const FrameHeader as usize;
        let at = |p: &AtomicU32| p as *const AtomicU32 as usize - base;
        assert_eq!([at(&h.magic), at(&h.seq), at(&h.visible), at(&h.x), at(&h.y), at(&h.w), at(&h.h), at(&h.capacity), at(&h.rect_count)], [0, 4, 8, 12, 16, 20, 24, 28, 32]);
        assert_eq!(at(&h.rects[1][2]), 36 + 16 + 8);
        assert_eq!(at(&h.flags), 164);
        assert_eq!(MAGIC.to_le_bytes(), *b"LBOV");
        assert_eq!(block_size(MIN_CAPACITY), Some(256 + 1920 * 1200 * 4));
        assert_eq!(block_size(usize::MAX), None);
        assert_eq!(clamp_capacity(10), MIN_CAPACITY);
        assert_eq!(clamp_capacity(usize::MAX), MAX_CAPACITY);
        assert!(fits(1920, 1200, block_size(MIN_CAPACITY).unwrap()));
        assert!(!fits(1920, 1201, block_size(MIN_CAPACITY).unwrap()));
        assert!(!fits(0, 10, 1 << 20));
        assert!(!fits(u32::MAX, u32::MAX, usize::MAX));
        assert!(!fits(1, 1, 10), "a block shorter than its header");
    }

    /// The words as the launcher wrote them: magic and capacity, then a frame between
    /// an odd and an even sequence; hiding is one step.
    #[test]
    fn frames_publish_and_hide() {
        let h = FrameHeader::new();
        assert!(!h.valid_magic());
        h.init(MIN_CAPACITY as u32);
        assert!(h.valid_magic());
        let rects: Vec<[u32; 4]> = (0..10).map(|i| [i, i + 1, i + 2, i + 3]).collect();
        let mut seen_odd = false;
        h.publish(&FrameWrite { visible: true, x: -5, y: 7, w: 640, h: 480, rects: &rects, flags: FLAG_COMPOSITOR_OUT }, || {
            seen_odd = h.read().writing();
        });
        assert!(seen_odd, "odd while the pixels are written");
        let f = h.read();
        assert_eq!((f.seq, f.visible, f.x, f.y, f.w, f.h, f.capacity, f.flags), (2, true, -5, 7, 640, 480, MIN_CAPACITY as u32, FLAG_COMPOSITOR_OUT));
        assert_eq!(f.opaque().len(), MAX_RECTS, "the rest are dropped");
        assert_eq!(f.opaque()[7], [7, 8, 9, 10]);
        h.hide();
        let f = h.read();
        assert_eq!((f.seq, f.visible), (4, false));
        h.hide();
        assert_eq!(h.read().seq, 4, "nothing to hide");
    }

    #[test]
    fn a_frame_rewritten_during_its_copy_is_not_taken() {
        let h = FrameHeader::new();
        h.init(1);
        let frame = FrameWrite { visible: true, w: 1, h: 1, ..Default::default() };
        h.publish(&frame, || {});
        assert_eq!(h.read_stable(|f| f.w), Some(1));
        // The writer starts another frame during the copy.
        assert_eq!(h.read_stable(|_| h.publish(&frame, || {})), None);
        // Or is halfway through when the reader looks.
        let seq = h.begin_write();
        assert_eq!(h.read_stable(|_| ()), None);
        h.end_write(seq);
        assert_eq!(h.read_stable(|f| f.seq), Some(seq + 2));
        // The sequence wraps without ever reading as complete mid-write.
        h.seq.store(u32::MAX - 1, Ordering::Relaxed);
        let seq = h.begin_write();
        assert!(h.read().writing());
        h.end_write(seq);
        assert!(!h.read().writing());
    }

    #[test]
    fn a_header_over_raw_memory() {
        let words: Vec<AtomicU32> = (0..64 + 4).map(|_| AtomicU32::new(0)).collect();
        let base = words.as_ptr() as *const u8;
        // SAFETY: the vector lives through the test, and only atomics touch it.
        let h = unsafe { FrameHeader::from_raw(base, words.len() * 4) }.unwrap();
        h.init(4);
        assert_eq!(words[0].load(Ordering::Acquire), MAGIC);
        assert_eq!(words[7].load(Ordering::Acquire), 4, "capacity at 28");
        assert!(unsafe { FrameHeader::from_raw(base, 255) }.is_none(), "too short");
        assert!(unsafe { FrameHeader::from_raw(base.wrapping_add(1), 512) }.is_none(), "misaligned");
        assert!(unsafe { FrameHeader::from_raw(std::ptr::null(), 512) }.is_none());
        // A writer and a reader on two threads: the reader only ever takes whole frames.
        let shared = std::sync::Arc::new(FrameHeader::new());
        shared.init(2);
        let pixels = std::sync::Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);
        let (w, p) = (shared.clone(), pixels.clone());
        let writer = std::thread::spawn(move || {
            for i in 1..=20_000u32 {
                w.publish(&FrameWrite { visible: true, w: i, h: 1, ..Default::default() }, || {
                    p[0].store(i, Ordering::Relaxed);
                    p[1].store(i, Ordering::Relaxed);
                });
            }
        });
        let read = || shared.read_stable(|f| (pixels[0].load(Ordering::Relaxed), pixels[1].load(Ordering::Relaxed), f.w));
        while !writer.is_finished() {
            if let Some((a, b, w)) = read() {
                assert_eq!((a, b), (w, w), "a frame from one write");
            }
        }
        writer.join().unwrap();
        assert_eq!(read(), Some((20_000, 20_000, 20_000)));
    }
}
