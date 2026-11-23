//! Following a launcher into the game it starts. Microsoft Store (GDK) games start
//! `GameLaunchHelper.exe`, which starts the real game; PeerParty is injected into the
//! helper (launcher/src/packaged.rs) and, from there, into each process the helper
//! creates: `CreateProcessInternalW` (what CreateProcessW, -A and -AsUser all reach) gets
//! CREATE_SUSPENDED added, the hook DLL is queued to load on the child's main thread (an
//! early-bird APC, as the launcher does), and the child resumes unless the caller asked
//! for it suspended.
//!
//! On in packaged processes (the helper and what it starts), following only programs
//! inside the game's own package: the helper also starts Microsoft's GamingServicesUI
//! for sign-in, which is left alone. In a game started from a folder, the launcher names
//! the game's install folder (`PEERPARTY_GAME_ROOT`, launcher/src/gameroot.rs), and a
//! program started from inside it is followed, as long as the process starting it hasn't
//! become the game itself (loaded a game SDK): a starter (Unreal's `<Game>.exe`
//! bootstrap, a Unity or .NET launcher, any `Launcher.exe` beside the game) passes the
//! hook on, a game starting a helper of its own doesn't. Never followed: anything outside
//! that folder, crash reporters, Epic's helper, anti-cheat starters (the game refuses
//! anti-cheat anyway) and installers of redistributables (`never_followed`). Without a
//! folder from the launcher, Unreal's layout still marks its starter (`unreal_root`).
//! With `PEERPARTY_FOLLOW=1` (any game) every child is followed. In the helper the hook
//! does nothing else: no pipe to the launcher, no LAN.
//!
//! The pipe to the launcher (the overlay) goes to the process that turns out to be the
//! game. A followed child doesn't connect at once: the parent makes an event named for
//! the child's pid (`Local\peerparty-follow-<pid>`), and the child signals it once it
//! loads a game SDK, shows a window of a game's size, or its parent has gone. Then the
//! parent lets the launcher go (ipc.rs `hand_over`) and the child connects. A starter
//! that starts nothing that turns into the game keeps the pipe.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use retour::GenericDetour;

use crate::sys::hook::with_lock;
use crate::log;
use crate::sys::win::wide;
use crate::sys::pe::Export;

type CreateInternal = unsafe extern "system" fn(
    usize,
    *const u16,
    *mut u16,
    usize,
    usize,
    i32,
    u32,
    usize,
    *const u16,
    usize,
    *mut ProcessInformation,
    usize,
) -> i32;

static CREATE: OnceLock<GenericDetour<CreateInternal>> = OnceLock::new();

/// The grant events made for followed children, by pid, until their claim is answered.
static GRANTS: std::sync::Mutex<Vec<(u32, usize)>> = std::sync::Mutex::new(Vec::new());
/// Whether a child's claim to be the game was granted (the first one only).
static GRANTED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
pub struct ProcessInformation {
    process: usize,
    thread: usize,
    pid: u32,
    tid: u32,
}

const CREATE_SUSPENDED: u32 = 0x4;
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x400;

/// An environment block, as CreateProcess takes it.
enum Block {
    Wide(Vec<u16>),
    Ansi(Vec<u8>),
}

/// Whether an environment entry ("NAME=value") is one of PeerParty's.
fn is_ours<T: Copy + Into<u32>>(entry: &[T]) -> bool {
    const PREFIX: &[u8] = b"PEERPARTY_";
    entry.len() >= PREFIX.len() && entry.iter().zip(PREFIX).all(|(&c, &p)| c.into() < 128 && (c.into() as u8).to_ascii_uppercase() == p)
}

/// The entries of a block that ends in an empty one, without PeerParty's; None when it
/// has none of PeerParty's (it can go as it is).
///
/// # Safety
/// `block` points at a whole environment block.
unsafe fn filter_block<T: Copy + Into<u32>>(block: *const T) -> Option<Vec<T>> {
    let mut out = Vec::new();
    let mut dropped = false;
    let mut at = block;
    loop {
        let len = (0..).take_while(|&i| unsafe { *at.add(i) }.into() != 0).count();
        if len == 0 {
            break;
        }
        let entry = unsafe { std::slice::from_raw_parts(at, len) };
        if is_ours(entry) {
            dropped = true;
        } else {
            out.extend_from_slice(entry);
            out.push(unsafe { *at.add(len) });
        }
        at = unsafe { at.add(len + 1) };
    }
    // The block's end: an empty entry (two NULs for an empty block).
    let nul = unsafe { *at };
    if out.is_empty() {
        out.push(nul);
    }
    out.push(nul);
    dropped.then_some(out)
}

/// The environment a child left alone gets: the one it was given (wide or ANSI), else
/// this process's, without PeerParty's variables. None: nothing to take out.
fn scrubbed_env(env: usize, unicode: bool) -> Option<Block> {
    use windows_sys::Win32::System::Environment::{FreeEnvironmentStringsW, GetEnvironmentStringsW};
    if env != 0 {
        return if unicode {
            unsafe { filter_block(env as *const u16) }.map(Block::Wide)
        } else {
            unsafe { filter_block(env as *const u8) }.map(Block::Ansi)
        };
    }
    let own = unsafe { GetEnvironmentStringsW() };
    if own.is_null() {
        return None;
    }
    let block = unsafe { filter_block(own as *const u16) };
    unsafe { FreeEnvironmentStringsW(own) };
    block.map(Block::Wide)
}

