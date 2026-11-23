//! `sys` on Windows: Win32, the IP Helper API and winsock.

use std::fs::{File, OpenOptions};
use std::io;
use std::mem::offset_of;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
    MIB_UDP6ROW_OWNER_PID, MIB_UDP6TABLE_OWNER_PID, MIB_UDPROW_OWNER_PID, MIB_UDPTABLE_OWNER_PID,
};

/// `%LOCALAPPDATA%\peerparty`.
pub fn os_data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("peerparty"))
}

/// The Windows account's name.
pub fn user_name() -> Option<String> {
    std::env::var("USERNAME").ok()
}

/// Opens (creating) `path` shared with nobody: fails while another handle has it open,
/// and holds it so until the `File` is dropped.
pub fn lock_exclusive(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).write(true).truncate(false).share_mode(0).open(path)
}

/// Creates (or empties) a peer board file for this copy to write. FILE_SHARE_READ only:
/// others may read it, nobody may delete it while we run.
pub fn open_board(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).write(true).truncate(true).share_mode(1).open(path)
}

/// Deletes a peer board file whose copy has gone; false while its copy still has it.
pub fn remove_if_unheld(path: &Path) -> bool {
    std::fs::remove_file(path).is_ok()
}

/// The program a running process was started from; None when it has ended.
pub fn process_exe(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
        WaitForSingleObject,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return None;
        }
        let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        (alive && ok).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Whether the process has a visible top-level window.
pub fn has_visible_window(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};
    struct Find {
        pid: u32,
        found: bool,
    }
    unsafe extern "system" fn each(hwnd: HWND, l: LPARAM) -> BOOL {
        let f = unsafe { &mut *(l as *mut Find) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == f.pid && unsafe { IsWindowVisible(hwnd) } != 0 {
            f.found = true;
            return 0;
        }
        1
    }
    let mut f = Find { pid, found: false };
    unsafe { EnumWindows(Some(each), &mut f as *mut Find as LPARAM) };
    f.found
}

/// The rows of a table the IP Helper API filled into `buf` (a row count, then the rows
/// from byte `rows_at`), from its first `len` bytes: never more rows than the count
/// says, nor than those bytes hold.
///
/// # Safety
/// `R` is the table's row type, and plain data: any bytes make a valid `R`.
unsafe fn table_rows<R: Copy>(buf: &[u64], len: usize, rows_at: usize) -> Vec<R> {
    let len = len.min(buf.len() * 8);
    debug_assert!(align_of::<R>() <= align_of::<u64>() && rows_at % align_of::<R>() == 0);
    if len < 4 || rows_at > len {
        return Vec::new();
    }
    let base = buf.as_ptr().cast::<u8>();
    // The count is the table's first field, at its start; `buf` is aligned for both.
    let count = unsafe { base.cast::<u32>().read() } as usize;
    let fits = (len - rows_at) / size_of::<R>();
    // In bounds: at most `fits` rows from `rows_at`, all within `len <= buf`'s bytes.
    unsafe { std::slice::from_raw_parts(base.add(rows_at).cast::<R>(), count.min(fits)) }.to_vec()
}

/// One of the system's tables, as `fill` (given the buffer and its size in bytes, which
/// it sets to the size needed when the buffer is too small) reads it: the buffer and how
/// many of its bytes hold the table. Empty when it can't be read.
fn read_table(fill: impl Fn(*mut core::ffi::c_void, &mut u32) -> u32) -> (Vec<u64>, usize) {
    let mut size = 0u32;
    for _ in 0..4 {
        let mut buf = vec![0u64; size as usize / 8 + 1];
        let mut len = (buf.len() * 8) as u32;
        if fill(buf.as_mut_ptr().cast(), &mut len) == 0 {
            let len = (len as usize).min(buf.len() * 8);
            return (buf, len);
        }
        size = len;
    }
    (Vec::new(), 0)
}

/// The (address, mask) of each IPv4 address this PC has, as the bytes are on the wire.
pub fn ipv4_networks() -> Vec<([u8; 4], [u8; 4])> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetIpAddrTable, MIB_IPADDRROW_XP, MIB_IPADDRTABLE};
    let (buf, len) = read_table(|buf, len| unsafe { GetIpAddrTable(buf.cast(), len, 0) });
    // SAFETY: the table's own row type, all integers.
    let rows: Vec<MIB_IPADDRROW_XP> = unsafe { table_rows(&buf, len, offset_of!(MIB_IPADDRTABLE, table)) };
    rows.iter().map(|r| (r.dwAddr.to_ne_bytes(), r.dwMask.to_ne_bytes())).collect()
}

