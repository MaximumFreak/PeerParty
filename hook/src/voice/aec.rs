//! The microphone through Windows' Voice Capture DSP (CLSID_CWMAudioAEC, mfwmaaec.dll):
//! acoustic echo cancellation, noise suppression and automatic gain, done by Windows.
//! In system mode the DSP opens the microphone itself and takes the speakers' own
//! output (a loopback of the render device) as the echo reference, so it cancels
//! everything the speakers play: the other players' voices and the game's sound.
//!
//! The devices are the ones voice uses (endpoints.rs: the game's choice, else the
//! communications defaults), found by their place in the active endpoint lists, which
//! is how the DSP numbers them. Output is 16 kHz mono 16-bit, PeerParty's device format.
//! Silence plays on those speakers while it runs (see `Silence`).
//!
//! References: "Voice Capture DSP" and its properties (learn.microsoft.com/windows/
//! win32/medfound/voicecapturedmo, .../mfpkey-wmaaecma-...), the AECMicArray sample of
//! the Windows SDK samples (the system-mode loop: ProcessOutput into an IMediaBuffer of
//! our own), wmcodecdsp.h for the property ids.

use std::ffi::c_void;

use super::DEVICE_RATE;
use crate::log;

use super::com::{self, CAPTURE, Guid, Hresult, IID_UNKNOWN, PropVariant, RENDER, release, vcall};

