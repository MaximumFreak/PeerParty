//! What the window remembers (`%LOCALAPPDATA%\peerparty\ui.json`) and the launches
//! peerparty.exe records (`%LOCALAPPDATA%\peerparty\recent.tsv`, written by the launcher
//! for every game it starts, from this window, a .bat or a Steam launch option).
//!
//! ui.json has more than one writer: the launcher window, and the overlay of every copy
//! of a game that runs. Each save is a read, a merge and a write of the whole file under
//! an advisory lock (`ui.json.lock` beside it, `File::lock`), the write a temporary file
//! renamed over ui.json so a reader never sees half a file. What a writer merges in is
//! only what it changed since it last read or wrote the file (`merge3`): two saves at
//! once each keep the other's changes. The lock orders the writers that take it; nothing
//! waits on it to read.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// PeerParty's own folder (`platform::data_dir`: `%LOCALAPPDATA%\peerparty` on Windows,
/// `$XDG_DATA_HOME/peerparty` on Linux).
pub fn data_dir() -> PathBuf {
    crate::platform::data_dir()
}

/// A game's identity across scans: its exe path, case folded.
pub fn key_of(exe: &Path) -> String {
    exe.to_string_lossy().to_lowercase()
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GameOptions {
    /// Copies on this PC: 1, or 2–4 for local multiplayer / split screen.
    pub players: u8,
    /// Run all copies in one split-screen window (when `players >= 2`).
    pub split_screen: bool,
    /// Split-screen layout preset: "" (auto: side by side for 2, 2+1 for 3, grid for 4),
    /// "horizontal", "vertical", "top2", "bottom2", "grid", or "custom".
    pub split_layout: String,
    /// Saved normalized custom layout from Layout Studio (`slots:x,y,w,h,fit,aspect,z;...`).
    pub split_custom_layout: String,
    /// Split-screen launch staggering (`""` = smart ready-gate, `"0"` = at once, or seconds).
    pub split_stagger: String,
    /// Turn off compositor-locked frame pacing (`--no-sync`; false = sync ON by default).
    pub split_no_sync: bool,
    /// Per-pane render resolution percentage (`--render-scale`; 0 or 100 = native, else 50..=100).
    pub split_render_scale: u8,
    /// Turn off Contrast Adaptive Sharpening (CAS) on upscaled panes (`--no-upscale`).
    pub split_no_upscale: bool,
    /// Per-copy FPS cap (`--fps`; default = match display refresh, `"1/2"`/`"1/3"`, or `15..=240`).
    pub split_fps: crate::launch::FpsCap,
    /// Turn off fair-share VRAM budget reporting (`--no-vram-budget`).
    pub split_no_vram_budget: bool,
    /// Turn off CPU core partitioning and worker thread scaling (`--no-cpu-balance`).
    pub split_no_cpu_balance: bool,
    /// What each split-screen player plays with, by player: "auto", "kbm" or "pad"
    /// (`launch::SlotInput`); missing ones are "auto".
    pub split_inputs: Vec<String>,
    /// Keep the PC's keyboard and mouse from reaching games in split screen (`--no-kbm`,
    /// controllers only), unless a player explicitly chose keyboard and mouse.
    pub split_no_kbm: bool,
    /// The monitor the split-screen window opens on (`--monitor`, from 1:
    /// `MonitorBounds::all`'s numbers); 0 or 1 is the main one.
    pub split_monitor: u8,
    /// How the split-screen window shows the copies (`PEERPARTY_COMPOSITOR`): "" (the
    /// default: their frames, drawn on the GPU), "dwm" (their windows) or "pixels".
    pub compositor: String,
    /// A DXVK release's folder (`PEERPARTY_DXVK`): a Direct3D 9 game's split-screen copies
    /// draw through it. Empty: off.
    pub dxvk: String,
    // "Copy Player 1's saves to Player N" isn't kept here: it is for one start (the window's
    // `App::copy_saves`). A `copy_saves` an older ui.json has is ignored on reading and
    // gone at the next save.
    pub args: String,
    /// A different program than the one found, chosen by the player.
    pub exe: Option<PathBuf>,
    /// This game's own answers to the global settings (`Prefs`); `None` follows them.
    pub auto_accept: Option<bool>,
    pub overlay: Option<bool>,
    pub voice: Option<bool>,
    pub steam_saves: Option<bool>,
    /// Per SDK ("photon", "eos", ...): "lan", "official" or "auto". Missing ones follow
    /// the global setting.
    pub modes: HashMap<String, String>,
    /// PeerParty layers turned off for this game (`PEERPARTY_SKIP`).
    pub skip: Vec<String>,
    /// The key players meet under (`PEERPARTY_GAME`); empty is the exe's name.
    pub game_key: String,
    /// Players 2–4's saves: "" (each player's own, the default) or "shared" (player
    /// 1's, `PEERPARTY_SEPARATE_SAVES=0`).
    pub save_mode: String,
    /// A later player with no saves yet starts from a copy of player 1's.
    pub seed_saves: bool,
    /// More folders of the game's saves, and folders every player shares.
    pub save_paths: Vec<String>,
    pub shared_save_paths: Vec<String>,
    /// The player a lone copy plays as (1–4; 0 is player 1).
    pub play_as: u8,
}

impl GameOptions {
    /// Whether this game launches in a unified split-screen session.
    pub fn uses_split_screen(&self) -> bool {
        self.players >= 2 && self.split_screen
    }

    /// Whether a saved Layout Studio custom layout exists for the current player count.
    pub fn has_custom_layout(&self) -> bool {
        let want = usize::from(self.players.clamp(2, 4));
        peerparty_protocol::session::LayoutPreset::parse_slots(&self.split_custom_layout).is_some_and(|s| s.len() == want)
    }

    /// The layout preset name selected on the game page.
    pub fn effective_split_layout(&self) -> &'static str {
        match self.split_layout.as_str() {
            "custom" if self.has_custom_layout() => "custom",
            "horizontal" => "horizontal",
            "vertical" => "vertical",
            "top2" => "top2",
            "bottom2" => "bottom2",
            "grid" => "grid",
            _ if self.players == 3 => "top2",
            _ if self.players >= 4 => "grid",
            _ => "horizontal",
        }
    }

    /// The value passed to `--layout` (resolving `"custom"` to its saved `slots:...` string).
    pub fn split_layout_arg(&self) -> String {
        if self.effective_split_layout() == "custom" {
            self.split_custom_layout.clone()
        } else {
            self.effective_split_layout().to_string()
        }
    }

    /// Whether any active split-screen player is explicitly assigned keyboard and mouse.
    pub fn has_kbm_player(&self) -> bool {
        let n = usize::from(self.players.clamp(2, 4));
        (0..n).any(|k| self.split_input(k) == "kbm")
    }

    /// Split-screen player `k`'s input: "auto", "kbm" or "pad".
    pub fn split_input(&self, k: usize) -> &'static str {
        crate::launch::SlotInput::named(self.split_inputs.get(k).map_or("", String::as_str)).unwrap_or_default().as_str()
    }

    pub fn set_split_input(&mut self, k: usize, v: &str) {
        if self.split_inputs.len() <= k {
            self.split_inputs.resize(k + 1, "auto".into());
        }
        self.split_inputs[k] = v.into();
    }

    /// `--inputs`' value for the players there are, or None when they're all "auto".
    pub fn split_inputs_arg(&self) -> Option<String> {
        let n = usize::from(self.players.clamp(2, 4));
        let v: Vec<&str> = (0..n).map(|k| self.split_input(k)).collect();
        v.iter().any(|i| *i != "auto").then(|| v.join(","))
    }

    /// Effective render scale percentage (`50..=100`; 0 in older files reads as 100%).
    pub fn effective_split_render_scale(&self) -> u8 {
        if self.split_render_scale == 0 || self.split_render_scale >= 100 { 100 } else { self.split_render_scale.clamp(50, 100) }
    }

    /// The settings a game can answer for itself (`None` follows `Prefs`), each with its
    /// field in `Prefs`.
    fn own_answers(&self) -> [(Option<bool>, fn(&mut Prefs) -> &mut bool); 4] {
        [
            (self.auto_accept, |p| &mut p.auto_accept),
            (self.overlay, |p| &mut p.overlay),
            (self.voice, |p| &mut p.voice),
            (self.steam_saves, |p| &mut p.steam_saves),
        ]
    }
}

