# Internet play without servers: discovery over the BitTorrent DHT

Friends in different homes play as if they were on one LAN, with no server run by
PeerParty: copies that share a room code find each other through the public BitTorrent
mainline DHT, punch through both routers with UDP, and carry the LAN connection through
an encrypted tunnel. Public sessions (docs/public-sessions.md) use the same path.

## Room codes

Code: `core/src/inet/` (`code.rs`, `crypto.rs`, `stun.rs`, `dht.rs`, `stream.rs`,
`mod.rs`), wired into `core/src/lan.rs`; launcher flags and a Settings section. Off
unless a room code is set.

**How players use it.** One player makes a code (the launcher's Play together > Friends
online > Make a room, or `peerparty --new-room`): `PP1-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-CC`. They send it to
their friends privately. Everyone joins with the same code in Play together (or starts the game with
`peerparty --room CODE ...`, or sets `PEERPARTY_ROOM`). Games started that way find
each other over the internet within about 15-60 seconds and then behave as LAN players:
lobbies, invites, P2P and voice run over the tunnel. LAN play is unchanged and still
works at the same time. `peerparty --net-test` checks a PC's path (STUN, NAT type, the
DHT) with no friend needed.

**How it works.**

1. **Room code** (`code.rs`): 120 random bits in Crockford base32 with a 10-bit check.
   HKDF-SHA256 derives the handshake pre-shared key, a key that hides the DHT topic, and a
   short room id. Codes are always random, never typed passphrases: the topic is public,
   so a guessable secret could be brute-forced offline.
2. **Topic**: `SHA1("peerparty/1/room/" + topic key + game name + hour slot)`. It changes
   every hour, and without the code nobody can tell which room or game a topic is. The
   previous hour's topic is also looked up during the first 15 minutes of an hour.
3. **DHT** (`dht.rs`, our own small BEP 5 client, read-only per BEP 43): bootstraps from
   router.bittorrent.com, dht.transmissionbt.com, dht.libtorrent.org and
   router.utorrent.com, then from good nodes cached in
   `%LOCALAPPDATA%\peerparty\dht-nodes.txt`. It announces with `implied_port` (the NAT's
   outside port), looks up every 15 s for the first 2 minutes, then every 30 s, and every
   2 minutes once connected.
