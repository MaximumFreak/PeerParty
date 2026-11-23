//! HTTPS GET for the update check (update.rs), through the system's curl: its
//! certificates and proxy settings, as WinHTTP's are on Windows. SteamOS and the
//! common desktop systems have it.

use std::process::{Command, Stdio};

pub fn get(url: &str, accept: &str, max: usize) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err("only https".into());
    }
    let out = Command::new("curl")
        .args(["--silent", "--show-error", "--fail", "--location", "--proto", "=https", "--max-time", "30"])
        .arg("--max-filesize")
        .arg(max.to_string())
        .arg("--user-agent")
        .arg(format!("PeerParty/{}", env!("CARGO_PKG_VERSION")))
        .arg("--header")
        .arg(format!("Accept: {accept}"))
        .arg("--header")
        .arg("Cache-Control: no-cache")
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("curl failed").trim().to_string());
    }
    if out.stdout.len() > max {
        return Err("the answer is too long".into());
    }
    Ok(out.stdout)
}
