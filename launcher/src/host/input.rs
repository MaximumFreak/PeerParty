//! Keyboard and mouse while the overlay shows. Low-level keyboard and mouse hooks
//! (installed only while it shows, on a thread of their own so they always answer at
//! once) take every key press, click, wheel turn and mouse movement before any program
//! sees it, raw input and DirectInput included, and hand them to the overlay instead.
//! The game gets nothing until the overlay closes. The real cursor stays put; the overlay
//! draws its own and moves it by the mouse's movement.
//!
//! Left alone: the Windows keys, Alt+Tab, Ctrl+Alt+Del (which the hook can't see) and
//! the VK_GAMEPAD_* keys Windows makes from controllers for XAML apps (Game Bar).
//! The first two also close the overlay (the host sees the game lose the foreground).
//! So is the release of a key that was already down when the overlay opened (Shift of
//! Shift+Tab): taking it would leave Windows thinking the key is still held.
//!
//! A watchdog: the host's loop stamps a heartbeat every tick, and if it hasn't for 1.5 s
//! (stalled) the hooks stop taking input, so the PC never ends up without a keyboard and
//! mouse because of PeerParty.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    Key { vk: u32, down: bool },
    Move { dx: i32, dy: i32 },
    Button { left: bool, down: bool },
    Wheel(f32),
}

static QUEUE: Mutex<VecDeque<Input>> = Mutex::new(VecDeque::new());
static THREAD: AtomicU32 = AtomicU32::new(0);
static ON: AtomicBool = AtomicBool::new(false);
static ANCHOR_X: AtomicI32 = AtomicI32::new(0);
static ANCHOR_Y: AtomicI32 = AtomicI32::new(0);
static ALT: AtomicBool = AtomicBool::new(false);
/// Keys whose press the hook took, by virtual-key code.
static TAKEN: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static HEARTBEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const STALLED_MS: u64 = 1500;

fn now_ms() -> u64 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
}

/// The host is alive (called every tick).
pub fn heartbeat() {
    HEARTBEAT.store(now_ms(), Ordering::Relaxed);
}

/// Whether input is being taken right now: capturing, and the host not stalled.
fn taking() -> bool {
    ON.load(Ordering::Relaxed) && now_ms().saturating_sub(HEARTBEAT.load(Ordering::Relaxed)) < STALLED_MS
}

const INSTALL: u32 = WM_APP + 1;
const REMOVE: u32 = WM_APP + 2;
/// Posted to the host's thread when input arrives, to wake its wait at once.
const WAKE: u32 = WM_APP + 3;
/// The host's thread, and whether a wake is already posted (one at a time: a mouse
/// sends hundreds of moves a second).
static HOST: AtomicU32 = AtomicU32::new(0);
static WOKEN: AtomicBool = AtomicBool::new(false);

/// Starts the hook thread (idle until `capture(true)`).
pub fn start() {
    if THREAD.load(Ordering::Relaxed) != 0 {
        return;
    }
    HOST.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("peerparty-input".into())
        .spawn(move || run(tx))
        .ok();
    if let Ok(id) = rx.recv() {
        THREAD.store(id, Ordering::Relaxed);
    }
}

/// Starts or stops taking keyboard and mouse input from everything else.
pub fn capture(on: bool) {
    heartbeat();
    if ON.swap(on, Ordering::Relaxed) == on {
        return;
    }
    let t = THREAD.load(Ordering::Relaxed);
    if t != 0 {
        unsafe { PostThreadMessageW(t, if on { INSTALL } else { REMOVE }, 0, 0) };
    }
    if !on {
        QUEUE.lock().unwrap().clear();
    }
}

/// Input taken since the last call, oldest first.
pub fn take() -> Vec<Input> {
    WOKEN.store(false, Ordering::Relaxed);
    QUEUE.lock().unwrap().drain(..).collect()
}

/// Whether input is waiting to be taken.
pub fn pending() -> bool {
    WOKEN.load(Ordering::Relaxed)
}

