//! One copy's virtual input: the session host's input hub writes what the copy's
//! assigned controller, keyboard, mouse and wheel are doing into this block, and the
//! hook's input layer (hook/src/input) answers the game's input calls from it.
//!
//! Layout (little-endian), 256 bytes, one 32-bit word each:
//!
//! | offset | field |
//! |---|---|
//! | 0 | magic `PPIN` |
//! | 4 | sequence: odd while being written, even when complete |
//! | 8 | mode (`InputMode`) |
//! | 12 | flags (none defined yet, zero) |
//! | 16 | pad: connected, buttons, left trigger, right trigger, thumbs lx, ly, rx, ry, packet number, real XInput slot + 1 (0: none), HID key low, high |
//! | 64 | mouse: active, cursor x, y, motion x, y, wheel, buttons; 1 reserved |
//! | 96 | keyboard: active, 8 words of keys (bit `vk % 32` of word `vk / 32`); 3 reserved |
//! | 144 | wheel: connected, steer, throttle, brake, clutch, buttons; 2 reserved |
//! | 176 | pane projection: dest x, y, w, h, uv[4], compositor hwnd lo/hi, pointer flags; 9 reserved |
//! | 256 | cursor channel (128 bytes, optional): host->hook coalesced move; hook->host cursor shape, hide, clip |
//!
//! Signed values (thumbs, cursor, motion, steer) are their `i32` bits; narrower ones
//! (triggers, pedals, thumbs) take the word's low bits.
//!
//! The pad's state is what the copy's controller is doing, for every input API the hook
//! answers itself. Where the copy's controller can be reached as it is, the pad also
//! says which one: its XInput slot (the hook's XInput calls go to that slot), or its HID
//! device's key (`hid_key`: the game's reads of that device go through untouched). A
//! device with a serial number of its own is keyed by it; one with none, or whose serial
//! another connected device shares (two identical pads), by where it's plugged in
//! (`hid_instance_key`), so neither is taken for the other.
//!
//! The mouse's motion and wheel are running totals (wrapping), not one event's: a
//! reader that misses a publish loses no motion, it takes the difference from the last
//! snapshot it saw (`VirtualMouse::motion_since`).
//!
//! The sequence lock is `shm`'s: one writer makes the sequence odd, writes the words,
//! then makes it even again; a reader that saw the same even sequence before and after
//! its copy has a whole snapshot.

use std::sync::atomic::{AtomicU32, Ordering, fence};

pub const INPUT_MAGIC: u32 = u32::from_le_bytes(*b"PPIN");
/// The block's size in bytes.
pub const INPUT_BLOCK_SIZE: usize = 256;
/// The keyboard's keys: one bit per Win32 virtual-key code.
pub const KEY_WORDS: usize = 8;

/// The mouse's buttons (`VirtualMouse::buttons`).
pub const MOUSE_LEFT: u32 = 1 << 0;
pub const MOUSE_RIGHT: u32 = 1 << 1;
pub const MOUSE_MIDDLE: u32 = 1 << 2;
pub const MOUSE_X1: u32 = 1 << 3;
pub const MOUSE_X2: u32 = 1 << 4;

/// `PaneProjection::flags`: this copy is driven by the real OS pointer, so the hook can
/// project the live hardware cursor directly in-process instead of waiting on host IPC.
pub const POINTER_LIVE_OS: u32 = 1 << 0;
/// `PaneProjection::flags`: the session's compositor window currently holds the
/// foreground (no overlay or Layout Studio open), so a one-shot visible `SetCursorPos`
/// from the game may move the real pointer into its pane.
pub const POINTER_COMPOSITOR_FRONT: u32 = 1 << 1;

/// Offset and size of the bidirectional `CursorChannel` inside the mapped 4 KiB page,
/// past `InputHeader` (256 bytes).
pub const CURSOR_CHANNEL_OFFSET: usize = 256;
pub const CURSOR_CHANNEL_SIZE: usize = 128;

/// Window message (`WM_APP + 0x43`) posted by the hook to `PaneProjection::compositor_hwnd`
/// whenever cursor shape, visibility, or confinement changes, waking the session host at
/// once instead of waiting for the next host tick.
pub const WM_APP_CURSOR: u32 = 0x8043;

/// `CursorFeedback::flags` bits.
pub const CURSOR_HAS_SHAPE: u32 = 1 << 0;
pub const CURSOR_HIDDEN_SHOW: u32 = 1 << 1;
pub const CURSOR_HIDDEN_SHAPE: u32 = 1 << 2;
pub const CURSOR_CLIPPED: u32 = 1 << 3;
pub const CURSOR_RECENTERING: u32 = 1 << 4;
pub const CURSOR_HAS_CLIP_RECT: u32 = 1 << 5;
pub const CURSOR_COALESCING: u32 = 1 << 6;

/// What the hook does with the game's input calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum InputMode {
    /// The game sees the real devices (one copy alone, or no hub yet).
    #[default]
    PassThrough = 0,
    /// The game sees nothing (a copy that's only watched, or the overlay is open).
    BlockAll = 1,
    /// The game sees only this block's devices.
    Virtual = 2,
}

impl InputMode {
    /// The mode a word names, or None for one this build doesn't know.
    pub fn from_u32(v: u32) -> Option<InputMode> {
        Some(match v {
            0 => InputMode::PassThrough,
            1 => InputMode::BlockAll,
            2 => InputMode::Virtual,
            _ => return None,
        })
    }

    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

/// The copy's controller, as XInput describes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VirtualPad {
    pub connected: bool,
    /// XInput's `wButtons` in the low 16 bits; buttons XInput has no bit for (the guide
    /// button's own, paddles, a touchpad press) in the high 16.
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
    /// XInput's `dwPacketNumber`: changes whenever the state does.
    pub packet_number: u32,
    /// The real XInput slot this controller is (system-wide), when it is one.
    pub xinput_slot: Option<u8>,
    /// Its HID device's key (`hid_key`), 0 when it isn't one or can't be told apart.
    pub hid_key: u64,
}

/// A HID device's key, the same in the launcher and in the game: its vendor, product and
/// serial number (case aside). Never 0, which means none.
pub fn hid_key(vid: u16, pid: u16, serial: &str) -> u64 {
    // FNV-1a, 64-bit.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let text = format!("{vid:04x}:{pid:04x}:{}", serial.trim().to_lowercase());
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h.max(1)
}

