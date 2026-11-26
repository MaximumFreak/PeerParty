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
through PeerParty. It opens where it was last left, at the size it had (maximized if it
was), or centred the first time and when that place is on a monitor no longer
connected. It finds what's installed on any PC:

- **Steam**: every library in `libraryfolders.vdf`, with Steam's cached box art.
- **Epic**: the launcher's install manifests (`Data\Manifests\*.item`).
- **GOG**: GOG Galaxy's registry entries.
- **Game folders**: every subfolder of the folders in the list, `C:\Games` to begin
  with. Add others, or remove `C:\Games`, in Settings > Library. Folders starting with
  `.` are skipped. A folder's program is picked by `launcher/src/ui/mainexe.rs`:
  a Unity game's exe, else one named after the folder, else the largest, skipping
  redistributables and old-renderer builds beside a newer one and ranking tools (an
  editor, a packer, a server) last. If it picks the wrong one, the program under the
  game's name in its panel lists the others in its folder.
- **Recent**: every game peerparty.exe starts (from the window, a .bat or a Steam
  launch option) is noted in `%LOCALAPPDATA%\peerparty\recent.tsv`, except a test run
  (`PEERPARTY_LOOPBACK=1`).
- **Add > Add a game…** picks any .exe.

The sidebar has three places:

- **Library**: the games, as covers or a list, with filters (All, Recent, each store,
  Games folder, Added, Hidden) and a search (Ctrl+F) that looks through All whichever
  filter is picked (from Hidden, through the hidden games). A game's panel has its
  poster, Play, the number of players on this PC (at 2–4 a card with **Split screen** to
  keep every player in one shared window, its layout, monitor and what each player plays
  with, or separate windows at 2 players with **Second instance starts after**, and the
  players' names), **Copy Player
  1's saves to Player N** (or to the other players in split screen; shown at 2–4 players, or at 1 once an instance runs and Play
  has become **Start another instance**, and not while an instance counts down; for
  the next instance only: it turns itself
  off once used, when no instance is left at 1 player, and every time the launcher
  starts, and warns that a save the new instance already has is replaced, with a
  backup kept), and **Player 2's name** (shown at 2 players, or once an instance runs;
  the name the second instance on this PC goes by, `PEERPARTY_OTHER_NAME`; players 3
  and 4 in split screen). While the
  second instance waits, Play reads **Start instance 2 now** and the Now playing card has
  Start now and Cancel. The Now playing card's **Close all** (after a yes) closes every
  game the window started, each instance: their windows are asked to close, and what
  still runs a few seconds later is ended. Then **Who can join you** (people on your network, with a
  Windows Firewall check and Fix; your room; public sessions; one line when two or more
  play on this PC, unless the firewall blocks the game), **This game's own
  settings** (invites, overlay, voice and saves for this game alone) and **Advanced**
  (multiplayer through PeerParty on or off, online services, PeerParty layers to turn
  off, "Meet players of", the program, extra arguments, the exact
  `peerparty` command and a .bat shortcut). A dot on a folded section says something in
  it differs from the recommended. Double-click or Enter plays; right-click for Hide
  and the shortcut.
