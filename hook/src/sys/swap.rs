//! Swapping an SDK's functions by name, without touching its code.
//!
//! A front end registers the SDK module and a resolver. Then:
//! - `GetProcAddress` on that module returns the resolver's function (.NET and IL2CPP
//!   P/Invoke, Mono, manual lookups). By default that's done in the import tables of
//!   the game's modules (the exe, Mono, GameAssembly, the .NET runtime...): their
//!   `GetProcAddress` slot points here, and kernelbase itself isn't touched, for
//!   compatibility: some games won't start once kernelbase's code has been patched.
//!   A slot another module already redirected (a mod loader such as Unity Doorstop,
//!   which BepInEx and MelonLoader use, does it to UnityPlayer.dll from its DllMain)
//!   keeps that redirect behind PeerParty's, so the mod loader still sees every lookup
//!   PeerParty doesn't answer itself. Asking
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

use crate::sys::hook::with_lock;
use crate::log;
use crate::sys::win::module_of;
use crate::sys::pe::Export;

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
    *INLINE.get_or_init(|| crate::config::HookConfig::global().swap_inline)
}

/// A packed module: its import table stays as the loader left it, for compatibility.
pub fn leave_alone(base: usize) {
    crate::sys::lock(&LEAVE_ALONE).insert(base);
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
        let file = lower.rsplit(['\\', '/']).next().unwrap_or_default().to_string();
        let system = (lower.starts_with(&windows) && !lower.contains(r"\microsoft.net\"))
            || lower.contains(r"\microsoft\windows defender\")
            || lower.contains(r"\windows defender\");
        if system
            || file.starts_with("peerparty_hook")
            || crate::sys::lock(&IAT_DONE).contains(&base)
            || crate::sys::lock(&LEAVE_ALONE).contains(&base)
        {
            continue;
        }
        let imports = unsafe { crate::sys::pe::imports(base as *const u8) };
        let mut n = 0;
        let mut chained = Vec::new();
        let mut unbound = false;
        for imp in imports.iter().filter(|i| i.name == "GetProcAddress" && loader_dll(&i.dll)) {
            let now = unsafe { imp.slot.read() };
            // An unbound slot still holds an RVA; a bound one an address in a DLL.
            if now < 0x1000_0000 {
                unbound = true;
                continue;
            }
            if is_ours(now) {
                continue;
            }
            // Another module already redirected this slot (a mod loader such as Unity
            // Doorstop does this from its DllMain, before the hook loads, to see the
            // runtime's init lookups): its function stays in the chain, called for
            // whatever PeerParty doesn't answer itself.
            let ours = if is_loader_module(module_of(now)) {
                detour_get_proc as *const () as usize
            } else {
                match chain(now) {
                    Some(f) => {
                        chained.push(module_name(module_of(now)));
                        f
                    }
                    None => {
                        log!("swap {full}: GetProcAddress already redirected and no chain slot left; left as it is");
                        continue;
                    }
                }
            };
            if unsafe { crate::sys::pe::write_slot(imp.slot, ours) } {
                n += 1;
            }
        }
        if unbound {
            continue;
        }
        crate::sys::lock(&IAT_DONE).insert(base);
        if n > 0 {
            log!("swap {full}: GetProcAddress comes to peerparty");
        }
        for by in chained {
            log!("swap {full}: GetProcAddress was already redirected to {by}; peerparty passes it what it doesn't answer");
        }
    }
}

fn loader_dll(dll: &str) -> bool {
    let d = dll.to_ascii_lowercase();
    d == "kernel32.dll" || d == "kernelbase.dll" || d.starts_with("api-ms-win-core-libraryloader-")
}

fn module_name(base: usize) -> String {
    crate::watch::modules_full()
        .into_iter()
        .find(|(b, _)| *b == base)
        .map(|(_, f)| f)
        .unwrap_or_else(|| format!("{base:#x}"))
}

/// GetProcAddress redirects found in place (`chain`), each with a detour of its own
/// that hands on to it: CHAINS of them at most.
const CHAINS: usize = 16;
static CHAIN_NEXT: [AtomicUsize; CHAINS] = [const { AtomicUsize::new(0) }; CHAINS];
static CHAIN_LOCK: Mutex<()> = Mutex::new(());

macro_rules! chained_detours {
    ($($n:literal)*) => { [$(chained_get_proc::<$n> as GetProcAddressFn),*] };
}
static CHAINED: [GetProcAddressFn; CHAINS] = chained_detours!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15);

/// The detour that answers what PeerParty answers and hands everything else to `next`
/// (another module's GetProcAddress redirect). The same `next` shares one detour.
fn chain(next: usize) -> Option<usize> {
    let _g = crate::sys::lock(&CHAIN_LOCK);
    let mut free = None;
    for (i, slot) in CHAIN_NEXT.iter().enumerate() {
        match slot.load(Ordering::Acquire) {
            n if n == next => return Some(CHAINED[i] as usize),
            0 if free.is_none() => free = Some(i),
            _ => {}
        }
    }
    let i = free?;
    CHAIN_NEXT[i].store(next, Ordering::Release);
    Some(CHAINED[i] as usize)
}

fn is_ours(f: usize) -> bool {
    f == detour_get_proc as *const () as usize || CHAINED.iter().any(|c| *c as usize == f)
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
        let mut m = crate::sys::lock(&MODULES);
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
    crate::sys::detour::arm(slot, target, detour, "swap", label)
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
        let m = crate::sys::lock(&MODULES);
        m.iter().find(|x| x.base == module)?.resolve
    };
    resolve(&crate::sys::read::cstr(name as usize)?)
}

unsafe extern "system" fn detour_get_proc(module: usize, name: *const u8) -> usize {
    unsafe { get_proc(module, name, 0) }
}

unsafe extern "system" fn chained_get_proc<const N: usize>(module: usize, name: *const u8) -> usize {
    unsafe { get_proc(module, name, CHAIN_NEXT[N].load(Ordering::Acquire)) }
}

/// GetProcAddress as the game sees it: PeerParty's swaps, else `next` (another
/// module's redirect this slot held before; 0 for none), else the real one.
unsafe fn get_proc(module: usize, name: *const u8, next: usize) -> usize {
    crate::watch::looked_up(module);
    crate::voice::vivox::looked_up(module);
    if crate::vr::refused(name) {
        return not_found();
    }
    if let Some(f) = substitute(module, name) {
        return f;
    }
    if let Some(hook) = GET_PROC.get() {
        return unsafe { hook.call(module, name) };
    }
    if next != 0 {
        let next: GetProcAddressFn = unsafe { std::mem::transmute(next) };
        return unsafe { next(module, name) };
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

/// GetProcAddress's answer for a name it doesn't find.
fn not_found() -> usize {
    // ERROR_PROC_NOT_FOUND.
    unsafe { windows_sys::Win32::Foundation::SetLastError(127) };
    0
}

unsafe extern "system" fn detour_get_proc_for_caller(
    module: usize,
    name: *const u8,
    caller: usize,
) -> usize {
    crate::watch::looked_up(module);
    crate::voice::vivox::looked_up(module);
    if crate::vr::refused(name) {
        return not_found();
    }
    if let Some(f) = substitute(module, name) {
        return f;
    }
    let Some(hook) = GET_PROC_FOR_CALLER.get() else {
        return 0;
    };
    unsafe { hook.call(module, name, caller) }
}

/// How many modules have had their imports of the SDK at `base` pointed here. (Vivox
/// asks; there's no 32-bit Vivox stand-in.)
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub fn importers_patched(base: usize) -> usize {
    crate::sys::lock(&IMPORTERS).iter().filter(|(_, b)| *b == base).count()
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
            if module == *base || !crate::sys::lock(&PATCHED).insert((module, *base)) {
                continue;
            }
            let mut cache: HashMap<String, Option<usize>> = HashMap::new();
            let mut n = 0;
            let imports = unsafe { crate::sys::pe::imports(module as *const u8) };
            let theirs: Vec<_> = imports.iter().filter(|i| i.dll.eq_ignore_ascii_case(sdk)).collect();
            if theirs.iter().any(|i| unsafe { i.slot.read() } == i.unbound) {
                crate::sys::lock(&PATCHED).remove(&(module, *base));
                continue;
            }
            if !theirs.is_empty() {
                crate::sys::lock(&IMPORTERS).insert((module, *base));
            }
            for imp in theirs {
                let f = *cache
                    .entry(imp.name.clone())
                    .or_insert_with(|| resolve(&imp.name));
                if let Some(f) = f {
                    if unsafe { crate::sys::pe::write_slot(imp.slot, f) } {
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

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "system" fn mod_loader_a(_: usize, _: *const u8) -> usize {
        0xa11
    }
    unsafe extern "system" fn mod_loader_b(_: usize, _: *const u8) -> usize {
        0xb22
    }

    #[test]
    fn a_redirect_found_in_a_slot_still_gets_the_lookups_peerparty_doesnt_answer() {
        let a = chain(mod_loader_a as *const () as usize).unwrap();
        let b = chain(mod_loader_b as *const () as usize).unwrap();
        assert_ne!(a, b);
        // The same redirect found in another module's slot shares its detour.
        assert_eq!(chain(mod_loader_a as *const () as usize), Some(a));
        assert!(is_ours(a) && is_ours(b) && is_ours(detour_get_proc as *const () as usize));
        let name = c"il2cpp_init";
        for (f, want) in [(a, 0xa11), (b, 0xb22)] {
            let f: GetProcAddressFn = unsafe { std::mem::transmute(f) };
            assert_eq!(unsafe { f(0x1234_0000, name.as_ptr().cast()) }, want);
        }
    }
}
