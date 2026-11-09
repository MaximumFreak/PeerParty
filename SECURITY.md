# Reporting a problem

## Security problems

PeerParty runs inside games and talks to other players' PCs, so a flaw in how it handles
data from another player (a room code, a public session, a LAN peer) matters. Please
report one privately instead of in a public issue:

1. Open the repository's **Security** tab.
2. Choose **Report a vulnerability** (GitHub's private reporting).
3. Say what you found, which version (the Details tab of the exe's Properties), and how to reproduce it.

You'll get an answer within a few days. A fix is released as soon as it's ready, and the
report is made public afterwards, crediting you if you like.

Only the latest release is supported.

## Concerns from rights holders

If you represent a rights holder and think something in this repository or in a release
shouldn't be there, please say so, either in a public issue or, if it shouldn't be
public, through the same private form above. Say what the material is, where it is, and
what you'd like done. Concerns are read promptly, and material shown to infringe is
removed. [LEGAL.md](LEGAL.md) explains what PeerParty is and isn't.

## Problems with a game

A game that doesn't work with PeerParty isn't a security problem: open a normal issue
with the game's log (see docs/testing.md). Issues asking PeerParty to get round a
store's ownership, licence or anti-cheat checks are closed.
