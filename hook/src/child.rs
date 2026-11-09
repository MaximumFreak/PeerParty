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
//! for sign-in, which is left alone. On in Unreal's starter too (below), following only
//! programs inside the game's install folder. With `PEERPARTY_FOLLOW=1` (any game) every
//! child is followed. In the helper the hook does nothing else: no pipe to the launcher,
//! no LAN.
//!
//! A starter program (Unreal's `<Game>.exe` bootstrap, which starts
//! `<Game>/Binaries/<Platform>/<Game>-<Platform>-Shipping.exe`) has the pipe to the
//! launcher until it starts the game; then it hands it over (ipc.rs), so the overlay
//! follows the game. "Starter" means it has no visible window of its own when it starts
//! the child; a game that later starts a helper program of its own keeps the pipe.
//! Stores often name the bootstrap as the program to run (Epic's manifests do), so
//! without following it the game itself would run without PeerParty.

use std::sync::OnceLock;

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;

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

#[repr(C)]
pub struct ProcessInformation {
    process: usize,
    thread: usize,
    pid: u32,
    tid: u32,
}

const CREATE_SUSPENDED: u32 = 0x4;

fn follow_all() -> bool {
    std::env::var("PEERPARTY_FOLLOW").is_ok_and(|v| v == "1")
}

/// Whether this process's children get the hook.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| is_helper() || follow_all() || !package_roots().is_empty() || unreal_root().is_some())
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
/// Empty for a process without package identity.
fn package_roots() -> &'static [String] {
    static ROOTS: OnceLock<Vec<String>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
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

/// The program a CreateProcess call starts: its application name, or the first word of
/// its command line.
fn image_of(app: *const u16, cmd: *const u16) -> String {
    let a = long_wstr(app);
    if !a.is_empty() {
        return a;
    }
    let line = long_wstr(cmd);
    let line = line.trim_start();
    match line.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or("").to_string(),
        None => line.split(' ').next().unwrap_or("").to_string(),
    }
}

/// Programs a game starts beside itself that are never the game: crash reporters and
/// Epic's helper. Following one gave it the overlay (a game can start its crash
/// reporter before it has a window), and some games end themselves when their crash
/// reporter is interfered with.
const NOT_GAMES: [&str; 5] = ["crashreportclient", "unitycrashhandler", "crashpad_handler", "epiconlineservices", "crashreporter"];

/// Whether a child started as `image` gets the hook.
fn should_follow(image: &str) -> bool {
    let file = image.rsplit(['\\', '/']).next().unwrap_or(image).to_ascii_lowercase();
    if NOT_GAMES.iter().any(|n| file.starts_with(n)) {
        return false;
    }
    if follow_all() {
        return true;
    }
    let image = image.to_ascii_lowercase().replace('/', "\\");
    package_roots().iter().map(String::as_str).chain(unreal_root()).any(|r| image.starts_with(r))
}

/// Whether this process has a visible top-level window (a game, not a starter).
fn has_window() -> bool {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};
    unsafe extern "system" fn each(w: HWND, found: LPARAM) -> i32 {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(w, &mut pid) };
        if pid == std::process::id() && unsafe { IsWindowVisible(w) } != 0 {
            unsafe { *(found as *mut bool) = true };
            return 0;
        }
        1
    }
    let mut found = false;
    unsafe { EnumWindows(Some(each), &mut found as *mut bool as LPARAM) };
    found
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
    let exe = std::env::var_os("PEERPARTY_EXE").ok_or("a child of the other bitness, and no PEERPARTY_EXE to inject it")?;
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
    let image = image_of(app, cmd);
    if !should_follow(&image) {
        log!("child: {image} is left alone (not part of the game)");
        return unsafe { real.call(token, app, cmd, pa, ta, inherit, flags, env, dir, si, pi, new_token) };
    }
    let asked_suspended = flags & CREATE_SUSPENDED != 0;
    let ok = unsafe { real.call(token, app, cmd, pa, ta, inherit, flags | CREATE_SUSPENDED, env, dir, si, pi, new_token) };
    if ok == 0 || pi.is_null() {
        return ok;
    }
    let info = unsafe { &*pi };
    match inject(info, asked_suspended) {
        Ok(()) => {
            log!("child: peerparty goes into {image} (pid {})", info.pid);
            if !is_helper() && !has_window() {
                crate::ipc::hand_over();
            }
        }
        Err(e) => log!("child: {image} (pid {}) started without peerparty: {e} (flags {flags:#x}, token {token:#x})", info.pid),
    }
    if !asked_suspended {
        unsafe { windows_sys::Win32::System::Threading::ResumeThread(info.thread as _) };
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::is_unreal_starter_dir;
    use std::fs;

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
