//! Which SDK a module is, decided from its export names rather than its file name.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sdk {
    SteamApi,
    SteamClient,
    Party,
    /// PlayFab Party's Xbox Live helper (PartyXboxLive.dll, GDK builds).
    PartyXbl,
    PlayFabMultiplayer,
    PlayFabCore,
    /// Logged only. The EOS front end is a later phase.
    Eos,
    /// GOG Galaxy (Galaxy64.dll / Galaxy.dll): decorated C++ exports in galaxy::api.
    Galaxy,
}

pub fn label(sdk: Sdk) -> &'static str {
    match sdk {
        Sdk::SteamApi => "steamworks",
        Sdk::SteamClient => "steamclient",
        Sdk::Party => "playfab-party",
        Sdk::PartyXbl => "playfab-party-xbl",
        Sdk::PlayFabMultiplayer => "playfab-multiplayer",
        Sdk::PlayFabCore => "playfab-core",
        Sdk::Eos => "eos",
        Sdk::Galaxy => "gog-galaxy",
    }
}

pub fn identify<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<Sdk> {
    let mut steam_api = false;
    let mut create_interface = false;
    let mut bget = false;
    let mut party = false;
    let mut party_xbl = false;
    let mut multiplayer = false;
    let mut core = false;
    let mut eos = false;
    let mut galaxy = false;
    for name in names {
        if name.starts_with("?ProcessData@api@galaxy@@") {
            galaxy = true;
        }
        match name {
            "SteamAPI_Init" | "SteamAPI_InitSafe" | "SteamInternal_SteamAPI_Init" => {
                steam_api = true
            }
            "CreateInterface" => create_interface = true,
            "Steam_BGetCallback" => bget = true,
            "PartyInitialize" => party = true,
            "PartyXblInitialize" => party_xbl = true,
            "PFMultiplayerInitialize" => multiplayer = true,
            "PFServiceConfigCreateHandle" | "PFInitialize" => core = true,
            "EOS_Platform_Create" => eos = true,
            _ => {}
        }
    }
    // steamclient also exports CreateInterface; the callback export is what
    // distinguishes it from a random DLL that happens to export that name.
    if bget && create_interface {
        return Some(Sdk::SteamClient);
    }
    if steam_api {
        return Some(Sdk::SteamApi);
    }
    if party {
        return Some(Sdk::Party);
    }
    if party_xbl {
        return Some(Sdk::PartyXbl);
    }
    if multiplayer {
        return Some(Sdk::PlayFabMultiplayer);
    }
    if core {
        return Some(Sdk::PlayFabCore);
    }
    if eos {
        return Some(Sdk::Eos);
    }
    if galaxy {
        return Some(Sdk::Galaxy);
    }
    None
}

/// `identify` for a loaded module, by its file name as well as its exports: a module
/// named `steam_api*` that exports `SteamAPI_Init` is the game's steam_api, which
/// PeerParty hooks, whatever else it exports. steamclient stays untouched.
pub fn identify_module(file: &str, names: &[&str]) -> Option<Sdk> {
    let kind = identify(names.iter().copied());
    let api = || names.iter().any(|n| matches!(*n, "SteamAPI_Init" | "SteamAPI_InitSafe" | "SteamInternal_SteamAPI_Init"));
    if kind == Some(Sdk::SteamClient) && file.to_ascii_lowercase().starts_with("steam_api") && api() {
        return Some(Sdk::SteamApi);
    }
    kind
}

/// Loaded modules that mean the game is using anti-cheat. The shipping EOS SDK
/// exports `EOS_AntiCheat*` even when the game never calls them, so those names
/// are not a reason to refuse.
pub fn is_anticheat(path: &str) -> bool {
    let n = path.to_ascii_lowercase();
    let file = n.rsplit(['\\', '/']).next().unwrap_or(&n);
    file.contains("easyanticheat")
        || file.contains("start_protected_game")
        || file.contains("battleye")
        || file.starts_with("beclient")
        || file.starts_with("beservice")
        || file.starts_with("bedaisy")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_each_sdk_and_prefers_steamclient() {
        assert_eq!(
            identify(["SteamAPI_Init", "SteamInternal_CreateInterface"]),
            Some(Sdk::SteamApi)
        );
        assert_eq!(
            identify(["CreateInterface", "Steam_BGetCallback"]),
            Some(Sdk::SteamClient)
        );
        assert_eq!(
            identify(["CreateInterface", "Steam_BGetCallback", "SteamAPI_Init"]),
            Some(Sdk::SteamClient)
        );
        // The file name decides when the exports fit both: steam_api's name is steam_api.
        let both = ["CreateInterface", "Steam_BGetCallback", "SteamAPI_Init"];
        assert_eq!(identify_module("steam_api.dll", &both), Some(Sdk::SteamApi));
        assert_eq!(identify_module("steamclient.dll", &both), Some(Sdk::SteamClient));
        assert_eq!(identify_module("steam_api.dll", &both[..2]), Some(Sdk::SteamClient));
        assert_eq!(
            identify(["PartyInitialize", "PartyCreateNewNetwork"]),
            Some(Sdk::Party)
        );
        assert_eq!(
            identify(["PFMultiplayerInitialize"]),
            Some(Sdk::PlayFabMultiplayer)
        );
        assert_eq!(
            identify(["PFServiceConfigCreateHandle"]),
            Some(Sdk::PlayFabCore)
        );
        assert_eq!(
            identify(["EOS_Platform_Create", "EOS_AntiCheatClient_BeginSession"]),
            Some(Sdk::Eos)
        );
        assert_eq!(
            identify(["?ProcessData@api@galaxy@@YAXXZ", "?User@api@galaxy@@YAPEAVIUser@12@XZ"]),
            Some(Sdk::Galaxy)
        );
        assert_eq!(identify(["GetProcAddress"]), None);
    }

    #[test]
    fn anticheat_is_the_module_name() {
        assert!(is_anticheat(r"C:\Games\Foo\EasyAntiCheat_x64.dll"));
        assert!(is_anticheat("start_protected_game.exe"));
        assert!(is_anticheat("BEClient_x64.dll"));
        assert!(is_anticheat("BEDaisy.sys"));
        assert!(!is_anticheat(r"C:\Games\Foo\EOSSDK-Win64-Shipping.dll"));
        assert!(!is_anticheat(r"C:\Games\Foo\steam_api64.dll"));
    }
}
