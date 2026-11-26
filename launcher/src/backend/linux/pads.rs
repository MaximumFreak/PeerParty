//! Controllers on Linux, from evdev (`/dev/input/event*`): every device with gamepad
//! buttons gets a reader thread that turns its `input_event`s into `host::pad`'s logical
//! layout (the same `Hid` entries Windows' HID readers make, so `Pads::poll` treats them
//! alike). The physical pads, the Steam Deck's own controls and the virtual pads Steam
//! Input makes all show up there; a logged-in user may read them (systemd gives the
//! active session access to joysticks).
//!
//! The translation (`Translate`) is plain code with tests; opening and reading devices
//! is Unix-only.

use crate::host::pad::{BACK, DOWN, EAST, HOME, LB, LEFT, LSTICK, NORTH, RB, RIGHT, RSTICK, SOUTH, START, State, UP, WEST};

/// `struct input_event` on 64-bit Linux: a `timeval` (two 64-bit words), then type,
/// code and value.
pub const EVENT_SIZE: usize = 24;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
/// The kernel's gamepad layout: Z / Rz the triggers, Rx / Ry the right stick.
pub const ABS_Z: u16 = 0x02;
pub const ABS_RX: u16 = 0x03;
pub const ABS_RY: u16 = 0x04;
pub const ABS_RZ: u16 = 0x05;
/// Digital triggers, on pads without trigger axes.
pub const BTN_TL2: u16 = 0x138;
pub const BTN_TR2: u16 = 0x139;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;

/// `BTN_SOUTH`: the button every gamepad has (`BTN_GAMEPAD`).
pub const BTN_SOUTH: u16 = 0x130;

/// One `input_event`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

/// The whole events in what one `read` gave (a partial one at the end is dropped: the
/// kernel only hands out whole events).
pub fn events(buf: &[u8]) -> impl Iterator<Item = Event> + '_ {
    buf.chunks_exact(EVENT_SIZE).map(|e| Event {
        kind: u16::from_ne_bytes([e[16], e[17]]),
        code: u16::from_ne_bytes([e[18], e[19]]),
        value: i32::from_ne_bytes([e[20], e[21], e[22], e[23]]),
    })
}

/// A key or button code as the logical layout's button. The face buttons go by their
/// position (`BTN_SOUTH` is the bottom one, `BTN_NORTH` the top), as the kernel's gamepad
/// layout defines them; the d-pad may come as buttons as well as a hat.
pub fn button(code: u16) -> Option<u32> {
    Some(match code {
        0x130 => SOUTH,
        0x131 => EAST,
        0x133 => NORTH,
        0x134 => WEST,
        0x136 => LB,
        0x137 => RB,
        0x13a => BACK,
        0x13b => START,
        0x13c => HOME,
        0x13d => LSTICK,
        0x13e => RSTICK,
        0x220 => UP,
        0x221 => DOWN,
        0x222 => LEFT,
        0x223 => RIGHT,
        _ => return None,
    })
}

/// An absolute axis's range (`EVIOCGABS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsRange {
    pub min: i32,
    pub max: i32,
}

impl AbsRange {
    /// `v` as -1..1 across the range (its middle is 0).
    pub fn unit(self, v: i32) -> f32 {
        if self.max <= self.min {
            return 0.0;
        }
        let half = (f64::from(self.max) - f64::from(self.min)) / 2.0;
        let mid = f64::from(self.min) + half;
        ((f64::from(v.clamp(self.min, self.max)) - mid) / half) as f32
    }
}

/// The sticks' and triggers' ranges (a device without them reads as signed 16-bit
/// sticks and 8-bit triggers, the common ones).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ranges {
    pub x: AbsRange,
    pub y: AbsRange,
    pub rx: AbsRange,
    pub ry: AbsRange,
    pub z: AbsRange,
    pub rz: AbsRange,
}

impl Default for Ranges {
    fn default() -> Ranges {
        let r = AbsRange { min: -32768, max: 32767 };
        let t = AbsRange { min: 0, max: 255 };
        Ranges { x: r, y: r, rx: r, ry: r, z: t, rz: t }
    }
}

impl AbsRange {
    /// `v` as 0..1 across the range: a trigger's travel.
    pub fn travel(self, v: i32) -> f32 {
        (self.unit(v) + 1.0) / 2.0
    }
}

