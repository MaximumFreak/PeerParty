//! Command-line parsing and dispatch for PeerParty.
//!
//! No arguments (or `--updated`) opens the launcher window (`ui::run`). Every other
//! invocation parses into a typed `Command` and runs without mutating this process's
//! environment (`launch.rs`).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::launch::{self, Launch, LaunchError, SplitScreen};
use crate::{backend, firewall, host};
use peerparty_protocol::session::LayoutPreset;

pub const USAGE: &str = "usage: peerparty [--room CODE] [--public] [--set key=value] [--log-dir DIR] [--multi-instance] [--copy-saves] [--player N] [--split-screen [--instances N] [--layout horizontal|vertical|top2|bottom2|grid|slots:...] [--inputs auto|kbm|pad,...] [--no-kbm] [--stagger smart|0..60] [--no-sync] [--names A,B,...] [--monitor N]] <game.exe> [args...]\n         plays the game through peerparty: the local network, plus the internet with --room or --public\n         a simple .bat or .cmd that starts the game works too: peerparty starts the program it runs\n         --set changes one setting for this run: name=Alex, voice=off, internet=off, overlay=off, auto_accept=on, ... (docs/usage.md)\n         --player N plays alone as player N (1-4): that player's saves, name and identity\n       peerparty --saves <game.exe>   (each player's saves for the game, and their backups)\n       peerparty --copy-save FROM TO <game.exe> | --swap-saves A B <game.exe> | --reset-save N <game.exe> | --restore-save N BACKUP <game.exe>   (players 1-4; the one replaced is backed up first)\n       peerparty --invite   (invites every other player to the game in front)\n       peerparty --new-room   (makes a room code for internet play)\n       peerparty --net-test   (checks this PC's internet path: STUN, NAT type, the DHT)\n       peerparty --pads   (lists the controllers the overlay reads, and their buttons)\n       peerparty --firewall <game.exe>   (whether Windows Firewall lets other players connect to it)\n       peerparty --fix-firewall <game.exe> [--public]   (as administrator: lets them, on private and domain networks, and public ones with --public)\n       peerparty --remove-firewall   (as administrator: removes every firewall rule peerparty added)\n       peerparty --blocked | --block FINGERPRINT | --unblock FINGERPRINT|all   (public players you blocked)\n       peerparty --friends | --friend FINGERPRINT | --unfriend FINGERPRINT|all   (friends for PEERPARTY_PUBLIC_FRIENDS_ONLY=1)";

/// A parsed command-line invocation.
pub enum Command {
    Help,
    NewRoom,
    NetTest,
    BlockList { what: String, arg: Option<String> },
    Invite,
    Firewall(PathBuf),
    FixFirewall { exe: PathBuf, public: bool, report: Option<String> },
    RemoveFirewall,
    Pads,
    Attach(Vec<String>),
    Saves { op: SaveOp, game: PathBuf },
    Launch { launch: Launch, why_lines: bool },
}

/// A save manager command (saves.rs); players from 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOp {
    List,
    Copy(u32, u32),
    Swap(u32, u32),
    Reset(u32),
    Restore(u32, String),
}

/// A player number on the command line (1 to 4), from 0.
fn player_arg(a: Option<OsString>) -> Result<u32, String> {
    let raw = a.ok_or(USAGE)?.to_string_lossy().into_owned();
    raw.trim().parse::<u32>().ok().filter(|n| (1..=peerparty_protocol::saves::MAX_PLAYERS).contains(n)).map(|n| n - 1).ok_or_else(|| format!("invalid player {raw:?} (expected 1 to {})", peerparty_protocol::saves::MAX_PLAYERS))
}

