//! Starting a game with the hook on Windows, and waiting for it.
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
//!   kernel32 and run on a remote thread (wow64.rs). Then the main thread jumps on to the
//!   entry point.

use std::env;
use std::ffi::{OsStr, OsString, c_void};
use std::path::{Path, PathBuf};
use std::{fs, mem, ptr};

use peerparty_protocol::env as penv;
use peerparty_protocol::session::{InstanceId, InstanceSpec};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, QueueUserAPC, ResumeThread, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};

use super::{cpu, os_error, packaged, tree, wide, wide_bytes, wow64, wrapper};
use crate::host::{self, SessionLaunch};
use crate::launch::{self, ChildEnv, Exited, LaunchError, SplitScreen};
use crate::ui::store::Prefs;

/// The game the command line names, worked out before anything starts.
pub struct Target {
    /// What the player chose (a .bat stays the game's name in Settings and Recent).
    pub listed: PathBuf,
    /// The program that is the game: a .bat's own program, a packaged game's exe.
    pub exe: PathBuf,
    args: Vec<OsString>,
    /// Where a .bat starts its program.
    cwd: Option<PathBuf>,
    /// A Microsoft Store (GDK) game's package.
    pkg: Option<packaged::Package>,
}

impl Target {
    /// A packaged game follows its own package's programs, not a folder of them.
    pub fn packaged(&self) -> bool {
        self.pkg.is_some()
    }
}

/// The game `first` (with its `args`): its full path, and for a .bat or .cmd the program
/// it runs (wrapper.rs, whose variables go into `env` for the game).
pub fn resolve(first: PathBuf, args: Vec<OsString>, env: &mut ChildEnv) -> Result<Target, LaunchError> {
    let exe = fs::canonicalize(&first).map_err(|e| LaunchError::missing(&first, &e))?;
    let exe = strip_verbatim(&exe);
    let listed = exe.clone();
    // A .bat or .cmd: the program it runs is the game (wrapper.rs).
    let (exe, args, cwd) = if wrapper::is_script(&exe) {
        let w = wrapper::read(&exe, &args).map_err(LaunchError::Other)?;
        let real = fs::canonicalize(&w.exe).map_err(|e| LaunchError::missing(&w.exe, &e))?;
        eprintln!("peerparty: {} runs {}: starting that", exe.display(), w.exe.display());
        for (k, v) in &w.vars {
            env.set(k, v);
        }
        (strip_verbatim(&real), w.args, w.cwd)
    } else {
        (exe, args, None)
    };
    // A packaged game's program can only be run, not read (its files allow execute but not
    // read data), and `--attach` works out its bitness from the running process.
    let pkg = packaged::find(&exe);
    Ok(Target { listed, exe, args, cwd, pkg })
}

/// `--attach`: run by Windows for a packaged (Microsoft Store) game (packaged.rs).
pub fn attach(rest: &[String]) -> Result<u32, LaunchError> {
    packaged::attach(rest).map_err(LaunchError::Other)
}

/// Starts the game with the hook (`env` is its full environment), hosts the overlay on
/// `pipe` while it runs, and returns how it ended once it (and, with a game folder
/// `root`, every program started from it) has ended.
pub fn start(t: Target, env: ChildEnv, pipe: Option<host::pipe::Pipe>, prefs: Prefs, log_dir: &Path, root: Option<&Path>) -> Result<Exited, LaunchError> {
    if let Some(pkg) = &t.pkg {
        return run_packaged(pkg, &t.exe, &t.args, &env, pipe, prefs, log_dir);
    }
    let exe = t.exe;
    let x86 = match pe_machine(&exe)?.0 {
        0x8664 => false,
        0x014c => true,
        m => return Err(LaunchError::Machine(m)),
    };
    let dll = crate::hooks::extract(x86).map_err(LaunchError::Other)?;

    let mut cmdline = quote(exe.as_os_str());
    for a in &t.args {
        cmdline.push(" ");
        cmdline.push(quote(a));
    }
    let inherited = env::current_dir().ok().map(|p| strip_verbatim(&p));
    let (cwd, cwd_why) = working_dir(&exe, t.cwd.as_deref(), root, inherited.as_deref());
    if let Some(why) = cwd_why {
        if !same_path(&cwd, exe.parent().unwrap_or(Path::new("."))) {
            eprintln!("peerparty: working folder is {} ({why})", cwd.display());
        }
    }

    let tree = tree::Tree::new(root);

    let pi = unsafe { start_injected(&exe, &cmdline, &cwd, &env, &dll, x86, false, tree.as_ref(), None)? };
    launch::remember(&t.listed);
    eprintln!(
        "peerparty: started {} (pid {}, {}), logs in {}",
        exe.display(),
        pi.dwProcessId,
        if x86 { "x86" } else { "x64" },
        log_dir.display()
    );

    let closer = tree.as_ref().map_or_else(|| tree::Closer::process(pi.hProcess), |t| t.closer(pi.hProcess));
    listen_for_close(closer);
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
            launch::log_exit(log_dir, starter, *pid, &format!("{} ended{code}; the game went on in {} (pid {})", starter.display(), ended.exe.display(), ended.pid));
        }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        if let Some(w) = &watch {
            CloseHandle(w.0);
        }
        Ok(launch::report_exit(log_dir, &ended.exe, ended.pid, ended.code, ended.ran))
    }
}

