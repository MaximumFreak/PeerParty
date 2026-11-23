//! Controllers, read in the launcher's process (the game's own reads are held back by
//! the hook's padblock.rs while the overlay shows):
//!
//! - XInput (Xbox controllers, and anything Steam Input or DS4Windows presents as one),
//!   through `XInputGetStateEx` (ordinal 100) for the Guide button.
//! - HID, one reader thread per device: DualShock 4, DualSense, Switch Pro (Bluetooth's
//!   simple 0x3F report, and the full 0x30 report when something else has turned it on)
//!   and any other HID gamepad through the HID parser (buttons, hat, X/Y). Devices that
//!   are also XInput devices (`IG_` in their path) are skipped so they don't count twice.
//!
//! Every pad maps to one logical layout: south / east / west / north face buttons, the
//! shoulders, back / start / home, the stick clicks, the d-pad, both sticks and the
//! triggers. The overlay works with the buttons, the d-pad and the left stick; a
//! split-screen copy's virtual controller gets all of it (host/input_hub.rs). `Style` says which
//! glyphs to show for the pad that was used last. A Home press is noticed per pad
//! (`home_pressed`), so any connected controller opens the overlay, whatever the others
//! are holding. It counts only after that pad's Home was seen up: a pad that appears
//! with Home down (Steam Input swapping its own pad in as a Steam game starts), or Home
//! still down from closing Game Bar outside the game (`hold_home`), opens nothing.
//!
//! On Linux there's no XInput: every controller is an evdev device, read by
//! backend/linux/pads.rs into the same per-device entries (`Hid`) the HID readers fill.
//!
//! The scan and the readers are threads `Pads` owns (`Threads`): dropping it stops them
//! and waits for them (a reader blocked on a quiet device is woken), so none outlives it.
//! `readings` gives each controller apart, by seat, for a session that hands controllers
//! out by index (host/input_hub.rs); in a split-screen session seat `k` is player `k`'s,
//! and the session says which seats a controller may take (`set_open`: not a keyboard
//! player's). A controller takes a seat the first time it's used (a button, or a stick
//! or trigger pushed past half way), not when it's first seen: a controller app that
//! swaps its virtual pad as a game starts (a virtual Xbox pad for a virtual DualSense,
//! Steam Input's pad for the real one) can leave the old one plugged in and idle, and
//! that one mustn't take a player's seat. One pad read twice (a remapper's virtual pad
//! and the real one under it, both pressed at once with the same buttons) takes one
//! seat: the other is its twin, and takes the seat over if the first goes. A controller
//! keeps its seat while it's away (asleep, out of battery, a wireless drop), found again
//! by its slot, path or serial number, so the next player's controller isn't handed to
//! its copy; a seat whose controller is away goes to a new controller only when no
//! other seat is free.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const SOUTH: u32 = 1 << 0;
pub const EAST: u32 = 1 << 1;
pub const WEST: u32 = 1 << 2;
pub const NORTH: u32 = 1 << 3;
pub const LB: u32 = 1 << 4;
pub const RB: u32 = 1 << 5;
pub const BACK: u32 = 1 << 6;
pub const START: u32 = 1 << 7;
pub const HOME: u32 = 1 << 8;
pub const UP: u32 = 1 << 9;
pub const DOWN: u32 = 1 << 10;
pub const LEFT: u32 = 1 << 11;
pub const RIGHT: u32 = 1 << 12;
/// The stick clicks (L3 / R3).
pub const LSTICK: u32 = 1 << 13;
pub const RSTICK: u32 = 1 << 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Style {
    #[default]
    Xbox,
    PlayStation,
    Nintendo,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct State {
    pub buttons: u32,
    /// Left stick, -1..1, up positive.
    pub lx: f32,
    pub ly: f32,
    /// Right stick, the same way.
    pub rx: f32,
    pub ry: f32,
    /// Triggers, 0 (released) to 1.
    pub lt: f32,
    pub rt: f32,
}

impl State {
    /// Whether a player is using the controller: a button down, or a stick or trigger
    /// pushed past half way (a resting stick's drift doesn't count).
    pub fn in_use(self) -> bool {
        self.buttons != 0 || [self.lx, self.ly, self.rx, self.ry].iter().any(|v| v.abs() > 0.5) || self.lt > 0.5 || self.rt > 0.5
    }

    /// The buttons, with the stick as the d-pad (`stick`, from rest).
    pub fn with_stick(self) -> u32 {
        self.buttons | self.stick(0)
    }

    /// The stick as one d-pad direction: the axis pushed further, so a diagonal push
    /// doesn't press two (and switch a tab while moving down a list). It presses past
    /// half way and lets go under 0.35, so a stick resting near the line doesn't flicker
    /// between pressed and not. `held`: the direction it gave at the last poll.
    pub fn stick(self, held: u32) -> u32 {
        let along = |d: u32| match d {
            UP => self.ly,
            DOWN => -self.ly,
            LEFT => -self.lx,
            RIGHT => self.lx,
            _ => 0.0,
        };
        if held != 0 && along(held) > 0.35 {
            return held;
        }
        let (x, y) = if self.lx.abs() >= self.ly.abs() { (self.lx, 0.0) } else { (0.0, self.ly) };
        match () {
            _ if x > 0.5 => RIGHT,
            _ if x < -0.5 => LEFT,
            _ if y > 0.5 => UP,
            _ if y < -0.5 => DOWN,
            _ => 0,
        }
    }
}

/// `XInputGetStateEx`'s state: `XINPUT_STATE` and a DWORD of padding (20 bytes; a 16-byte
/// `XINPUT_STATE` buffer is overrun).
const STATE_LEN: usize = 20;

type GetStateFn = unsafe extern "system" fn(u32, *mut [u8; STATE_LEN]) -> u32;

/// One controller read on a thread of its own: a HID device on Windows, an evdev one on
/// Linux. The thread writes `state` and clears `alive` when the device goes.
pub(crate) struct Hid {
    pub(crate) path: String,
    pub(crate) vid: u16,
    pub(crate) pid: u16,
    pub(crate) style: Style,
    pub(crate) state: Arc<Mutex<State>>,
    pub(crate) alive: Arc<AtomicBool>,
    /// Home was down at the last poll, or hasn't been seen up yet.
    pub(crate) home: bool,
    /// The direction its stick gave at the last poll (`State::stick`).
    pub(crate) stick: u32,
    /// Its keys for the game's hook: by its serial number (`input_shm::hid_key`, 0 when
    /// it has none) and by where it's plugged in (`input_shm::hid_instance_key`, 0 when
    /// its path isn't a HID one). `key` picks one.
    pub(crate) serial_key: u64,
    pub(crate) inst_key: u64,
}

impl Hid {
    /// The key the game's hook knows this device by: its serial's, unless it has none
    /// or another device in `all` has the same (two identical pads with one serial
    /// number), then where it's plugged in. 0 when neither can be had.
    pub(crate) fn key(&self, all: &[Hid]) -> u64 {
        let shared = all.iter().any(|o| !Arc::ptr_eq(&o.alive, &self.alive) && o.alive.load(Ordering::Relaxed) && o.serial_key == self.serial_key);
        if self.serial_key != 0 && !shared { self.serial_key } else { self.inst_key }
    }
}

/// One controller as the last poll read it (`Pads::readings`).
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// "XInput slot 0", "HID 054c:0ce6": the log's name for it.
    pub name: String,
    pub style: Style,
    /// Its buttons (the stick not folded in), sticks and triggers.
    pub state: State,
    /// An XInput pad: its slot and its `XINPUT_GAMEPAD` as XInput gave it.
    pub xinput: Option<(u32, [u8; 12])>,
    /// A HID pad's key (`input_shm::hid_key`); 0 for none, or when another pad read has
    /// the same one (two pads of a model without serial numbers can't be told apart).
    pub hid_key: u64,
}

/// How long `Threads::stop_all` waits for its threads before leaving the rest to end by
/// themselves (each checks `stop` after its next read).
const STOP_WAIT: Duration = Duration::from_millis(500);

/// The threads reading controllers: a flag that asks them to end, and their handles,
/// joined by `stop_all` (which `Pads` calls when it's dropped).
pub(crate) struct Threads {
    stop: AtomicBool,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl Threads {
    pub(crate) fn new() -> Arc<Threads> {
        Arc::new(Threads { stop: AtomicBool::new(false), handles: Mutex::new(Vec::new()) })
    }

    /// Whether the threads have been asked to end.
    pub(crate) fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Runs `f` on a thread of its own, kept to be joined; false (nothing started) once
    /// stopping, or when the thread can't start.
    pub(crate) fn spawn(&self, name: &str, f: impl FnOnce() + Send + 'static) -> bool {
        if self.stopping() {
            return false;
        }
        let Ok(t) = std::thread::Builder::new().name(name.into()).spawn(f) else { return false };
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        // The ones that ended (their device went) are joined as new ones start.
        let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut *handles).into_iter().partition(|t| t.is_finished());
        *handles = running;
        handles.push(t);
        drop(handles);
        for t in done {
            let _ = t.join();
        }
        true
    }

