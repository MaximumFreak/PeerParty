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
mod mainexe;
mod padgate;
mod pads;
mod scan;
mod settings;
pub mod store;
pub mod theme;
mod together;
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

pub fn run() -> eframe::Result {
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
            Ok(Box::new(App::new(cc.egui_ctx.clone(), settings)))
        }),
    )
}

/// Two linked nodes on a rounded tile: two PCs, bridged.
fn window_icon() -> egui::IconData {
    let n = 64usize;
    let mut rgba = vec![0u8; n * n * 4];
    let inside = |x: f32, y: f32| {
        let r = 14.0;
        let (cx, cy) = (x.clamp(r, n as f32 - r), y.clamp(r, n as f32 - r));
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    };
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let i = (y * n + x) * 4;
            if !inside(fx, fy) {
                continue;
            }
            let dot = |cx: f32, cy: f32| (fx - cx).powi(2) + (fy - cy).powi(2) <= 81.0;
            let line = (fy - 32.0).abs() <= 3.0 && (18.0..=46.0).contains(&fx);
            // The tile: blue to violet, top left to bottom right.
            let t = (fx + fy) / (2.0 * n as f32);
            let bg = [(0x5b as f32 + (0x9a - 0x5b) as f32 * t) as u8, (0x8d as f32 + (0x72 - 0x8d) as f32 * t) as u8, 0xff];
            let c = if dot(18.0, 32.0) || dot(46.0, 32.0) || line { [255, 255, 255] } else { bg };
            rgba[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    egui::IconData { rgba, width: n as u32, height: n as u32 }
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
    started: Instant,
    stderr: Arc<Mutex<String>>,
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
    fn lasts(&self) -> Duration {
        Duration::from_secs(if self.ok { 5 } else { 12 })
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

struct App {
    ctx: egui::Context,
    settings: Settings,
    scanned: Vec<Game>,
    entries: Vec<Game>,
    recent: HashMap<String, u64>,
    scanning: bool,
    found_rx: Receiver<Found>,
    icon_tx: Sender<IconJob>,
    icon_rx: Receiver<IconDone>,
    icons: HashMap<String, IconSlot>,
    glows: HashMap<String, Color32>,
    view: View,
    category: Category,
    search: String,
    selected: Option<String>,
    sessions: Vec<Session>,
    next_session: u64,
    ended_tx: Sender<(u64, Option<i32>)>,
    ended_rx: Receiver<(u64, Option<i32>)>,
    pending: Vec<PendingStart>,
    snack: Option<Snack>,
    peerparty: PathBuf,
    focus_search: bool,
    /// When ui.json was last read or written here (the overlay changes it too).
    settings_seen: Option<std::time::SystemTime>,
    /// Controllers, for picking a game from the couch.
    pads: pads::PadThread,
    /// Which presses count (only while this window is in front).
    pad_gate: padgate::PadGate,
    pad_repeat: Option<(u32, Instant)>,
    /// Controller presses for the game grid, from the input hook.
    pad_fire: u32,
    /// The controller was the last thing used (its hints show).
    pad_mode: bool,
    /// The first control of the page just opened takes the focus (controller, keyboard).
    focus_first: bool,
    /// The game panel's Play button takes the focus.
    focus_play: bool,
    /// Columns in the grid as last drawn (up / down move by a row).
    grid_cols: usize,
    /// Scroll the grid to the selected game.
    scroll_to_selected: bool,
    /// The top bar's controls this frame: down from one goes back to the grid.
    top_bar_ids: HashSet<egui::Id>,
    /// Reads Windows Firewall's rules for the warnings.
    firewall: firewall::Checker,
    /// "Also on public networks" for the firewall fix.
    fw_public: bool,
    /// A firewall fix under way: what to say when it's done, and whether it worked.
    fixing: Option<Receiver<(String, bool)>>,
    /// The Play button's spark: when it was pressed, and a seed for its arcs.
    spark: Option<(Instant, u64)>,
    /// The first-run setup, while it shows.
    welcome: Option<welcome::Welcome>,
    /// Play together's state (the code being typed, the blocked players).
    together: together::State,
    /// Settings' open section.
    settings_tab: settings::Tab,
    /// In background mode (a game runs, the window is behind it).
    background: bool,
}

impl App {
    fn new(ctx: egui::Context, settings: Settings) -> App {
        Self::with(ctx, settings, true)
    }

    /// The window's state; `scan` looks for installed games (tests show their own).
    fn with(ctx: egui::Context, settings: Settings, scan: bool) -> App {
        let (found_tx, found_rx) = channel();
        if scan {
            start_scan(&ctx, settings.folders.clone(), found_tx);
        }
        let (icon_tx, icon_rx) = icon_worker(ctx.clone());
        let (ended_tx, ended_rx) = channel();
        // Games start through this same program, with a command line.
        let peerparty = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("peerparty.exe"));
        let welcome = (!settings.onboarded).then(|| welcome::Welcome::new(&settings.prefs));
        let mut app = App {
            pads: pads::PadThread::start(ctx.clone()),
            ctx,
            settings,
            scanned: Vec::new(),
            entries: Vec::new(),
            recent: HashMap::new(),
            scanning: scan,
            found_rx,
            icon_tx,
            icon_rx,
            icons: HashMap::new(),
            glows: HashMap::new(),
            view: View::Library,
            category: Category::All,
            search: String::new(),
            selected: None,
            sessions: Vec::new(),
            next_session: 1,
            ended_tx,
            ended_rx,
            pending: Vec::new(),
            snack: None,
            peerparty,
            focus_search: false,
            settings_seen: store::Settings::modified(),
            pad_gate: padgate::PadGate::default(),
            pad_repeat: None,
            pad_fire: 0,
            pad_mode: false,
            focus_first: false,
            focus_play: false,
            grid_cols: 1,
            scroll_to_selected: false,
            top_bar_ids: HashSet::new(),
            firewall: firewall::Checker::default(),
            fw_public: false,
            fixing: None,
            spark: None,
            welcome,
            together: together::State::default(),
            settings_tab: settings::Tab::General,
            background: false,
        };
        app.reload_recent();
        if !app.recent.is_empty() {
            app.category = Category::Recent;
        }
        app.rebuild();
        app
    }

    fn reload_recent(&mut self) {
        self.recent = store::recent().into_iter().map(|r| (store::key_of(&r.exe), r.when)).collect();
    }

    /// The library: scanned games (one per program), then added programs and recent
    /// launches that no library knows. A game GOG or Epic installed into one of the
    /// player's game folders shows under that folder (its store name kept): only Steam
    /// and Xbox games start through their own client.
    fn rebuild(&mut self) {
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
        for exe in &self.settings.added {
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

    fn rescan(&mut self) {
        let (tx, rx) = channel();
        self.found_rx = rx;
        self.scanned.clear();
        self.scanning = true;
        start_scan(&self.ctx, self.settings.folders.clone(), tx);
        self.reload_recent();
        self.rebuild();
    }

    fn game(&self, key: &str) -> Option<&Game> {
        self.entries.iter().find(|g| store::key_of(&g.exe) == key)
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
            Category::Recent => self.recent.contains_key(key),
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
        self.entries.iter().filter(|g| self.in_category(g, &store::key_of(&g.exe), c)).count()
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

    /// What the grid shows: the category, narrowed by the search.
    fn visible(&self) -> Vec<usize> {
        let needle = self.search.trim().to_lowercase();
        let mut v: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, g)| {
                let key = store::key_of(&g.exe);
                self.in_category(g, &key, self.category) && (needle.is_empty() || g.name.to_lowercase().contains(&needle) || key.contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();
        if self.category == Category::Recent {
            v.sort_by_key(|i| std::cmp::Reverse(self.recent.get(&store::key_of(&self.entries[*i].exe)).copied()));
        }
        v
    }

    fn say(&mut self, msg: impl Into<String>, ok: bool) {
        self.snack = Some(Snack { text: msg.into(), ok, at: Instant::now() });
    }

    fn running(&self, key: &str) -> Vec<u8> {
        let mut p: Vec<u8> = self.sessions.iter().filter(|s| s.key == key).map(|s| s.player).collect();
        p.sort();
        p
    }

    /// peerparty.exe's arguments for a game with these options.
    fn command(&self, g: &Game, o: &GameOptions) -> Vec<String> {
        let mut args = Vec::new();
        if o.players >= 2 {
            args.push("--multi-instance".into());
            if o.copy_saves {
                args.push("--copy-saves".into());
            }
        }
        args.push(o.exe.clone().unwrap_or_else(|| g.exe.clone()).to_string_lossy().into_owned());
        args.extend(store::split_args(&o.args));
        args
    }

    fn play(&mut self, key: &str) {
        let Some(g) = self.game(key).cloned() else { return };
        let mut o = self.settings.options(key);
        // An Xbox app game runs one copy per PC.
        if g.source == Source::Xbox {
            o.players = 1;
        }
        // No bursts of copies, whatever asks (a stuck key, a held button, a script):
        // another instance only once the last one has had a few seconds to start.
        let starting = self.sessions.iter().any(|s| s.key == key && s.started.elapsed() < START_GUARD) || self.pending.iter().any(|p| p.key == key);
        if starting {
            self.say(t!("snack.still_starting", game = g.name), false);
            return;
        }
        // "Run another instance" while one is running: the new copy needs
        // --multi-instance too, or the game's single-instance check hands focus to the
        // running copy and quits.
        let first = self.running(key).last().map_or(1, |p| p + 1);
        let mut args = self.command(&g, &o);
        if first > 1 && o.players < 2 && g.source != Source::Xbox {
            args.insert(0, "--multi-instance".into());
        }
        if !self.spawn(key, &g.name, args.clone(), first) {
            return;
        }
        self.spark = Some((Instant::now(), self.next_session.wrapping_mul(7919)));
        if o.players >= 2 {
            let delay = self.settings.second_delay.max(1);
            self.pending.push(PendingStart { at: Instant::now() + Duration::from_secs(delay as u64), key: key.into(), name: g.name.clone(), args, player: first + 1 });
            self.say(t!("snack.started_two", game = g.name, n = delay), true);
        } else {
            self.say(t!("snack.started", game = g.name), true);
        }
        if self.settings.after_launch == "minimize" {
            // Once the spark has shown.
            let ctx = self.ctx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(700));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            });
        }
    }

    /// Starts peerparty.exe for a game; a thread of its own waits for it to end.
    fn spawn(&mut self, key: &str, name: &str, args: Vec<String>, player: u8) -> bool {
        let mut cmd = Command::new(&self.peerparty);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        match platform::quiet(&mut cmd).spawn() {
            Ok(mut child) => {
                let id = self.next_session;
                self.next_session += 1;
                let stderr = Arc::new(Mutex::new(String::new()));
                let pipe = child.stderr.take();
                let sink = stderr.clone();
                let tx = self.ended_tx.clone();
                let ctx = self.ctx.clone();
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
                self.sessions.push(Session { id, key: key.into(), name: name.into(), player, started: Instant::now(), stderr });
                true
            }
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
            self.settings_seen = m;
            self.settings.prefs = Settings::load().prefs;
        }
    }

    fn save_settings(&mut self) {
        self.settings.save();
        self.settings_seen = store::Settings::modified();
    }

    /// Starts due second copies, notices games that ended, and keeps the process out of
    /// a game's way while the window is behind it.
    fn tick(&mut self) {
        let now = Instant::now();
        self.reload_prefs();
        if let Some(rx) = &self.fixing {
            match rx.try_recv() {
                Ok((msg, ok)) => {
                    self.fixing = None;
                    self.firewall.forget();
                    self.say(msg, ok);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => self.ctx.request_repaint_after(Duration::from_millis(300)),
                Err(_) => self.fixing = None,
            }
        }
        let (due, wait): (Vec<PendingStart>, Vec<PendingStart>) = std::mem::take(&mut self.pending).into_iter().partition(|p| p.at <= now);
        self.pending = wait;
        for p in due {
            self.spawn(&p.key, &p.name, p.args, p.player);
        }
        if let Some(next) = self.pending.iter().map(|p| p.at).min() {
            // The countdown shows seconds.
            self.ctx.request_repaint_after(next.saturating_duration_since(now).min(Duration::from_secs(1)));
        }
        while let Ok((id, code)) = self.ended_rx.try_recv() {
            let Some(i) = self.sessions.iter().position(|s| s.id == id) else { continue };
            let s = self.sessions.remove(i);
            // PeerParty returns the game's own exit code; only a quick failure is news.
            if code != Some(0) && s.started.elapsed() < Duration::from_secs(20) {
                let text = s.stderr.lock().unwrap().clone();
                let why = text.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim_start_matches("peerparty: ").to_string());
                let why = why.unwrap_or_else(|| t!("snack.exited_at_once").to_string());
                self.say(t!("snack.didnt_start", game = s.name, n = s.player, why = why), false);
            }
            self.reload_recent();
        }
        if let Some(s) = &self.snack {
            let left = s.lasts().saturating_sub(s.at.elapsed());
            if left.is_zero() {
                self.snack = None;
            } else {
                self.ctx.request_repaint_after(left);
            }
        }
        let front = self.ctx.input(|i| i.focused);
        let back = !front && !self.sessions.is_empty();
        if back != self.background {
            self.background = back;
            platform::background(back);
        }
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Select(k) => self.selected = Some(k),
                Action::Play(k) => {
                    self.selected = Some(k.clone());
                    self.play(&k);
                }
                Action::Hide(k, hide) => {
                    self.settings.hidden.retain(|h| *h != k);
                    if hide {
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
        if self.fixing.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let peerparty = self.peerparty.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(firewall::fix_or_open(&peerparty, &exe, public));
            ctx.request_repaint();
        });
        self.fixing = Some(rx);
        self.say(t!("snack.firewall_asking"), true);
    }

    /// The firewall's verdict for a game played through PeerParty (none for an Xbox app
    /// game, whose rules are its package's), with its program.
    fn firewall_for(&mut self, key: &str) -> Option<(firewall::Verdict, PathBuf)> {
        let g = self.game(key)?;
        if g.source == Source::Xbox {
            return None;
        }
        let exe = self.settings.options(key).exe.unwrap_or_else(|| g.exe.clone());
        Some((self.firewall.verdict(&exe), exe))
    }

    fn make_bat(&mut self, key: &str) {
        let Some(g) = self.game(key).cloned() else { return };
        let o = self.settings.options(key);
        let safe: String = g.name.chars().filter(|c| !r#"\/:*?"<>|"#.contains(*c)).collect();
        let file = if o.players >= 2 { format!("{safe} (2 players).bat") } else { format!("{safe}.bat") };
        let dir = self.peerparty.parent().map(Path::to_path_buf).unwrap_or_default();
        let Some(path) = rfd::FileDialog::new().set_title(t!("dialog.save_bat")).set_directory(&dir).set_file_name(&file).add_filter("Batch file", &["bat"]).save_file() else {
            return;
        };
        let pp = if path.parent() == Some(dir.as_path()) { "%~dp0peerparty.exe".to_string() } else { self.peerparty.display().to_string() };
        let args = self.command(&g, &o);
        let quoted: Vec<String> = args.iter().map(|a| if a.contains(' ') || a.is_empty() { format!("\"{a}\"") } else { a.clone() }).collect();
        let line = quoted.join(" ");
        let text = if o.players >= 2 {
            format!(
                "@echo off\r\nsetlocal\r\nrem Two instances of {} on this PC through peerparty, for two players.\r\nstart \"\" \"{pp}\" {line}\r\ntimeout /t {} /nobreak >nul\r\nstart \"\" \"{pp}\" {line}\r\n",
                g.name, self.settings.second_delay
            )
        } else {
            format!("@echo off\r\nsetlocal\r\nrem {} through peerparty.\r\n\"{pp}\" {line} %*\r\nif errorlevel 1 pause\r\n", g.name)
        };
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
        self.selected = Some(key);
        self.search.clear();
        if !self.entries.iter().any(|g| self.in_category(g, &store::key_of(&g.exe), self.category)) {
            self.category = Category::All;
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

    fn invite_everyone(&mut self) {
        let mut cmd = Command::new(&self.peerparty);
        cmd.arg("--invite");
        let _ = platform::quiet(&mut cmd).spawn();
        self.say(t!("snack.invite_sent"), true);
    }

    fn request_icons(&mut self, keys: Vec<String>) {
        for key in keys {
            if self.icons.contains_key(&key) {
                continue;
            }
            let Some(g) = self.game(&key) else { continue };
            let exe = self.settings.options(&key).exe.unwrap_or_else(|| g.exe.clone());
            let png = g.art.clone().filter(|a| a.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")));
            let picture = g.cover.clone().or_else(|| g.art.clone());
            self.icons.insert(key.clone(), IconSlot::Loading);
            let _ = self.icon_tx.send(IconJob { key, exe, png, picture });
        }
    }

    /// The colour a game's light takes: its picture's, else one from its name.
    fn glow_of(&self, g: &Game, key: &str) -> Color32 {
        self.glows.get(key).copied().unwrap_or_else(|| theme::mix(theme::tint_for(&g.name), Color32::WHITE, 0.35))
    }

    // ---- controller and keyboard ----

    /// Turns controller presses into what they do here. The game grid takes them itself
    /// (`pad_fire`); anywhere else the d-pad becomes the arrow keys and A becomes Enter,
    /// so they move egui's focus and press what it's on. B goes back, Y to the next
    /// place, the shoulders through the library's filters or Settings' sections.
    fn controller(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let front = raw.focused && platform::in_front();
        self.pads.set_front(front);
        let now = Instant::now();
        let held = if front { self.pads.held() } else { 0 };
        let (b, pressed) = self.pad_gate.filter(front, held, now);
        if !front {
            self.pad_repeat = None;
            return;
        }
        let dirs = b & (pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT);
        let mut fire = pressed;
        match self.pad_repeat {
            Some((d, t)) if d == dirs && dirs != 0 && t.elapsed() > Duration::from_millis(400) => {
                fire |= dirs;
                self.pad_repeat = Some((d, t + Duration::from_millis(110)));
            }
            Some((d, _)) if d == dirs => {}
            _ => self.pad_repeat = (dirs != 0).then(|| (dirs, Instant::now())),
        }
        if fire == 0 {
            return;
        }
        self.pad_mode = true;
        widgets::set_nav_mode(true);
        let focused = ctx.memory(|m| m.focused());
        let popup = egui::Popup::is_any_open(ctx);
        let key = |raw: &mut egui::RawInput, k: Key| {
            for pressed in [true, false] {
                raw.events.push(egui::Event::Key { key: k, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE });
            }
        };
        if self.welcome.is_some() {
            // The setup is all focusable controls; B steps back.
            if fire & pad::EAST != 0 {
                if let Some(w) = self.welcome.as_mut() {
                    w.back();
                }
            }
            if focused.is_none() && fire & (pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT | pad::SOUTH) != 0 {
                self.focus_first = true;
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
                    let i = cats.iter().position(|c| *c == self.category).unwrap_or(0) as i32;
                    self.category = cats[(i + d).rem_euclid(cats.len() as i32) as usize];
                }
                View::Settings => {
                    self.settings_tab = self.settings_tab.step(d);
                    self.focus_first = true;
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
            } else if !self.search.is_empty() {
                self.search.clear();
            } else {
                self.selected = None;
            }
            return;
        }
        let in_grid = self.view == View::Library && focused.is_none();
        if in_grid {
            if fire & pad::WEST != 0 && self.selected.is_some() {
                self.focus_play = true;
                return;
            }
            self.pad_fire |= fire;
            return;
        }
        // Down from the library's top bar goes back to the grid.
        if self.view == View::Library && fire & pad::DOWN != 0 && focused.is_some_and(|f| self.top_bar_ids.contains(&f)) {
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
        let fire = std::mem::take(&mut self.pad_fire);
        if fire == 0 {
            return;
        }
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let keys: Vec<String> = visible.iter().map(|&i| store::key_of(&self.entries[i].exe)).collect();
        let at = self.selected.as_ref().and_then(|k| keys.iter().position(|x| x == k));
        let step = if self.settings.list_view { 1 } else { self.grid_cols.max(1) as i32 };
        if fire & pad::UP != 0 && at.is_some_and(|a| (a as i32) < step) {
            // Up from the top row: the filters.
            self.focus_first = true;
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
            self.selected = Some(keys[n].clone());
            self.scroll_to_selected = true;
        }
        if fire & pad::SOUTH != 0 {
            match &self.selected {
                Some(k) => {
                    let k = k.clone();
                    self.pad_gate.played(Instant::now());
                    self.play(&k);
                }
                None => self.selected = Some(keys[0].clone()),
            }
        }
    }

    fn open_view(&mut self, v: View, focus: bool) {
        self.view = v;
        self.focus_first = focus && v != View::Library;
        if v == View::Together {
            self.together.refresh();
        }
    }

    // ---- drawing ----

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        let p = ui.painter().clone();
        p.rect_filled(full, 0.0, SIDEBAR);
        // A faint blue light from the top left, and the edge against the content.
        fx::light(&p, full.left_top() + vec2(40.0, 30.0), vec2(260.0, 220.0), ACCENT.gamma_multiply(0.10));
        p.rect_filled(Rect::from_min_max(pos2(full.right() - 1.0, full.top()), full.right_bottom()), 0.0, theme::line());
        ui.add_space(22.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            let (mark, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            brand_mark(ui.painter(), mark);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("PeerParty").font(theme::strong(19.0)).color(TEXT));
                ui.add_space(-7.0);
                ui.label(egui::RichText::new(t!("brand.tagline")).size(11.5).color(TEXT3));
            });
        });
        ui.add_space(22.0);
        for v in View::ALL {
            let sub = match v {
                View::Library => Some(t!("nav.games", n = self.entries.len())),
                View::Together => Some(together::summary(&self.settings.prefs)),
                View::Settings => None,
            };
            let r = nav_item(ui, v.icon(), v.name(), sub.as_deref(), self.view == v);
            if r.clicked() {
                self.open_view(v, false);
            }
        }
        // What's running, with what can be done for it.
        let mut keys: Vec<String> = self.sessions.iter().map(|s| s.key.clone()).collect();
        keys.dedup();
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(14.0);
            ui.label(egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).size(11.0).color(TEXT3));
            if self.sessions.is_empty() && self.pending.is_empty() {
                return;
            }
            ui.add_space(8.0);
            let mut invite = false;
            let mut fix = None;
            widgets::card(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(t!("nav.now_playing").to_uppercase()).font(theme::strong(11.0)).color(TEXT3));
                for s in &self.sessions {
                    ui.horizontal(|ui| {
                        let (d, _) = ui.allocate_exact_size(vec2(12.0, 18.0), Sense::hover());
                        fx::dot(ui.painter(), d.center(), 4.0, theme::GOOD);
                        let name = if s.player > 1 { t!("nav.instance", game = s.name, n = s.player) } else { s.name.clone() };
                        ui.add(egui::Label::new(egui::RichText::new(name).size(13.0).color(TEXT)).truncate());
                    });
                }
                for p in &self.pending {
                    let left = p.at.saturating_duration_since(Instant::now()).as_secs() + 1;
                    ui.label(egui::RichText::new(t!("nav.instance_in", game = p.name, player = p.player, n = left)).size(12.0).color(TEXT2));
                }
                for key in &keys {
                    let Some(g) = self.game(key) else { continue };
                    let (g_name, exe) = (g.name.clone(), self.settings.options(key).exe.unwrap_or_else(|| g.exe.clone()));
                    if g.source == Source::Xbox {
                        continue;
                    }
                    let v = self.firewall.verdict(&exe);
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
            });
            if invite {
                self.invite_everyone();
            }
            if let Some(exe) = fix {
                self.fix_firewall(exe, self.fw_public);
            }
        });
    }

    /// The bottom line: controller hints while a controller is in use, else a count.
    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let r = ui.max_rect();
        let p = ui.painter();
        let y = r.center().y;
        if self.pad_mode {
            let style = self.pads.style;
            let grid = self.view == View::Library && ui.ctx().memory(|m| m.focused()).is_none() && self.welcome.is_none();
            let next = View::ALL[(View::ALL.iter().position(|v| *v == self.view).unwrap_or(0) + 1) % 3].name();
            let mut items: Vec<(Glyph, &str)> = Vec::new();
            if grid {
                items.push((Glyph::South, t!("hint.play")));
                if self.selected.is_some() {
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
            let text = if self.scanning { t!("library.scanning").to_string() } else { t!("library.count", n = self.visible().len()) };
            p.text(pos2(r.left() + 4.0, y), egui::Align2::LEFT_CENTER, text, theme::font(12.5), TEXT3);
        }
    }

    /// The message of the moment, floating over the bottom of the content.
    fn snackbar(&self, ctx: &egui::Context, area: Rect) {
        let Some(s) = &self.snack else { return };
        let age = s.at.elapsed().as_secs_f32();
        let left = s.lasts().as_secs_f32() - age;
        let fade = (age / 0.18).min(1.0).min(left / 0.3).clamp(0.0, 1.0);
        if fade < 1.0 && theme::effects() {
            ctx.request_repaint();
        }
        let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("snack"));
        let p = ctx.layer_painter(layer);
        let g = p.layout(s.text.clone(), theme::font(13.5), TEXT, (area.width() - 120.0).max(200.0));
        let w = g.size().x + 54.0;
        let rect = Rect::from_center_size(pos2(area.center().x, area.bottom() - 34.0 + (1.0 - fade) * 10.0), vec2(w, g.size().y + 22.0));
        let color = if s.ok { theme::GOOD } else { theme::BAD };
        fx::shadow(&p, rect, 12.0, 24, 8, (120.0 * fade) as u8);
        fx::glow(&p, rect, 12.0, color.gamma_multiply(0.18 * fade), 14.0);
        p.rect_filled(rect, 12.0, theme::CARD_HI.gamma_multiply(fade));
        p.rect_stroke(rect, 12.0, egui::Stroke::new(1.0, color.gamma_multiply(0.45 * fade)), egui::StrokeKind::Inside);
        let icon = if s.ok { Icon::Check } else { Icon::Warning };
        icon.paint(&p, pos2(rect.left() + 20.0, rect.center().y), 15.0, color.gamma_multiply(fade));
        p.galley(pos2(rect.left() + 38.0, rect.center().y - g.size().y / 2.0), g, TEXT.gamma_multiply(fade));
    }
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

/// PeerParty's mark: two nodes with a bright link, on a lit tile.
fn brand_mark(p: &egui::Painter, r: Rect) {
    fx::glow(p, r, 9.0, ACCENT.gamma_multiply(0.35), 12.0);
    fx::gradient(p, r, 9.0, false, &[(0.0, ACCENT), (1.0, theme::VIOLET)]);
    fx::gradient(p, r, 9.0, true, &[(0.0, Color32::from_white_alpha(46)), (0.55, Color32::TRANSPARENT)]);
    let (a, b) = (r.left_center() + vec2(8.5, 0.0), r.right_center() - vec2(8.5, 0.0));
    p.line_segment([a, b], egui::Stroke::new(2.4, Color32::WHITE));
    p.circle_filled(a, 4.2, Color32::WHITE);
    p.circle_filled(b, 4.2, Color32::WHITE);
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
                    self.pad_mode = false;
                }
                egui::Event::Key { key: Key::Tab | Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight, pressed: true, .. } => {
                    widgets::set_nav_mode(true);
                    self.pad_mode = false;
                }
                _ => {}
            }
        }
        self.controller(ctx, raw);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut changed = false;
        while let Ok(f) = self.found_rx.try_recv() {
            match f {
                Found::Game(g) => self.scanned.push(g),
                Found::Done => self.scanning = false,
            }
            changed = true;
        }
        if changed {
            self.rebuild();
        }
        while let Ok(done) = self.icon_rx.try_recv() {
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
        self.tick();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F)) {
            self.view = View::Library;
            self.focus_search = true;
        }
        let typing = ctx.egui_wants_keyboard_input();
        if self.welcome.is_none() && ctx.input(|i| i.key_pressed(Key::Escape)) && ctx.memory(|m| m.focused()).is_none() && !egui::Popup::is_any_open(&ctx) {
            if self.view != View::Library {
                self.view = View::Library;
            } else if !self.search.is_empty() {
                self.search.clear();
            } else {
                self.selected = None;
            }
        }
        let enter = self.welcome.is_none() && !typing && self.view == View::Library && ctx.memory(|m| m.focused()).is_none() && ctx.input(|i| i.key_pressed(Key::Enter));
        if self.view == View::Library && self.welcome.is_none() {
            self.grid_pad(&ctx);
        }

        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(232.0)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin::symmetric(14, 0)))
            .show(ui, |ui| self.sidebar(ui));
        if let Some(key) = self.selected.clone().filter(|_| self.view == View::Library) {
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
            // The room's light: a cool glow from the top and a violet one from the right.
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
            if let Some(k) = self.selected.clone() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::raster::{Canvas, Textures};

    fn game(name: &str, source: Source) -> Game {
        let exe = PathBuf::from(format!(r"C:\Games\{name}\{}.exe", name.replace(' ', "")));
        Game { name: name.into(), dir: exe.parent().unwrap().to_path_buf(), exe, source, cover: None, art: None }
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

    /// Draws the window in a few states to PNGs (in PEERPARTY_PREVIEW_DIR, when set),
    /// through the overlay's software renderer, and checks each draws something.
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
        app.scanned = sample();
        app.rebuild();
        app.category = Category::All;
        // PEERPARTY_PREVIEW_SIZE=900x560 renders at another window size.
        let (w, h) = std::env::var("PEERPARTY_PREVIEW_SIZE")
            .ok()
            .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
            .unwrap_or((1440usize, 900usize));
        let mut tex = Textures::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut t = 0.0;
        let scenes: [(&str, &dyn Fn(&mut App)); 8] = [
            ("library", &|a: &mut App| a.selected = None),
            ("game", &|a: &mut App| a.selected = Some(store::key_of(&a.entries[3].exe))),
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
                a.pad_mode = true;
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
