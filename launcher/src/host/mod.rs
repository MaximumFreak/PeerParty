//! What peerparty.exe does while the game it started runs: it already waits for the
//! game (to hand its exit code to a .bat or Steam), so it also hosts the overlay.
//!
//! - The pipe to the hook (pipe.rs): the game's state comes in, commands go out.
//! - Opening: the stores' hotkeys (Shift+Tab, Shift+F3, ...) and the controller's
//!   Home / Xbox Guide button (any controller's) or View + Menu held, while the game is
//!   in front; or the game
//!   itself (its invite dialog, its friends button).
//! - While open: keyboard and mouse come from low-level hooks (input.rs), controllers are
//!   read here (pad.rs) and held back from the game by the hook; the pages (ui.rs) are
//!   drawn by egui into a software canvas (raster.rs), shown in a layered window over the
//!   game (window.rs) or, for exclusive fullscreen, inside the game's frames (frame.rs).
//! - Closed: nothing is drawn and nothing is hooked. Corner notifications (an invite, a
//!   player arriving) are drawn alone in a small window.
//!
//! The game's own frame rate is never touched: everything here runs in this process,
//! and while the overlay is closed this process only checks a few keys and controllers
//! 30 times a second while the game is in front.
//!
//! One loop hosts every copy of the game a run started (`SessionHost`): one alone
//! (`run`), or several sharing the screen (`run_session`). Its parts:
//!
//! - instance.rs (`InstanceConn`): each copy's connection, state, frame and input blocks.
//! - input_hub.rs (`InputHub`): the controllers, keyboards and mice, read once for all
//!   of them, and what each copy's virtual input block says.
//! - overlay_render.rs (`OverlayRenderer`): drawing the overlay and its notifications.
//! - firewall_watch.rs (`FirewallWatcher`) and prefs_sync.rs (`PrefsSync`): the firewall
//!   checks and fixes, and saving the overlay's settings changes.
//! - compositor.rs (`CompositorWindow`): split screen as panes of one window.
//!
//! Each copy has an overlay of its own (`PaneOverlay`): its own state, pages and
//! notifications, drawn over its own pane, and commands go to its own hook. In a
//! split-screen session each player opens and works theirs with the controller their
//! copy was given, while the others play on; the PC's keyboard and mouse work the
//! overlay of the copy in front (`focus`).
//!
//! The desktop around the game is desktop.rs's (the game's window, the foreground, the
//! keyboard). On Linux the game runs under Wine, usually in Gamescope: the overlay is
//! drawn inside its frames only, worked with controllers (evdev), and the keyboard
//! hooks, the window over the game and Game Bar are stand-ins (unix.rs).

mod audio;
mod compositor;
mod desktop;
mod firewall_watch;
mod frame;
#[cfg(windows)]
mod gamebar;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod input;
mod input_hub;
mod instance;
mod layout_studio;
mod overlay_render;
pub(crate) mod pad;
pub mod pipe;
mod prefs_sync;
pub(crate) mod raster;
mod render;
mod ring;
// The Linux window's drawing (tested on Windows too, where Vulkan is).
#[cfg(any(unix, test))]
mod tonemap;
#[cfg(any(unix, test))]
mod vkblit;
#[cfg(unix)]
mod x11;
mod ui;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod window;
#[cfg(unix)]
use unix::{gamebar, input, window};

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use eframe::egui;
use peerparty_protocol::ipc::{HookMsg, HostMsg};
use peerparty_protocol::session::{InputAssignment, InstanceId, InstanceSpec, PaneRect, PaneSlot};

pub use compositor::{CompositorWindow, MonitorBounds};
pub use ring::pixel_frames;
pub use desktop::Ended;
use desktop::Rect;
pub use peerparty_protocol::session::LayoutPreset;

use crate::firewall;
use crate::launch::{SlotInput, StaggerMode};
use crate::ui::store::Prefs;
use audio::AudioMixer;
use firewall_watch::{FirewallEvent, FirewallWatcher};
use input_hub::InputHub;
use instance::{InstanceConn, Surface, Viewport};
use layout_studio::{LayoutStudio, PlayerInfo, StudioAction, StudioInfo, StudioPad, StudioSync};
use overlay_render::{FRAME_GAP, OverlayRenderer, scale_for};
use prefs_sync::PrefsSync;
use ui::{Cmd, Nav, Overlay, Page};

const VK_SHIFT: i32 = 0x10;
const VK_MENU: i32 = 0x12;
const VK_ESCAPE: i32 = 0x1B;
const VK_TAB: u32 = 0x09;
const VK_F1: u32 = 0x70;
const VK_F2: u32 = 0x71;
const VK_F3: u32 = 0x72;
pub(crate) const CHORD_HOLD: Duration = Duration::from_secs(1);
/// Controller buttons (not the d-pad or the stick folded into it) still down when the
/// overlay closes, the Start or B that closed it most often: the game's controllers stay
/// held back until they're up, or this long (the hook gives up after 2 s by itself).
const PAD_BUTTONS: u32 = !(pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT);
const PADS_HELD_MAX: Duration = Duration::from_secs(1);
/// `SessionHost::hotkeys`, by name.
const HOTKEY_NAMES: [&str; 4] = ["Shift+Tab", "Shift+F3", "Shift+F1", "Shift+F2"];
/// The store whose own in-game overlay opens on each of `HOTKEY_NAMES` (the hook's
/// `foreign` names), and its name for a player.
const HOTKEY_OWNERS: [(&str, &str); 4] = [("steam", "Steam"), ("epic", "Epic"), ("ea", "EA"), ("ubisoft", "Ubisoft")];
/// A split-screen session's keys, with Ctrl+Alt held while its window or a copy is in
/// front: L the next layout, Z the focused copy on the whole window, 1 to 4 (the number
/// row's, then the number pad's) the focus to that copy, E the live Layout Studio.
const SESSION_KEYS: [i32; 11] = [0x4C, 0x5A, 0x31, 0x32, 0x33, 0x34, 0x61, 0x62, 0x63, 0x64, 0x45];

/// Smart ready-gate launch staggering timings: wait for the previous copy to create its
/// window (plus a brief settle) or render its first frame, with a safety timeout.
const SMART_STAGGER_WINDOW_SETTLE: Duration = Duration::from_millis(500);
const SMART_STAGGER_MAX: Duration = Duration::from_secs(8);
const SMART_STAGGER_ABORT_WINDOW: Duration = Duration::from_secs(5);

/// Spawns copy `slot` (1..N) when the session host's launch stagger gate fires.
pub type PendingSpawner = Box<dyn FnMut(u32, Option<PaneRect>, Option<(u32, u32)>) -> Result<SessionLaunch, String>>;

/// Which of the hotkeys that are on (`on`) open the overlay in a game with `foreign`
/// store overlays loaded. A key a loaded store overlay opens on is left to it (a game
/// started from Steam with peerparty in its launch options has Steam's overlay, and
/// one press opened both), unless nothing else would open PeerParty's: no other key,
/// and no controller button or chord (`other`).
fn usable_hotkeys(on: [bool; 4], foreign: &[String], other: bool) -> [bool; 4] {
    let free: [bool; 4] = std::array::from_fn(|i| on[i] && !foreign.iter().any(|f| f == HOTKEY_OWNERS[i].0));
    if free.contains(&true) || other { free } else { on }
}
/// Recording a chord gives up after this long with no chord held.
pub(crate) const RECORD_FOR: Duration = Duration::from_secs(10);

/// One copy of the game for `run_session`: what signals its end, its process, the
/// connection its hook makes, and its place in the session (its slot, identity and
/// input; its pane is the layout's).
pub struct SessionLaunch {
    pub process: Ended,
    pub pid: u32,
    pub pipe: pipe::Pipe,
    pub spec: InstanceSpec,
    /// What the player plays with (their spec's `input_assignment` says it to the hook).
    pub input: SlotInput,
    /// What closes this copy, every program of it (a starter's game too).
    pub stop: Option<Box<dyn StopCopy>>,
}

/// Closes one copy of a session: the backend's, which knows all of its programs (on
/// Windows its job, tree.rs).
pub trait StopCopy: Send {
    /// Asks its windows to close as their close buttons would, `known` among them (its
    /// window as the session knows it, 0: none) even while hidden. How many were asked.
    fn ask(&self, known: u64) -> usize;
    /// Ends every program of it now.
    fn force(&self);
}

/// How long the copies have to close (and save) after being asked before they're ended.
const CLOSE_WAIT: Duration = Duration::from_secs(6);

/// Close all in the launcher's window, for a session running in this process: taken by
/// its next tick, as its own window's Close is.
static CLOSE_ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Asks the split-screen session in this process to close every copy (they're ended if
/// they haven't after `CLOSE_WAIT`).
pub fn close_session() {
    CLOSE_ASKED.store(true, std::sync::atomic::Ordering::Release);
}

/// Keys and mouse buttons pressed in a player's pane, with no controller of theirs, that
/// make them a keyboard player (`SessionHost::watch_keyboard`): more than a click that
/// focuses the pane or a hotkey.
const KBM_PLAYER_PRESSES: u64 = 4;

/// Where the keyboard's presses were counted (`SessionHost::watch_keyboard`).
#[derive(Default)]
struct KbmSeen {
    /// `InputHub::kbm_presses` at the last tick.
    last: u64,
    /// The pane they were counted for, and how many.
    pane: usize,
    count: u64,
}

/// One copy's overlay: its pages and notifications, whether it's open, and how it's
/// worked and drawn. Each player in a split-screen session has their own: opened by
/// their own controller (or the PC's keyboard while their pane has the focus), drawn
/// over their own pane, and holding back their own copy's input alone while the others
/// play on.
struct PaneOverlay {
    overlay: Overlay,
    render: OverlayRenderer,
    visible: bool,
    nav: PaneNav,
    /// While its toasts wait for the game to come back to the front: when they last did.
    toasts_held: Option<Instant>,
}

impl PaneOverlay {
    fn new(game: String) -> PaneOverlay {
        PaneOverlay { overlay: Overlay::new(game), render: OverlayRenderer::new(), visible: false, nav: PaneNav::new(), toasts_held: None }
    }
}

/// A pane's overlay's own navigation while it's open, and the edges of what its
/// controller opens it with while it's closed: the pointer, a chord held or recorded.
struct PaneNav {
    cursor: egui::Pos2,
    /// Whether the overlay's pointer shows: only once the mouse moves or clicks after
    /// opening, and hidden again when the controller or keyboard navigates.
    pointer: bool,
    /// A click that only showed the pointer: its release is swallowed too (left or not).
    click_shown: Option<bool>,
    /// Its controller's buttons at the last tick (`SessionHost::pane_pad`).
    pad_prev: u32,
    /// The overlay closed with controller buttons down (`PAD_BUTTONS`), at this time; or,
    /// while it's open, it opened with them down (the game reads its HID controllers as
    /// they are until they're up, hook overlay.rs).
    pads_held: Option<Instant>,
    /// The page the game asked for since the overlay opened (`HookMsg::Open`): asked
    /// again, the overlay stays where its player has gone on it.
    game_asked: Option<String>,
    chord_since: Option<Instant>,
    chord_fired: bool,
    repeat: crate::ui::shared::RepeatTimer,
}

impl PaneNav {
    fn new() -> PaneNav {
        PaneNav {
            cursor: egui::Pos2::new(400.0, 300.0),
            pointer: false,
            click_shown: None,
            pad_prev: 0,
            pads_held: None,
            game_asked: None,
            chord_since: None,
            chord_fired: false,
            repeat: crate::ui::shared::RepeatTimer::default(),
        }
    }
}

/// The PC's keyboard's edges, the same for every pane (it works the focused one's
/// overlay): the opening hotkeys, Shift, and a split-screen session's keys.
struct NavState {
    keys_down: [bool; 4],
    /// Closing hotkeys (`HOTKEY_NAMES`) held at the last tick while the focused overlay
    /// is open (rising-edge fallback alongside the low-level keyboard hook).
    close_keys_down: [bool; 4],
    /// Esc held at the last tick while the focused overlay is open (same fallback).
    esc_down: bool,
    shift: bool,
    /// Opening hotkeys held at the last tick, seen whether or not they could open the
    /// overlay, so a press that couldn't says why in the game's log.
    diag_keys: [bool; 4],
    /// Some controller's Home / Guide was down at the last tick, the same for Home.
    diag_home: bool,
    /// A split-screen session's keys held at the last tick (`SESSION_KEYS`).
    session_keys: [bool; SESSION_KEYS.len()],
    /// When Home last went down on a controller a copy was given (`real_homes`).
    given_home_at: Option<Instant>,
}

impl NavState {
    fn new() -> NavState {
        NavState {
            keys_down: [false; 4],
            close_keys_down: [true; 4],
            esc_down: true,
            shift: false,
            diag_keys: [false; 4],
            diag_home: true,
            session_keys: [true; SESSION_KEYS.len()],
            given_home_at: None,
        }
    }
}

/// Rising-edge poll for `Esc` and the active overlay hotkeys (e.g. Shift+Tab) while the
/// overlay is open, returning `(back, close)` and updating `esc_down` / `close_keys_down`.
/// Mirrors Layout Studio's Esc poll so dismissing works even when another low-level
/// keyboard hook ahead of ours handles or remaps keys, or when Shift stayed held from
/// opening the overlay.
fn poll_dismiss_keys(
    hotkeys: [(bool, u32); 4],
    shift: bool,
    alt: bool,
    esc_now: bool,
    keys_now: [bool; 4],
    esc_prev: &mut bool,
    keys_prev: &mut [bool; 4],
) -> (bool, bool) {
    let back = esc_now && !*esc_prev;
    *esc_prev = esc_now;
    let mut close = false;
    for (k, (on, _)) in hotkeys.iter().enumerate() {
        let d = keys_now[k];
        if *on && shift && !alt && d && !keys_prev[k] {
            close = true;
        }
        keys_prev[k] = d;
    }
    (back, close)
}

/// How long after Home on a controller a copy was given a Home on one no copy was is
/// the same press seen twice (`real_homes`).
const TWIN_HOME: Duration = Duration::from_millis(150);

/// This tick's Home presses, `(seat, name, given to a copy)`: the ones that open an
/// overlay, and the names of the ones that don't. One player's controller can be there
/// twice (a PlayStation pad, and the Xbox pad a remapper makes of it), one press showing
/// on both: a press on a controller no copy was given, with or just after one on a
/// controller a copy was, is that one's (opened on the focused copy, it would open a
/// second overlay on another player's pane).
fn real_homes(homes: Vec<(Option<usize>, String, bool)>, given_at: &mut Option<Instant>, now: Instant) -> (Vec<(Option<usize>, String)>, Vec<String>) {
    if homes.iter().any(|(_, _, given)| *given) {
        *given_at = Some(now);
    }
    let twin = given_at.is_some_and(|t| now.duration_since(t) < TWIN_HOME);
    let (mut keep, mut twins) = (Vec::new(), Vec::new());
    for (k, name, given) in homes {
        if !given && twin {
            twins.push(name);
        } else {
            keep.push((k, name));
        }
    }
    (keep, twins)
}

/// A controller's kind as the overlay names it.
fn style_word(s: pad::Style) -> &'static str {
    match s {
        pad::Style::Xbox => "Xbox",
        pad::Style::PlayStation => "PlayStation",
        pad::Style::Nintendo => "Nintendo",
    }
}

/// The controllers as this tick's one poll read them (a second poll would lose a Home
/// press): every controller's buttons together, each one's apart (`Pads::readings`
/// order), and the Home presses.
#[derive(Default)]
struct PadFrame {
    all: u32,
    per: Vec<u32>,
    /// The controllers in use that hold no seat (`Pads::spare`).
    spare: u32,
    homes: Vec<(Option<usize>, String)>,
}

