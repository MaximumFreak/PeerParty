//! Starting a game through PeerParty, from what the command line asked (`Launch`, made by
//! cli.rs): the game the command names (`backend::resolve`), the player's settings for
//! it (ui.json, `--set` on top), the environment the game starts with (`ChildEnv`), and
//! the start itself, the system's part (`backend::start`).
//!
//! Nothing here changes this process's own environment: every variable the game gets is
//! in its `ChildEnv`, handed to the game when it is created.
//!
//! A start that fails says why as a `LaunchError`: its English line for the command line
//! and the logs, and a reason the launcher window says in the player's language (asked
//! for with `--why`: `peerparty-why: <reason>\t<value>`, read by `ui::why_text`).

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use peerparty_protocol::env as penv;
use peerparty_protocol::session::{ANY_KEYBOARD, InputAssignment, LayoutPreset};

use crate::ui::store::{self, Prefs};
use crate::{anticheat, backend, gameroot, host, platform, steamid};

/// Why a game didn't start.
#[derive(Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum LaunchError {
    /// The game's program (or the one a .bat runs) isn't there.
    Missing { path: PathBuf, error: String },
    /// It isn't a Windows program.
    NotProgram(PathBuf),
    /// A Windows program for a machine other than x64 or x86.
    Machine(u16),
    /// The game must run as administrator (ERROR_ELEVATION_REQUIRED).
    Elevation(String),
    /// The system refused to start it: the English line, and its own message (already
    /// in the system's language).
    Os { english: String, error: String },
    /// The room code in the settings isn't one.
    Room(String),
    /// The hook couldn't be put in the game.
    Inject(String),
    /// The game uses anti-cheat (anticheat.rs): its file that says so.
    AntiCheat { exe: PathBuf, found: String },
    /// Anything else: its English line, with no reason of its own.
    Other(String),
}

impl LaunchError {
    /// A program that can't be opened: missing, else the system's reason.
    pub fn missing(path: &Path, e: &std::io::Error) -> LaunchError {
        if e.kind() == std::io::ErrorKind::NotFound {
            LaunchError::Missing { path: path.to_path_buf(), error: e.to_string() }
        } else {
            LaunchError::os(format!("{}: {e}", path.display()), e)
        }
    }

    /// An OS error from starting the game: one that must run as administrator, else
    /// the system's own message.
    pub fn os(english: String, e: &std::io::Error) -> LaunchError {
        match e.raw_os_error() {
            Some(740) => LaunchError::Elevation(english),
            _ => LaunchError::Os { english, error: e.to_string() },
        }
    }

    /// The reason and its value for the `peerparty-why:` line (None: only the English).
    pub fn why(&self) -> Option<(&'static str, String)> {
        Some(match self {
            LaunchError::Missing { path, .. } => ("missing", path.display().to_string()),
            LaunchError::NotProgram(path) => ("not_program", path.display().to_string()),
            LaunchError::Machine(_) => ("machine", String::new()),
            LaunchError::Elevation(_) => ("elevation", String::new()),
            LaunchError::Os { error, .. } => ("os", error.clone()),
            LaunchError::Room(_) => ("room", String::new()),
            LaunchError::Inject(_) => ("inject", String::new()),
            LaunchError::AntiCheat { found, .. } => ("anticheat", found.clone()),
            LaunchError::Other(_) => return None,
        })
    }
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LaunchError::Missing { path, error } => write!(f, "{}: {error}", path.display()),
            LaunchError::NotProgram(path) => write!(f, "{} is not a PE file", path.display()),
            LaunchError::Machine(m) => write!(f, "unsupported machine type {m:#x}"),
            LaunchError::Room(room) => write!(f, "{room:?} isn't a room code (peerparty --new-room makes one)"),
            LaunchError::AntiCheat { exe, found } => write!(f, "{} uses anti-cheat ({found}); peerparty doesn't start games that do (LEGAL.md)", exe.display()),
            LaunchError::Elevation(s) | LaunchError::Os { english: s, .. } | LaunchError::Inject(s) | LaunchError::Other(s) => f.write_str(s),
        }
    }
}

impl From<String> for LaunchError {
    fn from(s: String) -> Self {
        LaunchError::Other(s)
    }
}

impl From<&str> for LaunchError {
    fn from(s: &str) -> Self {
        LaunchError::Other(s.to_string())
    }
}

/// How a game started through PeerParty ended: its exit code, when it could be read.
pub struct Exited(pub Option<u32>);

impl Exited {
    /// The code PeerParty exits with: the game's whole code (a crash's NTSTATUS too),
    /// 1 when it couldn't be read.
    pub fn code(&self) -> u32 {
        self.0.unwrap_or(1)
    }