/// The part of a HID device interface path (`\\?\HID#VID_054C&PID_0CE6&MI_03&Col01#8&3a1b2c&0&0001#{guid}`)
/// that names the physical device where it's plugged in, the same for each of its
/// top-level collections and whoever reads the path (the session's own scan, Raw Input,
/// DirectInput, the game's CreateFile): the device id without its collection, and the
/// instance id without the collection's number, lower case. None for a path that isn't
/// a HID one.
pub fn hid_instance(path: &str) -> Option<String> {
    let lower = path.trim_end_matches('\0').to_ascii_lowercase();
    let rest = lower.strip_prefix(r"\\?\").or_else(|| lower.strip_prefix(r"\\.\")).unwrap_or(&lower);
    let mut parts = rest.split('#');
    if parts.next()? != "hid" {
        return None;
    }
    let device: Vec<&str> = parts.next()?.split('&').filter(|p| !p.is_empty() && !p.starts_with("col")).collect();
    let mut instance: Vec<&str> = parts.next()?.split('&').collect();
    if device.is_empty() || instance.iter().all(|p| p.is_empty()) {
        return None;
    }
    if instance.len() > 1 {
        instance.pop();
    }
    Some(format!("{}#{}", device.join("&"), instance.join("&")))
}

/// A HID device's key by where it's plugged in (`hid_instance`), for one without a
/// serial number of its own: never equal to a serial one (`hid_key`), never 0.
pub fn hid_instance_key(vid: u16, pid: u16, path: &str) -> Option<u64> {
    let instance = hid_instance(path)?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("{vid:04x}:{pid:04x}@{instance}").bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(h.max(1))
}

impl VirtualPad {
    /// The buttons XInput knows (`XINPUT_GAMEPAD::wButtons`).
    pub fn xinput_buttons(&self) -> u16 {
        self.buttons as u16
    }

    /// The buttons XInput has no bit for.
    pub fn extra_buttons(&self) -> u16 {
        (self.buttons >> 16) as u16
    }
}

/// The copy's mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VirtualMouse {
    pub active: bool,
    /// Where the copy's cursor is, in its client pixels.
    pub cursor_x: i32,
    pub cursor_y: i32,
    /// Motion and wheel so far, running totals that wrap (`motion_since`).
    pub delta_x: i32,
    pub delta_y: i32,
    pub wheel_delta: i32,
    /// `MOUSE_*`.
    pub buttons: u32,
}

impl VirtualMouse {
    /// The motion (x, y) and wheel between an earlier snapshot and this one.
    pub fn motion_since(&self, earlier: &VirtualMouse) -> (i32, i32, i32) {
        (self.delta_x.wrapping_sub(earlier.delta_x), self.delta_y.wrapping_sub(earlier.delta_y), self.wheel_delta.wrapping_sub(earlier.wheel_delta))
    }

    pub fn is_down(&self, button: u32) -> bool {
        self.buttons & button != 0
    }
}

/// The copy's keyboard: which keys are down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VirtualKeyboard {
    pub active: bool,
    /// Bit `vk % 32` of word `vk / 32` for each Win32 virtual-key code.
    pub keys: [u32; KEY_WORDS],
}

impl VirtualKeyboard {
    pub fn is_down(&self, vk: u8) -> bool {
        self.keys[usize::from(vk >> 5)] & (1 << (vk & 31)) != 0
    }

    pub fn set(&mut self, vk: u8, down: bool) {
        let (word, bit) = (&mut self.keys[usize::from(vk >> 5)], 1u32 << (vk & 31));
        if down {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }
}

/// The copy's wheel and pedals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VirtualWheel {
    pub connected: bool,
    /// Left negative, right positive.
    pub steer: i32,
    /// Pedals, 0 released to `u16::MAX` pressed all the way.
    pub throttle: u16,
    pub brake: u16,
    pub clutch: u16,
    pub buttons: u32,
}

/// Where this copy's frames sit on screen (compositor physical pixels) and which UV
/// sub-rect they sample, so the hook can map between the live OS hardware pointer and
/// the game's client pixels in-process with zero IPC latency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct PaneProjection {
    pub dest_x: i32,
    pub dest_y: i32,
    pub dest_w: u32,
    pub dest_h: u32,
    /// `f32::to_bits` of `[u0, v0, u1, v1]`; all-zero means `[0.0, 0.0, 1.0, 1.0]`.
    pub uv_bits: [u32; 4],
    /// Session compositor HWND (`0` when none), for instant `WM_APP_CURSOR` wakeups.
    pub compositor_hwnd: u64,
    /// `POINTER_*` flags.
    pub flags: u32,
}

impl PaneProjection {
    pub fn uv_to_bits(uv: [f32; 4]) -> [u32; 4] {
        [uv[0].to_bits(), uv[1].to_bits(), uv[2].to_bits(), uv[3].to_bits()]
    }

    /// Source UV sub-rectangle `[u0, v0, u1, v1]` in `0.0..=1.0`.
    pub fn uv(&self) -> [f32; 4] {
        if self.uv_bits == [0; 4] {
            [0.0, 0.0, 1.0, 1.0]
        } else {
            self.uv_bits.map(f32::from_bits)
        }
    }

    pub fn live_os(&self) -> bool {
        self.flags & POINTER_LIVE_OS != 0 && self.dest_w > 0 && self.dest_h > 0
    }

    pub fn compositor_front(&self) -> bool {
        self.flags & POINTER_COMPOSITOR_FRONT != 0
    }

    /// Projects a physical screen point `(sx, sy)` on the compositor into game client
    /// pixels `(0..cw, 0..ch)`.
    pub fn project_to_client(&self, sx: i32, sy: i32, cw: u32, ch: u32) -> Option<(i32, i32)> {
        if self.dest_w == 0 || self.dest_h == 0 || cw == 0 || ch == 0 {
            return None;
        }
        let uv = self.uv();
        let rx = ((f64::from(sx) - f64::from(self.dest_x)) / f64::from(self.dest_w)).clamp(0.0, 1.0);
        let ry = ((f64::from(sy) - f64::from(self.dest_y)) / f64::from(self.dest_h)).clamp(0.0, 1.0);
        let u = f64::from(uv[0]) + rx * f64::from(uv[2] - uv[0]);
        let v = f64::from(uv[1]) + ry * f64::from(uv[3] - uv[1]);
        let gx = (u * f64::from(cw)).floor().clamp(0.0, f64::from(cw - 1)) as i32;
        let gy = (v * f64::from(ch)).floor().clamp(0.0, f64::from(ch - 1)) as i32;
        Some((gx, gy))
    }

    /// Inverse of `project_to_client`: maps a game client pixel `(gx, gy)` back to
    /// physical screen coordinates inside the compositor's drawn frame rect.
    pub fn unproject_from_client(&self, gx: i32, gy: i32, cw: u32, ch: u32) -> Option<(i32, i32)> {
        if self.dest_w == 0 || self.dest_h == 0 || cw == 0 || ch == 0 {
            return None;
        }
        let uv = self.uv();
        let uw = f64::from(uv[2] - uv[0]);
        let vh = f64::from(uv[3] - uv[1]);
        if uw <= 1e-6 || vh <= 1e-6 {
            return None;
        }
        let u = (f64::from(gx) + 0.5) / f64::from(cw);
        let v = (f64::from(gy) + 0.5) / f64::from(ch);
        let rx = ((u - f64::from(uv[0])) / uw).clamp(0.0, 1.0);
        let ry = ((v - f64::from(uv[1])) / vh).clamp(0.0, 1.0);
        let max_x = f64::from(self.dest_w - 1);
        let max_y = f64::from(self.dest_h - 1);
        let sx = self.dest_x.saturating_add((rx * f64::from(self.dest_w)).floor().clamp(0.0, max_x) as i32);
        let sy = self.dest_y.saturating_add((ry * f64::from(self.dest_h)).floor().clamp(0.0, max_y) as i32);
        Some((sx, sy))
    }