struct SessionHost {
    /// Every copy, in pane order; a copy that ended stays, `gone`.
    instances: Vec<InstanceConn>,
    /// Each copy's overlay, by the same index.
    panes: Vec<PaneOverlay>,
    /// The copy in front, or the last one that was: the PC's keyboard and mouse work its
    /// overlay, and the log lines about the session as a whole go to it.
    focus: usize,
    input: InputHub,
    /// This tick's controllers (`poll_pads`).
    pads: PadFrame,
    firewall: FirewallWatcher,
    saver: PrefsSync,
    /// A split-screen session's window (none for a copy alone).
    compositor: Option<CompositorWindow>,
    /// The panes need telling to the copies again (a layout change, a copy's window
    /// reported).
    panes_stale: bool,
    /// When the user asked to close the session (its window's Close, or Close all): the
    /// copies still running after `CLOSE_WAIT` are ended, once (`ended_all`).
    closing_since: Option<Instant>,
    ended_all: bool,
    nav: NavState,
    key: String,
    prefs: Prefs,
    /// ui.json's settings for this game as last read, when it was last changed, and when
    /// that was last looked at (`reload_prefs`).
    file_prefs: Prefs,
    file_seen: Option<std::time::SystemTime>,
    file_checked: Instant,
    started: Instant,
    /// Game Bar's overlay over the game (it never takes the foreground), and whether it
    /// was open at the last tick: the overlays leave the input to it meanwhile.
    gamebar: gamebar::GameBar,
    under_gamebar: bool,
    /// A hotkey left to a store's overlay has been mentioned (once a run).
    key_left_told: bool,
    /// The controllers last named in the game's log (none named yet: `None`).
    pads_logged: Option<Vec<String>>,
    /// Ticks that panicked (each is caught; the first few go to the game's log).
    panics: u32,
    /// What each player plays with (`SessionLaunch::input`).
    inputs: Vec<SlotInput>,
    /// Keyboard and mouse are kept from reaching the games (controllers only); overlay
    /// and session hotkeys still work.
    kbm_off: bool,
    /// The mouse went to another player (`set_focus`): the pointer goes into its pane
    /// once no overlay is open (`update_pointer`).
    pointer_to_focus: bool,
    /// Each player seen playing with the keyboard and mouse before a controller of
    /// theirs was used (`watch_keyboard`): a controller goes to another player first.
    kbm_player: Vec<bool>,
    kbm_seen: KbmSeen,
    /// Interactive Layout Studio canvas drawn live over the compositor panes.
    layout_studio: LayoutStudio,
    /// Per-copy WASAPI audio mute controller.
    audio: AudioMixer,
    audio_refresh: Instant,
    /// Copies waiting to be started by the smart ready-gate stagger queue.
    pending_spawns: VecDeque<u32>,
    spawner: Option<PendingSpawner>,
    stagger: StaggerMode,
    last_spawned_at: Instant,
    stagger_ready_at: Option<Instant>,
    pixels: bool,
    /// Compositor-locked frame pacing ("Sync games to screen", ON by default).
    sync_frames: bool,
    /// Per-copy frame rate cap (`Display`, `Divisor(2|3)`, or `Fixed(15..=240)`).
    fps_cap: crate::launch::FpsCap,
    game_title: String,
}

/// Runs until the game has ended: `process` says so then (on Windows the game's process,
/// or tree.rs's event for the game's programs; on Linux the flag the thread waiting for
/// it sets). Returns when it has.
pub fn run(process: Ended, pid: u32, pipe: pipe::Pipe, exe: &Path, prefs: Prefs, packaged: bool) {
    prepare(&prefs);
    let copy = InstanceConn::new(InstanceSpec::solo(InstanceId(0), 0), process, pid, pipe);
    let host = SessionHost::new(vec![copy], None, exe, prefs, packaged);
    host.run();
}

/// Enables DPI awareness and measures the monitor a split-screen session opens on
/// (`--monitor`'s number, else the main one) before the copies start, so each is told
/// its pane in real pixels.
pub fn initial_session_bounds(monitor: Option<u32>) -> MonitorBounds {
    desktop::dpi_aware();
    let (b, why) = MonitorBounds::numbered(monitor);
    let n = if why.is_some() { 1 } else { monitor.unwrap_or(1) };
    if let Some(why) = why {
        eprintln!("peerparty: {why}");
    }
    eprintln!(
        "peerparty: split screen on monitor {n} of {}: {}x{} at ({}, {}), {}% scale",
        MonitorBounds::all().len(),
        b.width,
        b.height,
        b.x,
        b.y,
        b.dpi * 100 / 96
    );
    b
}

/// How a split-screen session's window looks: its layout, where it opens (the bounds
/// `initial_session_bounds` gave the copies' panes), whether the copies' frames come
/// as pixels (`ring::pixel_frames`), KBM lockout, launch staggering, and total copy count.
pub struct SessionLook {
    pub layout: LayoutPreset,
    pub bounds: MonitorBounds,
    pub pixels: bool,
    pub no_kbm: bool,
    pub stagger: StaggerMode,
    pub sync: bool,
    pub render_scale: u8,
    pub upscale: bool,
    pub fps: crate::launch::FpsCap,
    pub total_count: usize,
}

/// Runs a split-screen session until every copy in it has ended: one window shows them
/// in panes cut by `layout` (each copy told its pane and given a virtual input block),
/// and each copy has an overlay of its own, as `run` gives one. Copies beyond the initial
/// batch are started lazily via `spawner` according to `look.stagger`.
pub fn run_session(instances: Vec<SessionLaunch>, look: SessionLook, spawner: Option<PendingSpawner>, exe: &Path, prefs: Prefs, packaged: bool) {
    if instances.is_empty() {
        return;
    }
    prepare(&prefs);
    let inputs: Vec<SlotInput> = instances.iter().map(|l| l.input).collect();
    let copies: Vec<InstanceConn> = instances
        .into_iter()
        .map(|l| {
            let mut c = InstanceConn::new(l.spec, l.process, l.pid, l.pipe);
            c.stop = l.stop;
            c
        })
        .collect();
    let total = look.total_count.max(copies.len());
    CLOSE_ASKED.store(false, std::sync::atomic::Ordering::Release);
    let name = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let title = game_title(exe, &name);
    // The window is made on this thread, which hands it its messages (desktop::wait).
    let mut compositor = CompositorWindow::open(look.layout, look.bounds, total, &title, exe);
    compositor.set_quality(look.render_scale, look.upscale);
    compositor.set_sync_interval(if look.sync { look.fps.sync_divisor() } else { 1 });
    let mut host = SessionHost::new(copies, Some(compositor), exe, prefs, packaged);
    host.pixels = look.pixels;
    host.sync_frames = look.sync;
    host.fps_cap = look.fps;
    host.stagger = look.stagger;
    if let Some(sp) = spawner {
        let start = host.instances.len() as u32;
        for slot in start..total as u32 {
            host.pending_spawns.push_back(slot);
        }
        host.spawner = Some(sp);
        host.last_spawned_at = Instant::now();
    }
    host.set_inputs(inputs);
    host.set_kbm_off(look.no_kbm);
    let event = host.compositor.as_ref().and_then(|c| c.frame_event());
    let draws = host.compositor.as_ref().is_some_and(|c| c.draws_frames());
    for inst in &mut host.instances {
        inst.make_input_block();
        // A copy whose frames the session's window draws itself (not through its window's
        // thumbnail) copies them into shared textures, or pixels in a file, said in its
        // ring block.
        if draws {
            inst.make_ring(look.pixels);
            inst.set_sync(host.sync_frames);
            inst.set_fps_cap(host.fps_cap, host.sync_frames);
            inst.send_capture(event.as_deref());
        }
    }
    host.run();
}

/// This process's part before a session starts: real pixels, the keyboard and mouse
/// hooks, the player's language and effects.
fn prepare(prefs: &Prefs) {
    desktop::dpi_aware();
    input::start();
    crate::i18n::set(&prefs.language);
    crate::ui::theme::set_effects(prefs.effects);
}

impl SessionHost {
    fn new(instances: Vec<InstanceConn>, compositor: Option<CompositorWindow>, exe: &Path, prefs: Prefs, packaged: bool) -> SessionHost {
        let name = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let pid = instances.first().map_or(0, |i| i.pid);
        let title = game_title(exe, &name);
        let panes = (0..instances.len()).map(|_| PaneOverlay::new(title.clone())).collect();
        let file = crate::ui::store::Settings::load();
        let mut host = SessionHost {
            instances,
            panes,
            focus: 0,
            input: InputHub::new(),
            pads: PadFrame::default(),
            firewall: FirewallWatcher::new(crate::platform::process_exe(pid).unwrap_or_else(|| exe.to_path_buf()), packaged),
            saver: PrefsSync::start(&prefs),
            panes_stale: compositor.is_some(),
            closing_since: None,
            ended_all: false,
            compositor,
            nav: NavState::new(),
            key: crate::ui::store::key_of(exe),
            file_prefs: file.prefs.effective_for(&file.options(&crate::ui::store::key_of(exe))),
            file_seen: crate::ui::store::Settings::modified(),
            file_checked: Instant::now(),
            prefs,
            started: Instant::now(),
            gamebar: gamebar::GameBar::new(),
            under_gamebar: false,
            key_left_told: false,
            pads_logged: None,
            panics: 0,
            inputs: Vec::new(),
            kbm_off: false,
            pointer_to_focus: false,
            kbm_player: Vec::new(),
            kbm_seen: KbmSeen::default(),
            layout_studio: LayoutStudio::new(),
            audio: AudioMixer::new(),
            audio_refresh: Instant::now(),
            pending_spawns: VecDeque::new(),
            spawner: None,
            stagger: StaggerMode::Smart,
            last_spawned_at: Instant::now(),
            stagger_ready_at: None,
            pixels: false,
            sync_frames: true,
            fps_cap: crate::launch::FpsCap::Display,
            game_title: title,
        };
        host.inputs = vec![SlotInput::Auto; host.instances.len()];
        host.kbm_player = vec![false; host.instances.len()];
        host.refresh_hints();
        host
    }

