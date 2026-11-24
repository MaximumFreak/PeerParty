//! The `PEERPARTY_*` environment variables, and one way of reading each kind of value.
//!
//! The launcher sets most of these for the game it starts (launcher/src/main.rs); the
//! hook and the LAN core read them in the game's process. A player can set any of them
//! by hand too (docs/usage.md), so values are read forgivingly: case and surrounding
//! space don't matter.

// --- Set by the launcher for the game, read by the hook --------------------------------

/// Set (to anything) when a game runs under PeerParty: the LAN engine runs.
pub const ACTIVE: &str = "PEERPARTY_ACTIVE";
/// The overlay pipe's base name (`\\.\pipe\peerparty-<pid>-<digits>`), or an
/// `IpcEndpoint` string (transport.rs).
pub const PIPE: &str = "PEERPARTY_PIPE";
/// The launcher's version and git description, for the hook's version presence.
pub const VERSION: &str = "PEERPARTY_VERSION";
/// Where the hook writes its log.
pub const LOG_DIR: &str = "PEERPARTY_LOG_DIR";
/// The peerparty.exe that started the game (child processes start through it).
pub const EXE: &str = "PEERPARTY_EXE";
/// The folder the game was started from: child processes inside it are the game's.
pub const GAME_ROOT: &str = "PEERPARTY_GAME_ROOT";
/// `1`: every child process the game starts gets the hook too, not just its own.
pub const FOLLOW: &str = "PEERPARTY_FOLLOW";
/// Which players meet: copies with the same game key.
pub const GAME: &str = "PEERPARTY_GAME";
/// The player's name, and the second copy's on the same PC.
pub const NAME: &str = "PEERPARTY_NAME";
pub const OTHER_NAME: &str = "PEERPARTY_OTHER_NAME";
/// Layers turned off for the game, a list (`parse_list`) of `catalog::LAYERS` ids.
pub const SKIP: &str = "PEERPARTY_SKIP";
/// Each service's mode: `PEERPARTY_MODE_<SDK>` (`mode_env_key`).
pub const MODE_PREFIX: &str = "PEERPARTY_MODE_";
/// Where the overlay is drawn: `ingame` inside the game's frames, else a window.
pub const DRAW: &str = "PEERPARTY_DRAW";
/// Whether invites are accepted without asking (`auto`) or asked about (`ask`).
pub const INVITES: &str = "PEERPARTY_INVITES";
/// Voice chat on or off (`parse_bool`).
pub const VOICE: &str = "PEERPARTY_VOICE";
/// Voice's echo cancellation on or off.
pub const VOICE_AEC: &str = "PEERPARTY_VOICE_AEC";
/// Steam cloud saves kept apart per copy on this PC (off: `0`).
pub const STEAM_SAVES: &str = "PEERPARTY_STEAM_SAVES";
/// A second copy's saves kept apart from the first's (off: `0`).
pub const SEPARATE_SAVES: &str = "PEERPARTY_SEPARATE_SAVES";
/// The second copy starts with a copy of the first's saves (`--copy-saves`).
pub const COPY_SAVES: &str = "PEERPARTY_COPY_SAVES";
/// The game's id for its players' saves (saves.rs `game_id`), so a game's starter and
/// the game it starts keep the same folder.
pub const SAVE_GAME: &str = "PEERPARTY_SAVE_GAME";
/// The player this copy plays as (0 is player 1), when the launcher chose one: its
/// saves, registry and Steam Remote Storage are that player's from the hook's start.
pub const PLAYER_SLOT: &str = "PEERPARTY_PLAYER_SLOT";
/// `1`: a later player with no saves of its own yet starts from a copy of player 1's
/// (the launcher's "Start new players from Player 1's saves", `--copy-saves`).
pub const SAVE_SEED: &str = "PEERPARTY_SAVE_SEED";
/// `1`: player 1's copy notes where its saves are (saves.rs `Discovered`), for copying
/// them to other players.
pub const SAVE_WATCH: &str = "PEERPARTY_SAVE_WATCH";
/// More folders that are the game's saves: full paths, separated by `;` (`path_list`).
pub const SAVE_PATHS: &str = "PEERPARTY_SAVE_PATHS";
/// Folders every player shares even when they look like saves (a big mods folder).
pub const SHARED_SAVE_PATHS: &str = "PEERPARTY_SHARED_SAVE_PATHS";
/// More than one copy of the game on this PC (`--multi-instance`).
pub const MULTI_INSTANCE: &str = "PEERPARTY_MULTI_INSTANCE";
/// How a split-screen session's window shows its copies: `gpu` (the default: it draws
/// their frames from shared textures, and shows a copy's window where it can't), `dwm`
/// (always their windows, through DWM thumbnails) or `pixels` (it draws frames the copies
/// read back into files, as a Linux session does; for trying that path on Windows).
pub const COMPOSITOR: &str = "PEERPARTY_COMPOSITOR";
/// Where DXVK's d3d9.dll is (a DXVK release's folder, or `1` for
/// `%LOCALAPPDATA%\peerparty\dxvk`): a split-screen copy of a Direct3D 9 game draws through
/// it, so its frames reach the session's window through Vulkan.
pub const DXVK: &str = "PEERPARTY_DXVK";
/// Fair-share GPU memory budget divisor for split-screen copies (`2..=16`): clamps the
/// VRAM budget reported to each copy (`QueryVideoMemoryInfo` / `VK_EXT_memory_budget`) so
/// engines size their texture streaming pools for sharing one GPU instead of thrashing.
/// Unset or `0`/`1`: off.
pub const VRAM_SHARE: &str = "PEERPARTY_VRAM_SHARE";
/// Logical processor count cap reported to a split-screen copy (`GetSystemInfo` /
/// `GetNativeSystemInfo` / `GetActiveProcessorCount`) so N copies don't spawn N×all-cores
/// busy-spinning worker threads. Unset or `0`: off.
pub const CPU_CORES: &str = "PEERPARTY_CPU_CORES";

