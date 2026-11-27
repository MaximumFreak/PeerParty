//! One copy of the game in a session: its place (`InstanceSpec`), the connection to its
//! hook, what its hook last said (its state, its frames' size and window, a shared
//! surface), the overlay's frame block and the virtual input block it reads, and the
//! process the session waits for.
//!
//! A copy alone (`host::run`) is a session of one of these with a solo spec: no pane, no
//! input block, nothing sent about its place.

use std::time::{Duration, Instant};

use peerparty_protocol::ipc::HostMsg;
use peerparty_protocol::session::InstanceSpec;
use peerparty_protocol::transport::IpcEndpoint;

use super::desktop::{self, Ended, GameWindow, Rect};
use super::input_hub::InputBlock;
use super::{frame, pipe, ui};

/// How long an open overlay may go undrawn inside the game before it closes itself. The
/// hook says it draws within a quarter of a second of the game's next frame; a game
/// that shows no frame for this long (a loading screen) gets the overlay closed too,
/// which is the better mistake.
const UNSEEN_FOR: Duration = Duration::from_millis(2500);

/// Since when the open overlay has been handed to the game's frames without the hook
/// saying it draws there. The clock starts at the first hand-over, not each frame's.
#[derive(Default)]
pub struct Unseen(Option<Instant>);

impl Unseen {
    /// The overlay was handed to the game's frames.
    pub fn handed(&mut self, ingame: bool, now: Instant) {
        if !ingame {
            self.0.get_or_insert(now);
        }
    }

    /// The hook draws it, or the overlay closed.
    pub fn reset(&mut self) {
        self.0 = None;
    }

    /// Open and undrawn for too long: to the player the game has frozen.
    pub fn too_long(&self, ingame: bool, now: Instant) -> bool {
        !ingame && self.0.is_some_and(|t| now.saturating_duration_since(t) >= UNSEEN_FOR)
    }
}

/// The game's frames, as its hook reports them (`HookMsg::Viewport`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    /// Its window (0: none known).
    pub hwnd: u64,
    /// The graphics API drawing them (`d3d11`, `vulkan`, ...).
    pub api: String,
}

/// The textures a copy's frames are copied into for the session's window, as its hook
/// last reported them (`HookMsg::Surface`): which set, their size and format, and the
/// graphics API they come from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Surface {
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    /// A `DXGI_FORMAT` (0: unknown).
    pub format: u32,
    pub api: String,
}

pub struct InstanceConn {
    pub spec: InstanceSpec,
    /// The connection to its hook; None once the copy has ended.
    pipe: Option<pipe::Pipe>,
    /// Signalled when the copy ends; None for one the session doesn't wait for.
    ended: Option<Ended>,
    /// The copy has ended.
    pub gone: bool,
    /// Its process: the one talking on the pipe now (a starter program hands over).
    pub pid: u32,
    /// What its hook last said for the overlay.
    pub state: ui::GameState,
    /// The overlay's frame block, for drawing inside its frames.
    pub frame: Option<frame::Shared>,
    /// Its virtual input block (a session's copies; none alone).
    pub input: Option<InputBlock>,
    /// Its frames, and a shared surface of them, as the hook reported them.
    pub viewport: Option<Viewport>,
    pub surface: Option<Surface>,
    /// The block saying which of its shared frames is newest (a session's copies).
    pub ring: Option<std::sync::Arc<super::ring::RingBlock>>,
    game_window: GameWindow,
    hwnd_checked: Option<Instant>,
    /// Whether it was told it plays as a later slot (`toast.later_slot`).
    pub slot_told: bool,
    /// Its game locked its cursor (`HookMsg::CursorLock`).
    pub cursor_locked: bool,
    /// Its game hid its cursor (`HookMsg::CursorHide`).
    pub cursor_hidden: bool,
    /// The cursor its game set (`HookMsg::CursorShape`): None before it set one, 0 none.
    pub cursor_shape: Option<u64>,
    /// Invites and players at its last state, so each new one toasts once.
    pub seen_pending: Vec<u64>,
    pub seen_peers: Vec<String>,
    pub first_state: bool,
    /// Copies of the game on another PeerParty at its last state (toasted when it starts).
    pub other_version: u64,
    /// Its hook would rather draw the overlay inside the frames (Gamescope, or
    /// `PEERPARTY_DRAW=ingame` in its environment).
    pub prefer_ingame: bool,
    /// Its hook draws the overlay inside its frames.
    pub ingame: bool,
    /// Whether the open overlay is drawn in the frames it was handed to (`UNSEEN_FOR`).
    pub unseen: Unseen,
    /// Who has had the front since it lost it: (since when, that process), for its log.
    /// None while it's in front, or before it ever was.
    pub focus_away: Option<(Instant, u32)>,
    /// It has been the window in front at least once.
    pub focus_seen: bool,
    /// How its window came to the front: as it opened, or since.
    pub front: Front,
    /// What closes it, every program of it (a session's copies).
    pub stop: Option<Box<dyn super::StopCopy>>,
}

