//! `peerparty %command%` in a game's Steam launch options on Linux (SteamOS among
//! them): Steam's command for the game, run with PeerParty around it.
//!
//! The command is Steam's whole chain for a Proton game (its reaper, the runtime's
//! entry point, `proton waitforexitandrun <game.exe> <args>`) or a plain `<game.exe>`,
//! which runs under Wine. In it, the game's program is the first argument that is a
//! Windows program (`game_exe_in`). PeerParty:
//!
//! 1. Reads the player's settings for that game and puts them in the environment, which
//!    Proton hands to the game (main.rs, as on Windows). The paths the hook is given are
//!    Wine's (`linux_to_wine_path`: `/x/y` is `Z:\x\y`), PeerParty's folder among them
//!    when it is portable (portable.rs); else the hook uses the Wine prefix's own.
//! 2. Listens on loopback TCP for the hook (`Pipe::create_tcp`, `PEERPARTY_IPC`): a game
//!    under Wine can't open this side's named pipes, but it can reach 127.0.0.1. The
//!    overlay's frames go in a file in `$XDG_RUNTIME_DIR/peerparty` (host/frame.rs).
//! 3. Has Windows' peerparty.exe start the game inside the same Proton (`inject_via`):
//!    the game's program in the command becomes `peerparty.exe --hosted Z:\...\game.exe`,
//!    which starts the game with the hook as it does on Windows, and leaves the overlay
//!    to this process (`--hosted`). peerparty.exe is looked for beside this program (or
//!    `PEERPARTY_WINDOWS_EXE`); without it the game runs as it is, without PeerParty.
//! 4. Hosts the overlay while the game runs (controllers from evdev), answers
//!    `--invite` and Close all (control.rs), and returns the game's exit code.

use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Where Windows' peerparty.exe is, when it isn't beside this program.
pub const WINDOWS_EXE_VAR: &str = "PEERPARTY_WINDOWS_EXE";
/// The program a plain Windows program runs under, when the command isn't Proton's.
pub const WINE_VAR: &str = "PEERPARTY_WINE";

/// A path as Windows programs under Wine name it: an absolute Unix path is on drive Z:
/// (`/run/user/1000/f` is `Z:\run\user\1000\f`), which Wine maps to `/`. A relative path
/// keeps its place, with Windows' separators.
pub fn linux_to_wine_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.starts_with('/') && !s.starts_with("//") { format!("Z:{}", s.replace('/', "\\")) } else { s.replace('/', "\\") }
}

/// Whether `name` is a Windows program's (by its name: `.exe`, any case).
fn is_exe_name(name: &OsStr) -> bool {
    Path::new(name).extension().is_some_and(|x| x.eq_ignore_ascii_case("exe"))
}

/// The bytes a Windows program starts with (`MZ`).
pub fn is_windows_program(head: &[u8]) -> bool {
    head.starts_with(b"MZ")
}

/// Which argument of Steam's `command` is the game: the first `.exe` that is a file
/// (`exists`). Proton's tools come before it; the game's own arguments after.
pub fn game_exe_in(command: &[OsString], exists: impl Fn(&Path) -> bool) -> Option<usize> {
    command.iter().position(|a| is_exe_name(a) && exists(Path::new(a)))
}

/// The command with Windows' peerparty.exe starting the game at `game`: the game's
/// program becomes `peerparty.exe --hosted <game, as Wine names it>`. A plain Windows
/// program (not inside Proton's command) runs under `wine` (the runner) as well.
pub fn inject_via(command: &[OsString], game: usize, windows_exe: &Path, wine: Option<&OsStr>) -> Vec<OsString> {
    let mut out: Vec<OsString> = Vec::with_capacity(command.len() + 3);
    if game == 0 {
        out.push(wine.unwrap_or(OsStr::new("wine")).to_os_string());
    }
    out.extend(command[..game].iter().cloned());
    out.push(windows_exe.as_os_str().to_os_string());
    out.push("--hosted".into());
    out.push(linux_to_wine_path(Path::new(&command[game])).into());
    out.extend(command[game + 1..].iter().cloned());
    out
}