    /// Asks every thread to end, wakes the ones blocked reading a quiet device, and
    /// waits for them (at most `wait`). Whether all of them ended.
    pub(crate) fn stop_all(&self, wait: Duration) -> bool {
        self.stop.store(true, Ordering::Release);
        let until = Instant::now() + wait;
        let mut handles = std::mem::take(&mut *self.handles.lock().unwrap_or_else(|e| e.into_inner()));
        loop {
            let (done, running): (Vec<_>, Vec<_>) = handles.into_iter().partition(|t| t.is_finished());
            for t in done {
                let _ = t.join();
            }
            handles = running;
            if handles.is_empty() {
                return true;
            }
            if Instant::now() >= until {
                // Left to end by themselves at their next read.
                return false;
            }
            for t in &handles {
                wake(t);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Ends a reader's blocked `ReadFile` (it then sees `stop`). One between reads isn't
/// blocked: it sees `stop` itself, and this is tried again until it has ended.
#[cfg(windows)]
fn wake(t: &JoinHandle<()>) {
    use std::os::windows::io::AsRawHandle;
    unsafe { windows_sys::Win32::System::IO::CancelSynchronousIo(t.as_raw_handle() as _) };
}

/// Linux's readers wait in `poll` with a timeout, and see `stop` by themselves.
#[cfg(not(windows))]
fn wake(_t: &JoinHandle<()>) {}

/// Which controller a seat holds (`Pads::readings`): an XInput slot, or a HID (evdev)
/// device by where it's plugged in and, when it has one no other device shares, its
/// serial number's key (`input_shm::hid_key`). The serial finds a pad again after it
/// was plugged back in elsewhere, or came back over Bluetooth on another path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seat {
    XInput(u32),
    Device { path: String, serial: u64 },
}

impl Seat {
    /// Whether `other` is the same controller: the same slot, the same path, or the
    /// same serial.
    fn same(&self, other: &Seat) -> bool {
        match (self, other) {
            (Seat::XInput(a), Seat::XInput(b)) => a == b,
            (Seat::Device { path: p, serial: s }, Seat::Device { path: q, serial: t }) => p == q || (*s != 0 && s == t),
            _ => false,
        }
    }
}

/// Whether a seat may be given to a controller used for the first time (`Pads::set_open`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Open {
    /// Yes: a player who plays with a controller, or may.
    Yes,
    /// Only when no `Yes` seat is free: a player who may use a controller but has been
    /// playing with the keyboard and mouse.
    Later,
    /// Never: a player on the keyboard and mouse, or a copy that ended.
    No,
}

/// Whether two controllers' buttons read as one pad's seen twice: something down, the
/// same on both.
fn alike(a: u32, b: u32) -> bool {
    a != 0 && a == b
}

/// Polls a twin may disagree with its pad in a row before it counts as a controller of
/// its own (about half a second): a second player who pressed the same button at the
/// same moment as the first, and was taken for the first's pad seen twice.
const TWIN_DISAGREES: u32 = 15;

/// A controller seen since `Pads` started, and what the last poll read of it.
#[derive(Clone, Debug)]
struct Known {
    id: Seat,
    /// "XInput slot 0", "HID 054c:0ce6": the log's name for it.
    name: String,
    style: Style,
    /// In use at the last poll (`State::in_use`).
    used: bool,
    buttons: u32,
    /// The poll at which a button last went down on it (`Pads::polls`).
    pressed_at: u64,
    /// Another controller this one is the same pad as (a remapper's virtual pad for a
    /// real one, or the real one under a remapper's): it takes no seat of its own.
    twin_of: Option<Seat>,
    /// Polls in a row it disagreed with that pad (`TWIN_DISAGREES`).
    disagrees: u32,
}

/// One controller as this poll read it.
struct Live {
    id: Seat,
    name: String,
    style: Style,
    state: State,
    /// Its stick as a d-pad direction (`State::stick`).
    dir: u32,
    /// Home went down (after it was seen up).
    home: bool,
}

/// What one seat holds now, for the overlay's list of this PC's players.
#[derive(Clone, Debug, PartialEq)]
pub struct SeatInfo {
    pub style: Style,
    /// "XInput slot 0", "HID 054c:0ce6".
    pub name: String,
    /// Connected now (a seat keeps a controller that went away, for its return).
    pub here: bool,
}

pub struct Pads {
    xinput: Option<GetStateFn>,
    /// Each seat's controller, None for a seat no controller holds: a controller's index
    /// in `per` and `readings` is its seat. In a split-screen session seat `k` is pane
    /// `k`'s (`set_open`); a copy alone hands seats out in the order controllers are
    /// first used.
    seats: Vec<Option<Seat>>,
    /// Which seats a controller used for the first time may take (`set_open`); empty:
    /// any, a new one at the end when they're all held.
    open: Vec<Open>,
    /// Every controller seen, seated or not.
    known: Vec<Known>,
    /// Polls so far (`Known::pressed_at`).
    polls: u64,
    /// When each XInput slot was last tried while disconnected (a call on an empty slot
    /// is slow, so those are tried every two seconds).
    tried: [Option<Instant>; 4],
    connected: [bool; 4],
    hid: Arc<Mutex<Vec<Hid>>>,
    last_scan: Option<Instant>,
    /// The scan running now, if one is (one at a time).
    scan: Option<JoinHandle<()>>,
    /// The scan's and the readers' threads, stopped and joined when this is dropped.
    threads: Arc<Threads>,
    /// What each XInput slot read at the last poll (`readings`), decoded and as it came.
    x_state: [State; 4],
    x_raw: [[u8; 12]; 4],
    /// The first HID scan has finished (until then `found` may be missing HID pads).
    scanned: Arc<AtomicBool>,
    pub style: Style,
    last_buttons: u32,
    /// Home was down at the last poll, or hasn't been seen up yet, per XInput slot.
    x_home: [bool; 4],
    /// The direction each XInput pad's stick gave at the last poll (`State::stick`).
    x_stick: [u32; 4],
    /// Each seat's controller's buttons at the last poll, the stick folded into the
    /// d-pad (0 for one away now): a split-screen player's overlay reads their own.
    pub per: Vec<u32>,
    /// The buttons of the controllers in use that hold no seat (more controllers than
    /// players, or none of the seats open): the focused player's overlay reads them.
    pub spare: u32,
    /// Every controller whose Home went down since the last poll: its seat (None for one
    /// without) and its name for the log (two players can press at once).
    pub homes: Vec<(Option<usize>, String)>,
    /// Controllers that took a seat during the last poll: `(seat, name)`.
    pub newly_seated: Vec<(usize, String)>,
    /// What the last poll decided about seats besides that, for the game's log: a
    /// controller taken for another one seen twice, a seat handed on.
    pub notes: Vec<String>,
}

impl Pads {
    pub fn new() -> Pads {
        Pads {
            xinput: xinput(),
            seats: Vec::new(),
            open: Vec::new(),
            known: Vec::new(),
            polls: 0,
            tried: [None; 4],
            connected: [false; 4],
            hid: Arc::new(Mutex::new(Vec::new())),
            last_scan: None,
            scan: None,
            threads: Threads::new(),
            x_state: [State::default(); 4],
            x_raw: [[0; 12]; 4],
            scanned: Arc::new(AtomicBool::new(false)),
            style: Style::Xbox,
            last_buttons: 0,
            x_home: [true; 4],
            x_stick: [0; 4],
            per: Vec::new(),
            spare: 0,
            homes: Vec::new(),
            newly_seated: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Home held on every controller: it has to come up before it counts again.
    pub fn hold_home(&mut self) {
        self.x_home = [true; 4];
        for h in self.hid.lock().unwrap().iter_mut() {
            h.home = true;
        }
    }

    /// A split-screen session's seats, one per pane: which of them a controller used for
    /// the first time may take. A seat that turns `No` lets its controller go (it takes
    /// another seat at its next use, if one is open).
    pub fn set_open(&mut self, open: Vec<Open>) {
        if self.seats.len() < open.len() {
            self.seats.resize(open.len(), None);
        }
        for (k, o) in open.iter().enumerate() {
            if *o == Open::No && self.seats[k].is_some() {
                let name = self.name_of(self.seats[k].as_ref());
                self.seats[k] = None;
                self.notes.push(format!("input: {name} let go of seat {k}: that player takes no controller now"));
            }
        }
        self.open = open;
    }

    /// Swaps the controllers of seats `a` and `b` (a player's overlay asked).
    pub fn swap(&mut self, a: usize, b: usize) {
        let n = self.seats.len().max(a + 1).max(b + 1);
        self.seats.resize(n, None);
        self.seats.swap(a, b);
    }

    /// Seat `k` lets its controller go: the next controller used takes it (that one
    /// included).
    pub fn free(&mut self, k: usize) {
        if let Some(s) = self.seats.get_mut(k) {
            *s = None;
        }
    }

    /// What each seat holds now (None: no controller).
    pub fn seat_info(&self) -> Vec<Option<SeatInfo>> {
        let hid = self.hid.lock().unwrap();
        self.seats
            .iter()
            .map(|s| {
                let s = s.as_ref()?;
                let k = self.known.iter().find(|k| k.id.same(s))?;
                let here = match s {
                    Seat::XInput(slot) => self.connected.get(*slot as usize).copied().unwrap_or(false),
                    Seat::Device { .. } => hid.iter().any(|h| h.alive.load(Ordering::Relaxed) && device_seat(h, &hid).same(s)),
                };
                Some(SeatInfo { style: k.style, name: k.name.clone(), here })
            })
            .collect()
    }

    /// A seat's controller's name for the log.
    fn name_of(&self, s: Option<&Seat>) -> String {
        s.and_then(|s| self.known.iter().find(|k| k.id.same(s))).map_or_else(|| "a controller".to_string(), |k| k.name.clone())
    }

    /// Every pad's buttons together, the stick folded into the d-pad.
    pub fn poll(&mut self) -> u32 {
        self.polls += 1;
        self.homes.clear();
        self.newly_seated.clear();
        if self.last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) && self.scan.as_ref().is_none_or(|t| t.is_finished()) {
            self.last_scan = Some(Instant::now());
            if let Some(t) = self.scan.take() {
                let _ = t.join();
            }
            let (hid, scanned, threads) = (self.hid.clone(), self.scanned.clone(), self.threads.clone());
            self.scan = std::thread::Builder::new()
                .name("peerparty-pad-scan".into())
                .spawn(move || {
                    scan_hid(&hid, &threads);
                    scanned.store(true, Ordering::Relaxed);
                })
                .ok();
        }
        let mut live: Vec<Live> = Vec::new();
        if let Some(get) = self.xinput {
            for pad in 0..4u32 {
                let i = pad as usize;
                if !self.connected[i] && self.tried[i].is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    continue;
                }
                let mut s = [0u8; STATE_LEN];
                let ok = unsafe { get(pad, &mut s) } == 0;
                self.connected[i] = ok;
                if !ok {
                    self.tried[i] = Some(Instant::now());
                    self.x_home[i] = true;
                    self.x_stick[i] = 0;
                    self.x_state[i] = State::default();
                    self.x_raw[i] = [0; 12];
                    continue;
                }
                let b = from_xinput(&s);
                self.x_state[i] = b;
                self.x_raw[i].copy_from_slice(&s[4..16]);
                let home = b.buttons & HOME != 0;
                let pressed = home && !self.x_home[i];
                self.x_home[i] = home;
                self.x_stick[i] = b.stick(self.x_stick[i]);
                live.push(Live { id: Seat::XInput(pad), name: format!("XInput slot {pad}"), style: Style::Xbox, state: b, dir: self.x_stick[i], home: pressed });
            }
        }
        {
            let hid = self.hid.clone();
            let mut hid = hid.lock().unwrap();
            hid.retain(|h| h.alive.load(Ordering::Relaxed));
            let ids: Vec<Seat> = hid.iter().map(|h| device_seat(h, &hid)).collect();
            for (h, id) in hid.iter_mut().zip(ids) {
                let s = *h.state.lock().unwrap();
                let home = s.buttons & HOME != 0;
                let pressed = home && !h.home;
                h.home = home;
                h.stick = s.stick(h.stick);
                live.push(Live { id, name: format!("HID {:04x}:{:04x}", h.vid, h.pid), style: h.style, state: s, dir: h.stick, home: pressed });
            }
        }
        self.place(&live);
        self.per.clear();
        self.per.resize(self.seats.len(), 0);
        self.spare = 0;
        let mut all = 0u32;
        for l in &live {
            let b = l.state.buttons | l.dir;
            all |= b;
            if l.state.buttons != 0 && l.state.buttons != self.last_buttons {
                self.style = l.style;
            }
            let twin = self.known.iter().any(|k| k.id.same(&l.id) && k.twin_of.is_some());
            let seat = self.seat_of(&l.id);
            match seat {
                Some(k) => self.per[k] = b,
                None if !twin && l.state.in_use() => self.spare |= b,
                None => {}
            }
            // A twin's Home is its pad's, which has one of its own.
            if l.home && !twin {
                self.homes.push((seat, l.name.clone()));
            }
        }
        self.last_buttons = all;
        all
    }

    /// The seat controller `id` holds.
    fn seat_of(&self, id: &Seat) -> Option<usize> {
        self.seats.iter().position(|s| s.as_ref().is_some_and(|s| s.same(id)))
    }

    /// The seat a controller used for the first time takes: the first open one free, then
    /// the first open one whose controller is away, `Yes` ones before `Later` ones. With
    /// no seats set (a copy alone), the first free one, or a new one at the end.
    fn free_seat(&mut self, live: &[Live]) -> Option<usize> {
        let away = |s: &Option<Seat>| s.as_ref().is_some_and(|s| !live.iter().any(|l| l.id.same(s)));
        if self.open.is_empty() {
            if let Some(k) = self.seats.iter().position(Option::is_none) {
                return Some(k);
            }
            self.seats.push(None);
            return Some(self.seats.len() - 1);
        }
        for want in [Open::Yes, Open::Later] {
            let open = |k: usize| self.open.get(k) == Some(&want);
            if let Some(k) = (0..self.seats.len()).find(|&k| open(k) && self.seats[k].is_none()) {
                return Some(k);
            }
            if let Some(k) = (0..self.seats.len()).find(|&k| open(k) && away(&self.seats[k])) {
                return Some(k);
            }
        }
        None
    }

    /// Seats the controllers used for the first time this poll, and keeps track of the
    /// ones that are another's twin. A controller takes a seat the moment it starts being
    /// used (a button down, a stick or trigger pushed past half way), not when it's
    /// first seen: a controller app's idle virtual pad mustn't take a player's seat.
    fn place(&mut self, live: &[Live]) {
        // Known ones follow their device to its path now.
        for l in live {
            match self.known.iter_mut().find(|k| k.id.same(&l.id)) {
                Some(k) => k.id = l.id.clone(),
                None => self.known.push(Known { id: l.id.clone(), name: l.name.clone(), style: l.style, used: false, buttons: 0, pressed_at: 0, twin_of: None, disagrees: 0 }),
            }
        }
        for s in self.seats.iter_mut().flatten() {
            if let Some(l) = live.iter().find(|l| l.id.same(s)) {
                *s = l.id.clone();
            }
        }
        // When a button last went down on each, before anything is decided: a pad and
        // its twin are pressed in the same poll, or one poll apart.
        let polls = self.polls;
        for l in live {
            if let Some(k) = self.known.iter_mut().find(|k| k.id.same(&l.id))
                && l.state.buttons & !k.buttons != 0
            {
                k.pressed_at = polls;
            }
        }
        let known = |me: &Pads, id: &Seat| me.known.iter().position(|k| k.id.same(id)).expect("every live controller is known");
        for l in live {
            let i = known(self, &l.id);
            if let Some(of) = self.known[i].twin_of.clone() {
                self.twin(i, l, &of, live);
            } else if l.state.in_use() && !self.known[i].used && self.seat_of(&l.id).is_none() {
                self.first_use(i, l, live);
            }
            let k = &mut self.known[i];
            k.used = l.state.in_use();
            k.buttons = l.state.buttons;
        }
    }

    /// Controller `known[i]` (`l`) started being used and holds no seat: it's another
    /// controller's twin, or it takes a seat if one is open.
    fn first_use(&mut self, i: usize, l: &Live, live: &[Live]) {
        let polls = self.polls;
        // The same pad seen twice (a remapper's virtual pad beside the real one, both
        // read here): pressed with a controller that holds a seat, the same buttons down.
        let partner = live.iter().find(|o| {
            !o.id.same(&l.id)
                && self.seat_of(&o.id).is_some()
                && alike(o.state.buttons, l.state.buttons)
                && self.known.iter().any(|k| k.id.same(&o.id) && k.twin_of.is_none() && k.pressed_at + 1 >= polls)
        });
        if let Some(o) = partner {
            self.known[i].twin_of = Some(o.id.clone());
            self.known[i].disagrees = 0;
            self.notes.push(format!(
                "input: {} went down with {} (the same buttons, the same moment): one pad seen twice (a controller app's virtual pad beside the real one), so it takes no seat of its own",
                l.name, o.name
            ));
            return;
        }
        let Some(k) = self.free_seat(live) else {
            if !self.open.is_empty() {
                self.notes.push(format!("input: {} was used, but no player's seat is open for a controller: it works the focused player's overlay only", l.name));
            }
            return;
        };
        if let Some(old) = self.seats[k].take() {
            let name = self.name_of(Some(&old));
            self.notes.push(format!("input: seat {k}'s controller ({name}) is away, so {} takes it", l.name));
        }
        self.seats[k] = Some(l.id.clone());
        self.newly_seated.push((k, l.name.clone()));
    }

    /// Twin `known[i]` (`l`) of controller `of`: it takes `of`'s seat when `of` goes
    /// (the remapper closed), and is a controller of its own again once it has
    /// disagreed with `of` for a while.
    fn twin(&mut self, i: usize, l: &Live, of: &Seat, live: &[Live]) {
        let Some(o) = live.iter().find(|o| o.id.same(of)) else {
            self.known[i].twin_of = None;
            if let Some(k) = self.seat_of(of) {
                self.seats[k] = Some(l.id.clone());
                self.notes.push(format!("input: {} went away; {}, the same pad seen twice, keeps its seat {k}", self.name_of(Some(of)), l.name));
            }
            return;
        };
        let (mine, theirs) = (l.state.buttons, o.state.buttons);
        let k = &mut self.known[i];
        if mine != 0 && !alike(mine, theirs) {
            k.disagrees += 1;
        } else if mine != 0 {
            k.disagrees = 0;
        }
        if k.disagrees >= TWIN_DISAGREES {
            k.twin_of = None;
            k.disagrees = 0;
            self.notes.push(format!("input: {} isn't {} seen twice after all (their buttons differ): it takes a seat of its own at its next use", l.name, o.name));
        }
    }
}

/// A HID (evdev) device's seat: its path, and its serial's key unless another device
/// that's there has the same one (identical pads with one serial number).
fn device_seat(h: &Hid, all: &[Hid]) -> Seat {
    let shared = all.iter().any(|o| !Arc::ptr_eq(&o.alive, &h.alive) && o.alive.load(Ordering::Relaxed) && o.serial_key == h.serial_key);
    Seat::Device { path: h.path.clone(), serial: if shared { 0 } else { h.serial_key } }
}

impl Drop for Pads {
    /// Stops the scan and the readers, and waits for them.
    fn drop(&mut self) {
        self.threads.stop.store(true, Ordering::Release);
        if let Some(t) = self.scan.take() {
            let _ = t.join();
        }
        self.threads.stop_all(STOP_WAIT);
    }
}

impl Pads {
    /// Each seat's controller as the last poll read it: None for a seat with none, or
    /// whose controller is away now. A seat isn't handed to another controller while
    /// one is open (`free_seat`), so a copy given seat 0 doesn't get the next player's.
    pub fn readings(&self) -> Vec<Option<Reading>> {
        let hid = self.hid.lock().unwrap();
        let mut out: Vec<Option<Reading>> = self
            .seats
            .iter()
            .map(|seat| match seat.as_ref()? {
                Seat::XInput(slot) => {
                    let i = *slot as usize;
                    (self.xinput.is_some() && self.connected[i]).then(|| Reading {
                        name: format!("XInput slot {slot}"),
                        style: Style::Xbox,
                        state: self.x_state[i],
                        xinput: Some((*slot, self.x_raw[i])),
                        hid_key: 0,
                    })
                }
                s @ Seat::Device { .. } => hid.iter().find(|h| h.alive.load(Ordering::Relaxed) && device_seat(h, &hid).same(s)).map(|h| Reading {
                    name: format!("HID {:04x}:{:04x}", h.vid, h.pid),
                    style: h.style,
                    state: *h.state.lock().unwrap(),
                    xinput: None,
                    hid_key: h.key(&hid),
                }),
            })
            .collect();
        unique_keys(&mut out);
        out
    }

    /// Whether the first HID scan is done, so `found` names every controller there is.
    pub fn scanned(&self) -> bool {
        self.scanned.load(Ordering::Relaxed)
    }

    /// The controllers the last poll read, by name ("XInput slot 0", "HID 054c:0ce6"),
    /// for the game's log.
    pub fn found(&self) -> Vec<String> {
        let mut out: Vec<String> = (0..4).filter(|&i| self.xinput.is_some() && self.connected[i]).map(|i| format!("XInput slot {i}")).collect();
        for h in self.hid.lock().unwrap().iter().filter(|h| h.alive.load(Ordering::Relaxed)) {
            out.push(format!("HID {:04x}:{:04x}", h.vid, h.pid));
        }
        out
    }

    /// Each controller read, with its buttons now (`peerparty --pads`).
    pub fn describe(&mut self) -> Vec<(String, u32)> {
        let mut out = Vec::new();
        if let Some(get) = self.xinput {
            for pad in 0..4u32 {
                let mut s = [0u8; STATE_LEN];
                if unsafe { get(pad, &mut s) } == 0 {
                    out.push((format!("XInput slot {pad}"), from_xinput(&s).with_stick()));
                }
            }
        }
        for h in self.hid.lock().unwrap().iter().filter(|h| h.alive.load(Ordering::Relaxed)) {
            let serial_hint = h.path.rsplit('#').nth(1).unwrap_or("");
            out.push((format!("HID {:04x}:{:04x} {:?} ({serial_hint})", h.vid, h.pid, h.style), h.state.lock().unwrap().with_stick()));
        }
        out
    }
}

/// A key two readings share names neither: the game's hook couldn't tell which device
/// is whose, so both are left to the virtual pad.
fn unique_keys(readings: &mut [Option<Reading>]) {
    let keys: Vec<u64> = readings.iter().flatten().map(|r| r.hid_key).collect();
    for r in readings.iter_mut().flatten() {
        if r.hid_key != 0 && keys.iter().filter(|k| **k == r.hid_key).count() > 1 {
            r.hid_key = 0;
        }
    }
}

/// The buttons in `b`, by name.
pub fn names(b: u32) -> String {
    let all = [
        (SOUTH, "A/Cross"),
        (EAST, "B/Circle"),
        (WEST, "X/Square"),
        (NORTH, "Y/Triangle"),
        (LB, "LB"),
        (RB, "RB"),
        (BACK, "View/Create"),
        (START, "Menu/Options"),
        (HOME, "Home/Guide/PS"),
        (LSTICK, "L3"),
        (RSTICK, "R3"),
        (UP, "Up"),
        (DOWN, "Down"),
        (LEFT, "Left"),
        (RIGHT, "Right"),
    ];
    let v: Vec<&str> = all.iter().filter(|(x, _)| b & x != 0).map(|(_, n)| *n).collect();
    if v.is_empty() { "-".into() } else { v.join(" + ") }
}

/// XInput is Windows' alone.
#[cfg(not(windows))]
fn xinput() -> Option<GetStateFn> {
    None
}

#[cfg(windows)]
fn xinput() -> Option<GetStateFn> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    for dll in ["xinput1_4.dll", "xinput1_3.dll"] {
        let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
        // From System32 and nowhere else (see `only_system_dlls` in main.rs).
        let m = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if m.is_null() {
            continue;
        }
        let f = unsafe { GetProcAddress(m, 100 as *const u8) }
            .or_else(|| unsafe { GetProcAddress(m, c"XInputGetState".as_ptr().cast()) });
        if let Some(f) = f {
            return Some(unsafe { std::mem::transmute::<_, GetStateFn>(f) });
        }
    }
    None
}

fn from_xinput(s: &[u8; STATE_LEN]) -> State {
    let w = u16::from_le_bytes([s[4], s[5]]);
    let map = [
        (0x0001, UP),
        (0x0002, DOWN),
        (0x0004, LEFT),
        (0x0008, RIGHT),
        (0x0010, START),
        (0x0020, BACK),
        (0x0040, LSTICK),
        (0x0080, RSTICK),
        (0x0100, LB),
        (0x0200, RB),
        (0x0400, HOME),
        (0x1000, SOUTH),
        (0x2000, EAST),
        (0x4000, WEST),
        (0x8000, NORTH),
    ];
    let buttons = map.iter().filter(|(x, _)| w & x != 0).fold(0, |b, (_, l)| b | l);
    let thumb = |at: usize| (i16::from_le_bytes([s[at], s[at + 1]]) as f32 / 32767.0).max(-1.0);
    let trigger = |v: u8| f32::from(v) / 255.0;
    State { buttons, lx: thumb(8), ly: thumb(10), rx: thumb(12), ry: thumb(14), lt: trigger(s[6]), rt: trigger(s[7]) }
}

/// A d-pad hat value (0 = up, clockwise, 8 or more = centred).
#[cfg(any(windows, test))]
fn hat(v: u8) -> u32 {
    match v {
        0 => UP,
        1 => UP | RIGHT,
        2 => RIGHT,
        3 => RIGHT | DOWN,
        4 => DOWN,
        5 => DOWN | LEFT,
        6 => LEFT,
        7 => LEFT | UP,
        _ => 0,
    }
}

#[cfg(any(windows, test))]
fn axis(v: u8) -> f32 {
    ((v as f32 - 128.0) / 127.0).max(-1.0)
}

/// An 8-bit trigger, 0..1.
#[cfg(any(windows, test))]
fn trigger(v: u8) -> f32 {
    f32::from(v) / 255.0
}

/// A digital trigger (the Switch Pro's ZL / ZR): released or all the way.
#[cfg(any(windows, test))]
fn pulled(down: bool) -> f32 {
    if down { 1.0 } else { 0.0 }
}

/// A value's logical range from a generic pad's report descriptor, and its size in bits.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Range {
    min: i32,
    max: i32,
    bits: u16,
}

