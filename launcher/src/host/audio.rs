//! Per-copy audio mute for split-screen sessions.
//!
//! On Windows, walks WASAPI's audio session enumerator (`IAudioSessionManager2`) via
//! `ole32.dll` in System32 only (no new import-table dependencies) and mutes or unmutes
//! every audio session belonging to a copy's PID. Anything we muted is unmuted on Drop
//! so a process is never left silent if the host exits.

use std::collections::HashSet;

#[derive(Default)]
pub struct AudioMixer {
    muted: HashSet<u32>,
}

impl AudioMixer {
    pub fn new() -> AudioMixer {
        AudioMixer::default()
    }

    pub fn is_muted(&self, pid: u32) -> bool {
        pid != 0 && self.muted.contains(&pid)
    }

    /// Toggles mute for `pid` and returns the new muted state.
    pub fn toggle(&mut self, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        let mute = !self.muted.contains(&pid);
        self.set_muted(pid, mute);
        mute
    }

    /// Sets whether process `pid` is muted.
    pub fn set_muted(&mut self, pid: u32, mute: bool) {
        if pid == 0 {
            return;
        }
        if mute {
            self.muted.insert(pid);
        } else {
            self.muted.remove(&pid);
        }
        #[cfg(windows)]
        win::apply_pid_mute(pid, mute);
    }

    /// Re-applies active mutes (a game that starts audio lazily after launch opens its
    /// WASAPI session after the user may already have muted it).
    pub fn refresh(&self) {
        #[cfg(windows)]
        for &pid in &self.muted {
            win::apply_pid_mute(pid, true);
        }
    }

    /// Updates tracked PID when a starter process hands off to the real game process.
    pub fn follow_pid(&mut self, old: u32, new: u32) {
        if old != new && self.muted.remove(&old) && new != 0 {
            self.muted.insert(new);
            #[cfg(windows)]
            win::apply_pid_mute(new, true);
        }
    }
}

impl Drop for AudioMixer {
    fn drop(&mut self) {
        #[cfg(windows)]
        for &pid in &self.muted {
            win::apply_pid_mute(pid, false);
        }
    }
}

#[cfg(windows)]
mod win {
    use std::sync::OnceLock;

    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

    macro_rules! vcall {
        ($obj:expr, $slot:expr, fn($($t:ty),*) -> $r:ty $(, $arg:expr)* $(,)?) => {{
            let obj = $obj as usize;
            let vt = unsafe { *(obj as *const *const usize) };
            let f: unsafe extern "system" fn(usize $(, $t)*) -> $r = unsafe { std::mem::transmute(*vt.add($slot)) };
            unsafe { f(obj $(, $arg)*) }
        }};
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid(u32, u16, u16, [u8; 8]);

    const CLSID_MM_DEVICE_ENUMERATOR: Guid = Guid(0xBCDE0395, 0xE52F, 0x467C, [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E]);
    const IID_IMM_DEVICE_ENUMERATOR: Guid = Guid(0xA95664D2, 0x9614, 0x4F35, [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6]);
    const IID_IAUDIO_SESSION_MANAGER2: Guid = Guid(0x77AA99A0, 0x1BD6, 0x484F, [0x8B, 0xC7, 0x2C, 0x65, 0x4C, 0x9A, 0x9B, 0x6F]);
    const IID_IAUDIO_SESSION_CONTROL2: Guid = Guid(0xBFB7FF88, 0x7239, 0x4FC9, [0x8F, 0xA2, 0x07, 0xC9, 0x50, 0xBE, 0x9C, 0x6D]);
    const IID_ISIMPLE_AUDIO_VOLUME: Guid = Guid(0x87CE5498, 0x68D6, 0x44E5, [0x92, 0x15, 0x6D, 0xA4, 0x7E, 0xF8, 0x83, 0xD8]);

    struct Ole32 {
        co_init: unsafe extern "system" fn(usize, u32) -> i32,
        co_uninit: unsafe extern "system" fn(),
        co_create: unsafe extern "system" fn(*const Guid, usize, u32, *const Guid, *mut usize) -> i32,
    }

    fn ole32() -> Option<&'static Ole32> {
        static OLE: OnceLock<Option<Ole32>> = OnceLock::new();
        OLE.get_or_init(|| {
            let name: Vec<u16> = "ole32.dll".encode_utf16().chain(Some(0)).collect();
            let m = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
            if m.is_null() {
                return None;
            }
            let get = |f: &std::ffi::CStr| unsafe { GetProcAddress(m, f.as_ptr().cast()) };
            let (init, uninit, create) = (get(c"CoInitializeEx")?, get(c"CoUninitialize")?, get(c"CoCreateInstance")?);
            unsafe {
                Some(Ole32 {
                    co_init: std::mem::transmute(init),
                    co_uninit: std::mem::transmute(uninit),
                    co_create: std::mem::transmute(create),
                })
            }
        })
        .as_ref()
    }