/// Starts `split.instances` copies of `t` inside one unified `SessionHost` (`run_session`).
/// A split-screen copy's own Unity log, for a Unity game (`<exe>_Data` beside it) whose
/// arguments don't name one: every copy of a game writes the same `Player.log` under
/// LocalLow otherwise, their lines mixed mid-word, and nothing says which copy wrote an
/// error. It goes beside PeerParty's logs: `<game>-<start>-player<N>.unity.log`.
fn unity_log_for(exe: &Path, args: &[OsString], log_dir: &Path, stamp: &str, player: u32) -> Option<PathBuf> {
    let stem = exe.file_stem()?.to_string_lossy().into_owned();
    if !exe.parent()?.join(format!("{stem}_Data")).is_dir() {
        return None;
    }
    if args.iter().any(|a| a.to_string_lossy().trim_start_matches('-').eq_ignore_ascii_case("logFile")) {
        return None;
    }
    Some(log_dir.join(format!("{stem}-{stamp}-player{player}.unity.log")))
}

pub fn start_session(t: Target, base_env: ChildEnv, split: &SplitScreen, prefs: Prefs, log_dir: &Path, root: Option<&Path>) -> Result<Exited, LaunchError> {
    launch::set_log_dir(log_dir);
    if t.packaged() {
        return Err(LaunchError::Other("split-screen sessions aren't supported for packaged Microsoft Store games yet".into()));
    }
    let count = split.instances.clamp(1, SplitScreen::MAX_INSTANCES);
    let exe = t.exe;
    let (machine, console) = pe_machine(&exe)?;
    let x86 = match machine {
        0x8664 => false,
        0x014c => true,
        m => return Err(LaunchError::Machine(m)),
    };
    let dll = crate::hooks::extract(x86).map_err(LaunchError::Other)?;
    let mut cmdline = quote(exe.as_os_str());
    for a in &t.args {
        cmdline.push(" ");
        cmdline.push(quote(a));
    }
    let inherited = env::current_dir().ok().map(|p| strip_verbatim(&p));
    let (cwd, cwd_why) = working_dir(&exe, t.cwd.as_deref(), root, inherited.as_deref());
    if let Some(why) = cwd_why {
        if !same_path(&cwd, exe.parent().unwrap_or(Path::new("."))) {
            eprintln!("peerparty: working folder is {} ({why})", cwd.display());
        }
    }
    let stamp = {
        let [y, mo, d, h, mi, s] = crate::platform::local_time();
        format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
    };

    // Enable DPI awareness before measuring the monitor so every game starts with the
    // exact physical pane dimensions in `PEERPARTY_INSTANCE` before creating its window.
    let bounds = host::initial_session_bounds(split.monitor);
    // How the session's window shows the copies: the environment's or the game's setting
    // (`settings_env` put the game's in the environment unless one was there).
    let compositor = base_env.get(penv::COMPOSITOR).map(|v| v.to_string_lossy().into_owned());
    let dwm = compositor.as_deref().is_some_and(|v| v.trim().eq_ignore_ascii_case("dwm"));
    let pixels = host::pixel_frames(compositor.as_deref());
    eprintln!(
        "peerparty: the session's window shows {}",
        if dwm { "the copies' windows (PEERPARTY_COMPOSITOR=dwm)" } else if pixels { "the copies' frames, read back as pixels (PEERPARTY_COMPOSITOR=pixels)" } else { "the copies' frames, drawn on the GPU" }
    );
    let panes = split.layout.compute_panes(bounds.width, bounds.height, count as usize);
    let slots = split.layout.to_slots(count as usize);

    // Fair-share CPU core partitioning (soft Windows CPU Sets + reported logical core cap)
    // and GPU VRAM budget sharing, unless turned off in settings.
    let cpu_sets = if !split.no_cpu_balance && count >= 2 {
        let all = cpu::query_cpu_sets();
        let parts = cpu::partition_cpu_sets(&all, count as usize);
        if !parts.is_empty() {
            eprintln!("peerparty: split screen: balancing CPU cores across {count} copies ({} logical processors partitioned)", all.len());
        }
        parts
    } else {
        Vec::new()
    };
    let cpu_cores = if !split.no_cpu_balance && count >= 2 {
        let total = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(0);
        cpu::recommended_worker_cores(total, count)
    } else {
        None
    };

    let ctx = SpawnCtx {
        exe: exe.clone(),
        args: t.args,
        cmdline,
        cwd: cwd.to_path_buf(),
        base_env,
        split: split.clone(),
        dll,
        x86,
        console,
        log_dir: log_dir.to_path_buf(),
        root: root.map(Path::to_path_buf),
        stamp,
        bounds,
        dwm,
        count,
        cpu_sets,
        cpu_cores,
    };

    let simultaneous = count <= 1 || split.stagger == launch::StaggerMode::Fixed(0);
    let initial_count = if simultaneous { count } else { 1 };
    let copies = std::sync::Arc::new(std::sync::Mutex::new(Vec::<SessionCopy>::with_capacity(count as usize)));
    let mut launches: Vec<SessionLaunch> = Vec::with_capacity(initial_count as usize);

    for slot in 0..initial_count {
        let pane = panes.get(slot as usize).map(|p| peerparty_protocol::session::PaneRect {
            x: bounds.x.saturating_add(p.x),
            y: bounds.y.saturating_add(p.y),
            width: p.width,
            height: p.height,
        });
        let virt = pane.and_then(|p| {
            let (w, h) = slots.get(slot as usize)?.aspect.size_for(p, slots[slot as usize].fit, bounds.width, bounds.height);
            Some(peerparty_protocol::session::scale_dimensions(w, h, split.render_scale))
        });
        match ctx.spawn_slot(slot, pane, virt) {
            Ok((launch, copy)) => {
                launches.push(launch);
                copies.lock().unwrap_or_else(|e| e.into_inner()).push(copy);
            }
            Err(e) => {
                // The copies started already go: every program of each (a starter may
                // have started its game by now).
                for l in &launches {
                    if let Some(stop) = &l.stop {
                        stop.force();
                    }
                }
                let started: Vec<SessionCopy> = std::mem::take(&mut *copies.lock().unwrap_or_else(|e| e.into_inner()));
                for c in started {
                    unsafe { c.wait(log_dir, &exe) };
                }
                return Err(e);
            }
        }
    }

    let spawner: Option<host::PendingSpawner> = (!simultaneous).then(|| {
        let copies = copies.clone();
        Box::new(move |slot, pane, virt| {
            let (launch, copy) = ctx.spawn_slot(slot, pane, virt).map_err(|e| e.to_string())?;
            copies.lock().unwrap_or_else(|e| e.into_inner()).push(copy);
            Ok(launch)
        }) as host::PendingSpawner
    });

    launch::remember(&t.listed);
    // Close all in the launcher's window goes through the session, as its own window's
    // Close does: each copy asked, then ended after a while.
    listen_for_close_with(host::close_session);
    host::run_session(
        launches,
        host::SessionLook {
            layout: split.layout.clone(),
            bounds,
            pixels,
            no_kbm: split.no_kbm,
            stagger: split.stagger,
            sync: !split.no_sync,
            render_scale: split.render_scale,
            upscale: !split.no_upscale,
            fps: split.fps,
            total_count: count as usize,
        },
        spawner,
        &exe,
        prefs,
        false,
    );

    // PeerParty's exit code is the first player's, unless another player's game crashed:
    // a session is as good as its worst copy.
    let copies: Vec<SessionCopy> = std::mem::take(&mut *copies.lock().unwrap_or_else(|e| e.into_inner()));
    let mut exits = Vec::with_capacity(copies.len());
    for (i, c) in copies.into_iter().enumerate() {
        let exited = unsafe { c.wait(log_dir, &exe) };
        if exited.0 != Some(0) {
            eprintln!("peerparty: player {}'s game ended with {}", i + 1, exited.0.map_or("no exit code".into(), |c| format!("exit code {c:#X}")));
        }
        exits.push(exited);
    }
    Ok(session_exit(exits))
}

