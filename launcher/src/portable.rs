//! Portable mode: PeerParty's files beside peerparty.exe.
//!
//! A `peerparty-data` folder next to the exe is PeerParty's folder in place of
//! `%LOCALAPPDATA%\peerparty` (`~/.local/share/peerparty` on Linux): the window's
//! settings, the players' identities, the stand-ins' saves, the hook DLLs it writes out.
//! Making the folder by hand is enough. `init` points `PEERPARTY_DATA_DIR` at it first
//! thing, so everything this process uses or starts follows: core, the window, the games
//! and their hooks (they get this process's environment, launch.rs), and on Linux the
//! Windows peerparty.exe inside Proton (given the folder as Wine sees it,
//! backend/linux/launch.rs). Logs stay in the temp folder, as they always do.
//!
//! Settings > About switches between the two (`switch`): with no game running from the
//! folder (`in_use`), it is renamed aside (on Windows that fails while any file in it is
//! open, so a game started from outside the window is caught too), copied to the other
//! place under a `.part` name, and the copy renamed into place; any step failing puts it
//! back as it was. The window then starts again with `--moved`. The hook DLLs aren't
//! copied: they are written out again when a game next needs them.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use peerparty_protocol::env as penv;

/// The folder beside the exe that makes PeerParty portable.
pub const FOLDER: &str = "peerparty-data";

/// Where this process's folder came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The system's usual place.
    Usual,
    /// `peerparty-data` beside the exe.
    Portable,
    /// Another folder `PEERPARTY_DATA_DIR` named (tests): nothing to switch.
    Named,
}

static MODE: OnceLock<Mode> = OnceLock::new();
/// The folder this process moved PeerParty's files to: its writes until it quits go
/// there, never to a new copy of the folder they left (which, beside the exe, would
/// make the next start portable again).
static MOVED_TO: OnceLock<PathBuf> = OnceLock::new();

/// `peerparty-data` beside this exe.
pub fn folder() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(FOLDER))
}

fn named() -> Option<PathBuf> {
    std::env::var_os(penv::DATA_DIR).map(PathBuf::from).filter(|p| p.is_absolute())
}

/// Picks this process's folder; main calls it first, before any thread starts.
pub fn init() {
    let mode = match (named(), folder()) {
        // Given by the window or a peerparty that found it (a game's peerparty.exe).
        (Some(n), Some(f)) if n == f => Mode::Portable,
        (Some(_), _) => Mode::Named,
        (None, Some(f)) if f.is_dir() => {
            // SAFETY: main calls this before any thread starts, so nothing else reads or
            // writes the environment meanwhile.
            unsafe { std::env::set_var(penv::DATA_DIR, &f) };
            Mode::Portable
        }
        (None, _) => Mode::Usual,
    };
    let _ = MODE.set(mode);
}

pub fn mode() -> Mode {
    MODE.get().copied().unwrap_or(Mode::Usual)
}

pub fn moved_to() -> Option<PathBuf> {
    MOVED_TO.get().cloned()
}

/// A line for the launcher's messages (and so the game's log): where PeerParty's files
/// are, when they aren't in the usual place.
pub fn describe(dir: &Path) -> Option<String> {
    match mode() {
        Mode::Usual => None,
        Mode::Portable => Some(format!("peerparty: portable: PeerParty's files are in {} (the {FOLDER} folder beside peerparty.exe)", dir.display())),
        Mode::Named => Some(format!("peerparty: PeerParty's files are in {} ({})", dir.display(), penv::DATA_DIR)),
    }
}

/// Why the files can't move: a program still running from the folder (its identity slot's
/// owner file names it, core's id.rs), by its file name.
pub fn in_use(dir: &Path) -> Option<String> {
    let me = std::process::id();
    for e in std::fs::read_dir(dir.join("identity")).into_iter().flatten().flatten() {
        if !e.file_name().to_string_lossy().ends_with(".owner") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let mut lines = text.lines();
        let (Some(pid), Some(exe)) = (lines.next().and_then(|p| p.trim().parse::<u32>().ok()), lines.next()) else { continue };
        let exe = Path::new(exe.trim());
        // A process id may since belong to another program.
        let running = pid != me && crate::platform::process_exe(pid).is_some_and(|p| same_file_name(&p, exe));
        if running {
            return Some(exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| exe.display().to_string()));
        }
    }
    None
}