/// How a copy's window has come to the front. Windows puts a window in front as it
/// opens: a session's copy doing so wasn't chosen by a player, and the keyboard and mouse
/// don't follow it (the last copy to start took them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Front {
    /// Never in front yet.
    #[default]
    Never,
    /// In front since it opened, without having lost the front.
    Opened,
    /// It lost the front after opening: coming back is a player's doing.
    Since,
}

impl Front {
    /// The next state, with its window in front now or not.
    pub fn next(self, in_front: bool) -> Front {
        match (self, in_front) {
            (Front::Never, true) => Front::Opened,
            (Front::Opened, false) => Front::Since,
            (s, _) => s,
        }
    }
}

impl InstanceConn {
    pub fn new(spec: InstanceSpec, ended: Ended, pid: u32, pipe: pipe::Pipe) -> InstanceConn {
        InstanceConn { pid, pipe: Some(pipe), ended: Some(ended), ..InstanceConn::detached(spec) }
    }

    /// A copy with no connection and no process (yet): tests, and a pane kept for one.
    pub fn detached(spec: InstanceSpec) -> InstanceConn {
        InstanceConn {
            spec,
            pipe: None,
            ended: None,
            gone: false,
            pid: 0,
            state: ui::GameState::default(),
            frame: None,
            input: None,
            viewport: None,
            surface: None,
            ring: None,
            game_window: GameWindow::none(),
            hwnd_checked: None,
            slot_told: false,
            cursor_locked: false,
            cursor_hidden: false,
            cursor_shape: None,
            seen_pending: Vec::new(),
            seen_peers: Vec::new(),
            first_state: true,
            other_version: 0,
            prefer_ingame: false,
            ingame: false,
            unseen: Unseen::default(),
            focus_away: None,
            focus_seen: false,
            front: Front::Never,
            stop: None,
        }
    }

    pub fn send(&self, msg: &HostMsg) {
        if let Some(p) = &self.pipe {
            p.send(msg);
        }
    }

    pub fn pipe(&self) -> Option<&pipe::Pipe> {
        self.pipe.as_ref()
    }

    /// Tells the hook its place changed (its pane, virtual screen, focus or input).
    pub fn send_instance(&self) {
        self.send(&HostMsg::Instance(self.spec.clone()));
    }

    /// Tells the hook where its virtual input block is, if it has one.
    pub fn send_input_shm(&self) {
        if let Some(b) = &self.input {
            self.send(&HostMsg::InputShm { name: b.name.clone() });
        }
    }

    /// Asks the hook to share its frames with the session's window (through its ring
    /// block, setting the window's frame event `event` after each, where there is one),
    /// when the copy's spec says so and the block is there: as pixels when the block is
    /// a file (`ring::pixel_frames`).
    pub fn send_capture(&self, event: Option<&str>) {
        let Some(ring) = self.ring.as_ref().filter(|_| self.spec.compositor_capture) else { return };
        let pixels = ring.is_file();
        if event.is_none() && !pixels {
            return;
        }
        let pace = ring.pace_event_name().map(str::to_string);
        self.send(&HostMsg::Capture { enabled: true, ring: Some(ring.name.clone()), event: event.map(str::to_string), pace, pixels });
    }

