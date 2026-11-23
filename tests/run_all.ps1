# Every automated peerparty test in one go:
#   1. cargo test (unit and loopback tests: hook, core, launcher), through cargo-nextest when
#      it's installed (every test in a process of its own, a hung one stopped after 5 min:
#      .config/nextest.toml), else plain cargo test. PEERPARTY_HOOKS=none:
#      the launcher's unit tests never inject, so its build script embeds no hook DLLs
#      and doesn't build them. The hook's and the core's unit tests then run again as
#      32-bit code (--target i686-pc-windows-msvc): 32-bit games run that build, with its
#      own layouts and pointer sizes.
#   2. a testexe build (release optimizations, no link-time optimization, incremental).
#      The tests below run a copy of that peerparty.exe taken for this run, so a build
#      while they run (tools\build.ps1, another run's) can overwrite the original.
#      The shipping exe stays a release build. -SkipBuild reuses the last testexe build.
#      A 32-bit test DLL builds the x86 hook too; otherwise the hook build is x64 only.
#   3. Photon's own .NET client (tests/photon/run.ps1): Realtime over UDP, TCP and WebSocket,
#      an SQL lobby, rejoin with a PlayerTTL, and Chat. Incremental: a later run with the
#      same Photon DLLs and the same test sources skips `dotnet build`.
#   4. the two-copy SDK tests (tests/sdk/run_pair.ps1): EOS RTC, EOS lobby voice, EOS custom
#      invites, PlayFab Party chat, PlayFabCore sign-in, PlayFab's web API, Unity Gaming
#      Services, Vivox voice (Unity's VivoxNative.dll), GOG Galaxy, Steam licence checks (with the Steam client running), an older SDK's steam_api (old accessors,
#      classic dispatch), a starter handing over to the game, a two-player split-screen
#      session from start to Close all (its window hidden), and typed-IP games on async
#      sockets (two copies in a room)
#
# Two runs at once (two checkouts, a second terminal) never meet: each run has its own id
# ($RunId), and with it its own work folder (%TEMP%\ppwork\run-<id>), its own game keys
# and room codes, a PeerParty folder of its own for every job (PEERPARTY_DATA_DIR: identity
# slots, the peer board, settings), Photon servers on an address of its own
# (PEERPARTY_PHOTON_IP, 127.81.x.y) and ports claimed for it alone (the same lock files
# core/src/test_ports.rs uses). The jobs that use the one Steam client take turns across
# runs (a named mutex), as do updates of the kept DLL copies.
#
# SDK pairs and Photon run -Jobs at a time (default: half the logical processors, 4 to 8). The Steam tests run one at a time afterwards: they use the real Steam client. EOS and Steam
# builds are probed once per DLL, all at once; a DLL whose probe passed isn't probed
# again while it and the test script stay the same (probe-cache.txt beside the kept
# copies).
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
# Usage: tests\run_all.ps1 [-SkipBuild] [-AllBuilds [-Label <pattern>]] [-Only photon,eos_rtc,...] [-Jobs <n>]
param([switch]$SkipBuild, [switch]$AllBuilds, [string]$Label = '*', [string[]]$Only = @(), [int]$Jobs = 0)
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
$suiteStart = Get-Date
# Each pair is two game copies and their PeerParty: half the logical processors keeps
# them from starving each other (and the timing checks in their scripts).
if ($Jobs -lt 1) { $Jobs = [Math]::Max(4, [Math]::Min(8, [int]([Environment]::ProcessorCount / 2))) }
# .cargo/config.toml compiles through sccache: without it every cargo step would fail
# (PowerShell can't set RUSTC_WRAPPER to the empty value that turns it off).
if (-not $env:RUSTC_WRAPPER -and -not (Get-Command sccache -ErrorAction SilentlyContinue)) {
    Write-Error 'sccache is not installed: winget install Mozilla.sccache (or take rustc-wrapper out of .cargo\config.toml)'
    exit 1
}
$nextest = [bool](Get-Command cargo-nextest -ErrorAction SilentlyContinue)

