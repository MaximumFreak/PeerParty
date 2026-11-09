# Using PeerParty

The launcher window, the command line and its settings, what each SDK layer does, the
overlay, and internet play. README.md has the overview.

A note on words: "LAN" below in the SDK sections ("a LAN lobby", "LAN players") means
PeerParty's own lobbies and players in place of the online service's. They run on your
local network and, with a room code or public sessions, over the internet too. "Local
network" always means only the PCs on your own network.

## Running it

`peerparty.exe` is the only file. It carries the x64 and x86 hook DLLs and writes the
one a game needs to `%LOCALAPPDATA%\peerparty\hooks\<hash>\` on first use.

### The launcher window

Double-click `peerparty.exe` (no arguments) for a window to pick a game and start it
through PeerParty. It finds what's installed on any PC:

- **Steam**: every library in `libraryfolders.vdf`, with Steam's cached box art.
- **Epic**: the launcher's install manifests (`Data\Manifests\*.item`).
- **GOG**: GOG Galaxy's registry entries.
- **Game folders**: every subfolder of the folders in the list, `C:\Games` to begin
  with. Add others, or remove `C:\Games`, in Settings > Library. Folders starting with
  `.` are skipped. A folder's program is picked by `launcher/src/ui/mainexe.rs`:
  `.gamebox.json`'s `main`, else a Unity game's exe, else the largest, skipping
  redistributables, tools, and old-renderer builds beside a newer one.
- **Recent**: every game peerparty.exe starts (from the window, a .bat or a Steam
  launch option) is noted in `%LOCALAPPDATA%\peerparty\recent.tsv`.
- **Add > Add a game…** picks any .exe.

The sidebar has three places:

- **Library**: the games, as covers or a list, with filters (All, Recent, each store,
  Games folder, Added, Hidden) and a search (Ctrl+F). A game's panel has Play, the
  number of players on this PC, **Who can join you** (people on your network, with a
  Windows Firewall check and Fix; your room; public sessions), **This game's own
  settings** (invites, overlay, voice and saves for this game alone) and **Advanced**
  (multiplayer through PeerParty on or off, online services, PeerParty layers to turn
  off, "Meet players of", the program, extra arguments, the exact
  `peerparty` command and a .bat shortcut). A dot on a folded section says something in
  it differs from the recommended. Double-click or Enter plays; right-click for Hide
  and the shortcut.
- **Play together**: who can join you in every game. People on your network always
  can; **Friends online** makes or joins a room code (copy, show, leave); **Anyone
  online** turns on public sessions (with the name strangers see, voice with them, and
  the blocked players). Your name, invites, the controller invite buttons and voice
  chat are here too, and the **Internet play** switch that pauses the room and public
  sessions.
- **Settings**: General (language, lighting and animations, what the window does when
  a game starts, notifications), Overlay (on or off, the keys and controller buttons
  that open it, its size, button pictures and how it's drawn), Library (game folders,
  hidden games), Advanced (online services, existing saves under a new Steam player ID,
  logs, the first-run setup, Reset to recommended) and About.

The first time the window opens it asks three things (language and name, who you play
with, and voice and invites to match) and writes them only when you press Start; Skip
leaves everything as it was. A `ui.json` from an older PeerParty counts as set up.
Settings > Advanced > Run setup asks again.

Mouse, keyboard and controller work everywhere. The controller works while the window
is the foreground window: on the game grid the d-pad moves, A plays, X goes to the
game's panel, LB / RB change the filter; elsewhere the d-pad moves between controls and
A presses them. B goes back, Y goes to the next place (Library, Play together,
Settings), and LB / RB also switch Settings' sections. The bottom line shows the
buttons while a controller is in use. A button held or pressed as the window comes to
the front (or in its first 0.4 s there) needs a fresh press, and A doesn't start
another game for 5 s after the controller started one. Keyboard: Tab and the arrow keys
move, Enter presses, Esc goes back.

The window speaks the Windows display language (English, German, Spanish, French,
Italian, Dutch, Polish, Brazilian Portuguese, Russian, Ukrainian, Turkish, Japanese,
Korean, and Simplified and Traditional Chinese; English for anything else), or the one
picked in Settings > General (`language` in `ui.json`, `--set language=de`,
`PEERPARTY_LANG`). The overlay follows it from the next game start. Logs and the
command line stay in English. The translations are in `launcher/locales/`, one file
per language; a missing line shows in English.

The window stays out of a running game's way: it draws only when something changes
(no timers while idle; controllers are read on a thread of their own, only while the
window is in front), it minimizes when a game starts (Settings > General > When a game
starts), and while a game it started runs with the window behind, the process drops to
background priority. Lighting and animations can be turned off in Settings > General;
they're off when Windows' animation effects are.

Settings live in `%LOCALAPPDATA%\peerparty\ui.json`. The window is egui (Rust,
`launcher/src/ui/`); what it asks of the operating system goes through
`launcher/src/platform/`, and nothing in it is specific to one PC.

peerparty.exe is a Windows (GUI) program, so double-clicking it opens no console. From
a prompt it prints to that console; .bat files and Steam wait for it and get the
game's exit code (all 32 bits of it, so a crash reads as `-1073741819`, 0xC0000005).
When the game exits, PeerParty adds a line with the exit code, how long the game ran
and what a crash code means to the end of that run's log (`[launcher 17:32:07] game
exited after 0.3 s with exit code 3221225477 (0xC0000005), crash: access violation`),
starting a log for it if the game died before the hook could open one. PowerShell doesn't wait for GUI programs: use
`Start-Process -Wait` there if a script needs to.

### From a prompt

```
peerparty [--room CODE] [--log-dir DIR] [--appid N] [--multi-instance] [--copy-saves] [--set key=value]... <game.exe> [args...]
peerparty --new-room
peerparty --net-test
```

The launcher window's settings (`%LOCALAPPDATA%\peerparty\ui.json`: global ones and
each game's own) apply however the game is started: from the window, a shortcut or
Steam. `--set` changes one for a single run: `name=Alex`, `auto_accept=on`, `overlay=off`,
`invite_buttons=guide|combo|both`, `toasts=off`, `voice=off`, `draw=window|ingame`,
`mode.<service>=lan|official|auto`, `room=CODE` (empty: off), `public=on`, `internet=off`,
`language=de` (empty: the system's), `effects=off`. A variable already set in the environment wins over
both.

- Every game started through PeerParty plays on PeerParty's own multiplayer layers
  (EOS; Steam lobbies, P2P, friends and invites; PlayFab Party, Lobby & Matchmaking, and
  PlayFab's web API; Photon rooms; Unity's Authentication, Lobby and Relay), unless a
  service is set to the official servers. Everything works with no internet: see
  "Offline" below.
- `peerparty --invite`: the game in front invites every other player to where it is.
  Bind it to a hotkey for games without an invite button.
- `PEERPARTY_INVITES=auto`: invites from other players are accepted at once. The
  default is `ask`: the game hears an invite arrived (where its SDK has
  that event), a notification appears, and the player accepts or declines in the
  overlay.
- `PEERPARTY_INVITE_BUTTONS=guide|combo|both`: controller buttons that invite every other
  player without opening the overlay (the Home button, and/or View + Menu held for a
  second). None by default.
- `PEERPARTY_MODE_<SERVICE>=lan|official|auto` (`PHOTON`, `UGS`, `EOS`, `PLAYFAB`,
  `STEAM`, `GALAXY`; and `STEAMID`, default `auto`: `lan` shows the game a SteamID of its own for
  this PC so players never share one, `official` keeps the one its steam_api gives, `auto` is
  `lan` unless the Steam client runs the game): PeerParty's own stand-in (your local network, room and public players), the service's official servers (the
  game meets real online players), or official when they answer and PeerParty's otherwise. Set by the launcher from its settings,
  where every service is PeerParty's (`lan`) until changed; without the variable a service
  is PeerParty's too. `auto` and `official` leave the game's rooms on the service's servers,
  where PeerParty's overlay can't invite to them.
- `PEERPARTY_NAME`: the name other players see, in every service (Steam's persona name,
  EOS and GOG display names, PlayFab, Unity). Set from the launcher's Settings ("Your
  name"); empty means the Windows user name, and a second copy on one PC adds " (2)".
- `PEERPARTY_FOLLOW=1`: PeerParty also goes into the programs the game starts (on by
  itself in packaged games, for the programs of their own package).
- When a game exits with a non-zero code, its log says so with the modules on the
  caller's stack (`exit: ...`), which explains an `abort()` that leaves no crash dump.
- `--appid N`: rarely needed. A Steam game started outside the Steam client's own
  launcher needs its app id. PeerParty finds it in order: `--appid`, the `SteamAppId` variable, the app
  manifest of the Steam library the game is in, a `steam_appid.txt` next to the exe,
  any Steam library's manifest whose install folder has the name of one of the exe's
  folders. The Steam client still has to be running, and stays in charge of the game's
  licence.
- `--multi-instance`: let a second copy start on the same PC (Unity's single-instance
  mutex).
- `--copy-saves`: a second copy on the same PC starts from the first copy's progress,
  for games that don't offer multiplayer from a fresh save. A second copy plays under
  LAN ids (SteamID, EOS id, PlayFab id); the first time the game touches a path that
  contains one, the same path with the main id (the first copy's SteamID and ids) is
  copied over it, folder or file, newer files only. Saves that some games
  don't keep per player are shared by both copies already: back them up before playing
  two copies at once.
- `PEERPARTY_PLAYFAB=0` / `=1`: never / always answer PlayFab's web API locally
  (default: for games that ship or load PlayFab Party, Lobby & Matchmaking or the C SDK,
  decided at the first lookup since games sign in before loading them, and for any game
  when PlayFab can't be reached).
- `PEERPARTY_PHOTON=0`: leave Photon alone (by default, in a game started through PeerParty, Photon Cloud's hosts
  resolve to the LAN's Photon servers).
- `PEERPARTY_UGS=0` / `=1`: never / always answer Unity Gaming Services' Authentication,
  Lobby and Relay locally (default: for games that ship Unity's Lobby, Relay or
  Multiplayer SDK).
- `PEERPARTY_LOOPBACK=1`: every socket binds to 127.0.0.1 and no beacons are broadcast,
  so copies on one PC meet only through the peer board and Windows Firewall never asks.
  The tests set it (and `cargo` sets it for what it runs, .cargo/config.toml).
- `PEERPARTY_PEERS=host[:port],...`: extra beacon targets for networks that broadcast
  doesn't cross (VPNs, other subnets).
- `PEERPARTY_SKIP=layer,...`: turns PeerParty layers off for a game that objects to
  one: `swap`, `invite`, `eos`, `steam`, `party`, `pfmp`, `playfab`, `photon`, `ugs`, `galaxy`, `dns`,
  `padblock` (holding controllers back while the overlay shows), `present` (drawing the
  overlay inside the game), `ipfwd` (typed-IP forwarding to internet players), and
  `calls` (the call logging in SDK modules). The launcher
  window's game panel has these as checkboxes (Advanced).
- `PEERPARTY_GAME=name`: the game key players meet under, normally the exe's name
  (less a dedicated server's "_server" ending); for a dedicated server whose exe
  differs from its client's in some other way.
- `PEERPARTY_VOICE=0`: PeerParty never opens the microphone or speakers for voice
  chat (EOS voice rooms and PlayFab Party chat, their data and text, and game-fed
  audio still work; Party reports the input and output as "not found").
- `PEERPARTY_STEAM_SAVES=0`: a game shown a SteamID of its own for this PC starts its
  saves keyed by SteamID afresh, instead of from a copy of the ones under its old
  SteamID (the launcher's "Start from the game's existing saves", on by default). The
  copy is made once, the first time the game looks; the originals are only read.
- `PEERPARTY_STEAM_ACCOUNTS=1`: when players keep one shared SteamID (`STEAMID` mode
  `official`), also swap 32-bit account ids in game packets, not only whole SteamIDs.
  Off by default: a shared account id is usually a small number, and swapping it can
  mangle ordinary game data.
- `PEERPARTY_STEAM_DUMP=1` (or a number of packets, default 400): log Steam game
  packets, lobby chat and lobby data each way in hex (the first 160 bytes), naming every
  SteamID or account id found in them and how it's encoded (`u64le`, `u64be`, `dec`,
  `dec16`, `acc u32le`, ...), and what the game is told for lobby members in order and
  the lobby owner. The first 60 each way are logged regardless, after that only ones
  holding an id. A diagnostic for games that still mix players up when players keep
  one shared SteamID; logs get big, so leave it off otherwise.
- `PEERPARTY_SWAP=inline`: swap SDK functions by hooking kernelbase's `GetProcAddress`
  instead of pointing the game modules' `GetProcAddress` imports at PeerParty (the
  default, which leaves Windows' own code untouched, for compatibility).

The same thing from a prompt (x64 or x86):

```
.\peerparty.exe "C:\Program Files (x86)\Steam\steamapps\common\Some Game\SomeGame.exe"
```

Or set that exe as a Steam launch option (`"…\peerparty.exe" %command%`). Logs go to
`%TEMP%\peerparty\logs\`. Grep for `module`, `call`, `callback`, and `dns`.

What the hook does:

- Walks modules already loaded at startup, then watches for later loads.
- Hooks Steamworks, PlayFab Party, PlayFab Lobby & Matchmaking, and
  PlayFab Core exports, and logs each call. Interface version strings and callback
  ids are included. Hot calls are throttled.
- Hooks `getaddrinfo`, `GetAddrInfoW`, `GetAddrInfoExW`, `DnsQuery_A`, and
  `DnsQuery_W`, and logs the hostname.
- EOS exports are logged the same way.
- Stops the game if an EasyAntiCheat or BattlEye module loads, or, with the EOS layer
  on, when the game starts an EOS Anti-Cheat session (`EOS_AntiCheatClient_BeginSession`
  or `EOS_AntiCheatServer_BeginSession`). The SDK's other anti-cheat exports aren't a
  reason to stop (the shipping SDK has them even when the game never calls them); they
  fail as offline, like other calls the layer doesn't carry.

In a game started through PeerParty, the EOS layer (hook/src/eos) works for any EOS game, x64 or x86, any SDK
version that keeps Epic's append-only struct rule:

- The game's own EOS SDK stays loaded. Lookups of EOS functions in it
  (`GetProcAddress`, `GetProcAddressForCaller`, delay-load) return PeerParty's
  functions, and static imports of it are patched. Game files don't change.
- Connect, Auth, UserInfo, ProductUserId / EpicAccountId, Lobby (+ Search, Details,
  Modification) and P2P are PeerParty's, on the shared LAN core (core/). Every other
  function that could see a PeerParty id fails fast with the SDK's own result
  (tools/eos_table.py classifies all 635 functions in the 1.19 headers); the first call
  of each logs a `gap: eos <function>` line, so a game's log names what it uses outside
  the LAN layer. Platform,
  logging and string helpers stay with Epic's SDK.
- Copies of the same game (same EOS product id) find each other by UDP broadcast on
  port 47590 and connect over TCP. Each copy takes its own identity slot in
  `%LOCALAPPDATA%\peerparty\identity`.
- Sessions (modification, search, details, active sessions, registered players,
  invites) and Presence (status, rich text, data, join info) are PeerParty's too. A
  session whose host set no address reads as the host's LAN IP on the other copies.
- Custom invites (`EOS_CustomInvites_*`): the payload a game sets with
  `SetCustomInvite` goes to the players it names in `SendCustomInvite` as a LAN invite;
  the receiver gets `CustomInviteReceived` and, once the player accepts it in the overlay
  (at once with auto-accept on), `CustomInviteAccepted` (what Epic's overlay sends).
  Request to join (send, accept, reject, the response) works the same way.
- Voice (`EOS_RTC_*`, `EOS_RTCAudio_*`, `EOS_RTCData_*`, the lobby's RTC room):
  rooms are LAN rooms (hook/src/voice.rs, hook/src/eos/rtc.rs). Every LAN lobby has a
  room once the game uses lobby voice; participants, speaking, audio frames
  (`SendAudio` / `AudioBeforeRender`) and room data travel between the copies in it.
  Unless the game feeds and renders audio itself ("manual audio"), PeerParty records
  the communications default microphone (only while a room is sending, and only frames
  with voice in them) through Windows' echo cancellation and noise suppression (the
  Voice Capture DSP; `PEERPARTY_VOICE_AEC=0` skips it) and plays the room's voices on
  the communications default speakers, as raw 16 kHz PCM over the LAN.

In a game started through PeerParty, the Steam layer (hook/src/steamobj.rs, steammm.rs, steamp2p.rs,
steamlan.rs) works for native C++ games and for wrappers (Steamworks.NET, Facepunch):

- steam_api hands out Steam's interfaces as objects with a vtable. PeerParty hands out
  stand-ins: PeerParty answers the lobby (ISteamMatchmaking), P2P (ISteamNetworking),
  friends and local SteamID methods, and every other method jumps straight to the real
  object, so Steam keeps doing the rest. Slot layouts for every interface version
  come from the Steamworks interface headers (tools/steam_vtables.py).
- Steam lobbies are LAN lobbies: create, search with Steam's filters, join, data,
  member data, chat, game server, owner changes, invites. P2P packets ride LAN
  channels, with `P2PSessionRequest_t` until the game accepts.
- The newer networking too (hook/src/steamgns.rs): ISteamNetworkingSockets
  connections to a player's virtual port or to an IP:port (found among LAN players
  listening on that port), listen sockets, poll groups and socket pairs;
  ISteamNetworkingMessages; and ISteamNetworkingUtils reporting the relay network and
  ping data as available. Status changes go to the game's own callback function if it
  set one, else as Steam callbacks.
- Nothing is detected up front: whatever the game fetches gets PeerParty's stand-in,
  and the log says what PeerParty answers (`SteamNetworkingSockets012: PeerParty answers N
  of its M methods`). What PeerParty doesn't implement stays Steam's. Callbacks and call
  results reach the game through its own dispatch (registered `CCallback`/
  `CCallResult` objects, or ManualDispatch).
- The first copy of a game on a PC is shown a SteamID of its own for this PC (unless
  Valve's Steam client runs the game, or `STEAMID` mode is `official`), so players
  never share one; its saves keyed by SteamID start from a copy of the old ones. A
  second copy on the same PC gets a LAN one and a numbered name. Auth tickets between
  LAN players are accepted locally (LEGAL.md).
- Unreliable P2P (Steam and EOS) goes over UDP once a peer has answered a UDP ping;
  until then, or behind a firewall that blocks UDP, it stays on the TCP connection.

In a game started through PeerParty, PlayFab (hook/src/party.rs, hook/src/pfmp.rs, hook/src/playfab/):

- PlayFab Party: every `Party*` export of the game's Party DLL is PeerParty's; the real
  library never starts. A Party network is a LAN lobby, its descriptor serializes to
  `PP1-<guid>`, endpoints are its members, and endpoint messages ride LAN channels
  (`party:<guid>`), reliable or not as the game asks. State changes arrive through
  `PartyStartProcessingStateChanges` in the SDK's documented order. Layouts follow
  Party.h as mirrored by PlayFab's open-source Unity plugin
  (github.com/PlayFab/PlayFabPartyUnity) and Party_c.h from the C++ SDK
  (nuget.org/packages/Microsoft.PlayFab.PlayFabParty.Cpp.Windows).
- PlayFab Party chat (hook/src/party/chat.rs): chat controls, text chat and voice chat.
  A chat control the game connects to a network is announced as a member attribute,
  so the other copies see it created and joined, the way endpoints appear. Text
  (`PartyChatControlSendText`, with its attached data) and voice ride the network's
  voice room (voice.rs, `rtc:party:<guid>`). Chat permissions work as Party.h says:
  none by default, both sides must allow audio, text needs the receiver's
  `ReceiveText`, and incoming audio and text can be muted per chat control. Audio
  input and output selections mean the communications default microphone and speakers (opened only
  while a connected chat control has them selected and isn't muted); the capture,
  render and remote voice streams of Party's audio manipulation work without them.
  Talking indicators, render volume, language and options are kept. Transcription and
  text-to-speech report unsupported, translation and text filtering do nothing. A game
  that never makes a chat control sees Party exactly as before.
- PlayFab's web API: lookups of `<title>.playfabapi.com` resolve to a loopback address
  of the game's own process, where PeerParty answers HTTPS with PlayFab's JSON
  (models from github.com/PlayFab/CSharpSDK). The certificate is self-signed, made with
  Windows' own crypto; only the game's own check of that one certificate is told to
  trust it (crypt32's `CertGetCertificateChain` is hooked in the game). Nothing goes
  into a certificate store, and no other program is affected. The key lives in the
  user key container `peerparty-playfab`.
- PlayFab's newer C SDK (`PlayFabCore.*.dll`, github.com/PlayFab/PlayFabCSdk) uses
  the same service: its HTTP stack (libHttpClient, WinHTTP on Windows) sends the same
  REST calls, so the SDK's own code runs and only the host and certificate check are
  PeerParty's. Loading PlayFabCore turns the service on. The GDK (Xbox PC) build, which
  uses its own curl, is untried.
- Sign-in always succeeds, with PlayFab and entity ids from the player's LAN identity.
  Title data, profiles, CloudScript, catalogs, inventory, groups and telemetry succeed
  with nothing in them, as for a title with nothing configured. Purchases, receipts,
  refunds, coupons, virtual currency, item grants, trades, consuming, moving or revoking
  items, opening containers and Economy v2's inventory changes fail as unavailable
  (`ServiceUnavailable`) in the Client and Server APIs, as they would offline, and so
  does the whole Admin API. The player's statistics
  and user data are kept on disk (`%LOCALAPPDATA%\peerparty\playfab\<game>\`),
  leaderboards list the player's own score, and LAN players are the friends list.
- Lobbies through the web API (`CreateLobby`, `JoinLobby`, `JoinArrangedLobby`,
  `FindLobbies`, `GetLobby`, `UpdateLobby`, `LeaveLobby`, `RemoveMember`,
  `DeleteLobby`, `InviteToLobby`) are the PlayFab Multiplayer library's LAN lobbies, so
  a game can mix the two (tests/sdk/pf_rest_test.py).
- Matchmaking tickets are LAN lobbies. Two tickets in one queue match when each is the
  other's oldest compatible ticket: a host pairs with a joiner whose role it has open,
  platform lists overlap, and text attributes both have (a password or co-op word, a
  mode) are equal. Numbers such as levels aren't compared. PlayFab's real rules live
  on its servers, per title, so this is an approximation.
- No open-source PlayFab server implementation exists to follow (PlayFab's open-source
  LocalMultiplayerAgent only hosts game servers), so this part is PeerParty's own.
- PlayFab Lobby & Matchmaking (hook/src/pfmp.rs): every `PF*` export of the game's
  `PlayFabMultiplayerWin.dll` is PeerParty's. A lobby is a LAN lobby whose connection
  string is `LBL-<id>`; search properties, lobby properties and each member's
  properties are its attributes. `PFMultiplayerFindLobbies` searches the LAN and
  applies PlayFab's filter and sort strings itself (`string_key1 eq 'x' and
  lobby/memberCount lt 4`, `distance{number_key1=5} asc`); the web API's
  `/Lobby/FindLobbies` searches the same lobbies, for games that search over REST and
  join through the library. Owners hand over when they
  leave. Matchmaking tickets go through the same matchmaker as the web API's; a match's
  arrangement string names the member who creates the lobby, and the others join it.
  Lobby invites go to the LAN player with that entity id. Game-server lobbies and
  tickets for several local players aren't carried. Layouts follow PFLobby.h and
  PFMatchmaking.h (github.com/PlayFab/PlayFabMultiplayerUnreal), checked against
  PlayFab's 1.7.2 Unity interop.

In a game started through PeerParty, Photon (hook/src/photon/):

- Lookups of Photon Cloud's hosts (`ns.photonengine.io`, `ns.exitgames.com`,
  `*.exitgames.com`, `*.exitgamescloud.com`, `*.photonengine.io`, `*.photonengine.cn`)
  resolve to one copy of PeerParty on the LAN, which runs Photon's name, master and game
  servers inside the game's process on Photon's UDP ports (5058, 5055, 5056 and the
  27000-27002 alternatives). The first copy to look Photon up waits up to 3 s for a LAN
  peer that already hosts (lowest peer id wins), else hosts itself. Windows Firewall may
  ask to let the game accept connections the first time it hosts.
- Photon has no invites, so rooms go by code. The overlay's Home page shows the room
  you're in (code, who is there; the host publishes the member list in LAN presence so a
  joined player sees it too) with **Copy code**. **Invite** sends the code to the player;
  they get a row in their overlay and **Type it** closes the overlay and types the code
  into the game (it also goes on the clipboard). **Type a copied room code** does the
  same for a code copied from elsewhere. Typing is key presses by scan code
  (hook/src/typing.rs), so the game's code box must be focused first.
- Ported to Rust from Luxon Server (github.com/niansa/LuxonServer and
  gitlab.com/luxon_project/Luxon, BSD-3-Clause; hook/src/photon/LICENSE-Luxon): ENet,
  GpBinary 1.6 and 1.8, encryption (DH, and AuthOnce's secret), any app id signs in,
  regions, lobbies and room lists, create / join / join-random rooms, room and player
  properties, events (interest groups, the event cache), master client switch, leave.
  Photon Voice uses the same rooms. Bolt's Photon transport (udpkit.platform.photon:
  event 150 to one actor, punch info in events 80-82) needs nothing more. Fusion rooms
  that ask for "FusionPlugin" get PeerParty's (hook/src/photon/fusion.rs): Join / Start
  / ReflexiveInfo for Host, Server and Client mode, host-migration snapshots confirmed;
  Shared mode is served by PeerParty's own Shared server (hook/src/photon/shared.rs):
  every player connects to actor 0 through the room
  (event 101), and the server keeps every networked object, takes updates only from an
  object's state authority, forwards RPCs, grants authority requests, follows the master
  client's scene and tells everyone who joined and left. No interest management (every
  player gets every object), and reliable data (`SendReliableDataToPlayer`) isn't
  carried. Photon Chat is served by the master server (chat.rs) and TCP connections by
  host.rs (each becomes an ENet peer of the server's own), and so are WebSocket (`ws://`)
  connections (ws.rs). SQL lobby filters (WHERE and ORDER BY) are evaluated (sql.rs), and
  rooms with a PlayerTTL keep a dropped player inactive until it rejoins. Secure
  WebSockets (`wss://`) and datagram encryption aren't carried.
