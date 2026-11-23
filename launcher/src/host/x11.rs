//! The Linux session's window (compositor.rs): an X11 window, full screen, that draws its
//! copies' frames itself, as the Windows one does (gpu.rs). Under Gamescope (SteamOS's
//! game mode) and on a Wayland desktop it's an XWayland window, as the games' are.
//!
//! Each copy's hook reads its frames back into a pixel file (`HostMsg::Capture`'s
//! `pixels`); a thread of its own owns the X connection, the window and a Vulkan device
//! (vkblit.rs), and once a refresh takes each copy's newest frame from its ring block,
//! stages it, blits it into its pane and presents with vsync (FIFO). There is no frame
//! event across Wine: a ring is looked at each time round.
//!
//! libX11 and Vulkan are loaded at run time: without an X display (or Vulkan) there's no
//! window, and the session runs without one as before.
//!
//! Under Gamescope the window is marked an external overlay (`GAMESCOPE_EXTERNAL_OVERLAY`)
//! so it shows over the games' own windows, and carries the game's Steam app id
//! (`STEAM_GAME`). Neither has been tried on a Steam Deck yet.

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_ulong, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use peerparty_protocol::session::PaneRect;

use super::compositor::MonitorBounds;
use super::ring::{PixelFrames, RingBlock};
use super::vkblit::{BlitPane, Blitter, PRESENT_SRC, Pfn, UNDEFINED, vkfn};

type Display = c_void;
type Window = c_ulong;
type Atom = c_ulong;

/// The libX11 functions the window uses.
struct X {
    open: unsafe extern "C" fn(*const c_char) -> *mut Display,
    close: unsafe extern "C" fn(*mut Display) -> c_int,
    default_screen: unsafe extern "C" fn(*mut Display) -> c_int,
    root: unsafe extern "C" fn(*mut Display, c_int) -> Window,
    width: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    height: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
    black: unsafe extern "C" fn(*mut Display, c_int) -> c_ulong,
    create: unsafe extern "C" fn(*mut Display, Window, c_int, c_int, u32, u32, u32, c_ulong, c_ulong) -> Window,
    destroy: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    store_name: unsafe extern "C" fn(*mut Display, Window, *const c_char) -> c_int,
    select_input: unsafe extern "C" fn(*mut Display, Window, c_long) -> c_int,
    intern: unsafe extern "C" fn(*mut Display, *const c_char, c_int) -> Atom,
    set_protocols: unsafe extern "C" fn(*mut Display, Window, *mut Atom, c_int) -> c_int,
    change_property: unsafe extern "C" fn(*mut Display, Window, Atom, Atom, c_int, c_int, *const u8, c_int) -> c_int,
    map: unsafe extern "C" fn(*mut Display, Window) -> c_int,
    flush: unsafe extern "C" fn(*mut Display) -> c_int,
    pending: unsafe extern "C" fn(*mut Display) -> c_int,
    next_event: unsafe extern "C" fn(*mut Display, *mut [c_long; 24]) -> c_int,
    _lib: Lib,
}

struct Lib(*mut c_void);

impl Drop for Lib {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.0) };
    }
}

