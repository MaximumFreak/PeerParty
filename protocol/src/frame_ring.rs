//! A split-screen copy's frames for the session's compositor: the hook copies each frame
//! the game presents into one of `SLOTS` shared GPU textures, and the session window draws
//! the newest finished one in the copy's pane. This block, in shared memory the session
//! makes (`HostMsg::Capture`), says which texture that is and which ones are free.
//!
//! Neither side ever waits for the other on the CPU. Two shared GPU fences do the waiting:
//! the hook's *content* fence reaches `ready[slot]` when its copy into a slot is finished
//! (the compositor's GPU waits for it before sampling), and the compositor's *release*
//! fence reaches `released[slot]` when it has finished sampling (the hook only writes a
//! slot whose release it sees completed, and otherwise skips that frame's copy).
//!
//! Layout (little-endian, 256 bytes; every word atomic):
//!
//! | offset | field |
//! |---|---|
//! | 0 | magic `PPFR` |
//! | 4 | version |
//! | 8 | published: `generation << 32 \| sequence << 8 \| slot` (0: nothing yet) |
//! | 16 | ready: four content-fence values, one per slot (0: complete when published) |
//! | 48 | reading: `generation << 32 \| slot` the compositor samples (`NONE`: none) |
//! | 56 | released: four release-fence values, one per slot |
//! | 88 | presents: how many frames the compositor has shown |
//! | 96 | released_gen: four generations, the one each `released` value is of (0: none) |
//! | 112 | flags: compositor frame-pacing bits (`FLAG_SYNC_WANTED`, `FLAG_COMPOSITOR_LIVE`, `FLAG_HIDDEN`) |
//! | 116 | pace_us: minimum frame interval in microseconds (`0`: uncapped / VBlank only) |
//! | 120 | sync_divisor: compositor VSync interval (`0` or `1`: full refresh, `2`: 1/2, `3`: 1/3) |
//! | 124 | reserved, zero |
//!
//! A generation is one set of textures (`HookMsg::Surface`): the hook makes a new one when
//! the game's frames change size or format, and a slot only means something within its
//! generation. Each generation has release fences of its own, starting again at 0, so a
//! `released` value only counts for the generation `released_gen` names: a slot last
//! released in another generation is free in this one.
//!
//! Frames can come as pixels instead (`HostMsg::Capture`'s `pixels`): a game under Wine
//! can't hand a native Linux window a Windows shared texture, so its hook reads each
//! frame back and writes it into a file of its own (`SurfaceMsg::pixels`), `SLOTS` slots
//! of `slot_bytes` each. The handshake is the same; there are no fences (a slot is
//! published once its pixels are written, `ready` 0, and the compositor is done with it
//! when it has copied them out, so `released` stays 0), and no frame event: the
//! compositor looks at `published` once a refresh.
//!
//! Choosing and taking a slot is a two-sided handshake on `published` and `reading`, all
//! in sequentially consistent order: the compositor stores `reading`, then reads
//! `published` again and starts over if it moved; the hook stores `published`, and later
//! reads `reading` before choosing. So the hook never picks the slot the compositor is
//! about to sample.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const SLOTS: usize = 3;
pub const BLOCK_SIZE: usize = 256;
pub const MAGIC: u32 = u32::from_le_bytes(*b"PPFR");
pub const VERSION: u32 = 2;
/// `reading` when the compositor samples nothing.
pub const NONE: u64 = u64::MAX;

/// `RingHeader::flags`: the session wants this copy paced to the compositor's refresh.
pub const FLAG_SYNC_WANTED: u32 = 1 << 0;
/// `RingHeader::flags`: the compositor is actively presenting this copy's visible pane
/// right now (cleared when minimized, occluded, hidden, or stopping so a copy never waits).
pub const FLAG_COMPOSITOR_LIVE: u32 = 1 << 1;
/// `RingHeader::flags`: the compositor window is actively presenting, and this copy's
/// pane is hidden (another pane is zoomed full-screen), so the hook throttles this copy
/// to a low background tick rate while keeping its network and simulation alive.
pub const FLAG_HIDDEN: u32 = 1 << 2;

