//! The hook's settings: the launcher's `PEERPARTY_*` variables (protocol/src/env.rs),
//! read in one place, and what can change while the game runs.
//!
//! `HookConfig::global()` is the process's settings, read once at the first ask: the
//! launcher sets them before the game starts and nothing changes them after (the
//! settings the overlay changes live are settings.rs's). Values are read forgivingly, as
//! protocol/src/env.rs says: a switch is any of its yes or no words, any case, spaces
//! around ignored.
//!
//! A test that sets a variable calls `reload()` after it, and the next `global()` sees it.
//!
//! - settings.rs: settings the overlay can change live.
//! - modes.rs: per service, LAN stand-in or the official servers.

pub(crate) mod modes;
pub(crate) mod settings;

use std::path::PathBuf;
use std::sync::atomic::{AtomicPtr, Ordering};

use peerparty_protocol::env::{self, SdkMode};

/// Every `PEERPARTY_*` variable the hook reads. A switch that can be on, off or left to
/// PeerParty (`Option<bool>`) is None when it's unset or says neither.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HookConfig {
    /// `PEERPARTY_ACTIVE`: the game was started through PeerParty (set at all).
    pub active: bool,
    /// `PEERPARTY_VERSION`: the launcher's version.
    pub version: Option<String>,
    /// `PEERPARTY_GAME`: the game's key on the LAN, in place of the exe's name.
    pub game: Option<String>,
    /// `PEERPARTY_DEDICATED`: the dedicated server this copy is (or `0`).
    pub dedicated: Option<String>,
    /// `PEERPARTY_EXE`: the launcher's exe, to inject a child of the other bitness.
    pub exe: Option<String>,
    /// `PEERPARTY_GAME_ROOT`: the folder the game's own processes are started from.
    pub game_root: Option<String>,
    /// `PEERPARTY_FOLLOW`: follow the processes a launcher starts into the game.
    pub follow: bool,
    /// `PEERPARTY_LOG_DIR`: where the log goes.
    pub log_dir: Option<PathBuf>,
    /// `PEERPARTY_IPC` (else `PEERPARTY_PIPE`): the launcher's pipe or socket.
    pub ipc: Option<String>,
    /// `PEERPARTY_SKIP`: layers turned off for this game, lowercase.
    pub skip: Vec<String>,
    /// `PEERPARTY_MODE_<SDK>`: each service's mode as written, by lowercase id.
    pub modes: Vec<(String, String)>,
    /// `PEERPARTY_ROOM` (non-empty) and `PEERPARTY_PUBLIC`: internet play.
    pub room: Option<String>,
    pub public: Option<bool>,

    /// `PEERPARTY_MULTI_INSTANCE`: a second copy may start (set at all).
    pub multi_instance: bool,
    /// `PEERPARTY_VRAM_SHARE`: split-screen copy count (`2..=16`) for fair-share VRAM
    /// budget reporting (`video/vram.rs`).
    pub vram_share: Option<u32>,
    /// `PEERPARTY_CPU_CORES`: logical processor count cap (`1..=256`) for split-screen
    /// copies (`cpu.rs`).
    pub cpu_cores: Option<u32>,
    /// `PEERPARTY_COPY_SAVES` (set at all), `PEERPARTY_STEAM_SAVES`,
    /// `PEERPARTY_SEPARATE_SAVES`.
    pub copy_saves: bool,
    pub steam_saves: Option<bool>,
    pub separate_saves: Option<bool>,
    /// `PEERPARTY_SAVE_GAME`: the game's id for its players' saves.
    pub save_game: Option<String>,
    /// `PEERPARTY_PLAYER_SLOT`: the player this copy plays as (0 is player 1).
    pub player_slot: Option<u32>,
    /// `PEERPARTY_SAVE_SEED`, `PEERPARTY_SAVE_WATCH`.
    pub save_seed: bool,
    pub save_watch: bool,
    /// `PEERPARTY_SAVE_PATHS`, `PEERPARTY_SHARED_SAVE_PATHS`: full paths.
    pub save_paths: Vec<String>,
    pub shared_save_paths: Vec<String>,
    /// `PEERPARTY_SWAP=inline`: functions are swapped in place, not in import tables.
    pub swap_inline: bool,
    /// `PEERPARTY_INVITES`: its starting value.
    pub invites: String,
    /// `PEERPARTY_VOICE`, `PEERPARTY_VOICE_AEC`.
    pub voice: Option<bool>,
    pub voice_aec: Option<bool>,

    /// The services' own switches: `PEERPARTY_PHOTON`, `_PLAYFAB`, `_UGS`, `_XBL`,
    /// `_DEMONWARE`, `_VIVOX`.
    pub photon: Option<bool>,
    pub playfab: Option<bool>,
    pub ugs: Option<bool>,
    pub xbl: Option<bool>,
    pub demonware: Option<bool>,
    pub vivox: Option<bool>,
    /// `PEERPARTY_XBL_REAL`: pass the privacy calls on to the real Xbox Live.
    pub xbl_real: Option<bool>,
    /// `PEERPARTY_STEAM_ACCOUNTS`: each PC's SteamID its own, `PEERPARTY_STEAM_DUMP`.
    pub steam_accounts: Option<bool>,
    pub steam_dump: String,
    /// `PEERPARTY_STEAM_WEB`: PeerParty answers Steam's Web API host (steam/web.rs).
    pub steam_web: Option<bool>,
    /// `PEERPARTY_PHOTON_TRACE`, `PEERPARTY_XBL_TRACE`: log every message.
    pub photon_trace: bool,
    pub xbl_trace: bool,
    /// `PEERPARTY_PHOTON_IP`: a loopback address for the Photon servers (tests).
    pub photon_ip: Option<std::net::Ipv4Addr>,
    /// The stand-ins' folders, for tests: `PEERPARTY_PLAYFAB_DIR`, `_UGS_DIR`,
    /// `_GALAXY_DIR` (non-empty).
    pub playfab_dir: Option<PathBuf>,
    pub ugs_dir: Option<PathBuf>,
    pub galaxy_dir: Option<PathBuf>,
}