/// `1` or `0`: this copy is or isn't a dedicated server (else guessed from its exe).
pub const DEDICATED: &str = "PEERPARTY_DEDICATED";
/// `inline`: the `swap` layer hooks the loader's lookup inline.
pub const SWAP: &str = "PEERPARTY_SWAP";

/// Per-service switches and folders of the hook's stand-ins.
pub const PHOTON: &str = "PEERPARTY_PHOTON";
pub const PHOTON_TRACE: &str = "PEERPARTY_PHOTON_TRACE";
/// The loopback address (127.x.y.z) this copy's Photon servers listen on and Photon's
/// names resolve to, instead of 127.0.0.1: tests give each run one of its own, so runs
/// side by side don't meet on Photon's fixed ports.
pub const PHOTON_IP: &str = "PEERPARTY_PHOTON_IP";
pub const PLAYFAB: &str = "PEERPARTY_PLAYFAB";
pub const PLAYFAB_DIR: &str = "PEERPARTY_PLAYFAB_DIR";
pub const UGS: &str = "PEERPARTY_UGS";
pub const UGS_DIR: &str = "PEERPARTY_UGS_DIR";
pub const GALAXY_DIR: &str = "PEERPARTY_GALAXY_DIR";
pub const DEMONWARE: &str = "PEERPARTY_DEMONWARE";
pub const VIVOX: &str = "PEERPARTY_VIVOX";
pub const XBL: &str = "PEERPARTY_XBL";
pub const XBL_REAL: &str = "PEERPARTY_XBL_REAL";
pub const XBL_TRACE: &str = "PEERPARTY_XBL_TRACE";
pub const STEAM_ACCOUNTS: &str = "PEERPARTY_STEAM_ACCOUNTS";
pub const STEAM_DUMP: &str = "PEERPARTY_STEAM_DUMP";
/// `0`: Steam's Web API host goes to Steam as it is; `1`: PeerParty answers it in any
/// game (by default, in Unity games; hook/src/steam/web.rs).
pub const STEAM_WEB: &str = "PEERPARTY_STEAM_WEB";

// --- Read by the LAN core ----------------------------------------------------------------

/// Sockets bind 127.0.0.1 only (set for tests by .cargo/config.toml).
pub const LOOPBACK: &str = "PEERPARTY_LOOPBACK";
/// PeerParty's folder for its files (identity slots, the same-PC peer board, settings,
/// the stand-ins' stores), instead of `%LOCALAPPDATA%\peerparty` (`~/.local/share/peerparty`).
/// Tests give every run and job one of its own, so they never meet the player's files or
/// each other's. Read by core, the hook and the launcher alike (each checks the name).
pub const DATA_DIR: &str = "PEERPARTY_DATA_DIR";
/// LAN discovery on or off.
pub const LAN_DISCOVERY: &str = "PEERPARTY_LAN_DISCOVERY";
/// Fixed LAN peers' addresses.
pub const PEERS: &str = "PEERPARTY_PEERS";
/// The internet switch: off keeps a room code or public sessions from starting.
pub const INTERNET: &str = "PEERPARTY_INTERNET";
/// A room code for internet play.
pub const ROOM: &str = "PEERPARTY_ROOM";
/// Public sessions on (`parse_bool`), and their settings.
pub const PUBLIC: &str = "PEERPARTY_PUBLIC";
pub const PUBLIC_NAME: &str = "PEERPARTY_PUBLIC_NAME";
pub const PUBLIC_PORT: &str = "PEERPARTY_PUBLIC_PORT";
pub const PUBLIC_VOICE: &str = "PEERPARTY_PUBLIC_VOICE";
pub const PUBLIC_FRIENDS_ONLY: &str = "PEERPARTY_PUBLIC_FRIENDS_ONLY";
/// Internet play's tuning and diagnostics.
pub const INET_PEERS: &str = "PEERPARTY_INET_PEERS";
pub const INET_PORT: &str = "PEERPARTY_INET_PORT";
pub const INET_PORTMAP: &str = "PEERPARTY_INET_PORTMAP";
pub const INET_STUN: &str = "PEERPARTY_INET_STUN";
pub const INET_DHT: &str = "PEERPARTY_INET_DHT";
pub const INET_BOOTSTRAP: &str = "PEERPARTY_INET_BOOTSTRAP";
pub const INET_DEBUG: &str = "PEERPARTY_INET_DEBUG";
pub const IPV6: &str = "PEERPARTY_IPV6";