/// `DXGI_COLOR_SPACE_TYPE`s a frame's values are in (`SurfaceMsg::color_space`).
/// sRGB-encoded, BT.709 primaries: what every SDR frame is.
pub const COLOR_SPACE_SRGB: u32 = 0;
/// Linear light, BT.709 primaries, 1.0 = 80 nits (scRGB, a float frame).
pub const COLOR_SPACE_SCRGB: u32 = 10;
/// HDR10: SMPTE ST 2084 (PQ) encoded, BT.2020 primaries.
pub const COLOR_SPACE_HDR10: u32 = 12;

/// Bytes per pixel of the `DXGI_FORMAT`s frames come in as pixels: BGRA8, RGBA8 (and
/// their sRGB and X twins), R10G10B10A2 and R16G16B16A16_FLOAT. None for others.
pub fn bytes_per_pixel(format: u32) -> Option<u32> {
    match format {
        28 | 29 | 87 | 88 | 91 | 93 | 24 => Some(4),
        10 => Some(8),
        _ => None,
    }
}

/// The bytes one slot of a pixel file takes: `stride x height`, whole pages. None for an
/// empty frame, or one too big to address (in a 32-bit process).
pub fn slot_bytes(stride: u32, height: u32) -> Option<usize> {
    if stride == 0 || height == 0 {
        return None;
    }
    let bytes = usize::try_from(u64::from(stride) * u64::from(height)).ok()?;
    bytes.checked_next_multiple_of(4096)
}

/// The size of a pixel file of `SLOTS` such slots (None as for `slot_bytes`).
pub fn pixels_size(stride: u32, height: u32) -> Option<usize> {
    slot_bytes(stride, height)?.checked_mul(SLOTS)
}

#[repr(C)]
pub struct RingHeader {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub published: AtomicU64,
    pub ready: [AtomicU64; 4],
    pub reading: AtomicU64,
    pub released: [AtomicU64; 4],
    pub presents: AtomicU64,
    pub released_gen: [AtomicU32; 4],
    pub flags: AtomicU32,
    pub pace_us: AtomicU32,
    pub sync_divisor: AtomicU32,
    pub _pad: AtomicU32,
    pub reserved: [AtomicU64; 16],
}

const _: () = assert!(std::mem::size_of::<RingHeader>() == BLOCK_SIZE);
const _: () = assert!(std::mem::offset_of!(RingHeader, published) == 8);
const _: () = assert!(std::mem::offset_of!(RingHeader, ready) == 16);
const _: () = assert!(std::mem::offset_of!(RingHeader, reading) == 48);
const _: () = assert!(std::mem::offset_of!(RingHeader, released) == 56);
const _: () = assert!(std::mem::offset_of!(RingHeader, presents) == 88);
const _: () = assert!(std::mem::offset_of!(RingHeader, released_gen) == 96);
const _: () = assert!(std::mem::offset_of!(RingHeader, flags) == 112);
const _: () = assert!(std::mem::offset_of!(RingHeader, pace_us) == 116);
const _: () = assert!(std::mem::offset_of!(RingHeader, sync_divisor) == 120);

/// A frame the hook published: which generation and slot, its sequence number (24 bits,
/// wrapping), and the content-fence value it is finished at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Published {
    pub generation: u32,
    pub seq: u32,
    pub slot: usize,
    pub ready: u64,
}

fn pack(generation: u32, seq: u32, slot: usize) -> u64 {
    u64::from(generation) << 32 | u64::from(seq & 0xff_ffff) << 8 | (slot as u64 & 0xff)
}

fn unpack(v: u64) -> Option<(u32, u32, usize)> {
    let generation = (v >> 32) as u32;
    let slot = (v & 0xff) as usize;
    (generation != 0 && slot < SLOTS).then_some((generation, ((v >> 8) & 0xff_ffff) as u32, slot))
}

impl RingHeader {
    /// The header of the block at `ptr`, `len` bytes long (None if too short or misaligned).
    ///
    /// # Safety
    /// `ptr` must stay valid for `len` bytes for as long as the returned reference.
    pub unsafe fn from_raw<'a>(ptr: *const u8, len: usize) -> Option<&'a RingHeader> {
        if ptr.is_null() || len < BLOCK_SIZE || ptr as usize % 8 != 0 {
            return None;
        }
        Some(unsafe { &*(ptr as *const RingHeader) })
    }

