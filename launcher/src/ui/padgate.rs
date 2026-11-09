//! Which controller presses the launcher window acts on.
//!
//! Controllers are read with XInput / HID, which see the buttons whatever window is in
//! front. The launcher only takes a press while its own window is the foreground window
//! (not merely egui's idea of focus), only after it has been in front for a moment, and
//! only for a button that went down while it was in front: a button held, or pressed
//! while a game was in front, has to come up and go down again. After the controller
//! starts a game, A / Cross is ignored for a few seconds (games take a while to show a
//! window, and the launcher stays in front until they do).

use std::time::{Duration, Instant};

use crate::host::pad;

/// How long the window must be in front before a press counts.
pub const SETTLE: Duration = Duration::from_millis(400);
/// How long after the controller started a game A / Cross is ignored.
pub const PLAY_COOLDOWN: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct PadGate {
    /// When the window last came to the front (None while it isn't in front).
    front_since: Option<Instant>,
    /// Buttons that have to come up before they count again.
    latched: u32,
    /// The buttons that counted last poll.
    prev: u32,
    /// A / Cross is ignored until then.
    play_block: Option<Instant>,
}

impl PadGate {
    /// The buttons that count this poll: (held, newly pressed). Nothing counts while the
    /// window isn't in front.
    pub fn filter(&mut self, front: bool, held: u32, now: Instant) -> (u32, u32) {
        if !front {
            self.front_since = None;
            self.prev = 0;
            return (0, 0);
        }
        let since = *self.front_since.get_or_insert(now);
        // Released buttons may count again; anything down while settling or in the
        // play cooldown may not, until it comes up.
        self.latched &= held;
        if now.duration_since(since) < SETTLE {
            self.latched |= held;
        }
        if self.play_block.is_some_and(|t| now < t) {
            self.latched |= held & pad::SOUTH;
        }
        let ok = held & !self.latched;
        let pressed = ok & !self.prev;
        self.prev = ok;
        (ok, pressed)
    }

    /// The controller just started a game.
    pub fn played(&mut self, now: Instant) {
        self.play_block = Some(now + PLAY_COOLDOWN);
        self.latched |= pad::SOUTH;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pad::{DOWN, SOUTH};

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    #[test]
    fn nothing_counts_in_the_background() {
        let mut g = PadGate::default();
        let t = Instant::now();
        assert_eq!(g.filter(false, SOUTH, t), (0, 0));
        assert_eq!(g.filter(false, 0, ms(t, 50)), (0, 0));
        assert_eq!(g.filter(false, SOUTH, ms(t, 100)), (0, 0));
    }

    #[test]
    fn a_button_held_across_coming_to_the_front_needs_a_new_press() {
        let mut g = PadGate::default();
        let t = Instant::now();
        g.filter(false, SOUTH, t);
        // Comes to the front with A held, and A stays held well past the settle time.
        assert_eq!(g.filter(true, SOUTH, ms(t, 10)).1, 0);
        assert_eq!(g.filter(true, SOUTH, ms(t, 1000)).1, 0);
        // Up, then down again: that one counts.
        assert_eq!(g.filter(true, 0, ms(t, 1050)).1, 0);
        assert_eq!(g.filter(true, SOUTH, ms(t, 1100)).1, SOUTH);
    }

    #[test]
    fn presses_while_settling_are_ignored() {
        let mut g = PadGate::default();
        let t = Instant::now();
        g.filter(true, 0, t);
        assert_eq!(g.filter(true, DOWN, ms(t, 100)), (0, 0));
        assert_eq!(g.filter(true, 0, ms(t, 150)), (0, 0));
        assert_eq!(g.filter(true, SOUTH, ms(t, 200)), (0, 0));
        // Still held after settling: still ignored until it comes up.
        assert_eq!(g.filter(true, SOUTH, ms(t, 600)), (0, 0));
        g.filter(true, 0, ms(t, 650));
        assert_eq!(g.filter(true, DOWN, ms(t, 700)), (DOWN, DOWN));
    }

    #[test]
    fn leaving_the_front_resets_the_settle_time() {
        let mut g = PadGate::default();
        let t = Instant::now();
        g.filter(true, 0, t);
        assert_eq!(g.filter(true, SOUTH, ms(t, 500)).1, SOUTH);
        g.filter(true, 0, ms(t, 550));
        g.filter(false, 0, ms(t, 600));
        // Back in front: settling again.
        assert_eq!(g.filter(true, SOUTH, ms(t, 700)).1, 0);
        g.filter(true, 0, ms(t, 1200));
        assert_eq!(g.filter(true, SOUTH, ms(t, 1250)).1, SOUTH);
    }

    #[test]
    fn a_is_ignored_for_a_while_after_the_controller_starts_a_game() {
        let mut g = PadGate::default();
        let t = Instant::now();
        g.filter(true, 0, t);
        assert_eq!(g.filter(true, SOUTH, ms(t, 500)).1, SOUTH);
        g.played(ms(t, 500));
        // Mashing A while the game starts does nothing; the d-pad still works.
        for i in 0..20 {
            let at = ms(t, 600 + i * 200);
            g.filter(true, 0, at);
            assert_eq!(g.filter(true, SOUTH | DOWN, ms(at, 100)).1, DOWN);
        }
        // Held through the end of the cooldown: still needs a new press.
        assert_eq!(g.filter(true, SOUTH, ms(t, 5600)).1, 0);
        g.filter(true, 0, ms(t, 5700));
        assert_eq!(g.filter(true, SOUTH, ms(t, 5800)).1, SOUTH);
    }
}