// --- The launcher's own --------------------------------------------------------------------

/// The window's language.
pub const LANG: &str = "PEERPARTY_LANG";
/// Where the launcher's icon art is written (a developer's tool).
pub const ICON_DIR: &str = "PEERPARTY_ICON_DIR";
/// Screenshots of the window and overlay for the docs (`PREVIEW_DIR`, `PREVIEW_SIZE`).
pub const PREVIEW_DIR: &str = "PEERPARTY_PREVIEW_DIR";
pub const PREVIEW_SIZE: &str = "PEERPARTY_PREVIEW_SIZE";
/// Arguments for the window's own tests.
pub const TEST_ARGV: &str = "PEERPARTY_TEST_ARGV";
/// `1`: a split-screen session's window is made but never shown or put in front (the
/// split-screen smoke test, tests/sdk/split_screen_test.py, which mustn't cover the screen).
pub const SESSION_HIDDEN: &str = "PEERPARTY_SESSION_HIDDEN";
/// Where the core's integration tests log.
pub const TEST_LOG: &str = "PEERPARTY_TEST_LOG";

// --- Build time (launcher/build.rs, tools/) ----------------------------------------------

/// `x64`: only the 64-bit hook is built; `none`: no hook is built (empty stand-ins).
pub const HOOKS: &str = "PEERPARTY_HOOKS";
/// A folder of hook DLLs built elsewhere (`peerparty_hook64.dll`, `peerparty_hook32.dll`):
/// embedded as they are, for a build that can't compile them (Linux).
pub const HOOK_DIR: &str = "PEERPARTY_HOOK_DIR";
/// The release build's flags for the nested hook build.
pub const HOOK_RUSTFLAGS: &str = "PEERPARTY_HOOK_RUSTFLAGS";
/// The embedded hooks' paths, for `include_bytes!`.
pub const HOOK64: &str = "PEERPARTY_HOOK64";
pub const HOOK32: &str = "PEERPARTY_HOOK32";
/// The build's `git describe`.
pub const DESCRIBE: &str = "PEERPARTY_DESCRIBE";

// --- Not read by anything yet: the transports and instance layouts to come ---------------

/// The control connection, as an `IpcEndpoint` string (transport.rs), where a named pipe
/// can't be used (a Linux launcher and a game under Wine).
pub const IPC: &str = "PEERPARTY_IPC";
/// The overlay frame's backing, as a `ShmBacking` string (transport.rs).
pub const FRAME: &str = "PEERPARTY_FRAME";
/// This copy's `session::InstanceSpec`, as JSON.
pub const INSTANCE: &str = "PEERPARTY_INSTANCE";
/// This copy's virtual input block (input_shm.rs), as a `ShmBacking` string.
pub const INPUT_SHM: &str = "PEERPARTY_INPUT_SHM";
/// Where this copy's frames go for a compositor, as a `ShmBacking` string (optional).
pub const CAPTURE_SHM: &str = "PEERPARTY_CAPTURE_SHM";

/// A yes or no: `1`, `on`, `yes`, `true` or `0`, `off`, `no`, `false`, any case, spaces
/// around ignored. Anything else (empty included) is None: the caller's default.
pub fn parse_bool(v: &str) -> Option<bool> {
    let v = v.trim();
    if ["1", "on", "yes", "true"].iter().any(|t| v.eq_ignore_ascii_case(t)) {
        Some(true)
    } else if ["0", "off", "no", "false"].iter().any(|f| v.eq_ignore_ascii_case(f)) {
        Some(false)
    } else {
        None
    }
}

/// `parse_bool` of a variable: None when it's unset, not Unicode, or neither yes nor no.
pub fn env_bool(key: &str) -> Option<bool> {
    std::env::var(key).ok().as_deref().and_then(parse_bool)
}