#[cfg(any(windows, test))]
impl Range {
    fn new(min: i32, max: i32, bits: u16) -> Range {
        // A maximum written in too few bytes (65535 as two) comes back negative: it
        // means the field's unsigned top.
        let max = if max < min && min >= 0 && (1..32).contains(&bits) { ((1i64 << bits) - 1) as i32 } else { max };
        Range { min, max, bits }
    }

    /// The raw field as the number the descriptor means: a range below zero is
    /// two's complement in `bits`.
    fn signed(self, v: u32) -> i64 {
        if self.min < 0 && (1..32).contains(&self.bits) && (v >> (self.bits - 1)) & 1 == 1 {
            v as i64 - (1i64 << self.bits)
        } else {
            v as i64
        }
    }
}

/// A generic pad's stick and hat ranges, read once when it's opened.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default)]
struct Ranges {
    x: Option<Range>,
    y: Option<Range>,
    /// Z / Rz: the right stick on most DirectInput pads.
    z: Option<Range>,
    rz: Option<Range>,
    /// Rx / Ry: the right stick on pads without Z / Rz.
    rx: Option<Range>,
    ry: Option<Range>,
    hat: Option<Range>,
    /// Simulation Controls' Brake / Accelerator: the triggers, on pads that have them.
    brake: Option<Range>,
    accel: Option<Range>,
}

