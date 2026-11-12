//! PeerParty launcher. One x64 exe for every game: both hook DLLs are embedded (see
//! build.rs and hooks.rs). Started with no arguments (double-clicked) it opens the
//! launcher window (src/ui); with a game on its command line it starts that game.
//!
//! It's a GUI-subsystem program, so double-clicking it opens no console. From a
//! console it attaches to that console for its messages; a .bat file and Steam still
//! wait for it and get the game's exit code.
//!
//! Starts the game suspended, arranges for `LoadLibraryW(<hook dll>)` to run once the
//! loader has mapped the game's static imports and before the game's entry point, and
//! resumes it. Then it waits for the game and returns its exit code, so Steam sees the
//! game running for as long as it runs.
//!
//! - x64 games: an APC queued on the suspended main thread ("early bird").
//! - x86 games (WOW64): an early-bird APC can run before the 32-bit kernel32 is
//!   loaded, so instead the main thread starts on a `jmp $` of the launcher's own (its
//!   start address changed, the game's image untouched). Once it spins there, every
//!   static import is loaded; the 32-bit `LoadLibraryW` is looked up in the game's own
//!   kernel32 and run on a remote thread. Then the main thread jumps on to the entry
//!   point.
//!
//! ```text
//! peerparty [--room CODE] [--log-dir DIR] [--multi-instance] [--copy-saves] <game.exe> [args...]
//! peerparty --invite
//! peerparty --new-room | --net-test | --blocked | --block FP | --unblock FP|all
//! peerparty --friends | --friend FP | --unfriend FP|all
//! ```
//!
//! `--room CODE` (or the Settings page's room) adds internet play: copies
//! started with the same code find each other through the public BitTorrent DHT and
//! connect directly, encrypted, as if on one LAN (core/src/inet, docs/internet-dht.md).
//! `--public` (or the Settings toggle) adds public sessions: anyone online playing the
//! same game through PeerParty, each with a signed key (docs/public-sessions.md).
//!
//! A game started this way plays through PeerParty's own multiplayer layers (EOS, Steam,
//! PlayFab, ...): its lobbies, P2P, friends and invites run between copies on the local
//! network (and over the internet with `--room` or `--public`) instead of through online
//! services.
//!
//! `--invite` asks the game in front (started through peerparty) to invite every other player
//! to where it is, for games without an invite button. Bind it to a hotkey.
//!
//! `--multi-instance` lets a second copy start on the same PC (Unity's single-instance
//! mutex gets a per-process name), for testing two players on one machine.
//!
//! `--copy-saves` starts a second copy on the same PC from the first copy's progress:
//! saves the game keeps under the player's id are copied whole to the second copy's LAN
//! id when the game first looks for them, the second copy's own save moved aside to
//! `<save>.peerparty-before` first, for games that don't offer multiplayer from a fresh
//! save. Only a start given `--copy-saves` copies: a `PEERPARTY_COPY_SAVES` left in the
//! environment is cleared, so a game library, a .bat or a shortcut never replace a save.
//!
//! A Steam game started outside Steam needs its app id: steamid.rs finds it, and says
//! when it can't.
//!
//! As a Steam launch option: `"…\peerparty.exe" %command%`.

#![windows_subsystem = "windows"]

#[cfg(not(target_arch = "x86_64"))]
compile_error!("peerparty.exe is x64 only; it injects 32-bit games itself");

mod firewall;
mod gameroot;
mod host;
mod hooks;
mod i18n;
mod icon_art;
mod packaged;
mod platform;
mod steamid;
mod tree;
mod ui;
mod update;
mod wow64;
mod wrapper;

use std::env;
use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{fs, mem, ptr};

use windows_sys::Win32::Foundation::{CloseHandle, WAIT_FAILED};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION,
    QueueUserAPC, ResumeThread, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

const USAGE: &str = "usage: peerparty [--room CODE] [--public] [--set key=value] [--log-dir DIR] [--multi-instance] [--copy-saves] <game.exe> [args...]\n         plays the game through peerparty: the local network, plus the internet with --room or --public\n         a simple .bat or .cmd that starts the game works too: peerparty starts the program it runs\n         --set changes one setting for this run: name=Alex, voice=off, internet=off, overlay=off, auto_accept=on, ... (docs/usage.md)\n       peerparty --invite   (invites every other player to the game in front)\n       peerparty --new-room   (makes a room code for internet play)\n       peerparty --net-test   (checks this PC's internet path: STUN, NAT type, the DHT)\n       peerparty --pads   (lists the controllers the overlay reads, and their buttons)\n       peerparty --firewall <game.exe>   (whether Windows Firewall lets other players connect to it)\n       peerparty --fix-firewall <game.exe> [--public]   (as administrator: lets them, on private and domain networks, and public ones with --public)\n       peerparty --blocked | --block FINGERPRINT | --unblock FINGERPRINT|all   (public players you blocked)\n       peerparty --friends | --friend FINGERPRINT | --unfriend FINGERPRINT|all   (friends for PEERPARTY_PUBLIC_FRIENDS_ONLY=1)";