/// The environment's paths the hook reads, as Wine names them: (variable, value) for
/// each of `vars` that is set to an absolute Unix path.
pub fn wine_paths(vars: &[&str], get: impl Fn(&str) -> Option<OsString>) -> Vec<(String, String)> {
    vars.iter()
        .filter_map(|k| {
            let v = get(k)?;
            let p = Path::new(&v);
            p.is_absolute().then(|| (k.to_string(), linux_to_wine_path(p)))
        })
        .filter(|(_, v)| v.starts_with("Z:"))
        .collect()
}

#[cfg(unix)]
pub use run::{attach, resolve, start, start_session};

#[cfg(unix)]
mod run {
    use std::env;
    use std::ffi::OsString;
    use std::io::Read;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use peerparty_protocol::env as penv;
    use peerparty_protocol::ipc::HostMsg;
    use peerparty_protocol::session::{InstanceId, InstanceSpec};

    use super::{WINDOWS_EXE_VAR, WINE_VAR, game_exe_in, inject_via, is_windows_program, linux_to_wine_path, wine_paths};
    use crate::backend::linux::control;
    use crate::host::{self, SessionLaunch};
    use crate::launch::{self, ChildEnv, Exited, LaunchError, SplitScreen};
    use crate::ui::store::Prefs;

    /// The game in Steam's command.
    pub struct Target {
        /// The game's program (its settings, Recent and the overlay go by it).
        pub listed: PathBuf,
        pub exe: PathBuf,
        /// Steam's whole command.
        command: Vec<OsString>,
        /// Which of its arguments is the game (None: it runs as it is).
        game: Option<usize>,
    }

    impl Target {
        /// Packaged (Microsoft Store) games are Windows'.
        pub fn packaged(&self) -> bool {
            false
        }
    }