fn follow_all() -> bool {
    crate::config::HookConfig::global().follow
}

/// Whether this process's children get the hook.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| is_helper() || follow_all() || !package_roots().is_empty() || game_root().is_some() || unreal_root().is_some())
}

/// A folder as the comparisons here want it: lower case, backslashes, a trailing one,
/// no `\\?\` prefix.
fn norm_dir(s: &str) -> Option<String> {
    let mut s = norm_path(s);
    if s.is_empty() {
        return None;
    }
    if !s.ends_with('\\') {
        s.push('\\');
    }
    Some(s)
}

fn norm_path(s: &str) -> String {
    let s = s.trim().trim_matches('"').replace('/', "\\").to_ascii_lowercase();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// The game's install folder, as the launcher found it (`PEERPARTY_GAME_ROOT`).
fn game_root() -> Option<&'static str> {
    static ROOT: OnceLock<Option<String>> = OnceLock::new();
    ROOT.get_or_init(|| crate::config::HookConfig::global().game_root.as_deref().and_then(norm_dir)).as_deref()
}

/// Set once this process loads a game SDK: it is the game, so what it starts from then
/// on (a server, a tool) isn't followed, and a followed child may take the pipe.
static SDK_SEEN: AtomicBool = AtomicBool::new(false);

/// Whether a game SDK loaded in this process (so it is the game).
pub fn sdk_seen() -> bool {
    SDK_SEEN.load(Ordering::Relaxed)
}

/// Whether this process may still turn out to be only the game's starter: no game SDK
/// here, and a program it started (and peerparty followed) hasn't yet said whether it
/// is the game.
pub fn starter_pending() -> bool {
    !sdk_seen() && !crate::sys::lock(&GRANTS).is_empty()
}

/// A game SDK loaded here (watch.rs).
pub fn sdk_loaded(name: &str) {
    if !SDK_SEEN.swap(true, Ordering::Relaxed) && enabled() && !follow_all() && !is_helper() {
        log!("child: {name} is a game SDK, so this process is the game; programs it starts from now on aren't followed");
    }
}

/// The install folder, lower case with a trailing backslash, when this process is
/// Unreal's starter (see `is_unreal_starter_dir`).
fn unreal_root() -> Option<&'static str> {
    static ROOT: OnceLock<Option<String>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        if !is_unreal_starter_dir(dir) {
            return None;
        }
        let mut s = dir.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
        if !s.ends_with('\\') {
            s.push('\\');
        }
        log!("child: {} is Unreal's starter; the game it starts gets peerparty", exe.display());
        Some(s)
    })
    .as_deref()
}

/// Whether `dir` is an Unreal game's install folder holding its starter: an `Engine`
/// folder beside a `<Project>\Binaries\<Platform>\` folder with a `*-Shipping.exe` in
/// it. The game itself lives in that Binaries folder, so it never matches this.
fn is_unreal_starter_dir(dir: &std::path::Path) -> bool {
    if !dir.join("Engine").is_dir() {
        return false;
    }
    let Ok(projects) = std::fs::read_dir(dir) else { return false };
    projects.flatten().filter(|p| p.path().is_dir()).any(|project| {
        let Ok(platforms) = std::fs::read_dir(project.path().join("Binaries")) else { return false };
        platforms.flatten().any(|platform| {
            let Ok(files) = std::fs::read_dir(platform.path()) else { return false };
            files.flatten().any(|f| f.file_name().to_string_lossy().to_ascii_lowercase().ends_with("-shipping.exe"))
        })
    })
}

/// Whether this process only launches the game (GameLaunchHelper): nothing but child
/// following runs in it.
pub fn is_helper() -> bool {
    static HELPER: OnceLock<bool> = OnceLock::new();
    *HELPER.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()))
            .is_some_and(|n| n == "gamelaunchhelper.exe")
    })
}

/// Where this process's package is installed, lower case with a trailing backslash:
/// the package's path as Windows reports it, and the folder of the helper or starter
/// (`C:\Program Files\WindowsApps\<package>\`; an Xbox app install shows through there).
/// Empty for a process without package identity, and under Wine, which installs no
/// packages (its package functions are stubs; wine.rs).
fn package_roots() -> &'static [String] {
    static ROOTS: OnceLock<Vec<String>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        if crate::sys::wine::is_wine() {
            return Vec::new();
        }
        let k32: Vec<u16> = "kernel32.dll".encode_utf16().chain(Some(0)).collect();
        let Some(f) = (unsafe { GetProcAddress(GetModuleHandleW(k32.as_ptr()), c"GetCurrentPackagePath".as_ptr().cast()) }) else {
            return Vec::new();
        };
        let get: unsafe extern "system" fn(*mut u32, *mut u16) -> i32 = unsafe { std::mem::transmute(f) };
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        if unsafe { get(&mut len, buf.as_mut_ptr()) } != 0 {
            return Vec::new();
        }
        let norm = |s: String| {
            let mut s = s.to_ascii_lowercase().replace('/', "\\");
            if !s.ends_with('\\') {
                s.push('\\');
            }
            s
        };
        let mut roots = vec![norm(String::from_utf16_lossy(&buf[..len.saturating_sub(1) as usize]))];
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned())) {
            let dir = norm(dir);
            if !roots.iter().any(|r| dir.starts_with(r.as_str())) {
                roots.push(dir);
            }
        }
        roots
    })
}

