//! Swapping an SDK's functions by name, without touching its code.
//!
//! A front end registers the SDK module and a resolver. Then:
//! - `GetProcAddress` on that module returns the resolver's function (.NET and IL2CPP
//!   P/Invoke, Mono, manual lookups). By default that's done in the import tables of
//!   the game's modules (the exe, Mono, GameAssembly, the .NET runtime...): their
//!   `GetProcAddress` slot points here, and kernelbase itself isn't touched, for
//!   compatibility: some games won't start once kernelbase's code has been patched.
//!   Asking
//!   for `GetProcAddress` itself gets this one too. Windows' own modules and packed
//!   modules are left alone. `PEERPARTY_SWAP=inline` hooks kernelbase's
//!   `GetProcAddress` / `GetProcAddressForCaller` instead, which also catches code
//!   that found GetProcAddress some other way.
//! - Static imports of the module get their slots rewritten. A module's imports are
//!   only resolved once its load finishes, so `patch_imports` runs when the SDK is
//!   first used and then now and again, not as modules map.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use retour::GenericDetour;

use crate::hook::with_lock;
use crate::log;
use crate::pe::Export;

pub type Resolver = fn(&str) -> Option<usize>;

struct Module {
    base: usize,
    name: String,
    resolve: Resolver,
}

static MODULES: Mutex<Vec<Module>> = Mutex::new(Vec::new());
static PATCHED: LazyLock<Mutex<HashSet<(usize, usize)>>> = LazyLock::new(Mutex::default);
/// (importer, SDK) pairs whose imports point here.
static IMPORTERS: LazyLock<Mutex<HashSet<(usize, usize)>>> = LazyLock::new(Mutex::default);

type GetProcAddressFn = unsafe extern "system" fn(usize, *const u8) -> usize;
type GetProcAddressForCallerFn = unsafe extern "system" fn(usize, *const u8, usize) -> usize;

static GET_PROC: OnceLock<GenericDetour<GetProcAddressFn>> = OnceLock::new();
static GET_PROC_FOR_CALLER: OnceLock<GenericDetour<GetProcAddressForCallerFn>> = OnceLock::new();

/// kernelbase's own GetProcAddress, which the import-table detour calls.
static REAL_GET_PROC: AtomicUsize = AtomicUsize::new(0);
/// Modules whose GetProcAddress imports point here, and modules never to touch.
static IAT_DONE: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(Mutex::default);
static LEAVE_ALONE: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(Mutex::default);

fn inline() -> bool {
    static INLINE: OnceLock<bool> = OnceLock::new();
    *INLINE.get_or_init(|| std::env::var("PEERPARTY_SWAP").is_ok_and(|v| v.eq_ignore_ascii_case("inline")))
}

/// A packed module: its import table stays as the loader left it, for compatibility.
pub fn leave_alone(base: usize) {
    LEAVE_ALONE.lock().unwrap().insert(base);
}

