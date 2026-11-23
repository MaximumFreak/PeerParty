//! The overlay's settings changes, saved into ui.json off the overlay's thread (reading
//! and writing it takes a few milliseconds), only the latest when changes come faster.
//!
//! Every copy of a game hosts its own overlay, and the launcher window saves too: each
//! save is `Settings::update`, a read, a merge and a write under ui.json's lock, and what
//! goes in is only what the overlay changed since its last save
//! (`Settings::apply_overlay_edit`), so two overlays, or an overlay and the window,
//! saving at once each keep the other's changes.

use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

use crate::ui::store::{Prefs, Settings};

/// Saves the overlay's prefs on a thread of its own (`save`), and waits for the last one
/// when the game ends (`finish`).
pub struct PrefsSync {
    tx: Option<Sender<(String, Prefs)>>,
    thread: Option<JoinHandle<()>>,
}

impl PrefsSync {
    /// `prefs`: the overlay's settings as the session starts, what its first save's
    /// changes are told from.
    pub fn start(prefs: &Prefs) -> PrefsSync {
        let (tx, rx) = channel::<(String, Prefs)>();
        let mut base = prefs.clone();
        let thread = std::thread::Builder::new()
            .name("peerparty-save".into())
            .spawn(move || {
                while let Ok(mut job) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        job = newer;
                    }
                    let (key, prefs) = job;
                    Settings::update(|s| s.apply_overlay_edit(&key, &base, &prefs));
                    base = prefs;
                }
            })
            .ok();
        PrefsSync { tx: Some(tx), thread }
    }

    /// Saves the overlay's `prefs` for the game `key` (soon, on the saving thread).
    pub fn save(&self, key: &str, prefs: &Prefs) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((key.to_string(), prefs.clone()));
        }
    }

    /// Waits for the last save (the process ends when the game does).
    pub fn finish(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PrefsSync {
    fn drop(&mut self) {
        self.finish();
    }
}