    fn run(mut self) {
        // When the next whole tick is due. The raw input of a moving mouse wakes the wait
        // up to a thousand times a second: before then, with no overlay open and no key
        // or button changed, only the keyboards and mice are published (`kbm_tick`).
        let mut due = Instant::now();
        let mut others = self.input.kbm_others();
        loop {
            let open = self.any_open() || self.layout_studio.active;
            let now = Instant::now();
            if open || now >= due {
                due = now + Duration::from_millis(self.tick_wait(open).into());
            }
            if self.wait(due.saturating_duration_since(now).as_millis() as u32) {
                break;
            }
            let o = self.input.kbm_others();
            if !open && o == others && Instant::now() < due {
                if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.kbm_tick())) {
                    self.tick_failed(e);
                }
                continue;
            }
            others = o;
            // The next wait starts over from its full length, as after any tick.
            due = Instant::now();
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.tick())) {
                self.tick_failed(e);
            }
        }
        input::capture(false);
        input::watch_hotkeys(&[]);
        self.saver.finish();
    }

    /// A tick panicked: said (the first few times) in the focused copy's log.
    fn tick_failed(&mut self, e: Box<dyn std::any::Any + Send>) {
        self.panics += 1;
        if self.panics <= 3 {
            let what = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default();
            self.cur().send(&HostMsg::Note { text: format!("overlay: the overlay's host failed a tick ({what}); it carries on") });
        }
    }

    /// Screen-space pane projection of `focus` (with live-OS-pointer flag and foreground
    /// status), for in-process hardware `GetCursorPos` and one-shot `SetCursorPos` warps.
    fn focus_pane_projection(&self, focus: Option<usize>, front: bool) -> peerparty_protocol::input_shm::PaneProjection {
        let Some(c) = self.compositor.as_ref() else {
            return peerparty_protocol::input_shm::PaneProjection::default();
        };
        let overlay_open = self.panes.get(self.focus).is_some_and(|p| p.visible) || self.layout_studio.active;
        let comp_front = front && !overlay_open && !self.kbm_off && c.hwnd() != 0 && desktop::in_front(std::process::id());
        match focus {
            Some(f) => c.pane_projection(f, true, comp_front),
            None => peerparty_protocol::input_shm::PaneProjection { compositor_hwnd: c.hwnd(), ..Default::default() },
        }
    }

    /// Between ticks: forward queued mouse/key messages first (so `cursor_screen` is up
    /// to date and clicks/moves never lag), publish keyboards and mice to SHM, and apply
    /// any instant cursor shape/visibility/confinement update signalled via `WM_APP_CURSOR`.
    fn kbm_tick(&mut self) {
        self.forward_queued_input();
        let front = self.in_front();
        let focus = self.input.kbm_focus();
        let focus_cursor = focus.and_then(|f| self.compositor.as_ref().and_then(|c| c.cursor_screen(f)));
        let focus_pane = self.focus_pane_projection(focus, front);
        self.input.publish_kbm(&mut self.instances, focus_cursor, focus_pane);
        if self.compositor.as_ref().is_some_and(|c| c.take_cursor_dirty()) {
            self.update_pointer(front);
        }
    }

    /// How long the loop waits before its next tick (ms).
    fn tick_wait(&self, open: bool) -> u32 {
        let last_frame = if self.layout_studio.active {
            self.layout_studio.last_frame
        } else {
            self.panes[self.focus].render.last_frame
        };
        if open && input::pending() {
            // Input waits: the next frame as soon as the last one is far enough behind.
            FRAME_GAP.saturating_sub(last_frame.elapsed()).as_millis() as u32
        } else if open || self.panes.iter().any(|p| p.overlay.has_toasts()) {
            16
        } else if self.in_front() {
            33
        } else {
            // Often enough to see a Home / Guide press that can't open the overlay
            // (`why_not_open`).
            50
        }
    }

    /// Waits up to `ms` on the first copy still running, handing the host's windows
    /// their messages meanwhile, then looks at the others. True once every copy has
    /// ended.
    fn wait(&mut self, ms: u32) -> bool {
        let Some(first) = self.instances.iter().position(|i| !i.gone) else { return true };
        let ended = match self.instances[first].ended() {
            Some(e) => desktop::wait(e, ms),
            None => {
                std::thread::sleep(Duration::from_millis(ms.into()));
                false
            }
        };
        if ended {
            self.instances[first].end();
        }
        let mut any_ended = ended;
        for inst in self.instances.iter_mut().skip(first + 1) {
            any_ended |= !inst.gone && inst.check_ended();
        }
        if self.instances.iter().all(|i| i.gone) {
            return true;
        }
        if any_ended {
            self.panes_stale = true;
            for i in 0..self.instances.len() {
                if self.instances[i].gone {
                    self.hide(i);
                }
            }
            if self.instances[self.focus].gone {
                // The keyboard goes to a copy still running.
                let next = self.instances.iter().position(|i| !i.gone).unwrap_or(0);
                self.set_focus(next);
            }
        }
        false
    }

    /// The focused copy.
    fn cur(&self) -> &InstanceConn {
        &self.instances[self.focus]
    }

    /// Whether some copy's overlay is open.
    fn any_open(&self) -> bool {
        self.panes.iter().any(|p| p.visible)
    }

    /// The copy in front: the one whose window is, or while a session's window is, the
    /// copy in its focused pane.
    fn front_copy(&self) -> Option<usize> {
        if let Some(i) = self.instances.iter().position(|i| i.in_front()) {
            return Some(i);
        }
        let c = self.compositor.as_ref().filter(|c| c.hwnd() != 0)?;
        let i = c.focused();
        (desktop::in_front(std::process::id()) && self.instances.get(i).is_some_and(|i| !i.gone)).then_some(i)
    }

    /// Whether the game is on screen: the focused copy is the one in front (a session's
    /// other copies are on screen with it, in their panes).
    fn in_front(&self) -> bool {
        self.front_copy() == Some(self.focus)
    }

    /// Each copy's window: in front as it opened, or since (`Front`). A split-screen
    /// session's only; a copy alone follows the front as before.
    fn note_fronts(&mut self) {
        if self.compositor.is_none() || self.instances.len() <= 1 {
            return;
        }
        let fg = desktop::foreground().pid;
        for k in 0..self.instances.len() {
            let inst = &mut self.instances[k];
            if inst.gone {
                continue;
            }
            let was = inst.front;
            inst.front = was.next(inst.pid == fg);
            if inst.front == instance::Front::Opened && was == instance::Front::Never && k != self.focus {
                let text = format!(
                    "session: player {}'s window came to the front as it opened: the keyboard and mouse stay with player {} (click a pane, or Ctrl+Alt+1-4, to move them)",
                    k + 1,
                    self.focus + 1
                );
                inst.send(&HostMsg::Note { text: text.clone() });
                self.instances[self.focus].send(&HostMsg::Note { text });
            }
        }
    }

    /// Copy `i`'s window is in front only because it just opened (`Front::Opened`).
    fn opened_in_front(&self, i: usize) -> bool {
        self.instances.get(i).is_some_and(|c| c.in_front() && c.front == instance::Front::Opened)
    }

    /// The PC's keyboard and mouse go to copy `i` (and its overlay) from now on.
    fn set_focus(&mut self, i: usize) {
        if i >= self.instances.len() || i == self.focus {
            return;
        }
        self.focus = i;
        if let Some(exe) = crate::platform::process_exe(self.instances[i].pid) {
            self.firewall.set_exe(exe);
        }
        if let Some(c) = self.compositor.as_mut() {
            c.focus_pane(i);
            c.activate_pane(i, &self.instances);
        }
        self.pointer_to_focus = true;
        self.update_capture();
    }

    /// The keyboard and mouse are taken from the games while the focused copy's overlay
    /// or Layout Studio is open (and Game Bar isn't over it); another copy's open overlay
    /// is worked with its player's controller alone.
    fn update_capture(&self) {
        input::capture((self.panes[self.focus].visible || self.layout_studio.active) && !self.under_gamebar);
    }

    /// Controller button glyph style for Layout Studio (honors the user's glyph setting).
    fn studio_glyph_style(&mut self) -> pad::Style {
        match self.prefs.glyphs.as_str() {
            "xbox" => pad::Style::Xbox,
            "playstation" => pad::Style::PlayStation,
            "nintendo" => pad::Style::Nintendo,
            _ => self.input.pads().style,
        }
    }

    /// Snapshot of combined controller buttons, triggers, right stick and style for Layout Studio.
    fn studio_pad(&mut self) -> StudioPad {
        let (lt, rt, rx, ry, connected) = self.input.pads().studio_analog();
        let style = self.studio_glyph_style();
        StudioPad { buttons: self.pads.all, lt, rt, rx, ry, style, connected }
    }

    fn studio_sync(&self) -> StudioSync {
        if !self.sync_frames {
            StudioSync::Off
        } else {
            match self.fps_cap.sync_divisor() {
                2 => StudioSync::Half,
                3 => StudioSync::Third,
                _ => StudioSync::Full,
            }
        }
    }

    /// Opens the live Layout Studio canvas over the split-screen panes.
    fn open_layout_studio(&mut self) {
        if self.layout_studio.active {
            return;
        }
        let Some(bounds) = self.compositor.as_ref().filter(|c| c.hwnd() != 0).map(|c| c.bounds()) else {
            return;
        };
        let slots = self.compositor.as_ref().map(|c| c.slots().to_vec()).unwrap_or_default();
        if slots.is_empty() {
            return;
        }
        let (w, h) = (i32::try_from(bounds.width).unwrap_or(i32::MAX), i32::try_from(bounds.height).unwrap_or(i32::MAX));
        let rect = Rect { left: bounds.x, top: bounds.y, right: bounds.x.saturating_add(w), bottom: bounds.y.saturating_add(h) };
        let pad = self.studio_pad();
        // Activate Studio before hiding any open overlay so input capture stays on seamlessly
        // without tearing down and reinstalling the low-level keyboard/mouse hooks.
        self.layout_studio.open(slots, self.focus, self.studio_sync(), rect, self.prefs.overlay_scale, pad);
        for i in 0..self.panes.len() {
            if self.panes[i].visible {
                self.hide(i);
            }
        }
        self.update_capture();
        self.cur().send(&HostMsg::Note { text: "session: Layout Studio opened (drag dividers/corners to resize, drag panes to move or swap, Esc/B to finish)".into() });
    }

    /// Closes Layout Studio and restores normal session input.
    fn close_layout_studio(&mut self) {
        if !self.layout_studio.active {
            return;
        }
        self.layout_studio.close();
        self.nav.keys_down = [true; 4];
        self.update_capture();
        self.cur().send(&HostMsg::Note { text: "session: Layout Studio closed".into() });
    }

    /// Saves the chosen split-screen layout for this game in `ui.json` (using a clean
    /// preset name when it matches a built-in preset, else `"custom"` + `slots:...`).
    fn save_custom_layout(&self, slots: &[PaneSlot]) {
        let count = slots.len();
        let named = [
            ("horizontal", LayoutPreset::Horizontal, true),
            ("vertical", LayoutPreset::Vertical, true),
            ("grid", LayoutPreset::Grid2x2, count >= 3),
            ("top2", LayoutPreset::TopTwo, count == 3),
            ("bottom2", LayoutPreset::BottomTwo, count == 3),
        ]
        .into_iter()
        .find(|(_, p, ok)| *ok && p.to_slots(count) == slots)
        .map(|(name, _, _)| name.to_string());
        let encoded = LayoutPreset::format_slots(slots);
        let key = self.key.clone();
        let what = named.clone().unwrap_or_else(|| "custom".into());
        crate::ui::store::Settings::update(|s| {
            let o = s.games.entry(key).or_default();
            match named {
                Some(preset) => o.split_layout = preset,
                None => {
                    o.split_custom_layout = encoded;
                    o.split_layout = "custom".into();
                }
            }
        });
        self.cur().send(&HostMsg::Note { text: format!("session: saved {what} split-screen layout for this game") });
    }

    /// Runs one tick of Layout Studio while it is open.
    fn frame_studio(&mut self) {
        let Some((bounds, zoomed)) = self.compositor.as_ref().filter(|c| c.hwnd() != 0).map(|c| (c.bounds(), c.zoomed().then_some(c.focused()))) else {
            self.close_layout_studio();
            return;
        };
        let (w, h) = (i32::try_from(bounds.width).unwrap_or(i32::MAX), i32::try_from(bounds.height).unwrap_or(i32::MAX));
        let rect = Rect { left: bounds.x, top: bounds.y, right: bounds.x.saturating_add(w), bottom: bounds.y.saturating_add(h) };
        let total = self.layout_studio.slots.len().max(self.instances.len());
        let players: Vec<PlayerInfo> = (0..total)
            .map(|i| match self.instances.get(i) {
                Some(inst) => {
                    let name = if !inst.state.me.is_empty() {
                        inst.state.me.clone()
                    } else {
                        inst.spec.identity_override.clone().unwrap_or_default()
                    };
                    PlayerInfo { name, muted: self.audio.is_muted(inst.pid), starting: false, gone: inst.gone }
                }
                None => PlayerInfo { name: String::new(), muted: false, starting: true, gone: false },
            })
            .collect();
        let info = StudioInfo {
            players,
            screen: (bounds.width, bounds.height),
            keyboard: (!self.kbm_off).then_some(self.focus),
            zoomed,
            sync: self.studio_sync(),
            pictures: (0..total).map(|i| self.instances.get(i).and_then(|inst| inst.viewport.as_ref()).map(|v| (v.width, v.height)).filter(|&(w, h)| w > 0 && h > 0)).collect(),
        };
        let pad = self.studio_pad();
        let actions = self.layout_studio.tick(rect, self.prefs.overlay_scale, self.started.elapsed().as_secs_f64(), pad, &info);
        for action in actions {
            match action {
                StudioAction::Preview(slots) => {
                    if let Some(c) = self.compositor.as_mut() {
                        c.preview_slots(slots, &self.instances);
                    }
                }
                StudioAction::Commit(slots) => {
                    if let Some(c) = self.compositor.as_mut() {
                        c.set_zoomed(false);
                        c.set_layout(LayoutPreset::Slots(slots));
                    }
                    self.panes_stale = true;
                }
                StudioAction::Preset(preset) => {
                    if let Some(c) = self.compositor.as_mut() {
                        c.set_zoomed(false);
                        c.set_layout(preset);
                    }
                    self.panes_stale = true;
                }
                StudioAction::ToggleMute(i) => {
                    if let Some(inst) = self.instances.get(i) {
                        let muted = self.audio.toggle(inst.pid);
                        let msg = if muted {
                            format!("session: muted player {}'s game audio", i + 1)
                        } else {
                            format!("session: unmuted player {}'s game audio", i + 1)
                        };
                        inst.send(&HostMsg::Note { text: msg });
                        self.layout_studio.dirty = true;
                    }
                }
                StudioAction::ToggleZoom(i) => {
                    if i < self.instances.len() && !self.instances[i].gone {
                        self.set_focus(i);
                        if let Some(c) = self.compositor.as_mut() {
                            let same = c.zoomed() && c.focused() == i;
                            c.focus_pane(i);
                            c.set_zoomed(!same);
                        }
                        self.panes_stale = true;
                        self.close_layout_studio();
                    }
                }
                StudioAction::KeyboardStep(d) => {
                    self.keyboard_step(d);
                    self.layout_studio.dirty = true;
                }
                StudioAction::SetSync(mode) => {
                    let on = mode.is_on();
                    self.sync_frames = on;
                    match mode {
                        StudioSync::Full => {
                            if self.fps_cap.sync_divisor() > 1 {
                                self.fps_cap = crate::launch::FpsCap::Display;
                            }
                        }
                        StudioSync::Half => self.fps_cap = crate::launch::FpsCap::Divisor(2),
                        StudioSync::Third => self.fps_cap = crate::launch::FpsCap::Divisor(3),
                        StudioSync::Off => {}
                    }
                    let interval = if on { self.fps_cap.sync_divisor() } else { 1 };
                    if let Some(c) = self.compositor.as_mut() {
                        c.set_sync_interval(interval);
                    }
                    for inst in &self.instances {
                        inst.set_sync(on);
                        inst.set_fps_cap(self.fps_cap, on);
                    }
                    let msg = match mode {
                        StudioSync::Full => "session: VSync set to full display refresh",
                        StudioSync::Half => "session: VSync set to 1/2 display refresh",
                        StudioSync::Third => "session: VSync set to 1/3 display refresh",
                        StudioSync::Off => "session: VSync turned off (copies render at their own frame rate)",
                    };
                    self.cur().send(&HostMsg::Note { text: msg.into() });
                    self.layout_studio.dirty = true;
                }
                StudioAction::Save(slots) => {
                    if let Some(c) = self.compositor.as_mut() {
                        c.set_zoomed(false);
                        c.set_layout(LayoutPreset::Slots(slots.clone()));
                    }
                    self.save_custom_layout(&slots);
                    self.panes_stale = true;
                }
                StudioAction::SaveSync(mode) => {
                    let what = match mode {
                        StudioSync::Full => "full display refresh",
                        StudioSync::Half => "1/2 display refresh",
                        StudioSync::Third => "1/3 display refresh",
                        StudioSync::Off => "off",
                    };
                    self.cur().send(&HostMsg::Note { text: format!("session: VSync saved as this game's setting ({what}); the next start uses it") });
                    let key = self.key.clone();
                    crate::ui::store::Settings::update(|s| {
                        let g = s.games.entry(key).or_default();
                        match mode {
                            StudioSync::Full => {
                                g.split_no_sync = false;
                                if g.split_fps.sync_divisor() > 1 {
                                    g.split_fps = crate::launch::FpsCap::Display;
                                }
                            }
                            StudioSync::Half => {
                                g.split_no_sync = false;
                                g.split_fps = crate::launch::FpsCap::Divisor(2);
                            }
                            StudioSync::Third => {
                                g.split_no_sync = false;
                                g.split_fps = crate::launch::FpsCap::Divisor(3);
                            }
                            StudioSync::Off => {
                                g.split_no_sync = true;
                                if g.split_fps.sync_divisor() > 1 {
                                    g.split_fps = crate::launch::FpsCap::Display;
                                }
                            }
                        }
                    });
                }
                StudioAction::Close => self.close_layout_studio(),
            }
        }
    }

    /// Smart ready-gate launch staggering: spawns the next queued split-screen copy once
    /// the latest running copy is ready (first frame / viewport + short settle floor, or
    /// fallback timeout), or drops the queue immediately if the session is closing.
    fn stagger_tick(&mut self) {
        if self.pending_spawns.is_empty() || self.spawner.is_none() {
            return;
        }
        if self.closing_since.is_some() {
            self.pending_spawns.clear();
            return;
        }
        // If copy 0 died right after launch, don't pile on copies 2..N.
        if self.instances.first().is_some_and(|i| i.gone) && self.started.elapsed() < SMART_STAGGER_ABORT_WINDOW {
            self.pending_spawns.clear();
            return;
        }
        let now = Instant::now();
        let ready = match self.stagger {
            StaggerMode::Fixed(0) => true,
            StaggerMode::Fixed(secs) => self.last_spawned_at.elapsed() >= Duration::from_secs(secs.into()),
            StaggerMode::Smart => {
                let Some(last) = self.instances.last() else { return };
                if last.gone || last.surface.is_some() || self.last_spawned_at.elapsed() >= SMART_STAGGER_MAX {
                    true
                } else if last.best_hwnd() != 0 || last.viewport.as_ref().is_some_and(|v| v.width > 0 && v.height > 0) {
                    let since = *self.stagger_ready_at.get_or_insert(now);
                    since.elapsed() >= SMART_STAGGER_WINDOW_SETTLE
                } else {
                    false
                }
            }
        };
        if !ready {
            return;
        }
        let Some(slot) = self.pending_spawns.pop_front() else { return };
        let pane = self.compositor.as_ref().and_then(|c| c.screen_pane(slot as usize));
        let virt = self.compositor.as_ref().and_then(|c| c.virtual_size(slot as usize));
        let result = self.spawner.as_mut().expect("checked")(slot, pane, virt);
        self.last_spawned_at = now;
        self.stagger_ready_at = None;
        match result {
            Ok(launch) => {
                let input = launch.input;
                let mut conn = InstanceConn::new(launch.spec, launch.process, launch.pid, launch.pipe);
                conn.stop = launch.stop;
                conn.make_input_block();
                let event = self.compositor.as_ref().and_then(|c| c.frame_event());
                let draws = self.compositor.as_ref().is_some_and(|c| c.draws_frames());
                if draws {
                    conn.make_ring(self.pixels);
                    conn.set_sync(self.sync_frames);
                    conn.set_fps_cap(self.fps_cap, self.sync_frames);
                    conn.send_capture(event.as_deref());
                }
                self.instances.push(conn);
                self.panes.push(PaneOverlay::new(self.game_title.clone()));
                self.inputs.push(input);
                self.kbm_player.push(false);
                if input == SlotInput::KeyboardMouse {
                    self.kbm_off = false;
                }
                self.refresh_hints();
                self.panes_stale = true;
                self.layout_studio.dirty = true;
            }
            Err(e) => {
                eprintln!("peerparty: session: couldn't start player {}: {e}", slot + 1);
                self.pending_spawns.clear();
            }
        }
    }

    /// Which running split-screen copy owns controller `pad_index` (if any).
    fn instance_for_pad(&self, pad_index: usize) -> Option<usize> {
        let want = u32::try_from(pad_index).ok()?;
        self.instances.iter().position(|inst| {
            !inst.gone && matches!(inst.spec.input_assignment, InputAssignment::Controller { index } if index == want)
        })
    }

    /// Keeps the real mouse in the focused pane while its game has locked its cursor (an
    /// aiming game), and hides the pointer while locked, while the focused overlay or
    /// Layout Studio owns the cursor (`WH_MOUSE_LL` anchors the system cursor while the
    /// overlay draws its own), while the game hid its cursor (`ShowCursor`), or while the
    /// mouse sits idle with controllers active (or KBM off).
    fn update_pointer(&mut self, front: bool) {
        let f = self.focus;
        // The mouse went to another player: a pointer left over another pane would hold
        // the game's cursor at its pane's edge until it crossed over. Not while an overlay
        // or Layout Studio is open (the pointer is working it): once it closes.
        if self.pointer_to_focus && (self.kbm_off || self.instances.get(f).is_none_or(|i| i.gone)) {
            self.pointer_to_focus = false;
        }
        if self.pointer_to_focus
            && front
            && !self.panes[f].visible
            && !self.layout_studio.active
            && let Some(c) = self.compositor.as_ref()
        {
            self.pointer_to_focus = false;
            if desktop::in_front(std::process::id()) && c.pointer_into(f) {
                self.instances[f].send(&HostMsg::Note { text: "session: the mouse was over another pane: the pointer moved to the middle of this one, whose player has it now".into() });
            }
        }
        let has_pad = self.input.pads().seat_info().iter().any(Option::is_some);
        let fb = self.instances.get(f).and_then(|i| i.input.as_ref()).and_then(|b| b.cursor_channel()).and_then(|ch| ch.read_feedback());
        let Some(c) = self.compositor.as_mut() else { return };
        let ours = front && c.hwnd() != 0 && desktop::in_front(std::process::id());
        let overlay_owns = ours && (self.panes[f].visible || self.layout_studio.active) && !self.under_gamebar;
        let active = ours && !self.panes[f].visible && !self.layout_studio.active && self.instances.get(f).is_some_and(|i| !i.gone);
        let (clipped, recentering, hidden_show, hidden_shape, shape, shape_gen, clip_norm) = match fb {
            Some((fb, sgen)) => (
                fb.clipped(),
                fb.recentering(),
                fb.hidden_show(),
                fb.hidden_shape(),
                fb.has_shape().then_some(fb.handle),
                sgen,
                fb.clip_norm(),
            ),
            None => (
                self.instances[f].cursor_locked,
                false,
                self.instances[f].cursor_hidden,
                self.instances[f].cursor_shape == Some(0),
                self.instances[f].cursor_shape,
                0,
                None,
            ),
        };
        let lock = active && !self.kbm_off && (clipped || recentering);
        let shape = if active && !self.kbm_off { shape } else { None };
        let game_hide = active && !self.kbm_off && (hidden_show || hidden_shape || recentering);
        let idle_hide = active && (self.kbm_off || has_pad);
        if c.update_pointer(f, lock.then_some(f), clip_norm, overlay_owns, game_hide, idle_hide, shape.unwrap_or(0), shape_gen) {
            let text = if lock {
                if game_hide {
                    "session: the game locked its cursor: the mouse stays in its pane, the pointer hidden, while the session is in front".to_string()
                } else {
                    "session: the game confined its cursor: the mouse stays in its pane while the session is in front".to_string()
                }
            } else {
                "session: the mouse is free of the pane again (the cursor unlocked, the overlay opened, or the session left the front)".to_string()
            };
            self.instances[f].send(&HostMsg::Note { text });
        }
        if let Some((hidden, still)) = c.take_pointer_change() {
            let text = if hidden {
                format!("session: the pointer hides: the mouse sat still while {} ({still} mouse moves that didn't move it left out)", if self.kbm_off { "keyboard and mouse are off" } else { "controllers play" })
            } else {
                format!("session: the pointer shows: the mouse moved ({still} mouse moves that didn't move it left out)")
            };
            self.instances[f].send(&HostMsg::Note { text });
        }
    }

    /// Starts the session with keyboard and mouse kept from the games (controllers only),
    /// unless a player explicitly chose keyboard and mouse.
    fn set_kbm_off(&mut self, off: bool) {
        if off && !self.inputs.contains(&SlotInput::KeyboardMouse) {
            self.kbm_off = true;
            self.cur().send(&HostMsg::Note { text: "input: keyboard and mouse to games are off (controllers only; overlay and Ctrl+Alt+1-4 still work)".into() });
        }
    }

    /// What each player plays with, for a session just started: each one's said in
    /// their game's log, and the session starts with the keyboard player's pane focused.
    fn set_inputs(&mut self, inputs: Vec<SlotInput>) {
        self.inputs = inputs;
        self.inputs.resize(self.instances.len(), SlotInput::Auto);
        let has_kbm = self.inputs.contains(&SlotInput::KeyboardMouse);
        for k in 0..self.instances.len() {
            let what = match self.inputs[k] {
                SlotInput::Auto => "a controller (the first one used that no other player has), and the keyboard and mouse while their pane has the focus",
                SlotInput::KeyboardMouse => "the keyboard and mouse, never a controller",
                SlotInput::Controller if has_kbm => "a controller (clicking their pane won't pull the keyboard and mouse onto it; Ctrl+Alt+1-4 still can)",
                SlotInput::Controller => "a controller; the keyboard and mouse only while their pane has the focus",
            };
            self.instances[k].send(&HostMsg::Note { text: format!("input: player {} plays with {what}", k + 1) });
        }
        if let Some(k) = self.inputs.iter().position(|i| *i == SlotInput::KeyboardMouse) {
            self.instances[k].send(&HostMsg::Note { text: format!("input: the session starts with player {}'s pane focused: theirs are the keyboard and mouse", k + 1) });
            self.set_focus(k);
        }
    }

    /// Whether a mouse click on pane `i` may switch keyboard-and-mouse focus to it.
    /// When a player explicitly chose keyboard and mouse, clicking an explicit
    /// controller player's pane won't steal focus onto it (Ctrl+Alt+1-4 still can).
    fn click_can_focus(&self, i: usize) -> bool {
        !(self.inputs.contains(&SlotInput::KeyboardMouse) && self.inputs.get(i) == Some(&SlotInput::Controller))
    }

    /// Whether a split-screen session's seat `k` may take a controller now (`Pads::set_open`);
    /// None for a copy alone.
    fn seat_open(&self) -> Option<Vec<pad::Open>> {
        if self.compositor.is_none() || self.instances.len() <= 1 {
            return None;
        }
        let open = (0..self.instances.len()).map(|k| match self.inputs.get(k).copied().unwrap_or_default() {
            _ if self.instances[k].gone => pad::Open::No,
            SlotInput::KeyboardMouse => pad::Open::No,
            SlotInput::Controller => pad::Open::Yes,
            SlotInput::Auto if self.kbm_player.get(k) == Some(&true) => pad::Open::Later,
            SlotInput::Auto => pad::Open::Yes,
        });
        Some(open.collect())
    }

    /// Reads the controllers, once for the tick.
    fn poll_pads(&mut self) {
        let open = self.seat_open();
        let pads = self.input.pads();
        if let Some(open) = open {
            pads.set_open(open);
        }
        let all = pads.poll();
        let seated = std::mem::take(&mut pads.newly_seated);
        let notes = std::mem::take(&mut pads.notes);
        self.pads = PadFrame { all, per: pads.per.clone(), spare: pads.spare, homes: std::mem::take(&mut pads.homes) };
        let session = self.compositor.is_some() && self.instances.len() > 1;
        for (seat, name) in seated {
            let to = if seat < self.instances.len() { seat } else { self.focus };
            // Its player is told which controller is theirs now, with more than one
            // player (one playing alone needn't be).
            let product = session.then(|| self.input.pads().product_name(seat)).flatten();
            let named = product.as_ref().map(|p| format!(", {p:?}")).unwrap_or_default();
            let text = format!("input: {name}{named} claimed seat {seat} (player {})", seat + 1);
            self.instances[to].send(&HostMsg::Note { text });
            if let Some(p) = product
                && self.prefs.toasts
                && seat < self.panes.len()
            {
                self.panes[seat].overlay.toast(crate::t!("toast.pad_connected", name = p), crate::t!("toast.pad_connected_detail", n = seat + 1));
            }
        }
        for text in notes {
            self.cur().send(&HostMsg::Note { text });
        }
    }

    /// A split-screen session: a player with no controller yet who presses keys and
    /// mouse buttons in their pane (while it has the focus) plays with the keyboard and
    /// mouse, so a controller used next goes to another player first (`seat_open`).
    fn watch_keyboard(&mut self, front: bool) {
        if self.compositor.is_none() || self.instances.len() <= 1 {
            return;
        }
        let presses = self.input.kbm_presses();
        let new = presses.saturating_sub(self.kbm_seen.last);
        self.kbm_seen.last = presses;
        let f = self.focus;
        if new == 0 || !front || self.kbm_off || self.layout_studio.active || self.any_open() || self.kbm_player.get(f) != Some(&false) || self.inputs.get(f) != Some(&SlotInput::Auto) {
            return;
        }
        if self.kbm_seen.pane != f {
            self.kbm_seen.pane = f;
            self.kbm_seen.count = 0;
        }
        if self.input.pads().seat_info().get(f).is_some_and(Option::is_some) {
            return;
        }
        self.kbm_seen.count += new;
        if self.kbm_seen.count >= KBM_PLAYER_PRESSES {
            self.kbm_player[f] = true;
            let text = format!(
                "input: player {} plays with the keyboard and mouse ({} presses in their pane, no controller of theirs used yet): a controller used next goes to another player first",
                f + 1,
                self.kbm_seen.count
            );
            self.instances[f].send(&HostMsg::Note { text });
        }
    }

    /// The players on this PC, for an open overlay's Players page.
    fn seats(&mut self, me: usize) -> Option<ui::Seats> {
        if self.compositor.is_none() || self.instances.len() <= 1 {
            return None;
        }
        let info = self.input.pads().seat_info();
        let n = self.instances.len();
        let takes_pad = |k: usize, s: &SessionHost| !s.instances[k].gone && s.inputs.get(k) != Some(&SlotInput::KeyboardMouse);
        let pad_players = (0..n).filter(|&k| takes_pad(k, self)).count();
        let players = (0..n)
            .map(|k| {
                let seat = info.get(k).cloned().flatten();
                let detail = if self.instances[k].gone {
                    crate::t!("overlay.seat_gone").to_string()
                } else if self.inputs.get(k) == Some(&SlotInput::KeyboardMouse) {
                    crate::t!("overlay.seat_kbm").to_string()
                } else {
                    match &seat {
                        Some(s) if s.here => crate::t!("overlay.seat_pad", kind = style_word(s.style)),
                        Some(s) => crate::t!("overlay.seat_away", kind = style_word(s.style)),
                        None if self.kbm_player.get(k) == Some(&true) => crate::t!("overlay.seat_later").to_string(),
                        None => crate::t!("overlay.seat_none").to_string(),
                    }
                };
                ui::PlayerSeat { detail, can_swap: takes_pad(k, self) && pad_players > 1, can_free: takes_pad(k, self) && seat.is_some() }
            })
            .collect();
        Some(ui::Seats { players, me, keyboard: (!self.kbm_off).then_some(self.focus) })
    }

    /// Player `k`'s controller trades places with the next player's who plays with one.
    fn swap_seat(&mut self, k: usize, asked_by: usize) {
        let n = self.instances.len();
        let takes_pad = |j: usize| !self.instances[j].gone && self.inputs.get(j) != Some(&SlotInput::KeyboardMouse);
        let Some(other) = (1..n).map(|d| (k + d) % n).find(|&j| takes_pad(j)) else { return };
        self.input.pads().swap(k, other);
        let text = format!("input: player {} and player {} traded controllers (asked from player {}'s overlay)", k + 1, other + 1, asked_by + 1);
        for j in [k, other] {
            self.instances[j].send(&HostMsg::Note { text: text.clone() });
        }
    }

    /// Player `k`'s controller is let go: the next controller used takes their seat.
    fn free_seat(&mut self, k: usize, asked_by: usize) {
        self.input.pads().free(k);
        let text = format!("input: player {}'s controller let go (asked from player {}'s overlay): the next controller used takes their seat", k + 1, asked_by + 1);
        self.instances[k].send(&HostMsg::Note { text });
    }

    /// The keyboard and mouse go `d` steps on (wrapping through active players and Off).
    fn keyboard_step(&mut self, d: i32) {
        let mut opts: Vec<Option<usize>> = (0..self.instances.len()).filter(|&j| !self.instances[j].gone).map(Some).collect();
        opts.push(None);
        let cur = if self.kbm_off { None } else { Some(self.focus) };
        let at = opts.iter().position(|&o| o == cur).unwrap_or(0) as i32;
        match opts[(at + d).rem_euclid(opts.len() as i32) as usize] {
            Some(j) => {
                self.kbm_off = false;
                let text = format!("input: the keyboard and mouse go to player {} (from the overlay)", j + 1);
                self.instances[j].send(&HostMsg::Note { text });
                self.set_focus(j);
            }
            None => {
                self.kbm_off = true;
                self.cur().send(&HostMsg::Note { text: "input: keyboard and mouse to games turned off (controllers only, from the overlay)".into() });
            }
        }
    }

    /// Copy `i`'s player's controller this tick: a copy alone reads every controller; a
    /// session's copy reads the one it was given, and the focused copy also the ones no
    /// copy was given.
    fn pane_pad(&self, i: usize) -> u32 {
        if self.compositor.is_none() || self.instances.len() <= 1 {
            return self.pads.all;
        }
        let own = match self.instances.get(i).map(|c| &c.spec.input_assignment) {
            Some(InputAssignment::Controller { index }) => usize::try_from(*index).ok().and_then(|k| self.pads.per.get(k)).copied().unwrap_or(0),
            _ => 0,
        };
        let spare = if i == self.focus {
            self.pads.per.iter().enumerate().filter(|(k, _)| self.instance_for_pad(*k).is_none()).fold(self.pads.spare, |a, (_, b)| a | b)
        } else {
            0
        };
        own | spare
    }

    /// Whose overlay a Home press on the controller of seat `k` (None: one without a
    /// seat) is for: the copy that seat was given, else the focused one.
    fn pane_for_pad(&self, k: Option<usize>) -> usize {
        if self.compositor.is_none() {
            return self.focus;
        }
        k.and_then(|k| self.instance_for_pad(k)).unwrap_or(self.focus)
    }

    /// The hotkeys that open the overlay in this game: the ones that are on, but not one
    /// a store overlay loaded in the game opens on too (`usable_hotkeys`). In a
    /// split-screen session PeerParty's compositor window holds the foreground, so no
    /// store overlay opens on its hotkey.
    fn hotkeys(&self) -> [(bool, u32); 4] {
        let hk = &self.prefs.hotkeys;
        let foreign: &[String] = if self.compositor.is_some() { &[] } else { &self.panes[self.focus].overlay.state.foreign };
        let on = usable_hotkeys([hk.shift_tab, hk.shift_f3, hk.shift_f1, hk.shift_f2], foreign, self.guide_opens() || self.chord_opens());
        [(on[0], VK_TAB), (on[1], VK_F3), (on[2], VK_F1), (on[3], VK_F2)]
    }

    /// The first hotkey that is on but left to a store's overlay in this game, with
    /// that store's name.
    fn key_left(&self) -> Option<(&'static str, &'static str)> {
        let hk = &self.prefs.hotkeys;
        let on = [hk.shift_tab, hk.shift_f3, hk.shift_f1, hk.shift_f2];
        let usable = self.hotkeys();
        (0..4).find(|&i| on[i] && !usable[i].0).map(|i| (HOTKEY_NAMES[i], HOTKEY_OWNERS[i].1))
    }

    /// The hotkeys that are on.
    fn key_names(&self) -> Vec<&'static str> {
        self.hotkeys().iter().zip(HOTKEY_NAMES).filter(|((on, _), _)| *on).map(|(_, n)| n).collect()
    }

    /// What opens the overlay, for notifications: "Shift+Tab / Shift+F3 or the Home /
    /// Xbox Guide button".
    fn open_hint(&self) -> String {
        let mut parts = Vec::new();
        let keys = self.key_names().join(" / ");
        if !keys.is_empty() {
            parts.push(keys);
        }
        if self.guide_opens() {
            parts.push(crate::t!("overlay.home_button").to_string());
        }
        if self.chord_opens() {
            parts.push(crate::t!("hint.chord_held", buttons = pad::names(self.prefs.hotkeys.chord)));
        }
        match parts.len() {
            0 => crate::t!("hint.games_own_button").into(),
            1 => parts.remove(0),
            n => crate::t!("hint.or", first = parts[..n - 1].join(", "), last = parts[n - 1]),
        }
    }

    /// What closes it (besides B), for the overlay's footer. The Home button only opens.
    fn close_hint(&self) -> String {
        let mut parts = self.key_names();
        parts.push("Esc");
        parts.join(" / ")
    }

    /// Every overlay's footer names what closes it.
    fn refresh_hints(&mut self) {
        let hint = self.close_hint();
        for p in &mut self.panes {
            if p.overlay.hotkey_hint != hint {
                p.overlay.hotkey_hint = hint.clone();
                p.render.dirty = true;
            }
        }
    }

    fn guide_opens(&self) -> bool {
        guide_opens(&self.prefs)
    }

    fn chord_opens(&self) -> bool {
        chord_opens(&self.prefs)
    }

    /// Where the overlay or a toast goes for copy `i`: copy `i`'s pane of a session's
    /// window, else copy `i`'s own window.
    fn pane_rect(&mut self, i: usize) -> Option<Rect> {
        if let Some(c) = self.compositor.as_ref().filter(|c| c.hwnd() != 0) {
            let b = c.bounds();
            let p = c.display_panes().get(i).copied().filter(|p| p.width > 100 && p.height > 100)?;
            let (w, h) = (i32::try_from(p.width).unwrap_or(i32::MAX), i32::try_from(p.height).unwrap_or(i32::MAX));
            let (left, top) = (b.x.saturating_add(p.x), b.y.saturating_add(p.y));
            return Some(Rect { left, top, right: left.saturating_add(w), bottom: top.saturating_add(h) });
        }
        self.instances.get_mut(i)?.game_rect()
    }

    /// The focused copy's pane or window.
    fn game_rect(&mut self) -> Option<Rect> {
        self.pane_rect(self.focus)
    }

    fn tick(&mut self) {
        input::heartbeat();
        // Forward queued mouse/key messages and publish KBM immediately before controller
        // polling and window maintenance so clicks and moves never stall behind a full tick.
        if !self.any_open() && !self.layout_studio.active {
            self.kbm_tick();
        }
        // Poll controllers before handling hook messages: when the game opens the
        // overlay on a controller button press (its own Invite button), `show()` must
        // see that button already down in `pad_prev` so `gather()` doesn't fire Row 0
        // on the very first frame.
        self.poll_pads();
        self.messages();
        self.reload_prefs();
        self.note_fronts();
        if let Some(i) = self.front_copy().filter(|&i| i != self.focus) {
            // Another copy came to the front: the keyboard follows it, unless its window
            // is in front only because it just opened.
            if !self.opened_in_front(i) {
                self.set_focus(i);
            }
        }
        self.session_tick();
        self.stagger_tick();
        if self.audio_refresh.elapsed() >= Duration::from_secs(2) {
            self.audio_refresh = Instant::now();
            self.audio.refresh();
        }
        self.firewall_tick();
        let front = self.in_front();
        self.track_focus(front);
        self.watch_keyboard(front);
        // Game Bar opens over the whole screen: a session's whole window is looked at, so
        // one over another pane than the focused one's counts too.
        let rect = if !front {
            None
        } else if let Some(b) = self.compositor.as_ref().filter(|c| c.hwnd() != 0).map(|c| c.bounds()) {
            let (w, h) = (i32::try_from(b.width).unwrap_or(i32::MAX), i32::try_from(b.height).unwrap_or(i32::MAX));
            Some(Rect { left: b.x, top: b.y, right: b.x.saturating_add(w), bottom: b.y.saturating_add(h) })
        } else {
            self.game_rect()
        };
        let gamebar = front && self.gamebar.open(rect);
        if gamebar != self.under_gamebar {
            self.under_gamebar = gamebar;
            self.gamebar_changed(gamebar);
        }
        let was_focus_open = self.panes[self.focus].visible;
        for i in 0..self.panes.len() {
            if !self.panes[i].visible {
                continue;
            }
            if !front {
                self.hide(i);
            } else if self.instances[i].unseen.too_long(self.instances[i].ingame, Instant::now()) {
                // Open, holding the keyboard, mouse and controllers, and on no screen:
                // to the player the game has frozen. Close, and say why where it can
                // be read (the log; a toast once the game leaves fullscreen).
                self.instances[i].send(&HostMsg::Note { text: "overlay: closed again: it was to be drawn inside the game's frames (the game is fullscreen, or drawing is set to in-game) and the game's graphics API isn't one PeerParty draws into".into() });
                self.hide(i);
                self.panes[i].overlay.toast(crate::t!("toast.cant_draw"), crate::t!("toast.cant_draw_detail"));
            } else if gamebar {
                // Game Bar has the controller, keyboard and mouse. Buttons pressed in
                // it don't act here once it closes.
                let pad = self.pane_pad(i);
                let nav = &mut self.panes[i].nav;
                nav.pad_prev = pad;
                nav.repeat.reset();
            } else {
                self.frame_open(i);
            }
        }
        let focus_just_closed = was_focus_open && !self.panes[self.focus].visible;
        if self.layout_studio.active {
            if !front || gamebar {
                self.close_layout_studio();
            } else {
                self.frame_studio();
            }
        }
        if front && self.prefs.overlay && !gamebar && !self.layout_studio.active {
            // The game doesn't get the opening hotkeys (input.rs `watch_hotkeys`).
            let vks: Vec<u32> = self.hotkeys().iter().filter(|(on, _)| *on).map(|(_, vk)| *vk).collect();
            input::watch_hotkeys(&vks);
            if focus_just_closed {
                let _ = input::take_hotkey();
                self.nav.keys_down = [true; 4];
            } else {
                self.check_open();
            }
        } else {
            input::watch_hotkeys(&[]);
            if !self.layout_studio.active {
                self.why_not_open(front, gamebar);
            }
            // A Home or hotkey still down when the game comes back to the front (Guide
            // closing Game Bar) isn't a press.
            self.input.pads().hold_home();
            self.nav.keys_down = [true; 4];
        }
        for i in 0..self.panes.len() {
            self.release_pads(i);
        }
        self.frame_toasts();
        let open: Vec<bool> = self.panes.iter().map(|p| p.visible || self.layout_studio.active).collect();
        // The keyboard and mouse are the focused player's only while the session is in
        // front, Layout Studio is closed, and keyboard/mouse input to games is enabled.
        let focus = self.compositor.as_ref().map(|_| self.focus).filter(|_| front && !self.kbm_off && !self.layout_studio.active);
        let focus_cursor = self.compositor.as_ref().and_then(|c| c.cursor_screen(self.focus));
        let focus_pane = self.focus_pane_projection(focus, front);
        self.input.publish_session(&mut self.instances, &open, focus, focus_cursor, focus_pane);
        if let Some(c) = self.compositor.as_ref() {
            let _ = c.take_cursor_dirty();
        }
        self.update_pointer(front);
    }

    /// Every copy's messages since the last tick.
    fn messages(&mut self) {
        for i in 0..self.instances.len() {
            let msgs: Vec<HookMsg> = self.instances[i].pipe().map(|p| p.rx.try_iter().collect()).unwrap_or_default();
            for msg in msgs {
                self.message(i, msg);
            }
        }
    }

    /// Every overlay draws its next frame whole (the panes moved or changed size).
    fn redraw_all(&mut self) {
        for p in &mut self.panes {
            p.render.dirty = true;
        }
    }

    /// A split-screen session's window: a click on a pane focuses its copy, its keys
    /// switch the layout, zoom and focus, and the copies are told their panes when they
    /// changed.
    fn session_tick(&mut self) {
        let Some(c) = self.compositor.as_mut() else { return };
        if c.tick() {
            self.panes_stale = true;
        }
        let window_close = c.take_close_requested();
        let close_all = CLOSE_ASKED.swap(false, std::sync::atomic::Ordering::AcqRel);
        if window_close || close_all {
            let how = if window_close { "the session's window was closed" } else { "Close all in PeerParty's window" };
            if self.closing_since.is_none() {
                c.close_instances(&self.instances, how);
                self.closing_since = Some(Instant::now());
            } else if !self.ended_all {
                // Asked again while they close: ended now.
                self.ended_all = true;
                c.terminate_instances(&self.instances, &format!("{how} again while closing"));
            }
        } else if !self.ended_all && self.closing_since.is_some_and(|t| t.elapsed() >= CLOSE_WAIT) {
            self.ended_all = true;
            c.terminate_instances(&self.instances, &format!("still running {} s after being asked to close", CLOSE_WAIT.as_secs()));
        }
        let click = c.take_click();
        let clicked_pane = if self.kbm_off || self.layout_studio.active { None } else { click.and_then(|(x, y)| c.pane_at(x, y)) };
        let has_kbm = self.inputs.contains(&SlotInput::KeyboardMouse);
        let mut focus = clicked_pane.filter(|&i| !(has_kbm && self.inputs.get(i) == Some(&SlotInput::Controller)));
        // A focus asked for by Ctrl+Alt+1–4: said in that pane (outside a zoom nothing on
        // screen changes, so the keys seemed to do nothing).
        let mut keyed: Option<usize> = None;
        let mut toggle_studio = false;
        let ours = c.hwnd() != 0 && desktop::in_front(std::process::id());
        let held = ours || self.instances.iter().any(|i| i.in_front());
        let chord = held && ctrl_alt(down);
        for (k, vk) in SESSION_KEYS.iter().enumerate() {
            let d = chord && down(*vk);
            if d && !self.nav.session_keys[k] {
                match k {
                    0 => {
                        c.cycle_layout();
                        let text = format!("session: the {} layout", c.layout().as_str());
                        self.instances[self.focus].send(&HostMsg::Note { text });
                        if self.layout_studio.active {
                            self.layout_studio.set_slots(c.slots().to_vec());
                        }
                        self.panes_stale = true;
                    }
                    1 => {
                        c.set_zoomed(!c.zoomed());
                        let text = if c.zoomed() {
                            format!("session: player {}'s pane zoomed onto the whole window: its game is told the window's size", c.focused() + 1)
                        } else {
                            format!("session: back to the {} layout from the zoom", c.layout().as_str())
                        };
                        crate::launch::note(self.instances[c.focused()].pid, &text);
                        self.panes_stale = true;
                    }
                    10 => {
                        toggle_studio = true;
                    }
                    n => {
                        let i = (n - 2) % 4;
                        focus = Some(i);
                        keyed = Some(i);
                    }
                }
            }
            self.nav.session_keys[k] = d;
        }
        if let Some(i) = keyed {
            let pid = self.instances.get(self.focus).map_or(0, |inst| inst.pid);
            match self.instances.get(i) {
                Some(inst) if !inst.gone => crate::launch::note(inst.pid, &format!("session: Ctrl+Alt+{}: player {}'s pane has the keyboard and mouse", i + 1, i + 1)),
                _ => crate::launch::note(pid, &format!("session: Ctrl+Alt+{}: there's no player {} in this session", i + 1, i + 1)),
            }
        }
        if let Some(i) = focus.filter(|&i| i < self.instances.len() && !self.instances[i].gone) {
            if keyed == Some(i) {
                self.kbm_off = false;
            }
            c.focus_pane(i);
            c.activate_pane(i, &self.instances);
            self.panes_stale = true;
            self.set_focus(i);
            if keyed == Some(i) && self.prefs.toasts {
                self.panes[i].overlay.toast(crate::t!("toast.kbm_here", n = i + 1), crate::t!("toast.kbm_here_detail"));
            }
        }
        for inst in &mut self.instances {
            if inst.refresh_window() {
                self.panes_stale = true;
            }
        }
        if std::mem::take(&mut self.panes_stale) {
            if let Some(c) = self.compositor.as_mut() {
                c.sync_instances(&mut self.instances);
            }
            self.redraw_all();
            self.close_hidden_overlays();
        }
        if toggle_studio {
            if self.layout_studio.active {
                self.close_layout_studio();
            } else {
                self.open_layout_studio();
            }
        }
        let pending: Vec<u32> = self.pending_spawns.iter().copied().collect();
        if let Some(c) = self.compositor.as_mut() {
            c.maintain(&self.instances, &pending);
            // What changed in the panes, into each copy's log (a pane whose copy hasn't
            // started yet: player 1's).
            for (i, line) in c.take_notes() {
                if let Some(inst) = self.instances.get(i).or(self.instances.first()) {
                    crate::launch::note(inst.pid, &line);
                }
            }
        }
        self.forward_queued_input();
    }

    /// The keyboard and mouse messages the session's window got, to the focused copy's
    /// window (a click on another pane focuses it). Between ticks too: a game that moves
    /// its cursor by its window's mouse messages got them only every tick (33 ms), in
    /// bursts, and its cursor lagged and stepped behind the mouse.
    fn forward_queued_input(&mut self) {
        let overlay_open = self.panes[self.focus].visible || self.layout_studio.active;
        let can_click: Vec<bool> = (0..self.instances.len()).map(|i| self.click_can_focus(i)).collect();
        let clicked_focus = match self.compositor.as_mut() {
            Some(c) => c.forward_input(self.focus, overlay_open, self.kbm_off, &can_click, &self.instances),
            None => None,
        };
        if let Some(i) = clicked_focus.filter(|&i| i != self.focus && i < self.instances.len() && !self.instances[i].gone) {
            self.set_focus(i);
        }
    }

    /// An open overlay on a pane the session's window doesn't show now (another pane is
    /// zoomed) closes: left open, it would hold its player's controller and keep showing
    /// over the zoomed pane.
    fn close_hidden_overlays(&mut self) {
        let Some(c) = self.compositor.as_ref() else { return };
        let shown = c.display_panes();
        for i in 0..self.panes.len() {
            let hidden = shown.get(i).is_none_or(|p| p.width <= 100 || p.height <= 100);
            if hidden && self.panes[i].visible {
                self.instances[i].send(&HostMsg::Note { text: format!("overlay: closed: player {}'s pane isn't shown (another pane is zoomed)", i + 1) });
                self.hide(i);
            }
        }
    }

    /// The size of copy `i`'s overlay canvas over `r` (its pane on screen): the game's
    /// frames' when it's drawn inside them (the session's window shows them scaled into
    /// the pane, letterboxed), else the pane's.
    fn canvas_size(&self, i: usize, r: &Rect) -> (usize, usize) {
        let pane = (usize::try_from(r.right - r.left).unwrap_or(0), usize::try_from(r.bottom - r.top).unwrap_or(0));
        if !self.use_ingame_for(i) {
            return pane;
        }
        match self.instances[i].viewport.as_ref().filter(|v| v.width > 100 && v.height > 100) {
            Some(v) => (v.width as usize, v.height as usize),
            None => pane,
        }
    }

    /// The controller kind whose button pictures copy `i`'s overlay shows: its own
    /// player's controller in a split-screen session, else the one last used.
    fn pane_styles(&mut self) -> Vec<pad::Style> {
        let pads = self.input.pads();
        let last = pads.style;
        if self.compositor.is_none() || self.instances.len() <= 1 {
            return vec![last; self.panes.len()];
        }
        let seats = pads.seat_info();
        (0..self.panes.len()).map(|i| seats.get(i).and_then(|s| s.as_ref()).map_or(last, |s| s.style)).collect()
    }

    /// The buttons down when copy `i`'s overlay closed: once they're up, the game gets
    /// its controllers back. While it's open, the ones down when it opened: once they're
    /// up, the game's HID controllers are held at a report with them up.
    fn release_pads(&mut self, i: usize) {
        let Some(since) = self.panes[i].nav.pads_held else { return };
        if pads_let_go(self.pane_pad(i), since, Instant::now()) {
            self.panes[i].nav.pads_held = None;
            self.instances[i].send(&HostMsg::PadsHeld { on: false });
        }
    }

    /// A message from copy `i`'s hook.
    fn message(&mut self, i: usize, msg: HookMsg) {
        match msg {
            HookMsg::State(state) => self.state(i, &state),
            HookMsg::Note(note) => {
                let p = &mut self.panes[i];
                p.overlay.notice(&note, p.visible);
                p.render.dirty = true;
            }
            HookMsg::LocalSaves { service, .. } => {
                let p = &mut self.panes[i];
                p.overlay.note(crate::t!("toast.local_saves", service = service), crate::t!("toast.local_saves_detail").into(), p.visible);
                p.render.dirty = true;
            }
            HookMsg::Close => self.hide(i),
            HookMsg::Open { page, peer } => {
                let asked = format!("{page} {}", peer.as_deref().unwrap_or(""));
                let invite = page == "invite";
                // Anything else (the game's "settings", its overlay's home) opens where
                // the overlay opens by itself; its settings are the launcher window's.
                let landing = self.landing(i);
                let page = match page.as_str() {
                    "friends" | "invite" | "invites" => Page::Players,
                    "profile" => match peer {
                        Some(p) => Page::Profile(p),
                        None => Page::Players,
                    },
                    _ => landing,
                };
                // The game asked: its own copy's overlay opens, over its own pane. Asked
                // again for the same page while it's open (a game pressing its invite
                // button over and over), it stays where its player has gone on it.
                let again = self.panes[i].visible && self.panes[i].nav.game_asked.as_deref() == Some(asked.as_str());
                if self.prefs.overlay && !again {
                    self.show(i, page, invite, "the game");
                    if self.panes[i].visible {
                        self.panes[i].nav.game_asked = Some(asked);
                    }
                }
            }
            HookMsg::Log(_) => {}
            HookMsg::CursorLock { on } => self.instances[i].cursor_locked = on,
            HookMsg::CursorHide { hidden } => self.instances[i].cursor_hidden = hidden,
            HookMsg::CursorShape { handle } => self.instances[i].cursor_shape = Some(handle),
            HookMsg::Viewport { width, height, hwnd, api } => {
                let v = Viewport { width, height, hwnd, api };
                let inst = &mut self.instances[i];
                if inst.viewport.as_ref() != Some(&v) {
                    // A session's window shows the copy's window once it's known and
                    // re-fits its thumbnail when backbuffer dimensions change.
                    inst.set_viewport(v);
                    self.panes_stale = true;
                    self.panes[i].render.dirty = true;
                }
            }
            HookMsg::Surface(m) => {
                let s = Surface { generation: m.generation, width: m.width, height: m.height, format: m.format, api: m.api.clone() };
                // Its handles come out of the copy's process now, while they're still
                // open there (the hook keeps two sets back), or its pixel file is mapped.
                if let Some(c) = self.compositor.as_mut() {
                    let how = if m.pixels.is_some() { "pixels" } else { "shared textures" };
                    let what = format!("{}x{} {how} (format {}, colour space {}, {}, set {})", m.width, m.height, m.format, m.color_space, m.api, m.generation);
                    // Each new set in a burst isn't worth a line: the first few, then now and then.
                    let n = m.generation;
                    let line = if c.set_frames(i, &m, self.instances[i].ring.clone()) {
                        format!("session: player {}'s frames come as {what}: the window draws them", i + 1)
                    } else {
                        eprintln!("peerparty: session: player {}'s frames couldn't be shared with the window; its window is shown instead", i + 1);
                        format!("session: player {}'s frames ({what}) couldn't be opened by the window: its window's thumbnail is shown instead", i + 1)
                    };
                    if n <= 3 || n.is_power_of_two() {
                        crate::launch::note(self.instances[i].pid, &line);
                    }
                }
                self.instances[i].surface = Some(s);
            }
            HookMsg::SurfaceCleared => {
                if let Some(c) = self.compositor.as_mut() {
                    c.clear_frames(i);
                }
                self.instances[i].surface = None;
            }
        }
    }

    /// Copy `i`'s state: what its overlay shows, and notifications on its own screen for
    /// what's new since its last one.
    fn state(&mut self, i: usize, state: &peerparty_protocol::Json) {
        // A starter program handed over to the game it started (pipe.rs): the overlay
        // follows the process that's talking now.
        let pid = state.get("pid").as_u64().and_then(|p| u32::try_from(p).ok()).unwrap_or(0);
        let old_pid = self.instances[i].pid;
        let handoff = self.instances[i].follow_pid(pid);
        let connected = self.instances[i].first_state || handoff;
        if handoff {
            self.audio.follow_pid(old_pid, pid);
            self.send_settings(i);
            if i == self.focus {
                if let Some(exe) = crate::platform::process_exe(pid) {
                    self.firewall.set_exe(exe);
                }
            }
        }
        // Messages sent before the hook opened the pipe (during `run_session` start-up)
        // are dropped while no client is attached, so send the copy's session setup now
        // that its hook is talking.
        if connected && self.compositor.is_some() {
            let inst = &self.instances[i];
            inst.send_instance();
            inst.send_input_shm();
            inst.send_capture(self.compositor.as_ref().and_then(|c| c.frame_event()).as_deref());

            self.panes_stale = true;
        }
        let st = ui::GameState::from_json(&pipe::to_value(state));
        let focused = i == self.focus;
        let toasts = self.prefs.toasts;
        let hint = self.open_hint();
        let inst = &mut self.instances[i];
        let pane = &mut self.panes[i];
        let visible = pane.visible;
        inst.ingame = state.get("present").as_str().is_some_and(|s| !s.is_empty());
        inst.prefer_ingame = state.get("prefer_ingame").as_bool().unwrap_or(false);
        if inst.ingame {
            inst.unseen.reset();
        }
        // Notifications for what's new since the last state.
        if !inst.first_state && toasts && !visible {
            for p in &st.pending {
                if !inst.seen_pending.contains(&p.id) {
                    pane.overlay.toast(ui::invite_text(&p.name, &p.kind), crate::t!("toast.press_to_answer", keys = hint));
                }
            }
            for p in &st.peers {
                if !inst.seen_peers.contains(&p.id) {
                    if let Some(text) = ui::arrival_text(p) {
                        pane.overlay.toast(text, p.reach());
                    }
                }
            }
        }
        // Another copy of this game holds the PC's first player: this one is another
        // player (its own name, id and saves). Said once.
        if st.me_slot > 0 && !inst.slot_told && toasts {
            inst.slot_told = true;
            pane.overlay.toast(crate::t!("toast.later_slot", name = st.me), crate::t!("toast.later_slot_detail").to_string());
        }
        // Players of this game on another PeerParty, who never show up.
        if st.other_version > 0 && inst.other_version == 0 && !visible {
            pane.overlay.toast(ui::other_version_title(&st), ui::other_version_detail(&st));
        }
        inst.other_version = st.other_version;
        inst.seen_pending = st.pending.iter().map(|p| p.id).collect();
        inst.seen_peers = st.peers.iter().map(|p| p.id.clone()).collect();
        inst.first_state = false;
        pane.overlay.state = st.clone();
        pane.render.dirty = true;
        inst.state = st;
        if !focused {
            return;
        }
        // Players on our network we can't reach: say so once, when it starts.
        let unreachable = inst.state.lan_unreachable.unwrap_or(0);
        if unreachable > 0 && self.firewall.unreachable == 0 && !visible {
            pane.overlay.toast(crate::t!("overlay.unreachable", n = unreachable), crate::t!("toast.unreachable_detail"));
        }
        self.firewall.unreachable = unreachable;
        self.refresh_hints();
        // A hotkey left to a store's overlay: said once, with what opens ours.
        if let Some((key, store)) = self.key_left().filter(|_| !self.key_left_told) {
            self.key_left_told = true;
            self.cur().send(&HostMsg::Note { text: format!("overlay: {key} isn't used in this game: {store}'s overlay is loaded and opens on it too") });
            if self.prefs.toasts {
                let hint = self.open_hint();
                self.panes[i].overlay.toast(crate::t!("toast.key_left", key = key, store = store), crate::t!("toast.key_left_detail", keys = hint));
            }
        }
    }

    /// An opening hotkey or Home / Guide pressed while the overlay can't open: the game's
    /// log says why (another window in front, a console window such as a mod loader's
    /// among them, or Game Bar over the game), so a player's "it didn't open" has an
    /// answer.
    fn why_not_open(&mut self, front: bool, gamebar: bool) {
        let shift = down(VK_SHIFT);
        let mut pressed = None;
        for (i, (on, vk)) in self.hotkeys().iter().enumerate() {
            let d = down(*vk as i32);
            if *on && shift && d && !self.nav.diag_keys[i] {
                pressed = Some(HOTKEY_NAMES[i]);
            }
            self.nav.diag_keys[i] = d;
        }
        // Controllers are read every tick, so the log names them whether or not the
        // game is ever in front. Home with Game Bar over the game is Game Bar's (it
        // closes it).
        let home = self.pads.all & pad::HOME != 0;
        self.log_pads();
        if home && !self.nav.diag_home && !front && self.guide_opens() {
            pressed = pressed.or(Some("Home / Guide"));
        }
        self.nav.diag_home = home;
        let Some(key) = pressed else { return };
        let why = if !self.prefs.overlay {
            "the overlay is turned off for this game".to_string()
        } else if gamebar {
            "Game Bar looks open over the game (it has the keyboard and controller meanwhile)".to_string()
        } else if !front {
            let fg = desktop::foreground();
            let pid = fg.pid;
            let name = crate::platform::process_exe(pid)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| format!("process {pid}"));
            let console = if fg.console { ", a console window, like the one a mod loader opens: click the game to bring it to the front" } else { "" };
            format!("the game isn't the window in front ({name} is{console}; the game is process {})", self.cur().pid)
        } else {
            return;
        };
        self.cur().send(&HostMsg::Note { text: format!("overlay: {key} pressed but the overlay can't open: {why}") });
    }

    /// Into the game's log whenever the game stops being the window in front, with what
    /// took it (a Windows prompt, another program), and when it's back: a game that seems
    /// frozen or deaf to input may just not have the keyboard and controller.
    fn track_focus(&mut self, front: bool) {
        let inst = &mut self.instances[self.focus];
        if front {
            if let Some((since, _)) = inst.focus_away.take() {
                inst.send(&HostMsg::Note { text: format!("focus: the game is the window in front again, after {:.1} s", since.elapsed().as_secs_f32()) });
            }
            inst.focus_seen = true;
            return;
        }
        if !inst.focus_seen {
            return;
        }
        let fg = desktop::foreground();
        let pid = fg.pid;
        if inst.focus_away.is_some_and(|(_, p)| p == pid) {
            return;
        }
        let since = inst.focus_away.map_or_else(Instant::now, |(t, _)| t);
        inst.focus_away = Some((since, pid));
        let who = if fg.none {
            "no window (a Windows prompt, such as the administrator one, has the screen)".to_string()
        } else if pid == std::process::id() {
            "PeerParty's own window".to_string()
        } else {
            let name = crate::platform::process_exe(pid).and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| format!("process {pid}"));
            if fg.console { format!("{name}, a console window") } else { name }
        };
        inst.send(&HostMsg::Note { text: format!("focus: the game isn't the window in front: {who} is (many games ignore the keyboard and controller meanwhile)") });
    }

    /// The controllers the overlay reads, into the game's log whenever they change, so a
    /// button that doesn't open it shows whether its controller is read at all.
    /// "none" too, once the first scan is done: a log without the line can't tell no
    /// controller from controllers never read.
    fn log_pads(&mut self) {
        if !self.input.pads().scanned() {
            return;
        }
        let found = self.input.pads().found();
        if self.pads_logged.as_ref() != Some(&found) {
            let list = if found.is_empty() { "none".to_string() } else { found.join(", ") };
            self.cur().send(&HostMsg::Note { text: format!("overlay: controllers read: {list}") });
            self.pads_logged = Some(found);
        }
    }

    /// Opening hotkeys and buttons while the game is in front, for each overlay that's
    /// closed: the keyboard's hotkeys open the focused copy's, a controller's Home or
    /// chord the overlay of the copy it plays.
    fn check_open(&mut self) {
        self.nav.diag_keys = [true; 4];
        self.nav.diag_home = true;
        let shift = down(VK_SHIFT);
        let keys = self.hotkeys();
        // (copy, what opened it, Home / Guide did)
        let mut opens: Vec<(usize, String, bool)> = Vec::new();
        // Taken from the game by the keyboard hook (which keeps it from the key state
        // read below), or seen by polling where there's no hook.
        let hit = input::take_hotkey();
        for (k, (on, vk)) in keys.iter().enumerate() {
            let d = down(*vk as i32);
            if *on && ((shift && d && !self.nav.keys_down[k]) || hit == Some(*vk)) {
                opens.push((self.focus, HOTKEY_NAMES[k].to_string(), false));
            }
            self.nav.keys_down[k] = d;
        }
        let styles = self.pane_styles();
        for (p, style) in self.panes.iter_mut().zip(styles) {
            p.overlay.style = style;
        }
        self.log_pads();
        if self.guide_opens() {
            let split = self.compositor.is_some() && self.instances.len() > 1;
            let homes: Vec<(Option<usize>, String, bool)> =
                self.pads.homes.iter().map(|(k, name)| (*k, name.clone(), split && k.and_then(|k| self.instance_for_pad(k)).is_some())).collect();
            let (homes, twins) = real_homes(homes, &mut self.nav.given_home_at, Instant::now());
            for name in twins {
                self.cur().send(&HostMsg::Note {
                    text: format!("overlay: Home / Guide on {name} came with another player's: the same controller seen twice, so it opens only theirs"),
                });
            }
            for (k, name) in homes {
                opens.push((self.pane_for_pad(k), format!("Home / Guide on {name}"), true));
            }
        }
        let chord = self.prefs.hotkeys.chord;
        let chord_on = self.chord_opens();
        for i in 0..self.panes.len() {
            if self.instances[i].gone || self.panes[i].visible {
                continue;
            }
            let held = chord_on && self.pane_pad(i) & chord == chord;
            let nav = &mut self.panes[i].nav;
            match (held, nav.chord_since) {
                (true, None) => nav.chord_since = Some(Instant::now()),
                (true, Some(t)) if !nav.chord_fired && t.elapsed() >= CHORD_HOLD => {
                    nav.chord_fired = true;
                    opens.push((i, format!("{} held", pad::names(chord)), false));
                }
                (false, _) => {
                    nav.chord_since = None;
                    nav.chord_fired = false;
                }
                _ => {}
            }
        }
        for (i, why, home) in opens {
            // Already open (Home only opens it), or opened by another way this tick.
            if self.panes[i].visible {
                continue;
            }
            if home && !self.prefs.guide_tip_shown {
                self.panes[i].overlay.tip = Some(crate::t!("toast.gamebar_tip").into());
                self.prefs.guide_tip_shown = true;
                self.save_prefs();
            }
            let page = self.landing(i);
            self.show(i, page, false, &why);
        }
    }

    /// Game Bar's overlay opened or closed over the game. While it's open the keyboard,
    /// mouse and controller go to it; the overlays stay open underneath (also when the
    /// same Guide press opened both) and take them back once Game Bar closes.
    fn gamebar_changed(&mut self, open: bool) {
        let text = if open { "overlay: Game Bar opened over the game; input goes to it" } else { "overlay: Game Bar closed" };
        self.cur().send(&HostMsg::Note { text: text.into() });
        if !self.any_open() {
            return;
        }
        self.update_capture();
        if !open {
            for i in 0..self.panes.len() {
                if self.panes[i].visible {
                    let pad = self.pane_pad(i);
                    let p = &mut self.panes[i];
                    p.nav.pad_prev = pad;
                    p.render.dirty = true;
                }
            }
        }
    }

    /// Where copy `i`'s overlay opens by itself (`Overlay::landing`), with its session's
    /// players known first.
    fn landing(&mut self, i: usize) -> Page {
        let seats = self.seats(i);
        self.panes[i].overlay.seats = seats;
        self.panes[i].overlay.landing()
    }

    /// Settings changed in the launcher window while the game runs (ui.json, looked at
    /// about once a second): the overlay showing, its size and button pictures, what
    /// opens it, notifications and accepting invites apply at once. Only what changed in
    /// the file is taken, so a `--set` or environment answer for this run stays until
    /// that very setting is changed.
    fn reload_prefs(&mut self) {
        if self.file_checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.file_checked = Instant::now();
        let m = crate::ui::store::Settings::modified();
        if m.is_none() || m == self.file_seen {
            return;
        }
        let loaded = crate::ui::store::Settings::load();
        if loaded.unread {
            return;
        }
        self.file_seen = m;
        let now = loaded.prefs.effective_for(&loaded.options(&self.key));
        let was = std::mem::replace(&mut self.file_prefs, now.clone());
        let mut p = self.prefs.clone();
        let mut what = Vec::new();
        macro_rules! take {
            ($($f:ident),*) => {$(
                if now.$f != was.$f && p.$f != now.$f {
                    p.$f = now.$f.clone();
                    what.push(stringify!($f));
                }
            )*};
        }
        take!(overlay, overlay_scale, glyphs, hotkeys, toasts, auto_accept);
        if what.is_empty() {
            return;
        }
        self.cur().send(&HostMsg::Note { text: format!("overlay: settings changed in the PeerParty window, applied now: {}", what.join(", ")) });
        let accept = p.auto_accept != self.prefs.auto_accept;
        self.prefs = p;
        if accept {
            for k in 0..self.instances.len() {
                self.send_settings(k);
            }
        }
        // The footers name the hotkeys; the size and pictures show at once.
        self.refresh_hints();
        self.redraw_all();
    }

    /// Opens copy `i`'s overlay on `page`. `why` goes into the game's log: what opened it.
    fn show(&mut self, i: usize, page: Page, invite_mode: bool, why: &str) {
        if !self.in_front() || self.instances[i].gone {
            return;
        }
        self.panes[i].overlay.open(page, invite_mode);
        if self.panes[i].visible {
            self.panes[i].render.dirty = true;
            return;
        }
        let Some(r) = self.pane_rect(i) else {
            let pid = self.instances[i].pid;
            self.instances[i].send(&HostMsg::Note { text: format!("overlay: {why} can't open it: no window of the game's (process {pid}) to draw over") });
            return;
        };
        let pad = self.pane_pad(i);
        // The scale the first frame will have (`frame_open`), not the last one's: the
        // pointer starts in the middle of the panel.
        let (w, h) = self.canvas_size(i, &r);
        let ppp = scale_for(h as f32, self.prefs.overlay_scale);
        let p = &mut self.panes[i];
        p.visible = true;
        p.render.dirty = true;
        p.render.render.invalidate();
        p.nav.cursor = egui::Pos2::new(w as f32 / 2.0 / ppp, h as f32 / 2.0 / ppp);
        p.nav.pointer = false;
        p.nav.click_shown = None;
        // Buttons held while opening don't act on the first frame.
        p.nav.pad_prev = pad;
        p.nav.repeat.reset();
        // Opening ends a hold from the last close (in the hook too). Buttons down now
        // (the game's invite button, pressed to ask for it) are waited for instead: the
        // game's HID controllers are held at a report with them up.
        let held = pad & PAD_BUTTONS != 0;
        p.nav.pads_held = held.then(Instant::now);
        p.nav.game_asked = None;
        p.render.hide_toast_window();
        if i == self.focus {
            self.nav.shift = down(VK_SHIFT);
            self.nav.esc_down = down(VK_ESCAPE);
            self.nav.close_keys_down = [down(VK_TAB as i32), down(VK_F3 as i32), down(VK_F1 as i32), down(VK_F2 as i32)];
        }
        self.update_capture();
        self.instances[i].send(&HostMsg::Overlay { visible: true, why: Some(why.to_string()), pads_held: held });
    }

    /// Closes copy `i`'s overlay.
    fn hide(&mut self, i: usize) {
        if !self.panes[i].visible {
            return;
        }
        let pad = self.pane_pad(i);
        let p = &mut self.panes[i];
        p.visible = false;
        // Buttons still held when it closed don't reach the game as a fresh press
        // (`PAD_BUTTONS`).
        p.nav.pad_prev = pad;
        let held = pad & PAD_BUTTONS != 0;
        p.nav.pads_held = held.then(Instant::now);
        p.render.hide_window();
        p.overlay.tip = None;
        // Keys still held when it closed don't reopen it.
        self.nav.keys_down = [true; 4];
        let _ = input::take_hotkey();
        self.update_capture();
        let inst = &mut self.instances[i];
        inst.unseen.reset();
        inst.send(&HostMsg::Overlay { visible: false, why: None, pads_held: held });
        if let Some(f) = inst.frame.as_mut() {
            f.hide();
        }
    }

    fn save_prefs(&mut self) {
        self.saver.save(&self.key, &self.prefs);
    }

    /// The overlay's settings the hook in copy `i` acts on.
    fn send_settings(&self, i: usize) {
        let inst = &self.instances[i];
        inst.send(&HostMsg::Set { key: "auto_accept".into(), value: if self.prefs.auto_accept { "auto" } else { "ask" }.into() });
    }

    /// Navigation for copy `i`'s open overlay: from the keyboard (through the hooks) and
    /// mouse while it's the focused copy's, and from its player's controller, with
    /// repeat for held directions. Also closes on a hotkey.
    fn gather(&mut self, i: usize) -> (Nav, Vec<egui::Event>, bool) {
        let mut nav = Nav::default();
        let mut events = Vec::new();
        let mut moved = false;
        let screen = self.panes[i].render.screen_points();
        let ppp = self.panes[i].render.ppp;
        let hotkeys = self.hotkeys();
        // The PC's keyboard and mouse are the focused copy's (another copy's input was
        // never taken: `update_capture`).
        let taken = if i == self.focus { input::take() } else { Vec::new() };
        let shift = &mut self.nav.shift;
        let n = &mut self.panes[i].nav;
        let mut hook_esc = false;
        let mut hook_close = [false; 4];
        for ev in taken {
            match ev {
                input::Input::Key { vk, down: is_down } => {
                    if vk == 0x10 || vk == 0xA0 || vk == 0xA1 {
                        *shift = is_down;
                        continue;
                    }
                    if !is_down {
                        continue;
                    }
                    let shift_now = *shift || down(VK_SHIFT);
                    if shift_now && !down(VK_MENU) {
                        if let Some(k) = hotkeys.iter().position(|(on, k)| *on && *k == vk) {
                            nav.close = true;
                            hook_close[k] = true;
                            continue;
                        }
                    }
                    match vk {
                        0x26 => nav.up = true,
                        0x28 => nav.down = true,
                        0x25 => nav.left = true,
                        0x27 => nav.right = true,
                        0x0D | 0x20 => nav.select = true,
                        0x2E | 0x58 => nav.second = true,
                        0x59 => nav.third = true,
                        0x1B => {
                            nav.back = true;
                            hook_esc = true;
                        }
                        0x08 => nav.back = true,
                        0x51 | 0x21 => nav.prev_tab = true,
                        0x45 | 0x22 => nav.next_tab = true,
                        0x09 if shift_now => nav.prev_tab = true,
                        0x09 => nav.next_tab = true,
                        _ => {}
                    }
                }
                input::Input::Move { dx, dy } => {
                    n.cursor.x = (n.cursor.x + dx as f32 / ppp).clamp(0.0, screen.x - 1.0);
                    n.cursor.y = (n.cursor.y + dy as f32 / ppp).clamp(0.0, screen.y - 1.0);
                    moved = true;
                    // A click after a move acts: the pointer shows from the move on.
                    n.pointer = true;
                }
                input::Input::Button { left, down } => {
                    // A click while the pointer is hidden only shows it: the player
                    // hasn't seen where it is, so it doesn't act there.
                    if !n.pointer && down {
                        n.pointer = true;
                        n.click_shown = Some(left);
                        events.push(egui::Event::PointerMoved(n.cursor));
                        continue;
                    }
                    if !down && n.click_shown == Some(left) {
                        n.click_shown = None;
                        continue;
                    }
                    if !n.pointer {
                        n.pointer = true;
                        events.push(egui::Event::PointerMoved(n.cursor));
                    }
                    events.push(egui::Event::PointerButton {
                        pos: n.cursor,
                        button: if left { egui::PointerButton::Primary } else { egui::PointerButton::Secondary },
                        pressed: down,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                input::Input::Wheel(d) => events.push(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, d),
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Unknown,
                    modifiers: egui::Modifiers::NONE,
                }),
            }
        }
        if moved {
            n.pointer = true;
            events.insert(0, egui::Event::PointerMoved(n.cursor));
        }
        if i == self.focus {
            let keys_now = [down(VK_TAB as i32), down(VK_F3 as i32), down(VK_F1 as i32), down(VK_F2 as i32)];
            let (poll_back, poll_close) = poll_dismiss_keys(
                hotkeys,
                down(VK_SHIFT),
                down(VK_MENU),
                down(VK_ESCAPE),
                keys_now,
                &mut self.nav.esc_down,
                &mut self.nav.close_keys_down,
            );
            nav.back |= poll_back;
            nav.close |= poll_close;
            if hook_esc {
                self.nav.esc_down = true;
            }
            for k in 0..4 {
                if hook_close[k] {
                    self.nav.close_keys_down[k] = true;
                }
            }
        }
        let pad = self.pane_pad(i);
        if let Some(&style) = self.pane_styles().get(i) {
            self.panes[i].overlay.style = style;
        }
        let n = &mut self.panes[i].nav;
        let pressed = pad & !n.pad_prev;
        n.pad_prev = pad;
        // Held directions repeat after 400 ms, every 110 ms.
        let dirs = pad & (pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT);
        let fire = pressed | n.repeat.step(dirs, Instant::now());
        nav.up |= fire & pad::UP != 0;
        nav.down |= fire & pad::DOWN != 0;
        nav.left |= fire & pad::LEFT != 0;
        nav.right |= fire & pad::RIGHT != 0;
        nav.select |= pressed & pad::SOUTH != 0;
        nav.second |= pressed & pad::WEST != 0;
        nav.third |= pressed & pad::NORTH != 0;
        nav.back |= pressed & pad::EAST != 0;
        nav.prev_tab |= pressed & pad::LB != 0;
        nav.next_tab |= pressed & pad::RB != 0;
        // Home only opens the overlay (B goes back and closes, Start closes).
        nav.close |= pressed & pad::START != 0;
        // Controller or keyboard navigation hides the pointer (and its hover) until the
        // mouse moves again.
        if n.pointer && !moved && nav.any() {
            n.pointer = false;
            events.push(egui::Event::PointerGone);
        }
        (nav, events, moved)
    }

    /// Copy `i`'s open overlay: its input, and a frame when something changed.
    fn frame_open(&mut self, i: usize) {
        self.panes[i].render.fonts();
        let Some(r) = self.pane_rect(i) else { return };
        let pending = i == self.focus && input::pending();
        if pending && self.panes[i].render.last_frame.elapsed() < FRAME_GAP {
            // Soon: the loop wakes again when the gap is over.
            return;
        }
        let (w, h) = self.canvas_size(i, &r);
        let scale = scale_for(h as f32, self.prefs.overlay_scale);
        {
            let canvas = &self.panes[i].render.render.canvas;
            let pane = (usize::try_from(r.right - r.left).unwrap_or(0), usize::try_from(r.bottom - r.top).unwrap_or(0));
            if (canvas.w, canvas.h) != (w, h) && (w, h) != pane {
                self.instances[i].send(&HostMsg::Note { text: format!("overlay: drawn inside the game's {w}x{h} frames (its pane on screen is {}x{})", pane.0, pane.1) });
            }
            let p = &mut self.panes[i];
            p.render.fit(w, h, scale);
            if p.overlay.notice.as_ref().is_some_and(|(_, until)| *until <= Instant::now()) {
                p.overlay.notice = None;
                p.render.dirty = true;
            }
        }
        let seats = self.seats(i);
        if self.panes[i].overlay.seats != seats {
            self.panes[i].overlay.seats = seats;
            self.panes[i].render.dirty = true;
        }
        let (nav, events, moved) = self.gather(i);
        let mut cmds = Vec::new();
        let mut prefs = self.prefs.clone();
        let started = self.started;
        let p = &mut self.panes[i];
        if nav.any() || !events.is_empty() {
            p.render.dirty = true;
        }
        let pointer_changed = moved || p.nav.pointer != p.render.render.pointer_shown();
        if !p.render.dirty && !pointer_changed {
            return;
        }
        p.render.last_frame = Instant::now();
        let mut rects: Vec<[i32; 4]> = Vec::new();
        // The pointer comes off first: what's under it is the frame without it.
        p.render.render.pointer_off(&mut rects);
        let PaneOverlay { overlay, render: draw, nav: pnav, .. } = p;
        if draw.dirty {
            draw.dirty = false;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, draw.screen_points())),
                time: Some(started.elapsed().as_secs_f64()),
                events,
                ..Default::default()
            };
            draw.ctx.set_pixels_per_point(draw.ppp);
            overlay.paints_background = false;
            let out = draw.ctx.run_ui(input, |ui| overlay.frame(ui, nav, &mut prefs, &mut cmds));
            if out.viewport_output.values().any(|v| v.repaint_delay < Duration::from_millis(100)) {
                draw.dirty = true;
            }
            draw.render.draw(&draw.ctx, out, |screen| ui::background(ui::panel_rect(screen)), &mut rects);
        }
        if pnav.pointer {
            draw.render.pointer_on(&draw.ctx, pnav.cursor, draw.ppp, &mut rects);
        }
        if !rects.is_empty() {
            let ingame = self.use_ingame_for(i);
            let p = &mut self.panes[i];
            p.render.present(&mut self.instances[i], &p.overlay, ingame, r.left, r.top, &rects);
        }
        let prefs_changed = prefs != self.prefs;
        self.prefs = prefs;
        if prefs_changed {
            self.save_prefs();
            for k in 0..self.instances.len() {
                self.send_settings(k);
            }
            // The footers name the hotkeys.
            self.refresh_hints();
            self.redraw_all();
        }
        for c in cmds {
            match c {
                // Internet play is the session's, not one player's: every copy switches.
                Cmd::Hook(m @ HostMsg::Set { .. }) if matches!(&m, HostMsg::Set { key, .. } if key == "internet") => {
                    for inst in self.instances.iter().filter(|c| !c.gone) {
                        inst.send(&m);
                    }
                }
                Cmd::Hook(m) => self.instances[i].send(&m),
                Cmd::Close => self.hide(i),
                Cmd::Prefs => {}
                Cmd::FixFirewall(_) if self.firewall.fixing() => {}
                Cmd::FixFirewall(public) => {
                    if desktop::exclusive_fullscreen() {
                        // Windows' permission prompt would hide behind the game.
                        let p = &mut self.panes[i];
                        p.overlay.notice = Some((crate::t!("toast.fullscreen_fix").into(), Instant::now() + Duration::from_secs(10)));
                        p.render.dirty = true;
                    } else {
                        self.hide(i);
                        self.firewall.fix(public);
                    }
                }
                Cmd::SwapSeat(k) => self.swap_seat(k, i),
                Cmd::FreeSeat(k) => self.free_seat(k, i),
                Cmd::KeyboardStep(d) => self.keyboard_step(d),
                Cmd::OpenLayoutStudio => self.open_layout_studio(),
            }
        }
    }

    /// Checks Windows Firewall for the game now and then (only while PeerParty's engine
    /// runs in it), and picks up a fix's result.
    fn firewall_tick(&mut self) {
        let f = self.focus;
        match self.firewall.tick(self.panes[f].overlay.state.lan_unreachable.is_some()) {
            Some(FirewallEvent::Fixed { message, ok }) => {
                let p = &mut self.panes[f];
                if p.visible {
                    p.overlay.notice = Some((message, Instant::now() + Duration::from_secs(10)));
                } else {
                    p.overlay.toast(if ok { crate::t!("toast.firewall_fixed") } else { crate::t!("toast.firewall") }, message);
                }
                p.render.dirty = true;
            }
            Some(FirewallEvent::Checked { verdict, newly_blocked }) => {
                let fw = firewall_row(&verdict);
                if newly_blocked && !self.panes[f].visible {
                    let hint = self.open_hint();
                    let title = fw.as_ref().map(|f| f.title.clone()).unwrap_or_default();
                    self.panes[f].overlay.toast(title, crate::t!("toast.firewall_blocks", keys = hint));
                }
                for p in &mut self.panes {
                    if p.overlay.firewall != fw {
                        p.overlay.firewall = fw.clone();
                        p.render.dirty = true;
                    }
                }
            }
            None => {}
        }
    }

    /// Whether to draw inside copy `i`'s frames (`draws_inside`).
    fn use_ingame_for(&self, i: usize) -> bool {
        let c = &self.instances[i];
        draws_inside(desktop::DRAWS_OVER_GAMES, c.prefer_ingame, &self.prefs.draw, c.ingame, desktop::exclusive_fullscreen)
    }

    /// Whether to draw inside the focused copy's frames (`draws_inside`).
    #[cfg_attr(not(test), allow(dead_code))]
    fn use_ingame(&self) -> bool {
        self.use_ingame_for(self.focus)
    }

    /// Each copy's corner notifications, over its own pane, while its overlay is closed.
    fn frame_toasts(&mut self) {
        let front = self.in_front();
        let time = self.started.elapsed().as_secs_f64();
        let scale = self.prefs.overlay_scale;
        for i in 0..self.panes.len() {
            if self.panes[i].visible {
                continue;
            }
            if self.instances[i].gone || !self.panes[i].overlay.has_toasts() {
                self.panes[i].render.hide_toast_window();
                self.panes[i].toasts_held = None;
                if let Some(f) = self.instances[i].frame.as_mut() {
                    f.hide();
                }
                continue;
            }
            // A toast in a window of its own is topmost: while another program is in
            // front it would sit over that program, so it waits (one drawn inside the
            // game goes wherever the game goes), and the time away doesn't count against
            // it (the last three are kept).
            let ingame = self.use_ingame_for(i);
            if !(ingame && self.instances[i].frame.is_some()) && !front {
                let p = &mut self.panes[i];
                p.render.hide_toast_window();
                let now = Instant::now();
                if let Some(last) = p.toasts_held.replace(now) {
                    p.overlay.delay_toasts(now - last);
                }
                continue;
            }
            self.panes[i].toasts_held = None;
            let Some(r) = self.pane_rect(i) else {
                self.panes[i].render.hide_toast_window();
                continue;
            };
            let frames = self.instances[i].viewport.as_ref().filter(|v| ingame && v.width > 100 && v.height > 100).map(|v| (v.width, v.height));
            let p = &mut self.panes[i];
            p.render.fonts();
            p.render.toasts(&mut self.instances[i], &mut p.overlay, ingame, r, frames, scale, time);
        }
    }
}