/// Owned launch context so split-screen copies 2..N can be started on demand by
/// `SessionHost`'s smart ready-gate stagger queue.
struct SpawnCtx {
    exe: PathBuf,
    args: Vec<OsString>,
    cmdline: OsString,
    cwd: PathBuf,
    base_env: ChildEnv,
    split: SplitScreen,
    dll: PathBuf,
    x86: bool,
    /// The game is a console program (`pe_machine`): each copy's console has no window.
    console: bool,
    log_dir: PathBuf,
    root: Option<PathBuf>,
    stamp: String,
    bounds: host::MonitorBounds,
    dwm: bool,
    count: u32,
    cpu_sets: Vec<Vec<u32>>,
    cpu_cores: Option<u32>,
}

impl SpawnCtx {
    fn spawn_slot(
        &self,
        slot: u32,
        pane: Option<peerparty_protocol::session::PaneRect>,
        virt: Option<(u32, u32)>,
    ) -> Result<(SessionLaunch, SessionCopy), LaunchError> {
        let mut env = self.base_env.clone();
        if slot == 0 {
            env.remove(penv::COPY_SAVES);
        }
        let pipe = host::pipe::Pipe::for_game(env.get(penv::IPC).as_deref())
            .ok_or_else(|| LaunchError::Other(format!("couldn't create IPC pipe for player {}", slot + 1)))?;
        launch::connect(&mut env, &pipe);
        let mut spec = InstanceSpec::solo(InstanceId(slot), slot);
        spec.focus_spoof = self.count > 1;
        // The session's window draws the copy's frames itself (host/gpu.rs), unless told
        // to show the copies' windows.
        spec.compositor_capture = !self.dwm;
        spec.input_assignment = self.split.input(slot).assignment(slot);
        spec.identity_override = self.split.name(slot);
        // Its saves are that player's from the hook's start (hook saves/mod.rs).
        env.set(penv::PLAYER_SLOT, slot.to_string());
        if !self.split.no_vram_budget && self.count >= 2 {
            env.set_default(penv::VRAM_SHARE, self.count.to_string());
        }
        if let Some(cores) = self.cpu_cores {
            env.set_default(penv::CPU_CORES, cores.to_string());
        }
        if let Some(pane) = pane {
            let (vw, vh) = virt.unwrap_or((pane.width, pane.height));
            spec.pane = Some(peerparty_protocol::session::PaneRect { x: pane.x, y: pane.y, width: vw, height: vh });
            spec.virtual_width = Some(vw);
            spec.virtual_height = Some(vh);
            spec.dpi = Some(self.bounds.dpi);
        }
        env.set(penv::INSTANCE, spec.to_json().to_string());

        let mut line = self.cmdline.clone();
        if let Some(unity_log) = unity_log_for(&self.exe, &self.args, &self.log_dir, &self.stamp, slot + 1) {
            eprintln!("peerparty: player {}'s Unity log is {}", slot + 1, unity_log.display());
            line.push(" -logFile ");
            line.push(quote(unity_log.as_os_str()));
        }
        // Each copy has a job of its own: a starter (Unreal's bootstrap, a launcher beside
        // the game) ends once its game runs, and the copy is the game's programs from the
        // game's folder until the last of them ends, as a game started alone is (tree.rs).
        let tree = tree::Tree::new(self.root.as_deref());
        let preferred_cpus = self.cpu_sets.get(slot as usize).map(Vec::as_slice);
        // A console program started from a program without a console gets a new console
        // window (in Windows Terminal, on Windows 11) over the session's window, where it
        // only hides the panes, and closing it ends that player's game. Its console stays;
        // only the window goes.
        let pi = unsafe { start_injected(&self.exe, &line, &self.cwd, &env, &self.dll, self.x86, self.console, tree.as_ref(), preferred_cpus)? };
        eprintln!(
            "peerparty: started player {} ({}, pid {}, {})",
            slot + 1,
            self.exe.display(),
            pi.dwProcessId,
            if self.x86 { "x86" } else { "x64" }
        );
        // The copy's own log says so too (hook child.rs `note_own_console`): it isn't
        // there yet for a `launch::note`.
        if self.console {
            eprintln!("peerparty: player {}'s game is a console program: its console has no window", slot + 1);
        }
        let closer = tree.as_ref().map_or_else(|| tree::Closer::process(pi.hProcess), |tr| tr.closer(pi.hProcess));
        let watch = tree.map(|tr| tr.watch(pi.hProcess, pi.dwProcessId, self.exe.clone()));
        let done = watch.as_ref().map_or(pi.hProcess, |w| w.0);
        let launch = SessionLaunch { process: done, pid: pi.dwProcessId, pipe, spec, input: self.split.input(slot), stop: Some(Box::new(closer)) };
        let copy = SessionCopy { pi, watch };
        Ok((launch, copy))
    }
}

