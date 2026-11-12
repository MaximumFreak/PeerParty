//! Windows' sound devices (MMDevice API endpoints), and which ones voice uses.
//!
//! A thread keeps a snapshot of the active microphones and speakers, their names and
//! the two defaults (Sound settings' default device and default communications
//! device), looked at again every `REFRESH`; its `version` goes up whenever any of it
//! changes, so the SDKs can tell the game its device list changed. The game's choice
//! (`Choice`) is one for the whole process each way, as the SDKs' own are, and
//! resolves against the snapshot: a chosen device that went away falls back to the
//! communications default until it comes back.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Once};
use std::time::{Duration, Instant};

use super::com::{self, CAPTURE, COMMUNICATIONS, CONSOLE, RENDER, release};
use crate::log;

const REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The endpoint id (IMMDevice::GetId), what the SDKs hand back to choose it.
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Side {
    pub list: Vec<Endpoint>,
    /// Sound settings' default device's id.
    pub system: Option<String>,
    /// The default communications device's id.
    pub communications: Option<String>,
}

impl Side {
    pub fn get(&self, id: &str) -> Option<&Endpoint> {
        self.list.iter().find(|e| e.id == id)
    }

    /// The endpoint a choice comes to now, if Windows has it.
    pub fn resolve(&self, c: &Choice) -> Option<&Endpoint> {
        match c {
            Choice::Communications => self.communications.as_deref().and_then(|id| self.get(id)),
            Choice::System => self.system.as_deref().and_then(|id| self.get(id)),
            Choice::Device(id) => self.get(id),
        }
    }

    /// The endpoint used for a choice: the chosen one, or the communications default
    /// when it's gone (then the system default, then any).
    pub fn effective(&self, c: &Choice) -> Option<&Endpoint> {
        self.resolve(c)
            .or_else(|| self.resolve(&Choice::Communications))
            .or_else(|| self.resolve(&Choice::System))
            .or_else(|| self.list.first())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub input: Side,
    pub output: Side,
    /// Goes up when anything above changes.
    pub version: u64,
}

impl Snapshot {
    pub fn side(&self, output: bool) -> &Side {
        if output { &self.output } else { &self.input }
    }
}

/// Which device the game asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The default communications device (PeerParty's choice when the game makes none).
    Communications,
    /// Sound settings' default device.
    System,
    /// One endpoint, by id.
    Device(String),
}

static SNAP: LazyLock<Mutex<Arc<Snapshot>>> = LazyLock::new(Mutex::default);
static STARTED: Once = Once::new();
/// Snapshots taken so far (0: none yet).
static TAKEN: AtomicU64 = AtomicU64::new(0);
/// [input, output].
static CHOICE: LazyLock<Mutex<[Choice; 2]>> = LazyLock::new(|| Mutex::new([Choice::Communications, Choice::Communications]));

fn side(enumerator: usize, flow: u32) -> Side {
    let mut list = Vec::new();
    for dev in com::active_devices(enumerator, flow) {
        if let Some(id) = com::device_id(dev) {
            let name = com::device_name(dev).unwrap_or_else(|| id.clone());
            list.push(Endpoint { id, name });
        }
        release(dev);
    }
    let default = |role| {
        com::default_device(enumerator, flow, role).and_then(|d| {
            let id = com::device_id(d);
            release(d);
            id
        })
    };
    Side { list, system: default(CONSOLE), communications: default(COMMUNICATIONS) }
}

fn refresh() {
    let Some(e) = com::enumerator() else { return };
    let (input, output) = (side(e, CAPTURE), side(e, RENDER));
    release(e);
    let mut s = SNAP.lock().unwrap();
    if TAKEN.load(Ordering::Relaxed) == 0 || s.input != input || s.output != output {
        let version = s.version + 1;
        if TAKEN.load(Ordering::Relaxed) > 0 {
            log!(
                "voice: Windows' sound devices changed ({} microphones, {} speakers)",
                input.list.len(),
                output.list.len()
            );
        }
        *s = Arc::new(Snapshot { input, output, version });
    }
    TAKEN.fetch_add(1, Ordering::Relaxed);
}

/// Starts watching Windows' devices (the first look takes a moment).
pub fn start() {
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            loop {
                refresh();
                std::thread::sleep(REFRESH);
            }
        });
    });
}

/// The snapshot's version (0 before the first look), without waiting.
pub fn version() -> u64 {
    SNAP.lock().unwrap().version
}

/// The devices as last seen (waits briefly for the first look).
pub fn snapshot() -> Arc<Snapshot> {
    start();
    let until = Instant::now() + Duration::from_secs(2);
    while TAKEN.load(Ordering::Relaxed) == 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    SNAP.lock().unwrap().clone()
}

pub fn choose(output: bool, c: Choice) {
    let mut ch = CHOICE.lock().unwrap();
    if ch[output as usize] != c {
        log!("voice: {} set to {c:?}", if output { "speakers" } else { "microphone" });
        ch[output as usize] = c;
    }
}

pub fn chosen(output: bool) -> Choice {
    CHOICE.lock().unwrap()[output as usize].clone()
}

/// The endpoint voice uses now each way, by the game's choice.
pub fn effective(output: bool) -> Option<Endpoint> {
    let c = chosen(output);
    snapshot().side(output).effective(&c).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(id: &str) -> Endpoint {
        Endpoint { id: id.into(), name: format!("{id} name") }
    }

    #[test]
    fn a_choice_resolves_and_falls_back_when_its_device_is_gone() {
        let s = Side { list: vec![ep("a"), ep("b"), ep("c")], system: Some("a".into()), communications: Some("b".into()) };
        assert_eq!(s.effective(&Choice::Communications), Some(&ep("b")));
        assert_eq!(s.effective(&Choice::System), Some(&ep("a")));
        assert_eq!(s.effective(&Choice::Device("c".into())), Some(&ep("c")));
        assert_eq!(s.resolve(&Choice::Device("gone".into())), None);
        assert_eq!(s.effective(&Choice::Device("gone".into())), Some(&ep("b")));
        let none = Side { list: vec![ep("x")], system: None, communications: None };
        assert_eq!(none.effective(&Choice::Communications), Some(&ep("x")));
        assert_eq!(Side::default().effective(&Choice::System), None);
    }

    /// This PC's devices: every default is one of the listed endpoints, and each has a
    /// name (skipped without sound devices).
    #[test]
    fn windows_devices_are_listed_with_names_and_defaults() {
        let s = snapshot();
        for side in [&s.input, &s.output] {
            for id in side.system.iter().chain(side.communications.iter()) {
                assert!(side.get(id).is_some(), "default {id} isn't listed: {side:?}");
            }
            assert!(side.list.iter().all(|e| !e.name.is_empty() && !e.id.is_empty()));
        }
        eprintln!("{} microphones, {} speakers", s.input.list.len(), s.output.list.len());
    }
}
