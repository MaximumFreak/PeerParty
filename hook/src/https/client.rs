//! The client side of SChannel, for the few requests PeerParty passes on to a real
//! service (xboxlive/real.rs, steam/web.rs): `TlsStream::connect` checks the server's
//! chain and name with Windows' SSL policy, and never accepts PeerParty's own
//! certificate (a redirected lookup must not make PeerParty talk to itself). `pass_on`
//! sends a game's request on unchanged and reads the whole answer.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::ptr;
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Security::Authentication::Identity::{
    AcquireCredentialsHandleW, DeleteSecurityContext, FreeContextBuffer, FreeCredentialsHandle, InitializeSecurityContextW, QueryContextAttributesW,
    SCHANNEL_CRED, SecPkgContext_StreamSizes, UNISP_NAME_W,
};
use windows_sys::Win32::Security::Credentials::SecHandle;
use windows_sys::Win32::Security::Cryptography::{
    CERT_CHAIN_PARA, CERT_CHAIN_POLICY_PARA, CERT_CHAIN_POLICY_STATUS, CERT_CONTEXT, CertFreeCertificateChain, CertFreeCertificateContext,
    CertGetCertificateChain, CertVerifyCertificateChainPolicy,
};

use super::server::Request;
use super::tls::{
    MAX_HANDSHAKE, SEC_E_INCOMPLETE_MESSAGE, SEC_E_OK, SEC_I_CONTINUE_NEEDED, SECBUFFER_EMPTY, SECBUFFER_EXTRA, SECBUFFER_TOKEN, SECPKG_ATTR_STREAM_SIZES,
    TlsStream, buf, desc, is_peerparty, os_error, read_more,
};
use crate::sys::win::wide;
use crate::util::http::Dechunk;

const SECPKG_ATTR_REMOTE_CERT_CONTEXT: u32 = 0x53;
const SECPKG_CRED_OUTBOUND: u32 = 2;
const SCH_CRED_NO_DEFAULT_CREDS: u32 = 0x10;
const SCH_USE_STRONG_CRYPTO: u32 = 0x0040_0000;
const ISC_FLAGS: u32 = 0x4 // replay detect
    | 0x8 // sequence detect
    | 0x10 // confidentiality
    | 0x100 // allocate memory
    | 0x4000 // extended error
    | 0x8000; // stream

impl TlsStream {
    /// Runs the client side of a handshake with `host` (its name goes out as SNI and is
    /// what the server's certificate must name).
    pub fn connect(mut sock: TcpStream, host: &str) -> io::Result<TlsStream> {
        let mut sc: SCHANNEL_CRED = unsafe { std::mem::zeroed() };
        sc.dwVersion = 4;
        // No client certificate offered; the server's checked by Schannel and below.
        sc.dwFlags = SCH_CRED_NO_DEFAULT_CREDS | SCH_USE_STRONG_CRYPTO;
        let mut creds = SecHandle { dwLower: 0, dwUpper: 0 };
        let rc = unsafe {
            AcquireCredentialsHandleW(ptr::null(), UNISP_NAME_W, SECPKG_CRED_OUTBOUND, ptr::null(), &sc as *const _ as *const _, None, ptr::null(), &mut creds, ptr::null_mut())
        };
        if rc != SEC_E_OK {
            return Err(io::Error::other(format!("AcquireCredentialsHandle (client) {rc:#x}")));
        }
        let target = wide(host);
        let mut ctx = SecHandle { dwLower: 0, dwUpper: 0 };
        let mut incoming = Vec::new();
        let done = client_handshake(&mut sock, &creds, &target, &mut ctx, false, &mut incoming).and_then(|()| check_server(&ctx, host));
        let sizes = done.and_then(|()| {
            let mut sizes: SecPkgContext_StreamSizes = unsafe { std::mem::zeroed() };
            let rc = unsafe { QueryContextAttributesW(&ctx, SECPKG_ATTR_STREAM_SIZES, &mut sizes as *mut _ as *mut _) };
            if rc == SEC_E_OK { Ok(sizes) } else { Err(io::Error::other(format!("QueryContextAttributes {rc:#x}"))) }
        });
        match sizes {
            Ok(sizes) => Ok(TlsStream { sock, ctx, sizes, incoming, plain: Vec::new(), eof: false, client: Some((creds, target)) }),
            Err(e) => {
                if ctx.dwLower != 0 || ctx.dwUpper != 0 {
                    unsafe { DeleteSecurityContext(&ctx) };
                }
                unsafe { FreeCredentialsHandle(&creds) };
                Err(e)
            }
        }
    }
}

