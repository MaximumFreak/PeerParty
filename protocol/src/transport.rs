//! How the launcher and the hook reach each other, named in one string each so it can go
//! through the game's environment.
//!
//! - The control connection (`IpcEndpoint`): Windows named pipes on Windows; or a TCP
//!   connection on 127.0.0.1 with a secret token, which also crosses between a Linux
//!   launcher and a game under Wine, where a named pipe can't.
//! - The overlay frame's block (`ShmBacking`): a named file mapping on Windows; or a file
//!   both sides map, which a Linux launcher and a game under Wine can share.
//!
//! The TCP connection is this PC's alone: it listens on loopback only, refuses a peer
//! that isn't, and the first line must be `AUTH <token>` with the token the launcher put
//! in the game's environment (compared in constant time); anything else ends it. Then
//! the connection carries the same lines of JSON as the pipes (ipc.rs).

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// A token's shortest and longest length (hex digits: 128 bits at least).
pub const MIN_TOKEN: usize = 32;
pub const MAX_TOKEN: usize = 128;
/// How long each side waits for the other's handshake line.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest handshake line read (`AUTH ` and the longest token).
const MAX_HANDSHAKE: usize = 5 + MAX_TOKEN + 2;

const PIPE_PREFIX: &str = r"\\.\pipe\";
const TCP_PREFIX: &str = "tcp:";
const FILE_PREFIX: &str = "file:";
const MAPPING_PREFIX: &str = r"Local\";

/// Random bytes as a token (the caller has the random source).
pub fn token_from_bytes(random: &[u8]) -> String {
    random.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a token is one PeerParty would make: hex digits, long enough to guess never.
pub fn valid_token(token: &str) -> bool {
    (MIN_TOKEN..=MAX_TOKEN).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Equal bytes, in a time that doesn't depend on where they differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u64;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= u64::from(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

/// The pipes' base name for a run: the launcher's process id (for whoever reads a log,
/// and the hook's check of the pipe's server) and digits nobody else can guess.
pub fn pipe_name(pid: u32, random: &[u8]) -> String {
    format!(r"{PIPE_PREFIX}peerparty-{pid}-{}", token_from_bytes(random))
}

/// The launcher's pid from a pipe name (`\\.\pipe\peerparty-<pid>-<digits>`).
pub fn launcher_pid(pipe: &str) -> Option<u32> {
    let rest = pipe.rsplit('\\').next()?.strip_prefix("peerparty-")?;
    rest.split('-').next()?.parse().ok()
}

/// The frame mapping's name for a game's process.
pub fn frame_mapping_name(pid: u32, random: &[u8]) -> String {
    format!(r"{MAPPING_PREFIX}peerparty-frame-{pid}-{}", token_from_bytes(random))
}

/// Where the control connection is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IpcEndpoint {
    /// Named pipes `<name>-up` and `<name>-down` (hook/src/ipc.rs).
    WinPipe { name: String },
    /// TCP on loopback, opened with `AUTH <token>`.
    TcpToken { addr: SocketAddr, token: String },
}

impl IpcEndpoint {
    /// `\\.\pipe\...` or `tcp:<loopback ip>:<port>:<token>` (an IPv6 address in brackets).
    /// None for anything else, a non-loopback address, port 0 or a bad token.
    pub fn parse(s: &str) -> Option<IpcEndpoint> {
        let s = s.trim();
        if s.len() > PIPE_PREFIX.len() && s.get(..PIPE_PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(PIPE_PREFIX)) {
            return (s.len() <= 256 && !s.contains(['\0', '\n'])).then(|| IpcEndpoint::WinPipe { name: s.to_string() });
        }
        let (addr, token) = s.strip_prefix(TCP_PREFIX)?.rsplit_once(':')?;
        let addr: SocketAddr = addr.parse().ok()?;
        (addr.ip().is_loopback() && addr.port() != 0 && valid_token(token)).then(|| IpcEndpoint::TcpToken { addr, token: token.to_string() })
    }

    pub fn to_env_string(&self) -> String {
        match self {
            IpcEndpoint::WinPipe { name } => name.clone(),
            IpcEndpoint::TcpToken { addr, token } => format!("{TCP_PREFIX}{addr}:{token}"),
        }
    }
}

/// Where the overlay frame's block is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShmBacking {
    /// A named file mapping (`Local\peerparty-frame-...`).
    WinMapping { name: String },
    /// A file both sides map (`file:<path>`).
    FileMmap { path: String },
}