/// Whether to draw the overlay inside the game's frames rather than in a window over
/// them: always where no window shows over a game (`over_games` false: Gamescope) or the
/// hook would rather (`prefer_ingame`: Gamescope, `PEERPARTY_DRAW=ingame`); else as the
/// player set it (`draw`), automatically when the hook already draws there or the game
/// is in exclusive fullscreen, where a window over it can't show. (Whether the hook can
/// draw there isn't known until it has: `UNSEEN_FOR` in instance.rs.)
fn draws_inside(over_games: bool, prefer_ingame: bool, draw: &str, hook_draws: bool, exclusive: impl FnOnce() -> bool) -> bool {
    if !over_games || prefer_ingame {
        return true;
    }
    match draw {
        "window" => false,
        "ingame" => true,
        _ => hook_draws || exclusive(),
    }
}

/// A readable name for the game: its folder's, unless that's a generic one.
fn game_title(exe: &Path, stem: &str) -> String {
    let generic = ["bin", "binaries", "win64", "x64", "game", "content", "wingdk", "shipping"];
    for dir in exe.ancestors().skip(1).take(4) {
        let n = dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if !n.is_empty() && !generic.contains(&n.to_ascii_lowercase().as_str()) && !n.ends_with(':') {
            return n;
        }
    }
    stem.to_string()
}