impl Default for GameOptions {
    fn default() -> Self {
        GameOptions {
            players: 1,
            split_screen: false,
            split_layout: String::new(),
            split_custom_layout: String::new(),
            split_stagger: String::new(),
            split_no_sync: false,
            split_render_scale: 100,
            split_no_upscale: false,
            split_fps: crate::launch::FpsCap::Display,
            split_no_vram_budget: false,
            split_no_cpu_balance: false,
            split_inputs: Vec::new(),
            split_no_kbm: false,
            split_monitor: 0,
            compositor: String::new(),
            dxvk: String::new(),
            args: String::new(),
            exe: None,
            auto_accept: None,
            overlay: None,
            voice: None,
            steam_saves: None,
            modes: HashMap::new(),
            skip: Vec::new(),
            game_key: String::new(),
            save_mode: String::new(),
            seed_saves: false,
            save_paths: Vec::new(),
            shared_save_paths: Vec::new(),
            play_as: 0,
        }
    }
}

/// Which hotkeys open the overlay. The keyboard ones are the stores' own (Steam and GOG
/// Shift+Tab, Epic Shift+F3, EA Shift+F1, Ubisoft Shift+F2); one whose store's overlay
/// is in the game is left to that overlay.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Hotkeys {
    pub shift_tab: bool,
    pub shift_f3: bool,
    pub shift_f1: bool,
    pub shift_f2: bool,
    /// The Home / Xbox Guide / PS button.
    pub guide: bool,
    /// A controller chord the player recorded in the overlay (host::pad's button bits,
    /// two or more), held for a second; 0 = none. Replaces the fixed View + Menu chord,
    /// which Windows uses for its on-screen keyboard (a saved `combo` is ignored).
    pub chord: u32,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys { shift_tab: true, shift_f3: true, shift_f1: false, shift_f2: false, guide: true, chord: 0 }
    }
}

