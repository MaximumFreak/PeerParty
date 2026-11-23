//! Windows Firewall and the game, checked now and then off the overlay's thread while
//! PeerParty's engine runs in it (`tick`), and a fix the player asked for (`fix`). What
//! comes of them (`FirewallEvent`) is the session host's to show: a toast, the Internet
//! page's row, a notice.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use crate::firewall;

/// How often the firewall is checked while the game runs (it's quick: the rules are
/// read from the registry).
const FIREWALL_EVERY: Duration = Duration::from_secs(20);

/// What a check or a fix came to.
pub enum FirewallEvent {
    /// A fix ended: what to tell the player, and whether it worked.
    Fixed { message: String, ok: bool },
    /// A check's result, and whether it's a block the last check didn't see (toasted
    /// once).
    Checked { verdict: firewall::Verdict, newly_blocked: bool },
}

pub struct FirewallWatcher {
    /// The game's program, for the check (the process's own: a starter program hands
    /// over to the game), and whether it's an Xbox app game (its rules are the
    /// package's, which this doesn't read).
    exe: PathBuf,
    packaged: bool,
    /// A check running off this thread, and when the last one started.
    fw_rx: Option<Receiver<firewall::Verdict>>,
    fw_checked: Option<Instant>,
    /// What the last check found, so each new block toasts once.
    fw_blocked: bool,
    /// A fix under way: what to say when it's done, and whether it worked.
    fixing: Option<Receiver<(String, bool)>>,
    /// The players on our network we couldn't reach at the last state.
    pub unreachable: u64,
}

impl FirewallWatcher {
    pub fn new(exe: PathBuf, packaged: bool) -> FirewallWatcher {
        FirewallWatcher { exe, packaged, fw_rx: None, fw_checked: None, fw_blocked: false, fixing: None, unreachable: 0 }
    }

    /// The game is another program now (a starter handed over): checked again soon.
    pub fn set_exe(&mut self, exe: PathBuf) {
        self.exe = exe;
        self.fw_checked = None;
    }

    /// A fix is under way.
    pub fn fixing(&self) -> bool {
        self.fixing.is_some()
    }

    /// Picks up a fix's or a check's result, and starts a check when one is due. `engine`:
    /// PeerParty's engine runs in the game (its state says how many players it can't
    /// reach), without which there's nothing to check for.
    pub fn tick(&mut self, engine: bool) -> Option<FirewallEvent> {
        if let Some(rx) = &self.fixing {
            match rx.try_recv() {
                Ok((message, ok)) => {
                    self.fixing = None;
                    self.fw_checked = None;
                    return Some(FirewallEvent::Fixed { message, ok });
                }
                Err(TryRecvError::Empty) => return None,
                Err(_) => self.fixing = None,
            }
        }
        if let Some(rx) = &self.fw_rx {
            match rx.try_recv() {
                Ok(verdict) => {
                    self.fw_rx = None;
                    let newly_blocked = verdict.blocked() && !self.fw_blocked;
                    self.fw_blocked = verdict.blocked();
                    return Some(FirewallEvent::Checked { verdict, newly_blocked });
                }
                Err(TryRecvError::Empty) => return None,
                Err(_) => self.fw_rx = None,
            }
        }
        if self.packaged || !engine || self.fw_checked.is_some_and(|t| t.elapsed() < FIREWALL_EVERY) {
            return None;
        }
        self.fw_checked = Some(Instant::now());
        let exe = self.exe.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(firewall::assess(&firewall::rules(), &exe, firewall::profiles()));
        });
        self.fw_rx = Some(rx);
        None
    }

    /// Fixes the game's rules (or opens Windows' settings for them) off this thread; its
    /// result comes from `tick`. One at a time.
    pub fn fix(&mut self, public: bool) {
        if self.fixing.is_some() {
            return;
        }
        let peerparty = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("peerparty.exe"));
        self.fixing = Some(crate::ui::shared::spawn_firewall_fix(peerparty, self.exe.clone(), public, || {}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Nothing is checked for a packaged game or before the engine runs, and a fix's
    /// result is picked up before anything else.
    #[test]
    fn checks_only_when_there_is_something_to_check() {
        let mut w = FirewallWatcher::new(PathBuf::from("game.exe"), true);
        assert!(w.tick(true).is_none());
        assert!(w.fw_rx.is_none(), "a packaged game's rules aren't read");
        let mut w = FirewallWatcher::new(PathBuf::from("game.exe"), false);
        assert!(w.tick(false).is_none());
        assert!(w.fw_rx.is_none(), "no engine, no check");
        let (tx, rx) = channel();
        w.fixing = Some(rx);
        assert!(w.fixing());
        assert!(w.tick(true).is_none(), "waiting for the fix");
        tx.send(("done".to_string(), true)).unwrap();
        assert!(matches!(w.tick(true), Some(FirewallEvent::Fixed { ok: true, .. })));
        assert!(!w.fixing());
        w.set_exe(PathBuf::from("other.exe"));
        assert_eq!(w.exe, Path::new("other.exe"));
    }

    /// A block toasts the first time it's seen, not at every check.
    #[test]
    fn a_new_block_is_told_once() {
        let mut w = FirewallWatcher::new(PathBuf::from("game.exe"), false);
        let blocked = || firewall::Verdict { exe: "game.exe".into(), block_all: true, ..Default::default() };
        let mut seen = Vec::new();
        for v in [blocked(), blocked(), firewall::Verdict::default(), blocked()] {
            let (tx, rx) = channel();
            tx.send(v).unwrap();
            w.fw_rx = Some(rx);
            match w.tick(true) {
                Some(FirewallEvent::Checked { newly_blocked, .. }) => seen.push(newly_blocked),
                _ => panic!("a check's result"),
            }
        }
        assert_eq!(seen, [true, false, false, true]);
    }
}
