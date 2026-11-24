//! Signature-agnostic export hooks. One stub per function saves every register the
//! ABI treats as volatile (Win64, or all of them on x86), logs the call, then jumps
//! to retour's trampoline. The original function still runs unchanged.

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;

use retour::RawDetour;

use crate::log;
use crate::sys::pe::Export;
use crate::sys::read;

static HOOK_LOCK: Mutex<()> = Mutex::new(());

/// Runs `f` (which must not allocate, lock or log: a paused thread may hold the heap or
/// a lock) with every other thread of the process paused, none of them with its next
/// instruction in the first bytes of the function at `at` (the bytes a hook overwrites:
/// a thread paused there would resume into the middle of the new jump). For hooks
/// switched on while the game runs (the overlay's); the start-up hooks go in before the
/// game's threads exist, so in the hook's own start (`watch::starting`, under the loader
/// lock) nothing is paused.
#[inline]
pub fn frozen<R>(at: usize, f: impl FnOnce() -> R) -> R {
    let paused = freeze(&[at]);
    let r = f();
    drop(paused);
    r
}

/// The process's other threads, paused until this is dropped (`freeze`).
pub struct Paused(Vec<(windows_sys::Win32::Foundation::HANDLE, bool)>);

impl Paused {
    fn suspend_all(&mut self) {
        for (h, paused) in &mut self.0 {
            if !*paused && unsafe { windows_sys::Win32::System::Threading::SuspendThread(*h) } != u32::MAX {
                *paused = true;
            }
        }
    }
    fn resume_all(&mut self) {
        for (h, paused) in &mut self.0 {
            if *paused {
                unsafe { windows_sys::Win32::System::Threading::ResumeThread(*h) };
                *paused = false;
            }
        }
    }
}

impl Drop for Paused {
    fn drop(&mut self) {
        // Resume every thread before closing handles or freeing the Vec.
        self.resume_all();
        for (h, _) in &self.0 {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(*h) };
        }
    }
}

/// Pauses every other thread of the process, none of them with its next instruction in
/// the first bytes of any function in `at` (see `frozen`), until the result is dropped;
/// None (nothing paused) in the hook's own start. One function, not generic: every
/// hook armed while the game runs goes through it.
#[inline(never)]
pub fn freeze(at: &[usize]) -> Option<Paused> {
    use windows_sys::Win32::Foundation::CloseHandle;
    if crate::watch::starting() {
        return None;
    }
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::System::Threading::*;
    let me = unsafe { GetCurrentThreadId() };
    let pid = unsafe { GetCurrentProcessId() };
    // Collected before pausing anything (this allocates).
    let mut ids: Vec<u32> = Vec::with_capacity(256);
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap as isize != -1 {
        let mut e: THREADENTRY32 = unsafe { std::mem::zeroed() };
        e.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut ok = unsafe { Thread32First(snap, &mut e) } != 0;
        while ok {
            if e.th32OwnerProcessID == pid && e.th32ThreadID != me {
                ids.push(e.th32ThreadID);
            }
            ok = unsafe { Thread32Next(snap, &mut e) } != 0;
        }
        unsafe { CloseHandle(snap) };
    }
    let mut threads = Paused(Vec::with_capacity(ids.len()));
    for id in &ids {
        let h = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, 0, *id) };
        if !h.is_null() {
            threads.0.push((h, false));
        }
    }
    // A thread caught in those bytes runs on for a moment, then all are paused again.
    for _ in 0..100 {
        threads.suspend_all();
        if !threads.0.iter().any(|(h, paused)| *paused && at.iter().any(|&a| inside(*h, a))) {
            break;
        }
        threads.resume_all();
        unsafe { Sleep(1) };
    }
    Some(threads)
}

