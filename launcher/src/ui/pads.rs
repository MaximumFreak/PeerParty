//! Controllers for the window, read on a thread of their own so the window can sleep:
//! the thread reads them only while the window is in front (`set_front`), about 60
//! times a second, and wakes the window only when a button changes (or a direction is
//! held, for its repeat). With the window behind a game it only checks a flag.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;

use crate::host::pad::{self, Style};

pub struct PadThread {
    rx: Receiver<(u32, Style)>,
    front: Arc<AtomicBool>,
    held: u32,
    pub style: Style,
}

impl PadThread {
    pub fn start(ctx: egui::Context) -> PadThread {
        let (tx, rx) = channel();
        let front = Arc::new(AtomicBool::new(false));
        let f = front.clone();
        let _ = std::thread::Builder::new().name("pads".into()).spawn(move || {
            let mut pads = pad::Pads::new();
            let mut last = 0u32;
            loop {
                if !f.load(Ordering::Relaxed) {
                    if last != 0 {
                        last = 0;
                        if tx.send((0, pads.style)).is_err() {
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                let b = pads.poll();
                let dirs = b & (pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT);
                if b != last {
                    last = b;
                    if tx.send((b, pads.style)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                } else if dirs != 0 {
                    // A held direction repeats in the window: keep it ticking.
                    ctx.request_repaint_after(Duration::from_millis(40));
                }
                std::thread::sleep(Duration::from_millis(16));
            }
        });
        PadThread { rx, front, held: 0, style: Style::Xbox }
    }

    /// Whether the window is in front (the thread reads controllers only then).
    pub fn set_front(&self, front: bool) {
        self.front.store(front, Ordering::Relaxed);
    }

    /// The buttons held now.
    pub fn held(&mut self) -> u32 {
        while let Ok((b, style)) = self.rx.try_recv() {
            self.held = b;
            self.style = style;
        }
        self.held
    }
}