    /// The reason for the `peerparty-why:` line, for a game that ended with an error.
    pub fn why(&self) -> Option<(&'static str, String)> {
        let c = self.0.filter(|&c| c != 0)?;
        Some((if c == 0xC000_0135 { "exit_dll" } else { "exit" }, format!("{c:#010X}")))
    }
}

/// The `peerparty-why:` line for a reason.
pub fn why_line(reason: &str, value: &str) -> String {
    format!("peerparty-why: {reason}\t{}", value.replace(['\t', '\r', '\n'], " "))
}

/// The environment a game starts with: this process's own, with PeerParty's changes on
/// top (a variable set, or taken away). Names are matched as the system matches them:
/// any case on Windows.
#[derive(Clone, Default)]
pub struct ChildEnv {
    /// By `fold`ed name: the name as written, and its value (None: taken away).
    changes: BTreeMap<String, (String, Option<OsString>)>,
}

impl ChildEnv {
    fn fold(k: &str) -> String {
        if cfg!(windows) { k.to_ascii_uppercase() } else { k.to_string() }
    }

    /// A variable as the game will see it.
    pub fn get(&self, k: &str) -> Option<OsString> {
        match self.changes.get(&Self::fold(k)) {
            Some((_, v)) => v.clone(),
            None => env::var_os(k),
        }
    }

    pub fn is_set(&self, k: &str) -> bool {
        self.get(k).is_some()
    }

    pub fn set(&mut self, k: &str, v: impl AsRef<OsStr>) {
        self.changes.insert(Self::fold(k), (k.to_string(), Some(v.as_ref().to_os_string())));
    }

    /// Sets `k` unless it's set already (by the environment PeerParty was started in, or
    /// a .bat's own): those win.
    pub fn set_default(&mut self, k: &str, v: impl AsRef<OsStr>) {
        if !self.is_set(k) {
            self.set(k, v);
        }
    }

    pub fn remove(&mut self, k: &str) {
        self.changes.insert(Self::fold(k), (k.to_string(), None));
    }

    /// The whole environment the game gets, in the system's order (by name, any case on
    /// Windows), each name once.
    pub fn vars(&self) -> Vec<(OsString, OsString)> {
        let mut all: BTreeMap<String, (OsString, OsString)> = BTreeMap::new();
        for (k, v) in env::vars_os() {
            let name = Self::fold(&k.to_string_lossy());
            if !self.changes.contains_key(&name) {
                all.insert(name, (k, v));
            }
        }
        for (name, (k, v)) in &self.changes {
            if let Some(v) = v {
                all.insert(name.clone(), (k.into(), v.clone()));
            }
        }
        all.into_values().collect()
    }

    /// The changes on a `Command`, which starts from this process's environment.
    #[cfg(unix)]
    pub fn apply(&self, cmd: &mut std::process::Command) {
        for (k, v) in self.changes.values() {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
    }

    /// PeerParty's own variables and Steam's ids, as text, for a game started by Windows
    /// on PeerParty's behalf (a packaged game, packaged.rs).
    #[cfg(windows)]
    pub fn peerparty_vars(&self) -> Vec<(String, String)> {
        self.vars()
            .into_iter()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .filter(|(k, _)| k.starts_with("PEERPARTY_") || k == "SteamAppId" || k == "SteamGameId")
            .collect()
    }

    /// `CreateProcessW`'s environment block (with `CREATE_UNICODE_ENVIRONMENT`):
    /// `name=value` strings, each ending in a zero, and a zero after the last.
    #[cfg(windows)]
    pub fn block(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        let mut block = Vec::new();
        for (k, v) in self.vars() {
            block.extend(k.encode_wide());
            block.push(u16::from(b'='));
            block.extend(v.encode_wide());
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        block
    }
}

/// Several copies of the game sharing the screen, each in a pane (`--split-screen`).
#[derive(Clone, Debug, PartialEq)]
pub struct SplitScreen {
    pub instances: u32,
    pub layout: LayoutPreset,
    /// What each player plays with (`--inputs`), by player; missing ones are `Auto`.
    pub inputs: Vec<SlotInput>,
    /// Each player's name (`--names`), by player; blank or missing: the PC's naming
    /// (player 1 the PC's, player 2 the second player's name, then "<name> (3)").
    pub names: Vec<String>,
}

/// What a split-screen player plays with (`--inputs auto,kbm,pad`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SlotInput {
    /// A controller, the first one used that no other player has; the keyboard and
    /// mouse too while their pane has the focus. A player who has been playing with the
    /// keyboard and mouse is passed over by a controller while another player has none.
    #[default]
    Auto,
    /// The keyboard and mouse, never a controller.
    KeyboardMouse,
    /// A controller; the keyboard and mouse only while their pane has the focus.
    Controller,
}

impl SlotInput {
    pub fn named(name: &str) -> Option<SlotInput> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => SlotInput::Auto,
            "kbm" | "keyboard" | "mouse" => SlotInput::KeyboardMouse,
            "pad" | "controller" | "gamepad" => SlotInput::Controller,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SlotInput::Auto => "auto",
            SlotInput::KeyboardMouse => "kbm",
            SlotInput::Controller => "pad",
        }
    }

    /// The copy's input in its spec: player `slot`'s controller seat, or the keyboard
    /// and mouse (`ANY_KEYBOARD`) with no controller.
    pub fn assignment(self, slot: u32) -> InputAssignment {
        match self {
            SlotInput::KeyboardMouse => InputAssignment::KeyboardMouse { device_id: ANY_KEYBOARD },
            SlotInput::Auto | SlotInput::Controller => InputAssignment::Controller { index: slot },
        }
    }
}

