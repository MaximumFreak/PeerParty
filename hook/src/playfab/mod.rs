//! A local stand-in for PlayFab's web API, for games whose PlayFab servers are offline
//! or gone. On for games that use PlayFab Party or PlayFab Multiplayer, and
//! for any other game whenever PlayFab can't be reached (no internet, or the title's
//! servers are gone); a game that only uses PlayFab's web API keeps the real one while
//! it answers.
//!
//! The game's own PlayFab SDK keeps running. Lookups of `<title>.playfabapi.com` resolve
//! to a loopback address of this process (127.80.x.y, from the process id), where
//! PeerParty answers HTTPS (tls.rs) with PlayFab's JSON (api.rs): sign-in, entity
//! tokens, title data, profiles, CloudScript and telemetry succeed locally, and
//! matchmaking tickets are matched between copies of the game on the LAN.
//!
//! References: PlayFab's REST API documentation and its open-source C# SDK
//! (github.com/PlayFab/CSharpSDK), whose request and result models a game ships.
//!
//! PlayFab's newer C SDK (`PlayFabCore.*.dll`, github.com/PlayFab/PlayFabCSdk) signs in
//! through this service too: it is flat C over libHttpClient,
//! whose Win32 build sends the same REST calls through WinHTTP, so its own code runs
//! unchanged and only the host and the certificate check are PeerParty's. Its login
//! result needs `PlayFabId`, `NewlyCreated` and an `EntityToken` object
//! (Source/PlayFabCore/Source/Authentication/CombinedLoginResult.cpp,
//! Generated/AuthenticationTypes.cpp `LoginResult::FromJson`), which api.rs sends. Loading
//! PlayFabCore turns the service on, as Party does.
//!
//! The same HTTPS server answers Unity Gaming Services' hosts for ugs/ (its certificate
//! carries their names too); http.rs routes by the request's Host header.

pub(crate) mod api;
pub(crate) mod http;
pub(crate) mod tls;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, ToSocketAddrs};
use std::sync::OnceLock;

use crate::log;
use crate::pe::Export;

pub use api::{entity_id, entity_of};

/// Whether lookups of `host` come here. `PEERPARTY_PLAYFAB=0` turns it off, `=1` on
/// for any game; otherwise it's on when the game ships or has loaded PlayFab Party,
/// Lobby & Matchmaking or the C SDK, or when the real host doesn't resolve.
pub fn wanted(host: &str) -> bool {
    game_wants().unwrap_or_else(|| unreachable(host))
}

/// Whether the setting or the game's PlayFab SDKs decide that lookups come here; None
/// leaves it to whether the real host answers.
fn game_wants() -> Option<bool> {
    if !crate::net::enabled() || crate::modes::official("playfab") {
        return Some(false);
    }
    match std::env::var("PEERPARTY_PLAYFAB").ok().as_deref() {
        Some("0") => Some(false),
        Some("1") => Some(true),
        _ => (crate::party::loaded() || crate::pfmp::loaded() || CORE.load(Ordering::Relaxed) || ships_sdk()).then_some(true),
    }
}

/// Whether the game's folder holds PlayFab Party, Lobby & Matchmaking or the C SDK
/// (checked once). Games sign in before loading them: a sign-in at the real PlayFab
/// leaves the game searching the real lobbies for LAN ones.
fn ships_sdk() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else { return false };
        let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem()) else { return false };
        let plugins = dir.join(format!("{}_Data", stem.to_string_lossy())).join("Plugins");
        for d in [dir.to_path_buf(), plugins.join("x86_64"), plugins.join("x86"), plugins] {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                let sdk = name.ends_with(".dll")
                    && (name.starts_with("party") || name.starts_with("playfabmultiplayer") || name.starts_with("playfabcore"));
                if sdk {
                    log!("playfab: the game ships {}: PlayFab's web API comes to peerparty", e.file_name().to_string_lossy());
                    return true;
                }
            }
        }
        false
    })
}

static CORE: AtomicBool = AtomicBool::new(false);

/// The game loaded PlayFab's C SDK (PlayFabCore): its sign-in comes to the service.
pub fn core_loaded(module: &str) {
    if !CORE.swap(true, Ordering::Relaxed) {
        log!("playfab {module}: PlayFab's C SDK signs in through PeerParty's PlayFab service");
    }
}

thread_local! {
    /// Set while PeerParty looks up the real host itself, so that lookup isn't redirected.
    static PROBING: Cell<bool> = const { Cell::new(false) };
}

/// Whether PlayFab's real host can't be found (checked once a run).
fn unreachable(host: &str) -> bool {
    static GONE: OnceLock<bool> = OnceLock::new();
    *GONE.get_or_init(|| {
        PROBING.with(|p| p.set(true));
        let found = (host, 443).to_socket_addrs().is_ok_and(|mut a| a.next().is_some());
        PROBING.with(|p| p.set(false));
        if !found {
            log!("playfab {host} doesn't resolve: answering PlayFab locally");
        }
        !found
    })
}