- Checked with Photon's own .NET client library (tests/photon/run.ps1). In a game's
  log look for
  `photon ns.photonengine.io -> ...`, `photon: serving on`, `signed in as`,
  `created room` / `joined room ... as actor N`.

In a game started through PeerParty, Unity Gaming Services (hook/src/ugs/), for games that ship Unity's Lobby,
Relay or Multiplayer SDK:

- Lookups of `player-auth.`, `lobby.`, `relay-allocations.` and
  `social.services.api.unity.com` and `wire.unity3d.com` go to PeerParty's local HTTPS
  service (the PlayFab one). Sign-in always succeeds as this copy's LAN player; lobbies
  are LAN lobbies (create, query, quick join, join by code, data and player data with
  their visibility, Wire events over a WebSocket); a relay allocation makes the host's
  copy the relay, and a join code finds it on the LAN. Unity Transport's own relay
  protocol (com.unity.transport, Runtime/Relay) is spoken, unencrypted, or inside DTLS
  (pre-shared key, as Unity's relays) for a game that forces a secure relay.
- Trust stays inside the game: Unity's TLS reads the Windows ROOT store itself, so the
  ROOT store the game opens lists PeerParty's certificate too (crypt32's `CertOpenStore`
  hooked in the game only). Nothing is installed in Windows' certificate stores.
