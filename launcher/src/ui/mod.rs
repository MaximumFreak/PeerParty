//! PeerParty's launcher window, shown when peerparty.exe starts with no arguments: finds
//! the games installed on this PC (Steam, Epic, GOG, Xbox, the player's game folders,
//! programs they add, recent launches) and starts each through peerparty.exe itself,
//! with a command line.
//!
//! Three places, from the sidebar: the **Library** (pick a game, press Play; its panel
//! holds what's particular to it), **Play together** (who can join: people on this
//! network always, friends with a room code, anyone online) and **Settings** (the
//! overlay, the library's folders, languages, and everything advanced). A first run
//! opens a short setup over the library.
//!
//! Mouse, keyboard and controller all work everywhere: the controller's d-pad walks
//! the game grid, and elsewhere turns into the arrow keys, A into Enter, so it reaches
//! every control egui can focus.
//!
//! The window costs nothing while it waits: it draws only when something changes (no
//! timers while idle; controllers are read on their own thread, only while the window
//! is in front), it minimizes once a game starts (by default), and while a game it
//! started runs with the window behind, the process drops to background priority.

pub mod fx;
mod game;
pub mod icons;
mod library;
pub mod mainexe;
mod padgate;
mod pads;
pub mod scan;
pub mod settings;
pub mod shared;
pub mod store;
pub mod theme;
mod together;
mod updates;
mod welcome;
mod widgets;

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Margin, Pos2, Rect, Sense, TextureHandle, TextureOptions, pos2, vec2};

use crate::firewall;
use crate::host::pad;
use crate::platform;
use crate::t;
use icons::{Glyph, Icon};
use scan::{Found, Game, Source};
use store::{GameOptions, Settings};
use theme::{ACCENT, SIDEBAR, TEXT, TEXT2, TEXT3};

/// `after_update`: started by an update (`--updated`), or after PeerParty's files moved
/// (`moved`, `--moved`), while the window before closes.
pub fn run(after_update: bool, moved: bool) -> eframe::Result {
    if !platform::first_launcher(after_update) {
        return Ok(());
    }
    let settings = Settings::load();
    crate::i18n::set(&settings.prefs.language);
    theme::set_effects(settings.prefs.effects);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PeerParty")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 560.0])
            .with_icon(window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "peerparty",
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            theme::install_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            let mut app = App::new(cc.egui_ctx.clone(), settings);
            if moved {
                app.say(t!("settings.portable_moved", folder = platform::data_dir().display()), true);
            }
            Ok(Box::new(app))
        }),
    )
}

/// Three linked nodes on a rounded tile: a party of peers (`icon_art`, which the exe's
/// icon is drawn from too).
fn window_icon() -> egui::IconData {
    let n = 128;
    egui::IconData { rgba: crate::icon_art::icon_rgba(n), width: n as u32, height: n as u32 }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Library,
    Together,
    Settings,
}

impl View {
    const ALL: [View; 3] = [View::Library, View::Together, View::Settings];

    fn name(self) -> &'static str {
        match self {
            View::Library => t!("nav.library"),
            View::Together => t!("nav.together"),
            View::Settings => t!("nav.settings"),
        }
    }

    fn icon(self) -> Icon {
        match self {
            View::Library => Icon::Library,
            View::Together => Icon::People,
            View::Settings => Icon::Settings,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Category {
    All,
    Recent,
    Source(Source),
    Hidden,
}

enum IconSlot {
    Loading,
    Ready(TextureHandle),
    Missing,
}

struct Session {
    id: u64,
    key: String,
    name: String,
    player: u8,
    /// Number of players inside one split-screen session (0 for a normal single-copy process).
    split_players: u8,
    started: Instant,
    stderr: Arc<Mutex<String>>,
    /// peerparty.exe's process id: Close all reaches it by `platform::close_game`.
    pid: u32,
}

/// After a game is started, Play does nothing for it for this long (see `play`).
const START_GUARD: Duration = Duration::from_secs(10);

struct PendingStart {
    at: Instant,
    key: String,
    name: String,
    args: Vec<String>,
    player: u8,
}

enum Action {
    Select(String),
    Play(String),
    Hide(String, bool),
    Reveal(PathBuf),
    MakeBat(String),
    /// Let the game through Windows Firewall (and on public networks too).
    FixFirewall(PathBuf, bool),
}

/// A short message at the bottom of the window.
struct Snack {
    text: String,
    ok: bool,
    at: Instant,
}

impl Snack {
    /// How long it shows. A problem stays until it's dismissed (or another snack takes
    /// its place): some are instructions to follow, like the firewall fix's.
    fn lasts(&self) -> Option<Duration> {
        self.ok.then_some(Duration::from_secs(5))
    }
}

/// What the icon thread is asked: a game's icon (its folder's icon.png, else its
/// program's), and the colour its picture glows with.
struct IconJob {
    key: String,
    exe: PathBuf,
    png: Option<PathBuf>,
    picture: Option<PathBuf>,
}

struct IconDone {
    key: String,
    icon: Option<platform::Rgba>,
    glow: Option<Color32>,
}

/// Scanned games, recent history, filter, search, and selection state.
struct LibraryState {
    scanned: Vec<Game>,
    /// A rescan's finds so far: they replace `scanned` once it's done, so the library
    /// (its filter, the open game) doesn't empty out while it runs.
    rescanned: Option<Vec<Game>>,
    entries: Vec<Game>,
    recent: HashMap<String, u64>,
    scanning: bool,
    found_rx: Receiver<Found>,
    category: Category,
    search: String,
    selected: Option<String>,
    /// Columns in the grid as last drawn (up / down move by a row).
    grid_cols: usize,
    /// Scroll the grid to the selected game.
    scroll_to_selected: bool,
    focus_search: bool,
    /// The top bar's controls this frame: down from one goes back to the grid.
    top_bar_ids: HashSet<egui::Id>,
}

impl LibraryState {
    fn new(ctx: &egui::Context, folders: Vec<PathBuf>, scan: bool) -> LibraryState {
        let (found_tx, found_rx) = channel();
        if scan {
            start_scan(ctx, folders, found_tx);
        }
        let mut lib = LibraryState {
            scanned: Vec::new(),
            rescanned: None,
            entries: Vec::new(),
            recent: HashMap::new(),
            scanning: scan,
            found_rx,
            category: Category::All,
            search: String::new(),
            selected: None,
            grid_cols: 1,
            scroll_to_selected: false,
            focus_search: false,
            top_bar_ids: HashSet::new(),
        };
        lib.reload_recent();
        if !lib.recent.is_empty() {
            lib.category = Category::Recent;
        }
        lib
    }

    fn reload_recent(&mut self) {
        self.recent = store::recent().into_iter().map(|r| (store::key_of(&r.exe), r.when)).collect();
    }

    fn rebuild(&mut self, settings: &Settings) {
        let mut seen = HashSet::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        let mut out: Vec<Game> = Vec::new();
        for g in &self.scanned {
            let key = store::key_of(&g.exe);
            if seen.insert(key.clone()) {
                at.insert(key, out.len());
                out.push(g.clone());
            } else if g.source == Source::Folder {
                let e = &mut out[at[&key]];
                if matches!(e.source, Source::Gog | Source::Epic) {
                    e.source = Source::Folder;
                    if e.art.is_none() {
                        e.art = g.art.clone();
                    }
                }
            }
        }
        for exe in &settings.added {
            if exe.is_file() && seen.insert(store::key_of(exe)) {
                out.push(scan::loose(exe, Source::Added));
            }
        }
        for r in store::recent() {
            if r.exe.is_file() && seen.insert(store::key_of(&r.exe)) {
                out.push(scan::loose(&r.exe, Source::Added));
            }
        }
        out.sort_by_key(|g| g.name.to_lowercase());
        self.entries = out;
    }

    fn rescan(&mut self, ctx: &egui::Context, settings: &Settings) {
        let (tx, rx) = channel();
        self.found_rx = rx;
        self.rescanned = Some(Vec::new());
        self.scanning = true;
        start_scan(ctx, settings.folders.clone(), tx);
        self.reload_recent();
        self.rebuild(settings);
    }

    fn poll_scan(&mut self, settings: &Settings) {
        let mut changed = false;
        while let Ok(f) = self.found_rx.try_recv() {
            match f {
                Found::Game(g) => match self.rescanned.as_mut() {
                    Some(r) => r.push(g),
                    None => {
                        self.scanned.push(g);
                        changed = true;
                    }
                },
                Found::Done => {
                    if let Some(r) = self.rescanned.take() {
                        self.scanned = r;
                    }
                    self.scanning = false;
                    changed = true;
                }
            }
        }
        if changed {
            self.rebuild(settings);
        }
    }

    fn game(&self, key: &str) -> Option<&Game> {
        self.entries.iter().find(|g| store::key_of(&g.exe) == key)
    }

    /// The filter the grid is showing: a search looks through All, whichever filter was
    /// picked (in Hidden it looks through the hidden games, which All leaves out).
    fn shown(&self) -> Category {
        if self.search.trim().is_empty() || self.category == Category::Hidden { self.category } else { Category::All }
    }

    /// Picks a filter. A search goes on in All and Hidden; any other filter clears it,
    /// since while it's there the grid would show All instead.
    fn pick_category(&mut self, c: Category) {
        if !matches!(c, Category::All | Category::Hidden) {
            self.search.clear();
        }
        self.category = c;
    }
}

/// Running game sessions, queued second copies, and per-start save-copy toggles.
struct SessionManager {
    sessions: Vec<Session>,
    next_session: u64,
    ended_tx: Sender<(u64, Option<i32>)>,
    ended_rx: Receiver<(u64, Option<i32>)>,
    pending: Vec<PendingStart>,
    /// Games whose next extra copy starts from player 1's saves ("Copy Player 1's saves
    /// to Player N"). Never saved: off whenever the window starts, and off again once a start
    /// has used it, so a later Play can't replace a save by accident.
    copy_saves: HashSet<String>,
    /// Close all on the Now playing card asked first: Close all again closes everything.
    confirm_close: bool,
    peerparty: PathBuf,
    /// The Play button's spark: when it was pressed, and a seed for its arcs.
    spark: Option<(Instant, u64)>,
    /// In background mode (a game runs, the window is behind it).
    background: bool,
}

impl SessionManager {
    fn new() -> SessionManager {
        let (ended_tx, ended_rx) = channel();
        let peerparty = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("peerparty.exe"));
        SessionManager {
            sessions: Vec::new(),
            next_session: 1,
            ended_tx,
            ended_rx,
            pending: Vec::new(),
            copy_saves: HashSet::new(),
            confirm_close: false,
            peerparty,
            spark: None,
            background: false,
        }
    }

    fn running(&self, key: &str) -> Vec<u8> {
        let mut p: Vec<u8> = Vec::new();
        for s in self.sessions.iter().filter(|s| s.key == key) {
            let count = s.split_players.max(1);
            for offset in 0..count {
                p.push(s.player.saturating_add(offset));
            }
        }
        p.sort();
        p
    }

    /// Whether "Copy Player 1's saves to Player N" is shown.
    pub(super) fn copy_saves_offered(&mut self, key: &str, players: u8, running: bool) -> bool {
        let waiting = self.pending.iter().any(|p| p.key == key);
        let shown = (players >= 2 || running) && !waiting;
        if !shown {
            self.copy_saves.remove(key);
        }
        shown
    }

    /// The player the save switch is for: the next copy Play adds.
    pub(super) fn copy_saves_target(&self, key: &str) -> u8 {
        let running = self.running(key).last().copied().unwrap_or(0);
        let waiting = self.pending.iter().filter(|p| p.key == key).map(|p| p.player).max().unwrap_or(0);
        running.max(waiting).max(1) + 1
    }

    /// Starts peerparty.exe for a game; a thread of its own waits for it to end.
    fn spawn(&mut self, ctx: &egui::Context, key: &str, name: &str, args: Vec<String>, player: u8, split_players: u8) -> Result<(), std::io::Error> {
        let mut cmd = Command::new(&self.peerparty);
        cmd.arg("--why").args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = platform::quiet(&mut cmd).spawn()?;
        let pid = child.id();
        let id = self.next_session;
        self.next_session += 1;
        let stderr = Arc::new(Mutex::new(String::new()));
        let pipe = child.stderr.take();
        let sink = stderr.clone();
        let tx = self.ended_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut buf = String::new();
                let _ = pipe.read_to_string(&mut buf);
                sink.lock().unwrap().push_str(&buf);
            }
            let code = child.wait().ok().and_then(|s| s.code());
            let _ = tx.send((id, code));
            ctx.request_repaint();
        });
        self.sessions.push(Session { id, key: key.into(), name: name.into(), player, split_players, started: Instant::now(), stderr, pid });
        Ok(())
    }

    /// Close all on the Now playing card: returns how many running copies were asked to close.
    fn close_all(&mut self) -> usize {
        self.pending.clear();
        let mut asked = 0;
        for s in &self.sessions {
            if platform::close_game(s.pid) {
                asked += 1;
            }
        }
        asked
    }
}

