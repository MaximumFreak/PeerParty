//! The first frame on a mesh connection is a Hello. Where a mesh connection runs over
//! something other than its own TCP connection (an internet tunnel, `inet::bridge`; a
//! connection through another player, `lan::relay`), the carrier rewrites that Hello on
//! the way through: the UDP port it names (a relay socket's, or none), and the name.
//! Everything after the Hello passes as it is. The proof that follows the Hello signs
//! neither the name nor the port.

use crate::id::PeerId;
use crate::wire::Msg;

/// The biggest first frame taken for a Hello.
const HELLO_MAX: usize = 64 * 1024;

/// What a rewrite may change in a Hello: the sender's id (to check), and its name and
/// UDP port (to change). An `Err` ends the connection.
pub(crate) type HelloEdit<'a> = dyn FnMut(PeerId, &mut String, &mut u16) -> Result<(), String> + 'a;

/// Rewrites the first frame a mesh sends (its Hello), however it arrives in pieces.
#[derive(Default)]
pub(crate) struct HelloRewriter {
    done: bool,
    buf: Vec<u8>,
}

impl HelloRewriter {
    /// Feeds bytes of the stream; returns what may pass on (nothing until the whole Hello
    /// is in, then the rewritten Hello and whatever followed it, then everything as is).
    pub(crate) fn edit(&mut self, data: &[u8], f: &mut HelloEdit) -> Result<Vec<u8>, String> {
        if self.done {
            return Ok(data.to_vec());
        }
        self.buf.extend_from_slice(data);
        let Some(len) = self.buf.get(..4).map(|l| u32::from_le_bytes(l.try_into().unwrap()) as usize) else {
            return Ok(Vec::new());
        };
        if len > HELLO_MAX {
            return Err("the peer's first message is too big for a hello".into());
        }
        if self.buf.len() < 4 + len {
            return Ok(Vec::new());
        }
        let Some(Msg::Hello { version, game, id, mut name, mut udp, key, nonce }) = Msg::decode(&self.buf[4..4 + len]) else {
            return Err("the first message isn't a hello".into());
        };
        f(id, &mut name, &mut udp)?;
        let hello = Msg::Hello { version, game, id, name, udp, key, nonce }.encode();
        let mut out = (hello.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&hello);
        out.extend_from_slice(&self.buf[4 + len..]);
        self.buf = Vec::new();
        self.done = true;
        Ok(out)
    }

    /// The far end's side of a connection through another player: its Hello's UDP port
    /// can't be reached through the relay, so it says none and unreliable data stays on
    /// the connection.
    pub(crate) fn no_udp(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        self.edit(data, &mut |_, _, udp| {
            *udp = 0;
            Ok(())
        })
    }
}

/// Learns a mesh's UDP port from the Hello it sends first (`None` until it is all in;
/// `Some(0)` when the first frame isn't a Hello).
pub(crate) fn sniff_hello(buf: &mut Vec<u8>, data: &[u8]) -> Option<u16> {
    buf.extend_from_slice(data);
    let len = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    match Msg::decode(buf.get(4..4 + len)?) {
        Some(Msg::Hello { udp, .. }) => Some(udp),
        _ => Some(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> Vec<u8> {
        let hello = Msg::Hello { version: 8, game: "g".into(), id: PeerId([1; 16]), name: "n".into(), udp: 5000, key: [2; 32], nonce: [3; 16] }.encode();
        let mut s = (hello.len() as u32).to_le_bytes().to_vec();
        s.extend_from_slice(&hello);
        s.extend_from_slice(b"after");
        s
    }

    #[test]
    fn the_hello_is_rewritten_in_pieces_and_the_rest_passes() {
        let s = stream();
        let mut f = HelloRewriter::default();
        let mut out = f.no_udp(&s[..3]).unwrap();
        assert!(out.is_empty());
        out.extend(f.no_udp(&s[3..10]).unwrap());
        out.extend(f.no_udp(&s[10..]).unwrap());
        out.extend(f.no_udp(b"more").unwrap());
        let len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        assert!(matches!(Msg::decode(&out[4..4 + len]), Some(Msg::Hello { udp: 0, .. })));
        assert_eq!(&out[4 + len..], b"aftermore");
        assert!(HelloRewriter::default().no_udp(&[5, 0, 0, 0, 21, 0, 0, 0, 0]).is_err(), "not a hello");
        assert!(HelloRewriter::default().no_udp(&[0, 0, 2, 0]).is_err(), "too big for a hello");
    }

    #[test]
    fn an_edit_sees_the_sender_and_changes_name_and_port() {
        let mut f = HelloRewriter::default();
        let out = f
            .edit(&stream(), &mut |id, name, udp| {
                assert_eq!(id, PeerId([1; 16]));
                *name = "renamed".into();
                *udp = 7;
                Ok(())
            })
            .unwrap();
        let len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        match Msg::decode(&out[4..4 + len]) {
            Some(Msg::Hello { name, udp, .. }) => assert_eq!((name.as_str(), udp), ("renamed", 7)),
            m => panic!("{m:?}"),
        }
        assert!(HelloRewriter::default().edit(&stream(), &mut |_, _, _| Err("refused".into())).is_err());
        let mut buf = Vec::new();
        assert_eq!(sniff_hello(&mut buf, &stream()[..6]), None);
        assert_eq!(sniff_hello(&mut buf, &stream()[6..]), Some(5000));
    }
}
