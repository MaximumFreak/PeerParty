//! The launcher's end of the control connection to the hook in the game (hook/src/ipc.rs):
//! one line of JSON per message (`peerparty_protocol::ipc`: `HookMsg` up, `HostMsg`
//! down), over one of two transports (`IpcEndpoint`):
//!
//! - Named pipes, the default: two one-way pipes, `<name>-up` (the hook's messages) and
//!   `<name>-down` (our commands). The game's process gets the name in `PEERPARTY_PIPE`.
//! - Loopback TCP with a token (`create_tcp`), for where a named pipe can't reach (a
//!   Linux launcher and a game under Wine), asked for with `PEERPARTY_IPC=tcp` in the
//!   launcher's environment. The game's process gets it in `PEERPARTY_IPC`.
//!
//! More than one process can connect over a run, one after another: a starter program
//! (Unreal's bootstrap exe, a packaged game's first program) connects, then hands over
//! to the game it starts (hook/src/child.rs). Each connection gets a fresh pair of pipe
//! instances (or its own socket); messages from all of them arrive on `rx` (the state
//! names its pid), and commands go to the newest.
//!
//! The pipes are this PC's and this run's alone: their name ends in random digits (the
//! game's process gets it in its environment), the first instance of each must be the
//! first pipe of that name (another program can't have made one for the game to connect
//! to instead), and other PCs can't connect to them. Only this user's processes (and
//! the system) may open them (`owner_only`), and a line longer than MAX_HOOK_LINE ends
//! the connection rather than growing without end. The hook checks that the pipe's
//! server is the launcher the name says (its pid). The TCP transport listens on loopback
//! only and keeps a connection only once it has sent the run's token (transport.rs).

use std::io::{BufReader, Read, Write};
#[cfg(windows)]
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use peerparty_protocol::ipc::{HookMsg, HostMsg, MAX_HOOK_LINE, read_line_capped};
use peerparty_protocol::json::Json;
#[cfg(windows)]
use peerparty_protocol::transport::pipe_name;
use peerparty_protocol::transport::{IpcEndpoint, IpcStreams, TcpIpcServer, token_from_bytes};
#[cfg(windows)]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND};
#[cfg(windows)]
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};

#[cfg(windows)]
use crate::backend::windows::owner_only;

/// Where commands go: the newest connection's writing end.
type Current = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

pub struct Pipe {
    /// Where the hook connects: it goes in the game's environment, in `env_key`.
    pub endpoint: IpcEndpoint,
    pub rx: Receiver<HookMsg>,
    tx: Sender<String>,
    #[cfg(unix)]
    current: Current,
}

/// Commands to the hook from another thread than the one reading `rx` (Linux's
/// `--invite`, backend/linux/control.rs).
#[cfg(unix)]
pub struct Commands {
    tx: Sender<String>,
    current: Current,
}

#[cfg(unix)]
impl Commands {
    /// Sends `msg` to the newest connection; whether there is one.
    pub fn send(&self, msg: &HostMsg) -> bool {
        let connected = self.current.lock().unwrap().is_some();
        connected && self.tx.send(msg.to_line()).is_ok()
    }
}

impl Pipe {
    /// The connection for a game: on Windows, loopback TCP when the game's
    /// `PEERPARTY_IPC` (`ipc`) asks for it (`tcp`: where the game can't reach a named
    /// pipe), else named pipes; elsewhere (a game under Wine) loopback TCP.
    pub fn for_game(ipc: Option<&std::ffi::OsStr>) -> Option<Pipe> {
        #[cfg(windows)]
        {
            let tcp = ipc.is_some_and(|v| v.to_string_lossy().trim().eq_ignore_ascii_case("tcp"));
            if tcp { Pipe::create_tcp().or_else(Pipe::create) } else { Pipe::create() }
        }
        #[cfg(not(windows))]
        {
            let _ = ipc;
            Pipe::create_tcp()
        }
    }

    /// Creates the first pair of named pipes (so the name exists before the game starts)
    /// and waits for the hook in background threads.
    #[cfg(windows)]
    pub fn create() -> Option<Pipe> {
        let name = pipe_name(std::process::id(), &peerparty_core::inet::crypto::random_array::<8>());
        let first = (make(&format!("{name}-up"), PIPE_ACCESS_INBOUND, true)?, make(&format!("{name}-down"), PIPE_ACCESS_OUTBOUND, true)?);
        let endpoint = IpcEndpoint::WinPipe { name: name.clone() };
        Pipe::start(endpoint, move |tx, current| listen(name, first, tx, current))
    }