/// Async icon and glow color cache loaded off the UI thread.
struct IconLoader {
    tx: Sender<IconJob>,
    rx: Receiver<IconDone>,
    icons: HashMap<String, IconSlot>,
    glows: HashMap<String, Color32>,
}

impl IconLoader {
    fn start(ctx: egui::Context) -> IconLoader {
        let (tx, rx) = icon_worker(ctx);
        IconLoader { tx, rx, icons: HashMap::new(), glows: HashMap::new() }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.rx.try_recv() {
            let slot = match done.icon {
                Some(rgba) => {
                    let img = egui::ColorImage::from_rgba_unmultiplied([rgba.width, rgba.height], &rgba.pixels);
                    IconSlot::Ready(ctx.load_texture(&done.key, img, TextureOptions::LINEAR))
                }
                None => IconSlot::Missing,
            };
            if let Some(c) = done.glow {
                self.glows.insert(done.key.clone(), c);
            }
            self.icons.insert(done.key, slot);
        }
    }

    /// The colour a game's light takes: its picture's, else one from its name.
    fn glow_of(&self, g: &Game, key: &str) -> Color32 {
        self.glows.get(key).copied().unwrap_or_else(|| theme::mix(theme::tint_for(&g.name), Color32::WHITE, 0.35))
    }
}

/// Controller & keyboard navigation state.
struct InputNav {
    /// Controllers, for picking a game from the couch.
    pads: pads::PadThread,
    /// Which presses count (only while this window is in front).
    pad_gate: padgate::PadGate,
    repeat: shared::RepeatTimer,
    /// Controller presses for the game grid, from the input hook.
    pad_fire: u32,
    /// The controller was the last thing used (its hints show).
    pad_mode: bool,
    /// The first control of the page just opened takes the focus (controller, keyboard).
    focus_first: bool,
    /// The game panel's Play button takes the focus.
    focus_play: bool,
}

impl InputNav {
    fn start(ctx: egui::Context) -> InputNav {
        InputNav {
            pads: pads::PadThread::start(ctx),
            pad_gate: padgate::PadGate::default(),
            repeat: shared::RepeatTimer::default(),
            pad_fire: 0,
            pad_mode: false,
            focus_first: false,
            focus_play: false,
        }
    }
}

/// Background system checks: Windows Firewall, Steam app ids, invites, and update notices.
struct SystemChecks {
    /// Reads Windows Firewall's rules for the warnings.
    firewall: firewall::Checker,
    /// "Also on public networks" for the firewall fix.
    fw_public: bool,
    /// A firewall fix under way: what to say when it's done, and whether it worked.
    fixing: Option<Receiver<(String, bool)>>,
    /// The "Invite players at home" button's answer, while it is awaited.
    inviting: Option<Receiver<bool>>,
    /// Per program: its game folder looked through, whether its Steam app id can't be
    /// found, and when that was checked (steamid.rs).
    steam_ids: HashMap<PathBuf, (crate::steamid::Game, bool, Instant)>,
    /// The real window, not a test's or a preview's: it may ask GitHub for updates.
    live: bool,
    /// The newer version already announced.
    update_told: Option<String>,
}

impl SystemChecks {
    fn new(live: bool) -> SystemChecks {
        SystemChecks {
            firewall: firewall::Checker::default(),
            fw_public: false,
            fixing: None,
            inviting: None,
            steam_ids: HashMap::new(),
            live,
            update_told: None,
        }
    }
}

struct App {
    ctx: egui::Context,
    settings: Settings,
    /// The settings as last loaded or saved here, for 3-way locked merging (`save_over`).
    settings_base: Settings,
    /// When ui.json was last read or written here (the overlay changes it too).
    settings_seen: Option<std::time::SystemTime>,
    lib: LibraryState,
    sessions: SessionManager,
    icons: IconLoader,
    nav: InputNav,
    checks: SystemChecks,
    view: View,
    snack: Option<Snack>,
    /// The settings "Reset to recommended" replaced, offered back until one of them
    /// changes again or the window closes.
    reset_undo: Option<store::Prefs>,
    /// The first-run setup, while it shows.
    welcome: Option<welcome::Welcome>,
    /// Play together's state (the code being typed, the blocked players).
    together: together::State,
    /// Settings' open section.
    settings_tab: settings::Tab,
}

impl App {
    fn new(ctx: egui::Context, settings: Settings) -> App {
        Self::with(ctx, settings, true)
    }

    /// The window's state; `scan` looks for installed games (tests show their own).
    fn with(ctx: egui::Context, settings: Settings, scan: bool) -> App {
        let welcome = (!settings.onboarded).then(|| welcome::Welcome::new(&settings.prefs));
        let lib = LibraryState::new(&ctx, settings.folders.clone(), scan);
        let settings_base = settings.clone();
        let mut app = App {
            nav: InputNav::start(ctx.clone()),
            icons: IconLoader::start(ctx.clone()),
            sessions: SessionManager::new(),
            checks: SystemChecks::new(scan),
            lib,
            ctx,
            settings,
            settings_base,
            settings_seen: store::Settings::modified(),
            view: View::Library,
            snack: None,
            reset_undo: None,
            welcome,
            together: together::State::default(),
            settings_tab: settings::Tab::General,
        };
        app.rebuild();
        app
    }

    fn rebuild(&mut self) {
        self.lib.rebuild(&self.settings);
    }

    fn rescan(&mut self) {
        self.lib.rescan(&self.ctx, &self.settings);
    }

    fn hidden(&self, key: &str) -> bool {
        self.settings.hidden.iter().any(|h| h == key)
    }

    fn in_category(&self, g: &Game, key: &str, c: Category) -> bool {
        let hidden = self.hidden(key);
        match c {
            Category::Hidden => hidden,
            _ if hidden => false,
            Category::All => true,
            Category::Recent => self.lib.recent.contains_key(key),
            Category::Source(s) => g.source == s && !(s == Source::Added && !self.is_added(key)),
        }
    }