/// A NUL-terminated UTF-16 string of any length up to 32K units (command lines are
/// often longer than read::wstr allows).
fn long_wstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while n < 32768 && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

/// The program a CreateProcess call starts: its application name, or what Windows reads
/// from its command line (`program_of`), in `dir` when relative.
fn image_of(app: *const u16, cmd: *const u16, dir: *const u16) -> String {
    let a = long_wstr(app);
    if !a.is_empty() {
        return full_image(&a, dir);
    }
    program_of(&long_wstr(cmd), &|candidate: &str| {
        let full = full_image(candidate, dir);
        let p = std::path::Path::new(&full);
        p.is_file() || (p.extension().is_none() && p.with_extension("exe").is_file())
    })
    .map(|p| full_image(&p, dir))
    .unwrap_or_default()
}

/// The program a command line starts, as CreateProcess reads one without an application
/// name: a quoted first word as it is; unquoted, the shortest run of its first words
/// (split at spaces and tabs) that names a file (`exists`), so `C:\Program Files\Game\
/// game.exe -x` is the game, not `C:\Program`. Else the first word. `.exe` is added
/// to a name without an extension, as Windows does.
fn program_of(line: &str, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    let line = line.trim_start_matches([' ', '\t']);
    if let Some(rest) = line.strip_prefix('"') {
        let p = rest.split('"').next().unwrap_or("");
        return (!p.is_empty()).then(|| with_exe(p));
    }
    let mut first = None;
    for (i, c) in line.char_indices().chain(std::iter::once((line.len(), ' '))) {
        if c == ' ' || c == '\t' {
            let candidate = &line[..i];
            if candidate.is_empty() {
                continue;
            }
            first.get_or_insert(candidate);
            if exists(candidate) {
                return Some(with_exe(candidate));
            }
        }
    }
    first.map(with_exe)
}

fn with_exe(p: &str) -> String {
    if std::path::Path::new(p).extension().is_none() { format!("{p}.exe") } else { p.to_string() }
}

/// Programs a game starts beside itself that are never the game: crash reporters and
/// Epic's helper. Following one gave it the overlay (a game can start its crash
/// reporter before it has a window), and some games end themselves when their crash
/// reporter is interfered with.
const NOT_GAMES: [&str; 17] = [
    "crashreportclient", "unitycrashhandler", "crashpad_handler", "epiconlineservices", "crashreporter", "crashsender", "crashhandler", "crashrpt",
    "bssndrpt", "bugsplat", "sentry_", "werfault", "steamerrorreporter", "backtrace", "bugreporter", "errorreporter", "minidump",
];

/// Programs that only show a page or a store inside a game (embedded browsers, store
/// and launcher helpers): a window of theirs can be game-sized, so following one could
/// hand it the overlay.
const HELPERS: [&str; 10] = [
    "cefsharp.browsersubprocess", "cefprocess", "qtwebengineprocess", "msedgewebview2", "steamwebhelper", "upc.exe", "uplaywebcore", "ubisoftconnect",
    "eaconnect", "eabackgroundservice",
];

/// Anti-cheat starters: the game refuses anti-cheat (watch.rs), and touching these is
/// what gets a player flagged.
const ANTICHEAT: [&str; 14] = [
    "start_protected_game", "easyanticheat", "eac_launcher", "battleye", "beservice", "beclient", "eac_", "gameguard", "gamemon", "xigncode",
    "xxd-", "vgc.exe", "vgtray", "ace-",
];

/// Installers of what a game needs (run on its first start by some stores and
/// starters), and uninstallers.
const INSTALLERS: [&str; 16] = [
    "vcredist", "vc_redist", "dxsetup", "dxwebsetup", "dotnetfx", "ndp4", "windowsdesktop-runtime", "dotnet-runtime", "physx", "oalinst",
    "ue4prereqsetup", "ue5prereqsetup", "ueprereqsetup", "ue3redist", "unins", "setup",
];

/// Folders inside a game's that hold installers or anti-cheat, never the game.
const NOT_GAME_DIRS: [&str; 12] = [
    "\\_commonredist\\", "\\redist\\", "\\redistributables\\", "\\directx\\", "\\prerequisites\\", "\\prereqs\\", "\\__installer\\", "\\easyanticheat\\",
    "\\battleye\\",
    "\\gameguard\\",
    "\\xigncode\\",
    "\\crashreporter\\",
];

