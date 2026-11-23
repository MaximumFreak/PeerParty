//! Module watch. Static imports are already mapped when the hook DLL's entry
//! runs (the launcher's APC is after the loader maps them, before the entry
//! point), so the first pass walks the PEB. Later loads come from
//! `LdrRegisterDllNotification`, which runs before `LoadLibrary` returns to the
//! game.
//!
//! A packed DLL is mapped with its code still compressed and unpacks itself in its
//! DllMain, so it can't be hooked before that; PeerParty doesn't unpack it. Such a module
//! (its exports' first bytes all zero) waits, and is hooked once it has unpacked itself: when the game first looks up one of its functions (GetProcAddress on it,
//! which a packer never does on itself mid-unpack), or by a short-lived poller once its
//! code has stayed in place for two polls, whichever comes first.

pub(crate) mod sdk;

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{LazyLock, Mutex};
use std::{collections::HashMap, collections::HashSet, mem, ptr};

use windows_sys::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleW, GetProcAddress,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

use crate::log;
use crate::sys::win::wide;
use crate::sys::pe;

struct Mod {
    base: *mut core::ffi::c_void,
    full: String,
    name: String,
}

static SEEN: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
/// Packed modules waiting to unpack: (base, full path, name, seen unpacked by the poller).
static PACKED: Mutex<Vec<(usize, String, String, bool)>> = Mutex::new(Vec::new());
static ANY_PACKED: AtomicBool = AtomicBool::new(false);
static COOKIE: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static PENDING: RefCell<Vec<Mod>> = const { RefCell::new(Vec::new()) };
}

pub fn init(module: *mut core::ffi::c_void) {
    unsafe { DisableThreadLibraryCalls(module) };
    let exe = module_path(ptr::null_mut());
    let dll = module_path(module);
    let exe_name = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("game");
    let dll_dir = dll.parent().unwrap_or(Path::new(".")).to_path_buf();
    if let Some(path) = log::open(dll_dir, exe_name) {
        log!(
            "peerparty_hook {} exe={} dll={} log={}",
            env!("CARGO_PKG_VERSION"),
            exe.display(),
            dll.display(),
            path.display()
        );
        // Where the stand-ins keep this game's saves: a portable PeerParty's are beside
        // peerparty.exe (PEERPARTY_DATA_DIR), not in the usual folder.
        let named = std::env::var_os(peerparty_protocol::env::DATA_DIR).is_some_and(|d| Path::new(&d).is_absolute());
        if let Some(d) = crate::util::local_store::peerparty_dir() {
            log!("peerparty folder: {} ({})", d.display(), if named { "PEERPARTY_DATA_DIR" } else { "the usual one" });
        }
    }
    crate::sys::wine::log_if_present();
    crate::window::init_from_env();
    let notified = register_notification();
    if !crate::child::is_helper() {
        // A child a starter passed PeerParty to connects once it turns out to be the
        // game (child.rs).
        if !crate::child::claim_later() {
            crate::ipc::start();
        }
        crate::config::modes::start();
        crate::input::init_from_env();
    }
    let mods = loaded_modules();
    if let Some(m) = mods
        .iter()
        .find(|m| sdk::is_anticheat(&m.name) || sdk::is_anticheat(&m.full))
    {
        refuse(m);
    }
    for m in mods {
        watch_module(m);
    }
    if !notified {
        poll_modules();
    }
}

/// Asks the loader to say when a module loads. False when it can't (a Wine without
/// `LdrRegisterDllNotification`, or one that refuses it): `poll_modules` stands in.
fn register_notification() -> bool {
    let wide: Vec<u16> = OsStr::new("ntdll.dll")
        .encode_wide()
        .chain(Some(0))
        .collect();
    let ntdll = unsafe { GetModuleHandleW(wide.as_ptr()) };
    if ntdll.is_null() {
        log!("LdrRegisterDllNotification: ntdll not loaded{}", crate::sys::wine::tag());
        return false;
    }
    let Some(proc) =
        (unsafe { GetProcAddress(ntdll, c"LdrRegisterDllNotification".as_ptr().cast()) })
    else {
        log!("LdrRegisterDllNotification missing{}", crate::sys::wine::tag());
        return false;
    };
    type Reg = unsafe extern "system" fn(
        u32,
        *const core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut *mut core::ffi::c_void,
    ) -> i32;
    let reg: Reg = unsafe { mem::transmute(proc) };
    let mut cookie = ptr::null_mut();
    let status = unsafe {
        reg(
            0,
            on_dll as *const core::ffi::c_void,
            ptr::null_mut(),
            &mut cookie,
        )
    };
    if status < 0 {
        log!("LdrRegisterDllNotification failed {status:#x}{}", crate::sys::wine::tag());
        return false;
    }
    COOKIE.store(cookie, Ordering::Relaxed);
    true
}