/// Points the GetProcAddress imports of every loaded game module here. Modules whose
/// imports aren't bound yet are tried again on the next pass.
pub fn patch_get_proc_imports() {
    if inline() || REAL_GET_PROC.load(Ordering::Relaxed) == 0 {
        return;
    }
    let windows = std::env::var("SystemRoot")
        .unwrap_or_else(|_| r"C:\Windows".into())
        .to_ascii_lowercase();
    for (base, full) in crate::watch::modules_full() {
        let lower = full.to_ascii_lowercase();
        let file = lower.rsplit('\\').next().unwrap_or_default().to_string();
        let system = (lower.starts_with(&windows) && !lower.contains(r"\microsoft.net\"))
            || lower.contains(r"\microsoft\windows defender\")
            || lower.contains(r"\windows defender\");
        if system
            || file.starts_with("peerparty_hook")
            || IAT_DONE.lock().unwrap().contains(&base)
            || LEAVE_ALONE.lock().unwrap().contains(&base)
        {
            continue;
        }
        let imports = unsafe { crate::pe::imports(base as *const u8) };
        let mut n = 0;
        let mut unbound = false;
        for imp in imports.iter().filter(|i| i.name == "GetProcAddress" && loader_dll(&i.dll)) {
            let now = unsafe { imp.slot.read() };
            // An unbound slot still holds an RVA; a bound one an address in a DLL.
            if now < 0x1000_0000 {
                unbound = true;
                continue;
            }
            let ours = detour_get_proc as *const () as usize;
            if now != ours && unsafe { crate::pe::write_slot(imp.slot, ours) } {
                n += 1;
            }
        }
        if unbound {
            continue;
        }
        IAT_DONE.lock().unwrap().insert(base);
        if n > 0 {
            log!("swap {full}: GetProcAddress comes to peerparty");
        }
    }
}

fn loader_dll(dll: &str) -> bool {
    let d = dll.to_ascii_lowercase();
    d == "kernel32.dll" || d == "kernelbase.dll" || d.starts_with("api-ms-win-core-libraryloader-")
}

fn is_loader_module(module: usize) -> bool {
    crate::watch::modules_full().iter().any(|(b, full)| {
        *b == module && {
            let f = full.to_ascii_lowercase();
            f.ends_with(r"\kernel32.dll") || f.ends_with(r"\kernelbase.dll")
        }
    })
}

pub fn register(base: usize, name: &str, resolve: Resolver) {
    {
        let mut m = MODULES.lock().unwrap();
        m.retain(|x| x.base != base);
        m.push(Module {
            base,
            name: name.to_ascii_lowercase(),
            resolve,
        });
    }
    // Modules loaded since the last pass have bound their imports by now.
    patch_get_proc_imports();
}

fn arm<T>(slot: &OnceLock<GenericDetour<T>>, target: T, detour: T, label: &str) -> bool
where
    T: retour::Function + Copy + Send + Sync + 'static,
{
    with_lock(|| {
        let hook = match unsafe { GenericDetour::new(target, detour) } {
            Ok(h) => h,
            Err(e) => {
                log!("swap {label} not hooked: {e}");
                return false;
            }
        };
        if slot.set(hook).is_err() {
            return false;
        }
        if let Err(e) = unsafe { slot.get().unwrap().enable() } {
            log!("swap {label} enable failed: {e}");
            return false;
        }
        true
    })
}

pub fn attach_kernelbase(exports: &[Export]) -> usize {
    if let Some(e) = exports.iter().find(|e| e.name == "GetProcAddress") {
        REAL_GET_PROC.store(e.addr as usize, Ordering::Relaxed);
    }
    if !inline() {
        patch_get_proc_imports();
        return usize::from(REAL_GET_PROC.load(Ordering::Relaxed) != 0);
    }
    let mut n = 0;
    if let Some(e) = exports.iter().find(|e| e.name == "GetProcAddress") {
        let target: GetProcAddressFn = unsafe { std::mem::transmute(e.addr) };
        n += usize::from(arm(&GET_PROC, target, detour_get_proc, "GetProcAddress"));
    }
    if let Some(e) = exports.iter().find(|e| e.name == "GetProcAddressForCaller") {
        let target: GetProcAddressForCallerFn = unsafe { std::mem::transmute(e.addr) };
        n += usize::from(arm(
            &GET_PROC_FOR_CALLER,
            target,
            detour_get_proc_for_caller,
            "GetProcAddressForCaller",
        ));
    }
    n
}

/// Takes the GetProcAddress detours out (or puts them back) while a packed module
/// starts, for compatibility: some games won't start with GetProcAddress patched.
pub fn pause(paused: bool) {
    with_lock(|| unsafe {
        for d in [GET_PROC.get()].into_iter().flatten() {
            let _ = if paused { d.disable() } else { d.enable() };
        }
        for d in [GET_PROC_FOR_CALLER.get()].into_iter().flatten() {
            let _ = if paused { d.disable() } else { d.enable() };
        }
    });
}

fn substitute(module: usize, name: *const u8) -> Option<usize> {
    if module == 0 || (name as usize) <= 0xffff {
        return None;
    }
    let resolve = {
        let m = MODULES.lock().unwrap();
        m.iter().find(|x| x.base == module)?.resolve
    };
    resolve(&crate::read::cstr(name as usize)?)
}

unsafe extern "system" fn detour_get_proc(module: usize, name: *const u8) -> usize {
    crate::watch::looked_up(module);
    crate::vivox::looked_up(module);
    if let Some(f) = substitute(module, name) {
        return f;
    }
    if let Some(hook) = GET_PROC.get() {
        return unsafe { hook.call(module, name) };
    }
    // Import-table mode: someone asking for GetProcAddress itself gets this one.
    if (name as usize) > 0xffff
        && unsafe { std::ffi::CStr::from_ptr(name.cast()) }.to_bytes() == b"GetProcAddress"
        && is_loader_module(module)
    {
        return detour_get_proc as *const () as usize;
    }
    let real = REAL_GET_PROC.load(Ordering::Relaxed);
    if real == 0 {
        return 0;
    }
    let real: GetProcAddressFn = unsafe { std::mem::transmute(real) };
    unsafe { real(module, name) }
}

unsafe extern "system" fn detour_get_proc_for_caller(
    module: usize,
    name: *const u8,
    caller: usize,
) -> usize {
    crate::watch::looked_up(module);
    if let Some(f) = substitute(module, name) {
        return f;
    }
    let Some(hook) = GET_PROC_FOR_CALLER.get() else {
        return 0;
    };
    unsafe { hook.call(module, name, caller) }
}

/// How many modules have had their imports of the SDK at `base` pointed here.
pub fn importers_patched(base: usize) -> usize {
    IMPORTERS.lock().unwrap().iter().filter(|(_, b)| *b == base).count()
}

/// Points every module's static imports of a registered SDK at the resolver's
/// functions. Each (importer, SDK) pair is done once, once the loader has bound the
/// importer's slots: a slot written before that would be overwritten by the loader
/// (a DLL that imports the SDK is often still loading when the SDK is registered).
pub fn patch_imports() {
    patch_get_proc_imports();
    let sdks: Vec<(usize, String, Resolver)> = MODULES
        .lock()
        .unwrap()
        .iter()
        .map(|m| (m.base, m.name.clone(), m.resolve))
        .collect();
    if sdks.is_empty() {
        return;
    }
    for (module, _) in crate::watch::modules() {
        for (base, sdk, resolve) in &sdks {
            if module == *base || !PATCHED.lock().unwrap().insert((module, *base)) {
                continue;
            }
            let mut cache: HashMap<String, Option<usize>> = HashMap::new();
            let mut n = 0;
            let imports = unsafe { crate::pe::imports(module as *const u8) };
            let theirs: Vec<_> = imports.iter().filter(|i| i.dll.eq_ignore_ascii_case(sdk)).collect();
            if theirs.iter().any(|i| unsafe { i.slot.read() } == i.unbound) {
                PATCHED.lock().unwrap().remove(&(module, *base));
                continue;
            }
            if !theirs.is_empty() {
                IMPORTERS.lock().unwrap().insert((module, *base));
            }
            for imp in theirs {
                let f = *cache
                    .entry(imp.name.clone())
                    .or_insert_with(|| resolve(&imp.name));
                if let Some(f) = f {
                    if unsafe { crate::pe::write_slot(imp.slot, f) } {
                        n += 1;
                    }
                }
            }
            if n > 0 {
                log!("swap patched {n} imports of {sdk} in module {module:#x}");
            }
        }
    }
}