    /// A fresh block (the compositor's side, once, before the hook is told of it).
    pub fn init(&self) {
        self.published.store(0, Ordering::SeqCst);
        self.reading.store(NONE, Ordering::SeqCst);
        for i in 0..4 {
            self.ready[i].store(0, Ordering::SeqCst);
            self.released[i].store(0, Ordering::SeqCst);
            self.released_gen[i].store(0, Ordering::SeqCst);
        }
        self.presents.store(0, Ordering::SeqCst);
        self.flags.store(0, Ordering::SeqCst);
        self.pace_us.store(0, Ordering::SeqCst);
        self.sync_divisor.store(1, Ordering::SeqCst);
        self.version.store(VERSION, Ordering::SeqCst);
        self.magic.store(MAGIC, Ordering::SeqCst);
    }

    /// Sets or clears one bit in `flags`; returns whether the bit was previously set.
    pub fn set_flag(&self, bit: u32, on: bool) -> bool {
        let prev = if on {
            self.flags.fetch_or(bit, Ordering::AcqRel)
        } else {
            self.flags.fetch_and(!bit, Ordering::AcqRel)
        };
        prev & bit != 0
    }

    /// Whether compositor-locked frame pacing is both wanted by the session and live on
    /// the compositor right now.
    pub fn sync_active(&self) -> bool {
        const BOTH: u32 = FLAG_SYNC_WANTED | FLAG_COMPOSITOR_LIVE;
        self.flags.load(Ordering::Acquire) & BOTH == BOTH
    }

    /// Whether this copy's pane is hidden behind another zoomed pane while the session
    /// window is actively presenting.
    pub fn hidden(&self) -> bool {
        self.flags.load(Ordering::Acquire) & FLAG_HIDDEN != 0
    }

    /// Target minimum frame interval in microseconds (`0`: no explicit FPS cap).
    pub fn pace_us(&self) -> u32 {
        self.pace_us.load(Ordering::Acquire)
    }

    /// Sets the target minimum frame interval in microseconds (`0` clears the cap).
    pub fn set_pace_us(&self, us: u32) {
        self.pace_us.store(us, Ordering::Release);
    }

    /// Compositor VSync divisor (`1` = full refresh, `2` = 1/2, `3` = 1/3). A zero-initialised
    /// block reads as `1`.
    pub fn sync_divisor(&self) -> u8 {
        self.sync_divisor.load(Ordering::Acquire).clamp(1, 4) as u8
    }

    /// Sets the compositor VSync divisor (`1..=4`).
    pub fn set_sync_divisor(&self, divisor: u8) {
        self.sync_divisor.store(u32::from(divisor.clamp(1, 4)), Ordering::Release);
    }

    /// Whether any form of hook-side frame pacing is active (compositor sync, hidden-pane
    /// throttling, or an explicit FPS cap).
    pub fn pace_active(&self) -> bool {
        self.sync_active() || self.hidden() || self.pace_us() > 0
    }

    pub fn valid(&self) -> bool {
        self.magic.load(Ordering::SeqCst) == MAGIC && self.version.load(Ordering::SeqCst) == VERSION
    }

    /// The newest frame published, if any.
    pub fn published(&self) -> Option<Published> {
        let (generation, seq, slot) = unpack(self.published.load(Ordering::SeqCst))?;
        Some(Published { generation, seq, slot, ready: self.ready[slot].load(Ordering::SeqCst) })
    }

    // --- The hook's side ----------------------------------------------------------------

    /// A slot of `generation` the hook may copy the next frame into: not the newest one
    /// (the compositor may take it any moment), not the one it samples, and one whose
    /// release `released_done` says the compositor's GPU has passed. A release noted in
    /// another generation (other fences) doesn't hold a slot of this one. None: skip this
    /// frame's copy (the compositor still shows the one before).
    pub fn pick(&self, generation: u32, released_done: impl Fn(u64) -> bool) -> Option<usize> {
        self.pick_except(generation, released_done, |_| false)
    }

