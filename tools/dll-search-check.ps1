# Checks that peerparty.exe takes no DLL from its own folder.
#
#   powershell -ExecutionPolicy Bypass -File tools\dll-search-check.ps1 -Exe <peerparty.exe> [-NoWindow]
#
# Why: Windows looks beside an exe for most DLLs before it looks in System32, and
# peerparty.exe is run from wherever it was downloaded to. A file named like a system DLL
# in that folder would be loaded into it, and into the copy "Fix firewall" starts as
# administrator. launcher\build.rs and `only_system_dlls` in launcher\src\main.rs keep
# the folder out of the search; this shows whether they still do, after a dependency
# starts naming a new DLL, say.
#
# How: it runs the exe once to see every DLL it loads, then runs a copy of it from a
# scratch folder holding a copy of each of those DLLs (genuine ones, straight from where
# Windows loaded them), and lists the modules of that second run. Any module loaded from
# the scratch folder is a DLL the exe would take from beside itself. It does this for
# `--pads` (no window, a few seconds) and for the launcher window (which shows for a few
# seconds; -NoWindow skips it). Nothing is changed outside %TEMP%.
#
# Exit code 0: nothing taken. 1: something was, and it is named.

param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$NoWindow
)

$ErrorActionPreference = "Stop"
$Exe = (Resolve-Path -LiteralPath $Exe).Path
$taken = @()

function Start-Copy([string]$path, [string]$mode, [string]$scratch) {
    $run = @{ FilePath = $path; PassThru = $true }
    if ($mode -ne "window") {
        $run.ArgumentList = $mode
        $run.WindowStyle = "Hidden"
        $run.RedirectStandardOutput = Join-Path $scratch "out.txt"
        $run.RedirectStandardError = Join-Path $scratch "err.txt"
    }
    Start-Process @run
}

function Get-Loaded([Diagnostics.Process]$p) {
    Start-Sleep -Seconds 5
    $p.Refresh()
    if ($p.HasExited) { throw "peerparty.exe exited before its DLLs could be listed (exit code $($p.ExitCode))" }
    $mods = @((Get-Process -Id $p.Id).Modules | ForEach-Object FileName)
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 700
    $mods
}

$modes = @("--pads")
if (-not $NoWindow) { $modes += "window" }
foreach ($mode in $modes) {
    $scratch = Join-Path $env:TEMP ("peerparty-dll-check-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
    New-Item -ItemType Directory -Force $scratch | Out-Null
    try {
        $first = Get-Loaded (Start-Copy $Exe $mode $scratch)
        $copy = Join-Path $scratch "peerparty.exe"
        Copy-Item -LiteralPath $Exe $copy
        $planted = 0
        foreach ($m in $first) {
            if ($m -like "*.exe") { continue }
            try { Copy-Item -LiteralPath $m (Join-Path $scratch (Split-Path $m -Leaf)); $planted++ } catch {}
        }
        $second = Get-Loaded (Start-Copy $copy $mode $scratch)
        $here = @($second | Where-Object { $_ -like "$scratch\*" -and $_ -ne $copy } | ForEach-Object { Split-Path $_ -Leaf })
        Write-Host ("{0}: {1} DLLs beside the exe, {2} taken from there" -f $mode, $planted, $here.Count)
        $taken += $here | ForEach-Object { "$_ ($mode)" }
    } finally {
        Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
    }
}

if ($taken.Count -gt 0) {
    Write-Host "peerparty.exe loads these from its own folder:" -ForegroundColor Red
    $taken | ForEach-Object { Write-Host "  $_" }
    Write-Host "Add the DLL that names them to the delay-load list in launcher\build.rs (see the comment there)."
    exit 1
}
Write-Host "peerparty.exe takes no DLL from its own folder."
exit 0
