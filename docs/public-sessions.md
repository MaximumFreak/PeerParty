# Public sessions: finding anyone online who plays the same game through PeerParty

Off by default. Room codes (docs/internet-dht.md) are the way to play with friends
only.

## What it does

With public sessions on, PeerParty looks for other copies of **the same game** run through
PeerParty by anyone, anywhere, using the same serverless path as room codes (the public
BitTorrent DHT, UDP hole punching, an encrypted tunnel). Players it finds appear to the
game as LAN players, so their lobbies show in the game's own session browser or server
list and the game's search finds them, the same way LAN lobbies do. Nothing else changes
for the game.

Turn it on with any of:
- the launcher's Play together > Anyone online > **Public sessions** (also sets the name
  other players see);
- `peerparty --public game.exe` for one run (`--set public=off` turns the saved
  setting off for one run);
- `PEERPARTY_PUBLIC=1`, also for one run.

`PEERPARTY_PUBLIC_NAME` sets the name public players see. Without it, the name chosen in
Settings is used, else `Player-` and the start of your key's fingerprint. The Windows user
name is never used.

Blocking: in the overlay, open a public player's Details (Friends page) and choose
**Block this player**. From a prompt: `peerparty --blocked` lists blocked fingerprints,
`--block FINGERPRINT` adds one, `--unblock FINGERPRINT` (or `all`) removes. Settings shows
how many are blocked and has Unblock all.

## How it works

Code: `core/src/inet/public.rs` (keys, signed adverts, block list, key pins, rate
limiter, names), `core/src/inet/redact.rs` (address hiding), the public paths in
`core/src/inet/mod.rs` and `core/src/lan.rs`.

1. **Topic.** Public sessions are a "room" whose code everyone knows
   (`RoomCode::public()`), so the same topic rule gives one DHT topic per game name per
   hour: `SHA1("peerparty/1/room/" + public topic key + game + hour)`. Everyone running
   that game publicly announces and looks up that topic. The DHT values are only what BEP
   5 always stores: an IP and port under a 20-byte hash. No names, keys or game data go on
   the DHT. Public sessions use their own UDP socket (`PEERPARTY_PUBLIC_PORT`, else any)
   and node cache (`dht-nodes-public.txt`), apart from a room code's.
2. **Identity.** Each player has a long-term Ed25519 key per identity slot
   (`%LOCALAPPDATA%\peerparty\identity\slot-N.ed25519`). Its fingerprint is what you block,
   and the player's id (what games see as the user id) is 128 bits of the key's hash.
3. **Handshake.** The room handshake (X25519 ephemeral keys, ChaCha20-Poly1305, replay
   window) runs as for room codes, so session keys have forward secrecy. Because the
   public "room secret" is known to all, a public CONFIRM also carries a signed advert:
   the player's details (peer id, game, name), its public key, and an Ed25519 signature
   over both sides' fresh ephemeral keys and nonces plus those details (SIGMA-style). A
   CONFIRM without a valid advert, with altered details or key, or signed for another
   handshake, is refused, so an advert can't be forged, edited or replayed.
4. **An id is its key.** The id in an advert must be the hash of the key that signed it,
   or the handshake is refused: nobody can pass as another player, and nobody can get to
   a player's id before them. The mesh connection inside the tunnel asks for the same
   proof again (its hello is signed, see docs/internet-dht.md step 8), as a connection
   on the LAN does. `public-keys.txt` only remembers keys met before, for step 7 (4096
   in the file, 65536 more in memory; a full list keeps nobody out).
