//! core's integration tests, one binary: its tests run side by side (seven binaries ran
//! one after another, and linked seven times). Each module claims its own block of ports
//! (core/src/test_ports.rs) and uses game keys of its own, so they don't meet.
//!
//! One module: `cargo test -p peerparty_core --test integration public::`.
//!
//! Each test takes one of this PC's turns for heavy tests before its engines start
//! (`take_turn`, core/src/test_ports.rs), so two runs side by side, or cargo test beside
//! nextest, don't crowd each other into missed deadlines.

mod fwd;
mod internet;
mod internet_switch;
mod link_drop;
mod migrate;
mod public;
mod two_peers;

thread_local! {
    static TURN: std::cell::OnceCell<peerparty_core::test_ports::Turn> = const { std::cell::OnceCell::new() };
}

/// This test's turn (`test_ports::turn`), taken once per test thread and given back when
/// the thread ends with the test. The modules' `start` helpers take it.
pub fn take_turn() {
    TURN.with(|t| {
        t.get_or_init(peerparty_core::test_ports::turn);
    });
}