/// The next hop toward the internet (the route to 8.8.8.8). Sends nothing.
pub fn default_gateway() -> Option<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetBestRoute, MIB_IPFORWARDROW};
    let mut row: MIB_IPFORWARDROW = unsafe { std::mem::zeroed() };
    let dest = u32::from_ne_bytes([8, 8, 8, 8]);
    if unsafe { GetBestRoute(dest, 0, &mut row) } != 0 {
        return None;
    }
    Some(Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes()))
}

/// An IPv4 row's address (in network order in memory) and port (in network order in
/// the low 16 bits).
fn v4(addr: u32, port: u32) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::from(addr.to_ne_bytes()), u16::from_be(port as u16)))
}

fn v6(addr: [u8; 16], port: u32) -> SocketAddr {
    SocketAddr::from((Ipv6Addr::from(addr), u16::from_be(port as u16)))
}

/// The local addresses of `pid`'s TCP listeners (`tcp`) or UDP sockets, from
/// `GetExtendedTcpTable` (`TCP_TABLE_OWNER_PID_LISTENER`) and `GetExtendedUdpTable`
/// (`UDP_TABLE_OWNER_PID`). IPv4 ones first; a table that can't be read has none.
pub fn sockets_of(pid: u32, tcp: bool) -> Option<Vec<SocketAddr>> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_LISTENER, UDP_TABLE_OWNER_PID,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    let table = |af: u16| {
        read_table(|buf, len| unsafe {
            if tcp {
                GetExtendedTcpTable(buf, len, 0, af as u32, TCP_TABLE_OWNER_PID_LISTENER, 0)
            } else {
                GetExtendedUdpTable(buf, len, 0, af as u32, UDP_TABLE_OWNER_PID, 0)
            }
        })
    };
    let (v4t, v6t) = (table(AF_INET), table(AF_INET6));
    Some(if tcp { tcp_rows(&v4t, &v6t, pid) } else { udp_rows(&v4t, &v6t, pid) })
}

/// `pid`'s rows of the TCP tables (`MIB_TCPTABLE_OWNER_PID`, `MIB_TCP6TABLE_OWNER_PID`).
fn tcp_rows(v4t: &(Vec<u64>, usize), v6t: &(Vec<u64>, usize), pid: u32) -> Vec<SocketAddr> {
    // SAFETY: the tables' own row types, all integers and byte arrays.
    let r4: Vec<MIB_TCPROW_OWNER_PID> = unsafe { table_rows(&v4t.0, v4t.1, offset_of!(MIB_TCPTABLE_OWNER_PID, table)) };
    let r6: Vec<MIB_TCP6ROW_OWNER_PID> = unsafe { table_rows(&v6t.0, v6t.1, offset_of!(MIB_TCP6TABLE_OWNER_PID, table)) };
    let mut out: Vec<SocketAddr> = r4.iter().filter(|r| r.dwOwningPid == pid).map(|r| v4(r.dwLocalAddr, r.dwLocalPort)).collect();
    out.extend(r6.iter().filter(|r| r.dwOwningPid == pid).map(|r| v6(r.ucLocalAddr, r.dwLocalPort)));
    out
}

/// `pid`'s rows of the UDP tables (`MIB_UDPTABLE_OWNER_PID`, `MIB_UDP6TABLE_OWNER_PID`).
fn udp_rows(v4t: &(Vec<u64>, usize), v6t: &(Vec<u64>, usize), pid: u32) -> Vec<SocketAddr> {
    // SAFETY: the tables' own row types, all integers and byte arrays.
    let r4: Vec<MIB_UDPROW_OWNER_PID> = unsafe { table_rows(&v4t.0, v4t.1, offset_of!(MIB_UDPTABLE_OWNER_PID, table)) };
    let r6: Vec<MIB_UDP6ROW_OWNER_PID> = unsafe { table_rows(&v6t.0, v6t.1, offset_of!(MIB_UDP6TABLE_OWNER_PID, table)) };
    let mut out: Vec<SocketAddr> = r4.iter().filter(|r| r.dwOwningPid == pid).map(|r| v4(r.dwLocalAddr, r.dwLocalPort)).collect();
    out.extend(r6.iter().filter(|r| r.dwOwningPid == pid).map(|r| v6(r.ucLocalAddr, r.dwLocalPort)));
    out
}