/// The client side of a handshake on `ctx`: the first (`have_ctx` false), or one the
/// server starts later (TLS 1.3's tickets and key updates come as SEC_I_RENEGOTIATE).
pub(crate) fn client_handshake(sock: &mut TcpStream, creds: &SecHandle, target: &[u16], ctx: &mut SecHandle, mut have_ctx: bool, incoming: &mut Vec<u8>) -> io::Result<()> {
    loop {
        if have_ctx && incoming.is_empty() {
            read_more(sock, incoming)?;
        }
        if incoming.len() > MAX_HANDSHAKE {
            return Err(io::Error::other("handshake too long"));
        }
        let mut in_bufs = [buf(SECBUFFER_TOKEN, incoming.as_mut_ptr(), incoming.len()), buf(SECBUFFER_EMPTY, ptr::null_mut(), 0)];
        let in_desc = desc(&mut in_bufs);
        let mut out_bufs = [buf(SECBUFFER_TOKEN, ptr::null_mut(), 0)];
        let mut out_desc = desc(&mut out_bufs);
        let mut attrs = 0u32;
        let rc = unsafe {
            InitializeSecurityContextW(
                creds,
                if have_ctx { &*ctx } else { ptr::null() },
                target.as_ptr(),
                ISC_FLAGS,
                0,
                0,
                if have_ctx { &in_desc } else { ptr::null() },
                0,
                &mut *ctx,
                &mut out_desc,
                &mut attrs,
                ptr::null_mut(),
            )
        };
        have_ctx = true;
        let out = out_bufs[0];
        if out.cbBuffer > 0 && !out.pvBuffer.is_null() {
            let token = unsafe { std::slice::from_raw_parts(out.pvBuffer as *const u8, out.cbBuffer as usize) };
            let sent = sock.write_all(token);
            unsafe { FreeContextBuffer(out.pvBuffer) };
            sent?;
        }
        match rc {
            SEC_E_INCOMPLETE_MESSAGE => read_more(sock, incoming)?,
            SEC_I_CONTINUE_NEEDED | SEC_E_OK => {
                if in_bufs[1].BufferType == SECBUFFER_EXTRA && in_bufs[1].cbBuffer > 0 {
                    let keep = in_bufs[1].cbBuffer as usize;
                    incoming.drain(..incoming.len() - keep);
                } else {
                    incoming.clear();
                }
                if rc == SEC_E_OK {
                    return Ok(());
                }
            }
            other => return Err(io::Error::other(format!("InitializeSecurityContext {other:#x}"))),
        }
    }
}

/// The server's certificate chains to a trusted root and names `host` (Windows' SSL
/// policy, as .NET checks it), and isn't PeerParty's own.
fn check_server(ctx: &SecHandle, host: &str) -> io::Result<()> {
    let mut cert: *const CERT_CONTEXT = ptr::null();
    let rc = unsafe { QueryContextAttributesW(ctx, SECPKG_ATTR_REMOTE_CERT_CONTEXT, &mut cert as *mut _ as *mut _) };
    if rc != SEC_E_OK || cert.is_null() {
        return Err(io::Error::other(format!("no server certificate ({rc:#x})")));
    }
    let verdict = if is_peerparty(cert) { Err(io::Error::other("the server is PeerParty's own")) } else { ssl_policy(cert, host) };
    unsafe { CertFreeCertificateContext(cert) };
    verdict
}