/// `--blocked`, `--block FP`, `--unblock FP|all`: the public-sessions block list.
/// `--friends`, `--friend FP`, `--unfriend FP|all`: the same for the friends list.
fn block_list(what: &str, arg: Option<String>) -> Result<u32, String> {
    use peerparty_core::inet::public::{Blocklist, normalize_fingerprint};
    let friends = what.contains("friend");
    let list = if friends { Blocklist::friends(Blocklist::friends_path()) } else { Blocklist::open(Blocklist::default_path()) };
    let what = match what {
        "--friends" => "--blocked",
        "--friend" => "--block",
        "--unfriend" => "--unblock",
        w => w,
    };
    let (noun, did) = if friends { ("friend(s)", "added") } else { ("blocked public player(s)", "blocked") };
    let fp = |a: &Option<String>| a.as_deref().and_then(normalize_fingerprint).ok_or_else(|| "that isn't a key fingerprint (like abcd-ef01-2345-6789)".to_string());
    match what {
        "--blocked" => {
            for (fp, note) in list.list() {
                println!("{fp}  {note}");
            }
            eprintln!("peerparty: {} {noun}", list.len());
        }
        "--block" => {
            let f = fp(&arg)?;
            list.add(&f, "from the command line").map_err(|e| e.to_string())?;
            if friends {
                eprintln!("peerparty: {did} {f} to your friends");
            } else {
                eprintln!("peerparty: {did} {f}; running games drop them within seconds");
            }
        }
        _ => {
            if arg.as_deref().is_some_and(|a| a.eq_ignore_ascii_case("all")) {
                for (f, _) in list.list() {
                    list.remove(&f).map_err(|e| e.to_string())?;
                }
                eprintln!("peerparty: {} cleared", if friends { "friends list" } else { "block list" });
            } else {
                let f = fp(&arg)?;
                let was = list.remove(&f).map_err(|e| e.to_string())?;
                let (yes, no) = if friends { ("removed", "wasn't a friend") } else { ("unblocked", "wasn't blocked") };
                eprintln!("peerparty: {f} {}", if was { yes } else { no });
            }
        }
    }
    Ok(0)
}

/// `--net-test`: what internet play would see from this PC.
fn net_test() -> Result<u32, String> {
    use peerparty_core::inet::{self, Nat};
    eprintln!("peerparty: checking this PC's internet path (about 30 seconds)...");
    let stun: Vec<String> = inet::stun::SERVERS.iter().map(|s| s.to_string()).collect();
    let boot: Vec<String> = inet::dht::BOOTSTRAP.iter().map(|s| s.to_string()).collect();
    let p = inet::probe(&stun, &boot).map_err(|e| format!("net test: {e}"))?;
    for l in &p.lines {
        println!("{l}");
    }
    let verdict = match p.nat {
        _ if p.port_mapped && p.dht_replies > 0 => "internet play should work from here: the router forwards PeerParty's port",
        Nat::Open | Nat::Friendly if p.dht_replies > 0 => "internet play should work from here (the other side needs this too)",
        Nat::Open | Nat::Friendly => "STUN works but the DHT didn't answer: use PEERPARTY_INET_PEERS (each other's address) instead",
        Nat::Symmetric => "this router changes ports per destination (symmetric NAT): hole punching will likely fail; forward a UDP port and set PEERPARTY_INET_PORT",
        Nat::Unknown => "no STUN server answered: UDP may be blocked here",
    };
    println!("verdict: {verdict}");
    Ok(0)
}

/// The event the hook's invite thread watches (hook/src/invite.rs).
const INVITE_EVENT: &str = "Local\\peerparty.invite";
/// The hook's `EVENT_LAST` and `EVENT_ACK` (hook/src/invite.rs).
const INVITE_LAST_EVENT: &str = "Local\\peerparty.invite.last";
const INVITE_ACK_EVENT: &str = "Local\\peerparty.invite.ack";

/// Asks a running game to invite every player on the network: the game in front, or
/// with `last` the one that was last in front (for the launcher's own button, pressed
/// while the launcher is). Whether a game took it up: a game not in front, or none
/// running, doesn't, and saying "sent" then sent the player looking for an invite that
/// never went.
pub fn signal_invite(last: bool) -> Result<bool, String> {
    signal_named(if last { INVITE_LAST_EVENT } else { INVITE_EVENT }, INVITE_ACK_EVENT, 1500)
}