    /// Maps a normalized game client clip rect `[left, top, right, bottom]` in `0.0..=1.0`
    /// to a physical screen rect `[left, top, right, bottom]` clamped inside `dest`.
    pub fn project_clip_rect(&self, norm: [f32; 4]) -> Option<[i32; 4]> {
        if self.dest_w == 0 || self.dest_h == 0 {
            return None;
        }
        let uv = self.uv();
        let uw = f64::from(uv[2] - uv[0]);
        let vh = f64::from(uv[3] - uv[1]);
        if uw <= 1e-6 || vh <= 1e-6 {
            return None;
        }
        let map_x = |u: f32| {
            let rx = ((f64::from(u) - f64::from(uv[0])) / uw).clamp(0.0, 1.0);
            self.dest_x.saturating_add((rx * f64::from(self.dest_w)).round() as i32)
        };
        let map_y = |v: f32| {
            let ry = ((f64::from(v) - f64::from(uv[1])) / vh).clamp(0.0, 1.0);
            self.dest_y.saturating_add((ry * f64::from(self.dest_h)).round() as i32)
        };
        let l = map_x(norm[0].min(norm[2]));
        let t = map_y(norm[1].min(norm[3]));
        let r = map_x(norm[0].max(norm[2])).max(l.saturating_add(1));
        let b = map_y(norm[1].max(norm[3])).max(t.saturating_add(1));
        let max_r = self.dest_x.saturating_add(self.dest_w as i32);
        let max_b = self.dest_y.saturating_add(self.dest_h as i32);
        Some([l.clamp(self.dest_x, max_r.saturating_sub(1)), t.clamp(self.dest_y, max_b.saturating_sub(1)), r.clamp(self.dest_x.saturating_add(1), max_r), b.clamp(self.dest_y.saturating_add(1), max_b)])
    }
}

/// The block's words at one moment (one `InputHeader::read_stable`), or what a writer
/// publishes (its `seq` is the block's own, and ignored).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct InputSnapshot {
    pub seq: u32,
    pub mode: InputMode,
    pub flags: u32,
    pub pad: VirtualPad,
    pub mouse: VirtualMouse,
    pub keyboard: VirtualKeyboard,
    pub wheel: VirtualWheel,
    pub pane: PaneProjection,
}

/// The pad's words, at 16.
#[repr(C)]
pub struct PadWords {
    pub connected: AtomicU32,
    pub buttons: AtomicU32,
    pub left_trigger: AtomicU32,
    pub right_trigger: AtomicU32,
    pub thumb_lx: AtomicU32,
    pub thumb_ly: AtomicU32,
    pub thumb_rx: AtomicU32,
    pub thumb_ry: AtomicU32,
    pub packet_number: AtomicU32,
    /// `xinput_slot + 1`, 0 for none.
    pub xinput_slot: AtomicU32,
    pub hid_key_lo: AtomicU32,
    pub hid_key_hi: AtomicU32,
}

/// The mouse's words, at 64.
#[repr(C)]
pub struct MouseWords {
    pub active: AtomicU32,
    pub cursor_x: AtomicU32,
    pub cursor_y: AtomicU32,
    pub delta_x: AtomicU32,
    pub delta_y: AtomicU32,
    pub wheel_delta: AtomicU32,
    pub buttons: AtomicU32,
    pub reserved: [AtomicU32; 1],
}

/// The keyboard's words, at 96.
#[repr(C)]
pub struct KeyboardWords {
    pub active: AtomicU32,
    pub keys: [AtomicU32; KEY_WORDS],
    pub reserved: [AtomicU32; 3],
}

/// The wheel's words, at 144.
#[repr(C)]
pub struct WheelWords {
    pub connected: AtomicU32,
    pub steer: AtomicU32,
    pub throttle: AtomicU32,
    pub brake: AtomicU32,
    pub clutch: AtomicU32,
    pub buttons: AtomicU32,
    pub reserved: [AtomicU32; 2],
}

/// The pane projection's words, at 176 (44 bytes).
#[repr(C)]
pub struct PaneWords {
    pub dest_x: AtomicU32,
    pub dest_y: AtomicU32,
    pub dest_w: AtomicU32,
    pub dest_h: AtomicU32,
    pub uv_bits: [AtomicU32; 4],
    pub compositor_hwnd_lo: AtomicU32,
    pub compositor_hwnd_hi: AtomicU32,
    pub flags: AtomicU32,
}

/// The whole block, as both sides see it.
#[repr(C)]
pub struct InputHeader {
    pub magic: AtomicU32,
    pub seq: AtomicU32,
    /// An `InputMode`.
    pub mode: AtomicU32,
    pub flags: AtomicU32,
    pub pad: PadWords,
    pub mouse: MouseWords,
    pub keyboard: KeyboardWords,
    pub wheel: WheelWords,
    pub pane: PaneWords,
    pub reserved: [AtomicU32; 9],
}

const _: () = assert!(std::mem::size_of::<InputHeader>() == INPUT_BLOCK_SIZE);
const _: () = assert!(std::mem::align_of::<InputHeader>() == 4);
const _: () = assert!(std::mem::offset_of!(InputHeader, mode) == 8);
const _: () = assert!(std::mem::offset_of!(InputHeader, flags) == 12);
const _: () = assert!(std::mem::offset_of!(InputHeader, pad) == 16);
const _: () = assert!(std::mem::offset_of!(InputHeader, pad.packet_number) == 48);
const _: () = assert!(std::mem::offset_of!(InputHeader, mouse) == 64);
const _: () = assert!(std::mem::offset_of!(InputHeader, mouse.buttons) == 88);
const _: () = assert!(std::mem::offset_of!(InputHeader, keyboard) == 96);
const _: () = assert!(std::mem::offset_of!(InputHeader, keyboard.keys) == 100);
const _: () = assert!(std::mem::offset_of!(InputHeader, wheel) == 144);
const _: () = assert!(std::mem::offset_of!(InputHeader, wheel.buttons) == 164);
const _: () = assert!(std::mem::offset_of!(InputHeader, pane) == 176);
const _: () = assert!(std::mem::offset_of!(InputHeader, reserved) == 220);

/// Hook -> Host cursor state published lock-free in `CursorChannel` (sub-millisecond,
/// bypassing the JSON pipe and host tick).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CursorFeedback {
    /// Monotonic version (seqlock-even); changes on every shape/hide/clip update.
    pub seq: u32,
    /// Current `HCURSOR` handle (0 if hidden via `SetCursor(NULL)` or unset).
    pub handle: u64,
    /// `CURSOR_*` flags.
    pub flags: u32,
    /// Normalized `[left, top, right, bottom]` in `0.0..=1.0` of game client rect (`f32::to_bits`).
    pub clip_bits: [u32; 4],
}