/// Settings for every game, changed in the launcher window's Settings page or the
/// overlay. A game started from a shortcut or Steam reads them too.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Prefs {
    /// The name other players see, in every service (Steam's persona name, EOS and GOG
    /// display names, PlayFab, Unity). Empty: the Windows user name.
    pub name: String,
    /// The name a second copy of a game on this PC goes by (`PEERPARTY_OTHER_NAME`; the
    /// third onwards numbered). Empty: `name` with "(2)". Shown only on a game page set
    /// to two players, or with a copy running.
    pub other_name: String,
    /// The names split screen's players 3 and 4 go by (`--names` over them). Empty:
    /// `name` with "(3)", "(4)". Shown on a game page set to three or four players.
    pub third_name: String,
    pub fourth_name: String,
    /// Invites from other players are accepted without asking. Off: the overlay asks.
    pub auto_accept: bool,
    /// Controller buttons that invite every other player: "", "guide", "combo" or "both".
    pub invite_buttons: String,
    pub overlay: bool,
    pub hotkeys: Hotkeys,
    /// Overlay size, 0.75 to 1.5.
    pub overlay_scale: f32,
    /// Corner notifications (invites, players arriving) while the overlay is closed.
    pub toasts: bool,
    /// Button glyphs: "auto" (the pad in use), "xbox", "playstation" or "nintendo".
    pub glyphs: String,
    /// How the overlay is drawn: "auto", "window" (a window over the game) or "ingame"
    /// (inside the game's own frames).
    pub draw: String,
    /// Per SDK: "lan", "official" or "auto" (see the per-game `modes`).
    pub modes: HashMap<String, String>,
    pub voice: bool,
    /// A game given a SteamID of its own for this PC (the "Steam player ID" mode) starts
    /// its saves keyed by SteamID from a copy of the ones it had (`PEERPARTY_STEAM_SAVES=0`
    /// when off). On by default.
    pub steam_saves: bool,
    /// Internet play: a room code (`PEERPARTY_ROOM`) shared with the friends to play
    /// with over the internet. Empty: local network only.
    pub room: String,
    /// Public sessions (`PEERPARTY_PUBLIC`): also find anyone online playing the same
    /// game through PeerParty. Off by default.
    pub public: bool,
    /// The name public players see (`PEERPARTY_PUBLIC_NAME`). Empty: `name`, else
    /// "Player-" and the start of this PC's key.
    pub public_name: String,
    /// Voice and party chat with public players (`PEERPARTY_PUBLIC_VOICE`). Off by
    /// default: strangers hear the microphone only once the player says so. Room and
    /// LAN players always have voice.
    pub public_voice: bool,
    /// The internet switch (`PEERPARTY_INTERNET=0` when off): the room and public
    /// sessions are kept but not used, local network only. On by default; the overlay's
    /// Internet page flips it while a game runs.
    pub internet: bool,
    /// The overlay's Game Bar tip has been shown.
    pub guide_tip_shown: bool,
    /// The interface language: a code from `i18n::LANGS`, or empty for the system's.
    pub language: String,
    /// Lighting and animations in the window and the overlay (off also when Windows'
    /// animation effects are off).
    pub effects: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            name: String::new(),
            other_name: String::new(),
            third_name: String::new(),
            fourth_name: String::new(),
            auto_accept: false,
            invite_buttons: String::new(),
            overlay: true,
            hotkeys: Hotkeys::default(),
            overlay_scale: 1.0,
            toasts: true,
            glyphs: "auto".into(),
            draw: "auto".into(),
            modes: HashMap::new(),
            voice: true,
            steam_saves: true,
            room: String::new(),
            public: false,
            public_name: String::new(),
            public_voice: false,
            internet: true,
            guide_tip_shown: false,
            language: String::new(),
            effects: true,
        }
    }
}

