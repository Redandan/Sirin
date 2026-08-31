#requires -Version 5.1
<#
Single Windows entry point for Sirin physical-iPhone support.

Install and Upgrade provision Sirin's private iOS Driver runtime, scheduled
tasks, shared Codex MCP/Skill integration, and optional legacy SideTap task
migration. Status is strictly read-only. Remove unregisters Sirin-owned tasks
but preserves evidence, runtime files, Codex configuration, and rollback data.

No action installs signing assets, changes iPhone trust/unlock state, exposes a
LAN port, enters credentials, jailbreaks, logs in, orders, or pays.
#>

[CmdletBinding()]
param(
    [ValidateSet('Install', 'Upgrade', 'Status', 'Remove', 'InitializeUser', 'RecordCapabilityProof', 'PrepareRebootProof', 'VerifyRebootProof', 'RetireLegacy')]
    [string]$Action = 'Status',
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$Binary = '',
    [string]$CodexHome = (Join-Path $env:USERPROFILE '.codex'),
    [switch]$RunNow,
    [switch]$MigrateLegacySideTap,
    [switch]$SkipCodexIntegration,
    [switch]$EnableStartup
)

$ErrorActionPreference = 'Stop'

$Repo = [System.IO.Path]::GetFullPath($Repo)
if ([string]::IsNullOrWhiteSpace($Binary)) {
    $devBinary = Join-Path $Repo 'target\release\sirin.exe'
    $installedBinary = Join-Path $Repo 'sirin.exe'
    $Binary = if (Test-Path -LiteralPath $devBinary -PathType Leaf) { $devBinary } else { $installedBinary }
}
$Binary = [System.IO.Path]::GetFullPath($Binary)

$runtimeInstaller = Join-Path $Repo 'scripts\install-ios-driver-runtime.ps1'
$driverTaskInstaller = Join-Path $Repo 'scripts\install-ios-driver-task.ps1'
$daemonTaskInstaller = Join-Path $Repo 'scripts\install-sirin-daemon-task.ps1'
$codexInstaller = Join-Path $Repo 'scripts\install-codex-ios-integration.ps1'
$backupRoot = Join-Path $env:LOCALAPPDATA 'Sirin\installer-backups'
$endpoint = 'http://127.0.0.1:7700/mcp'
$daemonTaskName = 'Sirin Local Ops Daemon'
$driverTaskName = 'Sirin iOS Driver'
$legacyTaskNames = @('SideTap-Unattended', 'SideTap-Unattended-Watchdog')
$allTaskNames = @($daemonTaskName, $driverTaskName) + $legacyTaskNames
$startupRegistryPath = 'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run'
$startupRegistryName = 'Sirin'
$userDataRoot = Join-Path $env:LOCALAPPDATA 'Sirin'
$evidenceRoot = Join-Path $userDataRoot 'device_evidence\ios'
$rebootCheckpointPath = Join-Path $evidenceRoot 'sirin-ios-reboot-checkpoint.json'
$capabilityEvidencePattern = 'sirin-ios-capability-proof-*.json'

function Resolve-DefaultFile([string]$RelativePath) {
    $packaged = Join-Path (Join-Path $Repo 'defaults') $RelativePath
    if (Test-Path -LiteralPath $packaged -PathType Leaf) {
        return $packaged
    }
    $development = Join-Path $Repo $RelativePath
    if (Test-Path -LiteralPath $development -PathType Leaf) {
        return $development
    }
    throw "Sirin default file is missing: $RelativePath"
}

function Initialize-SirinUserFiles {
    $configRoot = Join-Path $userDataRoot 'config'
    [void](New-Item -ItemType Directory -Path $configRoot -Force)
    $created = @()
    $preserved = @()
    foreach ($entry in @(
        [pscustomobject]@{ relative = '.env.example'; destination = (Join-Path $userDataRoot '.env.example') },
        [pscustomobject]@{ relative = 'config\agents.yaml'; destination = (Join-Path $configRoot 'agents.yaml') },
        [pscustomobject]@{ relative = 'config\persona.yaml'; destination = (Join-Path $configRoot 'persona.yaml') }
    )) {
        if (Test-Path -LiteralPath $entry.destination -PathType Leaf) {
            $preserved += $entry.destination
            continue
        }
        $source = Resolve-DefaultFile $entry.relative
        Copy-Item -LiteralPath $source -Destination $entry.destination
        $created += $entry.destination
    }

    if ($EnableStartup) {
        if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
            throw "Sirin binary is missing: $Binary"
        }
        New-Item -Path $startupRegistryPath -Force | Out-Null
        Set-ItemProperty -LiteralPath $startupRegistryPath -Name $startupRegistryName -Value ('"' + $Binary + '"')
    }

    [pscustomobject]@{
        status = 'READY'
        action = 'INITIALIZE_USER'
        user_data_root = $userDataRoot
        created = @($created)
        preserved = @($preserved)
        startup_enabled = [bool]$EnableStartup
    }
}

function Get-HashOrNull([string]$Path) {
    if (Test-Path -LiteralPath $Path -PathType Leaf) {
        return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    }
    return $null
}

function Get-SirinWdaBundleSelection {
    $path = Join-Path $userDataRoot 'ios-driver\state\wda_bundle'
    $value = if (Test-Path -LiteralPath $path -PathType Leaf) {
        (Get-Content -LiteralPath $path -Raw).Trim()
    }
    else { '' }
    [pscustomobject]@{
        path = $path
        configured = -not [string]::IsNullOrWhiteSpace($value)
        bundle_id = if ([string]::IsNullOrWhiteSpace($value)) { $null } else { $value }
        source = if ([string]::IsNullOrWhiteSpace($value)) { 'auto-detect' } else { 'sirin-runtime-state' }
    }
}

function Get-LegacySideTapRoots {
    @($legacyTaskNames | ForEach-Object {
        $taskState = Get-TaskState $_
        if ($taskState.installed -and
            -not [string]::IsNullOrWhiteSpace([string]$taskState.working_directory)) {
            try { [System.IO.Path]::GetFullPath([string]$taskState.working_directory) }
            catch {}
        }
    } | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) } | Select-Object -Unique)
}

function Test-PathUnderLegacyRoot([string]$Path, [object[]]$Roots) {
    if ([string]::IsNullOrWhiteSpace($Path)) { return $false }
    try { $fullPath = [System.IO.Path]::GetFullPath($Path) }
    catch { return $false }
    foreach ($root in @($Roots)) {
        $prefix = ([string]$root).TrimEnd('\') + '\'
        if ($fullPath.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
    }
    return $false
}

function Get-LegacySideTapProcesses {
    $roots = @(Get-LegacySideTapRoots)
    if ($roots.Count -eq 0) { return @() }
    @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | ForEach-Object {
        $executable = [string]$_.ExecutablePath
        if (Test-PathUnderLegacyRoot $executable $roots) {
            [pscustomobject]@{
                process_id = [int]$_.ProcessId
                name = [string]$_.Name
                executable = $executable
                command_line = [string]$_.CommandLine
                legacy_root = @($roots | Where-Object {
                    Test-PathUnderLegacyRoot $executable @($_)
                } | Select-Object -First 1)[0]
            }
        }
    })
}

function Stop-LegacySideTapProcesses {
    $roots = @(Get-LegacySideTapRoots)
    $before = @(Get-LegacySideTapProcesses)
    $stopped = @()
    foreach ($candidate in $before) {
        if ($candidate.process_id -eq $PID) { continue }
        $current = Get-CimInstance Win32_Process -Filter "ProcessId=$($candidate.process_id)" -ErrorAction SilentlyContinue
        if ($current -and (Test-PathUnderLegacyRoot ([string]$current.ExecutablePath) $roots)) {
            Stop-Process -Id $candidate.process_id -Force -ErrorAction Stop
            $stopped += $candidate.process_id
        }
    }
    $deadline = (Get-Date).AddSeconds(5)
    do {
        $remaining = @(Get-LegacySideTapProcesses)
        if ($remaining.Count -eq 0) { break }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)
    if ($remaining.Count -gt 0) {
        throw "Legacy SideTap processes remain active after bounded cleanup: $($remaining.process_id -join ', ')"
    }
    [pscustomobject]@{
        status = if ($stopped.Count -gt 0) { 'CLEANED' } else { 'NONE_RUNNING' }
        roots = $roots
        stopped_process_ids = $stopped
        stopped_count = $stopped.Count
        remaining_count = $remaining.Count
        executable_path_verified_before_stop = $true
    }
}

function Get-SirinIosRuntimeProcesses {
    $runtimeExecutable = Join-Path $userDataRoot 'ios-driver\bin\ios.exe'
    if (-not (Test-Path -LiteralPath $runtimeExecutable -PathType Leaf)) { return @() }
    $expected = [System.IO.Path]::GetFullPath($runtimeExecutable)
    @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | ForEach-Object {
        $executable = [string]$_.ExecutablePath
        if (-not [string]::IsNullOrWhiteSpace($executable)) {
            try { $fullExecutable = [System.IO.Path]::GetFullPath($executable) }
            catch { $fullExecutable = '' }
            if ([string]::Equals($fullExecutable, $expected, [System.StringComparison]::OrdinalIgnoreCase)) {
                [pscustomobject]@{
                    process_id = [int]$_.ProcessId
                    name = [string]$_.Name
                    executable = $fullExecutable
                    command_line = [string]$_.CommandLine
                }
            }
        }
    })
}

