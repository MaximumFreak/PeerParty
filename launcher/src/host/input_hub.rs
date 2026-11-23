//! A session's input, read once for every copy of the game in it: the controllers
//! (pad.rs, every API there is), the keyboards and mice apart (Windows' raw input, by
//! device handle: `RawInput`), and wheels as readers for them come. Each copy that has a
//! virtual input block (`InputBlock`, `peerparty_protocol::input_shm`) gets what its
//! `InputAssignment` names written into it (`publish`), and the hook in that copy answers
//! the game's input calls from it:
//!
//! | assignment | the copy's block |
//! |---|---|
//! | `All` | `PassThrough`: the game reads the real devices |
//! | `None` | `BlockAll` |
//! | `Controller { index }` | `Virtual`: that controller (`Pads::readings`' seat) as its only pad |
//! | `KeyboardMouse { device_id }` | `Virtual`: that keyboard, and the mouse paired with it (`pair_mouse`; the mouse of the same id unpaired) |
//! | `KeyboardMouse { ANY_KEYBOARD }` | `Virtual`, no controller: every keyboard and mouse together while its pane has the focus |
//! | `Wheel { device_id }` | `Virtual`: that wheel |
//!
//! While the overlay is open every block says `BlockAll`: the overlay has the input.
//!
//! A copy alone (`host::run`) has no block: its hook holds the controllers back while
//! the overlay shows, as before (`HostMsg::Overlay`).

use std::collections::HashMap;
use std::path::Path;

use peerparty_protocol::input_shm::{INPUT_BLOCK_SIZE, InputHeader, InputMode, InputSnapshot, VirtualKeyboard, VirtualMouse, VirtualPad, VirtualWheel};
use peerparty_protocol::session::{ANY_KEYBOARD, InputAssignment};
use peerparty_protocol::transport::{FileShm, ShmBacking, token_from_bytes};

use super::instance::InstanceConn;
use super::pad;

/// One copy's virtual input block, in shared memory the hook maps: a named mapping
/// (`create`), or a file both sides map (`create_file`: a Linux launcher and a game under
/// Wine). The hook gets its `ShmBacking` string in `HostMsg::InputShm`.
pub struct InputBlock {
    /// Where the hook finds it (a `ShmBacking` string).
    pub name: String,
    #[cfg(windows)]
    map: windows_sys::Win32::Foundation::HANDLE,
    /// The mapping itself (its view is `view`), held for as long as this is.
    #[cfg(unix)]
    #[allow(dead_code)]
    map: crate::backend::linux::overlay::Mmap,
    view: *mut u8,
    /// The file behind a `create_file` block, deleted with it.
    file: Option<FileShm>,
    /// The last snapshot published, so an unchanged one isn't published again.
    last: Option<InputSnapshot>,
    /// The last pad identity `(connected, xinput_slot, hid_key)` seen in `Virtual` mode,
    /// so a controller arriving, switching, or leaving notifies the game once (not when
    /// the overlay opens or closes).
    last_pad: Option<(bool, Option<u8>, u64)>,
}

/// The size mapped: the block, rounded to a page (what a mapping takes anyway).
const MAPPED: usize = 4096;
const _: () = assert!(MAPPED >= INPUT_BLOCK_SIZE);

impl InputBlock {
    /// A named mapping for the game's process `pid`, named after it and random digits
    /// (nothing can open it by guessing, or make it first: one that exists is refused).
    #[cfg(windows)]
    pub fn create(pid: u32) -> Option<InputBlock> {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
        let name = format!(r"Local\peerparty-input-{pid}-{}", token_from_bytes(&peerparty_core::inet::crypto::random_array::<8>()));
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let map = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, MAPPED as u32, wide.as_ptr()) };
        if map.is_null() {
            return None;
        }
        // ERROR_ALREADY_EXISTS: someone else's block of that name.
        if unsafe { GetLastError() } == 183 {
            unsafe { CloseHandle(map) };
            return None;
        }
        InputBlock::map(ShmBacking::WinMapping { name }, map, None)
    }

    /// The same block in a file of its own in `dir`, deleted when this is dropped.
    #[cfg(windows)]
    pub fn create_file(dir: &Path, pid: u32) -> Option<InputBlock> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};
        let file = FileShm::create(&block_path(dir, pid), MAPPED as u64).ok()?;
        let map = unsafe { CreateFileMappingW(file.file.as_raw_handle(), std::ptr::null(), PAGE_READWRITE, 0, 0, std::ptr::null()) };
        if map.is_null() {
            let _ = file.remove();
            return None;
        }
        let backing = ShmBacking::FileMmap { path: file.path.to_string_lossy().into_owned() };
        InputBlock::map(backing, map, Some(file))
    }

    #[cfg(windows)]
    fn map(backing: ShmBacking, map: windows_sys::Win32::Foundation::HANDLE, file: Option<FileShm>) -> Option<InputBlock> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{FILE_MAP_ALL_ACCESS, MapViewOfFile};
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, MAPPED) };
        if view.Value.is_null() {
            unsafe { CloseHandle(map) };
            if let Some(f) = file {
                let _ = f.remove();
            }
            return None;
        }
        let b = InputBlock { name: backing.to_env_string(), map, view: view.Value.cast(), file, last: None, last_pad: None };
        b.header().init();
        Some(b)
    }

    /// The block in a file in `dir`, mapped with `mmap` (the file is all there is off
    /// Windows).
    #[cfg(unix)]
    pub fn create_file(dir: &Path, pid: u32) -> Option<InputBlock> {
        let file = FileShm::create(&block_path(dir, pid), MAPPED as u64).ok()?;
        let map = match crate::backend::linux::overlay::Mmap::shared(&file.file, MAPPED) {
            Ok(m) => m,
            Err(_) => {
                let _ = file.remove();
                return None;
            }
        };
        let name = ShmBacking::FileMmap { path: file.path.to_string_lossy().into_owned() }.to_env_string();
        let b = InputBlock { name, view: map.as_ptr(), map, file: Some(file), last: None, last_pad: None };
        b.header().init();
        Some(b)
    }

    /// The block's words. A view starts on a page, so it's aligned.
    pub fn header(&self) -> &InputHeader {
        // SAFETY: the view is mapped (read and write) for as long as self, and holds
        // MAPPED bytes; the hook touches it only through its words.
        unsafe { InputHeader::from_raw(self.view, MAPPED) }.expect("a mapped view is aligned and long enough")
    }

    /// Publishes `s`, unless it's the one published last. Whether it was published.
    pub fn publish(&mut self, s: &InputSnapshot) -> bool {
        if self.last.as_ref() == Some(s) {
            return false;
        }
        self.header().publish(s);
        self.last = Some(*s);
        true
    }

    /// Whether this copy's assigned controller arrived, switched, or went away in `s`.
    /// Ignores `BlockAll` (overlay open) so opening or closing the overlay doesn't look
    /// like an unplug.
    pub fn note_pad(&mut self, s: &InputSnapshot) -> bool {
        if s.mode != InputMode::Virtual {
            return false;
        }
        let now = (s.pad.connected, s.pad.xinput_slot, s.pad.hid_key);
        let prev = self.last_pad.replace(now);
        match prev {
            None => now.0,
            Some(old) => old != now,
        }
    }
}