/// Whether a paused thread's next instruction is in the 32 bytes from `at`.
fn inside(thread: windows_sys::Win32::Foundation::HANDLE, at: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::{CONTEXT, GetThreadContext};
    if at == 0 {
        return false;
    }
    // GetThreadContext requires 16-byte alignment on x86_64; windows-sys's CONTEXT is 8-aligned.
    #[repr(C, align(16))]
    struct Aligned(CONTEXT);
    let mut ctx: Aligned = unsafe { std::mem::zeroed() };
    #[cfg(target_arch = "x86_64")]
    {
        ctx.0.ContextFlags = 0x0010_0001; // CONTEXT_CONTROL (AMD64)
    }
    #[cfg(target_arch = "x86")]
    {
        ctx.0.ContextFlags = 0x0001_0001; // CONTEXT_CONTROL (x86)
    }
    if unsafe { GetThreadContext(thread, &mut ctx.0) } == 0 {
        return false;
    }
    #[cfg(target_arch = "x86_64")]
    let ip = ctx.0.Rip as usize;
    #[cfg(target_arch = "x86")]
    let ip = ctx.0.Eip as usize;
    (at..at + 32).contains(&ip)
}

static ARENA: Mutex<Arena> = Mutex::new(Arena { chunks: Vec::new() });

#[cfg(test)]
pub static LAST_ARGS: Mutex<[usize; 7]> = Mutex::new([0; 7]);

struct Arena {
    /// (where it's written, where it runs, capacity, used) for each slab (`wx_block`).
    /// Old slabs stay mapped: stubs in them keep running.
    chunks: Vec<(usize, usize, usize, usize)>,
}

/// Memory for code made at run time, mapped twice: written through a read-write view,
/// run from a read-execute one (the same pages), so no page is ever writable and
/// executable at once. (write, run) addresses; the code must not depend on where it
/// is, which every stub and thunk here doesn't (absolute targets, `[rip+0]`).
pub(crate) fn wx_block(size: usize) -> Option<(usize, usize)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_EXECUTE, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_EXECUTE_READWRITE};
    unsafe {
        let section = CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_EXECUTE_READWRITE, (size as u64 >> 32) as u32, size as u32, std::ptr::null());
        if section.is_null() {
            return None;
        }
        let write = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, size).Value as usize;
        let run = MapViewOfFile(section, FILE_MAP_READ | FILE_MAP_EXECUTE, 0, 0, size).Value as usize;
        // The views keep the section.
        CloseHandle(section);
        (write != 0 && run != 0).then_some((write, run))
    }
}

pub struct Summary {
    pub hooked: usize,
    pub failed: usize,
    pub samples: Vec<String>,
}

#[inline]
pub fn with_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = hook_lock();
    f()
}

/// The process-wide hook lock itself, held until the guard drops (`with_lock`).
pub fn hook_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::sys::lock(&HOOK_LOCK)
}

/// Hooks every code export except `skip` (already typed-hooked) and anti-cheat
/// entry points, which are never detoured.
pub fn install(exports: &[Export], skip: &HashSet<usize>) -> Summary {
    with_lock(|| install_locked(exports, skip))
}

fn install_locked(exports: &[Export], skip: &HashSet<usize>) -> Summary {
    let mut groups: HashMap<usize, Vec<String>> = HashMap::new();
    let mut order = Vec::new();
    for export in exports {
        let addr = export.addr as usize;
        if addr == 0 || skip.contains(&addr) {
            continue;
        }
        if export.name.contains("AntiCheat") || export.name.contains("EasyAntiCheat") {
            continue;
        }
        if !groups.contains_key(&addr) {
            order.push(addr);
        }
        groups.entry(addr).or_default().push(export.name.clone());
    }

    let mut summary = Summary {
        hooked: 0,
        failed: 0,
        samples: Vec::new(),
    };
    for addr in order {
        let names = &groups[&addr];
        let label = primary(names);
        let name_ptr = leak_cstr(&label);
        let (stub, stub_w, tramp_off) = match place_stub(name_ptr) {
            Some(v) => v,
            None => {
                note_fail(&mut summary, format!("{label}: stub alloc failed"));
                continue;
            }
        };
        let built = std::panic::catch_unwind(AssertUnwindSafe(|| unsafe {
            RawDetour::new(addr as *const (), stub as *const ())
        }));
        let detour = match built {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => {
                note_fail(&mut summary, format!("{label}: {e}"));
                continue;
            }
            Err(_) => {
                note_fail(&mut summary, format!("{label}: disassembler panicked"));
                continue;
            }
        };
        let tramp = detour.trampoline() as *const () as usize;
        if tramp == 0 {
            note_fail(&mut summary, format!("{label}: no trampoline"));
            continue;
        }
        unsafe { stub_w.add(tramp_off).cast::<usize>().write_unaligned(tramp) };
        if let Err(e) = unsafe { detour.enable() } {
            note_fail(&mut summary, format!("{label}: enable {e}"));
            continue;
        }
        // Leak so Drop doesn't unhook while the game is still inside a call.
        // The trampoline has to outlive this function, not just skip its destructor.
        Box::leak(Box::new(detour));
        summary.hooked += 1;
    }
    summary
}

