#requires -Version 5.1
<#
Inspect, verify, deploy, or explicitly roll back the Sirin Windows daemon.

Deploy verifies an exact candidate SHA-256, starts the candidate in the
side-effect-minimized --ai-monitor-only mode on an alternate loopback port,
backs up the current binary and scheduled-task action, then either replaces the
task-owned executable or atomically redirects the task to an immutable versioned
deployment directory. A failed live contract check restores both automatically.
#>

[CmdletBinding()]
param(
    [ValidateSet('Status', 'Verify', 'Deploy', 'Rollback')]
    [string]$Action = 'Status',
    [string]$Repo = '',
    [string]$TaskName = 'Sirin Local Ops Daemon',
    [string]$LiveBinary = '',
    [string]$CandidateBinary = '',
    [string]$ExpectedCandidateSha256 = '',
    [string]$BackupBinary = '',
    [string]$DeploymentRoot = '',
    [int]$LivePort = 7700,
    [int]$SmokePort = 17700,
    [int]$TimeoutSeconds = 30
)

$ErrorActionPreference = 'Stop'

# PSScriptRoot is not reliable inside a parameter default expression in
# Windows PowerShell 5.1. Resolve it only after parameter binding completes.
if ([string]::IsNullOrWhiteSpace($Repo)) {
    $Repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
}
$Repo = [System.IO.Path]::GetFullPath($Repo)
$defaultLiveBinary = Join-Path $Repo 'target\release\sirin.exe'
if ([string]::IsNullOrWhiteSpace($LiveBinary)) {
    $existingTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $existingAction = if ($existingTask) { @($existingTask.Actions) | Select-Object -First 1 } else { $null }
    $LiveBinary = if ($existingAction -and -not [string]::IsNullOrWhiteSpace([string]$existingAction.Execute)) {
        [string]$existingAction.Execute
    }
    else { $defaultLiveBinary }
}
$liveBinary = [System.IO.Path]::GetFullPath($LiveBinary)
$backupDir = Join-Path $env:LOCALAPPDATA 'Sirin\releases\backups'
$liveBase = "http://127.0.0.1:$LivePort"