impl Drop for InputBlock {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            windows_sys::Win32::Foundation::CloseHandle(self.map);
        }
        if let Some(f) = self.file.take() {
            let _ = f.remove();
        }
    }
}

/// A block file's name in `dir`, for a game's process.
fn block_path(dir: &Path, pid: u32) -> std::path::PathBuf {
    dir.join(format!("peerparty-input-{pid}-{}.bin", token_from_bytes(&peerparty_core::inet::crypto::random_array::<8>())))
}

/// The devices of a session as they are now, each apart.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Devices {
    /// The controllers, by index (`Pads::readings`' order).
    pub pads: Vec<VirtualPad>,
    /// Each controller's log name ("XInput slot 1", "HID 054c:0ce6"), matching `pads`.
    pub pad_names: Vec<Option<String>>,
    pub keyboards: HashMap<u64, VirtualKeyboard>,
    pub mice: HashMap<u64, VirtualMouse>,
    pub wheels: HashMap<u64, VirtualWheel>,
    /// A keyboard's mouse, by their ids: `KeyboardMouse { device_id }` is the keyboard
    /// `device_id` and this mouse.
    pub pairs: HashMap<u64, u64>,
}

impl Devices {
    /// The mouse that goes with keyboard `id`: the one paired with it, else the mouse of
    /// the same id.
    fn mouse_of(&self, id: u64) -> u64 {
        self.pairs.get(&id).copied().unwrap_or(id)
    }

    /// Combined state of every connected keyboard and mouse, for the focused pane of a
    /// split-screen session when it isn't bound to one specific device.
    pub fn combined_kbm(&self, cursor: Option<(i32, i32)>) -> (VirtualKeyboard, VirtualMouse) {
        let mut kb = VirtualKeyboard { active: true, ..Default::default() };
        for k in self.keyboards.values() {
            for (dst, src) in kb.keys.iter_mut().zip(&k.keys) {
                *dst |= *src;
            }
        }
        let mut ms = VirtualMouse { active: true, ..Default::default() };
        let mut any_cursor = false;
        for m in self.mice.values() {
            ms.buttons |= m.buttons;
            ms.delta_x = ms.delta_x.wrapping_add(m.delta_x);
            ms.delta_y = ms.delta_y.wrapping_add(m.delta_y);
            ms.wheel_delta = ms.wheel_delta.wrapping_add(m.wheel_delta);
            if !any_cursor {
                ms.cursor_x = m.cursor_x;
                ms.cursor_y = m.cursor_y;
                any_cursor = true;
            }
        }
        if let Some((cx, cy)) = cursor {
            ms.cursor_x = cx;
            ms.cursor_y = cy;
        }
        (kb, ms)
    }
}

/// What a copy with `assignment` sees now (`overlay_open`: the overlay has the input).
pub fn route(assignment: &InputAssignment, overlay_open: bool, d: &Devices) -> InputSnapshot {
    let mut s = InputSnapshot::default();
    if overlay_open {
        s.mode = InputMode::BlockAll;
        return s;
    }
    match *assignment {
        InputAssignment::All => s.mode = InputMode::PassThrough,
        InputAssignment::None => s.mode = InputMode::BlockAll,
        InputAssignment::Controller { index } => {
            s.mode = InputMode::Virtual;
            s.pad = usize::try_from(index).ok().and_then(|i| d.pads.get(i)).copied().unwrap_or_default();
        }
        InputAssignment::KeyboardMouse { device_id } => {
            s.mode = InputMode::Virtual;
            s.keyboard = d.keyboards.get(&device_id).map(|k| VirtualKeyboard { active: true, ..*k }).unwrap_or_default();
            s.mouse = d.mice.get(&d.mouse_of(device_id)).map(|m| VirtualMouse { active: true, ..*m }).unwrap_or_default();
        }
        InputAssignment::Wheel { device_id } => {
            s.mode = InputMode::Virtual;
            s.wheel = d.wheels.get(&device_id).map(|w| VirtualWheel { connected: true, ..*w }).unwrap_or_default();
        }
    }
    s
}

/// XInput's `wButtons` for the logical layout's buttons (pad.rs), Home as the Guide bit
/// `XInputGetStateEx` reports.
pub fn xinput_buttons(logical: u32) -> u32 {
    const MAP: [(u32, u32); 15] = [
        (pad::UP, 0x0001),
        (pad::DOWN, 0x0002),
        (pad::LEFT, 0x0004),
        (pad::RIGHT, 0x0008),
        (pad::START, 0x0010),
        (pad::BACK, 0x0020),
        (pad::LSTICK, 0x0040),
        (pad::RSTICK, 0x0080),
        (pad::LB, 0x0100),
        (pad::RB, 0x0200),
        (pad::HOME, 0x0400),
        (pad::SOUTH, 0x1000),
        (pad::EAST, 0x2000),
        (pad::WEST, 0x4000),
        (pad::NORTH, 0x8000),
    ];
    MAP.iter().filter(|(l, _)| logical & l != 0).fold(0, |b, (_, x)| b | x)
}