/// Without load notifications, the loader's list is read now and again for modules not
/// seen yet, so an SDK the game loads later (most load theirs after start-up) is still
/// hooked, if a little after its load rather than before LoadLibrary returns.
fn poll_modules() {
    const EVERY: std::time::Duration = std::time::Duration::from_millis(200);
    log!("module watch: no load notifications; checking the loaded modules every {} ms instead", EVERY.as_millis());
    let _ = std::thread::Builder::new()
        .name("peerparty-modules".into())
        .spawn(|| loop {
            std::thread::sleep(EVERY);
            let seen = crate::sys::lock(&SEEN).clone();
            for m in loaded_modules() {
                if !seen.contains(&(m.base as usize)) {
                    watch_module(m);
                }
            }
        });
}

/// Whether this thread is inside a module load (PeerParty's load notification or its
/// own start, both under the loader lock): nothing here may wait on another thread,
/// which would need that lock to load anything.
pub fn in_loader() -> bool {
    DEPTH.with(|d| d.get()) > 0
}

fn watch_module(m: Mod) {
    if DEPTH.with(|d| d.get()) > 0 {
        PENDING.with(|p| p.borrow_mut().push(m));
        return;
    }
    DEPTH.with(|d| d.set(1));
    let mut batch = vec![m];
    while let Some(m) = batch.pop() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| consider(m)));
        let more: Vec<Mod> = PENDING.with(|p| p.borrow_mut().drain(..).collect());
        batch.extend(more);
    }
    DEPTH.with(|d| d.set(0));
}