impl HookConfig {
    /// The settings in this process's environment now.
    pub fn from_env() -> HookConfig {
        let modes = std::env::vars()
            .filter_map(|(k, v)| Some((k.strip_prefix(env::MODE_PREFIX)?.to_ascii_lowercase(), v)))
            .collect();
        HookConfig { modes, ..HookConfig::from_lookup(|k| std::env::var(k).ok()) }
    }

    /// The settings `get` gives (a variable's value, None when unset). Modes are read
    /// by name: those `get` gives for the services in the catalog.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> HookConfig {
        let text = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let switch = |k: &str| get(k).as_deref().and_then(env::parse_bool);
        let dir = |k: &str| text(k).map(PathBuf::from);
        let modes = peerparty_protocol::catalog::LAYERS
            .iter()
            .map(|l| l.id)
            .chain(["steamid"])
            .filter_map(|id| Some((id.to_string(), get(&env::mode_env_key(id))?)))
            .collect();
        HookConfig {
            active: get(env::ACTIVE).is_some(),
            version: get(env::VERSION),
            game: get(env::GAME),
            dedicated: get(env::DEDICATED),
            exe: get(env::EXE),
            game_root: get(env::GAME_ROOT),
            follow: switch(env::FOLLOW) == Some(true),
            log_dir: get(env::LOG_DIR).map(PathBuf::from),
            ipc: text(env::IPC).or_else(|| text(env::PIPE)),
            skip: env::parse_list(&get(env::SKIP).unwrap_or_default()).into_iter().map(str::to_ascii_lowercase).collect(),
            modes,
            room: text(env::ROOM),
            public: switch(env::PUBLIC),
            multi_instance: get(env::MULTI_INSTANCE).is_some(),
            vram_share: text(env::VRAM_SHARE).and_then(|v| v.trim().parse::<u32>().ok()).filter(|&n| (2..=16).contains(&n)),
            cpu_cores: text(env::CPU_CORES).and_then(|v| v.trim().parse::<u32>().ok()).filter(|&n| (1..=256).contains(&n)),
            copy_saves: get(env::COPY_SAVES).is_some(),
            steam_saves: switch(env::STEAM_SAVES),
            separate_saves: switch(env::SEPARATE_SAVES),
            save_game: text(env::SAVE_GAME).map(|v| v.trim().to_string()).filter(|v| peerparty_protocol::saves::valid_game_id(v)),
            player_slot: text(env::PLAYER_SLOT).and_then(|v| v.trim().parse::<u32>().ok()).filter(|s| *s < peerparty_protocol::saves::MAX_PLAYERS),
            save_seed: switch(env::SAVE_SEED) == Some(true),
            save_watch: switch(env::SAVE_WATCH) == Some(true),
            save_paths: env::path_list(&get(env::SAVE_PATHS).unwrap_or_default()).into_iter().map(String::from).collect(),
            shared_save_paths: env::path_list(&get(env::SHARED_SAVE_PATHS).unwrap_or_default()).into_iter().map(String::from).collect(),
            swap_inline: get(env::SWAP).is_some_and(|v| v.trim().eq_ignore_ascii_case("inline")),
            invites: get(env::INVITES).unwrap_or_default(),
            voice: switch(env::VOICE),
            voice_aec: switch(env::VOICE_AEC),
            photon: switch(env::PHOTON),
            playfab: switch(env::PLAYFAB),
            ugs: switch(env::UGS),
            xbl: switch(env::XBL),
            demonware: switch(env::DEMONWARE),
            vivox: switch(env::VIVOX),
            xbl_real: switch(env::XBL_REAL),
            steam_accounts: switch(env::STEAM_ACCOUNTS),
            steam_dump: get(env::STEAM_DUMP).unwrap_or_default(),
            steam_web: switch(env::STEAM_WEB),
            photon_trace: switch(env::PHOTON_TRACE) == Some(true),
            xbl_trace: switch(env::XBL_TRACE) == Some(true),
            photon_ip: text(env::PHOTON_IP).and_then(|v| v.trim().parse::<std::net::Ipv4Addr>().ok()).filter(|ip| ip.is_loopback()),
            playfab_dir: dir(env::PLAYFAB_DIR),
            ugs_dir: dir(env::UGS_DIR),
            galaxy_dir: dir(env::GALAXY_DIR),
        }
    }

    /// The process's settings (see the module comment).
    pub fn global() -> &'static HookConfig {
        let p = CURRENT.load(Ordering::Acquire);
        if !p.is_null() {
            // SAFETY: set below from a leaked Box and never freed.
            return unsafe { &*p };
        }
        let fresh = Box::into_raw(Box::new(HookConfig::from_env()));
        match CURRENT.compare_exchange(std::ptr::null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => unsafe { &*fresh },
            Err(first) => {
                // Another thread read them first: its copy is the one.
                drop(unsafe { Box::from_raw(fresh) });
                unsafe { &*first }
            }
        }
    }

    /// Whether layer `id` is in `PEERPARTY_SKIP` (case aside).
    pub fn skips(&self, id: &str) -> bool {
        self.skip.iter().any(|s| s.eq_ignore_ascii_case(id))
    }

    /// Service `id`'s mode as written (`PEERPARTY_MODE_<ID>`), trimmed and lowercase;
    /// empty when unset.
    pub fn mode_text(&self, id: &str) -> String {
        self.modes.iter().find(|(k, _)| k.eq_ignore_ascii_case(id)).map(|(_, v)| v.trim().to_ascii_lowercase()).unwrap_or_default()
    }

    /// Service `id`'s mode (LAN when unset or unknown).
    pub fn mode(&self, id: &str) -> SdkMode {
        env::parse_mode(&self.mode_text(id))
    }

    /// Test-override directory for a stand-in store: checks the live process environment
    /// first (test harnesses like `pf_rest_test.py` / `galaxy_test.py` set `PEERPARTY_*_DIR`
    /// after `DllMain`), falling back to the value captured at startup.
    fn live_dir(key: &str, cached: Option<&PathBuf>) -> Option<PathBuf> {
        std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| cached.cloned())
    }

    pub fn playfab_dir(&self) -> Option<PathBuf> {
        Self::live_dir(env::PLAYFAB_DIR, self.playfab_dir.as_ref())
    }

    pub fn ugs_dir(&self) -> Option<PathBuf> {
        Self::live_dir(env::UGS_DIR, self.ugs_dir.as_ref())
    }

    pub fn galaxy_dir(&self) -> Option<PathBuf> {
        Self::live_dir(env::GALAXY_DIR, self.galaxy_dir.as_ref())
    }
}

