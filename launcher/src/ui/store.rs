//! What the window remembers (`%LOCALAPPDATA%\peerparty\ui.json`) and the launches
//! peerparty.exe records (`%LOCALAPPDATA%\peerparty\recent.tsv`, written by the launcher
//! for every game it starts, from this window, a .bat or a Steam launch option).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("peerparty")
}

/// A game's identity across scans: its exe path, case folded.
pub fn key_of(exe: &Path) -> String {
    exe.to_string_lossy().to_lowercase()
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GameOptions {
    /// Copies on this PC: 1, or 2 for testing two players on one machine.
    pub players: u8,
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
}

impl Default for GameOptions {
    fn default() -> Self {
        GameOptions {
            players: 1,
            args: String::new(),
            exe: None,
            auto_accept: None,
            overlay: None,
            voice: None,
            steam_saves: None,
            modes: HashMap::new(),
            skip: Vec::new(),
            game_key: String::new(),
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

/// The SDKs that can use official servers instead of PeerParty's own stand-ins, and what
/// each does by default.
pub const SDK_MODES: &[(&str, &str, &str)] = &[
    ("photon", "Photon", "lan"),
    ("eos", "Epic Online Services", "lan"),
    ("ugs", "Unity Gaming Services", "lan"),
    ("playfab", "PlayFab", "lan"),
    ("steam", "Steam lobbies", "lan"),
    ("galaxy", "GOG Galaxy", "lan"),
    // Not a service: whether the game gets a SteamID of its own for this PC ("lan"),
    // keeps steam_api's ("official"), or the first unless the Steam client runs it.
    ("steamid", "Steam player ID", "auto"),
];

impl Prefs {
    /// A game's mode for an SDK: its own, else the global one, else the default.
    pub fn mode(&self, game: &GameOptions, sdk: &str) -> String {
        game.modes
            .get(sdk)
            .or_else(|| self.modes.get(sdk))
            .cloned()
            .unwrap_or_else(|| SDK_MODES.iter().find(|m| m.0 == sdk).map(|m| m.2).unwrap_or("lan").to_string())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Folders whose subfolders are games. `C:\Games` to begin with; the player can
    /// remove it.
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
            folders: vec![PathBuf::from(r"C:\Games")],
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
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            // No file is a first run. Anything else is a file we mustn't replace.
            Err(e) => return Settings { unread: e.kind() != std::io::ErrorKind::NotFound, ..Settings::default() },
        };
        let Some(mut s) = Self::parse(&text) else {
            if !text.trim().is_empty() {
                let _ = fs::write(path.with_extension("json.bad"), &text);
            }
            return Settings::default();
        };
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
        s
    }

    fn parse(text: &str) -> Option<Settings> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let mut s: Settings = serde_json::from_value(v.clone()).ok()?;
        if v.get("ui_version").is_none() {
            s.ui_version = 0;
        }
        Some(s)
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    fn save_to(&self, path: &Path) {
        if self.unread {
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            // A name of this process's own: the window and a game's overlay both save.
            let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
            if fs::write(&tmp, text).is_ok() && fs::rename(&tmp, path).is_err() {
                let _ = fs::remove_file(&tmp);
            }
        }
    }

    /// When ui.json last changed.
    pub fn modified() -> Option<std::time::SystemTime> {
        fs::metadata(Self::path()).and_then(|m| m.modified()).ok()
    }

    pub fn options(&self, key: &str) -> GameOptions {
        self.games.get(key).cloned().unwrap_or_default()
    }
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
        s.save_to(&path);
        assert!(path.is_dir(), "the unreadable thing is still there");
        let fresh = dir.join("first-run.json");
        let mut s = Settings::load_from(&fresh);
        assert!(!s.unread);
        s.prefs.name = "Robin".into();
        s.save_to(&fresh);
        assert_eq!(Settings::load_from(&fresh).prefs.name, "Robin");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2, "no temporary file left behind");
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
        s.save_to(&path);
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

    #[test]
    fn splits_like_a_command_line() {
        assert_eq!(split_args(r#"-windowed  "-name=A B" -x"#), ["-windowed", "-name=A B", "-x"]);
        assert!(split_args("   ").is_empty());
        assert_eq!(split_args(r#""""#), [""]);
    }
}
