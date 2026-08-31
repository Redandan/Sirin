#requires -Version 5.1
<#
Inspect, verify, deploy, or explicitly roll back the Sirin Windows daemon.

Deploy verifies an exact candidate SHA-256, starts the candidate in the
side-effect-minimized --mcp-only mode on an alternate loopback port, stages it
under an immutable per-hash deployment directory, and switches only the
scheduled-task action. A failed live contract check restores the previous task
action automatically. The repository build output is never used as the live
runtime path.
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
    [string]$DeploymentRoot = '',
    [int]$LivePort = 7700,
    [int]$SmokePort = 17700,
    [int]$IosDriverSmokePort = 18770,
    [int]$TimeoutSeconds = 30
)

$ErrorActionPreference = 'Stop'

# PSScriptRoot is not reliable inside a parameter default expression in
# Windows PowerShell 5.1. Resolve it only after parameter binding completes.
if ([string]::IsNullOrWhiteSpace($Repo)) {
    $Repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
}
$Repo = [System.IO.Path]::GetFullPath($Repo)
$toolBaseline = Join-Path $Repo 'config\mcp_tool_baseline.json'
$backupDir = Join-Path $env:LOCALAPPDATA 'Sirin\releases\backups'
if ([string]::IsNullOrWhiteSpace($DeploymentRoot)) {
    $DeploymentRoot = Join-Path $env:LOCALAPPDATA 'Sirin\deployments'
}
$deploymentRoot = [System.IO.Path]::GetFullPath($DeploymentRoot)
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

