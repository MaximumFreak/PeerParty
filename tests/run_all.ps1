# Every automated peerparty test in one go:
#   1. cargo test (unit and loopback tests: hook, core, launcher). The launcher's nested
#      hook build is x64 only: these tests never inject a 32-bit DLL.
#   2. a testexe build (release optimizations, no link-time optimization, incremental).
#      The tests below run that peerparty.exe. The shipping exe stays a release build.
#      -SkipBuild reuses the last testexe build. A 32-bit test DLL builds the x86 hook
#      too; otherwise the hook build is x64 only.
#   3. Photon's own .NET client (tests/photon/run.ps1): Realtime over UDP, TCP and WebSocket,
#      an SQL lobby, rejoin with a PlayerTTL, and Chat. Incremental: a later run with the
#      same Photon DLLs and the same test sources skips `dotnet build`.
#   4. the two-copy SDK tests (tests/sdk/run_pair.ps1): EOS RTC, EOS lobby voice, EOS custom
#      invites, PlayFab Party chat, PlayFabCore sign-in, PlayFab's web API, Unity Gaming
#      Services, Vivox voice (Unity's VivoxNative.dll), GOG Galaxy, Steam licence checks (with the Steam client running), and
#      typed-IP games on async sockets (two copies in a room)
#
# SDK pairs and Photon run four at a time. Each pair gets its own PEERPARTY_GAME, so they
# don't meet on the peer board. At most one Photon run is in that pool (those tests bind
# fixed ports). The Steam licence test runs alone afterwards: it uses the real Steam
# client. EOS and Steam builds are probed once per DLL (the same file is not probed again).
#
# The SDK tests load copies of real SDK DLLs taken from games you have installed (never the games'
# own files, and never committed: they're the games' files). Each test reads its PPTEST_*
# variable, else the copies kept in %LOCALAPPDATA%\peerparty\test-dlls\<test>. That
# folder may hold a sources.ps1 setting $Sources = @{ <test> = @(<paths>) } to the DLLs
# in installed games; while those exist the kept copies are refreshed from them, so the
# tests keep working after a game is uninstalled. A test with no DLLs is skipped and
# listed. With -AllBuilds, each two-copy test runs once per build in sources.ps1's
# $Matrix = @{ <PPTEST_* variable> = [ordered]@{ <label> = @(<the DLL>, <files kept
# beside it>) } } instead: every SDK version and build of it you have.
# -Label '*pattern*' keeps the builds whose labels match.
# Usage: tests\run_all.ps1 [-SkipBuild] [-AllBuilds [-Label <pattern>]] [-Only photon,eos_rtc,...]
param([switch]$SkipBuild, [switch]$AllBuilds, [string]$Label = '*', [string[]]$Only = @())
# cargo and peerparty report on stderr; Windows PowerShell would stop on that.
$ErrorActionPreference = 'Continue'
# Everything binds to 127.0.0.1 (no Windows Firewall prompts); copies meet through the
# peer board.
$env:PEERPARTY_LOOPBACK = '1'
$root = Split-Path $PSScriptRoot
$copies = Join-Path $env:LOCALAPPDATA 'peerparty\test-dlls'
$Sources = @{}
$Matrix = @{}
$local = Join-Path $copies 'sources.ps1'
if (Test-Path $local) { . $local }
$results = [System.Collections.Generic.List[object]]::new()
# Same directory .cargo/config.toml builds into. testexe sits beside debug and release.
$PeerParty = Join-Path $env:TEMP 'cargo-target\peerparty\testexe\peerparty.exe'
function Want($name) { $Only.Count -eq 0 -or $Only -contains $name }
function Note($name, $state, $detail = '') { $results.Add([pscustomobject]@{ Test = $name; Result = $state; Detail = $detail }) }