/// What a split-screen session ends with: the first player's exit, unless another
/// player's game ended with an error.
fn session_exit(exits: Vec<Exited>) -> Exited {
    let first = exits.first().map_or(Exited(Some(0)), |e| Exited(e.0));
    if first.0 != Some(0) {
        return first;
    }
    exits.into_iter().find(|e| e.0 != Some(0)).unwrap_or(first)
}

/// One copy of a split-screen session: the program started, and the wait for all of its
/// programs (tree.rs; none if it couldn't be put in a job).
struct SessionCopy {
    pi: PROCESS_INFORMATION,
    watch: Option<(HANDLE, std::sync::Arc<std::sync::Mutex<Option<tree::Ended>>>)>,
}

impl SessionCopy {
    /// Waits for every program of it to end, says how it ended in its log, and lets its
    /// handles go.
    unsafe fn wait(self, log_dir: &Path, exe: &Path) -> Exited {
        let pi = self.pi;
        let done = self.watch.as_ref().map_or(pi.hProcess, |w| w.0);
        unsafe {
            let waited = WaitForSingleObject(done, INFINITE) != WAIT_FAILED;
            let ended = self.watch.as_ref().and_then(|w| w.1.lock().unwrap_or_else(|e| e.into_inner()).take()).unwrap_or_else(|| {
                let mut code = 0u32;
                let known = waited && GetExitCodeProcess(pi.hProcess, &mut code) != 0;
                tree::Ended { pid: pi.dwProcessId, exe: exe.to_path_buf(), code: known.then_some(code), ran: run_time(pi.hProcess), starter: None }
            });
            if let Some((pid, starter, code)) = &ended.starter {
                let code = code.map(|c| format!(" with exit code {c}")).unwrap_or_default();
                let line = format!("{} ended{code}; the game went on in {} (pid {})", starter.display(), ended.exe.display(), ended.pid);
                launch::log_exit(log_dir, starter, *pid, &line);
            }
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            if let Some(w) = &self.watch {
                CloseHandle(w.0);
            }
            launch::report_exit(log_dir, &ended.exe, ended.pid, ended.code, ended.ran)
        }
    }
}

