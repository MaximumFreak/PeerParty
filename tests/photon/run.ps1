# Photon's own .NET client (a game's Photon3Unity3D / PhotonRealtime / PhotonChat, copied
# to $env:PPTEST_PHOTON_DIR) against PeerParty's Photon servers, inside one
# `peerparty.exe` process: the client looks up ns.photonengine.io, the hook
# redirects it and hosts the name, master and game servers. Three clients (v1.6 with
# Auth, v1.8 with AuthOnce, v1.8 with Auth) create, list, join-random and join a room,
# swap events (a 5000-byte one), properties and a cached event, the master client
# leaves; clients over TCP and WebSocket join; an SQL lobby matches by filter; a player
# drops out of a room with a PlayerTTL and rejoins as the same actor; then two Photon
# Chat clients swap a channel message, a private message and a friend's status.
# Exit code 0 = PHOTON_TEST_OK.

param([string]$PeerParty = "$env:TEMP\cargo-target\peerparty\testexe\peerparty.exe")
$ErrorActionPreference = 'Stop'
if (-not $env:PPTEST_PHOTON_DIR) { throw 'Set PPTEST_PHOTON_DIR to a folder with copies of the Photon DLLs.' }
# One output folder per set of Photon DLLs (Chat or not, and which build). A later run
# with the same DLLs and the same test sources skips the build.
$dlls = Get-ChildItem -LiteralPath $env:PPTEST_PHOTON_DIR -File | Sort-Object Name
$stampSrc = ($dlls | ForEach-Object { '{0}|{1}|{2}' -f $_.Name, $_.Length, $_.LastWriteTimeUtc.Ticks }) -join "`n"
$sha = [Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes($stampSrc))
$stamp = ([BitConverter]::ToString($sha)).Replace('-', '').Substring(0, 16)
$out = Join-Path $env:TEMP "pp-photon-test\$stamp"
$built = Join-Path $out 'PhotonTest.exe'
$src = Get-ChildItem $PSScriptRoot -Recurse -File | Where-Object { $_.Extension -in '.cs', '.csproj', '.props' -and $_.FullName -notmatch '\\(bin|obj)\\' }
$newest = ($src | Measure-Object LastWriteTimeUtc -Maximum).Maximum
if (-not (Test-Path -LiteralPath $built) -or (Get-Item -LiteralPath $built).LastWriteTimeUtc -lt $newest) {
    dotnet build (Join-Path $PSScriptRoot 'PhotonTest\PhotonTest.csproj') -c Release -o $out -p:PPTEST_PHOTON_DIR=$env:PPTEST_PHOTON_DIR | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }
}
$start = Get-Date
# Loopback only: no Windows Firewall prompt for PhotonTest.exe.
$env:PEERPARTY_LOOPBACK = '1'
# peerparty reports on stderr; that isn't an error here.
$ErrorActionPreference = 'Continue'
& $PeerParty (Join-Path $out 'PhotonTest.exe') 2>&1 | ForEach-Object { "$_" }
$code = $LASTEXITCODE
$log = Get-ChildItem "$env:TEMP\peerparty\logs" -Filter 'PhotonTest-*' | Where-Object LastWriteTime -gt $start | Sort-Object LastWriteTime | Select-Object -Last 1
if ($log) { Get-Content $log.FullName | Select-String 'photon' | ForEach-Object Line }
if ($code -eq 0) { 'PHOTON_TEST_OK' } else { "PHOTON_TEST_FAIL (exit $code)"; exit 1 }