impl SplitScreen {
    /// Copies in one split-screen session.
    pub const MAX_INSTANCES: u32 = 4;

    /// A layout's name on the command line.
    pub fn layout_named(name: &str) -> Option<LayoutPreset> {
        Some(match name.to_ascii_lowercase().as_str() {
            "horizontal" => LayoutPreset::Horizontal,
            "vertical" => LayoutPreset::Vertical,
            "grid" | "grid2x2" => LayoutPreset::Grid2x2,
            _ => return None,
        })
    }

    /// `--inputs`' list: one per player, comma-separated.
    pub fn inputs_named(list: &str) -> Option<Vec<SlotInput>> {
        let v = list.split(',').map(SlotInput::named).collect::<Option<Vec<_>>>()?;
        (v.len() <= Self::MAX_INSTANCES as usize).then_some(v)
    }

    /// Player `slot`'s input.
    pub fn input(&self, slot: u32) -> SlotInput {
        self.inputs.get(slot as usize).copied().unwrap_or_default()
    }

    /// `--names`' list: one per player, comma-separated (blank: the PC's naming).
    pub fn names_listed(list: &str) -> Option<Vec<String>> {
        let v: Vec<String> = list.split(',').map(|n| n.trim().chars().take(32).collect()).collect();
        (v.len() <= Self::MAX_INSTANCES as usize).then_some(v)
    }

    /// Player `slot`'s own name, if the session gave one.
    pub fn name(&self, slot: u32) -> Option<String> {
        self.names.get(slot as usize).filter(|n| !n.trim().is_empty()).cloned()
    }
}

impl Default for SplitScreen {
    fn default() -> Self {
        SplitScreen { instances: 2, layout: LayoutPreset::Horizontal, inputs: Vec::new(), names: Vec::new() }
    }
}

/// A game to start, as the command line asked.
#[derive(Default)]
pub struct Launch {
    pub first: PathBuf,
    pub args: Vec<OsString>,
    pub log_dir: Option<PathBuf>,
    pub multi_instance: bool,
    pub copy_saves: bool,
    /// Started by peerparty on Linux, inside Proton (backend/linux/launch.rs): that
    /// process hosts the overlay, and the hook's connection to it is already in the
    /// environment (`PEERPARTY_IPC`).
    pub hosted: bool,
    /// `--set key=value`, `--room`, `--public`: settings for this run.
    pub sets: Vec<(String, String)>,
    pub split: Option<SplitScreen>,
}

