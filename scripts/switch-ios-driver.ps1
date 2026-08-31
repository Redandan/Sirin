#requires -Version 5.1
<#
Switch the loopback iPhone provider from legacy SideTap tasks to Sirin's
private iOS Driver task. Every affected scheduled task is exported before any
mutation. Deploy verifies the provider and a read-only MCP screen capture; a
failed switch restores the exact task definitions and prior running state.
#>

[CmdletBinding()]
param(
    [ValidateSet('Status', 'Deploy', 'Rollback')]
    [string]$Action = 'Status',
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$BackupPath = '',
    [int]$TimeoutSeconds = 90
)

$ErrorActionPreference = 'Stop'

$legacyTask = 'SideTap-Unattended'
$legacyWatchdogTask = 'SideTap-Unattended-Watchdog'
$driverTask = 'Sirin iOS Driver'
$affectedTasks = @($legacyTask, $legacyWatchdogTask, $driverTask)
$driverInstaller = Join-Path $Repo 'scripts\install-ios-driver-task.ps1'
$providerEndpoint = 'http://127.0.0.1:8770'
$mcpEndpoint = 'http://127.0.0.1:7700/mcp'
$backupRoot = Join-Path $env:LOCALAPPDATA 'Sirin\ios-driver\switch-backups'

function Invoke-Mcp([int]$Id, [string]$Method, $Params) {
    $request = @{ jsonrpc = '2.0'; id = $Id; method = $Method }
    if ($null -ne $Params) {
        $request.params = $Params
    }
    Invoke-RestMethod `
        -Uri $mcpEndpoint `
        -Method Post `
        -ContentType 'application/json' `
        -Body ($request | ConvertTo-Json -Depth 12 -Compress) `
        -TimeoutSec 20
}

function Invoke-IosTool([int]$Id, [string]$Name, $Arguments) {
    $response = Invoke-Mcp $Id 'tools/call' @{
        name = $Name
        arguments = $Arguments
    }
    if ([bool]$response.result.isError) {
        throw "$Name returned an MCP error: $($response.result.content[0].text)"
    }
    $response.result.content[0].text | ConvertFrom-Json
}

function Get-TaskRows {
    foreach ($name in $affectedTasks) {
        $task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
        $actionDef = if ($task) { $task.Actions | Select-Object -First 1 } else { $null }
        [pscustomobject]@{
            name = $name
            installed = $null -ne $task
            state = if ($task) { [string]$task.State } else { 'MISSING' }
            enabled = if ($task) { [bool]$task.Settings.Enabled } else { $false }
            execute = if ($actionDef) { [string]$actionDef.Execute } else { $null }
            arguments = if ($actionDef) { [string]$actionDef.Arguments } else { $null }
            working_directory = if ($actionDef) { [string]$actionDef.WorkingDirectory } else { $null }
            trigger_count = if ($task) { @($task.Triggers).Count } else { 0 }
        }
    }
}

function Get-ProviderStatus {
    try {
        Invoke-RestMethod -Uri "$providerEndpoint/api/status" -TimeoutSec 4
    }
    catch {
        $null
    }
}

function Get-SwitchStatus {
    $listener = Get-NetTCPConnection `
        -LocalAddress 127.0.0.1 `
        -LocalPort 8770 `
        -State Listen `
        -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $process = if ($listener) {
        Get-Process -Id $listener.OwningProcess -ErrorAction SilentlyContinue
    } else { $null }
    [pscustomobject]@{
        owner = 'sirin'
        endpoint = $providerEndpoint
        tasks = @(Get-TaskRows)
        listener = if ($listener) {
            [pscustomobject]@{
                pid = $listener.OwningProcess
                path = if ($process) { $process.Path } else { $null }
            }
        } else { $null }
        provider = Get-ProviderStatus
    }
}

function Save-TaskBackup {
    foreach ($name in $affectedTasks) {
        if (-not (Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue)) {
            throw "cannot switch safely because scheduled task is missing: $name"
        }
    }
    New-Item -ItemType Directory -Force -Path $backupRoot | Out-Null
    $timestamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $directory = Join-Path $backupRoot $timestamp
    New-Item -ItemType Directory -Path $directory | Out-Null
    $taskRows = @(Get-TaskRows)
    $manifestTasks = @()
    for ($index = 0; $index -lt $affectedTasks.Count; $index++) {
        $name = $affectedTasks[$index]
        $fileName = "task-$index.xml"
        $xml = Export-ScheduledTask -TaskName $name
        $xml | Set-Content -LiteralPath (Join-Path $directory $fileName) -Encoding UTF8
        $row = $taskRows | Where-Object { $_.name -eq $name } | Select-Object -First 1
        $manifestTasks += [pscustomobject]@{
            name = $name
            file = $fileName
            was_running = $row.state -eq 'Running'
            was_enabled = [bool]$row.enabled
        }
    }
    $manifest = [pscustomobject]@{
        created_at = (Get-Date).ToUniversalTime().ToString('o')
        provider_before = Get-ProviderStatus
        tasks = $manifestTasks
    }
    $manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $directory 'manifest.json') -Encoding UTF8
    $directory
}

function Resolve-SafeBackup([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw '-BackupPath is required; the latest backup is never inferred'
    }
    $resolvedRoot = (Resolve-Path -LiteralPath $backupRoot).Path.TrimEnd('\')
    $resolved = (Resolve-Path -LiteralPath $Path).Path.TrimEnd('\')
    if (-not $resolved.StartsWith(
        $resolvedRoot + '\',
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        throw "backup must remain below $resolvedRoot"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $resolved 'manifest.json') -PathType Leaf)) {
        throw "backup manifest is missing: $resolved"
    }
    $resolved
}

function Stop-And-Disable([string]$Name) {
    $task = Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue
    if (-not $task) { return }
    Stop-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue
    Disable-ScheduledTask -TaskName $Name | Out-Null
}

function Wait-PortClosed([int]$Seconds) {
    $deadline = (Get-Date).AddSeconds([Math]::Max(5, $Seconds))
    do {
        $listener = Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort 8770 -State Listen -ErrorAction SilentlyContinue
        if (-not $listener) { return }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw 'legacy provider did not release 127.0.0.1:8770'
}

function Wait-SirinProvider([int]$Seconds) {
    $deadline = (Get-Date).AddSeconds([Math]::Max(10, $Seconds))
    do {
        $status = Get-ProviderStatus
        if ($status -and
            $status.provider -eq 'sirin-ios-driver' -and
            [bool]$status.acceptance_only -and
            -not [bool]$status.lan_exposed -and
            [bool]$status.device_detected -and
            $status.link -eq 'up') {
            try {
                $phone = Invoke-RestMethod -Uri "$providerEndpoint/api/phone" -TimeoutSec 4
                if ([bool]$phone.info_readable) {
                    return [pscustomobject]@{ status = $status; phone = $phone }
                }
            }
            catch {}
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    throw 'Sirin iOS Driver did not become healthy with readable device information'
}

function Restore-TaskBackup([string]$Path) {
    $resolved = Resolve-SafeBackup $Path
    $manifest = Get-Content -LiteralPath (Join-Path $resolved 'manifest.json') -Raw | ConvertFrom-Json
    foreach ($name in $affectedTasks) {
        Stop-And-Disable $name
    }
    foreach ($entry in @($manifest.tasks)) {
        $xmlPath = Join-Path $resolved ([string]$entry.file)
        $xml = Get-Content -LiteralPath $xmlPath -Raw
        Register-ScheduledTask -TaskName ([string]$entry.name) -Xml $xml -Force | Out-Null
        if ([bool]$entry.was_enabled) {
            Enable-ScheduledTask -TaskName ([string]$entry.name) | Out-Null
        }
        else {
            Disable-ScheduledTask -TaskName ([string]$entry.name) | Out-Null
        }
    }
    foreach ($entry in @($manifest.tasks | Where-Object { [bool]$_.was_running })) {
        Start-ScheduledTask -TaskName ([string]$entry.name)
    }
    foreach ($entry in @($manifest.tasks | Where-Object { -not [bool]$_.was_running })) {
        Stop-ScheduledTask -TaskName ([string]$entry.name) -ErrorAction SilentlyContinue
    }
    $deadline = (Get-Date).AddSeconds([Math]::Max(10, $TimeoutSeconds))
    do {
        if (Get-ProviderStatus) { return Get-SwitchStatus }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)
    throw 'task definitions were restored but the prior provider did not recover'
}

if ($Action -eq 'Status') {
    Get-SwitchStatus | ConvertTo-Json -Depth 12
    return
}

if ($Action -eq 'Rollback') {
    Restore-TaskBackup $BackupPath | ConvertTo-Json -Depth 12
    return
}

if (-not (Test-Path -LiteralPath $driverInstaller -PathType Leaf)) {
    throw "Sirin iOS Driver installer is missing: $driverInstaller"
}
$before = Get-ProviderStatus
if (-not $before -or -not [bool]$before.acceptance_only -or [bool]$before.lan_exposed) {
    throw 'refusing switch because the current provider is not healthy, acceptance-only, and loopback-only'
}

$backup = Save-TaskBackup
try {
    Stop-And-Disable $legacyWatchdogTask
    Stop-And-Disable $legacyTask
    Stop-And-Disable $driverTask
    Wait-PortClosed 20

    & $driverInstaller -Action Install -Repo $Repo | Out-Null
    $providerProof = Wait-SirinProvider $TimeoutSeconds
    $mcpStatus = Invoke-IosTool 30 'ios_device_status' @{}
    if ($mcpStatus.capabilities.DEVICE_DETECTED.status -ne 'PASS' -or
        $mcpStatus.capabilities.INFO_READABLE.status -ne 'PASS' -or
        $mcpStatus.provider.provider -ne 'sirin-ios-driver') {
        throw 'Sirin MCP did not report fresh device and information proof from the internal driver'
    }
    $capture = Invoke-IosTool 31 'ios_screen_capture' @{ label = 'ios-driver-switch-readonly' }
    [pscustomobject]@{
        status = 'DEPLOYED'
        backup = $backup
        provider = $providerProof.status
        phone = $providerProof.phone
        mcp = $mcpStatus
        capture = $capture
        tasks = @(Get-TaskRows)
    } | ConvertTo-Json -Depth 14
}
catch {
    $switchError = $_.Exception.Message
    try {
        Restore-TaskBackup $backup | Out-Null
    }
    catch {
        throw "iOS Driver switch failed ($switchError); automatic task rollback also failed: $($_.Exception.Message)"
    }
    throw "iOS Driver switch failed and prior tasks were restored: $switchError"
}