/// `peerparty --pads`: every controller PeerParty reads for the overlay, and its buttons
/// as they change, for `seconds` (to see why a button doesn't open the overlay).
pub fn print_pads(seconds: u64) {
    let mut pads = pad::Pads::new();
    pads.poll();
    std::thread::sleep(Duration::from_millis(600));
    let start = Instant::now();
    let mut last: Vec<(String, u32)> = Vec::new();
    println!("Controllers peerparty reads for the overlay (press buttons; {seconds} s):");
    while start.elapsed() < Duration::from_secs(seconds) {
        pads.poll();
        let now = pads.describe();
        for (name, b) in &now {
            if !last.iter().any(|(n, _)| n == name) {
                println!("  found  {name}");
            }
            if last.iter().find(|(n, _)| n == name).map(|(_, x)| *x) != Some(*b) && *b != 0 {
                println!("  {:>5.1}s {name}: {}", start.elapsed().as_secs_f32(), pad::names(*b));
            }
        }
        for (name, _) in &last {
            if !now.iter().any(|(n, _)| n == name) {
                println!("  gone   {name}");
            }
        }
        last = now;
        std::thread::sleep(Duration::from_millis(30));
    }
    if last.is_empty() {
        println!("  none: no controller peerparty can read (hidden by HidHide, or none connected)");
    }
}

