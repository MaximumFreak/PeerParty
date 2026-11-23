# Runs a two-copy test under peerparty: run_pair.ps1 <test.py> [-Env NAME=VALUE,...]
# [-RoleEnv A:NAME=VALUE,...] [-Roles A[,B]] [-PeerParty exe] [-Game key] [-Work dir]
# Each copy is `peerparty --multi-instance python <test.py> A|B`. Prints both outputs
# and the peerparty log lines that matter. Work files stay in $env:PPTEST_DIR. -RoleEnv
# values are set for that copy only. -Game is PEERPARTY_GAME, so parallel pairs don't
# meet. Both copies are started together; the test waits until the other shows up.
param(
    [string]$Test,
    [string[]]$Env = @(),
    [string[]]$RoleEnv = @(),
    [string[]]$Roles = @('A', 'B'),
    [string]$PeerParty = '',
    [string]$Game = '',
    [string]$Work = ''
)
$ErrorActionPreference = 'Stop'
$pp = if ($PeerParty) { $PeerParty } elseif ($env:CARGO_TARGET_DIR) { Join-Path $env:CARGO_TARGET_DIR 'testexe\peerparty.exe' } else { "$env:TEMP\cargo-target\peerparty\testexe\peerparty.exe" }
$dir = if ($Work) { $Work } else { Join-Path $env:TEMP ("ppwork\run-" + [IO.Path]::GetFileNameWithoutExtension($Test)) }
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
New-Item -ItemType Directory $dir | Out-Null
$env:PPTEST_DIR = $dir
# Loopback only: no Windows Firewall prompt for python.exe.
$env:PEERPARTY_LOOPBACK = '1'
# Never public sessions, whatever the launcher's saved settings say (a test's -Env can
# still turn them on).
$env:PEERPARTY_PUBLIC = '0'
if ($Game) { $env:PEERPARTY_GAME = $Game }
foreach ($kv in $Env) { $k, $v = $kv -split '=', 2; Set-Item "env:$k" $v }
# PPTEST_PYTHON: another interpreter, such as a 32-bit one for a 32-bit DLL.
$py = if ($env:PPTEST_PYTHON) { $env:PPTEST_PYTHON } else { (Get-Command python).Source }
# pythonw beside it: peerparty.exe has no console here (its stderr is redirected), so a
# console python would open a window of its own on screen. The tests write their own
# logs; nothing reads python's output.
$pyw = Join-Path (Split-Path $py) 'pythonw.exe'
if (Test-Path -LiteralPath $pyw) { $py = $pyw }
$procs = foreach ($role in $Roles) {
    $mine = foreach ($re in $RoleEnv) { $r, $kv = $re -split ':', 2; if ($r -eq $role) { $kv } }
    foreach ($kv in $mine) { $k, $v = $kv -split '=', 2; Set-Item "env:$k" $v }
    Start-Process -FilePath $pp -ArgumentList @('--multi-instance', '--log-dir', "$dir\logs", $py, "`"$Test`"", $role) `
        -RedirectStandardError "$dir\$role.err" -PassThru -NoNewWindow -WorkingDirectory (Split-Path $Test)
    foreach ($kv in $mine) { $k, $v = $kv -split '=', 2; Remove-Item "env:$k" -ErrorAction SilentlyContinue }
}
# Everything this pair's peerparty processes started, and what those started in turn (a
# starter's game), now: once a copy exits its children's parent is gone, and they could
# no longer be told from another pair's.
# Each is kept with its start time: a process id seen once may belong to another program
# by the end.
function Get-Tree([int[]]$top) {
    $all = @(Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId, CreationDate)
    $mine = @{}
    foreach ($id in $top) { $mine[$id] = $null }
    $grew = $true
    while ($grew) {
        $grew = $false
        foreach ($p in $all) {
            if ($mine.ContainsKey([int]$p.ParentProcessId) -and -not $mine.ContainsKey([int]$p.ProcessId)) {
                $mine[[int]$p.ProcessId] = $p.CreationDate
                $grew = $true
            }
        }
    }
    foreach ($id in $top) { $mine.Remove($id) }
    $mine
}
$ours = @($procs | ForEach-Object { $_.Id })
$tree = @{}
$deadline = (Get-Date).AddSeconds(120)
while ((Get-Date) -lt $deadline -and @($procs | Where-Object { -not $_.HasExited }).Count) {
    $now = Get-Tree $ours
    foreach ($id in @($now.Keys)) { $tree[$id] = $now[$id] }
    Start-Sleep -Seconds 2
}
# A copy that hung (a build that stops on a dialog, say) mustn't outlive its test, nor
# anything it started.
$procs | Where-Object { -not $_.HasExited } | ForEach-Object { & taskkill.exe /PID $_.Id /T /F 2>&1 | Out-Null }
if ($tree.Count) {
    foreach ($p in @(Get-CimInstance Win32_Process -Property ProcessId, CreationDate)) {
        if ($tree.ContainsKey([int]$p.ProcessId) -and $tree[[int]$p.ProcessId] -eq $p.CreationDate) {
            Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
}
foreach ($role in $Roles) { "---- $role (exit $((Get-Content "$dir\$role.result" -ErrorAction SilentlyContinue)))"; Get-Content "$dir\$role.log" -ErrorAction SilentlyContinue }