/// Starts the game `l` names with PeerParty, hosts it, and returns how it ended.
pub fn start(l: Launch) -> Result<Exited, LaunchError> {
    let mut env = ChildEnv::default();
    // The game itself: a .bat's program on Windows (its variables go into `env`), the
    // game inside Steam's Proton command on Linux. `listed` is what the player chose (a
    // .bat stays the game's name in Settings and Recent).
    let target = backend::resolve(l.first, l.args, &mut env)?;
    let (exe, listed, packaged) = (target.exe.clone(), target.listed.clone(), target.packaged());
    // Before anything runs in the game (anticheat.rs).
    if let Some(found) = anticheat::refuse(&exe) {
        return Err(LaunchError::AntiCheat { exe, found });
    }

    // Hosted, the logs go where the peerparty outside Proton keeps them.
    let outside = l.hosted.then(|| env.get(penv::LOG_DIR).map(PathBuf::from)).flatten();
    let log_dir = l.log_dir.or(outside).unwrap_or_else(|| env::temp_dir().join("peerparty").join("logs"));
    fs::create_dir_all(&log_dir).map_err(|e| format!("{}: {e}", log_dir.display()))?;
    // Where the game's saves with PeerParty's stand-ins go, when not the usual place.
    if let Some(line) = crate::portable::describe(&platform::data_dir()) {
        eprintln!("{line}");
    }

    // The player's settings for this game (the launcher window's, ui.json), with
    // `--set` on top. Variables already set (a .bat's own) win over both.
    let settings = store::Settings::load();
    let game = settings.options(&store::key_of(&listed));
    // The game's folder: programs started from it get PeerParty too (the hook's
    // child.rs), and PeerParty runs until the last of them ends (tree.rs). Not for a
    // packaged game, which follows its own package's programs.
    let root = match env.get(penv::GAME_ROOT) {
        _ if packaged => None,
        Some(r) => Some(PathBuf::from(r)),
        None => gameroot::find(&exe, &settings.folders).map(|(r, how)| {
            eprintln!("peerparty: the game's folder is {} (from {how}); programs started from it get peerparty too", r.display());
            env.set(penv::GAME_ROOT, &r);
            r
        }),
    };
    // Steam sets SteamAppId itself; started outside Steam, the game needs it (steamid.rs).
    let id_known = env.is_set("SteamAppId") || packaged;
    let steam = if id_known { steamid::Game::none() } else { steamid::Game::look(&exe, root.as_deref()) };
    let appid = if id_known { None } else { steam.app_id(&exe) };
    if let Some(id) = &appid {
        eprintln!("peerparty: Steam app id {id}");
        env.set("SteamAppId", id);
        env.set("SteamGameId", id);
    } else if !id_known && steam.uses_steam {
        eprintln!(
            "peerparty: this game uses Steam, but its Steam app id wasn't found (it isn't in a Steam library, and no steam_appid.txt is in its folder): Steam may refuse to start it, or it plays without Steam multiplayer. Put a steam_appid.txt holding the id (the number in the address of its Steam store page) next to {}",
            exe.display()
        );
    }
    let mut prefs = settings.prefs.effective_for(&game);
    for (k, v) in &l.sets {
        apply_set(&mut prefs, k, v);
    }
    settings_env(&mut env, &prefs, &game)?;
    env.set(penv::LOG_DIR, &log_dir);
    // The hook tells its LAN peers which PeerParty it is, commit included (net.rs).
    env.set(penv::VERSION, format!("{} ({})", env!("CARGO_PKG_VERSION"), option_env!("PEERPARTY_DESCRIBE").unwrap_or("no git")));
    // For the hook's child following, which needs us for a child of the other bitness.
    if let Ok(me) = env::current_exe() {
        env.set(penv::EXE, me);
    }
    if l.multi_instance || l.split.is_some() {
        env.set(penv::MULTI_INSTANCE, "1");
    }
    // Tells the hook it is in a game started by peerparty (the hook's own tests run without it).
    env.set(penv::ACTIVE, "1");
    // Only a start given `--copy-saves` copies: one left in the environment is cleared.
    if l.copy_saves {
        env.set(penv::COPY_SAVES, "1");
    } else {
        env.remove(penv::COPY_SAVES);
    }

    if let Some(split) = &l.split {
        // The first player's id is on disk before any copy looks for it (core's
        // `Identity::ensure_first`). Under Proton the copies keep theirs inside Wine.
        #[cfg(windows)]
        peerparty_core::Identity::ensure_first(&peerparty_core::Identity::default_dir());
        return backend::start_session(target, env, split, prefs, &log_dir, root.as_deref());
    }
    // The overlay's connection to the hook. On Linux it is also how `--invite` reaches
    // the game (backend/linux/control.rs), so it is made whether the overlay is on or not.
    let pipe = (!l.hosted && (prefs.overlay || cfg!(unix))).then(|| host::pipe::Pipe::for_game(env.get(penv::IPC).as_deref())).flatten();
    if let Some(p) = &pipe {
        connect(&mut env, p);
    }
    backend::start(target, env, pipe, prefs, &log_dir, root.as_deref())
}

/// The hook's connection to `pipe`: ours is the only one the game gets (and
/// `PEERPARTY_IPC=tcp` was a request, not an endpoint).
pub fn connect(env: &mut ChildEnv, pipe: &host::pipe::Pipe) {
    env.remove(penv::PIPE);
    env.remove(penv::IPC);
    env.set(pipe.env_key(), pipe.endpoint.to_env_string());
}