/// Sets `event` until a game answers on `ack` or `wait_ms` is up, then clears both. An
/// answer left from before doesn't count: `ack` is cleared first.
fn signal_named(event: &str, ack: &str, wait_ms: u32) -> Result<bool, String> {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent, WaitForSingleObject};
    let open = |name: &str| {
        let name = wide(OsStr::new(name));
        unsafe { CreateEventW(ptr::null(), 1, 0, name.as_ptr()) }
    };
    unsafe {
        let (event, ack) = (open(event), open(ack));
        if event.is_null() || ack.is_null() {
            let e = os_error("CreateEvent");
            for h in [event, ack] {
                if !h.is_null() {
                    CloseHandle(h);
                }
            }
            return Err(e);
        }
        ResetEvent(ack);
        SetEvent(event);
        // A game's trigger thread wakes on the event; one that is busy takes a moment.
        let taken = WaitForSingleObject(ack, wait_ms) == WAIT_OBJECT_0;
        ResetEvent(event);
        ResetEvent(ack);
        CloseHandle(event);
        CloseHandle(ack);
        Ok(taken)
    }
}

/// Windows looks for a DLL beside the exe before it looks in System32, and peerparty.exe
/// is run from wherever it was downloaded to: a file named like a system DLL, dropped
/// into Downloads by any web page, would be loaded into it, and into the copy of it
/// that "Fix firewall" starts as administrator. Nothing peerparty.exe needs is beside
/// it (it is one file), so the folder is taken out of the search, first thing.
///
/// That covers what is loaded from here on. What Windows loads before `main` is the
/// linker's part (build.rs): the DLLs the exe names that Windows doesn't always take
/// from System32 are loaded on first use instead, which is after this, and so are the
/// DLLs those bring along. tools/dll-search-check.ps1 shows whether it holds.
fn only_system_dlls() {
    use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LOAD_LIBRARY_SEARCH_USER_DIRS, SetDefaultDllDirectories};
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_USER_DIRS) };
}

fn main() -> ExitCode {
    only_system_dlls();
    // No arguments: the window. `--updated`: the window, started by the update that
    // put this exe in place (update.rs).
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args.is_empty() || (args.len() == 1 && args[0] == "--updated") {
        update::clean_up();
        hooks::clean_up();
        return match ui::run(!args.is_empty()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        };
    }
    attach_console();
    match run() {
        // The whole exit code, not just its low byte, so a .bat or Steam sees a crash's
        // NTSTATUS (0xC0000005) rather than 5.
        Ok(code) => std::process::exit(code as i32),
        Err(e) => {
            eprintln!("peerparty: {e}");
            ExitCode::from(1)
        }
    }
}

/// Messages go to the console PeerParty was started from, when it has no output of its
/// own (a GUI-subsystem program gets none); redirected output is left alone. Redirected
/// or not, PeerParty joins that console, so a console program it starts shares it
/// instead of opening a window of its own.
fn attach_console() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    unsafe {
        let has = |which| {
            let h = GetStdHandle(which);
            !h.is_null() && h != INVALID_HANDLE_VALUE
        };
        let redirected = has(STD_ERROR_HANDLE);
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 || redirected {
            return;
        }
        let out = CreateFileW(
            wide(OsStr::new("CONOUT$")).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        );
        if out != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_ERROR_HANDLE, out);
            if !has(STD_OUTPUT_HANDLE) {
                SetStdHandle(STD_OUTPUT_HANDLE, out);
            }
        }
    }
}