fn consider(m: Mod) {
    if sdk::is_anticheat(&m.name) || sdk::is_anticheat(&m.full) {
        refuse(&m);
    }
    {
        let mut seen = crate::sys::lock(&SEEN);
        if !seen.insert(m.base as usize) {
            return;
        }
    }
    // Modules loaded before this one have bound their imports by now.
    crate::sys::swap::patch_get_proc_imports();
    let table = match unsafe { pe::exports(m.base.cast()) } {
        Ok(t) => t,
        Err(e) => {
            if noteworthy(&m.name) {
                log!("module {} ({e})", m.full);
            }
            return;
        }
    };
    let names: Vec<&str> = table.code.iter().map(|e| e.name.as_str()).collect();
    let kind = sdk::identify_module(&m.name, &names);
    if kind.is_some_and(|k| k != sdk::Sdk::SteamClient) {
        crate::child::sdk_loaded(&m.name);
    }
    if kind.is_some() && still_packed(&table.code) {
        log!("module {} is packed: hooking it once it has unpacked", m.full);
        crate::sys::swap::leave_alone(m.base as usize);
        if wrap_entry(m.base as usize) {
            log!("module {} starts through peerparty: hooked as soon as its start-up returns", m.full);
        }
        crate::sys::lock(&SEEN).remove(&(m.base as usize));
        PACKED
            .lock()
            .unwrap()
            .push((m.base as usize, m.full.clone(), m.name.clone(), false));
        if !ANY_PACKED.swap(true, Ordering::Relaxed) {
            start_packed_poller();
        }
        return;
    }
    if let Some(kind) = kind {
        let anticheat = table
            .code
            .iter()
            .filter(|e| e.name.contains("AntiCheat") || e.name.contains("EasyAntiCheat"))
            .count();
        // The call-logging hooks are diagnostics only. Steam's own client DLL is never
        // patched: PeerParty's Steam layer works through steam_api.
        // Nor is Vivox's: its calls are taken over by name only (vivox.rs).
        let watch = kind != sdk::Sdk::SteamClient && kind != sdk::Sdk::Vivox && !crate::net::skip("calls");
        let mut typed = if watch {
            crate::steam::attach(kind, &table.code)
        } else {
            crate::steam::Typed { skip: Default::default(), hooked: 0 }
        };
        if crate::net::enabled() {
            // A service the player turned off: the overlay says what that costs.
            let layer = match kind {
                sdk::Sdk::SteamApi => Some("steam"),
                sdk::Sdk::Eos => Some("eos"),
                sdk::Sdk::Party | sdk::Sdk::PartyXbl => Some("party"),
                sdk::Sdk::PlayFabCore => Some("playfab"),
                sdk::Sdk::PlayFabMultiplayer => Some("pfmp"),
                sdk::Sdk::Galaxy => Some("galaxy"),
                sdk::Sdk::Vivox => Some("vivox"),
                sdk::Sdk::SteamClient => None,
            };
            if let Some(layer) = layer {
                crate::net::needed_but_off(layer);
            }
        }
        if kind == sdk::Sdk::Eos && crate::net::enabled() {
            // In every mode: a game PeerParty started stays PeerParty's.
            typed.skip.extend(crate::eos::answer_restart_check(&table.code));
        }
        if kind == sdk::Sdk::Eos && crate::eos::enabled() && !crate::net::skip("eos") && !crate::config::modes::official_now("eos") {
            let mine = crate::eos::attach(m.base as usize, &m.name, &table.code);
            typed.hooked += mine.len();
            typed.skip.extend(mine);
        }
        if kind == sdk::Sdk::Party && crate::net::enabled() && !crate::net::skip("party") && !crate::config::modes::official_now("playfab") {
            let mine = crate::playfab::party::attach(m.base as usize, &m.name, &table.code);
            typed.skip.extend(mine);
        }
        if kind == sdk::Sdk::PartyXbl && crate::net::enabled() && !crate::net::skip("party") && !crate::config::modes::official_now("playfab") {
            let mine = crate::playfab::party::xbl::attach(m.base as usize, &m.name, &table.code);
            typed.skip.extend(mine);
        }
        if kind == sdk::Sdk::PlayFabCore && crate::net::enabled() && !crate::net::skip("playfab") && !crate::config::modes::official_now("playfab") {
            crate::playfab::core_loaded(&m.name);
        }
        if kind == sdk::Sdk::PlayFabMultiplayer && crate::net::enabled() && !crate::net::skip("pfmp") && !crate::config::modes::official_now("playfab") {
            let mine = crate::playfab::mp::attach(m.base as usize, &m.name, &table.code);
            typed.skip.extend(mine);
        }
        if kind == sdk::Sdk::Galaxy && crate::net::enabled() && !crate::net::skip("galaxy") && !crate::config::modes::official_now("galaxy") {
            let mine = crate::galaxy::attach(m.base as usize, &m.name, &table.code);
            typed.hooked += mine.len();
            typed.skip.extend(mine);
        }
        // Whether PeerParty answers is decided at the game's first Vivox call, not here
        // under the loader lock (it asks whether the game's Unity sign-in is ours).
        if kind == sdk::Sdk::Vivox && crate::net::enabled() && !crate::net::skip("vivox") {
            typed.skip.extend(crate::voice::vivox::attach(m.base as usize, &m.name, &table.code));
        }
        if kind == sdk::Sdk::SteamApi && crate::net::enabled() && !crate::net::skip("steam") && !crate::config::modes::official_now("steam") {
            crate::steam::lan::attach(m.base as usize, &m.name, &table.code);
            let mine = crate::steam::obj::attach(m.base as usize, &table.code);
            typed.hooked += mine.len();
            typed.skip.extend(mine);
        }
        let summary = if watch {
            crate::sys::hook::install(&table.code, &typed.skip)
        } else {
            crate::sys::hook::Summary { hooked: 0, failed: 0, samples: Vec::new() }
        };
        log!(
            "module {} base={:p} sdk={} exports={} forwarders={} skipped={} anticheat_unhooked={} hooked={} typed={} failed={}",
            m.full,
            m.base,
            sdk::label(kind),
            table.code.len(),
            table.forwarders,
            table.skipped,
            anticheat,
            summary.hooked,
            typed.hooked,
            summary.failed
        );
        for sample in &summary.samples {
            log!("hook failed {sample}");
        }
        if kind == sdk::Sdk::Eos && anticheat > 0 {
            log!(
                "eos anti-cheat exports present: not detoured; with the EOS layer on, starting a session stops the game and the rest fail as offline"
            );
        }
    } else if let Some(vr) = crate::vr::identify(names.iter().copied()) {
        crate::vr::attach(&m.full, vr, &table.code);
    } else if crate::video::is_graphics_module(&m.name) {
        crate::video::module_loaded(&m.name, m.base);
    } else if m.name.to_ascii_lowercase().starts_with("xinput")
        || m.name.eq_ignore_ascii_case("dinput8.dll")
        || m.name.eq_ignore_ascii_case("gameinput.dll")
        || m.name.eq_ignore_ascii_case("gameinputredist.dll")
        || m.name.eq_ignore_ascii_case("windows.gaming.input.dll")
    {
        crate::input::module_loaded(&m.name);
    } else if m.name.eq_ignore_ascii_case("mmdevapi.dll") {
        crate::input::audio_loaded();
    } else if m.name.eq_ignore_ascii_case("xgameruntime.dll") {
        if crate::net::enabled() && !crate::child::is_helper() {
            crate::net::needed_but_off("xgameui");
        }
        if crate::net::enabled() && !crate::net::skip("xgameui") && !crate::child::is_helper() {
            let n = crate::xboxlive::ui::attach(&table.code);
            log!("xgameui {} hooked {n}", m.full);
        }
    } else if m.name.eq_ignore_ascii_case("ws2_32.dll") {
        let n = if crate::net::skip("dns") { 0 } else { crate::net::dns::attach_winsock(&table.code) };
        log!("dns {} hooked {n}", m.full);
        let n = crate::net::ipfwd::attach_winsock(&table.code);
        if n > 0 {
            log!("ipfwd {} hooked {n}", m.full);
        }
    } else if m.name.eq_ignore_ascii_case("kernelbase.dll") {
        if crate::window::virtual_size().is_some() {
            let n = crate::screen_prefs::attach_kernelbase(&table.code);
            log!("screen {} hooked {n}", m.full);
        }
        if crate::instance::enabled() {
            let n = crate::instance::attach_kernelbase(&table.code);
            log!("instance {} hooked {n}", m.full);
        }
        if crate::saves::enabled() {
            let n = crate::saves::attach_kernelbase(&table.code);
            log!("saves {} file hooks {n}", m.full);
        }
        if crate::net::enabled() && !crate::net::skip("swap") {
            let n = crate::sys::swap::attach_kernelbase(&table.code);
            log!("swap ready for {} ({n})", m.full);
        }
        if crate::net::enabled() && !crate::net::skip("invite") && !crate::child::is_helper() {
            crate::invite::start();
        }
        if crate::net::enabled() {
            crate::sys::exitlog::attach(&table.code);
        }
        if crate::child::enabled() {
            let n = crate::child::attach_kernelbase(&table.code);
            log!("child: following the processes {} starts ({n})", m.full);
        }
    } else if m.name.eq_ignore_ascii_case("crypt32.dll") {
        // PeerParty's HTTPS server answers the services in `https::services()`: its
        // certificate check is needed unless every one of their layers is off.
        let https = crate::https::services().iter().any(|s| !crate::net::skip(s.name));
        if https && crate::https::attach_crypt32(&table.code) {
            log!("tls {} certificate check noted (hooked only once PeerParty's HTTPS server starts)", m.full);
        }
    } else if m.name.eq_ignore_ascii_case("sspicli.dll") {
        if crate::net::enabled() && !crate::net::skip("demonware") {
            let n = crate::demonware::schannel::attach(&table.code);
            log!("demonware {} hooked {n} (SChannel's certificate check, for Demonware hosts only)", m.full);
        }
    } else if m.name.eq_ignore_ascii_case("xcurl.dll") {
        if crate::net::enabled() && !crate::net::skip("xboxlive") && crate::xboxlive::curl::bodies() {
            let n = crate::xboxlive::curl::attach(&table.code);
            log!("xboxlive {} hooked {n} (the game's Xbox Live calls are traced)", m.full);
        }
    } else if m.name.eq_ignore_ascii_case("winhttp.dll") {
        if crate::net::enabled() && !crate::net::skip("xboxlive") && crate::xboxlive::curl::bodies() && crate::xboxlive::title().is_some() {
            let n = crate::xboxlive::winhttp::attach(&table.code);
            log!("xboxlive {} hooked {n} (the game's Xbox Live calls are traced)", m.full);
        }
    } else if m.name.eq_ignore_ascii_case("user32.dll") {
        if crate::instance::enabled() {
            let n = crate::instance::attach_user32(&table.code);
            log!("instance {} hooked {n}", m.full);
        }
        if crate::window::wants_hooks() {
            let n = crate::window::attach_user32(&table.code);
            log!("window {} hooked {n}", m.full);
        }
    } else if m.name.eq_ignore_ascii_case("dnsapi.dll") {
        let n = if crate::net::skip("dns") { 0 } else { crate::net::dns::attach_dnsapi(&table.code) };
        log!("dns {} hooked {n}", m.full);
    } else if noteworthy(&m.name) {
        log!(
            "module {} (no sdk signature, {} exports)",
            m.full,
            table.code.len()
        );
    }
}

