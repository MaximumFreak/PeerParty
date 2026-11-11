//! Checksums for release downloads: how the launcher knows a downloaded `peerparty.exe`
//! is the one published in the project's GitHub release (launcher/src/update.rs).
//!
//! GitHub publishes a SHA-256 for every release asset; for a release without one, a plain
//! checksum file beside the exe gives it, in either form:
//!
//! ```text
//! <64 hex digits>  peerparty.exe          (sha256sum's form; `*peerparty.exe` too)
//! ```
//!
//! ```text
//! file peerparty.exe                      (peerparty-release.txt; any line may be left out
//! size 24802816                            but sha256)
//! sha256 <64 hex digits>
//! ```
//!
//! Nothing is signed: there is no release key.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, x) in out.iter_mut().enumerate() {
        *x = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// A file's SHA-256 and size, read in pieces.
pub fn sha256_file(path: &Path) -> std::io::Result<([u8; 32], u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        h.update(&buf[..n]);
    }
    Ok((h.finalize().into(), size))
}

fn same_file(listed: &str, file: &str) -> bool {
    let name = listed.trim().trim_start_matches('*');
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    name.eq_ignore_ascii_case(file)
}

/// The SHA-256 for `file` in a checksum file, and its size if the file gives one. `None`
/// if it has none for `file`, or names another file.
pub fn read_checksum(text: &str, file: &str) -> Option<([u8; 32], Option<u64>)> {
    let (mut sha, mut size, mut other) = (None, None, false);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (first, rest) = line.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((line, ""));
        match first.to_ascii_lowercase().as_str() {
            "sha256" | "sha256:" => sha = unhex::<32>(rest.trim_start_matches("sha256:")).or(sha),
            "size" => size = rest.parse().ok().or(size),
            "file" | "name" => other |= !same_file(rest, file),
            "version" | "peerparty-release" => {}
            _ => {
                // sha256sum's form: the hash, then the file it is for (or nothing). Lines
                // for other files are skipped.
                if let Some(h) = unhex::<32>(first.trim_start_matches("sha256:"))
                    && (rest.is_empty() || same_file(rest, file))
                {
                    sha = Some(h);
                }
            }
        }
    }
    // A `file` line naming another file: not this one's.
    if other {
        return None;
    }
    sha.map(|s| (s, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_checksum_files_in_either_form() {
        let h = sha256(b"exe");
        let x = hex(&h);
        assert_eq!(read_checksum(&format!("{x}  peerparty.exe\n"), "peerparty.exe"), Some((h, None)));
        assert_eq!(read_checksum(&format!("{}  *dist/PeerParty.exe\r\n", x.to_uppercase()), "peerparty.exe"), Some((h, None)));
        assert_eq!(read_checksum(&format!("{x}\n"), "peerparty.exe"), Some((h, None)));
        // A list of several files: only this one's line counts.
        let list = format!("{}  THIRD-PARTY-NOTICES.md\n{x}  peerparty.exe\n", hex(&sha256(b"notes")));
        assert_eq!(read_checksum(&list, "peerparty.exe"), Some((h, None)));
        let described = format!("peerparty-release 1\nversion 0.2.0\nfile peerparty.exe\nsize 1234\nsha256 {x}\n");
        assert_eq!(read_checksum(&described, "peerparty.exe"), Some((h, Some(1234))));
        // A signature line from an older format is just ignored.
        assert_eq!(read_checksum(&format!("{described}signature abcd\n"), "peerparty.exe"), Some((h, Some(1234))));
    }

    #[test]
    fn refuses_checksums_for_other_files_or_none() {
        let x = hex(&sha256(b"exe"));
        assert_eq!(read_checksum(&format!("{x}  other.exe\n"), "peerparty.exe"), None);
        assert_eq!(read_checksum(&format!("file other.exe\nsha256 {x}\n"), "peerparty.exe"), None);
        assert_eq!(read_checksum("sha256 1234\n", "peerparty.exe"), None);
        assert_eq!(read_checksum("<html>Not Found</html>", "peerparty.exe"), None);
        assert_eq!(read_checksum("", "peerparty.exe"), None);
    }

    #[test]
    fn hashes_files_in_pieces() {
        let p = std::env::temp_dir().join(format!("peerparty-release-{}.bin", std::process::id()));
        let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        std::fs::write(&p, &data).unwrap();
        assert_eq!(sha256_file(&p).unwrap(), (sha256(&data), data.len() as u64));
        let _ = std::fs::remove_file(&p);
    }
}