fn down(vk: i32) -> bool {
    desktop::key_down(vk)
}

/// Ctrl+Alt for the session's keys, by `down` (each side's key: VK_LCONTROL ... VK_RMENU):
/// left Alt with either Ctrl, or right Alt with right Ctrl. AltGr is right Alt with a
/// left Ctrl Windows adds itself, and types characters on many keyboards (AltGr+Z is a
/// letter on a Polish one): it isn't the chord.
pub(crate) fn ctrl_alt(down: impl Fn(i32) -> bool) -> bool {
    let (lctrl, rctrl, lalt, ralt) = (down(0xA2), down(0xA3), down(0xA4), down(0xA5));
    (lalt && (lctrl || rctrl)) || (ralt && rctrl)
}

/// Whether Home / Guide opens the overlay.
pub(super) fn guide_opens(prefs: &Prefs) -> bool {
    prefs.hotkeys.guide
}

/// Whether a recorded chord (two buttons or more) opens the overlay.
pub(super) fn chord_opens(prefs: &Prefs) -> bool {
    prefs.hotkeys.chord.count_ones() >= 2
}

/// The Internet page's firewall row for a check's result.
fn firewall_row(v: &firewall::Verdict) -> Option<ui::Firewall> {
    use crate::t;
    let (title, detail) = if v.block_all {
        (t!("overlay.fw_all").to_string(), t!("overlay.fw_all_detail").to_string())
    } else if v.blocked() {
        let title = if v.blocks.iter().all(|r| r.policy) { t!("overlay.fw_blocks_policy", exe = v.exe) } else { t!("overlay.fw_blocks", exe = v.exe) };
        (title, t!("overlay.fw_blocks_detail").to_string())
    } else if !v.allowed {
        (t!("overlay.fw_not_allowed", exe = v.exe), t!("overlay.fw_not_allowed_detail").to_string())
    } else {
        return None;
    };
    let detail = if v.public { t!("overlay.fw_public", detail = detail) } else { detail };
    Some(ui::Firewall { title, detail })
}