function Get-ForeignIosControlProcesses {
    $runtimeExecutable = Join-Path $userDataRoot 'ios-driver\bin\ios.exe'
    $expected = if (Test-Path -LiteralPath $runtimeExecutable -PathType Leaf) {
        [System.IO.Path]::GetFullPath($runtimeExecutable)
    }
    else { '' }
    @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | ForEach-Object {
        if ([string]$_.Name -notmatch '^(?i)ios\.exe$' -or
            [string]$_.CommandLine -notmatch '(?i)(?:\btunnel\s+start\b|\brunwda\b|\bforward\s+\d+\b|\bsyslog\b)') {
            return
        }
        $executable = [string]$_.ExecutablePath
        try { $fullExecutable = [System.IO.Path]::GetFullPath($executable) }
        catch { $fullExecutable = '' }
        if ([string]::IsNullOrWhiteSpace($fullExecutable) -or
            -not [string]::Equals($fullExecutable, $expected, [System.StringComparison]::OrdinalIgnoreCase)) {
            [pscustomobject]@{
                process_id = [int]$_.ProcessId
                name = [string]$_.Name
                executable = $executable
                command_line = [string]$_.CommandLine
            }
        }
    })
}

function Stop-SirinIosRuntimeProcesses {
    $before = @(Get-SirinIosRuntimeProcesses)
    $stopped = @()
    $runtimeExecutable = Join-Path $userDataRoot 'ios-driver\bin\ios.exe'
    $expected = if (Test-Path -LiteralPath $runtimeExecutable -PathType Leaf) {
        [System.IO.Path]::GetFullPath($runtimeExecutable)
    }
    else { '' }
    foreach ($candidate in $before) {
        $current = Get-CimInstance Win32_Process -Filter "ProcessId=$($candidate.process_id)" -ErrorAction SilentlyContinue
        if (-not $current) { continue }
        try { $currentExecutable = [System.IO.Path]::GetFullPath([string]$current.ExecutablePath) }
        catch { $currentExecutable = '' }
        if (-not [string]::IsNullOrWhiteSpace($expected) -and
            [string]::Equals($currentExecutable, $expected, [System.StringComparison]::OrdinalIgnoreCase)) {
            Stop-Process -Id $candidate.process_id -Force -ErrorAction Stop
            $stopped += $candidate.process_id
        }
    }
    $deadline = (Get-Date).AddSeconds(5)
    do {
        $remaining = @(Get-SirinIosRuntimeProcesses)
        if ($remaining.Count -eq 0) { break }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)
    if ($remaining.Count -gt 0) {
        throw "Sirin iOS runtime processes remain active after bounded cleanup: $($remaining.process_id -join ', ')"
    }
    [pscustomobject]@{
        status = if ($stopped.Count -gt 0) { 'CLEANED' } else { 'NONE_RUNNING' }
        executable = $expected
        stopped_process_ids = $stopped
        stopped_count = $stopped.Count
        remaining_count = $remaining.Count
        executable_path_verified_before_stop = $true
    }
}

function Import-LegacyWdaBundleSelection {
    $current = Get-SirinWdaBundleSelection
    if ($current.configured) {
        return [pscustomobject]@{
            status = 'PRESERVED_SIRIN_STATE'
            source = $current.path
            destination = $current.path
            bundle_id = $current.bundle_id
        }
    }
    $candidateRoots = @(Get-LegacySideTapRoots)
    foreach ($root in $candidateRoots) {
        $legacyEnv = Join-Path $root '.env'
        if (-not (Test-Path -LiteralPath $legacyEnv -PathType Leaf)) {
            continue
        }
        $line = Select-String -LiteralPath $legacyEnv -Pattern '^\s*WDA_BUNDLE_ID\s*=\s*([^#\s]+)\s*$' |
            Select-Object -First 1
        if (-not $line) {
            continue
        }
        $bundleId = (($line.Line -split '=', 2)[1]).Trim().Trim('"').Trim("'")
        if ($bundleId -notmatch '^[A-Za-z0-9.-]{1,255}$' -or
            ($bundleId -notmatch '(?i)webdriveragent' -and $bundleId -notmatch '(?i)\.xctrunner(?:\.|$)')) {
            throw 'Legacy WDA_BUNDLE_ID is not a safe WebDriverAgent bundle identifier.'
        }
        $stateDir = Join-Path $userDataRoot 'ios-driver\state'
        [void](New-Item -ItemType Directory -Path $stateDir -Force)
        $selectionPath = Join-Path $stateDir 'wda_bundle'
        Set-Content -LiteralPath $selectionPath -Value $bundleId -Encoding ascii -NoNewline
        return [pscustomobject]@{
            status = 'IMPORTED'
            source = $legacyEnv
            destination = $selectionPath
            bundle_id = $bundleId
        }
    }
    [pscustomobject]@{
        status = 'NOT_FOUND'
        source = $null
        destination = (Join-Path $userDataRoot 'ios-driver\state\wda_bundle')
        bundle_id = $null
    }
}

function Get-TaskState([string]$Name) {
    $task = Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue
    if (-not $task) {
        return [pscustomobject]@{
            name = $Name
            installed = $false
            state = 'MISSING'
            enabled = $false
            execute = $null
            arguments = $null
            working_directory = $null
            execution_time_limit = $null
            restart_count = 0
            restart_interval = $null
            start_when_available = $false
            logon_type = $null
            has_logon_trigger = $false
            has_repeating_trigger = $false
        }
    }
    $actionDef = $task.Actions | Select-Object -First 1
    [pscustomobject]@{
        name = $Name
        installed = $true
        state = [string]$task.State
        enabled = [bool]$task.Settings.Enabled
        execute = if ($actionDef) { [string]$actionDef.Execute } else { $null }
        arguments = if ($actionDef) { [string]$actionDef.Arguments } else { $null }
        working_directory = if ($actionDef) { [string]$actionDef.WorkingDirectory } else { $null }
        execution_time_limit = [string]$task.Settings.ExecutionTimeLimit
        restart_count = [int]$task.Settings.RestartCount
        restart_interval = [string]$task.Settings.RestartInterval
        start_when_available = [bool]$task.Settings.StartWhenAvailable
        logon_type = [string]$task.Principal.LogonType
        has_logon_trigger = @($task.Triggers | Where-Object { $_.CimClass.CimClassName -eq 'MSFT_TaskLogonTrigger' }).Count -gt 0
        has_repeating_trigger = @($task.Triggers | Where-Object {
            -not [string]::IsNullOrWhiteSpace([string]$_.Repetition.Interval)
        }).Count -gt 0
    }
}

function Get-LoopbackPortOwner([int]$Port) {
    try {
        $connection = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction Stop |
            Select-Object -First 1
        if (-not $connection) {
            return $null
        }
        $process = Get-CimInstance Win32_Process -Filter "ProcessId=$($connection.OwningProcess)" -ErrorAction Stop
        [pscustomobject]@{
            port = $Port
            pid = [int]$connection.OwningProcess
            executable = [string]$process.ExecutablePath
            command_line = [string]$process.CommandLine
        }
    }
    catch {
        $null
    }
}