/// The player's settings as the hook reads them, each unless the environment already
/// says (a .bat's own variables, a test's).
fn settings_env(env: &mut ChildEnv, prefs: &Prefs, game: &store::GameOptions) -> Result<(), LaunchError> {
    if !prefs.name.trim().is_empty() {
        env.set_default(penv::NAME, prefs.name.trim());
    }
    if !prefs.other_name.trim().is_empty() {
        env.set_default(penv::OTHER_NAME, prefs.other_name.trim());
    }
    env.set_default(penv::INVITES, if prefs.auto_accept { "auto" } else { "ask" });
    env.set_default(penv::INVITE_BUTTONS, &prefs.invite_buttons);
    env.set_default(penv::DRAW, &prefs.draw);
    if !prefs.voice {
        env.set_default(penv::VOICE, "0");
    }
    if !prefs.steam_saves {
        env.set_default(penv::STEAM_SAVES, "0");
    }
    if !game.game_key.is_empty() {
        env.set_default(penv::GAME, &game.game_key);
    }
    // Internet play: the room code, checked here so a typo says so at once.
    let room = prefs.room.trim();
    let room_off = room.is_empty() || room == "0" || room.eq_ignore_ascii_case("off");
    if !room_off && !env.is_set(penv::ROOM) {
        let code = peerparty_core::inet::RoomCode::parse(room).ok_or_else(|| LaunchError::Room(room.to_string()))?;
        env.set(penv::ROOM, code.to_string());
        eprintln!("peerparty: internet play on, room {}", code.fingerprint());
    }
    // Public sessions: anyone online with PeerParty and this game.
    if prefs.public && !env.is_set(penv::PUBLIC) {
        env.set(penv::PUBLIC, "1");
        eprintln!("peerparty: public sessions on (anyone online playing this game through peerparty can see and join your games; IP addresses are never shown)");
    }
    let public_name = prefs.public_name.trim();
    if !public_name.is_empty() {
        env.set_default(penv::PUBLIC_NAME, public_name);
    }
    // The internet switch: off keeps the room and public sessions set up (so the overlay
    // can switch them on mid-game) but starts with them off.
    if !prefs.internet && !env.is_set(penv::INTERNET) && (env.is_set(penv::ROOM) || env.is_set(penv::PUBLIC)) {
        env.set(penv::INTERNET, "0");
        eprintln!("peerparty: internet play switched off: local network only (switch it on in the overlay's Internet page, or Settings)");
    }
    // Voice with public players: off unless turned on (an environment value wins).
    env.set_default(penv::PUBLIC_VOICE, if prefs.public_voice { "1" } else { "0" });
    if !game.skip.is_empty() {
        let mut skip: Vec<String> = penv::parse_list(&env.get(penv::SKIP).unwrap_or_default().to_string_lossy()).into_iter().map(String::from).collect();
        skip.extend(game.skip.iter().cloned());
        env.set(penv::SKIP, skip.join(","));
    }
    for (sdk, _, _) in store::sdk_modes() {
        env.set_default(&penv::mode_env_key(sdk), prefs.mode(game, sdk));
    }
    Ok(())
}

/// `--set key=value`: one setting for this run (`auto_accept=on`, `overlay=off`,
/// `invite_buttons=guide`, `voice=off`, `steam_saves=off`, `public_voice=off`, `internet=off`,
/// `language=de`, `effects=off`, `mode.photon=official`, ...).
pub fn apply_set(p: &mut Prefs, k: &str, v: &str) {
    let on = matches!(v.to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes" | "auto");
    match k {
        "name" => p.name = v.to_string(),
        "other_name" => p.other_name = v.to_string(),
        "auto_accept" | "invites" => p.auto_accept = on,
        "overlay" => p.overlay = on,
        "voice" => p.voice = on,
        "steam_saves" => p.steam_saves = on,
        "toasts" => p.toasts = on,
        "invite_buttons" => p.invite_buttons = v.to_string(),
        "draw" => p.draw = v.to_string(),
        "room" => p.room = v.to_string(),
        "public" => p.public = on,
        "public_name" => p.public_name = v.to_string(),
        "public_voice" => p.public_voice = on,
        "internet" => p.internet = on,
        "language" => p.language = v.to_string(),
        "effects" => p.effects = on,
        k if k.starts_with("mode.") => {
            p.modes.insert(k["mode.".len()..].to_string(), v.to_string());
        }
        _ => eprintln!("peerparty: unknown setting {k}"),
    }
}

/// What an exit code usually means: the NTSTATUS of a crash, or a runtime's
/// unhandled-exception code. `None` for an ordinary code the game chose itself.
fn exit_meaning(code: u32) -> Option<&'static str> {
    Some(match code {
        0 => "normal exit",
        0xC000_0005 => "crash: access violation",
        0xC000_0409 => "crash: fail fast / stack buffer overrun (__fastfail)",
        0xC000_0374 => "crash: heap corruption",
        0xC000_00FD => "crash: stack overflow",
        0xC000_001D => "crash: illegal instruction",
        0xC000_0094 => "crash: integer divide by zero",
        0xC000_0135 => "a DLL it needs wasn't found",
        0xC000_0142 => "a DLL failed to initialize",
        0xC000_013A => "stopped with Ctrl+C or its console closed",
        0x4001_0004 => "ended by a debugger or another program",
        0x8000_0003 => "crash: breakpoint",
        0xE043_4352 => "crash: unhandled .NET exception",
        0xE06D_7363 => "crash: unhandled C++ exception",
        _ => return None,
    })
}

