//! `sys` on Linux and other Unix-likes: POSIX calls and the `/proc` files (`procfs`).

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::procfs;

/// `$XDG_DATA_HOME/peerparty`, else `~/.local/share/peerparty`. Only absolute
/// locations count (the XDG Base Directory rules).
pub fn os_data_dir() -> Option<PathBuf> {
    let absolute = |v: std::ffi::OsString| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    std::env::var_os("XDG_DATA_HOME")
        .and_then(absolute)
        .or_else(|| std::env::var_os("HOME").and_then(absolute).map(|h| h.join(".local").join("share")))
        .map(|d| d.join("peerparty"))
}

/// The account's name.
pub fn user_name() -> Option<String> {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).ok()
}

/// An exclusive `flock` on `fd`, without waiting.
fn flock_now(fd: RawFd) -> io::Result<()> {
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Opens (creating) `path` with an exclusive lock on it: fails while another open of it
/// (in this process or another) has the lock, and holds it until the `File` is dropped.
pub fn lock_exclusive(path: &Path) -> io::Result<File> {
    let f = OpenOptions::new().create(true).write(true).truncate(false).open(path)?;
    flock_now(f.as_raw_fd())?;
    Ok(f)
}

/// Creates a peer board file for this copy to write, locked while we run: it's made
/// locked beside `path` and renamed over it, so nobody finds it unlocked and takes its
/// copy for gone.
pub fn open_board(path: &Path) -> io::Result<File> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let fresh = path.with_file_name(format!("{name}.{}.new", std::process::id()));
    let made = OpenOptions::new().create(true).write(true).truncate(true).open(&fresh).and_then(|f| {
        flock_now(f.as_raw_fd())?;
        fs::rename(&fresh, path)?;
        Ok(f)
    });
    if made.is_err() {
        let _ = fs::remove_file(&fresh);
    }
    made
}

/// Deletes a peer board file whose copy has gone (nobody holds its lock); false while
/// its copy still has it. A file already gone counts as deleted.
pub fn remove_if_unheld(path: &Path) -> bool {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) => return e.kind() == io::ErrorKind::NotFound,
    };
    if flock_now(f.as_raw_fd()).is_err() {
        return false;
    }
    // Still the file at `path`, not one its copy has since put there.
    let same = match (f.metadata(), fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    same && fs::remove_file(path).is_ok()
}

/// The program a running process was started from; None when it has ended, or when we
/// may not see its program (another account's, as on Windows).
pub fn process_exe(pid: u32) -> Option<String> {
    // Not 0, and not a negative pid_t: those name process groups.
    let id = libc::pid_t::try_from(pid).ok().filter(|&p| p > 0)?;
    // Signal 0 only asks whether the process exists; EPERM: it does, as another user's.
    if unsafe { libc::kill(id, 0) } != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return None;
    }
    // A zombie still answers kill until its parent reaps it.
    if fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|s| procfs::ended(&s)) {
        return None;
    }
    let exe = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    Some(exe.to_string_lossy().into_owned())
}

/// Whether the process has a visible window. There's no one way to ask outside a
/// display server's own API, so every process is taken to have one: a slot's holder
/// that still runs is a copy that's playing, and the next slot is taken at once rather
/// than waited for (a quitting copy lets its slot go itself: `Lan::release_slot`).
pub fn has_visible_window(_pid: u32) -> bool {
    true
}

/// The (address, mask) of each IPv4 address on an interface that's up, as the bytes are
/// on the wire (`getifaddrs`).
pub fn ipv4_networks() -> Vec<([u8; 4], [u8; 4])> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut p = head;
    while !p.is_null() {
        // A node of the list getifaddrs made, freed below.
        let ifa = unsafe { &*p };
        p = ifa.ifa_next;
        if ifa.ifa_flags & libc::IFF_UP as libc::c_uint == 0 || ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
            continue;
        }
        if libc::c_int::from(unsafe { (*ifa.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // An AF_INET address is a sockaddr_in; the mask beside it is one too.
        let (a, m) = unsafe {
            (
                ifa.ifa_addr.cast::<libc::sockaddr_in>().read_unaligned(),
                ifa.ifa_netmask.cast::<libc::sockaddr_in>().read_unaligned(),
            )
        };
        out.push((a.sin_addr.s_addr.to_ne_bytes(), m.sin_addr.s_addr.to_ne_bytes()));
    }
    unsafe { libc::freeifaddrs(head) };
    out
}

/// The default route's gateway (`/proc/net/route`). Sends nothing.
pub fn default_gateway() -> Option<Ipv4Addr> {
    procfs::default_gateway(&fs::read_to_string("/proc/net/route").ok()?)
}

/// The local addresses of `pid`'s TCP listeners (`tcp`) or UDP sockets: the sockets in
/// `/proc/net/{tcp,tcp6,udp,udp6}` whose inodes are among `/proc/<pid>/fd`'s. IPv4 ones
/// first. None when those can't be read (no `/proc`).
pub fn sockets_of(pid: u32, tcp: bool) -> Option<Vec<SocketAddr>> {
    let mut ours = HashSet::new();
    for e in fs::read_dir(format!("/proc/{pid}/fd")).ok()?.flatten() {
        if let Some(inode) = fs::read_link(e.path()).ok().and_then(|l| procfs::socket_inode(&l.to_string_lossy())) {
            ours.insert(inode);
        }
    }
    let (v4, v6) = if tcp { ("/proc/net/tcp", "/proc/net/tcp6") } else { ("/proc/net/udp", "/proc/net/udp6") };
    let v4 = fs::read_to_string(v4).ok()?;
    // No IPv6 table without IPv6.
    let v6 = fs::read_to_string(v6).unwrap_or_default();
    let rows = procfs::sockets(&v4, tcp).into_iter().chain(procfs::sockets(&v6, tcp));
    Some(rows.filter(|(_, inode)| ours.contains(inode)).map(|(addr, _)| addr).collect())
}

fn set_int(fd: RawFd, level: libc::c_int, opt: libc::c_int, v: libc::c_int) -> io::Result<()> {
    let r = unsafe {
        libc::setsockopt(fd, level, opt, (&v as *const libc::c_int).cast(), size_of::<libc::c_int>() as libc::socklen_t)
    };
    if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Turns on TCP keepalive with our timings (to the second; Linux's default waits two
/// hours before the first probe).
pub fn keepalive(stream: &TcpStream, idle: Duration, every: Duration) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    set_int(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1)?;
    let secs = |d: Duration| d.as_secs().clamp(1, i32::MAX as u64) as libc::c_int;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        set_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, secs(idle))?;
        set_int(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, secs(every))?;
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = (secs(idle), secs(every));
    Ok(())
}

/// A UDP socket on `ip:port` that every copy on this machine can share (SO_REUSEADDR:
/// each gets the broadcasts), with broadcast on. std can't set SO_REUSEADDR before
/// bind, so this builds the socket itself.
pub fn shared_udp_socket(ip: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_UDP) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Ours alone from here: closed on every way out.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    // Not passed on to programs this one starts, as std's own sockets aren't.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for opt in [libc::SO_REUSEADDR, libc::SO_BROADCAST] {
        set_int(fd, libc::SOL_SOCKET, opt, 1)?;
    }
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = port.to_be();
    addr.sin_addr.s_addr = u32::from(ip).to_be();
    let r = unsafe {
        libc::bind(
            fd,
            (&addr as *const libc::sockaddr_in).cast(),
            size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UdpSocket::from(sock))
}