impl CursorFeedback {
    pub fn clip_norm(&self) -> Option<[f32; 4]> {
        (self.flags & CURSOR_HAS_CLIP_RECT != 0).then(|| self.clip_bits.map(f32::from_bits))
    }

    pub fn has_shape(&self) -> bool {
        self.flags & CURSOR_HAS_SHAPE != 0
    }

    pub fn hidden_show(&self) -> bool {
        self.flags & CURSOR_HIDDEN_SHOW != 0
    }

    pub fn hidden_shape(&self) -> bool {
        self.flags & CURSOR_HIDDEN_SHAPE != 0
    }

    pub fn clipped(&self) -> bool {
        self.flags & CURSOR_CLIPPED != 0
    }

    pub fn recentering(&self) -> bool {
        self.flags & CURSOR_RECENTERING != 0
    }

    pub fn coalescing(&self) -> bool {
        self.flags & CURSOR_COALESCING != 0
    }
}

/// Bidirectional lock-free cursor & coalesced-move channel sitting at offset 256 of the
/// 4 KiB `InputBlock` mapping. Split across two 64-byte cache lines so host writes and
/// hook writes never false-share.
#[repr(C)]
pub struct CursorChannel {
    // --- Cache line 0 (offset 256..320): Host writes, Hook reads ---
    pub move_seq: AtomicU32,
    pub move_wparam: AtomicU32,
    pub move_client_x: AtomicU32,
    pub move_client_y: AtomicU32,
    pub _host_pad: [AtomicU32; 12],
    // --- Cache line 1 (offset 320..384): Hook writes, Host reads ---
    pub move_acked: AtomicU32,
    /// Odd while hook is writing `cursor_*` fields, even when stable (seqlock).
    pub cursor_seq: AtomicU32,
    pub cursor_handle_lo: AtomicU32,
    pub cursor_handle_hi: AtomicU32,
    pub cursor_flags: AtomicU32,
    pub clip_bits: [AtomicU32; 4],
    /// Monotonic generation bumped when `SetCursor` sets a new shape (even if handle
    /// value was reused after `DestroyCursor`).
    pub shape_gen: AtomicU32,
    /// Separate atomic bit set once `focus_proc` starts acknowledging coalesced moves
    /// (written without touching `cursor_seq`).
    pub coalescing: AtomicU32,
    pub _hook_pad: [AtomicU32; 5],
}

const _: () = assert!(std::mem::size_of::<CursorChannel>() == CURSOR_CHANNEL_SIZE);
const _: () = assert!(std::mem::offset_of!(CursorChannel, move_acked) == 64);

impl CursorChannel {
    /// A zeroed channel (tests).
    pub fn new() -> CursorChannel {
        unsafe { std::mem::zeroed() }
    }

    /// Borrows the channel from the base of a 4 KiB `InputBlock` mapping.
    ///
    /// # Safety
    /// Same requirements as `InputHeader::from_raw`.
    pub unsafe fn from_mapping<'a>(base: *const u8, len: usize) -> Option<&'a CursorChannel> {
        if base.is_null() || len < CURSOR_CHANNEL_OFFSET + CURSOR_CHANNEL_SIZE {
            return None;
        }
        let ptr = unsafe { base.add(CURSOR_CHANNEL_OFFSET) };
        if ptr.align_offset(std::mem::align_of::<CursorChannel>()) != 0 {
            return None;
        }
        Some(unsafe { &*(ptr as *const CursorChannel) })
    }

    /// Host: records the latest `WM_MOUSEMOVE` and bumps `move_seq`, returning the new
    /// sequence number. Uses `SeqCst` so the paired `move_acked` check cannot miss an ack.
    pub fn record_move(&self, wparam: u32, cx: i32, cy: i32) -> u32 {
        self.move_wparam.store(wparam, Ordering::Relaxed);
        self.move_client_x.store(cx as u32, Ordering::Relaxed);
        self.move_client_y.store(cy as u32, Ordering::Relaxed);
        self.move_seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }

    /// Host: whether `posted_seq` has been acknowledged by the hook (wrapping comparison).
    pub fn is_move_acked(&self, posted_seq: u32) -> bool {
        let acked = self.move_acked.load(Ordering::SeqCst);
        acked.wrapping_sub(posted_seq) < 0x8000_0000
    }

    /// Host: whether the hook's `focus_proc` is actively acknowledging coalesced moves.
    pub fn coalescing_active(&self) -> bool {
        self.coalescing.load(Ordering::Acquire) != 0
    }

    /// Hook: acknowledges the latest host `move_seq` before reading live cursor coordinates.
    pub fn ack_latest_move(&self) -> u32 {
        if self.coalescing.load(Ordering::Relaxed) == 0 {
            self.coalescing.store(1, Ordering::Release);
        }
        let seq = self.move_seq.load(Ordering::SeqCst);
        self.move_acked.store(seq, Ordering::SeqCst);
        seq
    }

    /// Hook: latest fallback client coordinates written by the host.
    pub fn latest_move_xy(&self) -> (i32, i32) {
        (self.move_client_x.load(Ordering::Relaxed) as i32, self.move_client_y.load(Ordering::Relaxed) as i32)
    }

    /// Hook: publishes cursor state (`handle`, `flags`, `clip_bits`, `shape_gen`) via
    /// seqlock on `cursor_seq`. Single writer (serialized by the hook's cursor mutex).
    pub fn publish_feedback(&self, fb: &CursorFeedback, shape_gen: u32) {
        let odd = self.cursor_seq.load(Ordering::Acquire).wrapping_add(1) | 1;
        self.cursor_seq.store(odd, Ordering::Release);
        fence(Ordering::Release);
        put(&self.cursor_handle_lo, fb.handle as u32);
        put(&self.cursor_handle_hi, (fb.handle >> 32) as u32);
        put(&self.cursor_flags, fb.flags);
        for (dst, src) in self.clip_bits.iter().zip(fb.clip_bits) {
            put(dst, src);
        }
        put(&self.shape_gen, shape_gen);
        self.cursor_seq.store(odd.wrapping_add(1), Ordering::Release);
    }

    /// Host: reads a tear-free cursor feedback snapshot and its shape generation, or
    /// `None` if never published (`seq == 0`) or torn mid-write.
    pub fn read_feedback(&self) -> Option<(CursorFeedback, u32)> {
        for _ in 0..8 {
            let seq = self.cursor_seq.load(Ordering::Acquire);
            if seq == 0 {
                return None;
            }
            if seq & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let handle = u64::from(get(&self.cursor_handle_lo)) | (u64::from(get(&self.cursor_handle_hi)) << 32);
            let mut flags = get(&self.cursor_flags);
            if self.coalescing.load(Ordering::Relaxed) != 0 {
                flags |= CURSOR_COALESCING;
            }
            let clip_bits = [get(&self.clip_bits[0]), get(&self.clip_bits[1]), get(&self.clip_bits[2]), get(&self.clip_bits[3])];
            let shape_gen = get(&self.shape_gen);
            fence(Ordering::Acquire);
            if self.cursor_seq.load(Ordering::Acquire) == seq {
                return Some((CursorFeedback { seq, handle, flags, clip_bits }, shape_gen));
            }
        }
        None
    }
}

