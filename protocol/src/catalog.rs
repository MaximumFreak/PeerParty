//! What PeerParty knows about each online service and each of its own layers, in one
//! place: the launcher's settings and game page, the hook's mode checks, the overlay's
//! notes and the anti-cheat check all read these tables.

use crate::env::SdkMode;

/// How an `auto` service is checked for an answer, once per run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoProbe {
    /// Not checked: `auto` acts as LAN.
    None,
    /// A short TCP connect to the service's host.
    Tcp { host: &'static str, port: u16 },
    /// Whether this user's Steam client is running.
    SteamClient,
    /// Whether a process of this exe name is running.
    Process(&'static str),
}

/// A service that can be answered by PeerParty (LAN) or by its own servers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdkEntry {
    /// Its id: in `PEERPARTY_MODE_<ID>`, the settings' mode map and the overlay's state.
    pub id: &'static str,
    /// Its name for players (the overlay's "goes through ...'s own servers").
    pub label: &'static str,
    /// `PEERPARTY_MODE_<ID>` (`env::mode_env_key`).
    pub env_key: &'static str,
    pub auto_probe: AutoProbe,
    /// PeerParty's own HTTPS server answers it: the certificate check is hooked while
    /// any such service is on.
    pub uses_https: bool,
    /// Its mode can be set (lan, auto, official).
    pub has_mode_toggle: bool,
    /// What its mode is unless set.
    pub default_mode: SdkMode,
    /// The layer (`LAYERS`) that turns PeerParty's handling of it off, if any.
    pub layer: Option<&'static str>,
    /// Its lobbies carry PeerParty's invites (hook/src/invite.rs `CARRIERS`).
    pub is_lobby_carrier: bool,
    /// An online service. False for a setting that only borrows the mode switch (the
    /// Steam player ID).
    pub is_service: bool,
}

impl SdkEntry {
    pub fn auto_probe_host(&self) -> Option<&'static str> {
        match self.auto_probe {
            AutoProbe::Tcp { host, .. } => Some(host),
            _ => None,
        }
    }

    pub fn auto_probe_port(&self) -> Option<u16> {
        match self.auto_probe {
            AutoProbe::Tcp { port, .. } => Some(port),
            _ => None,
        }
    }

    /// Whether the launcher's game page can turn its layer off.
    pub fn has_skip_toggle(&self) -> bool {
        self.layer.and_then(layer).is_some_and(|l| l.on_game_page)
    }
}

const fn sdk(id: &'static str, label: &'static str, env_key: &'static str, auto_probe: AutoProbe) -> SdkEntry {
    SdkEntry {
        id,
        label,
        env_key,
        auto_probe,
        uses_https: false,
        has_mode_toggle: true,
        default_mode: SdkMode::Lan,
        layer: None,
        is_lobby_carrier: false,
        is_service: true,
    }
}

/// Every service, in the order the launcher lists their modes.
pub const SDKS: &[SdkEntry] = &[
    SdkEntry { layer: Some("photon"), ..sdk("photon", "Photon", "PEERPARTY_MODE_PHOTON", AutoProbe::Tcp { host: "ns.photonengine.io", port: 4533 }) },
    SdkEntry {
        layer: Some("eos"),
        is_lobby_carrier: true,
        ..sdk("eos", "Epic Online Services", "PEERPARTY_MODE_EOS", AutoProbe::Tcp { host: "api.epicgames.dev", port: 443 })
    },
    SdkEntry {
        layer: Some("ugs"),
        uses_https: true,
        ..sdk("ugs", "Unity Gaming Services", "PEERPARTY_MODE_UGS", AutoProbe::Tcp { host: "player-auth.services.api.unity.com", port: 443 })
    },
    SdkEntry { layer: Some("playfab"), uses_https: true, ..sdk("playfab", "PlayFab", "PEERPARTY_MODE_PLAYFAB", AutoProbe::Tcp { host: "1.playfabapi.com", port: 443 }) },
    // Its sign-in hosts are each title's own: the domain answering stands in for them.
    SdkEntry {
        layer: Some("demonware"),
        uses_https: true,
        ..sdk("demonware", "Demonware", "PEERPARTY_MODE_DEMONWARE", AutoProbe::Tcp { host: "demonware.net", port: 443 })
    },
    // Its Web API host's liveness check is answered on the HTTPS server (hook/src/steam/web.rs).
    SdkEntry { layer: Some("steam"), is_lobby_carrier: true, uses_https: true, ..sdk("steam", "Steam", "PEERPARTY_MODE_STEAM", AutoProbe::SteamClient) },
    SdkEntry { layer: Some("galaxy"), is_lobby_carrier: true, ..sdk("galaxy", "GOG Galaxy", "PEERPARTY_MODE_GALAXY", AutoProbe::Process("galaxyclient.exe")) },
    // Always PeerParty's while its layer is on: there is no mode to read.
    SdkEntry { layer: Some("xboxlive"), uses_https: true, has_mode_toggle: false, ..sdk("xboxlive", "Xbox Live", "PEERPARTY_MODE_XBOXLIVE", AutoProbe::None) },
    // Whether the game gets a SteamID of its own for this PC (lan), keeps steam_api's
    // (official), or the first unless the Steam client runs the game (auto).
    SdkEntry { default_mode: SdkMode::Auto, is_service: false, ..sdk("steamid", "Steam player ID", "PEERPARTY_MODE_STEAMID", AutoProbe::None) },
];