    fn release(obj: usize) {
        if obj != 0 {
            vcall!(obj, 2, fn() -> u32);
        }
    }

    fn query(obj: usize, iid: &Guid) -> usize {
        let mut out = 0usize;
        if obj == 0 || vcall!(obj, 0, fn(*const Guid, *mut usize) -> i32, iid, &mut out) < 0 { 0 } else { out }
    }

    /// Sets mute on every active WASAPI render session belonging to `pid`.
    pub(super) fn apply_pid_mute(pid: u32, mute: bool) {
        let Some(ole) = ole32() else { return };
        // COINIT_MULTITHREADED = 0. RPC_E_CHANGED_MODE means COM is already initialized
        // in STA on this thread, which is fine; only uninitialize if we succeeded here.
        let init_hr = unsafe { (ole.co_init)(0, 0) };
        let did_init = init_hr == 0 || init_hr == 1;
        // CLSCTX_ALL = 0x17
        let mut enumerator = 0usize;
        if unsafe { (ole.co_create)(&CLSID_MM_DEVICE_ENUMERATOR, 0, 0x17, &IID_IMM_DEVICE_ENUMERATOR, &mut enumerator) } >= 0 && enumerator != 0 {
            let mut device = 0usize;
            // IMMDeviceEnumerator::GetDefaultAudioEndpoint (slot 4): eRender=0, eConsole=0.
            if vcall!(enumerator, 4, fn(u32, u32, *mut usize) -> i32, 0, 0, &mut device) >= 0 && device != 0 {
                let mut mgr = 0usize;
                // IMMDevice::Activate (slot 3).
                if vcall!(device, 3, fn(*const Guid, u32, usize, *mut usize) -> i32, &IID_IAUDIO_SESSION_MANAGER2, 0x17, 0, &mut mgr) >= 0 && mgr != 0 {
                    let mut list = 0usize;
                    // IAudioSessionManager2::GetSessionEnumerator (slot 5).
                    if vcall!(mgr, 5, fn(*mut usize) -> i32, &mut list) >= 0 && list != 0 {
                        let mut count = 0i32;
                        // IAudioSessionEnumerator::GetCount (slot 3).
                        if vcall!(list, 3, fn(*mut i32) -> i32, &mut count) >= 0 {
                            for i in 0..count.max(0) {
                                let mut ctl = 0usize;
                                // IAudioSessionEnumerator::GetSession (slot 4).
                                if vcall!(list, 4, fn(i32, *mut usize) -> i32, i, &mut ctl) < 0 || ctl == 0 {
                                    continue;
                                }
                                let ctl2 = query(ctl, &IID_IAUDIO_SESSION_CONTROL2);
                                if ctl2 != 0 {
                                    let mut spid = 0u32;
                                    // IAudioSessionControl2::GetProcessId (slot 14).
                                    if vcall!(ctl2, 14, fn(*mut u32) -> i32, &mut spid) >= 0 && spid == pid {
                                        let vol = query(ctl, &IID_ISIMPLE_AUDIO_VOLUME);
                                        if vol != 0 {
                                            // ISimpleAudioVolume::SetMute (slot 5).
                                            vcall!(vol, 5, fn(i32, *const Guid) -> i32, i32::from(mute), std::ptr::null());
                                            release(vol);
                                        }
                                    }
                                    release(ctl2);
                                }
                                release(ctl);
                            }
                        }
                        release(list);
                    }
                    release(mgr);
                }
                release(device);
            }
            release(enumerator);
        }
        if did_init {
            unsafe { (ole.co_uninit)() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixer_tracks_mute_and_pid_handoff() {
        let mut m = AudioMixer::new();
        assert!(!m.is_muted(0));
        assert!(!m.toggle(0));
        // Use a non-existent dummy PID so toggling touches no real system audio session.
        let dummy = 0x7FFF_FFFE;
        assert!(m.toggle(dummy));
        assert!(m.is_muted(dummy));
        m.follow_pid(dummy, dummy - 1);
        assert!(!m.is_muted(dummy));
        assert!(m.is_muted(dummy - 1));
        assert!(!m.toggle(dummy - 1));
        assert!(!m.is_muted(dummy - 1));
    }
}
