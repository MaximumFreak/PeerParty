//! Frames that reach PeerParty as pixels in memory, for the session's window: a plain
//! Direct3D 9 game's (its device can't share a texture), and every game's when the
//! session asks for pixels (`capture::pixels`: a game under Wine, whose Windows shared
//! textures a native Linux window can't open).
//!
//! `CpuFrames::deliver` takes one frame's rows and puts them where the session reads
//! them:
//!
//! - Pixels asked for: a file of this hook's own next to the session's ring block
//!   (`PixelFile`), `frame_ring::SLOTS` slots of the frame's rows, reported in a
//!   `surface` with its path. A slot is written only when the ring says it's free
//!   (`RingHeader::pick`), and published once written; there are no fences.
//! - Otherwise: uploaded into the shared textures of a `D3d11Ring` on a Direct3D 11
//!   device of PeerParty's own, as a Vulkan or OpenGL game's frames are copied there.

use std::sync::Mutex;

use peerparty_protocol::frame_ring::{bytes_per_pixel, pixels_size, slot_bytes};
use peerparty_protocol::ipc::SurfaceMsg;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

use super::capture::{self, D3d11Ring, Kind, private_device, typeless};
use super::com::vcall;
use crate::log;

/// One generation's pixel file, mapped for writing. The file goes when its last handle
/// does (the session maps it itself, and keeps its own view).
pub struct PixelFile {
    pub generation: u32,
    pub kind: Kind,
    pub stride: u32,
    slot: usize,
    /// As the session names it (a Linux path under Wine).
    pub path: String,
    file: HANDLE,
    map: HANDLE,
    view: *mut u8,
}

unsafe impl Send for PixelFile {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

impl PixelFile {
    /// A file in `dir` (the session's directory, as it names it) for frames of `kind`.
    pub fn create(dir: &str, kind: Kind) -> Option<PixelFile> {
        use windows_sys::Win32::Storage::FileSystem::{
            CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_DELETE_ON_CLOSE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE};
        let stride = kind.width.checked_mul(bytes_per_pixel(kind.format)?)?;
        let (Some(size), Some(slot)) = (pixels_size(stride, kind.height), slot_bytes(stride, kind.height)) else {
            log!("video: no pixel file for {}x{} frames: too big for this process to map", kind.width, kind.height);
            return None;
        };
        let generation = capture::next_generation();
        let sep = if dir.contains('\\') && !dir.contains('/') { '\\' } else { '/' };
        // A random part too: copies in Wine prefixes of their own can share a process id.
        let token: String = crate::util::crypto::random_bytes(4).iter().map(|b| format!("{b:02x}")).collect();
        let path = format!("{}{sep}pixels-{}-{token}-{generation}", dir.trim_end_matches(['/', '\\']), std::process::id());
        let local = crate::sys::wine::windows_path(&path).into_owned();
        let file = unsafe {
            CreateFileW(
                wide(&local).as_ptr(),
                0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE,
                std::ptr::null_mut(),
            )
        };
        if file == INVALID_HANDLE_VALUE || file.is_null() {
            log!("video: the frames' pixel file {local} couldn't be made");
            return None;
        }
        let map = unsafe { CreateFileMappingW(file, std::ptr::null(), PAGE_READWRITE, (size as u64 >> 32) as u32, size as u32, std::ptr::null()) };
        if map.is_null() {
            unsafe { CloseHandle(file) };
            log!("video: the frames' pixel file couldn't be sized to {size} bytes");
            return None;
        }
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size) }.Value as *mut u8;
        if view.is_null() {
            unsafe {
                CloseHandle(map);
                CloseHandle(file);
            }
            return None;
        }
        Some(PixelFile { generation, kind, stride, slot, path, file, map, view })
    }

    /// Lets go of the mapping (a whole generation of frames, hundreds of megabytes of
    /// address space at 4K), keeping only the file handle so the file stays for a session
    /// that maps it late.
    fn into_file(mut self) -> RetiredFile {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            CloseHandle(self.map);
        }
        self.view = std::ptr::null_mut();
        self.map = std::ptr::null_mut();
        let file = std::mem::replace(&mut self.file, std::ptr::null_mut());
        RetiredFile(file)
    }

    /// Copies a frame's rows (`pitch` bytes apart at `bits`) into `slot`.
    ///
    /// # Safety
    /// `bits` must hold `kind.height` rows of at least `stride` bytes, `pitch` apart.
    pub unsafe fn write(&self, slot: usize, bits: *const u8, pitch: usize) {
        let row = self.stride as usize;
        let base = unsafe { self.view.add(slot * self.slot) };
        if pitch == row {
            unsafe { std::ptr::copy_nonoverlapping(bits, base, row * self.kind.height as usize) };
            return;
        }
        for y in 0..self.kind.height as usize {
            unsafe { std::ptr::copy_nonoverlapping(bits.add(y * pitch), base.add(y * row), row) };
        }
    }

    /// Tells the session about it.
    pub fn report(&self, api: &str) {
        let Kind { width, height, format, color_space } = self.kind;
        log!("video: frames for the session's window as {width}x{height} pixels (format {format}, colour space {color_space}, {api}, set {})", self.generation);
        capture::send_surface(SurfaceMsg {
            pid: std::process::id(),
            generation: self.generation,
            textures: Vec::new(),
            content: 0,
            release: 0,
            width,
            height,
            format,
            api: api.to_string(),
            color_space,
            pixels: Some(self.path.clone()),
            stride: self.stride,
        });
    }
}