/// A service by its id (any case).
pub fn sdk_entry(id: &str) -> Option<&'static SdkEntry> {
    SDKS.iter().find(|s| s.id.eq_ignore_ascii_case(id))
}

/// The services whose mode can be set, in the launcher's order.
pub fn mode_sdks() -> impl Iterator<Item = &'static SdkEntry> {
    SDKS.iter().filter(|s| s.has_mode_toggle)
}

/// A service's name for players; "an online service" for an id that isn't one.
pub fn sdk_label(id: &str) -> &'static str {
    sdk_entry(id).map_or("an online service", |s| s.label)
}

/// What a layer is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerKind {
    /// One service's stand-in (it matters once the game uses that service).
    Service,
    /// Something that touches every game.
    General,
    /// Diagnostics only.
    Diagnostic,
}

/// A part of PeerParty a game can turn off (`PEERPARTY_SKIP`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerEntry {
    /// Its id in `PEERPARTY_SKIP` and the launcher's `layer.<id>` / `off.<id>` texts.
    pub id: &'static str,
    pub kind: LayerKind,
    /// The launcher's game page lists it (it has `layer.<id>` and `off.<id>` texts).
    pub on_game_page: bool,
    /// The overlay says so whenever it's off (hook/src/net.rs `GENERAL_LAYERS`); a
    /// service layer only once the game used the service.
    pub always_reported: bool,
    /// Lobbies and invites travel through it: with it off, there's nothing to invite to
    /// (hook/src/invite.rs `LOBBY_LAYERS`).
    pub carries_lobbies: bool,
}

const fn service(id: &'static str, carries_lobbies: bool) -> LayerEntry {
    LayerEntry { id, kind: LayerKind::Service, on_game_page: true, always_reported: false, carries_lobbies }
}

const fn general(id: &'static str) -> LayerEntry {
    LayerEntry { id, kind: LayerKind::General, on_game_page: true, always_reported: true, carries_lobbies: false }
}

/// Every layer, in the game page's order, then the ones only `PEERPARTY_SKIP` reaches.
pub const LAYERS: &[LayerEntry] = &[
    service("steam", true),
    service("eos", true),
    service("party", true),
    service("pfmp", true),
    service("playfab", true),
    service("galaxy", true),
    service("photon", true),
    service("ugs", true),
    service("vivox", false),
    service("xboxlive", false),
    service("xgameui", false),
    general("invite"),
    general("padblock"),
    general("present"),
    general("ipfwd"),
    general("swap"),
    general("dns"),
    // Not on the game page yet (no texts for it).
    LayerEntry { on_game_page: false, ..service("demonware", false) },
    // The game's own SteamID for this PC (hook/src/steamown.rs).
    LayerEntry { on_game_page: false, ..service("steamown", false) },
    // The call-logging hooks on SDK exports.
    LayerEntry { id: "calls", kind: LayerKind::Diagnostic, on_game_page: false, always_reported: false, carries_lobbies: false },
    // The hooks on the game's own libcurl (hook/src/curl/).
    LayerEntry { id: "curl", kind: LayerKind::General, on_game_page: false, always_reported: false, carries_lobbies: false },
];

/// A layer by its id (any case).
pub fn layer(id: &str) -> Option<&'static LayerEntry> {
    LAYERS.iter().find(|l| l.id.eq_ignore_ascii_case(id))
}

/// The layers the launcher's game page lists, in its order.
pub fn game_page_layers() -> impl Iterator<Item = &'static LayerEntry> {
    LAYERS.iter().filter(|l| l.on_game_page)
}