# The kept copies of a test's DLLs (tests never touch a game's folder): refreshed from
# $Sources when those files exist, else the copies from an earlier run.
function Copy-Dlls($name, [string[]]$leaves) {
    if (-not $leaves.Count) { return $null }
    $dir = Join-Path $copies $name
    $files = @($Sources[$name] | Where-Object { $_ })
    if ($files.Count -and -not ($files | Where-Object { -not (Test-Path $_) })) {
        New-Item -ItemType Directory -Force $dir | Out-Null
        foreach ($f in $files) { Copy-Item $f $dir -Force }
    }
    if (-not ($leaves | Where-Object { -not (Test-Path (Join-Path $dir $_)) })) { return $dir }
    return $null
}

# The kept copy of one -AllBuilds build: the DLL and the files beside it, each build in
# its own folder.
function Copy-Build($label, [string[]]$files) {
    $dir = Join-Path $copies "builds\$label"
    if (-not ($files | Where-Object { -not (Test-Path -LiteralPath $_) })) {
        New-Item -ItemType Directory -Force $dir | Out-Null
        foreach ($f in $files) { Copy-Item -LiteralPath $f $dir -Recurse -Force }
    }
    if (Test-Path -LiteralPath (Join-Path $dir (Split-Path $files[0] -Leaf))) { return $dir }
    return $null
}

# Whether a DLL is 32-bit (its PE header's machine is x86).
function Test-X86($dll) {
    $b = [IO.File]::ReadAllBytes($dll)
    $pe = [BitConverter]::ToInt32($b, 0x3C)
    [BitConverter]::ToUInt16($b, $pe + 4) -eq 0x14C
}

# A 32-bit Python for 32-bit DLLs: PPTEST_PYTHON32, else the py launcher's or uv's.
$script:Python32 = $null
$script:Python32Known = $false
function Get-Python32 {
    if ($script:Python32Known) { return $script:Python32 }
    $found = $null
    if ($env:PPTEST_PYTHON32) { $found = $env:PPTEST_PYTHON32 }
    else {
        $py = & py -3-32 -c 'import sys; print(sys.executable)' 2>$null
        if ($LASTEXITCODE -eq 0 -and $py) { $found = "$py".Trim() }
        elseif (Get-Command uv -ErrorAction SilentlyContinue) {
            $py = & uv python find cpython-windows-x86-none 2>$null
            if ($LASTEXITCODE -eq 0 -and $py) { $found = "$py".Trim() }
        }
    }
    $script:Python32 = $found
    $script:Python32Known = $true
    return $script:Python32
}

# Single-quoted PowerShell literal. The job spec is a script a child process dots.
function Quote([string]$s) {
    if ($null -eq $s) { $s = '' }
    "'" + $s.Replace("'", "''") + "'"
}

function Env-List($value) {
    @($value | Where-Object { $_ })
}

# One job's environment, applied in the child only. The parent stays as the caller left it.
function Job-Session($job) {
    $s = @{ PEERPARTY_LOOPBACK = '1' }
    if ($job.Kind -eq 'photon') {
        $s['PPTEST_PHOTON_DIR'] = [string]$job.PhotonDir
        $s['PEERPARTY_MODE_PHOTON'] = 'lan'
        return $s
    }
    if ($job.Var -and $job.DllPath) { $s[[string]$job.Var] = [string]$job.DllPath }
    if ($job.Python) { $s['PPTEST_PYTHON'] = [string]$job.Python }
    return $s
}

