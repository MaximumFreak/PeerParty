//! Waiting for the whole game, not just the program PeerParty started. A starter (a
//! launcher program in the game's folder, Unreal's bootstrap) starts the game and may end
//! at once; the hook follows the game (child.rs) and PeerParty keeps running until it
//! ends.
//!
//! The started program goes into a job object (no limits but letting a program leave it,
//! and closing it ends nothing), so every program it starts, and theirs, is known. Once
//! the first ends, the wait goes on for any still running from inside the game's folder;
//! the game is over when none is. Programs from elsewhere (a store's client the game
//! woke up) and crash reporters aren't waited for.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, SetEvent, TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
};

/// Programs that outlive a crash on purpose: not the game.
const NOT_WAITED: [&str; 4] = ["crashreportclient", "unitycrashhandler", "crashpad_handler", "crashreporter"];

/// How the game ended: the last of its programs to end.
#[derive(Clone, Debug)]
pub struct Ended {
    pub pid: u32,
    pub exe: PathBuf,
    pub code: Option<u32>,
    pub ran: Option<Duration>,
    /// The program PeerParty started ended before the game did.
    pub starter: Option<(u32, PathBuf, Option<u32>)>,
}

pub struct Tree {
    job: HANDLE,
    root: Option<String>,
}

// The job handle is only used by the waiting thread once it has it.
unsafe impl Send for Tree {}

impl Tree {
    /// A job for the game started from `root` (its folder; `None`: wait for the first
    /// program only).
    pub fn new(root: Option<&Path>) -> Option<Tree> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            SetInformationJobObject(job, JobObjectExtendedLimitInformation, &limits as *const _ as *const _, std::mem::size_of_val(&limits) as u32);
            Some(Tree { job, root: root.map(|r| lower(&r.to_string_lossy()) + "\\") })
        }
    }

    /// Puts a (still suspended) process in the job.
    pub fn add(&self, process: HANDLE) -> bool {
        unsafe { AssignProcessToJobObject(self.job, process) != 0 }
    }

    /// Waits on another thread for the game to end, starting with `first`. Returns an
    /// event set when it has, and where the result will be.
    pub fn watch(self, first: HANDLE, pid: u32, exe: PathBuf) -> (HANDLE, Arc<Mutex<Option<Ended>>>) {
        let done = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        let result = Arc::new(Mutex::new(None));
        let (out, first, ev) = (result.clone(), first as usize, done as usize);
        std::thread::spawn(move || {
            let ended = self.wait(first as HANDLE, pid, exe);
            *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(ended);
            unsafe { SetEvent(ev as HANDLE) };
        });
        (done, result)
    }

    fn wait(self, first: HANDLE, pid: u32, exe: PathBuf) -> Ended {
        let mut ended = unsafe {
            WaitForSingleObject(first, INFINITE);
            Ended { pid, code: exit_code(first), ran: super::launch::run_time(first), exe, starter: None }
        };
        let Some(root) = self.root.clone() else { return ended };
        let mut first_done = false;
        loop {
            let left: Vec<(u32, PathBuf, HANDLE)> = self.members().into_iter().filter_map(|p| self.waitable(p, &root)).collect();
            if left.is_empty() {
                break;
            }
            if !first_done {
                first_done = true;
                eprintln!(
                    "peerparty: {} ended; the game goes on in {}",
                    ended.exe.display(),
                    left.iter().map(|(p, e, _)| format!("{} (pid {p})", e.display())).collect::<Vec<_>>().join(", ")
                );
                ended.starter = Some((ended.pid, ended.exe.clone(), ended.code));
            }
            // At most 64 at once (MAXIMUM_WAIT_OBJECTS): more makes every wait fail, and
            // which one ended would never be seen. The rest are watched on later looks.
            let handles: Vec<HANDLE> = left.iter().take(64).map(|l| l.2).collect();
            // A program started meanwhile shows up on the next look.
            let r = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, 2000) };
            if r != WAIT_FAILED && (r as usize) < WAIT_OBJECT_0 as usize + handles.len() {
                let (p, e, h) = &left[(r - WAIT_OBJECT_0) as usize];
                ended.pid = *p;
                ended.exe = e.clone();
                ended.code = unsafe { exit_code(*h) };
                ended.ran = unsafe { super::launch::run_time(*h) };
            } else if r == WAIT_FAILED {
                std::thread::sleep(Duration::from_millis(500));
            }
            for (_, _, h) in left {
                unsafe { CloseHandle(h) };
            }
        }
        ended
    }

    /// The ids of the job's running programs.
    fn members(&self) -> Vec<u32> {
        members(self.job)
    }

    /// What closes the whole game later (the window's Close all), with handles of its own:
    /// `watch` takes the tree. `first` is the program PeerParty started.
    pub fn closer(&self, first: HANDLE) -> Closer {
        Closer { job: duplicate(self.job), first: duplicate(first) }
    }
}