/// This process's loopback address for the service.
fn address() -> Ipv4Addr {
    let pid = std::process::id();
    Ipv4Addr::new(127, 80, (pid >> 8) as u8, (pid as u8).max(1))
}

static SERVER: OnceLock<Option<Ipv4Addr>> = OnceLock::new();

/// Starts the service (once) and returns its address. Unity Gaming Services' stand-in
/// (ugs/) shares it: requests are told apart by their Host header (http.rs).
pub(crate) fn server() -> Option<Ipv4Addr> {
    *SERVER.get_or_init(|| {
        if !tls::ready() {
            return None;
        }
        if tls::arm_crypt32() == 1 {
            log!("playfab certificate check hooked (the game's PlayFab comes here now)");
        }
        // Unity's own TLS (UnityTls) checks against the ROOT store it reads, not crypt32's verdict.
        if tls::arm_root_store() == 1 {
            log!("playfab ROOT store reads hooked (PeerParty's certificate is listed in this game only)");
        }
        let ip = address();
        let listener = match TcpListener::bind(SocketAddrV4::new(ip, 443)) {
            Ok(l) => l,
            Err(e) => {
                log!("playfab service could not listen on {ip}:443: {e}");
                return None;
            }
        };
        log!("playfab service on https://{ip}:443 (PlayFab's hosts resolve here)");
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                std::thread::spawn(move || http::serve(conn));
            }
        });
        Some(ip)
    })
}

/// The address a lookup of `host` should resolve to instead, as text, or None to
/// leave the lookup alone.
pub fn redirect(host: &str) -> Option<String> {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if !(h.ends_with(tls::HOST_SUFFIX) || h == "playfabapi.com")
        || PROBING.with(|p| p.get())
        || !wanted(&h)
    {
        return None;
    }
    let ip = server()?;
    crate::log::throttled(&format!("playfab redirect {h}"), 2, |n| {
        log!("playfab {h} -> {ip} (PeerParty's PlayFab service) n={n}")
    });
    Some(ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The service answers .NET Framework's HttpClient (what PlayFab's C#
    /// SDK uses) over TLS 1.2, Expect: 100-continue and all. The certificate
    /// check is waved through here; tls.rs tests the real trust path.
    #[test]
    fn answers_dotnet_httpclient_over_https() {
        let ip = server().expect("service started");
        let script = format!(
            r#"Add-Type -TypeDefinition 'public static class PpTrust {{ public static void On() {{ System.Net.ServicePointManager.ServerCertificateValidationCallback = (a, b, c, d) => true; }} }}'
[PpTrust]::On()
[Net.ServicePointManager]::SecurityProtocol = 'Tls12'
Add-Type -AssemblyName System.Net.Http
$c = New-Object System.Net.Http.HttpClient
foreach ($i in 1..2) {{
  $body = New-Object System.Net.Http.StringContent('{{"CustomId":"test"}}', [Text.Encoding]::UTF8, 'application/json')
  try {{
    $r = $c.PostAsync([Uri]'https://{ip}/Client/LoginWithCustomID?sdk=test', $body).GetAwaiter().GetResult()
    Write-Output ([int]$r.StatusCode)
    Write-Output $r.Content.ReadAsStringAsync().GetAwaiter().GetResult()
  }} catch {{ Write-Output $_.Exception.ToString() }}
}}
"#
        );
        let file = std::env::temp_dir().join(format!("peerparty-playfab-test-{}.ps1", std::process::id()));
        std::fs::write(&file, script).unwrap();
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&file)
            .output()
            .expect("powershell");
        let _ = std::fs::remove_file(&file);
        let text = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        // Without a LAN the answer is PlayFab's "unavailable" error; with one (another
        // test may have started it), a sign-in. Either way twice, over one kept-alive
        // connection.
        let statuses: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| *l == "200" || *l == "503")
            .collect();
        assert_eq!(statuses.len(), 2, "stdout: {text}
stderr: {err}");
        let wanted = if statuses[0] == "200" {
            "\"Type\":\"title_player_account\""
        } else {
            "PeerParty's LAN isn't running"
        };
        assert_eq!(text.matches(wanted).count(), 2, "{text}");
    }
}

/// crypt32 just loaded: trust checks for the service's certificate go through here.
/// crypt32 loaded: note its certificate check, to hook when the service starts.
pub fn attach(exports: &[Export]) -> bool {
    if !crate::net::enabled() {
        return false;
    }
    let noted = tls::note_crypt32(exports);
    tls::note_open_store(exports);
    // A Unity game with Lobby / Relay (ugs/), or one whose PlayFab comes here (PlayFab's
    // Unity SDK calls through UnityWebRequest): UnityTls reads the ROOT store itself, maybe
    // long before the first lookup, so PeerParty's certificate is listed in it from now on
    // (in this process only; tls.rs).
    if (crate::ugs::wanted() || game_wants() == Some(true)) && tls::arm_root_store() == 1 {
        log!("tls: ROOT store reads hooked (PeerParty's certificate is listed in this game only)");
    }
    noted
}