/// The SDKs that can use official servers instead of PeerParty's own stand-ins
/// (`catalog::mode_sdks`): each one's id, name and mode by default. The last isn't a
/// service: whether the game gets a SteamID of its own for this PC ("lan"), keeps
/// steam_api's ("official"), or the first unless the Steam client runs it.
pub fn sdk_modes() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    peerparty_protocol::catalog::mode_sdks().map(|s| (s.id, s.label, s.default_mode.as_str()))
}

impl Prefs {
    /// The settings that apply to `game`: these, with its own answers on top.
    pub fn effective_for(&self, game: &GameOptions) -> Prefs {
        let mut p = self.clone();
        for (own, field) in game.own_answers() {
            if let Some(v) = own {
                *field(&mut p) = v;
            }
        }
        p
    }

    /// A game's mode for an SDK: its own, else the global one, else the default.
    pub fn mode(&self, game: &GameOptions, sdk: &str) -> String {
        game.modes
            .get(sdk)
            .or_else(|| self.modes.get(sdk))
            .cloned()
            .unwrap_or_else(|| sdk_modes().find(|m| m.0 == sdk).map_or("lan", |m| m.2).to_string())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Folders whose subfolders are games. `platform::default_games_dir` to begin with;
    /// the player can remove it.
    pub folders: Vec<PathBuf>,
    /// Programs added with "Add a game…".
    pub added: Vec<PathBuf>,
    pub hidden: Vec<String>,
    pub games: HashMap<String, GameOptions>,
    /// Seconds between starting player 1 and player 2.
    pub second_delay: u32,
    pub list_view: bool,
    /// What the window does once it has started a game: "minimize" (out of the game's
    /// way) or "keep".
    pub after_launch: String,
    /// The first-run setup has been done (or skipped). A ui.json from before it existed
    /// counts as done.
    pub onboarded: bool,
    /// The window's settings layout this file was written with (`UI_VERSION`); 0 is a
    /// file from before the redesign.
    pub ui_version: u32,
    /// Ask GitHub once a day whether a newer PeerParty is out (update.rs).
    pub check_updates: bool,
    /// When it last asked (seconds since 1970).
    pub update_checked: u64,
    pub prefs: Prefs,
    /// The file is there and couldn't be read (another program had it, say): these are
    /// defaults standing in, and saving them would put defaults over the player's real
    /// settings. Nothing is saved until a read works.
    #[serde(skip)]
    pub unread: bool,
}

/// `Settings::ui_version` now. 3: voice with strangers became off unless turned on; a
/// file from before has it switched off once, since "on" there was only the old default.
pub const UI_VERSION: u32 = 3;

/// The waits the window offers between starting one copy and the next, in seconds.
pub const SECOND_DELAYS: [u32; 10] = [3, 5, 8, 10, 12, 15, 20, 30, 45, 60];

impl Default for Settings {
    fn default() -> Self {
        Settings {
            folders: vec![crate::platform::default_games_dir()],
            added: Vec::new(),
            hidden: Vec::new(),
            games: HashMap::new(),
            second_delay: 12,
            list_view: false,
            after_launch: "minimize".into(),
            onboarded: false,
            ui_version: UI_VERSION,
            check_updates: true,
            update_checked: 0,
            prefs: Prefs::default(),
            unread: false,
        }
    }
}

impl Settings {
    fn path() -> PathBuf {
        data_dir().join("ui.json")
    }

    pub fn load() -> Settings {
        Self::load_from(&Self::path())
    }