# This run's own names, so another run at the same time never shares a folder, game key,
# room code or port with it.
$RunId = '{0}-{1}' -f $PID, [guid]::NewGuid().ToString('n').Substring(0, 6)
$work = Join-Path $env:TEMP 'ppwork'
$RunDir = Join-Path $work "run-$RunId"
New-Item -ItemType Directory -Force -Path $RunDir | Out-Null
# Earlier runs' folders, a day old (a folder a run still uses has a file open: it stays).
Get-ChildItem -LiteralPath $work -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like 'run-*' -and $_.Name -ne "run-$RunId" -and $_.LastWriteTime -lt (Get-Date).AddDays(-1) } |
    ForEach-Object { Remove-Item -LiteralPath $_.FullName -Recurse -Force -ErrorAction SilentlyContinue }
"run ${RunId}: $RunDir ($Jobs jobs at a time)"

# Where cargo builds: CARGO_TARGET_DIR, else what .cargo/config.toml says (cargo knows).
function Get-TargetDir {
    if ($env:CARGO_TARGET_DIR) { return [IO.Path]::GetFullPath((Join-Path $root $env:CARGO_TARGET_DIR)) }
    Push-Location $root
    try { return (cargo metadata --format-version 1 --no-deps 2>$null | ConvertFrom-Json).target_directory }
    finally { Pop-Location }
}
$Built = Join-Path (Get-TargetDir) 'testexe\peerparty.exe'
# This run's copy of it (step 2).
$PeerParty = Join-Path $RunDir 'peerparty.exe'
function Want($name) { $Only.Count -eq 0 -or $Only -contains $name }
function Note($name, $state, $detail = '', $secs = $null) {
    $t = if ($null -ne $secs) { '{0:n1}s' -f $secs } else { '' }
    $results.Add([pscustomobject]@{ Test = $name; Result = $state; Time = $t; Detail = $detail })
}

# A named mutex for this PC, held while $body runs: what two runs mustn't do at once.
function Use-Lock([string]$name, [scriptblock]$body) {
    $m = New-Object System.Threading.Mutex($false, "Local\$name")
    try { [void]$m.WaitOne() } catch [System.Threading.AbandonedMutexException] {}
    try { & $body }
    finally { $m.ReleaseMutex(); $m.Dispose() }
}

# Copies $from into $dir unless the copy there is already the same file (size and time):
# another run may have that copy loaded, and an unchanged file needn't be touched.
function Sync-Item([string]$from, [string]$dir) {
    $item = Get-Item -LiteralPath $from
    $to = Join-Path $dir $item.Name
    if ($item.PSIsContainer) { Copy-Item -LiteralPath $from $dir -Recurse -Force; return }
    $have = Get-Item -LiteralPath $to -ErrorAction SilentlyContinue
    if ($have -and $have.Length -eq $item.Length -and $have.LastWriteTimeUtc -eq $item.LastWriteTimeUtc) { return }
    Copy-Item -LiteralPath $from $to -Force
}

# The kept copies of a test's DLLs (tests never touch a game's folder): refreshed from
# $Sources when those files exist, else the copies from an earlier run.
function Copy-Dlls($name, [string[]]$leaves) {
    if (-not $leaves.Count) { return $null }
    $dir = Join-Path $copies $name
    $files = @($Sources[$name] | Where-Object { $_ })
    if ($files.Count -and -not ($files | Where-Object { -not (Test-Path $_) })) {
        Use-Lock 'PeerPartyTestDlls' {
            New-Item -ItemType Directory -Force $dir | Out-Null
            foreach ($f in $files) { Sync-Item $f $dir }
        }
    }
    if (-not ($leaves | Where-Object { -not (Test-Path (Join-Path $dir $_)) })) { return $dir }
    return $null
}