fn note_fail(summary: &mut Summary, msg: String) {
    summary.failed += 1;
    if summary.samples.len() < 8 {
        summary.samples.push(msg);
    }
}

fn primary(names: &[String]) -> String {
    names
        .iter()
        .find(|n| wants_detail(n))
        .cloned()
        .unwrap_or_else(|| names[0].clone())
}

fn leak_cstr(name: &str) -> *const u8 {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    let boxed = bytes.into_boxed_slice();
    let ptr = boxed.as_ptr();
    std::mem::forget(boxed);
    ptr
}

/// A stub for export `name`: (where it runs, where it's written, the trampoline
/// address's offset in it).
fn place_stub(name: *const u8) -> Option<(*mut u8, *mut u8, usize)> {
    let (bytes, tramp_off) = stub_bytes(name as usize, on_enter as *const () as usize);
    let mut arena = crate::sys::lock(&ARENA);
    let need = (bytes.len() + 15) & !15;
    let last_ok = arena
        .chunks
        .last()
        .is_some_and(|(_, _, cap, used)| cap - used >= need);
    if !last_ok {
        let cap = need.max(0x1_0000);
        let (write, run) = wx_block(cap)?;
        arena.chunks.push((write, run, cap, 0));
    }
    let (write, run, _, used) = arena.chunks.last_mut()?;
    let (w, r) = ((*write + *used) as *mut u8, (*run + *used) as *mut u8);
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), w, bytes.len()) };
    *used += need;
    Some((r, w, tramp_off))
}

/// Builds the stub. `tramp_off` is the offset of the pointer-sized absolute trampoline
/// address, patched after `RawDetour::new` (the address doesn't exist before that).
#[cfg(target_arch = "x86")]
fn stub_bytes(name: usize, logger: usize) -> (Vec<u8>, usize) {
    let mut c = Vec::with_capacity(64);
    // Every x86 convention the SDKs use (cdecl, stdcall) passes arguments on the
    // stack, so the logger reads copies and the original stack is left as it was.
    // After pushfd + pushad (36 bytes) the return address is at [esp+0x24] and
    // argument k at [esp+0x24+4k]. Each push moves esp by 4, so pushing arguments
    // 7 down to 1 reads [esp+0x40] every time.
    c.extend_from_slice(&[0x9C, 0x60]); // pushfd ; pushad
    for _ in 0..7 {
        c.extend_from_slice(&[0xFF, 0x74, 0x24, 0x40]); // push dword [esp+0x40]
    }
    c.push(0x68); // push name
    c.extend_from_slice(&(name as u32).to_le_bytes());
    c.push(0xB8); // mov eax, logger
    c.extend_from_slice(&(logger as u32).to_le_bytes());
    c.extend_from_slice(&[0xFF, 0xD0]); // call eax (cdecl)
    c.extend_from_slice(&[0x83, 0xC4, 0x20]); // add esp, 32
    c.extend_from_slice(&[0x61, 0x9D]); // popad ; popfd
    // There is no RIP-relative jmp on x86, so jump with push imm32 + ret.
    c.push(0x68);
    let tramp_off = c.len();
    c.extend_from_slice(&[0u8; 4]);
    c.push(0xC3);
    (c, tramp_off)
}