    /// Reads the settings at `path`. A file that's there but doesn't read (cut short,
    /// edited by hand) is kept beside it as `ui.json.bad` first: the defaults that take
    /// its place are saved over it at the next change, and with it would go the room
    /// code, the game folders and every game's own settings.
    fn load_from(path: &Path) -> Settings {
        match Self::read(path) {
            Ok(s) => s,
            // No file is a first run. Anything else is a file we mustn't replace.
            Err(Unread::Missing) => Settings::default(),
            Err(Unread::Held) => Settings { unread: true, ..Settings::default() },
            Err(Unread::Broken(text)) => {
                if !text.trim().is_empty() {
                    let _ = fs::write(path.with_extension("json.bad"), &text);
                }
                Settings::default()
            }
        }
    }

    /// The settings at `path` as they are, brought up to `UI_VERSION`.
    fn read(path: &Path) -> Result<Settings, Unread> {
        let text = fs::read_to_string(path).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { Unread::Missing } else { Unread::Held })?;
        let Some(mut s) = Self::parse(&text) else { return Err(Unread::Broken(text)) };
        if s.ui_version == 0 {
            // Someone who used PeerParty before the first-run setup existed: they've set
            // it up already.
            s.onboarded = true;
        }
        if s.ui_version < 3 {
            s.prefs.public_voice = false;
        }
        // The wait before a second copy is one the window offers. An older window let it
        // be dragged up to 120 s, and this one showed anything past 60 as 12 s while the
        // second copy still waited the whole time: past the last choice it is the 12 s
        // shown, between two choices the longer one.
        if !SECOND_DELAYS.contains(&s.second_delay) {
            s.second_delay = SECOND_DELAYS.iter().copied().find(|d| *d >= s.second_delay).unwrap_or(Settings::default().second_delay);
        }
        s.ui_version = UI_VERSION;
        Ok(s)
    }

    fn parse(text: &str) -> Option<Settings> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let mut s: Settings = serde_json::from_value(v.clone()).ok()?;
        if v.get("ui_version").is_none() {
            s.ui_version = 0;
        }
        Some(s)
    }

    /// Saves what changed here since `base` (what this window last read or wrote) over
    /// the file as it is now, under the lock: a change another writer made meanwhile
    /// stays. Returns the settings as saved, the next save's `base`.
    pub fn save_over(&self, base: &Settings) -> Settings {
        self.save_over_at(&Self::path(), base)
    }

    fn save_over_at(&self, path: &Path, base: &Settings) -> Settings {
        if self.unread {
            return self.clone();
        }
        locked(path, || {
            // A file that doesn't read (or isn't there) has nothing to keep: ours goes in whole.
            let merged = match Self::read(path) {
                Ok(disk) => merge3_into(base, self, &disk).unwrap_or_else(|| self.clone()),
                Err(_) => self.clone(),
            };
            merged.write_to(path);
            merged
        })
        .unwrap_or_else(|_| self.clone())
    }

    /// Reads the settings, changes them with `f` and writes them back, all under the
    /// lock (an overlay's save). Settings that couldn't be read aren't written.
    pub fn update(f: impl FnOnce(&mut Settings)) {
        Self::update_at(&Self::path(), f);
    }

    fn update_at(path: &Path, f: impl FnOnce(&mut Settings)) {
        let _ = locked(path, || {
            let mut s = Self::load_from(path);
            if !s.unread {
                f(&mut s);
                s.write_to(path);
            }
        });
    }

    /// The whole file, through a temporary file of this process's own renamed over it.
    fn write_to(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
            if fs::write(&tmp, text).is_ok() && fs::rename(&tmp, path).is_err() {
                let _ = fs::remove_file(&tmp);
            }
        }
    }

    /// An overlay's settings changes for the game `key`: what it changed since `base`
    /// (its prefs at its last save) goes over these. A game's own answers stay, and the
    /// global ones they stand in for too: what the overlay changed for this game alone
    /// isn't written over every other game's.
    pub fn apply_overlay_edit(&mut self, key: &str, base: &Prefs, edited: &Prefs) {
        let mut disk = self.prefs.clone();
        let mut p = merge3_into(base, edited, &disk).unwrap_or_else(|| edited.clone());
        for (own, field) in self.options(key).own_answers() {
            if own.is_some() {
                *field(&mut p) = *field(&mut disk);
            }
        }
        self.prefs = p;
    }

    /// When ui.json last changed.
    pub fn modified() -> Option<std::time::SystemTime> {
        fs::metadata(Self::path()).and_then(|m| m.modified()).ok()
    }

    /// A game's options by its program's key: the found program's, or the one chosen
    /// in its place ("Change…"), which is what peerparty.exe is started with.
    pub fn options(&self, key: &str) -> GameOptions {
        if let Some(o) = self.games.get(key) {
            return o.clone();
        }
        self.games.values().find(|o| o.exe.as_deref().is_some_and(|e| key_of(e) == key)).cloned().unwrap_or_default()
    }
}