/// Whether the game can have its controllers back after the overlay closed with buttons
/// down at `since`: the buttons are up (the d-pad and stick don't count), or it's been
/// long enough.
fn pads_let_go(pad: u32, since: Instant, now: Instant) -> bool {
    pad & PAD_BUTTONS == 0 || now.duration_since(since) >= PADS_HELD_MAX
}

#[cfg(test)]
mod tests {
    /// Ctrl+Alt from either Ctrl with left Alt, or both right keys; AltGr (right Alt with
    /// Windows' own left Ctrl) isn't it.
    #[test]
    fn altgr_is_not_ctrl_alt() {
        let held = |keys: &'static [i32]| move |vk: i32| keys.contains(&vk);
        assert!(super::ctrl_alt(held(&[0xA2, 0xA4])), "left Ctrl + left Alt");
        assert!(super::ctrl_alt(held(&[0xA3, 0xA4])), "right Ctrl + left Alt");
        assert!(super::ctrl_alt(held(&[0xA3, 0xA5])), "right Ctrl + right Alt");
        assert!(!super::ctrl_alt(held(&[0xA2, 0xA5])), "AltGr");
        assert!(!super::ctrl_alt(held(&[0xA4])), "Alt alone");
        assert!(!super::ctrl_alt(held(&[0xA2])), "Ctrl alone");
    }