#[cfg(target_arch = "x86_64")]
fn stub_bytes(name: usize, logger: usize) -> (Vec<u8>, usize) {
    let (name, logger) = (name as u64, logger as u64);
    let mut c = Vec::with_capacity(220);
    // Entry RSP ≡ 8 (mod 16): the game `call`ed the export, and retour jumps here
    // without touching RSP. Eight pushes keep that alignment. `sub rsp, 0xA8`
    // (8 mod 16) then leaves RSP ≡ 0, which the next `call` requires.
    // `sub rsp, imm8` would sign-extend, and 0xA8 does not fit in a positive imm8,
    // so this is the imm32 form.
    //
    // After the subtract, saved GPRs sit at:
    //   rbx +0xA8, r11 +0xB0, r10 +0xB8, r9 +0xC0, r8 +0xC8, rdx +0xD0, rcx +0xD8, rax +0xE0
    // and the caller's stack args are shifted by 0xE8:
    //   arg5 +0x110, arg6 +0x118, arg7 +0x120.
    c.extend_from_slice(&[
        0x50, // push rax
        0x51, // push rcx
        0x52, // push rdx
        0x41, 0x50, // push r8
        0x41, 0x51, // push r9
        0x41, 0x52, // push r10
        0x41, 0x53, // push r11
        0x53, // push rbx
        0x48, 0x81, 0xEC, 0xA8, 0x00, 0x00, 0x00, // sub rsp, 0xA8
    ]);
    // xmm0-5 are volatile. Steam's ABI doesn't pass vectors, but a float argument
    // travels in xmm, and the callee is allowed to trust what the caller passed.
    for i in 0..6u8 {
        movups(&mut c, true, i, 0x40 + i as u32 * 0x10);
    }
    c.extend_from_slice(&[0x48, 0xB9]); // mov rcx, name
    c.extend_from_slice(&name.to_le_bytes());
    mov_gpr(&mut c, 0x8B, 2, 0xD8); // mov rdx, [rsp+0xD8]  orig rcx
    mov_gpr(&mut c, 0x8B, 8, 0xD0); // mov r8,  [rsp+0xD0]  orig rdx
    mov_gpr(&mut c, 0x8B, 9, 0xC8); // mov r9,  [rsp+0xC8]  orig r8
    mov_gpr(&mut c, 0x8B, 0, 0xC0); // mov rax, [rsp+0xC0]  orig r9
    c.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]); // mov [rsp+0x20], rax
    mov_gpr(&mut c, 0x8B, 0, 0x110); // mov rax, [rsp+0x110] arg5
    c.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x28]);
    mov_gpr(&mut c, 0x8B, 0, 0x118);
    c.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x30]);
    mov_gpr(&mut c, 0x8B, 0, 0x120);
    c.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x38]);
    c.extend_from_slice(&[0x48, 0xB8]); // mov rax, logger
    c.extend_from_slice(&logger.to_le_bytes());
    c.extend_from_slice(&[0xFF, 0xD0]); // call rax
    for i in 0..6u8 {
        movups(&mut c, false, i, 0x40 + i as u32 * 0x10);
    }
    c.extend_from_slice(&[
        0x48, 0x81, 0xC4, 0xA8, 0x00, 0x00, 0x00, // add rsp, 0xA8
        0x5B, // pop rbx
        0x41, 0x5B, // pop r11
        0x41, 0x5A, // pop r10
        0x41, 0x59, // pop r9
        0x41, 0x58, // pop r8
        0x5A, // pop rdx
        0x59, // pop rcx
        0x58, // pop rax
        0xFF, 0x25, 0x00, 0x00, 0x00, 0x00, // jmp qword ptr [rip+0]
    ]);
    let tramp_off = c.len();
    c.extend_from_slice(&[0u8; 8]);
    (c, tramp_off)
}

