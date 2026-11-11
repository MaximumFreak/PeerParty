# PeerParty design

How PeerParty works: one LAN layer under the multiplayer SDKs a game already ships.
docs/usage.md covers using it; this file covers the architecture, each SDK layer, and
what is and isn't carried.

## The idea

A game is built against the API of a middleware SDK: `steam_api64.dll`,
`EOSSDK-Win64-Shipping.dll`, PlayFab's Party and multiplayer DLLs, `Galaxy64.dll`,
Photon's or Unity's client libraries. PeerParty starts the game with a small hook inside
it, waits for each known SDK module to load, and takes over that SDK's **multiplayer
calls only**:

1. **Multiplayer primitives** (identity, lobbies and sessions, presence and invites,
   P2P and sockets, voice, sign-in tokens) are answered by PeerParty's shared LAN core,
   between the copies of the game on the network.
2. **Everything else** (licence and DLC checks, entitlements, Steam's stats and
   achievements, Steam Input, the store's overlay) is **passed through** to the SDK the
   game loaded, unchanged, or fails as offline where the SDK would need the store's
   servers (EOS Ecom, and PlayFab's purchase, receipt, currency, grant, trade and
   inventory-change calls, and its Admin API); PlayFab's web API answers the rest of what
   it doesn't implement with an empty success.
   In the answers that do go to the store's SDK, the one thing PeerParty
   touches is a player's own SteamID:
   where the game is shown a SteamID of its own for this PC, results that carry the real
   one (such as `ISteamApps::GetAppOwner`) are translated to it, so the game sees one
   consistent player. What the call concludes isn't changed.
3. **Online-only calls that would hang offline** fail fast with the SDK's own
   "no connection" result. Games already have an offline path for that result, and an
   instant failure beats a 30-second timeout. The first such call of each kind is
   logged as a `gap:` line.

Licence, DLC, entitlement and purchase queries go to the store's SDK or come back empty
or offline,
and PeerParty never changes game files. Sign-in, including the sign-in players do with
each other (Steam's auth tickets, a game server approving a player), is answered between
the copies (LEGAL.md).

```
game.exe
 ├─ steam_api64.dll / EOSSDK / PlayFab Party, Multiplayer, Core / Galaxy64   ◄─┐ hooks installed
 ├─ Photon and Unity Gaming Services hosts (host lookups, HTTPS)              ◄─┤ as each module
 ├─ ws2_32 / dnsapi host lookups                                              ◄─┘ loads
 │
 └─ peerparty_hook.dll  (loaded first by peerparty.exe)
      ├─ front ends:  Steam · EOS · PlayFab · Galaxy · Photon · UGS · Xbox Live
      └─ LAN core:    identity · discovery · mesh · lobbies · presence · invites · channels · voice
                         │
                         └── TCP + UDP on the LAN, or an encrypted tunnel over the internet
```

## Starting a game

- `peerparty.exe <game.exe> [args]`, a Steam launch option (`"…\peerparty.exe"
  %command%`) or the launcher window. peerparty.exe carries the x64 and x86 hook DLLs and
  writes the one a game needs under `%LOCALAPPDATA%\peerparty\hooks\` on first use.
- The game is created suspended. A 64-bit game gets the hook through an early APC; a
  32-bit (WOW64) game is held at its entry point and the hook loaded there. Either way
  the hook loads before the game's own code runs, and nothing on disk or in the registry
  changes.
- **Microsoft Store (GDK) games** are packages Windows starts itself. PeerParty turns on
  package debugging for one launch (`IPackageDebugSettings::EnableDebugging` with
  `peerparty.exe --attach`), activates the app, and turns it off again; Windows hands
  PeerParty the suspended process, which it injects and resumes. Children a packaged game
  starts from its own package are followed the same way (`hook/src/child.rs`), never
  crash reporters.
- **Programs a game starts** (`hook/src/child.rs`, `launcher/src/gameroot.rs`,
  `launcher/src/tree.rs`): the launcher works out the game's install folder (Steam
  library, Epic's manifests, the window's game folders, Unreal's layout, or the
  program's folder climbing out of build-named folders; never a drive's root, Windows,
  Program Files, the user's folder or a library's root) and passes it as
  `PEERPARTY_GAME_ROOT`. The hook's `CreateProcessInternalW` detour follows a child
  started from inside it, unless it is an anti-cheat starter, an installer of
  redistributables, a crash reporter, or the parent has loaded a game SDK (then the
  parent is the game). A followed child connects to the overlay's pipe only once it
  turns out to be the game (a game SDK loads, a window of 640x360 or more, or its
  parent ends): it signals `Local\peerparty-follow-<pid>`, which the parent made, and
  the parent lets the pipe go. The launcher puts the game in a job object (breakaway
  allowed, nothing killed on close) and waits for the last of its programs inside the
  folder, reporting that one's exit code.
- **.bat / .cmd wrappers** (`launcher/src/wrapper.rs`): read, not run; the one program
  line they start is the game, with the script's folder, variables and arguments.
- **Epic's restart check**: `EOS_Platform_CheckForLauncherAndRestart` answers
  `EOS_NoChange` in a game PeerParty started, in every mode, so the game doesn't quit
  to restart through Epic's launcher.
- peerparty.exe stays running beside the game: it hands back the game's exit code and
  hosts the overlay.
- A game in which EasyAntiCheat or BattlEye loads is stopped as soon as that module
  loads, and, with the EOS layer on, one that starts an EOS Anti-Cheat session is stopped
  then.

## Hooking

- **Module watch** (`hook/src/watch.rs`): modules already mapped are walked from the
  PEB; later loads arrive through `LdrRegisterDllNotification`, before `LoadLibrary`
  returns to the game. A module is identified by its export names, not its file name
  (`hook/src/sdk.rs`).
- **Swapping by name** (`hook/src/swap.rs`): for each SDK, `GetProcAddress` on its
  module returns PeerParty's function (this covers .NET and IL2CPP P/Invoke, Mono and
  manual lookups), and the game modules' static imports of it are rewritten. By default
  the `GetProcAddress` slots of the game's own modules' import tables are pointed at
  PeerParty, so Windows' own code is never patched (`PEERPARTY_SWAP=inline` hooks
  kernelbase instead).
- **Inline detours** (the vendored `retour` crate) for the few entry points that have to
  run PeerParty's code around the SDK's own: platform creation and ticks, callback
  dispatch, host lookups.
- **Packed modules** (an SDK DLL mapped with its code still compressed, its exports'
  first bytes all zero) unpack themselves in their `DllMain`. PeerParty doesn't unpack
  or decrypt anything: it waits until the module's own start-up has run, then hooks it
  like any other.
- **C++ interfaces** (Steam's, Galaxy's) are objects with vtables. PeerParty hands the
  game stand-in objects whose vtable runs PeerParty's code for the multiplayer methods
  and, for every other slot, a short stub that swaps in the real object and jumps to the
  real method, so the store's own code keeps answering everything else whatever the
  signature. Slot layouts per interface version are generated from the SDKs'
  interface headers (`tools/steam_vtables.py`, `tools/galaxy_vtables.py`) and carry
  method names and slot order only, no header text; a version without a known layout is
  handed out unwrapped.
- **Host lookups** (`hook/src/dns.rs`): `getaddrinfo`, `GetAddrInfoW/ExW` and
  `DnsQuery` are hooked in the game. Hosts PeerParty stands in for resolve to a loopback
  address of the game's own process (127.80.x.y) or to the LAN copy that hosts a
  service; everything else resolves normally.

## The LAN core (`core/`)

Every front end maps onto the same primitives, so the networking is written once and
has no game or SDK code in it.

| Primitive | Steam | EOS | PlayFab | Galaxy | Photon / UGS |
|---|---|---|---|---|---|
| Identity (name, stable id → per-SDK ids) | SteamID | ProductUserId / EpicAccountId | entity key | GalaxyID | user id / player id |
| Lobby (owner-authoritative attributes, members, member data, search) | lobbies, game servers | lobbies, sessions | Party networks, lobbies, tickets | lobbies | rooms, UGS lobbies |
| Presence and invites | friends, rich presence | friends, presence, invites | invitations | friends, rich presence | — |
| Channels (reliable ordered, reliable unordered, unreliable) | P2P, networking sockets | P2P | endpoints | P2P | relay traffic |
| Voice rooms | — | RTC rooms | Party chat | — | — |

- **Identity.** Each copy takes an identity slot in `%LOCALAPPDATA%\peerparty\identity`,
  so two copies on one PC are two players. A slot is an Ed25519 key, and the player's id
  is 128 bits of its hash (`core/src/id.rs`): every way of connecting (the mesh's hello
  on the LAN and inside a tunnel, a room's and a public session's handshake) has the key
  sign for the connection, so an id can't be used by anyone but its player. The first copy of a Steam game is shown a
  SteamID of its own for this PC unless Valve's Steam client runs it (saves keyed by
  SteamID start, when the new id has none, from a copy of the save of an earlier
  PeerParty id kept in `slot-N.previous`, else the newest neighbouring PeerParty save,
  else steam_api's; `hook/src/saves.rs`); a second copy gets a LAN one, and starts from
  player 1's save only with `--copy-saves`, which replaces its own save and keeps it as
  `.peerparty-before`.
- **Discovery** (`core/src/mesh.rs`): UDP broadcast beacons carrying the game key (the
  exe's name, or `PEERPARTY_GAME`), plus a peer board under `%LOCALAPPDATA%\peerparty\
  peers` so copies on one PC find each other with no network at all. Extra beacon
  targets (`PEERPARTY_PEERS`) cover VPNs and networks broadcast doesn't cross.
- **Mesh.** One TCP connection per peer for reliable data, with keepalive and a capped
  send queue, plus a UDP socket for unreliable data once the peer answers a UDP probe.
- **Lobbies** (`core/src/lobby.rs`, `core/src/lan.rs`): owned by their creator's copy,
  changes sent as snapshots, the owner handed over when it leaves. Searches run on every
  peer and complete when each has answered or after a timeout.
- **Presence and invites:** each copy publishes key/value presence. An invite is a
  connect string and an optional lobby; it waits for the player in the overlay unless
  auto-accept is on, or the overlay is off (nothing could answer it there; a public
  player's still waits), and is then delivered through whichever SDK event the game
  listens for.
- **Wire protocol** (`core/src/wire.rs`): versioned messages; anything from an untrusted
  peer is size-, rate- and count-bounded before use.

## Front ends

### Steamworks (`hook/src/steam*.rs`)

- In a game started through PeerParty, `SteamInternal_CreateInterface`, `SteamInternal_FindOrCreateUserInterface`,
  `SteamInternal_FindOrCreateGameServerInterface` and the old `SteamFriends()`-style
  accessors hand out stand-ins, as do the interfaces `ISteamClient` returns.
- The flat API (Steamworks.NET, Facepunch and other wrappers) is swapped by name. Its
  accessors (`SteamAPI_SteamUser_v023` and the rest) hand out the stand-ins too, and
  each flat `SteamAPI_ISteam*` function calls the stand-in's own method, as Valve's flat
  functions do (in 32-bit games through a trampoline from the flat function's cdecl to
  the method's thiscall); the few that return a SteamID get small adapters, since the C++
  method returns it through a hidden pointer and the flat function as a number.
- Carried: ISteamMatchmaking (lobbies with every comparison Steam's filters have, data,
  member data, chat, owner changes), ISteamNetworking (legacy P2P), ISteamNetworkingSockets,
  ISteamNetworkingMessages and the relay / ping parts of ISteamNetworkingUtils, ISteamFriends
  (LAN players as friends, rich presence, invites and Join), ISteamUser's SteamID, logged-on
  state and auth tickets between LAN players, game servers (ISteamGameServer, its own
  networking and auth) and the server browser (ISteamMatchmakingServers; its secure
  (VAC), app id and white / black list filters don't apply to LAN servers and are
  ignored).
- Callbacks and call results reach the game through its own dispatch: registered
  `CCallbackBase` / `CCallResult` objects after `SteamAPI_RunCallbacks`, or the
  ManualDispatch queue. PeerParty's `SteamAPICall_t` handles come from a reserved range.
- Not carried: anything that needs Valve's relay network (Valve-hosted dedicated
  servers, FakeIP).
- **A SteamID of its own for each PC** (`steamlan.rs` `my_steam_id`, `steamown.rs`,
  `saves.rs`). A steam_api that gives every player the same SteamID (one account set up
  the same way on every PC) would have each game take the other player for itself (no
  lobby owner to join, packets looped back), and games also use their own SteamID or
  account id in ways PeerParty can't see (seating players by it). So the first copy's
  game is shown PeerParty's LAN SteamID for this PC (instance 1, account id above every real one, from its LAN
  identity with the top bit set, so never a real account's) through every way it can
  ask (`GetSteamID` on the C++ interface, the flat API and the old accessors, lobby
  members, P2P identities): every PC's game sees a different SteamID and
  account id. `PEERPARTY_MODE_STEAMID` (the launcher's "Steam player ID", global or per
  game): `auto` (default) does this unless Valve's steamclient (one with a steam.exe
  beside it) is in the game, `lan` always, `official` never.
  - **One SteamID for the game** (`steamown.rs`). steam_api still knows the player by
    its own SteamID, so what crosses between them is mapped: SteamIDs PeerParty passes
    on (the friends functions) and the flat functions that take one (`ISteamUserStats`'
    per-user stats, achievements and leaderboard entries for users, `ISteamUGC`'s
    per-user query) get steam_api's for ours; every callback and call result steam_api
    delivers, through `ManualDispatch_GetNextCallback`, `GetAPICallResult` or classic
    dispatch, has steam_api's SteamID swapped for ours wherever its 8 bytes appear, as
    do `ISteamApps::GetAppOwner` and a downloaded leaderboard entry's user. Classic
    dispatch calls the game's `CCallbackBase` objects directly, so each registered one
    gets a copy of its class's vtable whose two `Run` slots swap first (the RTTI pointer
    and every other slot kept). Not covered: a native game calling `ISteamUserStats`,
    `ISteamApps` or `ISteamUGC` through their C++ interfaces with its own SteamID (no
    vtable layouts for them; a game's own stats come from `RequestCurrentStats`, which
    takes no id). `PEERPARTY_SKIP=steamown` turns the mapping off.
  - **Saves** (`saves.rs`). Saves a game keys by SteamID are new ones under the new id.
    The first time the game touches a path holding the new SteamID, account id or hex
    SteamID as a path component or delimited file name token, the same path under
    steam_api's id is copied to it, once ever (only while nothing exists under the new
    name), through a temporary name so a copy cut short never passes for a finished one;
    over 1 GB it isn't copied. The original is only read: playing without PeerParty
    carries on from it, and a game that rejects the copy (a save naming its owner inside)
    can only lose the copy. The file hooks (kernelbase's `CreateFileW/A`, `CreateFile2`,
    `GetFileAttributes(Ex)W/A`, `FindFirstFileExW/A`, `CreateDirectoryW/A`) only look at
    the path, and are armed only once a game is shown a SteamID of its own.
    `PEERPARTY_STEAM_SAVES=0` (the launcher's "Start from the game's existing saves" off)
    leaves the new saves empty. An id fused into a longer token (`save12345.dat`) or one
    in the registry isn't matched: that game starts afresh. Device paths (`\\.\...`, a
    named pipe), the temp folder and Steam's own folder for an account are never copied;
    under `userdata\<account>` only the game's own `<app id>` folder is.
- **Clashing SteamIDs** (`hook/src/steamids.rs`).
  With the game's SteamID kept (`official`, a game Steam runs, or a player on an older
  PeerParty), players who share one are told apart by how they see *each other*:
  - Each copy publishes the SteamID its game uses (`peerparty:steamid_game`) and the one
    it goes by on the network (`peerparty:steamid`). A copy that finds another player
    whose game uses its SteamID goes by its LAN SteamID (instance 1, account above every real one, from its LAN id,
    never a real account's) on the network from then on, logs a `SteamID clash` line and
    republishes. Both sides do this, so a third copy shows each of them under its own
    LAN SteamID; real, non-clashing SteamIDs pass through unchanged.
  - A player whose game uses our SteamID is shown to our game under an ordered SteamID:
    ours with an account id below ours if their LAN id sorts before ours, above it
    otherwise (account ids stay >= 0x100 where there's room). Every copy then has the
    players in one order of SteamID and account id (with LAN SteamIDs, accounts >=
    0x80000000, each game came first on its own screen). Each copy shows the others
    its own way, so packets carry network SteamIDs: what our game shows a player as goes
    out as the SteamID they go by, and a player's network SteamID comes in as what we
    show them as.
  - Players told under our SteamID, or two players told under one SteamID (copies from
    before this, which only publish `peerparty:steamid`), are shown here under their LAN
    SteamIDs. Shown SteamIDs are sticky, since the game may have kept them.
  - Games put SteamIDs in their own packets (host ids, player lists). While a clash is
    on, the 8-byte little-endian SteamID and its 17-digit decimal form are swapped in
    place, same size, in ISteamNetworking, ISteamNetworkingSockets/Messages and lobby
    chat messages, and in lobby data, member data and rich presence read from a player:
    a clashing sender's own SteamID becomes the one it's shown as, and our network
    SteamID becomes our game's own. Copies from before this get the reverse on the way
    out. Without a clash nothing is touched. SteamIDs are swapped as 8 bytes either way
    round (games with their own stream code often write big-endian) and as decimal text
    in UTF-8 or UTF-16LE (C# strings). In packets (not text) the 32-bit account id, a
    SteamID's low half, is swapped too as 4 bytes either way round: games with their own
    netcode may name players by it, but only with `PEERPARTY_STEAM_ACCOUNTS=1`: a
    shared account id is usually small and as 4 bytes turns up in ordinary game data,
    which swapping would corrupt (account ids under 0x100 are never swapped). Not swapped: account ids in text, SteamIDs in other encodings
    (varints, hex text), and lobby data an owner writes for older copies (it's shared,
    so it can't be swapped per reader). `PEERPARTY_STEAM_DUMP=1` logs game packets,
    lobby chat and lobby data in hex with every SteamID or account id found in them and
    its encoding, and what `GetLobbyMemberByIndex`/`GetLobbyOwner` answered (on change),
    to see how a game that still mixes players up names or orders them.
  - The other services mint per-player ids from the LAN id (EOS ProductUserIds, PlayFab
    entities, UGS player ids, Galaxy ids), so they can't clash unless a PeerParty
    identity folder is cloned between PCs. Xbox Live lists a player under a stand-in XUID
    when they publish ours or one another LAN player also publishes (two copies on one
    Xbox account), and logs an `XUID clash` line.

### Epic Online Services (`hook/src/eos/`)

- EOS is a flat C API with versioned option structs. Option structs are read field by
  field behind `ApiVersion` cutoffs, so any SDK version works; output structs are written
  at the newest layout and read as a prefix by older games. x86 works too.
- The game's EOS SDK stays loaded for the platform, logging and helpers; the multiplayer
  functions are swapped by name. Completions are queued and fired from the game's
  `EOS_Platform_Tick`.
- Carried: Connect and Auth sign-in with a LAN identity, UserInfo, Friends, Lobby (with
  search, details, modification and the lobby's RTC room), Sessions, Presence, P2P, UI join
  events, custom invites and request to join, RTC voice and data, Title and Player Data
  Storage on disk, Xbox Live external-account mapping for GDK games.
- Every other function that could see a PeerParty id fails fast by its shape (async:
  `EOS_NoConnection` on the next tick; notify: an id that never fires; copy:
  `EOS_NotFound`; counts: 0). `tools/eos_table.py` classifies every export from the SDK
  headers into `hook/src/eos/table.rs`. Ecom (ownership), Achievements, Stats and
  Leaderboards are deliberately left that way.

### PlayFab (`hook/src/party.rs`, `party/`, `pfmp.rs`, `playfab/`)

- **Party:** every `Party*` export of the game's Party DLL is PeerParty's; the real
  library never starts. A network is a LAN lobby, endpoints are its members, messages
  ride LAN channels, and state changes are handed out through
  `PartyStartProcessingStateChanges` in the documented order. Chat controls, text chat
  and voice chat ride the network's voice room, with Party's permission rules.
  `PartyXboxLive.dll` (GDK builds) is answered on the LAN too.
- **Lobby & Matchmaking:** every `PF*` export of `PlayFabMultiplayerWin.dll` is
  PeerParty's. Lobbies are LAN lobbies with PlayFab's filter and sort strings evaluated
  locally; matchmaking tickets pair the mutually oldest compatible tickets.
- **Web API:** lookups of `<title>.playfabapi.com` resolve to the game process's own
  loopback address, where PeerParty answers HTTPS with PlayFab's JSON shapes: sign-in,
  entity tokens, profiles, statistics and user data kept on disk, lobbies, matchmaking,
  and empty title data, CloudScript and catalog answers. PlayFab's C SDK (PlayFabCore)
  uses the same service through its own HTTP stack.

### GOG Galaxy (`hook/src/galaxy.rs`, `galaxy/`)

- `galaxy::api::User()`, `Friends()`, `Matchmaking()`, `Networking()` and the listener
  registrar hand out stand-ins; `ProcessData()` runs PeerParty's listener calls after
  the real one. The DLL's file version picks the vtable layout (every SDK from 1.57 on),
  checked against the DLL's own vtables through MSVC RTTI.
- Carried: sign-in, user data, friends, persona names, rich presence, invites, lobbies,
  P2P, chat, stats and achievements on disk, storage and cloud saves on disk, tokens.
  IApps (DLC) stays with Galaxy. Game servers aren't carried.

### Photon (`hook/src/photon/`)

- Photon Cloud's hosts resolve to one copy on the LAN, which runs Photon's name, master
  and game servers inside its game process (UDP, TCP and WebSocket). The first copy to
  look Photon up hosts unless a LAN peer already does.
- The protocol layer (ENet, GpBinary 1.6 and 1.8, the key exchange and payload
  encryption, the server handlers) is ported from Luxon and Luxon Server (BSD-3-Clause,
  `hook/src/photon/LICENSE-Luxon`). On top: Photon Chat, SQL lobby filters, inactive
  players and rejoin, Bolt's relay events, and stand-ins for Fusion's room plugin in
  Host, Server, Client and Shared modes.
- Not carried: secure WebSockets, datagram encryption, server plugins and webhooks.
  The local server issues its own session tokens; a title's custom-authentication
  webhook isn't called, and every client is accepted.

### Unity Gaming Services (`hook/src/ugs/`)

- For games that ship Unity's Lobby, Relay or Multiplayer SDK: Authentication, Lobby
  (with Wire events over a WebSocket), Relay, Cloud Save and Remote Config are answered by
  the same local HTTPS service. A relay allocation makes the host's copy the relay, which
  speaks Unity Transport's relay protocol, plain or inside DTLS with a pre-shared key.
- QoS, Matchmaker and Vivox are left alone.

### Xbox Live (`hook/src/xboxlive/`, `xgameui.rs`)

- For Microsoft Store games: the social, profile, presence and privacy hosts are answered
  from the LAN, so a game's friends screen lists LAN players, and the Xbox invite screens
  open PeerParty's overlay. Everything else on Xbox Live (achievements, multiplayer
  session directory, title storage) resolves normally.

## Trust for the local HTTPS service

The local service (`hook/src/playfab/http.rs`, `tls.rs`) uses a self-signed certificate
made with Windows' own crypto. It is trusted **inside the game process only**:

- crypt32's `CertGetCertificateChain` is hooked in the game, and only for that
  certificate's exact bytes returns a chain whose root is the certificate;
- for clients that read the ROOT store and verify themselves (UnityTls on MbedTLS), the
  ROOT store the game opens comes back as a collection with PeerParty's certificate
  added in memory;
- GDK games' own pin check (`XNetworkingVerifyServerCertificate`, `hook/src/xgameui.rs`)
  passes for that certificate only.

Every other certificate chain, and every other server's pin check, goes to Windows and
the runtime unchanged. Nothing is written to any Windows certificate store, and no other
process is affected.

## Official servers per service (`hook/src/modes.rs`)

`PEERPARTY_MODE_<SDK>` = `lan`, `official` or `auto` for Photon, UGS, EOS, PlayFab, Steam
and Galaxy. Official leaves that layer out entirely, so the game reaches the real service.
Auto checks once per run whether the service answers (or, for Steam and Galaxy, whether
the client runs) and picks official if so, else LAN. Every service is LAN until the player
changes it (the launcher's defaults, `launcher/src/ui/store.rs` `SDK_MODES`): with auto,
the same game could end up on either side from one run to the next, and its rooms on
official servers are out of the overlay's invites.

## Internet play (`core/src/inet/`)

Room codes and public sessions carry the same mesh over the internet: discovery on the
public BitTorrent DHT, UDP hole punching, router port mapping, and an encrypted tunnel.
An internet peer is one more peer in the mesh, so every front end works unchanged.
Internet peers' addresses never reach the game, the overlay, names or logs; games see
stand-in addresses in 198.18.0.0/15, and typed-IP traffic to those is forwarded through
the tunnel (`core/src/fwd.rs`, `hook/src/ipfwd.rs`). docs/internet-dht.md and
docs/public-sessions.md have the details.

When a peer is reachable more than one way, the best path wins (LAN, then room, then
public). A peer that appears on the LAN mid-game moves to it without leaving: the old
connection is drained before the new one delivers, so nothing is lost or reordered.

## The overlay (`launcher/src/host/`, `hook/src/present/`, `hook/src/padblock/`)

- peerparty.exe draws the overlay: egui rendered by a small software rasterizer into a
  canvas, shown in a layered topmost window that never takes focus, or handed to the
  game through shared memory and drawn inside its frames (Direct3D 9, 10/11, 12 through
  D3D11On12, OpenGL, Vulkan) for exclusive fullscreen. The game process does no UI work.
- Hook and launcher talk over a pipe, one JSON line per message (state, open / close,
  invite, accept, join, settings).
- While it shows, keyboard and mouse are taken by low-level hooks in peerparty.exe, and
  controllers are held still inside the game (XInput, DirectInput, raw-input HID,
  Windows.Gaming.Input, GameInput, direct HID reads). peerparty.exe reads the
  controllers itself. Closed with a button down, it says so (`pads_held`), and the
  game's controllers stay held until peerparty.exe reads the buttons up (at most 1 s;
  the hook lets go after 2 s by itself).
- The game's own overlay calls (Steam's `ActivateGameOverlay*`, `EOS_UI_*`, Galaxy's
  invite dialog, the Xbox invite screens) open the matching overlay page, and games hear
  it open and close as their store's overlay.

## The launcher (`launcher/src/ui/`)

An egui window shown when peerparty.exe starts with no arguments. It lists Steam, Epic,
GOG and Xbox app installs and the player's own game folders, and starts games through
peerparty.exe with per-game options. Settings live in `%LOCALAPPDATA%\peerparty\ui.json`
and apply however a game is started.

- Three places: the library (`library.rs`, a game's panel in `game.rs`), Play together
  (`together.rs`: who can join, for every game) and Settings (`settings.rs`); a
  first-run setup over the library (`welcome.rs`) writes its answers only at the end.
- The look (`theme.rs`, `widgets.rs`, `fx.rs`, `icons.rs`) is shared with the overlay.
  Light is plain coloured meshes, so it costs the same on the GPU window and in the
  overlay's software canvas: a two-Gaussian bloom round shapes, radial lights, a
  spotlight that follows the pointer inside a card (a fan from the pointer to the card's
  outline, so it keeps the rounded shape), a rim that catches it, exact gradients on
  rounded rectangles, and an electric arc when Play is pressed. Nothing animates on its
  own: transitions run only while they move, and effects are off with Windows'
  animation effects.
- Fonts are the system's (Segoe UI, its semibold, Segoe Fluent Icons or MDL2 Assets);
  another script's font (CJK, Thai, Indic, emoji) is read the first time a character
  needs it, through egui's font provider. egui's own fonts stay in the program behind
  those, for glyphs the system fonts lack (THIRD-PARTY-NOTICES.md).
- Keyboard and controller reach every control: egui's focus moves with the arrow keys
  and Tab and presses with Enter, and the controller is turned into those keys
  (`raw_input_hook`), except on the game grid, which moves its selection itself.
- Cost while a game runs: no frame unless something changed (the controller thread in
  `pads.rs` wakes the window only on a change, a thread per game waits for it to end),
  the window minimizes when a game starts, and with a game running and the window
  behind it the process is in background mode (`PROCESS_MODE_BACKGROUND_BEGIN`; games
  start at normal priority regardless).
- Words come from `launcher/locales/*.txt` (`i18n.rs`, the `t!` macro): the system's
  display language unless one is chosen, English for any missing line, CLDR plural
  forms. A test checks every key the code uses is in English and every translation's
  `{values}` match.
- What the window asks of the operating system (the registry, icons, Explorer, the
  foreground window, languages, fonts, the animation setting, process priority) goes
  through `launcher/src/platform/`: `windows.rs`, and `other.rs`, a stand-in that
  compiles anywhere, as the place a port starts. The game side (injection, the hook,
  the firewall, the overlay's windows) is Windows-only.

## Updates (`launcher/src/update.rs`, `core/src/release.rs`)

Made to give antivirus heuristics nothing to object to, and to need no signing key. The
window asks GitHub's API for the latest release once a day over WinHTTP (Windows' own
HTTP client, its proxy settings and certificate checks); the setting is `check_updates`
in ui.json. What a download must be comes from that HTTPS answer: the `digest`
(`sha256:...`) and `size` GitHub gives for the `peerparty.exe` asset. A release without a
digest falls back to a plain checksum file among its assets (`peerparty.exe.sha256`,
`peerparty-release.txt` or `SHA256SUMS`; sha256sum's form or `sha256 <hex>` lines, read
by `core/src/release.rs`), fetched from github.com over HTTPS. With neither, the new
version is announced, the window says it can't check a download, and only the release
page is offered. Asset addresses must be on github.com. Nothing is signed: the check is
that the file is the one published in the project's GitHub release, as GitHub reports
it.

- The exe is never downloaded by PeerParty: the browser does it (an app that downloads
  and starts programs looks like a downloader to antivirus heuristics). The window
  watches the Downloads folder for 30 minutes (`peerparty*.exe` of the release's size),
  or takes a file the player picks, and accepts it only if it is a program (`MZ`) with
  the release's SHA-256 and size.
- Install: only with no other peerparty.exe running from the same file (no game), on
  the player's click. Rename the running exe to `.old`, copy the file in, check the copy
  again, start it with `--updated`; any failure renames the old one back. The new
  process deletes `.old` (retrying while the old window closes; a later start retries).
- In another app's add-on folder (an `extension.yaml` beside the exe) the window only
  says to update the add-on, which ships its own copy.
- No scripts, PowerShell or installers run, and the exe isn't code-signed (an
  Authenticode certificate would be a later option against SmartScreen).

Publishing a release: the release script builds `peerparty.exe`,
writes `peerparty.exe.sha256` beside it and publishes both in a GitHub release tagged
`v<version>`. Set the workspace `version` in `Cargo.toml` to that version before
building: a copy updates when the tag is higher than the version built into it. GitHub
adds its own SHA-256 to each asset; the `.sha256` file is the fallback. Published by
hand, attaching `peerparty.exe` (that exact name) is enough.

## Windows Firewall (`launcher/src/firewall.rs`)

- The check reads the rules from the registry (the firewall service's store, then Group
  Policy's) and the network profile in use from `INetFwPolicy2`; it takes about 10 ms.
  A game is blocked when an enabled inbound Block rule for its exe (TCP, UDP or any
  protocol, not narrowed to ports, addresses or a service) covers the profile in use, or
  that profile blocks all incoming connections. Block beats Allow, as in Windows. With
  no Allow rule for TCP and UDP and inbound blocked by default, a gentler note shows.
- The exe checked is the game process's own image (a starter program hands over), not
  peerparty.exe, whose sockets carry none of the game's traffic. Xbox app games are
  skipped: their rules belong to the package.
- The fix runs `peerparty --fix-firewall` elevated (`runas`): PowerShell's NetSecurity
  disables the game's local Block rules on the chosen profiles, replaces PeerParty's own
  earlier rule, and adds `peerparty: game.exe` (group `peerparty`). Block rules are
  turned off, not deleted. Declined or failed (an administrator account can't see a
  mapped drive peerparty.exe runs from): the "Allowed apps" control panel opens, or
  `wf.msc`. The overlay rechecks every 20 s and right after a fix.
- LAN dial timeouts (`mesh.rs`): three timed-out connects to a peer within a minute,
  with none succeeding, mark it unreachable; the state's `lan_unreachable` counts them.
  Since we hear that peer's beacons, the block is on their side.

## What isn't carried

- Anything that runs on the publisher's or platform's servers: server-side game logic
  (CloudScript and similar), hosted dedicated servers, Valve's relay network, Azure
  speech and translation behind Party's transcription and text-to-speech.
- Ownership, entitlements, DLC and licence checks, by design.
- Anti-cheat games, by design.
- Interfaces listed as gaps above, and anything a game's log names in a `gap:` line.

## Safety rails

- A game in which EasyAntiCheat or BattlEye loads, or that starts an EOS Anti-Cheat
  session with the EOS layer on, is stopped.
- Licence, DLC, entitlement and purchase queries go to the SDK the game loaded, or come
  back empty or offline; sign-in is answered locally (LEGAL.md).
- Game files are never changed; PeerParty's own files live under
  `%LOCALAPPDATA%\peerparty` (logs under `%TEMP%\peerparty\logs`). Elsewhere it writes only
  save copies under a PeerParty identity (originals only read), firewall rules when the
  player asks for the fix, its local certificate's key in the user's key store, and an
  Xbox app package's debug setting while it starts that game (cleared at once).
- Certificates are trusted inside the game only; nothing system-wide is installed.
- Untrusted peers (public sessions) are bounded and blockable; PeerParty never shows
  addresses (they are still visible at the network level and on the DHT; see
  public-sessions.md).

## Sources

SDK behaviour and layouts come from the vendors' documentation and SDK headers, the
vendors' open-source SDKs and samples, other public projects (whose code isn't
copied), and what PeerParty observes of a game's own calls and its SDK's
traffic at run time. No code from closed-source SDKs or games is included, and no
header text: the generated tables carry names and order only. Code that is ported keeps
its licence beside it (LEGAL.md).

- Steamworks: Valve's Steamworks documentation and SDK headers (every historical interface
  version, for vtable layouts).
- EOS: Epic's developer documentation and SDK headers.
- PlayFab: Microsoft's API reference, the open-source PlayFab SDKs
  (github.com/PlayFab/CSharpSDK, PlayFabCSdk, PlayFabPartyUnity,
  PlayFabMultiplayerUnreal) and libHttpClient, the Party C++ SDK's public headers.
- GOG Galaxy: GOG's SDK documentation and SDK headers (every version's layout).
- Photon: Luxon and Luxon Server (github.com/niansa/LuxonServer,
  gitlab.com/luxon_project/Luxon, BSD-3-Clause), ported; the behaviour of Photon's own
  .NET client.
- Unity Gaming Services: Unity's REST API references (services.docs.unity.com), Unity
  Transport's package sources (its wire format), Centrifuge's protocol (MIT), RFC 6455,
  6902, 6347, 5246, 4279 and 7627.
- Xbox (GDK): Microsoft's documentation for the calls; the interface ids the runtime is
  queried with and the slot numbers of its objects, with every hooked slot checked to
  point inside the loaded xgameruntime.dll before it is hooked.
- Windows: Microsoft's documentation for every API used, and the Detours technique for
  loading a DLL into a suspended process.
