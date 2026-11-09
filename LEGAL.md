# Legal notes

This isn't legal advice, and it doesn't change the licence ([LICENSE](LICENSE)), which
comes with no warranty.

## What PeerParty is

PeerParty is an independent, open-source interoperability tool. It gives a game's
multiplayer calls (lobbies, presence, invites, sign-in, connections, voice) a backend
that runs between the players' own PCs, so a game can be played over a LAN, by several
players on one PC, directly with friends, and after its online services have closed.

## What PeerParty doesn't do

- **No game or SDK files.** The repository and the releases contain no game files and
  no vendor SDK binaries, headers or libraries. Game files are never modified: PeerParty
  works in the game's memory, for that run.
- **Store checks stay with the store.** A game's licence, DLC, entitlement and purchase
  queries (Steam's licence and DLC calls, Epic's Ecom, Galaxy's DLC calls, store
  inventories) go to the store's own software, or come back empty or offline. PlayFab's
  purchase, receipt, coupon, currency, grant, trade and inventory-change calls, and its
  whole admin API, fail as unavailable;
  its other web API calls that PeerParty doesn't implement get an empty success, which
  grants nothing. PeerParty doesn't unlock
  content. It uses the SDK the game loaded, and doesn't provide a replacement SDK or
  change SDK files on disk.
- **Sign-in is answered locally.** To play without the vendors' servers, PeerParty
  answers sign-in itself and doesn't contact the services it replaces:
  - a game's sign-in to Epic Online Services, GOG Galaxy, PlayFab, Unity and Photon
    succeeds as the PeerParty player, and the tokens handed back (EOS's auth and Connect
    tokens, PlayFab's session and entity tokens, Galaxy's tokens and encrypted app
    ticket) are PeerParty's own strings; PlayFab's entity-token check succeeds for that
    player, Galaxy reports its services connected, and Photon accepts every client;
  - Steam reports signed in; a Steam game server's logon succeeds at once and reports
    the server as not VAC-secure; and when no Steam client is in the game, PeerParty
    completes the game's own auth-ticket requests;
  - the sign-in between players (Steam's auth tickets, a game server approving a player)
    accepts every player PeerParty connects you to, as the owner of their copy, without
    asking Steam;
  - Xbox Live friends, presence and privacy queries are answered for the players
    PeerParty connects you to, with every permission allowed;
  - EOS sessions report sanctions as disabled, with no sanctioned players (Epic's
    sanctions query itself still fails as offline).
- **No piracy.** PeerParty is for games you're licensed to run. Issues and pull requests
  about running unlicensed copies are closed.
- **No cheating.** PeerParty stops a game in which EasyAntiCheat or BattlEye loads, or
  that starts an EOS Anti-Cheat session while PeerParty's EOS layer is on. Don't use it in games whose online play is
  protected by anti-cheat.

## What it changes on your PC

PeerParty's own files live under `%LOCALAPPDATA%\peerparty`, and its logs under
`%TEMP%\peerparty\logs`. Outside those it writes only: copies of a game's saves under a
PeerParty player's identity, next to the originals, which are only read; Windows Firewall rules for a
game, when you press **Fix** and approve Windows' prompt; the key of its local
certificate, in your Windows user's key store; and, while it starts an Xbox app game,
Windows' debug setting for that package, which it clears at once.

The services PeerParty stands in for run inside the game's process, with a certificate
trusted by that process alone; nothing is installed in Windows' certificate stores.
PeerParty doesn't collect or forward account credentials, and its logs don't include
them unless you turn on a debug trace. Public sessions connect you directly to other
players, who can see your IP address, as in any peer-to-peer game; while they are on,
your address is also listed on the public BitTorrent DHT under the game's public topic.

## Your responsibility

Use PeerParty with copies of games you're licensed to run. Games' licence agreements and
platforms' terms may forbid running a game with third-party software, and a publisher or
platform may respond, for example by restricting an account. Use it at your own risk,
and not with accounts you can't afford to lose. Where PeerParty stands in for a service,
the vendor doesn't support it: don't contact them about problems it causes.

## Third-party code

Code taken from other projects keeps its licence beside it:

| Code | From | Licence |
|---|---|---|
| `vendor/retour` | retour (crates.io), with local fixes | BSD-2-Clause |
| `hook/src/photon` (protocol layer) | Luxon and Luxon Server | BSD-3-Clause (`hook/src/photon/LICENSE-Luxon`) |
| `hook/src/present/vkshaders.rs` | Dear ImGui's precompiled Vulkan shaders | MIT |

Every dependency's licence, and the notices those licences require, are in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md), which also ships with each release.

## Trademarks

PeerParty is not affiliated with, endorsed by or sponsored by any of the companies
below, or by any game publisher. Their names are used only to describe what PeerParty is
compatible with.

- Steam and Steamworks are trademarks of Valve Corporation.
- Epic Games, Epic Online Services, Easy Anti-Cheat and Unreal are trademarks of Epic
  Games, Inc.
- Microsoft, Windows, Xbox, PlayFab and DirectX are trademarks of the Microsoft group of
  companies.
- GOG and GOG Galaxy are trademarks of GOG sp. z o.o.
- Photon, PUN, Bolt and Fusion are trademarks of Exit Games GmbH.
- Unity and Vivox are trademarks of Unity Technologies.
- EA is a trademark of Electronic Arts Inc.
- Ubisoft is a trademark of Ubisoft Entertainment.
- BitTorrent is a trademark of BitTorrent, Inc.
- PlayStation, DualShock and DualSense are trademarks of Sony Interactive
  Entertainment Inc.
- Nintendo and Nintendo Switch are trademarks of Nintendo.
- BattlEye is a trademark of BattlEye Innovations e.K.
- All other trademarks and game names are the property of their owners.

## Contact

If you represent a rights holder and have a concern about anything in this project,
please open an issue on the repository, or report it privately as described in
[SECURITY.md](SECURITY.md) if it shouldn't be public. Concerns are answered promptly, and
material shown to infringe is removed.