/// The line PeerParty logs when the game exits.
pub fn exit_line(code: Option<u32>, ran: Option<std::time::Duration>) -> String {
    let ran = ran.map(|d| format!(" after {:.1} s", d.as_secs_f64())).unwrap_or_default();
    match code {
        None => format!("game exited{ran}; its exit code couldn't be read"),
        Some(c) => {
            let what = exit_meaning(c).map(|m| format!(", {m}")).unwrap_or_default();
            format!("game exited{ran} with exit code {c} ({c:#010X}){what}")
        }
    }
}

/// Says how the game ended (`exit_line`) on the command line and in the hook's log for
/// its process, and hands back that end.
pub fn report_exit(log_dir: &Path, exe: &Path, pid: u32, code: Option<u32>, ran: Option<std::time::Duration>) -> Exited {
    let line = exit_line(code, ran);
    eprintln!("peerparty: {line}");
    log_exit(log_dir, exe, pid, &line);
    Exited(code)
}

/// Appends `line` to the hook's log for that process (`<exe>-<date>-<time>-<pid>.log` in
/// the log folder). If the hook never opened one (the game died before it loaded),
/// a log of that name is started for the line alone.
pub fn log_exit(log_dir: &Path, exe: &Path, pid: u32, line: &str) {
    use std::io::Write;
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let [year, month, day, hour, minute, second] = platform::local_time();
    let path = find_log(log_dir, pid).unwrap_or_else(|| log_dir.join(format!("{stem}-{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{pid}.log")));
    let clock = format!("{hour:02}:{minute:02}:{second:02}");
    // What PeerParty didn't answer this run (the hook's `gap:` lines), in one place.
    let gaps: Vec<String> = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once("] gap: ").map(|(_, g)| g.to_string()))
        .collect();
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = write!(f, "[launcher {clock}] {line}\r\n");
        if !gaps.is_empty() {
            let _ = write!(f, "[launcher {clock}] peerparty didn't answer {} kind(s) of call this run:\r\n", gaps.len());
            for g in &gaps {
                let _ = write!(f, "[launcher {clock}]   gap: {g}\r\n");
            }
        }
    }
}

/// The folder the hooks write their logs in, once a launch said (`set_log_dir`).
static LOG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Where the hooks' logs go, for `note`.
pub fn set_log_dir(dir: &Path) {
    let _ = LOG_DIR.set(dir.to_path_buf());
}

/// A line from the launcher (the session's view of a copy: its panes, its frames) into
/// copy `pid`'s hook log, beside the hook's own: one place to read what happened. Left
/// out when that log isn't there yet.
pub fn note(pid: u32, line: &str) {
    use std::io::Write;
    let Some(path) = LOG_DIR.get().and_then(|d| find_log(d, pid)) else { return };
    let [_, _, _, hour, minute, second] = platform::local_time();
    if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&path) {
        let _ = write!(f, "[launcher {hour:02}:{minute:02}:{second:02}] {line}\r\n");
    }
}

/// The newest log in `dir` written by process `pid` (the hook names its log after the
/// exe and ends it with `-<pid>.log`).
fn find_log(dir: &Path, pid: u32) -> Option<PathBuf> {
    let suffix = format!("-{pid}.log");
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(&suffix))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// Notes the game in `recent.tsv` in PeerParty's folder (`platform::data_dir`: time,
/// exe), the launcher window's Recent list. A test run (`PEERPARTY_LOOPBACK=1`) isn't
/// noted.
///
/// Copies start at the same moment (two players on this PC, the tests), so the line is
/// appended in one write and the file is only ever replaced whole (a new file renamed
/// over it), never emptied and refilled: a launch that read it mid-rewrite used to write
/// back only its own line, and the rest of the list was gone.
pub fn remember(exe: &Path) {
    if peerparty_core::loopback_only() {
        return;
    }
    let dir = platform::data_dir();
    let _ = fs::create_dir_all(&dir);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    note_recent(&dir.join("recent.tsv"), &format!("{now}\t{}", exe.display()), RECENT_KEEP);
}