impl ShmBacking {
    pub fn parse(s: &str) -> Option<ShmBacking> {
        let s = s.trim();
        if s.contains(['\0', '\n']) || s.len() > 4096 {
            return None;
        }
        if let Some(path) = s.strip_prefix(FILE_PREFIX) {
            return (!path.is_empty()).then(|| ShmBacking::FileMmap { path: path.to_string() });
        }
        (s.len() > MAPPING_PREFIX.len() && s.starts_with(MAPPING_PREFIX)).then(|| ShmBacking::WinMapping { name: s.to_string() })
    }

    pub fn to_env_string(&self) -> String {
        match self {
            ShmBacking::WinMapping { name } => name.clone(),
            ShmBacking::FileMmap { path } => format!("{FILE_PREFIX}{path}"),
        }
    }
}

/// One authenticated connection, as a stream to read lines from (wrap it in a
/// `BufReader` for `ipc::read_line_capped`) and one to write them to.
pub struct IpcStreams {
    pub read: TcpStream,
    pub write: TcpStream,
}

impl IpcStreams {
    fn new(stream: TcpStream) -> std::io::Result<IpcStreams> {
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        let _ = stream.set_nodelay(true);
        Ok(IpcStreams { write: stream.try_clone()?, read: stream })
    }
}

fn refused(why: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, why.to_string())
}

/// One handshake line, read a byte at a time so nothing after it is taken from the
/// stream; None past `MAX_HANDSHAKE` bytes, at the end or on a timeout.
fn handshake_line(s: &mut TcpStream) -> Option<String> {
    let mut line = Vec::new();
    let mut b = [0u8];
    while line.len() <= MAX_HANDSHAKE {
        match s.read(&mut b) {
            Ok(1) if b[0] == b'\n' => {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return String::from_utf8(line).ok();
            }
            Ok(1) => line.push(b[0]),
            _ => return None,
        }
    }
    None
}

/// The launcher's end of a TCP control connection.
pub struct TcpIpcServer {
    listener: TcpListener,
    token: String,
}

impl TcpIpcServer {
    /// Listens on a free port of 127.0.0.1 for connections that know `token`.
    pub fn bind(token: &str) -> std::io::Result<TcpIpcServer> {
        if !valid_token(token) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "an IPC token is 32 to 128 hex digits"));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        Ok(TcpIpcServer { listener, token: token.to_string() })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// What goes in the game's environment for it to connect here.
    pub fn endpoint(&self) -> std::io::Result<IpcEndpoint> {
        Ok(IpcEndpoint::TcpToken { addr: self.local_addr()?, token: self.token.clone() })
    }

    /// Waits for the next connection and checks it: a peer that isn't on loopback, or
    /// whose first line isn't `AUTH <token>` within `HANDSHAKE_TIMEOUT`, is dropped with
    /// an error (call again for the next one).
    pub fn accept(&self) -> std::io::Result<IpcStreams> {
        let (mut s, peer) = self.listener.accept()?;
        if !peer.ip().is_loopback() {
            return Err(refused("an IPC connection from off this PC"));
        }
        s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        s.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let line = handshake_line(&mut s).unwrap_or_default();
        let given = line.strip_prefix("AUTH ").unwrap_or_default();
        if !ct_eq(given.as_bytes(), self.token.as_bytes()) {
            return Err(refused("an IPC connection without the token"));
        }
        s.write_all(b"OK\n")?;
        IpcStreams::new(s)
    }
}

/// The hook's end of a TCP control connection.
pub struct TcpIpcClient;

impl TcpIpcClient {
    /// Connects to a loopback `addr` and proves it knows `token`. Fails for an address
    /// off this PC, after `timeout` for the connection, or when the other end doesn't
    /// accept the token.
    pub fn connect(addr: SocketAddr, token: &str, timeout: Duration) -> std::io::Result<IpcStreams> {
        if !addr.ip().is_loopback() {
            return Err(refused("an IPC endpoint off this PC"));
        }
        let mut s = TcpStream::connect_timeout(&addr, timeout)?;
        s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        s.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        s.write_all(format!("AUTH {token}\n").as_bytes())?;
        if handshake_line(&mut s).as_deref() != Some("OK") {
            return Err(refused("the launcher didn't take the IPC token"));
        }
        IpcStreams::new(s)
    }

