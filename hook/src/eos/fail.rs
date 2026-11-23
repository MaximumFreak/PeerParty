//! What EOS functions the LAN layer doesn't implement do instead of reaching the real
//! SDK (which would dereference PeerParty's ids and handles, or wait on Epic's servers).
//! The class of each comes from tools/eos_table.py.
//!
//! The handlers don't know which function they stand in for, only its shape. On x86 an
//! EOS function is stdcall, so the callee pops its arguments and each argument count
//! needs its own handler; const generics give one per (class, slots).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::abi::result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// The real SDK answers.
    Pass,
    /// `(handle, options, client_data, callback)`: completes with EOS_NoConnection.
    Async,
    /// The same, with a pointer-sized field (a user id) between the callback info's
    /// ResultCode and ClientData (Mods, ProgressionSnapshot_DeleteSnapshot).
    AsyncAfterPtr,
    /// The same, with a 32-bit field there (ProgressionSnapshot_SubmitSnapshot).
    AsyncAfterU32,
    /// Returns a notification id that never fires.
    AddNotify,
    /// Returns EOS_NotFound and nulls the out handle at this argument index.
    Copy(u8),
    /// Returns EOS_NoConnection.
    Result,
    /// Returns 0 / null / false.
    Zero,
    Void,
}

/// Notification ids are shared with the implemented interfaces, so ids never collide.
pub static NEXT_NOTIFY: AtomicU64 = AtomicU64::new(1);

/// A failed callback info: ResultCode first, ClientData in pointer-sized slot `at`,
/// and room for whatever else the info has, all zero / null.
fn failed_info(client_data: usize, at: usize) -> [usize; 34] {
    let mut info = [0usize; 34];
    info[0] = result::NO_CONNECTION as u32 as usize;
    info[at] = client_data;
    info
}

/// The pointer-sized slot ClientData is in, by class: after ResultCode it is slot 1;
/// after ResultCode and a pointer, slot 2; after ResultCode and a uint32_t, slot 1 on
/// x64 (both share the first eight bytes) and slot 2 on x86.
fn client_data_slot(class: u8) -> usize {
    match class {
        ASYNC_AFTER_PTR => 2,
        ASYNC_AFTER_U32 if cfg!(target_pointer_width = "64") => 1,
        ASYNC_AFTER_U32 => 2,
        _ => 1,
    }
}

unsafe fn run(class: u8, a: &[usize]) -> usize {
    match class {
        ASYNC | ASYNC_AFTER_PTR | ASYNC_AFTER_U32 => {
            let (client_data, callback) = (a[2], a[3]);
            let at = client_data_slot(class);
            if callback != 0 {
                super::later(move || {
                    let info = failed_info(client_data, at);
                    let f: unsafe extern "system" fn(*const usize) = unsafe { std::mem::transmute(callback) };
                    unsafe { f(info.as_ptr()) };
                });
            }
            0
        }
        ADD_NOTIFY => NEXT_NOTIFY.fetch_add(1, Ordering::Relaxed) as usize,
        RESULT => result::NO_CONNECTION as u32 as usize,
        ZERO | VOID => 0,
        c => {
            let out = a.get((c - COPY) as usize).copied().unwrap_or(0);
            if out != 0 {
                unsafe { (out as *mut usize).write(0) };
            }
            result::NOT_FOUND as u32 as usize
        }
    }
}

const ASYNC: u8 = 0;
const ADD_NOTIFY: u8 = 1;
const RESULT: u8 = 2;
const ZERO: u8 = 3;
const VOID: u8 = 4;
const ASYNC_AFTER_PTR: u8 = 5;
const ASYNC_AFTER_U32: u8 = 6;
const COPY: u8 = 10;

macro_rules! handlers {
    ($($name:ident($($a:ident),*);)*) => {
        $(
            // A 64-bit return: EOS_NotificationId is a uint64_t, which x86 returns in
            // edx:eax (edx is zero here); on x64 it's rax either way. Callers of the
            // narrower results ignore edx.
            unsafe extern "system" fn $name<const C: u8>($($a: usize),*) -> u64 {
                unsafe { run(C, &[$($a),*]) as u64 }
            }
        )*
    };
}

handlers! {
    h0();
    h1(a0);
    h2(a0, a1);
    h3(a0, a1, a2);
    h4(a0, a1, a2, a3);
    h5(a0, a1, a2, a3, a4);
    h6(a0, a1, a2, a3, a4, a5);
    h7(a0, a1, a2, a3, a4, a5, a6);
    h8(a0, a1, a2, a3, a4, a5, a6, a7);
    h9(a0, a1, a2, a3, a4, a5, a6, a7, a8);
    h10(a0, a1, a2, a3, a4, a5, a6, a7, a8, a9);
}