    /// Turns compositor-locked frame pacing on or off for this copy, waking it at once
    /// when turned off.
    pub fn set_sync(&self, on: bool) {
        if let Some(ring) = &self.ring {
            let was = ring.header().set_flag(peerparty_protocol::frame_ring::FLAG_SYNC_WANTED, on);
            if was && !on {
                ring.signal_pace();
            }
        }
    }

    /// Sets a split-screen copy's frame rate cap (`Display`, `Divisor(2|3)` via compositor
    /// sync interval, or `Fixed(15..=240)`), waking the copy immediately if the cap is
    /// relaxed or cleared.
    pub fn set_fps_cap(&self, cap: crate::launch::FpsCap, sync: bool) {
        let Some(ring) = &self.ring else { return };
        let h = ring.header();
        let has_pace = ring.pace_event_name().is_some();
        let (us, div) = match cap {
            crate::launch::FpsCap::Display => (0u32, 1u8),
            crate::launch::FpsCap::Divisor(d) => {
                let d = d.clamp(2, 3);
                if sync && has_pace {
                    (0, d)
                } else if !has_pace {
                    // Platforms without a cross-process VBlank pace event (Linux/Wine)
                    // fall back to a timer interval based on 60 Hz.
                    (1_000_000 * u32::from(d) / 60, 1)
                } else {
                    (0, 1)
                }
            }
            crate::launch::FpsCap::Fixed(fps) => (1_000_000 / u32::from(fps.clamp(15, 240)), 1),
        };
        let prev_us = h.pace_us();
        let prev_div = h.sync_divisor();
        h.set_sync_divisor(div);
        h.set_pace_us(us);
        if (prev_us > 0 && (us == 0 || us < prev_us)) || div < prev_div {
            ring.signal_pace();
        }
    }

    /// A block for its frames' ring, if its frames are the session's to draw: a file in
    /// the frames' directory when they come as `pixels` (`ring::pixel_frames`).
    pub fn make_ring(&mut self, pixels: bool) {
        if !self.spec.compositor_capture || self.ring.is_some() {
            return;
        }
        if pixels {
            self.ring = super::ring::RingBlock::create_file(&desktop::frame_dir(), self.pid);
            return;
        }
        #[cfg(windows)]
        {
            self.ring = super::ring::RingBlock::create(self.pid);
        }
    }

    /// Whether it has ended now (once it has, its connection and blocks are let go).
    pub fn check_ended(&mut self) -> bool {
        if !self.gone && self.ended.as_ref().is_some_and(desktop::has_ended) {
            self.end();
        }
        self.gone
    }

    /// It ended: its connection and blocks go.
    pub fn end(&mut self) {
        self.gone = true;
        self.pipe = None;
        self.frame = None;
        self.input = None;
        self.surface = None;
        self.ring = None;
    }

    /// What signals its end.
    pub fn ended(&self) -> Option<&Ended> {
        self.ended.as_ref()
    }

    /// Its window is the one in front.
    pub fn in_front(&self) -> bool {
        !self.gone && desktop::in_front(self.pid)
    }

    /// Its process is another now (a starter program handed over to the game): its
    /// window and frame block are found again. Whether it changed.
    pub fn follow_pid(&mut self, pid: u32) -> bool {
        if pid == 0 || pid == self.pid {
            return false;
        }
        self.pid = pid;
        self.front = Front::Never;
        self.viewport = None;
        self.game_window = GameWindow::none();
        self.hwnd_checked = None;
        self.frame = None;
        true
    }

    /// The hook reported its frames.
    pub fn set_viewport(&mut self, v: Viewport) {
        self.viewport = Some(v);
    }