/// Why a program is never followed, whatever folder it is in. `path` is `norm_path`'s.
fn never_followed(path: &str) -> Option<&'static str> {
    let file = path.rsplit('\\').next().unwrap_or(path);
    if NOT_GAMES.iter().any(|n| file.starts_with(n)) {
        return Some("a crash reporter or Epic's helper");
    }
    if HELPERS.iter().any(|n| file.starts_with(n)) {
        return Some("a browser or store helper");
    }
    if ANTICHEAT.iter().any(|n| file.starts_with(n)) || file.ends_with("_be.exe") {
        return Some("anti-cheat");
    }
    if INSTALLERS.iter().any(|n| file.starts_with(n)) {
        return Some("an installer");
    }
    if NOT_GAME_DIRS.iter().any(|d| path.contains(d)) {
        return Some("in a folder of installers or anti-cheat");
    }
    None
}

/// Whether a child started as `image` gets the hook, and if not, why.
fn follow_verdict(image: &str) -> Result<(), &'static str> {
    let path = norm_path(image);
    if is_peerparty(&path, crate::config::HookConfig::global().exe.as_deref().map(norm_path).as_deref()) {
        return Err("PeerParty itself");
    }
    verdict(&path, follow_all(), package_roots(), game_root().or(unreal_root()), SDK_SEEN.load(Ordering::Relaxed))
}

/// PeerParty's own program (the launcher's `PEERPARTY_EXE`, or any `peerparty.exe`): its
/// `--attach` helper, started from here for a child of the other bitness, mustn't be
/// followed in turn. `path` and `exe` are `norm_path`'s.
fn is_peerparty(path: &str, exe: Option<&str>) -> bool {
    exe.is_some_and(|e| e == path) || path.rsplit('\\').next() == Some("peerparty.exe")
}

fn verdict(path: &str, all: bool, packages: &[String], root: Option<&str>, sdk_seen: bool) -> Result<(), &'static str> {
    if let Some(why) = never_followed(path) {
        return Err(why);
    }
    if all {
        return Ok(());
    }
    if packages.iter().any(|r| path.starts_with(r.as_str())) {
        return Ok(());
    }
    match root {
        Some(root) if path.starts_with(root) => {
            if sdk_seen {
                Err("this process is the game (a game SDK loaded), so what it starts isn't")
            } else {
                Ok(())
            }
        }
        Some(_) => Err("outside the game's folder"),
        None => Err("not part of the game"),
    }
}

/// The program a CreateProcess call starts, as a full path where it can be: a relative
/// one is the starting folder's (or ours).
fn full_image(image: &str, dir: *const u16) -> String {
    let p = std::path::Path::new(image.trim_matches('"'));
    if p.is_absolute() || image.starts_with(r"\\") {
        return image.to_string();
    }
    // Where CreateProcess looks for a relative name: this program's folder, then the
    // current folder (the child's `dir` isn't searched, but is the best guess left).
    let bases: Vec<std::path::PathBuf> = [
        std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf())),
        std::env::current_dir().ok(),
        Some(long_wstr(dir)).filter(|d| !d.is_empty()).map(std::path::PathBuf::from),
    ]
    .into_iter()
    .flatten()
    .collect();
    let found = bases.iter().map(|b| b.join(p)).find(|f| f.is_file() || (f.extension().is_none() && f.with_extension("exe").is_file()));
    match found.or_else(|| bases.last().map(|b| b.join(p))) {
        Some(f) => f.to_string_lossy().into_owned(),
        None => image.to_string(),
    }
}
/// The event a parent makes for a child it followed; the child signals it when it turns
/// out to be the game.
fn follow_event(pid: u32) -> Vec<u16> {
    wide(&format!("Local\\peerparty-follow-{pid}"))
}

/// The event a parent sets for a child whose claim it granted: the child connects to the
/// launcher only then (or once the parent is gone), so of two children that both look
/// like the game, only the first gets the overlay.
fn grant_event(pid: u32) -> Vec<u16> {
    wide(&format!("Local\\peerparty-follow-{pid}-go"))
}

/// In the parent, before the child `pid` runs: the event it signals once it is the game
/// (and its grant, `grant_event`, made with it).
fn make_follow_event(pid: u32) -> Option<usize> {
    let grant = unsafe { windows_sys::Win32::System::Threading::CreateEventW(std::ptr::null(), 1, 0, grant_event(pid).as_ptr()) };
    if grant.is_null() {
        log!("child: no grant event for pid {pid}: {}", std::io::Error::last_os_error());
        return None;
    }
    crate::sys::lock(&GRANTS).push((pid, grant as usize));
    let event = unsafe { windows_sys::Win32::System::Threading::CreateEventW(std::ptr::null(), 1, 0, follow_event(pid).as_ptr()) };
    if event.is_null() {
        log!("child: no hand-over event for pid {pid}: {}", std::io::Error::last_os_error());
        // Nothing will hand over to this child, so nothing would ever take its grant
        // back: left listed, a starter would look pending for the whole run.
        crate::sys::lock(&GRANTS).retain(|&(p, _)| p != pid);
        unsafe { windows_sys::Win32::Foundation::CloseHandle(grant) };
        return None;
    }
    Some(event as usize)
}