    /// `connect` to an endpoint string's `TcpToken`.
    pub fn connect_endpoint(endpoint: &IpcEndpoint, timeout: Duration) -> std::io::Result<IpcStreams> {
        match endpoint {
            IpcEndpoint::TcpToken { addr, token } => TcpIpcClient::connect(*addr, token, timeout),
            IpcEndpoint::WinPipe { .. } => Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a named pipe isn't a TCP endpoint")),
        }
    }
}

/// A file of an exact size for a `FileMmap` frame block. Mapping it is the caller's:
/// `CreateFileMappingW` + `MapViewOfFile` on Windows (Wine included), `mmap` with
/// `MAP_SHARED` on Linux. The file is only ever this size, so a mapping of it can't be
/// read past its end; on Unix only its owner can open it.
pub struct FileShm {
    pub file: std::fs::File,
    pub path: std::path::PathBuf,
    pub size: u64,
}

impl FileShm {
    /// Makes the file, `size` bytes of zeros. One that exists already is refused (it
    /// isn't ours: names carry random digits, so nobody else makes it first by chance).
    pub fn create(path: &std::path::Path, size: u64) -> std::io::Result<FileShm> {
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let file = o.open(path)?;
        file.set_len(size)?;
        Ok(FileShm { file, path: path.to_path_buf(), size })
    }

    /// Opens an existing block (the reader's end), with its size as it is on disk.
    pub fn open(path: &std::path::Path, write: bool) -> std::io::Result<FileShm> {
        let file = std::fs::OpenOptions::new().read(true).write(write).open(path)?;
        let size = file.metadata()?.len();
        Ok(FileShm { file, path: path.to_path_buf(), size })
    }

    /// The block's file name in `dir`, for a game's process.
    pub fn path_in(dir: &std::path::Path, pid: u32, random: &[u8]) -> std::path::PathBuf {
        dir.join(format!("peerparty-frame-{pid}-{}.bin", token_from_bytes(random)))
    }

