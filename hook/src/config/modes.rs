//! Per online service, whether PeerParty's LAN stand-in answers it or the official
//! servers do: `PEERPARTY_MODE_<SDK>` = `lan`, `official` or `auto` (the launcher sets
//! these from its settings). Without the variable a service is `lan`, as before modes
//! existed.
//!
//! Official leaves the game's own SDK alone, so it meets real online players. Auto is
//! official when
//! the service answers, else LAN: each auto service is checked once, in a thread started
//! with the hook, by a short TCP connect to its host (or, for Steam and GOG Galaxy,
//! whether their client is running). The answer sticks for the run.
//!
//! A decision needed before the check has finished (an SDK DLL the game links statically
//! loads at start-up, under the loader lock, where waiting isn't safe) falls back to LAN.
//! Decisions made from the game's own threads (host lookups) wait for the check, up to
//! two seconds.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use peerparty_protocol::catalog::{AutoProbe, SDKS, SdkEntry, sdk_label};
pub use peerparty_protocol::env::SdkMode as Mode;

use crate::log;

const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const WAIT: Duration = Duration::from_secs(2);

/// Each auto service's check result, when it has finished.
static PROBED: Mutex<Vec<(&'static str, bool)>> = Mutex::new(Vec::new());
/// Decisions already made (they don't change during a run).
static DECIDED: Mutex<Vec<(&'static str, bool)>> = Mutex::new(Vec::new());
static STARTED: OnceLock<Instant> = OnceLock::new();

pub fn mode(sdk: &str) -> Mode {
    super::HookConfig::global().mode(sdk)
}

/// Starts the checks for auto services (once).
pub fn start() {
    if !crate::net::enabled() || STARTED.set(Instant::now()).is_err() {
        return;
    }
    // The services with a check (catalog.rs); one without (the Steam player ID's
    // setting) decides its auto where it's read.
    let auto: Vec<&'static SdkEntry> = SDKS.iter().filter(|s| s.auto_probe != AutoProbe::None && mode(s.id) == Mode::Auto).collect();
    if auto.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new().name("peerparty-modes".into()).spawn(move || {
        // This thread's own lookups must reach the real hosts.
        crate::net::dns::bypass_this_thread();
        for s in auto {
            let up = match s.auto_probe {
                AutoProbe::Tcp { host, port } => reachable(host, port),
                AutoProbe::SteamClient => steam_running(),
                AutoProbe::Process(exe) => process_running(exe),
                AutoProbe::None => false,
            };
            log!("modes: {} is auto: {}", s.id, if up { "official servers answer, using them" } else { "no answer, using LAN" });
            crate::sys::lock(&PROBED).push((s.id, up));
        }
    });
}

fn reachable(host: &str, port: u16) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else { return false };
    addrs.take(2).any(|a| TcpStream::connect_timeout(&a, PROBE_TIMEOUT).is_ok())
}

/// Whether the Steam client is running for this user (its registry key names a
/// live process).
fn steam_running() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let key: Vec<u16> = "Software\\Valve\\Steam\\ActiveProcess".encode_utf16().chain(Some(0)).collect();
    let val: Vec<u16> = "pid".encode_utf16().chain(Some(0)).collect();
    let mut pid = 0u32;
    let mut len = 4u32;
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut pid as *mut u32).cast(), &mut len)
    };
    rc == 0 && pid != 0 && process_alive(pid)
}

fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut code = 0u32;
    let ok = unsafe { windows_sys::Win32::System::Threading::GetExitCodeProcess(h, &mut code) } != 0;
    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    ok && code == 259 // STILL_ACTIVE
}

fn process_running(exe: &str) -> bool {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap as isize == -1 {
        return false;
    }
    let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut found = false;
    let mut ok = unsafe { Process32FirstW(snap, &mut e) } != 0;
    while ok {
        let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
        if String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case(exe) {
            found = true;
            break;
        }
        ok = unsafe { Process32NextW(snap, &mut e) } != 0;
    }
    unsafe { windows_sys::Win32::Foundation::CloseHandle(snap) };
    found
}

fn decide(sdk: &'static str, wait: bool) -> bool {
    if let Some((_, v)) = crate::sys::lock(&DECIDED).iter().find(|(s, _)| *s == sdk) {
        return *v;
    }
    let official = match mode(sdk) {
        Mode::Lan => false,
        Mode::Official => true,
        Mode::Auto => {
            // Under the loader lock the check's thread can't load what it needs, so
            // waiting for it would only stall the game.
            let wait = wait && !crate::watch::in_loader();
            let deadline = Instant::now() + if wait { WAIT } else { Duration::ZERO };
            loop {
                if let Some((_, v)) = crate::sys::lock(&PROBED).iter().find(|(s, _)| *s == sdk) {
                    break *v;
                }
                if Instant::now() >= deadline || STARTED.get().is_none() {
                    log!("modes: {sdk} is auto but its check hasn't finished; using LAN");
                    break false;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    };
    if mode(sdk) == Mode::Official {
        log!("modes: {sdk} uses its official servers");
    }
    crate::sys::lock(&DECIDED).push((sdk, official));
    official
}

/// Whether `sdk` goes to its official servers, from a place that can't wait (a module
/// load): an unfinished auto check counts as LAN.
pub fn official_now(sdk: &'static str) -> bool {
    decide(sdk, false)
}

/// The services this run decided to leave on their official servers (decided only once
/// the game used them), by name for the overlay: PeerParty can't invite through these.
pub fn official_in_use() -> Vec<&'static str> {
    crate::sys::lock(&DECIDED).iter().filter(|(_, official)| *official).map(|(sdk, _)| sdk_label(sdk)).collect()
}

/// Whether `sdk` goes to its official servers, waiting for an auto check if needed.
pub fn official(sdk: &'static str) -> bool {
    decide(sdk, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_read_from_the_environment() {
        unsafe { std::env::set_var("PEERPARTY_MODE_TESTSDK", "Official") };
        crate::config::reload();
        assert_eq!(mode("testsdk"), Mode::Official);
        unsafe { std::env::set_var("PEERPARTY_MODE_TESTSDK", "auto") };
        crate::config::reload();
        assert_eq!(mode("testsdk"), Mode::Auto);
        unsafe { std::env::remove_var("PEERPARTY_MODE_TESTSDK") };
        crate::config::reload();
        assert_eq!(mode("testsdk"), Mode::Lan);
    }
}