/// Whether a module's code hasn't been unpacked yet: the first bytes of several of its
/// exports are all zero (real code never starts with `add [rax], al` everywhere).
fn still_packed(exports: &[pe::Export]) -> bool {
    let sample: Vec<&pe::Export> = exports.iter().step_by((exports.len() / 8).max(1)).take(8).collect();
    !sample.is_empty()
        && sample.iter().all(|e| {
            let bytes = unsafe { std::slice::from_raw_parts(e.addr as *const u8, 8) };
            bytes.iter().all(|b| *b == 0)
        })
}

/// Whether every sampled export has code now.
fn unpacked(base: usize) -> bool {
    let Ok(t) = (unsafe { pe::exports(base as *const u8) }) else {
        return false;
    };
    let sample: Vec<&pe::Export> = t.code.iter().step_by((t.code.len() / 8).max(1)).take(8).collect();
    !sample.is_empty()
        && sample.iter().all(|e| {
            let bytes = unsafe { std::slice::from_raw_parts(e.addr as *const u8, 8) };
            bytes.iter().any(|b| *b != 0)
        })
}

/// Packed modules whose entry point (DllMain) PeerParty stands in for: base -> theirs.
static WRAPPED: LazyLock<Mutex<HashMap<usize, usize>>> = LazyLock::new(Mutex::default);

