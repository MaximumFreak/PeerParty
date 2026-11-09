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

mod frame;
mod gamebar;
mod input;
pub(crate) mod pad;
pub mod pipe;
pub(crate) mod raster;
mod render;
mod ui;
mod window;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use eframe::egui;
use serde_json::{json, Value};
use windows_sys::Win32::Foundation::{HANDLE, HWND, POINT, RECT, WAIT_OBJECT_0};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::firewall;
use crate::ui::store::{GameOptions, Prefs, Settings};
use ui::{Cmd, Nav, Overlay, Page};

const VK_SHIFT: i32 = 0x10;
const VK_TAB: u32 = 0x09;
const VK_F1: u32 = 0x70;
const VK_F2: u32 = 0x71;
const VK_F3: u32 = 0x72;
const CHORD_HOLD: Duration = Duration::from_secs(1);
/// `Host::hotkeys`, by name.
const HOTKEY_NAMES: [&str; 4] = ["Shift+Tab", "Shift+F3", "Shift+F1", "Shift+F2"];
/// Recording a chord gives up after this long with no chord held.
const RECORD_FOR: Duration = Duration::from_secs(10);

/// The settings that apply to this game: the global ones with its own answers on top.
pub fn effective(settings: &Settings, game: &GameOptions) -> Prefs {
    let mut p = settings.prefs.clone();
    if let Some(v) = game.auto_accept {
        p.auto_accept = v;
    }
    if let Some(v) = game.overlay {
        p.overlay = v;
    }
    if let Some(v) = game.voice {
        p.voice = v;
    }
    if let Some(v) = game.steam_saves {
        p.steam_saves = v;
    }
    p
}

struct Host {
    pid: u32,
    key: String,
    pipe: pipe::Pipe,
    prefs: Prefs,
    overlay: Overlay,
    ctx: egui::Context,
    toast_ctx: egui::Context,
    /// The overlay's canvas and how it's drawn (render.rs).
    render: render::Renderer,
    toast_textures: raster::Textures,
    toast_canvas: raster::Canvas,
    window: Option<window::Layered>,
    toast_window: Option<window::Layered>,
    pads: pad::Pads,
    visible: bool,
    dirty: bool,
    /// When the last frame was drawn (frames are at most this often: `FRAME_GAP`).
    last_frame: Instant,
    saver: Saver,
    game_hwnd: HWND,
    hwnd_checked: Option<Instant>,
    cursor: egui::Pos2,
    /// Whether the overlay's pointer shows: only once the mouse moves or clicks after
    /// opening, and hidden again when the controller or keyboard navigates.
    pointer: bool,
    ppp: f32,
    started: Instant,
    // Hotkey edges.
    keys_down: [bool; 4],
    pad_prev: u32,
    chord_since: Option<Instant>,
    chord_fired: bool,
    /// Recording a chord: when it started, the buttons held and since when.
    recording: Option<(Instant, u32, Instant)>,
    repeat: Option<(u32, Instant)>,
    shift: bool,
    seen_pending: Vec<u64>,
    seen_peers: Vec<String>,
    first_state: bool,
    frame: Option<frame::Shared>,
    ingame: bool,
    /// Game Bar's overlay over the game (it never takes the foreground), and whether it
    /// was open at the last tick: the overlay leaves the input to it meanwhile.
    gamebar: gamebar::GameBar,
    under_gamebar: bool,
    /// The game's program, for the firewall check (the process's own: a starter program
    /// hands over to the game), and whether it's an Xbox app game (its rules are the
    /// package's, which this doesn't read).
    exe: PathBuf,
    packaged: bool,
    /// A firewall check running off this thread, and when the last one started.
    fw_rx: Option<Receiver<firewall::Verdict>>,
    fw_checked: Option<Instant>,
    /// What the last check found (blocked or not) and the players we couldn't reach, so
    /// each new problem toasts once.
    fw_blocked: bool,
    unreachable: u64,
    /// A firewall fix under way: what to say when it's done, and whether it worked.
    fixing: Option<Receiver<(String, bool)>>,
    /// Opening hotkeys held at the last tick, seen whether or not they could open the
    /// overlay, so a press that couldn't says why in the game's log.
    diag_keys: [bool; 4],
    /// The controllers last named in the game's log.
    pads_logged: Vec<String>,
    /// Ticks that panicked (each is caught; the first few go to the game's log).
    panics: u32,
    /// The fonts are loaded into both contexts (the first time anything is drawn: a
    /// game whose overlay never shows never reads them).
    fonts: bool,
}

/// How often the firewall is checked while the game runs (it's quick: the rules are
/// read from the registry).
const FIREWALL_EVERY: Duration = Duration::from_secs(20);