fn same_file_name(a: &Path, b: &Path) -> bool {
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
    a == b || (name(a).is_some() && name(a) == name(b))
}

/// Why `switch` didn't move anything.
#[derive(Debug)]
pub enum SwitchError {
    /// A program still runs from the folder.
    InUse(String),
    /// A step failed (nothing changed): the folder it was going to, and why.
    Failed(PathBuf, String),
}

/// Moves PeerParty's files beside the exe (`on`) or back to the usual place, and returns
/// where they are now. An error means nothing changed. The caller starts the window
/// again (`restart`): this process's environment still names the old folder.
pub fn switch(on: bool) -> Result<PathBuf, SwitchError> {
    let from = crate::platform::data_dir();
    let to = if on { folder().ok_or_else(|| SwitchError::Failed(PathBuf::from(FOLDER), "can't tell where peerparty.exe is".into()))? } else { crate::platform::usual_data_dir() };
    if from == to {
        return Ok(to);
    }
    if let Some(program) = in_use(&from) {
        eprintln!("peerparty: portable: {} still runs from {}: nothing moved", program, from.display());
        return Err(SwitchError::InUse(program));
    }
    match move_dir(&from, &to) {
        Ok(()) => {
            eprintln!("peerparty: portable: moved PeerParty's files from {} to {}", from.display(), to.display());
            let _ = MOVED_TO.set(to.clone());
            Ok(to)
        }
        Err(e) => {
            eprintln!("peerparty: portable: moving {} to {} failed ({e}): nothing moved", from.display(), to.display());
            Err(SwitchError::Failed(to, e.to_string()))
        }
    }
}

/// Starts the window again (`--moved`), finding the folder afresh.
pub fn restart() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--moved");
    if mode() == Mode::Portable {
        cmd.env_remove(penv::DATA_DIR);
    }
    cmd.spawn().map(drop)
}

fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    dir.with_file_name(name)
}

/// Folders not copied: written out again when needed.
const SKIP: &[&str] = &["hooks"];