/// One device's events into its state: changes gather until the kernel's `SYN_REPORT`
/// closes the packet, which hands out the whole state.
#[derive(Clone, Debug, Default)]
pub struct Translate {
    pub ranges: Ranges,
    buttons: u32,
    hat_x: i32,
    hat_y: i32,
    lx: f32,
    ly: f32,
    rx: f32,
    ry: f32,
    lt: f32,
    rt: f32,
    /// Events were dropped (`SYN_DROPPED`): what comes until the next report is stale.
    dropped: bool,
}

impl Translate {
    pub fn new(ranges: Ranges) -> Translate {
        Translate { ranges, ..Default::default() }
    }

    /// Takes one event; at the end of a packet, the state it leaves.
    pub fn apply(&mut self, e: Event) -> Option<State> {
        match (e.kind, e.code) {
            (EV_SYN, SYN_DROPPED) => {
                self.dropped = true;
                None
            }
            (EV_SYN, SYN_REPORT) => {
                if std::mem::take(&mut self.dropped) {
                    // The buffer overflowed: start again from rest rather than keep a
                    // button down whose release was lost.
                    *self = Translate::new(self.ranges);
                }
                Some(self.state())
            }
            _ if self.dropped => None,
            (EV_KEY, code @ (BTN_TL2 | BTN_TR2)) => {
                let v = if e.value != 0 { 1.0 } else { 0.0 };
                if code == BTN_TL2 { self.lt = v } else { self.rt = v }
                None
            }
            (EV_KEY, code) => {
                if let Some(b) = button(code) {
                    // 1 down, 2 autorepeat (still down), 0 up.
                    if e.value != 0 { self.buttons |= b } else { self.buttons &= !b }
                }
                None
            }
            (EV_ABS, ABS_X) => {
                self.lx = self.ranges.x.unit(e.value);
                None
            }
            // evdev's Y grows downwards; the layout's up is positive.
            (EV_ABS, ABS_Y) => {
                self.ly = -self.ranges.y.unit(e.value);
                None
            }
            (EV_ABS, ABS_RX) => {
                self.rx = self.ranges.rx.unit(e.value);
                None
            }
            (EV_ABS, ABS_RY) => {
                self.ry = -self.ranges.ry.unit(e.value);
                None
            }
            (EV_ABS, ABS_Z) => {
                self.lt = self.ranges.z.travel(e.value);
                None
            }
            (EV_ABS, ABS_RZ) => {
                self.rt = self.ranges.rz.travel(e.value);
                None
            }
            (EV_ABS, ABS_HAT0X) => {
                self.hat_x = e.value.signum();
                None
            }
            (EV_ABS, ABS_HAT0Y) => {
                self.hat_y = e.value.signum();
                None
            }
            _ => None,
        }
    }

    pub fn state(&self) -> State {
        let hat = match self.hat_x {
            -1 => LEFT,
            1 => RIGHT,
            _ => 0,
        } | match self.hat_y {
            -1 => UP,
            1 => DOWN,
            _ => 0,
        };
        State { buttons: self.buttons | hat, lx: self.lx, ly: self.ly, rx: self.rx, ry: self.ry, lt: self.lt, rt: self.rt }
    }
}

#[cfg(unix)]
pub use device::scan;

/// Finding and reading the devices.
#[cfg(unix)]
mod device {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{AbsRange, BTN_SOUTH, EVENT_SIZE, Ranges, Translate, events};
    use crate::host::pad::{Hid, State, Threads, style_of};

    /// How long a reader waits for its device before looking at `Threads::stopping`.
    const POLL_MS: i32 = 100;

    /// `_IOC(_IOC_READ, 'E', nr, size)`.
    const fn ior(nr: u32, size: u32) -> libc::c_ulong {
        ((2u32 << 30) | (size << 16) | (0x45 << 8) | nr) as libc::c_ulong
    }

    /// `EVIOCGID`: bus, vendor, product, version.
    const EVIOCGID: libc::c_ulong = ior(0x02, 8);
    /// `EVIOCGBIT(EV_KEY, 96)`: which keys and buttons it has (KEY_MAX is 0x2ff).
    const EVIOCGBIT_KEY: libc::c_ulong = ior(0x21, 96);
    /// `EVIOCGABS(axis)`: an axis's value, minimum, maximum, fuzz, flat, resolution.
    const fn eviocgabs(axis: u16) -> libc::c_ulong {
        ior(0x40 + axis as u32, 24)
    }
    /// `EVIOCGNAME(128)`: the device's name.
    const EVIOCGNAME: libc::c_ulong = ior(0x06, 128);