function Invoke-SirinMcp([string]$Method, $Params, [int]$Id) {
    $body = @{
        jsonrpc = '2.0'
        id = $Id
        method = $Method
        params = $Params
    } | ConvertTo-Json -Depth 12
    Invoke-RestMethod -Uri $endpoint -Method Post -ContentType 'application/json' -Body $body -TimeoutSec 10
}

function Get-LiveIosStatus {
    try {
        $tools = (Invoke-SirinMcp -Method 'tools/list' -Params @{} -Id 1).result.tools
        $physicalTools = @(
            'ios_device_status',
            'ios_screen_capture',
            'ios_control_session_start',
            'ios_control_session_status',
            'ios_control_session_stop',
            'ios_swipe',
            'ios_home',
            'ios_recover_home',
            'ios_open_app',
            'ios_open_route',
            'ios_acceptance_run',
            'ios_tap'
        )
        $toolNames = @($tools | ForEach-Object { [string]$_.name })
        $missingTools = @($physicalTools | Where-Object { $_ -notin $toolNames })
        $statusEnvelope = Invoke-SirinMcp -Method 'tools/call' -Params @{
            name = 'ios_device_status'
            arguments = @{}
        } -Id 2
        $status = $statusEnvelope.result.content[0].text | ConvertFrom-Json
        [pscustomobject]@{
            reachable = $true
            tool_count = $toolNames.Count
            required_physical_ios_tool_count = $physicalTools.Count
            missing_physical_ios_tools = $missingTools
            status = $status
        }
    }
    catch {
        [pscustomobject]@{
            reachable = $false
            tool_count = 0
            required_physical_ios_tool_count = 12
            missing_physical_ios_tools = @()
            status = $null
            error = $_.Exception.Message
        }
    }
}

function Request-IosLinkReadiness {
    try {
        $started = Invoke-SirinMcp -Method 'tools/call' -Params @{
            name = 'ios_control_session_start'
            arguments = @{
                owner = 'sirin-installer-readiness'
                policy = 'acceptance_browse_only'
                ttl_secs = 60
            }
        } -Id 3
        if ($null -ne $started.error) {
            return [pscustomobject]@{
                status = 'NOT_READY'
                error = [string]$started.error.message
            }
        }
        $text = [string]$started.result.content[0].text
        $lease = $text | ConvertFrom-Json
        if ([string]::IsNullOrWhiteSpace([string]$lease.session_id)) {
            return [pscustomobject]@{
                status = 'NOT_READY'
                error = 'Sirin did not return an iPhone control lease.'
            }
        }
        $stopped = Invoke-SirinMcp -Method 'tools/call' -Params @{
            name = 'ios_control_session_stop'
            arguments = @{
                session_id = [string]$lease.session_id
            }
        } -Id 4
        [pscustomobject]@{
            status = if ($null -eq $stopped.error) { 'READY' } else { 'RELEASE_FAILED' }
            session_released = $null -eq $stopped.error
            error = if ($null -ne $stopped.error) { [string]$stopped.error.message } else { $null }
        }
    }
    catch {
        [pscustomobject]@{
            status = 'NOT_READY'
            error = $_.Exception.Message
        }
    }
}