/// A stick value, -1..1, as XInput's.
fn thumb(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// A trigger value, 0..1, as XInput's.
fn trigger(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// A controller as read (`Pads::readings`) as a copy's virtual pad, with `packet` as
/// its packet number: an XInput pad's state as XInput gave it, byte for byte, with its
/// slot (the game's XInput calls go to that slot); another pad's translated
/// (`virtual_pad`), with its HID key (the game's reads of that device go through).
pub fn reading_pad(r: &pad::Reading, packet: u32) -> VirtualPad {
    let Some((slot, raw)) = r.xinput else {
        return VirtualPad { hid_key: r.hid_key, ..virtual_pad(&r.state, packet) };
    };
    let i16_at = |at: usize| i16::from_le_bytes([raw[at], raw[at + 1]]);
    VirtualPad {
        connected: true,
        buttons: u32::from(u16::from_le_bytes([raw[0], raw[1]])),
        left_trigger: raw[2],
        right_trigger: raw[3],
        thumb_lx: i16_at(4),
        thumb_ly: i16_at(6),
        thumb_rx: i16_at(8),
        thumb_ry: i16_at(10),
        packet_number: packet,
        xinput_slot: u8::try_from(slot).ok(),
        hid_key: 0,
    }
}

/// A controller's state as a copy's virtual pad: its buttons, both sticks and both
/// triggers, with `packet` as its packet number.
pub fn virtual_pad(state: &pad::State, packet: u32) -> VirtualPad {
    VirtualPad {
        connected: true,
        buttons: xinput_buttons(state.buttons),
        left_trigger: trigger(state.lt),
        right_trigger: trigger(state.rt),
        thumb_lx: thumb(state.lx),
        thumb_ly: thumb(state.ly),
        thumb_rx: thumb(state.rx),
        thumb_ry: thumb(state.ry),
        packet_number: packet,
        xinput_slot: None,
        hid_key: 0,
    }
}

/// The keyboards and mice apart, from their raw input events (`RawInput` on Windows).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kbm {
    pub keyboards: HashMap<u64, VirtualKeyboard>,
    pub mice: HashMap<u64, VirtualMouse>,
    /// The client size of the copy each mouse moves in, its cursor kept inside.
    bounds: HashMap<u64, (u32, u32)>,
    /// Keys and mouse buttons gone down so far (a held key's repeats and the modifiers
    /// not counted): a session sees from it which player plays with the keyboard.
    pub presses: u64,
}

/// Shift, Ctrl, Alt and the Windows keys, either side: held for a hotkey (Ctrl+Alt+1
/// focusing a pane), not playing.
fn modifier(vk: u8) -> bool {
    matches!(vk, 0x10..=0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

/// One raw input event, by device. (Off Windows nothing reads keyboards and mice apart
/// yet: an evdev reader would make these.)
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RawEvent {
    Key { device: u64, vk: u8, down: bool },
    /// Relative motion.
    Motion { device: u64, dx: i32, dy: i32 },
    /// `MOUSE_*` buttons gone down and up.
    Buttons { device: u64, down: u32, up: u32 },
    Wheel { device: u64, delta: i32 },
}

impl Kbm {
    pub fn apply(&mut self, e: RawEvent) {
        match e {
            RawEvent::Key { device, vk, down } => {
                let k = self.keyboards.entry(device).or_default();
                if down && !k.is_down(vk) && !modifier(vk) {
                    self.presses += 1;
                }
                k.set(vk, down);
                // A left or right Shift, Ctrl or Alt is also the key either side reads
                // as (VK_SHIFT ...), down while either is.
                if let 0xA0..=0xA5 = vk {
                    let left = vk & !1;
                    k.set(0x10 + (vk - 0xA0) / 2, k.is_down(left) || k.is_down(left + 1));
                }
            }
            RawEvent::Motion { device, dx, dy } => {
                let bounds = self.bounds.get(&device).copied();
                let m = self.mice.entry(device).or_default();
                m.delta_x = m.delta_x.wrapping_add(dx);
                m.delta_y = m.delta_y.wrapping_add(dy);
                let (w, h) = bounds.unwrap_or((u32::MAX, u32::MAX));
                let max = |n: u32| i32::try_from(n.saturating_sub(1)).unwrap_or(i32::MAX);
                m.cursor_x = m.cursor_x.saturating_add(dx).clamp(0, max(w));
                m.cursor_y = m.cursor_y.saturating_add(dy).clamp(0, max(h));
            }
            RawEvent::Buttons { device, down, up } => {
                let m = self.mice.entry(device).or_default();
                if down & !m.buttons != 0 {
                    self.presses += 1;
                }
                m.buttons = (m.buttons | down) & !up;
            }
            RawEvent::Wheel { device, delta } => {
                let m = self.mice.entry(device).or_default();
                m.wheel_delta = m.wheel_delta.wrapping_add(delta);
            }
        }
    }

    /// The mouse `device` moves in a copy `w` by `h` pixels: its cursor stays inside.
    pub fn set_bounds(&mut self, device: u64, w: u32, h: u32) {
        self.bounds.insert(device, (w, h));
        if let Some(m) = self.mice.get_mut(&device) {
            m.cursor_x = m.cursor_x.clamp(0, i32::try_from(w.saturating_sub(1)).unwrap_or(i32::MAX));
            m.cursor_y = m.cursor_y.clamp(0, i32::try_from(h.saturating_sub(1)).unwrap_or(i32::MAX));
        }
    }
}

/// The session's devices and every copy's block.
pub struct InputHub {
    pads: pad::Pads,
    /// The controllers as last published, and their packet numbers (which change when a
    /// controller's state does).
    last_pads: Vec<(pad::State, u32)>,
    wheels: HashMap<u64, VirtualWheel>,
    pairs: HashMap<u64, u64>,
    /// The keyboards and mice apart, while a copy has one (`RawInput`).
    #[cfg(windows)]
    raw: Option<RawInput>,
    /// Off Windows (and in tests) the keyboards and mice come in through `apply_raw`.
    #[cfg(not(windows))]
    kbm: Kbm,
}

impl InputHub {
    pub fn new() -> InputHub {
        InputHub {
            pads: pad::Pads::new(),
            last_pads: Vec::new(),
            wheels: HashMap::new(),
            pairs: HashMap::new(),
            #[cfg(windows)]
            raw: None,
            #[cfg(not(windows))]
            kbm: Kbm::default(),
        }
    }

    /// The controllers, for the overlay (it reads them while open, and for its buttons).
    pub fn pads(&mut self) -> &mut pad::Pads {
        &mut self.pads
    }

    /// Mouse `mouse` goes with keyboard `keyboard` (`InputAssignment::KeyboardMouse`),
    /// as the session's player picks them.
    #[allow(dead_code)]
    pub fn pair_mouse(&mut self, keyboard: u64, mouse: u64) {
        self.pairs.insert(keyboard, mouse);
    }

    /// A wheel's state, from its reader.
    #[allow(dead_code)]
    pub fn set_wheel(&mut self, device: u64, wheel: VirtualWheel) {
        self.wheels.insert(device, wheel);
    }

    /// Starts telling keyboards and mice apart (raw input, on this thread's window
    /// messages), for copies given one.
    fn want_kbm(&mut self) {
        #[cfg(windows)]
        if self.raw.is_none() {
            self.raw = RawInput::start();
        }
    }

    /// A keyboard or mouse event from a reader other than Windows' raw input.
    #[cfg(not(windows))]
    #[allow(dead_code)]
    pub fn apply_raw(&mut self, e: RawEvent) {
        self.kbm.apply(e);
    }

    fn with_kbm<T>(&mut self, f: impl FnOnce(&mut Kbm) -> T) -> T {
        #[cfg(windows)]
        {
            let mut kbm = raw_kbm().lock().unwrap_or_else(|e| e.into_inner());
            f(&mut kbm)
        }
        #[cfg(not(windows))]
        {
            f(&mut self.kbm)
        }
    }

    /// Keys and mouse buttons pressed so far (`Kbm::presses`).
    pub fn kbm_presses(&mut self) -> u64 {
        self.with_kbm(|k| k.presses)
    }

    /// The devices now: the controllers as the last poll read them (a packet number
    /// moves on for each one whose state changed), the keyboards and mice, the wheels.
    pub fn devices(&mut self) -> Devices {
        let readings = self.pads.readings();
        self.last_pads.resize(readings.len(), (pad::State::default(), 0));
        let mut pads = Vec::with_capacity(readings.len());
        let mut pad_names = Vec::with_capacity(readings.len());
        for (r, (last, packet)) in readings.iter().zip(self.last_pads.iter_mut()) {
            // A seat whose controller is away: its copy has no controller meanwhile.
            let Some(r) = r else {
                pads.push(VirtualPad::default());
                pad_names.push(None);
                continue;
            };
            if r.state != *last {
                *last = r.state;
                *packet = packet.wrapping_add(1);
            }
            pads.push(reading_pad(r, *packet));
            pad_names.push(Some(r.name.clone()));
        }
        let (keyboards, mice) = self.with_kbm(|k| (k.keyboards.clone(), k.mice.clone()));
        Devices { pads, pad_names, keyboards, mice, wheels: self.wheels.clone(), pairs: self.pairs.clone() }
    }

    /// Writes each copy's block from its assignment (`route`). A copy given a keyboard
    /// and mouse starts the raw input reader; its mouse's cursor is kept inside the
    /// copy's screen.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn publish(&mut self, instances: &mut [InstanceConn], overlay_open: bool) {
        let open = vec![overlay_open; instances.len()];
        self.publish_session(instances, &open, None, None);
    }

    /// Writes each copy's block from its assignment (`route`; `open[i]`: copy `i`'s
    /// overlay is open, and has its input), and in a split-screen session (`focus:
    /// Some(i)`) also routes combined keyboard and mouse state to the focused pane (a
    /// controller player's, or the keyboard player's: `ANY_KEYBOARD`) while keeping
    /// unfocused panes isolated.
    pub fn publish_session(&mut self, instances: &mut [InstanceConn], open: &[bool], focus: Option<usize>, focus_cursor: Option<(i32, i32)>) {
        if !instances.iter().any(|i| i.input.is_some()) {
            return;
        }
        if focus.is_some() {
            self.want_kbm();
        }
        for inst in instances.iter().filter(|i| i.input.is_some()) {
            if let InputAssignment::KeyboardMouse { device_id } = inst.spec.input_assignment
                && device_id != ANY_KEYBOARD
            {
                self.want_kbm();
                let mouse = self.pairs.get(&device_id).copied().unwrap_or(device_id);
                if let Some((w, h)) = inst.screen_size() {
                    self.with_kbm(|k| k.set_bounds(mouse, w, h));
                }
            }
        }
        let devices = self.devices();
        let is_open = |i: usize| open.get(i).copied().unwrap_or(false);
        let focused_kbm = focus.filter(|&f| !is_open(f)).map(|_| devices.combined_kbm(focus_cursor));
        for (idx, inst) in instances.iter_mut().enumerate() {
            let mut snapshot = route(&inst.spec.input_assignment, is_open(idx), &devices);
            if let Some((kb, ms)) = focused_kbm
                && focus == Some(idx)
                && matches!(inst.spec.input_assignment, InputAssignment::Controller { .. } | InputAssignment::KeyboardMouse { device_id: ANY_KEYBOARD })
            {
                snapshot.keyboard = kb;
                snapshot.mouse = ms;
            }
            if let Some(block) = inst.input.as_mut() {
                block.publish(&snapshot);
                if block.note_pad(&snapshot) {
                    let (seat_idx, seat_name) = match inst.spec.input_assignment {
                        InputAssignment::Controller { index } => (
                            Some(index),
                            usize::try_from(index)
                                .ok()
                                .and_then(|i| devices.pad_names.get(i))
                                .and_then(|n| n.as_deref()),
                        ),
                        _ => (None, None),
                    };
                    let text = match (snapshot.pad.connected, seat_idx, seat_name) {
                        (true, Some(seat), Some(name)) => format!(
                            "input: this copy's controller (seat {seat}) is {name} (xinput_slot={:?}, hid_key={:#x})",
                            snapshot.pad.xinput_slot, snapshot.pad.hid_key
                        ),
                        (true, _, _) => format!(
                            "input: this copy's controller connected (xinput_slot={:?}, hid_key={:#x})",
                            snapshot.pad.xinput_slot, snapshot.pad.hid_key
                        ),
                        (false, Some(seat), _) => format!("input: this copy's controller (seat {seat}) went away"),
                        (false, None, _) => "input: this copy's controller went away".into(),
                    };
                    inst.send(&peerparty_protocol::ipc::HostMsg::Note { text });
                    inst.send(&peerparty_protocol::ipc::HostMsg::DevicesChanged);
                }
            }
        }
    }
}

/// The keyboards and mice raw input has seen, written by `RawInput`'s window procedure.
#[cfg(windows)]
fn raw_kbm() -> &'static std::sync::Mutex<Kbm> {
    static KBM: std::sync::OnceLock<std::sync::Mutex<Kbm>> = std::sync::OnceLock::new();
    KBM.get_or_init(Default::default)
}

/// Windows' raw input for every keyboard and mouse, by device handle, whatever window is
/// in front (`RIDEV_INPUTSINK`), on a message-only window of this thread: the host's
/// loop hands it its messages (desktop.rs `wait`).
#[cfg(windows)]
pub struct RawInput {
    hwnd: windows_sys::Win32::Foundation::HWND,
}

#[cfg(windows)]
mod raw {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Input::{GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RID_INPUT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_INPUT};

    use super::RawEvent;
    use peerparty_protocol::input_shm::{MOUSE_LEFT, MOUSE_MIDDLE, MOUSE_RIGHT, MOUSE_X1, MOUSE_X2};

    const RIM_TYPEMOUSE: u32 = 0;
    const RIM_TYPEKEYBOARD: u32 = 1;
    const RI_KEY_BREAK: u16 = 1;
    const RI_KEY_E0: u16 = 2;
    const MOUSE_MOVE_ABSOLUTE: u16 = 1;
    const RI_MOUSE_WHEEL: u16 = 0x0400;
    /// `RI_MOUSE_*` down and up bits, each with its `MOUSE_*` button.
    const BUTTONS: [(u16, u16, u32); 5] =
        [(0x0001, 0x0002, MOUSE_LEFT), (0x0004, 0x0008, MOUSE_RIGHT), (0x0010, 0x0020, MOUSE_MIDDLE), (0x0040, 0x0080, MOUSE_X1), (0x0100, 0x0200, MOUSE_X2)];

    /// A keyboard's raw report as a key: Shift, Ctrl and Alt as the side they are
    /// (VK_LSHIFT ... VK_RMENU: raw input says only VK_SHIFT, the scan code or the E0
    /// prefix tells the side). None for the 0xFF Windows sends for a key that is part
    /// of another (an escape sequence's first half).
    pub(super) fn key_event(device: u64, vkey: u16, make: u16, flags: u16) -> Option<RawEvent> {
        let e0 = flags & RI_KEY_E0 != 0;
        let vk = match vkey {
            0x10 => if make == 0x36 { 0xA1 } else { 0xA0 },
            0x11 => if e0 { 0xA3 } else { 0xA2 },
            0x12 => if e0 { 0xA5 } else { 0xA4 },
            0xFF => return None,
            v => u8::try_from(v).ok()?,
        };
        Some(RawEvent::Key { device, vk, down: flags & RI_KEY_BREAK == 0 })
    }

    /// The events in a mouse's raw report: its button changes, wheel and motion.
    pub(super) fn mouse_events(device: u64, flags: u16, button_flags: u16, button_data: u16, dx: i32, dy: i32) -> Vec<RawEvent> {
        let mut out = Vec::new();
        let (down, up) = BUTTONS.iter().fold((0, 0), |(d, u), (bd, bu, b)| (d | if button_flags & bd != 0 { *b } else { 0 }, u | if button_flags & bu != 0 { *b } else { 0 }));
        if down | up != 0 {
            out.push(RawEvent::Buttons { device, down, up });
        }
        if button_flags & RI_MOUSE_WHEEL != 0 {
            out.push(RawEvent::Wheel { device, delta: i32::from(button_data as i16) });
        }
        // A tablet's absolute positions aren't motion.
        if flags & MOUSE_MOVE_ABSOLUTE == 0 && (dx != 0 || dy != 0) {
            out.push(RawEvent::Motion { device, dx, dy });
        }
        out
    }

    pub(super) unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if m == WM_INPUT {
            let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
            let mut size = size_of::<RAWINPUT>() as u32;
            let got = unsafe { GetRawInputData(l as HRAWINPUT, RID_INPUT, (&mut raw as *mut RAWINPUT).cast(), &mut size, size_of::<RAWINPUTHEADER>() as u32) };
            if got != u32::MAX && got as usize >= size_of::<RAWINPUTHEADER>() {
                let device = raw.header.hDevice as u64;
                let events = match raw.header.dwType {
                    RIM_TYPEKEYBOARD => {
                        let k = unsafe { raw.data.keyboard };
                        key_event(device, k.VKey, k.MakeCode, k.Flags).into_iter().collect()
                    }
                    RIM_TYPEMOUSE => {
                        let ms = unsafe { raw.data.mouse };
                        let b = unsafe { ms.Anonymous.Anonymous };
                        mouse_events(device, ms.usFlags, b.usButtonFlags, b.usButtonData, ms.lLastX, ms.lLastY)
                    }
                    _ => Vec::new(),
                };
                let mut kbm = super::raw_kbm().lock().unwrap_or_else(|e| e.into_inner());
                for e in events {
                    kbm.apply(e);
                }
            }
        }
        unsafe { DefWindowProcW(h, m, w, l) }
    }
}