/// Why the settings file didn't read.
enum Unread {
    Missing,
    /// There, and held by another program (or not a file).
    Held,
    /// There, and not settings: its text.
    Broken(String),
}

/// The lock file beside the settings at `path` (`ui.json.lock`).
fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("json.lock")
}

/// Runs `f` holding the advisory lock beside `path` (made if it isn't there), waiting for
/// whoever holds it now. The lock goes when `f` returns (or the process ends).
fn locked<T>(path: &Path, f: impl FnOnce() -> T) -> std::io::Result<T> {
    let lock = lock_path(path);
    if let Some(dir) = lock.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let file = File::options().read(true).write(true).create(true).truncate(false).open(&lock)?;
    file.lock()?;
    let out = f();
    let _ = file.unlock();
    Ok(out)
}

/// `merge3` on values of a type, as that type again (None if the merge doesn't read
/// back as one).
fn merge3_into<T: Serialize + serde::de::DeserializeOwned>(base: &T, ours: &T, theirs: &T) -> Option<T> {
    let v = |x: &T| serde_json::to_value(x).ok();
    serde_json::from_value(merge3(&v(base)?, &v(ours)?, &v(theirs)?)).ok()
}

/// A three-way merge: what `ours` changed since `base` goes over `theirs`, which keeps
/// everything else. Objects (the settings, a game's options, the map of games) merge
/// key by key; anything else (a list, a number) is taken whole.
fn merge3(base: &Value, ours: &Value, theirs: &Value) -> Value {
    let (Value::Object(b), Value::Object(o), Value::Object(t)) = (base, ours, theirs) else {
        return if ours == base { theirs.clone() } else { ours.clone() };
    };
    let mut out = t.clone();
    for k in b.keys().chain(o.keys()).collect::<BTreeSet<_>>() {
        match (b.get(k), o.get(k), t.get(k)) {
            (bv, ov, _) if bv == ov => {}
            (_, None, _) => {
                out.remove(k);
            }
            (Some(bv), Some(ov), Some(tv)) => {
                out.insert(k.clone(), merge3(bv, ov, tv));
            }
            (_, Some(ov), _) => {
                out.insert(k.clone(), ov.clone());
            }
        }
    }
    Value::Object(out)
}

pub struct Recent {
    pub when: u64,
    pub exe: PathBuf,
}

/// Launches peerparty.exe recorded, newest first, one per program.
pub fn recent() -> Vec<Recent> {
    let Ok(text) = fs::read_to_string(data_dir().join("recent.tsv")) else {
        return Vec::new();
    };
    let mut out: Vec<Recent> = Vec::new();
    for line in text.lines().rev() {
        let mut parts = line.split('\t');
        let (Some(when), Some(exe)) = (parts.next(), parts.next()) else { continue };
        let Ok(when) = when.parse() else { continue };
        let exe = PathBuf::from(exe);
        if out.iter().any(|r| key_of(&r.exe) == key_of(&exe)) {
            continue;
        }
        out.push(Recent { when, exe });
    }
    out
}