/// Parses command-line arguments (without `argv[0]`).
pub fn parse(raw: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = raw.into_iter().peekable();
    let mut launch = Launch::default();
    let mut why_lines = false;
    let mut layout_given = false;
    while let Some(a) = args.peek() {
        match a.to_str() {
            Some("--set") => {
                args.next();
                let kv = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                let (k, v) = kv.split_once('=').ok_or(USAGE)?;
                launch.sets.push((k.trim().to_string(), v.trim().to_string()));
            }
            Some("--log-dir") => {
                args.next();
                launch.log_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?));
            }
            Some("--multi-instance") => {
                args.next();
                launch.multi_instance = true;
            }
            Some("--copy-saves") => {
                args.next();
                launch.copy_saves = true;
            }
            Some("--player") => {
                args.next();
                launch.player = Some(player_arg(args.next())?);
            }
            Some(w @ ("--saves" | "--copy-save" | "--swap-saves" | "--reset-save" | "--restore-save")) => {
                let w = w.to_string();
                args.next();
                let op = match w.as_str() {
                    "--saves" => SaveOp::List,
                    "--copy-save" => SaveOp::Copy(player_arg(args.next())?, player_arg(args.next())?),
                    "--swap-saves" => SaveOp::Swap(player_arg(args.next())?, player_arg(args.next())?),
                    "--reset-save" => SaveOp::Reset(player_arg(args.next())?),
                    _ => SaveOp::Restore(player_arg(args.next())?, args.next().ok_or(USAGE)?.to_string_lossy().into_owned()),
                };
                let game = PathBuf::from(args.next().ok_or(USAGE)?);
                return Ok(Command::Saves { op, game });
            }
            Some("--hosted") => {
                args.next();
                launch.hosted = true;
            }
            Some("--split-screen") => {
                args.next();
                launch.split.get_or_insert_with(SplitScreen::default);
            }
            Some("--instances") => {
                args.next();
                let raw_n = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                let n: u32 = raw_n.parse().map_err(|_| format!("invalid --instances count {raw_n:?} (expected 1..={})", SplitScreen::MAX_INSTANCES))?;
                if n == 0 || n > SplitScreen::MAX_INSTANCES {
                    return Err(format!("--instances must be between 1 and {}", SplitScreen::MAX_INSTANCES));
                }
                launch.split.get_or_insert_with(SplitScreen::default).instances = n;
            }
            Some("--layout") => {
                args.next();
                let name = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                let layout = SplitScreen::layout_named(&name).ok_or_else(|| format!("unknown layout {name:?} (expected horizontal, vertical, top2, bottom2, or grid)"))?;
                launch.split.get_or_insert_with(SplitScreen::default).layout = layout;
                layout_given = true;
            }
            Some("--monitor") => {
                args.next();
                let raw_n = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                let n: u32 = raw_n.parse().ok().filter(|n| (1..=16).contains(n)).ok_or_else(|| format!("invalid --monitor {raw_n:?} (expected 1 for the main monitor, or 2.. for the others, left to right)"))?;
                launch.split.get_or_insert_with(SplitScreen::default).monitor = Some(n);
            }
            Some("--inputs") => {
                args.next();
                let list = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                let inputs = SplitScreen::inputs_named(&list).ok_or_else(|| format!("bad --inputs {list:?} (expected up to {} of auto, kbm or pad, comma-separated)", SplitScreen::MAX_INSTANCES))?;
                launch.split.get_or_insert_with(SplitScreen::default).inputs = inputs;
            }
            Some("--no-kbm") => {
                args.next();
                launch.split.get_or_insert_with(SplitScreen::default).no_kbm = true;
            }
            Some("--stagger") => {
                args.next();
                let raw = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                let mode = launch::StaggerMode::parse(&raw).ok_or_else(|| format!("bad --stagger {raw:?} (expected smart, or 0..=60 seconds)"))?;
                launch.split.get_or_insert_with(SplitScreen::default).stagger = mode;
            }
            Some("--no-sync") => {
                args.next();
                launch.split.get_or_insert_with(SplitScreen::default).no_sync = true;
            }
            Some("--names") => {
                args.next();
                let list = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                let names = SplitScreen::names_listed(&list).ok_or_else(|| format!("bad --names {list:?} (expected up to {} names, comma-separated)", SplitScreen::MAX_INSTANCES))?;
                launch.split.get_or_insert_with(SplitScreen::default).names = names;
            }
            Some("--why") => {
                args.next();
                why_lines = true;
            }
            Some("--room") => {
                args.next();
                let code = args.next().ok_or(USAGE)?.to_string_lossy().into_owned();
                launch.sets.push(("room".into(), code));
            }
            Some("--new-room") => return Ok(Command::NewRoom),
            Some("--net-test") => return Ok(Command::NetTest),
            Some("--public") => {
                args.next();
                launch.sets.push(("public".into(), "on".into()));
            }
            Some(w @ ("--blocked" | "--block" | "--unblock" | "--friends" | "--friend" | "--unfriend")) => {
                let what = w.to_string();
                args.next();
                let arg = if what == "--blocked" || what == "--friends" {
                    None
                } else {
                    Some(args.next().ok_or(USAGE)?.to_string_lossy().into_owned())
                };
                return Ok(Command::BlockList { what, arg });
            }
            Some("--invite") => return Ok(Command::Invite),
            Some("--firewall") => {
                args.next();
                let exe = PathBuf::from(args.next().ok_or(USAGE)?);
                return Ok(Command::Firewall(exe));
            }
            Some("--fix-firewall") => {
                args.next();
                let exe = PathBuf::from(args.next().ok_or(USAGE)?);
                let rest: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
                let public = rest.iter().any(|a| a == "--public");
                let report = rest.iter().position(|a| a == "--report").and_then(|i| rest.get(i + 1)).cloned();
                return Ok(Command::FixFirewall { exe, public, report });
            }
            Some("--remove-firewall") => return Ok(Command::RemoveFirewall),
            Some("--pads") => return Ok(Command::Pads),
            Some("--attach") => {
                let rest: Vec<String> = args.skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
                return Ok(Command::Attach(rest));
            }
            Some("-h" | "--help" | "/?") => return Ok(Command::Help),
            _ => break,
        }
    }
    // Three players get 2 + 1 (two on top, one wide below) and four get a 2x2 grid unless
    // a layout was asked for, matching the launcher's game page.
    if let Some(split) = launch.split.as_mut().filter(|s| !layout_given && s.instances >= 3) {
        split.layout = if split.instances == 3 { LayoutPreset::TopTwo } else { LayoutPreset::Grid2x2 };
    }
    if launch.player.is_some() && launch.split.is_some() {
        return Err("--player is for one copy; a split-screen session gives each copy its player".into());
    }
    launch.first = PathBuf::from(args.next().ok_or(USAGE)?);
    launch.args = args.collect();
    Ok(Command::Launch { launch, why_lines })
}