/// Where a service's calls go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SdkMode {
    /// PeerParty's own stand-in.
    Lan,
    /// The service's own servers: the game's SDK is left alone.
    Official,
    /// Official when the service answers, else LAN (checked once per run).
    Auto,
}

impl SdkMode {
    /// The value the launcher writes and the settings store.
    pub fn as_str(self) -> &'static str {
        match self {
            SdkMode::Lan => "lan",
            SdkMode::Official => "official",
            SdkMode::Auto => "auto",
        }
    }
}

/// A mode's value, when it names one: `lan`, `official` (or `online`), `auto`.
pub fn try_parse_mode(v: &str) -> Option<SdkMode> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("lan") {
        Some(SdkMode::Lan)
    } else if v.eq_ignore_ascii_case("official") || v.eq_ignore_ascii_case("online") {
        Some(SdkMode::Official)
    } else if v.eq_ignore_ascii_case("auto") {
        Some(SdkMode::Auto)
    } else {
        None
    }
}

/// A mode's value: anything that isn't `official` or `auto` is LAN, as before modes
/// existed.
pub fn parse_mode(v: &str) -> SdkMode {
    try_parse_mode(v).unwrap_or(SdkMode::Lan)
}

/// The variable holding a service's mode: `PEERPARTY_MODE_PHOTON` for `photon`.
pub fn mode_env_key(sdk_id: &str) -> String {
    format!("{MODE_PREFIX}{}", sdk_id.to_ascii_uppercase())
}

/// A service's mode from the environment (LAN when unset).
pub fn env_mode(sdk_id: &str) -> SdkMode {
    parse_mode(&std::env::var(mode_env_key(sdk_id)).unwrap_or_default())
}

/// A list's items: split at commas, semicolons and white space, each trimmed, empty
/// ones dropped. Case is kept (`list_contains_ci` compares without it).
pub fn parse_list(v: &str) -> Vec<&str> {
    v.split(|c: char| c == ',' || c == ';' || c.is_whitespace()).filter(|s| !s.is_empty()).collect()
}

/// A list of full paths, separated by `;` as `PATH` is (paths hold spaces and commas).
pub fn path_list(v: &str) -> Vec<&str> {
    v.split(';').map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// Whether a list (`parse_list`) holds `item`, ignoring ASCII case.
pub fn list_contains_ci(list: &str, item: &str) -> bool {
    parse_list(list).iter().any(|s| s.eq_ignore_ascii_case(item.trim()))
}

/// A list variable's items, lowercased (`SKIP`'s layer ids): empty when unset.
pub fn env_list_lower(key: &str) -> Vec<String> {
    let v = std::env::var(key).unwrap_or_default();
    parse_list(&v).into_iter().map(str::to_ascii_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_and_no_read_one_way() {
        for v in ["1", "on", "YES", " True "] {
            assert_eq!(parse_bool(v), Some(true), "{v:?}");
        }
        for v in ["0", "OFF", "no", "false\n"] {
            assert_eq!(parse_bool(v), Some(false), "{v:?}");
        }
        for v in ["", "  ", "auto", "2", "maybe"] {
            assert_eq!(parse_bool(v), None, "{v:?}");
        }
        assert_eq!(env_bool("PEERPARTY_NOT_SET_ANYWHERE_PROTOCOL"), None);
    }

    #[test]
    fn modes_and_their_variables() {
        assert_eq!(parse_mode("Official"), SdkMode::Official);
        assert_eq!(parse_mode(" online "), SdkMode::Official);
        assert_eq!(parse_mode("AUTO"), SdkMode::Auto);
        assert_eq!(parse_mode("lan"), SdkMode::Lan);
        assert_eq!(parse_mode(""), SdkMode::Lan);
        assert_eq!(parse_mode("something"), SdkMode::Lan);
        assert_eq!(try_parse_mode("something"), None);
        for m in [SdkMode::Lan, SdkMode::Official, SdkMode::Auto] {
            assert_eq!(try_parse_mode(m.as_str()), Some(m));
        }
        assert_eq!(mode_env_key("photon"), "PEERPARTY_MODE_PHOTON");
        assert_eq!(mode_env_key("steamid"), "PEERPARTY_MODE_STEAMID");
        assert_eq!(env_mode("protocoltestsdk-unset"), SdkMode::Lan);
    }

    #[test]
    fn lists_split_on_any_separator() {
        assert_eq!(parse_list(" steam, eos;;padblock  present\tdns,"), ["steam", "eos", "padblock", "present", "dns"]);
        assert!(parse_list("").is_empty());
        assert!(parse_list(" , ; ").is_empty());
        assert!(list_contains_ci("Steam,EOS", "eos"));
        assert!(list_contains_ci("steam, eos", " STEAM "));
        assert!(!list_contains_ci("steamown", "steam"), "whole items only");
    }
}