type DllMain = unsafe extern "system" fn(usize, u32, usize) -> i32;

/// Points the loader's record of a packed module at `packed_entry`, so its start-up
/// (where it unpacks) runs with GetProcAddress unhooked, and the module is hooked as soon as it returns, before
/// `LoadLibrary` returns to the game. The module's own code isn't touched.
fn wrap_entry(base: usize) -> bool {
    let original = under_loader_lock(|| {
        let entry = unsafe { &mut *ldr_entry(base)? };
        let original = entry.entry_point as usize;
        if original == 0 || original == packed_entry as *const () as usize {
            return None;
        }
        entry.entry_point = packed_entry as *mut core::ffi::c_void;
        Some(original)
    });
    let Some(original) = original else { return false };
    crate::sys::lock(&WRAPPED).insert(base, original);
    true
}

unsafe extern "system" fn packed_entry(module: usize, reason: u32, reserved: usize) -> i32 {
    let Some(original) = crate::sys::lock(&WRAPPED).get(&module).copied() else {
        return 1;
    };
    let original: DllMain = unsafe { std::mem::transmute(original) };
    // DLL_PROCESS_ATTACH
    if reason != 1 {
        return unsafe { original(module, reason, reserved) };
    }
    crate::sys::swap::pause(true);
    let ok = unsafe { original(module, reason, reserved) };
    crate::sys::swap::pause(false);
    if ok != 0 {
        recheck(Some(module));
    }
    ok
}

/// The game looked up a function in `module`: if it's a packed module waiting, it has
/// finished loading (and unpacking), so hook it now, before the lookup returns.
pub fn looked_up(module: usize) {
    if ANY_PACKED.load(Ordering::Relaxed) {
        recheck(Some(module));
    }
}

/// Hooks waiting packed modules that are ready: `module` if given (it's loaded),
/// else (the poller) those whose code was already there on the previous poll.
fn recheck(module: Option<usize>) {
    let ready: Vec<(usize, String, String)> = {
        let mut list = crate::sys::lock(&PACKED);
        let mut ready = Vec::new();
        list.retain_mut(|(base, full, name, seen)| {
            let done = match module {
                Some(m) => m == *base && unpacked(*base),
                None => {
                    let now = unpacked(*base);
                    let stable = now && *seen;
                    *seen = now;
                    stable
                }
            };
            if done {
                ready.push((*base, full.clone(), name.clone()));
            }
            !done
        });
        ready
    };
    for (base, full, name) in ready {
        log!("module {full} has unpacked");
        watch_module(Mod {
            base: base as *mut core::ffi::c_void,
            full,
            name,
        });
    }
}