/// `store`: `movups [rsp+disp], xmm`. Otherwise `movups xmm, [rsp+disp]`.
#[cfg(target_arch = "x86_64")]
fn movups(c: &mut Vec<u8>, store: bool, xmm: u8, disp: u32) {
    let modrm = 0x84 | (xmm << 3);
    c.push(0x0F);
    c.push(if store { 0x11 } else { 0x10 });
    c.push(modrm);
    c.push(0x24);
    c.extend_from_slice(&disp.to_le_bytes());
}

/// `mov reg, [rsp+disp32]`. `reg` is 0..7 for rax..rdi, or 8/9 for r8/r9.
#[cfg(target_arch = "x86_64")]
fn mov_gpr(c: &mut Vec<u8>, opcode: u8, reg: u8, disp: u32) {
    let rex = if reg >= 8 { 0x4C } else { 0x48 };
    let modrm = 0x84 | ((reg & 7) << 3);
    c.extend_from_slice(&[rex, opcode, modrm, 0x24]);
    c.extend_from_slice(&disp.to_le_bytes());
}

unsafe extern "C" fn on_enter(
    name: *const u8,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
) {
    let args = [a1, a2, a3, a4, a5, a6, a7];
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if name.is_null() {
            return;
        }
        let text = unsafe { CStr::from_ptr(name.cast()) }.to_string_lossy();
        #[cfg(test)]
        {
            if let Ok(mut slot) = LAST_ARGS.lock() {
                *slot = args;
            }
        }
        let extra = if wants_detail(&text) {
            detail(&text, args)
        } else {
            String::new()
        };
        let key = if extra.is_empty() {
            text.to_string()
        } else {
            format!("{text} {extra}")
        };
        log::throttled(&key, 4, |n| {
            if extra.is_empty() {
                log!("call {text} n={n}");
            } else {
                log!("call {text} n={n} {extra}");
            }
        });
        if let Some(method) = bypassed(&text, a1) {
            log::gap("steam", &format!("{method} ran on steam_api's own object, not PeerParty's stand-in: the game got that object some way PeerParty doesn't wrap, so this never reaches the LAN"));
        }
    }));
}

/// The Steam multiplayer methods whose flat function, called on an object that isn't
/// one of PeerParty's stand-ins, means the game plays on steam_api's own objects:
/// a lobby made or an invite sent that way never reaches PeerParty. A flat function
/// that came to PeerParty through the by-name swap isn't traced, and one called on a
/// stand-in goes on to PeerParty's method, so neither is counted.
const MULTIPLAYER: &[&str] = &[
    "SteamAPI_ISteamMatchmaking_CreateLobby",
    "SteamAPI_ISteamMatchmaking_JoinLobby",
    "SteamAPI_ISteamMatchmaking_RequestLobbyList",
    "SteamAPI_ISteamMatchmaking_InviteUserToLobby",
    "SteamAPI_ISteamFriends_ActivateGameOverlayInviteDialog",
    "SteamAPI_ISteamNetworking_SendP2PPacket",
    "SteamAPI_ISteamNetworkingSockets_ConnectP2P",
    "SteamAPI_ISteamNetworkingMessages_SendMessageToUser",
];

/// `name`'s method ("ISteamMatchmaking CreateLobby") when it's one of `MULTIPLAYER` and
/// `this` isn't a stand-in, while PeerParty's Steam layer is on.
fn bypassed(name: &str, this: usize) -> Option<String> {
    if !MULTIPLAYER.contains(&name) || this == 0 || !crate::net::enabled() || crate::net::skip("steam") {
        return None;
    }
    if crate::steam::obj::is_stand_in(this) {
        return None;
    }
    let (iface, method) = name.strip_prefix("SteamAPI_")?.split_once('_')?;
    Some(format!("{iface} {method}"))
}

fn wants_detail(name: &str) -> bool {
    matches!(
        name,
        "CreateInterface"
            | "SteamInternal_CreateInterface"
            | "SteamInternal_FindOrCreateUserInterface"
            | "SteamInternal_FindOrCreateGameServerInterface"
            | "SteamAPI_RegisterCallback"
            | "SteamAPI_UnregisterCallback"
            | "SteamAPI_RegisterCallResult"
            | "SteamAPI_UnregisterCallResult"
            | "SteamInternal_SteamAPI_Init"
            | "SteamInternal_GameServer_Init_V2"
    )
}