# The kept copy of one -AllBuilds build: the DLL and the files beside it, each build in
# its own folder.
function Copy-Build($label, [string[]]$files) {
    $dir = Join-Path $copies "builds\$label"
    if (-not ($files | Where-Object { -not (Test-Path -LiteralPath $_) })) {
        Use-Lock 'PeerPartyTestDlls' {
            New-Item -ItemType Directory -Force $dir | Out-Null
            foreach ($f in $files) { Sync-Item $f $dir }
        }
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

# Ports for this run alone: a block of core/src/test_ports.rs's (same range, same lock
# files), held open with no sharing until this script exits, once each of its ports
# binds for TCP and UDP on 127.0.0.1.
$script:PortLocks = [System.Collections.Generic.List[object]]::new()
function Test-PortFree([int]$p) {
    try { $t = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $p); $t.Start(); $t.Stop() } catch { return $false }
    try { $u = [Net.Sockets.UdpClient]::new([Net.IPEndPoint]::new([Net.IPAddress]::Loopback, $p)); $u.Close() } catch { return $false }
    return $true
}
function Claim-Ports([int]$n) {
    $block = 64
    $first = 10000
    $blocks = [int]((48000 - $first) / $block)
    $dir = Join-Path $env:TEMP 'peerparty-test-ports'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $start = Get-Random -Maximum $blocks
    for ($i = 0; $i -lt $blocks; $i++) {
        $b = ($start + $i) % $blocks
        $base = $first + $b * $block
        try { $f = [IO.File]::Open((Join-Path $dir "$b.lock"), 'OpenOrCreate', 'ReadWrite', 'None') } catch { continue }
        $ok = $true
        for ($p = $base; $p -lt $base + $n; $p++) { if (-not (Test-PortFree $p)) { $ok = $false; break } }
        if ($ok) { $script:PortLocks.Add($f); return $base }
        $f.Dispose()
    }
    throw "no block of $n free ports"
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
    $s = @{ PEERPARTY_LOOPBACK = '1'; PEERPARTY_DATA_DIR = [string]$job.DataDir }
    if ($job.Kind -eq 'photon') {
        $s['PPTEST_PHOTON_DIR'] = [string]$job.PhotonDir
        $s['PPTEST_DIR'] = [string]$job.Work
        $s['PEERPARTY_MODE_PHOTON'] = 'lan'
        $s['PEERPARTY_PHOTON_IP'] = [string]$job.PhotonIp
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
        "`$Direct = `$$([bool]$job.Direct)"
        '$Session = @{'
    )
    foreach ($k in @($session.Keys)) { $body += "    $(Quote $k) = $(Quote ([string]$session[$k]))" }
    $body += '}'
    Set-Content -LiteralPath $path -Value $body -Encoding UTF8
}

# A probe's process: python with the probe's variables in its own environment (this
# process's stays as it is, so probes can run side by side), no console window.
function Start-Probe($job) {
    $pd = Join-Path $RunDir ('probe-' + [guid]::NewGuid().ToString('n'))
    New-Item -ItemType Directory -Force -Path $pd | Out-Null
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = if ($job.Python) { [string]$job.Python } else { (Get-Command python).Source }
    $psi.Arguments = "`"$($job.Test)`" probe"
    $psi.WorkingDirectory = Split-Path -Parent $job.Test
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardError = $true
    $psi.RedirectStandardOutput = $true
    foreach ($kv in (Env-List $job.Env)) {
        $k, $v = $kv -split '=', 2
        $psi.EnvironmentVariables[$k] = $v
    }
    if ($job.Var) { $psi.EnvironmentVariables[[string]$job.Var] = [string]$job.DllPath }
    if ($job.Python) { $psi.EnvironmentVariables['PPTEST_PYTHON'] = [string]$job.Python }
    $psi.EnvironmentVariables['PPTEST_DIR'] = $pd
    $psi.EnvironmentVariables['PEERPARTY_DATA_DIR'] = Join-Path $pd 'data'
    $p = [System.Diagnostics.Process]::Start($psi)
    # Read both pipes as they fill, or a chatty probe would stop on a full one.
    @{ Dir = $pd; Proc = $p; Out = $p.StandardOutput.ReadToEndAsync(); Err = $p.StandardError.ReadToEndAsync(); Started = Get-Date }
}

# $null when the SDK started. Otherwise the first FAIL/SKIP line (or the interpreter's).
function Finish-Probe($probe) {
    $pd = $probe.Dir
    $reason = $null
    try {
        $left = 60000 - ((Get-Date) - $probe.Started).TotalMilliseconds
        if (-not $probe.Proc.WaitForExit([Math]::Max(0, [int]$left))) { & taskkill.exe /PID $probe.Proc.Id /T /F 2>&1 | Out-Null }
        $result = Get-Content (Join-Path $pd 'probe.result') -ErrorAction SilentlyContinue
        if ($result -ne 'ok') {
            $why = @(Get-Content (Join-Path $pd 'probe.log') -ErrorAction SilentlyContinue | Where-Object { $_ -match '^(FAIL|SKIP) ' } | Select-Object -First 1)
            if (-not $why) {
                $err = if ($probe.Err.Wait(2000)) { $probe.Err.Result } else { '' }
                $why = @("$err" -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -Last 1)
            }
            if (-not $why) { $why = @('no probe.result') }
            $reason = [string]$why[0]
        }
    }
    finally {
        Remove-Item -Recurse -Force $pd -ErrorAction SilentlyContinue
    }
    return $reason
}

# Which probes passed before: one line per key (Probe-Key). A DLL that started once
# starts again; one that didn't is probed every run (the reason may have gone).
$probeCache = Join-Path $copies 'probe-cache.txt'
function Probe-Key($job) {
    $item = Get-Item -LiteralPath $job.DllPath
    $script = Get-Item -LiteralPath $job.Test
    '{0}|{1}|{2}|{3}|{4}' -f $job.ProbeOf, $item.FullName, $item.Length, $item.LastWriteTimeUtc.Ticks, $script.LastWriteTimeUtc.Ticks
}

$script:NextJob = 0
$script:LiveProcs = [System.Collections.Generic.List[object]]::new()
# 127.81.x.y addresses for this run's Photon servers, from a random start.
$script:PhotonNext = Get-Random -Maximum 60000
function Start-SuiteJob($job) {
    $script:NextJob = $script:NextJob + 1
    $n = $script:NextJob
    $safe = ([string]$job.Name).ToLower() -replace '[^a-z0-9]+', '-'
    $safe = $safe.Trim('-')
    if (-not $safe) { $safe = 'job' }
    $dir = Join-Path $RunDir "jobs\$('{0:d3}' -f $n)-$safe"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    # run_pair deletes -Work. Logs and the spec stay in the parent folder.
    $job.Work = Join-Path $dir 'work'
    $job.DataDir = Join-Path $dir 'data'
    $job.Game = if ($job.Kind -eq 'pair') { "pptest-$RunId-$n-$safe" } else { '' }
    if ($job.Kind -eq 'photon') {
        $v = $script:PhotonNext
        $script:PhotonNext = ($v + 1) % 60000
        $job.PhotonIp = '127.81.{0}.{1}' -f ([int][Math]::Floor($v / 250) % 250 + 1), ($v % 250 + 1)
    }
    $job.PeerParty = $PeerParty
    $spec = Join-Path $dir 'spec.ps1'
    Write-Spec $spec $job
    $job.OutFile = Join-Path $dir 'out.txt'
    $job.ErrFile = Join-Path $dir 'err.txt'
    $runner = Join-Path $PSScriptRoot 'sdk\run_job.ps1'
    $arg = "-NoProfile -ExecutionPolicy Bypass -File `"$runner`" -Spec `"$spec`""
    $job.Started = Get-Date
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
    $secs = ((Get-Date) - $job.Started).TotalSeconds
    if ($job.Kind -eq 'photon') {
        $left = @($out | Select-String 'left out$' | ForEach-Object Line) -join '; '
        if ($out -contains 'PHOTON_TEST_OK') {
            Note $job.Name 'pass' $(if ($left) { $left } else { 'Realtime UDP + TCP + WebSocket, SQL lobby, rejoin, Chat' }) $secs
        }
        else {
            $why = @($out | Select-String 'check failed|timed out|FAIL' | Select-Object -First 1 | ForEach-Object Line)
            if (-not $why) { $why = @($out | Where-Object { "$_".Trim() } | Select-Object -Last 1) }
            if (-not $why) { $why = @("exit $($job.Proc.ExitCode)") }
            Note $job.Name 'FAIL' ([string]$why[0]) $secs
        }
        return
    }
    $roles = @(Env-List $job.Roles)
    $exits = @($out | Select-String '^---- (\w) \(exit (.*)\)$' | ForEach-Object { $_.Matches[0].Groups[2].Value })
    $skip = $out | Select-String '^SKIP (.*)' | Select-Object -First 1
    if ($skip -and -not ($out | Select-String '^(PASS|FAIL) ')) { Note $job.Name 'skipped' $skip.Matches[0].Groups[1].Value $secs }
    # Counted, not tested for truth: a role that wrote no result is an empty string, which
    # PowerShell takes for false.
    elseif ($exits.Count -eq $roles.Count -and @($exits | Where-Object { $_ -ne 'ok' }).Count -eq 0) { Note $job.Name 'pass' '' $secs }
    else {
        $fails = @($out | Select-String '^FAIL ' | Select-Object -First 2 | ForEach-Object Line)
        $bits = @()
        $exitText = @($exits) -join ' / '
        if ($exitText) { $bits += $exitText }
        $bits += @($fails | Where-Object { $_ })
        $detail = $bits -join ' | '
        if (-not "$detail".Trim()) { $detail = "exit $($job.Proc.ExitCode)" }
        Note $job.Name 'FAIL' $detail $secs
    }
}

# $width jobs at a time. Each has its own PeerParty folder, game key and (Photon) server
# address, so none waits for another.
function Invoke-Pool($jobs, [int]$width) {
    $pending = [System.Collections.Generic.Queue[object]]::new()
    foreach ($j in @($jobs)) { $pending.Enqueue($j) }
    $running = [System.Collections.Generic.List[object]]::new()
    while ($pending.Count -gt 0 -or $running.Count -gt 0) {
        while ($running.Count -lt $width -and $pending.Count -gt 0) {
            $job = $pending.Dequeue()
            try {
                Start-SuiteJob $job
                $running.Add($job)
                Write-Host "start $($job.Name)"
            }
            catch {
                Note $job.Name 'FAIL' "$_"
            }
        }
        $done = @($running.ToArray() | Where-Object {
            try { $_.Proc.Refresh() | Out-Null; $_.Proc.HasExited } catch { $true }
        })
        if (-not $done) { Start-Sleep -Milliseconds 50; continue }
        foreach ($job in $done) {
            try { [void]$job.Proc.WaitForExit() } catch {}
            $running.Remove($job) | Out-Null
            Finish-SuiteJob $job
            $row = $results[$results.Count - 1]
            Write-Host "$($row.Result) $($job.Name) ($($row.Time))"
        }
    }
}

# A resolved SDK or Photon run: skipped jobs are noted here and left out.
function New-PairJob($name, $p, $dll, $python, $x86) {
    $roles = if ($p.Roles) { @($p.Roles) } else { @('A', 'B') }
    return @{
        Name = $name; Kind = 'pair'; Steam = [bool]$p.Steam
        Py = $p.Py; Test = (Join-Path $PSScriptRoot "sdk\$($p.Py)")
        Runner = (Join-Path $PSScriptRoot 'sdk\run_pair.ps1')
        Var = $p.Var; DllPath = $dll; File = [bool]$p.File; Python = $python; X86 = [bool]$x86
        Env = @(Env-List $p.Env); RoleEnv = @(Env-List $p.RoleEnv); Roles = $roles
        Probe = [bool]$p.Probe; ProbeOf = $p.ProbeOf; NeedsRoom = [bool]$p.NeedsRoom; Direct = [bool]$p.Direct
    }
}

$savedHooks = [Environment]::GetEnvironmentVariable('PEERPARTY_HOOKS', 'Process')
function Restore-Hooks {
    if ($null -eq $savedHooks) { Remove-Item Env:\PEERPARTY_HOOKS -ErrorAction SilentlyContinue }
    else { Set-Item Env:\PEERPARTY_HOOKS -Value $savedHooks }
}

# One cargo test run, timed: cargo-nextest's when it's installed (its summary, and each
# failed, hung or slow test with its output), else cargo test's.
function Invoke-CargoTests([string]$name, [string[]]$cargoArgs) {
    $t = Get-Date
    if ($nextest) {
        cargo nextest run @cargoArgs 2>&1 | Select-String '^\s*Summary|^\s*(FAIL|TIMEOUT|SLOW|SIGSEGV|ABORT|LEAK)\b|panicked|error(\[|:)' | ForEach-Object Line
    }
    else {
        cargo test @cargoArgs 2>&1 | Select-String '^test result|FAILED|panicked|error(\[|:)' | ForEach-Object Line
    }
    $secs = ((Get-Date) - $t).TotalSeconds
    if ($LASTEXITCODE -eq 0) { Note $name 'pass' '' $secs } else { Note $name 'FAIL' 'see output above' $secs }
}

Push-Location $root
try {
    if (Want 'cargo') {
        # Launcher unit tests never inject: no hook DLLs to build or embed.
        $env:PEERPARTY_HOOKS = 'none'
        $runner = if ($nextest) { 'cargo nextest' } else { 'cargo test' }
        Invoke-CargoTests $runner @('--workspace', '--no-fail-fast')
        Invoke-CargoTests "$runner (32-bit)" @('-p', 'peerparty_core', '-p', 'peerparty_hook', '--lib', '--target', 'i686-pc-windows-msvc')
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
                    Name = $name; Kind = 'photon'; Steam = $false; X86 = $false
                    Runner = (Join-Path $PSScriptRoot 'photon\run.ps1'); Test = ''; PhotonDir = $dir
                    Env = @(); RoleEnv = @(); Roles = @(); Probe = $false; NeedsRoom = $false; File = $false
                })
            }
        }
    }

    $eos = 'EOSSDK-Win64-Shipping.dll'
    # Two internet ports and the game's port, which nobody else on this PC has: this
    # run's. A fixed game port once met another run's copy of the same test.
    $p1 = if (Want 'ipfwd') { Claim-Ports 3 } else { 0 }
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
        # A game from before the flat API: SteamMatchmaking() and classic dispatch, through a
        # steam_api64.dll from an older SDK (with any files it reads kept beside it). Its
        # lobby's LobbyCreated_t must reach the game. No Steam client needed when the DLL
        # doesn't ask for one.
        @{ Name = 'steam_legacy'; Py = 'steam_legacy_test.py'; Var = 'PPTEST_STEAM_LEGACY_DLL'; Files = @('steam_api64.dll'); File = $true; Env = @('SteamAppId=480'); Roles = @('A') },
        # A starter that starts the game, with a room code set: the game, not the starter,
        # is this PC's first player. Only loads the DLL: no Steam client needed. Each with
        # a room of its own (the two run side by side).
        @{ Name = 'starter'; Dlls = 'steam_own_id'; Py = 'starter_test.py'; Var = 'PPTEST_STARTER_DLL'; Files = @('steam_api64.dll'); File = $true; Env = @("PEERPARTY_ROOM=starter-$RunId-1", 'PEERPARTY_INET_DHT=0', 'PEERPARTY_INET_STUN=0', 'PEERPARTY_INET_PORTMAP=0'); Roles = @('A') },
        # The same with a menu first (5 s): the starter starts its LAN, then gives it up.
        @{ Name = 'starter_menu'; Dlls = 'steam_own_id'; Py = 'starter_test.py'; Var = 'PPTEST_STARTER_DLL'; Files = @('steam_api64.dll'); File = $true; Env = @("PEERPARTY_ROOM=starter-$RunId-2", 'PEERPARTY_INET_DHT=0', 'PEERPARTY_INET_STUN=0', 'PEERPARTY_INET_PORTMAP=0', 'PPTEST_STARTER_MENU=5'); Roles = @('A') },
        # One peerparty, two copies, a starter in each that hands over to the game, then
        # Close all. The test starts peerparty itself; the session window stays hidden.
        # No SDK DLL.
        @{ Name = 'split_screen'; Py = 'split_screen_test.py'; Var = $null; Files = @(); File = $false; Env = @(); Roles = @('A'); Direct = $true },
        @{ Name = 'ipfwd'; Py = 'ipfwd_test.py'; Var = $null; Files = @(); File = $false; Env = @($inet + "PPTEST_IPFWD_PORT=$($p1 + 2)"); NeedsRoom = $true; RoleEnv = @("A:PEERPARTY_INET_PORT=$p1", "B:PEERPARTY_INET_PORT=$($p1 + 1)", "B:PEERPARTY_INET_PEERS=127.0.0.1:$p1") }
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
        $t = Get-Date
        cargo build --profile testexe -p peerparty 2>&1 | Tee-Object -Variable buildOut | Select-Object -Last 4
        if ($LASTEXITCODE -ne 0) {
            $buildOut | Select-String 'error' | Select-Object -Last 20 | ForEach-Object Line
            throw 'testexe build failed'
        }
        Note 'testexe build' 'pass' '' ((Get-Date) - $t).TotalSeconds
    }
    elseif ($prepared.Count -gt 0 -and -not (Test-Path -LiteralPath $Built)) {
        throw "no testexe build at $Built (run without -SkipBuild)"
    }
    # This run's own copy: a build while the tests run can then replace the original.
    if ($prepared.Count -gt 0) { Copy-Item -LiteralPath $Built $PeerParty -Force }
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

    # Once per probe family and DLL (path, length, mtime, test script). A failure skips
    # the rest. Probes not passed before all run at once.
    $passed = @{}
    foreach ($line in @(Get-Content -LiteralPath $probeCache -ErrorAction SilentlyContinue)) { if ($line) { $passed[$line] = $true } }
    $probes = @{}
    $started = @{}
    foreach ($job in @($prepared)) {
        if (-not $job.Probe) { continue }
        $key = Probe-Key $job
        if ($passed.ContainsKey($key)) { $probes[$key] = 'ok' }
        elseif (-not $started.ContainsKey($key)) { $started[$key] = Start-Probe $job }
    }
    $fresh = @()
    foreach ($key in @($started.Keys)) {
        $why = Finish-Probe $started[$key]
        if ($why) { $probes[$key] = "doesn't start here without peerparty either: $why" }
        else { $probes[$key] = 'ok'; $fresh += $key }
    }
    if ($fresh) {
        Use-Lock 'PeerPartyTestDlls' {
            New-Item -ItemType Directory -Force $copies | Out-Null
            Add-Content -LiteralPath $probeCache -Value $fresh -Encoding UTF8
        }
    }
    $runnable = [System.Collections.Generic.List[object]]::new()
    foreach ($job in @($prepared)) {
        if (-not $job.Probe) { $runnable.Add($job); continue }
        $state = $probes[(Probe-Key $job)]
        if ($state -eq 'ok') { $runnable.Add($job) } else { Note $job.Name 'skipped' $state }
    }

    $pool = [System.Collections.Generic.List[object]]::new()
    $steam = [System.Collections.Generic.List[object]]::new()
    foreach ($job in $runnable) {
        if ($job.Steam) { $steam.Add($job) } else { $pool.Add($job) }
    }
    Invoke-Pool $pool $Jobs
    # The one Steam client: one test at a time, this run's and any other's.
    if ($steam.Count) { Use-Lock 'PeerPartySteamTests' { Invoke-Pool $steam 1 } }
}
finally {
    foreach ($p in @($script:LiveProcs)) {
        try {
            if ($p -and -not $p.HasExited) { & taskkill.exe /PID $p.Id /T /F 2>&1 | Out-Null }
        }
        catch {}
    }
    foreach ($f in @($script:PortLocks)) { try { $f.Dispose() } catch {} }
    Restore-Hooks
    Pop-Location
}

''
$results | Format-Table -AutoSize | Out-String -Width 200
'total {0:n0}s, run {1}' -f ((Get-Date) - $suiteStart).TotalSeconds, $RunId
if ($results | Where-Object Result -eq 'FAIL') { exit 1 }