- Cloud Save (player, public, protected and custom items, with write locks) and Remote
  Config are answered from disk (`%LOCALAPPDATA%\peerparty\ugs\<game>\`; put a
  `remote-config.json` there to give a game its settings), since the real ones would
  refuse PeerParty's sign-in. QoS, Matchmaker and Vivox are left alone; WebSocket
  relays (WebGL only) aren't carried. References are in docs/design.md, "Sources".
- In a game's log look for `ugs: the game ships ...`, `tls: the game read the ROOT certificate
  store`, `ugs POST lobby.services.api.unity.com/v1/create`, `ugs relay: allocation`,
  `join code`, `connected to host` (and `DTLS session with ... open` when the game
  forces DTLS).

Offline (no internet at all, or the online services shut down):

- Steam: lobbies, P2P, NetworkingSockets, friends and invites are all LAN. Steam in
  offline mode is enough: `BLoggedOn` reports signed in in a game started through
  PeerParty.
- EOS: sign-in, lobbies, sessions, presence and P2P are PeerParty's; Epic's servers are
  never needed.
- PlayFab: answered locally as above, also for games that only use PlayFab's web API
  once `playfabapi.com` stops resolving.
- Photon: rooms are hosted by a copy on the LAN; Photon Cloud is never contacted.
- Unity Gaming Services: sign-in, lobbies and relays are PeerParty's (QoS and other
  Unity services are not).
- Finding each other: by UDP broadcast on the LAN, and copies on one PC also through a
  peer board (`%LOCALAPPDATA%\peerparty\peers`), which needs no network adapter at
  all.

### Internet play

Friends in other homes play as if they were on the LAN, with no server of ours: copies
that share a **room code** find each other through the public BitTorrent DHT, punch
through both routers with UDP, and carry PeerParty's LAN connection through an encrypted
tunnel. Off unless a code is set. How it works and its limits: docs/internet-dht.md.

1. One player makes a code: the launcher's Play together > Friends online > **Make a room**, or
   `peerparty --new-room`. It looks like `PP1-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-CC`.
2. Send it to your friends privately (anyone with it can join), and everyone pastes it
   into Play together > Friends online > **Join with a code**. Check that the room's name
   ("Room 1a2b3c4d") matches.
3. Start the game through PeerParty as usual (the launcher window does). Within about a minute
   the other players show up like LAN players: lobbies, invites, P2P and voice. The log
   says `inet connected to NAME (...) at peer-xxxxxx` and `... now runs through the
   tunnel` (addresses are logged as short tags; `PEERPARTY_INET_DEBUG=1` shows them).

- `peerparty --net-test`: checks this PC's path to the internet (STUN, the router's NAT
  type, the DHT) and says whether hole punching should work. No friend needed.
- `--room CODE` (or `PEERPARTY_ROOM=CODE`): the code for one run; `--room off` turns it
  off.
- The **Internet play** switch (the overlay's Network page, the launcher's Play together,
  `--set internet=on|off`, or `PEERPARTY_INTERNET=0` for one run; the environment wins)
  turns internet play off and on without clearing the code. It takes effect at once in a
  running game: off ends the room and public sessions and disconnects their players,
  players on your local network stay connected; on brings the sessions back and the
  others reconnect within about a minute.
- Windows Firewall may ask to let **the game** use the network (the tunnel runs in the
  game's process). Allow it. Most home routers need nothing else. If both homes' routers
  are strict ("symmetric" in `--net-test`, common on mobile and CGNAT connections), one
  player forwards a UDP port on their router and sets `PEERPARTY_INET_PORT` to it.
  PeerParty asks the router to forward its port itself (UPnP, NAT-PMP or PCP; off with
  `PEERPARTY_INET_PORTMAP=0`), and uses IPv6 directly when both PCs have it (off with
  `PEERPARTY_IPV6=0`).
- **Firewall warnings.** Cancelling Windows' firewall prompt leaves rules that block the
  game. For games played through PeerParty, the launcher's game panel (Who can join you) and
  sidebar, and the overlay (Home, Network, and a notification), say when Windows Firewall
  blocks the game on the network in use, or hasn't been told to let it in yet. **Fix**
  (or the overlay row's button) asks Windows for permission, turns off the Block rules
  for the game and adds an Allow rule (private and domain networks; public ones only when
  you choose so). If the prompt is declined or the change fails, the firewall's
  "Allowed apps" settings open instead. Over an exclusive-fullscreen game the overlay
  asks you to Alt+Tab to the launcher, since the prompt would hide behind it. When
  players on your network are heard but can't be reached, the overlay says so: their
  PC's firewall is the likely cause, and they fix it on their PC.
  `peerparty --firewall game.exe` prints the same check; `peerparty --fix-firewall
  game.exe [--public]` (as administrator) does the fix. Rules set by an organisation's
  policy are reported, not changed.
- `PEERPARTY_INET_PORT=N`: a fixed UDP port; `PEERPARTY_INET_PEERS=host:port,...`: also
  punch these addresses (a forwarded port, a VPN; IPv6 as `[addr]:port`); `PEERPARTY_INET_DHT=0`: those
  addresses only; `PEERPARTY_INET_BOOTSTRAP`, `PEERPARTY_INET_STUN`: other DHT routers or
  STUN servers (`0`: none); `PEERPARTY_LAN_DISCOVERY=0`: no LAN beacons (to test the
  internet path on one LAN).
- Limits: anyone with the code is trusted like a LAN player. PeerParty never shows a
  room member's public IP (the game sees a stand-in address in 198.18.0.0/15, the overlay
  says "your room"), but their PCs still see it at the network level. Games that connect
  by typed IP address use the stand-in shown on the overlay's Network > Connection details, and the hook
  carries that traffic through the tunnel. Each member's player id is bound to its key
  (`room-keys.txt`), so one member can't pass as another. Both sides on symmetric NATs
  can't connect without a forwarded port (there's no relay).

### Public sessions

Opt-in: find **anyone** online who is playing the same game through PeerParty, no code
needed. Their lobbies show up in the game's own session browser like LAN lobbies. Turn it
on in Play together > Anyone online > **Public sessions** (pick the name others see there), or
for one run with `--public` or `PEERPARTY_PUBLIC=1`. Room codes and LAN play are
unchanged and work at the same time. Details: docs/public-sessions.md.

- Every player has a signed key. Adverts that are forged, altered or replayed are refused,
  and a known player's key can't be swapped (pinned on first use).
- Public players get lobbies, invites, game traffic (all of it, PlayFab Party's
  included) and voice. Voice and party chat with them is on by default; turn it off in
  Play together > Anyone online > **Voice chat with strangers**, with
  `--set public_voice=off`, or with `PEERPARTY_PUBLIC_VOICE=0` (the environment wins).
  "Invite everyone on your network" never reaches them, and their invites always wait
  for you to accept.
- Block a player from the overlay (Players > their Details > **Block this player**) or
  with `peerparty --block FINGERPRINT`. `--blocked` lists them and `--unblock FP|all`
  removes them (also Settings > Unblock all).
- Public IPs are **never shown**: not in the overlay, Settings, names, logs (short tags
  unless `PEERPARTY_INET_DEBUG=1`) or anything the game sees. **But** players you connect
  to can still see your IP at the network level, as with any peer-to-peer game. Use room
  codes, or a VPN, if that matters.
- Limits: caps of 16 public sessions, 4 per player key and 32 per address (so a house
  full of players behind one router works), and handshake rate limits. Checked on the
  real DHT with two copies on one PC, and between two homes (the PCs found and reached
  each other).

### LAN first

When a friend is reachable both on your LAN and through a room code or public session,
PeerParty uses the LAN. If they show up on the LAN mid-game, they move to it **without
leaving** (same player, same lobby, nothing lost or reordered), and the internet tunnel is
closed. If the LAN goes, the tunnel takes them back in about 5 s (they leave and rejoin). A
public player who turns out to be in your room or on your LAN is moved there and trusted
like any room or LAN player. On guest Wi-Fi or a router with client isolation, LAN
discovery can't work: use the main network, or add the other PC's address as an extra
peer (`PEERPARTY_PEERS`).

### The overlay

peerparty.exe already stays running beside the game it started (it hands the game's exit
code to a .bat or Steam), so it also draws an overlay for it. The game only ever sees
PeerParty's SDK answers and, while the overlay shows, its controllers held still.

- Opens with **Shift+Tab** (Steam's and GOG's key) or **Shift+F3** (Epic's), optionally
  Shift+F1 (EA) and Shift+F2 (Ubisoft), any controller's **Home / Xbox Guide** button
  (it only opens the overlay; B or Start closes it), or a **controller chord** you record
  in the overlay (Settings > Open the overlay with > Controller chord: hold two or more buttons; then holding
  them a second opens it; none by default, since Windows takes View + Menu for its
  on-screen keyboard). Every key that's on works in every game: a game
  started through PeerParty isn't started through its store, so the store's own overlay
  doesn't come up on the same key. Windows may open Game Bar on the Home
  button too (Settings > Gaming > Game Bar turns that off; the overlay says so once).
  A key or Home button counts only once it has been seen up while the game is in front:
  a controller that appears with Home down (Steam Input swapping in its own pad as a
  Steam game starts) or Home still held from closing Game Bar opens nothing. The game's
  log names what opened it (`overlay: opened by Home / Guide on XInput slot 0`).
  A hotkey that can't open it says why (`overlay: Shift+Tab pressed but the overlay
  can't open: the game isn't the window in front (...)`), and `overlay: controllers
  read:` lists the controllers it reads each time they change: a Home button that does
  nothing on a controller missing there isn't reaching Windows (a remapper's virtual pad
  that failed, say).
- The game's own buttons open it: Steam's `ActivateGameOverlay("Friends" ...)`,
  `ActivateGameOverlayToUser`, the invite dialogs (`ActivateGameOverlayInviteDialog`,
  `...ConnectString`), EOS's `EOS_UI_ShowFriends` / `ShowNativeProfile` /
  `ShowBlockPlayer` / `ShowReportPlayer`, GOG's `ShowOverlayInviteDialog`. The invite
  dialogs open the Players page in invite mode (pick players, or everyone). Games hear it
  open and close as their store's overlay (`GameOverlayActivated_t`, EOS's display
  settings), so they pause or free the cursor as they do for Steam's.
- Pages: **Home** (what needs you now: a firewall that keeps players out, with Fix;
  invites waiting, accepted with A / Cross or declined with X / Square; Invite everyone
  on your network;
  who else is playing; the lobby you're in and its members; how you're online),
  **Players** (invites waiting, then every other player of the game, on your network,
  in your room or in public sessions: A / Cross joins them (asks them to invite you),
  X / Square invites them; Details for players in your lobby, and Block for public
  players; opened from the game's invite button, A invites; a different version of the
  game is flagged), **Network** (this PC's Windows Firewall, which keeps out players at
  home as much as online, with Fix; players on your network and those that can't be
  reached; the internet play switch; your room and public sessions; **Connection
  details**: the router, port forwarding, ping per player and the stand-in addresses)
  and **Settings** (notifications, invites, size, button pictures; **Open the overlay
  with**: the keys, the Home button and the controller chord). B goes back from a
  player's details, the connection details and the keys, and closes from a page.
  Players and Network show a badge when an invite waits or something keeps players
  out.
- Controllers first: d-pad / stick moves, A / Cross acts, X / Square is the second
  action, B / Circle goes back, LB / RB (or left / right on a row without a value)
  switch pages, Start closes (Home only opens), a click on the dimmed game closes. Xbox pads
  (and anything presented as XInput), DualShock 4, DualSense, Switch Pro and other HID
  gamepads are read by peerparty.exe itself; glyphs follow the pad in use. Keyboard
  (arrows, Enter, Esc, Q / E, Tab) and mouse work too.
- Nothing reaches the game while it shows: keyboard and mouse are taken by low-level
  hooks in peerparty.exe (only while open), controllers are held still in the game
  (XInput, DirectInput and raw-input HID at rest; Windows.Gaming.Input's Gamepad and
  RawGameController readings at rest; GameInput readings standing still at the one from
  when it opened, and its reading callbacks not called, in every GameInput API version;
  HID devices read directly, SDL's HIDAPI, repeating their first report).
- Xbox Game Bar opens over the game without taking the foreground, so PeerParty looks for
  it (a hit test of the game's area): while it shows, the overlay leaves the
  controller, keyboard and mouse to it and stays open underneath (also when one Guide
  press opened both); close Game Bar and the overlay takes input again. The VK_GAMEPAD keys Windows makes from
  controllers for apps like Game Bar always pass.
- Notifications (an invite, a player arriving) appear in the top-right corner while the
  overlay is closed.
- Drawing: the overlay is rendered by egui into a software canvas in peerparty.exe, then
  shown in a layered window over a windowed / borderless game, or, for exclusive
  fullscreen (or `draw=ingame`), drawn inside the game's frames by hooks on Direct3D 9,
  10/11, 12 (D3D11On12), OpenGL and Vulkan presents (hook/src/present/). Those hooks go in
  only when first needed (Vulkan's at load, and only with `draw=ingame`: a hook at start-up
  is the one thing that touches a game that never opens the overlay), and while the overlay is
  closed a frame costs the game one flag check. Vulkan draws with a small pipeline of its
  own (Dear ImGui's precompiled shaders), blended like the other APIs.

Friends and invites (hook/src/steamlan.rs, eos/, invite.rs):

- Every other copy of the game connected through PeerParty is a friend: in Steam's friend list (after
  your real friends, online and playing this game, with its rich presence and the
  lobby it's in) and in EOS Friends and Presence. A game's own "join friend" button
  then works through PeerParty.
- An invite is the game's Steam `connect` rich presence or EOS join info, and/or the
  lobby or session it's in. The game's own invite button (Steam's invite dialogs,
  `InviteUserToGame`, `InviteUserToLobby`, `EOS_Lobby_SendInvite`,
  `EOS_Sessions_SendInvite`) sends one. The receiving copy accepts it at once through
  the SDK it listens on: Steam's `GameLobbyJoinRequested_t` or
  `GameRichPresenceJoinRequested_t`, or EOS's invite accepted / join accepted / join
  game accepted.
- Arriving invites wait for the player: the game hears it arrived
  where its SDK can (EOS's and Steam's "invite received", GOG's invitation), a
  notification shows, and accepting in the overlay sends the SDK's "accepted" event.
  "Accept invites automatically" (overlay or launcher settings,
  `PEERPARTY_INVITES=auto`) accepts them at once.
- For games without an invite button, these invite every other player on your network
  to where you are, in the game whose window is in front: the overlay's "Invite everyone
  on your network", the window's "Invite players at home", or `peerparty --invite`; and,
  only when turned on, the Home button or View + Menu held. Friends in your room and
  public players are never invited this way: invite them one at a time from the
  overlay's Players page. A chime confirms it; an error chime means you aren't in a
  lobby yet.