/// Runs until the game process exits. Returns when it has.
pub fn run(process: HANDLE, pid: u32, pipe: pipe::Pipe, exe: &Path, prefs: Prefs, packaged: bool) {
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
    };
    input::start();
    crate::i18n::set(&prefs.language);
    crate::ui::theme::set_effects(prefs.effects);
    let name = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut host = Host {
        pid,
        key: crate::ui::store::key_of(exe),
        pipe,
        overlay: Overlay::new(game_title(exe, &name)),
        prefs,
        ctx: egui::Context::default(),
        toast_ctx: egui::Context::default(),
        render: render::Renderer::default(),
        toast_textures: raster::Textures::default(),
        last_frame: Instant::now(),
        saver: Saver::start(),
        toast_canvas: raster::Canvas::new(0, 0),
        window: None,
        toast_window: None,
        pads: pad::Pads::new(),
        visible: false,
        dirty: true,
        game_hwnd: std::ptr::null_mut(),
        hwnd_checked: None,
        cursor: egui::Pos2::new(400.0, 300.0),
        pointer: false,
        ppp: 1.0,
        started: Instant::now(),
        keys_down: [false; 4],
        pad_prev: 0,
        chord_since: None,
        chord_fired: false,
        recording: None,
        repeat: None,
        shift: false,
        seen_pending: Vec::new(),
        seen_peers: Vec::new(),
        first_state: true,
        frame: None,
        ingame: false,
        gamebar: gamebar::GameBar::new(),
        under_gamebar: false,
        exe: firewall::process_exe(pid).unwrap_or_else(|| exe.to_path_buf()),
        packaged,
        fw_rx: None,
        fw_checked: None,
        fw_blocked: false,
        unreachable: 0,
        fixing: None,
        diag_keys: [false; 4],
        pads_logged: Vec::new(),
        panics: 0,
        fonts: false,
    };
    host.overlay.hotkey_hint = host.close_hint();
    loop {
        let timeout = if host.visible && input::pending() {
            // Input waits: the next frame as soon as the last one is far enough behind.
            FRAME_GAP.saturating_sub(host.last_frame.elapsed()).as_millis() as u32
        } else if host.visible || host.overlay.has_toasts() {
            16
        } else if host.in_front() {
            33
        } else {
            200
        };
        let r = unsafe {
            MsgWaitForMultipleObjects(1, &process, 0, timeout, QS_ALLINPUT)
        };
        if r == WAIT_OBJECT_0 {
            break;
        }
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.tick())) {
            host.panics += 1;
            if host.panics <= 3 {
                let what = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default();
                host.pipe.send(json!({"t": "note", "text": format!("overlay: the overlay's host failed a tick ({what}); it carries on")}));
            }
        }
    }
    input::capture(false);
    host.saver.finish();
}

/// The shortest time between two overlay frames (a mouse reports hundreds of moves a
/// second; each frame takes them all).
const FRAME_GAP: Duration = Duration::from_millis(7);