/// In the parent: once the child `pid` signals `event` (it is the game), let the
/// launcher's pipe go to it. A child that ends without becoming the game ends the wait.
fn hand_over_when_claimed(event: usize, process: usize, pid: u32, image: &str) {
    use windows_sys::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, INFINITE, WaitForMultipleObjects};
    // Our own handle to the child: the caller may close the one CreateProcess gave it.
    let mut child: HANDLE = std::ptr::null_mut();
    let me = unsafe { GetCurrentProcess() };
    let dup = unsafe { DuplicateHandle(me, process as _, me, &mut child, 0, 0, DUPLICATE_SAME_ACCESS) } != 0;
    let child = if dup { child as usize } else { 0 };
    let image = image.to_string();
    let _ = std::thread::Builder::new().name("peerparty-follow".into()).spawn(move || {
        let handles: Vec<HANDLE> = [event, child].into_iter().filter(|&h| h != 0).map(|h| h as HANDLE).collect();
        let claimed = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) } == 0;
        let grant = {
            let mut g = crate::sys::lock(&GRANTS);
            let at = g.iter().position(|(p, _)| *p == pid);
            at.map(|i| g.remove(i).1)
        };
        if claimed {
            // Once: a second program that looks like the game too doesn't take the
            // overlay from the first.
            if GRANTED.swap(true, Ordering::SeqCst) || crate::ipc::handed_over() {
                log!("child: {image} (pid {pid}) looks like the game too, but the overlay went to another program already");
            } else {
                log!("child: {image} (pid {pid}) is the game: the launcher goes to it");
                give_up_lan();
                crate::ipc::hand_over();
                if let Some(g) = grant {
                    unsafe { windows_sys::Win32::System::Threading::SetEvent(g as _) };
                }
            }
        }
        for h in handles.into_iter().chain(grant.map(|g| g as HANDLE)) {
            unsafe { CloseHandle(h) };
        }
    });
}

/// A starter that started its LAN before the game it started said it is the game (it
/// waited long enough to take itself for the game: a menu before the game starts, with
/// internet play set up) lets it go: its player slot goes to the game, which waits for
/// it (core's `take_slot_from`), and its peers see it leave.
fn give_up_lan() {
    if sdk_seen() {
        return;
    }
    let Some(lan) = crate::net::lan_if_started() else { return };
    log!("child: this starter's LAN goes, so the game it started is this PC's player");
    lan.release_port_maps();
    lan.shutdown();
    lan.release_slot();
}

/// The process that started this one, opened to wait on (None if it is gone already).
fn parent_process() -> Option<usize> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, PROCESS_SYNCHRONIZE};
    #[repr(C)]
    struct BasicInfo {
        exit_status: isize,
        peb: usize,
        affinity: usize,
        priority: isize,
        pid: usize,
        parent: usize,
    }
    let ntdll = unsafe { GetModuleHandleW(wide("ntdll.dll").as_ptr()) };
    let f = unsafe { GetProcAddress(ntdll, c"NtQueryInformationProcess".as_ptr().cast()) }?;
    let query: unsafe extern "system" fn(isize, u32, *mut BasicInfo, u32, *mut u32) -> i32 = unsafe { std::mem::transmute(f) };
    let mut info: BasicInfo = unsafe { std::mem::zeroed() };
    if unsafe { query(GetCurrentProcess() as isize, 0, &mut info, std::mem::size_of::<BasicInfo>() as u32, std::ptr::null_mut()) } != 0 {
        return None;
    }
    // Query rights too: `claim_later` asks the handle for the parent's id, and
    // GetProcessId answers 0 on a handle that can only be waited on.
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
    let mut h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, info.parent as u32) };
    if h.is_null() {
        h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, info.parent as u32) };
    }
    (!h.is_null()).then_some(h as usize)
}

/// Whether this process shows a window the size of a game's (not a splash or a tray
/// helper's).
fn game_window() -> bool {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible};
    unsafe extern "system" fn each(w: HWND, found: LPARAM) -> i32 {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(w, &mut pid) };
        if pid != std::process::id() || unsafe { IsWindowVisible(w) } == 0 {
            return 1;
        }
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if unsafe { GetWindowRect(w, &mut r) } != 0 && r.right - r.left >= 640 && r.bottom - r.top >= 360 {
            unsafe { *(found as *mut bool) = true };
            return 0;
        }
        1
    }
    let mut found = false;
    unsafe { EnumWindows(Some(each), &mut found as *mut bool as LPARAM) };
    found
}