/// Splits extra arguments the way a command line would: spaces, with "quoted" parts.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ui.json from before the first-run setup reads as set up; a new one doesn't.
    #[test]
    fn older_settings_count_as_set_up() {
        let old = Settings::parse(r#"{"prefs": {"room": "x"}}"#).unwrap();
        assert_eq!(old.ui_version, 0);
        let new = Settings::parse(&serde_json::to_string(&Settings::default()).unwrap()).unwrap();
        assert_eq!(new.ui_version, UI_VERSION);
        assert!(!new.onboarded);
        assert!(Settings::default().prefs.effects && Settings::default().prefs.language.is_empty());
    }

    /// Settings that don't read are set aside, not lost to the next save.
    #[test]
    fn unreadable_settings_are_kept_beside_the_new_ones() {
        let dir = std::env::temp_dir().join(format!("peerparty-settings-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.json");
        let broken = r#"{"prefs": {"room": "PP1-AAAA", "public": tru"#;
        fs::write(&path, broken).unwrap();
        let s = Settings::load_from(&path);
        assert!(s.prefs.room.is_empty() && !s.onboarded, "defaults stand in");
        assert_eq!(fs::read_to_string(dir.join("ui.json.bad")).unwrap(), broken);
        // A good file reads as before and leaves nothing behind.
        fs::remove_file(dir.join("ui.json.bad")).unwrap();
        fs::write(&path, r#"{"ui_version": 2, "prefs": {"name": "Robin"}}"#).unwrap();
        assert_eq!(Settings::load_from(&path).prefs.name, "Robin");
        assert!(!dir.join("ui.json.bad").exists());
        assert!(Settings::load_from(&dir.join("none.json")).prefs.name.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A file that is there and can't be read (here: it is a folder) leaves defaults in
    /// memory, and they are never saved over it. No file at all is a first run, which saves.
    #[test]
    fn settings_that_couldnt_be_read_arent_written_over() {
        let dir = std::env::temp_dir().join(format!("peerparty-unread-{}", std::process::id()));
        let path = dir.join("ui.json");
        fs::create_dir_all(&path).unwrap();
        let s = Settings::load_from(&path);
        assert!(s.unread);
        s.save_over_at(&path, &Settings::default());
        assert!(path.is_dir(), "the unreadable thing is still there");
        let fresh = dir.join("first-run.json");
        let base = Settings::load_from(&fresh);
        assert!(!base.unread);
        let mut s = base.clone();
        s.prefs.name = "Robin".into();
        s.save_over_at(&fresh, &base);
        assert_eq!(Settings::load_from(&fresh).prefs.name, "Robin");
        let tmp = fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).count();
        assert_eq!(tmp, 0, "no temporary file left behind");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Voice with strangers was on unless turned off; it is off unless turned on. A file
    /// from before can't say which the player meant, so it is asked again, once.
    #[test]
    fn an_older_file_has_voice_with_strangers_switched_off_once() {
        let dir = std::env::temp_dir().join(format!("peerparty-voice-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.json");
        fs::write(&path, r#"{"ui_version": 2, "prefs": {"public_voice": true, "voice": true}}"#).unwrap();
        let s = Settings::load_from(&path);
        assert!(!s.prefs.public_voice && s.prefs.voice && s.ui_version == UI_VERSION);
        // Turned on since, it stays on.
        fs::write(&path, format!(r#"{{"ui_version": {UI_VERSION}, "prefs": {{"public_voice": true}}}}"#)).unwrap();
        assert!(Settings::load_from(&path).prefs.public_voice);
        let _ = fs::remove_dir_all(&dir);
    }

    /// "Copy player 1's saves" used to be a game's setting, so every later second copy
    /// replaced its save. An older file's is ignored, the rest of the game's settings
    /// read as before, and the next save leaves it out.
    #[test]
    fn an_older_files_copy_saves_is_dropped() {
        let dir = std::env::temp_dir().join(format!("peerparty-copy-saves-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.json");
        fs::write(&path, r#"{"ui_version": 3, "games": {"c:/games/a/a.exe": {"players": 2, "copy_saves": true, "args": "-windowed"}}}"#).unwrap();
        let s = Settings::load_from(&path);
        let o = s.options("c:/games/a/a.exe");
        assert_eq!((o.players, o.args.as_str()), (2, "-windowed"));
        s.save_over_at(&path, &s);
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("copy_saves") && text.contains("-windowed"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A wait the window doesn't offer (an older window's 100 s) reads as one it does,
    /// so what the window shows is what the second copy waits.
    #[test]
    fn a_second_copys_wait_is_one_the_window_offers() {
        let dir = std::env::temp_dir().join(format!("peerparty-delay-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.json");
        for (stored, read) in [(100, 12), (7, 8), (0, 3), (45, 45)] {
            fs::write(&path, format!(r#"{{"ui_version": {UI_VERSION}, "second_delay": {stored}}}"#)).unwrap();
            assert_eq!(Settings::load_from(&path).second_delay, read, "{stored}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A game's own answers go over the settings for every game; the rest follow them.
    #[test]
    fn a_games_own_answers_go_on_top() {
        let p = Prefs { auto_accept: false, voice: true, overlay_scale: 1.25, ..Prefs::default() };
        let game = GameOptions { auto_accept: Some(true), voice: Some(false), ..GameOptions::default() };
        let e = p.effective_for(&game);
        assert!(e.auto_accept && !e.voice && e.overlay && e.steam_saves);
        assert_eq!(e.overlay_scale, 1.25);
        assert!(p.effective_for(&GameOptions::default()) == p);
    }

    /// What an overlay changed goes over the file; a game's own answers stay as they
    /// were on disk (and the global ones under them), and what the overlay didn't change
    /// (an answer only `--set` gave this run, a change the window made since) stays too.
    #[test]
    fn an_overlays_edit_keeps_a_games_own_answers_and_other_changes() {
        let mut s = Settings::default();
        s.prefs.auto_accept = false;
        s.prefs.voice = true;
        s.games.insert("game".into(), GameOptions { auto_accept: Some(true), ..Default::default() });
        // The overlay's prefs: this game's (its own answer on top) with `--set voice=off`.
        let base = Prefs { voice: false, ..s.prefs.effective_for(&s.options("game")) };
        // Meanwhile the window changed the name.
        s.prefs.name = "Robin".into();
        let mut edited = base.clone();
        edited.auto_accept = false;
        edited.overlay_scale = 1.5;
        s.apply_overlay_edit("game", &base, &edited);
        assert!(!s.prefs.auto_accept, "the game has its own answer: the global one stays");
        assert!(s.prefs.voice, "the overlay didn't change voice: --set's answer stays this run's");
        assert_eq!(s.prefs.overlay_scale, 1.5);
        assert_eq!(s.prefs.name, "Robin", "the window's change stays");
        // Another game's overlay changes the global answer.
        let base = s.prefs.clone();
        let edited = Prefs { auto_accept: true, ..base.clone() };
        s.apply_overlay_edit("other", &base, &edited);
        assert!(s.prefs.auto_accept);
    }

    #[test]
    fn a_three_way_merge_keeps_both_sides_changes() {
        let v = |t: &str| serde_json::from_str::<Value>(t).unwrap();
        let base = v(r#"{"a": 1, "b": {"x": 1, "y": 1}, "list": [1], "gone": 1}"#);
        let ours = v(r#"{"a": 2, "b": {"x": 2, "y": 1}, "list": [1], "new": 1}"#);
        let theirs = v(r#"{"a": 1, "b": {"x": 1, "y": 3}, "list": [1, 2], "gone": 1, "theirs": 1}"#);
        assert_eq!(merge3(&base, &ours, &theirs), v(r#"{"a": 2, "b": {"x": 2, "y": 3}, "list": [1, 2], "new": 1, "theirs": 1}"#));
        // The same field changed on both sides: ours, the later save, wins.
        assert_eq!(merge3(&v("1"), &v("2"), &v("3")), v("2"));
    }

    /// The window and several overlays saving at once, each from what it read before the
    /// others wrote: every one's change is in the file afterwards, and the file always
    /// reads whole.
    #[test]
    fn saves_at_once_keep_every_change() {
        let dir = std::env::temp_dir().join(format!("peerparty-locked-save-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.json");
        let first = Settings { onboarded: true, ..Settings::default() };
        first.save_over_at(&path, &first);
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|sc| {
            for t in 0..4 {
                let (path, start) = (&path, &start);
                // The window: a game's options each time, from a base read before them all.
                sc.spawn(move || {
                    let mut base = Settings::load_from(path);
                    start.wait();
                    for i in 0..10 {
                        let mut ours = base.clone();
                        ours.games.insert(format!("game-{t}-{i}"), GameOptions { players: 2, ..GameOptions::default() });
                        base = ours.save_over_at(path, &base);
                    }
                });
                // An overlay: one setting of its own.
                sc.spawn(move || {
                    let base = Settings::load_from(path).prefs;
                    start.wait();
                    for i in 0..10 {
                        let mut edited = base.clone();
                        edited.modes.insert(format!("sdk-{t}"), format!("mode-{i}"));
                        Settings::update_at(path, |s| s.apply_overlay_edit("k", &base, &edited));
                        assert!(Settings::read(path).is_ok(), "a reader sees a whole file");
                    }
                });
            }
        });
        let s = Settings::load_from(&path);
        assert!(s.onboarded);
        for t in 0..4 {
            for i in 0..10 {
                assert_eq!(s.options(&format!("game-{t}-{i}")).players, 2, "game-{t}-{i} lost");
            }
            assert_eq!(s.prefs.modes.get(&format!("sdk-{t}")).map(String::as_str), Some("mode-9"), "overlay {t} lost");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn splits_like_a_command_line() {
        assert_eq!(split_args(r#"-windowed  "-name=A B" -x"#), ["-windowed", "-name=A B", "-x"]);
        assert!(split_args("   ").is_empty());
        assert_eq!(split_args(r#""""#), [""]);
    }
}
