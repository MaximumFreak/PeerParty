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

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetExitCodeProcess, INFINITE, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
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
            Ended { pid, code: exit_code(first), ran: crate::run_time(first), exe, starter: None }
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
            let handles: Vec<HANDLE> = left.iter().map(|l| l.2).collect();
            // A program started meanwhile shows up on the next look.
            let r = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, 2000) };
            if r != WAIT_FAILED && (r as usize) < WAIT_OBJECT_0 as usize + handles.len() {
                let (p, e, h) = &left[(r - WAIT_OBJECT_0) as usize];
                ended.pid = *p;
                ended.exe = e.clone();
                ended.code = unsafe { exit_code(*h) };
                ended.ran = unsafe { crate::run_time(*h) };
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
        // JOBOBJECT_BASIC_PROCESS_ID_LIST: two u32 counts, then the ids (ULONG_PTRs).
        const MAX: usize = 1024;
        let w = std::mem::size_of::<usize>();
        let mut buf = vec![0usize; 8 / w + MAX];
        let ok = unsafe { QueryInformationJobObject(self.job, JobObjectBasicProcessIdList, buf.as_mut_ptr().cast(), (buf.len() * w) as u32, std::ptr::null_mut()) };
        if ok == 0 {
            return Vec::new();
        }
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * w) };
        let listed = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        buf[8 / w..8 / w + listed.min(MAX)].iter().map(|&p| p as u32).collect()
    }

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
