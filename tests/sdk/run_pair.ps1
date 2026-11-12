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
$pp = if ($PeerParty) { $PeerParty } else { "$env:TEMP\cargo-target\peerparty\testexe\peerparty.exe" }
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
$procs | Wait-Process -Timeout 120 -ErrorAction SilentlyContinue
# A copy that hung (a build that stops on a dialog, say) mustn't outlive its test.
# Only the copies this pair's peerparty processes started: other pairs may run the same
# test at the same time (-AllBuilds).
$ours = @($procs | ForEach-Object { $_.Id })
Get-CimInstance Win32_Process -Filter "Name = 'python.exe' OR Name = 'pythonw.exe'" |
    Where-Object { $ours -contains $_.ParentProcessId -and $_.CommandLine -like "*$Test*" } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
$procs | Where-Object { -not $_.HasExited } | Stop-Process -Force -ErrorAction SilentlyContinue
foreach ($role in $Roles) { "---- $role (exit $((Get-Content "$dir\$role.result" -ErrorAction SilentlyContinue)))"; Get-Content "$dir\$role.log" -ErrorAction SilentlyContinue }
