#requires -Version 5.1
<#
Install, inspect, or remove Sirin's per-user Windows Task Scheduler daemon.

By default the task starts sirin.exe directly at user logon. That keeps the
runtime model to one Sirin process; Windows Task Scheduler is only the logon
launcher, not a restart supervisor. Use -UseWatchdog only for development
sessions that need auto-relaunch after rebuilding target\release\sirin.exe.

Examples:
  .\scripts\install-sirin-daemon-task.ps1 -Action Install
  .\scripts\install-sirin-daemon-task.ps1 -Action Status
  .\scripts\install-sirin-daemon-task.ps1 -Action Remove
#>

[CmdletBinding()]
param(
    [ValidateSet('Install', 'Remove', 'Status', 'Plan')]
    [string]$Action = 'Install',
    [string]$TaskName = 'Sirin Local Ops Daemon',
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$Binary = '',
    [switch]$RunNow,
    [switch]$UseWatchdog,
    [bool]$EnableIosDriverSupervisor = $true,
    [ValidateRange(1, 60)]
    [int]$HeartbeatMinutes = 5
)

$ErrorActionPreference = 'Stop'

$watcher = Join-Path $Repo 'scripts\watch-sirin.ps1'
if ([string]::IsNullOrWhiteSpace($Binary)) {
    $devBinary = Join-Path $Repo 'target\release\sirin.exe'
    $installedBinary = Join-Path $Repo 'sirin.exe'
    $Binary = if (Test-Path -LiteralPath $devBinary -PathType Leaf) { $devBinary } else { $installedBinary }
}
$binary = [System.IO.Path]::GetFullPath($Binary)
$binaryWorkingDirectory = Split-Path -Parent $binary

function Write-TaskLine([string]$Message) {
    Write-Host "[sirin-daemon-task] $Message"
}

function Get-SirinTask {
    Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
}

function Show-Status {
    $task = Get-SirinTask
    if (-not $task) {
        Write-TaskLine "not installed: $TaskName"
        return
    }
    $info = Get-ScheduledTaskInfo -TaskName $TaskName
    Write-TaskLine "installed: $TaskName"
    Write-TaskLine "state: $($task.State)"
    Write-TaskLine "last run: $($info.LastRunTime)"
    $resultText = switch ($info.LastTaskResult) {
        0 { '0 (success)' }
        267009 { '267009 / 0x41301 (running)' }
        default { "$($info.LastTaskResult)" }
    }
    Write-TaskLine "last result: $resultText"
    Write-TaskLine "next run: $($info.NextRunTime)"
    Write-TaskLine "ui: http://127.0.0.1:7700/ui/"
    Write-TaskLine "logs: $([System.IO.Path]::Combine($env:LOCALAPPDATA, 'Sirin\logs'))"
    $action = $task.Actions | Select-Object -First 1
    if ($action) {
        Write-TaskLine "action: $($action.Execute) $($action.Arguments)"
        $iosSupervisorEnabled =
            $action.Arguments -match '(?:^|\s)--ios-driver-autostart(?:\s|$)' -or
            $action.Arguments -match '(?:^|\s)-IosDriverAutostart(?:\s|$)'
        Write-TaskLine "ios driver supervisor: $iosSupervisorEnabled"
    }
    $proc = Get-Process sirin -ErrorAction SilentlyContinue | Where-Object {
        try {
            [string]::Equals($_.Path, $binary, [System.StringComparison]::OrdinalIgnoreCase)
        }
        catch {
            $false
        }
    }
    if ($proc) {
        Write-TaskLine "sirin running pid=$(@($proc).Id -join ',')"
    }
    else {
        Write-TaskLine "sirin process not running from $binary"
    }
}

if ($Action -eq 'Status') {
    Show-Status
    return
}

if ($Action -eq 'Remove') {
    if (Get-SirinTask) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-TaskLine "removed: $TaskName"
    }
    else {
        Write-TaskLine "not installed: $TaskName"
    }
    return
}

if (-not (Test-Path -LiteralPath $binary)) {
    Write-TaskLine "warning: release binary not found yet: $binary"
    Write-TaskLine "build it with: cargo build --release"
}

if ($UseWatchdog) {
    if (-not (Test-Path -LiteralPath $watcher)) {
        throw "watcher script not found: $watcher"
    }
    $arguments = @(
        '-NoProfile',
        '-ExecutionPolicy', 'Bypass',
        '-WindowStyle', 'Hidden',
        '-File', "`"$watcher`"",
        '-Repo', "`"$Repo`""
    )
    if ($EnableIosDriverSupervisor) {
        $arguments += '-IosDriverAutostart'
    }
    $arguments = $arguments -join ' '
    $actionDef = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $arguments -WorkingDirectory $Repo
}
else {
    $daemonArguments = @('--headless')
    if ($EnableIosDriverSupervisor) {
        $daemonArguments += '--ios-driver-autostart'
    }
    $actionDef = New-ScheduledTaskAction -Execute $binary -Argument ($daemonArguments -join ' ') -WorkingDirectory $binaryWorkingDirectory
}
$logonTrigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$heartbeatTrigger = New-ScheduledTaskTrigger `
    -Once `
    -At (Get-Date).AddMinutes(1) `
    -RepetitionInterval (New-TimeSpan -Minutes $HeartbeatMinutes)
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -RestartCount 999 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -MultipleInstances IgnoreNew
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
$task = New-ScheduledTask -Action $actionDef -Trigger @($logonTrigger, $heartbeatTrigger) -Settings $settings -Principal $principal `
    -Description 'Keep Sirin running as the local operations and physical-device control daemon after user logon.'

if ($Action -eq 'Plan') {
    [pscustomobject]@{
        status = 'PLANNED'
        owner = 'sirin'
        task = $TaskName
        action = [pscustomobject]@{
            execute = [string]$actionDef.Execute
            arguments = [string]$actionDef.Arguments
            working_directory = [string]$actionDef.WorkingDirectory
        }
        principal = [pscustomobject]@{
            user_id = [string]$principal.UserId
            logon_type = [string]$principal.LogonType
            run_level = [string]$principal.RunLevel
        }
        triggers = @($task.Triggers | ForEach-Object {
            [pscustomobject]@{
                type = $_.CimClass.CimClassName
                repetition_interval = [string]$_.Repetition.Interval
            }
        })
        settings = [pscustomobject]@{
            execution_time_limit = [string]$settings.ExecutionTimeLimit
            restart_count = [int]$settings.RestartCount
            restart_interval = [string]$settings.RestartInterval
            start_when_available = [bool]$settings.StartWhenAvailable
            multiple_instances = [string]$settings.MultipleInstances
        }
    } | ConvertTo-Json -Depth 6
    return
}

Register-ScheduledTask -TaskName $TaskName -InputObject $task -Force | Out-Null
Write-TaskLine "installed: $TaskName"

if ($RunNow) {
    Start-ScheduledTask -TaskName $TaskName
    Write-TaskLine "started task"
}

Show-Status