/// Turns on TCP keepalive with our timings (SIO_KEEPALIVE_VALS: Windows' default waits
/// two hours before the first probe).
pub fn keepalive(stream: &TcpStream, idle: Duration, every: Duration) -> io::Result<()> {
    use windows_sys::Win32::Networking::WinSock::{SIO_KEEPALIVE_VALS, SOCKET, WSAIoctl};
    #[repr(C)]
    struct Vals {
        onoff: u32,
        time_ms: u32,
        interval_ms: u32,
    }
    let v = Vals { onoff: 1, time_ms: idle.as_millis() as u32, interval_ms: every.as_millis() as u32 };
    let mut ret = 0u32;
    let r = unsafe {
        WSAIoctl(
            stream.as_raw_socket() as SOCKET,
            SIO_KEEPALIVE_VALS,
            &v as *const Vals as *const core::ffi::c_void,
            size_of::<Vals>() as u32,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
            None,
        )
    };
    if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// A UDP socket on `ip:port` that every copy on this machine can share (SO_REUSEADDR),
/// with broadcast on. std can't set SO_REUSEADDR before bind, so this builds the socket
/// with winsock.
pub fn shared_udp_socket(ip: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, IN_ADDR, INVALID_SOCKET, IPPROTO_UDP, SO_BROADCAST, SO_REUSEADDR, SOCK_DGRAM,
        SOCKADDR, SOCKADDR_IN, SOL_SOCKET, WSADATA, WSAStartup, bind, closesocket, setsockopt,
        socket,
    };
    unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        // Returns the error itself (WSAGetLastError isn't usable before it succeeds).
        let ret = WSAStartup(0x0202, &mut data);
        if ret != 0 {
            return Err(io::Error::from_raw_os_error(ret));
        }
        let s = socket(AF_INET as i32, SOCK_DGRAM, IPPROTO_UDP);
        if s == INVALID_SOCKET {
            return Err(io::Error::last_os_error());
        }
        let on: i32 = 1;
        for opt in [SO_REUSEADDR, SO_BROADCAST] {
            if setsockopt(s, SOL_SOCKET, opt, &on as *const i32 as *const u8, 4) != 0 {
                let e = io::Error::last_os_error();
                closesocket(s);
                return Err(e);
            }
        }
        let mut addr: SOCKADDR_IN = std::mem::zeroed();
        addr.sin_family = AF_INET;
        addr.sin_port = port.to_be();
        addr.sin_addr = IN_ADDR {
            S_un: std::mem::zeroed(),
        };
        addr.sin_addr.S_un.S_addr = u32::from(ip).to_be();
        if bind(
            s,
            &addr as *const SOCKADDR_IN as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_IN>() as i32,
        ) != 0
        {
            let e = io::Error::last_os_error();
            closesocket(s);
            return Err(e);
        }
        Ok(UdpSocket::from_raw_socket(s as RawSocket))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table as the IP Helper API lays it out: `count`, then `rows` from `rows_at`.
    fn table<R: Copy>(count: u32, rows: &[R], rows_at: usize) -> (Vec<u64>, usize) {
        let len = rows_at + rows.len() * size_of::<R>();
        let mut buf = vec![0u64; len.div_ceil(8)];
        unsafe {
            let base = buf.as_mut_ptr().cast::<u8>();
            base.cast::<u32>().write(count);
            std::ptr::copy_nonoverlapping(rows.as_ptr(), base.add(rows_at).cast::<R>(), rows.len());
        }
        (buf, len)
    }

    fn tcp4(ip: [u8; 4], port: u16, pid: u32) -> MIB_TCPROW_OWNER_PID {
        MIB_TCPROW_OWNER_PID {
            dwState: 2,
            dwLocalAddr: u32::from_ne_bytes(ip),
            dwLocalPort: port.to_be() as u32,
            dwRemoteAddr: 0,
            dwRemotePort: 0,
            dwOwningPid: pid,
        }
    }

    fn tcp6(ip: Ipv6Addr, port: u16, pid: u32) -> MIB_TCP6ROW_OWNER_PID {
        MIB_TCP6ROW_OWNER_PID {
            ucLocalAddr: ip.octets(),
            dwLocalScopeId: 0,
            dwLocalPort: port.to_be() as u32,
            ucRemoteAddr: [0; 16],
            dwRemoteScopeId: 0,
            dwRemotePort: 0,
            dwState: 2,
            dwOwningPid: pid,
        }
    }

    fn udp4(ip: [u8; 4], port: u16, pid: u32) -> MIB_UDPROW_OWNER_PID {
        MIB_UDPROW_OWNER_PID { dwLocalAddr: u32::from_ne_bytes(ip), dwLocalPort: port.to_be() as u32, dwOwningPid: pid }
    }

    fn udp6(ip: Ipv6Addr, port: u16, pid: u32) -> MIB_UDP6ROW_OWNER_PID {
        MIB_UDP6ROW_OWNER_PID { ucLocalAddr: ip.octets(), dwLocalScopeId: 0, dwLocalPort: port.to_be() as u32, dwOwningPid: pid }
    }

    /// Only our own rows, IPv4 first, addresses and ports out of network order.
    #[test]
    fn socket_tables_give_a_process_its_own_rows() {
        let me = 4242;
        let at4 = offset_of!(MIB_TCPTABLE_OWNER_PID, table);
        let at6 = offset_of!(MIB_TCP6TABLE_OWNER_PID, table);
        let r4 = [tcp4([127, 0, 0, 1], 1000, me), tcp4([10, 1, 2, 3], 1002, me + 1), tcp4([0; 4], 1001, me)];
        let r6 = [tcp6(Ipv6Addr::LOCALHOST, 1003, me), tcp6(Ipv6Addr::UNSPECIFIED, 1004, me + 1)];
        let got = tcp_rows(&table(3, &r4, at4), &table(2, &r6, at6), me);
        assert_eq!(
            got,
            vec![
                SocketAddr::from(([127, 0, 0, 1], 1000)),
                SocketAddr::from(([0, 0, 0, 0], 1001)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, 1003)),
            ]
        );
        let at4 = offset_of!(MIB_UDPTABLE_OWNER_PID, table);
        let at6 = offset_of!(MIB_UDP6TABLE_OWNER_PID, table);
        let r4 = [udp4([192, 168, 1, 5], 27015, me), udp4([0; 4], 1, me + 1)];
        let r6 = [udp6(Ipv6Addr::UNSPECIFIED, 27016, me)];
        let got = udp_rows(&table(2, &r4, at4), &table(1, &r6, at6), me);
        assert_eq!(got, vec![SocketAddr::from(([192, 168, 1, 5], 27015)), SocketAddr::from((Ipv6Addr::UNSPECIFIED, 27016))]);
    }

    /// A table whose count claims more rows than its bytes hold is read as far as the
    /// bytes go; a short or empty one has none.
    #[test]
    fn short_tables_are_read_only_as_far_as_they_go() {
        let me = 7;
        let at = offset_of!(MIB_TCPTABLE_OWNER_PID, table);
        let rows = [tcp4([127, 0, 0, 1], 1, me), tcp4([127, 0, 0, 1], 2, me)];
        let none = (Vec::new(), 0);
        let (buf, len) = table(1000, &rows, at);
        assert_eq!(tcp_rows(&(buf.clone(), len), &none, me).len(), 2, "the count is more than the bytes");
        assert_eq!(tcp_rows(&(buf.clone(), len - 1), &none, me).len(), 1, "a row cut short isn't read");
        assert_eq!(tcp_rows(&(buf.clone(), 3), &none, me).len(), 0, "not even a count");
        assert_eq!(tcp_rows(&(buf.clone(), usize::MAX), &none, me).len(), 2, "never past the buffer");
        let (buf, len) = table(1, &rows, at);
        assert_eq!(tcp_rows(&(buf, len), &none, me).len(), 1, "no more than the count");
        assert!(tcp_rows(&none, &none, me).is_empty());
        assert!(udp_rows(&none, &none, me).is_empty());
    }

    #[test]
    fn this_pc_has_networks() {
        // Loopback at least, which the API lists too.
        assert!(ipv4_networks().iter().any(|(a, _)| Ipv4Addr::from(*a).is_loopback()));
    }
}