/// A Microsoft Store (GDK) game: started through Windows' packaged-app activation with
/// PeerParty attached (packaged.rs), then hosted and waited for like any other.
fn run_packaged(
    pkg: &packaged::Package,
    exe: &Path,
    game_args: &[OsString],
    env: &ChildEnv,
    pipe: Option<host::pipe::Pipe>,
    prefs: Prefs,
    log_dir: &Path,
) -> Result<Exited, LaunchError> {
    let peerparty = env::current_exe().map_err(|e| LaunchError::Other(e.to_string()))?;
    let vars = env.peerparty_vars();
    let args: Vec<String> = game_args.iter().map(|a| quote(a).to_string_lossy().into_owned()).collect();
    let helper = packaged::start(pkg, &peerparty, &args.join(" "), &vars).map_err(LaunchError::Other)?;
    launch::remember(exe);
    eprintln!("peerparty: started {} (package {}, helper pid {helper}), logs in {}", pkg.aumid, pkg.full_name, log_dir.display());
    let (process, pid) = packaged::wait_for_game(pkg, helper).ok_or_else(|| LaunchError::Other("the game's process never appeared".into()))?;
    listen_for_close(tree::Closer::process(process));
    if let Some(pipe) = pipe {
        host::run(process, pid, pipe, &pkg.game, prefs, true);
    }
    unsafe {
        let mut code = 0u32;
        let known = WaitForSingleObject(process, INFINITE) != WAIT_FAILED && GetExitCodeProcess(process, &mut code) != 0;
        let ran = run_time(process);
        CloseHandle(process);
        Ok(launch::report_exit(log_dir, &pkg.game, pid, known.then_some(code), ran))
    }
}

