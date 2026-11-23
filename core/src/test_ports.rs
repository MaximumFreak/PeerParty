//! Ports for tests: blocks of consecutive ones on 127.0.0.1 that no other test process on
//! this PC is using, however many run at once (two `cargo test`s, the 32-bit run beside
//! the 64-bit one, the suite's SDK pairs).
//!
//! The range is 10000..48000, below where Windows hands out ports to sockets bound to
//! port 0 (49152 and up; Linux's start at 32768, and its own ephemeral ports are skipped
//! the way any port in use is). The range is cut into blocks of `BLOCK` ports. A process
//! claims a block with a lock file in the temp folder
//! (`peerparty-test-ports\<block>.lock`), held until it exits (or crashes: the OS lets
//! go), and only once each of its ports binds for TCP and UDP right now: Windows keeps
//! ranges of ports out of use (`netsh int ipv4 show excludedportrange protocol=tcp`), and
//! another program may hold one. A block that's taken or won't bind is passed over.
//!
//! Scripts can claim blocks too (tests/run_all.ps1 does for its two-copy jobs): a file
//! held open with no sharing reads as taken here.
//!
//! `turn` caps how many of the heavy tests (engines meeting over loopback) run at once
//! on this PC, across every test process: with 48 at once (three runs side by side) a
//! few lost their timing races, a goodbye or a path switch arriving after a test's
//! deadline, though the CPU was mostly idle.

use std::fs::{File, OpenOptions};
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::sync::Mutex;

/// Ports in a block.
pub const BLOCK: u16 = 64;
const FIRST: u16 = 10_000;
const END: u16 = 48_000;

/// This process's claimed blocks' lock files, kept open until it exits.
static HELD: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// The first of `n` (1..=`BLOCK`) consecutive ports that bind on 127.0.0.1 for TCP and
/// UDP, kept from every other test on this PC for as long as this process runs. Each call
/// claims a block of its own.
pub fn claim(n: u16) -> u16 {
    assert!((1..=BLOCK).contains(&n), "a test claims 1..={BLOCK} ports at a time, not {n}");
    let blocks = (END - FIRST) / BLOCK;
    let dir = std::env::temp_dir().join("peerparty-test-ports");
    let _ = std::fs::create_dir_all(&dir);
    // Somewhere random to start, so processes started together don't all try the same
    // blocks first.
    let start = u16::from_le_bytes(crate::crypto::random_array::<2>()) % blocks;
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..blocks {
        let block = (start + i) % blocks;
        let base = FIRST + block * BLOCK;
        let Ok(file) = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(format!("{block}.lock"))) else {
            continue;
        };
        if file.try_lock().is_err() {
            continue;
        }
        if (base..base + n).all(binds) {
            held.push(file);
            return base;
        }
    }
    panic!("no block of {n} free ports in {FIRST}..{END} on 127.0.0.1");
}

/// One of this PC's turns for a heavy test (`turn`), held until dropped or the process
/// exits.
pub struct Turn {
    _file: File,
    index: usize,
}

/// How many heavy tests run at once on this PC: half its logical processors, at least 4.
/// Every process works out the same number.
fn turns() -> usize {
    std::thread::available_parallelism().map_or(8, |n| n.get() / 2).max(4)
}

/// Waits for a turn: a lock file of `turns()` in the temp folder
/// (`peerparty-test-turns\<n>.lock`) that no other test process holds. Says so on stderr
/// when the wait is long (shown with a failing test's output).
pub fn turn() -> Turn {
    let n = turns();
    let dir = std::env::temp_dir().join("peerparty-test-turns");
    let _ = std::fs::create_dir_all(&dir);
    let start = usize::from(u16::from_le_bytes(crate::crypto::random_array::<2>())) % n;
    let t0 = std::time::Instant::now();
    let mut said = false;
    loop {
        for i in 0..n {
            let index = (start + i) % n;
            let path = dir.join(format!("{index}.lock"));
            let Ok(file) = OpenOptions::new().create(true).truncate(false).write(true).open(path) else {
                continue;
            };
            if file.try_lock().is_ok() {
                let turn = Turn { _file: file, index };
                if said {
                    eprintln!("test_ports: got test turn {} after {:?}", turn.index, t0.elapsed());
                }
                return turn;
            }
        }
        if !said && t0.elapsed() > std::time::Duration::from_secs(2) {
            eprintln!("test_ports: waiting for one of {n} test turns (other tests on this PC hold them)");
            said = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Whether `port` binds on 127.0.0.1, TCP and UDP, now (the sockets close again).
fn binds(port: u16) -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() && UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn claims_are_apart_and_bind() {
        let a = super::claim(super::BLOCK);
        let b = super::claim(3);
        assert!(a.abs_diff(b) >= super::BLOCK, "{a} and {b} overlap");
        for p in [a, a + super::BLOCK - 1, b, b + 2] {
            assert!(super::binds(p), "{p}");
        }
    }

    /// A turn held is no one else's, this process's next one included. (Two at most:
    /// taking them all could wait forever on another run doing the same.)
    #[test]
    fn turns_are_held_apart() {
        let a = super::turn();
        let b = super::turn();
        assert_ne!(a.index, b.index);
    }
}