/// A stick value as -1..1 across its range. Without one (a descriptor Windows
/// couldn't read), a guess: 8 or 16 bits, unsigned.
#[cfg(any(windows, test))]
fn stick(v: u32, r: Option<Range>) -> f32 {
    match r {
        Some(r) if r.max > r.min => {
            let v = r.signed(v).clamp(r.min as i64, r.max as i64) as f64;
            let half = (r.max as f64 - r.min as f64) / 2.0;
            ((v - r.min as f64 - half) / half) as f32
        }
        _ if v <= 255 => axis(v as u8),
        _ => ((v as f32 - 32768.0) / 32767.0).clamp(-1.0, 1.0),
    }
}

/// The d-pad from a hat value: positions count from the range's minimum (some pads
/// start at 1), 4 of them on a 4-way hat. Outside the range is the null value pads send
/// for released, not a direction.
#[cfg(any(windows, test))]
fn hat_in(v: u32, r: Option<Range>) -> u32 {
    let Some(r) = r else { return hat(v.min(255) as u8) };
    let v = r.signed(v);
    if v < r.min as i64 || v > r.max as i64 || r.max - r.min > 8 {
        return 0;
    }
    let pos = (v - r.min as i64) as u8;
    if r.max - r.min == 3 { hat(pos * 2) } else { hat(pos) }
}

/// The logical range of usage `page`:`usage` in a pad's input reports.
#[cfg(windows)]
fn value_range(pre: isize, page: u16, usage: u16) -> Option<Range> {
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{HIDP_VALUE_CAPS, HidP_GetSpecificValueCaps, HidP_Input};
    let mut caps: [HIDP_VALUE_CAPS; 4] = unsafe { std::mem::zeroed() };
    let mut n = caps.len() as u16;
    let rc = unsafe { HidP_GetSpecificValueCaps(HidP_Input, page, 0, usage, caps.as_mut_ptr(), &mut n, pre) };
    (rc == 0x0011_0000 && n >= 1).then(|| Range::new(caps[0].LogicalMin, caps[0].LogicalMax, caps[0].BitSize))
}

/// DualShock 4 / DualSense in their common layout: sticks at 0..4, buttons from `b`,
/// the triggers' travel at `t` and `t + 1`.
#[cfg(any(windows, test))]
fn playstation(r: &[u8], b: usize, t: usize) -> Option<State> {
    if r.len() < (b + 3).max(t + 2) {
        return None;
    }
    let (b0, b1, b2) = (r[b], r[b + 1], r[b + 2]);
    let mut buttons = hat(b0 & 0x0f);
    let face = [(0x10, WEST), (0x20, SOUTH), (0x40, EAST), (0x80, NORTH)];
    buttons |= face.iter().filter(|(x, _)| b0 & x != 0).fold(0, |a, (_, l)| a | l);
    let more = [(0x01, LB), (0x02, RB), (0x10, BACK), (0x20, START), (0x40, LSTICK), (0x80, RSTICK)];
    buttons |= more.iter().filter(|(x, _)| b1 & x != 0).fold(0, |a, (_, l)| a | l);
    if b2 & 0x01 != 0 {
        buttons |= HOME;
    }
    Some(State { buttons, lx: axis(r[0]), ly: -axis(r[1]), rx: axis(r[2]), ry: -axis(r[3]), lt: trigger(r[t]), rt: trigger(r[t + 1]) })
}