/// How long the game process ran, from its creation and exit times.
pub unsafe fn run_time(process: HANDLE) -> Option<std::time::Duration> {
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

unsafe fn start_injected(
    exe: &Path,
    cmdline: &OsStr,
    cwd: &Path,
    env: &ChildEnv,
    dll: &Path,
    x86: bool,
    no_console_window: bool,
    tree: Option<&tree::Tree>,
    cpu_sets: Option<&[u32]>,
) -> Result<PROCESS_INFORMATION, LaunchError> {
    unsafe {
        let app = wide(exe);
        let mut cmd = wide(cmdline);
        let dir = wide(cwd);
        let env_block = env.block();
        let mut si: STARTUPINFOW = mem::zeroed();
        si.cb = mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = mem::zeroed();
        if CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | if no_console_window { CREATE_NO_WINDOW } else { 0 },
            env_block.as_ptr().cast(),
            dir.as_ptr(),
            &si,
            &mut pi,
        ) == 0
        {
            let e = std::io::Error::last_os_error();
            return Err(LaunchError::os(format!("CreateProcess failed: {e}"), &e));
        }

        let fail = |err: LaunchError| {
            TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            err
        };

        let injected = if x86 { wow64::inject(&pi, dll) } else { inject_x64(&pi, dll) };
        if let Err(what) = injected {
            return Err(fail(LaunchError::Inject(what)));
        }
        // Before it runs, so every program it starts is in the job too.
        if tree.is_some_and(|t| !t.add(pi.hProcess)) {
            eprintln!("peerparty: can't watch the programs the game starts ({}); waiting for this one only", std::io::Error::last_os_error());
        }
        if let Some(ids) = cpu_sets {
            let _ = cpu::apply_cpu_sets(pi.hProcess, ids);
        }
        // An x64 game's hook loads once it runs (the APC): it says so on this event, and
        // a game it never loads in is reported (a 32-bit one's load was waited for).
        if !x86 {
            watch_hook_load(&pi);
        }
        if ResumeThread(pi.hThread) == u32::MAX {
            return Err(fail(LaunchError::Other(format!("ResumeThread failed: {}", std::io::Error::last_os_error()))));
        }
        Ok(pi)
    }
}

/// Waits on a thread of its own for the window's Close all, then closes the game. The
/// wait for the game to end goes on as usual and reports how it ended.
fn listen_for_close(closer: tree::Closer) {
    listen_for_close_with(move || closer.close());
}

/// Waits on a thread of its own for the window's Close all, then runs `close`.
fn listen_for_close_with(close: impl FnOnce() + Send + 'static) {
    use windows_sys::Win32::System::Threading::CreateEventW;
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, super::invite::close_event(std::process::id()).as_ptr()) };
    if event.is_null() {
        return;
    }
    let event = event as usize;
    std::thread::spawn(move || unsafe {
        if WaitForSingleObject(event as _, INFINITE) == 0 {
            close();
        }
        CloseHandle(event as _);
    });
}

/// The event the hook sets once it has loaded in process `pid` (hook/src/lib.rs).
pub fn loaded_event(pid: u32) -> Vec<u16> {
    wide(format!("Local\\peerparty-loaded-{pid}"))
}

/// Made before the game runs: a thread waits for the hook's word that it loaded, and
/// says so in PeerParty's output when it hasn't within LOAD_WAIT while the game still
/// runs (security software can stop a DLL from loading without a word to us).
unsafe fn watch_hook_load(pi: &PROCESS_INFORMATION) {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, WaitForMultipleObjects};
    const LOAD_WAIT: u32 = 20_000;
    unsafe {
        let event = CreateEventW(ptr::null(), 1, 0, loaded_event(pi.dwProcessId).as_ptr());
        if event.is_null() {
            return;
        }
        let mut process = ptr::null_mut();
        let me = GetCurrentProcess();
        if DuplicateHandle(me, pi.hProcess, me, &mut process, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
            CloseHandle(event);
            return;
        }
        let (event, process, pid) = (event as usize, process as usize, pi.dwProcessId);
        std::thread::spawn(move || {
            let handles = [event as _, process as _];
            let r = WaitForMultipleObjects(2, handles.as_ptr(), 0, LOAD_WAIT);
            // Loaded (WAIT_OBJECT_0), or the game ended first: nothing to say.
            if r == WAIT_TIMEOUT {
                eprintln!("peerparty: the game (pid {pid}) runs without peerparty: its hook hasn't loaded after {} s (security software may have stopped it)", LOAD_WAIT / 1000);
            }
            CloseHandle(event as _);
            CloseHandle(process as _);
        });
    }
}