static CURRENT: AtomicPtr<HookConfig> = AtomicPtr::new(std::ptr::null_mut());

/// Reads the environment again, for a test that just changed it. The old settings stay
/// allocated (a thread may still be reading them). One reload at a time: two tests
/// reloading together could otherwise store the older reading last, losing the variable
/// the other had just set.
#[cfg(test)]
pub fn reload() {
    static RELOAD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = RELOAD.lock().unwrap_or_else(|e| e.into_inner());
    CURRENT.store(Box::into_raw(Box::new(HookConfig::from_env())), Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(pairs: &[(&str, &str)]) -> HookConfig {
        HookConfig::from_lookup(|k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()))
    }

    #[test]
    fn nothing_set_is_every_default() {
        let c = from(&[]);
        assert_eq!(c, HookConfig::default());
        assert!(!c.skips("photon"));
        assert_eq!(c.mode("photon"), SdkMode::Lan);
    }

    #[test]
    fn values_are_read_forgivingly() {
        let c = from(&[
            ("PEERPARTY_ACTIVE", ""),
            ("PEERPARTY_SKIP", "Photon, UGS;eos"),
            ("PEERPARTY_PLAYFAB", " 1 "),
            ("PEERPARTY_UGS", "off"),
            ("PEERPARTY_PHOTON", "maybe"),
            ("PEERPARTY_XBL_TRACE", "YES"),
            ("PEERPARTY_SWAP", "Inline"),
            ("PEERPARTY_ROOM", "   "),
            ("PEERPARTY_PIPE", r"\\.\pipe\x"),
            ("PEERPARTY_UGS_DIR", ""),
            ("PEERPARTY_GALAXY_DIR", r"C:\g"),
            ("PEERPARTY_MODE_PHOTON", " Official "),
            ("PEERPARTY_MODE_STEAMID", "Shared"),
        ]);
        assert!(c.active, "set, even empty");
        assert!(c.skips("photon") && c.skips("ugs") && c.skips("EOS") && !c.skips("steam"));
        assert_eq!((c.playfab, c.ugs, c.photon), (Some(true), Some(false), None));
        assert!(c.xbl_trace && c.swap_inline);
        assert_eq!(c.room, None, "only spaces is no room");
        assert_eq!(c.ipc.as_deref(), Some(r"\\.\pipe\x"));
        assert_eq!(c.ugs_dir, None);
        assert_eq!(c.galaxy_dir, Some(PathBuf::from(r"C:\g")));
        assert_eq!(c.mode("photon"), SdkMode::Official);
        assert_eq!(c.mode_text("steamid"), "shared");
    }

    #[test]
    fn the_pipe_prefers_ipc() {
        assert_eq!(from(&[("PEERPARTY_IPC", "tcp:1"), ("PEERPARTY_PIPE", "p")]).ipc.as_deref(), Some("tcp:1"));
    }

    #[test]
    fn a_reload_sees_a_changed_variable() {
        unsafe { std::env::set_var("PEERPARTY_MODE_CONFIGTESTSDK", "auto") };
        reload();
        assert_eq!(HookConfig::global().mode("configtestsdk"), SdkMode::Auto);
        unsafe { std::env::remove_var("PEERPARTY_MODE_CONFIGTESTSDK") };
        reload();
        assert_eq!(HookConfig::global().mode("configtestsdk"), SdkMode::Lan);
    }
}