/// The layers the overlay mentions whenever they're off.
pub fn always_reported_layers() -> impl Iterator<Item = &'static LayerEntry> {
    LAYERS.iter().filter(|l| l.always_reported)
}

/// Whether a layer carries lobbies and invites.
pub fn carries_lobbies(id: &str) -> bool {
    layer(id).is_some_and(|l| l.carries_lobbies)
}

/// The layers of the services PeerParty's HTTPS server answers: its certificate check
/// is needed unless every one of them is off.
pub fn https_layers() -> impl Iterator<Item = &'static str> {
    SDKS.iter().filter(|s| s.uses_https).filter_map(|s| s.layer)
}

/// One of the hook's front ends: where a service's events come from (hook/src/net.rs
/// `Front`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrontEntry {
    /// Its name in the overlay state's `sdks` list.
    pub name: &'static str,
    /// Its name for players.
    pub label: &'static str,
    /// The layer it belongs to.
    pub layer: &'static str,
    /// Invites can travel through its lobbies (hook/src/invite.rs `CARRIERS`).
    pub invite_carrier: bool,
}

pub const FRONTS: &[FrontEntry] = &[
    FrontEntry { name: "Eos", label: "Epic Online Services", layer: "eos", invite_carrier: true },
    FrontEntry { name: "Steam", label: "Steam", layer: "steam", invite_carrier: true },
    FrontEntry { name: "Party", label: "PlayFab Party", layer: "party", invite_carrier: false },
    FrontEntry { name: "PlayFab", label: "PlayFab matchmaking", layer: "playfab", invite_carrier: false },
    FrontEntry { name: "PfLobby", label: "PlayFab Lobby", layer: "pfmp", invite_carrier: true },
    FrontEntry { name: "Galaxy", label: "GOG Galaxy", layer: "galaxy", invite_carrier: true },
    FrontEntry { name: "Ugs", label: "Unity Gaming Services", layer: "ugs", invite_carrier: false },
    FrontEntry { name: "Vivox", label: "Vivox", layer: "vivox", invite_carrier: false },
];

/// A front end by its name.
pub fn front(name: &str) -> Option<&'static FrontEntry> {
    FRONTS.iter().find(|f| f.name == name)
}

