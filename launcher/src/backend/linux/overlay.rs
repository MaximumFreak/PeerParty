//! The overlay's frame block in a file both sides map (`ShmBacking::FileMmap`): this
//! process `mmap`s it shared and writes frames into it (host/frame.rs), and the hook in
//! the game maps the same file through Wine (`CreateFileMappingW` on its `Z:\...` name)
//! and draws it inside the game's own frames, which is what shows inside Gamescope.

use std::os::fd::AsRawFd;

/// A shared, read-write mapping of a whole file, unmapped on drop.
pub struct Mmap {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain shared memory owned by this value; who writes when is the
// frame block's own sequence lock (`peerparty_protocol::shm`).
unsafe impl Send for Mmap {}

impl Mmap {
    /// Maps the first `len` bytes of `file` (opened for reading and writing), shared, so
    /// what is written reaches every other mapping of it.
    pub fn shared(file: &std::fs::File, len: usize) -> std::io::Result<Mmap> {
        if len == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "an empty mapping"));
        }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Mmap { ptr: ptr.cast(), len })
    }

    /// The mapping's first byte (page aligned).
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_protocol::transport::FileShm;

    /// Bytes written through the mapping are the file's, as another reader sees them.
    #[test]
    fn a_shared_mapping_writes_the_file() {
        let dir = std::env::temp_dir();
        let path = FileShm::path_in(&dir, std::process::id(), &[0x5a, 0xa5]);
        let shm = FileShm::create(&path, 8192).unwrap();
        let map = Mmap::shared(&shm.file, 8192).unwrap();
        assert_eq!(map.len(), 8192);
        assert_eq!(map.as_ptr() as usize % 4096, 0, "page aligned");
        unsafe { map.as_ptr().add(4100).write(0x42) };
        drop(map);
        assert_eq!(std::fs::read(&path).unwrap()[4100], 0x42);
        shm.remove().unwrap();
        assert!(Mmap::shared(&std::fs::File::open("/dev/null").unwrap(), 0).is_err());
    }
}