impl Default for CursorChannel {
    fn default() -> CursorChannel {
        CursorChannel::new()
    }
}

fn put(word: &AtomicU32, v: u32) {
    word.store(v, Ordering::Release);
}

fn get(word: &AtomicU32) -> u32 {
    word.load(Ordering::Acquire)
}

impl InputHeader {
    /// A zeroed block, for one that isn't shared memory (tests, one process).
    pub fn new() -> InputHeader {
        // SAFETY: every field is an AtomicU32, for which all-zero bytes are valid.
        unsafe { std::mem::zeroed() }
    }

    /// The block at the start of a mapping.
    ///
    /// # Safety
    /// `base` must point to at least `len` bytes that stay mapped, readable and (for a
    /// writer) writable for `'a`, and that other processes touch only through atomic
    /// word accesses (this layout). None when `len` is too short or `base` misaligned.
    pub unsafe fn from_raw<'a>(base: *const u8, len: usize) -> Option<&'a InputHeader> {
        if base.is_null() || len < INPUT_BLOCK_SIZE || base.align_offset(std::mem::align_of::<InputHeader>()) != 0 {
            return None;
        }
        // SAFETY: the caller's promise, and the checks above.
        Some(unsafe { &*(base as *const InputHeader) })
    }

    /// The writer's start on a new block: its magic, after everything else is zero.
    pub fn init(&self) {
        self.magic.store(INPUT_MAGIC, Ordering::Release);
    }

    pub fn valid_magic(&self) -> bool {
        self.magic.load(Ordering::Acquire) == INPUT_MAGIC
    }

    /// The mode now: the one load an input call makes while it passes through. A mode
    /// this build doesn't know passes through (the game keeps its input).
    pub fn mode(&self) -> InputMode {
        InputMode::from_u32(get(&self.mode)).unwrap_or_default()
    }

    /// Marks a snapshot as being written; returns the odd sequence to give `end_write`.
    /// A sequence left odd by a writer that died mid-write moves on to the next odd one.
    fn begin_write(&self) -> u32 {
        let odd = self.seq.load(Ordering::Acquire).wrapping_add(1) | 1;
        self.seq.store(odd, Ordering::Release);
        fence(Ordering::Release);
        odd
    }

    /// Marks the snapshot begun at `odd` as complete.
    fn end_write(&self, odd: u32) {
        self.seq.store(odd.wrapping_add(1), Ordering::Release);
    }

    /// Writes a snapshot's words (its `seq` aside). Call between `begin_write` and
    /// `end_write`.
    fn set_words(&self, s: &InputSnapshot) {
        put(&self.flags, s.flags);
        let (p, w) = (&s.pad, &self.pad);
        put(&w.connected, u32::from(p.connected));
        put(&w.buttons, p.buttons);
        put(&w.left_trigger, p.left_trigger.into());
        put(&w.right_trigger, p.right_trigger.into());
        put(&w.thumb_lx, i32::from(p.thumb_lx) as u32);
        put(&w.thumb_ly, i32::from(p.thumb_ly) as u32);
        put(&w.thumb_rx, i32::from(p.thumb_rx) as u32);
        put(&w.thumb_ry, i32::from(p.thumb_ry) as u32);
        put(&w.packet_number, p.packet_number);
        put(&w.xinput_slot, p.xinput_slot.map_or(0, |k| u32::from(k) + 1));
        put(&w.hid_key_lo, p.hid_key as u32);
        put(&w.hid_key_hi, (p.hid_key >> 32) as u32);
        let (m, w) = (&s.mouse, &self.mouse);
        put(&w.active, u32::from(m.active));
        put(&w.cursor_x, m.cursor_x as u32);
        put(&w.cursor_y, m.cursor_y as u32);
        put(&w.delta_x, m.delta_x as u32);
        put(&w.delta_y, m.delta_y as u32);
        put(&w.wheel_delta, m.wheel_delta as u32);
        put(&w.buttons, m.buttons);
        let (k, w) = (&s.keyboard, &self.keyboard);
        put(&w.active, u32::from(k.active));
        for (word, v) in w.keys.iter().zip(k.keys) {
            put(word, v);
        }
        let (r, w) = (&s.wheel, &self.wheel);
        put(&w.connected, u32::from(r.connected));
        put(&w.steer, r.steer as u32);
        put(&w.throttle, r.throttle.into());
        put(&w.brake, r.brake.into());
        put(&w.clutch, r.clutch.into());
        put(&w.buttons, r.buttons);
        let (pn, pw) = (&s.pane, &self.pane);
        put(&pw.dest_x, pn.dest_x as u32);
        put(&pw.dest_y, pn.dest_y as u32);
        put(&pw.dest_w, pn.dest_w);
        put(&pw.dest_h, pn.dest_h);
        for (dst, src) in pw.uv_bits.iter().zip(pn.uv_bits) {
            put(dst, src);
        }
        put(&pw.compositor_hwnd_lo, pn.compositor_hwnd as u32);
        put(&pw.compositor_hwnd_hi, (pn.compositor_hwnd >> 32) as u32);
        put(&pw.flags, pn.flags);
        // The mode last: a reader that only checks it sees the new devices' state with it.
        put(&self.mode, s.mode.as_u32());
    }

