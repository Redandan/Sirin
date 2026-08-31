#requires -Version 5.1
<#
Sirin watchdog for long-lived local daemon use.

Behavior:
  1. Sirin not running           -> launch
  2. Sirin running, binary newer -> kill + relaunch
  3. Sirin running, binary same  -> keep it

Examples:
  .\scripts\watch-sirin.ps1
  .\scripts\watch-sirin.ps1 -Headless:$false
  .\scripts\watch-sirin.ps1 -Once

For normal always-on ops, Task Scheduler should run sirin.exe directly. Use
this script only for development watchdog mode:
  .\scripts\install-sirin-daemon-task.ps1 -Action Install -RunNow -UseWatchdog
#>

[CmdletBinding()]
param(
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$Binary = '',
    [string]$LogDir = '',
    [int]$PollSeconds = 30,
    [switch]$Once,
    [switch]$Headless = $true,
    [switch]$IosDriverAutostart
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($Binary)) {
    $Binary = Join-Path $Repo 'target\release\sirin.exe'
}
if ([string]::IsNullOrWhiteSpace($LogDir)) {
    $local = $env:LOCALAPPDATA
    if ([string]::IsNullOrWhiteSpace($local)) {
        $local = Join-Path $Repo '.sirin'
    }
    $LogDir = Join-Path $local 'Sirin\logs'
}

$LogOut = Join-Path $LogDir 'sirin-daemon.out.log'
$LogErr = Join-Path $LogDir 'sirin-daemon.err.log'

function Write-Tag([string]$Message) {
    Write-Host "[sirin-watchdog $(Get-Date -Format HH:mm:ss)] $Message"
}

function Get-SirinProc {
    Get-Process sirin -ErrorAction SilentlyContinue |
        Where-Object {
            try {
                [string]::Equals($_.Path, $Binary, [System.StringComparison]::OrdinalIgnoreCase)
            }
            catch {
                $false
            }
        }
}

function Start-Sirin {
    if (-not (Test-Path -LiteralPath $Binary)) {
        Write-Tag "binary missing: $Binary"
        return $null
    }
    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
    $args = @()
    if ($Headless) {
        $args += '--headless'
    }
    if ($IosDriverAutostart) {
        $args += '--ios-driver-autostart'
    }
    Write-Tag "launching $Binary $($args -join ' ')"
    $proc = Start-Process -FilePath $Binary `
        -ArgumentList $args `
        -WorkingDirectory $Repo `
        -RedirectStandardOutput $LogOut `
        -RedirectStandardError  $LogErr `
        -WindowStyle Hidden `
        -PassThru
    Start-Sleep -Seconds 2
    return $proc
}

function Stop-Sirin {
    Get-SirinProc | ForEach-Object {
        Write-Tag "stopping pid=$($_.Id)"
        Stop-Process -Id $_.Id -Force
    }
    Start-Sleep -Seconds 1
}

function Invoke-WatchdogTick {
    $proc = Get-SirinProc

    if (-not $proc) {
        Write-Tag "not running"
        Start-Sirin | Out-Null
        return
    }

    if (Test-Path -LiteralPath $Binary) {
        $binaryMtime = (Get-Item -LiteralPath $Binary).LastWriteTime
        $newerThanAny = @($proc | Where-Object { $binaryMtime -gt $_.StartTime }).Count -gt 0
        if ($newerThanAny) {
            Write-Tag "binary newer than running process; relaunching"
            Stop-Sirin
            Start-Sirin | Out-Null
            return
        }
    }

    Write-Tag "running pid=$(@($proc).Id -join ',')"
}

Write-Tag "watching $Binary"

while ($true) {
    try {
        Invoke-WatchdogTick
    }
    catch {
        Write-Tag "loop error: $($_.Exception.Message)"
    }

    if ($Once) {
        break
    }
    Start-Sleep -Seconds ([Math]::Max(5, $PollSeconds))
}