function Test-IosSoakContinuity($Summary, [string]$SummaryPath) {
    $reasons = @()
    $expectedJsonl = if ($SummaryPath.EndsWith('.summary.json', [System.StringComparison]::OrdinalIgnoreCase)) {
        $SummaryPath.Substring(0, $SummaryPath.Length - '.summary.json'.Length) + '.jsonl'
    }
    else {
        [System.IO.Path]::ChangeExtension($SummaryPath, '.jsonl')
    }
    $declaredJsonl = if ([string]::IsNullOrWhiteSpace([string]$Summary.evidence_jsonl)) {
        $expectedJsonl
    }
    else {
        [System.IO.Path]::GetFullPath([string]$Summary.evidence_jsonl)
    }
    $evidenceRootFull = [System.IO.Path]::GetFullPath($evidenceRoot).TrimEnd('\') + '\'
    if (-not $declaredJsonl.StartsWith($evidenceRootFull, [System.StringComparison]::OrdinalIgnoreCase)) {
        $reasons += 'JSONL path is outside the Sirin evidence directory.'
    }
    if (-not [string]::Equals($declaredJsonl, $expectedJsonl, [System.StringComparison]::OrdinalIgnoreCase)) {
        $reasons += 'Summary and JSONL base names do not match.'
    }
    if (-not (Test-Path -LiteralPath $declaredJsonl -PathType Leaf)) {
        $reasons += 'JSONL evidence file is missing.'
        return [pscustomobject]@{
            pass = $false
            reasons = $reasons
            evidence_jsonl = $declaredJsonl
        }
    }

    $rows = @()
    try {
        $lineNumber = 0
        foreach ($line in Get-Content -LiteralPath $declaredJsonl) {
            $lineNumber++
            if ([string]::IsNullOrWhiteSpace($line)) {
                $reasons += "JSONL line $lineNumber is blank."
                continue
            }
            $rows += ($line | ConvertFrom-Json)
        }
    }
    catch {
        $reasons += "JSONL parsing failed: $($_.Exception.Message)"
    }

    $intervalSeconds = [int]$Summary.interval_seconds
    if ($intervalSeconds -lt 5 -or $intervalSeconds -gt 60) {
        $reasons += 'Summary interval_seconds is outside 5..60.'
        $intervalSeconds = 60
    }
    $requiredDurationSeconds = [math]::Max(28800, [int]$Summary.requested_duration_minutes * 60)
    $maxAllowedGapSeconds = [math]::Max(180, $intervalSeconds * 3)
    $minimumSamples = [math]::Floor($requiredDurationSeconds / ($intervalSeconds * 1.5)) + 1
    $timestamps = @()
    for ($index = 0; $index -lt $rows.Count; $index++) {
        $row = $rows[$index]
        if ([int]$row.sample -ne ($index + 1)) {
            $reasons += "Sample sequence is not continuous at row $($index + 1)."
            break
        }
        if ($row.pass -ne $true) {
            $reasons += "Sample $($index + 1) did not pass."
            break
        }
        try {
            $timestamps += ([datetime]$row.checked_at).ToUniversalTime()
        }
        catch {
            $reasons += "Sample $($index + 1) has an invalid checked_at timestamp."
            break
        }
    }

    $maxObservedGapSeconds = 0.0
    for ($index = 1; $index -lt $timestamps.Count; $index++) {
        $gap = ($timestamps[$index] - $timestamps[$index - 1]).TotalSeconds
        $maxObservedGapSeconds = [math]::Max($maxObservedGapSeconds, $gap)
    }
    $observedDurationSeconds = if ($timestamps.Count -ge 2) {
        ($timestamps[-1] - $timestamps[0]).TotalSeconds
    }
    else {
        0.0
    }
    if ($rows.Count -lt $minimumSamples) {
        $reasons += "Only $($rows.Count) samples exist; at least $minimumSamples are required."
    }
    if ($observedDurationSeconds -lt $requiredDurationSeconds) {
        $reasons += "Observed sample span is only $([math]::Round($observedDurationSeconds, 2)) seconds."
    }
    if ($maxObservedGapSeconds -gt $maxAllowedGapSeconds) {
        $reasons += "Maximum sample gap is $([math]::Round($maxObservedGapSeconds, 3)) seconds; limit is $maxAllowedGapSeconds."
    }
    if ([int]$Summary.samples -ne $rows.Count) {
        $reasons += 'Summary sample count does not match JSONL.'
    }

    [pscustomobject]@{
        pass = $reasons.Count -eq 0
        reasons = $reasons
        evidence_jsonl = $declaredJsonl
        evidence_jsonl_sha256 = Get-HashOrNull $declaredJsonl
        samples = $rows.Count
        minimum_samples = $minimumSamples
        observed_duration_seconds = [math]::Round($observedDurationSeconds, 2)
        max_allowed_gap_seconds = $maxAllowedGapSeconds
        max_observed_gap_seconds = [math]::Round($maxObservedGapSeconds, 3)
    }
}

function Get-IosSoakEvidence {
    $qualified = $null
    $latestRejected = $null
    if (Test-Path -LiteralPath $evidenceRoot -PathType Container) {
        foreach ($file in @(Get-ChildItem -LiteralPath $evidenceRoot -Filter 'sirin-ios-soak-*.summary.json' -File |
            Sort-Object LastWriteTime -Descending)) {
            try {
                $candidate = Get-Content -LiteralPath $file.FullName -Raw | ConvertFrom-Json
                $continuity = Test-IosSoakContinuity $candidate $file.FullName
                if (
                    $candidate.status -eq 'PASS' -and
                    [int]$candidate.requested_duration_minutes -ge 480 -and
                    [double]$candidate.actual_duration_seconds -ge 28800 -and
                    [int]$candidate.failures -eq 0 -and
                    $candidate.contract.pass -eq $true -and
                    $continuity.pass -eq $true
                ) {
                    $qualified = [pscustomobject]@{
                        path = $file.FullName
                        sha256 = Get-HashOrNull $file.FullName
                        summary = $candidate
                        continuity = $continuity
                    }
                    break
                }
                if ($null -eq $latestRejected) {
                    $latestRejected = [pscustomobject]@{
                        path = $file.FullName
                        sha256 = Get-HashOrNull $file.FullName
                        summary_status = $candidate.status
                        continuity = $continuity
                    }
                }
            }
            catch {
                if ($null -eq $latestRejected) {
                    $latestRejected = [pscustomobject]@{
                        path = $file.FullName
                        error = $_.Exception.Message
                    }
                }
                continue
            }
        }
    }

    $latestIncomplete = if (Test-Path -LiteralPath $evidenceRoot -PathType Container) {
        Get-ChildItem -LiteralPath $evidenceRoot -Filter 'sirin-ios-soak-*.jsonl' -File |
            Where-Object { -not (Test-Path -LiteralPath ([System.IO.Path]::ChangeExtension($_.FullName, '.summary.json')) -PathType Leaf) } |
            Sort-Object LastWriteTime -Descending |
            Select-Object -First 1
    }
    else {
        $null
    }

    [pscustomobject]@{
        status = if ($qualified) { 'PASS' } else { 'MISSING_PROOF' }
        qualified_summary = $qualified
        latest_rejected_summary = $latestRejected
        latest_incomplete_jsonl = if ($latestIncomplete) { $latestIncomplete.FullName } else { $null }
        required_duration_minutes = 480
        read_only_sampling = $true
        continuity_revalidated_from_jsonl = $true
    }
}

function Test-CaptureEvidence($Evidence) {
    if ($null -eq $Evidence) {
        return $false
    }
    $path = [string]$Evidence.path
    $expectedHash = ([string]$Evidence.sha256).ToLowerInvariant()
    if (
        [string]::IsNullOrWhiteSpace($path) -or
        [string]::IsNullOrWhiteSpace($expectedHash) -or
        -not (Test-Path -LiteralPath $path -PathType Leaf) -or
        [int]$Evidence.width -lt 100 -or
        [int]$Evidence.height -lt 100 -or
        [int64]$Evidence.size_bytes -le 0
    ) {
        return $false
    }
    (Get-HashOrNull $path) -eq $expectedHash
}

function Get-IosCapabilityEvidence([string]$CurrentSourceHash) {
    if (-not (Test-Path -LiteralPath $evidenceRoot -PathType Container)) {
        return [pscustomobject]@{ status = 'MISSING_PROOF'; evidence = $null }
    }
    foreach ($file in @(Get-ChildItem -LiteralPath $evidenceRoot -Filter $capabilityEvidencePattern -File |
        Sort-Object LastWriteTime -Descending)) {
        try {
            $proof = Get-Content -LiteralPath $file.FullName -Raw | ConvertFrom-Json
            $captureValid = Test-CaptureEvidence $proof.screen_capture
            $controlValid = (
                $proof.screen_control.status -eq 'PASS' -and
                $proof.screen_control.action -eq 'swipe' -and
                $proof.screen_control.changed -eq $true -and
                $proof.screen_control.visual_change.material_content_change -eq $true -and
                $proof.screen_control.visual_change.vertical_scroll_proven -eq $true -and
                [math]::Abs([int]$proof.screen_control.visual_change.best_vertical_shift_pixels) -gt 0 -and
                -not [string]::IsNullOrWhiteSpace([string]$proof.screen_control.before_frame_id) -and
                $proof.screen_control.after_frame_id -eq $proof.screen_capture.frame_id -and
                $proof.screen_control.before_frame_id -ne $proof.screen_control.after_frame_id
            )
            $valid = (
                $proof.status -eq 'PASS' -and
                $proof.provider -eq 'sirin-ios-driver' -and
                $proof.acceptance_only -eq $true -and
                $proof.lan_exposed -eq $false -and
                $proof.active_lease_after -eq $null -and
                $proof.binary_sha256 -eq (Get-HashOrNull $Binary) -and
                $proof.driver_source_sha256 -eq $CurrentSourceHash -and
                $captureValid -and
                $controlValid
            )
            if ($valid) {
                return [pscustomobject]@{
                    status = 'PASS'
                    evidence = $file.FullName
                    sha256 = Get-HashOrNull $file.FullName
                    binary_matches_current = $true
                    driver_source_matches_current = $true
                    screen_capture_file_valid = $true
                    proof = $proof
                }
            }
        }
        catch {
            continue
        }
    }
    [pscustomobject]@{ status = 'MISSING_PROOF'; evidence = $null }
}

function Get-SystemBootTime {
    $os = Get-CimInstance Win32_OperatingSystem
    ([datetime]$os.LastBootUpTime).ToUniversalTime()
}

function Get-RebootEvidence {
    $proofFile = if (Test-Path -LiteralPath $evidenceRoot -PathType Container) {
        Get-ChildItem -LiteralPath $evidenceRoot -Filter 'sirin-ios-reboot-proof-*.json' -File |
            Sort-Object LastWriteTime -Descending |
            Select-Object -First 1
    }
    else {
        $null
    }
    if (-not $proofFile) {
        return [pscustomobject]@{
            status = 'MISSING_PROOF'
            evidence = $null
            checkpoint = if (Test-Path -LiteralPath $rebootCheckpointPath -PathType Leaf) { $rebootCheckpointPath } else { $null }
        }
    }

    try {
        $proof = Get-Content -LiteralPath $proofFile.FullName -Raw | ConvertFrom-Json
        $currentBoot = Get-SystemBootTime
        $proofBoot = ([datetime]$proof.boot_at).ToUniversalTime()
        $bootMatches = [math]::Abs(($currentBoot - $proofBoot).TotalSeconds) -le 5
        $binaryMatches = $proof.binary_sha256 -eq (Get-HashOrNull $Binary)
        $valid = $proof.status -eq 'PASS' -and $bootMatches -and $binaryMatches
        [pscustomobject]@{
            status = if ($valid) { 'PASS' } else { 'MISSING_PROOF' }
            evidence = $proofFile.FullName
            sha256 = Get-HashOrNull $proofFile.FullName
            boot_matches_current = $bootMatches
            binary_matches_current = $binaryMatches
            proof = $proof
        }
    }
    catch {
        [pscustomobject]@{
            status = 'MISSING_PROOF'
            evidence = $proofFile.FullName
            error = $_.Exception.Message
        }
    }
}

function Get-UnifiedStatus {
    $runtime = try {
        & $runtimeInstaller -Action Status -Repo $Repo | ConvertFrom-Json
    }
    catch {
        [pscustomobject]@{
            source_ready = $false
            python_ready = $false
            go_ios_ready = $false
            wda_unsigned_ipa_ready = $false
            wda_license_ready = $false
            error = $_.Exception.Message
        }
    }
    $runtime | Add-Member -NotePropertyName wda_bundle_selection -NotePropertyValue (Get-SirinWdaBundleSelection) -Force
    $driver = try {
        & $driverTaskInstaller -Action Status -Repo $Repo | ConvertFrom-Json
    }
    catch {
        [pscustomobject]@{ installed = $false; action_matches = $false; error = $_.Exception.Message }
    }
    $codex = if ($SkipCodexIntegration) {
        [pscustomobject]@{ status = 'SKIPPED' }
    }
    else {
        try {
            & $codexInstaller -Action Status -Repo $Repo -CodexHome $CodexHome -ManageProjectConfig:$false | ConvertFrom-Json
        }
        catch {
            [pscustomobject]@{ status = 'ERROR'; error = $_.Exception.Message }
        }
    }

    $daemonTask = Get-TaskState $daemonTaskName
    $driverTask = Get-TaskState $driverTaskName
    $legacyTasks = @($legacyTaskNames | ForEach-Object { Get-TaskState $_ })
    $legacyLiveProcesses = @(Get-LegacySideTapProcesses)
    $sirinIosRuntimeProcesses = @(Get-SirinIosRuntimeProcesses)
    $foreignIosControlProcesses = @(Get-ForeignIosControlProcesses)
    $startupRunValue = (Get-ItemProperty -LiteralPath $startupRegistryPath -Name $startupRegistryName -ErrorAction SilentlyContinue).Sirin
    $live = Get-LiveIosStatus
    $soak = Get-IosSoakEvidence
    $reboot = Get-RebootEvidence
    $capability = Get-IosCapabilityEvidence ([string]$runtime.source_sha256)
    $daemonOwner = Get-LoopbackPortOwner 7700
    $driverOwner = Get-LoopbackPortOwner 8770
    $daemonActionMatches = (
        $daemonTask.installed -and
        [string]::Equals($daemonTask.execute, $Binary, [System.StringComparison]::OrdinalIgnoreCase) -and
        $daemonTask.arguments -match '(?:^|\s)--headless(?:\s|$)' -and
        $daemonTask.arguments -match '(?:^|\s)--ios-driver-autostart(?:\s|$)'
    )
    $daemonDurable = (
        $daemonTask.execution_time_limit -eq 'PT0S' -and
        $daemonTask.restart_count -ge 3 -and
        $daemonTask.start_when_available -and
        $daemonTask.logon_type -eq 'Interactive' -and
        $daemonTask.has_logon_trigger -and
        $daemonTask.has_repeating_trigger
    )
    $driverDurable = (
        $driverTask.execution_time_limit -eq 'PT0S' -and
        $driverTask.restart_count -ge 3 -and
        $driverTask.start_when_available -and
        $driverTask.logon_type -eq 'Interactive' -and
        -not $driverTask.has_logon_trigger -and
        -not $driverTask.has_repeating_trigger
    )
    $daemonProcessMatchesBinary = (
        $null -ne $daemonOwner -and
        [string]::Equals($daemonOwner.executable, $Binary, [System.StringComparison]::OrdinalIgnoreCase)
    )
    $driverProcessMatchesSource = (
        $null -ne $driverOwner -and
        $driverOwner.command_line -match [regex]::Escape([string]$driver.provider_root) -and
        $driverOwner.command_line -match [regex]::Escape([string]$driver.runtime_root)
    )
    $legacySafe = @($legacyTasks | Where-Object enabled -eq $true).Count -eq 0
    $codexReady = $SkipCodexIntegration -or $codex.status -eq 'READY'
    $activeDependencyText = @(
        if ($daemonTask.enabled) { $daemonTask.execute; $daemonTask.arguments; $daemonTask.working_directory }
        if ($driverTask.enabled) { $driverTask.execute; $driverTask.arguments; $driverTask.working_directory }
        $runtime.source_root
    ) -join "`n"
    $activeDependenciesUseSideTap = (
        $activeDependencyText -match '(?i)\\SideTap(?:\\|$)' -or
        $legacyLiveProcesses.Count -gt 0
    )
    $liveDeviceDetected = $live.status.capabilities.DEVICE_DETECTED.status -eq 'PASS'
    $liveInfoReadable = $live.status.capabilities.INFO_READABLE.status -eq 'PASS'
    $liveInputReady = $live.status.provider.input_reported -eq $true
    $liveWdaLinkUp = $live.status.provider.wda_link_reported -eq 'up'
    $liveWdaNotStarting = $live.status.provider.starting -ne $true
    $passiveLanReady = @(
        'forwards_inactive_firewall_ready',
        'firewall_blocked_lan_listener',
        'loopback_listener'
    ) -contains [string]$live.status.provider.lan_protection
    $humanBlockers = @($live.status.human_blockers | Where-Object {
        -not [string]::IsNullOrWhiteSpace([string]$_)
    })
    $wdaOperatorHandoffReady = (
        $runtime.wda_unsigned_ipa_ready -eq $true -and
        $runtime.wda_license_ready -eq $true
    )
    $liveReady = (
        $live.reachable -and
        @($live.missing_physical_ios_tools).Count -eq 0 -and
        $live.status.provider.provider -eq 'sirin-ios-driver' -and
        $live.status.provider.acceptance_only -eq $true -and
        $live.status.provider.lan_exposed -eq $false -and
        $liveDeviceDetected -and
        $liveInfoReadable -and
        $liveInputReady -and
        $liveWdaLinkUp -and
        $liveWdaNotStarting
    )
    $passiveReady = (
        $live.reachable -and
        @($live.missing_physical_ios_tools).Count -eq 0 -and
        $live.status.provider.provider -eq 'sirin-ios-driver' -and
        $live.status.provider.provider_reachable -eq $true -and
        $live.status.provider.acceptance_only -eq $true -and
        $live.status.provider.attach_mode -eq 'passive' -and
        $live.status.provider.lan_exposed -eq $false -and
        $passiveLanReady -and
        $live.status.provider.device_probe_status -ne 'failed' -and
        $liveWdaNotStarting
    )
    $controlPlaneReady = (
        (Test-Path -LiteralPath $Binary -PathType Leaf) -and
        $runtime.source_ready -and
        $runtime.python_ready -and
        $runtime.go_ios_ready -and
        $wdaOperatorHandoffReady -and
        $driver.installed -and
        $driver.action_matches -and
        $daemonActionMatches -and
        $daemonDurable -and
        $driverDurable -and
        $daemonProcessMatchesBinary -and
        $driverProcessMatchesSource -and
        -not $activeDependenciesUseSideTap -and
        $foreignIosControlProcesses.Count -eq 0 -and
        [string]::IsNullOrWhiteSpace([string]$startupRunValue) -and
        $legacySafe -and
        $codexReady
    )
    $ready = $controlPlaneReady -and $passiveReady
    $nextSafeAction = if ($foreignIosControlProcesses.Count -gt 0) {
        'REPAIR_FOREIGN_IOS_CONTROL_PROCESS_DEPENDENCY'
    }
    elseif ($activeDependenciesUseSideTap) {
        'REPAIR_ACTIVE_SIDETAP_PROCESS_DEPENDENCY'
    }
    elseif (-not $daemonDurable -or -not $driverDurable) {
        'RUN_INSTALL_OR_UPGRADE_FOR_DURABLE_TASKS'
    }
    elseif (-not $wdaOperatorHandoffReady) {
        'RUN_INSTALL_OR_UPGRADE_FOR_VERIFIED_WDA_OPERATOR_ASSET'
    }
    elseif (@($humanBlockers | Where-Object { $_ -like 'SECURITY_BLOCK_*' }).Count -gt 0) {
        'REVIEW_EXISTING_LAN_PROTECTION_OUTSIDE_SIRIN'
    }
    elseif (-not $ready) {
        'REPAIR_SIRIN_PASSIVE_CONTROL_PLANE'
    }
    elseif ($humanBlockers -contains 'NEEDS_HUMAN_WDA_INSTALL') {
        'USER_SIGN_INSTALL_WDA_FROM_SIRIN_OPERATOR_ASSET'
    }
    elseif ($humanBlockers -contains 'NEEDS_HUMAN_USB') {
        'USER_CONNECT_IPHONE_BY_USB'
    }
    elseif ($humanBlockers -contains 'NEEDS_HUMAN_WAKE_UNLOCK') {
        'USER_WAKE_AND_UNLOCK_IPHONE'
    }
    elseif ($humanBlockers -contains 'NEEDS_HUMAN_DDI') {
        'USER_PREPARE_DEVELOPER_DISK_IMAGE_EXPLICITLY'
    }
    elseif (-not $liveReady) {
        'RUN_IOS_ACCEPTANCE_ON_DEMAND'
    }
    elseif ($capability.status -ne 'PASS') {
        'RUN_ONE_SAFE_SIRIN_VERTICAL_SWIPE_RELEASE_LEASE_THEN_RECORD_CAPABILITY_PROOF'
    }
    elseif ($soak.status -ne 'PASS') {
        'WAIT_FOR_QUALIFYING_8H_SOAK'
    }
    elseif ($reboot.status -ne 'PASS' -and -not [string]::IsNullOrWhiteSpace([string]$reboot.checkpoint)) {
        'WAIT_FOR_EXPLICIT_USER_REBOOT_THEN_VERIFY_REBOOT_PROOF'
    }
    elseif ($reboot.status -ne 'PASS') {
        'PREPARE_REBOOT_PROOF'
    }
    elseif (@($legacyTasks | Where-Object installed -eq $true).Count -gt 0) {
        'RETIRE_DISABLED_LEGACY_TASKS'
    }
    else {
        'REVIEW_DIRTY_SIDETAP_CHECKOUT_DISPOSITION_WITH_USER'
    }

    [pscustomobject]@{
        status = if ($ready) { 'READY' } else { 'NEEDS_ATTENTION' }
        owner = 'sirin'
        repo = $Repo
        binary = $Binary
        binary_sha256 = Get-HashOrNull $Binary
        public_entry = $endpoint
        runtime = $runtime
        driver = $driver
        codex = $codex
        tasks = [pscustomobject]@{
            daemon = $daemonTask
            driver = $driverTask
            legacy = $legacyTasks
        }
        gates = [pscustomobject]@{
            daemon_action_matches = $daemonActionMatches
            daemon_task_durable = $daemonDurable
            driver_task_durable = $driverDurable
            daemon_process_matches_binary = $daemonProcessMatchesBinary
            driver_process_matches_source = $driverProcessMatchesSource
            wda_operator_handoff_ready = $wdaOperatorHandoffReady
            duplicate_startup_registry_absent = [string]::IsNullOrWhiteSpace([string]$startupRunValue)
            legacy_tasks_disabled_or_missing = $legacySafe
            codex_ready = $codexReady
            control_plane_ready = $controlPlaneReady
            live_device_detected = $liveDeviceDetected
            live_info_readable = $liveInfoReadable
            live_input_ready = $liveInputReady
            live_wda_link_up = $liveWdaLinkUp
            live_wda_not_starting = $liveWdaNotStarting
            passive_ready = $passiveReady
            live_ready = $liveReady
        }
        migration = [pscustomobject]@{
            unattended_mode = 'logged_in_user_session'
            cold_boot_without_user_logon_supported = $false
            active_dependencies_use_sidetap = $activeDependenciesUseSideTap
            legacy_live_processes = $legacyLiveProcesses
            foreign_ios_control_processes = $foreignIosControlProcesses
            capability_proof = $capability
            soak_8h = $soak
            next_safe_action = $nextSafeAction
            legacy_retirement_ready = (
                -not $activeDependenciesUseSideTap -and
                $legacySafe -and
                $capability.status -eq 'PASS' -and
                $soak.status -eq 'PASS' -and
                $reboot.status -eq 'PASS'
            )
            reboot_recovery = $reboot
        }
        live = $live
        processes = [pscustomobject]@{
            daemon = $daemonOwner
            driver = $driverOwner
            legacy_sidetap = $legacyLiveProcesses
            sirin_ios_runtime = $sirinIosRuntimeProcesses
            foreign_ios_control = $foreignIosControlProcesses
        }
        safety = [pscustomobject]@{
            local_only_required = $true
            credentials_supported = $false
            trust_or_unlock_supported = $false
            signing_supported = $false
            jailbreak_supported = $false
            commerce_actions_supported = $false
        }
    }
}

function Save-TaskBackup {
    [void](New-Item -ItemType Directory -Path $backupRoot -Force)
    $directory = Join-Path $backupRoot (Get-Date -Format 'yyyyMMdd-HHmmss')
    [void](New-Item -ItemType Directory -Path $directory -Force)
    $manifest = foreach ($name in $allTaskNames) {
        $task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
        $safeName = $name -replace '[^A-Za-z0-9_.-]', '_'
        $xmlPath = Join-Path $directory "$safeName.xml"
        if ($task) {
            Export-ScheduledTask -TaskName $name | Set-Content -LiteralPath $xmlPath -Encoding Unicode
        }
        [pscustomobject]@{
            name = $name
            existed = $null -ne $task
            was_running = $null -ne $task -and [string]$task.State -eq 'Running'
            xml = if ($task) { $xmlPath } else { $null }
        }
    }
    $startupValue = (Get-ItemProperty -LiteralPath $startupRegistryPath -Name $startupRegistryName -ErrorAction SilentlyContinue).Sirin
    $backupState = [pscustomobject]@{
        directory = $directory
        tasks = @($manifest)
        startup_registry_existed = -not [string]::IsNullOrWhiteSpace([string]$startupValue)
        startup_registry_value = $startupValue
    }
    $manifestPath = Join-Path $directory 'task-manifest.json'
    $backupState | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath $manifestPath -Encoding utf8
    $backupState
}

function Restore-TaskBackup($Backup) {
    foreach ($entry in $Backup.tasks) {
        if (Get-ScheduledTask -TaskName $entry.name -ErrorAction SilentlyContinue) {
            Stop-ScheduledTask -TaskName $entry.name -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $entry.name -Confirm:$false
        }
        if ($entry.existed) {
            $xml = Get-Content -LiteralPath $entry.xml -Raw
            Register-ScheduledTask -TaskName $entry.name -Xml $xml | Out-Null
        }
    }
    foreach ($entry in @($Backup.tasks | Where-Object { $_.existed -and $_.was_running })) {
        Start-ScheduledTask -TaskName $entry.name -ErrorAction SilentlyContinue
    }
    if ($Backup.startup_registry_existed) {
        New-Item -Path $startupRegistryPath -Force | Out-Null
        Set-ItemProperty -LiteralPath $startupRegistryPath -Name $startupRegistryName -Value $Backup.startup_registry_value
    }
    else {
        Remove-ItemProperty -LiteralPath $startupRegistryPath -Name $startupRegistryName -ErrorAction SilentlyContinue
    }
}

function Stop-SirinTasksForUpgrade {
    # Disable both tasks first. The daemon heartbeat can otherwise re-trigger
    # itself, and its start-only supervisor can re-trigger the Driver while the
    # installer is waiting for both processes to exit.
    foreach ($name in @($daemonTaskName, $driverTaskName)) {
        $task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
        if ($task -and [bool]$task.Settings.Enabled) {
            Disable-ScheduledTask -TaskName $name | Out-Null
        }
    }
    foreach ($name in @($daemonTaskName, $driverTaskName)) {
        $task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
        if ($task -and [string]$task.State -eq 'Running') {
            Stop-ScheduledTask -TaskName $name -ErrorAction Stop
        }
    }
    $deadline = (Get-Date).AddSeconds(20)
    do {
        $running = @(@($daemonTaskName, $driverTaskName) | Where-Object {
            $task = Get-ScheduledTask -TaskName $_ -ErrorAction SilentlyContinue
            $task -and [string]$task.State -eq 'Running'
        })
        if ($running.Count -eq 0) {
            return
        }
        Start-Sleep -Milliseconds 500
    } until ((Get-Date) -ge $deadline)
    throw "Sirin scheduled tasks did not stop for upgrade: $($running -join ', ')"
}

if ($Action -eq 'InitializeUser') {
    Initialize-SirinUserFiles | ConvertTo-Json -Depth 5
    return
}

foreach ($requiredScript in @($runtimeInstaller, $driverTaskInstaller, $daemonTaskInstaller, $codexInstaller)) {
    if (-not (Test-Path -LiteralPath $requiredScript -PathType Leaf)) {
        throw "Sirin installer component is missing: $requiredScript"
    }
}

if ($Action -eq 'Status') {
    Get-UnifiedStatus | ConvertTo-Json -Depth 14
    return
}

if ($Action -eq 'RecordCapabilityProof') {
    $live = Get-LiveIosStatus
    $runtime = & $runtimeInstaller -Action Status -Repo $Repo | ConvertFrom-Json
    if (-not $live.reachable) {
        throw 'Sirin MCP is unreachable; capability proof cannot be recorded.'
    }
    $deviceStatus = $live.status
    $capture = $deviceStatus.capabilities.SCREEN_CAPTURE
    $control = $deviceStatus.capabilities.SCREEN_CONTROL
    if (
        $deviceStatus.provider.provider -ne 'sirin-ios-driver' -or
        $deviceStatus.provider.acceptance_only -ne $true -or
        $deviceStatus.provider.lan_exposed -ne $false
    ) {
        throw 'The live provider does not satisfy the Sirin acceptance-only loopback contract.'
    }
    if ($null -ne $deviceStatus.active_lease) {
        throw 'Release the iPhone control lease before recording capability proof.'
    }
    if (
        $capture.status -ne 'PASS' -or
        $control.status -ne 'PASS' -or
        $control.evidence.action -ne 'swipe' -or
        $control.evidence.changed -ne $true -or
        $control.evidence.visual_change.material_content_change -ne $true -or
        $control.evidence.visual_change.vertical_scroll_proven -ne $true -or
        [math]::Abs([int]$control.evidence.visual_change.best_vertical_shift_pixels) -le 0 -or
        $control.evidence.after_frame_id -ne $capture.evidence.frame_id -or
        $control.evidence.before_frame_id -eq $control.evidence.after_frame_id -or
        -not (Test-CaptureEvidence $capture.evidence)
    ) {
        throw 'Fresh Sirin SCREEN_CAPTURE=PASS plus content-region material vertical-displacement SCREEN_CONTROL=PASS evidence is required.'
    }
    $now = (Get-Date).ToUniversalTime()
    $capturedAt = ([datetime]$capture.evidence.captured_at).ToUniversalTime()
    $observedAt = ([datetime]$control.evidence.observed_at).ToUniversalTime()
    if (
        ($now - $capturedAt).TotalMinutes -gt 15 -or
        ($now - $observedAt).TotalMinutes -gt 15 -or
        [math]::Abs(($capturedAt - $observedAt).TotalMinutes) -gt 5
    ) {
        throw 'The capture/control evidence is stale; perform one new safe vertical swipe through Sirin and retry.'
    }
    $driverOwner = Get-LoopbackPortOwner 8770
    if (
        $null -eq $driverOwner -or
        $driverOwner.command_line -notmatch [regex]::Escape([string]$runtime.source_root) -or
        $driverOwner.command_line -notmatch [regex]::Escape([string]$runtime.runtime_root)
    ) {
        throw 'The process listening on port 8770 is not the Sirin-owned Driver source/runtime.'
    }

    [void](New-Item -ItemType Directory -Path $evidenceRoot -Force)
    $proofPath = Join-Path $evidenceRoot ("sirin-ios-capability-proof-{0}.json" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))
    $proof = [pscustomobject]@{
        status = 'PASS'
        recorded_at = $now.ToString('o')
        binary = $Binary
        binary_sha256 = Get-HashOrNull $Binary
        driver_source_root = $runtime.source_root
        driver_source_sha256 = $runtime.source_sha256
        driver_runtime_root = $runtime.runtime_root
        provider = $deviceStatus.provider.provider
        acceptance_only = $deviceStatus.provider.acceptance_only
        lan_exposed = $deviceStatus.provider.lan_exposed
        device_detected = $deviceStatus.capabilities.DEVICE_DETECTED.status
        info_readable = $deviceStatus.capabilities.INFO_READABLE.status
        screen_capture = $capture.evidence
        screen_control = $control.evidence
        active_lease_after = $deviceStatus.active_lease
        recording_phone_action_performed = $false
        safety = [pscustomobject]@{
            credentials_supported = $false
            trust_or_unlock_supported = $false
            signing_supported = $false
            jailbreak_supported = $false
            commerce_actions_supported = $false
        }
    }
    $proof | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $proofPath -Encoding utf8
    $recorded = Get-IosCapabilityEvidence ([string]$runtime.source_sha256)
    if ($recorded.status -ne 'PASS') {
        throw 'The capability proof was written but failed independent validation.'
    }
    $recorded | ConvertTo-Json -Depth 12
    return
}