/// Executes a parsed command and exits with the game's full exit code (or 1 on error).
pub fn execute(args: Vec<OsString>) -> ExitCode {
    let cmd = match parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("peerparty: {e}");
            return ExitCode::from(1);
        }
    };
    match cmd {
        Command::Launch { launch, why_lines } => match launch::start(launch) {
            Ok(exited) => {
                if why_lines && let Some((reason, value)) = exited.why() {
                    eprintln!("{}", launch::why_line(reason, &value));
                }
                std::process::exit(exited.code() as i32);
            }
            Err(e) => {
                eprintln!("peerparty: {e}");
                if why_lines && let Some((reason, value)) = e.why() {
                    eprintln!("{}", launch::why_line(reason, &value));
                }
                ExitCode::from(1)
            }
        },
        other => match run_utility(other) {
            Ok(code) => std::process::exit(code as i32),
            Err(e) => {
                eprintln!("peerparty: {e}");
                ExitCode::from(1)
            }
        },
    }
}

fn run_utility(cmd: Command) -> Result<u32, LaunchError> {
    match cmd {
        Command::Help => {
            println!("{USAGE}");
            Ok(0)
        }
        Command::NewRoom => {
            let code = peerparty_core::inet::RoomCode::generate();
            println!("{code}");
            eprintln!("peerparty: a new room code. Give it to the friends you play with (privately: anyone with it can join), and start the game with --room {code}, or put it in the launcher's Settings.");
            Ok(0)
        }
        Command::NetTest => net_test().map_err(LaunchError::Other),
        Command::BlockList { what, arg } => block_list(&what, arg).map_err(LaunchError::Other),
        Command::Invite => {
            if backend::signal_invite(false).map_err(LaunchError::Other)? {
                eprintln!("peerparty: the game in front is inviting every player on your network");
                Ok(0)
            } else {
                Err(LaunchError::Other("no game took the invite: it goes to a game started through peerparty whose window is in front".into()))
            }
        }
        Command::Firewall(exe) => {
            let exe = std::path::absolute(&exe).unwrap_or(exe);
            let v = firewall::Checker::default().verdict(&exe);
            match (v.warning(), v.note()) {
                (Some(w), _) => eprintln!("peerparty: {w} ({} rule(s): peerparty --fix-firewall to fix)", v.blocks.len()),
                (None, Some(n)) => eprintln!("peerparty: {n}"),
                (None, None) => eprintln!("peerparty: Windows Firewall lets other players connect to {}", v.exe),
            }
            Ok(if v.blocked() { 1 } else { 0 })
        }
        Command::FixFirewall { exe, public, report } => {
            let exe = std::path::absolute(&exe).unwrap_or(exe);
            match firewall::fix(&exe, public) {
                Ok(msg) => {
                    eprintln!("peerparty: {msg}");
                    Ok(0)
                }
                Err(e) => {
                    if let Some(pipe) = report {
                        firewall::report(&pipe, &e);
                    }
                    Err(LaunchError::Other(format!("the firewall change failed: {e}")))
                }
            }
        }
        Command::RemoveFirewall => match firewall::remove_all() {
            Ok(msg) => {
                eprintln!("peerparty: {msg}");
                Ok(0)
            }
            Err(e) => Err(LaunchError::Other(format!("removing the firewall rules failed (run it as administrator): {e}"))),
        },
        Command::Pads => {
            host::print_pads(30);
            Ok(0)
        }
        Command::Attach(rest) => backend::attach(&rest),
        Command::Saves { op, game } => save_command(op, &game).map_err(LaunchError::Other),
        Command::Launch { .. } => unreachable!(),
    }
}