fn run() -> Result<u32, String> {
    let mut args = env::args_os().skip(1).peekable();
    let mut log_dir = None;
    let mut multi_instance = false;
    let mut copy_saves = false;
    let mut sets: Vec<(String, String)> = Vec::new();
    while let Some(a) = args.peek() {
        match a.to_str() {
            Some("--set") => {
                args.next();
                let kv = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                let (k, v) = kv.split_once('=').ok_or(USAGE)?;
                sets.push((k.trim().to_string(), v.trim().to_string()));
            }
            Some("--log-dir") => {
                args.next();
                log_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?));
            }
            Some("--multi-instance") => {
                args.next();
                multi_instance = true;
            }
            Some("--copy-saves") => {
                args.next();
                copy_saves = true;
            }
            Some("--room") => {
                args.next();
                let code = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                sets.push(("room".into(), code));
            }
            Some("--new-room") => {
                let code = peerparty_core::inet::RoomCode::generate();
                println!("{code}");
                eprintln!("peerparty: a new room code. Give it to the friends you play with (privately: anyone with it can join), and start the game with --room {code}, or put it in the launcher's Settings.");
                return Ok(0);
            }
            Some("--net-test") => return net_test(),
            Some("--public") => {
                args.next();
                sets.push(("public".into(), "on".into()));
            }
            Some(w @ ("--blocked" | "--block" | "--unblock" | "--friends" | "--friend" | "--unfriend")) => {
                let w = w.to_string();
                args.next();
                let arg = if w == "--blocked" || w == "--friends" { None } else { Some(args.next().ok_or(USAGE)?.to_string_lossy().into_owned()) };
                return block_list(&w, arg);
            }
            Some("--invite") => {
                return if signal_invite(false)? {
                    eprintln!("peerparty: the game in front is inviting every player on your network");
                    Ok(0)
                } else {
                    Err("no game took the invite: it goes to a game started through peerparty whose window is in front".into())
                };
            }
            Some("--firewall") => {
                args.next();
                let exe = PathBuf::from(args.next().ok_or(USAGE)?);
                let v = firewall::Checker::default().verdict(&exe);
                match (v.warning(), v.note()) {
                    (Some(w), _) => eprintln!("peerparty: {w} ({} rule(s): peerparty --fix-firewall to fix)", v.blocks.len()),
                    (None, Some(n)) => eprintln!("peerparty: {n}"),
                    (None, None) => eprintln!("peerparty: Windows Firewall lets other players connect to {}", v.exe),
                }
                return Ok(if v.blocked() { 1 } else { 0 });
            }
            // Started elevated by the launcher or the overlay, when the player asks.
            Some("--fix-firewall") => {
                args.next();
                let exe = PathBuf::from(args.next().ok_or(USAGE)?);
                let rest: Vec<String> = args.by_ref().map(|a| a.to_string_lossy().into_owned()).collect();
                let public = rest.iter().any(|a| a == "--public");
                // The launcher's pipe for why it failed (this runs hidden).
                let report = rest.iter().position(|a| a == "--report").and_then(|i| rest.get(i + 1));
                return match firewall::fix(&exe, public) {
                    Ok(msg) => {
                        eprintln!("peerparty: {msg}");
                        Ok(0)
                    }
                    Err(e) => {
                        if let Some(pipe) = report {
                            firewall::report(pipe, &e);
                        }
                        Err(format!("the firewall change failed: {e}"))
                    }
                };
            }
            Some("--pads") => {
                host::print_pads(30);
                return Ok(0);
            }
            // Run by Windows for a packaged (Microsoft Store) game: see packaged.rs.
            Some("--attach") => {
                let rest: Vec<String> = args.skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
                return packaged::attach(&rest);
            }
            // Asked for: on standard output, and not an error.
            Some("-h" | "--help" | "/?") => {
                println!("{USAGE}");
                return Ok(0);
            }
            _ => break,
        }
    }
    let exe = PathBuf::from(args.next().ok_or(USAGE)?);
    let game_args: Vec<OsString> = args.collect();

    let exe = fs::canonicalize(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let exe = strip_verbatim(&exe);
    // What the player chose (a .bat stays the game's name in Settings and Recent).
    let listed = exe.clone();
    // A .bat or .cmd: the program it runs is the game (wrapper.rs).
    let (exe, game_args, script_cwd) = if wrapper::is_script(&exe) {
        let w = wrapper::read(&exe, &game_args)?;
        let real = fs::canonicalize(&w.exe).map_err(|e| format!("{}: {e}", w.exe.display()))?;
        eprintln!("peerparty: {} runs {}: starting that", exe.display(), w.exe.display());
        for (k, v) in &w.vars {
            unsafe { env::set_var(k, v) };
        }
        (strip_verbatim(&real), w.args, w.cwd)
    } else {
        (exe, game_args, None)
    };
    // A packaged game's program can only be run, not read (its files allow execute but not
    // read data), and `--attach` works out its bitness from the running process.
    let pkg = packaged::find(&exe);

    let log_dir = log_dir.unwrap_or_else(|| env::temp_dir().join("peerparty").join("logs"));
    fs::create_dir_all(&log_dir).map_err(|e| format!("{}: {e}", log_dir.display()))?;

    // The player's settings for this game (the launcher window's, ui.json), with
    // `--set` on top. Variables already set (a .bat's own) win over both.
    let settings = ui::store::Settings::load();
    let game = settings.options(&ui::store::key_of(&listed));
    // The game's folder: programs started from it get PeerParty too (the hook's
    // child.rs), and PeerParty runs until the last of them ends (tree.rs). Not for a
    // packaged game, which follows its own package's programs.
    let root = match env::var_os("PEERPARTY_GAME_ROOT") {
        _ if pkg.is_some() => None,
        Some(r) => Some(PathBuf::from(r)),
        None => gameroot::find(&exe, &settings.folders).map(|(r, how)| {
            eprintln!("peerparty: the game's folder is {} (from {how}); programs started from it get peerparty too", r.display());
            unsafe { env::set_var("PEERPARTY_GAME_ROOT", &r) };
            r
        }),
    };
    // The child inherits these. Steam sets SteamAppId itself; started outside Steam, the
    // game needs it (steamid.rs).
    let id_known = env::var_os("SteamAppId").is_some() || pkg.is_some();
    let steam = if id_known { steamid::Game::none() } else { steamid::Game::look(&exe, root.as_deref()) };
    let appid = if id_known { None } else { steam.app_id(&exe) };
    if let Some(id) = &appid {
        eprintln!("peerparty: Steam app id {id}");
    } else if !id_known && steam.uses_steam {
        eprintln!(
            "peerparty: this game uses Steam, but its Steam app id wasn't found (it isn't in a Steam library, and no steam_appid.txt is in its folder): Steam may refuse to start it, or it plays without Steam multiplayer. Put a steam_appid.txt holding the id (the number in the address of its Steam store page) next to {}",
            exe.display()
        );
    }
    let mut prefs = host::effective(&settings, &game);
    for (k, v) in &sets {
        apply_set(&mut prefs, k, v);
    }
    let pipe = prefs.overlay.then(host::pipe::Pipe::create).flatten();
    unsafe {
        let default = |k: &str, v: &str| {
            if env::var_os(k).is_none() {
                env::set_var(k, v);
            }
        };
        if !prefs.name.trim().is_empty() {
            default("PEERPARTY_NAME", prefs.name.trim());
        }
        default("PEERPARTY_INVITES", if prefs.auto_accept { "auto" } else { "ask" });
        default("PEERPARTY_INVITE_BUTTONS", &prefs.invite_buttons);
        default("PEERPARTY_DRAW", &prefs.draw);
        if !prefs.voice {
            default("PEERPARTY_VOICE", "0");
        }
        if !prefs.steam_saves {
            default("PEERPARTY_STEAM_SAVES", "0");
        }
        if !game.game_key.is_empty() {
            default("PEERPARTY_GAME", &game.game_key);
        }
        // Internet play: the room code, checked here so a typo says so at once.
        let room = prefs.room.trim();
        let room_off = room.is_empty() || room == "0" || room.eq_ignore_ascii_case("off");
        if !room_off && env::var_os("PEERPARTY_ROOM").is_none() {
            match peerparty_core::inet::RoomCode::parse(room) {
                Some(code) => {
                    env::set_var("PEERPARTY_ROOM", code.to_string());
                    eprintln!("peerparty: internet play on, room {}", code.fingerprint());
                }
                None => return Err(format!("{room:?} isn't a room code (peerparty --new-room makes one)")),
            }
        }
        // Public sessions: anyone online with PeerParty and this game.
        if prefs.public && env::var_os("PEERPARTY_PUBLIC").is_none() {
            env::set_var("PEERPARTY_PUBLIC", "1");
            eprintln!("peerparty: public sessions on (anyone online playing this game through peerparty can see and join your games; IP addresses are never shown)");
        }
        let public_name = prefs.public_name.trim();
        if !public_name.is_empty() && env::var_os("PEERPARTY_PUBLIC_NAME").is_none() {
            env::set_var("PEERPARTY_PUBLIC_NAME", public_name);
        }
        // The internet switch: off keeps the room and public sessions set up (so the
        // overlay can switch them on mid-game) but starts with them off.
        if !prefs.internet && env::var_os("PEERPARTY_INTERNET").is_none() && (env::var_os("PEERPARTY_ROOM").is_some() || env::var_os("PEERPARTY_PUBLIC").is_some()) {
            env::set_var("PEERPARTY_INTERNET", "0");
            eprintln!("peerparty: internet play switched off: local network only (switch it on in the overlay's Internet page, or Settings)");
        }
        // Voice with public players: off unless turned on (an environment value wins).
        if env::var_os("PEERPARTY_PUBLIC_VOICE").is_none() {
            env::set_var("PEERPARTY_PUBLIC_VOICE", if prefs.public_voice { "1" } else { "0" });
        }
        if !game.skip.is_empty() {
            let mut skip: Vec<String> = env::var("PEERPARTY_SKIP").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            skip.extend(game.skip.iter().cloned());
            env::set_var("PEERPARTY_SKIP", skip.join(","));
        }
        for (sdk, _, _) in ui::store::SDK_MODES {
            default(&format!("PEERPARTY_MODE_{}", sdk.to_ascii_uppercase()), &prefs.mode(&game, sdk));
        }
        if let Some(p) = &pipe {
            env::set_var("PEERPARTY_PIPE", &p.name);
        }
        env::set_var("PEERPARTY_LOG_DIR", &log_dir);
        // The hook tells its LAN peers which PeerParty it is, commit included (net.rs).
        env::set_var(
            "PEERPARTY_VERSION",
            format!("{} ({})", env!("CARGO_PKG_VERSION"), option_env!("PEERPARTY_DESCRIBE").unwrap_or("no git")),
        );
        // For the hook's child following, which needs us for a child of the other bitness.
        if let Ok(me) = env::current_exe() {
            env::set_var("PEERPARTY_EXE", me);
        }
        if let Some(id) = &appid {
            env::set_var("SteamAppId", id);
            env::set_var("SteamGameId", id);
        }
        if multi_instance {
            env::set_var("PEERPARTY_MULTI_INSTANCE", "1");
        }
        // Tells the hook it is in a game started by peerparty (the hook's own tests run without it).
        env::set_var("PEERPARTY_ACTIVE", "1");
        if copy_saves {
            env::set_var("PEERPARTY_COPY_SAVES", "1");
        } else {
            env::remove_var("PEERPARTY_COPY_SAVES");
        }
    }

    if let Some(pkg) = pkg {
        return run_packaged(&pkg, &exe, &game_args, pipe, prefs, &log_dir);
    }
    let x86 = match pe_machine(&exe)? {
        0x8664 => false,
        0x014c => true,
        m => return Err(format!("unsupported machine type {m:#x}")),
    };
    let dll = hooks::extract(x86)?;

    let mut cmdline = quote(exe.as_os_str());
    for a in &game_args {
        cmdline.push(" ");
        cmdline.push(quote(a));
    }
    let cwd = script_cwd.as_deref().or(exe.parent()).unwrap_or(Path::new("."));

    let tree = tree::Tree::new(root.as_deref());

    let pi = unsafe { start_injected(&exe, &cmdline, cwd, &dll, x86, tree.as_ref())? };
    remember(&listed);
    eprintln!(
        "peerparty: started {} (pid {}, {}), logs in {}",
        exe.display(),
        pi.dwProcessId,
        if x86 { "x86" } else { "x64" },
        log_dir.display()
    );

    let watch = tree.map(|t| t.watch(pi.hProcess, pi.dwProcessId, exe.clone()));
    let done = watch.as_ref().map_or(pi.hProcess, |w| w.0);
    if let Some(pipe) = pipe {
        host::run(done, pi.dwProcessId, pipe, &exe, prefs, false);
    }
    unsafe {
        let waited = WaitForSingleObject(done, INFINITE) != WAIT_FAILED;
        let ended = watch.as_ref().and_then(|w| w.1.lock().unwrap_or_else(|e| e.into_inner()).take()).unwrap_or_else(|| {
            let mut code = 0u32;
            let known = waited && GetExitCodeProcess(pi.hProcess, &mut code) != 0;
            tree::Ended { pid: pi.dwProcessId, exe: exe.clone(), code: known.then_some(code), ran: run_time(pi.hProcess), starter: None }
        });
        if let Some((pid, starter, code)) = &ended.starter {
            let code = code.map(|c| format!(" with exit code {c}")).unwrap_or_default();
            log_exit(&log_dir, starter, *pid, &format!("{} ended{code}; the game went on in {} (pid {})", starter.display(), ended.exe.display(), ended.pid));
        }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        if let Some(w) = &watch {
            CloseHandle(w.0);
        }
        let line = exit_line(ended.code, ended.ran);
        eprintln!("peerparty: {line}");
        log_exit(&log_dir, &ended.exe, ended.pid, &line);
        Ok(ended.code.unwrap_or(1))
    }
}