/// Parses one input report from a known pad. `report` starts with the report id.
#[cfg(any(windows, test))]
fn parse_known(vid: u16, pid: u16, report: &[u8]) -> Option<State> {
    let (id, r) = report.split_first()?;
    match (vid, pid) {
        // DualShock 4 (v1, v2, wireless adapter).
        (0x054c, 0x05c4 | 0x09cc | 0x0ba0) => match id {
            // Triggers right after the buttons.
            0x01 => playstation(r, 4, 7),
            0x11 if r.len() > 6 => playstation(&r[2..], 4, 7),
            _ => None,
        },
        // DualSense, DualSense Edge.
        (0x054c, 0x0ce6 | 0x0df2) => match id {
            // The full report: triggers right after the sticks.
            0x01 if r.len() >= 10 && r.len() > 30 => playstation(r, 7, 4),
            // Bluetooth's simple report is laid out as the DualShock 4's.
            0x01 => playstation(r, 4, 7),
            0x31 if r.len() > 11 => playstation(&r[1..], 7, 4),
            _ => None,
        },
        // Switch Pro Controller.
        (0x057e, 0x2009) => match id {
            0x3f if r.len() >= 3 => {
                let (b0, b1) = (r[0], r[1]);
                let mut buttons = hat(r[2]);
                // Nintendo's positions: B south, A east, Y west, X north.
                let map0 = [(0x01, SOUTH), (0x02, EAST), (0x04, WEST), (0x08, NORTH), (0x10, LB), (0x20, RB)];
                buttons |= map0.iter().filter(|(x, _)| b0 & x != 0).fold(0, |a, (_, l)| a | l);
                let map1 = [(0x01, BACK), (0x02, START), (0x04, LSTICK), (0x08, RSTICK), (0x10, HOME)];
                buttons |= map1.iter().filter(|(x, _)| b1 & x != 0).fold(0, |a, (_, l)| a | l);
                // The sticks after the hat: four 16-bit values, centred on 0x8000, down
                // positive.
                let at = |i: usize| ((f32::from(u16::from_le_bytes([r[i], r[i + 1]])) - 32768.0) / 32767.0).clamp(-1.0, 1.0);
                let (lx, ly, rx, ry) = if r.len() >= 11 { (at(3), -at(5), at(7), -at(9)) } else { (0.0, 0.0, 0.0, 0.0) };
                Some(State { buttons, lx, ly, rx, ry, lt: pulled(b0 & 0x40 != 0), rt: pulled(b0 & 0x80 != 0) })
            }
            0x30 if r.len() >= 11 => {
                let (right, shared, left) = (r[2], r[3], r[4]);
                let map = [
                    (right, 0x01, WEST),
                    (right, 0x02, NORTH),
                    (right, 0x04, SOUTH),
                    (right, 0x08, EAST),
                    (right, 0x40, RB),
                    (shared, 0x01, BACK),
                    (shared, 0x02, START),
                    (shared, 0x04, RSTICK),
                    (shared, 0x08, LSTICK),
                    (shared, 0x10, HOME),
                    (left, 0x01, DOWN),
                    (left, 0x02, UP),
                    (left, 0x04, RIGHT),
                    (left, 0x08, LEFT),
                    (left, 0x40, LB),
                ];
                let buttons = map.iter().filter(|(v, x, _)| v & x != 0).fold(0, |a, (_, _, l)| a | l);
                // Each stick: two 12-bit values packed in three bytes, up positive.
                let stick_at = |at: usize| {
                    let raw_x = u16::from(r[at]) | (u16::from(r[at + 1] & 0x0f) << 8);
                    let raw_y = u16::from(r[at + 1] >> 4) | (u16::from(r[at + 2]) << 4);
                    let unit = |v: u16| ((v as f32 - 2048.0) / 1400.0).clamp(-1.0, 1.0);
                    (unit(raw_x), unit(raw_y))
                };
                let (lx, ly) = stick_at(5);
                let (rx, ry) = stick_at(8);
                Some(State { buttons, lx, ly, rx, ry, lt: pulled(left & 0x80 != 0), rt: pulled(right & 0x80 != 0) })
            }
            _ => None,
        },
        _ => None,
    }
}

/// The glyphs for a pad by its USB vendor.
pub(crate) fn style_of(vid: u16) -> Style {
    match vid {
        0x054c => Style::PlayStation,
        0x057e => Style::Nintendo,
        _ => Style::Xbox,
    }
}

/// Linux's controllers are its evdev devices.
#[cfg(unix)]
fn scan_hid(list: &Arc<Mutex<Vec<Hid>>>, threads: &Arc<Threads>) {
    crate::backend::linux::pads::scan(list, threads);
}

#[cfg(windows)]
fn scan_hid(list: &Arc<Mutex<Vec<Hid>>>, threads: &Arc<Threads>) {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    };
    use windows_sys::Win32::Devices::HumanInterfaceDevice::HidD_GetHidGuid;
    let mut guid = unsafe { std::mem::zeroed() };
    unsafe { HidD_GetHidGuid(&mut guid) };
    let set = unsafe { SetupDiGetClassDevsW(&guid, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE) };
    if set as isize == -1 {
        return;
    }
    let mut paths = Vec::new();
    for i in 0.. {
        let mut iface: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
        iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if unsafe { SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, i, &mut iface) } == 0 {
            break;
        }
        let mut buf = vec![0u8; 1024];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(set, &iface, detail, buf.len() as u32, std::ptr::null_mut(), std::ptr::null_mut())
        } == 0
        {
            continue;
        }
        let p = unsafe { std::ptr::addr_of!((*detail).DevicePath) } as *const u16;
        let mut n = 0;
        while unsafe { *p.add(n) } != 0 && n < 500 {
            n += 1;
        }
        paths.push(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) }));
    }
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    for path in paths {
        if threads.stopping() {
            return;
        }
        if path.to_ascii_lowercase().contains("ig_") {
            continue;
        }
        if list.lock().unwrap().iter().any(|h| h.path == path) {
            continue;
        }
        open_hid(list, threads, path);
    }
}

#[cfg(windows)]
fn open_hid(list: &Arc<Mutex<Vec<Hid>>>, threads: &Arc<Threads>, path: String) {
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HIDD_ATTRIBUTES, HIDP_CAPS, HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetPreparsedData, HidP_GetCaps,
    };
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let h = unsafe {
        CreateFileW(wide.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
    };
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    let close = || unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    let mut attr: HIDD_ATTRIBUTES = unsafe { std::mem::zeroed() };
    attr.Size = size_of::<HIDD_ATTRIBUTES>() as u32;
    let mut pre = 0isize;
    if unsafe { HidD_GetAttributes(h, &mut attr) } == 0 || unsafe { HidD_GetPreparsedData(h, &mut pre) } == 0 {
        close();
        return;
    }
    let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
    unsafe { HidP_GetCaps(pre, &mut caps) };
    // Generic desktop joystick (4) or gamepad (5) only.
    if caps.UsagePage != 1 || !(caps.Usage == 4 || caps.Usage == 5) || caps.InputReportByteLength == 0 {
        unsafe { HidD_FreePreparsedData(pre) };
        close();
        return;
    }
    let state = Arc::new(Mutex::new(State::default()));
    let alive = Arc::new(AtomicBool::new(true));
    let serial = serial_of(h);
    let serial_key = if serial.trim().is_empty() { 0 } else { peerparty_protocol::input_shm::hid_key(attr.VendorID, attr.ProductID, &serial) };
    let inst_key = peerparty_protocol::input_shm::hid_instance_key(attr.VendorID, attr.ProductID, &path).unwrap_or(0);
    let entry = Hid { path, vid: attr.VendorID, pid: attr.ProductID, style: style_of(attr.VendorID), state: state.clone(), alive: alive.clone(), home: true, stick: 0, serial_key, inst_key };
    let (vid, pid, len) = (attr.VendorID, attr.ProductID, caps.InputReportByteLength as usize);
    let ranges = Ranges {
        x: value_range(pre, 0x01, 0x30),
        y: value_range(pre, 0x01, 0x31),
        z: value_range(pre, 0x01, 0x32),
        rz: value_range(pre, 0x01, 0x35),
        rx: value_range(pre, 0x01, 0x33),
        ry: value_range(pre, 0x01, 0x34),
        hat: value_range(pre, 0x01, 0x39),
        brake: value_range(pre, 0x02, 0xC5),
        accel: value_range(pre, 0x02, 0xC4),
    };
    let (h, pre) = (h as usize, pre);
    let stop = threads.clone();
    let reader_alive = alive.clone();
    let read = move || {
        let alive = reader_alive;
        let mut buf = vec![0u8; len.max(64)];
        // A generic pad's trigger axes count only if they rest at the bottom of their
        // range in its first report: an axis that's really a stick's rests half way,
        // and would read as a trigger held half down.
        let mut triggers_rest: Option<bool> = None;
        // Ends when the device goes, or when `Pads` is dropped (`Threads::stop_all`
        // cancels a read that waits).
        while !stop.stopping() {
            let mut got = 0u32;
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::ReadFile(h as _, buf.as_mut_ptr(), len as u32, &mut got, std::ptr::null_mut())
            };
            if ok == 0 || got == 0 {
                break;
            }
            let report = &buf[..got as usize];
            let parsed = parse_known(vid, pid, report).or_else(|| {
                let mut s = parse_generic(pre, ranges, report)?;
                if !*triggers_rest.get_or_insert(s.lt < 0.25 && s.rt < 0.25) {
                    (s.lt, s.rt) = (0.0, 0.0);
                }
                Some(s)
            });
            if let Some(s) = parsed {
                *state.lock().unwrap() = s;
            }
        }
        alive.store(false, Ordering::Relaxed);
        unsafe { HidD_FreePreparsedData(pre) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h as _) };
    };
    list.lock().unwrap().push(entry);
    if !threads.spawn("peerparty-pad-hid", read) {
        // Stopping, or no thread: the reader never ran, so the device is taken off the
        // list and its handles are freed here.
        list.lock().unwrap().retain(|e| !Arc::ptr_eq(&e.alive, &alive));
        unsafe { HidD_FreePreparsedData(pre) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h as _) };
    }
}