    /// The words now, whether or not they're mid-change.
    fn read(&self) -> InputSnapshot {
        let seq = self.seq.load(Ordering::Acquire);
        let (p, m, k, r, pn) = (&self.pad, &self.mouse, &self.keyboard, &self.wheel, &self.pane);
        let mut keys = [0u32; KEY_WORDS];
        for (v, word) in keys.iter_mut().zip(&k.keys) {
            *v = get(word);
        }
        InputSnapshot {
            seq,
            mode: InputMode::from_u32(get(&self.mode)).unwrap_or_default(),
            flags: get(&self.flags),
            pad: VirtualPad {
                connected: get(&p.connected) != 0,
                buttons: get(&p.buttons),
                left_trigger: get(&p.left_trigger) as u8,
                right_trigger: get(&p.right_trigger) as u8,
                thumb_lx: get(&p.thumb_lx) as i16,
                thumb_ly: get(&p.thumb_ly) as i16,
                thumb_rx: get(&p.thumb_rx) as i16,
                thumb_ry: get(&p.thumb_ry) as i16,
                packet_number: get(&p.packet_number),
                xinput_slot: get(&p.xinput_slot).checked_sub(1).and_then(|k| u8::try_from(k).ok()),
                hid_key: u64::from(get(&p.hid_key_lo)) | (u64::from(get(&p.hid_key_hi)) << 32),
            },
            mouse: VirtualMouse {
                active: get(&m.active) != 0,
                cursor_x: get(&m.cursor_x) as i32,
                cursor_y: get(&m.cursor_y) as i32,
                delta_x: get(&m.delta_x) as i32,
                delta_y: get(&m.delta_y) as i32,
                wheel_delta: get(&m.wheel_delta) as i32,
                buttons: get(&m.buttons),
            },
            keyboard: VirtualKeyboard { active: get(&k.active) != 0, keys },
            wheel: VirtualWheel {
                connected: get(&r.connected) != 0,
                steer: get(&r.steer) as i32,
                throttle: get(&r.throttle) as u16,
                brake: get(&r.brake) as u16,
                clutch: get(&r.clutch) as u16,
                buttons: get(&r.buttons),
            },
            pane: PaneProjection {
                dest_x: get(&pn.dest_x) as i32,
                dest_y: get(&pn.dest_y) as i32,
                dest_w: get(&pn.dest_w),
                dest_h: get(&pn.dest_h),
                uv_bits: [get(&pn.uv_bits[0]), get(&pn.uv_bits[1]), get(&pn.uv_bits[2]), get(&pn.uv_bits[3])],
                compositor_hwnd: u64::from(get(&pn.compositor_hwnd_lo)) | (u64::from(get(&pn.compositor_hwnd_hi)) << 32),
                flags: get(&pn.flags),
            },
        }
    }

    /// Publishes a snapshot (its `seq` is ignored: the block keeps its own). One writer
    /// at a time.
    pub fn publish(&self, snapshot: &InputSnapshot) {
        let odd = self.begin_write();
        self.set_words(snapshot);
        self.end_write(odd);
    }

    /// A whole snapshot, or None while one is being written or when one was written
    /// during the copy (keep the last, or try again).
    pub fn read_stable(&self) -> Option<InputSnapshot> {
        let snapshot = self.read();
        self.unchanged_since(&snapshot).then_some(snapshot)
    }

    /// Whether `snapshot` was read whole: its sequence was even, and nothing was
    /// written since.
    fn unchanged_since(&self, snapshot: &InputSnapshot) -> bool {
        fence(Ordering::Acquire);
        snapshot.seq & 1 == 0 && self.seq.load(Ordering::Acquire) == snapshot.seq
    }
}

