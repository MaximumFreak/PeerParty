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
    pub copy_saves: bool,
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
            copy_saves: false,
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
    /// Voice and party chat with public players (`PEERPARTY_PUBLIC_VOICE=0` when off).
    /// On by default; room and LAN players always have voice.
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
            public_voice: true,
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
    pub prefs: Prefs,
}

/// `Settings::ui_version` now.
pub const UI_VERSION: u32 = 2;

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
            prefs: Prefs::default(),
        }
    }
}

impl Settings {
    fn path() -> PathBuf {
        data_dir().join("ui.json")
    }

    pub fn load() -> Settings {
        let Some(mut s) = fs::read_to_string(Self::path()).ok().and_then(|t| Self::parse(&t)) else {
            return Settings::default();
        };
        if s.ui_version == 0 {
            // Someone who used PeerParty before the first-run setup existed: they've set
            // it up already.
            s.onboarded = true;
            s.ui_version = UI_VERSION;
        }
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
        let _ = fs::create_dir_all(data_dir());
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let tmp = Self::path().with_extension("json.tmp");
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, Self::path());
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

    #[test]
    fn splits_like_a_command_line() {
        assert_eq!(split_args(r#"-windowed  "-name=A B" -x"#), ["-windowed", "-name=A B", "-x"]);
        assert!(split_args("   ").is_empty());
        assert_eq!(split_args(r#""""#), [""]);
    }
}