    /// Where a game came from, as the player sees it.
    fn origin(&self, g: &Game, key: &str) -> &'static str {
        if g.source == Source::Added && !self.is_added(key) { t!("source.recent") } else { source_name(g.source) }
    }

    fn is_added(&self, key: &str) -> bool {
        self.settings.added.iter().any(|p| store::key_of(p) == key)
    }

    fn count(&self, c: Category) -> usize {
        self.lib.entries.iter().filter(|g| self.in_category(g, &store::key_of(&g.exe), c)).count()
    }

    /// The filters with games in them, in order.
    fn categories(&self) -> Vec<Category> {
        let mut cats = vec![Category::All, Category::Recent];
        for s in [Source::Steam, Source::Epic, Source::Gog, Source::Xbox, Source::Folder, Source::Added] {
            if self.count(Category::Source(s)) > 0 {
                cats.push(Category::Source(s));
            }
        }
        if self.count(Category::Hidden) > 0 {
            cats.push(Category::Hidden);
        }
        cats
    }

    /// What the grid shows: the filter, or All narrowed by the search.
    fn visible(&self) -> Vec<usize> {
        let needle = self.lib.search.trim().to_lowercase();
        let shown = self.lib.shown();
        let mut v: Vec<usize> = self
            .lib
            .entries
            .iter()
            .enumerate()
            .filter(|(_, g)| {
                let key = store::key_of(&g.exe);
                self.in_category(g, &key, shown) && (needle.is_empty() || g.name.to_lowercase().contains(&needle) || key.contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();
        if shown == Category::Recent {
            v.sort_by_key(|i| std::cmp::Reverse(self.lib.recent.get(&store::key_of(&self.lib.entries[*i].exe)).copied()));
        }
        v
    }

    fn say(&mut self, msg: impl Into<String>, ok: bool) {
        self.snack = Some(Snack { text: msg.into(), ok, at: Instant::now() });
    }

    /// peerparty.exe's arguments for a game with these options.
    fn command(&self, g: &Game, o: &GameOptions) -> Vec<String> {
        let mut args = Vec::new();
        // Never --copy-saves: that is for one start only (`plan`), not for a command line
        // kept in a .bat or copied from the game's page.
        if o.uses_split_screen() && g.source != Source::Xbox {
            args.push("--split-screen".into());
            args.push("--instances".into());
            args.push(o.players.clamp(2, 4).to_string());
            args.push("--layout".into());
            args.push(o.effective_split_layout().into());
            if let Some(inputs) = o.split_inputs_arg() {
                args.push("--inputs".into());
                args.push(inputs);
            }
        } else if o.players >= 2 && g.source != Source::Xbox {
            args.push("--multi-instance".into());
        }
        args.push(o.exe.clone().unwrap_or_else(|| g.exe.clone()).to_string_lossy().into_owned());
        args.extend(store::split_args(&o.args));
        args
    }

    fn play(&mut self, key: &str) {
        let Some(g) = self.lib.game(key).cloned() else { return };
        let mut o = self.settings.options(key);
        // An Xbox app game runs one copy per PC.
        if g.source == Source::Xbox {
            o.players = 1;
            o.split_screen = false;
        }
        // No bursts of copies, whatever asks (a stuck key, a held button, a script):
        // another instance only once the last one has had a few seconds to start.
        if self.sessions.sessions.iter().any(|s| s.key == key && s.started.elapsed() < START_GUARD) {
            self.say(t!("snack.still_starting", game = g.name), false);
            return;
        }
        // A copy waiting for its turn (the two-player start's second): Play starts it
        // now, rather than turning the player away or adding a third.
        if self.sessions.pending.iter().any(|p| p.key == key) {
            self.start_pending_now(key);
            return;
        }
        let running = self.sessions.running(key);
        let first = running.last().map_or(1, |p| p + 1);
        let split_players = if o.uses_split_screen() && running.is_empty() && g.source != Source::Xbox { o.players.clamp(2, 4) } else { 0 };
        let plan = plan(self.command(&g, &o), o.players, o.split_screen, !running.is_empty(), g.source == Source::Xbox, self.sessions.copy_saves.contains(key));
        if !self.spawn(key, &g.name, plan.now, first, split_players) {
            return;
        }
        // "Copy Player 1's saves to Player N" is for one start: off once a start used it.
        if plan.copied {
            self.sessions.copy_saves.remove(key);
        }
        self.sessions.spark = Some((Instant::now(), self.sessions.next_session.wrapping_mul(7919)));
        match plan.later {
            Some(args) => {
                let delay = self.settings.second_delay.max(1);
                self.sessions.pending.push(PendingStart { at: Instant::now() + Duration::from_secs(delay as u64), key: key.into(), name: g.name.clone(), args, player: first + 1 });
                // The window minimizes once the second copy has started (`tick`): until
                // then its countdown, Start now and Cancel stay in view.
                self.say(t!("snack.started_two", game = g.name, n = delay), true);
            }
            None if split_players >= 2 => {
                self.say(t!("snack.started_split", game = g.name, n = split_players), true);
                self.minimize_after_start();
            }
            None => {
                self.say(t!("snack.started", game = g.name), true);
                self.minimize_after_start();
            }
        }
    }

    /// Starts a game's waiting copy now (Play, or Start now on the Now playing card).
    fn start_pending_now(&mut self, key: &str) {
        let Some(i) = self.sessions.pending.iter().position(|p| p.key == key) else { return };
        let p = self.sessions.pending.remove(i);
        if self.spawn(&p.key, &p.name, p.args, p.player, 0) {
            self.sessions.spark = Some((Instant::now(), self.sessions.next_session.wrapping_mul(7919)));
            self.say(t!("snack.started", game = p.name), true);
            self.minimize_after_start();
        }
    }

    /// Close all on the Now playing card.
    fn close_all(&mut self) {
        let asked = self.sessions.close_all();
        self.say(t!("snack.closing_all", n = asked), asked > 0);
    }

    /// Cancel on the Now playing card: the waiting copy won't start.
    fn cancel_pending(&mut self, key: &str) {
        if let Some(i) = self.sessions.pending.iter().position(|p| p.key == key) {
            let p = self.sessions.pending.remove(i);
            self.say(t!("snack.copy_cancelled", game = p.name, player = p.player), true);
        }
    }

    /// "After launch: minimize": out of the game's way, once the spark has shown.
    fn minimize_after_start(&self) {
        if self.settings.after_launch == "minimize" {
            let ctx = self.ctx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(700));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            });
        }
    }

    /// Starts peerparty.exe for a game; a thread of its own waits for it to end.
    fn spawn(&mut self, key: &str, name: &str, args: Vec<String>, player: u8, split_players: u8) -> bool {
        match self.sessions.spawn(&self.ctx, key, name, args, player, split_players) {
            Ok(()) => true,
            Err(e) => {
                self.say(t!("snack.cant_start", error = e), false);
                false
            }
        }
    }

    /// Picks up settings a running game's overlay changed (they live in the same file).
    fn reload_prefs(&mut self) {
        let m = store::Settings::modified();
        if m.is_some() && m != self.settings_seen {
            let loaded = Settings::load();
            if loaded.unread {
                return;
            }
            self.settings_seen = m;
            self.settings.prefs = loaded.prefs.clone();
            self.settings_base.prefs = loaded.prefs;
        }
    }

    fn save_settings(&mut self) {
        let saved = self.settings.save_over(&self.settings_base);
        self.settings = saved.clone();
        self.settings_base = saved;
        self.settings_seen = store::Settings::modified();
    }

    /// Starts due second copies, notices games that ended, and keeps the process out of
    /// a game's way while the window is behind it.
    fn tick(&mut self) {
        let now = Instant::now();
        self.reload_prefs();
        if let Some(rx) = &self.checks.fixing {
            match rx.try_recv() {
                Ok((msg, ok)) => {
                    self.checks.fixing = None;
                    self.checks.firewall.forget();
                    self.say(msg, ok);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.ctx.request_repaint_after(Duration::from_millis(300)),
                Err(_) => self.checks.fixing = None,
            }
        }
        if let Some(rx) = &self.checks.inviting {
            match rx.try_recv() {
                Ok(taken) => {
                    self.checks.inviting = None;
                    self.say(if taken { t!("snack.invite_sent") } else { t!("snack.invite_none") }, taken);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.ctx.request_repaint_after(Duration::from_millis(100)),
                Err(_) => self.checks.inviting = None,
            }
        }
        // Copies that ended first: a first copy that failed at once takes its waiting
        // second copy with it, even when that one is due in this same frame.
        while let Ok((id, code)) = self.sessions.ended_rx.try_recv() {
            let Some(i) = self.sessions.sessions.iter().position(|s| s.id == id) else { continue };
            let s = self.sessions.sessions.remove(i);
            // The game's last copy closed before the next one's turn: that one isn't
            // started on its own after everything has closed.
            if !self.sessions.sessions.iter().any(|o| o.key == s.key) {
                self.sessions.pending.retain(|p| p.key != s.key);
            }
            // PeerParty returns the game's own exit code; only a quick failure is news.
            if code != Some(0) && s.started.elapsed() < Duration::from_secs(20) {
                let text = s.stderr.lock().unwrap().clone();
                let why = why_text(&text).unwrap_or_else(|| t!("snack.exited_at_once").to_string());
                self.say(t!("snack.didnt_start", game = s.name, n = s.player, why = why), false);
            }
            self.lib.reload_recent();
        }
        let (due, wait): (Vec<PendingStart>, Vec<PendingStart>) = std::mem::take(&mut self.sessions.pending).into_iter().partition(|p| p.at <= now);
        self.sessions.pending = wait;
        let mut started = false;
        for p in due {
            started |= self.spawn(&p.key, &p.name, p.args, p.player, 0);
        }
        if started {
            // Two players on this PC: the window kept the countdown in view until now.
            self.minimize_after_start();
        }
        if let Some(next) = self.sessions.pending.iter().map(|p| p.at).min() {
            // The countdown shows seconds.
            self.ctx.request_repaint_after(next.saturating_duration_since(now).min(Duration::from_secs(1)));
        }
        if let Some(lasts) = self.snack.as_ref().and_then(|s| s.lasts().map(|l| (l, s.at))) {
            let left = lasts.0.saturating_sub(lasts.1.elapsed());
            if left.is_zero() {
                self.snack = None;
            } else {
                self.ctx.request_repaint_after(left);
            }
        }
        let front = self.ctx.input(|i| i.focused);
        let back = !front && !self.sessions.sessions.is_empty();
        if back != self.sessions.background {
            self.sessions.background = back;
            platform::background(back);
        }
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Select(k) => self.lib.selected = Some(k),
                Action::Play(k) => {
                    self.lib.selected = Some(k.clone());
                    self.play(&k);
                }
                Action::Hide(k, hide) => {
                    self.settings.hidden.retain(|h| *h != k);
                    if hide {
                        if let Some(name) = self.lib.game(&k).map(|g| g.name.clone()) {
                            self.say(t!("snack.hidden", game = name, list = t!("filter.hidden")), true);
                        }
                        if self.lib.category != Category::Hidden && self.lib.selected.as_deref() == Some(k.as_str()) {
                            self.lib.selected = None;
                        }
                        self.settings.hidden.push(k);
                    }
                    self.save_settings();
                }
                Action::Reveal(p) => platform::reveal(&p),
                Action::MakeBat(k) => self.make_bat(&k),
                Action::FixFirewall(exe, public) => self.fix_firewall(exe, public),
            }
        }
    }

    /// Asks Windows (as administrator) to let `exe` through the firewall, off this
    /// thread; if that's declined or fails, opens the firewall's settings instead.
    fn fix_firewall(&mut self, exe: PathBuf, public: bool) {
        if self.checks.fixing.is_some() {
            return;
        }
        let ctx = self.ctx.clone();
        self.checks.fixing = Some(shared::spawn_firewall_fix(self.sessions.peerparty.clone(), exe, public, move || ctx.request_repaint()));
        self.say(t!("snack.firewall_asking"), true);
    }

    /// The firewall's verdict for a game played through PeerParty (none for an Xbox app
    /// game, whose rules are its package's), with its program.
    fn firewall_for(&mut self, key: &str) -> Option<(firewall::Verdict, PathBuf)> {
        let g = self.lib.game(key)?;
        if g.source == Source::Xbox {
            return None;
        }
        let exe = self.settings.options(key).exe.unwrap_or_else(|| g.exe.clone());
        Some((self.checks.firewall.verdict(&exe), exe))
    }

    fn make_bat(&mut self, key: &str) {
        let Some(g) = self.lib.game(key).cloned() else { return };
        let o = self.settings.options(key);
        let safe: String = g.name.chars().filter(|c| !r#"\/:*?"<>|"#.contains(*c)).collect();
        let split = o.uses_split_screen() && g.source != Source::Xbox;
        let file = if split {
            format!("{safe} ({}p split screen).bat", o.players.clamp(2, 4))
        } else if o.players >= 2 && g.source != Source::Xbox {
            format!("{safe} (2 players).bat")
        } else {
            format!("{safe}.bat")
        };
        let dir = self.sessions.peerparty.parent().map(Path::to_path_buf).unwrap_or_default();
        let Some(path) = rfd::FileDialog::new().set_title(t!("dialog.save_bat")).set_directory(&dir).set_file_name(&file).add_filter("Batch file", &["bat"]).save_file() else {
            return;
        };
        let pp = if path.parent() == Some(dir.as_path()) { "%~dp0peerparty.exe".to_string() } else { bat_text(&self.sessions.peerparty.display().to_string()) };
        let args = self.command(&g, &o);
        let second_after = (!split && o.players >= 2 && g.source != Source::Xbox).then_some(self.settings.second_delay);
        let text = bat_file(&g.name, &pp, &args, second_after);
        match std::fs::write(&path, text) {
            Ok(()) => self.say(t!("snack.saved", path = path.display()), true),
            Err(e) => self.say(t!("snack.cant_save", path = path.display(), error = e), false),
        }
    }

    fn add_game(&mut self) {
        let Some(exe) = rfd::FileDialog::new().set_title(t!("dialog.add_game")).add_filter("Programs", &["exe"]).pick_file() else {
            return;
        };
        let key = store::key_of(&exe);
        if !self.settings.added.iter().any(|p| store::key_of(p) == key) {
            self.settings.added.push(exe.clone());
            self.save_settings();
        }
        self.rebuild();
        self.view = View::Library;
        self.lib.selected = Some(key);
        self.lib.search.clear();
        if !self.lib.entries.iter().any(|g| self.in_category(g, &store::key_of(&g.exe), self.lib.category)) {
            self.lib.category = Category::All;
        }
    }

    fn add_folder(&mut self) {
        let Some(dir) = rfd::FileDialog::new().set_title(t!("dialog.add_folder")).pick_folder() else {
            return;
        };
        if !self.settings.folders.contains(&dir) {
            self.settings.folders.push(dir);
            self.save_settings();
            self.rescan();
        }
    }

    /// Asks the game that was last in front (this window is, now) to invite everyone
    /// on the network, off this thread; `tick` says whether a game took it up.
    fn invite_everyone(&mut self) {
        if self.checks.inviting.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::signal_invite(true).unwrap_or(false));
            ctx.request_repaint();
        });
        self.checks.inviting = Some(rx);
    }

    fn request_icons(&mut self, keys: Vec<String>) {
        for key in keys {
            if self.icons.icons.contains_key(&key) {
                continue;
            }
            let Some(g) = self.lib.game(&key) else { continue };
            let exe = self.settings.options(&key).exe.unwrap_or_else(|| g.exe.clone());
            let png = g.art.clone().filter(|a| a.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")));
            let picture = g.cover.clone().or_else(|| g.art.clone());
            self.icons.icons.insert(key.clone(), IconSlot::Loading);
            let _ = self.icons.tx.send(IconJob { key, exe, png, picture });
        }
    }

    // ---- controller and keyboard ----

    /// Turns controller presses into what they do here. The game grid takes them itself
    /// (`pad_fire`); anywhere else the d-pad becomes the arrow keys and A becomes Enter,
    /// so they move egui's focus and press what it's on. B goes back, Y to the next
    /// place, the shoulders through the library's filters or Settings' sections.
    fn controller(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let front = raw.focused && platform::in_front();
        self.nav.pads.set_front(front);
        let now = Instant::now();
        let held = if front { self.nav.pads.held() } else { 0 };
        let (b, pressed) = self.nav.pad_gate.filter(front, held, now);
        if !front {
            self.nav.repeat.reset();
            return;
        }
        let dirs = b & (pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT);
        let fire = pressed | self.nav.repeat.step(dirs, now);
        if fire == 0 {
            return;
        }
        self.nav.pad_mode = true;
        widgets::set_nav_mode(true);
        let focused = ctx.memory(|m| m.focused());
        let popup = egui::Popup::is_any_open(ctx);
        let key = |raw: &mut egui::RawInput, k: Key| {
            for pressed in [true, false] {
                raw.events.push(egui::Event::Key { key: k, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE });
            }
        };
        // B dismisses a problem's snack first.
        if fire & pad::EAST != 0 && !popup && self.welcome.is_none() && self.snack.as_ref().is_some_and(|s| !s.ok) {
            self.snack = None;
            return;
        }
        if self.welcome.is_some() {
            if fire & pad::EAST != 0 {
                if let Some(w) = self.welcome.as_mut() {
                    w.back();
                }
            }
            if focused.is_none() && fire & (pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT | pad::SOUTH) != 0 {
                self.nav.focus_first = true;
                return;
            }
            self.keys_for(raw, fire, &key);
            return;
        }
        if fire & pad::NORTH != 0 && !popup {
            let i = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
            self.open_view(View::ALL[(i + 1) % View::ALL.len()], true);
            return;
        }
        if fire & (pad::LB | pad::RB) != 0 && !popup {
            let d = if fire & pad::RB != 0 { 1 } else { -1 };
            match self.view {
                View::Library => {
                    let cats = self.categories();
                    let i = cats.iter().position(|c| *c == self.lib.shown()).unwrap_or(0) as i32;
                    self.lib.pick_category(cats[(i + d).rem_euclid(cats.len() as i32) as usize]);
                }
                View::Settings => {
                    self.settings_tab = self.settings_tab.step(d);
                    self.nav.focus_first = true;
                }
                View::Together => {}
            }
        }
        if fire & pad::EAST != 0 {
            if popup {
                key(raw, Key::Escape);
            } else if let Some(id) = focused {
                ctx.memory_mut(|m| m.surrender_focus(id));
                if self.view != View::Library {
                    self.view = View::Library;
                }
            } else if self.view != View::Library {
                self.view = View::Library;
            } else if !self.lib.search.is_empty() {
                self.lib.search.clear();
            } else {
                self.lib.selected = None;
            }
            return;
        }
        let in_grid = self.view == View::Library && focused.is_none();
        if in_grid {
            if fire & pad::WEST != 0 && self.lib.selected.is_some() {
                self.nav.focus_play = true;
                return;
            }
            self.nav.pad_fire |= fire;
            return;
        }
        // Down from the library's top bar goes back to the grid.
        if self.view == View::Library && fire & pad::DOWN != 0 && focused.is_some_and(|f| self.lib.top_bar_ids.contains(&f)) {
            if let Some(id) = focused {
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
            return;
        }
        self.keys_for(raw, fire, &key);
    }

    fn keys_for(&self, raw: &mut egui::RawInput, fire: u32, key: &dyn Fn(&mut egui::RawInput, Key)) {
        for (bit, k) in [(pad::UP, Key::ArrowUp), (pad::DOWN, Key::ArrowDown), (pad::LEFT, Key::ArrowLeft), (pad::RIGHT, Key::ArrowRight), (pad::SOUTH, Key::Enter)] {
            if fire & bit != 0 {
                key(raw, k);
            }
        }
    }

    /// The grid's own controller handling: the d-pad moves through the games, A plays.
    fn grid_pad(&mut self, ctx: &egui::Context) {
        let fire = std::mem::take(&mut self.nav.pad_fire);
        if fire == 0 {
            return;
        }
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let keys: Vec<String> = visible.iter().map(|&i| store::key_of(&self.lib.entries[i].exe)).collect();
        let at = self.lib.selected.as_ref().and_then(|k| keys.iter().position(|x| x == k));
        let step = if self.settings.list_view { 1 } else { self.lib.grid_cols.max(1) as i32 };
        if fire & pad::UP != 0 && at.is_some_and(|a| (a as i32) < step) {
            self.nav.focus_first = true;
            ctx.request_repaint();
            return;
        }
        let mv = |d: i32| at.map_or(0, |a| (a as i32 + d).clamp(0, keys.len() as i32 - 1) as usize);
        let next = if fire & pad::RIGHT != 0 {
            Some(mv(1))
        } else if fire & pad::LEFT != 0 {
            Some(mv(-1))
        } else if fire & pad::DOWN != 0 {
            Some(mv(step))
        } else if fire & pad::UP != 0 {
            Some(mv(-step))
        } else {
            None
        };
        if let Some(n) = next {
            self.lib.selected = Some(keys[n].clone());
            self.lib.scroll_to_selected = true;
        }
        if fire & pad::SOUTH != 0 {
            match &self.lib.selected {
                Some(k) => {
                    let k = k.clone();
                    self.nav.pad_gate.played(Instant::now());
                    self.play(&k);
                }
                None => self.lib.selected = Some(keys[0].clone()),
            }
        }
    }

    fn open_view(&mut self, v: View, focus: bool) {
        self.view = v;
        self.nav.focus_first = focus && v != View::Library;
        if v == View::Together {
            self.together.refresh();
        }
    }

    // ---- drawing ----

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        let p = ui.painter().clone();
        p.rect_filled(full, 0.0, SIDEBAR);
        fx::light(&p, full.left_top() + vec2(40.0, 30.0), vec2(260.0, 220.0), ACCENT.gamma_multiply(0.10));
        p.rect_filled(Rect::from_min_max(pos2(full.right() - 1.0, full.top()), full.right_bottom()), 0.0, theme::line());
        ui.add_space(22.0);
        let (head, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
        let mark = Rect::from_center_size(pos2(head.left() + 24.0, head.center().y), vec2(32.0, 32.0));
        brand_mark(ui.painter(), mark);
        ui.painter().text(pos2(head.left() + 46.0, head.center().y), egui::Align2::LEFT_CENTER, "PeerParty", theme::strong(20.0), TEXT);
        ui.add_space(22.0);
        for v in View::ALL {
            let sub = match v {
                View::Library => Some(t!("nav.games", n = self.lib.entries.len())),
                View::Together => Some(together::summary(&self.settings.prefs)),
                View::Settings => None,
            };
            let r = nav_item(ui, v.icon(), v.name(), sub.as_deref(), self.view == v);
            if r.clicked() {
                self.open_view(v, false);
            }
        }
        if self.sessions.sessions.is_empty() {
            self.sessions.confirm_close = false;
        }
        let mut keys: Vec<String> = Vec::new();
        for s in &self.sessions.sessions {
            if !keys.contains(&s.key) {
                keys.push(s.key.clone());
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(14.0);
            ui.label(egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).size(11.0).color(TEXT3));
            if self.sessions.sessions.is_empty() && self.sessions.pending.is_empty() {
                return;
            }
            ui.add_space(8.0);
            let mut invite = false;
            let mut close_all = false;
            let mut confirm_close = self.sessions.confirm_close;
            let mut fix = None;
            let (mut start_now, mut cancel) = (None, None);
            widgets::card(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(t!("nav.now_playing").to_uppercase()).font(theme::strong(11.0)).color(TEXT3));
                for s in &self.sessions.sessions {
                    ui.horizontal(|ui| {
                        let (d, _) = ui.allocate_exact_size(vec2(12.0, 18.0), Sense::hover());
                        fx::dot(ui.painter(), d.center(), 4.0, theme::GOOD);
                        let name = if s.split_players >= 2 {
                            t!("nav.split_session", game = s.name, n = s.split_players)
                        } else if s.player > 1 {
                            t!("nav.instance", game = s.name, n = s.player)
                        } else {
                            s.name.clone()
                        };
                        ui.add(egui::Label::new(egui::RichText::new(name).size(13.0).color(TEXT)).truncate());
                    });
                }
                for p in &self.sessions.pending {
                    let left = p.at.saturating_duration_since(Instant::now()).as_secs() + 1;
                    ui.label(egui::RichText::new(t!("nav.instance_in", game = p.name, player = p.player, n = left)).size(12.0).color(TEXT2));
                    ui.horizontal(|ui| {
                        if widgets::button(ui, None, t!("nav.start_now"), widgets::Kind::Secondary).clicked() {
                            start_now = Some(p.key.clone());
                        }
                        if widgets::icon_button(ui, Icon::Close, t!("common.cancel")).clicked() {
                            cancel = Some(p.key.clone());
                        }
                    });
                }
                for key in &keys {
                    let Some(g) = self.lib.game(key) else { continue };
                    let (g_name, exe) = (g.name.clone(), self.settings.options(key).exe.unwrap_or_else(|| g.exe.clone()));
                    if g.source == Source::Xbox {
                        continue;
                    }
                    let v = self.checks.firewall.verdict(&exe);
                    if v.blocked() {
                        widgets::note(ui, Icon::Shield, theme::WARN, &t!("nav.firewall_blocks", game = g_name));
                        if widgets::button(ui, Some(Icon::Shield), t!("common.fix"), widgets::Kind::Secondary).clicked() {
                            fix = Some(exe);
                        }
                    }
                }
                ui.add_space(2.0);
                if widgets::wide_button(ui, Some(Icon::People), t!("nav.invite_everyone"), widgets::Kind::Secondary, 32.0)
                    .on_hover_text(t!("nav.invite_everyone_tip"))
                    .clicked()
                {
                    invite = true;
                }
                if !self.sessions.sessions.is_empty() {
                    if !confirm_close {
                        if widgets::wide_button(ui, Some(Icon::Close), t!("nav.close_all"), widgets::Kind::Ghost, 28.0)
                            .on_hover_text(t!("nav.close_all_tip"))
                            .clicked()
                        {
                            confirm_close = true;
                        }
                    } else {
                        ui.add(egui::Label::new(egui::RichText::new(t!("nav.close_all_confirm")).size(12.0).color(TEXT)).wrap());
                        ui.horizontal(|ui| {
                            if widgets::button(ui, None, t!("nav.close_all"), widgets::Kind::Danger).clicked() {
                                close_all = true;
                                confirm_close = false;
                            }
                            if widgets::icon_button(ui, Icon::Close, t!("common.cancel")).clicked() {
                                confirm_close = false;
                            }
                        });
                    }
                }
            });
            if invite {
                self.invite_everyone();
            }
            self.sessions.confirm_close = confirm_close;
            if close_all {
                self.close_all();
            }
            if let Some(key) = start_now {
                self.start_pending_now(&key);
            }
            if let Some(key) = cancel {
                self.cancel_pending(&key);
            }
            if let Some(exe) = fix {
                self.fix_firewall(exe, self.checks.fw_public);
            }
        });
    }

    /// The bottom line: controller hints while a controller is in use, else a count.
    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let r = ui.max_rect();
        let p = ui.painter();
        let y = r.center().y;
        if self.nav.pad_mode {
            let style = self.nav.pads.style;
            let grid = self.view == View::Library && ui.ctx().memory(|m| m.focused()).is_none() && self.welcome.is_none();
            let next = View::ALL[(View::ALL.iter().position(|v| *v == self.view).unwrap_or(0) + 1) % 3].name();
            let mut items: Vec<(Glyph, &str)> = Vec::new();
            if grid {
                items.push((Glyph::South, t!("hint.play")));
                if self.lib.selected.is_some() {
                    items.push((Glyph::West, t!("hint.options")));
                }
                items.push((Glyph::Lb, t!("hint.filter")));
            } else {
                items.push((Glyph::South, t!("hint.select")));
                if self.view == View::Settings {
                    items.push((Glyph::Lb, t!("hint.section")));
                }
            }
            items.push((Glyph::East, t!("hint.back")));
            if self.welcome.is_none() {
                items.push((Glyph::North, next));
            }
            icons::hints(p, pos2(r.left() + 4.0, y), &items, style, TEXT2);
        } else if self.view == View::Library {
            let text = if self.lib.scanning { t!("library.scanning").to_string() } else { t!("library.count", n = self.visible().len()) };
            p.text(pos2(r.left() + 4.0, y), egui::Align2::LEFT_CENTER, text, theme::font(12.5), TEXT3);
        }
    }

    /// The message of the moment, floating over the bottom of the content.
    fn snackbar(&mut self, ctx: &egui::Context, area: Rect) {
        let Some(s) = &self.snack else { return };
        let age = s.at.elapsed().as_secs_f32();
        let left = s.lasts().map_or(f32::MAX, |l| l.as_secs_f32() - age);
        let fade = (age / 0.18).min(1.0).min(left / 0.3).clamp(0.0, 1.0);
        if fade < 1.0 && theme::effects() {
            ctx.request_repaint();
        }
        let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("snack"));
        let p = ctx.layer_painter(layer);
        let g = p.layout(s.text.clone(), theme::font(13.5), TEXT, (area.width() - 120.0).max(200.0));
        // A problem's snack has a close mark: it stays until it's clicked (or B).
        let w = g.size().x + if s.ok { 54.0 } else { 80.0 };
        let rect = Rect::from_center_size(pos2(area.center().x, area.bottom() - 34.0 + (1.0 - fade) * 10.0), vec2(w, g.size().y + 22.0));
        let color = if s.ok { theme::GOOD } else { theme::BAD };
        fx::shadow(&p, rect, 12.0, 24, 8, (120.0 * fade) as u8);
        fx::glow(&p, rect, 12.0, color.gamma_multiply(0.18 * fade), 14.0);
        p.rect_filled(rect, 12.0, theme::CARD_HI.gamma_multiply(fade));
        p.rect_stroke(rect, 12.0, egui::Stroke::new(1.0, color.gamma_multiply(0.45 * fade)), egui::StrokeKind::Inside);
        let icon = if s.ok { Icon::Check } else { Icon::Warning };
        icon.paint(&p, pos2(rect.left() + 20.0, rect.center().y), 15.0, color.gamma_multiply(fade));
        p.galley(pos2(rect.left() + 38.0, rect.center().y - g.size().y / 2.0), g, TEXT.gamma_multiply(fade));
        if s.ok {
            return;
        }
        let hit = egui::Area::new(egui::Id::unique("snack-close"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.min)
            .show(ctx, |ui| ui.allocate_rect(rect, Sense::click()))
            .inner;
        let x = if hit.hovered() { TEXT } else { TEXT2 };
        Icon::Close.paint(&p, pos2(rect.right() - 20.0, rect.center().y), 12.0, x.gamma_multiply(fade));
        #[cfg(test)]
        ctx.data_mut(|d| d.insert_temp(egui::Id::unique("snack-rect"), rect));
        if hit.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.snack = None;
        }
    }
}

