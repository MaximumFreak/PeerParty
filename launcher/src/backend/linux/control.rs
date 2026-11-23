//! What Windows does with named events (`peerparty --invite`, the window's Close all),
//! a Linux run does with a socket of its own: `run-<pid>.sock` in the runtime folder
//! (this user's alone, mode 0700), made while its game runs. One line asks
//! (`invite`, `close`), one line answers (`1` done, `0` not).
//!
//! There's no "game in front" to pick under Gamescope (the game has the screen): an
//! invite goes to every game PeerParty runs for this user, which is the one being played.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::runtime_dir;

const PREFIX: &str = "run-";
const SUFFIX: &str = ".sock";
/// How long a question waits for its answer.
const WAIT: Duration = Duration::from_millis(1500);

/// This run's socket, deleted when it's dropped (the game has ended).
pub struct Control {
    path: PathBuf,
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn socket_for(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{PREFIX}{pid}{SUFFIX}"))
}

/// Answers questions for this run on a thread of its own: `invite` asks every player on
/// the network to the game (`invite` says whether it went), `close` closes the game.
pub fn listen(invite: impl Fn() -> bool + Send + 'static, close: impl Fn() + Send + 'static) -> Option<Control> {
    listen_in(&runtime_dir(), std::process::id(), invite, close)
}

fn listen_in(dir: &Path, pid: u32, invite: impl Fn() -> bool + Send + 'static, close: impl Fn() + Send + 'static) -> Option<Control> {
    let path = socket_for(dir, pid);
    // One left by an earlier process of this id (it ended without cleaning up).
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).ok()?;
    std::thread::Builder::new()
        .name("peerparty-control".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(WAIT));
                let mut line = String::new();
                // A question is a word: anything longer isn't one.
                let mut r = BufReader::new(&stream).take(64);
                if r.read_line(&mut line).is_err() {
                    continue;
                }
                let answer = match line.trim() {
                    "invite" => invite(),
                    "close" => {
                        close();
                        true
                    }
                    _ => false,
                };
                let _ = (&stream).write_all(if answer { b"1\n" } else { b"0\n" });
            }
        })
        .ok()?;
    Some(Control { path })
}

/// Asks one run; its answer, or None when nothing answers there (a socket left behind
/// by a run that ended is deleted).
fn ask(path: &Path, question: &str) -> Option<bool> {
    let mut s = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::ConnectionRefused {
                let _ = std::fs::remove_file(path);
            }
            return None;
        }
    };
    s.set_read_timeout(Some(WAIT)).ok()?;
    s.set_write_timeout(Some(WAIT)).ok()?;
    s.write_all(format!("{question}\n").as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(&s).take(8).read_line(&mut line).ok()?;
    Some(line.trim() == "1")
}

/// Every run's socket in `dir`.
fn runs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX)))
        .collect()
}

/// `peerparty --invite`, the window's Invite: whether a game took it. (`last`, the game
/// last in front, is Windows' distinction: here every running game is asked.)
pub fn signal_invite(_last: bool) -> Result<bool, String> {
    Ok(invite_in(&runtime_dir()))
}

fn invite_in(dir: &Path) -> bool {
    let mut taken = false;
    for run in runs(dir) {
        taken |= ask(&run, "invite") == Some(true);
    }
    taken
}

/// Close all: asks the peerparty of process `pid` to close the game it started.
pub fn close_game(pid: u32) -> bool {
    ask(&socket_for(&runtime_dir(), pid), "close") == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A run answers its questions, a left-behind socket is cleared away, and the socket
    /// goes when the run does.
    #[test]
    fn runs_answer_invites_and_close() {
        let dir = std::env::temp_dir().join(format!("peerparty-control-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (invited, closed) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (i, c) = (invited.clone(), closed.clone());
        let run = listen_in(&dir, 4242, move || i.fetch_add(1, Ordering::SeqCst) == 0, move || {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .expect("a socket");
        // A socket nobody listens on any more.
        let stale = socket_for(&dir, 99);
        drop(UnixListener::bind(&stale).unwrap());
        assert!(invite_in(&dir), "the first invite went");
        assert!(!stale.exists(), "the stale socket was cleared");
        assert!(!invite_in(&dir), "the second one didn't");
        assert_eq!(ask(&socket_for(&dir, 4242), "close"), Some(true));
        assert_eq!(ask(&socket_for(&dir, 4242), "something else"), Some(false));
        assert_eq!((invited.load(Ordering::SeqCst), closed.load(Ordering::SeqCst)), (2, 1));
        drop(run);
        assert!(runs(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
