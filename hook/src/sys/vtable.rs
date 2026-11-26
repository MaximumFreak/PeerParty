//! C++ stand-in objects, as Steam's and Galaxy's front ends hand them to a game: a vtable
//! of PeerParty's methods for the slots it answers and, for every other slot, a stub
//! that swaps in the real object and jumps to the real method, whatever its signature.
//!
//! A stand-in starts `#[repr(C)] { vtable: *const usize, real: usize, .. }`: the stubs
//! read the real object at `this + pointer size`.
//!
//! On x86 C++ methods are `thiscall` (`this` in ecx, callee pops); on x64 they're the
//! ordinary convention (`vmethod!` picks).

use std::sync::OnceLock;

/// Defines a C++ virtual method for a stand-in vtable.
macro_rules! vmethod {
    // A `bool` return only defines al, in Rust as in MSVC, and the rest of eax/rax is
    // whatever the method left there. A game may still test the whole register (its
    // compiler widened the result, or it declared the method `BOOL`), and then reads
    // false as true; so a `bool` method returns 0 or 1 in the full register.
    ($(#[$m:meta])* fn $name:ident($($a:ident: $t:ty),* $(,)?) -> bool $body:block) => {
        $(#[$m])*
        #[cfg(target_arch = "x86")]
        unsafe extern "thiscall" fn $name($($a: $t),*) -> usize {
            unsafe fn inner($($a: $t),*) -> bool $body
            unsafe { inner($($a),*) as usize }
        }
        $(#[$m])*
        #[cfg(not(target_arch = "x86"))]
        unsafe extern "C" fn $name($($a: $t),*) -> usize {
            unsafe fn inner($($a: $t),*) -> bool $body
            unsafe { inner($($a),*) as usize }
        }
    };
    ($(#[$m:meta])* fn $name:ident($($a:ident: $t:ty),* $(,)?) $(-> $r:ty)? $body:block) => {
        $(#[$m])*
        #[cfg(target_arch = "x86")]
        unsafe extern "thiscall" fn $name($($a: $t),*) $(-> $r)? $body
        $(#[$m])*
        #[cfg(not(target_arch = "x86"))]
        unsafe extern "C" fn $name($($a: $t),*) $(-> $r)? $body
    };
}
pub(crate) use vmethod;

/// A layout's method names in vtable order. The generated tables (steam/vt.rs,
/// galaxy/vt.rs) keep each name once in a pool and a layout as indices into it: a table
/// of `&str`s costs 16 bytes a slot on x64 and a relocation each on x86.
#[derive(Clone, Copy)]
pub struct Names {
    pool: &'static [&'static str],
    ix: &'static [u16],
}

impl Names {
    pub const fn new(pool: &'static [&'static str], ix: &'static [u16]) -> Self {
        Names { pool, ix }
    }

    pub fn len(self) -> usize {
        self.ix.len()
    }

    pub fn get(self, i: usize) -> Option<&'static &'static str> {
        self.pool.get(*self.ix.get(i)? as usize)
    }

    pub fn iter(self) -> NamesIter {
        self.into_iter()
    }

    pub fn contains(self, name: &str) -> bool {
        self.iter().any(|n| *n == name)
    }
}

pub type NamesIter = std::iter::Map<std::iter::Zip<std::slice::Iter<'static, u16>, std::iter::Repeat<&'static [&'static str]>>, fn((&'static u16, &'static [&'static str])) -> &'static &'static str>;

impl IntoIterator for Names {
    type Item = &'static &'static str;
    type IntoIter = NamesIter;

    fn into_iter(self) -> NamesIter {
        self.ix.iter().zip(std::iter::repeat(self.pool)).map(|(&i, pool)| &pool[i as usize])
    }
}

impl PartialEq for Names {
    fn eq(&self, other: &Names) -> bool {
        self.iter().eq(other.iter())
    }
}

impl std::fmt::Debug for Names {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The most slots a stand-in's vtable forwards.
pub const MAX_SLOTS: usize = 128;
const STUB: usize = 16;

/// A block of `n` executable stubs of `each` bytes, `code(i)` in the ith (the rest of
/// each is int3). 0 when the memory can't be had.
///
/// # Safety
/// Each `code(i)` is no longer than `each`, and is code that's safe to run.
pub unsafe fn exec_block(n: usize, each: usize, code: impl Fn(usize) -> Vec<u8>) -> usize {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualProtect};
    let size = n * each;
    let p = unsafe { VirtualAlloc(std::ptr::null(), size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) } as *mut u8;
    if p.is_null() {
        return 0;
    }
    for i in 0..n {
        let c = code(i);
        debug_assert!(c.len() <= each);
        unsafe {
            std::ptr::write_bytes(p.add(i * each), 0xCC, each);
            std::ptr::copy_nonoverlapping(c.as_ptr(), p.add(i * each), c.len().min(each));
        }
    }
    let mut old = 0;
    unsafe { VirtualProtect(p.cast(), size, PAGE_EXECUTE_READ, &mut old) };
    p as usize
}

/// Every slot's forwarding stub, in one executable block.
fn stubs() -> Option<usize> {
    static BASE: OnceLock<usize> = OnceLock::new();
    let base = *BASE.get_or_init(|| unsafe {
        exec_block(MAX_SLOTS, STUB, |i| {
            let disp = ((i * size_of::<usize>()) as u32).to_le_bytes();
            #[cfg(target_arch = "x86_64")]
            // mov rcx,[rcx+8]; mov rax,[rcx]; jmp [rax+disp32]
            let code: Vec<u8> = [&[0x48, 0x8B, 0x49, 0x08, 0x48, 0x8B, 0x01, 0xFF, 0xA0][..], &disp].concat();
            #[cfg(target_arch = "x86")]
            // mov ecx,[ecx+4]; mov eax,[ecx]; jmp [eax+disp32]
            let code: Vec<u8> = [&[0x8B, 0x49, 0x04, 0x8B, 0x01, 0xFF, 0xA0][..], &disp].concat();
            code
        })
    });
    (base != 0).then_some(base)
}

/// The stub that forwards vtable slot `i` to the object at `this + pointer size`.
#[cfg(test)]
pub fn forward_stub(i: usize) -> Option<usize> {
    (i < MAX_SLOTS).then(|| stubs().map(|b| b + i * STUB)).flatten()
}

/// A stand-in's vtable of `slots` entries, kept for the process's life: slot `i` is
/// `ours(i)` where that's Some, else the stub forwarding it to the real object. Also how
/// many slots are PeerParty's. None when it can't forward them all.
pub fn build(slots: usize, mut ours: impl FnMut(usize) -> Option<usize>) -> Option<(*const usize, usize)> {
    if slots > MAX_SLOTS {
        return None;
    }
    let base = stubs()?;
    let mut count = 0;
    let table: Vec<usize> = (0..slots)
        .map(|i| match ours(i) {
            Some(f) => {
                count += 1;
                f
            }
            None => base + i * STUB,
        })
        .collect();
    Some((Box::leak(table.into_boxed_slice()).as_ptr(), count))
}

#[cfg(test)]
mod tests {
    use super::*;

    vmethod! { fn twenty(this: usize, a: u32) -> usize { this.wrapping_add(20 + a as usize) } }
    vmethod! { fn ours(_this: usize, a: u32) -> usize { 1000 + a as usize } }
    vmethod! {
        fn is_nonzero(_this: usize, a: u32) -> bool {
            if a == 0 {
                return false;
            }
            true
        }
    }

    #[repr(C)]
    struct Proxy {
        vtable: *const usize,
        real: usize,
    }

    #[test]
    fn stand_ins_forward_what_they_dont_answer() {
        // A "real" object whose slot 1 is `twenty`.
        let real_vt = [0usize, twenty as *const () as usize];
        let real = Box::leak(Box::new(real_vt.as_ptr())) as *const *const usize as usize;
        let (vt, count) = build(2, |i| (i == 0).then_some(ours as *const () as usize)).unwrap();
        assert_eq!(count, 1);
        let proxy = Proxy { vtable: vt, real };
        let this = &proxy as *const Proxy as usize;
        #[cfg(not(target_arch = "x86"))]
        type M = unsafe extern "C" fn(usize, u32) -> usize;
        #[cfg(target_arch = "x86")]
        type M = unsafe extern "thiscall" fn(usize, u32) -> usize;
        let slot = |i: usize| -> M { unsafe { std::mem::transmute(*vt.add(i)) } };
        assert_eq!(unsafe { slot(0)(this, 5) }, 1005, "ours");
        assert_eq!(unsafe { slot(1)(this, 5) }, real + 25, "the real method, on the real object");
        assert!(build(MAX_SLOTS + 1, |_| None).is_none());
        assert!(forward_stub(MAX_SLOTS).is_none() && forward_stub(0).is_some());
    }

    #[test]
    fn bool_vmethods_widen_to_full_register() {
        assert_eq!(unsafe { is_nonzero(0, 0) }, 0usize);
        assert_eq!(unsafe { is_nonzero(0, 7) }, 1usize);
    }
}