/// Text inside a .bat file's line: cmd reads `%` as the start of a variable, even inside
/// quotes, so one that's meant is written twice.
fn bat_text(s: &str) -> String {
    s.replace('%', "%%")
}

/// One argument on a .bat file's command line, read twice: by cmd, which acts on
/// `& | < > ^ ( )` outside quotes (a folder named `Rock & Stone` would end the command
/// at its `&` and run the rest as another), then by peerparty.exe, which splits the
/// line by the C runtime's rules (`main::quote`): there `\"` is a quote inside the
/// argument, so backslashes before a quote are doubled (`-dir=C:\My Saves\` would
/// otherwise run into the next argument). Quoted when it's empty or holds a space or
/// a character cmd acts on.
///
/// An argument with a `"` in it can't simply be quoted: each `"` turns cmd's own
/// quoting on or off, so `a"&x&"b` would run `x`. Then every character cmd acts on,
/// the quotes included, gets cmd's escape `^`: cmd never sees a quote, acts on
/// nothing and drops the carets. A line break can't be in a .bat line at all; control
/// characters other than a tab are left out.
fn bat_arg(a: &str) -> String {
    let a: String = bat_text(a).chars().filter(|c| *c == '\t' || !c.is_control()).collect();
    if a.contains('"') {
        let mut out = String::new();
        for c in crate::crt_quote(&a).chars() {
            if "\"&|<>^()".contains(c) {
                out.push('^');
            }
            out.push(c);
        }
        out
    } else if a.is_empty() || a.contains(|c: char| c.is_whitespace() || "&|<>^()!,;=".contains(c)) {
        crate::crt_quote(&a)
    } else {
        a
    }
}

