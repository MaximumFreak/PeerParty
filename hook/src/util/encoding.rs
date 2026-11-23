//! Text encodings the stand-ins' services speak: base64 (RFC 4648), GUID-shaped ids and
//! random text from an alphabet.

use crate::util::crypto::random_bytes;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 (RFC 4648 section 4), padded.
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Base64url without padding (RFC 4648 section 5), as JWTs use.
pub fn base64url(data: &[u8]) -> String {
    base64(data).trim_end_matches('=').replace('+', "-").replace('/', "_")
}

/// Either alphabet, padded or not, line breaks skipped. None for any other character.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Lowercase hex.
pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// 16 bytes as a GUID's text (`8-4-4-4-12` lowercase hex digits).
pub fn guid(id: &[u8; 16]) -> String {
    let h = hex(id);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// A random GUID-shaped id.
pub fn uuid() -> String {
    guid(&random_bytes(16).try_into().unwrap())
}

/// Random text from `alphabet`.
pub fn random_text(len: usize, alphabet: &[u8]) -> String {
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in random_bytes(16) {
            if out.len() < len {
                out.push(alphabet[b as usize % alphabet.len()] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (plain, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(plain.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), plain.as_bytes());
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(base64_decode("Zm9v!"), None);
    }

    #[test]
    fn random_text_uses_the_alphabet() {
        let t = random_text(40, b"AB");
        assert_eq!(t.len(), 40);
        assert!(t.bytes().all(|b| b == b'A' || b == b'B'));
    }

    #[test]
    fn guids_are_8_4_4_4_12() {
        assert_eq!(guid(&[0xab; 16]), "abababab-abab-abab-abab-abababababab");
        let u = uuid();
        assert_eq!(u.split('-').map(str::len).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        assert_ne!(u, uuid());
    }
}