    /// `pick`, never a slot `taken` names: one the hook's copy into is still under way,
    /// though not published yet (a copy whose slot is published later, by another thread).
    pub fn pick_except(&self, generation: u32, released_done: impl Fn(u64) -> bool, taken: impl Fn(usize) -> bool) -> Option<usize> {
        let newest = unpack(self.published.load(Ordering::SeqCst)).filter(|p| p.0 == generation).map(|p| p.2);
        let reading = self.reading.load(Ordering::SeqCst);
        let sampled = (reading != NONE && (reading >> 32) as u32 == generation).then_some((reading & 0xff) as usize);
        let start = newest.map_or(0, |n| n + 1);
        let free = |s: usize| self.released_gen[s].load(Ordering::SeqCst) != generation || released_done(self.released[s].load(Ordering::SeqCst));
        (0..SLOTS).map(|i| (start + i) % SLOTS).find(|&s| Some(s) != newest && Some(s) != sampled && !taken(s) && free(s))
    }

    /// The copy into `slot` of `generation` is submitted: finished when the content fence
    /// reaches `ready` (0: it already is). Returns the frame's sequence number.
    pub fn publish(&self, generation: u32, slot: usize, ready: u64) -> u32 {
        let seq = unpack(self.published.load(Ordering::SeqCst)).map_or(0, |p| p.1.wrapping_add(1));
        self.ready[slot % SLOTS].store(ready, Ordering::SeqCst);
        self.published.store(pack(generation, seq, slot % SLOTS), Ordering::SeqCst);
        seq & 0xff_ffff
    }

    // --- The compositor's side ----------------------------------------------------------

    /// Takes the newest frame of `generation` for sampling (marks it `reading`), or keeps
    /// sampling the slot it already had when the newest is of another generation. None:
    /// no frame of that generation yet.
    /// The frame `take` would give now, as (slot, seq), without taking it: nothing is
    /// marked as read, so the hook may still publish a newer one over it.
    pub fn peek(&self, generation: u32) -> Option<(usize, u32)> {
        unpack(self.published.load(Ordering::SeqCst)).filter(|p| p.0 == generation).map(|(_, seq, slot)| (slot, seq))
    }

    pub fn take(&self, generation: u32) -> Option<Published> {
        loop {
            let raw = self.published.load(Ordering::SeqCst);
            let Some((g, seq, slot)) = unpack(raw).filter(|p| p.0 == generation) else {
                return None;
            };
            self.reading.store(u64::from(g) << 32 | slot as u64, Ordering::SeqCst);
            if self.published.load(Ordering::SeqCst) == raw {
                return Some(Published { generation: g, seq, slot, ready: self.ready[slot].load(Ordering::SeqCst) });
            }
        }
    }

    /// The compositor's GPU is done with `slot` of `generation` once that generation's
    /// release fence reaches `value`. (The slot is still `reading` while both words are
    /// written, so the hook never acts on half of them.)
    pub fn release(&self, generation: u32, slot: usize, value: u64) {
        self.released[slot % SLOTS].store(value, Ordering::SeqCst);
        self.released_gen[slot % SLOTS].store(generation, Ordering::SeqCst);
    }

    /// The compositor samples nothing of this copy any more.
    pub fn stop_reading(&self) {
        self.reading.store(NONE, Ordering::SeqCst);
    }