pub fn detail(name: &str, a: [usize; 7]) -> String {
    match name {
        "CreateInterface" | "SteamInternal_CreateInterface" => format!("ver={}", show(a[0])),
        "SteamInternal_FindOrCreateUserInterface"
        | "SteamInternal_FindOrCreateGameServerInterface" => {
            format!("user={} ver={}", a[0], show(a[1]))
        }
        "SteamAPI_RegisterCallback" => format!("id={} obj={:#x}", a[1] as u32, a[0]),
        "SteamAPI_UnregisterCallback" => format!("obj={:#x}", a[0]),
        "SteamAPI_RegisterCallResult" | "SteamAPI_UnregisterCallResult" => {
            format!("call={:#x} obj={:#x}", a[1], a[0])
        }
        "SteamInternal_SteamAPI_Init" => {
            format!(
                "interfaces={}",
                read::steam_version_blob(a[0]).unwrap_or_else(|| "-".into())
            )
        }
        // uint32 ip, uint16 game, uint16 query, int mode, const char* version,
        // version blob, SteamErrMsg*. The error buffer is an output.
        "SteamInternal_GameServer_Init_V2" => {
            let version = read::cstr(a[4]).unwrap_or_else(|| "-".into());
            let interfaces = read::steam_version_blob(a[5]).unwrap_or_else(|| "-".into());
            format!(
                "ip={:#x} port={} query={} mode={} version={version} interfaces={interfaces}",
                a[0] as u32, a[1] as u16, a[2] as u16, a[3] as u32
            )
        }
        _ => String::new(),
    }
}