/// How an anti-cheat file name is recognized (lowercased).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamePattern {
    Contains(&'static str),
    StartsWith(&'static str),
}

/// Anti-cheat programs and modules, by file name. PeerParty isn't for games with
/// anti-cheat (LEGAL.md): the launcher won't start one of these or a game with one
/// beside it, and the hook stops a game in which one loads. Names only: the shipping
/// EOS SDK exports its anti-cheat functions even when a game never calls them.
pub const ANTICHEAT_PATTERNS: &[NamePattern] = &[
    NamePattern::Contains("easyanticheat"),
    NamePattern::Contains("start_protected_game"),
    NamePattern::Contains("battleye"),
    NamePattern::StartsWith("beclient"),
    NamePattern::StartsWith("beservice"),
    NamePattern::StartsWith("bedaisy"),
    NamePattern::StartsWith("eac_launcher"),
];

/// Whether a file (a name, or a path: its last part is what counts) is anti-cheat.
pub fn is_anticheat_name(file: &str) -> bool {
    let file = file.rsplit(['\\', '/']).next().unwrap_or(file).to_ascii_lowercase();
    ANTICHEAT_PATTERNS.iter().any(|p| match p {
        NamePattern::Contains(s) => file.contains(s),
        NamePattern::StartsWith(s) => file.starts_with(s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_service_is_consistent() {
        for s in SDKS {
            assert_eq!(s.env_key, crate::env::mode_env_key(s.id), "{}", s.id);
            assert_eq!(s.id, s.id.to_ascii_lowercase());
            if let Some(l) = s.layer {
                assert!(layer(l).is_some(), "{}'s layer {l} is in LAYERS", s.id);
            }
            assert_eq!(s.auto_probe_host().is_some(), s.auto_probe_port().is_some());
            assert_eq!(sdk_entry(&s.id.to_ascii_uppercase()), Some(s));
        }
        let ids = |it: &mut dyn Iterator<Item = &'static SdkEntry>| it.map(|s| s.id).collect::<Vec<_>>();
        // The hook's auto checks (modes.rs SERVICES) and the launcher's list (store.rs
        // SDK_MODES), with demonware, which the launcher didn't offer.
        assert_eq!(ids(&mut mode_sdks()), ["photon", "eos", "ugs", "playfab", "demonware", "steam", "galaxy", "steamid"]);
        assert_eq!(ids(&mut SDKS.iter().filter(|s| s.uses_https)), ["ugs", "playfab", "demonware", "steam", "xboxlive"]);
        assert_eq!(https_layers().collect::<Vec<_>>(), ["ugs", "playfab", "demonware", "steam", "xboxlive"]);
        assert_eq!(sdk_entry("photon").unwrap().auto_probe_port(), Some(4533));
        assert_eq!(sdk_entry("steam").unwrap().auto_probe, AutoProbe::SteamClient);
        assert_eq!(sdk_entry("steamid").unwrap().default_mode, SdkMode::Auto);
        assert!(sdk_entry("steam").unwrap().has_skip_toggle());
        assert!(!sdk_entry("demonware").unwrap().has_skip_toggle());
        assert!(!sdk_entry("steamid").unwrap().has_skip_toggle());
        // The hook's names for services left on their servers (modes.rs official_in_use).
        assert_eq!(sdk_label("ugs"), "Unity Gaming Services");
        assert_eq!(sdk_label("nothing"), "an online service");
    }

    #[test]
    fn layers_match_the_lists_they_replace() {
        let page: Vec<&str> = game_page_layers().map(|l| l.id).collect();
        // launcher/src/ui/settings.rs layers()
        assert_eq!(
            page,
            ["steam", "eos", "party", "pfmp", "playfab", "galaxy", "photon", "ugs", "vivox", "xboxlive", "xgameui", "invite", "padblock", "present", "ipfwd", "swap", "dns"]
        );
        // hook/src/net.rs GENERAL_LAYERS, in any order
        let mut general: Vec<&str> = always_reported_layers().map(|l| l.id).collect();
        general.sort_unstable();
        assert_eq!(general, ["dns", "invite", "ipfwd", "padblock", "present", "swap"]);
        // hook/src/invite.rs LOBBY_LAYERS
        let mut lobbies: Vec<&str> = LAYERS.iter().filter(|l| l.carries_lobbies).map(|l| l.id).collect();
        lobbies.sort_unstable();
        assert_eq!(lobbies, ["eos", "galaxy", "party", "pfmp", "photon", "playfab", "steam", "ugs"]);
        // Every layer the hook checks with skip().
        for id in ["steam", "eos", "party", "pfmp", "playfab", "galaxy", "photon", "ugs", "vivox", "xboxlive", "xgameui", "demonware", "invite", "padblock", "present", "ipfwd", "swap", "dns", "steamown", "calls", "curl"] {
            assert!(layer(id).is_some(), "{id}");
        }
        let mut seen = std::collections::HashSet::new();
        assert!(LAYERS.iter().all(|l| seen.insert(l.id)), "no layer twice");
        assert!(carries_lobbies("STEAM") && !carries_lobbies("vivox") && !carries_lobbies("nothing"));
    }

    #[test]
    fn fronts_match_the_hook() {
        // hook/src/invite.rs CARRIERS
        let carriers: Vec<&str> = FRONTS.iter().filter(|f| f.invite_carrier).map(|f| f.name).collect();
        assert_eq!(carriers, ["Eos", "Steam", "PfLobby", "Galaxy"]);
        for f in FRONTS {
            assert!(layer(f.layer).is_some_and(|l| l.carries_lobbies || !f.invite_carrier), "{}", f.name);
        }
        assert_eq!(front("PfLobby").unwrap().label, "PlayFab Lobby");
        assert_eq!(front("nothing"), None);
    }

    #[test]
    fn anticheat_is_the_file_name() {
        assert!(is_anticheat_name(r"C:\Games\Foo\EasyAntiCheat_x64.dll"));
        assert!(is_anticheat_name("start_protected_game.exe"));
        assert!(is_anticheat_name("BEClient_x64.dll"));
        assert!(is_anticheat_name("BEService.exe"));
        assert!(is_anticheat_name("BEDaisy.sys"));
        assert!(!is_anticheat_name("/games/foo/BattlEye/launcher.dll"), "a folder's name isn't the file's");
        assert!(is_anticheat_name("MyGame_BattlEye.exe"));
        assert!(is_anticheat_name(r"C:\Games\Foo\EAC_Launcher.exe"));
        assert!(!is_anticheat_name(r"C:\Games\Foo\EOSSDK-Win64-Shipping.dll"));
        assert!(!is_anticheat_name(r"C:\Games\Foo\steam_api64.dll"));
        assert!(!is_anticheat_name("my_beclient.dll"), "a prefix only at the start");
    }
}