fn pick<const C: u8>(slots: u8) -> Option<usize> {
    Some(match slots {
        0 => h0::<C> as *const () as usize,
        1 => h1::<C> as *const () as usize,
        2 => h2::<C> as *const () as usize,
        3 => h3::<C> as *const () as usize,
        4 => h4::<C> as *const () as usize,
        5 => h5::<C> as *const () as usize,
        6 => h6::<C> as *const () as usize,
        7 => h7::<C> as *const () as usize,
        8 => h8::<C> as *const () as usize,
        9 => h9::<C> as *const () as usize,
        10 => h10::<C> as *const () as usize,
        _ => return None,
    })
}

/// The stand-in for a function of this class taking `slots` 4-byte stack slots on x86.
/// `None` for Pass, or a shape too wide to stand in for.
pub fn handler(class: Class, slots: u8) -> Option<usize> {
    match class {
        Class::Pass => None,
        Class::Async if slots >= 4 => pick::<ASYNC>(slots),
        Class::Async => None,
        Class::AsyncAfterPtr if slots >= 4 => pick::<ASYNC_AFTER_PTR>(slots),
        Class::AsyncAfterU32 if slots >= 4 => pick::<ASYNC_AFTER_U32>(slots),
        Class::AsyncAfterPtr | Class::AsyncAfterU32 => None,
        Class::AddNotify => pick::<ADD_NOTIFY>(slots),
        Class::Result => pick::<RESULT>(slots),
        Class::Zero => pick::<ZERO>(slots),
        Class::Void => pick::<VOID>(slots),
        Class::Copy(1) => pick::<{ COPY + 1 }>(slots),
        Class::Copy(2) => pick::<{ COPY + 2 }>(slots),
        Class::Copy(3) => pick::<{ COPY + 3 }>(slots),
        Class::Copy(4) => pick::<{ COPY + 4 }>(slots),
        Class::Copy(_) => pick::<RESULT>(slots),
    }
}

// ---- gap notes ----
//
// The handlers are shared by every function of a shape, so they can't say which one the
// game called. `noted` puts a small thunk in front of a handler, one per function: it
// tells `called` the function's id, then jumps to the handler with the arguments as
// they were. The first call of each logs `gap: eos <name>`, so a game's log lists what
// it uses outside the LAN layer (functions it only imports stay quiet).

/// Thunks per block, and the bytes each takes.
const THUNKS: usize = 4096;
const THUNK: usize = 64;

static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static CALLED: [AtomicBool; THUNKS] = [const { AtomicBool::new(false) }; THUNKS];

unsafe extern "system" fn called(id: usize) {
    if CALLED.get(id).is_some_and(|c| !c.swap(true, Ordering::Relaxed)) {
        let name = NAMES.lock().ok().and_then(|n| n.get(id).copied()).unwrap_or("?");
        crate::log::gap("eos", &format!("{name} (outside the LAN layer: answered as offline)"));
    }
}

/// The thunks' memory: (where they're written, where they run). Written while earlier
/// thunks in it may be running, so through a view of its own (crate::sys::hook::wx_block).
fn block() -> Option<(usize, usize)> {
    static BASE: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    *BASE.get_or_init(|| crate::sys::hook::wx_block(THUNKS * THUNK))
}

fn thunk_code(id: usize, handler: usize) -> Vec<u8> {
    let note = called as *const () as usize;
    #[cfg(target_arch = "x86_64")]
    {
        // Keep the four register arguments across the call; rsp stays 16-byte aligned
        // with 32 bytes of shadow space. Stack arguments aren't touched.
        let mut c = vec![0x51, 0x52, 0x41, 0x50, 0x41, 0x51, 0x48, 0x83, 0xEC, 0x28]; // push rcx, rdx, r8, r9; sub rsp,0x28
        c.extend([0x48, 0xB9]); // mov rcx, id
        c.extend((id as u64).to_le_bytes());
        c.extend([0x48, 0xB8]); // mov rax, called
        c.extend((note as u64).to_le_bytes());
        c.extend([0xFF, 0xD0, 0x48, 0x83, 0xC4, 0x28, 0x41, 0x59, 0x41, 0x58, 0x5A, 0x59]); // call rax; add rsp,0x28; pop r9, r8, rdx, rcx
        c.extend([0x48, 0xB8]); // mov rax, handler
        c.extend((handler as u64).to_le_bytes());
        c.extend([0xFF, 0xE0]); // jmp rax
        c
    }
    #[cfg(target_arch = "x86")]
    {
        // stdcall: `called` pops its own argument; the game's are left as they were.
        let mut c = vec![0x68]; // push id
        c.extend((id as u32).to_le_bytes());
        c.push(0xB8); // mov eax, called
        c.extend((note as u32).to_le_bytes());
        c.extend([0xFF, 0xD0, 0xB8]); // call eax; mov eax, handler
        c.extend((handler as u32).to_le_bytes());
        c.extend([0xFF, 0xE0]); // jmp eax
        c
    }
}

