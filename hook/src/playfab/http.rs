//! HTTP/1.1 over the TLS stream: PlayFab's SDKs POST JSON and keep the connection.
//! Requests for Unity Gaming Services' hosts (by their Host header) go to ugs/, and a
//! WebSocket upgrade there (Wire) takes the connection over; Xbox Live's friends hosts
//! go to xboxlive/.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use super::api;
use super::tls::TlsStream;
use crate::log;

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub fn serve(conn: TcpStream) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = conn.set_nodelay(true);
    let mut tls = match TlsStream::accept(conn) {
        Ok(t) => t,
        Err(e) => {
            log!("playfab tls handshake failed: {e}");
            return;
        }
    };
    let mut pending = Vec::new();
    loop {
        let req = match read_request(&mut tls, &mut pending) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                // The game closing its connection (quitting) isn't news, and neither is a
                // kept-alive connection the game left idle past the read timeout.
                if !matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) {
                    log!("playfab connection ended: {e}");
                }
                return;
            }
        };
        let close = req
            .header("Connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"));
        let unity = req.header("Host").is_some_and(crate::ugs::is_host);
        if unity
            && req
                .header("Upgrade")
                .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            crate::ugs::wire::serve(tls, &req, pending);
            return;
        }
        let xbox = req.header("Host").is_some_and(|h| crate::xboxlive::service_of(h).is_some());
        let (status, body) = if unity {
            crate::ugs::handle(&req)
        } else if xbox {
            crate::xboxlive::handle(&req)
        } else {
            api::handle(&req)
        };
        let head = format!(
            "HTTP/1.1 {status} {}\r\n{}Content-Length: {}\r\nX-RequestId: peerparty\r\n{}\r\n",
            reason(status),
            if status == 204 { "" } else { "Content-Type: application/json; charset=utf-8\r\n" },
            body.len(),
            if close { "Connection: close\r\n" } else { "" }
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(body.as_bytes());
        if tls.write_all(&out).is_err() || close {
            return;
        }
    }
}

/// The most a request's head and body may be. Only this PC's own programs reach this
/// service, but one of them sending without end shouldn't take the game's memory.
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 16 << 20;

fn too_big() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "request too big")
}

/// Reads one request. `pending` holds bytes read past the previous one.
fn read_request(tls: &mut TlsStream, pending: &mut Vec<u8>) -> io::Result<Option<Request>> {
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = find(pending, b"\r\n\r\n") {
            break i;
        }
        if pending.len() > MAX_HEAD {
            return Err(too_big());
        }
        let n = tls.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        pending.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&pending[..head_end]).to_string();
    pending.drain(..head_end + 4);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_ascii_uppercase();
    let path = parts.next().unwrap_or("/").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut req = Request {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    if req
        .header("Transfer-Encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        req.body = read_chunked(tls, pending)?;
        return Ok(Some(req));
    }
    let len: usize = req
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err(too_big());
    }
    // .NET Framework asks before sending a POST body.
    if req
        .header("Expect")
        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
        && pending.len() < len
    {
        tls.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    while pending.len() < len {
        let n = tls.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        pending.extend_from_slice(&chunk[..n]);
    }
    req.body = pending.drain(..len).collect();
    Ok(Some(req))
}

/// A chunked request body (RFC 9112 section 7.1); trailers are skipped.
fn read_chunked(tls: &mut TlsStream, pending: &mut Vec<u8>) -> io::Result<Vec<u8>> {
    let mut chunk = [0u8; 8192];
    let mut body = Vec::new();
    let mut more = |tls: &mut TlsStream, pending: &mut Vec<u8>| -> io::Result<()> {
        if pending.len() > MAX_BODY + MAX_HEAD {
            return Err(too_big());
        }
        let n = tls.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        pending.extend_from_slice(&chunk[..n]);
        Ok(())
    };
    loop {
        let line_end = loop {
            if let Some(i) = find(pending, b"\r\n") {
                break i;
            }
            more(tls, pending)?;
        };
        let size_text = String::from_utf8_lossy(&pending[..line_end]).to_string();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("0").trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        pending.drain(..line_end + 2);
        // Before it is added to anything: a size near the largest number wrapped
        // round in `size + 2` below, and the copy after it panicked.
        if size > MAX_BODY - body.len().min(MAX_BODY) {
            return Err(too_big());
        }
        if size == 0 {
            // Trailers, up to the empty line.
            loop {
                let end = loop {
                    if let Some(i) = find(pending, b"\r\n") {
                        break i;
                    }
                    more(tls, pending)?;
                };
                pending.drain(..end + 2);
                if end == 0 {
                    return Ok(body);
                }
            }
        }
        while pending.len() < size + 2 {
            more(tls, pending)?;
        }
        body.extend_from_slice(&pending[..size]);
        pending.drain(..size + 2);
    }
}

fn reason(status: u16) -> &'static str {
    match status {
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

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