/// `--saves` and the save manager's commands, for the game `game` names as the launcher
/// knows it (its folder found as a start would).
fn save_command(op: SaveOp, game: &std::path::Path) -> Result<u32, String> {
    let exe = std::path::absolute(game).unwrap_or_else(|_| game.to_path_buf());
    let settings = crate::ui::store::Settings::load();
    let root = crate::gameroot::find(&exe, &settings.folders).map(|(r, _)| r);
    let saves = crate::saves::GameSaves::for_game(&crate::ui::store::key_of(&exe), Some(&exe), root.as_deref());
    let say = |what: &str, r: crate::saves::Report| {
        eprintln!(
            "peerparty: {what} ({} files, {} registry keys){}{}",
            r.files,
            r.keys,
            r.backup.map(|b| format!("; what was replaced is backup {b}")).unwrap_or_default(),
            if r.notes.is_empty() { String::new() } else { format!("; {}", r.notes.join("; ")) }
        )
    };
    match op {
        SaveOp::List => {
            println!("{} ({})", exe.display(), saves.id);
            for p in saves.inspect() {
                let state = if p.running { ", playing now" } else { "" };
                let kinds: Vec<&str> = p.kinds.iter().map(|k| k.label()).collect();
                if p.has_saves() {
                    println!("player {}: {} files, {} KB [{}]{state}", p.slot + 1, p.files, p.bytes.div_ceil(1024), kinds.join(", "));
                } else {
                    println!("player {}: no saves yet{state}", p.slot + 1);
                }
                if let Some(f) = &p.folder {
                    println!("  {}", f.display());
                }
                for b in &p.backups {
                    println!("  backup {}", b.id);
                }
            }
            if saves.discovered().is_empty() {
                eprintln!("peerparty: where player 1's saves are isn't known yet: start the game through peerparty with 2 players (or --player 2) once");
            }
        }
        SaveOp::Copy(from, to) => say(&format!("player {}'s saves copied to player {}", from + 1, to + 1), saves.copy(from, to).map_err(|e| e.to_string())?),
        SaveOp::Swap(a, b) => say(&format!("players {} and {} swapped saves", a + 1, b + 1), saves.swap(a, b).map_err(|e| e.to_string())?),
        SaveOp::Reset(n) => say(&format!("player {} starts fresh", n + 1), saves.reset(n).map_err(|e| e.to_string())?),
        SaveOp::Restore(n, id) => say(&format!("player {}'s saves are backup {id} again", n + 1), saves.restore(n, &id).map_err(|e| e.to_string())?),
    }
    Ok(0)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::SlotInput;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_launch_flags_and_split_screen() {
        let cmd = parse(args(&[
            "--room",
            "PP1-ABC",
            "--public",
            "--set",
            "voice=off",
            "--split-screen",
            "--instances",
            "4",
            "--layout",
            "grid",
            "--inputs",
            "kbm,auto",
            "--names",
            ",Sam, Robin ",
            "--why",
            "game.exe",
            "-windowed",
        ]))
        .unwrap();
        let Command::Launch { launch, why_lines } = cmd else { panic!("expected Launch") };
        assert!(why_lines);
        assert_eq!(launch.first, PathBuf::from("game.exe"));
        assert_eq!(launch.args, args(&["-windowed"]));
        assert_eq!(launch.split, Some(SplitScreen { instances: 4, layout: LayoutPreset::Grid2x2, inputs: vec![SlotInput::KeyboardMouse, SlotInput::Auto], no_kbm: false, names: vec!["".into(), "Sam".into(), "Robin".into()], monitor: None, stagger: launch::StaggerMode::Smart, no_sync: false }));
        let split = launch.split.as_ref().unwrap();
        assert_eq!((split.name(0), split.name(1), split.name(3)), (None, Some("Sam".into()), None));
        assert_eq!(
            launch.sets,
            vec![("room".into(), "PP1-ABC".into()), ("public".into(), "on".into()), ("voice".into(), "off".into())]
        );
    }

    /// Three players default to 2 + 1 and four to a grid unless a layout is given; two stay side by side.
    #[test]
    fn three_or_four_players_default_to_a_grid() {
        let split = |v: &[&str]| match parse(args(v)) {
            Ok(Command::Launch { launch, .. }) => launch.split.unwrap(),
            _ => panic!("expected a split-screen launch"),
        };
        assert_eq!(split(&["--split-screen", "game.exe"]).layout, LayoutPreset::Horizontal);
        assert_eq!(split(&["--split-screen", "--instances", "3", "game.exe"]).layout, LayoutPreset::TopTwo);
        assert_eq!(split(&["--instances", "4", "--split-screen", "game.exe"]).layout, LayoutPreset::Grid2x2);
        assert_eq!(split(&["--layout", "bottom2", "--instances", "3", "game.exe"]).layout, LayoutPreset::BottomTwo, "asked for");
        assert_eq!(split(&["--layout", "vertical", "--instances", "4", "game.exe"]).layout, LayoutPreset::Vertical, "asked for");
        assert!(split(&["--split-screen", "--no-kbm", "game.exe"]).no_kbm);
        assert!(split(&["--split-screen", "--no-sync", "game.exe"]).no_sync);
        assert!(!split(&["--split-screen", "game.exe"]).no_sync);
        assert_eq!(split(&["--split-screen", "--stagger", "0", "game.exe"]).stagger, launch::StaggerMode::Fixed(0));
        assert_eq!(split(&["--split-screen", "--stagger", "10", "game.exe"]).stagger, launch::StaggerMode::Fixed(10));
        assert_eq!(split(&["--split-screen", "--stagger", "smart", "game.exe"]).stagger, launch::StaggerMode::Smart);
        let slots_arg = LayoutPreset::format_slots(&LayoutPreset::pip_slots(2));
        assert_eq!(split(&["--split-screen", "--layout", &slots_arg, "game.exe"]).layout, LayoutPreset::Slots(LayoutPreset::pip_slots(2)));
        let s = split(&["--split-screen", "--monitor", "2", "game.exe"]);
        assert_eq!(s.monitor, Some(2));
    }

    #[test]
    fn rejects_invalid_split_screen_options() {
        assert!(parse(args(&["--instances", "0", "game.exe"])).is_err());
        assert!(parse(args(&["--instances", "5", "game.exe"])).is_err());
        assert!(parse(args(&["--layout", "spiral", "game.exe"])).is_err());
        assert!(parse(args(&["--stagger", "99", "game.exe"])).is_err());
        assert!(parse(args(&["--stagger", "later", "game.exe"])).is_err());
        assert!(parse(args(&["--inputs", "kbm,joystick", "game.exe"])).is_err());
        assert!(parse(args(&["--names", "a,b,c,d,e", "game.exe"])).is_err());
        assert!(parse(args(&["--monitor", "0", "game.exe"])).is_err());
        assert!(parse(args(&["--monitor", "left", "game.exe"])).is_err());
        assert!(parse(args(&["--set", "broken", "game.exe"])).is_err());
        assert!(parse(args(&[])).is_err());
    }

    #[test]
    fn parses_utility_commands() {
        assert!(matches!(parse(args(&["--help"])), Ok(Command::Help)));
        assert!(matches!(parse(args(&["--new-room"])), Ok(Command::NewRoom)));
        assert!(matches!(parse(args(&["--invite"])), Ok(Command::Invite)));
        assert!(matches!(parse(args(&["--pads"])), Ok(Command::Pads)));
        assert!(matches!(parse(args(&["--blocked"])), Ok(Command::BlockList { ref what, arg: None }) if what == "--blocked"));
        assert!(matches!(
            parse(args(&["--fix-firewall", "game.exe", "--public", "--report", "pipe1"])),
            Ok(Command::FixFirewall { public: true, report: Some(ref r), .. }) if r == "pipe1"
        ));
    }
}
