//! Reading Linux's `/proc` text files: the routing table, the socket tables and a
//! process's status. Pure functions over the text, so they are tested on every OS.
//!
//! The kernel prints IPv4 addresses (and each 32-bit word of an IPv6 one) as the
//! number the address's bytes make in host order: "0100007F" is 127.0.0.1 on x86.
//! Ports are printed as plain numbers.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

/// RTF_UP | RTF_GATEWAY (`<linux/route.h>`).
const ROUTE_UP_GATEWAY: u32 = 0x1 | 0x2;
/// TCP_LISTEN, as a socket table's `st` column has it.
const TCP_LISTEN: &str = "0A";

/// An address word as `/proc` prints it.
fn word(hex: &str) -> Option<[u8; 4]> {
    if hex.len() != 8 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(u32::to_ne_bytes)
}

/// The default route's gateway in `/proc/net/route`: destination and mask 0.0.0.0, up
/// and through a gateway; the lowest metric when there are several.
pub(super) fn default_gateway(route: &str) -> Option<Ipv4Addr> {
    let mut best: Option<(u32, Ipv4Addr)> = None;
    // Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT
    for line in route.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        let (Some(dest), Some(gw), Ok(flags), Ok(metric), Some(mask)) =
            (word(f[1]), word(f[2]), u32::from_str_radix(f[3], 16), f[6].parse::<u32>(), word(f[7]))
        else {
            continue;
        };
        if dest != [0; 4] || mask != [0; 4] || flags & ROUTE_UP_GATEWAY != ROUTE_UP_GATEWAY {
            continue;
        }
        if best.is_none_or(|(m, _)| metric < m) {
            best = Some((metric, Ipv4Addr::from(gw)));
        }
    }
    best.map(|(_, gw)| gw)
}

/// The sockets in a `/proc/net/{tcp,tcp6,udp,udp6}` table, with their inodes: each
/// row's local address, TCP ones only while listening when `listening` is set. Rows
/// with no inode (a connection closing, owned by nobody) are left out.
pub(super) fn sockets(table: &str, listening: bool) -> Vec<(SocketAddr, u64)> {
    let mut out = Vec::new();
    // sl local_address rem_address st tx_queue:rx_queue tr:tm->when retrnsmt uid timeout inode
    for line in table.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || (listening && f[3] != TCP_LISTEN) {
            continue;
        }
        let Some((ip, port)) = f[1].split_once(':') else { continue };
        let (Ok(port), Ok(inode)) = (u16::from_str_radix(port, 16), f[9].parse::<u64>()) else { continue };
        if inode == 0 {
            continue;
        }
        let addr = match ip.len() {
            8 => word(ip).map(|b| SocketAddr::from((Ipv4Addr::from(b), port))),
            32 => {
                let mut b = [0u8; 16];
                let words = (0..4).map(|i| word(&ip[i * 8..i * 8 + 8]));
                let mut ok = true;
                for (i, w) in words.enumerate() {
                    match w {
                        Some(w) => b[i * 4..i * 4 + 4].copy_from_slice(&w),
                        None => ok = false,
                    }
                }
                ok.then(|| SocketAddr::from((Ipv6Addr::from(b), port)))
            }
            _ => None,
        };
        if let Some(addr) = addr {
            out.push((addr, inode));
        }
    }
    out
}

