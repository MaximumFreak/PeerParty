//! HTTP/1.1 message pieces both directions use: the local HTTPS server reading a game's
//! requests (https/server.rs) and the one client PeerParty runs (xboxlive/real.rs).

use std::io;

/// Where `needle` first starts in `hay`.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn bad(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

/// A chunked body (RFC 9112 section 7.1) decoded as its bytes arrive; trailers are
/// skipped. No more than `max` bytes of body are taken, and a chunk size is checked
/// against that before it's added to anything, so a size near the largest number can't
/// wrap round.
pub struct Dechunk {
    body: Vec<u8>,
    max: usize,
    /// Past the last chunk, in the trailers.
    trailers: bool,
}

impl Dechunk {
    pub fn new(max: usize) -> Dechunk {
        Dechunk { body: Vec::new(), max, trailers: false }
    }

    /// Takes what it can from the front of `buf` (what it took is gone from it).
    /// Ok(true) once the body is whole; Ok(false) while it needs more; Err for a size
    /// that isn't one or a body past the limit.
    pub fn feed(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        loop {
            let Some(line) = find(buf, b"\r\n") else { return Ok(false) };
            if self.trailers {
                buf.drain(..line + 2);
                if line == 0 {
                    return Ok(true);
                }
                continue;
            }
            let text = std::str::from_utf8(&buf[..line]).map_err(|_| bad("bad chunk size"))?;
            let size = usize::from_str_radix(text.split(';').next().unwrap_or("").trim(), 16).map_err(|_| bad("bad chunk size"))?;
            if size > self.max - self.body.len().min(self.max) {
                return Err(bad("body too big"));
            }
            if size == 0 {
                buf.drain(..line + 2);
                self.trailers = true;
                continue;
            }
            // The size line, the data and its CRLF, all here before any of it is taken.
            let Some(end) = (line + 2).checked_add(size).filter(|e| e.checked_add(2).is_some()) else {
                return Err(bad("body too big"));
            };
            if buf.len() < end + 2 {
                return Ok(false);
            }
            self.body.extend_from_slice(&buf[line + 2..end]);
            buf.drain(..end + 2);
        }
    }

    /// The body so far.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn into_body(self) -> Vec<u8> {
        self.body
    }
}

/// The reason phrase for the statuses the stand-ins answer with.
pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Bad Request",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole(bytes: &[u8], max: usize) -> io::Result<Option<(Vec<u8>, usize)>> {
        let mut buf = bytes.to_vec();
        let mut d = Dechunk::new(max);
        Ok(d.feed(&mut buf)?.then(|| (d.into_body(), buf.len())))
    }

    #[test]
    fn chunked_bodies_decode_with_extensions_and_trailers() {
        let b = b"3\r\nabc\r\n2;x=y\r\nde\r\n0\r\nTrailer: 1\r\n\r\nNEXT";
        assert_eq!(whole(b, 100).unwrap(), Some((b"abcde".to_vec(), 4)), "what follows is left");
        // Byte by byte, as slowly as a socket may hand it over.
        let (mut d, mut buf) = (Dechunk::new(100), Vec::new());
        let mut done = false;
        for &c in &b[..b.len() - 4] {
            buf.push(c);
            assert!(!done);
            done = d.feed(&mut buf).unwrap();
        }
        assert!(done && d.body() == b"abcde" && buf.is_empty());
    }

    #[test]
    fn hostile_sizes_are_refused() {
        assert!(whole(b"zz\r\n", 100).is_err());
        assert!(whole(b"\xff\r\n", 100).is_err());
        assert!(whole(b"65\r\n", 100).is_err(), "past the limit before any data");
        // Near the largest number: no sum wraps round.
        assert!(whole(format!("{:x}\r\nab\r\n", usize::MAX).as_bytes(), 100).is_err());
        assert!(whole(format!("{:x}\r\nab\r\n", usize::MAX - 1).as_bytes(), usize::MAX).is_err());
        assert!(whole(format!("{:x}\r\nab\r\n", usize::MAX / 2).as_bytes(), usize::MAX).unwrap().is_none());
        assert!(whole(b"5\r\nab", 100).unwrap().is_none(), "waits for the rest");
        // Many small chunks still can't pass the limit together.
        assert!(whole(&b"1\r\na\r\n".repeat(11), 10).is_err());
    }

    #[test]
    fn find_and_reasons() {
        assert_eq!(find(b"ab\r\n\r\ncd", b"\r\n\r\n"), Some(2));
        assert_eq!(find(b"ab", b"abc"), None);
        assert_eq!(reason(503), "Service Unavailable");
        assert_eq!(reason(299), "Bad Request");
    }
}