/// Saves the settings off the overlay's thread (reading and writing ui.json takes a few
/// milliseconds), only the latest when changes come faster than that.
struct Saver {
    tx: Option<std::sync::mpsc::Sender<(String, Prefs)>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Saver {
    fn start() -> Saver {
        let (tx, rx) = channel::<(String, Prefs)>();
        let thread = std::thread::Builder::new()
            .name("peerparty-save".into())
            .spawn(move || {
                while let Ok(mut job) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        job = newer;
                    }
                    save_global(&job.0, job.1);
                }
            })
            .ok();
        Saver { tx: Some(tx), thread }
    }

    fn save(&self, key: &str, prefs: &Prefs) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((key.to_string(), prefs.clone()));
        }
    }

    /// Waits for the last save (the process ends when the game does).
    fn finish(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Saves the global settings from the overlay's `prefs` for the game `key`: a game's own
/// answers stay (and win).
fn save_global(key: &str, prefs: Prefs) {
    let mut s = Settings::load();
    let game = s.options(key);
    let mut global = prefs;
    if game.auto_accept.is_some() {
        global.auto_accept = s.prefs.auto_accept;
    }
    if game.overlay.is_some() {
        global.overlay = s.prefs.overlay;
    }
    if game.voice.is_some() {
        global.voice = s.prefs.voice;
    }
    if game.steam_saves.is_some() {
        global.steam_saves = s.prefs.steam_saves;
    }
    s.prefs = global;
    s.save();
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
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}

impl Host {
    fn in_front(&self) -> bool {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
        pid == self.pid
    }

    /// Every hotkey that's on works in every game. Games started through PeerParty
    /// aren't started through Steam, Epic or EA, so their own overlays don't come up
    /// on these keys even when their module is loaded.
    fn hotkeys(&self) -> [(bool, u32); 4] {
        let hk = &self.prefs.hotkeys;
        [(hk.shift_tab, VK_TAB), (hk.shift_f3, VK_F3), (hk.shift_f1, VK_F1), (hk.shift_f2, VK_F2)]
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


    fn guide_opens(&self) -> bool {
        self.prefs.hotkeys.guide && !matches!(self.prefs.invite_buttons.as_str(), "guide" | "both")
    }

    /// The recorded chord, unless it's View + Menu while that also invites everyone.
    fn chord_opens(&self) -> bool {
        let c = self.prefs.hotkeys.chord;
        let view_menu = pad::BACK | pad::START;
        c.count_ones() >= 2 && !(c == view_menu && matches!(self.prefs.invite_buttons.as_str(), "combo" | "both"))
    }

    fn game_rect(&mut self) -> Option<RECT> {
        let stale = self.hwnd_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(1));
        if self.game_hwnd.is_null() || stale || unsafe { IsWindow(self.game_hwnd) } == 0 {
            self.game_hwnd = find_window(self.pid);
            self.hwnd_checked = Some(Instant::now());
        }
        if self.game_hwnd.is_null() {
            return None;
        }
        let mut r: RECT = unsafe { std::mem::zeroed() };
        let mut tl = POINT { x: 0, y: 0 };
        unsafe {
            GetClientRect(self.game_hwnd, &mut r);
            windows_sys::Win32::Graphics::Gdi::ClientToScreen(self.game_hwnd, &mut tl);
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        (w > 100 && h > 100).then_some(RECT { left: tl.x, top: tl.y, right: tl.x + w, bottom: tl.y + h })
    }

    fn tick(&mut self) {
        input::heartbeat();
        while let Ok(msg) = self.pipe.rx.try_recv() {
            self.message(&msg);
        }
        self.firewall_tick();
        let front = self.in_front();
        let rect = if front { self.game_rect() } else { None };
        let gamebar = front && self.gamebar.open(rect);
        if gamebar != self.under_gamebar {
            self.under_gamebar = gamebar;
            self.gamebar_changed(gamebar);
        }
        if self.visible {
            if !front {
                self.hide();
            } else if gamebar {
                // Game Bar has the controller, keyboard and mouse. Buttons pressed in
                // it don't act here once it closes.
                self.pad_prev = self.pads.poll();
                self.repeat = None;
            } else {
                self.frame_open();
            }
        } else if front && self.prefs.overlay && !gamebar {
            self.check_open();
        } else {
            self.why_not_open(front, gamebar);
            // A Home or hotkey still down when the game comes back to the front (Guide
            // closing Game Bar) isn't a press.
            self.pads.hold_home();
            self.keys_down = [true; 4];
        }
        if !self.visible {
            self.frame_toasts();
        }
    }

    fn message(&mut self, msg: &Value) {
        match msg.get("t").and_then(Value::as_str) {
            Some("state") => {
                // A starter program handed over to the game it started (pipe.rs): the
                // overlay follows the process that's talking now.
                if let Some(pid) = msg.get("pid").and_then(Value::as_u64).map(|p| p as u32).filter(|&p| p != 0 && p != self.pid) {
                    self.pid = pid;
                    self.game_hwnd = std::ptr::null_mut();
                    self.hwnd_checked = None;
                    self.frame = None;
                    self.send_settings();
                    if let Some(exe) = firewall::process_exe(pid) {
                        self.exe = exe;
                        self.fw_checked = None;
                    }
                }
                let st = ui::GameState::from_json(msg);
                self.ingame = msg.get("present").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
                // Notifications for what's new since the last state.
                if !self.first_state && self.prefs.toasts && !self.visible {
                    for p in &st.pending {
                        if !self.seen_pending.contains(&p.id) {
                            let hint = self.open_hint();
                            self.overlay.toast(ui::invite_text(&p.name, &p.kind), crate::t!("toast.press_to_answer", keys = hint));
                        }
                    }
                    for p in &st.peers {
                        if !self.seen_peers.contains(&p.id) {
                            self.overlay.toast(crate::t!("toast.playing_too", name = p.name), p.reach());
                        }
                    }
                }
                // Players on our network we can't reach: say so once, when it starts.
                let unreachable = st.lan_unreachable.unwrap_or(0);
                if unreachable > 0 && self.unreachable == 0 && !self.visible {
                    self.overlay.toast(crate::t!("overlay.unreachable", n = unreachable), crate::t!("toast.unreachable_detail"));
                }
                self.unreachable = unreachable;
                self.seen_pending = st.pending.iter().map(|p| p.id).collect();
                self.seen_peers = st.peers.iter().map(|p| p.id.clone()).collect();
                self.first_state = false;
                self.overlay.state = st;
                self.overlay.hotkey_hint = self.close_hint();
                self.dirty = true;
            }
            Some("note") => {
                let s = |k: &str| msg.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                self.overlay.note(&s("title"), &s("detail"), self.visible);
                self.dirty = true;
            }
            Some("close") => self.hide(),
            Some("open") => {
                let page = match msg.get("page").and_then(Value::as_str).unwrap_or("home") {
                    "friends" | "invite" | "invites" => Page::Players,
                    "settings" => Page::Settings,
                    "profile" => match msg.get("peer").and_then(Value::as_str) {
                        Some(p) => Page::Profile(p.to_string()),
                        None => Page::Players,
                    },
                    _ => Page::Home,
                };
                let invite = msg.get("page").and_then(Value::as_str) == Some("invite");
                if self.prefs.overlay {
                    self.show(page, invite, "the game");
                }
            }
            _ => {}
        }
    }

    /// An opening hotkey pressed while the overlay can't open: the game's log says why
    /// (another window in front, Game Bar over the game), so a player's "it didn't open"
    /// has an answer.
    fn why_not_open(&mut self, front: bool, gamebar: bool) {
        let shift = down(VK_SHIFT);
        let mut pressed = None;
        for (i, (on, vk)) in self.hotkeys().iter().enumerate() {
            let d = down(*vk as i32);
            if *on && shift && d && !self.diag_keys[i] {
                pressed = Some(HOTKEY_NAMES[i]);
            }
            self.diag_keys[i] = d;
        }
        let Some(key) = pressed else { return };
        let why = if !self.prefs.overlay {
            "the overlay is turned off for this game".to_string()
        } else if gamebar {
            "Game Bar looks open over the game (it has the keyboard and controller meanwhile)".to_string()
        } else if !front {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
            let name = firewall::process_exe(pid)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| format!("process {pid}"));
            format!("the game isn't the window in front ({name} is; the game is process {})", self.pid)
        } else {
            return;
        };
        self.pipe.send(json!({"t": "note", "text": format!("overlay: {key} pressed but the overlay can't open: {why}")}));
    }

    /// The controllers the overlay reads, into the game's log whenever they change, so a
    /// button that doesn't open it shows whether its controller is read at all.
    fn log_pads(&mut self) {
        let found = self.pads.found();
        if found != self.pads_logged {
            let list = if found.is_empty() { "none".to_string() } else { found.join(", ") };
            self.pipe.send(json!({"t": "note", "text": format!("overlay: controllers read: {list}")}));
            self.pads_logged = found;
        }
    }

    /// Opening hotkeys and buttons, while the overlay is closed and the game in front.
    fn check_open(&mut self) {
        self.diag_keys = [true; 4];
        let shift = down(VK_SHIFT);
        let keys = self.hotkeys();
        let mut open = None;
        for (i, (on, vk)) in keys.iter().enumerate() {
            let d = down(*vk as i32);
            if *on && shift && d && !self.keys_down[i] {
                open = Some(HOTKEY_NAMES[i].to_string());
            }
            self.keys_down[i] = d;
        }
        let pad = self.pads.poll();
        self.pad_prev = pad;
        self.overlay.style = self.pads.style;
        self.log_pads();
        if self.guide_opens() && self.pads.home_pressed {
            open = Some(format!("Home / Guide on {}", self.pads.home_by));
            if !self.prefs.guide_tip_shown {
                self.overlay.tip = Some(crate::t!("toast.gamebar_tip").into());
                self.prefs.guide_tip_shown = true;
                self.save_prefs();
            }
        }
        let chord = self.prefs.hotkeys.chord;
        let held = self.chord_opens() && pad & chord == chord;
        match (held, self.chord_since) {
            (true, None) => self.chord_since = Some(Instant::now()),
            (true, Some(t)) if !self.chord_fired && t.elapsed() >= CHORD_HOLD => {
                self.chord_fired = true;
                open = Some(format!("{} held", pad::names(chord)));
            }
            (false, _) => {
                self.chord_since = None;
                self.chord_fired = false;
            }
            _ => {}
        }
        if let Some(why) = open {
            let page = Page::Home;
            self.show(page, false, &why);
        }
    }

    /// Game Bar's overlay opened or closed over the game. While it's open the keyboard,
    /// mouse and controller go to it; the overlay stays open underneath (also when the
    /// same Guide press opened both) and takes them back once Game Bar closes.
    fn gamebar_changed(&mut self, open: bool) {
        let text = if open { "overlay: Game Bar opened over the game; input goes to it" } else { "overlay: Game Bar closed" };
        self.pipe.send(json!({"t": "note", "text": text}));
        if !self.visible {
            return;
        }
        if open {
            input::capture(false);
        } else {
            input::capture(true);
            self.pad_prev = self.pads.poll();
            self.dirty = true;
        }
    }

    /// `why` goes into the game's log: what opened it.
    fn show(&mut self, page: Page, invite_mode: bool, why: &str) {
        if !self.in_front() {
            return;
        }
        self.overlay.open(page, invite_mode);
        if self.visible {
            self.dirty = true;
            return;
        }
        let Some(r) = self.game_rect() else {
            self.pipe.send(json!({"t": "note", "text": format!("overlay: {why} can't open it: no window of the game's (process {}) to draw over", self.pid)}));
            return;
        };
        self.visible = true;
        self.dirty = true;
        self.render.invalidate();
        self.cursor = egui::Pos2::new(((r.right - r.left) / 2) as f32 / self.ppp, ((r.bottom - r.top) / 2) as f32 / self.ppp);
        self.pointer = false;
        // Buttons held while opening don't act on the first frame.
        self.pad_prev = self.pads.poll();
        self.repeat = None;
        self.shift = false;
        input::capture(true);
        self.pipe.send(json!({"t": "overlay", "visible": true, "why": why}));
        if let Some(w) = self.toast_window.as_mut() {
            w.hide();
        }
    }

    fn hide(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.recording = None;
        input::capture(false);
        self.pipe.send(json!({"t": "overlay", "visible": false}));
        if let Some(w) = self.window.as_mut() {
            w.hide();
        }
        if let Some(f) = self.frame.as_mut() {
            f.hide();
        }
        self.overlay.tip = None;
        // Keys still held when it closed don't reopen it.
        self.keys_down = [true; 4];
        self.pad_prev = self.pads.poll();
    }

    fn save_prefs(&mut self) {
        self.saver.save(&self.key, &self.prefs);
    }

    fn send_settings(&self) {
        self.pipe.send(json!({"t": "set", "key": "auto_accept", "value": if self.prefs.auto_accept { "auto" } else { "ask" }}));
        self.pipe.send(json!({"t": "set", "key": "invite_buttons", "value": self.prefs.invite_buttons}));
    }

    /// Navigation from the keyboard (through the hooks) and controllers, with repeat for
    /// held directions. Also closes on a hotkey.
    fn gather(&mut self) -> (Nav, Vec<egui::Event>, bool) {
        let mut nav = Nav::default();
        let mut events = Vec::new();
        let mut moved = false;
        let screen = self.screen_points();
        for i in input::take() {
            match i {
                input::Input::Key { vk, down } => {
                    if vk == 0x10 || vk == 0xA0 || vk == 0xA1 {
                        self.shift = down;
                        continue;
                    }
                    if !down {
                        continue;
                    }
                    if self.shift && self.hotkeys().iter().any(|(on, k)| *on && *k == vk) {
                        nav.close = true;
                        continue;
                    }
                    match vk {
                        0x26 => nav.up = true,
                        0x28 => nav.down = true,
                        0x25 => nav.left = true,
                        0x27 => nav.right = true,
                        0x0D | 0x20 => nav.select = true,
                        0x2E | 0x58 => nav.second = true,
                        0x1B | 0x08 => nav.back = true,
                        0x51 | 0x21 => nav.prev_tab = true,
                        0x45 | 0x22 => nav.next_tab = true,
                        0x09 if self.shift => nav.prev_tab = true,
                        0x09 => nav.next_tab = true,
                        _ => {}
                    }
                }
                input::Input::Move { dx, dy } => {
                    self.cursor.x = (self.cursor.x + dx as f32 / self.ppp).clamp(0.0, screen.x - 1.0);
                    self.cursor.y = (self.cursor.y + dy as f32 / self.ppp).clamp(0.0, screen.y - 1.0);
                    moved = true;
                }
                input::Input::Button { left, down } => {
                    if !self.pointer {
                        self.pointer = true;
                        events.push(egui::Event::PointerMoved(self.cursor));
                    }
                    events.push(egui::Event::PointerButton {
                        pos: self.cursor,
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
            self.pointer = true;
            events.insert(0, egui::Event::PointerMoved(self.cursor));
        }
        let pad = self.pads.poll();
        self.overlay.style = self.pads.style;
        if self.recording.is_some() {
            self.record_chord(pad);
            self.pad_prev = pad;
            return (Nav::default(), events, moved);
        }
        let pressed = pad & !self.pad_prev;
        self.pad_prev = pad;
        // Held directions repeat after 400 ms, every 110 ms.
        let dirs = pad & (pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT);
        let mut fire = pressed;
        match self.repeat {
            Some((d, t)) if d == dirs && dirs != 0 && t.elapsed() > Duration::from_millis(400) => {
                fire |= dirs;
                self.repeat = Some((d, t + Duration::from_millis(110)));
            }
            Some((d, _)) if d == dirs => {}
            _ => self.repeat = (dirs != 0).then(|| (dirs, Instant::now())),
        }
        nav.up |= fire & pad::UP != 0;
        nav.down |= fire & pad::DOWN != 0;
        nav.left |= fire & pad::LEFT != 0;
        nav.right |= fire & pad::RIGHT != 0;
        nav.select |= pressed & pad::SOUTH != 0;
        nav.second |= pressed & pad::WEST != 0;
        nav.back |= pressed & pad::EAST != 0;
        nav.prev_tab |= pressed & pad::LB != 0;
        nav.next_tab |= pressed & pad::RB != 0;
        // Home only opens the overlay (B goes back and closes, Start closes).
        nav.close |= pressed & pad::START != 0;
        // Controller or keyboard navigation hides the pointer (and its hover) until the
        // mouse moves again.
        if self.pointer && !moved && nav.any() {
            self.pointer = false;
            events.push(egui::Event::PointerGone);
        }
        (nav, events, moved)
    }

    /// Recording a chord: two or more buttons (Home aside) held unchanged for a second
    /// become it. The controller doesn't navigate meanwhile.
    fn record_chord(&mut self, pad: u32) {
        let Some((started, held, since)) = self.recording else { return };
        let now = Instant::now();
        let buttons = pad & !pad::HOME;
        self.dirty = true;
        if buttons.count_ones() >= 2 {
            if buttons != held {
                self.recording = Some((started, buttons, now));
            } else if now - since >= CHORD_HOLD {
                self.prefs.hotkeys.chord = buttons;
                self.recording = None;
                self.save_prefs();
                self.overlay.notice = Some((crate::t!("toast.chord_set", buttons = pad::names(buttons)), now + Duration::from_secs(5)));
                // The buttons still held don't reopen it once it closes.
                self.chord_fired = true;
                self.chord_since = Some(now);
            }
        } else if buttons != held {
            self.recording = Some((started, buttons, now));
        } else if now - started >= RECORD_FOR {
            self.recording = None;
            self.overlay.notice = Some((crate::t!("toast.chord_none").into(), now + Duration::from_secs(4)));
        }
    }

    fn screen_points(&self) -> egui::Vec2 {
        egui::vec2(self.render.canvas.w as f32 / self.ppp, self.render.canvas.h as f32 / self.ppp)
    }

    /// Loads the interface fonts the first time the overlay or a notification draws.
    fn fonts(&mut self) {
        if !self.fonts {
            self.fonts = true;
            crate::ui::theme::install_fonts(&self.ctx);
            crate::ui::theme::install_fonts(&self.toast_ctx);
        }
    }

    fn frame_open(&mut self) {
        self.fonts();
        let Some(r) = self.game_rect() else { return };
        if input::pending() && self.last_frame.elapsed() < FRAME_GAP {
            // Soon: the loop wakes again when the gap is over.
            return;
        }
        let (w, h) = ((r.right - r.left) as usize, (r.bottom - r.top) as usize);
        let ppp = (h as f32 / 900.0).clamp(0.8, 3.0) * self.prefs.overlay_scale;
        if (ppp - self.ppp).abs() > 0.001 || self.render.canvas.w != w || self.render.canvas.h != h {
            self.ppp = ppp;
            self.render.resize(w, h);
            self.render.invalidate();
            self.dirty = true;
        }
        if self.overlay.notice.as_ref().is_some_and(|(_, until)| *until <= Instant::now()) {
            self.overlay.notice = None;
            self.dirty = true;
        }
        let (nav, events, moved) = self.gather();
        if nav.any() || !events.is_empty() {
            self.dirty = true;
        }
        let pointer_changed = moved || self.pointer != self.render.pointer_shown();
        if !self.dirty && !pointer_changed {
            return;
        }
        self.last_frame = Instant::now();
        let mut rects: Vec<[i32; 4]> = Vec::new();
        // The pointer comes off first: what's under it is the frame without it.
        self.render.pointer_off(&mut rects);
        let mut cmds = Vec::new();
        let mut prefs = self.prefs.clone();
        if self.dirty {
            self.dirty = false;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, self.screen_points())),
                time: Some(self.started.elapsed().as_secs_f64()),
                events,
                ..Default::default()
            };
            self.ctx.set_pixels_per_point(self.ppp);
            self.overlay.paints_background = false;
            let overlay = &mut self.overlay;
            let out = self.ctx.run_ui(input, |ui| overlay.frame(ui, nav, &mut prefs, &mut cmds));
            if out.viewport_output.values().any(|v| v.repaint_delay < Duration::from_millis(100)) {
                self.dirty = true;
            }
            self.render.draw(&self.ctx, out, |screen| ui::background(ui::panel_rect(screen)), &mut rects);
        }
        if self.pointer {
            self.render.pointer_on(&self.ctx, self.cursor, self.ppp, &mut rects);
        }
        if !rects.is_empty() {
            self.present(r.left, r.top, &rects);
        }
        let prefs_changed = prefs != self.prefs;
        self.prefs = prefs;
        if prefs_changed {
            self.save_prefs();
            self.send_settings();
            self.overlay.hotkey_hint = self.close_hint();
            // The footer names the hotkeys.
            self.dirty = true;
        }
        for c in cmds {
            match c {
                Cmd::Hook(v) => self.pipe.send(v),
                Cmd::Close => self.hide(),
                Cmd::Prefs => {}
                Cmd::FixFirewall(_) if self.fixing.is_some() => {}
                Cmd::FixFirewall(public) => {
                    if exclusive_fullscreen() {
                        // Windows' permission prompt would hide behind the game.
                        self.overlay.notice = Some((crate::t!("toast.fullscreen_fix").into(), Instant::now() + Duration::from_secs(10)));
                        self.dirty = true;
                    } else {
                        self.hide();
                        let peerparty = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("peerparty.exe"));
                        let exe = self.exe.clone();
                        let (tx, rx) = channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(firewall::fix_or_open(&peerparty, &exe, public));
                        });
                        self.fixing = Some(rx);
                    }
                }
                Cmd::RecordChord => {
                    let now = Instant::now();
                    self.recording = Some((now, 0, now));
                    self.overlay.notice = Some((crate::t!("toast.hold_chord").into(), now + RECORD_FOR));
                    self.dirty = true;
                }
            }
        }
    }

    /// Checks Windows Firewall for the game now and then (only while PeerParty's engine
    /// runs in it), and picks up a fix's result.
    fn firewall_tick(&mut self) {
        if let Some(rx) = &self.fixing {
            match rx.try_recv() {
                Ok((msg, ok)) => {
                    self.fixing = None;
                    self.fw_checked = None;
                    if self.visible {
                        self.overlay.notice = Some((msg, Instant::now() + Duration::from_secs(10)));
                    } else {
                        self.overlay.toast(if ok { crate::t!("toast.firewall_fixed") } else { crate::t!("toast.firewall") }, msg);
                    }
                    self.dirty = true;
                }
                Err(TryRecvError::Empty) => return,
                Err(_) => self.fixing = None,
            }
        }
        if let Some(rx) = &self.fw_rx {
            match rx.try_recv() {
                Ok(v) => {
                    self.fw_rx = None;
                    self.firewall_seen(v);
                }
                Err(TryRecvError::Empty) => return,
                Err(_) => self.fw_rx = None,
            }
        }
        if self.packaged || self.overlay.state.lan_unreachable.is_none() || self.fw_checked.is_some_and(|t| t.elapsed() < FIREWALL_EVERY) {
            return;
        }
        self.fw_checked = Some(Instant::now());
        let exe = self.exe.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(firewall::assess(&firewall::rules(), &exe, firewall::profiles()));
        });
        self.fw_rx = Some(rx);
    }

    fn firewall_seen(&mut self, v: firewall::Verdict) {
        let fw = firewall_row(&v);
        if v.blocked() && !self.fw_blocked && !self.visible {
            let hint = self.open_hint();
            let title = fw.as_ref().map(|f| f.title.clone()).unwrap_or_default();
            self.overlay.toast(title, crate::t!("toast.firewall_blocks", keys = hint));
        }
        self.fw_blocked = v.blocked();
        if self.overlay.firewall != fw {
            self.overlay.firewall = fw;
            self.dirty = true;
        }
    }

    /// Shows the canvas: inside the game when the hook draws it there, else in a window.
    /// Whether to draw inside the game's frames: when asked to, when the hook already
    /// does, or (automatically) when the game is in exclusive fullscreen, where a window
    /// over it can't show.
    fn use_ingame(&self) -> bool {
        match self.prefs.draw.as_str() {
            "window" => false,
            "ingame" => true,
            _ => self.ingame || exclusive_fullscreen(),
        }
    }

    fn present(&mut self, x: i32, y: i32, dirty: &[[i32; 4]]) {
        if self.use_ingame() {
            if self.frame.is_none() {
                self.frame = frame::Shared::create(self.pid);
                if let Some(f) = &self.frame {
                    self.pipe.send(json!({"t": "frame", "name": f.name}));
                }
            }
            if let Some(f) = self.frame.as_mut() {
                let rects = self.overlay.panel_px(self.ppp).into_iter().collect::<Vec<_>>();
                f.write(&self.render.canvas, 0, 0, true, &rects, Some(dirty));
                if let Some(w) = self.window.as_mut() {
                    w.hide();
                }
                return;
            }
        }
        if self.window.is_none() {
            self.window = window::Layered::new();
        }
        if let Some(w) = self.window.as_mut() {
            w.present(&self.render.canvas, x, y, dirty);
        }
    }

    fn frame_toasts(&mut self) {
        if !self.overlay.has_toasts() {
            if let Some(w) = self.toast_window.as_mut() {
                w.hide();
            }
            if let Some(f) = self.frame.as_mut() {
                f.hide();
            }
            return;
        }
        self.fonts();
        let Some(r) = self.game_rect() else { return };
        let (w, h) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
        let ppp = (h / 900.0).clamp(0.8, 3.0) * self.prefs.overlay_scale;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w / ppp, h / ppp))),
            time: Some(self.started.elapsed().as_secs_f64()),
            ..Default::default()
        };
        self.toast_ctx.set_pixels_per_point(ppp);
        let mut area = None;
        let overlay = &mut self.overlay;
        let out = self.toast_ctx.run_ui(input, |ui| area = overlay.toasts_frame(ui));
        self.toast_textures.apply(out.textures_delta);
        let Some(area) = area else { return };
        let prims = self.toast_ctx.tessellate(out.shapes, out.pixels_per_point);
        let x0 = (area.min.x * ppp).floor().max(0.0);
        let y0 = (area.min.y * ppp).floor().max(0.0);
        let cw = ((area.max.x * ppp).ceil() - x0).max(1.0) as usize;
        let ch = ((area.max.y * ppp).ceil() - y0).max(1.0) as usize;
        if self.use_ingame() {
            if self.frame.is_none() {
                self.frame = frame::Shared::create(self.pid);
                if let Some(f) = &self.frame {
                    self.pipe.send(json!({"t": "frame", "name": f.name}));
                }
            }
        }
        if self.use_ingame() && self.frame.is_some() {
            // Inside the game: the full-size canvas, mostly clear.
            self.render.resize(w as usize, h as usize);
            self.render.invalidate();
            let canvas = &mut self.render.canvas;
            canvas.clear(egui::Color32::TRANSPARENT);
            self.toast_textures.draw(canvas, &prims, out.pixels_per_point, (0.0, 0.0));
            let rect = [x0 as u32, y0 as u32, cw as u32, ch as u32];
            if let Some(f) = self.frame.as_mut() {
                f.write(&self.render.canvas, 0, 0, true, &[rect], None);
            }
            return;
        }
        self.toast_canvas.resize(cw, ch);
        self.toast_canvas.clear(egui::Color32::TRANSPARENT);
        self.toast_textures.draw(&mut self.toast_canvas, &prims, out.pixels_per_point, (x0, y0));
        if self.toast_window.is_none() {
            self.toast_window = window::Layered::new();
        }
        if let Some(win) = self.toast_window.as_mut() {
            let all = [0, 0, self.toast_canvas.w as i32, self.toast_canvas.h as i32];
            win.present(&self.toast_canvas, r.left + x0 as i32, r.top + y0 as i32, &[all]);
        }
    }
}