/// Closes a game PeerParty started: its windows are asked to close, as their close
/// button would; whatever still runs after `CLOSE_WAIT` is ended, every program in the
/// job (a game stuck on a loading screen doesn't answer).
pub struct Closer {
    job: HANDLE,
    first: HANDLE,
}

// Its handles are only used by the one thread that closes.
unsafe impl Send for Closer {}

const CLOSE_WAIT: Duration = Duration::from_secs(6);

impl Closer {
    /// For a game PeerParty couldn't put in a job: only the program it started.
    pub fn process(first: HANDLE) -> Closer {
        Closer { job: std::ptr::null_mut(), first: duplicate(first) }
    }

    pub fn close(&self) {
        let asked = self.ask(0);
        eprintln!("peerparty: closing the game (Close all in PeerParty's window): {asked} window(s) asked to close");
        let until = std::time::Instant::now() + CLOSE_WAIT;
        while std::time::Instant::now() < until {
            if !self.runs() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        eprintln!("peerparty: the game still ran {}s after being asked to close: ended", CLOSE_WAIT.as_secs());
        self.force();
    }

    /// Asks the game's windows to close, as their close buttons would: the visible ones,
    /// and `known` (a split-screen copy's window, hidden while another pane is zoomed),
    /// which would otherwise be ended without the chance to save. How many were asked.
    pub fn ask(&self, known: u64) -> usize {
        let pids: Vec<u32> = if self.job.is_null() { Vec::new() } else { members(self.job) };
        ask_windows_to_close(&pids, self.first, known)
    }

    /// Whether any program of the game still runs.
    pub fn runs(&self) -> bool {
        let left = if self.job.is_null() { 0 } else { members(self.job).len() };
        left > 0 || (!self.first.is_null() && unsafe { WaitForSingleObject(self.first, 0) } != WAIT_OBJECT_0)
    }

    /// Ends every program of the game now, with exit code 1 (ended, not finished).
    pub fn force(&self) {
        unsafe {
            if !self.job.is_null() {
                TerminateJobObject(self.job, 1);
            }
            if !self.first.is_null() {
                TerminateProcess(self.first, 1);
            }
        }
    }
}

impl crate::host::StopCopy for Closer {
    fn ask(&self, known: u64) -> usize {
        Closer::ask(self, known)
    }

