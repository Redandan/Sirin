#requires -Version 5.1
<#
Sirin watchdog. Run once and leave it alive.

Behavior:
  1. Sirin not running              → launch
  2. Sirin running, binary newer    → kill + relaunch (cargo build picked up)
  3. Sirin running, binary same     → nothing (poll every 5s)

Usage (PowerShell):
  .\scripts\watch-sirin.ps1

Or pin to Windows Task Scheduler "登入時觸發" with action:
  Program:    powershell.exe
  Arguments:  -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "C:\Users\Redan\IdeaProjects\Sirin\scripts\watch-sirin.ps1"

Stops only when you Ctrl+C or kill the powershell host process.
#>

$ErrorActionPreference = 'Stop'
$repo    = "C:\Users\Redan\IdeaProjects\Sirin"
$binary  = Join-Path $repo "target\release\sirin.exe"
$logOut  = Join-Path $repo "sirin.log"
$logErr  = Join-Path $repo "sirin.err.log"
$pollSec = 5

function Write-Tag([string]$msg) {
    Write-Host "[watchdog $(Get-Date -Format HH:mm:ss)] $msg"
}

function Get-SirinProc {
    Get-Process sirin -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $binary }
}

function Start-Sirin {
    if (-not (Test-Path $binary)) {
        Write-Tag "binary missing: $binary — skip launch"
        return $null
    }
    Write-Tag "launching $binary"
    $p = Start-Process -FilePath $binary `
        -RedirectStandardOutput $logOut `
        -RedirectStandardError  $logErr `
        -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 2
    return $p
}

function Stop-Sirin {
    Get-SirinProc | ForEach-Object {
        Write-Tag "stopping pid=$($_.Id)"
        Stop-Process -Id $_.Id -Force
    }
    Start-Sleep -Seconds 1
}

Write-Tag "watching $binary"

while ($true) {
    try {
        $proc = Get-SirinProc

        if (-not $proc) {
            Write-Tag "not running"
            Start-Sirin | Out-Null
        }
        elseif (Test-Path $binary) {
            $binaryMtime = (Get-Item $binary).LastWriteTime
            if ($binaryMtime -gt $proc.StartTime) {
                Write-Tag "binary newer ($binaryMtime > pid $($proc.Id) startup $($proc.StartTime)) — relaunching"
                Stop-Sirin
                Start-Sirin | Out-Null
            }
        }
    }
    catch {
        Write-Tag "loop err: $($_.Exception.Message)"
    }

    Start-Sleep -Seconds $pollSec
}