if ($Action -eq 'PrepareRebootProof') {
    $status = Get-UnifiedStatus
    if ($status.status -ne 'READY') {
        throw 'Sirin is not READY; reboot proof cannot be armed.'
    }
    if ($status.migration.active_dependencies_use_sidetap) {
        throw 'An active dependency still uses SideTap; reboot proof cannot be armed.'
    }
    if ($status.migration.soak_8h.status -ne 'PASS') {
        throw 'A qualifying eight-hour soak PASS is required before reboot proof.'
    }
    if ($status.migration.capability_proof.status -ne 'PASS') {
        throw 'Fresh Sirin screen capture/control capability proof is required before reboot proof.'
    }
    [void](New-Item -ItemType Directory -Path $evidenceRoot -Force)
    $checkpoint = [pscustomobject]@{
        status = 'ARMED'
        created_at = (Get-Date).ToUniversalTime().ToString('o')
        boot_before = (Get-SystemBootTime).ToString('o')
        binary = $Binary
        binary_sha256 = Get-HashOrNull $Binary
        soak_summary = $status.migration.soak_8h.qualified_summary.path
        soak_summary_sha256 = $status.migration.soak_8h.qualified_summary.sha256
        capability_proof = $status.migration.capability_proof.evidence
        capability_proof_sha256 = $status.migration.capability_proof.sha256
        safety = [pscustomobject]@{
            reboot_requested = $false
            phone_action_performed = $false
            trust_or_unlock_supported = $false
            user_logon_required_after_reboot = $true
            windows_password_or_autologon_configured = $false
        }
    }
    $checkpoint | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $rebootCheckpointPath -Encoding utf8
    $checkpoint | Add-Member -NotePropertyName evidence -NotePropertyValue $rebootCheckpointPath
    $checkpoint | ConvertTo-Json -Depth 8
    return
}