/// What Play starts, from the game's command (`App::command`).
#[derive(Debug, PartialEq)]
struct Plan {
    /// The copy started now.
    now: Vec<String>,
    /// Two players on this PC: the second copy, started after the wait.
    later: Option<Vec<String>>,
    /// "Copy Player 1's saves to Player N" went into one of them.
    copied: bool,
}

/// Drops leading `--split-screen`, `--instances <n>`, and `--layout <preset>` flags from
/// a command line so adding one more copy alongside a running session starts a single
/// `--multi-instance` copy rather than a second split-screen session.
fn strip_split_flags(args: &[String]) -> Vec<String> {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--split-screen" => i += 1,
            "--instances" | "--layout" | "--inputs" => i += 2,
            _ => break,
        }
    }
    args[i.min(args.len())..].to_vec()
}

/// With a copy running, Play starts exactly one more, with --multi-instance (else the
/// game's single-instance check hands focus to the running copy and quits). With
/// nothing running and split screen on, one peerparty.exe starts every copy in a shared
/// window; with split screen off and two players on this PC, the second copy follows
/// later. `copy_saves` only ever goes to copies after the first: player 1's own save is
/// never replaced.
fn plan(command: Vec<String>, players: u8, split_screen: bool, running: bool, xbox: bool, copy_saves: bool) -> Plan {
    let copy_saves = copy_saves && !xbox;
    let split = split_screen && players >= 2 && !xbox;
    if running {
        let mut now = if command.first().is_some_and(|a| a == "--split-screen") {
            let mut rest = strip_split_flags(&command);
            rest.insert(0, "--multi-instance".into());
            rest
        } else {
            let mut cmd = command;
            if !xbox && cmd.first().is_none_or(|a| a != "--multi-instance") {
                cmd.insert(0, "--multi-instance".into());
            }
            cmd
        };
        let mut copied = false;
        if copy_saves {
            now.insert(1, "--copy-saves".into());
            copied = true;
        }
        return Plan { now, later: None, copied };
    }
    if split {
        let mut now = command;
        let mut copied = false;
        if copy_saves {
            now.insert(1, "--copy-saves".into());
            copied = true;
        }
        return Plan { now, later: None, copied };
    }
    let mut copied = false;
    let later = (players >= 2 && !xbox).then(|| {
        let mut later = command.clone();
        if copy_saves {
            later.insert(1, "--copy-saves".into());
            copied = true;
        }
        later
    });
    Plan { now: command, later, copied }
}

/// The .bat that starts a game through peerparty (`pp`, as the file names it) with
/// `args`; with `second_after`, twice, that many seconds apart, for two players on this
/// PC (`start /b`: peerparty.exe is a windowed program, so it changes nothing for it,
/// and a console program in its place gets no window of its own). Names beyond ASCII
/// are written as UTF-8, which cmd reads once told to (it reads
/// the file in the console's own code page otherwise, and wouldn't find the game).
fn bat_file(name: &str, pp: &str, args: &[String], second_after: Option<u32>) -> String {
    let line = args.iter().map(|a| bat_arg(a)).collect::<Vec<_>>().join(" ");
    // A comment, on one line.
    let name: String = bat_text(name).chars().filter(|c| !c.is_control()).collect();
    let body = match second_after {
        Some(delay) => format!(
            "rem Two instances of {name} on this PC through peerparty, for two players.\r\nstart \"\" /b \"{pp}\" {line}\r\ntimeout /t {delay} /nobreak >nul\r\nstart \"\" /b \"{pp}\" {line}\r\n"
        ),
        None => format!("rem {name} through peerparty.\r\n\"{pp}\" {line} %*\r\nif errorlevel 1 pause\r\n"),
    };
    let utf8 = if body.is_ascii() { "" } else { "chcp 65001 >nul\r\n" };
    format!("@echo off\r\nsetlocal\r\n{utf8}{body}")
}

/// The name of a game's source.
fn source_name(s: Source) -> &'static str {
    match s {
        Source::Steam => "Steam",
        Source::Epic => "Epic",
        Source::Gog => "GOG",
        Source::Xbox => "Xbox",
        Source::Folder => t!("source.folder"),
        Source::Added => t!("source.added"),
    }
}

/// PeerParty's mark: delegates to the shared 3-node brand mark (`ui/shared.rs`).
fn brand_mark(p: &egui::Painter, r: Rect) {
    shared::brand_mark(p, r);
}