/// A HID device's serial number, empty when it has none (the game's hook reads the same,
/// `input_shm::hid_key`).
#[cfg(windows)]
fn serial_of(h: windows_sys::Win32::Foundation::HANDLE) -> String {
    let mut buf = [0u16; 128];
    let ok = unsafe { windows_sys::Win32::Devices::HumanInterfaceDevice::HidD_GetSerialNumberString(h, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32) };
    if ok == 0 {
        return String::new();
    }
    let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// Any other gamepad, through Windows' HID parser: buttons 1-4 as the face buttons
/// (south, east, west, north: the common order), 5-6 shoulders, 7-8 back / start, 9-10
/// the stick clicks, 13 home; the hat switch as the d-pad; X / Y as the left stick, and
/// Z / Rz as the right one (DirectInput's usual) unless it has Rx / Ry too. The triggers:
/// Simulation Controls' Brake and Accelerator where it has them, else Z and Rz beside an
/// Rx / Ry right stick, else Z alone beside one (both triggers on one axis, the left
/// above its middle); pads without any of those have digital ones among their buttons.
/// (A trigger axis that doesn't rest at its bottom is dropped by the reader.) Home is also whichever of the
/// usages pads give it instead of a button number: Generic Desktop's System Main Menu
/// (0x85) and Consumer's AC Home (0x223). Sticks and hat are read by their descriptor's
/// `ranges`: a centred stick isn't always 128, and a released hat isn't always 8.
#[cfg(windows)]
fn parse_generic(pre: isize, ranges: Ranges, report: &[u8]) -> Option<State> {
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{HidP_GetUsageValue, HidP_GetUsages, HidP_Input};
    let mut usages = [0u16; 32];
    let mut n = usages.len() as u32;
    let rc = unsafe {
        HidP_GetUsages(HidP_Input, 0x09, 0, usages.as_mut_ptr(), &mut n, pre, report.as_ptr() as *mut _, report.len() as u32)
    };
    if rc != 0x0011_0000 {
        return None;
    }
    let map = [
        (1, SOUTH),
        (2, EAST),
        (3, WEST),
        (4, NORTH),
        (5, LB),
        (6, RB),
        (7, BACK),
        (8, START),
        (9, LSTICK),
        (10, RSTICK),
        (13, HOME),
    ];
    let mut buttons = 0;
    for u in &usages[..n as usize] {
        if let Some((_, b)) = map.iter().find(|(x, _)| x == u) {
            buttons |= b;
        }
    }
    for (page, usage) in [(0x01u16, 0x85u16), (0x0C, 0x223)] {
        let mut other = [0u16; 16];
        let mut m = other.len() as u32;
        let rc = unsafe {
            HidP_GetUsages(HidP_Input, page, 0, other.as_mut_ptr(), &mut m, pre, report.as_ptr() as *mut _, report.len() as u32)
        };
        if rc == 0x0011_0000 && other[..m as usize].contains(&usage) {
            buttons |= HOME;
        }
    }
    let value = |usage: u16| -> Option<u32> {
        let mut v = 0u32;
        let rc = unsafe { HidP_GetUsageValue(HidP_Input, 0x01, 0, usage, &mut v, pre, report.as_ptr() as *mut _, report.len() as u32) };
        (rc == 0x0011_0000).then_some(v)
    };
    if let Some(h) = value(0x39) {
        buttons |= hat_in(h, ranges.hat);
    }
    let pair = |x: u16, rx: Option<Range>, y: u16, ry: Option<Range>| match (value(x), value(y)) {
        (Some(vx), Some(vy)) => Some((stick(vx, rx), -stick(vy, ry))),
        _ => None,
    };
    let (lx, ly) = pair(0x30, ranges.x, 0x31, ranges.y).unwrap_or_default();
    let rxry = ranges.rx.is_some() && ranges.ry.is_some();
    let right = if ranges.z.is_some() && ranges.rz.is_some() && !rxry {
        pair(0x32, ranges.z, 0x35, ranges.rz)
    } else {
        pair(0x33, ranges.rx, 0x34, ranges.ry)
    };
    let (rx, ry) = right.unwrap_or_default();
    let sim = |usage: u16, r: Option<Range>| -> Option<f32> {
        let mut v = 0u32;
        let rc = unsafe { HidP_GetUsageValue(HidP_Input, 0x02, 0, usage, &mut v, pre, report.as_ptr() as *mut _, report.len() as u32) };
        (rc == 0x0011_0000).then(|| pulled_by(v, r))
    };
    let (lt, rt) = if ranges.brake.is_some() || ranges.accel.is_some() {
        (sim(0xC5, ranges.brake).unwrap_or(0.0), sim(0xC4, ranges.accel).unwrap_or(0.0))
    } else if rxry && ranges.z.is_some() && ranges.rz.is_some() {
        (value(0x32).map_or(0.0, |v| pulled_by(v, ranges.z)), value(0x35).map_or(0.0, |v| pulled_by(v, ranges.rz)))
    } else if rxry && ranges.z.is_some() {
        value(0x32).map_or((0.0, 0.0), |v| shared_triggers(stick(v, ranges.z)))
    } else {
        (0.0, 0.0)
    };
    Some(State { buttons, lx, ly, rx, ry, lt, rt })
}

/// A trigger axis, 0 at the bottom of its range to 1 at the top. Without a range, a
/// guess: 8 or 16 bits.
#[cfg(any(windows, test))]
fn pulled_by(v: u32, r: Option<Range>) -> f32 {
    match r {
        Some(r) if r.max > r.min => ((r.signed(v).clamp(r.min as i64, r.max as i64) - r.min as i64) as f64 / (r.max as f64 - r.min as f64)) as f32,
        _ if v <= 255 => trigger(v as u8),
        _ => (v as f32 / 65535.0).clamp(0.0, 1.0),
    }
}

/// Both triggers on one axis (`stick`'s -1..1): the left one above its middle, the
/// right one below.
#[cfg(any(windows, test))]
fn shared_triggers(z: f32) -> (f32, f32) {
    (z.max(0.0), (-z).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_reports_parse() {
        // DualShock 4 over USB: cross and d-pad right, PS button.
        let mut r = vec![0u8; 64];
        r[0] = 0x01;
        r[1] = 128;
        r[2] = 128;
        r[5] = 0x20 | 2;
        r[7] = 0x01;
        // Right stick full right, R3, L2 all the way.
        r[3] = 255;
        r[4] = 128;
        r[6] = 0x80;
        r[8] = 255;
        let s = parse_known(0x054c, 0x09cc, &r).unwrap();
        assert_eq!(s.buttons, SOUTH | RIGHT | HOME | RSTICK);
        assert!(s.rx > 0.99 && s.ry.abs() < 0.01 && s.lt > 0.99 && s.rt == 0.0, "{s:?}");
        // DualSense over USB: triggers right after the sticks, buttons from byte 7.
        let mut d = vec![0u8; 64];
        d[0] = 0x01;
        d[1..5].copy_from_slice(&[128, 128, 128, 0]);
        d[6] = 200;
        d[8] = 0x20 | 8;
        let s = parse_known(0x054c, 0x0ce6, &d).unwrap();
        assert_eq!(s.buttons, SOUTH);
        assert!(s.ry > 0.99 && s.rt > 0.78 && s.lt == 0.0, "right stick up, R2 most of the way: {s:?}");
        // Switch Pro simple report: A (east) and home.
        let s = parse_known(0x057e, 0x2009, &[0x3f, 0x02, 0x10, 0x08]).unwrap();
        assert_eq!(s.buttons, EAST | HOME);
        // Switch Pro full report: the right stick packed as the left is, ZR.
        let mut n = vec![0u8; 13];
        n[0] = 0x30;
        n[3] = 0x80;
        for at in [6, 9] {
            n[at] = 0x00;
            n[at + 1] = 0x08;
            n[at + 2] = 0x80;
        }
        n[9] = 0xFF;
        n[10] = 0x0F;
        let s = parse_known(0x057e, 0x2009, &n).unwrap();
        assert!(s.rx > 0.99 && s.rt == 1.0 && s.lt == 0.0, "{s:?}");
        // XInput: A, start, d-pad up, both sticks and triggers.
        let mut x = [0u8; STATE_LEN];
        x[4..6].copy_from_slice(&(0x1000u16 | 0x0010 | 0x0001 | 0x0080).to_le_bytes());
        x[6] = 255;
        x[12..14].copy_from_slice(&(-32768i16).to_le_bytes());
        x[14..16].copy_from_slice(&16384i16.to_le_bytes());
        let s = from_xinput(&x);
        assert_eq!(s.buttons, SOUTH | START | UP | RSTICK);
        assert!(s.rx == -1.0 && (s.ry - 0.5).abs() < 0.01 && s.lt == 1.0 && s.rt == 0.0, "{s:?}");
        // Stick folds into the d-pad.
        assert_eq!(State { ly: -0.9, ..Default::default() }.with_stick(), DOWN);
    }

    /// A diagonal push is one direction, the axis pushed further; a held direction
    /// lets go only under 0.35, and turning the stick hands over to the next.
    #[test]
    fn the_stick_is_one_direction_with_hysteresis() {
        let st = |lx: f32, ly: f32| State { lx, ly, ..Default::default() };
        assert_eq!(st(0.6, -0.8).stick(0), DOWN, "down and a little right is down");
        assert_eq!(st(0.8, -0.6).stick(0), RIGHT);
        assert_eq!(st(0.0, 0.45).stick(0), 0, "not yet half way");
        assert_eq!(st(0.0, 0.45).stick(UP), UP, "held: still up");
        assert_eq!(st(0.0, 0.3).stick(UP), 0, "let go");
        assert_eq!(st(0.7, 0.4).stick(UP), UP, "turning: up holds while it's past 0.35");
        assert_eq!(st(0.8, 0.2).stick(UP), RIGHT, "then right takes over");
    }

    /// Generic pads' values read by their descriptor's ranges.
    #[test]
    fn generic_values_follow_their_ranges() {
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;
        // 8-bit unsigned, the common case: ends and middle.
        let u8r = Some(Range::new(0, 255, 8));
        assert!(near(stick(0, u8r), -1.0) && near(stick(255, u8r), 1.0) && near(stick(128, u8r), 0.0));
        // Signed 16-bit: -1 arrives as 0xFFFF and is centred, not full right.
        let s16 = Some(Range::new(-32768, 32767, 16));
        assert!(near(stick(0xFFFF, s16), 0.0) && near(stick(0x8000, s16), -1.0) && near(stick(0x7FFF, s16), 1.0));
        // 10-bit and 12-bit sticks: their own middle is centred.
        assert!(near(stick(512, Some(Range::new(0, 1023, 10))), 0.0));
        assert!(near(stick(2048, Some(Range::new(0, 4095, 12))), 0.0));
        // 65535 written in two bytes reads back as -1: still 16-bit unsigned.
        let wrapped = Some(Range::new(0, -1, 16));
        assert!(near(stick(32768, wrapped), 0.0) && near(stick(65535, wrapped), 1.0));
        // No range: the old guess.
        assert!(near(stick(128, None), 0.0) && near(stick(32768, None), 0.0));
        // Hat 0-7 with 8 as null, hat 1-8 with 0 as null, and 4-way 0-3.
        let h07 = Some(Range::new(0, 7, 4));
        assert_eq!((hat_in(0, h07), hat_in(3, h07), hat_in(8, h07), hat_in(15, h07)), (UP, RIGHT | DOWN, 0, 0));
        let h18 = Some(Range::new(1, 8, 4));
        assert_eq!((hat_in(1, h18), hat_in(8, h18), hat_in(0, h18)), (UP, LEFT | UP, 0));
        let h03 = Some(Range::new(0, 3, 2));
        assert_eq!((hat_in(1, h03), hat_in(3, h03)), (RIGHT, LEFT));
        assert_eq!((hat_in(2, None), hat_in(258, None)), (RIGHT, 0));
    }

    /// One XInput pad on slot 0 whose buttons are `FAKE_BUTTONS`.
    static FAKE_BUTTONS: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    unsafe extern "system" fn fake_get(pad: u32, s: *mut [u8; STATE_LEN]) -> u32 {
        if pad != 0 {
            return 1167; // ERROR_DEVICE_NOT_CONNECTED
        }
        unsafe { (&mut *s)[4..6].copy_from_slice(&FAKE_BUTTONS.load(Ordering::Relaxed).to_le_bytes()) };
        0
    }

    /// Threads that end by themselves when asked, and one blocked reading something
    /// quiet, all end when stopped; nothing starts afterwards.
    #[test]
    fn reader_threads_stop_and_are_joined() {
        let threads = Threads::new();
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..3 {
            let (t, ran) = (threads.clone(), ran.clone());
            assert!(threads.spawn("test-reader", move || {
                while !t.stopping() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                ran.fetch_add(1, Ordering::SeqCst);
            }));
        }
        // A reader that ended by itself is joined when the next one starts.
        assert!(threads.spawn("test-done", || {}));
        #[cfg(windows)]
        let pipe_ends = {
            use windows_sys::Win32::System::Pipes::CreatePipe;
            let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
            assert_ne!(unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 0) }, 0);
            let (rh, t) = (r as usize, threads.clone());
            // Blocked in a read nothing ever answers, like a quiet controller's.
            assert!(threads.spawn("test-blocked", move || {
                while !t.stopping() {
                    let (mut buf, mut got) = ([0u8; 8], 0u32);
                    if unsafe { windows_sys::Win32::Storage::FileSystem::ReadFile(rh as _, buf.as_mut_ptr(), 8, &mut got, std::ptr::null_mut()) } == 0 {
                        break;
                    }
                }
            }));
            (r, w)
        };
        let t0 = Instant::now();
        assert!(threads.stop_all(Duration::from_secs(5)), "every thread ended");
        assert!(t0.elapsed() < Duration::from_secs(2), "promptly: {:?}", t0.elapsed());
        assert_eq!(ran.load(Ordering::SeqCst), 3);
        assert!(threads.handles.lock().unwrap().is_empty());
        assert!(!threads.spawn("too-late", || {}), "nothing starts once stopping");
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(pipe_ends.0);
            windows_sys::Win32::Foundation::CloseHandle(pipe_ends.1);
        }
    }

    /// Dropping `Pads` stops its scan and readers, and waits for them.
    #[test]
    fn pads_stop_their_threads_when_dropped() {
        let mut pads = Pads::new();
        pads.xinput = None;
        pads.poll();
        assert!(pads.scan.is_some(), "the first poll scans");
        let threads = pads.threads.clone();
        drop(pads);
        assert!(threads.stopping());
        assert!(threads.handles.lock().unwrap().is_empty(), "joined");
    }

    /// Each controller apart, by slot: what a session hands out by index.
    #[test]
    fn readings_list_each_controller_apart() {
        let mut pads = Pads::new();
        pads.xinput = Some(fake_second);
        pads.last_scan = Some(Instant::now());
        pads.poll();
        let r = pads.readings();
        assert_eq!(r.len(), 1, "{r:?}");
        let r0 = r[0].as_ref().unwrap();
        assert_eq!((r0.name.as_str(), r0.state.buttons, r0.style), ("XInput slot 1", SOUTH | UP, Style::Xbox));
    }

    /// A controller keeps its seat: the first one seen going away leaves seat 0 empty
    /// and the second stays on seat 1 (its copy doesn't get the first player's); back,
    /// the first takes seat 0 again.
    #[test]
    fn a_controller_keeps_its_seat_while_another_is_away() {
        let _g = FAKE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut pads = Pads::new();
        pads.xinput = Some(fake_two);
        pads.last_scan = Some(Instant::now());
        let names = |pads: &Pads| pads.readings().iter().map(|r| r.as_ref().map(|r| r.name.clone())).collect::<Vec<_>>();
        let (s0, s1) = (Some("XInput slot 0".to_string()), Some("XInput slot 1".to_string()));
        FAKE_SLOT0.store(true, Ordering::Relaxed);
        pads.poll();
        assert_eq!(names(&pads), [s0.clone(), s1.clone()]);
        FAKE_SLOT0.store(false, Ordering::Relaxed);
        pads.poll();
        assert_eq!(names(&pads), [None, s1.clone()], "slot 1 keeps seat 1");
        assert_eq!(pads.per, [0, SOUTH], "its buttons on its own seat");
        FAKE_SLOT0.store(true, Ordering::Relaxed);
        pads.tried = [None; 4];
        pads.poll();
        assert_eq!(names(&pads), [s0, s1]);
    }

    static FAKE_SLOT0: AtomicBool = AtomicBool::new(true);
    /// What slot 0 holds (B, unless a test says otherwise).
    static FAKE_SLOT0_BUTTONS: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0x2000);
    /// The tests that change FAKE_SLOT0 / FAKE_SLOT0_BUTTONS run one at a time.
    static FAKE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A controller that's plugged in but never used (a controller app's parked virtual
    /// pad) takes no seat: the one used after it takes seat 0, and it gets seat 1 only
    /// once it's used.
    #[test]
    fn a_controller_takes_a_seat_when_its_first_used() {
        let _g = FAKE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut pads = Pads::new();
        pads.xinput = Some(fake_two);
        pads.last_scan = Some(Instant::now());
        let names = |pads: &Pads| pads.readings().iter().map(|r| r.as_ref().map(|r| r.name.clone())).collect::<Vec<_>>();
        FAKE_SLOT0.store(true, Ordering::Relaxed);
        FAKE_SLOT0_BUTTONS.store(0, Ordering::Relaxed);
        pads.poll();
        assert_eq!(names(&pads), [Some("XInput slot 1".to_string())], "slot 0 idle: no seat");
        assert_eq!(pads.per, [SOUTH]);
        FAKE_SLOT0_BUTTONS.store(0x2000, Ordering::Relaxed);
        pads.poll();
        FAKE_SLOT0_BUTTONS.store(0, Ordering::Relaxed);
        pads.poll();
        assert_eq!(names(&pads), [Some("XInput slot 1".to_string()), Some("XInput slot 0".to_string())], "kept once used");
        FAKE_SLOT0_BUTTONS.store(0x2000, Ordering::Relaxed);
    }

    #[test]
    fn resting_sticks_and_triggers_are_not_use() {
        let rest = State { lx: 0.1, ly: -0.2, rx: 0.05, ry: 0.0, lt: 0.1, rt: 0.0, buttons: 0 };
        assert!(!rest.in_use());
        assert!(State { ly: 0.8, ..rest }.in_use());
        assert!(State { rt: 0.9, ..rest }.in_use());
        assert!(State { buttons: START, ..rest }.in_use());
    }

    /// Slot 0 there while `FAKE_SLOT0` says so (holding `FAKE_SLOT0_BUTTONS`), slot 1
    /// holding A, nothing on the others.
    unsafe extern "system" fn fake_two(pad: u32, s: *mut [u8; STATE_LEN]) -> u32 {
        match pad {
            0 if FAKE_SLOT0.load(Ordering::Relaxed) => {
                unsafe { (&mut *s)[4..6].copy_from_slice(&FAKE_SLOT0_BUTTONS.load(Ordering::Relaxed).to_le_bytes()) };
                0
            }
            1 => {
                unsafe { (&mut *s)[4..6].copy_from_slice(&0x1000u16.to_le_bytes()) };
                0
            }
            _ => 1167,
        }
    }

    /// A pad on XInput slot 1 holding A and up, and nothing on the others.
    unsafe extern "system" fn fake_second(pad: u32, s: *mut [u8; STATE_LEN]) -> u32 {
        if pad != 1 {
            return 1167;
        }
        unsafe { (&mut *s)[4..6].copy_from_slice(&0x1001u16.to_le_bytes()) };
        0
    }

    #[test]
    fn home_counts_only_after_it_was_seen_up() {
        let mut pads = Pads::new();
        pads.xinput = Some(fake_get);
        pads.last_scan = Some(Instant::now());
        let press = |pads: &mut Pads, buttons: u16| {
            FAKE_BUTTONS.store(buttons, Ordering::Relaxed);
            pads.poll();
            !pads.homes.is_empty()
        };
        // Down the first time the pad is seen (Steam Input's pad arriving): nothing.
        assert!(!press(&mut pads, 0x0400));
        assert!(!press(&mut pads, 0));
        assert!(press(&mut pads, 0x0400));
        assert_eq!(pads.homes, [(Some(0), "XInput slot 0".to_string())]);
        assert_eq!(pads.per, [HOME], "its buttons apart");
        assert!(!press(&mut pads, 0x0400));
        // Still down when the game comes back to the front: nothing until it comes up.
        assert!(!press(&mut pads, 0));
        pads.hold_home();
        assert!(!press(&mut pads, 0x0400));
        assert!(!press(&mut pads, 0));
        assert!(press(&mut pads, 0x0400));
    }

    /// XInput slots as the tests set them: `u32::MAX` for an empty slot, else its
    /// `wButtons`.
    static FAKE_PADS: [std::sync::atomic::AtomicU32; 4] = [const { std::sync::atomic::AtomicU32::new(u32::MAX) }; 4];
    /// The tests that use FAKE_PADS run one at a time.
    static FAKE_PADS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    unsafe extern "system" fn fake_pads(pad: u32, s: *mut [u8; STATE_LEN]) -> u32 {
        match FAKE_PADS.get(pad as usize).map(|b| b.load(Ordering::Relaxed)) {
            Some(b) if b != u32::MAX => {
                unsafe { (&mut *s)[4..6].copy_from_slice(&(b as u16).to_le_bytes()) };
                0
            }
            _ => 1167,
        }
    }

    /// Pads reading `FAKE_PADS` (all empty) and the HID devices the test adds.
    fn fake_session() -> (std::sync::MutexGuard<'static, ()>, Pads) {
        let g = FAKE_PADS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for p in &FAKE_PADS {
            p.store(u32::MAX, Ordering::Relaxed);
        }
        let mut pads = Pads::new();
        pads.xinput = Some(fake_pads);
        pads.last_scan = Some(Instant::now());
        (g, pads)
    }

    /// A poll, with every empty slot tried again (no two-second wait).
    fn poll(pads: &mut Pads) {
        pads.tried = [None; 4];
        pads.notes.clear();
        pads.poll();
    }

    fn set_x(slot: usize, buttons: u32) {
        FAKE_PADS[slot].store(buttons, Ordering::Relaxed);
    }

    /// A HID device on the list, as the scan adds one: its state and liveness to set.
    fn add_hid(pads: &Pads, path: &str, serial_key: u64) -> (Arc<Mutex<State>>, Arc<AtomicBool>) {
        let (state, alive) = (Arc::new(Mutex::new(State::default())), Arc::new(AtomicBool::new(true)));
        let h = Hid { path: path.into(), vid: 0x054c, pid: 0x09cc, style: Style::PlayStation, state: state.clone(), alive: alive.clone(), home: true, stick: 0, serial_key, inst_key: 0 };
        pads.hid.lock().unwrap().push(h);
        (state, alive)
    }

    fn names(pads: &Pads) -> Vec<Option<String>> {
        pads.readings().iter().map(|r| r.as_ref().map(|r| r.name.clone())).collect()
    }

    /// A keyboard player's seat takes no controller; one that has been playing with the
    /// keyboard (`Later`) takes one only once the others are held.
    #[test]
    fn controllers_take_the_open_seats_in_order() {
        let (_g, mut pads) = fake_session();
        pads.set_open(vec![Open::Later, Open::No, Open::Yes]);
        set_x(0, 0x1000);
        poll(&mut pads);
        assert_eq!(pads.newly_seated, [(2, "XInput slot 0".to_string())], "past the keyboard players, to player 3");
        set_x(1, 0x2000);
        poll(&mut pads);
        assert_eq!(pads.newly_seated, [(0, "XInput slot 1".to_string())], "the one who was on the keyboard takes the next");
        set_x(2, 0x4000);
        poll(&mut pads);
        assert!(pads.newly_seated.is_empty(), "no seat left");
        assert_eq!(pads.per, [EAST, 0, SOUTH]);
        assert_eq!(pads.spare, WEST, "the seatless one works the focused overlay");
        assert!(pads.notes.iter().any(|n| n.contains("no player's seat is open")), "{:?}", pads.notes);
        // A seat closed lets its controller go.
        pads.set_open(vec![Open::No, Open::No, Open::Yes]);
        assert_eq!(names(&pads), [None, None, Some("XInput slot 0".to_string())]);
    }

    /// One pad read twice (a remapper's virtual Xbox pad and the real one) takes one
    /// seat; the twin takes it over when the first goes away.
    #[test]
    fn one_pad_seen_twice_takes_one_seat() {
        let (_g, mut pads) = fake_session();
        pads.set_open(vec![Open::Yes, Open::Yes]);
        let (ds4, _alive) = add_hid(&pads, "hid-ds4", 7);
        set_x(0, 0x1000);
        *ds4.lock().unwrap() = State { buttons: SOUTH, ..Default::default() };
        poll(&mut pads);
        assert_eq!(names(&pads), [Some("XInput slot 0".to_string()), None], "one seat, not two");
        assert!(pads.notes.iter().any(|n| n.contains("one pad seen twice")), "{:?}", pads.notes);
        assert!(pads.spare == 0, "the twin works nothing of its own");
        set_x(0, 0);
        *ds4.lock().unwrap() = State::default();
        poll(&mut pads);
        // The real one's press a poll behind the virtual one's is the same pad's too.
        set_x(0, 0x2000);
        poll(&mut pads);
        *ds4.lock().unwrap() = State { buttons: EAST, ..Default::default() };
        poll(&mut pads);
        assert_eq!(names(&pads), [Some("XInput slot 0".to_string()), None]);
        // The remapper closes: its pad goes, the real one keeps the seat.
        set_x(0, u32::MAX);
        poll(&mut pads);
        assert_eq!(names(&pads), [Some("HID 054c:09cc".to_string()), None]);
        assert!(pads.notes.iter().any(|n| n.contains("keeps its seat 0")), "{:?}", pads.notes);
    }

    /// Two players who happened to press the same button at once: the second is taken
    /// for the first's pad seen twice until their buttons differ, then takes a seat.
    #[test]
    fn a_twin_that_disagrees_is_a_controller_of_its_own() {
        let (_g, mut pads) = fake_session();
        pads.set_open(vec![Open::Yes, Open::Yes]);
        set_x(0, 0x1000);
        set_x(1, 0x1000);
        poll(&mut pads);
        assert_eq!(names(&pads), [Some("XInput slot 0".to_string()), None]);
        set_x(0, 0);
        set_x(1, 0x2000);
        let mut told = false;
        for _ in 0..TWIN_DISAGREES {
            poll(&mut pads);
            told |= pads.notes.iter().any(|n| n.contains("after all"));
        }
        assert!(told);
        set_x(1, 0);
        poll(&mut pads);
        set_x(1, 0x2000);
        poll(&mut pads);
        assert_eq!(names(&pads), [Some("XInput slot 0".to_string()), Some("XInput slot 1".to_string())]);
    }

    /// A HID pad is found again by its serial after coming back on another path; a seat
    /// whose controller is away goes to a new one only when no other seat is free.
    #[test]
    fn seats_follow_serials_and_are_reused_last() {
        let (_g, mut pads) = fake_session();
        pads.set_open(vec![Open::Yes, Open::Yes]);
        let (a, a_alive) = add_hid(&pads, "usb-path-a", 42);
        *a.lock().unwrap() = State { buttons: NORTH, ..Default::default() };
        poll(&mut pads);
        assert_eq!(pads.newly_seated, [(0, "HID 054c:09cc".to_string())]);
        // Unplugged, back over Bluetooth on another path.
        a_alive.store(false, Ordering::Relaxed);
        poll(&mut pads);
        assert_eq!(names(&pads), [None, None]);
        let (b, b_alive) = add_hid(&pads, "bt-path-b", 42);
        *b.lock().unwrap() = State { buttons: NORTH, ..Default::default() };
        poll(&mut pads);
        assert!(pads.newly_seated.is_empty(), "{:?}", pads.newly_seated);
        assert_eq!(names(&pads), [Some("HID 054c:09cc".to_string()), None], "its own seat again");
        // Player 1's pad away and player 2 seated: a third pad takes player 1's seat.
        set_x(0, 0x1000);
        poll(&mut pads);
        assert_eq!(pads.newly_seated, [(1, "XInput slot 0".to_string())]);
        b_alive.store(false, Ordering::Relaxed);
        poll(&mut pads);
        set_x(1, 0x2000);
        poll(&mut pads);
        assert_eq!(pads.newly_seated, [(0, "XInput slot 1".to_string())]);
        assert!(pads.notes.iter().any(|n| n.contains("is away, so XInput slot 1 takes it")), "{:?}", pads.notes);
    }

    /// The overlay swaps two players' controllers, or frees one's for the next pressed.
    #[test]
    fn seats_swap_and_free() {
        let (_g, mut pads) = fake_session();
        pads.set_open(vec![Open::Yes, Open::Yes]);
        set_x(0, 0x1000);
        poll(&mut pads);
        set_x(1, 0x2000);
        poll(&mut pads);
        pads.swap(0, 1);
        poll(&mut pads);
        assert_eq!(names(&pads), [Some("XInput slot 1".to_string()), Some("XInput slot 0".to_string())]);
        assert_eq!(pads.per, [EAST, SOUTH]);
        let info = pads.seat_info();
        assert_eq!(info[0].as_ref().map(|i| (i.name.as_str(), i.here)), Some(("XInput slot 1", true)));
        pads.free(1);
        assert_eq!(pads.seat_info()[1], None);
        set_x(0, 0);
        poll(&mut pads);
        set_x(0, 0x1000);
        poll(&mut pads);
        assert_eq!(pads.newly_seated, [(1, "XInput slot 0".to_string())], "the next one pressed takes it");
    }

    /// Generic pads' triggers: one axis each, or both on one; the Switch Pro's simple
    /// Bluetooth report has its sticks after the hat.
    #[test]
    fn generic_triggers_and_switch_sticks() {
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;
        let r = Some(Range::new(0, 1023, 10));
        assert!(near(pulled_by(0, r), 0.0) && near(pulled_by(1023, r), 1.0) && near(pulled_by(512, r), 0.5));
        assert!(near(pulled_by(255, None), 1.0) && near(pulled_by(65535, None), 1.0));
        assert_eq!(shared_triggers(0.8), (0.8, 0.0));
        assert_eq!(shared_triggers(-0.5), (0.0, 0.5));
        let mut r = vec![0x3f, 0, 0, 8];
        for v in [0xFFFFu16, 0x8000, 0x0000, 0x8000] {
            r.extend_from_slice(&v.to_le_bytes());
        }
        let s = parse_known(0x057e, 0x2009, &r).unwrap();
        assert!(near(s.lx, 1.0) && near(s.ly, 0.0) && near(s.rx, -1.0) && near(s.ry, 0.0), "{s:?}");
    }
}
