#requires -Version 5.1
<#
Install or inspect Sirin's private, on-demand iOS Driver task.

The task launches the provider source maintained inside Sirin with a generated
runtime under %LOCALAPPDATA%\Sirin. It has no logon trigger: the Sirin daemon
owns start decisions. The default is acceptance-only and the provider has no
signing, trust, unlock, credential, order, or payment endpoint.
#>

[CmdletBinding()]
param(
    [ValidateSet('Install', 'Status', 'Remove', 'Plan')]
    [string]$Action = 'Status',
    [string]$TaskName = 'Sirin iOS Driver',
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$ProviderRoot = (Join-Path $Repo 'integrations\ios-driver'),
    [string]$RuntimeRoot = (Join-Path $env:LOCALAPPDATA 'Sirin\ios-driver')
)

$ErrorActionPreference = 'Stop'

$resolvedRoot = if (Test-Path -LiteralPath $ProviderRoot -PathType Container) {
    (Resolve-Path -LiteralPath $ProviderRoot).Path
} else {
    $ProviderRoot
}
$resolvedRuntime = [System.IO.Path]::GetFullPath($RuntimeRoot).TrimEnd('\')
$allowedRuntimeRoot = [System.IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'Sirin')).TrimEnd('\')
if (-not $resolvedRuntime.StartsWith(
    $allowedRuntimeRoot + '\',
    [System.StringComparison]::OrdinalIgnoreCase
)) {
    throw "RuntimeRoot must remain below $allowedRuntimeRoot"
}
$pythonw = Join-Path $resolvedRuntime 'venv\Scripts\pythonw.exe'
$hostScript = Join-Path $resolvedRoot 'scripts\unattended_host.py'
$goIos = Join-Path $resolvedRuntime 'bin\ios.exe'
$endpoint = 'http://127.0.0.1:8770/api/status'

function Get-ProviderHealth {
    try {
        $status = Invoke-RestMethod -Uri $endpoint -TimeoutSec 3
        [pscustomobject]@{
            reachable = $true
            provider = $status.provider
            device_detected = $status.device_detected
            acceptance_only = $status.acceptance_only
            attach_mode = $status.attach_mode
            input = $status.input
            link = $status.link
            lan_exposed = $status.lan_exposed
            lan_protection = $status.lan_protection
        }
    }
    catch {
        [pscustomobject]@{
            reachable = $false
            provider = $null
            device_detected = $null
            acceptance_only = $null
            attach_mode = $null
            input = $null
            link = $null
            lan_exposed = $null
            lan_protection = $null
        }
    }
}

function Get-DriverStatus {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $actionDef = if ($task) { $task.Actions | Select-Object -First 1 } else { $null }
    [pscustomobject]@{
        owner = 'sirin'
        mode = 'sirin_private_provider'
        task = $TaskName
        installed = $null -ne $task
        task_state = if ($task) { [string]$task.State } else { 'MISSING' }
        task_has_logon_trigger = if ($task) {
            @($task.Triggers | Where-Object { $null -ne $_ }).Count -gt 0
        } else { $false }
        action_matches = if ($actionDef) {
            [string]::Equals($actionDef.Execute, $pythonw, [System.StringComparison]::OrdinalIgnoreCase) -and
            $actionDef.Arguments -match [regex]::Escape($hostScript) -and
            $actionDef.Arguments -match '--runtime-root' -and
            $actionDef.Arguments -match [regex]::Escape($resolvedRuntime) -and
            $actionDef.Arguments -match '(?:^|\s)--attach-mode\s+passive(?:\s|$)' -and
            $actionDef.Arguments -notmatch '--supervised-control'
        } else { $false }
        provider_root = $resolvedRoot
        runtime_root = $resolvedRuntime
        provider = Get-ProviderHealth
        repair_scope = 'start-only; no stop, install, signing, trust, unlock, or firewall mutation'
    }
}

if ($Action -eq 'Status') {
    Get-DriverStatus | ConvertTo-Json -Depth 5
    return
}

if ($Action -eq 'Remove') {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    }
    Get-DriverStatus | ConvertTo-Json -Depth 5
    return
}

if ($Action -ne 'Plan') {
    if (-not (Test-Path -LiteralPath $pythonw -PathType Leaf)) {
        throw "private iOS Driver Python runtime is missing: $pythonw"
    }
    if (-not (Test-Path -LiteralPath $hostScript -PathType Leaf)) {
        throw "private iOS Driver host is missing: $hostScript"
    }
    if (-not (Test-Path -LiteralPath $goIos -PathType Leaf)) {
        throw "private iOS Driver go-ios runtime is missing: $goIos"
    }
}

$arguments = @(
    "`"$hostScript`"",
    '--runtime-root', "`"$resolvedRuntime`"",
    '--attach-mode', 'passive',
    '--health-poll-seconds', '60'
) -join ' '
$actionDef = New-ScheduledTaskAction -Execute $pythonw -Argument $arguments -WorkingDirectory $resolvedRoot
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -RestartCount 999 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -MultipleInstances IgnoreNew
$principal = New-ScheduledTaskPrincipal `
    -UserId "$env:USERDOMAIN\$env:USERNAME" `
    -LogonType Interactive `
    -RunLevel Limited
$task = New-ScheduledTask `
    -Action $actionDef `
    -Settings $settings `
    -Principal $principal `
    -Description 'Sirin-owned loopback iOS Driver; acceptance-only by default and started only by Sirin.'

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
        triggers = @($task.Triggers | Where-Object { $null -ne $_ } | ForEach-Object { $_.CimClass.CimClassName })
        settings = [pscustomobject]@{
            execution_time_limit = [string]$settings.ExecutionTimeLimit
            restart_count = [int]$settings.RestartCount
            restart_interval = [string]$settings.RestartInterval
            start_when_available = [bool]$settings.StartWhenAvailable
            multiple_instances = [string]$settings.MultipleInstances
        }
        safety = [pscustomobject]@{
            has_trigger = @($task.Triggers | Where-Object { $null -ne $_ }).Count -gt 0
            acceptance_only_default = $true
            signing_supported = $false
            trust_or_unlock_supported = $false
        }
    } | ConvertTo-Json -Depth 6
    return
}

Register-ScheduledTask -TaskName $TaskName -InputObject $task -Force | Out-Null
Get-DriverStatus | ConvertTo-Json -Depth 5