    /// The compositor showed a frame.
    pub fn presented(&self) {
        self.presents.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block() -> Box<RingHeader> {
        let b: Box<RingHeader> = Box::new(unsafe { std::mem::zeroed() });
        b.init();
        b
    }

    #[test]
    fn nothing_until_published() {
        let r = block();
        assert!(r.valid());
        assert_eq!(r.published(), None);
        assert_eq!(r.take(1), None);
        assert_eq!(r.reading.load(Ordering::SeqCst), NONE);
        assert_eq!(r.pick(1, |_| true), Some(0));
        assert!(!r.sync_active());
        assert!(!r.set_flag(FLAG_SYNC_WANTED, true));
        assert!(!r.sync_active(), "needs compositor live too");
        assert!(!r.set_flag(FLAG_COMPOSITOR_LIVE, true));
        assert!(r.sync_active());
        assert!(r.pace_active());
        assert!(r.set_flag(FLAG_COMPOSITOR_LIVE, false));
        assert!(!r.sync_active());
        assert!(!r.pace_active());
        assert!(!r.hidden());
        assert!(!r.set_flag(FLAG_HIDDEN, true));
        assert!(r.hidden() && r.pace_active());
        assert!(r.set_flag(FLAG_HIDDEN, false));
        assert!(!r.pace_active());
        r.set_pace_us(16_667);
        assert_eq!(r.pace_us(), 16_667);
        assert!(r.pace_active());
        r.set_pace_us(0);
        assert!(!r.pace_active());
        assert_eq!(r.sync_divisor(), 1);
        r.set_sync_divisor(2);
        assert_eq!(r.sync_divisor(), 2);
        r.set_sync_divisor(3);
        assert_eq!(r.sync_divisor(), 3);
        r.set_sync_divisor(0);
        assert_eq!(r.sync_divisor(), 1);
    }

    /// The hook never writes the slot published last or the one sampled, and moves on to
    /// the free one.
    #[test]
    fn the_hook_writes_around_the_compositor() {
        let r = block();
        assert_eq!(r.publish(1, 0, 10), 0);
        assert_eq!(r.pick(1, |_| true), Some(1), "not the newest");
        let p = r.take(1).unwrap();
        assert_eq!((p.slot, p.seq, p.ready), (0, 0, 10));
        assert_eq!(r.publish(1, 1, 11), 1);
        // Slot 0 is sampled, slot 1 is newest: slot 2 is the one left.
        assert_eq!(r.pick(1, |_| true), Some(2));
        r.release(1, 0, 5);
        let p = r.take(1).unwrap();
        assert_eq!(p.slot, 1);
        // Slot 0's release isn't done on the compositor's GPU yet: slot 2.
        assert_eq!(r.pick(1, |v| v < 5), Some(2));
        r.publish(1, 2, 12);
        // 1 sampled, 2 newest, 0 still in use on the GPU: skip the frame.
        assert_eq!(r.pick(1, |v| v < 5), None);
        assert_eq!(r.pick(1, |v| v <= 5), Some(0));
    }

    /// A new generation's slots don't collide with the old one's sampling.
    #[test]
    fn generations_stand_apart() {
        let r = block();
        r.publish(1, 2, 0);
        r.take(1).unwrap();
        assert_eq!(r.take(2), None, "no frame of generation 2 yet");
        // Generation 2: the compositor samples slot 2 of generation 1, which is free here.
        assert_eq!(r.pick(2, |_| true), Some(0));
        r.publish(2, 0, 3);
        assert_eq!(r.pick(2, |_| true), Some(1));
        assert_eq!(r.take(2).map(|p| (p.generation, p.slot, p.ready)), Some((2, 0, 3)));
        assert_eq!(r.take(1), None, "the newest is generation 2's");
    }

    /// A resize makes a new set whose release fence starts again at 0: the old set's high
    /// release values don't hold its slots (they did, forever: the pane froze on the old
    /// set's last frame).
    #[test]
    fn a_new_generation_isnt_held_by_the_old_ones_releases() {
        let r = block();
        for (slot, value) in [(0, 598), (1, 599), (2, 600)] {
            r.publish(1, slot, 0);
            r.take(1).unwrap();
            r.release(1, slot, value);
        }
        assert_eq!(r.pick(1, |v| v <= 0), None, "generation 1 still waits on its own fence");
        assert_eq!(r.pick(2, |v| v <= 0), Some(0), "generation 2 isn't held by generation 1's values");
        r.publish(2, 0, 1);
        r.take(2).unwrap();
        r.release(2, 0, 1);
        r.publish(2, 1, 2);
        r.take(2).unwrap();
        r.release(2, 1, 2);
        // Slot 0 waits on generation 2's fence now (value 1), slot 1 is sampled, slot 2
        // only holds a generation-1 release.
        assert_eq!(r.pick(2, |v| v <= 0), Some(2));
        r.publish(2, 2, 3);
        r.take(2).unwrap();
        r.release(2, 2, 3);
        assert_eq!(r.pick(2, |v| v <= 0), None, "both other slots wait on generation 2's fence");
        assert_eq!(r.pick(2, |v| v <= 1), Some(0));
    }

    /// A copy published later (by another thread) still holds its slot: the next frame
    /// doesn't pick it again while it's being written.
    #[test]
    fn a_slot_still_being_copied_isnt_picked_again() {
        let r = block();
        assert_eq!(r.pick(1, |_| true), Some(0));
        assert_eq!(r.pick(1, |_| true), Some(0), "plain pick doesn't know the copy is under way");
        assert_eq!(r.pick_except(1, |_| true, |s| s == 0), Some(1));
        assert_eq!(r.pick_except(1, |_| true, |s| s <= 1), Some(2));
        r.publish(1, 0, 0);
        assert_eq!(r.pick_except(1, |_| true, |s| s == 1), Some(2), "not the newest, not the one in flight");
        assert_eq!(r.pick_except(1, |_| true, |s| s >= 1), None, "every slot busy: the frame isn't copied");
    }

    #[test]
    fn pixel_files_are_whole_pages_per_slot() {
        assert_eq!(slot_bytes(1920 * 4, 1080), Some(1920 * 4 * 1080));
        assert_eq!(slot_bytes(64 * 4, 3), Some(4096));
        assert_eq!(pixels_size(64 * 4, 17), Some(3 * 8192));
        assert_eq!(slot_bytes(0, 1080), None, "an empty frame");
        assert_eq!(slot_bytes(64, 0), None);
        if usize::BITS == 32 {
            assert_eq!(slot_bytes(u32::MAX, 2), None, "too big to address");
        } else {
            assert_eq!(pixels_size(u32::MAX, u32::MAX), None, "too big to address");
        }
        assert_eq!(bytes_per_pixel(87), Some(4));
        assert_eq!(bytes_per_pixel(10), Some(8));
        assert_eq!(bytes_per_pixel(90), None, "pixels are typed");
    }

    #[test]
    fn sequences_wrap_in_24_bits() {

        let r = block();
        r.published.store(pack(7, 0xff_ffff, 1), Ordering::SeqCst);
        assert_eq!(r.publish(7, 2, 0), 0);
        assert_eq!(unpack(pack(u32::MAX, 5, 2)), Some((u32::MAX, 5, 2)));
        assert_eq!(unpack(pack(0, 5, 2)), None, "generation 0 is nothing");
        assert_eq!(unpack(pack(1, 5, 3)), None, "slot out of the ring");
    }

    /// Both sides racing on real threads: while the compositor holds a slot it took, the
    /// hook never writes into it (each write bumps that slot's counter, which the
    /// compositor checks stayed put until it lets the slot go).
    #[test]
    fn racing_sides_never_share_a_slot() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        let r: Arc<Box<RingHeader>> = Arc::new(block());
        let writes: Arc<[AtomicU64; SLOTS]> = Arc::new(Default::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (r2, w2, s2) = (r.clone(), writes.clone(), stop.clone());
        let compositor = std::thread::spawn(move || {
            let mut frames = 0u32;
            while !s2.load(Ordering::SeqCst) {
                let Some(p) = r2.take(1) else { continue };
                let before = w2[p.slot].load(Ordering::SeqCst);
                for _ in 0..50 {
                    std::hint::spin_loop();
                }
                assert_eq!(w2[p.slot].load(Ordering::SeqCst), before, "slot {} written while sampled", p.slot);
                frames += 1;
            }
            frames
        });
        let mut written = 0u32;
        for _ in 0..200_000 {
            if let Some(s) = r.pick(1, |_| true) {
                writes[s].fetch_add(1, Ordering::SeqCst);
                r.publish(1, s, 0);
                written += 1;
            }
        }
        stop.store(true, Ordering::SeqCst);
        assert!(compositor.join().unwrap() > 0);
        assert!(written > 0);
    }
}