const CLSID_AEC: Guid = Guid(0x745057C7, 0xF353, 0x4F2D, [0xA7, 0xEE, 0x58, 0x43, 0x44, 0x77, 0x73, 0x0E]);
const IID_MEDIA_OBJECT: Guid = Guid(0xD8AD0F58, 0x5494, 0x4102, [0x97, 0xC5, 0xEC, 0x79, 0x8E, 0x59, 0xBC, 0xF4]);
const IID_PROPERTY_STORE: Guid = Guid(0x886D8EEB, 0x8CF2, 0x4446, [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99]);
const IID_MEDIA_BUFFER: Guid = Guid(0x59EFF8B9, 0x938C, 0x4A26, [0x82, 0xF2, 0x95, 0xCB, 0x84, 0xCD, 0xC8, 0x37]);
const IID_AUDIO_CLIENT: Guid = Guid(0x1CB9AD4C, 0xDBFA, 0x4C32, [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2]);
const IID_RENDER_CLIENT: Guid = Guid(0xF294ACFC, 0x3146, 0x4483, [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2]);
const MEDIATYPE_AUDIO: Guid = Guid(0x73647561, 0, 0x10, [0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
const SUBTYPE_PCM: Guid = Guid(1, 0, 0x10, [0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
const FORMAT_WAVEFORMATEX: Guid = Guid(0x05589F81, 0xC356, 0x11CE, [0xBF, 0x01, 0x00, 0xAA, 0x00, 0x55, 0x59, 0x5A]);
/// MFPKEY_WMAAECMA_*'s property set; the ids are PID_FIRST_USABLE (2) + n.
const AEC_KEYS: Guid = Guid(0x6F52C567, 0x0360, 0x4BD2, [0x96, 0x17, 0xCC, 0xBF, 0x14, 0x21, 0xC9, 0x39]);
const SYSTEM_MODE: u32 = 2;
const DEVICE_INDEXES: u32 = 4;
const FEATURE_MODE: u32 = 5;
const FEATR_NS: u32 = 8;
const FEATR_AGC: u32 = 9;

fn set_prop(store: usize, pid: u32, vt: u16, value: i32) -> bool {
    #[repr(C)]
    struct Key(Guid, u32);
    let key = Key(AEC_KEYS, pid);
    let mut v = PropVariant { vt, reserved: [0; 3], value: [0; 2] };
    v.value[0] = value as u32 as u64;
    vcall!(store, 6, fn(*const Key, *const PropVariant) -> Hresult, &key, &v) >= 0
}

/// The place of endpoint `id` among the active endpoints of `flow`.
fn index_of(enumerator: usize, flow: u32, id: &str) -> Option<u32> {
    let mut found = None;
    for (i, dev) in com::active_devices(enumerator, flow).into_iter().enumerate() {
        if found.is_none() && com::device_id(dev).as_deref() == Some(id) {
            found = Some(i as u32);
        }
        release(dev);
    }
    found
}

/// An IMediaBuffer of our own that the DSP writes into.
#[repr(C)]
struct Buffer {
    vtbl: *const BufferVtbl,
    refs: u32,
    len: u32,
    data: Vec<u8>,
}

#[repr(C)]
struct BufferVtbl {
    qi: unsafe extern "system" fn(*mut Buffer, *const Guid, *mut usize) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut Buffer) -> u32,
    release: unsafe extern "system" fn(*mut Buffer) -> u32,
    set_length: unsafe extern "system" fn(*mut Buffer, u32) -> Hresult,
    max_length: unsafe extern "system" fn(*mut Buffer, *mut u32) -> Hresult,
    get: unsafe extern "system" fn(*mut Buffer, *mut *mut u8, *mut u32) -> Hresult,
}

static BUFFER_VTBL: BufferVtbl = BufferVtbl {
    qi: buf_qi,
    add_ref: buf_add_ref,
    release: buf_release,
    set_length: buf_set_length,
    max_length: buf_max_length,
    get: buf_get,
};

unsafe extern "system" fn buf_qi(this: *mut Buffer, iid: *const Guid, out: *mut usize) -> Hresult {
    let iid = unsafe { &*iid };
    if iid.same(&IID_MEDIA_BUFFER) || iid.same(&IID_UNKNOWN) {
        unsafe {
            (*this).refs += 1;
            *out = this as usize;
        }
        0
    } else {
        unsafe { *out = 0 };
        0x8000_4002_u32 as i32 // E_NOINTERFACE
    }
}

unsafe extern "system" fn buf_add_ref(this: *mut Buffer) -> u32 {
    unsafe {
        (*this).refs += 1;
        (*this).refs
    }
}

/// The buffer lives in `Capture` (a Box), so the count only tracks the DSP's references.
unsafe extern "system" fn buf_release(this: *mut Buffer) -> u32 {
    unsafe {
        (*this).refs = (*this).refs.saturating_sub(1);
        (*this).refs
    }
}

unsafe extern "system" fn buf_set_length(this: *mut Buffer, len: u32) -> Hresult {
    let b = unsafe { &mut *this };
    if len as usize > b.data.len() {
        return 0x8007_0057_u32 as i32; // E_INVALIDARG
    }
    b.len = len;
    0
}

unsafe extern "system" fn buf_max_length(this: *mut Buffer, out: *mut u32) -> Hresult {
    unsafe { *out = (*this).data.len() as u32 };
    0
}

unsafe extern "system" fn buf_get(this: *mut Buffer, data: *mut *mut u8, len: *mut u32) -> Hresult {
    let b = unsafe { &mut *this };
    unsafe {
        if !data.is_null() {
            *data = b.data.as_mut_ptr();
        }
        if !len.is_null() {
            *len = b.len;
        }
    }
    0
}

#[repr(C)]
struct WaveFormatEx {
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    cb_size: u16,
}

#[repr(C)]
struct MediaType {
    major: Guid,
    sub: Guid,
    fixed_size: i32,
    temporal: i32,
    sample_size: u32,
    format_type: Guid,
    unk: usize,
    format_len: u32,
    format: *const WaveFormatEx,
}

#[repr(C)]
struct OutputBuffer {
    buffer: *mut Buffer,
    status: u32,
    timestamp: i64,
    length: i64,
}

/// Silence played on the speakers the DSP listens to. Its echo reference is a loopback
/// of that device, and a loopback hands out nothing while nothing plays: without this
/// the DSP waits, and the microphone sends nothing, until some sound plays.
struct Silence {
    client: usize,
    render: usize,
    frames: u32,
}

impl Silence {
    fn start(device: usize) -> Option<Silence> {
        let mut client = 0usize;
        if vcall!(device, 3, fn(*const Guid, u32, *const c_void, *mut usize) -> Hresult, &IID_AUDIO_CLIENT, 0x17, std::ptr::null(), &mut client) < 0 {
            return None;
        }
        let mut format: *mut c_void = std::ptr::null_mut();
        let ok = vcall!(client, 8, fn(*mut *mut c_void) -> Hresult, &mut format) >= 0
            && vcall!(client, 3, fn(u32, u32, i64, i64, *const c_void, *const Guid) -> Hresult, 0, 0, 1_000_000, 0, format, std::ptr::null()) >= 0;
        if !format.is_null() {
            unsafe { com::CoTaskMemFree(format) };
        }
        let mut render = 0usize;
        let mut frames = 0u32;
        if !ok
            || vcall!(client, 4, fn(*mut u32) -> Hresult, &mut frames) < 0
            || vcall!(client, 14, fn(*const Guid, *mut usize) -> Hresult, &IID_RENDER_CLIENT, &mut render) < 0
        {
            release(client);
            return None;
        }
        let s = Silence { client, render, frames };
        s.top_up();
        vcall!(client, 10, fn() -> Hresult);
        Some(s)
    }

    /// Fills what the device has played since with more silence.
    fn top_up(&self) {
        let mut padding = 0u32;
        if vcall!(self.client, 6, fn(*mut u32) -> Hresult, &mut padding) < 0 {
            return;
        }
        let free = self.frames.saturating_sub(padding);
        let mut data: *mut u8 = std::ptr::null_mut();
        if free > 0 && vcall!(self.render, 3, fn(u32, *mut *mut u8) -> Hresult, free, &mut data) >= 0 {
            vcall!(self.render, 4, fn(u32, u32) -> Hresult, free, 2); // AUDCLNT_BUFFERFLAGS_SILENT
        }
    }
}

impl Drop for Silence {
    fn drop(&mut self) {
        vcall!(self.client, 11, fn() -> Hresult);
        release(self.render);
        release(self.client);
    }
}

/// The DSP, running: read it every few milliseconds.
pub struct Capture {
    dmo: usize,
    buf: Box<Buffer>,
    silence: Option<Silence>,
}

unsafe impl Send for Capture {}

impl Capture {
    /// Starts the DSP on microphone `mic` and speakers `speakers` (endpoint ids). None
    /// when Windows doesn't have it, or there's no such microphone.
    pub fn open(mic: &str, speakers: Option<&str>) -> Option<Capture> {
        let Some(enumerator) = com::enumerator() else {
            log!("voice: no audio endpoints; echo cancellation off");
            return None;
        };
        let silence = speakers.and_then(|id| com::device_by_id(enumerator, id)).and_then(|d| {
            let s = Silence::start(d);
            release(d);
            s
        });
        let speakers = speakers.and_then(|id| index_of(enumerator, RENDER, id));
        let mic = index_of(enumerator, CAPTURE, mic);
        release(enumerator);
        let mic = mic?;
        let mut dmo = 0usize;
        let hr = unsafe { com::CoCreateInstance(&CLSID_AEC, std::ptr::null(), 1, &IID_MEDIA_OBJECT, &mut dmo) };
        if hr < 0 {
            log!("voice: Windows' Voice Capture DSP isn't available ({hr:#x}); echo cancellation off");
            return None;
        }
        let mut store = 0usize;
        let ok = vcall!(dmo, 0, fn(*const Guid, *mut usize) -> Hresult, &IID_PROPERTY_STORE, &mut store) >= 0
            && set_prop(store, SYSTEM_MODE, 3, 0) // VT_I4 SINGLE_CHANNEL_AEC
            && set_prop(store, DEVICE_INDEXES, 3, ((speakers.unwrap_or(0) as i32) << 16) | (mic as i32 & 0xFFFF))
            && set_prop(store, FEATURE_MODE, 11, -1) // VT_BOOL VARIANT_TRUE
            && set_prop(store, FEATR_NS, 3, 1)
            && set_prop(store, FEATR_AGC, 11, -1);
        release(store);
        if !ok {
            log!("voice: Voice Capture DSP refused its settings; echo cancellation off");
            release(dmo);
            return None;
        }
        let wf = WaveFormatEx {
            format_tag: 1,
            channels: 1,
            samples_per_sec: DEVICE_RATE,
            avg_bytes_per_sec: DEVICE_RATE * 2,
            block_align: 2,
            bits_per_sample: 16,
            cb_size: 0,
        };
        let mt = MediaType {
            major: MEDIATYPE_AUDIO,
            sub: SUBTYPE_PCM,
            fixed_size: 1,
            temporal: 0,
            sample_size: 2,
            format_type: FORMAT_WAVEFORMATEX,
            unk: 0,
            format_len: size_of::<WaveFormatEx>() as u32,
            format: &wf,
        };
        let hr = vcall!(dmo, 9, fn(u32, *const MediaType, u32) -> Hresult, 0, &mt, 0);
        let hr2 = if hr >= 0 { vcall!(dmo, 18, fn() -> Hresult) } else { hr };
        if hr2 < 0 {
            log!("voice: Voice Capture DSP didn't start ({hr2:#x}); echo cancellation off");
            release(dmo);
            return None;
        }
        // A second of audio; the DSP hands out 10 ms at a time.
        let buf = Box::new(Buffer { vtbl: &BUFFER_VTBL, refs: 1, len: 0, data: vec![0; DEVICE_RATE as usize * 2] });
        Some(Capture { dmo, buf, silence })
    }

    /// The samples the DSP has ready (cleaned microphone audio), appended to `out`.
    /// False when the DSP failed (the microphone went away).
    pub fn read(&mut self, out: &mut Vec<i16>) -> bool {
        if let Some(s) = &self.silence {
            s.top_up();
        }
        loop {
            self.buf.len = 0;
            let mut ob = OutputBuffer { buffer: &mut *self.buf, status: 0, timestamp: 0, length: 0 };
            let mut status = 0u32;
            let hr = vcall!(self.dmo, 22, fn(u32, u32, *mut OutputBuffer, *mut u32) -> Hresult, 0, 1, &mut ob, &mut status);
            if hr < 0 {
                log!("voice: Voice Capture DSP stopped ({hr:#x})");
                return false;
            }
            let n = self.buf.len as usize / 2;
            out.extend((0..n).map(|i| i16::from_le_bytes([self.buf.data[i * 2], self.buf.data[i * 2 + 1]])));
            // DMO_OUTPUT_DATA_BUFFERF_INCOMPLETE: more is ready.
            if ob.status & 0x0100_0000 == 0 || n == 0 {
                return true;
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        vcall!(self.dmo, 19, fn() -> Hresult); // FreeStreamingResources
        release(self.dmo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens the DSP on this PC's devices and reads a moment of audio (skipped without
    /// a microphone).
    #[test]
    fn the_dsp_hands_out_16k_audio() {
        let snap = super::super::endpoints::snapshot();
        let speakers = snap.output.communications.clone();
        let Some(mut c) = snap.input.communications.as_deref().and_then(|mic| Capture::open(mic, speakers.as_deref())) else {
            eprintln!("no microphone or no DSP: skipped");
            return;
        };
        let mut got = Vec::new();
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(1500) {
            assert!(c.read(&mut got));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        eprintln!("DSP: {} samples in {:?}", got.len(), start.elapsed());
        // Roughly real time once it's running (it may take a moment to start).
        assert!(got.len() > DEVICE_RATE as usize / 10, "{} samples", got.len());
    }
}