if ($Action -eq 'VerifyRebootProof') {
    if (-not (Test-Path -LiteralPath $rebootCheckpointPath -PathType Leaf)) {
        throw 'Reboot checkpoint is missing. Run PrepareRebootProof before an explicitly approved reboot.'
    }
    $checkpoint = Get-Content -LiteralPath $rebootCheckpointPath -Raw | ConvertFrom-Json
    $currentBoot = Get-SystemBootTime
    $checkpointCreated = ([datetime]$checkpoint.created_at).ToUniversalTime()
    $bootOccurredAfterCheckpoint = $currentBoot -gt $checkpointCreated
    $status = Get-UnifiedStatus
    $daemonInfo = Get-ScheduledTaskInfo -TaskName $daemonTaskName -ErrorAction SilentlyContinue
    $driverInfo = Get-ScheduledTaskInfo -TaskName $driverTaskName -ErrorAction SilentlyContinue
    $daemonRanAfterBoot = $null -ne $daemonInfo -and ([datetime]$daemonInfo.LastRunTime).ToUniversalTime() -ge $currentBoot.AddSeconds(-5)
    $driverRanAfterBoot = $null -ne $driverInfo -and ([datetime]$driverInfo.LastRunTime).ToUniversalTime() -ge $currentBoot.AddSeconds(-5)
    $binaryMatchesCheckpoint = (Get-HashOrNull $Binary) -eq $checkpoint.binary_sha256
    $pass = (
        $bootOccurredAfterCheckpoint -and
        $daemonRanAfterBoot -and
        $driverRanAfterBoot -and
        $binaryMatchesCheckpoint -and
        $status.status -eq 'READY' -and
        -not $status.migration.active_dependencies_use_sidetap -and
        $status.migration.capability_proof.status -eq 'PASS' -and
        $status.live.status.capabilities.DEVICE_DETECTED.status -eq 'PASS' -and
        $status.live.status.capabilities.INFO_READABLE.status -eq 'PASS'
    )
    [void](New-Item -ItemType Directory -Path $evidenceRoot -Force)
    $proofPath = Join-Path $evidenceRoot ("sirin-ios-reboot-proof-{0}.json" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))
    $proof = [pscustomobject]@{
        status = if ($pass) { 'PASS' } else { 'DEFECT' }
        checked_at = (Get-Date).ToUniversalTime().ToString('o')
        checkpoint = $rebootCheckpointPath
        checkpoint_sha256 = Get-HashOrNull $rebootCheckpointPath
        boot_at = $currentBoot.ToString('o')
        boot_occurred_after_checkpoint = $bootOccurredAfterCheckpoint
        binary = $Binary
        binary_sha256 = Get-HashOrNull $Binary
        binary_matches_checkpoint = $binaryMatchesCheckpoint
        daemon_ran_after_boot = $daemonRanAfterBoot
        driver_ran_after_boot = $driverRanAfterBoot
        user_logon_required = $true
        cold_boot_without_user_logon_proven = $false
        unified_status = $status.status
        active_dependencies_use_sidetap = $status.migration.active_dependencies_use_sidetap
        capability_proof = $status.migration.capability_proof.evidence
        capability_proof_sha256 = $status.migration.capability_proof.sha256
        device_detected = $status.live.status.capabilities.DEVICE_DETECTED.status
        info_readable = $status.live.status.capabilities.INFO_READABLE.status
        phone_action_performed = $false
    }
    $proof | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $proofPath -Encoding utf8
    $proof | Add-Member -NotePropertyName evidence -NotePropertyValue $proofPath
    $proof | ConvertTo-Json -Depth 8
    if (-not $pass) {
        exit 1
    }
    return
}