impl Drop for PixelFile {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        unsafe {
            if !self.view.is_null() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            }
            if !self.map.is_null() {
                CloseHandle(self.map);
            }
            if !self.file.is_null() {
                CloseHandle(self.file);
            }
        }
    }
}

/// A retired generation's pixel file: only its handle (the file is deleted when it goes).
struct RetiredFile(HANDLE);

unsafe impl Send for RetiredFile {}

impl Drop for RetiredFile {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// The files of the two generations before the current one, kept so the session can
/// still map them if its `surface` came late.
static RETIRED: Mutex<Vec<RetiredFile>> = Mutex::new(Vec::new());

fn retire(f: PixelFile) {
    let mut r = crate::sys::lock(&RETIRED);
    r.push(f.into_file());
    while r.len() > 2 {
        r.remove(0);
    }
}

enum Target {
    None,
    Textures(D3d11Ring),
    File(PixelFile),
}

/// Where a copy's frames in memory go (see the module's notes).
pub struct CpuFrames {
    luid: Option<(u32, i32)>,
    api: &'static str,
    private: Option<(usize, usize)>,
    target: Target,
    /// The frames the target is for, also when it couldn't be made (not retried every
    /// frame), whether that was as pixels, and for which session setup (`epoch`).
    made_for: Option<(Kind, bool, u64)>,
}

unsafe impl Send for CpuFrames {}

impl CpuFrames {
    /// For frames of graphics `api`, shared textures made on the adapter `luid` (the
    /// default one when None).
    pub fn new(luid: Option<(u32, i32)>, api: &'static str) -> CpuFrames {
        CpuFrames { luid, api, private: None, target: Target::None, made_for: None }
    }

    fn ensure(&mut self, kind: Kind) -> bool {
        let pixels = capture::pixels();
        let epoch = capture::epoch();
        if self.made_for == Some((kind, pixels, epoch)) {
            return !matches!(self.target, Target::None);
        }
        match std::mem::replace(&mut self.target, Target::None) {
            Target::File(f) => retire(f),
            Target::Textures(r) => drop(r),
            Target::None => {}
        }
        self.made_for = Some((kind, pixels, epoch));
        if pixels {
            let Some(dir) = capture::pixel_dir() else {
                log!("video: pixels asked for, but the ring block isn't a file; no frames for the session");
                return false;
            };
            if let Some(f) = PixelFile::create(&dir, kind) {
                f.report(self.api);
                self.target = Target::File(f);
            }
        } else {
            if self.private.is_none() {
                self.private = private_device(self.luid);
            }
            let Some((device, _)) = self.private else { return false };
            if let Some(mut r) = D3d11Ring::new(device, kind.width, kind.height, typeless(kind.format), 0x08 | 0x20) {
                r.color_space = kind.color_space;
                r.report(self.api);
                self.target = Target::Textures(r);
            }
        }
        !matches!(self.target, Target::None)
    }

    /// Whether a frame of `kind` would be taken now (somewhere to put it, a slot free):
    /// when not, the game's frame needn't be read back at all.
    pub fn wants(&mut self, kind: Kind) -> bool {
        if !self.ensure(kind) {
            return false;
        }
        match &self.target {
            Target::Textures(r) => r.pick().is_some(),
            Target::File(f) => capture::pick(f.generation, u64::MAX).is_some(),
            Target::None => false,
        }
    }

    /// Puts a frame of `kind` (its rows `pitch` bytes apart at `bits`) where the session
    /// reads it, and publishes it. Whether it was taken.
    ///
    /// # Safety
    /// `bits` must hold `kind.height` rows of `kind.width` pixels, `pitch` apart.
    pub unsafe fn deliver(&mut self, kind: Kind, bits: *const u8, pitch: usize) -> bool {
        if bits.is_null() || !self.ensure(kind) {
            return false;
        }
        match &mut self.target {
            Target::Textures(r) => {
                let Some(slot) = r.pick() else { return false };
                // ID3D11DeviceContext::UpdateSubresource is slot 48.
                vcall!(r.ctx, 48, fn(usize, u32, usize, *const u8, u32, u32) -> (), r.textures[slot], 0, 0, bits, pitch as u32, 0);
                r.finish(slot);
                true
            }
            Target::File(f) => {
                let Some(slot) = capture::pick(f.generation, u64::MAX) else { return false };
                unsafe { f.write(slot, bits, pitch) };
                capture::publish(f.generation, slot, 0);
                true
            }
            Target::None => false,
        }
    }
}

impl Drop for CpuFrames {
    fn drop(&mut self) {
        if let Target::File(f) = std::mem::replace(&mut self.target, Target::None) {
            retire(f);
        }
        self.target = Target::None;
        if let Some((dev, ctx)) = self.private.take() {
            super::com::release(ctx);
            super::com::release(dev);
        }
    }
}

#[cfg(test)]
impl CpuFrames {
    /// Slot `slot`'s shared texture, when the frames go to textures.
    pub fn texture_handle(&self, slot: usize) -> Option<HANDLE> {
        match &self.target {
            Target::Textures(r) => Some(r.handle(slot)),
            _ => None,
        }
    }
}
