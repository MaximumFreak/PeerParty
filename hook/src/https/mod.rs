//! PeerParty's local HTTPS server, which the stand-ins for web services answer on: the
//! game's lookups of a service's host resolve to this process's loopback address
//! (127.80.x.y, from the process id), where TLS (tls.rs) with a certificate made for
//! the services' names, trusted inside this game only, carries HTTP/1.1 (server.rs).
//! Each request goes to the service its `Host` header names (`HttpsService`, listed in
//! hub/mod.rs `HTTPS`).
//!
//! - tls.rs: the certificate, the server side of SChannel, and the crypt32 hooks that
//!   make the game trust that one certificate.
//! - server.rs: requests and answers, routed by Host.
//! - client.rs: the client side, for the few requests passed on to a real service.

pub(crate) mod client;
pub(crate) mod server;
pub(crate) mod tls;

use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::sync::OnceLock;

pub use server::Request;
use tls::TlsStream;

use crate::log;
use crate::sys::pe::Export;

/// A web service the local server answers for.
pub struct HttpsService {
    /// Its name, for the log (and its layer, `PEERPARTY_SKIP`).
    pub name: &'static str,
    /// The names its hosts' certificate must carry (`*.` covers one label).
    pub cert_names: fn() -> Vec<&'static str>,
    /// Whether a request's `Host` header (port and case as the game sent them) is one
    /// of this service's.
    pub is_host: fn(&str) -> bool,
    /// Answers a request: (HTTP status, JSON body; empty for 204).
    pub handle: fn(&Request) -> (u16, String),
    /// Takes the connection over for a WebSocket upgrade, for a service that has one:
    /// the stream, the request and the bytes read past it.
    pub upgrade: Option<fn(TlsStream, &Request, Vec<u8>)>,
    /// Whether the game may check this service's certificate against the ROOT store it
    /// reads itself (UnityTls), before its first lookup comes here: then PeerParty's
    /// certificate is listed there from crypt32's load on.
    pub reads_root_early: fn() -> bool,
}

/// The services, in the order requests are matched; the last answers any request whose
/// Host none of the others claims.
pub fn services() -> &'static [&'static HttpsService] {
    crate::hub::HTTPS
}

/// The service a request's `Host` header names.
pub fn service_for(host: Option<&str>) -> &'static HttpsService {
    let all = services();
    host.and_then(|h| all.iter().find(|s| (s.is_host)(h))).unwrap_or(&all[all.len() - 1])
}