    /// The copy's window handle: the one its hook reported with its frames, else the
    /// process's main window found on Windows (0 if unknown).
    pub fn best_hwnd(&self) -> u64 {
        self.viewport.as_ref().map(|v| v.hwnd).filter(|&h| h != 0).unwrap_or_else(|| self.game_window.hwnd())
    }

    /// Finds the copy's window handle on Windows when its hook hasn't reported one yet
    /// (every 100 ms while waiting for the window to appear, once a second once alive).
    /// True if `best_hwnd()` changed.
    pub fn refresh_window(&mut self) -> bool {
        // The window its frames came from is gone (the game made a new one, as for a
        // display mode change): looked for again.
        if let Some(v) = self.viewport.as_mut().filter(|v| v.hwnd != 0 && !super::desktop::window_alive(v.hwnd)) {
            v.hwnd = 0;
        }
        if self.gone || self.pid == 0 || self.viewport.as_ref().is_some_and(|v| v.hwnd != 0) {
            return false;
        }
        let interval = if self.game_window.alive() { Duration::from_secs(1) } else { Duration::from_millis(100) };
        let stale = self.hwnd_checked.is_none_or(|t| t.elapsed() > interval);
        if !stale && self.game_window.alive() {
            return false;
        }
        let before = self.best_hwnd();
        self.game_window = GameWindow::find(self.pid);
        self.hwnd_checked = Some(Instant::now());
        self.best_hwnd() != before
    }

    /// The size of the screen the copy draws for, when it's known: its virtual screen,
    /// else its frames'.
    pub fn screen_size(&self) -> Option<(u32, u32)> {
        match (self.spec.virtual_width, self.spec.virtual_height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
            _ => self.viewport.as_ref().filter(|v| v.width > 0 && v.height > 0).map(|v| (v.width, v.height)),
        }
    }