#[cfg(windows)]
impl RawInput {
    /// The message-only window, registered for every keyboard's and mouse's input.
    fn start() -> Option<RawInput> {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_INPUTSINK, RegisterRawInputDevices};
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE, RegisterClassW, WNDCLASSW};
        static CLASS: std::sync::Once = std::sync::Once::new();
        let class: Vec<u16> = "peerparty-rawinput".encode_utf16().chain(Some(0)).collect();
        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        CLASS.call_once(|| {
            let wc = WNDCLASSW { lpfnWndProc: Some(raw::proc), hInstance: module, lpszClassName: class.as_ptr(), ..unsafe { std::mem::zeroed() } };
            unsafe { RegisterClassW(&wc) };
        });
        let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, std::ptr::null_mut(), module, std::ptr::null()) };
        if hwnd.is_null() {
            return None;
        }
        // Generic desktop mouse (2) and keyboard (6).
        let devices = [2u16, 6].map(|usage| RAWINPUTDEVICE { usUsagePage: 1, usUsage: usage, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd });
        if unsafe { RegisterRawInputDevices(devices.as_ptr(), devices.len() as u32, size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
            unsafe { DestroyWindow(hwnd) };
            return None;
        }
        Some(RawInput { hwnd })
    }
}

#[cfg(windows)]
impl Drop for RawInput {
    fn drop(&mut self) {
        use windows_sys::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_REMOVE, RegisterRawInputDevices};
        let devices = [2u16, 6].map(|usage| RAWINPUTDEVICE { usUsagePage: 1, usUsage: usage, dwFlags: RIDEV_REMOVE, hwndTarget: std::ptr::null_mut() });
        unsafe {
            RegisterRawInputDevices(devices.as_ptr(), devices.len() as u32, size_of::<RAWINPUTDEVICE>() as u32);
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::input_shm::{MOUSE_LEFT, MOUSE_RIGHT};
    use peerparty_protocol::session::{InstanceId, InstanceSpec};

    fn devices() -> Devices {
        let mut d = Devices::default();
        d.pads.push(virtual_pad(&pad::State { buttons: pad::SOUTH, lx: 1.0, ly: -1.0, ..Default::default() }, 3));
        d.pads.push(virtual_pad(&pad::State { buttons: pad::START | pad::HOME, ly: 0.5, ..Default::default() }, 9));
        let mut k = VirtualKeyboard::default();
        k.set(0x41, true);
        d.keyboards.insert(10, k);
        d.keyboards.insert(20, VirtualKeyboard::default());
        d.mice.insert(11, VirtualMouse { cursor_x: 5, buttons: MOUSE_LEFT, ..Default::default() });
        d.mice.insert(20, VirtualMouse { cursor_x: 7, ..Default::default() });
        d.pairs.insert(10, 11);
        d.wheels.insert(30, VirtualWheel { steer: -100, throttle: 500, ..Default::default() });
        d
    }

    /// Each assignment's copy sees what it names and nothing else; with the overlay open
    /// every copy sees nothing.
    #[test]
    fn each_copy_gets_what_it_was_assigned() {
        let d = devices();
        assert_eq!(route(&InputAssignment::All, false, &d), InputSnapshot { mode: InputMode::PassThrough, ..Default::default() });
        assert_eq!(route(&InputAssignment::None, false, &d), InputSnapshot { mode: InputMode::BlockAll, ..Default::default() });
        let p1 = route(&InputAssignment::Controller { index: 1 }, false, &d);
        assert_eq!(p1.mode, InputMode::Virtual);
        assert_eq!(p1.pad, d.pads[1]);
        assert_eq!((p1.pad.xinput_buttons(), p1.pad.packet_number, p1.pad.thumb_ly), (0x0010 | 0x0400, 9, 16384));
        assert!(!p1.keyboard.active && !p1.mouse.active && !p1.wheel.connected, "only its controller");
        let p0 = route(&InputAssignment::Controller { index: 0 }, false, &d);
        assert_eq!((p0.pad.xinput_buttons(), p0.pad.thumb_lx, p0.pad.thumb_ly), (0x1000, 32767, -32767));
        let gone = route(&InputAssignment::Controller { index: 7 }, false, &d);
        assert_eq!((gone.mode, gone.pad.connected), (InputMode::Virtual, false), "a controller that isn't there: none");
        let kbm = route(&InputAssignment::KeyboardMouse { device_id: 10 }, false, &d);
        assert_eq!(kbm.mode, InputMode::Virtual);
        assert!(kbm.keyboard.active && kbm.keyboard.is_down(0x41));
        assert!(kbm.mouse.active && kbm.mouse.is_down(MOUSE_LEFT), "its paired mouse");
        assert_eq!(kbm.mouse.cursor_x, 5);
        assert!(!kbm.pad.connected, "no controller");
        let unpaired = route(&InputAssignment::KeyboardMouse { device_id: 20 }, false, &d);
        assert_eq!((unpaired.keyboard.is_down(0x41), unpaired.mouse.cursor_x), (false, 7), "the mouse of the same id");
        let missing = route(&InputAssignment::KeyboardMouse { device_id: 99 }, false, &d);
        assert!(!missing.keyboard.active && !missing.mouse.active);
        let wheel = route(&InputAssignment::Wheel { device_id: 30 }, false, &d);
        assert_eq!((wheel.mode, wheel.wheel.connected, wheel.wheel.steer, wheel.wheel.throttle), (InputMode::Virtual, true, -100, 500));
        assert!(!wheel.pad.connected && !wheel.keyboard.active);
        assert!(!route(&InputAssignment::Wheel { device_id: 31 }, false, &d).wheel.connected);
        for a in [InputAssignment::All, InputAssignment::None, InputAssignment::Controller { index: 0 }, InputAssignment::KeyboardMouse { device_id: 10 }, InputAssignment::Wheel { device_id: 30 }] {
            assert_eq!(route(&a, true, &d), InputSnapshot { mode: InputMode::BlockAll, ..Default::default() }, "{a:?} with the overlay open");
        }
    }

    #[test]
    fn logical_buttons_become_xinputs() {
        assert_eq!(xinput_buttons(0), 0);
        assert_eq!(xinput_buttons(pad::SOUTH | pad::EAST | pad::WEST | pad::NORTH), 0xf000);
        assert_eq!(xinput_buttons(pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT), 0x000f);
        assert_eq!(xinput_buttons(pad::LB | pad::RB | pad::BACK | pad::START | pad::HOME), 0x0100 | 0x0200 | 0x0020 | 0x0010 | 0x0400);
        let p = virtual_pad(&pad::State { lx: -3.0, ..Default::default() }, 1);
        assert_eq!((p.connected, p.thumb_lx, p.thumb_ly), (true, -32767, 0), "clamped");
        // A twin-stick shooter aims with the right stick; triggers and stick clicks too.
        let p = virtual_pad(&pad::State { buttons: pad::LSTICK | pad::RSTICK, rx: 1.0, ry: -0.5, lt: 1.0, rt: 0.5, ..Default::default() }, 2);
        assert_eq!((p.thumb_rx, p.thumb_ry, p.left_trigger, p.right_trigger), (32767, -16384, 255, 128));
        assert_eq!(p.xinput_buttons(), 0x0040 | 0x0080);
        // An XInput pad goes over as XInput gave it, with its slot; a HID pad with its key.
        let mut raw = [0u8; 12];
        raw[0..2].copy_from_slice(&(0x1000u16 | 0x0400).to_le_bytes());
        raw[2] = 7;
        raw[8..10].copy_from_slice(&(-123i16).to_le_bytes());
        let r = pad::Reading { name: "XInput slot 2".into(), style: pad::Style::Xbox, state: pad::State::default(), xinput: Some((2, raw)), hid_key: 0 };
        let p = reading_pad(&r, 5);
        assert_eq!((p.xinput_buttons(), p.left_trigger, p.thumb_rx, p.xinput_slot, p.packet_number), (0x1400, 7, -123, Some(2), 5));
        let r = pad::Reading { name: "HID 054c:09cc".into(), style: pad::Style::PlayStation, state: pad::State { buttons: pad::SOUTH, ..Default::default() }, xinput: None, hid_key: 99 };
        let p = reading_pad(&r, 1);
        assert_eq!((p.xinput_buttons(), p.xinput_slot, p.hid_key), (0x1000, None, 99));
    }

    /// Keyboards and mice stay apart by device; motion and wheel are running totals; a
    /// cursor stays inside its copy's screen.
    #[test]
    fn keyboards_and_mice_stay_apart() {
        let mut k = Kbm::default();
        k.apply(RawEvent::Key { device: 1, vk: 0x57, down: true });
        k.apply(RawEvent::Key { device: 2, vk: 0x26, down: true });
        assert!(k.keyboards[&1].is_down(0x57) && !k.keyboards[&1].is_down(0x26));
        assert!(k.keyboards[&2].is_down(0x26) && !k.keyboards[&2].is_down(0x57));
        k.apply(RawEvent::Key { device: 1, vk: 0x57, down: false });
        assert!(!k.keyboards[&1].is_down(0x57));
        k.set_bounds(5, 100, 50);
        for _ in 0..3 {
            k.apply(RawEvent::Motion { device: 5, dx: 40, dy: -10 });
        }
        let m = k.mice[&5];
        assert_eq!((m.delta_x, m.delta_y), (120, -30), "the motion, whole");
        assert_eq!((m.cursor_x, m.cursor_y), (99, 0), "the cursor, inside");
        k.apply(RawEvent::Motion { device: 6, dx: 7, dy: 8 });
        assert_eq!((k.mice[&6].cursor_x, k.mice[&5].delta_x), (7, 120), "another mouse");
        k.apply(RawEvent::Buttons { device: 5, down: MOUSE_LEFT | MOUSE_RIGHT, up: 0 });
        k.apply(RawEvent::Buttons { device: 5, down: 0, up: MOUSE_LEFT });
        assert_eq!(k.mice[&5].buttons, MOUSE_RIGHT);
        k.apply(RawEvent::Wheel { device: 5, delta: -120 });
        k.apply(RawEvent::Wheel { device: 5, delta: 240 });
        assert_eq!(k.mice[&5].wheel_delta, 120);
        // Presses: a key going down once (not its repeats, not a modifier), a button.
        let before = k.presses;
        k.apply(RawEvent::Key { device: 1, vk: 0x41, down: true });
        k.apply(RawEvent::Key { device: 1, vk: 0x41, down: true });
        k.apply(RawEvent::Key { device: 1, vk: 0xA2, down: true });
        k.apply(RawEvent::Buttons { device: 5, down: MOUSE_LEFT, up: 0 });
        assert_eq!(k.presses - before, 2);
        k.set_bounds(6, 4, 4);
        assert_eq!((k.mice[&6].cursor_x, k.mice[&6].cursor_y), (3, 3), "a smaller screen pulls the cursor in");
    }

    /// Raw input's Shift / Ctrl / Alt read as their side, and as the either-side key
    /// while one side is down.
    #[cfg(windows)]
    #[test]
    fn modifiers_have_sides() {
        assert_eq!(raw::key_event(1, 0x10, 0x36, 0), Some(RawEvent::Key { device: 1, vk: 0xA1, down: true }));
        assert_eq!(raw::key_event(1, 0x11, 0x1D, 2 | 1), Some(RawEvent::Key { device: 1, vk: 0xA3, down: false }));
        assert_eq!(raw::key_event(1, 0x12, 0x38, 0), Some(RawEvent::Key { device: 1, vk: 0xA4, down: true }));
        assert_eq!(raw::key_event(1, 0xFF, 0x2A, 0), None);
        let mut k = Kbm::default();
        k.apply(RawEvent::Key { device: 1, vk: 0xA0, down: true });
        k.apply(RawEvent::Key { device: 1, vk: 0xA1, down: true });
        k.apply(RawEvent::Key { device: 1, vk: 0xA0, down: false });
        assert!(k.keyboards[&1].is_down(0x10), "the right one still holds Shift");
        k.apply(RawEvent::Key { device: 1, vk: 0xA1, down: false });
        assert!(!k.keyboards[&1].is_down(0x10));
        k.apply(RawEvent::Key { device: 1, vk: 0xA5, down: true });
        assert!(k.keyboards[&1].is_down(0x12) && k.keyboards[&1].is_down(0xA5));
    }

    #[cfg(windows)]
    #[test]
    fn raw_mouse_reports_read_as_events() {
        use peerparty_protocol::input_shm::{MOUSE_MIDDLE, MOUSE_X2};
        assert_eq!(
            raw::mouse_events(4, 0, 0x0001 | 0x0020 | 0x0100, 0, 3, -2),
            [RawEvent::Buttons { device: 4, down: MOUSE_LEFT | MOUSE_X2, up: MOUSE_MIDDLE }, RawEvent::Motion { device: 4, dx: 3, dy: -2 }]
        );
        assert_eq!(raw::mouse_events(4, 0, 0x0400, (-120i16) as u16, 0, 0), [RawEvent::Wheel { device: 4, delta: -120 }]);
        assert!(raw::mouse_events(4, 1, 0, 0, 500, 500).is_empty(), "absolute positions aren't motion");
    }

    /// A block's snapshot is the one the hook reads from the same mapping; an unchanged
    /// one isn't published again.
    #[cfg(windows)]
    fn round_trip(block: &mut InputBlock, reader: &InputHeader) {
        assert!(reader.valid_magic());
        assert_eq!(reader.mode(), InputMode::PassThrough);
        let s = route(&InputAssignment::Controller { index: 0 }, false, &devices());
        assert!(block.publish(&s));
        assert!(!block.publish(&s), "the same again");
        let got = reader.read_stable().expect("a whole snapshot");
        assert_eq!(InputSnapshot { seq: 0, ..got }, s);
        assert_eq!(got.seq, 2);
        block.publish(&route(&InputAssignment::None, true, &devices()));
        assert_eq!(reader.mode(), InputMode::BlockAll);
    }

    #[cfg(windows)]
    #[test]
    fn a_named_input_block_round_trips() {
        use windows_sys::Win32::System::Memory::{FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile};
        let mut block = InputBlock::create(std::process::id()).expect("a block");
        let Some(ShmBacking::WinMapping { name }) = ShmBacking::parse(&block.name) else { panic!("{}", block.name) };
        assert!(name.starts_with(r"Local\peerparty-input-"), "{name}");
        // The hook's end: the mapping opened by its name.
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let map = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide.as_ptr()) };
        assert!(!map.is_null());
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, INPUT_BLOCK_SIZE) };
        let reader = unsafe { InputHeader::from_raw(view.Value.cast(), INPUT_BLOCK_SIZE) }.unwrap();
        round_trip(&mut block, reader);
        unsafe {
            UnmapViewOfFile(view);
            windows_sys::Win32::Foundation::CloseHandle(map);
        }
    }

    /// The file a Linux launcher shares with a game under Wine: what's published is in
    /// its bytes, and it goes when the block does.
    #[test]
    fn a_file_input_block_round_trips_and_goes() {
        let mut block = InputBlock::create_file(&std::env::temp_dir(), std::process::id()).expect("a file block");
        let Some(ShmBacking::FileMmap { path }) = ShmBacking::parse(&block.name) else { panic!("{}", block.name) };
        let reader = InputHeader::new();
        // The words as another mapping of the file sees them.
        let copy = |reader: &InputHeader| {
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(bytes.len(), MAPPED);
            for (i, w) in bytes[..INPUT_BLOCK_SIZE].chunks(4).enumerate() {
                let v = u32::from_le_bytes(w.try_into().unwrap());
                // SAFETY: InputHeader is INPUT_BLOCK_SIZE bytes of AtomicU32 words.
                unsafe { &*(reader as *const InputHeader as *const std::sync::atomic::AtomicU32).add(i) }.store(v, std::sync::atomic::Ordering::Relaxed);
            }
        };
        copy(&reader);
        assert!(reader.valid_magic());
        let s = route(&InputAssignment::KeyboardMouse { device_id: 10 }, false, &devices());
        assert!(block.publish(&s));
        copy(&reader);
        assert_eq!(InputSnapshot { seq: 0, ..reader.read_stable().unwrap() }, s);
        drop(block);
        assert!(!Path::new(&path).exists());
    }

    /// The hub writes every copy's block from its assignment, and blocks a copy while its
    /// overlay is open.
    #[test]
    fn the_hub_publishes_each_copys_block() {
        let mut hub = InputHub::new();
        let dir = std::env::temp_dir();
        let mut instances: Vec<InstanceConn> = [InputAssignment::All, InputAssignment::None, InputAssignment::Controller { index: 0 }]
            .into_iter()
            .enumerate()
            .map(|(i, a)| {
                let mut spec = InstanceSpec::solo(InstanceId(i as u32), i as u32);
                spec.input_assignment = a;
                let mut inst = InstanceConn::detached(spec);
                inst.input = InputBlock::create_file(&dir, std::process::id());
                inst
            })
            .collect();
        instances.push(InstanceConn::detached(InstanceSpec::solo(InstanceId(3), 3)));
        hub.publish(&mut instances, false);
        let modes: Vec<_> = instances[..3].iter().map(|i| i.input.as_ref().unwrap().header().mode()).collect();
        assert_eq!(modes, [InputMode::PassThrough, InputMode::BlockAll, InputMode::Virtual]);
        hub.publish(&mut instances, true);
        assert!(instances[..3].iter().all(|i| i.input.as_ref().unwrap().header().mode() == InputMode::BlockAll));
        hub.publish(&mut instances, false);
        assert_eq!(instances[2].input.as_ref().unwrap().header().mode(), InputMode::Virtual, "closed again");
        // One player's overlay open holds back that copy alone.
        hub.publish_session(&mut instances, &[false, false, true, false], None, None);
        let modes: Vec<_> = instances[..3].iter().map(|i| i.input.as_ref().unwrap().header().mode()).collect();
        assert_eq!(modes, [InputMode::PassThrough, InputMode::BlockAll, InputMode::BlockAll]);
        hub.publish_session(&mut instances, &[true, false, false, false], None, None);
        let modes: Vec<_> = instances[..3].iter().map(|i| i.input.as_ref().unwrap().header().mode()).collect();
        assert_eq!(modes, [InputMode::BlockAll, InputMode::BlockAll, InputMode::Virtual], "the others play on");
        // A copy given a keyboard and mouse keeps its mouse's cursor inside its screen.
        hub.pair_mouse(10, 11);
        assert_eq!(hub.pairs.get(&10), Some(&11));
        // Focused controller pane receives combined KBM with mapped cursor; unfocused stays isolated.
        let mut d = devices();
        let (kb, ms) = d.combined_kbm(Some((640, 360)));
        assert!(kb.active && kb.is_down(0x41));
        assert!(ms.active && ms.is_down(MOUSE_LEFT) && (ms.cursor_x, ms.cursor_y) == (640, 360));
        d.mice.clear();
        let (_, ms2) = d.combined_kbm(Some((10, 20)));
        assert!(ms2.active && (ms2.cursor_x, ms2.cursor_y) == (10, 20));
    }

    /// A copy's block notices when its assigned controller arrives, switches, or leaves,
    /// and stays quiet on button presses or overlay open/close.
    #[test]
    fn pad_arrival_and_overlay_changes_are_told_apart() {
        let dir = std::env::temp_dir();
        let mut block = InputBlock::create_file(&dir, std::process::id()).unwrap();
        let virt = |pad: VirtualPad| InputSnapshot { mode: InputMode::Virtual, pad, ..Default::default() };
        let empty = virt(VirtualPad::default());
        assert!(!block.note_pad(&empty), "no controller at start-up");
        assert!(!block.note_pad(&empty));
        let xi1 = VirtualPad { connected: true, xinput_slot: Some(1), ..Default::default() };
        assert!(block.note_pad(&virt(xi1)), "controller took its seat");
        let xi1_pressed = VirtualPad { buttons: 0x1000, packet_number: 2, ..xi1 };
        assert!(!block.note_pad(&virt(xi1_pressed)), "same controller, different buttons");
        // Overlay opens and closes: not a hot-plug.
        assert!(!block.note_pad(&InputSnapshot { mode: InputMode::BlockAll, ..Default::default() }));
        assert!(!block.note_pad(&virt(xi1)), "back from overlay with same controller");
        // Switching to a HID pad, then unplugging.
        let hid = VirtualPad { connected: true, xinput_slot: None, hid_key: 42, ..Default::default() };
        assert!(block.note_pad(&virt(hid)), "switched controller");
        assert!(block.note_pad(&empty), "controller went away");
    }
}
