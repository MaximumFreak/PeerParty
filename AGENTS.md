# Working on PeerParty

Notes for anyone changing PeerParty, people and coding agents alike. README.md says what
it does and docs/usage.md how to use it; this file is about how changes get in.

## Layout

- `core`: the LAN engine, with no game or SDK code. `mesh.rs` finds peers (UDP beacons,
  the same-PC peer board) and keeps one TCP connection per peer, with UDP for unreliable
  data; `lan.rs` is the engine games talk to (lobbies, presence, invites, channels);
  `wire.rs` the messages between copies; `fwd.rs` the typed-IP forwarder; `inet/` internet
  play (room codes, STUN, the BitTorrent DHT, hole punching, the encrypted tunnel, public
  sessions, port mapping).
- `hook`: the DLL injected into the game. `watch.rs` sees SDK modules load and hooks them;
  one module or folder per SDK (`steam*.rs`, `eos/`, `party.rs` and `party/`, `pfmp.rs`,
  `playfab/`, `galaxy*`, `photon/`, `ugs/`, `xboxlive/`); `padblock/` holds controllers
  still while the overlay shows; `present/` draws the overlay inside a game's frames;
  `ipfwd.rs` redirects typed-IP sockets; `*vt.rs` are generated vtable layouts.
- `launcher`: `peerparty.exe`. `main.rs` starts a game with the hook; `ui/` is the
  launcher window; `host/` runs beside the game and draws the overlay. `build.rs` builds
  the hook for x64 and x86 and embeds both.
- `tools`: generators for the vtable and EOS tables (`*_vtables.py`, `eos_table.py`). They
  read an SDK's headers from a folder you point them at; never commit those.
- `docs`: how it works (`design.md`), how to use it (`usage.md`), internet play
  (`internet-dht.md`, `public-sessions.md`), testing (`testing.md`).

## Build and check

- Windows with the MSVC Rust toolchain and `rustup target add i686-pc-windows-msvc`.
  `cargo test --workspace --locked` must pass with no new compiler warnings.
  `tests\run_all.ps1` also runs the two-copy SDK tests; those need SDK DLLs copied from
  games you have installed, named by `PPTEST_*` variables (docs/testing.md), and skip themselves
  without them. Never commit a game's or SDK's files, or paths to them.
- The repo isn't rustfmt-formatted. Don't run rustfmt over files; format only the lines
  you change, like the code around them.
- Socket tests bind 127.0.0.1 (`PEERPARTY_LOOPBACK=1`, set by .cargo/config.toml) and use
  a local test DHT. A test that needs the internet is `#[ignore]`d.

## Rules

- **Store checks stay with the store.** PeerParty replaces multiplayer services
  (identity, sign-in, lobbies, presence, invites, P2P) and passes licence, DLC,
  entitlement and purchase queries to the SDK the game loaded, or answers them empty or
  offline. A change that makes such a
  query report something as owned, or unlocks content, won't be taken. LEGAL.md lists
  what sign-in answers locally; keep it complete.
- **No anti-cheat games.** PeerParty stops a game in which EasyAntiCheat or BattlEye
  loads, or that starts an EOS Anti-Cheat session with the EOS layer on; keep it that way.
- **No piracy.** Nothing helps anyone obtain, unlock or run a copy they have no licence
  for, and issues and PRs asking for that are closed.
- **Don't name or recommend SDK replacements.** PeerParty doesn't provide one, and
  code, comments and docs don't name, link or recommend other implementations of an
  SDK. Describe the SDK's API and its documented behaviour.
- **Don't copy code you can't license.** Code taken from another project must be under a
  licence that allows it (MIT, BSD, Apache-2.0 and the like), keep that licence beside
  it, and be credited as the licence asks. Write PeerParty's own code yourself. Links to
  other projects are limited to ones checked in advance, and the release check refuses
  any other.
- **New dependencies need a permissive licence** (MIT, Apache-2.0, BSD, ISC, Zlib and the
  like). Anything copyleft, or with no licence, isn't taken; the release check refuses it.
- **No game names.** Code, comments, tests and docs describe SDK behaviour, not which
  game showed it ("a game that ...", not the title). Test fixtures use made-up names.
  Screenshots (`docs/images`) show made-up players, games and covers only: no real
  game, store art or logo.
- **Addresses stay hidden.** Internet peers' addresses never reach the game, the overlay,
  names or logs (`core/src/inet/redact.rs`; short tags unless `PEERPARTY_INET_DEBUG=1`).
  New code that logs or shows an address goes through `redact`.
- **Strangers are hostile.** Anything from a public-session player is untrusted until
  checked: bound its size, rate and count, and add it to the fuzz tests
  (`core/src/fuzz_tests.rs`, `core/tests/public.rs`).
- **Game files are never changed.** Hooks live in memory; PeerParty's settings and data
  go under `%LOCALAPPDATA%\peerparty`. Anything written elsewhere (save copies, firewall
  rules, the local certificate's key) must be listed under "What it changes on your PC"
  in LEGAL.md.
- A gap in an SDK layer fails fast with the SDK's own "no connection" result (PlayFab's
  web API: an empty success, which grants nothing; its store calls fail) and logs a `gap:` line, rather than
  hanging.

## Sources

Keep third-party licences next to ported code (`hook/src/photon/LICENSE-Luxon`). Never
contribute:

- code copied from closed-source SDKs or games, or from projects whose licence doesn't
  allow it;
- leaked or confidential material.

LEGAL.md says the same to users; keep the two in step. THIRD-PARTY-NOTICES.md, SECURITY.md
and LEGAL.md are part of what a release must carry.

## Pull requests

- One change per PR, without unrelated edits or reformatting.
- Comments explain why, not what. Match the naming and style of the surrounding code.
- Say what you tested: the automated tests, and for anything a test can't reach, the
  SDK and its version and the setup (two PCs, one PC with two copies, internet). Name
  the game in the PR if it helps review; it stays out of the code.