function Test-PathUnderRoot([string]$Path, [string]$Root) {
    $fullPath = [System.IO.Path]::GetFullPath($Path)
    $fullRoot = [System.IO.Path]::GetFullPath($Root).TrimEnd(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    $fullPath.StartsWith(
        $fullRoot + [System.IO.Path]::DirectorySeparatorChar,
        [System.StringComparison]::OrdinalIgnoreCase
    )
}

function Get-TaskBinary($Snapshot) {
    if ($null -eq $Snapshot -or [string]::IsNullOrWhiteSpace([string]$Snapshot.execute)) {
        throw 'scheduled-task action does not identify a live executable'
    }
    [System.IO.Path]::GetFullPath([string]$Snapshot.execute)
}

function Get-SourceProvenance {
    Push-Location $Repo
    try {
        $head = (& git rev-parse HEAD 2>$null | Select-Object -First 1)
        $branch = (& git branch --show-current 2>$null | Select-Object -First 1)
        $remote = (& git config --get remote.origin.url 2>$null | Select-Object -First 1)
        $upstream = (& git rev-parse '@{u}' 2>$null | Select-Object -First 1)
        $dirty = @(& git status --porcelain 2>$null)
        if ([string]::IsNullOrWhiteSpace([string]$head) -or
            [string]::IsNullOrWhiteSpace([string]$branch)) {
            throw 'deployment requires a named Git branch and commit'
        }
        if ($dirty.Count -ne 0) {
            throw "deployment requires a clean worktree; dirty entries=$($dirty.Count)"
        }
        if ([string]::IsNullOrWhiteSpace([string]$upstream) -or $upstream -ne $head) {
            throw "deployment requires HEAD to equal its pushed upstream; head=$head upstream=$upstream"
        }
        [pscustomobject]@{
            repository = [string]$remote
            branch = [string]$branch
            commit = [string]$head
            remoteCommit = [string]$upstream
            sourceMode = 'cleanPushedHeadWithReviewedArtifactSha'
            snapshotOverlayRequired = $false
        }
    }
    finally {
        Pop-Location
    }
}

function Stage-ImmutableCandidate([string]$Path, [string]$Sha256) {
    New-Item -ItemType Directory -Force -Path $deploymentRoot | Out-Null
    $deploymentDir = Join-Path $deploymentRoot "sirin-$($Sha256.Substring(0, 12))"
    if (-not (Test-PathUnderRoot $deploymentDir $deploymentRoot)) {
        throw "deployment path escaped the configured root: $deploymentDir"
    }
    New-Item -ItemType Directory -Force -Path $deploymentDir | Out-Null
    $target = Join-Path $deploymentDir 'sirin.exe'
    if (Test-Path -LiteralPath $target -PathType Leaf) {
        $existingHash = Get-Hash $target
        if ($existingHash -ne $Sha256) {
            throw "immutable deployment collision: $target has SHA-256 $existingHash"
        }
        return [pscustomobject]@{ directory = $deploymentDir; binary = $target; reused = $true }
    }
    $unexpected = @(Get-ChildItem -LiteralPath $deploymentDir -Force -ErrorAction Stop)
    if ($unexpected.Count -ne 0) {
        throw "immutable deployment directory already contains unexpected files: $deploymentDir"
    }

    $stage = Join-Path $deploymentDir ("sirin.exe.stage-" + [Guid]::NewGuid().ToString('N'))
    try {
        Copy-Item -LiteralPath $Path -Destination $stage
        $stageHash = Get-Hash $stage
        if ($stageHash -ne $Sha256) {
            throw "staged candidate hash mismatch: expected $Sha256, got $stageHash"
        }
        Move-Item -LiteralPath $stage -Destination $target
    }
    finally {
        if (Test-Path -LiteralPath $stage -PathType Leaf) {
            Remove-Item -LiteralPath $stage -Force
        }
    }
    [pscustomobject]@{ directory = $deploymentDir; binary = $target; reused = $false }
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

function Get-ExpectedToolNames {
    if (-not (Test-Path -LiteralPath $toolBaseline -PathType Leaf)) {
        throw "committed MCP tool baseline is missing: $toolBaseline"
    }
    try {
        $parsed = Get-Content -Raw -LiteralPath $toolBaseline | ConvertFrom-Json
        # Windows PowerShell 5.1 emits a top-level JSON array as one array
        # object. Re-emit its contents so both 5.1 and PowerShell 7 count the
        # baseline names consistently.
        $names = @($parsed | ForEach-Object { $_ })
    }
    catch {
        throw "committed MCP tool baseline is invalid JSON: $($_.Exception.Message)"
    }
    $normalized = @($names | ForEach-Object { [string]$_ } | Sort-Object -Unique)
    if ($normalized.Count -ne $names.Count -or $normalized.Count -ne 190) {
        throw "committed MCP tool baseline must contain exactly 190 unique names"
    }
    $normalized
}

function Test-McpContract([string]$BaseUrl) {
    $initialize = Invoke-Mcp $BaseUrl 1 'initialize' @{
        protocolVersion = '2025-03-26'
        capabilities = @{}
        clientInfo = @{ name = 'sirin-daemon-switch'; version = '2' }
    }
    $tools = Invoke-Mcp $BaseUrl 2 'tools/list' $null
    $names = @($tools.result.tools | ForEach-Object { [string]$_.name } | Sort-Object -Unique)
    $expectedNames = @(Get-ExpectedToolNames)
    $inventoryMissing = @($expectedNames | Where-Object { $_ -notin $names })
    $inventoryUnexpected = @($names | Where-Object { $_ -notin $expectedNames })
    if ($inventoryMissing.Count -gt 0 -or $inventoryUnexpected.Count -gt 0) {
        throw ('candidate MCP inventory does not match the committed baseline; ' +
            "missing=[$($inventoryMissing -join ', ')], " +
            "unexpected=[$($inventoryUnexpected -join ', ')]")
    }
    $requiredTools = @(
        'help',
        'ios_device_status',
        'ios_control_session_start',
        'ios_control_session_stop',
        'ios_acceptance_run'
    )
    $missing = @($requiredTools | Where-Object { $_ -notin $names })
    if ($missing.Count -gt 0) {
        throw "candidate is missing required tools: $($missing -join ', ')"
    }

    [pscustomobject]@{
        version = [string]$initialize.result.serverInfo.version
        server_name = [string]$initialize.result.serverInfo.name
        tool_count = $names.Count
        expected_tool_count = $expectedNames.Count
        tool_inventory_match = $true
        required_tools_ready = $true
        mcp_only_smoke = $true
    }
}

function Test-IosMcpFailClosedContract([string]$BaseUrl) {
    $tools = Invoke-Mcp $BaseUrl 31 'tools/list' $null
    $names = @($tools.result.tools | ForEach-Object { $_.name })
    $required = @(
        'ios_device_status',
        'ios_control_session_start',
        'ios_control_session_stop',
        'ios_acceptance_run'
    )
    $missing = @($required | Where-Object { $_ -notin $names })
    if ($missing.Count -gt 0) {
        throw "candidate is missing required iPhone tools: $($missing -join ', ')"
    }

    $response = Invoke-Mcp $BaseUrl 32 'tools/call' @{
        name = 'ios_device_status'
        arguments = @{}
    }
    if ($response.error) {
        throw "candidate iPhone status call failed: $($response.error.message)"
    }
    $text = @($response.result.content | Where-Object { $_.type -eq 'text' } |
        Select-Object -First 1).text
    if ([string]::IsNullOrWhiteSpace([string]$text)) {
        throw 'candidate iPhone status omitted its structured text result'
    }
    try { $status = $text | ConvertFrom-Json }
    catch { throw "candidate iPhone status returned invalid JSON: $($_.Exception.Message)" }

    $capabilityNames = @('DEVICE_DETECTED', 'INFO_READABLE', 'SCREEN_CAPTURE', 'SCREEN_CONTROL')
    $unexpectedCapabilities = @($capabilityNames | Where-Object {
        [string]$status.capabilities.$_.status -ne 'MISSING_PROOF'
    })
    if ([bool]$status.ok -or
        [string]$status.provider_contract_status -ne 'STALE_OR_UNPROVEN' -or
        [bool]$status.provider.provider_reachable -or
        $null -ne $status.active_lease -or
        [bool]$status.link_cleanup_required -or
        [bool]$status.lifecycle.autostart_enabled -or
        [bool]$status.lifecycle.manager_loop_started -or
        $unexpectedCapabilities.Count -gt 0) {
        throw ('candidate iPhone unavailable-provider contract did not fail closed; ' +
            "unexpected capabilities: $($unexpectedCapabilities -join ', ')")
    }

    [pscustomobject]@{
        required_tools_ready = $true
        required_tool_count = $required.Count
        provider_contract_status = [string]$status.provider_contract_status
        provider_reachable = [bool]$status.provider.provider_reachable
        active_lease = $status.active_lease
        link_cleanup_required = [bool]$status.link_cleanup_required
        manager_loop_started = [bool]$status.lifecycle.manager_loop_started
        capabilities = @($capabilityNames | ForEach-Object {
            [pscustomobject]@{ name = $_; status = [string]$status.capabilities.$_.status }
        })
    }
}

function Test-Candidate([string]$Path) {
    if (Get-NetTCPConnection -LocalPort $SmokePort -State Listen -ErrorAction SilentlyContinue) {
        throw "candidate smoke port is already in use: $SmokePort"
    }
    if (Get-NetTCPConnection -LocalPort $IosDriverSmokePort -State Listen -ErrorAction SilentlyContinue) {
        throw "candidate iPhone Driver smoke port is already in use: $IosDriverSmokePort"
    }
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    $nonce = [Guid]::NewGuid().ToString('N')
    $stdout = Join-Path $env:TEMP "sirin-daemon-switch-$nonce.out.log"
    $stderr = Join-Path $env:TEMP "sirin-daemon-switch-$nonce.err.log"
    $trendState = Join-Path $env:TEMP "sirin-daemon-switch-$nonce-token-trend.jsonl"
    $supervisorState = Join-Path $env:TEMP "sirin-daemon-switch-$nonce-supervisor-state.json"
    $previousPort = $env:SIRIN_RPC_PORT
    $previousTrendState = $env:SIRIN_AI_MONITOR_TREND_PATH
    $previousSupervisorState = $env:SIRIN_CODEX_SUPERVISOR_STATE_PATH
    $previousAwakeGuard = $env:SIRIN_AI_MONITOR_DISABLE_AWAKE_GUARD
    $previousIosDriverUrl = $env:SIRIN_IOS_DRIVER_URL
    $previousIosDriverAutostart = $env:SIRIN_IOS_DRIVER_AUTOSTART
    $process = $null
    try {
        $env:SIRIN_RPC_PORT = [string]$SmokePort
        $env:SIRIN_AI_MONITOR_TREND_PATH = $trendState
        $env:SIRIN_CODEX_SUPERVISOR_STATE_PATH = $supervisorState
        $env:SIRIN_AI_MONITOR_DISABLE_AWAKE_GUARD = '1'
        $env:SIRIN_IOS_DRIVER_URL = "http://127.0.0.1:$IosDriverSmokePort"
        $env:SIRIN_IOS_DRIVER_AUTOSTART = '0'
        $process = Start-Process `
            -FilePath $resolved `
            -ArgumentList @('--mcp-only') `
            -WorkingDirectory $Repo `
            -RedirectStandardOutput $stdout `
            -RedirectStandardError $stderr `
            -WindowStyle Hidden `
            -PassThru
        $baseUrl = "http://127.0.0.1:$SmokePort"
        Wait-Mcp $baseUrl $TimeoutSeconds | Out-Null
        $proof = Test-McpContract $baseUrl
        $proof | Add-Member -NotePropertyName iphone_fail_closed -NotePropertyValue (
            Test-IosMcpFailClosedContract $baseUrl
        )
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
        if ($null -eq $previousSupervisorState) {
            Remove-Item Env:SIRIN_CODEX_SUPERVISOR_STATE_PATH -ErrorAction SilentlyContinue
        }
        else {
            $env:SIRIN_CODEX_SUPERVISOR_STATE_PATH = $previousSupervisorState
        }
        if ($null -eq $previousIosDriverUrl) {
            Remove-Item Env:SIRIN_IOS_DRIVER_URL -ErrorAction SilentlyContinue
        }
        else {
            $env:SIRIN_IOS_DRIVER_URL = $previousIosDriverUrl
        }
        if ($null -eq $previousIosDriverAutostart) {
            Remove-Item Env:SIRIN_IOS_DRIVER_AUTOSTART -ErrorAction SilentlyContinue
        }
        else {
            $env:SIRIN_IOS_DRIVER_AUTOSTART = $previousIosDriverAutostart
        }
        Remove-Item -LiteralPath $stdout, $stderr, $trendState, $supervisorState `
            -Force -ErrorAction SilentlyContinue
    }
}

function Get-LiveStatus {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $taskSnapshot = if ($task) { Get-TaskActionSnapshot } else { $null }
    $taskBinary = $null
    try {
        if ($taskSnapshot) { $taskBinary = Get-TaskBinary $taskSnapshot }
    }
    catch {}
    $listener = Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort $LivePort -State Listen `
        -ErrorAction SilentlyContinue | Select-Object -First 1
    $process = if ($listener) {
        Get-Process -Id $listener.OwningProcess -ErrorAction SilentlyContinue
    } else { $null }
    $proof = $null
    $toolCount = $null
    try {
        $tools = Invoke-Mcp $liveBase 81 'tools/list' $null
        $toolCount = @($tools.result.tools).Count
    }
    catch {}
    try {
        $proof = Test-McpContract $liveBase
    }
    catch {}
    [pscustomobject]@{
        task_installed = $null -ne $task
        task_state = if ($task) { [string]$task.State } else { 'MISSING' }
        task_action = $taskSnapshot
        binary = $taskBinary
        binary_exists = $null -ne $taskBinary -and (Test-Path -LiteralPath $taskBinary -PathType Leaf)
        binary_sha256 = if ($null -ne $taskBinary -and (Test-Path -LiteralPath $taskBinary -PathType Leaf)) {
            Get-Hash $taskBinary
        } else { $null }
        immutable_deployment = $null -ne $taskBinary -and (Test-PathUnderRoot $taskBinary $deploymentRoot)
        deployment_manifest = if ($null -ne $taskBinary) {
            $manifest = Join-Path (Split-Path -Parent $taskBinary) 'deployment-manifest.json'
            if (Test-Path -LiteralPath $manifest -PathType Leaf) { $manifest } else { $null }
        } else { $null }
        listener_pid = if ($listener) { $listener.OwningProcess } else { $null }
        listener_path = if ($process) { $process.Path } else { $null }
        task_binary_same_file = $null -ne $taskBinary -and
            (Test-Path -LiteralPath $taskBinary -PathType Leaf)
        listener_binary_same_file = if ($process) {
            $null -ne $taskBinary -and (Test-SameFilePath ([string]$process.Path) $taskBinary)
        } else { $false }
        tool_count = $toolCount
        mcp_ready = $null -ne $proof
        mcp_proof = $proof
    }
}

function Stop-LiveTask([string]$ExpectedBinary) {
    Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $deadline = (Get-Date).AddSeconds(10)
    do {
        $processes = @(Get-Process sirin -ErrorAction SilentlyContinue | Where-Object {
            try { Test-SameFilePath $_.Path $ExpectedBinary } catch { $false }
        })
        if ($processes.Count -eq 0) {
            $listener = Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort $LivePort -State Listen `
                -ErrorAction SilentlyContinue | Select-Object -First 1
            if (-not $listener) { return }
            $listenerProcess = Get-Process -Id $listener.OwningProcess -ErrorAction SilentlyContinue
            if ($listenerProcess -and -not (Test-SameFilePath $listenerProcess.Path $ExpectedBinary)) {
                throw "live port is owned by a different executable: $($listenerProcess.Path)"
            }
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw "scheduled task did not stop the exact Sirin binary: $ExpectedBinary"
}

function Start-And-VerifyLive([string]$ExpectedBinary, [string]$ExpectedSha256) {
    Start-ScheduledTask -TaskName $TaskName
    Wait-Mcp $liveBase $TimeoutSeconds | Out-Null
    $proof = Test-McpContract $liveBase
    $taskAction = Get-TaskActionSnapshot
    if (-not (Test-SameFilePath (Get-TaskBinary $taskAction) $ExpectedBinary)) {
        throw "scheduled-task action does not match deployed binary: $($taskAction.execute)"
    }
    $actualHash = Get-Hash $ExpectedBinary
    if ($actualHash -ne $ExpectedSha256) {
        throw "live binary hash mismatch: expected $ExpectedSha256, got $actualHash"
    }
    $listener = Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort $LivePort -State Listen `
        -ErrorAction Stop | Select-Object -First 1
    $process = Get-Process -Id $listener.OwningProcess -ErrorAction Stop
    if (-not (Test-SameFilePath $process.Path $ExpectedBinary)) {
        throw "live listener path mismatch: expected $ExpectedBinary, got $($process.Path)"
    }
    $ui = Invoke-WebRequest -UseBasicParsing -Uri "$liveBase/ui/" -TimeoutSec 15
    if ([int]$ui.StatusCode -ne 200) {
        throw "live UI returned HTTP $($ui.StatusCode)"
    }
    [pscustomobject]@{
        mcp = $proof
        listener_pid = [int]$listener.OwningProcess
        listener_path = [string]$process.Path
        listener_path_matches_task = $true
        binary_sha256 = $actualHash
        binary_sha_matches_candidate = $true
        ui_http_status = [int]$ui.StatusCode
    }
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
    $restoreTarget = Get-TaskBinary $actionSnapshot
    if (-not (Test-PathUnderRoot $restoreTarget $deploymentRoot) -or
        -not [string]::Equals(
            [System.IO.Path]::GetFileName($restoreTarget),
            'sirin.exe',
            [System.StringComparison]::OrdinalIgnoreCase
        )) {
        throw "rollback action must target an immutable Sirin deployment: $restoreTarget"
    }
    $backupHash = Get-Hash $resolvedBackup
    if (Test-Path -LiteralPath $restoreTarget -PathType Leaf) {
        $restoreHash = Get-Hash $restoreTarget
        if ($restoreHash -ne $backupHash) {
            throw "rollback target exists with a different SHA-256: $restoreTarget"
        }
    }
    else {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $restoreTarget) | Out-Null
        Copy-Item -LiteralPath $resolvedBackup -Destination $restoreTarget
    }
    $currentAction = Get-TaskActionSnapshot
    Stop-LiveTask (Get-TaskBinary $currentAction)
    Set-TaskActionFromSnapshot $actionSnapshot
    Start-And-VerifyLive $restoreTarget $backupHash | Out-Null
}

function Write-DeploymentManifest(
    $Stage,
    $Source,
    [string]$CandidateSha256,
    [string]$PreviousBinary,
    [string]$PreviousSha256,
    [string]$Backup,
    [string]$TaskActionBackup,
    $CandidateProof,
    $LiveProof,
    $TaskAction
) {
    $manifestPath = Join-Path ([string]$Stage.directory) 'deployment-manifest.json'
    if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
        $existing = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
        if ([string]$existing.artifact.sha256 -ne $CandidateSha256) {
            throw "immutable deployment manifest does not match candidate SHA-256: $manifestPath"
        }
        return $manifestPath
    }

    $manifest = [ordered]@{
        schemaVersion = 2
        deployedAt = [DateTimeOffset]::Now.ToString('o')
        source = $Source
        artifact = [ordered]@{
            binary = [string]$Stage.binary
            sha256 = $CandidateSha256
            profile = 'release'
            reviewedSha256Required = $true
            immutableDirectory = $true
        }
        scheduledTask = [ordered]@{
            name = $TaskName
            arguments = [string]$TaskAction.arguments
            workingDirectory = [string]$TaskAction.working_directory
        }
        validation = [ordered]@{
            candidateSmoke = $CandidateProof
            live = $LiveProof
            exactToolInventory = $true
            expectedToolCount = 190
        }
        rollback = [ordered]@{
            previousBinary = $PreviousBinary
            previousSha256 = $PreviousSha256
            backupBinary = $Backup
            backupTaskAction = $TaskActionBackup
            automaticOnDeployFailure = $true
        }
    }
    $manifest | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $manifestPath -Encoding UTF8
    $manifestPath
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
$taskAction = Get-TaskActionSnapshot
$liveBinary = Get-TaskBinary $taskAction
if (-not (Test-Path -LiteralPath $liveBinary -PathType Leaf)) {
    throw "scheduled-task Sirin binary is missing: $liveBinary"
}
if (-not [string]::Equals(
    [System.IO.Path]::GetExtension($liveBinary),
    '.exe',
    [System.StringComparison]::OrdinalIgnoreCase
)) {
    throw "scheduled task must execute sirin.exe directly before immutable deployment: $liveBinary"
}
if ($liveBefore.listener_pid -and -not [bool]$liveBefore.listener_binary_same_file) {
    throw "live listener does not match the scheduled-task executable: $($liveBefore.listener_path)"
}

$source = Get-SourceProvenance
$stage = Stage-ImmutableCandidate $CandidateBinary $candidateHash
$newTaskAction = [pscustomobject]@{
    execute = [string]$stage.binary
    arguments = [string]$taskAction.arguments
    working_directory = [string]$stage.directory
}
New-Item -ItemType Directory -Force -Path $backupDir | Out-Null
$liveHash = Get-Hash $liveBinary
$timestamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$backup = Join-Path $backupDir "sirin-$timestamp-$liveHash.exe"
Copy-Item -LiteralPath $liveBinary -Destination $backup
$taskActionBackup = "$backup.task-action.json"
$taskAction | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath $taskActionBackup -Encoding UTF8

try {
    Stop-LiveTask $liveBinary
    Set-TaskActionFromSnapshot $newTaskAction
    $liveProof = Start-And-VerifyLive ([string]$stage.binary) $candidateHash
    $manifest = Write-DeploymentManifest `
        $stage `
        $source `
        $candidateHash `
        $liveBinary `
        $liveHash `
        $backup `
        $taskActionBackup `
        $candidateProof `
        $liveProof `
        $newTaskAction
    [pscustomobject]@{
        status = 'DEPLOYED'
        candidate_sha256 = $candidateHash
        deployment_directory = [string]$stage.directory
        live_binary = [string]$stage.binary
        immutable_binary_reused = [bool]$stage.reused
        deployment_manifest = $manifest
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