5. **Limited trust.** A public player is trusted less than a LAN or room player:
   - Lobbies, presence (capped at 32 keys and 4 KB), invites, game traffic and voice.
     Game traffic is every game channel, PlayFab Party's endpoint messages
     (`party:<guid>`) included: none of it is ever filtered for public players.
   - Their lobbies are bounded before the game sees them: at most 100 attributes (keys
     up to 256 bytes, text values up to 1000 bytes, no NaN or infinite numbers; larger
     ones are dropped), 64 members and 64 slots, ids, namespaces and buckets up to 256
     bytes (a lobby over these is dropped), invite connect strings up to 1 KB.
   - Voice and party chat (voice.rs's `rtc:` rooms: EOS RTC, EOS lobby voice, PlayFab
     Party / Xbox party chat at `rtc:party:<guid>`) are **off by default**. Turn them on
     in Play together > Anyone online > **Voice chat with strangers**, for one run with
     `--set public_voice=on`, or with `PEERPARTY_PUBLIC_VOICE=1` (the environment wins
     over the setting; `=0` forces it off). Off, voice to and from public players is
     dropped both ways (the log says so once per player and room); room and LAN players
     keep voice.
   - "Invite everyone on your network" never reaches them (it's only for players on
     your network), and their invites are never auto-accepted: they always wait for you
     to accept. They may send 5 at once, then one every 4 seconds.
   - Lobbies the game didn't make public (friends only, invite only, a party network
     behind a lobby) are closed to them: a search by id or by member doesn't show one,
     and asking to join gets "not allowed". Two things open them: you invite the player
     (it stands for 15 minutes, and ends if you kick them), or they are in a lobby with
     you already. The same goes for voice rooms: saying "I joined" isn't enough.
   - A join by lobby id asks every player, and only the lobby's owner should answer. A
     public player's answer waits a second and counts only if no LAN or room player
     answered meanwhile, so a stranger can't answer for a friend's lobby.
   - At most 256 channels each, with names up to 256 bytes.
   - The overlay shows "public player" and offers Block.
6. **Against floods and junk.** At most 16 public sessions, 4 per player key and 32 per
   internet address (several players behind one home router each have their own key), and
   at most 8 unfinished handshakes per address and 8 from strangers in all. A first
   handshake packet from an address we have no session with is answered only with a
   cookie (a MAC of its address, key and the time, no bigger than the packet), and nothing
   is kept: the sender gets a session by sending it back, which a made-up source address
   can't, so those take none of the 8. A per-address handshake rate limit (a
   burst of 32, then 8 per second), and at most 32 cookies a second in all. Inbound
   handshakes that don't finish within 10 s are dropped (and logged: our replies aren't
   reaching them). Stale DHT finds can't fill the 16 sessions: at most 12 punch addresses
   that haven't answered, a live player's handshake replaces the oldest such punch, and a
   find that never answers is dropped after 3 tries with growing waits (docs/internet-dht.md,
   step 6). 1 MB of early buffering per
   session. Tunnel segments bigger than PeerParty sends end the session. The block list
   and the file of known keys are capped. Malformed packets are dropped (the test
   sends 3000 junk packets, and 2400 sealed but malformed frames inside a live session).
   A blocked player's address is ignored for 10 minutes, and its key stays blocked for
   good.
7. **Coming back with a new key.** Making a new key is free, so a blocked player could
   return as someone "new". For 30 minutes after a block, a key we've never seen from
   that player's address is refused (known keys and friends from that address still get
   in; the cost is that someone sharing their router can't join with a brand-new key for
   that half hour). For full control there's **friends-only**:
   `PEERPARTY_PUBLIC_FRIENDS_ONLY=1` lets in only keys on your friends list
   (`%LOCALAPPDATA%\peerparty\public-friends.txt`), managed with `peerparty --friends`,
   `--friend FP` and `--unfriend FP|all` (FP: the fingerprint the overlay shows).
   Not done: proof-of-work on new keys. At a cost that wouldn't make honest players wait,
   it only slows a determined abuser to about one key per second per core, and it would
   change every player's id.
8. **Names.** Shown names are cleaned: invisible and control characters (including
   bidirectional overrides) are removed, whitespace is collapsed, at most 32 characters.
   A name that looks like an IP address becomes "Player".

## Keeping IP addresses out of sight

- **Games and lobby data:** an internet player (public or room) is reported to the game
  at a stable stand-in address in 198.18.0.0/15 (a range reserved for testing, derived
  from the peer id), never its real one. An EOS session's host address that a remote host
  set is replaced by the stand-in if it holds an address. Photon uses a LAN host if there
  is one, else an internet host at its stand-in.