/// Copies `bytes` into a new read-write region of the game process.
pub unsafe fn write_remote(pi: &PROCESS_INFORMATION, bytes: &[u8]) -> Result<usize, String> {
    unsafe {
        let remote = VirtualAllocEx(pi.hProcess, ptr::null(), bytes.len(), MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            return Err(os_error("VirtualAllocEx"));
        }
        if WriteProcessMemory(pi.hProcess, remote, bytes.as_ptr() as *const c_void, bytes.len(), ptr::null_mut()) == 0 {
            return Err(os_error("WriteProcessMemory"));
        }
        Ok(remote as usize)
    }
}

/// Early bird: the APC runs on the main thread after the loader maps static imports.
/// kernel32 sits at the same address in every x64 process for this boot, so our own
/// `LoadLibraryW` is valid in the game.
pub unsafe fn inject_x64(pi: &PROCESS_INFORMATION, dll: &Path) -> Result<(), String> {
    unsafe {
        let remote = write_remote(pi, &wide_bytes(dll.as_os_str()))?;
        let k32 = GetModuleHandleW(wide("kernel32.dll").as_ptr());
        let load_library = GetProcAddress(k32, c"LoadLibraryW".as_ptr() as *const u8).ok_or_else(|| os_error("GetProcAddress(LoadLibraryW)"))?;
        let apc: unsafe extern "system" fn(usize) = mem::transmute(load_library);
        if QueueUserAPC(Some(apc), pi.hThread, remote) == 0 {
            return Err(os_error("QueueUserAPC"));
        }
        Ok(())
    }
}

/// IMAGE_FILE_HEADER.Machine of a PE file, and whether it's a console program (its
/// optional header's Subsystem is IMAGE_SUBSYSTEM_WINDOWS_CUI).
fn pe_machine(exe: &Path) -> Result<(u16, bool), LaunchError> {
    let buf = fs::read(exe).map_err(|e| LaunchError::missing(exe, &e))?;
    let rd16 = |o: usize| buf.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let rd32 = |o: usize| buf.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let not_pe = || LaunchError::NotProgram(exe.to_path_buf());
    let nt = rd32(0x3c).ok_or_else(not_pe)? as usize;
    if rd16(0) != Some(0x5a4d) || rd32(nt) != Some(0x4550) {
        return Err(not_pe());
    }
    let machine = rd16(nt + 4).ok_or_else(|| LaunchError::Other("truncated PE header".into()))?;
    // Subsystem sits at the same offset in PE32 and PE32+ optional headers.
    Ok((machine, rd16(nt + 24 + 68) == Some(3)))
}

/// `fs::canonicalize` returns `\\?\C:\…`; games don't expect that in argv[0].
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

