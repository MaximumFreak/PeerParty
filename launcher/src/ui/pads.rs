//! Controllers for the window, read on a thread of their own so the window can sleep:
//! the thread reads them only while the window is in front (`set_front`), about 60
//! times a second, and wakes the window only when a button changes (or a direction is
//! held, for its repeat). With the window behind a game it only checks a flag.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::host::pad::{self, DeviceInfo, Style};

pub struct PadThread {
    rx: Receiver<(u32, Style)>,
    front: Arc<AtomicBool>,
    devices: Arc<Mutex<Vec<DeviceInfo>>>,
    held: u32,
    pub style: Style,
}

impl PadThread {
    pub fn start(ctx: egui::Context) -> PadThread {
        let (tx, rx) = channel();
        let front = Arc::new(AtomicBool::new(false));
        let devices = Arc::new(Mutex::new(Vec::<DeviceInfo>::new()));
        let f = front.clone();
        let devs_shared = devices.clone();
        let _ = std::thread::Builder::new().name("pads".into()).spawn(move || {
            let mut pads = pad::Pads::new();
            let mut last = 0u32;
            let mut last_devs: Vec<DeviceInfo> = Vec::new();
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
                let now_devs = pads.device_list();
                let devs_changed = now_devs != last_devs;
                if devs_changed {
                    last_devs = now_devs.clone();
                    *devs_shared.lock().unwrap_or_else(|e| e.into_inner()) = now_devs;
                }
                let dirs = b & (pad::UP | pad::DOWN | pad::LEFT | pad::RIGHT);
                if b != last {
                    last = b;
                    if tx.send((b, pads.style)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                } else if devs_changed {
                    ctx.request_repaint();
                } else if dirs != 0 {
                    // A held direction repeats in the window: keep it ticking.
                    ctx.request_repaint_after(Duration::from_millis(40));
                }
                std::thread::sleep(Duration::from_millis(16));
            }
        });
        PadThread { rx, front, devices, held: 0, style: Style::Xbox }
    }

    /// Whether the window is in front (the thread reads controllers only then).
    pub fn set_front(&self, front: bool) {
        self.front.store(front, Ordering::Relaxed);
    }

    /// Connected controllers, wheels, and sticks as last polled.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.devices.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// First currently active connected device (button down, stick/wheel moved, or pedal pressed).
    pub fn active_device(&self) -> Option<DeviceInfo> {
        self.devices.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|d| d.active).cloned()
    }

    /// The buttons held now, and any that went down and up again since the last look:
    /// a quick tap while the window was busy (both changes waiting) still counts as a
    /// press, held for this one look.
    pub fn held(&mut self) -> u32 {
        let mut seen = self.held;
        while let Ok((b, style)) = self.rx.try_recv() {
            seen |= b;
            self.held = b;
            self.style = style;
        }
        seen
    }
}