    /// Listens on a free port of 127.0.0.1 instead, for connections that open with this
    /// run's random token.
    pub fn create_tcp() -> Option<Pipe> {
        let server = TcpIpcServer::bind(&token_from_bytes(&peerparty_core::inet::crypto::random_array::<16>())).ok()?;
        let endpoint = server.endpoint().ok()?;
        Pipe::start(endpoint, move |tx, current| listen_tcp(server, tx, current))
    }

    fn start(endpoint: IpcEndpoint, listen: impl FnOnce(Sender<HookMsg>, Current) + Send + 'static) -> Option<Pipe> {
        let (in_tx, rx) = mpsc::channel();
        let (tx, out_rx) = mpsc::channel::<String>();
        let current: Current = Arc::new(Mutex::new(None));
        let listen_current = current.clone();
        let writer_current = current.clone();
        std::thread::Builder::new()
            .name("peerparty-pipe-listen".into())
            .spawn(move || listen(in_tx, listen_current))
            .ok()?;
        std::thread::Builder::new()
            .name("peerparty-pipe-out".into())
            .spawn(move || writer(writer_current, out_rx))
            .ok()?;
        Some(Pipe {
            endpoint,
            rx,
            tx,
            #[cfg(unix)]
            current,
        })
    }

    /// The variable the game's process finds the endpoint in.
    pub fn env_key(&self) -> &'static str {
        match self.endpoint {
            IpcEndpoint::WinPipe { .. } => peerparty_protocol::env::PIPE,
            IpcEndpoint::TcpToken { .. } => peerparty_protocol::env::IPC,
        }
    }

    pub fn send(&self, msg: &HostMsg) {
        let _ = self.tx.send(msg.to_line());
    }

    /// A handle for sending commands from another thread.
    #[cfg(unix)]
    pub fn commands(&self) -> Commands {
        Commands { tx: self.tx.clone(), current: self.current.clone() }
    }
}

/// A state's JSON as `serde_json`'s, which the overlay's `GameState` reads. 64-bit ids
/// stay exact (both keep integers as integers); a key found twice is its first, as
/// `Json::get` reads it.
pub fn to_value(v: &Json) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(n) => Value::from(*n),
        Json::U64(n) => Value::from(*n),
        Json::Num(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
        Json::Str(s) => Value::String(s.clone()),
        Json::Arr(items) => Value::Array(items.iter().map(to_value).collect()),
        Json::Obj(pairs) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                if !map.contains_key(k) {
                    map.insert(k.clone(), to_value(v));
                }
            }
            Value::Object(map)
        }
    }
}

/// A raw pipe handle as a usize, so it can move to its thread. `first`: fails if a pipe
/// of that name exists already (later instances join the one we made).
#[cfg(windows)]
fn make(name: &str, access: u32, first: bool) -> Option<usize> {
    let wide = crate::backend::windows::wide(name);
    let open = access | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    let mode = PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let sd = owner_only()?;
    let sa = sd.attributes();
    let h = unsafe { CreateNamedPipeW(wide.as_ptr(), open, mode, PIPE_UNLIMITED_INSTANCES, 65536, 65536, 0, &sa) };
    (h != INVALID_HANDLE_VALUE).then_some(h as usize)
}

#[cfg(windows)]
fn connect(h: usize) -> std::fs::File {
    // Fails with ERROR_PIPE_CONNECTED when the client was quicker; that's connected too.
    unsafe { ConnectNamedPipe(h as _, std::ptr::null_mut()) };
    unsafe { std::fs::File::from_raw_handle(h as _) }
}

/// Accepts connections one after another, each on a new pair of instances.
#[cfg(windows)]
fn listen(name: String, first: (usize, usize), tx: Sender<HookMsg>, current: Current) {
    let mut pair = Some(first);
    loop {
        let Some((up, down)) = pair.take().or_else(|| Some((make(&format!("{name}-up"), PIPE_ACCESS_INBOUND, false)?, make(&format!("{name}-down"), PIPE_ACCESS_OUTBOUND, false)?))) else {
            return;
        };
        // The hook opens -up, then -down.
        let up = connect(up);
        let down = connect(down);
        *current.lock().unwrap() = Some(Box::new(down));
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("peerparty-pipe-in".into()).spawn(move || reader(up, tx));
    }
}