/// The inode of a socket, from where one of a process's `/proc/<pid>/fd` entries
/// links: "socket:[12345]".
pub(super) fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// Whether a `/proc/<pid>/status` says its process has ended: a zombie (`Z`) waiting
/// for its parent, or dead (`X`).
pub(super) fn ended(status: &str) -> bool {
    status
        .lines()
        .find_map(|l| l.strip_prefix("State:"))
        .is_some_and(|s| s.trim_start().starts_with(['Z', 'X']))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An IPv4 address as the kernel prints it on this machine.
    fn hex4(ip: [u8; 4]) -> String {
        format!("{:08X}", u32::from_ne_bytes(ip))
    }

    fn hex6(ip: Ipv6Addr) -> String {
        ip.octets().chunks(4).map(|c| hex4(c.try_into().unwrap())).collect()
    }

    const ROUTE_HEAD: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT";

    fn route(iface: &str, dest: [u8; 4], gw: [u8; 4], flags: u32, metric: u32, mask: [u8; 4]) -> String {
        format!("{iface}\t{}\t{}\t{flags:04X}\t0\t0\t{metric}\t{}\t0\t0\t0", hex4(dest), hex4(gw), hex4(mask))
    }

    #[test]
    fn the_default_route_names_the_gateway() {
        let table = [
            ROUTE_HEAD.to_string(),
            route("eth0", [192, 168, 1, 0], [0; 4], 0x1, 100, [255, 255, 255, 0]),
            route("eth0", [0; 4], [192, 168, 1, 1], 0x3, 100, [0; 4]),
        ]
        .join("\n");
        assert_eq!(default_gateway(&table), Some(Ipv4Addr::new(192, 168, 1, 1)));
        // The bytes as an x86 kernel prints them.
        if cfg!(target_endian = "little") {
            let text = format!("{ROUTE_HEAD}\nwlan0\t00000000\t0102A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n");
            assert_eq!(default_gateway(&text), Some(Ipv4Addr::new(192, 168, 2, 1)));
        }
    }

    #[test]
    fn only_an_up_default_route_through_a_gateway_counts() {
        let rows = |r: &[String]| [ROUTE_HEAD.to_string()].iter().chain(r).cloned().collect::<Vec<_>>().join("\n");
        assert_eq!(default_gateway(""), None);
        assert_eq!(default_gateway(ROUTE_HEAD), None);
        // Down, on-link (no gateway flag), and not the default route.
        assert_eq!(default_gateway(&rows(&[route("a", [0; 4], [10, 0, 0, 1], 0x2, 0, [0; 4])])), None);
        assert_eq!(default_gateway(&rows(&[route("a", [0; 4], [10, 0, 0, 1], 0x1, 0, [0; 4])])), None);
        assert_eq!(default_gateway(&rows(&[route("a", [10, 0, 0, 0], [10, 0, 0, 1], 0x3, 0, [255, 0, 0, 0])])), None);
        assert_eq!(default_gateway(&rows(&[route("a", [0; 4], [10, 0, 0, 1], 0x3, 0, [255, 0, 0, 0])])), None);
        // Two default routes: the lower metric wins, whichever comes first.
        let two = rows(&[
            route("wlan0", [0; 4], [10, 0, 0, 1], 0x3, 600, [0; 4]),
            route("eth0", [0; 4], [192, 168, 1, 1], 0x3, 100, [0; 4]),
            route("junk", [0; 4], [1, 2, 3, 4], 0x3, 50, [0; 4]).replace("\t50\t", "\tfifty\t"),
            "short line".into(),
        ]);
        assert_eq!(default_gateway(&two), Some(Ipv4Addr::new(192, 168, 1, 1)));
    }

    const NET_HEAD: &str =
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";

    fn row(n: usize, local: &str, port: u16, state: &str, inode: u64) -> String {
        format!(
            "{n:4}: {local}:{port:04X} {}:0000 {state} 00000000:00000000 00:00000000 00000000  1000        0 {inode} 1 0000000000000000 100 0 0 10 0",
            "0".repeat(local.len())
        )
    }

    #[test]
    fn socket_tables_give_addresses_ports_and_inodes() {
        let tcp = [
            NET_HEAD.to_string(),
            row(0, &hex4([127, 0, 0, 1]), 8080, "0A", 111),
            row(1, &hex4([0; 4]), 27015, "0A", 112),
            row(2, &hex4([10, 1, 2, 3]), 50000, "01", 113),
            row(3, &hex4([10, 1, 2, 3]), 50001, "06", 0),
        ]
        .join("\n");
        let lo = Ipv4Addr::LOCALHOST;
        assert_eq!(
            sockets(&tcp, true),
            vec![(SocketAddr::from((lo, 8080)), 111), (SocketAddr::from((Ipv4Addr::UNSPECIFIED, 27015)), 112)]
        );
        assert_eq!(sockets(&tcp, false).len(), 3, "every row with an owner");
        let mapped = lo.to_ipv6_mapped();
        let tcp6 = [
            NET_HEAD.to_string(),
            row(0, &hex6(Ipv6Addr::LOCALHOST), 1001, "0A", 7),
            row(1, &hex6(Ipv6Addr::UNSPECIFIED), 1004, "0A", 8),
            row(2, &hex6(mapped), 1006, "0A", 9),
        ]
        .join("\n");
        assert_eq!(
            sockets(&tcp6, true),
            vec![
                (SocketAddr::from((Ipv6Addr::LOCALHOST, 1001)), 7),
                (SocketAddr::from((Ipv6Addr::UNSPECIFIED, 1004)), 8),
                (SocketAddr::from((mapped, 1006)), 9),
            ]
        );
        if cfg!(target_endian = "little") {
            let text = format!("{NET_HEAD}\n{}", row(0, "0100007F", 0x1F90, "07", 5));
            assert_eq!(sockets(&text, false), vec![(SocketAddr::from((lo, 8080)), 5)]);
        }
    }

    #[test]
    fn broken_socket_rows_are_skipped() {
        assert!(sockets("", false).is_empty());
        assert!(sockets(NET_HEAD, false).is_empty());
        let bad = [
            NET_HEAD.to_string(),
            row(0, "0100007", 80, "0A", 1),
            row(1, "ZZ00007F", 80, "0A", 2),
            row(2, &"0".repeat(31), 80, "0A", 3),
            row(3, "0100007F", 80, "0A", 4).replace(":0050 ", ":XYZW "),
            row(4, "0100007F", 80, "0A", 5).replace(" 5 1 ", " five 1 "),
            "   5: 0100007F:0050".into(),
            row(6, "0100007F", 80, "0A", 6),
        ]
        .join("\n");
        assert_eq!(sockets(&bad, false).iter().map(|s| s.1).collect::<Vec<_>>(), vec![6]);
    }

    #[test]
    fn fd_links_name_socket_inodes() {
        assert_eq!(socket_inode("socket:[12345]"), Some(12345));
        assert_eq!(socket_inode("pipe:[12345]"), None);
        assert_eq!(socket_inode("/dev/null"), None);
        assert_eq!(socket_inode("socket:[]"), None);
        assert_eq!(socket_inode("socket:[12"), None);
    }

    #[test]
    fn zombies_and_dead_processes_have_ended() {
        assert!(!ended("Name:\tgame\nUmask:\t0022\nState:\tS (sleeping)\nPid:\t42\n"));
        assert!(!ended("State:\tR (running)"));
        assert!(ended("Name:\tgame\nState:\tZ (zombie)\n"));
        assert!(ended("State:\tX (dead)\n"));
        assert!(!ended(""), "no state line: not known to have ended");
    }
}