/// In a child a parent followed (it made our event): true, and the pipe to the launcher
/// waits until this process turns out to be the game (a game SDK, a game-sized window,
/// or the parent gone), then connects. False otherwise: connect now.
pub fn claim_later() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, follow_event(std::process::id()).as_ptr()) };
    if event.is_null() {
        return false;
    }
    let event = event as usize;
    let parent = parent_process();
    // A slot the starter holds is ours once this is the game (`give_up_lan` there).
    if let Some(p) = parent {
        let pid = unsafe { windows_sys::Win32::System::Threading::GetProcessId(p as _) };
        if pid != 0 {
            peerparty_core::take_slot_from(pid);
        }
    }
    log!("child: started by a starter that passed peerparty on; the overlay comes here once this is the game");
    let _ = std::thread::Builder::new().name("peerparty-claim".into()).spawn(move || {
        let why = loop {
            // A program this one started took the pipe first: nothing to claim.
            if crate::ipc::handed_over() {
                unsafe {
                    CloseHandle(event as _);
                    if let Some(p) = parent {
                        CloseHandle(p as _);
                    }
                }
                return;
            }
            if SDK_SEEN.load(Ordering::Relaxed) {
                break "a game SDK loaded";
            }
            if game_window() {
                break "it shows a game-sized window";
            }
            if parent.is_none_or(|p| unsafe { WaitForSingleObject(p as _, 0) } == 0) {
                break "the program that started it has ended";
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        };
        log!("child: this is the game ({why}): asking for the launcher's pipe");
        // The parent's answer: its grant (no other program took the overlay first), or
        // its end (then nothing else holds the pipe).
        let grant = unsafe { OpenEventW(SYNCHRONIZE, 0, grant_event(std::process::id()).as_ptr()) };
        unsafe {
            SetEvent(event as _);
            CloseHandle(event as _);
        }
        let granted = match (grant.is_null(), parent) {
            (true, _) | (_, None) => true,
            (false, Some(p)) => {
                let handles = [grant, p as _];
                let r = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 10_000) };
                r == 0 || r == 1
            }
        };
        unsafe {
            if !grant.is_null() {
                CloseHandle(grant);
            }
            if let Some(p) = parent {
                CloseHandle(p as _);
            }
        }
        if !granted {
            log!("child: the overlay went to another program; this one goes without it");
            return;
        }
        crate::ipc::start();
    });
    true
}

pub fn attach_kernelbase(exports: &[Export]) -> usize {
    let Some(e) = exports.iter().find(|e| e.name == "CreateProcessInternalW") else {
        log!("child: CreateProcessInternalW not exported");
        return 0;
    };
    with_lock(|| {
        let target: CreateInternal = unsafe { std::mem::transmute(e.addr) };
        match unsafe { GenericDetour::new(target, create_hook as CreateInternal) } {
            Ok(h) => {
                let _ = CREATE.set(h);
                usize::from(unsafe { CREATE.get().unwrap().enable() }.is_ok())
            }
            Err(err) => {
                log!("child: CreateProcessInternalW not hooked: {err}");
                0
            }
        }
    })
}

/// This DLL's own path (the child loads the same file).
fn own_path() -> Option<Vec<u16>> {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleExW,
    };
    let mut m = std::ptr::null_mut();
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            own_path as *const u16,
            &mut m,
        )
    };
    if ok == 0 {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0).then(|| {
        buf.truncate(n);
        buf.push(0);
        buf
    })
}

/// Whether a process is 32-bit (under WOW64); None if Windows won't say.
fn is_x86(process: usize) -> Option<bool> {
    use windows_sys::Win32::System::Threading::IsWow64Process2;
    let (mut proc_machine, mut native) = (0u16, 0u16);
    if unsafe { IsWow64Process2(process as _, &mut proc_machine, &mut native) } == 0 {
        return None;
    }
    Some(proc_machine == 0x014c)
}