/// One place in the sidebar: an icon, a name, a line under it, lit when it's the one
/// showing.
fn nav_item(ui: &mut egui::Ui, icon: Icon, label: &str, sub: Option<&str>, selected: bool) -> egui::Response {
    let h = if sub.is_some() { 52.0 } else { 42.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    let t = theme::anim(ui.ctx(), resp.id.with("sel"), selected);
    let hot = theme::anim(ui.ctx(), resp.id.with("hot"), resp.hovered());
    let p = ui.painter();
    if t > 0.0 {
        fx::gradient(p, rect, 10.0, false, &[(0.0, ACCENT.gamma_multiply(0.30 * t)), (1.0, ACCENT.gamma_multiply(0.04 * t))]);
        let pill = Rect::from_min_size(pos2(rect.left() + 1.0, rect.center().y - 11.0), vec2(3.0, 22.0));
        fx::glow(p, pill, 1.5, ACCENT.gamma_multiply(0.7 * t), 10.0);
        p.rect_filled(pill, 1.5, theme::mix(ACCENT, theme::ACCENT_HI, 0.4).gamma_multiply(t));
    }
    if hot > 0.0 && !selected {
        p.rect_filled(rect, 10.0, Color32::from_white_alpha((10.0 * hot) as u8));
    }
    let ic = theme::mix(TEXT2, theme::ACCENT_HI, t);
    icon.paint(p, pos2(rect.left() + 24.0, rect.center().y), 17.0, ic);
    let tx = rect.left() + 46.0;
    match sub {
        Some(s) => {
            p.text(pos2(tx, rect.center().y - 9.0), egui::Align2::LEFT_CENTER, label, theme::strong(14.5), theme::mix(TEXT2, TEXT, t.max(hot)));
            let g = p.layout_no_wrap(s.to_string(), theme::font(12.0), TEXT3);
            p.with_clip_rect(Rect::from_min_max(pos2(tx, rect.top()), pos2(rect.right() - 8.0, rect.bottom()))).galley(pos2(tx, rect.center().y + 3.0), g, TEXT3);
        }
        None => {
            p.text(pos2(tx, rect.center().y), egui::Align2::LEFT_CENTER, label, theme::strong(14.5), theme::mix(TEXT2, TEXT, t.max(hot)));
        }
    }
    widgets::ring(ui, &resp, 10.0);
    resp
}

/// egui's file loader wants `file:///C:/...` on Windows.
fn file_uri(p: &Path) -> String {
    format!("file:///{}", p.display().to_string().replace('\\', "/"))
}

fn start_scan(ctx: &egui::Context, folders: Vec<PathBuf>, tx: Sender<Found>) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let (inner_tx, inner_rx) = channel();
        let forward = std::thread::spawn(move || {
            for f in inner_rx {
                let done = matches!(f, Found::Done);
                if tx.send(f).is_err() {
                    return;
                }
                ctx.request_repaint();
                if done {
                    return;
                }
            }
        });
        scan::scan(folders, inner_tx);
        let _ = forward.join();
    });
}

/// Icons and glow colours, off the UI thread: a folder's icon.png, else the program's
/// own icon; the glow from the game's picture (or that icon).
fn icon_worker(ctx: egui::Context) -> (Sender<IconJob>, Receiver<IconDone>) {
    let (req_tx, req_rx) = channel::<IconJob>();
    let (res_tx, res_rx) = channel();
    std::thread::spawn(move || {
        for job in req_rx {
            let thumb = |p: &Path, side: u32| {
                image::open(p).ok().map(|img| {
                    let img = img.thumbnail(side, side).to_rgba8();
                    platform::Rgba { width: img.width() as usize, height: img.height() as usize, pixels: img.into_raw() }
                })
            };
            let icon = job.png.as_deref().and_then(|p| thumb(p, 128)).or_else(|| platform::exe_icon(&job.exe, 128));
            let glow = job.picture.as_deref().and_then(|p| thumb(p, 24)).map(|t| t.pixels).or_else(|| icon.as_ref().map(|i| i.pixels.clone()));
            let glow = glow.and_then(|px| theme::glow_of(&px));
            if res_tx.send(IconDone { key: job.key, icon, glow }).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
    (req_tx, res_rx)
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // The pointer hides the focus ring; the keyboard and controller show it.
        for e in &raw.events {
            match e {
                egui::Event::PointerMoved(_) | egui::Event::PointerButton { .. } | egui::Event::MouseWheel { .. } => {
                    widgets::set_nav_mode(false);
                    self.nav.pad_mode = false;
                }
                egui::Event::Key { key: Key::Tab | Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight, pressed: true, .. } => {
                    widgets::set_nav_mode(true);
                    self.nav.pad_mode = false;
                }
                _ => {}
            }
        }
        self.controller(ctx, raw);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.lib.poll_scan(&self.settings);
        self.icons.poll(ctx);
        self.updates_tick(ctx);
        self.tick();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F)) {
            self.view = View::Library;
            self.lib.focus_search = true;
        }
        let typing = ctx.egui_wants_keyboard_input();
        if self.welcome.is_none() && ctx.input(|i| i.key_pressed(Key::Escape)) && ctx.memory(|m| m.focused()).is_none() && !egui::Popup::is_any_open(&ctx) {
            if self.view != View::Library {
                self.view = View::Library;
            } else if self.lib.selected.is_some() {
                self.lib.selected = None;
            } else {
                self.lib.search.clear();
            }
        }
        let enter = self.welcome.is_none() && !typing && self.view == View::Library && ctx.memory(|m| m.focused()).is_none() && ctx.input(|i| i.key_pressed(Key::Enter));
        if self.view == View::Library && self.welcome.is_none() {
            if !typing && ctx.memory(|m| m.focused()).is_none() && !egui::Popup::is_any_open(&ctx) {
                for (k, bit) in [(Key::ArrowUp, pad::UP), (Key::ArrowDown, pad::DOWN), (Key::ArrowLeft, pad::LEFT), (Key::ArrowRight, pad::RIGHT)] {
                    if ctx.input(|i| i.key_pressed(k)) {
                        self.nav.pad_fire |= bit;
                    }
                }
            }
            self.grid_pad(&ctx);
        }

        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(232.0)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin::symmetric(14, 0)))
            .show(ui, |ui| self.sidebar(ui));
        if let Some(key) = self.lib.selected.clone().filter(|_| self.view == View::Library) {
            egui::Panel::right("game")
                .resizable(false)
                .exact_size(384.0)
                .frame(egui::Frame::new().fill(theme::SURFACE).inner_margin(Margin::ZERO))
                .show(ui, |ui| self.details(ui, key));
        }
        egui::Panel::bottom("hints")
            .exact_size(34.0)
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::symmetric(22, 0)))
            .show(ui, |ui| self.bottom_bar(ui));
        let mut content = Rect::NOTHING;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin { left: 26, right: 18, top: 0, bottom: 0 })).show(ui, |ui| {
            content = ui.max_rect();
            let p = ui.painter();
            let full = ui.clip_rect();
            fx::light(p, pos2(full.left() + full.width() * 0.3, full.top() - 40.0), vec2(full.width() * 0.6, 280.0), ACCENT.gamma_multiply(0.07));
            fx::light(p, pos2(full.right() + 40.0, full.top() + full.height() * 0.35), vec2(320.0, full.height() * 0.5), theme::VIOLET.gamma_multiply(0.05));
            match self.view {
                View::Library => self.library(ui),
                View::Together => self.together_page(ui),
                View::Settings => self.settings_page(ui),
            }
        });
        if self.welcome.is_some() {
            self.welcome_modal(&ctx);
        }
        self.snackbar(&ctx, content);
        if enter {
            if let Some(k) = self.lib.selected.clone() {
                self.play(&k);
            }
        }
    }
}

/// Takes the focus for the first control of a page when asked.
fn first_focus(r: &egui::Response, flag: &mut bool) {
    if std::mem::take(flag) {
        r.request_focus();
        r.scroll_to_me(Some(egui::Align::Center));
    }
}

/// The pointer, if it's over `rect`.
fn hover_in(ui: &egui::Ui, rect: Rect) -> Option<Pos2> {
    ui.ctx().pointer_hover_pos().filter(|p| rect.contains(*p))
}