impl X {
    fn load() -> Option<X> {
        let lib = [c"libX11.so.6", c"libX11.so"].iter().map(|n| unsafe { libc::dlopen(n.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) }).find(|h| !h.is_null())?;
        let lib = Lib(lib);
        macro_rules! f {
            ($n:literal) => {{
                let p = unsafe { libc::dlsym(lib.0, $n.as_ptr()) };
                if p.is_null() {
                    return None;
                }
                unsafe { std::mem::transmute::<*mut c_void, _>(p) }
            }};
        }
        Some(X {
            open: f!(c"XOpenDisplay"),
            close: f!(c"XCloseDisplay"),
            default_screen: f!(c"XDefaultScreen"),
            root: f!(c"XRootWindow"),
            width: f!(c"XDisplayWidth"),
            height: f!(c"XDisplayHeight"),
            black: f!(c"XBlackPixel"),
            create: f!(c"XCreateSimpleWindow"),
            destroy: f!(c"XDestroyWindow"),
            store_name: f!(c"XStoreName"),
            select_input: f!(c"XSelectInput"),
            intern: f!(c"XInternAtom"),
            set_protocols: f!(c"XSetWMProtocols"),
            change_property: f!(c"XChangeProperty"),
            map: f!(c"XMapWindow"),
            flush: f!(c"XFlush"),
            pending: f!(c"XPending"),
            next_event: f!(c"XNextEvent"),
            _lib: lib,
        })
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

const STRUCTURE_NOTIFY: c_long = 1 << 17;
const CONFIGURE_NOTIFY: c_int = 22;
const CLIENT_MESSAGE: c_int = 33;
const XA_ATOM: Atom = 4;
const XA_CARDINAL: Atom = 6;

/// A pane the window draws: where, and the copy's frames and ring.
#[derive(Clone)]
struct Want {
    dest: PaneRect,
    ring: Option<Arc<RingBlock>>,
    frames: Option<Arc<PixelFrames>>,
}

struct Shared {
    want: Mutex<(u64, Vec<Want>)>,
    live: Mutex<Vec<bool>>,
    size: Mutex<(u32, u32)>,
    resized: AtomicBool,
    close: AtomicBool,
    quit: AtomicBool,
}

/// The window and its drawing thread.
pub struct LinuxWindow {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    surfaces: Vec<(Option<Arc<RingBlock>>, Option<Arc<PixelFrames>>)>,
    dests: Vec<PaneRect>,
}

impl LinuxWindow {
    /// A full-screen window titled `title` and its thread, or None without an X display
    /// or a Vulkan device that can present to it.
    pub fn open(bounds: MonitorBounds, title: &str) -> Option<LinuxWindow> {
        let shared = Arc::new(Shared {
            want: Mutex::new((0, Vec::new())),
            live: Mutex::new(Vec::new()),
            size: Mutex::new((bounds.width, bounds.height)),
            resized: AtomicBool::new(false),
            close: AtomicBool::new(false),
            quit: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel::<bool>();
        let s = shared.clone();
        let title = CString::new(title.replace('\0', "")).unwrap_or_default();
        let thread = std::thread::Builder::new().name("peerparty-compositor".into()).spawn(move || run(&s, &title, &tx)).ok()?;
        if !rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false) {
            shared.quit.store(true, Ordering::Release);
            let _ = thread.join();
            eprintln!("peerparty: session: no window of its own (no X display, or no Vulkan device that can show one); the copies show themselves");
            return None;
        }
        Some(LinuxWindow { shared, thread: Some(thread), surfaces: Vec::new(), dests: Vec::new() })
    }

    /// The window's size now, as a monitor's bounds.
    pub fn client_bounds(&self) -> MonitorBounds {
        let (width, height) = *lock(&self.shared.size);
        MonitorBounds { x: 0, y: 0, width, height, dpi: 96 }
    }

    /// Whether it was resized since the last look.
    pub fn take_resized(&self) -> bool {
        self.shared.resized.swap(false, Ordering::AcqRel)
    }

    pub fn take_close_requested(&self) -> bool {
        self.shared.close.swap(false, Ordering::AcqRel)
    }

    /// Copy `i`'s frames (None: it has none to show), and its ring block.
    pub fn set_pixels(&mut self, i: usize, frames: Option<Arc<PixelFrames>>, ring: Option<Arc<RingBlock>>) {
        if self.surfaces.len() <= i {
            self.surfaces.resize(i + 1, (None, None));
        }
        self.surfaces[i] = (ring, frames);
        self.push();
    }

    /// Where each copy is shown (in the window).
    pub fn show(&mut self, dests: Vec<PaneRect>) {
        if dests != self.dests {
            self.dests = dests;
            self.push();
        }
    }

    fn push(&self) {
        let n = self.dests.len().max(self.surfaces.len());
        let wants: Vec<Want> = (0..n)
            .map(|i| {
                let (ring, frames) = self.surfaces.get(i).cloned().unwrap_or((None, None));
                Want { dest: self.dests.get(i).copied().unwrap_or(PaneRect { x: 0, y: 0, width: 0, height: 0 }), ring, frames }
            })
            .collect();
        let mut w = lock(&self.shared.want);
        w.0 += 1;
        w.1 = wants;
    }

    /// Whether pane `i` is drawn (its copy's frames have come).
    #[allow(dead_code)]
    pub fn live(&self, i: usize) -> bool {
        lock(&self.shared.live).get(i).copied().unwrap_or(false)
    }
}

impl Drop for LinuxWindow {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The swap chain and what goes with it.
struct Swap {
    swap: u64,
    images: Vec<u64>,
    /// Per image: the semaphore the present waits on.
    rendered: Vec<u64>,
    acquired: u64,
    size: (u32, u32),
}

struct SwapFns {
    caps: unsafe extern "system" fn(usize, u64, *mut [u32; 13]) -> i32,
    formats: unsafe extern "system" fn(usize, u64, *mut u32, *mut [u32; 2]) -> i32,
    create: unsafe extern "system" fn(usize, *const SwapchainInfo, usize, *mut u64) -> i32,
    destroy: unsafe extern "system" fn(usize, u64, usize),
    images: unsafe extern "system" fn(usize, u64, *mut u32, *mut u64) -> i32,
    acquire: unsafe extern "system" fn(usize, u64, u64, u64, u64, *mut u32) -> i32,
    present: unsafe extern "system" fn(usize, *const PresentInfo) -> i32,
    destroy_surface: unsafe extern "system" fn(usize, u64, usize),
}

#[repr(C)]
struct SwapchainInfo {
    s_type: u32,
    next: usize,
    flags: u32,
    surface: u64,
    min_images: u32,
    format: u32,
    color_space: u32,
    width: u32,
    height: u32,
    layers: u32,
    usage: u32,
    sharing: u32,
    family_count: u32,
    families: usize,
    transform: u32,
    alpha: u32,
    mode: u32,
    clipped: u32,
    old: u64,
}

#[repr(C)]
struct PresentInfo {
    s_type: u32,
    next: usize,
    wait_count: u32,
    waits: *const u64,
    swap_count: u32,
    swaps: *const u64,
    indices: *const u32,
    results: *mut i32,
}

const OUT_OF_DATE: i32 = -1_000_001_004;
const SUBOPTIMAL: i32 = 1_000_001_003;

/// A swap chain for `surface` at `size` (the surface's own size where it says), after
/// `old` (whose objects go).
fn make_swap(b: &Blitter, f: &SwapFns, surface: u64, size: (u32, u32), old: Option<Swap>) -> Option<Swap> {
    let mut caps = [0u32; 13];
    unsafe { (f.caps)(b.phys, surface, &mut caps) };
    if caps[12] & 0x2 == 0 {
        eprintln!("peerparty: session: the window's swap chain can't be copied into");
        return None;
    }
    let mut n = 0u32;
    unsafe { (f.formats)(b.phys, surface, &mut n, std::ptr::null_mut()) };
    let mut formats = vec![[0u32; 2]; n as usize];
    unsafe { (f.formats)(b.phys, surface, &mut n, formats.as_mut_ptr()) };
    // BGRA or RGBA, UNORM, sRGB-encoded: what the frames are (a blit doesn't re-encode).
    let format = formats.iter().take(n as usize).find(|f| f[0] == 44 && f[1] == 0).or_else(|| formats.iter().find(|f| f[0] == 37 && f[1] == 0)).copied()?;
    let extent = if caps[2] != u32::MAX { (caps[2], caps[3]) } else { (size.0.clamp(caps[4], caps[6].max(1)), size.1.clamp(caps[5], caps[7].max(1))) };
    if extent.0 == 0 || extent.1 == 0 {
        return None;
    }
    let mut min_images = caps[0].max(2);
    if caps[1] != 0 {
        min_images = min_images.min(caps[1]);
    }
    let alpha = if caps[11] & 1 != 0 { 1 } else { caps[11] & caps[11].wrapping_neg() };
    let old_swap = old.as_ref().map_or(0, |o| o.swap);
    let info = SwapchainInfo {
        s_type: 1000001000,
        next: 0,
        flags: 0,
        surface,
        min_images,
        format: format[0],
        color_space: format[1],
        width: extent.0,
        height: extent.1,
        layers: 1,
        usage: 0x2, // TRANSFER_DST
        sharing: 0,
        family_count: 0,
        families: 0,
        transform: caps[10],
        alpha,
        mode: 2, // FIFO: vsync
        clipped: 1,
        old: old_swap,
    };
    let mut swap = 0u64;
    let made = unsafe { (f.create)(b.dev, &info, 0, &mut swap) } == 0;
    if let Some(o) = old {
        drop_swap(b, f, o);
    }
    if !made {
        return None;
    }
    let mut count = 0u32;
    unsafe { (f.images)(b.dev, swap, &mut count, std::ptr::null_mut()) };
    let mut images = vec![0u64; count as usize];
    unsafe { (f.images)(b.dev, swap, &mut count, images.as_mut_ptr()) };
    let sem = || {
        let mut s = 0u64;
        unsafe { (b.fx.create_semaphore)(b.dev, &[9, 0, 0], 0, &mut s) };
        s
    };
    let rendered = images.iter().map(|_| sem()).collect();
    Some(Swap { swap, images, rendered, acquired: sem(), size: extent })
}

fn drop_swap(b: &Blitter, f: &SwapFns, s: Swap) {
    unsafe {
        (b.fx.device_idle)(b.dev);
        for sem in s.rendered.into_iter().chain([s.acquired]) {
            (b.fx.destroy_semaphore)(b.dev, sem, 0);
        }
        (f.destroy)(b.dev, s.swap, 0);
    }
}

/// The window's thread: the window, its device and swap chain, and the drawing loop.
/// `ready` says whether the window could be made.
fn run(shared: &Shared, title: &CStr, ready: &mpsc::Sender<bool>) {
    let Some(x) = X::load() else {
        let _ = ready.send(false);
        return;
    };
    let dpy = unsafe { (x.open)(std::ptr::null()) };
    if dpy.is_null() {
        let _ = ready.send(false);
        return;
    }
    let screen = unsafe { (x.default_screen)(dpy) };
    let (w, h) = unsafe { ((x.width)(dpy, screen).max(1) as u32, (x.height)(dpy, screen).max(1) as u32) };
    let win = unsafe { (x.create)(dpy, (x.root)(dpy, screen), 0, 0, w, h, 0, 0, (x.black)(dpy, screen)) };
    let atom = |n: &CStr| unsafe { (x.intern)(dpy, n.as_ptr(), 0) };
    let mut delete = atom(c"WM_DELETE_WINDOW");
    unsafe {
        (x.store_name)(dpy, win, title.as_ptr());
        (x.set_protocols)(dpy, win, &mut delete, 1);
        let fullscreen = atom(c"_NET_WM_STATE_FULLSCREEN");
        (x.change_property)(dpy, win, atom(c"_NET_WM_STATE"), XA_ATOM, 32, 0, (&fullscreen as *const Atom).cast(), 1);
        if std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some() {
            let one: c_ulong = 1;
            (x.change_property)(dpy, win, atom(c"GAMESCOPE_EXTERNAL_OVERLAY"), XA_CARDINAL, 32, 0, (&one as *const c_ulong).cast(), 1);
        }
        if let Some(app) = ["SteamGameId", "SteamAppId"].iter().find_map(|k| std::env::var(k).ok()?.trim().parse::<c_ulong>().ok()).filter(|a| *a != 0) {
            (x.change_property)(dpy, win, atom(c"STEAM_GAME"), XA_CARDINAL, 32, 0, (&app as *const c_ulong).cast(), 1);
        }
        (x.select_input)(dpy, win, STRUCTURE_NOTIFY);
        (x.map)(dpy, win);
        (x.flush)(dpy);
    }
    *lock(&shared.size) = (w, h);

    // Vulkan, presenting to the window.
    type CreateXlib = unsafe extern "system" fn(usize, *const [usize; 5], usize, *mut u64) -> i32;
    type Support = unsafe extern "system" fn(usize, u32, u64, *mut u32) -> i32;
    let made = Blitter::new(
        &[c"VK_KHR_surface", c"VK_KHR_xlib_surface"],
        &[c"VK_KHR_swapchain"],
        |vk, instance| {
            let Some(create) = vkfn!(vk.instance_fn(instance, c"vkCreateXlibSurfaceKHR"), CreateXlib) else { return 0 };
            // VkXlibSurfaceCreateInfoKHR: sType, pNext, flags, dpy, window.
            let info: [usize; 5] = [1000004000, 0, 0, dpy as usize, win as usize];
            let mut s = 0u64;
            unsafe { create(instance, &info, 0, &mut s) };
            s
        },
        |vk, instance, phys, family, surface| {
            let Some(support) = vkfn!(vk.instance_fn(instance, c"vkGetPhysicalDeviceSurfaceSupportKHR"), Support) else { return false };
            let mut yes = 0u32;
            surface != 0 && unsafe { support(phys, family, surface, &mut yes) } == 0 && yes != 0
        },
    );
    let Some((mut b, surface)) = made else {
        unsafe {
            (x.destroy)(dpy, win);
            (x.close)(dpy);
        }
        let _ = ready.send(false);
        return;
    };
    let gdpa = b.vk.instance_fn(b.instance, c"vkGetDeviceProcAddr");
    type Gdpa = unsafe extern "system" fn(usize, *const c_char) -> Pfn;
    let fns = (|| {
        let gdpa = vkfn!(gdpa, Gdpa)?;
        let d = |n: &CStr| unsafe { gdpa(b.dev, n.as_ptr()) };
        let i = |n: &CStr| b.vk.instance_fn(b.instance, n);
        Some(SwapFns {
            caps: vkfn!(i(c"vkGetPhysicalDeviceSurfaceCapabilitiesKHR"), _)?,
            formats: vkfn!(i(c"vkGetPhysicalDeviceSurfaceFormatsKHR"), _)?,
            destroy_surface: vkfn!(i(c"vkDestroySurfaceKHR"), _)?,
            create: vkfn!(d(c"vkCreateSwapchainKHR"), _)?,
            destroy: vkfn!(d(c"vkDestroySwapchainKHR"), _)?,
            images: vkfn!(d(c"vkGetSwapchainImagesKHR"), _)?,
            acquire: vkfn!(d(c"vkAcquireNextImageKHR"), _)?,
            present: vkfn!(d(c"vkQueuePresentKHR"), _)?,
        })
    })();
    let Some(f) = fns else {
        drop(b);
        unsafe {
            (x.destroy)(dpy, win);
            (x.close)(dpy);
        }
        let _ = ready.send(false);
        return;
    };
    let mut swap = make_swap(&b, &f, surface, (w, h), None);
    let _ = ready.send(swap.is_some());
    if swap.is_none() {
        unsafe { (f.destroy_surface)(b.instance, surface, 0) };
        drop(b);
        unsafe {
            (x.destroy)(dpy, win);
            (x.close)(dpy);
        }
        return;
    }
    if let Some(s) = &swap {
        *lock(&shared.size) = s.size;
    }

    let mut wants: Vec<Want> = Vec::new();
    let mut seen = u64::MAX;
    let mut takes: Vec<Option<(usize, u32)>> = Vec::new();
    let mut dirty = true;
    let mut event = [0 as c_long; 24];
    while !shared.quit.load(Ordering::Acquire) {
        // The window's events.
        let mut resize = None;
        while unsafe { (x.pending)(dpy) } > 0 {
            unsafe { (x.next_event)(dpy, &mut event) };
            let kind = event[0] as c_int;
            if kind == CONFIGURE_NOTIFY {
                // XConfigureEvent: width and height are the ints at bytes 56 and 60.
                let words = unsafe { std::slice::from_raw_parts(event.as_ptr().cast::<c_int>(), 48) };
                resize = Some((words[14].max(1) as u32, words[15].max(1) as u32));
            } else if kind == CLIENT_MESSAGE && event[7] as Atom == delete {
                // XClientMessageEvent: data.l[0], the protocol, is the long at byte 56.
                shared.close.store(true, Ordering::Release);
            }
        }
        if let Some(size) = resize
            && swap.as_ref().is_none_or(|s| s.size != size)
        {
            b.wait();
            swap = make_swap(&b, &f, surface, size, swap.take());
            *lock(&shared.size) = swap.as_ref().map_or(size, |s| s.size);
            shared.resized.store(true, Ordering::Release);
            dirty = true;
        }
        // What the session wants.
        {
            let w = lock(&shared.want);
            if w.0 != seen {
                seen = w.0;
                for (i, (old, new)) in wants.iter().zip(w.1.iter()).enumerate() {
                    let same = match (&old.frames, &new.frames) {
                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                        (None, None) => true,
                        _ => false,
                    };
                    if !same {
                        takes[i] = None;
                        b.forget(i);
                        if new.frames.is_none()
                            && let Some(r) = &old.ring
                        {
                            r.header().stop_reading();
                        }
                    }
                }
                wants = w.1.clone();
                takes.resize(wants.len(), None);
                dirty = true;
            }
        }
        // Each copy's newest frame.
        let mut fresh = false;
        for (i, want) in wants.iter().enumerate() {
            let (Some(frames), Some(ring)) = (&want.frames, &want.ring) else { continue };
            if let Some(p) = ring.header().take(frames.generation) {
                if takes[i] != Some((p.slot, p.seq)) {
                    fresh = true;
                }
                takes[i] = Some((p.slot, p.seq));
            }
        }
        {
            let live: Vec<bool> = wants.iter().zip(&takes).map(|(w, t)| w.frames.is_some() && t.is_some()).collect();
            *lock(&shared.live) = live;
        }
        let Some(s) = swap.as_ref() else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        if !fresh && !dirty {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        b.wait();
        let mut index = 0u32;
        let r = unsafe { (f.acquire)(b.dev, s.swap, 1_000_000_000, s.acquired, 0, &mut index) };
        if r == OUT_OF_DATE {
            let size = s.size;
            swap = make_swap(&b, &f, surface, size, swap.take());
            continue;
        }
        if r != 0 && r != SUBOPTIMAL {
            continue;
        }
        let panes: Vec<BlitPane> = wants.iter().zip(&takes).map(|(w, t)| BlitPane { dest: w.dest, frames: w.frames.clone(), take: *t }).collect();
        let target = s.images[index as usize];
        let drawn = b.record(target, s.size, UNDEFINED, PRESENT_SRC, &panes);
        let rendered = s.rendered[index as usize];
        if !b.submit(s.acquired, rendered) {
            continue;
        }
        let pi = PresentInfo { s_type: 1000001001, next: 0, wait_count: 1, waits: &rendered, swap_count: 1, swaps: &s.swap, indices: &index, results: std::ptr::null_mut() };
        let r = unsafe { (f.present)(b.queue, &pi) };
        for i in drawn {
            if let Some(ring) = &wants[i].ring {
                ring.header().presented();
            }
        }
        dirty = false;
        if r == OUT_OF_DATE || r == SUBOPTIMAL {
            b.wait();
            let size = *lock(&shared.size);
            swap = make_swap(&b, &f, surface, size, swap.take());
        }
    }
    for w in &wants {
        if let Some(r) = &w.ring {
            r.header().stop_reading();
        }
    }
    b.wait();
    if let Some(s) = swap.take() {
        drop_swap(&b, &f, s);
    }
    unsafe { (f.destroy_surface)(b.instance, surface, 0) };
    drop(b);
    unsafe {
        (x.destroy)(dpy, win);
        (x.close)(dpy);
    }
}