/// A Microsoft Store (GDK) game: started through Windows' packaged-app activation with
/// PeerParty attached (packaged.rs), then hosted and waited for like any other.
fn run_packaged(
    pkg: &packaged::Package,
    exe: &Path,
    game_args: &[OsString],
    pipe: Option<host::pipe::Pipe>,
    prefs: ui::store::Prefs,
    log_dir: &Path,
) -> Result<u32, String> {
    let peerparty = env::current_exe().map_err(|e| e.to_string())?;
    let vars: Vec<(String, String)> = env::vars().filter(|(k, _)| k.starts_with("PEERPARTY_") || k == "SteamAppId" || k == "SteamGameId").collect();
    let args: Vec<String> = game_args.iter().map(|a| quote(a).to_string_lossy().into_owned()).collect();
    let helper = packaged::start(pkg, &peerparty, &args.join(" "), &vars)?;
    remember(exe);
    eprintln!("peerparty: started {} (package {}, helper pid {helper}), logs in {}", pkg.aumid, pkg.full_name, log_dir.display());
    let (process, pid) = packaged::wait_for_game(pkg, helper).ok_or("the game's process never appeared")?;
    if let Some(pipe) = pipe {
        host::run(process, pid, pipe, &pkg.game, prefs, true);
    }
    unsafe {
        let mut code = 0u32;
        let known = WaitForSingleObject(process, INFINITE) != WAIT_FAILED && GetExitCodeProcess(process, &mut code) != 0;
        let ran = run_time(process);
        CloseHandle(process);
        let line = exit_line(known.then_some(code), ran);
        eprintln!("peerparty: {line}");
        log_exit(log_dir, &pkg.game, pid, &line);
        Ok(if known { code } else { 1 })
    }
}

