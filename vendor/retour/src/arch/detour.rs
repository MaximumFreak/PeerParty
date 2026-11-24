use super::memory;
use crate::error::{Error, Result};
use crate::{alloc, arch, util};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

/// An architecture-independent implementation of a base detour.
///
/// This class is never instantiated by itself, it merely exposes an API
/// available through it's descendants.
pub struct Detour {
  #[allow(dead_code)]
  relay: Option<alloc::ExecutableMemory>,
  trampoline: alloc::ExecutableMemory,
  patcher: UnsafeCell<arch::Patcher>,
  enabled: AtomicBool,
}

impl Detour {
  pub unsafe fn new(target: *const (), detour: *const ()) -> Result<Self> {
    if target == detour {
      Err(Error::SameAddress)?;
    }

    // Lock this so OS operations are not performed in parallell
    let mut pool = memory::POOL.lock().unwrap_or_else(|e| e.into_inner());

    if !util::is_executable_address(target)? || !util::is_executable_address(detour)? {
      Err(Error::NotExecutable)?;
    }

    // Create a trampoline generator for the target function
    let margin = arch::meta::prolog_margin(target);
    let trampoline = arch::Trampoline::new(target, margin)?;

    // A relay is used in case a normal branch cannot reach the destination
    let relay = if let Some(emitter) = arch::meta::relay_builder(target, detour)? {
      Some(memory::allocate_pic(&mut pool, &emitter, target)?)
    } else {
      None
    };

    // If a relay is supplied, use it instead of the detour address
    let detour = relay
      .as_ref()
      .map(|code| code.as_ptr() as *const ())
      .unwrap_or(detour);

    Ok(Detour {
      patcher: UnsafeCell::new(arch::Patcher::new(
        target,
        detour,
        trampoline.prolog_size(),
      )?),
      trampoline: memory::allocate_pic(&mut pool, trampoline.emitter(), target)?,
      enabled: AtomicBool::default(),
      relay,
    })
  }

  /// Enables the detour.
  pub unsafe fn enable(&self) -> Result<()> {
    self.toggle(true)
  }

  /// Disables the detour.
  pub unsafe fn disable(&self) -> Result<()> {
    self.toggle(false)
  }

  /// Returns whether the detour is enabled or not.
  pub fn is_enabled(&self) -> bool {
    self.enabled.load(Ordering::SeqCst)
  }

  /// Returns a reference to the generated trampoline.
  pub fn trampoline(&self) -> &() {
    unsafe {
      (self.trampoline.as_ptr() as *const ())
        .as_ref()
        .expect("trampoline should not be null")
    }
  }

  /// Enables or disables the detour.
  unsafe fn toggle(&self, enabled: bool) -> Result<()> {
    let _guard = memory::POOL.lock().unwrap_or_else(|e| e.into_inner());

    if self.enabled.load(Ordering::SeqCst) == enabled {
      return Ok(());
    }

    // Runtime code is by default only read-execute.
    // On Windows this runs with the process's other threads paused, so it must
    // not allocate on the process heap (a paused thread may hold the heap lock):
    // `region::protect_with_handle` collects queried regions into a `Vec`.
    let _handle = {
      let area = (*self.patcher.get()).area();
      protect_patch_area(area)
    }?;

    // Copy either the detour or the original bytes of the function
    (*self.patcher.get()).toggle(enabled);
    flush_instruction_cache((*self.patcher.get()).area());
    self.enabled.store(enabled, Ordering::SeqCst);
    Ok(())
  }
}

#[cfg(windows)]
mod protect {
  use crate::error::Result;
  use std::ffi::c_void;
  use std::io;

  const PAGE_EXECUTE_READWRITE: u32 = 0x40;
  const PAGE_SIZE: usize = 0x1000;
  const CURRENT_PROCESS: isize = -1;

  #[link(name = "kernel32")]
  extern "system" {
    fn VirtualProtect(
      lpAddress: *const c_void,
      dwSize: usize,
      flNewProtect: u32,
      lpflOldProtect: *mut u32,
    ) -> i32;
    fn FlushInstructionCache(
      hProcess: isize,
      lpBaseAddress: *const c_void,
      dwSize: usize,
    ) -> i32;
  }

  /// Restores each touched page's protection on drop using only stack storage
  /// (no heap allocation while other threads may be suspended).
  pub(super) struct Guard {
    /// `(address, len, old_protect)` per touched 4 KiB page. A detour patch is
    /// a few bytes so it touches at most two pages; four leaves room to spare.
    pages: [(usize, usize, u32); 4],
    count: usize,
  }

  impl Guard {
    pub(super) unsafe fn new(area: &[u8]) -> Result<Self> {
      if area.is_empty() {
        return Err(region::Error::InvalidParameter("size").into());
      }
      let mut guard = Guard {
        pages: [(0, 0, 0); 4],
        count: 0,
      };
      let mut addr = area.as_ptr() as usize;
      let mut rem = area.len();
      while rem > 0 {
        if guard.count >= guard.pages.len() {
          return Err(region::Error::InvalidParameter("size").into());
        }
        let page_off = addr & (PAGE_SIZE - 1);
        let chunk = rem.min(PAGE_SIZE - page_off);
        let mut old = 0u32;
        if VirtualProtect(
          addr as *const c_void,
          chunk,
          PAGE_EXECUTE_READWRITE,
          &mut old,
        ) == 0
        {
          return Err(region::Error::SystemCall(io::Error::last_os_error()).into());
        }
        guard.pages[guard.count] = (addr, chunk, old);
        guard.count += 1;
        addr += chunk;
        rem -= chunk;
      }
      Ok(guard)
    }
  }

  impl Drop for Guard {
    fn drop(&mut self) {
      for i in (0..self.count).rev() {
        let (addr, len, old) = self.pages[i];
        let mut dummy = 0u32;
        unsafe {
          VirtualProtect(addr as *const c_void, len, old, &mut dummy);
        }
      }
    }
  }

  pub(super) unsafe fn flush_icache(area: &[u8]) {
    FlushInstructionCache(CURRENT_PROCESS, area.as_ptr() as *const c_void, area.len());
  }
}

#[cfg(windows)]
unsafe fn protect_patch_area(area: &[u8]) -> Result<protect::Guard> {
  protect::Guard::new(area)
}

#[cfg(not(windows))]
unsafe fn protect_patch_area(area: &[u8]) -> Result<region::ProtectGuard> {
  Ok(region::protect_with_handle(
    area.as_ptr(),
    area.len(),
    region::Protection::READ_WRITE_EXECUTE,
  )?)
}

#[cfg(windows)]
unsafe fn flush_instruction_cache(area: &[u8]) {
  protect::flush_icache(area);
}

#[cfg(not(windows))]
unsafe fn flush_instruction_cache(_area: &[u8]) {}

impl Drop for Detour {
  /// Disables the detour, if enabled.
  fn drop(&mut self) {
    // The disable runs in every build: inside `debug_assert!` it was compiled out of
    // release builds, leaving the patch pointing at a freed trampoline.
    let disabled = unsafe { self.disable() };
    debug_assert!(disabled.is_ok());
  }
}

impl fmt::Debug for Detour {
  /// Output whether the detour is enabled or not.
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(
      f,
      "Detour {{ enabled: {}, trampoline: {:?} }}",
      self.is_enabled(),
      self.trampoline()
    )
  }
}

unsafe impl Send for Detour {}
unsafe impl Sync for Detour {}