/// Whether a Direct3D game holds the screen in exclusive fullscreen (Windows' own
/// notification state, which is what keeps notifications away from such games).
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

fn exclusive_fullscreen() -> bool {
    let mut state = 0;
    let hr = unsafe { windows_sys::Win32::UI::Shell::SHQueryUserNotificationState(&mut state) };
    hr >= 0 && state == 3 // QUNS_RUNNING_D3D_FULL_SCREEN
}

/// The game's main window: its biggest visible top-level window.
fn find_window(pid: u32) -> HWND {
    struct Find {
        pid: u32,
        best: HWND,
        area: i64,
    }
    unsafe extern "system" fn each(h: HWND, l: isize) -> i32 {
        let f = unsafe { &mut *(l as *mut Find) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut pid) };
        if pid != f.pid || unsafe { IsWindowVisible(h) } == 0 {
            return 1;
        }
        let ex = unsafe { GetWindowLongW(h, GWL_EXSTYLE) } as u32;
        if ex & WS_EX_TOOLWINDOW != 0 || !unsafe { GetWindow(h, GW_OWNER) }.is_null() {
            return 1;
        }
        let mut r: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(h, &mut r) };
        let area = i64::from(r.right - r.left) * i64::from(r.bottom - r.top);
        if area > f.area {
            f.area = area;
            f.best = h;
        }
        1
    }
    let mut f = Find { pid, best: std::ptr::null_mut(), area: 0 };
    unsafe { EnumWindows(Some(each), &mut f as *mut Find as isize) };
    f.best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firewall::{Rule, Verdict};

    fn block(policy: bool) -> Rule {
        Rule { id: "x".into(), name: "game".into(), inbound: true, active: true, policy, ..Default::default() }
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
}