fn norm_path(p: &Path) -> String {
    let s = strip_verbatim(p).to_string_lossy().replace('/', "\\");
    let trimmed = s.trim_end_matches('\\');
    if trimmed.len() == 2 && trimmed.ends_with(':') {
        format!("{trimmed}\\").to_ascii_lowercase()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    norm_path(a) == norm_path(b)
}

fn under_or_eq(dir: &Path, root: &Path) -> bool {
    let (d, r) = (norm_path(dir), norm_path(root));
    d == r || d.starts_with(&if r.ends_with('\\') { r } else { format!("{r}\\") })
}

/// The working folder the game starts in, and why when it isn't the program's own folder:
/// 1. A wrapper script's explicit `cd` / `start /d`.
/// 2. The caller's working folder when it is already inside the game's install folder
///    (Steam launch options and store launchers set it to the game's root or configured
///    working directory), unless it is just the program's own folder.
/// 3. An ancestor folder (up to `root`) that holds an executable with the same filename:
///    games that ship an x64 or compatibility build in a subfolder beside a root binary
///    resolve their relative data folders from that ancestor.
/// 4. Otherwise, the program's own folder.
fn working_dir(exe: &Path, explicit: Option<&Path>, root: Option<&Path>, inherited: Option<&Path>) -> (PathBuf, Option<&'static str>) {
    if let Some(d) = explicit {
        return (d.to_path_buf(), Some("from the wrapper script"));
    }
    let exe_dir = exe.parent().unwrap_or(Path::new("."));
    if let Some(root) = root {
        if let Some(cur) = inherited {
            if under_or_eq(cur, root) && !same_path(cur, exe_dir) && cur.is_dir() {
                return (cur.to_path_buf(), Some("kept from the caller, inside the game's folder"));
            }
        }
        if let Some(name) = exe.file_name() {
            if under_or_eq(exe_dir, root) && !same_path(exe_dir, root) {
                let mut cur = exe_dir.parent();
                while let Some(anc) = cur {
                    if !under_or_eq(anc, root) {
                        break;
                    }
                    if anc.join(name).is_file() {
                        return (anc.to_path_buf(), Some("same-named program sits in that folder; this build shares its data"));
                    }
                    if same_path(anc, root) {
                        break;
                    }
                    cur = anc.parent();
                }
            }
        }
    }
    (exe_dir.to_path_buf(), None)
}

/// Quotes one argument by the MSVC CRT rules (CommandLineToArgvW).
fn quote(arg: &OsStr) -> OsString {
    let s = arg.to_string_lossy();
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return arg.to_os_string();
    }
    launch::crt_quote(&s).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console program's copies are started without a console window: it's told from
    /// a windowed one by its header.
    #[test]
    fn a_console_game_is_told_from_a_windowed_one() {
        let windir = env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        assert_eq!(pe_machine(&windir.join(r"System32\cmd.exe")).unwrap(), (0x8664, true));
        assert!(!pe_machine(&windir.join("explorer.exe")).unwrap().1, "a windowed program");
        // PeerParty itself is a windowed one (main.rs), as this test binary is.
        assert!(!pe_machine(&env::current_exe().unwrap()).unwrap().1);
    }

    /// A session ends with its first player's exit code, unless another player's game
    /// crashed.
    #[test]
    fn a_session_ends_with_its_worst_copy() {
        let ex = |v: &[Option<u32>]| session_exit(v.iter().map(|&c| Exited(c)).collect()).0;
        assert_eq!(ex(&[Some(0), Some(0)]), Some(0));
        assert_eq!(ex(&[Some(0), Some(0xC000_0005)]), Some(0xC000_0005));
        assert_eq!(ex(&[Some(3), Some(0xC000_0005)]), Some(3));
        assert_eq!(ex(&[Some(0), None]), None);
        assert_eq!(ex(&[]), Some(0));
    }

    #[test]
    fn a_unity_copy_gets_its_own_log() {
        let dir = env::temp_dir().join(format!("peerparty-unitylog-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Game_Data")).unwrap();
        let exe = dir.join("Game.exe");
        let logs = Path::new(r"C:\logs");
        let got = unity_log_for(&exe, &[], logs, "20261123-204051", 2);
        assert_eq!(got, Some(logs.join("Game-20261123-204051-player2.unity.log")));
        // The game's own arguments win; a game that isn't Unity gets none.
        assert_eq!(unity_log_for(&exe, &["-logFile".into(), "x.log".into()], logs, "s", 1), None);
        assert_eq!(unity_log_for(&dir.join("Other.exe"), &[], logs, "s", 1), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn working_dir_handles_wrapper_caller_and_subdir_builds() {
        let base = env::temp_dir().join(format!("peerparty-cwd-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("Some Game");
        let sub = root.join("compat");
        let x64 = root.join("x64");
        let bin = root.join("bin");
        let outside = base.join("Launcher");
        for d in [&sub, &x64, &bin, &outside] {
            fs::create_dir_all(d).unwrap();
        }
        let root_exe = root.join("SomeGame.exe");
        let sub_exe = sub.join("SomeGame.exe");
        let x64_exe = x64.join("SomeGame.exe");
        let bin_exe = bin.join("DifferentName.exe");
        for f in [&root_exe, &sub_exe, &x64_exe, &bin_exe] {
            fs::write(f, b"MZ").unwrap();
        }

        // Subdir build with same-named exe in root -> uses root even from an unrelated cwd or subdir cwd.
        assert_eq!(working_dir(&sub_exe, None, Some(&root), Some(&outside)).0, root);
        assert_eq!(working_dir(&sub_exe, None, Some(&root), Some(&sub)).0, root);
        assert_eq!(working_dir(&x64_exe, None, Some(&root), None).0, root);

        // Subdir exe without a same-named ancestor stays in its own folder unless caller set cwd inside root.
        assert_eq!(working_dir(&bin_exe, None, Some(&root), Some(&outside)).0, bin);
        assert_eq!(working_dir(&bin_exe, None, Some(&root), Some(&root)).0, root);

        // Wrapper script's explicit cwd always wins.
        assert_eq!(working_dir(&sub_exe, Some(&sub), Some(&root), Some(&root)).0, sub);

        let _ = fs::remove_dir_all(&base);
    }
}