    /// Its window's client area on screen, looked up again at most once a second. Off
    /// Windows the game has the screen: its frames' size, once its hook has said it, is
    /// better than guessing the display's.
    pub fn game_rect(&mut self) -> Option<Rect> {
        #[cfg(unix)]
        if let Some(v) = self.viewport.as_ref().filter(|v| v.width > 100 && v.height > 100) {
            let (w, h) = (i32::try_from(v.width).unwrap_or(i32::MAX), i32::try_from(v.height).unwrap_or(i32::MAX));
            return Some(Rect { left: 0, top: 0, right: w, bottom: h });
        }
        let stale = self.hwnd_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(1));
        if stale || !self.game_window.alive() {
            self.game_window = GameWindow::find(self.pid);
            self.hwnd_checked = Some(Instant::now());
        }
        self.game_window.rect()
    }

    /// The frame block for drawing inside the game, made (or made again, bigger) for a
    /// frame of `pixels`; the hook is told its name each time.
    pub fn frame_for(&mut self, pixels: usize) {
        if self.frame.as_ref().is_some_and(|f| f.fits(pixels)) {
            return;
        }
        let Some(endpoint) = self.pipe.as_ref().map(|p| &p.endpoint) else { return };
        let made = match endpoint {
            // A game reached over TCP may be one a named mapping can't reach either (under
            // Wine, from a Linux launcher): a file both sides map.
            IpcEndpoint::TcpToken { .. } => frame::Shared::create_file(&desktop::frame_dir(), self.pid, pixels),
            #[cfg(windows)]
            IpcEndpoint::WinPipe { .. } => frame::Shared::create(self.pid, pixels),
            // Named pipes are Windows' alone: a game is never reached through one here.
            #[cfg(not(windows))]
            IpcEndpoint::WinPipe { .. } => None,
        };
        if let Some(f) = made {
            self.send(&HostMsg::Frame { name: f.name.clone() });
            self.frame = Some(f);
        }
    }

    /// Its virtual input block, made the way its frame block would be (a file for a
    /// game reached over TCP), and the hook told where it is. Whether it has one.
    pub fn make_input_block(&mut self) -> bool {
        if self.input.is_some() {
            return true;
        }
        let Some(endpoint) = self.pipe.as_ref().map(|p| &p.endpoint) else { return false };
        self.input = match endpoint {
            IpcEndpoint::TcpToken { .. } => InputBlock::create_file(&desktop::frame_dir(), self.pid),
            #[cfg(windows)]
            IpcEndpoint::WinPipe { .. } => InputBlock::create(self.pid),
            #[cfg(not(windows))]
            IpcEndpoint::WinPipe { .. } => None,
        };
        self.send_input_shm();
        self.input.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::session::{InputAssignment, InstanceId, PaneRect};

    /// A window in front as it opens isn't a player's choice until it has lost the front
    /// once; every return after that is.
    #[test]
    fn a_window_in_front_as_it_opens_is_told_apart() {
        let f = Front::Never.next(false);
        assert_eq!(f, Front::Never, "not opened yet");
        let f = f.next(true);
        assert_eq!(f, Front::Opened);
        assert_eq!(f.next(true), Front::Opened, "still in front from opening");
        let f = f.next(false);
        assert_eq!(f, Front::Since);
        assert_eq!(f.next(true), Front::Since, "brought back: a choice");
    }

    /// The overlay handed to a game that never draws it closes 2.5 s after the first
    /// hand-over (frames handed over meanwhile don't restart the clock); one the hook
    /// draws, or that closed, doesn't.
    #[test]
    fn an_overlay_the_game_never_draws_closes_itself() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut u = Unseen::default();
        assert!(!u.too_long(false, at(10_000)), "never handed over");
        u.handed(false, at(0));
        for ms in (0..2500).step_by(16) {
            u.handed(false, at(ms));
            assert!(!u.too_long(false, at(ms)), "{ms} ms");
        }
        assert!(u.too_long(false, at(2500)));
        // The hook says it draws: never too long, and the clock is gone.
        assert!(!u.too_long(true, at(5000)));
        u.reset();
        assert!(!u.too_long(false, at(5000)));
        // Handed over while the hook draws: no clock at all.
        u.handed(true, at(5000));
        assert!(!u.too_long(false, at(9000)));
        // Opened again later: a new clock.
        u.handed(false, at(9000));
        assert!(!u.too_long(false, at(11_000)));
        assert!(u.too_long(false, at(11_500)));
    }

    /// A copy's screen is its virtual one when it has one, else its frames'; a starter
    /// handing over moves it to the new process; ending lets everything go.
    #[test]
    fn a_copys_screen_process_and_end() {
        let mut spec = InstanceSpec::solo(InstanceId(1), 1);
        spec.input_assignment = InputAssignment::Controller { index: 1 };
        let mut i = InstanceConn::detached(spec);
        assert_eq!(i.screen_size(), None);
        i.set_viewport(Viewport { width: 1280, height: 800, hwnd: 0x10a2e, api: "vulkan".into() });
        assert_eq!(i.screen_size(), Some((1280, 800)));
        i.spec.pane = Some(PaneRect { x: 0, y: 0, width: 960, height: 1080 });
        i.spec.virtual_width = Some(960);
        i.spec.virtual_height = Some(1080);
        assert_eq!(i.screen_size(), Some((960, 1080)), "the virtual screen wins");
        assert_eq!(i.best_hwnd(), 0x10a2e);
        assert!(i.follow_pid(42));
        assert_eq!(i.best_hwnd(), 0, "pid handoff clears stale viewport hwnd");
        assert!(!i.follow_pid(42) && !i.follow_pid(0));
        assert_eq!(i.pid, 42);
        assert!(!i.make_input_block(), "no connection, no block");
        assert!(!i.check_ended(), "nothing to wait for");
        i.surface = Some(Surface { generation: 5, width: 2, height: 3, format: 87, api: "d3d11".into() });
        i.end();
        assert!(i.gone && i.surface.is_none() && i.pipe().is_none());
        assert!(!i.in_front());
    }
}