4. **STUN** (`stun.rs`, RFC 5389): Google's and Cloudflare's public STUN servers report
   our public address; two different answers mean a symmetric NAT. The DHT's BEP 42 `ip`
   field gives the same information without STUN: it's taken once two nodes at different
   addresses agree on the IP (not the port, which differs per node behind a symmetric
   NAT; the latest agreeing node's port is kept), so one lying node can't set it.
5. **One UDP socket** (port from `PEERPARTY_INET_PORT`, else any) carries STUN, the DHT
   and the tunnel, so the port announced on the DHT is the one friends punch to.
6. **Hole punching**: every address found (and every `PEERPARTY_INET_PEERS` entry) gets
   handshake packets: every 250 ms for 5 s, then once a second for up to 40 s. Both sides
   do this at once, which opens both NATs. Our own addresses are skipped. DHT finds keep
   old addresses for a while (a copy that quit, an earlier run), so not every find is
   treated alike: addresses that answered this run go first and are always
   punched, then `PEERPARTY_INET_PEERS` entries, then finds under this hour's topic, the
   newest first. At most three quarters of the session table (24 of 32 sessions in a room,
   12 of 16 public) punches addresses that haven't answered; a DHT find that hasn't answered
   holds its slot for 20 s (10 s on later tries), waits 30 s, then 60 s, and is
   dropped after 3 tries without an answer (not re-added from the DHT for
   30 minutes unless it reaches us itself). A handshake from an address arriving while the
   table is full replaces the oldest such punch, so a live peer is never turned away by
   stale ones; if nothing can make room, the log says so once per address. An inbound
   handshake that never finishes is logged ("our replies aren't reaching them": a one-way
   path), as is a peer that answered but didn't finish. A find at our
   own public IP is also tried at our own LAN address on the same port. That reaches a
   second copy on the same PC when the router doesn't hairpin; another PC on the LAN is
   found by LAN discovery anyway.
7. **Handshake and encryption** (`crypto.rs`): X25519 ephemeral keys with the room PSK
   mixed in (Noise NNpsk0 in spirit). The first packet carries an HMAC under a key from
   the code, so strangers and other rooms get no reply at all. Session keys come from
   HKDF over the DH result, salted with the PSK and bound to both sides' keys and
   nonces. Every packet after that is ChaCha20-Poly1305 with a replay window.
8. **Tunnel contents** (`stream.rs`, `mod.rs`): a reliable ordered stream (selective
   acks, RTO, fast retransmit with a reordering allowance, a congestion window) carries
   the existing mesh TCP connection byte for byte, and datagrams carry the mesh's UDP
   channel data. The mesh is not changed. The lower peer id's side opens a loopback
   listener that its mesh dials (`Mesh::dial_local`), the other side connects to its own
   mesh, and each gateway relays. Only the peer's Hello is rewritten (its UDP port
   becomes a local relay socket's). **The LAN wire protocol is unchanged**: as on the
   LAN, each side's hello carries its key and a fresh number, and is followed by its
   signature over both hellos' ids and numbers. A peer id is its key's hash, so a
   connection is a player's only once that key has signed for it, whichever way it came.
   A datagram is passed on only if it names the player the tunnel was proved to be.
9. **Keepalives**: one every 5 s. A peer silent for 30 s is dropped, a tunnel that ends
   says goodbye (CLOSE), and the DHT finds them again. A peer the LAN already reaches
   gets no tunnel.

**Design choices.** Everyone in a room announces and looks up the same topic, so both
sides punch at the same time with no rendezvous server. Public STUN servers are used as
well as the DHT's `ip` field. PeerParty has its own small DHT client and reliable stream,
to keep the 32-bit hook small and avoid an async runtime in game processes. There are no
relays (they add latency and someone has to run them), no IPv6 discovery (BEP 32) and no
BEP 44 items.

**IPv6 dual stack.** Discovery and the handshake stay on IPv4. A second UDP
socket on `[::]` (same port when free) carries tunnel data only. After the handshake
each side sends its usable global IPv6 addresses (2000::/3, not Teredo or documentation
ranges; at most 4) inside the encrypted tunnel every 60 s. The other side probes them for
20 s and moves the tunnel to IPv6 as soon as an authenticated packet arrives that way,
which skips both NATs. If IPv6 goes quiet for 12 s, or the address is no longer
advertised, it falls back to the IPv4 path, which is kept alive meanwhile. The overlay
says "N ms over IPv6"; addresses are never shown. Off with `PEERPARTY_IPV6=0`. Static
peers (`PEERPARTY_INET_PEERS`) are IPv4 only.

**TCP keepalive and send queues.** Every mesh TCP connection has keepalive
(idle 10 s, probes every 1 s: a vanished PC is noticed in about 20 s), and a peer's send
queue is capped at 64 MiB; a peer that stops reading is cut and reconnects.

**LAN first.** A peer found on both the LAN and through the internet uses
the LAN. If the LAN path appears later (a friend joins your Wi-Fi mid-game), the peer moves
to it without leaving and the tunnel is closed; if the LAN path goes, the tunnel takes the
peer back within about 5 s (as a leave and rejoin). A room path likewise replaces a public
session. LAN tries to a peer we only reach through a tunnel happen at most every 15 s.

**Same router, LAN discovery blocked.** Guest Wi-Fi and "client isolation" (AP isolation)
stop broadcast beacons and often TCP between PCs on the same router. Then PeerParty uses
the internet tunnel even though you're in one house, which works but goes out through the
router (NAT hairpinning has to work, which most routers do). If TCP between the PCs still
works and only broadcasts are blocked, put the other PC's address in `extra_targets`
(`PEERPARTY_PEERS=192.168.1.20`, comma-separated, on either PC): the beacon is then sent
straight to it and the LAN path is found as usual. The better fix is to put both PCs on the main network, not the guest one.

**Port mapping (`portmap.rs`).** At start, PeerParty asks the router to
forward its UDP port: PCP (RFC 6887), then NAT-PMP (RFC 6886) at the default gateway's
port 5351, then UPnP IGD (SSDP, then `AddPortMapping` on WANIPConnection or
WANPPPConnection). It renews at half the lease (2 h for PCP/NAT-PMP, 1 h for UPnP, with
a permanent lease when a router refuses timed ones), retries after 1 min doubling to
30 min when the router has none, and deletes the mapping when the room stops or the game
process exits (`RtlExitUserProcess`/`TerminateProcess` hooks; a crash leaves it to the
lease). When the router's external address is public, the DHT announce carries the
forwarded port (`implied_port` 0), so friends reach us even behind a router that would
change ports per destination. A router behind another NAT (CGNAT) gets a mapping that
isn't announced. UPnP only talks to the control URL on the router that answered SSDP,
and a port another PC already holds is left alone (we take a different external port).
Addresses aren't logged or shown: the status says e.g. "UPnP: external port 49294".
`peerparty --net-test` maps and deletes a test port. Off with `PEERPARTY_INET_PORTMAP=0`.

**Settings** (environment; the launcher sets `PEERPARTY_ROOM` from Settings or `--room`):

| Variable | Meaning |
|---|---|
| `PEERPARTY_ROOM` | The room code. Unset, empty, `0` or `off`: internet play off |
| `PEERPARTY_INET_PORT` | Fixed UDP port (to forward on the router); default any |
| `PEERPARTY_INET_PEERS` | Extra `host:port` addresses to punch (a forwarded port, a VPN address; IPv6 as `[addr]:port`), comma-separated |
| `PEERPARTY_INET_DHT=0` | No DHT; only `PEERPARTY_INET_PEERS` |
| `PEERPARTY_INET_PORTMAP=0` | Don't ask the router to forward the port (PCP / NAT-PMP / UPnP) |
| `PEERPARTY_IPV6=0` | Tunnel over IPv4 only (no direct IPv6 path) |
| `PEERPARTY_INET_BOOTSTRAP`, `PEERPARTY_INET_STUN` | Other DHT routers / STUN servers (`0`: none) |
| `PEERPARTY_LAN_DISCOVERY=0` | No LAN beacons (internet peers only; for testing) |

**Security limits.**
- Anyone with the code is in the room and is trusted the same way a LAN player is. Share
  codes privately, and make a new one if a code leaks. Each member signs its handshake
  with its identity key (as public players do), and its peer id must be that key's hash,
  so one member can't pass as another.
- Room members' PCs see each other's public IP address at the network level, but
  PeerParty doesn't show it: games get a stand-in address in 198.18.0.0/15, the overlay
  says "your room", and logs use short tags (`PEERPARTY_INET_DEBUG=1` for real
  addresses). DHT nodes that store the topic see
  that some IP announced some hash, but not which game or room.
- Some DHT nodes hand out junk addresses. We send them a few handshake packets that they
  can't answer. Sessions are capped at 32.
- The mesh protocol inside the tunnel still assumes friendly peers: room members are
  trusted. Public sessions (docs/public-sessions.md) added signed keys, a block list,
  handshake rate limits per address, caps and half-open timeouts for strangers; those
  limits apply to public sessions only.
- A symmetric NAT on both sides (CGNAT, some mobile networks) can't connect. There is no
  relay, so let the router map the port (UPnP / NAT-PMP / PCP, on by default), forward a
  UDP port (`PEERPARTY_INET_PORT`) or use a VPN address in `PEERPARTY_INET_PEERS`.
- Games that connect by IP address use the peer's stand-in (198.18.0.0/15, shown on the
  overlay's Network > Connection details); the hook redirects those socket calls through the tunnel
  (docs/public-sessions.md). `ConnectEx` and overlapped receives are covered too, however
  the game collects them.