/// A child of the other bitness: `peerparty.exe --attach` (the launcher, which the
/// game's environment names) injects the right hook into it, as Windows has it do for a
/// packaged game's first process.
fn attach_from_outside(pi: &ProcessInformation, keep_suspended: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let exe = crate::config::HookConfig::global().exe.clone().ok_or("a child of the other bitness, and no PEERPARTY_EXE to inject it")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--attach", "-p", &pi.pid.to_string(), "-tid", &pi.tid.to_string()]);
    if keep_suspended {
        cmd.arg("--keep-suspended");
    }
    // CREATE_NO_WINDOW.
    let status = cmd.creation_flags(0x0800_0000).status().map_err(|e| format!("running peerparty --attach: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("peerparty --attach failed ({status})")) }
}

/// Queues `LoadLibraryW(<this DLL>)` on the suspended child's main thread (a child of
/// our own bitness), or has the launcher inject the other hook (attach_from_outside).
fn inject(pi: &ProcessInformation, keep_suspended: bool) -> Result<(), String> {
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx};
    use windows_sys::Win32::System::Threading::QueueUserAPC;
    let child_x86 = is_x86(pi.process).ok_or("its bitness is unknown")?;
    if child_x86 != cfg!(target_arch = "x86") {
        return attach_from_outside(pi, keep_suspended);
    }
    let path = own_path().ok_or("own path unknown")?;
    let bytes: Vec<u8> = path.iter().flat_map(|u| u.to_le_bytes()).collect();
    let remote = unsafe { VirtualAllocEx(pi.process as _, std::ptr::null(), bytes.len(), MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
    if remote.is_null() || unsafe { WriteProcessMemory(pi.process as _, remote, bytes.as_ptr().cast(), bytes.len(), std::ptr::null_mut()) } == 0 {
        return Err(format!("writing the path failed: {}", std::io::Error::last_os_error()));
    }
    let k32: Vec<u16> = "kernel32.dll".encode_utf16().chain(Some(0)).collect();
    let load = unsafe { GetProcAddress(GetModuleHandleW(k32.as_ptr()), c"LoadLibraryW".as_ptr().cast()) }.ok_or("no LoadLibraryW")?;
    let apc: unsafe extern "system" fn(usize) = unsafe { std::mem::transmute(load) };
    if unsafe { QueueUserAPC(Some(apc), pi.thread as _, remote as usize) } == 0 {
        return Err(format!("QueueUserAPC failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

unsafe extern "system" fn create_hook(
    token: usize,
    app: *const u16,
    cmd: *mut u16,
    pa: usize,
    ta: usize,
    inherit: i32,
    flags: u32,
    env: usize,
    dir: *const u16,
    si: usize,
    pi: *mut ProcessInformation,
    new_token: usize,
) -> i32 {
    let real = CREATE.get().unwrap();
    // A process started while one is being followed (`attach_from_outside` starts
    // `peerparty.exe --attach`) is just started.
    thread_local!(static INSIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if INSIDE.with(|i| i.replace(true)) {
        return unsafe { real.call(token, app, cmd, pa, ta, inherit, flags, env, dir, si, pi, new_token) };
    }
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            INSIDE.with(|i| i.set(false));
        }
    }
    let _leave = Leave;
    let image = image_of(app, cmd, dir);
    if let Err(why) = follow_verdict(&image) {
        log!("child: {image} is left alone ({why})");
        // A program left alone gets no PeerParty variables: they mean nothing to it, and
        // the room code and pipe name shouldn't travel on in a crash report's copy of its
        // environment. PeerParty's own program keeps them (its `--attach` needs them).
        let ours = is_peerparty(&norm_path(&image), crate::config::HookConfig::global().exe.as_deref().map(norm_path).as_deref());
        if !ours && let Some(block) = scrubbed_env(env, flags & CREATE_UNICODE_ENVIRONMENT != 0) {
            let (env, flags) = match &block {
                Block::Wide(b) => (b.as_ptr() as usize, flags | CREATE_UNICODE_ENVIRONMENT),
                Block::Ansi(b) => (b.as_ptr() as usize, flags),
            };
            return unsafe { real.call(token, app, cmd, pa, ta, inherit, flags, env, dir, si, pi, new_token) };
        }
        return unsafe { real.call(token, app, cmd, pa, ta, inherit, flags, env, dir, si, pi, new_token) };
    }
    let asked_suspended = flags & CREATE_SUSPENDED != 0;
    let ok = unsafe { real.call(token, app, cmd, pa, ta, inherit, flags | CREATE_SUSPENDED, env, dir, si, pi, new_token) };
    if ok == 0 || pi.is_null() {
        return ok;
    }
    let info = unsafe { &*pi };
    // Made before the child can run (`--attach` may resume it). The helper has no pipe
    // to give (its game connects at once).
    let event = (!is_helper() && crate::ipc::enabled()).then(|| make_follow_event(info.pid)).flatten();
    match inject(info, asked_suspended) {
        Ok(()) => {
            log!("child: peerparty goes into {image} (pid {})", info.pid);
            if let Some(event) = event {
                hand_over_when_claimed(event, info.process, info.pid, &image);
            }
        }
        Err(e) => {
            if let Some(event) = event {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(event as _) };
            }
            log!("child: {image} (pid {}) started without peerparty: {e} (flags {flags:#x}, token {token:#x})", info.pid)
        }
    }
    if !asked_suspended {
        unsafe { windows_sys::Win32::System::Threading::ResumeThread(info.thread as _) };
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::{filter_block, is_peerparty, program_of, is_unreal_starter_dir, norm_dir, norm_path, verdict};
    use std::fs;

    fn follows(image: &str, root: &str, sdk_seen: bool) -> bool {
        verdict(&norm_path(image), false, &[], norm_dir(root).as_deref(), sdk_seen).is_ok()
    }

    #[test]
    fn an_unquoted_command_line_is_read_as_windows_reads_it() {
        let game = r"C:\Program Files\Some Game\Game.exe";
        let exists = |c: &str| c == game;
        assert_eq!(program_of(r"C:\Program Files\Some Game\Game.exe -windowed", &exists).as_deref(), Some(game));
        assert_eq!(program_of(r#""C:\Program Files\Some Game\Game.exe" -x"#, &|_| false).as_deref(), Some(game));
        assert_eq!(program_of("  Game -x", &|_| false).as_deref(), Some("Game.exe"), "the first word, with .exe");
        assert_eq!(program_of("Game.exe\t-x", &|_| false).as_deref(), Some("Game.exe"));
        assert_eq!(program_of("", &|_| false), None);
    }

    #[test]
    fn a_child_left_alone_gets_no_peerparty_variables() {
        let block: Vec<u16> = "A=1\0peerparty_room=PP1-x\0PEERPARTY_PIPE=p\0B=2\0\0".encode_utf16().collect();
        let out = unsafe { filter_block(block.as_ptr()) }.expect("ours were in it");
        assert_eq!(String::from_utf16(&out).unwrap(), "A=1\0B=2\0\0");
        let ansi = b"A=1\0B=2\0\0";
        assert!(unsafe { filter_block(ansi.as_ptr()) }.is_none(), "nothing to take out");
        let only = b"PEERPARTY_ROOM=x\0\0";
        assert_eq!(unsafe { filter_block(only.as_ptr()) }.unwrap(), b"\0\0");
    }

    #[test]
    fn never_follows_peerparty_itself() {
        let exe = norm_path(r"C:\Tools\PeerParty\peerparty.exe");
        assert!(is_peerparty(&exe, Some(&exe)));
        assert!(is_peerparty(&norm_path(r"C:\Games\X\PeerParty.exe"), None));
        assert!(is_peerparty(&norm_path(r"C:\Tools\pp-renamed.exe"), Some(&norm_path(r"c:\tools\PP-renamed.exe"))));
        assert!(!is_peerparty(&norm_path(r"C:\Games\X\game.exe"), Some(&exe)));
    }

    #[test]
    fn follows_the_games_own_programs_only() {
        let root = r"D:\Games\Some Game";
        assert!(follows(r"D:\Games\Some Game\Binaries\Win64\Game-Win64-Shipping.exe", root, false));
        assert!(follows(r"d:/games/some game/Game.exe", root, false));
        assert!(follows(r"\\?\D:\Games\Some Game\Game.exe", root, false));
        // Outside the folder, or a folder that only starts the same.
        assert!(!follows(r"D:\Games\Some Game 2\Game.exe", root, false));
        assert!(!follows(r"C:\Windows\System32\cmd.exe", root, false));
        // Once this process is the game, what it starts isn't followed.
        assert!(!follows(r"D:\Games\Some Game\Server.exe", root, true));
        // No folder: nothing.
        assert!(verdict(&norm_path(r"D:\Games\Some Game\Game.exe"), false, &[], None, false).is_err());
    }

    #[test]
    fn never_follows_anti_cheat_installers_or_crash_reporters() {
        let root = r"D:\Games\Some Game";
        for image in [
            r"D:\Games\Some Game\start_protected_game.exe",
            r"D:\Games\Some Game\EasyAntiCheat\EasyAntiCheat_EOS_Setup.exe",
            r"D:\Games\Some Game\Game_BE.exe",
            r"D:\Games\Some Game\BattlEye\BEService_x64.exe",
            r"D:\Games\Some Game\_CommonRedist\vcredist\2019\VC_redist.x64.exe",
            r"D:\Games\Some Game\Redist\something.exe",
            r"D:\Games\Some Game\Engine\Extras\Redist\en-us\UE4PrereqSetup_x64.exe",
            r"D:\Games\Some Game\DXSETUP.exe",
            r"D:\Games\Some Game\unins000.exe",
            r"D:\Games\Some Game\Engine\Binaries\Win64\CrashReportClient.exe",
            r"D:\Games\Some Game\UnityCrashHandler64.exe",
            r"D:\Games\Some Game\BsSndRpt64.exe",
            r"D:\Games\Some Game\CrashSender1403.exe",
            r"D:\Games\Some Game\CefSharp.BrowserSubprocess.exe",
            r"D:\Games\Some Game\QtWebEngineProcess.exe",
            r"D:\Games\Some Game\GameGuard\GameMon.des",
            r"D:\Games\Some Game\xigncode\x3.xem",
        ] {
            assert!(!follows(image, root, false), "{image}");
        }
        assert!(follows(r"D:\Games\Some Game\Sentry Tower.exe", root, false), "a game's own name");
        assert!(follows(r"D:\Games\Some Game\Upcoming.exe", root, false), "a game's own name");
        // Even when following everything.
        assert!(verdict(&norm_path(r"C:\x\start_protected_game.exe"), true, &[], None, false).is_err());
        assert!(verdict(&norm_path(r"C:\x\Game.exe"), true, &[], None, false).is_ok());
    }

    #[test]
    fn packaged_games_follow_their_package() {
        let packages = vec![norm_dir(r"C:\XboxGames\Some Game\Content").unwrap()];
        assert!(verdict(&norm_path(r"C:\XboxGames\Some Game\Content\Game.exe"), false, &packages, None, false).is_ok());
        assert!(verdict(&norm_path(r"C:\Windows\GamingServicesUI.exe"), false, &packages, None, false).is_err());
    }

    #[test]
    fn finds_unreal_starter_folders() {
        let base = std::env::temp_dir().join(format!("peerparty-unreal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let game = base.join("Game");
        fs::create_dir_all(game.join("Engine").join("Binaries")).unwrap();
        let bin = game.join("Proj").join("Binaries").join("Win64");
        fs::create_dir_all(&bin).unwrap();
        // No Shipping exe yet: not a starter's folder.
        assert!(!is_unreal_starter_dir(&game));
        fs::write(bin.join("Proj-Win64-Shipping.exe"), b"").unwrap();
        assert!(is_unreal_starter_dir(&game));
        // The game's own folder, and a folder without Engine, aren't.
        assert!(!is_unreal_starter_dir(&bin));
        fs::remove_dir_all(game.join("Engine")).unwrap();
        assert!(!is_unreal_starter_dir(&game));
        let _ = fs::remove_dir_all(&base);
    }
}