#[repr(C)]
struct SslPolicy {
    size: u32,
    auth_type: u32,
    checks: u32,
    server_name: *const u16,
}

/// Windows' SSL policy verdict on `cert` for `host`: chain built with the default
/// engine, then checked (0 = trusted).
fn ssl_policy(cert: *const CERT_CONTEXT, host: &str) -> io::Result<()> {
    let mut para: CERT_CHAIN_PARA = unsafe { std::mem::zeroed() };
    para.cbSize = std::mem::size_of::<CERT_CHAIN_PARA>() as u32;
    let mut chain = ptr::null_mut();
    let built = unsafe { CertGetCertificateChain(0, cert, ptr::null(), ptr::null_mut(), &para, 0, ptr::null(), &mut chain) };
    if built == 0 {
        return Err(io::Error::other(os_error("CertGetCertificateChain")));
    }
    let name = wide(host);
    let mut ssl = SslPolicy { size: std::mem::size_of::<SslPolicy>() as u32, auth_type: 2, checks: 0, server_name: name.as_ptr() };
    let policy = CERT_CHAIN_POLICY_PARA { cbSize: std::mem::size_of::<CERT_CHAIN_POLICY_PARA>() as u32, dwFlags: 0, pvExtraPolicyPara: &mut ssl as *mut _ as *mut _ };
    let mut status: CERT_CHAIN_POLICY_STATUS = unsafe { std::mem::zeroed() };
    status.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_STATUS>() as u32;
    let ok = unsafe { CertVerifyCertificateChainPolicy(4 as _, chain, &policy, &mut status) };
    unsafe { CertFreeCertificateChain(chain) };
    match (ok, status.dwError) {
        (0, _) => Err(io::Error::other(os_error("CertVerifyCertificateChainPolicy"))),
        (_, 0) => Ok(()),
        (_, e) => Err(io::Error::other(format!("the server's certificate isn't trusted for {host} ({e:#x})"))),
    }
}

// ---- a game's request, passed on to the real host ----

/// Headers not passed on: about this connection, or ours to set.
const HOP: &[&str] = &["host", "connection", "content-length", "transfer-encoding", "accept-encoding", "keep-alive", "te", "upgrade", "proxy-connection", "proxy-authorization"];
/// The longest answer read.
const MAX_ANSWER: usize = 1 << 20;

/// How long a request passed on may take: to connect, and in all.
#[derive(Clone, Copy)]
pub struct Waits {
    pub connect: Duration,
    pub answer: Duration,
}

/// The game's request passed on to the real `host`, unchanged but for the headers about
/// the connection: (status, body). It runs on a thread of its own, whose lookups aren't
/// redirected (dns.rs), and the caller waits at most `waits.answer`.
pub fn pass_on(host: &'static str, req: &Request, waits: Waits) -> Result<(u16, String), String> {
    let raw = request_bytes(host, req);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("peerparty-https-pass-on".into())
        .spawn(move || {
            crate::net::dns::bypass_this_thread();
            let _ = tx.send(exchange(host, &raw, waits));
        })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(waits.answer).unwrap_or_else(|_| Err(format!("no answer in {} s", waits.answer.as_secs())))
}