    use super::*;
    use crate::firewall::{Rule, Verdict};
    use peerparty_protocol::Json;

    /// The Start or B that closed the overlay holds the game's controllers back until
    /// it's up; a stick or d-pad push doesn't, and nothing holds them for long.
    #[test]
    fn a_button_that_closed_the_overlay_is_held_until_up() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert!(!pads_let_go(pad::START, t0, at(100)));
        assert!(!pads_let_go(pad::EAST | pad::LEFT, t0, at(900)));
        assert!(pads_let_go(0, t0, at(16)), "up");
        assert!(pads_let_go(pad::UP | pad::RIGHT, t0, at(16)), "the d-pad and stick don't count");
        assert!(pads_let_go(pad::START, t0, at(1000)), "held at most PADS_HELD_MAX");
        for b in [pad::SOUTH, pad::EAST, pad::WEST, pad::NORTH, pad::LB, pad::RB, pad::BACK, pad::START, pad::HOME] {
            assert!(b & PAD_BUTTONS != 0);
        }
    }

    /// One press of a player's PS button, seen on the pad and on the Xbox pad a remapper
    /// makes of it: only the player's own overlay opens, also when the twin comes a tick
    /// late. A spare controller's Home on its own still opens one.
    #[test]
    fn a_controller_seen_twice_opens_one_overlay() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut given = None;
        let ps = (Some(0), "HID 054c:0ce6".to_string(), true);
        let twin = (Some(2), "XInput slot 0".to_string(), false);
        let (keep, twins) = real_homes(vec![ps.clone(), twin.clone()], &mut given, t0);
        assert_eq!(keep, [(Some(0), "HID 054c:0ce6".to_string())]);
        assert_eq!(twins, ["XInput slot 0"]);
        let (keep, _) = real_homes(vec![ps], &mut given, at(2000));
        assert_eq!(keep.len(), 1);
        let (keep, twins) = real_homes(vec![twin.clone()], &mut given, at(2016));
        assert!(keep.is_empty() && twins.len() == 1, "a tick late");
        let (keep, twins) = real_homes(vec![twin], &mut given, at(5000));
        assert!(keep.len() == 1 && twins.is_empty(), "a spare controller's own press");
    }

    fn block(policy: bool) -> Rule {
        Rule { id: "x".into(), name: "game".into(), inbound: true, active: true, policy, ..Default::default() }
    }

    /// Shift+Tab is Steam's overlay key too: with Steam's overlay in the game it's left
    /// to Steam, unless nothing else would open PeerParty's. A store not loaded keeps
    /// its key ours.
    #[test]
    fn a_store_overlays_key_is_left_to_it() {
        let steam = vec!["steam".to_string()];
        let on = [true, true, false, false];
        assert_eq!(usable_hotkeys(on, &[], false), on);
        assert_eq!(usable_hotkeys(on, &steam, false), [false, true, false, false]);
        assert_eq!(usable_hotkeys(on, &["epic".to_string(), "ea".to_string()], false), [true, false, false, false]);
        // Only Shift+Tab on: with a controller button it goes; with nothing else it stays.
        let only = [true, false, false, false];
        assert_eq!(usable_hotkeys(only, &steam, true), [false; 4]);
        assert_eq!(usable_hotkeys(only, &steam, false), only);
        let state = ui::GameState::from_json(&serde_json::json!({"t": "state", "foreign": ["steam", "epic"]}));
        assert_eq!(state.foreign, ["steam", "epic"]);
    }

    /// Esc and active Shift+<hotkey> dismiss the overlay on a rising edge even without
    /// the low-level keyboard hook, without firing while the opening key is still held
    /// or on Alt+Shift+Tab.
    #[test]
    fn dismiss_keys_fire_on_rising_edge_without_hook() {
        let hk = [(true, VK_TAB), (true, VK_F3), (false, VK_F1), (false, VK_F2)];
        // Opened by Shift+Tab (still held): doesn't close immediately.
        let mut esc_prev = false;
        let mut keys_prev = [true, false, false, false];
        assert_eq!(poll_dismiss_keys(hk, true, false, false, [true, false, false, false], &mut esc_prev, &mut keys_prev), (false, false));
        // Tab released while Shift stays held, then Tab pressed again -> closes.
        assert_eq!(poll_dismiss_keys(hk, true, false, false, [false; 4], &mut esc_prev, &mut keys_prev), (false, false));
        assert_eq!(poll_dismiss_keys(hk, true, false, false, [true, false, false, false], &mut esc_prev, &mut keys_prev), (false, true));
        // Alt+Shift+Tab does not close.
        assert_eq!(poll_dismiss_keys(hk, true, true, false, [false; 4], &mut esc_prev, &mut keys_prev), (false, false));
        assert_eq!(poll_dismiss_keys(hk, true, true, false, [true, false, false, false], &mut esc_prev, &mut keys_prev), (false, false));
        // Esc rising edge triggers back once, not while held.
        assert_eq!(poll_dismiss_keys(hk, false, false, true, [false; 4], &mut esc_prev, &mut keys_prev), (true, false));
        assert_eq!(poll_dismiss_keys(hk, false, false, true, [false; 4], &mut esc_prev, &mut keys_prev), (false, false));
    }

    /// What the Internet page's firewall row says for each kind of check result.
    #[test]
    fn firewall_rows_name_the_problem() {
        let exe = "game.exe".to_string();
        assert_eq!(firewall_row(&Verdict { exe: exe.clone(), allowed: true, ..Default::default() }), None);
        let r = firewall_row(&Verdict { exe: exe.clone(), blocks: vec![block(false)], ..Default::default() }).unwrap();
        assert_eq!(r.title, "Windows Firewall blocks game.exe");
        assert!(!r.detail.contains("public network"));
        let r = firewall_row(&Verdict { exe: exe.clone(), blocks: vec![block(true)], public: true, ..Default::default() }).unwrap();
        assert_eq!(r.title, "Windows Firewall blocks game.exe (your organisation's policy)");
        assert!(r.detail.contains("public networks too"), "{}", r.detail);
        let r = firewall_row(&Verdict { exe: exe.clone(), block_all: true, ..Default::default() }).unwrap();
        assert_eq!(r.title, "Windows Firewall blocks all incoming connections");
        let r = firewall_row(&Verdict { exe, ..Default::default() }).unwrap();
        assert_eq!(r.title, "game.exe isn't allowed through Windows Firewall yet");
    }

    /// A hook that would rather draw inside the frames (Gamescope, `PEERPARTY_DRAW`)
    /// always gets it, even over the player's "window"; otherwise the setting decides,
    /// automatically by whether the hook draws there or the game is exclusive.
    #[test]
    fn where_the_overlay_is_drawn() {
        let never = || panic!("not asked");
        assert!(draws_inside(false, false, "window", false, never), "nothing shows over a game");
        assert!(draws_inside(true, true, "window", false, never), "the hook would rather");
        assert!(draws_inside(true, true, "auto", false, never));
        assert!(!draws_inside(true, false, "window", true, never));
        assert!(draws_inside(true, false, "ingame", false, never));
        assert!(draws_inside(true, false, "auto", true, never), "the hook draws there already");
        assert!(draws_inside(true, false, "auto", false, || true), "exclusive fullscreen");
        assert!(!draws_inside(true, false, "auto", false, || false));
    }

    fn session(n: u32) -> SessionHost {
        let copies = (0..n).map(|i| InstanceConn::detached(InstanceSpec::solo(InstanceId(i), i))).collect();
        SessionHost::new(copies, None, Path::new(r"C:\Games\Some Game\bin\game.exe"), Prefs::default(), false)
    }

    /// The hook's new reports land on the copy that sent them: its frames' size, window
    /// and API, its shared surface, and whether it would rather draw inside its frames.
    #[test]
    fn viewport_surface_and_prefer_ingame_reach_their_copy() {
        let mut host = session(2);
        assert_eq!(host.panes[1].overlay.game_name, "Some Game");
        host.message(1, HookMsg::Viewport { width: 3840, height: 2160, hwnd: 0x10a2e, api: "d3d12".into() });
        assert_eq!(host.instances[1].viewport, Some(Viewport { width: 3840, height: 2160, hwnd: 0x10a2e, api: "d3d12".into() }));
        assert_eq!(host.instances[1].screen_size(), Some((3840, 2160)));
        assert!(host.instances[0].viewport.is_none(), "the other copy's own");
        let m = peerparty_protocol::SurfaceMsg { pid: 0, generation: 2, textures: vec![4, 8, 12], content: 16, release: 20, width: 1920, height: 1080, format: 90, api: "d3d11".into(), color_space: 0, pixels: None, stride: 0 };
        host.message(1, HookMsg::Surface(m));
        assert_eq!(host.instances[1].surface, Some(Surface { generation: 2, width: 1920, height: 1080, format: 90, api: "d3d11".into() }));
        host.message(1, HookMsg::SurfaceCleared);
        assert_eq!(host.instances[1].surface, None);
        let state = |s: &str| Json::parse(s).unwrap();
        host.message(0, HookMsg::State(state(r#"{"t":"state","prefer_ingame":true,"me":{"name":"Sam"},"present":""}"#)));
        assert!(host.instances[0].prefer_ingame && !host.instances[0].ingame);
        assert!(host.use_ingame(), "the focused copy's hook would rather");
        host.message(0, HookMsg::State(state(r#"{"t":"state","prefer_ingame":false,"present":"d3d11"}"#)));
        assert!(!host.instances[0].prefer_ingame && host.instances[0].ingame);
        assert!(host.use_ingame(), "it draws there already");
    }

    /// Each copy's overlay shows that copy's state, whichever has the focus, and a copy
    /// closing its page closes its own overlay alone.
    #[test]
    fn each_copy_has_its_own_overlay() {
        let mut host = session(2);
        let state = |me: &str, slot: u32| HookMsg::State(Json::parse(&format!(r#"{{"t":"state","me":{{"name":"{me}","slot":{slot}}}}}"#)).unwrap());
        host.message(0, state("First", 0));
        host.message(1, state("Second", 1));
        assert_eq!(host.panes[0].overlay.state.me, "First");
        assert_eq!(host.panes[1].overlay.state.me, "Second");
        host.set_focus(1);
        assert_eq!(host.focus, 1);
        assert_eq!(host.panes[0].overlay.state.me, "First", "the focus doesn't move what an overlay shows");
        host.set_focus(5);
        assert_eq!(host.focus, 1, "no sixth copy");
        // Both open at once; one closing its page leaves the other open.
        host.panes[0].visible = true;
        host.panes[1].visible = true;
        host.message(0, HookMsg::Close);
        assert!(!host.panes[0].visible && host.panes[1].visible);
        host.message(1, HookMsg::Close);
        assert!(!host.panes[1].visible);
        // Copies that end: the session waits for the rest, and ends with the last.
        host.instances[0].end();
        assert!(!host.wait(0), "one still runs");
        host.instances[1].end();
        assert!(host.wait(0));
    }

    /// A copy's notifications go to its own overlay: the second player's "you play as"
    /// shows on the second player's screen, not the first's.
    #[test]
    fn notifications_go_to_their_own_copy() {
        let mut host = session(2);
        let state = |me: &str, slot: u32| HookMsg::State(Json::parse(&format!(r#"{{"t":"state","me":{{"name":"{me}","slot":{slot}}}}}"#)).unwrap());
        host.message(0, state("Kim", 0));
        host.message(1, state("Sam", 1));
        assert!(!host.panes[0].overlay.has_toasts(), "the first player is told nothing");
        assert_eq!(host.panes[1].overlay.toasts.len(), 1);
        assert!(host.panes[1].overlay.toasts[0].title.contains("Sam"), "{}", host.panes[1].overlay.toasts[0].title);
        host.message(0, HookMsg::Note(peerparty_protocol::ipc::Notice::text("Saved", "ok")));
        assert_eq!((host.panes[0].overlay.toasts.len(), host.panes[1].overlay.toasts.len()), (1, 1));
    }

    /// In a session each copy's overlay reads the controller the copy was given (the
    /// focused copy also the ones no copy was), and a Home press opens the overlay of
    /// the copy that controller plays.
    #[test]
    fn each_player_works_their_own_overlay() {
        let copies = (0..3)
            .map(|i| {
                let mut spec = InstanceSpec::solo(InstanceId(i), i);
                spec.input_assignment = InputAssignment::Controller { index: i };
                InstanceConn::detached(spec)
            })
            .collect();
        let bounds = MonitorBounds { x: 0, y: 0, width: 1920, height: 1080, dpi: 96 };
        let compositor = CompositorWindow::new(LayoutPreset::Grid2x2, bounds, 3);
        let mut host = SessionHost::new(copies, Some(compositor), Path::new(r"C:\Games\Some Game\game.exe"), Prefs::default(), false);
        host.pads = PadFrame { all: pad::SOUTH | pad::EAST | pad::NORTH | pad::WEST, per: vec![pad::SOUTH, 0, pad::EAST, pad::NORTH], spare: pad::WEST, homes: vec![] };
        assert_eq!(host.pane_pad(0), pad::SOUTH | pad::NORTH | pad::WEST, "its own, and the spare ones as the focused copy");
        assert_eq!(host.pane_pad(1), 0);
        assert_eq!(host.pane_pad(2), pad::EAST);
        host.set_focus(1);
        assert_eq!((host.pane_pad(0), host.pane_pad(1)), (pad::SOUTH, pad::NORTH | pad::WEST));
        assert_eq!(host.pane_for_pad(Some(2)), 2);
        assert_eq!(host.pane_for_pad(Some(3)), 1, "a spare controller's press is the focused copy's");
        assert_eq!(host.pane_for_pad(None), 1);
        // A copy alone reads them all.
        let mut solo = session(1);
        solo.pads = PadFrame { all: pad::SOUTH | pad::EAST, per: vec![pad::SOUTH, pad::EAST], spare: 0, homes: vec![] };
        assert_eq!(solo.pane_pad(0), pad::SOUTH | pad::EAST);
        assert_eq!(solo.pane_for_pad(Some(1)), 0);
    }

    /// Which seats take a controller: none for a keyboard player or a copy that ended;
    /// a player who has been on the keyboard, only after the others.
    #[test]
    fn seats_open_by_what_players_play_with() {
        let copies = (0..4)
            .map(|i| {
                let mut spec = InstanceSpec::solo(InstanceId(i), i);
                spec.input_assignment = InputAssignment::Controller { index: i };
                InstanceConn::detached(spec)
            })
            .collect();
        let bounds = MonitorBounds { x: 0, y: 0, width: 1920, height: 1080, dpi: 96 };
        let compositor = CompositorWindow::new(LayoutPreset::Grid2x2, bounds, 4);
        let mut host = SessionHost::new(copies, Some(compositor), Path::new(r"C:\Games\Some Game\game.exe"), Prefs::default(), false);
        host.set_inputs(vec![SlotInput::Auto, SlotInput::KeyboardMouse, SlotInput::Controller]);
        assert_eq!(host.focus, 1, "the keyboard player's pane has the focus");
        host.kbm_player[0] = true;
        host.instances[3].gone = true;
        assert_eq!(host.seat_open(), Some(vec![pad::Open::Later, pad::Open::No, pad::Open::Yes, pad::Open::No]));
        assert_eq!(session(1).seat_open(), None, "a copy alone hands seats out as controllers come");
        let seats = host.seats(0).unwrap();
        assert_eq!((seats.me, seats.keyboard), (0, Some(1)));
        assert!(seats.players[0].can_swap && !seats.players[1].can_swap && !seats.players[3].can_swap);
        host.keyboard_step(1);
        assert_eq!((host.focus, host.kbm_off), (2, false));
        host.keyboard_step(1);
        assert!(host.kbm_off, "Off step in the cycle");
        assert_eq!(host.seats(0).unwrap().keyboard, None);
        host.keyboard_step(1);
        assert_eq!((host.focus, host.kbm_off), (0, false), "wraps past the one that ended");
        // With an explicit KBM player, clicking an explicit Controller pane doesn't pull
        // KBM focus onto it; Auto panes and KBM panes still allow click-focus.
        assert!(host.click_can_focus(0), "Auto pane accepts click focus");
        assert!(host.click_can_focus(1), "KBM pane accepts click focus");
        assert!(!host.click_can_focus(2), "Controller pane resists click focus when a KBM player exists");
        // Without an explicit KBM player, any pane can be click-focused.
        host.set_inputs(vec![SlotInput::Auto, SlotInput::Controller, SlotInput::Controller]);
        assert!(host.click_can_focus(0) && host.click_can_focus(1) && host.click_can_focus(2));
    }
}