    /// Deletes the file (the creator's, at the end of the run).
    pub fn remove(self) -> std::io::Result<()> {
        let FileShm { file, path, .. } = self;
        drop(file);
        std::fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{HostMsg, MAX_HOST_LINE, read_line_capped};
    use std::io::BufReader;

    const TOKEN: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn endpoints_parse_and_print() {
        let pipe = pipe_name(4242, &[0, 0x11, 0xaa, 0xff]);
        assert_eq!(pipe, r"\\.\pipe\peerparty-4242-0011aaff");
        assert_eq!(launcher_pid(&pipe), Some(4242));
        assert_eq!(launcher_pid(r"\\.\pipe\something-else"), None);
        let e = IpcEndpoint::parse(&pipe).unwrap();
        assert_eq!(e, IpcEndpoint::WinPipe { name: pipe.clone() });
        assert_eq!(e.to_env_string(), pipe);
        let tcp = format!("tcp:127.0.0.1:50123:{TOKEN}");
        let e = IpcEndpoint::parse(&tcp).unwrap();
        assert_eq!(e, IpcEndpoint::TcpToken { addr: "127.0.0.1:50123".parse().unwrap(), token: TOKEN.into() });
        assert_eq!(e.to_env_string(), tcp);
        let v6 = format!("tcp:[::1]:50123:{TOKEN}");
        assert_eq!(IpcEndpoint::parse(&v6).map(|e| e.to_env_string()), Some(v6));
        for bad in [
            format!("tcp:192.168.1.5:50123:{TOKEN}"),
            format!("tcp:0.0.0.0:50123:{TOKEN}"),
            format!("tcp:127.0.0.1:0:{TOKEN}"),
            "tcp:127.0.0.1:50123:short".into(),
            format!("tcp:127.0.0.1:50123:{}", "zz".repeat(16)),
            format!("tcp:localhost:50123:{TOKEN}"),
            r"\\.\pipe\".into(),
            "".into(),
            "something".into(),
        ] {
            assert_eq!(IpcEndpoint::parse(&bad), None, "{bad}");
        }
        let m = frame_mapping_name(7, &[1, 2]);
        assert_eq!(m, r"Local\peerparty-frame-7-0102");
        assert_eq!(ShmBacking::parse(&m), Some(ShmBacking::WinMapping { name: m.clone() }));
        let f = ShmBacking::parse("file:/run/user/1000/peerparty/frame-7.bin").unwrap();
        assert_eq!(f, ShmBacking::FileMmap { path: "/run/user/1000/peerparty/frame-7.bin".into() });
        assert_eq!(f.to_env_string(), "file:/run/user/1000/peerparty/frame-7.bin");
        assert_eq!(ShmBacking::parse(&f.to_env_string()), Some(f));
        for bad in ["file:", r"Local\", "Global\\x", "/tmp/x", ""] {
            assert_eq!(ShmBacking::parse(bad), None, "{bad}");
        }
        assert!(valid_token(TOKEN) && !valid_token("abc") && !valid_token(&"a".repeat(200)));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"abc", b"abcd") && !ct_eq(b"", b"a") && ct_eq(b"", b""));
    }

    /// The token opens the connection, lines go both ways, and the wrong token, none,
    /// or an address off this PC doesn't get in.
    #[test]
    fn tcp_connections_need_the_token() {
        let server = TcpIpcServer::bind(TOKEN).unwrap();
        assert!(TcpIpcServer::bind("short").is_err());
        let endpoint = IpcEndpoint::parse(&server.endpoint().unwrap().to_env_string()).unwrap();
        let addr = server.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
        let client = std::thread::spawn(move || {
            let wrong = TcpIpcClient::connect(addr, "ffeeddccbbaa99887766554433221100", Duration::from_secs(5));
            assert!(wrong.is_err(), "the wrong token");
            // A first line that isn't a handshake.
            let mut junk = TcpStream::connect(addr).unwrap();
            junk.write_all(b"GET / HTTP/1.1\n").unwrap();
            let IpcStreams { read, mut write } = TcpIpcClient::connect_endpoint(&endpoint, Duration::from_secs(5)).unwrap();
            // Written right after the handshake: nothing may be lost to it.
            write.write_all(format!("{}\n", HostMsg::Mic { on: true }.to_line()).as_bytes()).unwrap();
            let mut r = BufReader::new(read);
            let mut line = Vec::new();
            assert!(read_line_capped(&mut r, &mut line, MAX_HOST_LINE));
            assert_eq!(HostMsg::from_line(std::str::from_utf8(&line).unwrap()), Some(HostMsg::Paste));
        });
        let e = server.accept().err().expect("the wrong token is refused");
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(server.accept().is_err(), "a line that isn't AUTH is refused");
        let IpcStreams { read, mut write } = server.accept().unwrap();
        let mut r = BufReader::new(read);
        let mut line = Vec::new();
        assert!(read_line_capped(&mut r, &mut line, MAX_HOST_LINE));
        assert_eq!(HostMsg::from_line(std::str::from_utf8(&line).unwrap()), Some(HostMsg::Mic { on: true }));
        write.write_all(format!("{}\n", HostMsg::Paste.to_line()).as_bytes()).unwrap();
        client.join().unwrap();
        let off_pc: SocketAddr = "192.0.2.1:50123".parse().unwrap();
        assert_eq!(TcpIpcClient::connect(off_pc, TOKEN, Duration::from_millis(100)).err().map(|e| e.kind()), Some(std::io::ErrorKind::PermissionDenied));
        let pipe = IpcEndpoint::WinPipe { name: r"\\.\pipe\peerparty-1-00".into() };
        assert!(TcpIpcClient::connect_endpoint(&pipe, Duration::from_millis(100)).is_err());
    }

    #[test]
    fn file_blocks_are_made_once_at_their_size() {
        let dir = std::env::temp_dir();
        let path = FileShm::path_in(&dir, std::process::id(), &[0xab, 0xcd, 0x01, 0x23, 0x45, 0x67, 0x89, 0xef]);
        assert!(path.file_name().unwrap().to_str().unwrap().starts_with("peerparty-frame-"));
        let _ = std::fs::remove_file(&path);
        let size = crate::shm::block_size(64).unwrap() as u64;
        let made = FileShm::create(&path, size).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size);
        assert!(FileShm::create(&path, size).is_err(), "one that exists is refused");
        let opened = FileShm::open(&path, false).unwrap();
        assert_eq!(opened.size, size);
        drop(opened);
        made.remove().unwrap();
        assert!(!path.exists());
    }
}