    /// Opens every gamepad in `/dev/input` not read yet, each read on one of `threads`.
    pub fn scan(list: &Arc<Mutex<Vec<Hid>>>, threads: &Arc<Threads>) {
        let Ok(dir) = std::fs::read_dir("/dev/input") else { return };
        let mut paths: Vec<String> = dir
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        paths.sort();
        for path in paths {
            if threads.stopping() {
                return;
            }
            if list.lock().unwrap().iter().any(|h| h.path == path) {
                continue;
            }
            open(list, threads, path);
        }
    }

    fn open(list: &Arc<Mutex<Vec<Hid>>>, threads: &Arc<Threads>, path: String) {
        let Ok(mut file) = std::fs::File::open(&path) else { return };
        let fd = file.as_raw_fd();
        let mut keys = [0u8; 96];
        if unsafe { libc::ioctl(fd, EVIOCGBIT_KEY, keys.as_mut_ptr()) } < 0 {
            return;
        }
        let has = |code: u16| keys[usize::from(code / 8)] & (1 << (code % 8)) != 0;
        // Gamepads only (a keyboard or mouse has no BTN_SOUTH).
        if !has(BTN_SOUTH) {
            return;
        }
        let mut id = [0u16; 4];
        if unsafe { libc::ioctl(fd, EVIOCGID, id.as_mut_ptr()) } < 0 {
            return;
        }
        let range = |axis: u16| {
            let mut info = [0i32; 6];
            (unsafe { libc::ioctl(fd, eviocgabs(axis), info.as_mut_ptr()) } >= 0 && info[2] > info[1]).then_some(AbsRange { min: info[1], max: info[2] })
        };
        let default = Ranges::default();
        let ranges = Ranges {
            x: range(super::ABS_X).unwrap_or(default.x),
            y: range(super::ABS_Y).unwrap_or(default.y),
            rx: range(super::ABS_RX).unwrap_or(default.rx),
            ry: range(super::ABS_RY).unwrap_or(default.ry),
            z: range(super::ABS_Z).unwrap_or(default.z),
            rz: range(super::ABS_RZ).unwrap_or(default.rz),
        };
        let (vid, pid) = (id[1], id[2]);
        let mut name = [0u8; 128];
        let product = if unsafe { libc::ioctl(fd, EVIOCGNAME, name.as_mut_ptr()) } > 0 {
            let n = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            String::from_utf8_lossy(&name[..n]).trim().to_string()
        } else {
            String::new()
        };
        let state = Arc::new(Mutex::new(State::default()));
        let alive = Arc::new(AtomicBool::new(true));
        list.lock().unwrap().push(Hid { path, vid, pid, product, style: style_of(vid), state: state.clone(), alive: alive.clone(), home: true, stick: 0, serial_key: 0, inst_key: 0 });
        let (stop, reader_alive) = (threads.clone(), alive.clone());
        let read = move || {
            let alive = reader_alive;
            let mut t = Translate::new(ranges);
            let mut buf = [0u8; EVENT_SIZE * 64];
            // Reads end when the device goes (unplugged, or its access taken away), or
            // when the pads are dropped: a quiet device is waited on in short `poll`s.
            while !stop.stopping() {
                let mut ready = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                match unsafe { libc::poll(&mut ready, 1, POLL_MS) } {
                    0 => continue,
                    n if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => continue,
                    n if n < 0 => break,
                    _ => {}
                }
                let Ok(n) = file.read(&mut buf) else { break };
                if n == 0 {
                    break;
                }
                for e in events(&buf[..n]) {
                    if let Some(s) = t.apply(e) {
                        *state.lock().unwrap() = s;
                    }
                }
            }
            alive.store(false, Ordering::Relaxed);
        };
        if !threads.spawn("peerparty-pad-evdev", read) {
            // Stopping, or no thread: the device (closed with the unrun reader) isn't read.
            list.lock().unwrap().retain(|e| !Arc::ptr_eq(&e.alive, &alive));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(kind: u16, code: u16, value: i32) -> Vec<u8> {
        let mut v = vec![0u8; 16];
        v.extend_from_slice(&kind.to_ne_bytes());
        v.extend_from_slice(&code.to_ne_bytes());
        v.extend_from_slice(&value.to_ne_bytes());
        v
    }

    #[test]
    fn events_come_out_of_a_read_whole() {
        let mut buf = raw(EV_KEY, 0x130, 1);
        buf.extend(raw(EV_SYN, SYN_REPORT, 0));
        buf.extend_from_slice(&[1, 2, 3]);
        let got: Vec<Event> = events(&buf).collect();
        assert_eq!(got, [Event { kind: EV_KEY, code: 0x130, value: 1 }, Event { kind: EV_SYN, code: SYN_REPORT, value: 0 }]);
    }

    /// Buttons, the hat and the stick into the layout, handed out at each report.
    #[test]
    fn a_packet_becomes_the_layouts_state() {
        let mut t = Translate::new(Ranges::default());
        let feed = |t: &mut Translate, kind, code, value| t.apply(Event { kind, code, value });
        assert!(feed(&mut t, EV_KEY, 0x130, 1).is_none(), "nothing until the report");
        feed(&mut t, EV_KEY, 0x13c, 1);
        feed(&mut t, EV_ABS, ABS_HAT0X, 1);
        feed(&mut t, EV_ABS, ABS_HAT0Y, -1);
        let s = feed(&mut t, EV_SYN, SYN_REPORT, 0).unwrap();
        assert_eq!(s.buttons, SOUTH | HOME | RIGHT | UP);
        // Autorepeat is still down; release lets go.
        feed(&mut t, EV_KEY, 0x130, 2);
        feed(&mut t, EV_KEY, 0x13c, 0);
        feed(&mut t, EV_ABS, ABS_HAT0X, 0);
        feed(&mut t, EV_ABS, ABS_HAT0Y, 0);
        assert_eq!(feed(&mut t, EV_SYN, SYN_REPORT, 0).unwrap().buttons, SOUTH);
        // The stick: evdev's down is the layout's down (negative), folding into the d-pad.
        feed(&mut t, EV_ABS, ABS_Y, 32767);
        let s = feed(&mut t, EV_SYN, SYN_REPORT, 0).unwrap();
        assert!(s.ly < -0.99 && s.with_stick() & DOWN != 0, "{s:?}");
        // Keys that aren't the layout's are ignored.
        feed(&mut t, EV_KEY, 0x13f, 1);
        assert_eq!(feed(&mut t, EV_SYN, SYN_REPORT, 0).unwrap().buttons, SOUTH);
        // The right stick (up positive), the triggers' travel, the stick clicks.
        feed(&mut t, EV_ABS, ABS_RX, 32767);
        feed(&mut t, EV_ABS, ABS_RY, -32768);
        feed(&mut t, EV_ABS, ABS_Z, 255);
        feed(&mut t, EV_KEY, 0x13e, 1);
        let s = feed(&mut t, EV_SYN, SYN_REPORT, 0).unwrap();
        assert!(s.rx > 0.99 && s.ry > 0.99 && s.lt > 0.99 && s.rt == 0.0, "{s:?}");
        assert_eq!(s.buttons, SOUTH | RSTICK);
        // A pad whose triggers are buttons.
        feed(&mut t, EV_KEY, BTN_TR2, 1);
        assert_eq!(feed(&mut t, EV_SYN, SYN_REPORT, 0).unwrap().rt, 1.0);
    }

    /// After the kernel dropped events, nothing stays held whose release may be lost.
    #[test]
    fn dropped_events_start_again_from_rest() {
        let mut t = Translate::new(Ranges::default());
        t.apply(Event { kind: EV_KEY, code: 0x131, value: 1 });
        t.apply(Event { kind: EV_SYN, code: SYN_REPORT, value: 0 });
        t.apply(Event { kind: EV_SYN, code: SYN_DROPPED, value: 0 });
        t.apply(Event { kind: EV_KEY, code: 0x130, value: 1 });
        assert_eq!(t.apply(Event { kind: EV_SYN, code: SYN_REPORT, value: 0 }).unwrap().buttons, 0);
    }

    #[test]
    fn axes_follow_their_ranges() {
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;
        let u8r = AbsRange { min: 0, max: 255 };
        assert!(near(u8r.unit(0), -1.0) && near(u8r.unit(255), 1.0) && near(u8r.unit(128), 0.0));
        let s16 = AbsRange { min: -32768, max: 32767 };
        assert!(near(s16.unit(0), 0.0) && near(s16.unit(-32768), -1.0) && near(s16.unit(99_999), 1.0));
        assert_eq!(AbsRange { min: 5, max: 5 }.unit(5), 0.0);
        assert_eq!((button(0x134), button(0x133), button(0x13b), button(0x999)), (Some(WEST), Some(NORTH), Some(START), None));
    }
}