fn copy_dir(from: &Path, to: &Path, top: bool) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        if top && SKIP.iter().any(|s| name.to_string_lossy().eq_ignore_ascii_case(s)) {
            continue;
        }
        let dest = to.join(&name);
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dest, false)?;
        } else {
            std::fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

/// Moves `from`'s files to `to`, all or nothing. A folder already at `to` (one left
/// from before) is kept aside as `<name>.old`, replacing an earlier one.
fn move_dir(from: &Path, to: &Path) -> io::Result<()> {
    if !from.exists() {
        // Nothing written yet: an empty folder is all there is to move.
        return std::fs::create_dir_all(to);
    }
    // Renamed aside first: on Windows this fails while a game has a file in it open.
    let aside = sibling(from, &format!(".moving-{}", std::process::id()));
    std::fs::rename(from, &aside).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", from.display())))?;
    let back = |e: io::Error| {
        let _ = std::fs::rename(&aside, from);
        e
    };
    let part = sibling(to, ".part");
    let _ = std::fs::remove_dir_all(&part);
    if let Err(e) = copy_dir(&aside, &part, true) {
        let _ = std::fs::remove_dir_all(&part);
        return Err(back(e));
    }
    let old = sibling(to, ".old");
    if to.exists() {
        let _ = std::fs::remove_dir_all(&old);
        if let Err(e) = std::fs::rename(to, &old) {
            let _ = std::fs::remove_dir_all(&part);
            return Err(back(e));
        }
        eprintln!("peerparty: portable: {} was there already; kept as {}", to.display(), old.display());
    }
    if let Err(e) = std::fs::rename(&part, to) {
        let _ = std::fs::rename(&old, to);
        let _ = std::fs::remove_dir_all(&part);
        return Err(back(e));
    }
    // Left behind if it can't all go: under that name it's never used.
    if let Err(e) = std::fs::remove_dir_all(&aside) {
        eprintln!("peerparty: portable: couldn't delete {} ({e}); it isn't used", aside.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("peerparty-portable-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(&r).unwrap();
        r
    }

    #[test]
    fn moves_everything_but_the_hooks_and_back_again() {
        let r = root("move");
        let (usual, portable) = (r.join("peerparty"), r.join(FOLDER));
        std::fs::create_dir_all(usual.join("identity")).unwrap();
        std::fs::create_dir_all(usual.join("hooks").join("0123")).unwrap();
        std::fs::write(usual.join("ui.json"), "{}").unwrap();
        std::fs::write(usual.join("identity").join("slot-0.ed25519"), "key").unwrap();
        std::fs::write(usual.join("hooks").join("0123").join("peerparty_hook.dll"), "dll").unwrap();
        // A stale half copy is replaced, not merged into.
        std::fs::create_dir_all(sibling(&portable, ".part")).unwrap();
        std::fs::write(sibling(&portable, ".part").join("stale"), "").unwrap();

        move_dir(&usual, &portable).unwrap();
        assert!(!usual.exists());
        assert_eq!(std::fs::read_to_string(portable.join("identity").join("slot-0.ed25519")).unwrap(), "key");
        assert!(portable.join("ui.json").is_file());
        assert!(!portable.join("hooks").exists(), "the hook DLLs are written out again, not copied");
        assert!(!portable.join("stale").exists());
        assert!(!sibling(&portable, ".part").exists());

        // Back, over a folder left in the usual place since: kept aside, not merged.
        std::fs::create_dir_all(&usual).unwrap();
        std::fs::write(usual.join("left"), "").unwrap();
        move_dir(&portable, &usual).unwrap();
        assert!(!portable.exists(), "a portable folder left behind would be used again");
        assert!(usual.join("ui.json").is_file());
        assert!(!usual.join("left").exists());
        assert!(sibling(&usual, ".old").join("left").is_file());
        let leftovers: Vec<_> = std::fs::read_dir(&r).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.contains(".moving") || n.contains(".part")).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn nothing_written_yet_moves_as_an_empty_folder() {
        let r = root("empty");
        move_dir(&r.join("peerparty"), &r.join(FOLDER)).unwrap();
        assert!(r.join(FOLDER).is_dir());
        let _ = std::fs::remove_dir_all(&r);
    }

    #[cfg(windows)]
    #[test]
    fn a_file_held_open_moves_nothing() {
        use std::os::windows::fs::OpenOptionsExt;
        let r = root("held");
        let (usual, portable) = (r.join("peerparty"), r.join(FOLDER));
        std::fs::create_dir_all(usual.join("identity")).unwrap();
        // As a game holds its identity slot: shared with nobody.
        let held = std::fs::OpenOptions::new().create(true).truncate(false).write(true).share_mode(0).open(usual.join("identity").join("slot-0.lock")).unwrap();
        assert!(move_dir(&usual, &portable).is_err());
        assert!(usual.join("identity").is_dir(), "the folder is where it was");
        assert!(!portable.exists());
        drop(held);
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn a_slot_owned_by_a_running_program_is_in_use() {
        let r = root("owner");
        std::fs::create_dir_all(r.join("identity")).unwrap();
        assert_eq!(in_use(&r), None);
        // This process is the window itself: not a game.
        let me = std::env::current_exe().unwrap();
        std::fs::write(r.join("identity").join("slot-0.owner"), format!("{}\r\n{}\r\n", std::process::id(), me.display())).unwrap();
        assert_eq!(in_use(&r), None);
        // A process that's gone.
        std::fs::write(r.join("identity").join("slot-1.owner"), format!("{}\r\nC:\\Games\\Lantern.exe\r\n", u32::MAX - 3)).unwrap();
        assert_eq!(in_use(&r), None);
        // A running one, named as it was started (a stand-in for a game: no window).
        let mut cmd = if cfg!(windows) { std::process::Command::new("ping") } else { std::process::Command::new("sleep") };
        if cfg!(windows) {
            cmd.args(["-n", "30", "127.0.0.1"]);
        } else {
            cmd.arg("30");
        }
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let mut child = crate::platform::quiet(&mut cmd).spawn().unwrap();
        let exe = crate::platform::process_exe(child.id()).unwrap();
        std::fs::write(r.join("identity").join("slot-2.owner"), format!("{}\r\n{}\r\n", child.id(), exe.display())).unwrap();
        let named = in_use(&r);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(named, exe.file_name().map(|n| n.to_string_lossy().into_owned()));
        let _ = std::fs::remove_dir_all(&r);
    }
}