    fn force(&self) {
        Closer::force(self)
    }
}

impl Drop for Closer {
    fn drop(&mut self) {
        unsafe {
            for h in [self.job, self.first] {
                if !h.is_null() {
                    CloseHandle(h);
                }
            }
        }
    }
}

fn duplicate(h: HANDLE) -> HANDLE {
    let mut out = std::ptr::null_mut();
    unsafe {
        let me = GetCurrentProcess();
        if h.is_null() || DuplicateHandle(me, h, me, &mut out, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
            return std::ptr::null_mut();
        }
    }
    out
}

/// Posts WM_CLOSE to the visible top-level windows of `pids` (and of `first`'s
/// program), and to `known` (0: none) whether it shows or not, if it is one of theirs;
/// returns how many.
fn ask_windows_to_close(pids: &[u32], first: HANDLE, known: u64) -> usize {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE};
    let mut want: Vec<u32> = pids.to_vec();
    if !first.is_null() {
        let pid = unsafe { windows_sys::Win32::System::Threading::GetProcessId(first) };
        if pid != 0 && !want.contains(&pid) {
            want.push(pid);
        }
    }
    struct Ctx {
        want: Vec<u32>,
        known: u64,
        asked: usize,
    }
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> i32 {
        unsafe {
            let ctx = &mut *(lp as *mut Ctx);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let shown = IsWindowVisible(hwnd) != 0 || hwnd as u64 == ctx.known;
            if ctx.want.contains(&pid) && shown && PostMessageW(hwnd, WM_CLOSE, 0 as WPARAM, 0) != 0 {
                ctx.asked += 1;
            }
        }
        1
    }
    let mut ctx = Ctx { want, known, asked: 0 };
    unsafe { EnumWindows(Some(each), &mut ctx as *mut Ctx as LPARAM) };
    ctx.asked
}

/// The ids of a job's running programs.
fn members(job: HANDLE) -> Vec<u32> {
    // JOBOBJECT_BASIC_PROCESS_ID_LIST: two u32 counts, then the ids (ULONG_PTRs).
    const MAX: usize = 1024;
    let w = std::mem::size_of::<usize>();
    let mut buf = vec![0usize; 8 / w + MAX];
    let ok = unsafe { QueryInformationJobObject(job, JobObjectBasicProcessIdList, buf.as_mut_ptr().cast(), (buf.len() * w) as u32, std::ptr::null_mut()) };
    if ok == 0 {
        return Vec::new();
    }
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * w) };
    let listed = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    buf[8 / w..8 / w + listed.min(MAX)].iter().map(|&p| p as u32).collect()
}

impl Tree {

    /// A running program of the job worth waiting for: inside the game's folder, not a
    /// crash reporter.
    fn waitable(&self, pid: u32, root: &str) -> Option<(u32, PathBuf, HANDLE)> {
        unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let exe = image(h);
            let keep = exe.as_ref().is_some_and(|e| {
                let l = lower(&e.to_string_lossy());
                let file = l.rsplit('\\').next().unwrap_or(&l).to_string();
                l.starts_with(root) && !NOT_WAITED.iter().any(|n| file.starts_with(n))
            }) && WaitForSingleObject(h, 0) != WAIT_OBJECT_0;
            if !keep {
                CloseHandle(h);
                return None;
            }
            Some((pid, exe?, h))
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.job) };
    }
}

fn lower(s: &str) -> String {
    s.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
}

unsafe fn exit_code(h: HANDLE) -> Option<u32> {
    let mut code = 0u32;
    (unsafe { GetExitCodeProcess(h, &mut code) } != 0).then_some(code)
}

unsafe fn image(h: HANDLE) -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    if unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) } == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;

    /// Close all ends a game that has no window to ask (one stuck loading, or a console
    /// program here), every program in its job, once the wait is over.
    #[test]
    fn close_ends_what_doesnt_answer() {
        let tree = Tree::new(None).expect("a job");
        let mut cmd = std::process::Command::new("ping");
        cmd.args(["-n", "60", "127.0.0.1"]).stdout(std::process::Stdio::null());
        let mut child = crate::platform::quiet(&mut cmd).spawn().expect("ping starts");
        let h = child.as_raw_handle() as HANDLE;
        assert!(tree.add(h));
        let closer = tree.closer(h);
        let started = std::time::Instant::now();
        closer.close();
        let status = child.wait().expect("ended");
        assert!(!status.success(), "ended, not finished");
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(members(closer.job).is_empty());
    }
}