function Write-Spec($path, $job) {
    $session = Job-Session $job
    $pair = @(foreach ($e in (Env-List $job.Env)) { Quote $e }) -join ', '
    $role = @(foreach ($e in (Env-List $job.RoleEnv)) { Quote $e }) -join ', '
    $roles = @(foreach ($e in (Env-List $job.Roles)) { Quote $e }) -join ', '
    $body = @(
        "`$Kind = $(Quote $job.Kind)"
        "`$Runner = $(Quote $job.Runner)"
        "`$Test = $(Quote $job.Test)"
        "`$PairEnv = @($pair)"
        "`$RoleEnv = @($role)"
        "`$Roles = @($roles)"
        "`$PeerParty = $(Quote $job.PeerParty)"
        "`$Game = $(Quote $job.Game)"
        "`$Work = $(Quote $job.Work)"
        '$Session = @{'
    )
    foreach ($k in @($session.Keys)) { $body += "    $(Quote $k) = $(Quote ([string]$session[$k]))" }
    $body += '}'
    Set-Content -LiteralPath $path -Value $body -Encoding UTF8
}

# Probe env is set on this process (python inherits it) and put back before we return.
# A child job starts afterwards and would otherwise inherit the probe's DLL variables.
function Restore-Env($bag) {
    foreach ($n in @($bag.Keys)) {
        if ($null -eq $bag[$n]) { Remove-Item "Env:\$n" -ErrorAction SilentlyContinue }
        else { Set-Item "Env:\$n" -Value $bag[$n] }
    }
}