    fn windows_program(p: &Path) -> bool {
        let mut head = [0u8; 2];
        std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut head)).is_ok() && is_windows_program(&head)
    }

    /// `first` and `args`: Steam's command, or a Windows program and its arguments.
    pub fn resolve(first: PathBuf, args: Vec<OsString>, _env: &mut ChildEnv) -> Result<Target, LaunchError> {
        let command: Vec<OsString> = std::iter::once(first.into_os_string()).chain(args).collect();
        let game = game_exe_in(&command, |p| p.is_file() && windows_program(p));
        let named = PathBuf::from(&command[game.unwrap_or(0)]);
        let exe = std::fs::canonicalize(&named).map_err(|e| LaunchError::missing(&named, &e))?;
        Ok(Target { listed: exe.clone(), exe, command, game })
    }

    /// `--attach` is Windows' (packaged games).
    pub fn attach(_rest: &[String]) -> Result<u32, LaunchError> {
        Err(LaunchError::Other("--attach is for Windows' packaged games".into()))
    }

    /// Windows' peerparty.exe, which starts the game with the hook inside Proton.
    fn windows_exe() -> Option<PathBuf> {
        let named = env::var_os(WINDOWS_EXE_VAR).map(PathBuf::from);
        let beside = env::current_exe().ok().map(|me| me.with_file_name("peerparty.exe"));
        named.into_iter().chain(beside).find(|p| p.is_file())
    }

    /// Translates paths in `env` for Wine and chooses the command to spawn.
    fn prepare_command(t: &Target, env: &mut ChildEnv) -> Vec<OsString> {
        // PeerParty's folder too when it was named (portable.rs: `peerparty-data` beside
        // this program, so the hook keeps its saves there as well).
        for (k, v) in wine_paths(&[penv::LOG_DIR, penv::GAME_ROOT, penv::DATA_DIR], |k| env.get(k)) {
            env.set(&k, v);
        }
        // This program's path means nothing inside Wine; peerparty.exe's does (the hook
        // starts it for the game's own programs of the other bitness).
        env.remove(penv::EXE);
        match (t.game, windows_exe()) {
            (Some(game), Some(pp)) => {
                env.set(penv::EXE, linux_to_wine_path(&pp));
                inject_via(&t.command, game, &pp, env.get(WINE_VAR).as_deref())
            }
            (game, _) => {
                let what = if game.is_some() { "no Windows peerparty.exe beside this program (or in PEERPARTY_WINDOWS_EXE)" } else { "no Windows program in the command" };
                eprintln!("peerparty: {what}: the game runs without PeerParty's hook");
                t.command.clone()
            }
        }
    }

    /// Runs the game (`env` is its environment), hosts the overlay on `pipe`, and
    /// returns how it ended.
    pub fn start(t: Target, mut env: ChildEnv, pipe: Option<host::pipe::Pipe>, prefs: Prefs, log_dir: &Path, _root: Option<&Path>) -> Result<Exited, LaunchError> {
        let command = prepare_command(&t, &mut env);
        let mut cmd = std::process::Command::new(&command[0]);
        cmd.args(&command[1..]);
        env.apply(&mut cmd);
        // Its own process group: Close all ends the game and everything it started.
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| LaunchError::os(format!("{}: {e}", Path::new(&command[0]).display()), &e))?;
        let pid = child.id();
        let started = Instant::now();
        launch::remember(&t.listed);
        eprintln!("peerparty: started {} (pid {pid}), logs in {}", t.exe.display(), log_dir.display());

        let ended = Arc::new(AtomicBool::new(false));
        let commands = pipe.as_ref().map(|p| p.commands());
        let closing = ended.clone();
        let _control = control::listen(move || commands.as_ref().is_some_and(|c| c.send(&HostMsg::Invite { to: None })), move || close_group(pid, closing.clone()));
        let done = ended.clone();
        let waiter = std::thread::spawn(move || {
            let status = child.wait();
            done.store(true, Ordering::SeqCst);
            status
        });
        match pipe {
            Some(pipe) if prefs.overlay => host::run(ended.clone(), pid, pipe, &t.exe, prefs, false),
            // The overlay is off: the hook's messages are only read (and the connection
            // kept for `--invite`).
            Some(pipe) => {
                while !ended.load(Ordering::SeqCst) {
                    while pipe.rx.try_recv().is_ok() {}
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            None => {}
        }
        let status = waiter.join().map_err(|_| LaunchError::Other("waiting for the game failed".into()))?.map_err(|e| LaunchError::Other(format!("waiting for the game: {e}")))?;
        // A game ended by a signal reports it as a shell does (128 + the signal).
        let code = status.code().map(|c| c as u32).or_else(|| status.signal().map(|s| 128 + s as u32));
        Ok(launch::report_exit(log_dir, &t.exe, pid, code, Some(started.elapsed())))
    }

    /// Runs `split.instances` copies in one `SessionHost` (`run_session`).
    pub fn start_session(t: Target, mut base_env: ChildEnv, split: &SplitScreen, prefs: Prefs, log_dir: &Path, _root: Option<&Path>) -> Result<Exited, LaunchError> {
        launch::set_log_dir(log_dir);
        let count = split.instances.clamp(1, SplitScreen::MAX_INSTANCES);
        let command = prepare_command(&t, &mut base_env);
        let mut launches = Vec::with_capacity(count as usize);
        let mut waiters = Vec::with_capacity(count as usize);
        let mut pids = Vec::with_capacity(count as usize);

        let bounds = host::initial_session_bounds(split.monitor);
        let compositor = base_env.get(penv::COMPOSITOR).map(|v| v.to_string_lossy().into_owned());
        let panes = split.layout.compute_panes(bounds.width, bounds.height, count as usize);

        for slot in 0..count {
            let mut env = base_env.clone();
            if slot == 0 {
                env.remove(penv::COPY_SAVES);
            }
            let pipe = host::pipe::Pipe::for_game(env.get(penv::IPC).as_deref())
                .ok_or_else(|| LaunchError::Other(format!("couldn't create IPC socket for player {}", slot + 1)));
            let pipe = match pipe {
                Ok(p) => p,
                Err(e) => return Err(abandon(&pids, waiters, e)),
            };
            launch::connect(&mut env, &pipe);
            let mut spec = InstanceSpec::solo(InstanceId(slot), slot);
            spec.focus_spoof = count > 1;
            // The session's window draws the copy's frames itself (host/x11.rs), read back
            // into a file by its hook, unless told to leave the copies' windows be.
            spec.compositor_capture = !compositor.as_deref().is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "dwm" | "off" | "none"));
            spec.input_assignment = split.input(slot).assignment(slot);
            spec.identity_override = split.name(slot);
            env.set(penv::PLAYER_SLOT, slot.to_string());
            if !split.no_vram_budget && count >= 2 {
                env.set_default(penv::VRAM_SHARE, count.to_string());
            }
            if let Some(&pane) = panes.get(slot as usize) {
                let (vw, vh) = peerparty_protocol::session::scale_dimensions(pane.width, pane.height, split.render_scale);
                spec.pane = Some(peerparty_protocol::session::PaneRect {
                    x: bounds.x.saturating_add(pane.x),
                    y: bounds.y.saturating_add(pane.y),
                    width: vw,
                    height: vh,
                });
                spec.virtual_width = Some(vw);
                spec.virtual_height = Some(vh);
                spec.dpi = Some(bounds.dpi);
            }
            env.set(penv::INSTANCE, spec.to_json().to_string());

            let mut cmd = std::process::Command::new(&command[0]);
            cmd.args(&command[1..]);
            env.apply(&mut cmd);
            cmd.process_group(0);
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => return Err(abandon(&pids, waiters, LaunchError::os(format!("{}: {e}", Path::new(&command[0]).display()), &e))),
            };
            let pid = child.id();
            let started = Instant::now();
            let ended = Arc::new(AtomicBool::new(false));
            let done = ended.clone();
            // How long this copy ran, its own clock.
            let waiter = std::thread::spawn(move || {
                let status = child.wait();
                done.store(true, Ordering::SeqCst);
                (status, started.elapsed())
            });
            pids.push((pid, ended.clone()));
            waiters.push((pid, waiter));
            let stop = GroupStop { pid, ended: ended.clone() };
            launches.push(SessionLaunch { process: ended, pid, pipe, spec, input: split.input(slot), stop: Some(Box::new(stop)) });
        }

        launch::remember(&t.listed);
        let _control = control::listen(|| false, host::close_session);
        let look = host::SessionLook {
            layout: split.layout.clone(),
            bounds,
            pixels: host::pixel_frames(compositor.as_deref()),
            no_kbm: split.no_kbm,
            stagger: launch::StaggerMode::Fixed(0),
            sync: !split.no_sync,
            render_scale: split.render_scale,
            upscale: !split.no_upscale,
            fps: split.fps,
            total_count: launches.len(),
        };
        host::run_session(launches, look, None, &t.exe, prefs, false);

        // The first player's exit, unless another player's game ended with an error.
        let mut exits = Vec::with_capacity(waiters.len());
        for (i, (pid, waiter)) in waiters.into_iter().enumerate() {
            let (status, ran) = waiter.join().map_err(|_| LaunchError::Other("waiting for the game failed".into()))?;
            let status = status.map_err(|e| LaunchError::Other(format!("waiting for the game: {e}")))?;
            let code = status.code().map(|c| c as u32).or_else(|| status.signal().map(|s| 128 + s as u32));
            if code != Some(0) {
                eprintln!("peerparty: player {}'s game ended with {}", i + 1, code.map_or("no exit code".into(), |c| format!("exit code {c}")));
            }
            exits.push(launch::report_exit(log_dir, &t.exe, pid, code, Some(ran)));
        }
        let first = exits.first().map_or(Exited(Some(0)), |e| Exited(e.0));
        Ok(if first.0 == Some(0) { exits.into_iter().find(|e| e.0 != Some(0)).unwrap_or(first) } else { first })
    }

    type Waiter = std::thread::JoinHandle<(std::io::Result<std::process::ExitStatus>, Duration)>;

    /// A session that couldn't start every copy: the ones started go (their whole process
    /// groups), and are waited for.
    fn abandon(pids: &[(u32, Arc<AtomicBool>)], waiters: Vec<(u32, Waiter)>, e: LaunchError) -> LaunchError {
        for (pid, _) in pids {
            unsafe { libc::kill(-(*pid as libc::pid_t), libc::SIGKILL) };
        }
        for (_, w) in waiters {
            let _ = w.join();
        }
        e
    }

    /// Closes one copy of a session: its process group (the game and what it started).
    struct GroupStop {
        pid: u32,
        ended: Arc<AtomicBool>,
    }

    impl host::StopCopy for GroupStop {
        fn ask(&self, _known: u64) -> usize {
            if self.ended.load(Ordering::SeqCst) {
                return 0;
            }
            unsafe { libc::kill(-(self.pid as libc::pid_t), libc::SIGTERM) };
            1
        }

        fn force(&self) {
            if !self.ended.load(Ordering::SeqCst) {
                unsafe { libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL) };
            }
        }
    }

    fn close_group(pid: u32, ended: Arc<AtomicBool>) {
        if ended.load(Ordering::SeqCst) {
            return;
        }
        let group = -(pid as libc::pid_t);
        unsafe { libc::kill(group, libc::SIGTERM) };
        std::thread::spawn(move || {
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(100));
                if ended.load(Ordering::SeqCst) {
                    return;
                }
            }
            unsafe { libc::kill(group, libc::SIGKILL) };
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Where Windows' peerparty.exe is in these tests.
    fn test_exe() -> PathBuf {
        PathBuf::from("/opt/peerparty/peerparty.exe")
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn unix_paths_are_on_wines_drive_z() {
        assert_eq!(linux_to_wine_path(Path::new("/run/user/1000/peerparty/frame-1.bin")), r"Z:\run\user\1000\peerparty\frame-1.bin");
        assert_eq!(linux_to_wine_path(Path::new("/home/player/Games/Lantern Keep/Lantern.exe")), r"Z:\home\player\Games\Lantern Keep\Lantern.exe");
        assert_eq!(linux_to_wine_path(Path::new("logs/today")), r"logs\today");
        assert_eq!(linux_to_wine_path(Path::new("//server/share")), r"\\server\share");
    }

    /// In Steam's Proton chain the game is the first Windows program; its own arguments
    /// stay after it, and peerparty.exe starts it.
    #[test]
    fn the_game_in_steams_command_is_started_by_peerparty_exe() {
        let command = os(&[
            "/home/player/.local/share/Steam/ubuntu12_32/reaper",
            "SteamLaunch",
            "AppId=1234570",
            "--",
            "/home/player/.local/share/Steam/steamapps/common/Proton 9.0/proton",
            "waitforexitandrun",
            "/home/player/Games/Lantern Keep/Lantern.exe",
            "-windowed",
            "save.exe",
        ]);
        let exists = |p: &Path| p.to_string_lossy().contains("Lantern");
        assert_eq!(game_exe_in(&command, exists), Some(6));
        let wrapped = inject_via(&command, 6, &test_exe(), None);
        assert_eq!(&wrapped[..6], &command[..6], "Proton's part is as it was");
        assert_eq!(&wrapped[6..], &os(&["/opt/peerparty/peerparty.exe", "--hosted", r"Z:\home\player\Games\Lantern Keep\Lantern.exe", "-windowed", "save.exe"])[..]);
        assert_eq!(game_exe_in(&os(&["/usr/bin/native-game", "--fast"]), |_| true), None);
    }

    /// A Windows program on its own runs under Wine (or the runner named).
    #[test]
    fn a_plain_windows_program_runs_under_wine() {
        let command = os(&["/games/Tidewatch/Tidewatch.EXE", "-x"]);
        assert_eq!(game_exe_in(&command, |_| true), Some(0));
        assert_eq!(inject_via(&command, 0, &test_exe(), None), os(&["wine", "/opt/peerparty/peerparty.exe", "--hosted", r"Z:\games\Tidewatch\Tidewatch.EXE", "-x"]));
        assert_eq!(inject_via(&command, 0, &test_exe(), Some(OsStr::new("/opt/wine/bin/wine")))[0], "/opt/wine/bin/wine");
        assert!(is_windows_program(b"MZ\x90\0") && !is_windows_program(b"\x7fELF"));
    }

    /// The hook's paths go into the environment as Wine names them; unset or relative
    /// ones are left alone.
    #[test]
    fn the_hooks_paths_are_wines() {
        let get = |k: &str| match k {
            "PEERPARTY_LOG_DIR" => Some(OsString::from("/tmp/peerparty/logs")),
            "PEERPARTY_GAME_ROOT" => Some(OsString::from("relative/root")),
            _ => None,
        };
        assert_eq!(
            wine_paths(&["PEERPARTY_LOG_DIR", "PEERPARTY_GAME_ROOT", "PEERPARTY_OTHER"], get),
            // On Windows a path starting with `/` isn't absolute: nothing to change there.
            if cfg!(unix) { vec![("PEERPARTY_LOG_DIR".to_string(), r"Z:\tmp\peerparty\logs".to_string())] } else { vec![] }
        );
    }
}
