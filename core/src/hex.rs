//! Lowercase hex for keys, ids and checksums.

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Exactly `N` bytes' worth of hex digits (either case; surrounding whitespace is
/// ignored), else `None`.
pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, x) in out.iter_mut().enumerate() {
        *x = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_junk() {
        assert_eq!(hex(&[0x00, 0xab, 0x7f]), "00ab7f");
        assert_eq!(unhex::<3>(" 00AB7f\n"), Some([0x00, 0xab, 0x7f]));
        assert_eq!(unhex::<3>("00ab7"), None);
        assert_eq!(unhex::<3>("00ab7f00"), None);
        // from_str_radix alone takes a sign: "+f" would read as 0x0f.
        assert_eq!(unhex::<2>("+f+f"), None);
        assert_eq!(unhex::<2>("xyzw"), None);
        assert_eq!(unhex::<1>("\u{e9}"), None);
    }
}