fn show(p: usize) -> String {
    read::cstr(p).unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::hint::black_box;
    use std::path::PathBuf;

    #[test]
    fn decodes_version_callback_and_init_blob() {
        let ver = CString::new("SteamNetworkingSockets012").unwrap();
        let got = detail(
            "SteamInternal_FindOrCreateUserInterface",
            [9, ver.as_ptr() as usize, 0, 0, 0, 0, 0],
        );
        assert_eq!(got, "user=9 ver=SteamNetworkingSockets012");
        let got = detail("SteamAPI_RegisterCallback", [0x1000, 333, 0, 0, 0, 0, 0]);
        assert_eq!(got, "id=333 obj=0x1000");
        let mut blob = b"SteamUser023\0SteamFriends017\0\0".to_vec();
        let got = detail(
            "SteamInternal_SteamAPI_Init",
            [blob.as_mut_ptr() as usize, 0, 0, 0, 0, 0, 0],
        );
        assert_eq!(got, "interfaces=SteamUser023 SteamFriends017");
    }

    #[inline(never)]
    unsafe extern "C" fn sum7(
        a: usize,
        b: usize,
        c: usize,
        d: usize,
        e: usize,
        f: usize,
        g: usize,
    ) -> usize {
        a + b + c + d + e + f + g
    }

    #[inline(never)]
    unsafe extern "C" fn mix(n: u64, f: f64) -> f64 {
        f + n as f64
    }

    #[inline(never)]
    unsafe extern "C" fn find_iface(user: usize, ver: *const i8) -> usize {
        let _ = ver;
        user
    }

    #[cfg(target_arch = "x86_64")]
    static FLAG: i32 = -1;

    /// A crash seen in a game: a prologue starting with `cmp dword [rip+d], imm8`. The
    /// trampoline has to keep both the displacement and the trailing immediate.
    /// x86 has no RIP-relative addressing.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn relocates_rip_operand_with_trailing_immediate() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc};
        let code = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                0x1000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        } as *mut u8;
        assert!(!code.is_null());
        // cmp dword [rip+d], -1 ; sete al ; movzx eax, al ; ret
        // d points at a copy of FLAG placed right after the code.
        let mut f = vec![
            0x83, 0x3D, 0, 0, 0, 0, 0xFF, 0x0F, 0x94, 0xC0, 0x0F, 0xB6, 0xC0, 0xC3,
        ];
        let data_off = 0x40usize;
        let disp = (data_off - 7) as i32;
        f[2..6].copy_from_slice(&disp.to_le_bytes());
        unsafe {
            std::ptr::copy_nonoverlapping(f.as_ptr(), code, f.len());
            code.add(data_off).cast::<i32>().write(FLAG);
        }
        let func: unsafe extern "C" fn() -> u32 = unsafe { std::mem::transmute(code) };
        assert_eq!(unsafe { black_box(func)() }, 1);

        let summary = install(
            &[Export {
                name: "CmpRipImm8".into(),
                addr: code as *const (),
            }],
            &HashSet::new(),
        );
        assert_eq!(summary.hooked, 1, "{:?}", summary.samples);
        assert_eq!(unsafe { black_box(func)() }, 1);
        unsafe { code.add(data_off).cast::<i32>().write(0) };
        assert_eq!(unsafe { black_box(func)() }, 0);
    }

    #[test]
    fn stub_preserves_registers_and_logs_the_version() {
        let dir = std::env::temp_dir().join(format!("peerparty-hook-test-{}", std::process::id()));
        unsafe { std::env::set_var("PEERPARTY_LOG_DIR", &dir) };
        crate::config::reload();
        let log_path = log::open(PathBuf::from("."), "stub").expect("log");

        install(
            &[Export {
                name: "SumSeven".into(),
                addr: sum7 as *const (),
            }],
            &HashSet::new(),
        );
        install(
            &[Export {
                name: "Mix".into(),
                addr: mix as *const (),
            }],
            &HashSet::new(),
        );
        install(
            &[Export {
                name: "SteamInternal_FindOrCreateUserInterface".into(),
                addr: find_iface as *const (),
            }],
            &HashSet::new(),
        );

        let sum7 = black_box(
            sum7 as unsafe extern "C" fn(usize, usize, usize, usize, usize, usize, usize) -> usize,
        );
        assert_eq!(unsafe { sum7(1, 2, 3, 4, 5, 6, 7) }, 28);
        assert_eq!(*crate::sys::lock(&LAST_ARGS), [1, 2, 3, 4, 5, 6, 7]);

        let mix = black_box(mix as unsafe extern "C" fn(u64, f64) -> f64);
        let got = unsafe { mix(4, 1.5) };
        assert!((got - 5.5).abs() < 1e-9, "{got}");

        let ver = CString::new("SteamMatchMaking009").unwrap();
        let find = black_box(find_iface as unsafe extern "C" fn(usize, *const i8) -> usize);
        assert_eq!(unsafe { find(7, ver.as_ptr()) }, 7);

        let text = std::fs::read_to_string(&log_path).unwrap();
        assert!(text.contains("call SumSeven n=1"), "{text}");
        assert!(
            text.contains("SteamInternal_FindOrCreateUserInterface"),
            "{text}"
        );
        assert!(text.contains("SteamMatchMaking009"), "{text}");
        assert!(text.contains("user=7"), "{text}");
    }

    unsafe extern "C" fn ret_22() -> u32 {
        22
    }

    /// `RawDetour::enable`/`disable` run inside `frozen` while every other thread is
    /// paused and may hold the process heap lock; toggling must never touch the heap,
    /// and must restore distinct protections across a straddled page boundary.
    #[test]
    fn enable_does_not_allocate_while_process_heap_is_locked() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows_sys::Win32::System::Memory::{
            GetProcessHeap, HeapLock, HeapUnlock, MEMORY_BASIC_INFORMATION, MEM_COMMIT,
            MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualProtect, VirtualQuery,
        };

        let pages = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                0x2000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        } as *mut u8;
        assert!(!pages.is_null());

        // `mov eax, 11 ; ret` (6 bytes, valid on x86 and x86_64) straddling the 4 KiB
        // boundary between page 0 (RX) and page 1 (RWX).
        let entry = unsafe { pages.add(0x1000 - 2) };
        let body: [u8; 6] = [0xB8, 11, 0, 0, 0, 0xC3];
        unsafe { std::ptr::copy_nonoverlapping(body.as_ptr(), entry, body.len()) };

        let mut old = 0u32;
        assert_ne!(unsafe { VirtualProtect(pages.cast(), 0x1000, PAGE_EXECUTE_READ, &mut old) }, 0);

        let func: unsafe extern "C" fn() -> u32 = unsafe { std::mem::transmute(entry) };
        assert_eq!(unsafe { black_box(func)() }, 11);

        let prot = |p: *const u8| -> u32 {
            let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            assert_ne!(unsafe { VirtualQuery(p.cast(), &mut mbi, size_of::<MEMORY_BASIC_INFORMATION>()) }, 0);
            mbi.Protect
        };

        // Hold HOOK_LOCK so no parallel test can hold retour's POOL lock while blocked on the heap.
        with_lock(|| {
            let detour = unsafe { RawDetour::new(entry.cast(), ret_22 as *const ()) }.unwrap();
            let locked = Arc::new(AtomicBool::new(false));
            let release = Arc::new(AtomicBool::new(false));
            let (l, r) = (locked.clone(), release.clone());
            let holder = std::thread::spawn(move || unsafe {
                let heap = GetProcessHeap();
                assert_ne!(HeapLock(heap), 0);
                l.store(true, Ordering::Release);
                while !r.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                assert_ne!(HeapUnlock(heap), 0);
            });
            struct ReleaseGuard(Arc<AtomicBool>);
            impl Drop for ReleaseGuard {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let guard = ReleaseGuard(release);
            while !locked.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }

            let e1 = unsafe { detour.enable() };
            let e2 = unsafe { detour.disable() };
            let e3 = unsafe { detour.enable() };

            drop(guard);
            holder.join().unwrap();

            e1.unwrap();
            e2.unwrap();
            e3.unwrap();
            assert_eq!(unsafe { black_box(func)() }, 22);
            assert_eq!(prot(pages), PAGE_EXECUTE_READ);
            assert_eq!(prot(unsafe { pages.add(0x1000) }), PAGE_EXECUTE_READWRITE);

            unsafe { detour.disable() }.unwrap();
            assert_eq!(unsafe { black_box(func)() }, 11);
            assert_eq!(prot(pages), PAGE_EXECUTE_READ);
            assert_eq!(prot(unsafe { pages.add(0x1000) }), PAGE_EXECUTE_READWRITE);
        });

        unsafe { VirtualFree(pages.cast(), 0, MEM_RELEASE) };
    }

    #[test]
    fn frozen_enable_does_not_deadlock_with_allocating_threads() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
            VirtualAlloc, VirtualFree, VirtualProtect,
        };

        let code = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                0x1000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        } as *mut u8;
        assert!(!code.is_null());
        let body: [u8; 6] = [0xB8, 11, 0, 0, 0, 0xC3];
        unsafe { std::ptr::copy_nonoverlapping(body.as_ptr(), code, body.len()) };
        let mut old = 0u32;
        assert_ne!(unsafe { VirtualProtect(code.cast(), 0x1000, PAGE_EXECUTE_READ, &mut old) }, 0);

        let stop = Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = (0..4)
            .map(|i| {
                let s = stop.clone();
                std::thread::spawn(move || {
                    let mut n = 0usize;
                    while !s.load(Ordering::Relaxed) {
                        let v = vec![i as u8; 64 + (n & 1023)];
                        black_box(v);
                        n = n.wrapping_add(1);
                    }
                })
            })
            .collect();

        with_lock(|| {
            let detour = unsafe { RawDetour::new(code.cast(), ret_22 as *const ()) }.unwrap();
            for _ in 0..64 {
                frozen(code as usize, || unsafe { detour.enable() }).unwrap();
                frozen(code as usize, || unsafe { detour.disable() }).unwrap();
            }
        });

        stop.store(true, Ordering::Relaxed);
        for w in workers {
            w.join().unwrap();
        }
        unsafe { VirtualFree(code.cast(), 0, MEM_RELEASE) };
    }
}