/// `--set key=value`: one setting for this run (`auto_accept=on`, `overlay=off`,
/// `invite_buttons=guide`, `voice=off`, `steam_saves=off`, `public_voice=off`, `internet=off`,
/// `language=de`, `effects=off`, `mode.photon=official`, ...).
fn apply_set(p: &mut ui::store::Prefs, k: &str, v: &str) {
    let on = matches!(v.to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes" | "auto");
    match k {
        "name" => p.name = v.to_string(),
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

/// How long the game process ran, from its creation and exit times.
unsafe fn run_time(process: windows_sys::Win32::Foundation::HANDLE) -> Option<std::time::Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    let ticks = t(exited).checked_sub(t(created))?;
    Some(std::time::Duration::from_nanos(ticks * 100))
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
fn exit_line(code: Option<u32>, ran: Option<std::time::Duration>) -> String {
    let ran = ran.map(|d| format!(" after {:.1} s", d.as_secs_f64())).unwrap_or_default();
    match code {
        None => format!("game exited{ran}; its exit code couldn't be read"),
        Some(c) => {
            let what = exit_meaning(c).map(|m| format!(", {m}")).unwrap_or_default();
            format!("game exited{ran} with exit code {c} ({c:#010X}){what}")
        }
    }
}

/// Appends `line` to the hook's log for that process (`<exe>-<date>-<time>-<pid>.log` in
/// the log folder). If the hook never opened one (the game died before it loaded),
/// a log of that name is started for the line alone.
fn log_exit(log_dir: &Path, exe: &Path, pid: u32, line: &str) {
    use std::io::Write;
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let path = find_log(log_dir, pid).unwrap_or_else(|| {
        let t = unsafe {
            let mut t = mem::zeroed();
            windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
            t
        };
        log_dir.join(format!(
            "{stem}-{:04}{:02}{:02}-{:02}{:02}{:02}-{pid}.log",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        ))
    });
    let clock = unsafe {
        let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
    };
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

/// Notes the game in `%LOCALAPPDATA%\peerparty\recent.tsv` (time, exe), the launcher
/// window's Recent list. The file keeps the last 200 launches.
fn remember(exe: &Path) {
    let Some(dir) = env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("peerparty")) else {
        return;
    };
    let file = dir.join("recent.tsv");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let old = fs::read_to_string(&file).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().filter(|l| !l.is_empty()).collect();
    let line = format!("{now}\t{}", exe.display());
    lines.push(&line);
    let keep = &lines[lines.len().saturating_sub(200)..];
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(&file, keep.join("\n") + "\n");
}

unsafe fn start_injected(
    exe: &Path,
    cmdline: &OsStr,
    cwd: &Path,
    dll: &Path,
    x86: bool,
    tree: Option<&tree::Tree>,
) -> Result<PROCESS_INFORMATION, String> {
    unsafe {
        let app = wide(exe.as_os_str());
        let mut cmd = wide(cmdline);
        let dir = wide(cwd.as_os_str());
        let mut si: STARTUPINFOW = mem::zeroed();
        si.cb = mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = mem::zeroed();
        if CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_SUSPENDED,
            ptr::null(),
            dir.as_ptr(),
            &si,
            &mut pi,
        ) == 0
        {
            return Err(format!(
                "CreateProcess failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let fail = |what: String| {
            TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            what
        };

        let injected = if x86 {
            wow64::inject(&pi, dll)
        } else {
            inject_x64(&pi, dll)
        };
        if let Err(what) = injected {
            return Err(fail(what));
        }
        // Before it runs, so every program it starts is in the job too.
        if tree.is_some_and(|t| !t.add(pi.hProcess)) {
            eprintln!("peerparty: can't watch the programs the game starts ({}); waiting for this one only", std::io::Error::last_os_error());
        }
        if ResumeThread(pi.hThread) == u32::MAX {
            return Err(fail(format!(
                "ResumeThread failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(pi)
    }
}

/// Copies `bytes` into a new read-write region of the game process.
pub(crate) unsafe fn write_remote(pi: &PROCESS_INFORMATION, bytes: &[u8]) -> Result<usize, String> {
    unsafe {
        let remote = VirtualAllocEx(
            pi.hProcess,
            ptr::null(),
            bytes.len(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            return Err(os_error("VirtualAllocEx"));
        }
        if WriteProcessMemory(
            pi.hProcess,
            remote,
            bytes.as_ptr() as *const c_void,
            bytes.len(),
            ptr::null_mut(),
        ) == 0
        {
            return Err(os_error("WriteProcessMemory"));
        }
        Ok(remote as usize)
    }
}

pub(crate) fn os_error(what: &str) -> String {
    format!("{what} failed: {}", std::io::Error::last_os_error())
}

pub(crate) fn wide_bytes(s: &OsStr) -> Vec<u8> {
    wide(s).iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// Early bird: the APC runs on the main thread after the loader maps static imports.
/// kernel32 sits at the same address in every x64 process for this boot, so our own
/// `LoadLibraryW` is valid in the game.
pub(crate) unsafe fn inject_x64(pi: &PROCESS_INFORMATION, dll: &Path) -> Result<(), String> {
    unsafe {
        let remote = write_remote(pi, &wide_bytes(dll.as_os_str()))?;
        let k32 = GetModuleHandleW(wide(OsStr::new("kernel32.dll")).as_ptr());
        let load_library = GetProcAddress(k32, c"LoadLibraryW".as_ptr() as *const u8)
            .ok_or_else(|| os_error("GetProcAddress(LoadLibraryW)"))?;
        let apc: unsafe extern "system" fn(usize) = mem::transmute(load_library);
        if QueueUserAPC(Some(apc), pi.hThread, remote) == 0 {
            return Err(os_error("QueueUserAPC"));
        }
        Ok(())
    }
}

/// IMAGE_FILE_HEADER.Machine of a PE file.
fn pe_machine(exe: &Path) -> Result<u16, String> {
    let buf = fs::read(exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let rd16 = |o: usize| buf.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let rd32 = |o: usize| {
        buf.get(o..o + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let nt = rd32(0x3c).ok_or("not a PE file")? as usize;
    if rd16(0) != Some(0x5a4d) || rd32(nt) != Some(0x4550) {
        return Err(format!("{} is not a PE file", exe.display()));
    }
    rd16(nt + 4).ok_or_else(|| "truncated PE header".into())
}

/// `fs::canonicalize` returns `\\?\C:\…`; games don't expect that in argv[0].
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// Quotes one argument by the MSVC CRT rules (CommandLineToArgvW).
fn quote(arg: &OsStr) -> OsString {
    let s = arg.to_string_lossy();
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return arg.to_os_string();
    }
    crt_quote(&s).into()
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

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launcher's names for the invite events are the hook's.
    #[test]
    fn invite_event_names_match_the_hook() {
        let hook = include_str!("../../hook/src/invite.rs");
        for (name, value) in [("EVENT_NAME", INVITE_EVENT), ("EVENT_LAST", INVITE_LAST_EVENT), ("EVENT_ACK", INVITE_ACK_EVENT)] {
            let line = format!("pub const {name}: &str = \"{}\";", value.replace('\\', "\\\\"));
            assert!(hook.contains(&line), "hook/src/invite.rs has no `{line}`");
        }
    }

    /// An invite reports whether a game took it: no game is a "no" (after the wait), an
    /// answer left from an earlier invite doesn't count, a game's answer is a "yes", and
    /// the event is cleared after either (a game started later isn't triggered by it).
    /// Names of its own, so a game running on this PC isn't asked.
    #[test]
    fn an_invite_says_whether_a_game_took_it() {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
        let id = std::process::id();
        let (event, ack) = (format!("Local\\peerparty.test.invite.{id}"), format!("Local\\peerparty.test.invite.ack.{id}"));
        let open = |name: &str| {
            let name = wide(OsStr::new(name));
            let h = unsafe { CreateEventW(ptr::null(), 1, 0, name.as_ptr()) };
            assert!(!h.is_null());
            h as usize
        };
        let (ev, ak) = (open(&event), open(&ack));
        // Nobody listening, and an old answer still set.
        unsafe { SetEvent(ak as _) };
        let start = std::time::Instant::now();
        assert_eq!(signal_named(&event, &ack, 300), Ok(false));
        assert!(start.elapsed() >= std::time::Duration::from_millis(250));
        assert_eq!(unsafe { WaitForSingleObject(ev as _, 0) }, WAIT_TIMEOUT, "the event was left set");
        // A game that answers, the way hook/src/invite.rs does.
        let game = std::thread::spawn(move || {
            let woke = unsafe { WaitForSingleObject(ev as _, 5000) } == WAIT_OBJECT_0;
            if woke {
                unsafe { SetEvent(ak as _) };
            }
            woke
        });
        assert_eq!(signal_named(&event, &ack, 5000), Ok(true));
        assert!(game.join().unwrap());
        assert_eq!(unsafe { WaitForSingleObject(ev as _, 0) }, WAIT_TIMEOUT, "the event was left set");
        assert_eq!(unsafe { WaitForSingleObject(ak as _, 0) }, WAIT_TIMEOUT, "the answer was left set");
        unsafe {
            CloseHandle(ev as _);
            CloseHandle(ak as _);
        }
    }

    /// `--set internet=off|on` flips the internet switch for one run; it's on by default.
    #[test]
    fn set_switches_internet_play() {
        let mut p = ui::store::Prefs::default();
        assert!(p.internet);
        apply_set(&mut p, "internet", "off");
        assert!(!p.internet);
        apply_set(&mut p, "internet", "on");
        assert!(p.internet);
        // Saved settings without the field read as on.
        let old: ui::store::Prefs = serde_json::from_str(r#"{"room": "x"}"#).unwrap();
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
        log_exit(&dir, Path::new(r"C:\Games\game.exe"), 4242, "game exited with exit code 0");
        let text = fs::read_to_string(&hook_log).unwrap();
        assert!(text.contains("hook loaded"), "{text}");
        assert!(text.contains("] game exited with exit code 0\r\n"), "{text}");
        assert_eq!(fs::read_to_string(dir.join("game-20261031-101010-14242.log")).unwrap(), "");
        // No hook log for that process: one is started for the line.
        log_exit(&dir, Path::new(r"C:\Games\game.exe"), 99, "game exited with exit code 1");
        let started = find_log(&dir, 99).unwrap();
        assert!(started.file_name().unwrap().to_string_lossy().starts_with("game-"));
        let _ = fs::remove_dir_all(&dir);
    }
}
