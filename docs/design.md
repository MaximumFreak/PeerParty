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
   it doesn't implement with an empty success, shaped as that call's result (its lists
   and maps empty rather than missing).
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
  the parent lets the pipe go. A starter never starts the LAN for itself: with internet
  play set up (a room code, public sessions), a process without a game SDK waits until a
  program it started has said whether it is the game (3 s with none pending, 30 s at
  most) and starts none once the pipe has gone to it, so the game, not the starter, is
  this PC's first player (its name, its saves). The launcher puts the game in a job object (breakaway
  allowed, nothing killed on close) and waits for the last of its programs inside the
  folder, reporting that one's exit code.
- **.bat / .cmd wrappers** (`launcher/src/wrapper.rs`): read, not run; the one program
  line they start is the game, with the script's folder, variables and arguments.
- **Epic's restart check**: `EOS_Platform_CheckForLauncherAndRestart` answers
  `EOS_NoChange` in a game PeerParty started, in every mode, so the game doesn't quit
  to restart through Epic's launcher.
- peerparty.exe stays running beside the game: it hands back the game's exit code and
  hosts the overlay.
- A game whose program is an anti-cheat starter, or has an anti-cheat module beside
  it, isn't started (the launcher looks first: launcher/src/anticheat.rs). A game in
  which EasyAntiCheat or BattlEye loads anyway is stopped as soon as that module
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
  kernelbase instead). A slot that another module already redirected keeps that
  redirect behind PeerParty's: mod loaders built on Unity Doorstop (BepInEx,
  MelonLoader) point UnityPlayer.dll's slot at themselves from their `DllMain`, before
  the hook loads, to catch the runtime's start-up lookups, and still get every lookup
  PeerParty doesn't answer, so the game's mods load.
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
- **Host lookups** (`hook/src/dns.rs`): `getaddrinfo`, `GetAddrInfoW/ExA/ExW`,
  `gethostbyname`, `DnsQuery_A/W` and `DnsQueryEx` are hooked in the game. Hosts
  PeerParty stands in for resolve to a loopback address of the game's own process
  (127.80.x.y, the next free one if another process has it) or to the LAN copy that
  hosts a service; a DnsQuery gets an A record dnsapi itself made (no AAAA). Everything
  else resolves normally.

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
  so two copies of one game on one PC are two players (slots are held per game: the
  player is the same player in two different games at once, and the overlay tells a
  copy on a later slot that it is another player). A slot is an Ed25519 key, and the player's id
  is 128 bits of its hash (`core/src/id.rs`): every way of connecting (the mesh's hello
  on the LAN and inside a tunnel, a room's and a public session's handshake) has the key
  sign for the connection, so an id can't be used by anyone but its player. The first copy of a Steam game is shown a
  SteamID of its own for this PC unless Valve's Steam client runs it (saves keyed by
  SteamID start, when the new id has none, from a copy of the save of an earlier
  PeerParty id kept in `slot-N.previous`, else the newest neighbouring PeerParty save,
  else steam_api's; `hook/src/saves/mod.rs`); a later copy gets a LAN one. Its saves are
  its own (**Players' saves**, below).
- **Players' saves** (`protocol/src/saves.rs`, `hook/src/saves/`, `launcher/src/saves.rs`).
  Player 1 keeps the real folders, registry and Steam Cloud. Players 2–4 each have a
  folder of their own, `%LOCALAPPDATA%\peerparty\saves\<game>-<hash>\player-N`, and the
  hook sends their game there from its first file call (the launcher says which player a
  copy is: `PEERPARTY_PLAYER_SLOT`). A path falls in a zone (`classify`): Windows' data
  folders (`AppData\LocalLow`, `Local`, `Roaming`, Documents, Saved Games) below the
  folders every program shares (drivers' shader caches, Windows', stores', PeerParty's:
  `EXCLUDED`), Steam's `userdata\<account>\<app>`, folders named for the game
  (`PEERPARTY_SAVE_PATHS`), and the game's own folder. In the data folders the player's
  folder is all there is: nothing of player 1's is read, so two copies never open one
  file. The game's own folder is read through until written: a write copies the file
  in first, a delete is noted (`whiteouts.txt`), folders named for saves aren't read
  through, and the game's files are never written. kernelbase's file calls are hooked
  (`CreateFile*`, attributes, listings, directories, delete, move, replace, copy
  including `CopyFile2`, hard links, handle renames through `SetFileInformationByHandle`,
  and the profile `.ini` calls), and the registry's key calls: a game's
  `HKCU\Software\<vendor>` keys (Unity's PlayerPrefs) are under
  `HKCU\Software\PeerParty\Saves\<game>\pN`, except a key only read that player 1's game
  never writes (an installer's), which reads through. `ISteamRemoteStorage` is answered
  from the player's `steam-remote` folder (`hook/src/steam/remote.rs`). Player 1's copy
  notes where its game writes (`discovered.txt`); the launcher's save manager uses that
  to copy, swap, reset and restore any player's saves (ids in names renamed to the other
  player's, the replaced saves backed up first, five backups kept), and copies player
  1's to a new player before the game starts when asked ("Start new players from Player
  1's saves", `--copy-saves`); a game folder it doesn't know yet the hook copies the
  first time the game uses it. `PEERPARTY_SEPARATE_SAVES=0` shares player 1's.
- **Discovery** (`core/src/mesh.rs`): UDP broadcast beacons carrying the game key (the
  exe's name, or `PEERPARTY_GAME`), plus a peer board under `%LOCALAPPDATA%\peerparty\
  peers` so copies on one PC find each other with no network at all. Extra beacon
  targets (`PEERPARTY_PEERS`) cover VPNs and networks broadcast doesn't cross.
- **Mesh.** One TCP connection per peer for reliable data, with keepalive and a capped
  send queue, plus a UDP socket for unreliable data once the peer answers a UDP probe.
- **Dropped connections** (`core/src/link.rs`): a peer whose connection drops keeps its
  place for 10 s: it stays in its lobbies and channels, and what is sent to it is kept.
  Each reliable message of a peer's session carries its number, and each side keeps
  what the other hasn't acknowledged (up to 4 MiB). The receiver goes by the numbers: a
  message it already has is dropped, and messages lost on one connection while another
  carried later ones (a duplicate connection retired when both sides dialed at once,
  then cut) are asked for again, so nothing arrives twice or goes missing. When the peer
  is back, on any path, both sides exchange where they stand and make the same call:
  pick up where they left, each sending again what the other missed, so the game sees
  nothing; or, after a restart or a gap longer than what was kept, a leave and a fresh
  join. What came before that call waits and is delivered first, and anything another
  of the peer's connections brings meanwhile queues behind it. Not back in time, it
  leaves. A copy that quits, switches internet play off or
  blocks a player says goodbye and leaves at once. Before this, a break between a lobby
  member and the owner split the lobby: the member promoted a new owner, the owner
  dropped the member.
- **Connections through another player** (`core/src/lan/relay.rs`): when only the link
  between two players breaks (a member and its lobby's owner) while both still reach a
  third in a lobby with them, their meshes connect again through that player. The two
  ends trade X25519 halves, each signed with its player's key, and seal every chunk with
  ChaCha20-Poly1305 under the key they agree on, numbered each way: the player carrying
  it sees only sizes, and can neither read the connection nor pass as either end. The
  hellos and proofs run end to end, and the session picks up where it left. Either end
  may start it (the higher id waits 1.5 s; when both start, the lower id's offer wins),
  and an offer names the connection its sender lost, so an end that hadn't noticed the
  break drops that connection then, and a replayed offer changes nothing. It ranks below
  every direct path, so the direct link takes over once it mends. When nobody can carry
  it, a member that lost the owner asks the members it still reaches, which pass the
  question on to the owner; if the owner answers, only that member's link broke and it
  is out of the lobby (the owner has dropped it), else the election goes ahead 3 s later
  than it used to.
- **Lobbies** (`core/src/lobby.rs`, `core/src/lan.rs`): owned by their creator's copy,
  changes sent as snapshots, the owner handed over when it leaves. A join can carry a
  secret (a lobby's password) that the owner's front end checks, so what only the owner
  may decide isn't left to the joiner's copy. Searches run on every
  peer and complete when each has answered or after a timeout. Ownership is signed: the
  creator makes a key pair for the lobby, every snapshot, search result and join answer
  carries the owner's claim signed with it, and only members get the secret half, so
  whoever owns the lobby next can sign. A copy that has a lobby's key (from a search
  result, an invite, being in it) takes word of that lobby only under that key, so
  nobody outside it can answer for it or take it over. A join by bare id, with no key
  in hand yet, asks every player and every player answers (the owner signed, the rest
  "not mine"). It is settled once all have answered, or 3 s after the first signed
  answer: LAN and room players' answers count over public players', and if the answers
  that count name different lobby keys the join is refused, and every player that let
  us in is told we aren't coming. So a LAN player can stop a bare-id join, but not
  redirect it.
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
  method returns it through a hidden pointer and the flat function as a number, and so
  do ISteamNetworkingUtils's inline helpers (`SetGlobalCallback_*`,
  `SetGlobalConfigValue*`), which are one `SetConfigValue` call on the stand-in. A
  steam_api from before the flat API (it exports only the accessors) is reached through
  the real objects' own methods.
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
  ManualDispatch queue. PeerParty's `SteamAPICall_t` handles come from a reserved range;
  a game that polls them (ISteamUtils's `IsAPICallCompleted` / `GetAPICallResult`) gets
  them from ISteamUtils's stand-in.
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
    and every other slot kept). A native game calling `ISteamUserStats`, `ISteamApps` or
    `ISteamUGC` through their C++ interfaces gets the same: those are stand-ins whose
    methods map the id and call the real object's (GetUserStat's two overloads, one slot
    each, are told apart as `GetUserStat~3@0` and `@1`). `PEERPARTY_SKIP=steamown` turns
    the mapping off.
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
  - Steam's Web API (`steam/web.rs`): `api.steampowered.com` resolves to the local
    HTTPS service in Unity games whose Steam is PeerParty's. `ISteamWebAPIUtil/GetServerInfo`,
    which some games poll every few seconds to see Steam is up (and leave the match on
    the first miss), is answered there with the server time as Steam writes it; any
    other request is passed on unchanged to the real host (`https/client.rs`
    `pass_on`), a 503 when it can't be reached. Not Unity: left alone, since a game
    with its own TLS roots wouldn't trust the certificate (`PEERPARTY_STEAM_WEB=1`
    overrides).
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
  and voice chat ride the network's voice room, with Party's permission rules. Who
  gets in is the network owner's copy's call: a user authenticates with one of the
  network's invitations (with their entity lists), checked there, and a member it
  hasn't let in is shown to nobody and dropped after a minute. An invitation's creator
  may revoke it (`PartyNetworkRevokeInvitation`), anyone in may revoke one made
  revocable by anyone, and every copy tells its game when an invitation it holds is
  gone (revoked, or its creator left).
  `PartyXboxLive.dll` (GDK builds) is answered on the LAN too; its chat permissions
  follow what the real Xbox Live privacy service said (below).
- **Lobby & Matchmaking:** every `PF*` export of `PlayFabMultiplayerWin.dll` is
  PeerParty's. Lobbies are LAN lobbies with PlayFab's filter and sort strings evaluated
  locally; matchmaking tickets pair the mutually oldest compatible tickets.
- **Web API:** lookups of `<title>.playfabapi.com` resolve to the game process's own
  loopback address, where PeerParty answers HTTPS with PlayFab's JSON shapes: sign-in,
  entity tokens, profiles, statistics and user data kept on disk, lobbies, matchmaking,
  and empty title data, CloudScript and catalog answers. A call with no stand-in answers
  with its result as a title with nothing set up has it: the lists and maps a game reads
  from it come back empty, not missing (a table of PlayFab's calls, else for a `Get`,
  `List`, `Find` or `Search` call the plural nouns in its name), since a game that loops
  over a missing list can crash. PlayFab's C SDK (PlayFabCore) uses the same service
  through its own HTTP stack.

### GOG Galaxy (`hook/src/galaxy.rs`, `galaxy/`)

- `galaxy::api::User()`, `Friends()`, `Matchmaking()`, `Networking()` and the listener
  registrar hand out stand-ins; `ProcessData()` runs PeerParty's listener calls after
  the real one. The DLL's file version picks the vtable layout (every SDK from 1.57 on),
  checked against the DLL's own vtables through MSVC RTTI. An older SDK's shape of a
  method (before per-call listeners, a user argument or a savegame type) is answered as
  the newest one, its results going to the global listeners; a listener whose callback
  had fewer arguments then is called with that many.
- Carried: sign-in, user data, friends, persona names, rich presence, invites, lobbies,
  P2P, chat, stats and achievements on disk, storage and cloud saves on disk, tokens.
  IApps (DLC) stays with Galaxy. Game servers aren't carried.

### Photon (`hook/src/photon/`)

- Photon Cloud's hosts resolve to one copy on the LAN, which runs Photon's name, master
  and game servers inside its game process (UDP, TCP and WebSocket). The first copy to
  look Photon up hosts unless a LAN peer already does.
- The other copies reach those servers at the host's stand-in address, through the
  typed-IP forwarder over their PeerParty connection, so the host's firewall never has
  to let them in (a network set to Public still works). A game's connected UDP socket
  is connected to the forwarder's local socket for that peer and port.
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
  the same local HTTPS service. A locked lobby and a lobby's password are checked by the
  host's copy; the password's hash is kept for members only. A private lobby is
  unlisted: the host's copy answers only a search for its id or its exact code, so no
  query (a modified copy's either) lists it, and anyone with the code still joins. A relay allocation makes
  the host's copy the relay, which
  speaks Unity Transport's relay protocol, plain or inside DTLS with a pre-shared key.
- Each allocation has a random key that only the relay and its player know: a joiner's
  copy asks the code's host for its allocation over the core's channel to that player.
  A Bind must carry Unity Transport's HMAC with the key, and a copied Bind moves
  nothing. DTLS asks for a cookie before keeping anything for an address, and drops
  replayed records.
- Vivox (`hook/src/vivox.rs`): a game whose Unity sign-in is PeerParty's hands Vivox a
  token its servers refuse, so the Vivox core SDK's C API (vivoxsdk.dll, under Unity's
  VivoxNative.dll) is answered instead, on the LAN voice rooms EOS and PlayFab Party use.
  The real SDK is never initialized. Players appear with the account URIs Unity's
  package builds, so the game's participant lists match its players; in positional
  channels each speaker is turned down by distance with the channel's fade model and
  placed left or right of where the listener faces (voice.rs mixes in stereo; the far
  side goes down to a quarter, never silent). Device lists and choices are Windows'
  own endpoints (voice/endpoints.rs), shared with EOS and PlayFab Party: one choice
  each way per process, as the SDKs have, and a device that goes away falls back to
  the communications default until it's back.
- QoS and Matchmaker are left alone.

### Xbox Live (`hook/src/xboxlive/`, `xgameui.rs`)

- For Microsoft Store games: the social, profile, presence and privacy hosts are answered
  from the LAN, so a game's friends screen lists LAN players, and the Xbox invite screens
  open PeerParty's overlay. Everything else on Xbox Live (achievements, multiplayer
  session directory, title storage) resolves normally.
- Privacy requests also go on, unchanged and as the game signed them, to the real
  privacy service (`real.rs`, its own TLS client that checks the real certificate), and
  the stricter answer wins: the player's real block and mute lists and the account's
  limits (a child account's settings) apply to LAN players too, and to PlayFab Party
  chat through `PartyXboxLive.dll`. Offline or signed out, the LAN's answers stand
  alone. `PEERPARTY_XBL_REAL=0` turns it off.

### Demonware (`hook/src/demonware/`)

- Activision's online service, statically linked into the game. Lookups of
  `*.demonware.net` resolve to PeerParty's own address in a game started through it.
- Sign-in (auth3, JSON over HTTPS) is answered by the local HTTPS service; the lobby
  service (TCP 3074) does Demonware's handshake and encrypted calls, answers server time
  and matchmaking, and logs every other call; Demonware's address check (UDP 3074)
  reports the LAN address. Sessions a copy hosts reach the other copies through LAN
  presence, so the game's matchmaking search and find-by-player see them.
- The game's Steam encrypted app ticket is PeerParty's while Demonware is answered
  locally, carrying the data the game put in it.
- libcurl over SChannel validates certificates automatically, outside the crypt32 hook;
  for Demonware's names only, such a client gets a manually validated twin of its
  credential, which then trusts PeerParty's certificate like every other client does.
- Players in one Demonware session count as together in the overlay even when the game
  put them in different Steam lobbies (it makes one per player who joins).
- `PEERPARTY_DEMONWARE=0` or `PEERPARTY_SKIP=demonware` turns it off. Written from the
  wire format only: an open-source community client was read for protocol facts, and
  none of its code is used.

## Trust for the local HTTPS service

The local service (`hook/src/playfab/http.rs`, `tls.rs`) uses a self-signed certificate
made with Windows' own crypto. It is trusted **inside the game process only**:

- crypt32's `CertGetCertificateChain` is hooked in the game, and only for that
  certificate's exact bytes returns a chain whose root is the certificate;
- for clients that read the ROOT store and verify themselves (UnityTls on MbedTLS), the
  ROOT store the game opens comes back as a collection with PeerParty's certificate
  added in memory;
- GDK games' own pin check (`XNetworkingVerifyServerCertificate`, `hook/src/xgameui.rs`)
  passes for that certificate only;
- a libcurl with its own TLS (a libcurl DLL, or one linked into the game and found by
  the patterns in the player's per-module hooks files, `hook/src/curl/`) is told, for a request to a host whose
  lookup comes to the local server only, to pin that certificate's public key
  (`CURLOPT_PINNEDPUBLICKEY`) with its chain and name checks off: curl aborts before
  sending anything when the key differs. Its other requests keep the game's settings,
  and a curl that can't pin is left as it is.

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
- Its pages (`host/ui.rs`) are for what can be done in the game now: Players (each
  player once, invites on the sender's row, the same A / X / Y buttons on every row),
  Network, and in a split-screen session Split screen first. How it looks and what
  opens it are the launcher's settings: the session host looks at ui.json about once a
  second and takes the overlay's settings and auto-accept that changed there
  (`reload_prefs`), so a `--set` for the run stays until that setting is changed.

## Split screen (`launcher/src/host/`, `hook/src/video/`, `hook/src/input/`, `hook/src/saves/`)

A split-screen session is one peerparty.exe (the session host, `host/mod.rs`) running two
to four copies of the game, each a normal PeerParty copy with its own identity, that
meet over the LAN core as copies on two PCs would. The host owns the session's window,
the controllers, the keyboard and mouse, and the layout. What follows is the Windows
session; a Linux one (frames read back as pixels) isn't at the same level yet.

- **One window, drawn on the GPU** (`compositor.rs`, `gpu.rs`). A thread of the host
  owns a Direct3D 11 device on the copies' graphics card and a flip-model swap chain on
  the window. Each pane is drawn from its copy's newest finished frame, scaled into the
  pane with a source crop (`Crop` fills the pane by cutting the frame's edges) and in
  the panes' Z order, so a picture-in-picture pane draws over the one behind it. Frames
  keep their colour space: on an HDR monitor the swap chain is scRGB and SDR panes are
  brought up to Windows' SDR white level, on an SDR one HDR panes are tone-mapped down.
  A pane whose copy can't share frames (another graphics card, before its first frame)
  shows the copy's own window through a DWM thumbnail instead; `PEERPARTY_COMPOSITOR=dwm`
  does that for every pane.
- **Frames** (`protocol/src/frame_ring.rs`, `hook/src/video/capture.rs`). At each
  present, after the overlay is drawn, the hook copies the back buffer on the GPU into
  one of three shared textures and publishes the slot in a small shared-memory ring.
  Two shared fences do the waiting, both on the GPU: the window waits on the hook's
  content fence before sampling a slot, and the hook writes only a slot whose release
  fence the window has passed (it skips that frame's copy otherwise). Neither process
  ever waits for the other on the CPU. Direct3D 11 and 12 copy on the game's own device;
  Vulkan and OpenGL go through a Direct3D 11 device of PeerParty's own on the same
  adapter; Direct3D 9Ex opens a texture shared by that device, and plain Direct3D 9 reads
  its frames back (or, with `PEERPARTY_DXVK`, is drawn through Vulkan and shares them as
  a Vulkan game does). `PEERPARTY_COMPOSITOR=pixels` reads every frame back into a file
  instead, the way a game under Wine has to.
- **Frame pacing** (`protocol/src/frame_ring.rs`, `host/gpu.rs`, `hook/src/video/capture.rs`).
  With **Sync games to screen** on (the default; `--no-sync` or Layout Studio turns it off),
  each copy's ring header carries `FLAG_SYNC_WANTED` and `FLAG_COMPOSITOR_LIVE` alongside a
  per-copy auto-reset pace event. On DXGI, the hook passes sync interval 0 to the hidden
  swap chain so it never double-vsyncs against DWM, then after present calls
  `capture::pace()`. If the compositor's `presents` counter already advanced since the last
  frame (a copy running below the monitor's refresh rate), it returns at once with no stall;
  otherwise it waits for the pace event signalled right after the compositor's `Present`,
  capped at 35 ms per VSync interval (`RingHeader::sync_divisor`). Fractional refresh caps
  (`--fps 1/2` or `1/3`) set the compositor's DXGI sync interval to 2 or 3 so every copy
  locks in phase to hardware VBlanks with no pulldown judder or timer drift on fixed-refresh
  displays; fixed FPS caps (`--fps 15..240`, `RingHeader::pace_us`) pace copies to a
  drift-free interval snapped to compositor VBlanks, and panes hidden while another pane
  is zoomed (`FLAG_HIDDEN`) throttle to 20 FPS so LAN simulation stays alive while freeing
  the GPU for the zoomed player. The compositor clears `FLAG_COMPOSITOR_LIVE` and wakes
  waiting copies whenever a pane is hidden, minimized, occluded or stopping, so a copy never stalls.
- **Performance & resource sharing** (`hook/src/video/vram.rs`, `hook/src/cpu.rs`,
  `launcher/src/backend/windows/cpu.rs`, `launcher/src/host/gpu.rs`).
  - **VRAM budget fair-share** (`PEERPARTY_VRAM_SHARE`, on by default; `--no-vram-budget`
    turns it off): clamps `IDXGIAdapter3::QueryVideoMemoryInfo` (D3D11/12) and Vulkan's
    `VK_EXT_memory_budget` device-local heaps to `min(real_budget, 85% * physical / N)`
    (floored at 512 MiB) so engines size their own texture streaming pools for sharing one
    GPU instead of thrashing over PCIe.
  - **Render scale & sharp upscaling** (`--render-scale 50..=100`, `--no-upscale`): scales
    each copy's virtual viewport down below its pane size and scales it back up in the
    D3D11 compositor with an optional single-pass Contrast Adaptive Sharpening (CAS) shader
    (active only when upscaling and toggleable off for plain bilinear sampling).
  - **CPU balancing** (`PEERPARTY_CPU_CORES`, on by default; `--no-cpu-balance` turns it
    off): assigns topology-aware soft Windows CPU Sets (`SetProcessDefaultCpuSets`, keeping
    L3 cache domains separate on multi-CCD CPUs and balancing P/E cores fairly on hybrid
    CPUs) and scales reported logical core counts (`GetSystemInfo`, `GetNativeSystemInfo`,
    `GetActiveProcessorCount`) so N copies don't oversubscribe worker threads.
- **The pane is the game's screen** (`hook/src/window.rs`). Each copy is told its pane:
  its window, display modes and screen size read as the pane's (or the aspect Layout
  Studio set), so the game renders at the pane's own size. A zoomed pane is told the
  whole window. A game's saved resolution reads as the pane's too: Unity's registry
  values (`hook/src/screen_prefs.rs`), and Unreal's `GameUserSettings.ini`, read as a
  fullscreen window at the pane's size, since in exclusive fullscreen Unreal doesn't
  follow its window when the pane is resized (`hook/src/saves/screen_ini.rs`; what the
  game saves goes back to its file with the single-window values kept). The copy keeps
  reading as the window in front (`GetForegroundWindow`,
  `GetFocus` and focus-loss messages) while another pane has the keyboard and mouse.
- **Input** (`protocol/src/input_shm.rs`, `host/input_hub.rs`, `hook/src/input/`). The
  host reads every controller, the keyboard and the mouse, and writes each copy's share
  into that copy's 256-byte shared-memory block. The hook answers XInput, DirectInput,
  HID, Raw Input, Windows.Gaming.Input and GameInput from it, so a pane sees its own
  player's controller only; where that controller can be reached as it is (its XInput
  slot, its HID device) the game's reads go to it untouched, and the other players'
  devices read as unplugged or at rest. Devices are told apart by which one's buttons
  moved, so two identical controllers stay two seats. The mouse is held inside the
  focused pane while its game holds its cursor, and the pointer hides then, while the
  game hid it, and when the mouse sits still while players are on controllers.
- **Players' saves** (`hook/src/saves/`, `launcher/src/saves.rs`). Player 1's copy uses
  the game's own saves, untouched and the same ones it has when played alone. In players
  2–4's copies, the file calls for a save folder (Windows' data folders, Documents,
  Saved Games, Steam's userdata) go to the player's own folder under
  `<data>\saves\<game>\player-N`; the game's own folder is read through, and a file the
  game writes there is copied into the player's folder first, so the game's files are
  never written. Keys under `HKCU\Software\<vendor>` go to
  `HKCU\Software\PeerParty\Saves\<game>\p<N>`, and Steam Cloud files to the player's
  folder.
- **Sound** (`host/audio.rs`). Muting a pane mutes every WASAPI audio session of that
  copy's process (`IAudioSessionManager2`), again when a game opens its audio late, and
  everything muted is unmuted when the host exits.
- **Starting** (`host/mod.rs`). Copy 1 starts first; with the smart stagger (the
  default) each next copy starts once the one before has a window or a viewport (plus
  half a second) or its first frame, 8 s at most, so copies don't compile shaders and
  load from disk at the same moment. `--stagger N` waits N seconds instead.
- **Layout Studio** (`host/layout_studio.rs`) edits the panes as slots in ten-thousandths
  of the window (`protocol/src/session.rs` `PaneSlot`: rectangle, fit, aspect, Z), drawn
  in egui over the live frames. A drag previews without telling the copies; letting go
  commits, and each copy is told its new pane. **Done** keeps the slots in ui.json as the
  game's layout (`slots:...`, the same text `--layout` takes).

## The launcher (`launcher/src/ui/`)

An egui window shown when peerparty.exe starts with no arguments. It lists Steam, Epic,
GOG and Xbox app installs and the player's own game folders, and starts games through
peerparty.exe with per-game options. Settings live in `%LOCALAPPDATA%\peerparty\ui.json`
and apply however a game is started.

- Three places: the library (`library.rs`, a game's panel in `game.rs`), Play together
  (`together.rs`: player names and who can join, for every game) and Settings
  (`settings.rs`, the overlay's look and keys, its controller chord recorded with the
  window's own controller reader); a first-run setup over the library (`welcome.rs`:
  split screen, home, friends online, anyone online) writes its answers only at the end.
- The README's screenshots are made by two ignored tests (`readme_launcher_screenshot`,
  `readme_overlay_screenshot`) from made-up players and games with covers drawn in code.
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
  earlier rules, and adds `peerparty: game.exe` for TCP and for UDP (group `peerparty`;
  `--remove-firewall` deletes the group). Block rules are
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
  `%LOCALAPPDATA%\peerparty`, or a `peerparty-data` folder beside peerparty.exe when it is
  portable (launcher/src/portable.rs; logs under `%TEMP%\peerparty\logs`). Players 2–4's
  saves are in its `saves` folder and their registry under `HKCU\Software\PeerParty\Saves`.
  Elsewhere it writes only save copies under a PeerParty identity (originals only read),
  player 1's saves when the player copies, swaps, resets or restores them (backed up
  first), firewall rules when the player asks for the fix, its local certificate's key
  in the user's key store, and an Xbox app package's debug setting while it starts that
  game (cleared at once).
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
  Transport's package sources (its wire format and the Bind HMAC), Centrifuge's protocol
  (MIT), RFC 6455, 6902, 6347, 5246, 4279 and 7627.
- Vivox: Unity's Vivox core reference (docs.unity.com/vivox-core: the request, response
  and event structs), the order Unity's Vivox package (com.unity.services.vivox) expects
  them in, and the field offsets VivoxNative.dll's own accessors use; OpenAL's distance
  models for the fade.
- Xbox (GDK): Microsoft's documentation for the calls; the interface ids the runtime is
  queried with and the slot numbers of its objects, with every hooked slot checked to
  point inside the loaded xgameruntime.dll before it is hooked.
- Windows: Microsoft's documentation for every API used, and the Detours technique for
  loading a DLL into a suspended process.