if ($Action -eq 'RetireLegacy') {
    $status = Get-UnifiedStatus
    if (-not $status.migration.legacy_retirement_ready) {
        throw 'Legacy retirement gates have not passed. Sirin capture/control proof, eight-hour soak, and reboot recovery proof are all required.'
    }
    $backup = Save-TaskBackup
    foreach ($name in $legacyTaskNames) {
        if (Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue) {
            Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $name -Confirm:$false
        }
    }
    [pscustomobject]@{
        status = 'PASS'
        action = 'RETIRE_LEGACY'
        installer_backup = $backup.directory
        removed_tasks = $legacyTaskNames
        sidetap_files_deleted = $false
        note = 'Legacy scheduled tasks were removed only after soak and reboot gates. SideTap files remain recoverable.'
    } | ConvertTo-Json -Depth 6
    return
}

if ($Action -eq 'Remove') {
    foreach ($name in @($daemonTaskName, $driverTaskName)) {
        if (Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue) {
            Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $name -Confirm:$false
        }
    }
    $startupValue = (Get-ItemProperty -LiteralPath $startupRegistryPath -Name $startupRegistryName -ErrorAction SilentlyContinue).Sirin
    $normalizedStartupValue = ([string]$startupValue).Trim().Trim('"')
    if ([string]::Equals($normalizedStartupValue, $Binary, [System.StringComparison]::OrdinalIgnoreCase)) {
        Remove-ItemProperty -LiteralPath $startupRegistryPath -Name $startupRegistryName -ErrorAction SilentlyContinue
    }
    Get-UnifiedStatus | ConvertTo-Json -Depth 14
    return
}