/// The game's request as it goes to `host`.
pub fn request_bytes(host: &str, req: &Request) -> Vec<u8> {
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {host}\r\n", req.method, req.path);
    for (k, v) in &req.headers {
        if !HOP.iter().any(|h| k.eq_ignore_ascii_case(h)) && !k.contains(['\r', '\n']) && !v.contains(['\r', '\n']) {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    head.push_str(&format!("Content-Length: {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n", req.body.len()));
    let mut out = head.into_bytes();
    out.extend_from_slice(&req.body);
    out
}

/// Sends `raw` to `host` on port 443 and reads the whole answer: (status, body).
pub fn exchange(host: &str, raw: &[u8], waits: Waits) -> Result<(u16, String), String> {
    let addr = (host, 443).to_socket_addrs().map_err(|e| format!("lookup: {e}"))?.find(|a| a.is_ipv4()).ok_or("no IPv4 address")?;
    if addr.ip().is_loopback() {
        return Err("the name points at this PC".into());
    }
    let sock = TcpStream::connect_timeout(&addr, waits.connect).map_err(|e| format!("connect to {addr}: {e}"))?;
    let _ = sock.set_read_timeout(Some(waits.answer));
    let _ = sock.set_write_timeout(Some(waits.answer));
    let mut tls = TlsStream::connect(sock, host).map_err(|e| format!("TLS: {e}"))?;
    tls.write_all(raw).map_err(|e| format!("send: {e}"))?;
    let mut got = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        match tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&chunk[..n]),
            Err(e) if got.is_empty() => return Err(format!("receive: {e}")),
            Err(_) => break,
        }
        if got.len() > MAX_ANSWER {
            return Err("answer too long".into());
        }
        if parse_response(&got).is_some_and(|(_, _, complete)| complete) {
            break;
        }
    }
    let (status, body, _) = parse_response(&got).ok_or("not an HTTP answer")?;
    Ok((status, body))
}

/// (status, body, whether the whole body is here) of an HTTP/1.1 answer read so far.
pub fn parse_response(got: &[u8]) -> Option<(u16, String, bool)> {
    let end = got.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&got[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let header = |name: &str| {
        head.split("\r\n").skip(1).filter_map(|l| l.split_once(':')).find(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string())
    };
    let rest = &got[end + 4..];
    if header("Transfer-Encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        // The sizes are the server's: one past anything read so far is "not yet" (the
        // read stops at MAX_ANSWER), never a sum that wraps.
        let mut d = Dechunk::new(MAX_ANSWER);
        let complete = d.feed(&mut rest.to_vec()).ok()?;
        return Some((status, String::from_utf8_lossy(d.body()).into(), complete));
    }
    match header("Content-Length").and_then(|n| n.parse::<usize>().ok()) {
        Some(n) => Some((status, String::from_utf8_lossy(&rest[..rest.len().min(n)]).into(), rest.len() >= n)),
        None => Some((status, String::from_utf8_lossy(rest).into(), false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Request {
        Request { method: method.into(), path: path.into(), headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: body.as_bytes().to_vec() }
    }

    #[test]
    fn the_games_request_goes_on_as_it_came() {
        let r = req(
            "POST",
            "/users/xuid(1)/permission/validate",
            &[("Host", "privacy.xboxlive.com"), ("Authorization", "XBL3.0 x=1;tok"), ("Signature", "AAAA"), ("x-xbl-contract-version", "3"), ("Accept-Encoding", "gzip"), ("Connection", "keep-alive"), ("Content-Length", "2")],
            "{}",
        );
        let text = String::from_utf8(request_bytes("privacy.xboxlive.com", &r)).unwrap();
        assert!(text.starts_with("POST /users/xuid(1)/permission/validate HTTP/1.1\r\nHost: privacy.xboxlive.com\r\n"), "{text}");
        for kept in ["Authorization: XBL3.0 x=1;tok\r\n", "Signature: AAAA\r\n", "x-xbl-contract-version: 3\r\n", "Content-Length: 2\r\n", "Accept-Encoding: identity\r\n", "Connection: close\r\n"] {
            assert!(text.contains(kept), "{kept} in {text}");
        }
        assert!(!text.contains("gzip") && !text.contains("keep-alive"));
        assert!(text.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn answers_are_read_whole_plain_or_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_response(plain), Some((200, "hello".into(), true)));
        assert_eq!(parse_response(&plain[..plain.len() - 2]).map(|r| r.2), Some(false));
        let chunked = b"HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked), Some((403, "abcde".into(), true)));
        assert_eq!(parse_response(&chunked[..60]).map(|r| r.2), Some(false));
        assert_eq!(parse_response(&chunked[..30]), None, "headers not all here yet");
        assert_eq!(parse_response(b"garbage"), None);
    }
}