- **Play together**: player names (yours, and players 2–4 on this PC) and who can join
  you in every game. People on your network always can; **Friends online** makes or
  joins a room code (copy, show, leave); **Anyone online** turns on public sessions
  (with the name strangers see, voice with them, and the blocked players; what
  strangers learn shows while it's on, or being turned on). Invites and voice chat are
  here too, and, once there's a room or public sessions, the **Internet play** switch
  that pauses them.
- **Settings**: General (language, lighting and animations, what the window does when
  a game starts), Overlay (on or off, notifications, the keys and controller buttons
  that open it, with **Record** for a controller chord, its size, button pictures and
  how it's drawn), Library (game folders, hidden games), Advanced (online services,
  existing saves under a new Steam player ID, logs, the first-run setup, Reset to
  recommended) and About. A running game takes the overlay's settings and Accept
  invites automatically from here within a second.

The first time the window opens it asks three things (language and name; how you play:
split screen on this PC, at home, friends online, anyone online; and what that means:
Player 2's name for split screen, voice and invites when something online is picked)
and writes them only when you press Start. Its last step says how to start split
screen and shows a new room's code to copy. Skip leaves everything as it was. A
`ui.json` from an older PeerParty counts as set up. Settings > Advanced > Run setup
asks again.

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

Settings live in `%LOCALAPPDATA%\peerparty\ui.json` (one that can't be read as
settings is kept as `ui.json.bad` before defaults replace it). The window is egui (Rust,
`launcher/src/ui/`); what it asks of the operating system goes through
`launcher/src/platform/`, and nothing in it is specific to one PC.

**Portable.** A `peerparty-data` folder beside peerparty.exe holds everything that
would go in `%LOCALAPPDATA%\peerparty` (`~/.local/share/peerparty` on Linux): settings,
players' identities, the stand-ins' saves, the hook DLLs. Settings > About > Keep
PeerParty's files beside peerparty.exe moves them there (or back) and restarts the
window; it refuses while a game PeerParty started still runs. Making an empty
`peerparty-data` folder by hand works too: PeerParty then starts fresh there. Games
started by a portable PeerParty keep their saves there as well (under Proton too: the
hook is given the folder as Wine sees it). Logs stay in the temp folder.

peerparty.exe is a Windows (GUI) program, so double-clicking it opens no console. From
a prompt it prints to that console; .bat files and Steam wait for it and get the
game's exit code (all 32 bits of it, so a crash reads as `-1073741819`, 0xC0000005).
When the game exits, PeerParty adds a line with the exit code, how long the game ran
and what a crash code means to the end of that run's log (`[launcher 17:32:07] game
exited after 0.3 s with exit code 3221225477 (0xC0000005), crash: access violation`),
starting a log for it if the game died before the hook could open one. A game that
didn't start at all gets a log of its own saying why (`<exe>-<date>-<time>-launcher.log`,
for example when PeerParty couldn't attach to it). PowerShell doesn't wait for GUI programs: use
`Start-Process -Wait` there if a script needs to.

### Updates

Once a day the window asks GitHub whether a newer PeerParty is out (Settings > About >
Updates, where "Check for updates every day" turns it off and "Check now" asks at once).
Nothing downloads or installs by itself. **Download** opens the new `peerparty.exe` in
your browser, which scans and saves it like any download; the window finds it in your
Downloads folder ("Saved it somewhere else…" if you put it elsewhere) and checks it is the
release's file: a program with the size and SHA-256 GitHub publishes for it (read from
GitHub's releases API over HTTPS). For a release GitHub has no SHA-256 for, a plain
checksum file published beside the exe (`peerparty.exe.sha256`) is used instead; with
neither, the window says it can't check the download and offers only the release page.
Nothing is signed. **Install and
restart** renames the running exe to `peerparty.exe.old`, copies the new one into its
place and starts it; any step that fails puts the old one back. It waits until no game
runs through PeerParty. PeerParty never downloads a program itself, and runs no scripts
to update. A copy that came with another app's add-on only says that a newer version is
out: update the add-on to get it.

### From a prompt

```
peerparty [--room CODE] [--log-dir DIR] [--multi-instance] [--copy-saves] [--player N] [--set key=value]... <game.exe> [args...]
peerparty --new-room
peerparty --net-test
peerparty --saves <game.exe>
peerparty --copy-save FROM TO <game.exe>
peerparty --swap-saves A B <game.exe>
peerparty --reset-save N <game.exe>
peerparty --restore-save N BACKUP <game.exe>
```

The launcher window's settings (`%LOCALAPPDATA%\peerparty\ui.json`: global ones and
each game's own) apply however the game is started: from the window, a shortcut or
Steam. `--set` changes one for a single run: `name=Alex`, `other_name=Sam`, `auto_accept=on`, `overlay=off`,
`toasts=off`, `voice=off`, `draw=window|ingame`,
`mode.<service>=lan|official|auto`, `room=CODE` (empty: off), `public=on`, `internet=off`,
`language=de` (empty: the system's), `effects=off`. A variable already set in the environment wins over
both.

- Every game started through PeerParty plays on PeerParty's own multiplayer layers
  (EOS; Steam lobbies, P2P, friends and invites; PlayFab Party, Lobby & Matchmaking, and
  PlayFab's web API; Photon rooms; Unity's Authentication, Lobby and Relay), unless a
  service is set to the official servers. Everything works with no internet: see
  "Offline" below.
- `peerparty --invite`: the game in front invites every other player to where it is.
  It says so, or fails (exit code 1) when no game took it. The window's "Invite local
  players" asks the game that was last in front.
  Bind it to a hotkey for games without an invite button.
- `PEERPARTY_INVITES=auto`: invites from other players are accepted at once. The
  default is `ask`: the game hears an invite arrived (where its SDK has
  that event), a notification appears, and the player accepts or declines in the
  overlay.
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
- `PEERPARTY_OTHER_NAME`: the name a second copy of a game on this PC goes by instead
  of "name (2)" (the third and fourth stay "name (3)", "name (4)": other players again).
  Set from a game's page ("Player 2's name", shown only at two players or with an
  instance running); the first copy keeps `PEERPARTY_NAME`.
- Programs the game starts: PeerParty works out the game's install folder (its Steam
  library folder, Epic's install list, the folder under one of the window's game folders,
  Unreal's layout, or the program's own folder above ones named like `bin` or `Win64`;
  `PEERPARTY_GAME_ROOT` sets it) and goes into programs started from inside it, so a
  launcher or starter program in the game's folder passes PeerParty on to the game. The
  overlay goes to the program that loads a game SDK or opens a window of a game's size,
  and so does your player name: a starter never takes it, so the game isn't "Name (2)".
  Never followed: anything outside that folder, anti-cheat starters, installers of
  what the game needs (`_CommonRedist`, `vcredist`, ...), crash reporters, and anything
  the game starts once it has loaded a game SDK itself. PeerParty runs until the last of
  the game's programs in that folder ends, and hands back its exit code.
- `PEERPARTY_FOLLOW=1`: PeerParty goes into every program the game starts (on by itself
  in packaged games, for the programs of their own package).
- A `.bat` or `.cmd` in place of the exe: PeerParty reads it and starts the program it
  runs, with its arguments, folder and variables (`@echo off`, comments, `cd`, `pushd`,
  `set`, `start ""`, `%~dp0`, `%*` and `%1`..`%9`; one program line). A script that does
  more (`if`, `goto`, `call`, a second program) is refused with a message to start the
  game's .exe instead.
- A game that asks Epic's SDK whether Epic's launcher started it, and would restart
  itself through the launcher, is told to carry on when PeerParty started it.
- When a game exits with a non-zero code, its log says so with the modules on the
  caller's stack (`exit: ...`), which explains an `abort()` that leaves no crash dump.
- A Steam game started outside the Steam client needs its app id. PeerParty finds it in
  order: the `SteamAppId` variable (Steam sets it), the app manifest of the Steam library
  the game is in, a `steam_appid.txt` next to any program in the game's folder (the one
  PeerParty starts first), any Steam library's manifest whose install folder has the name
  of one of the exe's folders. Programs the game starts inherit the id, so next to a
  starter or next to the program it starts both work. When none has it and the game uses
  Steam (a `steam_api` in its folder; not Epic's or GOG's copies), the game's panel and
  the log say so: put a `steam_appid.txt` holding the id (the number in the address of
  its Steam store page) next to the game's program. The Steam client still has to be running, and stays in charge of the
  game's licence.
- `--multi-instance`: let a second copy start on the same PC: a named mutex that reads
  as a single-instance check (Unity's, or one whose name says "instance" or holds the
  game's program name; stores' and Windows' own are left alone) is made per copy, and
  FindWindow doesn't find the other copy's windows.
- `--split-screen`: start two to four copies on this PC in one shared split-screen
  window. Its options, keys and Layout Studio are under **Split screen**, below.
- **Players' saves.** Player 1 plays with the game's saves where the game keeps them,
  as it does alone. Players 2 to 4 each have their own, in PeerParty's folder
  (`%LOCALAPPDATA%\peerparty\saves\<game>\player-N`): files the game keeps in
  `AppData\LocalLow`, `AppData\Local`, `AppData\Roaming`, Documents, Saved Games and
  Steam's `userdata`, its registry settings under `HKCU\Software\<company>` (Unity's
  PlayerPrefs), and its Steam Cloud files. A new player starts with none (the game
  starts fresh), unless "Start new players from Player 1's saves" is on. In the game's
  own folder a player reads the game's files and writes its own copies; the game's
  files are never changed. Nothing is written beside player 1's saves, so Steam Cloud
  and the game never see the other players' files. The game page's **Saves** card (shown
  with two players or more, or once a later player has saves) lists each player's saves
  and backups, copies or swaps them between any two players (`Copy`, `Swap`), resets a
  player (`Reset`: a fresh start, kept as a backup) and restores a backup; whatever is
  replaced is backed up first, the five newest backups per player are kept, and nothing
  changes while the game runs. Saves named by a player's id (its SteamID, PeerParty id
  or PlayFab id) are renamed to the other player's on the way. `Play solo` (or `Play as`
  on the game page, `--player N` on the command line) starts the game alone as that
  player, with its saves and name, in a normal window with voice chat; a player can only
  be playing once at a time. When where player 1's saves are isn't known yet (the game
  hasn't run with two players), **Pick save folder…** says so. The same on the command
  line: `--saves`, `--copy-save FROM TO`, `--swap-saves A B`, `--reset-save N` and
  `--restore-save N BACKUP`, players numbered 1 to 4. Saves made by an older PeerParty
  beside player 1's (`<save>.peerparty-2`) are taken over the first time the game uses
  that folder.
- `--copy-saves`: later players start from a copy of player 1's saves, for games that
  don't offer multiplayer from a fresh save: the launcher copies them before the game
  starts (what the player had is backed up), and the game copies a folder it doesn't
  know yet the first time it uses it; saves under player 1's id (SteamID, EOS id,
  PlayFab id) become the player's own ids'. A save over 1 GB isn't copied. Without the
  flag nothing is copied, and `PEERPARTY_COPY_SAVES` is cleared, so a game library,
  shortcut or command-line start never copies unless asked.
- Player 1's own Steam saves: when the first copy is shown a SteamID of its own and
  that SteamID has no save folder yet (or only an empty one), it starts from a copy of
  the save of the id this PC used before: first one recorded in
  `identity\slot-0.previous` (an old identity is recorded there before it is retired),
  then, with no record, the newest-modified sibling save folder of a PeerParty SteamID
  that isn't another copy's (every candidate and the choice are logged), then
  steam_api's own. A save for the new id that already has files is never touched.
- `PEERPARTY_PLAYFAB=0` / `=1`: never / always answer PlayFab's web API locally
  (default: for games that ship or load PlayFab Party, Lobby & Matchmaking or the C SDK,
  decided at the first lookup since games sign in before loading them, and for any game
  when PlayFab can't be reached).
- `PEERPARTY_PHOTON=0`: leave Photon alone (by default, in a game started through PeerParty, Photon Cloud's hosts
  resolve to the LAN's Photon servers).
- `PEERPARTY_UGS=0` / `=1`: never / always answer Unity Gaming Services' Authentication,
  Lobby and Relay locally (default: for games that ship Unity's Lobby, Relay or
  Multiplayer SDK).
- `PEERPARTY_VIVOX=0` / `=1`: never / always answer Vivox voice chat on the LAN
  (default: whenever Unity Gaming Services are answered locally, since Vivox would
  refuse that sign-in).
- `PEERPARTY_SEPARATE_SAVES=0` (the Saves card's "Shared with Player 1"): players 2 to 4
  play with player 1's saves, as one player would.
- `PEERPARTY_SAVE_PATHS=dir;dir`: more folders that are the game's saves (each player
  gets its own copy of them); `PEERPARTY_SHARED_SAVE_PATHS=dir;dir`: folders every
  player shares even where they'd be a player's own (a large mods folder in Documents).
- `PEERPARTY_PLAYER_SLOT=N` (0 is player 1), `PEERPARTY_SAVE_GAME`, `PEERPARTY_SAVE_SEED`,
  `PEERPARTY_SAVE_WATCH`: set by the launcher for each copy (which player it is, the
  game's saves folder, "Start new players from Player 1's saves", player 1's copy noting
  where its saves are).
- `PEERPARTY_DEMONWARE=0`: PeerParty doesn't answer Demonware (Activision's online
  service), and the game reaches the real servers.
- `PEERPARTY_LOOPBACK=1`: every socket binds to 127.0.0.1 and no beacons are broadcast,
  so copies on one PC meet only through the peer board and Windows Firewall never asks.
  The tests set it (and `cargo` sets it for what it runs, .cargo/config.toml).
- `PEERPARTY_PEERS=host[:port],...`: extra beacon targets for networks that broadcast
  doesn't cross (VPNs, other subnets).
- `PEERPARTY_SKIP=layer,...`: turns PeerParty layers off for a game that objects to
  one: `swap`, `invite`, `eos`, `steam`, `party`, `pfmp`, `playfab`, `photon`, `ugs`, `galaxy`, `demonware`, `dns`,
  `padblock` (holding controllers back while the overlay shows), `present` (drawing the
  overlay inside the game), `ipfwd` (typed-IP forwarding to internet players, and the
  path to another PC's Photon servers; without it they're reached directly, which that
  PC's firewall must allow), `vivox` (voice chat), `xboxlive` (the Xbox Live friends
  list), `xgameui` (Xbox invite screens in Microsoft Store games), `curl` (the hooks on
  the game's own libcurl, below), and
  `calls` (the call logging in SDK modules). The launcher
  window's game panel lists all but `calls` and `curl` under Advanced, Parts of PeerParty, each
  switched on unless turned off for that game. When a turned-off layer is one the game
  needs (it loaded that service's SDK or looked up its servers; the general layers
  always), the overlay's Network page says so (and Players, at its top), with what it
  costs and where to turn it back on, and Invite names it instead of asking to host
  first.
- `PEERPARTY_GAME=name`: the game key players meet under, normally the exe's name
  less a dedicated server's "server" word, with any build tail kept
  (`mygame_server` meets `mygame`, `MyGameServer-Win64-Shipping` meets
  `MyGame-Win64-Shipping`, `MyGameServer_x64` meets `MyGame_x64`); for a dedicated
  server whose exe differs from its client's in some other way (Valve's `srcds`, or a
  server named apart from its game).
- `PEERPARTY_DEDICATED=1` (or `0`): this program is (or isn't) a dedicated server.
  Normally PeerParty tells by itself: a program that starts a Steam game server and
  never Steam's client API is one, whatever its exe is called (some games run their
  own exe as a server, with a switch); otherwise its exe's name says so (a "server" or
  "dedicated" word before any build tail like `-Win64-Shipping-Cmd` or `_x64`, or
  `srcds` / `hlds`). A dedicated server takes
  an identity slot of its own (`server-N` in `%LOCALAPPDATA%\peerparty\identity`,
  named "<your name>'s server"), never a player's, so a server started before the game
  doesn't leave the player with a numbered name and a second copy's ids. Other copies
  list it in the overlay as a server.
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
- `PEERPARTY_STEAM_WEB=0`: Steam's Web API host (`api.steampowered.com`) goes to Steam
  as it is, instead of PeerParty answering whether Steam is up; `=1` answers it in games
  that aren't Unity games too (only if the game's TLS trusts the system's roots).
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
- `PEERPARTY_LIBRARY=<folder>` (the window only): the library shows only the Steam games
  in a folder laid out like Steam's (`steamapps\appmanifest_<id>.acf`, each with its
  `steamapps\common\<installdir>` holding an .exe, and covers in
  `appcache\librarycache\<id>_library_600x900.jpg`), and none of the PC's stores or game
  folders: for a recording or screenshot that mustn't show your own games. An
  `installdir` that is a full path lists a game installed elsewhere, so a real one can be
  started from among them.
- `PEERPARTY_HTTP_TRACE=1`: log the HTTP calls the game makes, through its curl or
  WinHTTP, to the services PeerParty stands in for (PlayFab, Unity Gaming Services,
  Xbox Live, Demonware, Steam's Web API), with their URLs and bodies (up to 4 KB each;
  never their headers). Other hosts aren't logged. `PEERPARTY_XBL_TRACE=1` does the same.
- `PEERPARTY_PATTERNS_DIR=folder`: the folder of pattern files to read instead of
  `%LOCALAPPDATA%\peerparty\patterns` (below).

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
  ids are included. Hot calls are throttled. For an SDK whose functions PeerParty
  already hands the game by name (steam_api, EOS, PlayFab Party and Multiplayer, Vivox), the game is handed
  each function's logging stub that way and the SDK's code isn't patched at all (some
  games check it); the `module` line says which (`calls_logged=`).
  `PEERPARTY_CALLS_SKIP=prefix,...` leaves exports whose names start with one of the
  prefixes (`*` for all) out of the logging, to find one a game objects to.
- Hooks `getaddrinfo`, `GetAddrInfoW`, `GetAddrInfoExA/W`, `gethostbyname`,
  `DnsQuery_A/W` and `DnsQueryEx`, and logs the hostname.
- EOS exports are logged the same way.
- Doesn't start a game at all when its program is an anti-cheat starter or has an
  anti-cheat module (.dll) beside it (launcher/src/anticheat.rs); an installer in a
  subfolder alone isn't enough.
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
  `%LOCALAPPDATA%\peerparty\identity`: a signing key, whose hash is the player's id.
  A connection counts as a player's once that key has signed for it.
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
  the microphone (only while a room is sending, and only frames with voice in them)
  through Windows' echo cancellation and noise suppression (the Voice Capture DSP;
  `PEERPARTY_VOICE_AEC=0` skips it) and plays the room's voices on the speakers. Voice
  travels as Opus (about 3 KB/s per speaker; a frame Opus can't take as it is goes as
  raw PCM). The device lists are Windows' own microphones and speakers;
  EOS's default is Sound settings' default device, and the game's device settings move
  PeerParty's microphone and speakers (AudioDevicesChanged fires when Windows' list
  changes).

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
  LAN players are accepted locally (LEGAL.md): a copy without Valve's Steam client puts
  a signed proof of its player on the end of its tickets, and a ticket for a player that
  isn't theirs is refused.
- Unreliable P2P (Steam and EOS) goes over UDP once a peer has answered a UDP ping;
  until then, or behind a firewall that blocks UDP, it stays on the TCP connection.

In a game started through PeerParty, PlayFab (hook/src/party.rs, hook/src/pfmp.rs, hook/src/playfab/):

- PlayFab Party: every `Party*` export of the game's Party DLL is PeerParty's; the real
  library never starts. A Party network is a LAN lobby, its descriptor serializes to
  `PP1-<guid>`, endpoints are its members, and endpoint messages ride LAN channels
  (`party:<guid>`), reliable or not as the game asks. State changes arrive through
  `PartyStartProcessingStateChanges` in the SDK's documented order. A user gets in
  with one of the network's invitations, checked by the copy that owns the network (a
  wrong one gets `UserNotAuthorized`, as from Party's service). Invitations can be
  revoked by their creator (or by anyone, when made that way). Layouts follow
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
  input and output selections pick the microphone and speakers: the system default,
  the platform user's default (Windows' default communications device) or a device by
  its endpoint id (opened only while a connected chat control has them selected and
  isn't muted; "not found" when Windows has no such device); the capture,
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
  with nothing in them, as for a title with nothing configured. A CloudScript function
  that deletes the player's account (its name has "delete" and "account") deletes
  nothing and returns "Not needed while using PeerParty", which the game shows as the
  reason. Purchases, receipts,
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
- Photon has no invites, so rooms go by code. The overlay's Players page shows the room
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
  player gets every object); Fusion 2 reliable data (`SendReliableDataToPlayer` /
  `SendReliableDataToServer`) is relayed (Fusion 1's isn't). Photon Chat is served by the master server (chat.rs) and TCP connections by
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
  (pre-shared key, as Unity's relays) for a game that forces a secure relay. Every
  allocation has its own random key, made by the host's copy and given only to the
  player it's for; the relay checks each Bind's HMAC with it.
- Vivox voice chat (Unity's Vivox package) goes over the LAN: players in the same
  channel hear each other, and in a proximity (positional) channel a player fades with
  distance as the game set the channel up and sounds from their side (left or right of
  where you face; in the middle if the game turns panning off). Mute, which channels
  the microphone goes to, channel volume and blocking work. The game's device lists are
  Windows' microphones and speakers (with Vivox's "Default System Device" and "Default
  Communication Device"); voice starts on the system defaults, follows the game's
  choice, and a device plugged in or out shows in the game's lists.
- Trust stays inside the game: Unity's TLS reads the Windows ROOT store itself, so the
  ROOT store the game opens lists PeerParty's certificate too (crypt32's `CertOpenStore`
  hooked in the game only). Nothing is installed in Windows' certificate stores.
- Cloud Save (player, public, protected and custom items, with write locks) and Remote
  Config are answered from disk (`%LOCALAPPDATA%\peerparty\ugs\<game>\`; put a
  `remote-config.json` there to give a game its settings), since the real ones would
  refuse PeerParty's sign-in. QoS and Matchmaker are left alone; WebSocket
  relays (WebGL only) aren't carried. References are in docs/design.md, "Sources".
- In a game's log look for `ugs: the game ships ...`, `tls: the game read the ROOT certificate
  store`, `ugs POST lobby.services.api.unity.com/v1/create`, `ugs relay: allocation`,
  `join code`, `Bind HMAC checks out`, `connected to host` (and `DTLS session with
  ... open` when the game forces DTLS); for voice, `vivox: PeerParty answers`, `vivox:
  joined channel`, `vivox: ... is in channel` and, in a proximity channel, `vivox: ... is
  N away ...: volume N%, pan P` (-1 left to +1 right); `voice: microphone '...'` and
  `voice: speakers '...' open` name the devices in use.

Offline (no internet at all, or the online services shut down):

- Steam: lobbies, P2P, NetworkingSockets, friends and invites are all LAN. Steam in
  offline mode is enough: `BLoggedOn` reports signed in in a game started through
  PeerParty, and in Unity games the Web API's "is Steam up" question
  (`api.steampowered.com/ISteamWebAPIUtil/GetServerInfo`) is answered by PeerParty, so
  a game that asks it every few seconds doesn't drop out of a LAN match when Steam is
  slow or the PC is offline. Other Web API requests still go to Steam.
- EOS: sign-in, lobbies, sessions, presence and P2P are PeerParty's; Epic's servers are
  never needed.
- PlayFab: answered locally as above, also for games that only use PlayFab's web API
  once `playfabapi.com` stops resolving.
- Photon: rooms are hosted by a copy on the LAN; Photon Cloud is never contacted.
- Unity Gaming Services: sign-in, lobbies, relays and Vivox voice are PeerParty's (QoS
  and other Unity services are not).
- Finding each other: by UDP broadcast on the LAN, and copies on one PC also through a
  peer board (`%LOCALAPPDATA%\peerparty\peers`), which needs no network adapter at
  all.

When a player drops out of a match, the game's logs say whose side gave up:

- `lan traffic over the last 30.0 s with <peer> (...)`: every 30 s while the copies
  talk, what went each way on each channel (sent, got, read by the game, and when last),
  what waits unread, and the connection itself (UDP or TCP only, bytes queued to send,
  stale datagrams dropped).
- `lan: the game leaves lobby ... their links as it did:` and `steam-lan
  CloseSessionWithUser ...: the link as it did`: the same, at the moment the game itself
  leaves. A link still carrying packets a few milliseconds before means the game chose
  to leave (a check of its own failed); one that had gone quiet means the other copy
  stopped sending.
- `lan peer ... lost its connection` / `didn't come back within 10s`: PeerParty's own
  connection to that player dropped.
- `steam-web ...`: what PeerParty answered on Steam's Web API host, what it passed on to
  Steam and how long Steam took, and `steam-web: the game looks up api.steampowered.com,
  which goes to Steam as it is` when it isn't answered here.

### Split screen

Two to four copies of a game on this PC share one window, one pane per player. Start
them from a game's panel (Players 2–4 with **Split screen** on, a layout and a monitor)
or from a prompt:

```
peerparty --split-screen [--instances 1..4] [--layout L] [--monitor N] [--inputs auto|kbm|pad,...] [--no-kbm] [--stagger smart|0..60] [--no-sync] [--fps 0|1/2|1/3|15..240] [--render-scale 50..100] [--no-upscale] [--no-vram-budget] [--no-cpu-balance] [--names A,B,...] <game.exe> [args...]
```

- `--instances N`: how many copies (default 2).
- `--layout horizontal|vertical|top2|bottom2|grid|slots:...`: side by side, top and
  bottom, two on top and one wide below (`top2`, the default at 3 players), one on top
  and two below (`bottom2`), a 2×2 grid (the default at 4). `slots:x,y,w,h,fit,ratio,z;...`
  is a layout of your own, one pane per player, in ten-thousandths of the window
  (`slots:0,0,5000,10000,fit,match,0;5000,0,5000,10000,fit,match,0` is side by side):
  Layout Studio writes one when you keep a layout, and the game's panel then shows it
  as Custom.
- `--monitor N`: the monitor the window opens on (1 the main one, 2.. the others, left
  to right).
- `--inputs auto|kbm|pad,...`: what each player plays with, in player order. `kbm` the
  keyboard and mouse and never a controller (the session starts with that pane
  focused), `pad` a controller, `auto` (the default) a controller, passing over a player
  who has already been playing with the keyboard and mouse while another player has
  none.
- `--no-kbm`: controllers only (the panel's "Keyboard and mouse in games" off). The
  keyboard and mouse never reach the games; the overlay and the keys below still work.
- `--stagger smart|0..60`: how the copies after player 1 start. `smart` (the default,
  the panel's Advanced > Launch order "Smart (wait until ready)") starts each one once
  the one before has opened its window or drawn its first frame, 8 s at most, so heavy
  games don't fight over the CPU and disk at startup; a number waits that many seconds
  between copies, `0` starts them all at once.
- `--no-sync`: turns off **Sync games to screen** (the panel's Advanced > Split screen),
  which is on by default and keeps each copy from drawing frames faster than the
  session's window shows them.
- `--fps 0|1/2|1/3|15..240`: caps each copy's frame rate (`0` matches the display
  refresh; `1/2` / `half` and `1/3` / `third` use hardware fractional VSync on the
  compositor for judder-free pacing on fixed-refresh displays; `15..240` sets a fixed FPS
  cap). Useful on high-refresh or non-VRR monitors so two to four copies pace evenly
  instead of fighting for full refresh rate.
- `--render-scale 50..100`: renders each copy below its pane's pixel size (`100`, the
  default, renders native) to cut GPU load and render-target VRAM, then scales it up in
  the compositor.
- `--no-upscale`: turns off **Sharp upscaling** (single-pass contrast-adaptive
  sharpening when a pane is scaled up), using plain bilinear scaling instead.
- `--no-vram-budget`: turns off **Share GPU memory budget** (`PEERPARTY_VRAM_SHARE`),
  which by default reports each copy's fair share of VRAM so engines size their texture
  streaming pools without stuttering.
- `--no-cpu-balance`: turns off **Balance CPU cores** (`PEERPARTY_CPU_CORES` and
  Windows CPU Sets), which by default assigns preferred CPU cores per copy and trims
  reported worker core counts to reduce thread contention.
- `--names A,B,...`: names of their own, in player order (a blank one keeps the usual:
  player 1 `PEERPARTY_NAME`, player 2 `PEERPARTY_OTHER_NAME`, then "name (3)").
- `--player N` is for one copy: it plays alone as player N (1–4), with that player's
  saves and name (**Players' saves**, above). It can't be given with `--split-screen`,
  which gives each copy its player.

Each pane gets its own controller (assigned when a controller is first used, and kept
across unplugging), its own in-game overlay when that player presses the overlay
button, and its own audio and rumble. One controller read twice (a remapper's virtual
pad beside the real one) takes one seat. A game sees only its own player's controller,
whichever way it reads controllers, and keeps acting as the window in front while
another pane has the keyboard and mouse. While the focused pane's game holds its
cursor (an aiming game), the mouse stays inside that pane and its pointer hides; the
pointer also hides once the mouse has sat still for a moment while players are on
controllers. The overlay's **Split screen** tab (first, and where it opens unless an
invite waits) lists this PC's players: swap two players' controllers, free one for the
next controller pressed, or hand the keyboard and mouse to another player.

The keyboard and mouse start with player 1 (or the player given them with `--inputs`)
and move when a player clicks a pane, presses `Ctrl+Alt+1`–`4` or uses the overlay, or
switches to a copy's window: not when a copy's window comes to the front by itself as it
opens. A pane whose copy hasn't started yet (copies start one after another) says it's
starting soon; once its copy runs and until its picture shows, that it's starting.

Keys, with the session's window or a copy in front:

- `Ctrl+Alt+E`: opens or closes Layout Studio.
- `Ctrl+Alt+L`: the next layout.
- `Ctrl+Alt+Z`: the focused pane on the whole window, and back. Its game is told the
  window's size, as a layout change does, and goes back to its pane after.
- `Ctrl+Alt+1`–`4` (number row or number pad): the keyboard and mouse to that player's
  pane, which says so.

**Layout Studio** arranges the panes over the running games (`Ctrl+Alt+E`, or the
overlay's Split screen tab > **Arrange panes**). Every change shows at once:

- Drag a divider between panes to resize every pane along it; edges snap to halves,
  thirds, quarters and the other panes (**Snap** turns it off, `Alt` held drags freely).
  Drag a pane's corner or edge to resize it alone, for a picture in a picture.
- Drag a pane onto another to swap the two players. The pane you pick up comes to the
  front, so in a picture-in-picture layout a small pane can sit over a large one.
- A game that draws one shape whatever its pane's (black bars above and below, or at
  the sides) is outlined by its picture, and its bars are hatched as the empty room they
  are. Grabbing it moves or resizes the picture alone: the pane becomes the picture's
  size, its bars go, and its corners keep the picture's shape (`Alt` held resizes
  freely, and the pane is a shape of its own again). **Trim bars** on a pane does the
  same without moving it, and **Fit pictures** on the bar (left stick click on a
  controller) arranges every pane for the biggest pictures with no bars. A pane kept as
  its picture stays one, also in the saved layout: the divider between two such panes
  resizes both pictures, flush against it, and the controller's Resize mode grows or
  shrinks it as a picture (the other way from its middle, or from one side when another
  pane is in the way).
- Each pane: **Scale** `Fit` (the whole picture, with black bars), `Crop` (fills the
  pane, cutting the edges) or `Stretch`; **Ratio** `Auto` (the pane's own shape),
  `16:9`, `16:10`, `4:3` or `21:9`, the shape the game is told to render at, for a HUD
  that crowds in a narrow pane; **Sound** / **Muted** for that copy's audio; **Full Screen**.
- The bar: the layouts for this many players and **PiP** (player 1 on the whole window,
  the others in corners), **KBM** (which player has the keyboard and mouse, or Off),
  **Sync: On / Off** (turns **Sync games to screen** on or off live), **Snap**, **Reset**
  (back to the layout and sync state Layout Studio opened with) and **Done** (or Esc),
  which keeps them for this game's next sessions.
- With a controller: live button prompts match your controller (Xbox, PlayStation or
  Nintendo). The left stick or d-pad selects a pane, LB / RB switch layout preset, Y
  changes Scale, X changes Ratio, LT mutes, RT toggles Full Screen, View / Share toggles
  Sync, and A enters Resize / Swap mode for the selected pane (left stick / d-pad resizes
  with LT for fine steps, right stick moves, X swaps slots, Y toggles Snap, A or B
  returns). B or Start is Done.

A game with a starter program (one that starts the real game and ends) stays in its
pane: each copy is all the programs it started from the game's folder. Players 2 to 4
have saves of their own from the game's first look at them (**Players' saves**,
above). Voice chat uses the PC's microphone and speakers once, through player 1's copy:
the other copies send and play no voice. Closing the session's window (or Close all)
asks every copy to close and save; what still runs 6 seconds later is ended, at once if
asked again. PeerParty's exit code is player 1's, or another player's when theirs
crashed.

How the window shows the copies (the game panel's Advanced > Split screen):

- `PEERPARTY_COMPOSITOR`: unset (**Frames**, recommended) draws each copy's frames on
  the graphics card, from textures the copy shares with the window, and shows a copy's
  own window in its pane where its frames can't be shared (another graphics card, or a
  graphics API that can't share them). `dwm` (**Game windows**) always shows the copies'
  own windows, for a game whose pane stays black. `pixels` (**Pixels**) copies the
  frames through memory instead: slower, for trying that path.
- `PEERPARTY_DXVK=<folder>` (environment only): the folder of a DXVK release you
  downloaded (`1`: `%LOCALAPPDATA%\peerparty\dxvk`). A Direct3D 9 game's split-screen
  copies load its `d3d9.dll` and draw through Vulkan, so their frames reach the window as
  other games' do; for an older Direct3D 9 game whose panes stay black, without changing
  the game's folder. A `d3d9.dll` from DXVK already in the game's folder works the same
  with nothing set.

What split screen does for now: it runs on Windows; one keyboard and mouse is shared
between the players, plus a controller each, for up to four players. Racing wheels and
a keyboard and mouse per player are planned.

### A game's own libcurl

A game whose web calls go through its own libcurl does its own TLS: it never asks
Windows about certificates, so the trust PeerParty gives its local certificate (see the
PlayFab notes above) doesn't reach it, and a call to a service PeerParty answers fails
its certificate check. PeerParty hooks that curl's `curl_easy_setopt`, and a request to a
host whose lookup comes to PeerParty's local server is told to accept that server's
public key and no other (`CURLOPT_PINNEDPUBLICKEY`, with curl's own chain and name
checks off for that request only). Every other request keeps the game's settings.

- A libcurl DLL (any name, exporting `curl_easy_setopt`) is found by itself.
- A libcurl linked into the game's exe, or into a DLL that doesn't export it, is found
  by its bytes. Each such module has a file of its own,
  `%LOCALAPPDATA%\peerparty\patterns\<module>.json` (`MyGame-Win64-Shipping.exe.json`,
  `SomeLibrary.dll.json`), since its bytes change from one game and one build to the
  next. Its `versions` list ties patterns to builds: an entry with `sha256` (of the
  module's file) or `file_version` (its version resource) is for that build only, one
  with neither for any; a SHA-256 match wins over a version match, which wins over an
  entry for any build. An entry lists patterns for `curl_easy_setopt` (the function with
  the `...`, not `Curl_vsetopt`) and `curl_easy_cleanup`, both needed, and, if the game
  uses it, `curl_easy_reset`. Bytes are hex, `??` a wildcard; each pattern must match
  exactly one place in the module's code, and the first that does is used. A packed exe
  is searched again for a minute while it unpacks.

  ```json
  { "versions": [
      { "file_version": "1.4.0.0",
        "curl_easy_setopt_patterns": [ "89 54 24 10 4C 89 44 24 18 4C 89 4C 24 20 48 83 EC 28" ],
        "curl_easy_cleanup_patterns": [ "40 53 48 83 EC 20 48 8B D9 48 85 C9 0F 84 ?? ?? ?? ??" ] } ] }
  ```

  The log says which entry applied (`hooks: <module> (version ...): entry 1 of ...`), or,
  when none is for this build, the module's version and SHA-256 to write one; then what
  each pattern found (`curl <module>: curl_easy_setopt at +0x...`).
  [docs/examples/patterns/MyGame-Win64-Shipping.exe.json](examples/patterns/MyGame-Win64-Shipping.exe.json)
  is a fuller example, with notes.
- A curl that can't pin a key (older than 7.39, or a TLS back end without it) is left
  alone, and its calls to PeerParty's hosts still fail (the log says `gap: curl ... can't
  pin`). `PEERPARTY_SKIP=curl` turns the curl hooks off.

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
  sidebar, and the overlay (Players, Network, and a notification), say when Windows Firewall
  blocks the game on the network in use, or hasn't been told to let it in yet. **Fix**
  (or the overlay row's button) asks Windows for permission, turns off the Block rules
  for the game and adds an Allow rule (private and domain networks; public ones only when
  you choose so). If the prompt is declined or the change fails, the firewall's
  "Allowed apps" settings open instead. Over an exclusive-fullscreen game the overlay
  asks you to Alt+Tab to the launcher, since the prompt would hide behind it. When
  players on your network are heard but can't be reached, the overlay says so: their
  PC's firewall is the likely cause, and they fix it on their PC.
  `peerparty --firewall game.exe` prints the same check; `peerparty --fix-firewall
  game.exe [--public]` (as administrator) does the fix: two Allow rules for the game,
  TCP and UDP, in the group `peerparty`. They stay after PeerParty is gone, as the
  game's own rules would; `peerparty --remove-firewall` (as administrator) removes every
  rule PeerParty added. Rules set by an organisation's policy are reported, not changed.
- `PEERPARTY_INET_PORT=N`: a fixed UDP port; `PEERPARTY_INET_PEERS=host:port,...`: also
  punch these addresses (a forwarded port, a VPN; IPv6 as `[addr]:port`); `PEERPARTY_INET_DHT=0`: those
  addresses only; `PEERPARTY_INET_BOOTSTRAP`, `PEERPARTY_INET_STUN`: other DHT routers or
  STUN servers (`0`: none); `PEERPARTY_LAN_DISCOVERY=0`: no LAN beacons (to test the
  internet path on one LAN).
- Limits: anyone with the code is trusted like a LAN player. PeerParty never shows a
  room member's public IP (the game sees a stand-in address in 198.18.0.0/15, the overlay
  says "your room"), but their PCs still see it at the network level. Games that connect
  by typed IP address use the stand-in shown on the overlay's Network > Connection details, and the hook
  carries that traffic through the tunnel. Each member's player id is its key's hash,
  so one member can't pass as another. Both sides on symmetric NATs
  can't connect without a forwarded port (there's no relay).

### Public sessions

Opt-in: find **anyone** online who is playing the same game through PeerParty, no code
needed. Their lobbies show up in the game's own session browser like LAN lobbies. Turn it
on in Play together > Anyone online > **Public sessions** (pick the name others see there), or
for one run with `--public` or `PEERPARTY_PUBLIC=1`. Room codes and LAN play are
unchanged and work at the same time. Details: docs/public-sessions.md.

- Every player has a signing key, and a player's id is that key's hash. Adverts that are
  forged, altered or replayed are refused, and nobody can use another player's id.
- Lobbies the game made friends-only or invite-only stay closed to public players until
  you invite them or they are in a lobby with you.
- Public players get lobbies, invites, game traffic (all of it, PlayFab Party's
  included) and, if you turn it on, voice. Voice and party chat with them is off by
  default; turn it on in Play together > Anyone online > **Voice chat with strangers**,
  with `--set public_voice=on`, or with `PEERPARTY_PUBLIC_VOICE=1` (the environment wins).
  "Invite everyone on your network" never reaches them, and their invites always wait
  for you to accept.
- Block a player from the overlay (Players > their Details > **Block this player**,
  pressed twice; **Undo** on Players for 30 s after) or
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

- The opening hotkey is taken from the game (it doesn't reach the game's own menus).
  While a voice chat is on, Players has **Your microphone** (every voice chat of the
  game) and a player's page **Mute their voice**.
- Opens with **Shift+Tab** (Steam's and GOG's key) or **Shift+F3** (Epic's), optionally
  Shift+F1 (EA) and Shift+F2 (Ubisoft), any controller's **Home / Xbox Guide** button
  (it only opens the overlay; B or Start closes it), or a **controller chord** you record
  in the launcher (Settings > Overlay > Controller chord > Record: hold two or more
  buttons for a second; then holding them a second opens it; none by default, since
  Windows takes View + Menu for its on-screen keyboard). Every key that's on works in every game: a game
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
- Pages, for what can be done in the game now (how it looks and opens is set in the
  launcher): **Players** (a line under the tabs with the lobby you're in; at the top
  what keeps players out, with Fix; a Photon room's code; Invite everyone on your
  network, when someone there can be invited; then every player once: invites waiting,
  on the sender's row, then this PC's other copies, your network, the internet and
  dedicated servers, each under its heading. The same buttons on every row: A / Cross
  joins them (asks them to invite you), X / Square invites them (where the game has
  somewhere to invite to), Y / Triangle opens their page; an invite's row accepts with
  A and declines with X; someone in your lobby is muted with A while voice is on;
  opened from the game's invite button, A invites. Internet players' rows show their
  ping, and a different version of the game is flagged. A dedicated server is listed by
  the name it advertises, with Join only, which hands a Steam game Steam's own "join
  this server" request with its address: its LAN address when it is listed on Steam,
  else the address its PlayFab lobby advertises, as for a crossplay server. A player's
  page has how they're connected, the stand-in address to type for an internet player,
  Mute their voice, Invite and Join where they apply, and Block for public players),
  **Network** (this PC's Windows Firewall, which keeps out players at home as much as
  online, with Fix; players on your network and those that can't be reached; the
  internet play switch; your room and public sessions; **Connection details**: the
  router, port forwarding, finding players, and the services PeerParty answers for),
  and in a split-screen session **Split screen**, first (Layout Studio, each player's
  controller, the keyboard and mouse). B goes back from a player's page and the
  connection details (so does a click on the line naming them), and closes from a
  page. Players and Network show a badge when an invite waits or something keeps
  players out.
- Controllers first: d-pad / stick moves, A / Cross acts, X / Square is the second
  action, Y / Triangle opens a player's page, B / Circle goes back, LB / RB (or left /
  right on a row without a value) switch pages, Start closes (Home only opens), a click
  on the dimmed game closes. Xbox pads (and anything presented as XInput), DualShock 4,
  DualSense, Switch Pro and other HID gamepads are read by peerparty.exe itself; glyphs
  follow the pad in use. Keyboard (arrows, Enter, Esc, Q / E, Tab, X, Y) and mouse work
  too.
- Nothing reaches the game while it shows: keyboard and mouse are taken by low-level
  hooks in peerparty.exe (only while open), controllers are held still in the game
  (XInput, DirectInput and raw-input HID at rest, DirectInput's sticks at the middle of
  their range; Windows.Gaming.Input's Gamepad and RawGameController readings at rest;
  GameInput's controller readings standing still at the one from when it opened, and its
  reading callbacks not called, in every GameInput API version; HID devices read
  directly, SDL's HIDAPI, repeating their first report). A button still down when it
  closes (the Start or B that closed it) keeps controllers held until it's let go, at
  most a second, so the game doesn't read it as a new press.
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
  own (Dear ImGui's precompiled shaders), blended like the other APIs; into an HDR swap
  chain (scRGB, or HDR10's PQ) with PeerParty's own shaders, which put the overlay at SDR
  white as the Direct3D hooks do (hook/src/video/glsl, compiled ahead of time by
  tools/vk_shaders.py).

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
  "Accept invites automatically" (the launcher's Play together, or a game's own settings,
  `PEERPARTY_INVITES=auto`) accepts them at once.
- For games without an invite button, these invite every other player on your network
  to where you are, in the game whose window is in front: the overlay's "Invite everyone
  on your network", the window's "Invite local players", or `peerparty --invite`. Friends in your room and
  public players are never invited this way: invite them one at a time from the
  overlay's Players page. A chime confirms it; an error chime means you aren't in a
  lobby yet.
- A game that doesn't say where to join (no Steam rich presence `connect`, no lobby of an
  SDK PeerParty hands over, as with Unity's Lobby) but has invites of its own shows
  PeerParty how they look: invite someone once from the game's own friends list, and
  that invite, with its lobby id as a placeholder, is kept for the game
  (`%LOCALAPPDATA%\peerparty\invite-templates`). From then on the overlay's Invite and
  Join send it with the lobby you're in. Until then the overlay asks you to invite once
  from the game's friends list, instead of "Host or join a game first". The other details in it (player counts, password,
  private) are the ones from that first invite.
