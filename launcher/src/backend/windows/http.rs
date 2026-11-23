//! HTTPS GET over WinHTTP (Windows' own client: its proxy settings, its certificate
//! checks), for GitHub's API and a release's small checksum file only (update.rs).

use std::ffi::c_void;
use windows_sys::Win32::Networking::WinHttp::*;

struct H(*mut c_void);
impl Drop for H {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn err(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

pub fn get(url: &str, accept: &str, max: usize) -> Result<Vec<u8>, String> {
    let rest = url.strip_prefix("https://").ok_or("only https")?;
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    unsafe {
        let agent = w(&format!("PeerParty/{}", env!("CARGO_PKG_VERSION")));
        let session = H(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), 0));
        if session.0.is_null() {
            return Err(err("WinHttpOpen"));
        }
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 15_000);
        let conn = H(WinHttpConnect(session.0, w(host).as_ptr(), INTERNET_DEFAULT_HTTPS_PORT as u16, 0));
        if conn.0.is_null() {
            return Err(err("WinHttpConnect"));
        }
        let req = H(WinHttpOpenRequest(conn.0, w("GET").as_ptr(), w(&path).as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null(), WINHTTP_FLAG_SECURE));
        if req.0.is_null() {
            return Err(err("WinHttpOpenRequest"));
        }
        let headers = w(&format!("Accept: {accept}\r\nCache-Control: no-cache\r\n"));
        if WinHttpSendRequest(req.0, headers.as_ptr(), u32::MAX, std::ptr::null(), 0, 0, 0) == 0 || WinHttpReceiveResponse(req.0, std::ptr::null_mut()) == 0 {
            return Err(err("the request failed"));
        }
        let mut code = 0u32;
        let mut len = 4u32;
        WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, std::ptr::null(), (&mut code as *mut u32).cast(), &mut len, std::ptr::null_mut());
        if code != 200 {
            return Err(format!("HTTP {code}"));
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let mut n = 0u32;
            if WinHttpReadData(req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n) == 0 {
                return Err(err("reading the answer"));
            }
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            if out.len() > max {
                return Err("the answer is too long".into());
            }
        }
        Ok(out)
    }
}
