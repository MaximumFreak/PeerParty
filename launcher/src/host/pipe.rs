//! The launcher's end of the pipe to the hook in the game (hook/src/ipc.rs): two
//! one-way pipes, `<name>-up` (the hook's messages) and `<name>-down` (our commands),
//! one line of JSON per message. The game's process gets the name in `PEERPARTY_PIPE`.
//!
//! More than one process can connect over a run, one after another: a starter program
//! (Unreal's bootstrap exe, a packaged game's first program) connects, then hands over
//! to the game it starts (hook/src/child.rs). Each connection gets a fresh pair of pipe
//! instances; messages from all of them arrive on `rx` (the state names its pid), and
//! commands go to the newest.
//!
//! The pipes are this PC's and this run's alone: their name ends in random digits (the
//! game's process gets it in its environment), the first instance of each must be the
//! first pipe of that name (another program can't have made one for the game to connect
//! to instead), and other PCs can't connect to them.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};

pub struct Pipe {
    pub name: String,
    pub rx: Receiver<serde_json::Value>,
    tx: Sender<String>,
}

impl Pipe {
    /// Creates the first pair of pipes (so the name exists before the game starts) and
    /// waits for the hook in background threads.
    pub fn create() -> Option<Pipe> {
        let name = pipe_name(std::process::id(), &peerparty_core::inet::crypto::random_array::<8>());
        let first = (make(&format!("{name}-up"), PIPE_ACCESS_INBOUND, true)?, make(&format!("{name}-down"), PIPE_ACCESS_OUTBOUND, true)?);
        let (in_tx, rx) = mpsc::channel();
        let (tx, out_rx) = mpsc::channel::<String>();
        let current: Arc<Mutex<Option<std::fs::File>>> = Arc::new(Mutex::new(None));
        let listen_name = name.clone();
        let listen_current = current.clone();
        std::thread::Builder::new()
            .name("peerparty-pipe-listen".into())
            .spawn(move || listen(listen_name, first, in_tx, listen_current))
            .ok()?;
        std::thread::Builder::new()
            .name("peerparty-pipe-out".into())
            .spawn(move || writer(current, out_rx))
            .ok()?;
        Some(Pipe { name, rx, tx })
    }

    pub fn send(&self, msg: serde_json::Value) {
        let _ = self.tx.send(msg.to_string());
    }
}

/// The pipes' base name for a run: the launcher's process id (for whoever reads a log)
/// and digits nobody else can guess.
fn pipe_name(pid: u32, random: &[u8]) -> String {
    let digits: String = random.iter().map(|b| format!("{b:02x}")).collect();
    format!(r"\\.\pipe\peerparty-{pid}-{digits}")
}

/// A raw pipe handle as a usize, so it can move to its thread. `first`: fails if a pipe
/// of that name exists already (later instances join the one we made).
fn make(name: &str, access: u32, first: bool) -> Option<usize> {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let open = access | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    let mode = PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let h = unsafe { CreateNamedPipeW(wide.as_ptr(), open, mode, PIPE_UNLIMITED_INSTANCES, 65536, 65536, 0, std::ptr::null()) };
    (h != INVALID_HANDLE_VALUE).then_some(h as usize)
}

fn connect(h: usize) -> std::fs::File {
    // Fails with ERROR_PIPE_CONNECTED when the client was quicker; that's connected too.
    unsafe { ConnectNamedPipe(h as _, std::ptr::null_mut()) };
    unsafe { std::fs::File::from_raw_handle(h as _) }
}

/// Accepts connections one after another, each on a new pair of instances.
fn listen(name: String, first: (usize, usize), tx: Sender<serde_json::Value>, current: Arc<Mutex<Option<std::fs::File>>>) {
    let mut pair = Some(first);
    loop {
        let Some((up, down)) = pair.take().or_else(|| Some((make(&format!("{name}-up"), PIPE_ACCESS_INBOUND, false)?, make(&format!("{name}-down"), PIPE_ACCESS_OUTBOUND, false)?))) else {
            return;
        };
        // The hook opens -up, then -down.
        let up = connect(up);
        let down = connect(down);
        *current.lock().unwrap() = Some(down);
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("peerparty-pipe-in".into()).spawn(move || reader(up, tx));
    }
}

fn reader(file: std::fs::File, tx: Sender<serde_json::Value>) {
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if let Ok(v) = serde_json::from_str(&line) {
            if tx.send(v).is_err() {
                break;
            }
        }
    }
}

fn writer(current: Arc<Mutex<Option<std::fs::File>>>, rx: Receiver<String>) {
    for msg in rx {
        let mut cur = current.lock().unwrap();
        if let Some(file) = cur.as_mut() {
            if file.write_all(msg.as_bytes()).and_then(|_| file.write_all(b"\n")).is_err() {
                *cur = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipe_name_is_taken_once() {
        let random = peerparty_core::inet::crypto::random_array::<8>;
        let name = pipe_name(std::process::id(), &random());
        assert!(name.starts_with(r"\\.\pipe\peerparty-") && name.ends_with(|c: char| c.is_ascii_hexdigit()), "{name}");
        assert_ne!(name, pipe_name(std::process::id(), &random()), "two runs, two names");
        let close = |h: usize| unsafe { windows_sys::Win32::Foundation::CloseHandle(h as _) };
        let up = format!("{name}-up");
        let a = make(&up, PIPE_ACCESS_INBOUND, true).expect("the first instance");
        // Someone else claiming to be first is refused; another instance of ours isn't.
        assert!(make(&up, PIPE_ACCESS_INBOUND, true).is_none());
        let b = make(&up, PIPE_ACCESS_INBOUND, false).expect("a second instance");
        close(a);
        close(b);
    }
}
