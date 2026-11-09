# Testing PeerParty

## Automated

`tests\run_all.ps1` runs everything that doesn't need a person at the keyboard:

- `cargo test --workspace`: unit and loopback tests (hook, core, launcher). Some use the
  PC's own devices and skip themselves without them: GameInput, the Vulkan overlay
  pipeline (GPU), Windows' Voice Capture DSP (microphone).
- A testexe build: the same optimizations as a release build, without link-time
  optimization, so the suite's exe builds faster. The shipping exe is still the release
  build. `-SkipBuild` reuses the last testexe build.
- Photon's own .NET client (`tests/photon`): rooms over UDP, TCP and WebSocket, an SQL
  lobby, rejoin after a dropped connection, and two Photon Chat clients.
- The two-copy SDK tests (`tests/sdk`): two Python processes under `peerparty`
  load a real SDK DLL with ctypes and talk to each other through PeerParty, as two copies
  of a game would. EOS RTC voice, EOS lobby voice and EOS custom invites; PlayFab Party
  chat and PlayFabCore sign-in; PlayFab's web API (lobbies, statistics, user data,
  friends; no DLL needed); Unity Gaming Services (no DLL needed); GOG Galaxy (stats,
  storage, cloud saves, telemetry, tokens, chat); Steam licence checks, which must reach
  Steam itself and read "can't verify" for a LAN player (needs the Steam client running,
  and shows the account as playing the app used).
- Typed-IP games on async sockets (`tests/sdk/ipfwd_test.py`, no DLL): two copies in a
  fresh room over loopback, with LAN discovery off so each sees the other only at its
  stand-in address. Each answer's overlapped receive is collected seven ways (completion
  routine, `GetQueuedCompletionStatus(Ex)`, `WSAGetOverlappedResult`,
  `GetOverlappedResult`, a thread-pool IO object, `BindIoCompletionCallback`) and must
  come from the other's stand-in.
- Internet play (`core/tests/internet.rs`, `core/tests/public.rs`, no internet needed):
  room-code peers through a tunnel; three public-session players finding each other and
  each other's lobbies through a local test DHT, with no address in names, lobbies,
  status or logs; a blocked player dropped and kept out; forged, altered and replayed
  adverts refused; thousands of junk packets survived. Tests marked "needs the
  internet" use the real DHT (`cargo test -p peerparty_core -- --ignored` with
  `PEERPARTY_LOOPBACK=0`).

### SDK DLLs for the tests

The SDK tests need SDK DLLs copied from games you have installed. They are never committed: they
are the vendors' files. Each test reads an `PPTEST_*` variable naming the DLL (or the
folder holding several):

| Variable | What |
|---|---|
| `PPTEST_EOS_DLL` | A 64-bit `EOSSDK-Win64-Shipping.dll` |
| `PPTEST_PARTY_DLL` | `PartyWin.dll` or `PartyWin7.dll` |
| `PPTEST_PFCORE_DIR` | A folder with `PlayFabCore.Win32.dll` and `libHttpClient.Win32.dll` |
| `PPTEST_GALAXY_DLL` | A 64-bit `Galaxy64.dll` (SDK 1.144 or newer; the cargo test wants 1.152.11) |
| `PPTEST_GALAXY_1152_6_DLL` | A 64-bit `Galaxy64.dll`, SDK 1.152.6 (cargo test only) |
| `PPTEST_PHOTON_DIR` | A folder with `Photon3Unity3D.dll`, `PhotonRealtime.dll` and, if the game has it, `PhotonChat.dll` |
| `PPTEST_STEAM_DLL`, `PPTEST_STEAM_APPID` | A `steam_api64.dll` or 32-bit `steam_api.dll` (any SDK with the flat API), and the app id of a game the signed-in Steam account owns |
| `PPTEST_SDK_DLLS`, `PPTEST_SDK_DLLS_X86` | `;`-separated SDK DLLs the cargo test maps and hooks without running (64-bit, 32-bit) |

Without the variable, `run_all.ps1` uses copies kept in
`%LOCALAPPDATA%\peerparty\test-dlls\<test>\`. A `sources.ps1` in that folder can set
`$Sources = @{ <test> = @(<paths>) }` to the DLLs in your installed games (and any
`PPTEST_*` variable, such as `$env:PPTEST_STEAM_APPID`); while those
exist, the kept copies are refreshed from them, so the tests keep working after a game
is uninstalled. A test with no DLL is skipped and named in the summary. `-Only
photon,ugs` runs some; `-SkipBuild` reuses the last testexe build. The two-copy tests
run four at a time, each with its own game key so they don't meet on the peer board.
The Steam licence test runs by itself afterwards, because it uses the real Steam client.
Photon shares that pool, one Photon build at a time (its ports are fixed).

`-AllBuilds` runs each test once per SDK build you have: `sources.ps1` can also set
`$Matrix = @{ <PPTEST_* variable> = [ordered]@{ <label> = @(<the DLL>, <files kept beside
it>) } }`, one entry per version or build (`-Label '*pattern*'` picks some). Each build
is copied to its own folder with the settings files beside it. The Steam and EOS tests
first start the SDK once without PeerParty, once per DLL rather than once per test: a
build that can't start on this PC at all
(too old for today's Steam client, or one that restarts the process with its own
arguments) is skipped with the reason, rather than counted as PeerParty's failure.
Parts an older SDK doesn't have (Galaxy's cloud storage before 1.148.6, EOS RTC and
custom invites in early SDKs, Photon's WebSocket client and Chat) are left out and
logged. A 32-bit DLL is run under a 32-bit Python: `PPTEST_PYTHON32`, else the one
`py -3-32` or `uv python find cpython-windows-x86-none` finds (`uv python install
cpython-3.12-windows-x86-none` gets one).

## In games

PeerParty has been played with two copies on one PC on Steam, EOS, PlayFab Party, GOG
Galaxy and Photon Fusion games, and started as a package for Microsoft Store games. Many
parts are covered by the automated tests above but haven't been played in a game yet,
among them: drawing the overlay inside a game's frames, voice through the default
microphone and speakers, Unity Gaming Services in a real Unity game, and play between two
PCs over the internet.

When you try a game, the log in `%TEMP%\peerparty\logs\` says what each layer did. Lines
worth looking for:

| Layer | Log lines |
|---|---|
| Steam | `steam-lan`, `SteamNetworkingSockets...: peerparty answers N of its M methods` |
| EOS | `eos-lan`, `gap: eos <function>` |
| PlayFab | `party-lan`, `pfmp-lan`, `playfab POST ...` |
| GOG Galaxy | `galaxy signed in on the LAN`, `galaxy lobby` |
| Photon | `photon ns.photonengine.io -> ...`, `signed in as`, `joined room ... as actor N` |
| Unity Gaming Services | `ugs: the game ships ...`, `ugs relay: allocation`, `connected to host` |
| Overlay | `overlay: opened by ...`, `present: drawing the overlay inside the game's ... frames` |
| Internet play | `inet connected to NAME`, `... now runs through the tunnel` |

Please report what worked and what didn't, with the log. PeerParty stops a game in which
EasyAntiCheat or BattlEye loads, or that starts an EOS Anti-Cheat session with the EOS
layer on; don't run it in
a game whose online play is protected by anti-cheat.