/// `handler` behind a thunk that logs the first call of `name` as a gap. Bookkeeping
/// (`_Release`, `_RemoveNotify...`) isn't a gap and gets the handler itself.
pub fn noted(name: &str, handler: usize) -> usize {
    if name.ends_with("_Release") || name.contains("_RemoveNotify") {
        return handler;
    }
    let Some((write, base)) = block() else { return handler };
    let mut names = crate::sys::lock(&NAMES);
    if let Some(i) = names.iter().position(|n| *n == name) {
        return base + i * THUNK;
    }
    let id = names.len();
    if id >= THUNKS {
        return handler;
    }
    let code = thunk_code(id, handler);
    debug_assert!(code.len() <= THUNK);
    unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), (write + id * THUNK) as *mut u8, code.len()) };
    names.push(Box::leak(name.to_string().into_boxed_str()));
    base + id * THUNK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noted_thunks_reach_the_handler_with_the_arguments() {
        let h = handler(Class::Copy(3), 4).unwrap();
        let f: unsafe extern "system" fn(usize, usize, usize, usize) -> usize =
            unsafe { std::mem::transmute(noted("EOS_Test_CopyThing", h)) };
        let mut out = 0x5678usize;
        assert_eq!(unsafe { f(1, 2, 3, &mut out as *mut usize as usize) }, result::NOT_FOUND as usize);
        assert_eq!(out, 0, "the fourth argument came through");
        let id = crate::sys::lock(&NAMES).iter().position(|n| *n == "EOS_Test_CopyThing").unwrap();
        assert!(CALLED[id].load(Ordering::Relaxed));
        assert_eq!(noted("EOS_Test_CopyThing", h), noted("EOS_Test_CopyThing", h));
        assert_eq!(noted("EOS_Test_Release", h), h);
        // Six arguments: two on the stack past the shadow space.
        let z = handler(Class::Async, 6).unwrap();
        let g: unsafe extern "system" fn(usize, usize, usize, usize, usize, usize) -> usize =
            unsafe { std::mem::transmute(noted("EOS_Test_Six", z)) };
        assert_eq!(unsafe { g(0, 0, 0, 0, 5, 6) }, 0);
    }

    #[test]
    fn handlers_answer_by_class() {
        let copy: unsafe extern "system" fn(usize, usize, usize) -> usize =
            unsafe { std::mem::transmute(handler(Class::Copy(2), 3).unwrap()) };
        let mut out = 0x1234usize;
        assert_eq!(
            unsafe { copy(1, 2, &mut out as *mut usize as usize) },
            result::NOT_FOUND as usize
        );
        assert_eq!(out, 0);
        let notify: unsafe extern "system" fn(usize, usize, usize, usize) -> usize =
            unsafe { std::mem::transmute(handler(Class::AddNotify, 4).unwrap()) };
        let (a, b) = unsafe { (notify(0, 0, 0, 0), notify(0, 0, 0, 0)) };
        assert!(a != 0 && b != a);
        assert!(handler(Class::Pass, 1).is_none());
        // EOS_NotificationId is 64-bit: the whole value comes back.
        let notify64: unsafe extern "system" fn(usize, usize, usize, usize) -> u64 =
            unsafe { std::mem::transmute(handler(Class::AddNotify, 4).unwrap()) };
        let c = unsafe { notify64(0, 0, 0, 0) };
        assert!(c > b as u64 && c >> 32 == 0);
    }

    #[test]
    fn failed_callback_infos_put_client_data_where_each_struct_has_it() {
        // { EOS_EResult ResultCode; EOS_EpicAccountId LocalUserId; void* ClientData; ... }
        #[repr(C)]
        struct AfterPtr {
            result: i32,
            user: usize,
            client_data: usize,
        }
        // { EOS_EResult ResultCode; uint32_t SnapshotId; void* ClientData; }
        #[repr(C)]
        struct AfterU32 {
            result: i32,
            snapshot: u32,
            client_data: usize,
        }
        #[repr(C)]
        struct Plain {
            result: i32,
            client_data: usize,
        }
        let read = |class: u8| failed_info(0xC0DE, client_data_slot(class));
        let i = read(ASYNC_AFTER_PTR);
        let s = unsafe { &*(i.as_ptr() as *const AfterPtr) };
        assert_eq!((s.result, s.user, s.client_data), (result::NO_CONNECTION, 0, 0xC0DE));
        let i = read(ASYNC_AFTER_U32);
        let s = unsafe { &*(i.as_ptr() as *const AfterU32) };
        assert_eq!((s.result, s.snapshot, s.client_data), (result::NO_CONNECTION, 0, 0xC0DE));
        let i = read(ASYNC);
        let s = unsafe { &*(i.as_ptr() as *const Plain) };
        assert_eq!((s.result, s.client_data), (result::NO_CONNECTION, 0xC0DE));
    }
}