# $null when the SDK starts. Otherwise the first FAIL/SKIP line (or the interpreter's).
function Invoke-Probe($job) {
    $pd = Join-Path $env:TEMP ('ppwork\probe-' + [guid]::NewGuid().ToString('n'))
    New-Item -ItemType Directory -Force -Path $pd | Out-Null
    $names = New-Object System.Collections.Generic.List[string]
    $names.Add('PPTEST_DIR')
    if ($job.Var) { $names.Add([string]$job.Var) }
    $names.Add('PPTEST_PYTHON')
    foreach ($kv in (Env-List $job.Env)) { $names.Add(($kv -split '=', 2)[0]) }
    $saved = @{}
    foreach ($n in $names) {
        if (-not $saved.ContainsKey($n)) { $saved[$n] = [Environment]::GetEnvironmentVariable($n, 'Process') }
    }
    $reason = $null
    try {
        foreach ($kv in (Env-List $job.Env)) {
            $k, $v = $kv -split '=', 2
            Set-Item "Env:\$k" -Value $v
        }
        if ($job.Var) { Set-Item "Env:\$($job.Var)" -Value ([string]$job.DllPath) }
        if ($job.Python) { Set-Item Env:\PPTEST_PYTHON -Value ([string]$job.Python) }
        Set-Item Env:\PPTEST_DIR -Value $pd
        $python = if ($job.Python) { [string]$job.Python } else { 'python' }
        $proc = Start-Process -FilePath $python -ArgumentList "`"$($job.Test)`" probe" -PassThru -NoNewWindow `
            -RedirectStandardOutput (Join-Path $pd 'out.txt') -RedirectStandardError (Join-Path $pd 'err.txt') `
            -WorkingDirectory (Split-Path -Parent $job.Test)
        if (-not $proc) { $reason = 'probe did not start' }
        else {
            if (-not $proc.WaitForExit(60000)) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
            $result = Get-Content (Join-Path $pd 'probe.result') -ErrorAction SilentlyContinue
            if ($result -ne 'ok') {
                $why = @(Get-Content (Join-Path $pd 'probe.log') -ErrorAction SilentlyContinue | Where-Object { $_ -match '^(FAIL|SKIP) ' } | Select-Object -First 1)
                if (-not $why) { $why = @(Get-Content (Join-Path $pd 'err.txt') -ErrorAction SilentlyContinue | Select-Object -Last 1) }
                if (-not $why) { $why = @('no probe.result') }
                $reason = [string]$why[0]
            }
        }
    }
    finally {
        Restore-Env $saved
        Remove-Item -Recurse -Force $pd -ErrorAction SilentlyContinue
    }
    return $reason
}

function Probe-Key($job) {
    $item = Get-Item -LiteralPath $job.DllPath
    '{0}|{1}|{2}|{3}' -f $job.ProbeOf, $item.FullName, $item.Length, $item.LastWriteTimeUtc.Ticks
}

$script:NextJob = 0
$script:LiveProcs = [System.Collections.Generic.List[object]]::new()
function Start-SuiteJob($job) {
    $script:NextJob = $script:NextJob + 1
    $n = $script:NextJob
    $safe = ([string]$job.Name).ToLower() -replace '[^a-z0-9]+', '-'
    $safe = $safe.Trim('-')
    if (-not $safe) { $safe = 'job' }
    $dir = Join-Path $env:TEMP "ppwork\jobs\$('{0:d3}' -f $n)-$safe"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    # run_pair deletes -Work. Logs and the spec stay in the parent folder.
    $job.Work = Join-Path $dir 'work'
    $job.Game = if ($job.Kind -eq 'pair') { "pptest-$n-$safe" } else { '' }
    $job.PeerParty = $PeerParty
    $spec = Join-Path $dir 'spec.ps1'
    Write-Spec $spec $job
    $job.OutFile = Join-Path $dir 'out.txt'
    $job.ErrFile = Join-Path $dir 'err.txt'
    $runner = Join-Path $PSScriptRoot 'sdk\run_job.ps1'
    $arg = "-NoProfile -ExecutionPolicy Bypass -File `"$runner`" -Spec `"$spec`""
    $job.Proc = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList $arg -PassThru -NoNewWindow `
        -RedirectStandardOutput $job.OutFile -RedirectStandardError $job.ErrFile
    if (-not $job.Proc) { throw "couldn't start $($job.Name)" }
    $script:LiveProcs.Add($job.Proc)
}

function Job-Output($job) {
    $lines = @()
    foreach ($f in @($job.OutFile, $job.ErrFile)) {
        if ($f -and (Test-Path -LiteralPath $f)) { $lines += @(Get-Content -LiteralPath $f -ErrorAction SilentlyContinue) }
    }
    return $lines
}

function Finish-SuiteJob($job) {
    $out = Job-Output $job
    if ($job.Kind -eq 'photon') {
        $left = @($out | Select-String 'left out$' | ForEach-Object Line) -join '; '
        if ($out -contains 'PHOTON_TEST_OK') {
            Note $job.Name 'pass' $(if ($left) { $left } else { 'Realtime UDP + TCP + WebSocket, SQL lobby, rejoin, Chat' })
        }
        else {
            $why = @($out | Select-String 'check failed|timed out|FAIL' | Select-Object -First 1 | ForEach-Object Line)
            if (-not $why) { $why = @($out | Where-Object { "$_".Trim() } | Select-Object -Last 1) }
            if (-not $why) { $why = @("exit $($job.Proc.ExitCode)") }
            Note $job.Name 'FAIL' ([string]$why[0])
        }
        return
    }
    $roles = @(Env-List $job.Roles)
    $exits = @($out | Select-String '^---- (\w) \(exit (.*)\)$' | ForEach-Object { $_.Matches[0].Groups[2].Value })
    $skip = $out | Select-String '^SKIP (.*)' | Select-Object -First 1
    if ($skip -and -not ($out | Select-String '^(PASS|FAIL) ')) { Note $job.Name 'skipped' $skip.Matches[0].Groups[1].Value }
    elseif ($exits.Count -eq $roles.Count -and -not ($exits | Where-Object { $_ -ne 'ok' })) { Note $job.Name 'pass' }
    else {
        $fails = @($out | Select-String '^FAIL ' | Select-Object -First 2 | ForEach-Object Line)
        $bits = @()
        $exitText = @($exits) -join ' / '
        if ($exitText) { $bits += $exitText }
        $bits += @($fails | Where-Object { $_ })
        $detail = $bits -join ' | '
        if (-not "$detail".Trim()) { $detail = "exit $($job.Proc.ExitCode)" }
        Note $job.Name 'FAIL' $detail
    }
}

# Four jobs at a time. Two Photon runs would bind the same ports, so a Photon job waits
# until the one already running has finished. Four pairs is eight identity slots (0..32).
function Invoke-Pool($jobs, [int]$width) {
    $pending = [System.Collections.Generic.List[object]]::new()
    foreach ($j in @($jobs)) { $pending.Add($j) }
    $running = [System.Collections.Generic.List[object]]::new()
    while ($pending.Count -gt 0 -or $running.Count -gt 0) {
        while ($running.Count -lt $width -and $pending.Count -gt 0) {
            $photonOn = $false
            foreach ($r in $running) { if ($r.Exclusive -eq 'photon') { $photonOn = $true } }
            $pick = -1
            for ($i = 0; $i -lt $pending.Count; $i++) {
                if ($pending[$i].Exclusive -eq 'photon' -and $photonOn) { continue }
                $pick = $i
                break
            }
            if ($pick -lt 0) { break }
            $job = $pending[$pick]
            $pending.RemoveAt($pick)
            try {
                Start-SuiteJob $job
                $running.Add($job)
                Write-Host "start $($job.Name)"
            }
            catch {
                Note $job.Name 'FAIL' "$_"
            }
            Start-Sleep -Milliseconds 200
        }
        $done = @($running.ToArray() | Where-Object {
            try { $_.Proc.Refresh() | Out-Null; $_.Proc.HasExited } catch { $true }
        })
        if (-not $done) { Start-Sleep -Milliseconds 200; continue }
        foreach ($job in $done) {
            try { [void]$job.Proc.WaitForExit() } catch {}
            $running.Remove($job) | Out-Null
            Finish-SuiteJob $job
            $row = $results[$results.Count - 1]
            Write-Host "$($row.Result) $($job.Name)"
        }
    }
}

# A resolved SDK or Photon run: skipped jobs are noted here and left out.
function New-PairJob($name, $p, $dll, $python, $x86) {
    $roles = if ($p.Roles) { @($p.Roles) } else { @('A', 'B') }
    return @{
        Name = $name; Kind = 'pair'; Exclusive = $null; Steam = [bool]$p.Steam
        Py = $p.Py; Test = (Join-Path $PSScriptRoot "sdk\$($p.Py)")
        Runner = (Join-Path $PSScriptRoot 'sdk\run_pair.ps1')
        Var = $p.Var; DllPath = $dll; File = [bool]$p.File; Python = $python; X86 = [bool]$x86
        Env = @(Env-List $p.Env); RoleEnv = @(Env-List $p.RoleEnv); Roles = $roles
        Probe = [bool]$p.Probe; ProbeOf = $p.ProbeOf; NeedsRoom = [bool]$p.NeedsRoom
    }
}

$savedHooks = [Environment]::GetEnvironmentVariable('PEERPARTY_HOOKS', 'Process')
function Restore-Hooks {
    if ($null -eq $savedHooks) { Remove-Item Env:\PEERPARTY_HOOKS -ErrorAction SilentlyContinue }
    else { Set-Item Env:\PEERPARTY_HOOKS -Value $savedHooks }
}

Push-Location $root
try {
    if (Want 'cargo') {
        # Launcher unit tests never inject, so the nested hook build skips x86.
        $env:PEERPARTY_HOOKS = 'x64'
        cargo test --workspace 2>&1 | Tee-Object -Variable out | Select-String '^test result|FAILED|panicked' | ForEach-Object Line
        if ($LASTEXITCODE -eq 0) { Note 'cargo test' 'pass' } else { Note 'cargo test' 'FAIL' 'see output above' }
    }

    # Which SDK DLLs this run will load, before the exe build, so a 32-bit one can
    # include the x86 hook. The room code waits until that exe exists.
    $prepared = [System.Collections.Generic.List[object]]::new()
    if (Want 'photon') {
        $given = $env:PPTEST_PHOTON_DIR
        $builds = [ordered]@{ '' = $null }
        if ($AllBuilds -and $Matrix['PPTEST_PHOTON_DIR']) { $builds = $Matrix['PPTEST_PHOTON_DIR'] }
        foreach ($build in @($builds.Keys | Where-Object { -not $_ -or $_ -like $Label })) {
            $name = if ($build) { "photon $build" } else { 'photon' }
            $dir = if ($build) { Copy-Build $build @($builds[$build]) } elseif ($given) { $given } else { Copy-Dlls 'photon' @('Photon3Unity3D.dll', 'PhotonRealtime.dll', 'PhotonChat.dll') }
            if (-not $dir) { Note $name 'skipped' "needs a game's Photon DLLs in $copies\photon (or PPTEST_PHOTON_DIR)" }
            elseif (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) { Note $name 'skipped' 'needs the .NET SDK' }
            else {
                $prepared.Add(@{
                    Name = $name; Kind = 'photon'; Exclusive = 'photon'; Steam = $false; X86 = $false
                    Runner = (Join-Path $PSScriptRoot 'photon\run.ps1'); Test = ''; PhotonDir = $dir
                    Env = @(); RoleEnv = @(); Roles = @(); Probe = $false; NeedsRoom = $false; File = $false
                })
            }
        }
    }

    $eos = 'EOSSDK-Win64-Shipping.dll'
    $p1 = 41000 + (Get-Random -Maximum 2000) * 2
    # The room code is stamped on after --new-room. LAN discovery stays off.
    $inet = @('PEERPARTY_INTERNET=1', 'PEERPARTY_LAN_DISCOVERY=0', 'PEERPARTY_INET_DHT=0', 'PEERPARTY_INET_STUN=0', 'PEERPARTY_INET_PORTMAP=0', 'PEERPARTY_IPV6=0')
    $pairs = @(
        @{ Name = 'eos_rtc'; Py = 'eos_rtc_test.py'; Var = 'PPTEST_EOS_DLL'; Files = @($eos); File = $true; Env = @('PEERPARTY_VOICE=0'); Probe = $true; ProbeOf = 'eos' },
        @{ Name = 'eos_rtc_lobby'; Py = 'eos_rtc_lobby_test.py'; Var = 'PPTEST_EOS_DLL'; Files = @($eos); File = $true; Env = @('PEERPARTY_VOICE=0'); Probe = $true; ProbeOf = 'eos' },
        @{ Name = 'eos_custominvites'; Py = 'eos_custominvites_test.py'; Var = 'PPTEST_EOS_DLL'; Files = @($eos); File = $true; Env = @('PEERPARTY_INVITES=auto'); Probe = $true; ProbeOf = 'eos' },
        @{ Name = 'party_chat'; Py = 'party_chat_test.py'; Var = 'PPTEST_PARTY_DLL'; Files = @('PartyWin.dll'); File = $true; Env = @('PEERPARTY_VOICE=0') },
        @{ Name = 'pf_core'; Py = 'pf_core_test.py'; Var = 'PPTEST_PFCORE_DIR'; Files = @('libHttpClient.Win32.dll', 'PlayFabCore.Win32.dll'); File = $false; Env = @(); Roles = @('A') },
        @{ Name = 'ugs'; Py = 'ugs_test.py'; Var = $null; Files = @(); File = $false; Env = @('PEERPARTY_UGS=1') },
        @{ Name = 'vivox'; Py = 'vivox_test.py'; Var = 'PPTEST_VIVOX_DIR'; Files = @('VivoxNative.dll', 'vivoxsdk.dll'); File = $false; Env = @('PEERPARTY_VIVOX=1', 'PEERPARTY_VOICE=0') },
        @{ Name = 'pf_rest'; Py = 'pf_rest_test.py'; Var = $null; Files = @(); File = $false; Env = @('PEERPARTY_PLAYFAB=1') },
        @{ Name = 'galaxy'; Py = 'galaxy_test.py'; Var = 'PPTEST_GALAXY_DLL'; Files = @('Galaxy64.dll'); File = $true; Env = @() },
        # Needs the Steam client running and PPTEST_STEAM_APPID = a game the account owns.
        # Its own probe key: a different DLL and script from steam_own_id. Runs after the pool.
        @{ Name = 'steam_license'; Py = 'steam_license_test.py'; Var = 'PPTEST_STEAM_DLL'; Files = @('steam_api64.dll'); File = $true; Env = @("SteamAppId=$env:PPTEST_STEAM_APPID"); Steam = $true; Probe = $true; ProbeOf = 'steam_license' },
        # Needs the Steam client running. The game is shown a SteamID of its own for this PC
        # (steamown.rs, saves.rs), once per callback dispatch.
        @{ Name = 'steam_own_id'; Py = 'steam_own_id_test.py'; Var = 'PPTEST_STEAM_OWN_DLL'; Files = @('steam_api64.dll'); File = $true; Env = @('SteamAppId=480', 'PEERPARTY_MODE_STEAMID=lan'); Steam = $true; Roles = @('A'); Probe = $true; ProbeOf = 'steam_own' },
        @{ Name = 'steam_own_id_classic'; Dlls = 'steam_own_id'; Py = 'steam_own_id_test.py'; Var = 'PPTEST_STEAM_OWN_DLL'; Files = @('steam_api64.dll'); File = $true; Env = @('SteamAppId=480', 'PEERPARTY_MODE_STEAMID=lan', 'PPTEST_DISPATCH=classic'); Steam = $true; Roles = @('A'); Probe = $true; ProbeOf = 'steam_own' },
        @{ Name = 'ipfwd'; Py = 'ipfwd_test.py'; Var = $null; Files = @(); File = $false; Env = $inet; NeedsRoom = $true; RoleEnv = @("A:PEERPARTY_INET_PORT=$p1", "B:PEERPARTY_INET_PORT=$($p1 + 1)", "B:PEERPARTY_INET_PEERS=127.0.0.1:$p1") }
    )
    foreach ($p in $pairs) {
        if (-not (Want $p.Name)) { continue }
        if ($p.Steam) {
            if (-not $env:PPTEST_STEAM_APPID) { Note $p.Name 'skipped' 'needs PPTEST_STEAM_APPID (a game this Steam account owns)'; continue }
            if (-not (Get-Process steam -ErrorAction SilentlyContinue)) { Note $p.Name 'skipped' 'needs the Steam client running'; continue }
        }
        $builds = [ordered]@{ '' = $null }
        if ($AllBuilds -and $p.Var -and $Matrix[$p.Var]) { $builds = $Matrix[$p.Var] }
        foreach ($build in @($builds.Keys | Where-Object { -not $_ -or $_ -like $Label })) {
            $name = if ($build) { "$($p.Name) $build" } else { $p.Name }
            $have = $null
            if ($build) {
                $dir = Copy-Build $build @($builds[$build])
                if (-not $dir) { Note $name 'skipped' "the build's files are gone and no copy was kept"; continue }
                $have = if ($p.File) { Join-Path $dir (Split-Path @($builds[$build])[0] -Leaf) } else { $dir }
            }
            elseif ($p.Var) {
                $have = [Environment]::GetEnvironmentVariable($p.Var)
                if (-not $have) {
                    $kept = if ($p.Dlls) { $p.Dlls } else { $p.Name }
                    $dir = Copy-Dlls $kept $p.Files
                    if (-not $dir) { Note $name 'skipped' "needs $($p.Files -join ', ') in $copies\$kept (or $($p.Var))"; continue }
                    $have = if ($p.File) { Join-Path $dir $p.Files[0] } else { $dir }
                }
            }
            $python = $null
            $x86 = $false
            if ($p.File) {
                if (-not (Test-Path -LiteralPath $have)) { Note $name 'skipped' "DLL not found: $have"; continue }
                $have = (Resolve-Path -LiteralPath $have).Path
                $x86 = Test-X86 $have
                if ($x86) {
                    $python = Get-Python32
                    if (-not $python) { Note $name 'skipped' 'a 32-bit DLL: needs a 32-bit Python (PPTEST_PYTHON32, py -3-32 or uv)'; continue }
                }
            }
            $prepared.Add((New-PairJob $name $p $have $python $x86))
        }
    }

    $needX86 = $false
    foreach ($j in $prepared) { if ($j.X86) { $needX86 = $true } }
    if (-not $SkipBuild) {
        # cargo test and this build share a target dir, so they stay one after the other.
        if ($needX86) { Remove-Item Env:\PEERPARTY_HOOKS -ErrorAction SilentlyContinue }
        else { $env:PEERPARTY_HOOKS = 'x64' }
        cargo build --profile testexe -p peerparty 2>&1 | Tee-Object -Variable buildOut | Select-Object -Last 4
        if ($LASTEXITCODE -ne 0) {
            $buildOut | Select-String 'error' | Select-Object -Last 20 | ForEach-Object Line
            throw 'testexe build failed'
        }
    }
    elseif ($prepared.Count -gt 0 -and -not (Test-Path -LiteralPath $PeerParty)) {
        throw "no testexe build at $PeerParty (run without -SkipBuild)"
    }
    # Children inherit this process. Put the caller's hook setting back before they start.
    Restore-Hooks

    $needRoom = @($prepared | Where-Object { $_.NeedsRoom })
    if ($needRoom) {
        $raw = (& $PeerParty --new-room 2>&1 | Out-String).Trim()
        $code = if ($raw -match 'PP1-[A-Z0-9-]+') { $Matches[0] } else { $null }
        if (-not $code) {
            foreach ($j in $needRoom) { Note $j.Name 'FAIL' "no room code from --new-room: $raw" }
            $keptJobs = [System.Collections.Generic.List[object]]::new()
            foreach ($j in @($prepared)) { if (-not $j.NeedsRoom) { $keptJobs.Add($j) } }
            $prepared = $keptJobs
        }
        else {
            foreach ($j in $needRoom) { $j.Env = [string[]](@("PEERPARTY_ROOM=$code") + @(Env-List $j.Env)) }
        }
    }

    # Once per probe family and DLL (path, length, mtime). A failure skips the rest.
    $probes = @{}
    $runnable = [System.Collections.Generic.List[object]]::new()
    foreach ($job in @($prepared)) {
        if (-not $job.Probe) { $runnable.Add($job); continue }
        $key = Probe-Key $job
        if ($probes.ContainsKey($key)) {
            if ($probes[$key] -eq 'ok') { $runnable.Add($job) }
            else { Note $job.Name 'skipped' $probes[$key] }
            continue
        }
        $why = Invoke-Probe $job
        if ($why) {
            $probes[$key] = "doesn't start here without peerparty either: $why"
            Note $job.Name 'skipped' $probes[$key]
        }
        else {
            $probes[$key] = 'ok'
            $runnable.Add($job)
        }
    }

    $pool = [System.Collections.Generic.List[object]]::new()
    $steam = [System.Collections.Generic.List[object]]::new()
    foreach ($job in $runnable) {
        if ($job.Steam) { $steam.Add($job) } else { $pool.Add($job) }
    }
    Invoke-Pool $pool 4
    Invoke-Pool $steam 1
}
finally {
    foreach ($p in @($script:LiveProcs)) {
        try {
            if ($p -and -not $p.HasExited) { & taskkill.exe /PID $p.Id /T /F 2>&1 | Out-Null }
        }
        catch {}
    }
    Restore-Hooks
    Pop-Location
}

''
$results | Format-Table -AutoSize | Out-String -Width 200
if ($results | Where-Object Result -eq 'FAIL') { exit 1 }