fn push(i: Input) {
    let mut q = QUEUE.lock().unwrap();
    if q.len() < 4096 {
        q.push_back(i);
    }
    drop(q);
    if !WOKEN.swap(true, Ordering::Relaxed) {
        let host = HOST.load(Ordering::Relaxed);
        if host != 0 {
            unsafe { PostThreadMessageW(host, WAKE, 0, 0) };
        }
    }
}

fn run(tx: std::sync::mpsc::Sender<u32>) {
    // A message queue must exist before anyone posts to this thread.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE) };
    let _ = tx.send(unsafe { GetCurrentThreadId() });
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let (mut kb, mut mouse): (HHOOK, HHOOK) = (std::ptr::null_mut(), std::ptr::null_mut());
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        match msg.message {
            INSTALL if kb.is_null() => {
                let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
                unsafe { GetCursorPos(&mut p) };
                ANCHOR_X.store(p.x, Ordering::Relaxed);
                ANCHOR_Y.store(p.y, Ordering::Relaxed);
                ALT.store(false, Ordering::Relaxed);
                for t in &TAKEN {
                    t.store(false, Ordering::Relaxed);
                }
                kb = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), module, 0) };
                mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0) };
            }
            REMOVE => {
                if !kb.is_null() {
                    unsafe { UnhookWindowsHookEx(kb) };
                }
                if !mouse.is_null() {
                    unsafe { UnhookWindowsHookEx(mouse) };
                }
                kb = std::ptr::null_mut();
                mouse = std::ptr::null_mut();
            }
            _ => unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }
}

const VK_TAB: u32 = 0x09;
const VK_MENU: u32 = 0x12;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
/// VK_GAMEPAD_A .. VK_GAMEPAD_RIGHT_THUMBSTICK_LEFT.
const VK_GAMEPAD_FIRST: u32 = 0xC3;
const VK_GAMEPAD_LAST: u32 = 0xDA;

unsafe extern "system" fn keyboard(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && taking() {
        let k = unsafe { &*(l as *const KBDLLHOOKSTRUCT) };
        let down = matches!(w as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let vk = k.vkCode;
        if matches!(vk, VK_MENU | VK_LMENU | VK_RMENU) {
            ALT.store(down, Ordering::Relaxed);
        }
        let system = matches!(vk, VK_LWIN | VK_RWIN) || (vk == VK_TAB && ALT.load(Ordering::Relaxed));
        let alt_key = matches!(vk, VK_MENU | VK_LMENU | VK_RMENU);
        // Controllers as keys (VK_GAMEPAD_*), which Windows sends to XAML apps such as
        // Game Bar to move around with: the overlay reads controllers itself, and
        // taking these left Game Bar deaf to the controller over an open overlay.
        let gamepad_key = (VK_GAMEPAD_FIRST..=VK_GAMEPAD_LAST).contains(&vk);
        if !system && !alt_key && !gamepad_key {
            push(Input::Key { vk, down });
            let taken = &TAKEN[(vk & 0xFF) as usize];
            if down {
                taken.store(true, Ordering::Relaxed);
                return 1;
            }
            if taken.swap(false, Ordering::Relaxed) {
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, w, l) }
}

unsafe extern "system" fn mouse_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && taking() {
        let m = unsafe { &*(l as *const MSLLHOOKSTRUCT) };
        match w as u32 {
            WM_MOUSEMOVE => {
                let dx = m.pt.x - ANCHOR_X.load(Ordering::Relaxed);
                let dy = m.pt.y - ANCHOR_Y.load(Ordering::Relaxed);
                if dx != 0 || dy != 0 {
                    push(Input::Move { dx, dy });
                }
            }
            WM_LBUTTONDOWN => push(Input::Button { left: true, down: true }),
            WM_LBUTTONUP => push(Input::Button { left: true, down: false }),
            WM_RBUTTONDOWN => push(Input::Button { left: false, down: true }),
            WM_RBUTTONUP => push(Input::Button { left: false, down: false }),
            WM_MOUSEWHEEL => {
                let delta = (m.mouseData >> 16) as u16 as i16;
                push(Input::Wheel(delta as f32 / 120.0));
            }
            _ => {}
        }
        return 1;
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, w, l) }
}