/// Every name the certificate carries, each once.
pub fn cert_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for s in services() {
        for n in (s.cert_names)() {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    names
}

/// The loopback addresses this process may listen on, its own first: 127.80-95.x.y from
/// its process id (unique for ids below 2^20, Windows' are far below that), then the
/// ones after it, in case another process has it (an id wrapped round, or .0 taken as
/// .1).
fn addresses() -> impl Iterator<Item = Ipv4Addr> {
    let pid = std::process::id();
    let first = (pid & 0xF_FFFF).max(1);
    (0..64u32).map(move |i| {
        let n = (first + i) & 0xF_FFFF;
        let n = if n & 0xFF == 0 { n | 1 } else { n };
        Ipv4Addr::new(127, 80 + (n >> 16) as u8, (n >> 8) as u8, n as u8)
    })
}

/// Connections served at once: only this process's game connects, a few at a time;
/// one more than this waits to be accepted until one ends.
const MAX_CONNECTIONS: usize = 64;

static SERVER: OnceLock<Option<Ipv4Addr>> = OnceLock::new();

/// Whether the server is up: then the game's web services are PeerParty's (PlayFab's
/// entity for every player is the one their PeerParty id gives, `playfab::entity_of`).
pub fn serving() -> bool {
    SERVER.get().is_some_and(|s| s.is_some())
}

/// Starts the server (once) and returns its address: the first service a lookup is
/// redirected for starts it, and with it the certificate check hooks.
pub fn server() -> Option<Ipv4Addr> {
    *SERVER.get_or_init(|| {
        if !tls::ready() {
            return None;
        }
        if tls::arm_crypt32() == 1 {
            log!("https certificate check hooked (the game's web services come here now)");
        }
        // Unity's own TLS (UnityTls) checks against the ROOT store it reads, not crypt32's verdict.
        if tls::arm_root_store() == 1 {
            log!("https ROOT store reads hooked (PeerParty's certificate is listed in this game only)");
        }
        let mut last = None;
        let Some((ip, listener)) = addresses().find_map(|ip| match TcpListener::bind(SocketAddrV4::new(ip, 443)) {
            Ok(l) => Some((ip, l)),
            Err(e) => {
                last = Some((ip, e));
                None
            }
        }) else {
            if let Some((ip, e)) = last {
                log!("https service could not listen on {ip}:443 (or the addresses before it): {e}");
            }
            return None;
        };
        // This game's alone: it answers as the player's own web services (their saved
        // data too), so the typed-IP forwarder never brings another player's connection
        // to it.
        peerparty_core::own::tcp(443).forever();
        log!("https service on https://{ip}:443 (the stand-ins' hosts resolve here)");
        std::thread::spawn(move || {
            static OPEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            use std::sync::atomic::Ordering::SeqCst;
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                while OPEN.load(SeqCst) >= MAX_CONNECTIONS {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                OPEN.fetch_add(1, SeqCst);
                let started = std::thread::Builder::new().name("peerparty-https-conn".into()).spawn(move || {
                    struct Done;
                    impl Drop for Done {
                        fn drop(&mut self) {
                            OPEN.fetch_sub(1, SeqCst);
                        }
                    }
                    let _done = Done;
                    server::serve(conn)
                });
                if started.is_err() {
                    OPEN.fetch_sub(1, SeqCst);
                }
            }
        });
        Some(ip)
    })
}

/// crypt32 loaded: its certificate check is noted, to hook when the server starts; and
/// when a service's game reads the ROOT store itself (`reads_root_early`), PeerParty's
/// certificate is listed in it from now on (in this process only; tls.rs).
pub fn attach_crypt32(exports: &[Export]) -> bool {
    if !crate::net::enabled() {
        return false;
    }
    let noted = tls::note_crypt32(exports);
    tls::note_open_store(exports);
    if services().iter().any(|s| (s.reads_root_early)()) && tls::arm_root_store() == 1 {
        log!("tls: ROOT store reads hooked (PeerParty's certificate is listed in this game only)");
    }
    noted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_go_to_the_service_their_host_names() {
        assert_eq!(service_for(Some("Lobby.Services.Api.Unity.Com:443")).name, "ugs");
        assert_eq!(service_for(Some("title-pc-auth3.prod.demonware.net")).name, "demonware");
        assert_eq!(service_for(Some("social.xboxlive.com")).name, "xboxlive");
        assert_eq!(service_for(Some("a3cac.playfabapi.com")).name, "playfab");
        assert_eq!(service_for(Some("api.steampowered.com")).name, "steam");
        // Anything else, or no Host at all: the last one (PlayFab's API).
        assert_eq!(service_for(Some("www.example.com")).name, "playfab");
        assert_eq!(service_for(None).name, "playfab");
        assert_eq!(services().last().unwrap().name, "playfab");
        let names = cert_names();
        for n in ["*.playfabapi.com", "playfabapi.com", "*.services.api.unity.com", "wire.unity3d.com", "*.demonware.net", "privacy.xboxlive.com", "api.steampowered.com"] {
            assert!(names.contains(&n), "{n}");
        }
        assert_eq!(names.len(), names.iter().collect::<std::collections::HashSet<_>>().len(), "each once");
    }

    #[test]
    fn loopback_addresses_are_this_processs_own() {
        let a: Vec<Ipv4Addr> = addresses().collect();
        assert_eq!(a.len(), 64);
        assert!(a.iter().all(|ip| ip.octets()[0] == 127 && (80..96).contains(&ip.octets()[1]) && ip.octets()[3] != 0));
    }

    /// The server answers .NET Framework's HttpClient (what PlayFab's C# SDK uses)
    /// over TLS 1.2, Expect: 100-continue and all, with PlayFab's API. The certificate
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
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("powershell")
            // No console window of its own (tests mustn't flash one on screen).
            .creation_flags(0x0800_0000)
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&file)
            .output()
            .expect("powershell");
        let _ = std::fs::remove_file(&file);
        let text = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        // Without a LAN the answer is PlayFab's "unavailable" error; with one, a sign-in.
        // Another test can start the process's LAN between the two requests, so each is
        // checked on its own: twice, over one kept-alive connection.
        let statuses: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| *l == "200" || *l == "503")
            .collect();
        assert_eq!(statuses.len(), 2, "stdout: {text}
stderr: {err}");
        let signed_in = statuses.iter().filter(|s| **s == "200").count();
        assert_eq!(text.matches("\"Type\":\"title_player_account\"").count(), signed_in, "{text}");
        assert_eq!(text.matches("PeerParty's LAN isn't running").count(), 2 - signed_in, "{text}");
    }
}