/// Why a game didn't start, from what peerparty.exe wrote: its `peerparty-why:` line in
/// the player's language, else its last line as it is (English), else None.
fn why_text(stderr: &str) -> Option<String> {
    let reason = stderr.lines().rev().find_map(|l| l.strip_prefix("peerparty-why: ")).map(|r| {
        let (key, value) = r.split_once('\t').unwrap_or((r, ""));
        (key.trim().to_string(), value.trim().to_string())
    });
    let said = match reason.as_ref().map(|(k, v)| (k.as_str(), v.clone())) {
        Some(("missing", v)) => Some(t!("why.missing", path = v)),
        Some(("not_program", v)) => Some(t!("why.not_program", path = v)),
        Some(("machine", _)) => Some(t!("why.machine").to_string()),
        Some(("elevation", _)) => Some(t!("why.elevation").to_string()),
        Some(("os", v)) => Some(t!("why.os", error = v)),
        Some(("room", _)) => Some(t!("why.room").to_string()),
        Some(("inject", _)) => Some(t!("why.inject").to_string()),
        Some(("exit", v)) => Some(t!("why.exit", code = v)),
        Some(("exit_dll", _)) => Some(t!("why.exit_dll").to_string()),
        Some(("anticheat", v)) => Some(t!("why.anticheat", file = v)),
        _ => None,
    };
    said.or_else(|| {
        let last = stderr.lines().rev().find(|l| !l.trim().is_empty() && !l.starts_with("peerparty-why: "))?;
        Some(last.trim_start_matches("peerparty: ").to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::raster::{Canvas, Textures};

    fn game(name: &str, source: Source) -> Game {
        let exe = PathBuf::from(format!(r"C:\Games\{name}\{}.exe", name.replace(' ', "")));
        Game { name: name.into(), dir: exe.parent().unwrap().to_path_buf(), exe, source, cover: None, art: None }
    }

    #[test]
    fn a_reason_line_is_said_in_words_and_anything_else_is_shown_as_it_is() {
        let said = why_text("peerparty: C:\\x.exe: The system cannot find the file specified. (os error 2)\npeerparty-why: missing\tC:\\x.exe\n");
        assert_eq!(said.as_deref(), Some(t!("why.missing", path = "C:\\x.exe").as_str()));
        let said = why_text("peerparty: game exited after 0.2 s with exit code 1 (0x00000001)\npeerparty-why: exit\t0x00000001\n");
        assert_eq!(said.as_deref(), Some(t!("why.exit", code = "0x00000001").as_str()));
        // A reason this window doesn't know: the English line.
        assert_eq!(why_text("peerparty: something new\npeerparty-why: later\t\n").as_deref(), Some("something new"));
        assert_eq!(why_text("\n"), None);
    }

    fn sample() -> Vec<Game> {
        [
            ("Lantern Keep", Source::Steam),
            ("Orbit Couriers", Source::Epic),
            ("Moss and Mortar", Source::Gog),
            ("Tidewatch", Source::Folder),
            ("Paper Comets", Source::Steam),
            ("Glasshouse Derby", Source::Xbox),
            ("Night Market Heist", Source::Folder),
            ("Cinder Valley", Source::Steam),
            ("Quiet Harbor", Source::Folder),
            ("Starfall Arcade", Source::Epic),
            ("Foxglove Rally", Source::Steam),
            ("Brick Signal", Source::Folder),
        ]
        .into_iter()
        .map(|(n, s)| game(n, s))
        .collect()
    }

    /// A search looks through All whatever filter is picked (Hidden searches the hidden
    /// games), and picking another filter clears it.
    #[test]
    fn a_search_looks_through_all() {
        let ctx = egui::Context::default();
        let mut app = App::with(ctx, Settings { onboarded: true, ..Settings::default() }, false);
        app.lib.scanned = sample();
        app.rebuild();
        let names = |a: &App| a.visible().into_iter().map(|i| a.lib.entries[i].name.clone()).collect::<Vec<_>>();
        let tide = store::key_of(&app.lib.entries.iter().find(|g| g.name == "Tidewatch").unwrap().exe);
        app.lib.recent = HashMap::from([(tide.clone(), 1)]);
        app.lib.category = Category::Recent;
        assert_eq!(names(&app), ["Tidewatch"]);
        app.lib.search = "lantern".into();
        assert_eq!(app.lib.shown(), Category::All);
        assert_eq!(names(&app), ["Lantern Keep"], "found outside Recent");
        app.lib.category = Category::Source(Source::Epic);
        assert_eq!(names(&app), ["Lantern Keep"], "found outside Epic");
        app.lib.pick_category(Category::All);
        assert_eq!(app.lib.search, "lantern", "All keeps the search");
        app.lib.pick_category(Category::Recent);
        assert!(app.lib.search.is_empty(), "another filter clears it");
        assert_eq!(names(&app), ["Tidewatch"]);
        // Hidden: the search stays among the hidden games.
        app.settings.hidden.push(tide);
        app.lib.pick_category(Category::Hidden);
        app.lib.search = "tide".into();
        assert_eq!(names(&app), ["Tidewatch"]);
        app.lib.search = "lantern".into();
        assert!(names(&app).is_empty());
    }

    #[test]
    fn bat_args_are_quoted_for_cmd_and_the_c_runtime() {
        assert_eq!(bat_arg("-windowed"), "-windowed");
        assert_eq!(bat_arg(""), r#""""#);
        assert_eq!(bat_arg(r"C:\Games\Rock & Stone\x.exe"), r#""C:\Games\Rock & Stone\x.exe""#);
        // A backslash before the closing quote is doubled; one not before a quote isn't.
        assert_eq!(bat_arg(r"-dir=C:\My Saves\"), r#""-dir=C:\My Saves\\""#);
        assert_eq!(bat_arg(r"C:\Saves\"), r"C:\Saves\");
        assert_eq!(bat_arg("100%"), "100%%");
        // A quote inside: escaped for the C runtime, and everything cmd would act on
        // (the quotes too) carries a caret.
        assert_eq!(bat_arg(r#"a"&x&"b"#), r#"^"a\^"^&x^&\^"b^""#);
        assert_eq!(bat_arg(r#"say \"hi\""#), r#"^"say \\\^"hi\\\^"^""#);
        // A line break would end the command; it is left out.
        assert_eq!(bat_arg("one\r\ntwo"), "onetwo");
        assert_eq!(bat_arg("a\tb"), "\"a\tb\"");
    }

    /// The test binary stands in for peerparty.exe in `a_bat_passes_its_arguments_intact`:
    /// started with PEERPARTY_TEST_ARGV naming a folder, it writes the arguments it was
    /// given there and exits, before the test harness would read them as test names.
    #[cfg(windows)]
    #[used]
    #[unsafe(link_section = ".CRT$XCU")]
    static ARGV_ECHO: extern "C" fn() = argv_echo;

    #[cfg(windows)]
    extern "C" fn argv_echo() {
        let Some(dir) = std::env::var_os("PEERPARTY_TEST_ARGV") else { return };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let path = Path::new(&dir).join(format!("{}.json", std::process::id()));
        let tmp = path.with_extension("tmp");
        let _ = std::fs::write(&tmp, serde_json::to_string(&args).unwrap_or_default()).and_then(|_| std::fs::rename(&tmp, &path));
        std::process::exit(0);
    }

    /// The .bat files the launcher writes, run by cmd: every argument arrives as it was
    /// meant, in one start and in two (`start`), and nothing in a name or an argument
    /// runs as a command of its own.
    #[cfg(windows)]
    #[test]
    fn a_bat_passes_its_arguments_intact() {
        use std::os::windows::process::CommandExt;
        let dir = std::env::temp_dir().join(format!("peerparty-bat-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran.txt");
        let inject = format!("echo x>{}", marker.display());
        assert!(!marker.display().to_string().contains(' '), "the test folder has a space in it: {}", marker.display());
        let exe = std::env::current_exe().unwrap();
        let cases: Vec<Vec<String>> = vec![
            vec![r"C:\Games\Lantern Keep\LanternKeep.exe".into()],
            vec![r"C:\Games\Rock & Stone\x.exe".into(), "-windowed".into()],
            vec![r"C:\Games\x.exe".into(), r"-savedir=C:\My Saves\".into(), "-next".into()],
            vec![r"C:\Games\x.exe".into(), r"C:\Saves\".into(), r"\\server\share\".into()],
            vec![r"C:\Games\x.exe".into(), format!(r#"a"&{inject}&"b"#)],
            vec![r"C:\Games\x.exe".into(), format!(r#"-name="a" & {inject} & rem ""#)],
            vec![r"C:\Games\x.exe".into(), r#"say \"hi\" \\"#.into(), r#"end\""#.into(), "\"".into()],
            vec![r"C:\Games\x.exe".into(), "%USERNAME%".into(), "100%".into(), "%".into(), "".into()],
            vec![r"C:\Games\x.exe".into(), "^caret^".into(), "(paren)".into(), "<in>|out".into(), "!bang!".into(), "a,b;c=d".into()],
            vec![r"C:\Games\Nachtmarkt Größe\名前.exe".into(), "tab\there".into()],
        ];
        let bat = dir.join("start.bat");
        for (i, args) in cases.iter().enumerate() {
            for second in [None, Some(0)] {
                std::fs::write(&bat, bat_file(&format!("Rock & {inject} ^ 100%"), &bat_text(&exe.display().to_string()), args, second)).unwrap();
                // Copies made with `start` run on their own: wait for both.
                let want = if second.is_some() { 2 } else { 1 };
                // Windows now and then fails to start a process while the other tests
                // start theirs (0xC0000142, a DLL's start-up failed): that copy is
                // missing, not wrong, and the case is run again. A wrong argument or a
                // command that ran fails at once.
                for attempt in 1..=3 {
                    let out = dir.join(format!("out-{i}-{}-{attempt}", second.is_some()));
                    std::fs::create_dir_all(&out).unwrap();
                    // With no console of its own: a console made for each run costs
                    // seconds on some PCs.
                    let status = Command::new("cmd")
                        .creation_flags(0x0800_0000)
                        .args(["/d", "/c"])
                        .arg(&bat)
                        .env("PEERPARTY_TEST_ARGV", &out)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .unwrap();
                    let until = Instant::now() + Duration::from_secs(10);
                    let got = loop {
                        let got: Vec<PathBuf> = std::fs::read_dir(&out).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
                        if got.len() >= want || Instant::now() > until {
                            break got;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    };
                    for p in &got {
                        let seen: Vec<String> = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
                        assert_eq!(&seen, args, "case {i} ({second:?})");
                    }
                    assert!(!marker.exists(), "case {i} ({second:?}): an argument ran as a command");
                    if status.success() && got.len() == want {
                        break;
                    }
                    assert!(attempt < 3, "case {i} ({second:?}): cmd exited with {status}, {} of {want} copies started", got.len());
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Draws the window in a few states to PNGs (in PEERPARTY_PREVIEW_DIR, when set),
    /// through the overlay's software renderer, and checks each draws something.
    /// A problem's snack stays until it's clicked; a good one goes by itself.
    #[test]
    fn a_problem_stays_until_dismissed() {
        crate::i18n::set("en");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let mut app = App::with(ctx.clone(), Settings { onboarded: true, ..Settings::default() }, false);
        let mut frame = eframe::Frame::_new_kittest();
        let mut t = 0.0;
        let mut run = |app: &mut App, events: Vec<egui::Event>| {
            t += 0.5;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0))),
                time: Some(t),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                eframe::App::logic(app, &ui.ctx().clone(), &mut frame);
                eframe::App::ui(app, ui, &mut frame);
            });
            out.textures_delta.clear();
        };
        let long_ago = Instant::now().checked_sub(Duration::from_secs(120)).unwrap();
        app.say("saved", true);
        app.snack.as_mut().unwrap().at = long_ago;
        run(&mut app, vec![]);
        assert!(app.snack.is_none(), "a good snack goes after a while");
        app.say("couldn't fix the firewall", false);
        app.snack.as_mut().unwrap().at = long_ago;
        run(&mut app, vec![]);
        run(&mut app, vec![]);
        assert!(app.snack.is_some(), "a problem stays");
        let rect: Rect = ctx.data(|d| d.get_temp(egui::Id::unique("snack-rect"))).unwrap();
        let at = rect.right_center() - vec2(20.0, 0.0);
        let click = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        run(&mut app, vec![egui::Event::PointerMoved(at), click(true)]);
        run(&mut app, vec![click(false)]);
        assert!(app.snack.is_none(), "a click dismisses it");
    }

    /// Play's plan: with a copy running, exactly one copy more, with --multi-instance;
    /// with nothing running and two players, the second copy later. "Start from player
    /// 1's saves" only ever goes to a copy after the first, and never into the command
    /// a .bat or the game's page shows.
    #[test]
    fn play_starts_one_copy_more_and_copies_saves_only_into_a_later_one() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let one = s(&[r"C:\g\a.exe"]);
        let two = s(&["--multi-instance", r"C:\g\a.exe"]);
        let copied = s(&["--multi-instance", "--copy-saves", r"C:\g\a.exe"]);
        assert_eq!(plan(one.clone(), 1, false, false, false, true), Plan { now: one.clone(), later: None, copied: false });
        assert_eq!(plan(two.clone(), 2, false, false, false, true), Plan { now: two.clone(), later: Some(copied.clone()), copied: true });
        assert_eq!(plan(two.clone(), 2, false, false, false, false), Plan { now: two.clone(), later: Some(two.clone()), copied: false });
        for (cmd, players) in [(one.clone(), 1), (two.clone(), 2)] {
            assert_eq!(plan(cmd, players, false, true, false, true), Plan { now: copied.clone(), later: None, copied: true }, "{players}");
        }
        assert_eq!(plan(one.clone(), 1, false, true, false, false).now, two);
        assert_eq!(plan(one.clone(), 1, false, true, true, true), Plan { now: one, later: None, copied: false }, "an Xbox app game: one copy");
        // Split screen: one peerparty.exe runs all copies; copy-saves goes on that command
        // (start_session strips it for player 1). Adding another copy while running strips
        // the split flags and launches one --multi-instance copy.
        let split = s(&["--split-screen", "--instances", "3", "--layout", "grid", r"C:\g\a.exe"]);
        let split_copy = s(&["--split-screen", "--copy-saves", "--instances", "3", "--layout", "grid", r"C:\g\a.exe"]);
        assert_eq!(plan(split.clone(), 3, true, false, false, false), Plan { now: split.clone(), later: None, copied: false });
        assert_eq!(plan(split.clone(), 3, true, false, false, true), Plan { now: split_copy, later: None, copied: true });
        assert_eq!(plan(split, 3, true, true, false, true), Plan { now: copied, later: None, copied: true });
        // The command shown and kept in a .bat has no --copy-saves, whatever is switched on.
        let ctx = egui::Context::default();
        let mut app = App::with(ctx, Settings { onboarded: true, ..Settings::default() }, false);
        app.lib.scanned = sample();
        app.rebuild();
        let g = app.lib.entries[0].clone();
        let key = store::key_of(&g.exe);
        app.sessions.copy_saves.insert(key.clone());
        let o = GameOptions { players: 2, ..GameOptions::default() };
        assert!(!app.command(&g, &o).iter().any(|a| a == "--copy-saves"));
        let o_split = GameOptions { players: 4, split_screen: true, split_layout: "vertical".into(), ..GameOptions::default() };
        let mut o_inputs = o_split.clone();
        o_inputs.set_split_input(1, "kbm");
        assert_eq!(o_inputs.split_inputs_arg().as_deref(), Some("auto,kbm,auto,auto"));
        assert_eq!(o_split.split_inputs_arg(), None, "all auto: no --inputs");
        o_inputs.players = 2;
        o_inputs.set_split_input(3, "pad");
        assert_eq!(o_inputs.split_inputs_arg().as_deref(), Some("auto,kbm"), "only the players there are");
        let cmd = app.command(&g, &o_split);
        assert_eq!(&cmd[..5], ["--split-screen", "--instances", "4", "--layout", "vertical"]);
        assert!(!cmd.iter().any(|a| a == "--copy-saves"));
    }

    /// "Copy Player 1's saves to Player N" is never a setting: off when the window starts, off
    /// once a start used it. Play with copy 2 waiting starts it now; Cancel drops it; and
    /// when the game's last copy closes first, it isn't started on its own.
    #[test]
    fn the_save_copy_switch_is_for_one_start_and_a_waiting_copy_can_go_now_or_not() {
        crate::i18n::set("en");
        let ctx = egui::Context::default();
        let mut settings = Settings { onboarded: true, after_launch: "keep".into(), ..Settings::default() };
        let mut app = App::with(ctx.clone(), settings.clone(), false);
        assert!(app.sessions.copy_saves.is_empty(), "off when the window starts");
        app.lib.scanned = sample();
        app.rebuild();
        // Something harmless in peerparty.exe's place: it exits at once.
        app.sessions.peerparty = PathBuf::from(r"C:\Windows\System32\where.exe");
        let key = store::key_of(&app.lib.entries[0].exe);
        settings.games.insert(key.clone(), GameOptions { players: 2, ..GameOptions::default() });
        app.settings.games = settings.games.clone();
        app.sessions.copy_saves.insert(key.clone());
        app.play(&key);
        assert_eq!(app.sessions.sessions.len(), 1);
        assert_eq!(app.sessions.pending.len(), 1);
        assert!(app.sessions.pending[0].args.iter().any(|a| a == "--copy-saves"), "the second copy starts from player 1's saves");
        assert!(app.sessions.copy_saves.is_empty(), "used, so off");
        // Too soon after the first: turned away. Then Play starts the waiting copy now.
        app.play(&key);
        assert_eq!((app.sessions.sessions.len(), app.sessions.pending.len()), (1, 1));
        let long_ago = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
        app.sessions.sessions[0].started = long_ago;
        app.play(&key);
        assert_eq!((app.sessions.sessions.len(), app.sessions.pending.len()), (2, 0));
        assert_eq!(app.sessions.sessions[1].player, 2);
        // A copy running: Play adds exactly one, with nothing waiting after it.
        app.sessions.sessions[1].started = long_ago;
        app.play(&key);
        assert_eq!((app.sessions.sessions.len(), app.sessions.pending.len()), (3, 0));
        // Cancel drops a waiting copy.
        app.sessions.pending.push(PendingStart { at: Instant::now() + Duration::from_secs(30), key: key.clone(), name: "x".into(), args: vec![], player: 4 });
        app.cancel_pending(&key);
        assert!(app.sessions.pending.is_empty());
        // The game's copies all close before copy 2's turn: it isn't started after.
        app.sessions.pending.push(PendingStart { at: Instant::now() + Duration::from_secs(30), key: key.clone(), name: "x".into(), args: vec![], player: 4 });
        for s in &app.sessions.sessions {
            app.sessions.ended_tx.send((s.id, Some(0))).unwrap();
        }
        app.tick();
        assert!(app.sessions.sessions.is_empty());
        assert!(app.sessions.pending.is_empty(), "nothing starts on its own after everything closed");
    }

    #[test]
    fn the_save_copy_switch_shows_at_two_players_or_with_a_copy_running() {
        let ctx = egui::Context::default();
        let mut app = App::with(ctx, Settings { onboarded: true, ..Settings::default() }, false);
        assert!(!app.sessions.copy_saves_offered("k", 1, false));
        assert!(app.sessions.copy_saves_offered("k", 2, false));
        assert!(app.sessions.copy_saves_offered("k", 1, true));
        app.sessions.copy_saves.insert("k".into());
        assert!(app.sessions.copy_saves_offered("k", 1, true));
        assert!(app.sessions.copy_saves.contains("k"), "still on while shown");
        assert!(!app.sessions.copy_saves_offered("k", 1, false));
        assert!(app.sessions.copy_saves.is_empty(), "never comes back on by itself");
        app.sessions.copy_saves.insert("k".into());
        assert!(app.sessions.copy_saves_offered("k", 2, false));
        assert!(app.sessions.copy_saves.contains("k"));
        crate::i18n::set("en");
        assert_eq!(t!("game.copy_saves", player = app.sessions.copy_saves_target("k")), "Copy Player 1's saves to Player 2");
        let at = Instant::now();
        app.sessions.sessions.push(Session { id: 1, key: "k".into(), name: "x".into(), player: 1, split_players: 0, started: at, stderr: Arc::default(), pid: 0 });
        app.sessions.pending.push(PendingStart { at, key: "k".into(), name: "x".into(), args: vec![], player: 2 });
        assert!(!app.sessions.copy_saves_offered("k", 2, true));
        assert!(!app.sessions.copy_saves_offered("k", 1, true));
        assert!(app.sessions.copy_saves.is_empty(), "hidden, so off");
        app.sessions.pending.clear();
        assert!(app.sessions.copy_saves_offered("k", 2, true), "back once copy 2 has started");
        app.sessions.sessions.push(Session { id: 2, key: "k".into(), name: "x".into(), player: 2, split_players: 0, started: at, stderr: Arc::default(), pid: 0 });
        assert_eq!(t!("game.copy_saves", player = app.sessions.copy_saves_target("k")), "Copy Player 1's saves to Player 3");
        // A 3-player split-screen session counts all 3 players as running.
        app.sessions.sessions.clear();
        app.sessions.sessions.push(Session { id: 3, key: "k".into(), name: "x".into(), player: 1, split_players: 3, started: at, stderr: Arc::default(), pid: 0 });
        assert_eq!(app.sessions.running("k"), vec![1, 2, 3]);
        assert_eq!(app.sessions.copy_saves_target("k"), 4);
    }

    #[test]
    fn window_renders() {
        let dir = std::env::var_os("PEERPARTY_PREVIEW_DIR");
        crate::i18n::set(&std::env::var("PEERPARTY_LANG").unwrap_or_else(|_| "en".into()));
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx);
        let mut settings = Settings { onboarded: true, ..Settings::default() };
        settings.prefs.room = peerparty_core::inet::RoomCode::generate().to_string();
        let mut app = App::with(ctx.clone(), settings, false);
        app.lib.scanned = sample();
        app.rebuild();
        app.lib.category = Category::All;
        let (w, h) = std::env::var("PEERPARTY_PREVIEW_SIZE")
            .ok()
            .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
            .unwrap_or((1440usize, 900usize));
        let mut tex = Textures::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut t = 0.0;
        let scenes: [(&str, &dyn Fn(&mut App)); 13] = [
            ("library", &|a: &mut App| a.lib.selected = None),
            ("game", &|a: &mut App| a.lib.selected = Some(store::key_of(&a.lib.entries[3].exe))),
            ("together", &|a: &mut App| a.open_view(View::Together, false)),
            ("settings", &|a: &mut App| a.open_view(View::Settings, false)),
            ("settings-overlay", &|a: &mut App| a.settings_tab = settings::Tab::Overlay),
            ("welcome", &|a: &mut App| {
                a.view = View::Library;
                a.welcome = Some(welcome::Welcome::new(&a.settings.prefs));
            }),
            ("welcome-who", &|a: &mut App| a.welcome.as_mut().unwrap().step = 1),
            ("list", &|a: &mut App| {
                a.welcome = None;
                a.settings.list_view = true;
                a.nav.pad_mode = true;
            }),
            ("game-two", &|a: &mut App| {
                a.settings.list_view = false;
                a.nav.pad_mode = false;
                let key = store::key_of(&a.lib.entries[3].exe);
                a.settings.games.insert(key.clone(), GameOptions { players: 2, ..GameOptions::default() });
                a.sessions.copy_saves.insert(key.clone());
                a.lib.selected = Some(key);
            }),
            ("game-split", &|a: &mut App| {
                let key = store::key_of(&a.lib.entries[3].exe);
                a.settings.games.insert(key.clone(), GameOptions { players: 4, split_screen: true, split_layout: "grid".into(), ..GameOptions::default() });
                a.lib.selected = Some(key);
            }),
            ("game-waiting", &|a: &mut App| {
                let key = store::key_of(&a.lib.entries[3].exe);
                let name = a.lib.entries[3].name.clone();
                a.sessions.sessions.push(Session { id: 9_999, key: key.clone(), name: name.clone(), player: 1, split_players: 0, started: Instant::now(), stderr: Arc::default(), pid: 0 });
                a.sessions.pending.push(PendingStart { at: Instant::now() + Duration::from_secs(9), key, name, args: vec![], player: 2 });
            }),
            ("game-another", &|a: &mut App| {
                a.sessions.pending.clear();
                let key = store::key_of(&a.lib.entries[3].exe);
                a.settings.games.insert(key.clone(), GameOptions { players: 1, ..GameOptions::default() });
                a.sessions.copy_saves.insert(key);
            }),
            ("settings-update", &|a: &mut App| {
                a.sessions.sessions.clear();
                a.lib.selected = None;
                a.open_view(View::Settings, false);
                a.settings_tab = settings::Tab::About;
                crate::update::preview();
            }),
        ];
        for (name, set) in scenes {
            set(&mut app);
            let mut canvas = Canvas::new(w, h);
            for i in 0..4 {
                t += 0.4;
                let mut events = Vec::new();
                if name == "library" || name == "game" {
                    // The pointer over the second card, for its light.
                    events.push(egui::Event::PointerMoved(pos2(520.0, 230.0)));
                }
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(w as f32, h as f32))),
                    time: Some(t),
                    focused: i == 0,
                    events,
                    ..Default::default()
                };
                let out = ctx.run_ui(input, |ui| {
                    eframe::App::logic(&mut app, &ui.ctx().clone(), &mut frame);
                    eframe::App::ui(&mut app, ui, &mut frame);
                });
                tex.apply(out.textures_delta);
                // The frames before the last settle layout and animations; only the last
                // is looked at, so only it is drawn (drawing all four took most of the
                // test's time, unoptimized).
                if i < 3 {
                    continue;
                }
                let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
                canvas.clear(theme::BG);
                tex.draw(&mut canvas, &prims, out.pixels_per_point, (0.0, 0.0));
            }
            let lit = canvas.px.iter().filter(|p| (**p & 0xff_ffff) != 0).count();
            assert!(lit > 100_000, "{name} drew {lit} pixels");
            if let Some(d) = &dir {
                let rgba: Vec<u8> = canvas.px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255]).collect();
                image::RgbaImage::from_raw(w as u32, h as u32, rgba).unwrap().save(std::path::Path::new(d).join(format!("window-{name}.png"))).unwrap();
            }
        }
    }
}