/// A fallback for a packed module nothing else rechecks: polls for a minute.
fn start_packed_poller() {
    let _ = std::thread::Builder::new()
        .name("peerparty-unpack".into())
        .spawn(|| {
            for _ in 0..1200 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if crate::sys::lock(&PACKED).is_empty() {
                    return;
                }
                recheck(None);
            }
        });
}

fn noteworthy(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "steam", "playfab", "party", "eos", "photon", "easyanti", "battleye", "beclient",
    ]
    .iter()
    .any(|k| n.contains(k))
}

fn refuse(m: &Mod) -> ! {
    stop(&format!("anti-cheat module {}", m.full))
}

/// Ends a game that uses anti-cheat: PeerParty isn't for those games (LEGAL.md).
pub(crate) fn stop(what: &str) -> ! {
    log!("refusing to start: {what}");
    log::line(format_args!(""));
    unsafe { TerminateProcess(GetCurrentProcess(), 1) };
    std::process::abort();
}

fn module_path(module: *mut core::ffi::c_void) -> PathBuf {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 {
        return PathBuf::from("unknown");
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]))
}

#[repr(C)]
struct ListEntry {
    flink: *mut ListEntry,
    blink: *mut ListEntry,
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
struct PebLdr {
    length: u32,
    initialized: u8,
    _pad: [u8; 3],
    ss_handle: *mut core::ffi::c_void,
    in_load_order: ListEntry,
}

#[repr(C)]
struct LdrEntry {
    in_load_order: ListEntry,
    in_memory_order: ListEntry,
    in_init_order: ListEntry,
    dll_base: *mut core::ffi::c_void,
    entry_point: *mut core::ffi::c_void,
    // repr(C) adds the 4 bytes of padding x64 has here; x86 has none.
    size_of_image: u32,
    full_dll_name: UnicodeString,
    base_dll_name: UnicodeString,
}

#[repr(C)]
struct LdrNote {
    flags: u32,
    full_dll_name: *const UnicodeString,
    base_dll_name: *const UnicodeString,
    dll_base: *mut core::ffi::c_void,
    size_of_image: u32,
}

/// (base, full path) of every loaded module.
pub(crate) fn modules_full() -> Vec<(usize, String)> {
    loaded_modules()
        .into_iter()
        .map(|m| (m.base as usize, m.full))
        .collect()
}

/// (base, file name) of every loaded module.
pub(crate) fn modules() -> Vec<(usize, String)> {
    loaded_modules()
        .into_iter()
        .map(|m| (m.base as usize, m.name))
        .collect()
}

fn loaded_modules() -> Vec<Mod> {
    under_loader_lock(|| {
        ldr_entries()
            .into_iter()
            .map(|e| {
                let e = unsafe { &*e };
                let full = utf16(&e.full_dll_name);
                let name = file_name(utf16(&e.base_dll_name), &full);
                Mod {
                    base: e.dll_base,
                    full,
                    name,
                }
            })
            .collect()
    })
}

/// Runs `f` holding the loader lock, so no other thread loads or unloads a module (and
/// frees its record) while `f` walks the loader's list. The lock is only tried for, for
/// up to about 200 ms: a thread holding it (a module's DllMain, or `on_dll`) may be
/// waiting for a lock the caller holds, and waiting on it outright could deadlock the
/// game. Not got by then, `f` runs without it, as it always did before. On a thread that
/// already holds it (`on_dll` runs under it) it's taken again at once.
fn under_loader_lock<R>(f: impl FnOnce() -> R) -> R {
    type Lock = unsafe extern "system" fn(u32, *mut u32, *mut usize) -> i32;
    type Unlock = unsafe extern "system" fn(u32, usize) -> i32;
    static FNS: LazyLock<Option<(Lock, Unlock)>> = LazyLock::new(|| {
        let ntdll = unsafe { GetModuleHandleW(wide("ntdll.dll").as_ptr()) };
        if ntdll.is_null() {
            return None;
        }
        let lock = unsafe { GetProcAddress(ntdll, c"LdrLockLoaderLock".as_ptr().cast()) }?;
        let unlock = unsafe { GetProcAddress(ntdll, c"LdrUnlockLoaderLock".as_ptr().cast()) }?;
        Some(unsafe { (mem::transmute::<_, Lock>(lock), mem::transmute::<_, Unlock>(unlock)) })
    });
    const TRY_ONLY: u32 = 2;
    const ACQUIRED: u32 = 1;
    let Some((lock, unlock)) = *FNS else { return f() };
    let mut cookie = 0usize;
    let mut held = false;
    for attempt in 0..200 {
        let mut disposition = 0u32;
        let status = unsafe { lock(TRY_ONLY, &mut disposition, &mut cookie) };
        if status < 0 {
            break;
        }
        if disposition == ACQUIRED {
            held = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(if attempt < 20 { 0 } else { 1 }));
    }
    let out = f();
    if held {
        unsafe { unlock(0, cookie) };
    }
    out
}
/// The loader's record of the module at `base`. Call under `under_loader_lock`.
fn ldr_entry(base: usize) -> Option<*mut LdrEntry> {
    ldr_entries()
        .into_iter()
        .find(|e| unsafe { (**e).dll_base as usize } == base)
}

/// The loader's list of modules. Call under `under_loader_lock`: a module unloaded
/// mid-walk frees its record.
fn ldr_entries() -> Vec<*mut LdrEntry> {
    let mut peb: *const u8;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!(
            "mov {}, gs:[0x60]",
            out(reg) peb,
            options(nostack, preserves_flags),
        );
    }
    #[cfg(target_arch = "x86")]
    unsafe {
        core::arch::asm!(
            "mov {}, fs:[0x30]",
            out(reg) peb,
            options(nostack, preserves_flags),
        );
    }
    if peb.is_null() {
        return Vec::new();
    }
    // PEB.Ldr follows two pointer-sized fields after 4 bytes of flags.
    let ldr_offset = if cfg!(target_pointer_width = "64") {
        0x18
    } else {
        0x0c
    };
    let ldr = unsafe { peb.add(ldr_offset).cast::<*const PebLdr>().read() };
    if ldr.is_null() {
        return Vec::new();
    }
    let head = unsafe { &(*ldr).in_load_order as *const ListEntry };
    let mut cur = unsafe { (*head).flink };
    let mut out = Vec::new();
    for _ in 0..512 {
        if cur.is_null() || cur == head as *mut ListEntry {
            break;
        }
        let ptr = cur.cast::<LdrEntry>();
        let entry = unsafe { &*ptr };
        if !entry.dll_base.is_null() {
            out.push(ptr);
        }
        cur = entry.in_load_order.flink;
    }
    out
}