/// Accepts TCP connections one after another; one from off this PC, or that doesn't
/// open with the token, is dropped (`TcpIpcServer::accept`).
fn listen_tcp(server: TcpIpcServer, tx: Sender<HookMsg>, current: Current) {
    loop {
        match server.accept() {
            Ok(IpcStreams { read, write }) => {
                *current.lock().unwrap() = Some(Box::new(write));
                let tx = tx.clone();
                let _ = std::thread::Builder::new().name("peerparty-pipe-in".into()).spawn(move || reader(read, tx));
            }
            // A connection refused, or one that failed its handshake: on to the next.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
            // Anything else may be the listener's own failure: no busy loop on it.
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
}

/// The hook's messages as they come; a line that isn't one this build knows is skipped.
fn reader(stream: impl Read, tx: Sender<HookMsg>) {
    let mut r = BufReader::new(stream);
    let mut line = Vec::new();
    while read_line_capped(&mut r, &mut line, MAX_HOOK_LINE) {
        if let Some(msg) = std::str::from_utf8(&line).ok().and_then(HookMsg::from_line) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    }
}

fn writer(current: Current, rx: Receiver<String>) {
    for msg in rx {
        let mut cur = current.lock().unwrap();
        if let Some(out) = cur.as_mut() {
            if out.write_all(msg.as_bytes()).and_then(|_| out.write_all(b"\n")).and_then(|_| out.flush()).is_err() {
                *cur = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::ipc::{MAX_HOST_LINE, Notice};
    use peerparty_protocol::transport::TcpIpcClient;
    use std::time::Duration;

    #[cfg(windows)]
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

    /// The hook's side of one exchange: its messages arrive typed (a state's 64-bit id
    /// exact, a line this build doesn't know skipped), and a command comes back.
    fn exchange(pipe: &Pipe, up: &mut impl Write, down: impl Read) {
        let state = r#"{"t":"state","pid":4242,"pending":[{"id":18446744073709551615,"name":"Sam"}]}"#;
        let note = HookMsg::Note(Notice::cant_join("Sam")).to_line();
        up.write_all(format!("{state}\n{{\"t\":\"from_the_future\"}}\nnot json\n{note}\n").as_bytes()).unwrap();
        up.flush().unwrap();
        let next = || pipe.rx.recv_timeout(Duration::from_secs(10)).expect("a message from the hook");
        let HookMsg::State(v) = next() else { panic!("a state first") };
        assert_eq!(to_value(&v)["pending"][0]["id"].as_u64(), Some(u64::MAX));
        assert_eq!(to_value(&v)["pid"].as_u64(), Some(4242));
        assert_eq!(next(), HookMsg::Note(Notice::cant_join("Sam")), "the lines in between skipped");
        // The reader is running, so the connection is the one commands go to.
        pipe.send(&HostMsg::Accept { id: u64::MAX });
        pipe.send(&HostMsg::Overlay { visible: true, why: Some("Shift+Tab".into()), pads_held: false });
        let mut r = std::io::BufReader::new(down);
        let mut line = Vec::new();
        for want in [HostMsg::Accept { id: u64::MAX }, HostMsg::Overlay { visible: true, why: Some("Shift+Tab".into()), pads_held: false }] {
            assert!(read_line_capped(&mut r, &mut line, MAX_HOST_LINE));
            assert_eq!(HostMsg::from_line(std::str::from_utf8(&line).unwrap()), Some(want));
        }
    }

    /// Messages go both ways over the named pipes, as the hook opens them (-up, then
    /// -down).
    #[cfg(windows)]
    #[test]
    fn messages_cross_the_named_pipes() {
        let pipe = Pipe::create().expect("the pipes");
        let IpcEndpoint::WinPipe { name } = &pipe.endpoint else { panic!("{:?}", pipe.endpoint) };
        assert_eq!(pipe.env_key(), "PEERPARTY_PIPE");
        assert_eq!(IpcEndpoint::parse(&pipe.endpoint.to_env_string()), Some(pipe.endpoint.clone()));
        let mut up = std::fs::OpenOptions::new().write(true).open(format!("{name}-up")).unwrap();
        let down = std::fs::OpenOptions::new().read(true).open(format!("{name}-down")).unwrap();
        exchange(&pipe, &mut up, down);
    }

    /// And over loopback TCP, once the token is sent; the wrong token gets nowhere.
    #[test]
    fn messages_cross_tcp_with_the_token() {
        let pipe = Pipe::create_tcp().expect("the listener");
        assert_eq!(pipe.env_key(), "PEERPARTY_IPC");
        let endpoint = IpcEndpoint::parse(&pipe.endpoint.to_env_string()).expect("an endpoint string the hook reads");
        let IpcEndpoint::TcpToken { addr, token } = &endpoint else { panic!("{endpoint:?}") };
        assert!(addr.ip().is_loopback() && token.len() == 32, "{endpoint:?}");
        let wrong = "0".repeat(32);
        assert!(TcpIpcClient::connect(*addr, &wrong, Duration::from_secs(5)).is_err(), "the wrong token");
        let IpcStreams { read, mut write } = TcpIpcClient::connect_endpoint(&endpoint, Duration::from_secs(5)).unwrap();
        exchange(&pipe, &mut write, read);
    }
}
