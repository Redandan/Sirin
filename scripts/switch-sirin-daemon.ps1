#requires -Version 5.1
<#
Inspect, verify, deploy, or explicitly roll back the Sirin Windows daemon.

Deploy verifies an exact candidate SHA-256, starts the candidate in the
side-effect-minimized --ai-monitor-only mode on an alternate loopback port,
backs up the current binary and scheduled-task action, then replaces only the
task-owned executable. A failed live contract check restores both automatically.
#>

[CmdletBinding()]
param(
    [ValidateSet('Status', 'Verify', 'Deploy', 'Rollback')]
    [string]$Action = 'Status',
    [string]$Repo = '',
    [string]$TaskName = 'Sirin Local Ops Daemon',
    [string]$CandidateBinary = '',
    [string]$ExpectedCandidateSha256 = '',
    [string]$BackupBinary = '',
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
$liveBinary = Join-Path $Repo 'target\release\sirin.exe'
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
    $requiredTools = @('help', 'ai_monitor_speed_test')
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

    [pscustomobject]@{
        version = [string]$monitor.version
        server_name = [string]$initialize.result.serverInfo.name
        tool_count = $names.Count
        ai_monitor_tool_ready = 'ai_monitor_speed_test' -in $names
        background_sampling = [bool]$monitor.safety.background_sampling
        local_only = [bool]$monitor.safety.local_only
        read_only = [bool]$monitor.safety.read_only
        history_points = @($monitor.ai_work.codex_token_trend.history).Count
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
        Test-MonitorContract $baseUrl
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
    $proof = $null
    $toolCount = $null
    try {
        $tools = Invoke-Mcp $liveBase 81 'tools/list' $null
        $toolCount = @($tools.result.tools).Count
    }
    catch {}
    try {
        $proof = Test-MonitorContract $liveBase
    }
    catch {}
    [pscustomobject]@{
        task_installed = $null -ne $task
        task_state = if ($task) { [string]$task.State } else { 'MISSING' }
        binary = $liveBinary
        binary_exists = Test-Path -LiteralPath $liveBinary -PathType Leaf
        binary_sha256 = if (Test-Path -LiteralPath $liveBinary -PathType Leaf) {
            Get-Hash $liveBinary
        } else { $null }
        listener_pid = if ($listener) { $listener.OwningProcess } else { $null }
        listener_path = if ($process) { $process.Path } else { $null }
        task_binary_same_file = if ($taskAction) {
            Test-SameFilePath ([string]$taskAction.Execute) $liveBinary
        } else { $false }
        listener_binary_same_file = if ($process) {
            Test-SameFilePath ([string]$process.Path) $liveBinary
        } else { $false }
        tool_count = $toolCount
        monitor_ready = $null -ne $proof
        monitor_proof = $proof
    }
}

function Stop-LiveTask {
    Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $deadline = (Get-Date).AddSeconds(10)
    do {
        $processes = @(Get-Process sirin -ErrorAction SilentlyContinue | Where-Object {
            try { Test-SameFilePath $_.Path $liveBinary } catch { $false }
        })
        if ($processes.Count -eq 0) {
            return
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw "scheduled task did not stop the exact Sirin binary: $liveBinary"
}

function Start-And-VerifyLive {
    Start-ScheduledTask -TaskName $TaskName
    Wait-Mcp $liveBase $TimeoutSeconds | Out-Null
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
    Stop-LiveTask
    Copy-Item -LiteralPath $resolvedBackup -Destination $liveBinary -Force
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
        status = 'VERIFIED_NOT_DEPLOYED'
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

try {
    Stop-LiveTask
    Copy-Item -LiteralPath $CandidateBinary -Destination $liveBinary -Force
    $liveProof = Start-And-VerifyLive
    [pscustomobject]@{
        status = 'DEPLOYED'
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