/// A module's file name: the loader's, or else the last part of its path, which may be
/// split by `/` as well as `\` (a Unix path Wine hands over as it is).
fn file_name(name: String, full: &str) -> String {
    if !name.is_empty() {
        return name;
    }
    full.rsplit(['\\', '/']).next().unwrap_or_default().to_string()
}

fn utf16(u: &UnicodeString) -> String {
    if u.buffer.is_null() || u.length < 2 {
        return String::new();
    }
    let n = u.length as usize / 2;
    let slice = unsafe { std::slice::from_raw_parts(u.buffer, n) };
    String::from_utf16_lossy(slice)
}

unsafe extern "system" fn on_dll(reason: u32, data: *const LdrNote, _: *mut core::ffi::c_void) {
    if reason != 1 || data.is_null() {
        return;
    }
    let data = unsafe { &*data };
    let full = unsafe { data.full_dll_name.as_ref() }
        .map(utf16)
        .unwrap_or_default();
    let name = file_name(
        unsafe { data.base_dll_name.as_ref() }
            .map(utf16)
            .unwrap_or_default(),
        &full,
    );
    watch_module(Mod {
        base: data.dll_base,
        full,
        name,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryExW;

    /// A module the loader names without a file name gets its path's last part, split
    /// at either slash (Wine paths).
    #[test]
    fn a_module_without_a_name_takes_its_paths() {
        assert_eq!(file_name("steam_api64.dll".into(), r"C:\x\other.dll"), "steam_api64.dll");
        assert_eq!(file_name(String::new(), r"Z:\home\me\Game\sdk.dll"), "sdk.dll");
        assert_eq!(file_name(String::new(), r"Z:\home/me/Game/sdk.dll"), "sdk.dll");
        assert_eq!(file_name(String::new(), "/home/me/Game/sdk.dll"), "sdk.dll");
        assert_eq!(file_name(String::new(), ""), "");
    }

    #[test]
    fn installs_hooks_on_real_sdk_images() {
        // Maps the DLL without running its DllMain, then hooks the real prologues.
        // The functions are not called: unresolved imports would crash. Steam's client
        // DLL is checked where Steam is installed; PPTEST_SDK_DLLS (PPTEST_SDK_DLLS_X86 for
        // a 32-bit build) adds SDK DLLs copied from games you have installed, separated by `;`, each
        // of which must be recognised as an SDK.
        const DONT_RESOLVE: u32 = 0x1;
        #[cfg(target_arch = "x86")]
        let client = r"C:\Program Files (x86)\Steam\steamclient.dll";
        #[cfg(target_arch = "x86_64")]
        let client = r"C:\Program Files (x86)\Steam\steamclient64.dll";
        let listed = std::env::var(if cfg!(target_arch = "x86") { "PPTEST_SDK_DLLS_X86" } else { "PPTEST_SDK_DLLS" }).unwrap_or_default();
        let mut cases: Vec<(String, Option<sdk::Sdk>)> = vec![(client.to_string(), Some(sdk::Sdk::SteamClient))];
        cases.extend(listed.split(';').map(str::trim).filter(|s| !s.is_empty()).map(|s| (s.to_string(), None)));
        // Earlier runs' copies (made over ten minutes ago: a run going now may be about to
        // load its own) go, unless a process still has one loaded.
        let old = |e: &std::fs::DirEntry| {
            e.metadata().and_then(|m| m.created()).is_ok_and(|t| t.elapsed().is_ok_and(|a| a.as_secs() > 600))
        };
        for e in std::fs::read_dir(std::env::temp_dir()).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with("peerparty-probe-") && old(&e) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let mut ran = 0;
        for (path, expect) in &cases {
            let path = path.as_str();
            if !Path::new(path).is_file() {
                assert!(expect.is_some(), "PPTEST_SDK_DLLS: {path} isn't a file");
                continue;
            }
            ran += 1;
            let file_name = Path::new(path).file_name().unwrap();
            // This process's own copy: a loaded DLL can't be overwritten, and another
            // test run (the 32-bit one, a second checkout) may have its copy loaded now.
            let copy = std::env::temp_dir().join(format!(
                "peerparty-probe-{}-{ran}-{}",
                std::process::id(),
                file_name.to_string_lossy()
            ));
            std::fs::copy(path, &copy).unwrap_or_else(|e| panic!("copy {path}: {e}"));
            let wide: Vec<u16> = OsStr::new(&copy).encode_wide().chain(Some(0)).collect();
            let base = unsafe { LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), DONT_RESOLVE) };
            assert!(
                !base.is_null(),
                "{path}: {}",
                std::io::Error::last_os_error()
            );
            let table = unsafe { pe::exports(base.cast()).unwrap() };
            let names: Vec<&str> = table.code.iter().map(|e| e.name.as_str()).collect();
            let found = sdk::identify(names);
            assert!(found.is_some() && (expect.is_none() || found == *expect), "{path}: {found:?}");
            let expect = found.unwrap();
            let typed = crate::steam::attach(expect, &table.code);
            let summary = crate::sys::hook::install(&table.code, &typed.skip);
            let ok = summary.hooked + typed.hooked;
            assert!(
                ok > 20,
                "{path} hooked {ok} failed {} {:?}",
                summary.failed,
                summary.samples
            );
            assert!(
                summary.failed * 5 < ok.max(1),
                "{path} too many failures ({}/{}) {:?}",
                summary.failed,
                ok,
                summary.samples
            );
            if matches!(expect, sdk::Sdk::SteamApi | sdk::Sdk::SteamClient) {
                assert!(
                    typed.hooked > 0,
                    "{path} typed hooks did not install: failed {:?}",
                    summary.samples
                );
            }
        }
        if ran == 0 {
            crate::skip("no SDK image to probe: no Steam client here and PPTEST_SDK_DLLS names none");
        }
    }
}