- **Typed IPs:** the stand-in isn't a dead end. The hook redirects
  the game's `connect` / `WSAConnect` / `sendto` / `WSASendTo` / `recvfrom` /
  `WSARecvFrom` calls to a stand-in through the forwarder (core/src/fwd.rs,
  hook/src/ipfwd.rs), which carries the UDP and TCP traffic over the encrypted tunnel. The
  overlay's Network > Connection details shows the stand-in to type. A peer only delivers to ports its own
  game process has open, and never to PeerParty's own (its mesh, its tunnels, its stand-in
  web service: `core/src/own.rs`), which are open in that process too. A public player is
  also never delivered to a port the game keeps to the PC (a socket bound to 127.0.0.1 or
  `::1` only, such as a debug console); LAN and room players still are. Two players whose
  ids give the same stand-in are told apart by trust, then by who was here first, so a
  stranger can't take a friend's. A datagram a tunnel carries must name the player the
  tunnel was proved to be. `PEERPARTY_SKIP=ipfwd` turns it off.
- **Lobby data both ways:** IP addresses in public lobby and member data
  (IPv4 with a port, a private or CGNAT IPv4, any IPv6) are replaced by the sender's
  stand-in, on the way out and on the way in. That includes addresses in full-width
  digits, LAN addresses stuck to a word, and numbers holding an IPv4 address under a key
  that names one ("ip", "HostAddr"; either byte order). Ports, version numbers and
  stand-ins are kept.
- **Overlay and launcher:** the overlay's Friends page and profile show "public player"
  or "your room" for internet players, never an address. A LAN player's local address is
  still shown. The hook's peer list only fills "ip" for LAN players.
- **Logs and `--net-test`:** an internet address is written as a short tag
  (`peer-3fa9c1`). The tag is a keyed hash with a key made fresh each run, so it matches
  within one log but can't be reversed or compared across runs. `PEERPARTY_INET_DEBUG=1`
  logs real addresses for troubleshooting. This applies to room codes too.
- **Names:** never the Windows user name, never an address (see above).

**What this can't hide.** Anyone you connect to sees your public IP address at the network
level, because packets come from it. So do DHT nodes that store the topic, and, since the
public topic is worked out from the game's name alone, anyone who looks it up. A public player
could log it with their own tools. This is true of every peer-to-peer game. Hiding it
needs a relay, which PeerParty doesn't have. Don't turn public sessions on if that matters
to you. Use a VPN, or room codes with people you know.

## Limits and untested parts

- Games that connect by **typed IP address** reach internet players through the
  forwarder (above): EOS sessions' host address, Steam game-server IPs (server list and
  join by IP), Unity Relay joins and Photon all hand the game the peer's LAN address, else
  its stand-in. Checked with core tests over a public tunnel, not yet in a real game.
  `ConnectEx` and overlapped receives are covered too: a completion routine,
  a completion port, `(WSA)GetOverlappedResult`, a thread-pool IO object or
  `BindIoCompletionCallback` all see the peer's stand-in as the sender. A lobby member's address maps to that member's own stand-in, also
  when the lobby owner relays it.
- Other data the game puts in lobbies or presence (lobby names, player names, custom
  attributes) is passed through with addresses scrubbed (above), capped in size.
- A player who loses their key file (new PC, reinstall) is a new player: a new id, so
  whatever a game keyed by the old one (a save slot, "last host") doesn't follow.
- One topic per game name: everybody playing that game publicly ends up in one pool,
  capped at 16 sessions per player. There is no matchmaking, region choice or skill
  ranking beyond what the game's own browser does.
- Symmetric NAT on both sides still can't connect (no relay). Port mapping (UPnP,
  NAT-PMP, PCP) is used when the router offers it. IPv6 is used directly when both sides
  have a global address (see docs/internet-dht.md).
- A public player who is also on your LAN or in your room moves to that path without
  leaving and is trusted like a LAN or room player from then on.
- Per-address limits can't tell a household from one person with many keys: a single
  abuser can still open up to 32 sessions from one address (capped at 16 in total, and
  each key can be blocked).
- Checked by the automated tests (three public players on one PC through a local test
  DHT: discovery, lobbies, blocking, forged adverts, junk packets) and once on the real
  DHT: two public players on one PC under a random game name found each other in about
  18 s (`cargo test -p peerparty_core --test public real_dht -- --ignored` with
  `PEERPARTY_LOOPBACK=0`). Not yet tried between two homes, with strangers, or in a game.