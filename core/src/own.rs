//! The ports PeerParty's own sockets hold in the game's process: the mesh's listener and
//! sockets, the internet tunnels' sockets and bridges, the forwarder's own ends, a local
//! stand-in service.
//!
//! The typed-IP forwarder (`fwd`) hands a peer's traffic to "ports the game has open",
//! which it tells by the process that owns them, and these belong to the game's process
//! too. Without this list a peer could ask the forwarder for the mesh's own listener: it
//! would arrive there from loopback, where a player on this PC arrives, and be trusted
//! as one (a public stranger as a LAN player, or under a friend's id). So every socket
//! of ours that listens says so here, and the forwarder never delivers to one.

use std::sync::Mutex;

static HELD: Mutex<Vec<(bool, u16)>> = Mutex::new(Vec::new());

fn held() -> std::sync::MutexGuard<'static, Vec<(bool, u16)>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// A port of ours, off limits to the forwarder until this is dropped (with its socket).
#[derive(Debug)]
pub struct Held {
    tcp: bool,
    port: u16,
}

impl Held {
    /// For a socket that lives as long as the process.
    pub fn forever(self) {
        std::mem::forget(self);
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut h = held();
        if let Some(i) = h.iter().position(|e| *e == (self.tcp, self.port)) {
            h.swap_remove(i);
        }
    }
}

fn hold(tcp: bool, port: u16) -> Held {
    held().push((tcp, port));
    Held { tcp, port }
}

/// A TCP port one of our listeners has.
pub fn tcp(port: u16) -> Held {
    hold(true, port)
}

/// A UDP port one of our sockets has.
pub fn udp(port: u16) -> Held {
    hold(false, port)
}

/// Whether a port is one of ours (the forwarder then leaves it alone).
pub fn is_held(tcp: bool, port: u16) -> bool {
    held().contains(&(tcp, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_held_until_every_holder_lets_go() {
        // Ports no other test's sockets get (below the ephemeral range, unused here).
        let (a, b) = (tcp(7), tcp(7));
        let u = udp(9);
        assert!(is_held(true, 7) && is_held(false, 9));
        assert!(!is_held(false, 7) && !is_held(true, 9), "TCP and UDP ports are apart");
        drop(a);
        assert!(is_held(true, 7), "a second holder still has it");
        drop(b);
        assert!(!is_held(true, 7));
        u.forever();
        assert!(is_held(false, 9));
    }
}