/// How many launches recent.tsv keeps (trimmed back to that once it has twice as many).
const RECENT_KEEP: usize = 200;

/// Appends `line` to `file` in one write, then, once the file has `2 * keep` lines, cuts
/// it to its last `keep` through a new file renamed over it: a reader sees the old file
/// or the new one, whole.
fn note_recent(file: &Path, line: &str, keep: usize) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(file) {
        use std::io::Write;
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
    let Ok(text) = fs::read_to_string(file) else { return };
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    if lines.len() < keep * 2 {
        return;
    }
    let tmp = file.with_extension(format!("tsv.{}.tmp", std::process::id()));
    if fs::write(&tmp, lines[lines.len() - keep..].join("\n") + "\n").is_err() || fs::rename(&tmp, file).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// `s` in quotes as the C runtime reads them back: a `"` inside is `\"`, and the
/// backslashes before a quote (the closing one too) are doubled.
pub fn crt_quote(s: &str) -> String {
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in s.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Launches at the same moment each keep their line and the ones before them (a
    /// rewrite used to drop the list when another launch read it half written), and the
    /// file is trimmed to its newest lines.
    #[test]
    fn recent_launches_at_once_all_stay() {
        let dir = env::temp_dir().join(format!("peerparty-recent-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("recent.tsv");
        fs::write(&file, "1\tC:\\older.exe\n").unwrap();
        std::thread::scope(|s| {
            for t in 0..8 {
                let file = &file;
                s.spawn(move || {
                    for i in 0..20 {
                        note_recent(file, &format!("2\tC:\\game-{t}-{i}.exe"), 1000);
                    }
                });
            }
        });
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), 161, "every launch noted, none lost");
        assert!(text.starts_with("1\tC:\\older.exe\n"), "the earlier launches are still there");
        note_recent(&file, "3\tC:\\newest.exe", 80);
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), 80);
        assert!(text.ends_with("3\tC:\\newest.exe\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `--set internet=off|on` flips the internet switch for one run; it's on by default.
    #[test]
    fn set_switches_internet_play() {
        let mut p = Prefs::default();
        assert!(p.internet);
        apply_set(&mut p, "internet", "off");
        assert!(!p.internet);
        apply_set(&mut p, "internet", "on");
        assert!(p.internet);
        // Saved settings without the field read as on.
        let old: Prefs = serde_json::from_str(r#"{"room": "x"}"#).unwrap();
        assert!(old.internet);
    }

    #[test]
    fn exit_lines_name_crashes() {
        let ran = Some(std::time::Duration::from_millis(12_340));
        assert_eq!(exit_line(Some(0), ran), "game exited after 12.3 s with exit code 0 (0x00000000), normal exit");
        assert_eq!(
            exit_line(Some(0xC000_0005), None),
            "game exited with exit code 3221225477 (0xC0000005), crash: access violation"
        );
        assert_eq!(exit_line(Some(7), None), "game exited with exit code 7 (0x00000007)");
        assert!(exit_line(None, None).contains("couldn't be read"));
    }

    #[test]
    fn exit_goes_into_the_hooks_log_for_that_process() {
        let dir = env::temp_dir().join(format!("peerparty-exitlog-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let hook_log = dir.join("game-20261031-101010-4242.log");
        fs::write(&hook_log, "[   0.001 t1    ] peerparty hook loaded\r\n").unwrap();
        fs::write(dir.join("game-20261031-101010-14242.log"), "").unwrap();
        let game = Path::new("Games").join("game.exe");
        log_exit(&dir, &game, 4242, "game exited with exit code 0");
        let text = fs::read_to_string(&hook_log).unwrap();
        assert!(text.contains("hook loaded"), "{text}");
        assert!(text.contains("] game exited with exit code 0\r\n"), "{text}");
        assert_eq!(fs::read_to_string(dir.join("game-20261031-101010-14242.log")).unwrap(), "");
        // No hook log for that process: one is started for the line.
        log_exit(&dir, &game, 99, "game exited with exit code 1");
        let started = find_log(&dir, 99).unwrap();
        assert!(started.file_name().unwrap().to_string_lossy().starts_with("game-"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Each reason reads back in the window as it did when the reasons were noted in a
    /// global: the same English line, the same `peerparty-why:` line.
    #[test]
    fn reasons_keep_their_lines() {
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        let e = LaunchError::missing(Path::new(r"C:\x.exe"), &gone);
        assert_eq!(e.to_string(), format!(r"C:\x.exe: {gone}"));
        assert_eq!(e.why(), Some(("missing", r"C:\x.exe".to_string())));
        let elevated = std::io::Error::from_raw_os_error(740);
        assert!(matches!(LaunchError::os("CreateProcess failed".into(), &elevated), LaunchError::Elevation(_)));
        let denied = std::io::Error::from_raw_os_error(5);
        let e = LaunchError::os(format!("CreateProcess failed: {denied}"), &denied);
        assert_eq!(e.why(), Some(("os", denied.to_string())));
        assert_eq!(LaunchError::Room("PP1-x".into()).to_string(), r#""PP1-x" isn't a room code (peerparty --new-room makes one)"#);
        assert_eq!(LaunchError::Machine(0xaa64).to_string(), "unsupported machine type 0xaa64");
        assert_eq!(LaunchError::from("no -p").why(), None);
        assert_eq!(Exited(Some(0xC000_0135)).why(), Some(("exit_dll", "0xC0000135".to_string())));
        assert_eq!(Exited(Some(1)).why(), Some(("exit", "0x00000001".to_string())));
        assert_eq!((Exited(Some(0)).why(), Exited(None).why(), Exited(None).code()), (None, None, 1));
        assert_eq!(why_line("os", "two\tparts\r\n"), "peerparty-why: os\ttwo parts  ");
    }

    /// The game's environment: values already there win over the settings', removals
    /// take a variable away, and on Windows a name matches in any case.
    #[test]
    fn the_childs_environment_is_its_own() {
        let mut e = ChildEnv::default();
        // PATH is always set where tests run: a default doesn't replace it.
        let path = env::var_os("PATH").unwrap();
        e.set_default("PATH", "nope");
        assert_eq!(e.get("PATH"), Some(path.clone()));
        e.set("PEERPARTY_TEST_CHILD_ENV", "1");
        e.set_default("PEERPARTY_TEST_CHILD_ENV", "2");
        assert_eq!(e.get("PEERPARTY_TEST_CHILD_ENV"), Some("1".into()));
        assert!(env::var_os("PEERPARTY_TEST_CHILD_ENV").is_none(), "this process's own environment is untouched");
        e.remove("PATH");
        assert!(!e.is_set("PATH"));
        let vars = e.vars();
        assert!(vars.iter().any(|(k, v)| k == "PEERPARTY_TEST_CHILD_ENV" && v == "1"));
        assert!(!vars.iter().any(|(k, _)| k.eq_ignore_ascii_case("PATH")));
        if cfg!(windows) {
            assert_eq!(e.get("peerparty_test_child_env"), Some("1".into()));
        }
        #[cfg(windows)]
        {
            let block = e.block();
            assert_eq!(&block[block.len() - 2..], [0, 0]);
            let text = String::from_utf16_lossy(&block);
            assert!(text.contains("PEERPARTY_TEST_CHILD_ENV=1\0"));
        }
    }

    #[test]
    fn split_screen_layouts_by_name() {
        assert_eq!(SplitScreen::layout_named("Vertical"), Some(LayoutPreset::Vertical));
        assert_eq!(SplitScreen::layout_named("grid"), Some(LayoutPreset::Grid2x2));
        assert_eq!(SplitScreen::layout_named("diagonal"), None);
        assert_eq!(SplitScreen::default(), SplitScreen { instances: 2, layout: LayoutPreset::Horizontal, inputs: Vec::new(), names: Vec::new() });
    }

    #[test]
    fn split_screen_inputs_by_name() {
        assert_eq!(SplitScreen::inputs_named("kbm,PAD,auto"), Some(vec![SlotInput::KeyboardMouse, SlotInput::Controller, SlotInput::Auto]));
        assert_eq!(SplitScreen::inputs_named("kbm,wheel"), None);
        assert_eq!(SplitScreen::inputs_named("pad,pad,pad,pad,pad"), None, "more than a session has");
        let split = SplitScreen { inputs: vec![SlotInput::KeyboardMouse], ..SplitScreen::default() };
        assert_eq!((split.input(0), split.input(1)), (SlotInput::KeyboardMouse, SlotInput::Auto));
        assert_eq!(SlotInput::KeyboardMouse.assignment(0), InputAssignment::KeyboardMouse { device_id: ANY_KEYBOARD });
        assert_eq!(SlotInput::Auto.assignment(2), InputAssignment::Controller { index: 2 });
    }
}