impl Default for InputHeader {
    fn default() -> InputHeader {
        InputHeader::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot with every field set from `i`, its signed ones negative, so a mix of
    /// two shows.
    fn snapshot(i: u32) -> InputSnapshot {
        let n = i as i32;
        let mut keyboard = VirtualKeyboard { active: true, keys: [i; KEY_WORDS] };
        keyboard.set(0xff, true);
        InputSnapshot {
            seq: 0,
            mode: InputMode::Virtual,
            flags: i,
            pad: VirtualPad {
                connected: true,
                buttons: i,
                left_trigger: i as u8,
                right_trigger: !i as u8,
                thumb_lx: (i as i16).wrapping_neg(),
                thumb_ly: i as i16,
                thumb_rx: i16::MIN,
                thumb_ry: i16::MAX,
                packet_number: i,
                xinput_slot: Some((i % 4) as u8),
                hid_key: u64::from(i) << 32 | 0x1234,
            },
            mouse: VirtualMouse { active: true, cursor_x: n.wrapping_neg(), cursor_y: n, delta_x: n.wrapping_mul(3), delta_y: n.wrapping_neg(), wheel_delta: n.wrapping_mul(120), buttons: MOUSE_LEFT | MOUSE_X2 },
            keyboard,
            wheel: VirtualWheel { connected: true, steer: n.wrapping_neg(), throttle: i as u16, brake: u16::MAX, clutch: !i as u16, buttons: i },
            pane: PaneProjection {
                dest_x: n.wrapping_neg(),
                dest_y: n,
                dest_w: 960,
                dest_h: 1080,
                uv_bits: PaneProjection::uv_to_bits([0.1, 0.0, 0.9, 1.0]),
                compositor_hwnd: u64::from(i) << 16 | 0x42,
                flags: POINTER_LIVE_OS | POINTER_COMPOSITOR_FRONT,
            },
        }
    }

    #[test]
    fn the_layout_is_the_one_both_sides_use() {
        assert_eq!(std::mem::size_of::<InputHeader>(), INPUT_BLOCK_SIZE);
        let h = InputHeader::new();
        let base = &h as *const InputHeader as usize;
        let at = |p: &AtomicU32| p as *const AtomicU32 as usize - base;
        assert_eq!([at(&h.magic), at(&h.seq), at(&h.mode), at(&h.flags)], [0, 4, 8, 12]);
        let p = &h.pad;
        assert_eq!(
            [at(&p.connected), at(&p.buttons), at(&p.left_trigger), at(&p.right_trigger), at(&p.thumb_lx), at(&p.thumb_ly), at(&p.thumb_rx), at(&p.thumb_ry), at(&p.packet_number)],
            [16, 20, 24, 28, 32, 36, 40, 44, 48]
        );
        assert_eq!([at(&p.xinput_slot), at(&p.hid_key_lo), at(&p.hid_key_hi)], [52, 56, 60]);
        let m = &h.mouse;
        assert_eq!([at(&m.active), at(&m.cursor_x), at(&m.cursor_y), at(&m.delta_x), at(&m.delta_y), at(&m.wheel_delta), at(&m.buttons)], [64, 68, 72, 76, 80, 84, 88]);
        assert_eq!([at(&h.keyboard.active), at(&h.keyboard.keys[0]), at(&h.keyboard.keys[7])], [96, 100, 128]);
        let w = &h.wheel;
        assert_eq!([at(&w.connected), at(&w.steer), at(&w.throttle), at(&w.brake), at(&w.clutch), at(&w.buttons)], [144, 148, 152, 156, 160, 164]);
        assert_eq!([at(&h.pane.dest_x), at(&h.pane.flags)], [176, 216]);
        assert_eq!([at(&h.reserved[0]), at(&h.reserved[8])], [220, 252]);
        assert_eq!(INPUT_MAGIC.to_le_bytes(), *b"PPIN");
        for m in [InputMode::PassThrough, InputMode::BlockAll, InputMode::Virtual] {
            assert_eq!(InputMode::from_u32(m.as_u32()), Some(m));
        }
        assert_eq!([InputMode::PassThrough.as_u32(), InputMode::BlockAll.as_u32(), InputMode::Virtual.as_u32()], [0, 1, 2]);
        assert_eq!(InputMode::from_u32(3), None);
    }

    #[test]
    fn snapshots_publish_and_come_back() {
        let h = InputHeader::new();
        assert!(!h.valid_magic());
        h.init();
        assert!(h.valid_magic());
        assert_eq!(h.mode(), InputMode::PassThrough, "a new block passes through");
        assert_eq!(h.read_stable(), Some(InputSnapshot::default()));
        for i in [1, 0x8000, 0xdead_beef, u32::MAX] {
            let s = snapshot(i);
            h.publish(&InputSnapshot { seq: 12345, ..s });
            let got = h.read_stable().unwrap();
            assert_eq!(got, InputSnapshot { seq: got.seq, ..s }, "{i:#x}");
            assert_eq!(got.seq & 1, 0);
        }
        assert_eq!(h.read_stable().unwrap().seq, 8, "two per publish, whatever the snapshot said");
        assert_eq!(h.mode(), InputMode::Virtual);
        // The words as the hook reads them: signed values as their bits.
        assert_eq!(h.pad.thumb_rx.load(Ordering::Relaxed), i32::from(i16::MIN) as u32);
        assert_eq!(h.mouse.buttons.load(Ordering::Relaxed), 0b1_0001);
        // A mode from a newer hub passes through.
        h.mode.store(7, Ordering::Relaxed);
        assert_eq!(h.mode(), InputMode::PassThrough);
        assert_eq!(h.read_stable().unwrap().mode, InputMode::PassThrough);
        // Back to nothing.
        h.publish(&InputSnapshot { mode: InputMode::BlockAll, ..Default::default() });
        let s = h.read_stable().unwrap();
        assert_eq!(InputSnapshot { seq: 0, ..s }, InputSnapshot { mode: InputMode::BlockAll, ..Default::default() });
    }

    #[test]
    fn a_snapshot_written_during_its_copy_is_not_taken() {
        let h = InputHeader::new();
        h.init();
        h.publish(&snapshot(1));
        assert!(h.read_stable().is_some());
        // The writer is halfway through when the reader looks.
        let odd = h.begin_write();
        assert_eq!(odd & 1, 1);
        h.set_words(&snapshot(2));
        assert_eq!(h.read_stable(), None);
        h.end_write(odd);
        assert_eq!(h.read_stable().map(|s| s.pad.buttons), Some(2));
        // Or finishes one between the reader's two looks.
        let copy = h.read();
        assert!(h.unchanged_since(&copy));
        h.publish(&snapshot(3));
        assert!(!h.unchanged_since(&copy), "a copy taken across a publish");
        // Or starts one between them.
        let copy = h.read();
        let odd = h.begin_write();
        assert!(!h.unchanged_since(&copy));
        h.end_write(odd);
        assert!(!h.unchanged_since(&copy), "and finishes it");
        // A writer that died mid-write leaves the sequence odd: the next one still ends even.
        h.seq.store(41, Ordering::Relaxed);
        assert_eq!(h.read_stable(), None);
        h.publish(&snapshot(4));
        assert_eq!(h.read_stable().map(|s| s.seq), Some(44));
        // The sequence wraps without ever reading as complete mid-write.
        for start in [u32::MAX - 1, u32::MAX] {
            h.seq.store(start, Ordering::Relaxed);
            let odd = h.begin_write();
            assert_eq!(odd & 1, 1, "{start}");
            assert_eq!(h.read_stable(), None);
            h.end_write(odd);
            assert_eq!(h.read_stable().map(|s| s.seq & 1), Some(0), "{start}");
        }
    }

    #[test]
    fn keys_buttons_and_motion() {
        let mut k = VirtualKeyboard::default();
        for vk in 0..=255u8 {
            assert!(!k.is_down(vk));
        }
        for vk in [0u8, 1, 31, 32, 0x41, 0xa0, 0xff] {
            k.set(vk, true);
            assert!(k.is_down(vk), "{vk:#x}");
        }
        assert_eq!(k.keys[0], 1 | 2 | 1 << 31);
        assert_eq!(k.keys[1], 1);
        assert_eq!(k.keys[2], 1 << 1, "0x41 is A");
        assert_eq!(k.keys[5], 1, "0xa0 is left shift");
        assert_eq!(k.keys[7], 1 << 31);
        assert_eq!((0..=255u8).filter(|vk| k.is_down(*vk)).count(), 7);
        k.set(0x41, false);
        k.set(0x41, false);
        assert!(!k.is_down(0x41));
        assert!(k.is_down(0xa0), "the others stay");
        k.set(0xff, true);
        assert_eq!(k.keys[7], 1 << 31, "setting twice is once");

        let pad = VirtualPad { buttons: 0x0004_1000, ..Default::default() };
        assert_eq!((pad.xinput_buttons(), pad.extra_buttons()), (0x1000, 4));
        // A device's key is the same whoever computes it, its serial's case aside.
        assert_eq!(hid_key(0x054c, 0x09cc, "AB:CD"), hid_key(0x054c, 0x09cc, " ab:cd "));
        assert_ne!(hid_key(0x054c, 0x09cc, "ab:cd"), hid_key(0x054c, 0x09cc, "ab:ce"));
        assert_ne!(hid_key(0x054c, 0x09cc, ""), 0);
        // Where it's plugged in: one key for every collection of one device, whoever
        // reads its path (any case, with or without the interface's GUID).
        let col1 = r"\\?\HID#VID_054C&PID_0CE6&MI_03&Col01#8&3a1b2c&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        let col2 = r"\\?\hid#vid_054c&pid_0ce6&mi_03&col02#8&3a1b2c&0&0001#{884b96c3-56ef-11d1-bc8c-00a0c91405dd}";
        let other_port = r"\\?\HID#VID_054C&PID_0CE6&MI_03&Col01#8&99ffee&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(hid_instance(col1).as_deref(), Some("vid_054c&pid_0ce6&mi_03#8&3a1b2c&0"));
        assert_eq!(hid_instance_key(0x054c, 0x0ce6, col1), hid_instance_key(0x054c, 0x0ce6, col2));
        assert_ne!(hid_instance_key(0x054c, 0x0ce6, col1), hid_instance_key(0x054c, 0x0ce6, other_port), "an identical pad elsewhere");
        assert_ne!(hid_instance_key(0x054c, 0x0ce6, col1), Some(hid_key(0x054c, 0x0ce6, "")));
        let bluetooth = r"\\?\HID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002054c_PID&0ce6#9&2b3c4d&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert!(hid_instance(bluetooth).is_some());
        assert_eq!(hid_instance(r"\\?\USB#VID_045E&PID_028E#123#{x}"), None, "not a HID path");
        assert_eq!(hid_instance(r"C:\saves\slot1.dat"), None);

        let earlier = VirtualMouse { delta_x: i32::MAX - 1, delta_y: 10, wheel_delta: -120, ..Default::default() };
        let now = VirtualMouse { delta_x: i32::MIN + 2, delta_y: 4, wheel_delta: 120, buttons: MOUSE_RIGHT | MOUSE_MIDDLE, ..Default::default() };
        assert_eq!(now.motion_since(&earlier), (4, -6, 240), "running totals wrap");
        assert!(now.is_down(MOUSE_RIGHT) && now.is_down(MOUSE_MIDDLE));
        assert!(!now.is_down(MOUSE_LEFT) && !now.is_down(MOUSE_X1) && !now.is_down(MOUSE_X2));
    }

    #[test]
    fn a_block_over_raw_memory() {
        let words: Vec<AtomicU32> = (0..INPUT_BLOCK_SIZE / 4 + 1).map(|_| AtomicU32::new(0)).collect();
        let base = words.as_ptr() as *const u8;
        // SAFETY: the vector lives through the test, and only atomics touch it.
        let h = unsafe { InputHeader::from_raw(base, words.len() * 4) }.unwrap();
        h.init();
        h.publish(&snapshot(5));
        assert_eq!(words[0].load(Ordering::Acquire), INPUT_MAGIC);
        assert_eq!(words[1].load(Ordering::Acquire), 2, "the sequence at 4");
        assert_eq!(words[2].load(Ordering::Acquire), InputMode::Virtual.as_u32(), "the mode at 8");
        assert_eq!(words[5].load(Ordering::Acquire), 5, "the pad's buttons at 20");
        assert_eq!(words[64].load(Ordering::Acquire), 0, "nothing past the block");
        assert!(unsafe { InputHeader::from_raw(base, INPUT_BLOCK_SIZE) }.is_some(), "exactly the block");
        assert!(unsafe { InputHeader::from_raw(base, INPUT_BLOCK_SIZE - 1) }.is_none(), "too short");
        assert!(unsafe { InputHeader::from_raw(base.wrapping_add(2), 512) }.is_none(), "misaligned");
        assert!(unsafe { InputHeader::from_raw(std::ptr::null(), 512) }.is_none());
    }

    /// A writer and readers on their own threads: a reader only ever takes snapshots from
    /// one publish, and their sequences never go back.
    #[test]
    fn readers_only_take_whole_snapshots() {
        const WRITES: u32 = 50_000;
        let shared = std::sync::Arc::new(InputHeader::new());
        shared.init();
        let w = shared.clone();
        let writer = std::thread::spawn(move || {
            for i in 1..=WRITES {
                w.publish(&snapshot(i));
            }
        });
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let h = shared.clone();
                std::thread::spawn(move || {
                    let (mut last_seq, mut taken) = (0u32, 0u32);
                    loop {
                        let done = h.seq.load(Ordering::Acquire) == WRITES * 2;
                        if let Some(s) = h.read_stable() {
                            assert_eq!(s.seq & 1, 0);
                            assert!(s.seq >= last_seq, "{} after {last_seq}", s.seq);
                            last_seq = s.seq;
                            if s.seq != 0 {
                                let i = s.seq / 2;
                                assert_eq!(s, InputSnapshot { seq: s.seq, ..snapshot(i) }, "a snapshot from one publish");
                                taken += 1;
                            }
                        }
                        if done {
                            return taken;
                        }
                    }
                })
            })
            .collect();
        writer.join().unwrap();
        for r in readers {
            assert!(r.join().unwrap() > 0);
        }
        assert_eq!(shared.read_stable(), Some(InputSnapshot { seq: WRITES * 2, ..snapshot(WRITES) }));
    }

    #[test]
    fn pane_projection_round_trips_and_maps_clip() {
        let p = PaneProjection {
            dest_x: 960,
            dest_y: 0,
            dest_w: 960,
            dest_h: 1080,
            uv_bits: PaneProjection::uv_to_bits([0.25, 0.0, 0.75, 1.0]),
            compositor_hwnd: 0x1234,
            flags: POINTER_LIVE_OS | POINTER_COMPOSITOR_FRONT,
        };
        assert!(p.live_os() && p.compositor_front());
        // Middle of pane maps to middle of client (even with Fill UV crop).
        assert_eq!(p.project_to_client(960 + 480, 540, 1920, 1080), Some((960, 540)));
        assert_eq!(p.unproject_from_client(960, 540, 1920, 1080), Some((960 + 480, 540)));
        // Left edge of pane maps to u=0.25 of client (x=480), and unprojects back to pane left.
        assert_eq!(p.project_to_client(960, 0, 1920, 1080), Some((480, 0)));
        assert_eq!(p.unproject_from_client(480, 0, 1920, 1080), Some((960, 0)));
        // Outside pane clamps cleanly to the crop edges.
        assert_eq!(p.project_to_client(100, -50, 1920, 1080), Some((480, 0)));
        assert_eq!(p.project_to_client(2500, 2000, 1920, 1080), Some((1440, 1079)));
        // Full client clip maps to full pane dest; sub-rect inside crop maps proportionally.
        assert_eq!(p.project_clip_rect([0.0, 0.0, 1.0, 1.0]), Some([960, 0, 1920, 1080]));
        assert_eq!(p.project_clip_rect([0.25, 0.25, 0.75, 0.75]), Some([960, 270, 1920, 810]));
    }

    #[test]
    fn cursor_channel_coalesces_moves_and_publishes_feedback() {
        let words: Vec<AtomicU32> = (0..(CURSOR_CHANNEL_OFFSET + CURSOR_CHANNEL_SIZE) / 4).map(|_| AtomicU32::new(0)).collect();
        let base = words.as_ptr() as *const u8;
        assert!(unsafe { CursorChannel::from_mapping(base, INPUT_BLOCK_SIZE) }.is_none(), "256-byte block has no channel");
        let ch = unsafe { CursorChannel::from_mapping(base, words.len() * 4) }.unwrap();
        assert!(!ch.coalescing_active());
        assert_eq!(ch.read_feedback(), None);

        // Host posts move 1 -> in flight until hook acks.
        let s1 = ch.record_move(1, 10, 20);
        assert_eq!(s1, 1);
        assert!(!ch.is_move_acked(s1));
        assert_eq!(ch.latest_move_xy(), (10, 20));
        // Host records move 2 while move 1 is still in flight.
        let s2 = ch.record_move(1, 30, 40);
        assert_eq!(s2, 2);
        assert!(!ch.is_move_acked(s1));
        // Hook handles move 1 and acks the latest seq (2): s1 and s2 are both acked.
        assert_eq!(ch.ack_latest_move(), 2);
        assert!(ch.coalescing_active());
        assert!(ch.is_move_acked(s1));
        assert!(ch.is_move_acked(s2));

        // Feedback seqlock round-trip.
        let fb = CursorFeedback {
            seq: 0,
            handle: 0x0001_0003,
            flags: CURSOR_HAS_SHAPE | CURSOR_CLIPPED | CURSOR_HAS_CLIP_RECT,
            clip_bits: [0.0f32.to_bits(), 0.25f32.to_bits(), 1.0f32.to_bits(), 0.75f32.to_bits()],
        };
        ch.publish_feedback(&fb, 7);
        let (got, shape_gen) = ch.read_feedback().unwrap();
        assert_eq!(shape_gen, 7);
        assert_eq!(got.handle, 0x0001_0003);
        assert!(got.has_shape() && got.clipped() && got.coalescing() && !got.hidden_shape());
        assert_eq!(got.clip_norm(), Some([0.0, 0.25, 1.0, 0.75]));
    }
}