if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    throw "Sirin binary is missing: $Binary"
}

Initialize-SirinUserFiles | Out-Null
$backup = Save-TaskBackup
try {
    $enabledLegacy = @($legacyTaskNames | ForEach-Object { Get-TaskState $_ } | Where-Object enabled -eq $true)
    if ($enabledLegacy.Count -gt 0 -and -not $MigrateLegacySideTap) {
        throw 'Legacy SideTap tasks are enabled. Re-run the same Sirin installer entry with -MigrateLegacySideTap after reviewing the migration.'
    }
    if ($MigrateLegacySideTap) {
        foreach ($legacy in $legacyTaskNames) {
            if (Get-ScheduledTask -TaskName $legacy -ErrorAction SilentlyContinue) {
                Stop-ScheduledTask -TaskName $legacy -ErrorAction SilentlyContinue
                Disable-ScheduledTask -TaskName $legacy | Out-Null
            }
        }
    }

    # The scheduled daemon is the sole startup owner for iPhone mode. Remove a
    # previous HKCU Run entry so upgrades cannot launch a second Sirin process.
    Remove-ItemProperty -LiteralPath $startupRegistryPath -Name $startupRegistryName -ErrorAction SilentlyContinue

    Stop-SirinTasksForUpgrade
    $legacyProcessMigration = if ($MigrateLegacySideTap) {
        Stop-LegacySideTapProcesses
    }
    else {
        [pscustomobject]@{ status = 'SKIPPED'; stopped_count = 0 }
    }
    $iosRuntimeProcessMigration = Stop-SirinIosRuntimeProcesses
    & $runtimeInstaller -Action Install -Repo $Repo | Out-Null
    $wdaBundleMigration = if ($MigrateLegacySideTap) {
        Import-LegacyWdaBundleSelection
    }
    else {
        [pscustomobject]@{ status = 'SKIPPED'; bundle_id = $null }
    }
    & $driverTaskInstaller -Action Install -Repo $Repo | Out-Null
    & $daemonTaskInstaller -Action Install -Repo $Repo -Binary $Binary -EnableIosDriverSupervisor $true -RunNow:$RunNow | Out-Null
    if (-not $SkipCodexIntegration) {
        & $codexInstaller -Action Install -Repo $Repo -CodexHome $CodexHome -ManageProjectConfig:$false | Out-Null
    }

    $status = $null
    $linkInitialization = $null
    if ($RunNow) {
        # A healthy endpoint can appear before the supervised Driver has
        # rebuilt its userspace tunnel, forwards and WDA session. Wait for the
        # complete live gate, not just TCP reachability. A locked or otherwise
        # unavailable phone is an explicit NEEDS_ATTENTION result, not grounds
        # to roll back a successfully installed Sirin control plane.
        $deadline = (Get-Date).AddSeconds(135)
        do {
            Start-Sleep -Seconds 3
            $status = Get-UnifiedStatus
            if (
                $null -eq $linkInitialization -and
                $status.live.reachable -eq $true -and
                $status.live.status.provider.provider_reachable -eq $true
            ) {
                # Passive attach is the durable default.  Only the explicit
                # -RunNow path asks Sirin's public MCP control plane to activate the
                # link, proves readiness, then releases the temporary lease and
                # every Sirin-owned transient link process.
                $linkInitialization = Request-IosLinkReadiness
                $status = Get-UnifiedStatus
            }
            $explicitHumanBoundary = (
                @($status.live.status.human_blockers | Where-Object { $_ -like 'NEEDS_HUMAN_*' }).Count -gt 0 -and
                $status.live.status.provider.starting -ne $true
            )
            $explicitSafetyBoundary = (
                @($status.live.status.human_blockers | Where-Object { $_ -like 'SECURITY_BLOCK_*' }).Count -gt 0 -and
                $status.live.status.provider.starting -ne $true
            )
        } until (
            $status.status -eq 'READY' -or
            $explicitHumanBoundary -or
            $explicitSafetyBoundary -or
            (Get-Date) -ge $deadline
        )

        if ($status.gates.control_plane_ready -ne $true) {
            throw "Sirin control plane installation did not reach its structural readiness gates (next action: $($status.migration.next_safe_action))."
        }
    }

    if ($null -eq $status) {
        $status = Get-UnifiedStatus
    }
    $status | Add-Member -NotePropertyName installer_backup -NotePropertyValue $backup.directory
    $status | Add-Member -NotePropertyName action -NotePropertyValue $Action.ToUpperInvariant()
    $status | Add-Member -NotePropertyName wda_bundle_migration -NotePropertyValue $wdaBundleMigration
    $status | Add-Member -NotePropertyName legacy_process_migration -NotePropertyValue $legacyProcessMigration
    $status | Add-Member -NotePropertyName ios_runtime_process_migration -NotePropertyValue $iosRuntimeProcessMigration
    $status | Add-Member -NotePropertyName link_initialization -NotePropertyValue $linkInitialization
    $status | ConvertTo-Json -Depth 14
}
catch {
    $installError = $_.Exception.Message
    Restore-TaskBackup $backup
    [pscustomobject]@{
        status = 'ROLLED_BACK'
        error = $installError
        installer_backup = $backup.directory
        note = 'Scheduled tasks were restored. Runtime/evidence and Codex user files were preserved.'
    } | ConvertTo-Json -Depth 8
    exit 1
}