if (-not ('SirinFileIdentity' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class SirinFileIdentity
{
    [StructLayout(LayoutKind.Sequential)]
    private struct ByHandleFileInformation
    {
        public uint FileAttributes;
        public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
        public uint VolumeSerialNumber;
        public uint FileSizeHigh;
        public uint FileSizeLow;
        public uint NumberOfLinks;
        public uint FileIndexHigh;
        public uint FileIndexLow;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetFileInformationByHandle(
        SafeFileHandle handle,
        out ByHandleFileInformation information);

    public static string Get(string path)
    {
        using (FileStream stream = new FileStream(
            path,
            FileMode.Open,
            FileAccess.Read,
            FileShare.ReadWrite | FileShare.Delete))
        {
            ByHandleFileInformation information;
            if (!GetFileInformationByHandle(stream.SafeFileHandle, out information))
            {
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            }
            return String.Format(
                "{0:x8}:{1:x8}{2:x8}",
                information.VolumeSerialNumber,
                information.FileIndexHigh,
                information.FileIndexLow);
        }
    }
}
'@
}

function Get-Hash([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Test-SameFilePath([string]$Left, [string]$Right) {
    try {
        $leftFull = [System.IO.Path]::GetFullPath($Left)
        $rightFull = [System.IO.Path]::GetFullPath($Right)
        if ([string]::Equals($leftFull, $rightFull, [System.StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
        if (-not (Test-Path -LiteralPath $leftFull -PathType Leaf) -or
            -not (Test-Path -LiteralPath $rightFull -PathType Leaf)) {
            return $false
        }
        return [string]::Equals(
            [SirinFileIdentity]::Get($leftFull),
            [SirinFileIdentity]::Get($rightFull),
            [System.StringComparison]::OrdinalIgnoreCase
        )
    }
    catch {
        return $false
    }
}

function Get-TaskActionSnapshot {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    $actions = @($task.Actions)
    if ($actions.Count -ne 1) {
        throw "expected exactly one scheduled-task action, found $($actions.Count)"
    }
    [pscustomobject]@{
        execute = [string]$actions[0].Execute
        arguments = [string]$actions[0].Arguments
        working_directory = [string]$actions[0].WorkingDirectory
    }
}

function Set-TaskActionFromSnapshot($Snapshot) {
    if ($null -eq $Snapshot -or [string]::IsNullOrWhiteSpace([string]$Snapshot.execute)) {
        throw 'scheduled-task action snapshot is invalid'
    }
    $params = @{ Execute = [string]$Snapshot.execute }
    if (-not [string]::IsNullOrWhiteSpace([string]$Snapshot.arguments)) {
        $params.Argument = [string]$Snapshot.arguments
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$Snapshot.working_directory)) {
        $params.WorkingDirectory = [string]$Snapshot.working_directory
    }
    Set-ScheduledTask -TaskName $TaskName -Action (New-ScheduledTaskAction @params) | Out-Null
}

function Invoke-Mcp([string]$BaseUrl, [int]$Id, [string]$Method, $Params) {
    $request = @{ jsonrpc = '2.0'; id = $Id; method = $Method }
    if ($null -ne $Params) {
        $request.params = $Params
    }
    Invoke-RestMethod `
        -Uri "$BaseUrl/mcp" `
        -Method Post `
        -ContentType 'application/json' `
        -Body ($request | ConvertTo-Json -Depth 12 -Compress) `
        -TimeoutSec 15
}

function Wait-Mcp([string]$BaseUrl, [int]$Seconds) {
    $deadline = (Get-Date).AddSeconds([Math]::Max(5, $Seconds))
    do {
        try {
            return Invoke-Mcp $BaseUrl 90 'tools/list' $null
        }
        catch {
            Start-Sleep -Milliseconds 250
        }
    } while ((Get-Date) -lt $deadline)
    throw "MCP did not become ready: $BaseUrl/mcp"
}

function Test-MonitorContract([string]$BaseUrl) {
    $initialize = Invoke-Mcp $BaseUrl 1 'initialize' @{
        protocolVersion = '2025-03-26'
        capabilities = @{}
        clientInfo = @{ name = 'sirin-daemon-switch'; version = '2' }
    }
    $tools = Invoke-Mcp $BaseUrl 2 'tools/list' $null
    $names = @($tools.result.tools | ForEach-Object { $_.name })
    $requiredTools = @(
        'help',
        'ai_monitor_speed_test',
        'codex_supervisor_snapshot',
        'codex_supervisor_report',
        'codex_supervisor_claim',
        'codex_supervisor_complete_action'
    )
    $missing = @($requiredTools | Where-Object { $_ -notin $names })
    if ($missing.Count -gt 0) {
        throw "candidate is missing required tools: $($missing -join ', ')"
    }

    $monitor = Invoke-RestMethod -Uri "$BaseUrl/api/ai-monitor" -TimeoutSec 20
    if ([string]::IsNullOrWhiteSpace([string]$monitor.version)) {
        throw 'AI monitor response has no version'
    }
    if (-not [bool]$monitor.safety.local_only -or
        -not [bool]$monitor.safety.read_only -or
        [bool]$monitor.safety.credentials_accessed -or
        [bool]$monitor.safety.automatic_download_test -or
        -not [bool]$monitor.safety.background_sampling) {
        throw 'AI monitor safety contract failed'
    }
    if ($null -eq $monitor.ai_work.codex_token_trend.history) {
        throw 'AI monitor token history contract is missing'
    }
    $trend = $monitor.ai_work.codex_token_trend
    if ([string]::IsNullOrWhiteSpace([string]$trend.lifecycle) -or
        $null -eq $trend.PSObject.Properties['source_available']) {
        throw 'AI monitor token lifecycle contract is missing'
    }
    $overhead = $monitor.overhead
    if ($null -eq $overhead -or
        [int]$overhead.network_cache_ttl_secs -lt 60 -or
        [bool]$overhead.background_network_probes -or
        [bool]$overhead.background_download_tests -or
        [int64]$overhead.sampler_runs_total -lt 1 -or
        [string]$overhead.process.memory_evidence -ne 'MEASURED') {
        throw 'AI monitor low-overhead contract failed'
    }
    $acceptance = $monitor.acceptance
    $requiredAcceptanceModes = @(
        'IDLE',
        'APP_CLOSURE',
        'LOCK_CYCLE',
        'STANDBY_CYCLE',
        'RESTART_RESTORE',
        'TOKEN_SOURCE_RECOVERY'
    )
    $acceptanceModes = @($acceptance.modes | ForEach-Object { [string]$_.mode })
    $missingAcceptanceModes = @(
        $requiredAcceptanceModes | Where-Object { $_ -notin $acceptanceModes }
    )
    if ($null -eq $acceptance -or
        [string]::IsNullOrWhiteSpace([string]$acceptance.status) -or
        -not [bool]$acceptance.ledger_persisted -or
        $missingAcceptanceModes.Count -gt 0) {
        throw "AI monitor acceptance ledger contract failed; missing modes: $($missingAcceptanceModes -join ', ')"
    }
    $health = $monitor.codex_health
    $resources = $monitor.ai_work.system_resources
    if ($null -eq $health -or
        [string]::IsNullOrWhiteSpace([string]$health.status) -or
        [string]::IsNullOrWhiteSpace([string]$health.progress_status) -or
        [string]::IsNullOrWhiteSpace([string]$health.local_resource_status) -or
        [string]::IsNullOrWhiteSpace([string]$health.network_status) -or
        $null -eq $health.PSObject.Properties['remote_limit_status'] -or
        $null -eq $resources -or
        [string]$resources.memory_evidence -ne 'MEASURED' -or
        [string]$resources.disk_evidence -ne 'MEASURED' -or
        $null -eq $resources.system_drive_free_gb) {
        throw 'AI monitor Codex health contract failed'
    }

    [pscustomobject]@{
        version = [string]$monitor.version
        server_name = [string]$initialize.result.serverInfo.name
        tool_count = $names.Count
        ai_monitor_tool_ready = 'ai_monitor_speed_test' -in $names
        background_sampling = [bool]$monitor.safety.background_sampling
        local_only = [bool]$monitor.safety.local_only
        read_only = [bool]$monitor.safety.read_only
        history_points = @($monitor.ai_work.codex_token_trend.history).Count
        token_lifecycle = [string]$trend.lifecycle
        token_source_available = [bool]$trend.source_available
        overhead_ready = $true
        network_cache_ttl_secs = [int]$overhead.network_cache_ttl_secs
        sampler_runs_total = [int64]$overhead.sampler_runs_total
        background_network_probes = [bool]$overhead.background_network_probes
        background_download_tests = [bool]$overhead.background_download_tests
        process_memory_evidence = [string]$overhead.process.memory_evidence
        acceptance_status = [string]$acceptance.status
        acceptance_missing_required = @($acceptance.missing_required_modes).Count
        acceptance_ledger_persisted = [bool]$acceptance.ledger_persisted
        acceptance_mode_count = $acceptanceModes.Count
        codex_health_status = [string]$health.status
        codex_progress_status = [string]$health.progress_status
        local_resource_status = [string]$health.local_resource_status
        network_status = [string]$health.network_status
        remote_limit_status = [string]$health.remote_limit_status
        resource_memory_evidence = [string]$resources.memory_evidence
        resource_disk_evidence = [string]$resources.disk_evidence
    }
}

function Test-SupervisorEmptyScanContract([string]$BaseUrl, [string]$Nonce) {
    $reportResponse = Invoke-Mcp $BaseUrl 71 'tools/call' @{
        name = 'codex_supervisor_report'
        arguments = @{
            threadId = 'sirin-supervisor-scan'
            latestUserTurnKey = "scan-contract-$Nonce"
            latestTurnStatus = 'COMPLETED'
        }
    }
    $reportText = [string](@($reportResponse.result.content) | Select-Object -First 1).text
    $report = $reportText | ConvertFrom-Json
    if ([string]$report.status -ne 'SCAN_RECORDED' -or
        [string]$report.scanStatus -ne 'SUCCESS') {
        throw 'supervisor scan heartbeat was not recorded'
    }

    $snapshotResponse = Invoke-Mcp $BaseUrl 72 'tools/call' @{
        name = 'codex_supervisor_snapshot'
        arguments = @{}
    }
    $snapshotText = [string](@($snapshotResponse.result.content) | Select-Object -First 1).text
    $snapshot = $snapshotText | ConvertFrom-Json
    if ([string]$snapshot.status -ne 'HEALTHY_IDLE' -or
        [int]$snapshot.taskCount -ne 0 -or
        [string]$snapshot.scanStatus -ne 'SUCCESS' -or
        $null -eq $snapshot.lastScanAtMs) {
        throw 'successful empty scan did not produce HEALTHY_IDLE'
    }

    [pscustomobject]@{
        report_status = [string]$report.status
        snapshot_status = [string]$snapshot.status
        task_count = [int]$snapshot.taskCount
        scan_status = [string]$snapshot.scanStatus
        last_scan_at_ms = [int64]$snapshot.lastScanAtMs
    }
}

function Test-Candidate([string]$Path) {
    if (Get-NetTCPConnection -LocalPort $SmokePort -State Listen -ErrorAction SilentlyContinue) {
        throw "candidate smoke port is already in use: $SmokePort"
    }
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    $nonce = [Guid]::NewGuid().ToString('N')
    $stdout = Join-Path $env:TEMP "sirin-daemon-switch-$nonce.out.log"
    $stderr = Join-Path $env:TEMP "sirin-daemon-switch-$nonce.err.log"
    $trendState = Join-Path $env:TEMP "sirin-daemon-switch-$nonce-token-trend.jsonl"
    $previousPort = $env:SIRIN_RPC_PORT
    $previousTrendState = $env:SIRIN_AI_MONITOR_TREND_PATH
    $previousAwakeGuard = $env:SIRIN_AI_MONITOR_DISABLE_AWAKE_GUARD
    $process = $null
    try {
        $env:SIRIN_RPC_PORT = [string]$SmokePort
        $env:SIRIN_AI_MONITOR_TREND_PATH = $trendState
        $env:SIRIN_AI_MONITOR_DISABLE_AWAKE_GUARD = '1'
        $process = Start-Process `
            -FilePath $resolved `
            -ArgumentList @('--ai-monitor-only') `
            -WorkingDirectory $Repo `
            -RedirectStandardOutput $stdout `
            -RedirectStandardError $stderr `
            -WindowStyle Hidden `
            -PassThru
        $baseUrl = "http://127.0.0.1:$SmokePort"
        Wait-Mcp $baseUrl $TimeoutSeconds | Out-Null
        $proof = Test-MonitorContract $baseUrl
        $supervisorProof = Test-SupervisorEmptyScanContract $baseUrl $nonce
        $proof | Add-Member -NotePropertyName supervisor_empty_scan -NotePropertyValue $supervisorProof
        $proof
    }
    finally {
        if ($process) {
            $current = Get-Process -Id $process.Id -ErrorAction SilentlyContinue
            if ($current -and (Test-SameFilePath $current.Path $resolved)) {
                Stop-Process -Id $current.Id
                Wait-Process -Id $current.Id -Timeout 10 -ErrorAction SilentlyContinue
            }
        }
        if ($null -eq $previousPort) {
            Remove-Item Env:SIRIN_RPC_PORT -ErrorAction SilentlyContinue
        }
        else {
            $env:SIRIN_RPC_PORT = $previousPort
        }
        if ($null -eq $previousTrendState) {
            Remove-Item Env:SIRIN_AI_MONITOR_TREND_PATH -ErrorAction SilentlyContinue
        }
        else {
            $env:SIRIN_AI_MONITOR_TREND_PATH = $previousTrendState
        }
        if ($null -eq $previousAwakeGuard) {
            Remove-Item Env:SIRIN_AI_MONITOR_DISABLE_AWAKE_GUARD -ErrorAction SilentlyContinue
        }
        else {
            $env:SIRIN_AI_MONITOR_DISABLE_AWAKE_GUARD = $previousAwakeGuard
        }
        Remove-Item -LiteralPath $stdout, $stderr, $trendState -Force -ErrorAction SilentlyContinue
    }
}

function Get-LiveStatus {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $listener = Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort $LivePort -State Listen `
        -ErrorAction SilentlyContinue | Select-Object -First 1
    $process = if ($listener) {
        Get-Process -Id $listener.OwningProcess -ErrorAction SilentlyContinue
    } else { $null }
    $taskAction = if ($task) { @($task.Actions) | Select-Object -First 1 } else { $null }
    $effectiveBinary = if ($taskAction -and -not [string]::IsNullOrWhiteSpace([string]$taskAction.Execute)) {
        [System.IO.Path]::GetFullPath([string]$taskAction.Execute)
    }
    else { $liveBinary }
    $proof = $null
    $toolCount = $null
    $toolError = $null
    $proofError = $null
    try {
        $tools = Invoke-Mcp $liveBase 81 'tools/list' $null
        $toolCount = @($tools.result.tools).Count
    }
    catch { $toolError = $_.Exception.Message }
    try {
        $proof = Test-MonitorContract $liveBase
    }
    catch { $proofError = $_.Exception.Message }
    [pscustomobject]@{
        task_installed = $null -ne $task
        task_state = if ($task) { [string]$task.State } else { 'MISSING' }
        binary = $effectiveBinary
        binary_exists = Test-Path -LiteralPath $effectiveBinary -PathType Leaf
        binary_sha256 = if (Test-Path -LiteralPath $effectiveBinary -PathType Leaf) {
            Get-Hash $effectiveBinary
        } else { $null }
        listener_pid = if ($listener) { $listener.OwningProcess } else { $null }
        listener_path = if ($process) { $process.Path } else { $null }
        task_binary_same_file = if ($taskAction) {
            Test-SameFilePath ([string]$taskAction.Execute) $effectiveBinary
        } else { $false }
        listener_binary_same_file = if ($process) {
            Test-SameFilePath ([string]$process.Path) $effectiveBinary
        } else { $false }
        tool_count = $toolCount
        tool_error = $toolError
        monitor_ready = $null -ne $proof
        monitor_error = $proofError
        monitor_proof = $proof
    }
}

function Stop-LiveTask([string]$ExpectedBinary) {
    Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
    $listener = Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort $LivePort -State Listen `
        -ErrorAction SilentlyContinue | Select-Object -First 1
    $processId = if ($listener) { [int]$listener.OwningProcess } else { 0 }
    if ($processId -gt 0) {
        $process = Get-Process -Id $processId -ErrorAction Stop
        if (-not (Test-SameFilePath $process.Path $ExpectedBinary)) {
            throw "refusing to stop unexpected live process: $($process.Path)"
        }
    }
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($processId -eq 0) { return }
    Wait-Process -Id $processId -Timeout 12 -ErrorAction SilentlyContinue
    if (Get-Process -Id $processId -ErrorAction SilentlyContinue) {
        Stop-Process -Id $processId -Force
        Wait-Process -Id $processId -Timeout 5 -ErrorAction SilentlyContinue
    }
    if (Get-Process -Id $processId -ErrorAction SilentlyContinue) {
        throw "scheduled task did not stop verified PID ${processId}: $ExpectedBinary"
    }
}

function Start-And-VerifyLive([string]$ExpectedBinary) {
    Start-ScheduledTask -TaskName $TaskName
    Wait-Mcp $liveBase $TimeoutSeconds | Out-Null
    $listener = Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort $LivePort -State Listen `
        -ErrorAction Stop | Select-Object -First 1
    if (-not $listener) {
        throw "live listener missing after scheduled-task start: $liveBase"
    }
    $process = Get-Process -Id ([int]$listener.OwningProcess) -ErrorAction Stop
    if (-not (Test-SameFilePath $process.Path $ExpectedBinary)) {
        throw "live listener PID $($listener.OwningProcess) does not own expected binary: $ExpectedBinary"
    }
    $taskAction = Get-TaskActionSnapshot
    if (-not (Test-SameFilePath ([string]$taskAction.execute) $ExpectedBinary)) {
        throw "scheduled-task action drifted after start: $($taskAction.execute)"
    }
    Test-MonitorContract $liveBase
}

function Restore-Backup([string]$Path) {
    $resolvedBackup = (Resolve-Path -LiteralPath $Path).Path
    $resolvedBackupDir = (Resolve-Path -LiteralPath $backupDir).Path
    if (-not $resolvedBackup.StartsWith(
        $resolvedBackupDir + [System.IO.Path]::DirectorySeparatorChar,
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        throw "backup must be an exact file under $resolvedBackupDir"
    }
    $actionSnapshotPath = "$resolvedBackup.task-action.json"
    if (-not (Test-Path -LiteralPath $actionSnapshotPath -PathType Leaf)) {
        throw "scheduled-task action snapshot is missing: $actionSnapshotPath"
    }
    $actionSnapshot = Get-Content -LiteralPath $actionSnapshotPath -Raw -Encoding UTF8 | ConvertFrom-Json
    $currentAction = Get-TaskActionSnapshot
    Stop-LiveTask ([string]$currentAction.execute)
    $restoreBinary = [System.IO.Path]::GetFullPath([string]$actionSnapshot.execute)
    $restoreDirectory = Split-Path -Parent $restoreBinary
    if (-not (Test-Path -LiteralPath $restoreDirectory -PathType Container)) {
        New-Item -ItemType Directory -Force -Path $restoreDirectory | Out-Null
    }
    $restoreMatches = (Test-Path -LiteralPath $restoreBinary -PathType Leaf) -and
        ((Get-Hash $restoreBinary) -eq (Get-Hash $resolvedBackup))
    if (-not $restoreMatches) {
        Copy-Item -LiteralPath $resolvedBackup -Destination $restoreBinary -Force
    }
    Set-TaskActionFromSnapshot $actionSnapshot
    Start-ScheduledTask -TaskName $TaskName
    Wait-Mcp $liveBase $TimeoutSeconds | Out-Null
}

if ($Action -eq 'Status') {
    Get-LiveStatus | ConvertTo-Json -Depth 6
    return
}

if ($Action -eq 'Rollback') {
    if ([string]::IsNullOrWhiteSpace($BackupBinary)) {
        throw '-BackupBinary is required for Rollback; latest is never inferred'
    }
    Restore-Backup $BackupBinary
    Get-LiveStatus | ConvertTo-Json -Depth 6
    return
}

if ([string]::IsNullOrWhiteSpace($CandidateBinary) -or
    -not (Test-Path -LiteralPath $CandidateBinary -PathType Leaf)) {
    throw '-CandidateBinary must identify an existing candidate exe'
}
if ($ExpectedCandidateSha256 -notmatch '^[A-Fa-f0-9]{64}$') {
    throw '-ExpectedCandidateSha256 must be the reviewed 64-character SHA-256'
}
$candidateHash = Get-Hash $CandidateBinary
if ($candidateHash -ne $ExpectedCandidateSha256.ToLowerInvariant()) {
    throw "candidate hash mismatch: expected $ExpectedCandidateSha256, got $candidateHash"
}

$candidateProof = Test-Candidate $CandidateBinary
$liveBefore = Get-LiveStatus
$liveToolCount = if ($null -ne $liveBefore.tool_count) { [int]$liveBefore.tool_count } else { 0 }
$toolRegression = $liveToolCount -gt 0 -and
    [int]$candidateProof.tool_count -lt $liveToolCount

if ($Action -eq 'Verify') {
    [pscustomobject]@{
        status = if ($toolRegression) { 'BLOCKED_TOOL_REGRESSION' } else { 'VERIFIED_NOT_DEPLOYED' }
        candidate_deployable = -not $toolRegression
        candidate = (Resolve-Path -LiteralPath $CandidateBinary).Path
        candidate_sha256 = $candidateHash
        tool_count_regression = $toolRegression
        live_before = $liveBefore
        candidate_proof = $candidateProof
    } | ConvertTo-Json -Depth 7
    return
}

if ($toolRegression) {
    throw "candidate MCP tool regression: live=$liveToolCount, candidate=$($candidateProof.tool_count)"
}
if (-not (Test-Path -LiteralPath $liveBinary -PathType Leaf)) {
    throw "live Sirin binary is missing: $liveBinary"
}
$taskAction = Get-TaskActionSnapshot
if (-not (Test-SameFilePath ([string]$taskAction.execute) $liveBinary)) {
    throw "scheduled task does not own the expected live binary: $($taskAction.execute)"
}

New-Item -ItemType Directory -Force -Path $backupDir | Out-Null
$liveHash = Get-Hash $liveBinary
$timestamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$backup = Join-Path $backupDir "sirin-$timestamp-$liveHash.exe"
Copy-Item -LiteralPath $liveBinary -Destination $backup
$taskActionBackup = "$backup.task-action.json"
$taskAction | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath $taskActionBackup -Encoding UTF8

$deploymentMode = 'in_place'
$deployedBinary = $liveBinary
$deploymentAction = $taskAction
if (-not [string]::IsNullOrWhiteSpace($DeploymentRoot)) {
    $deploymentMode = 'versioned_directory'
    $resolvedDeploymentRoot = [System.IO.Path]::GetFullPath($DeploymentRoot)
    New-Item -ItemType Directory -Force -Path $resolvedDeploymentRoot | Out-Null
    $deploymentDirectory = Join-Path $resolvedDeploymentRoot "sirin-$($candidateHash.Substring(0, 12))"
    $deployedBinary = Join-Path $deploymentDirectory 'sirin.exe'
    if (Test-Path -LiteralPath $deploymentDirectory) {
        if (-not (Test-Path -LiteralPath $deploymentDirectory -PathType Container)) {
            throw "versioned deployment path is not a directory: $deploymentDirectory"
        }
        if (-not (Test-Path -LiteralPath $deployedBinary -PathType Leaf)) {
            throw "existing versioned deployment is incomplete: $deployedBinary"
        }
        $existingDeploymentHash = Get-Hash $deployedBinary
        if ($existingDeploymentHash -ne $candidateHash) {
            throw "immutable deployment hash mismatch: expected $candidateHash, got $existingDeploymentHash"
        }
    }
    else {
        New-Item -ItemType Directory -Path $deploymentDirectory | Out-Null
        Copy-Item -LiteralPath $CandidateBinary -Destination $deployedBinary
        $deployedHash = Get-Hash $deployedBinary
        if ($deployedHash -ne $candidateHash) {
            throw "copied deployment hash mismatch: expected $candidateHash, got $deployedHash"
        }
    }
    $deploymentAction = [pscustomobject]@{
        execute = $deployedBinary
        arguments = [string]$taskAction.arguments
        working_directory = $deploymentDirectory
    }
}

try {
    Stop-LiveTask $liveBinary
    if ($deploymentMode -eq 'versioned_directory') {
        Set-TaskActionFromSnapshot $deploymentAction
    }
    else {
        Copy-Item -LiteralPath $CandidateBinary -Destination $liveBinary -Force
    }
    $liveProof = Start-And-VerifyLive $deployedBinary
    [pscustomobject]@{
        status = 'DEPLOYED'
        deployment_mode = $deploymentMode
        deployed_binary = $deployedBinary
        candidate_sha256 = $candidateHash
        backup = $backup
        task_action_backup = $taskActionBackup
        task_action = Get-TaskActionSnapshot
        candidate_proof = $candidateProof
        live_proof = $liveProof
    } | ConvertTo-Json -Depth 7
}
catch {
    $deployError = $_.Exception.Message
    try {
        Restore-Backup $backup
    }
    catch {
        throw "deploy failed ($deployError); automatic rollback also failed: $($_.Exception.Message)"
    }
    throw "deploy failed and old binary was restored: $deployError"
}
