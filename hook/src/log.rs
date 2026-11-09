//! One log file per game run: `<exe>-<yyyymmdd-hhmmss>-<pid>.log` in PEERPARTY_LOG_DIR
//! (set by the launcher), else a `logs` folder next to the DLL.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};

struct Log {
    file: Mutex<File>,
    start: Instant,
}

static LOG: OnceLock<Log> = OnceLock::new();
static CALLS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    /// Set while this thread is inside PeerParty code, so hooked calls made by our own
    /// logging (file writes, name lookups) aren't logged again.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

pub fn open(dll_dir: PathBuf, exe_name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("PEERPARTY_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dll_dir.join("logs"));
    let _ = std::fs::create_dir_all(&dir);
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    let name = format!(
        "{exe_name}-{:04}{:02}{:02}-{:02}{:02}{:02}-{}.log",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        unsafe { GetCurrentProcessId() }
    );
    let path = dir.join(name);
    let file = File::create(&path).ok()?;
    LOG.set(Log {
        file: Mutex::new(file),
        start: Instant::now(),
    })
    .ok()?;
    Some(path)
}

pub fn line(args: fmt::Arguments) {
    let Some(log) = LOG.get() else { return };
    let ms = log.start.elapsed().as_millis();
    let text = format!(
        "[{:>4}.{:03} t{:<5}] {args}\r\n",
        ms / 1000,
        ms % 1000,
        unsafe { GetCurrentThreadId() }
    );
    if let Ok(mut f) = log.file.lock() {
        let _ = f.write_all(text.as_bytes());
        // A spike log is only useful if a crash still leaves it on disk.
        let _ = f.flush();
    }
}

struct ClearBusy;
impl Drop for ClearBusy {
    fn drop(&mut self) {
        let _ = BUSY.try_with(|b| b.set(false));
    }
}

/// Runs `f` unless this thread is already inside PeerParty code.
/// The guard clears on panic so a later call on this thread is logged again.
pub fn guarded<R>(f: impl FnOnce() -> R) -> Option<R> {
    let entered = BUSY.try_with(|b| !b.replace(true)).unwrap_or(false);
    if !entered {
        return None;
    }
    let _clear = ClearBusy;
    Some(f())
}

/// Logs `body` for the first `first` hits of `key`, then at powers of two.
/// Skipped when this thread is already inside PeerParty (a log write re-entered a hook).
pub fn throttled(key: &str, first: u64, body: impl FnOnce(u64)) {
    guarded(|| {
        if let Some(n) = throttle(&CALLS, key, first) {
            body(n);
        }
    });
}

/// A call PeerParty doesn't answer itself (it reports unsupported, fails, answers with
/// an empty success, or the real service answers without its servers): logged once per
/// `what` as `gap: <layer> <what>`. The launcher sums these up when the game exits, so
/// a game's first run names what it needs that PeerParty lacks.
pub fn gap(layer: &str, what: &str) {
    static SEEN: LazyLock<Mutex<std::collections::HashSet<String>>> = LazyLock::new(Mutex::default);
    let key = format!("{layer} {what}");
    if SEEN.lock().is_ok_and(|mut s| s.insert(key.clone())) {
        crate::log!("gap: {key}");
    }
}

/// Counts occurrences of `key` and says whether this one should be logged: the first
/// `first` times, then at every power of two, so hot calls show their rate without
/// flooding the log.
pub fn throttle(map: &Mutex<HashMap<String, u64>>, key: &str, first: u64) -> Option<u64> {
    let mut m = map.lock().ok()?;
    let n = match m.get_mut(key) {
        Some(n) => {
            *n += 1;
            *n
        }
        None => {
            m.insert(key.to_owned(), 1);
            1
        }
    };
    (n <= first || n.is_power_of_two()).then_some(n)
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::line(format_args!($($t)*)) };
}
