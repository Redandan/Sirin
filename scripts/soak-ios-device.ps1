#requires -Version 5.1
<#
Collect read-only Sirin/iPhone stability evidence for a bounded period.

This script calls only MCP tools/list and ios_device_status. It never captures
the screen, acquires a control lease, or performs a phone action. Each sample
is appended as one JSON object; a final summary is written beside the JSONL.
#>

[CmdletBinding()]
param(
    [ValidateRange(1, 1440)]
    [int]$DurationMinutes = 480,
    [ValidateRange(5, 60)]
    [int]$IntervalSeconds = 60,
    [string]$Endpoint = 'http://127.0.0.1:7700/mcp',
    [string]$OutputPath = ''
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $evidenceDir = Join-Path $env:LOCALAPPDATA 'Sirin\device_evidence\ios'
    [void](New-Item -ItemType Directory -Path $evidenceDir -Force)
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $OutputPath = Join-Path $evidenceDir "sirin-ios-soak-$stamp.jsonl"
}
$OutputPath = [System.IO.Path]::GetFullPath($OutputPath)
$summaryPath = [System.IO.Path]::ChangeExtension($OutputPath, '.summary.json')

$requiredPhysicalIosTools = @(
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
    'ios_tap'
)

function Invoke-Mcp([string]$Method, $Params, [int]$Id) {
    $body = @{
        jsonrpc = '2.0'
        id = $Id
        method = $Method
        params = $Params
    } | ConvertTo-Json -Depth 12
    Invoke-RestMethod -Uri $Endpoint -Method Post -ContentType 'application/json' -Body $body
}

function Get-TaskState([string]$Name) {
    $task = Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue
    if ($null -eq $task) {
        return [pscustomobject]@{ state = 'MISSING'; enabled = $false }
    }
    [pscustomobject]@{
        state = [string]$task.State
        enabled = [bool]$task.Settings.Enabled
    }
}

$startedAt = Get-Date
$deadline = $startedAt.AddMinutes($DurationMinutes)
$sampleCount = 0
$failureCount = 0
$contract = $null
$previousCheckedAt = $null
$maxObservedGapSeconds = 0.0
$gapViolationCount = 0
$maxAllowedGapSeconds = [math]::Max(180, $IntervalSeconds * 3)
$minimumSamples = [math]::Floor(($DurationMinutes * 60) / ($IntervalSeconds * 1.5)) + 1

try {
    $toolsResponse = Invoke-Mcp -Method 'tools/list' -Params @{} -Id 1
    $toolNames = @($toolsResponse.result.tools | ForEach-Object { [string]$_.name })
    $missingTools = @($requiredPhysicalIosTools | Where-Object { $_ -notin $toolNames })
    $contract = [pscustomobject]@{
        tool_count = $toolNames.Count
        required_physical_ios_tool_count = $requiredPhysicalIosTools.Count
        missing_physical_ios_tools = $missingTools
        pass = ($missingTools.Count -eq 0)
    }
} catch {
    $contract = [pscustomobject]@{
        tool_count = 0
        required_physical_ios_tool_count = $requiredPhysicalIosTools.Count
        missing_physical_ios_tools = $requiredPhysicalIosTools
        pass = $false
        error = $_.Exception.Message
    }
}

while ($true) {
    $checkedAt = Get-Date
    $sampleCount++
    $gapSeconds = if ($null -ne $previousCheckedAt) {
        [math]::Round(($checkedAt - $previousCheckedAt).TotalSeconds, 3)
    }
    else {
        $null
    }
    $intervalPass = $null -eq $gapSeconds -or $gapSeconds -le $maxAllowedGapSeconds
    if ($null -ne $gapSeconds) {
        $maxObservedGapSeconds = [math]::Max($maxObservedGapSeconds, $gapSeconds)
    }
    if (-not $intervalPass) {
        $gapViolationCount++
    }
    $previousCheckedAt = $checkedAt
    try {
        $response = Invoke-Mcp -Method 'tools/call' -Params @{
            name = 'ios_device_status'
            arguments = @{}
        } -Id (1000 + $sampleCount)
        $status = $response.result.content[0].text | ConvertFrom-Json
        $daemonTask = Get-TaskState 'Sirin Local Ops Daemon'
        $driverTask = Get-TaskState 'Sirin iOS Driver'
        $legacyTask = Get-TaskState 'SideTap-Unattended'
        $legacyWatchdogTask = Get-TaskState 'SideTap-Unattended-Watchdog'

        $pass = (
            $intervalPass -and
            $contract.pass -and
            $status.ok -and
            $status.provider.provider -eq 'sirin-ios-driver' -and
            $status.capabilities.DEVICE_DETECTED.status -eq 'PASS' -and
            $status.capabilities.INFO_READABLE.status -eq 'PASS' -and
            $null -eq $status.active_lease -and
            $status.provider.acceptance_only -eq $true -and
            $status.provider.lan_exposed -eq $false -and
            $status.lifecycle.owner -eq 'sirin' -and
            $status.lifecycle.autostart_enabled -eq $true -and
            $daemonTask.state -eq 'Running' -and
            $driverTask.state -eq 'Running' -and
            $legacyTask.enabled -eq $false -and
            $legacyWatchdogTask.enabled -eq $false
        )
        if (-not $pass) {
            $failureCount++
        }

        $sample = [pscustomobject]@{
            checked_at = $checkedAt.ToString('o')
            sample = $sampleCount
            pass = $pass
            gap_seconds = $gapSeconds
            interval_pass = $intervalPass
            provider = $status.provider.provider
            device_detected = $status.capabilities.DEVICE_DETECTED.status
            info_readable = $status.capabilities.INFO_READABLE.status
            screen_capture = $status.capabilities.SCREEN_CAPTURE.status
            screen_control = $status.capabilities.SCREEN_CONTROL.status
            active_lease = $status.active_lease
            acceptance_only = $status.provider.acceptance_only
            lan_exposed = $status.provider.lan_exposed
            lifecycle = $status.lifecycle
            tasks = [pscustomobject]@{
                sirin_daemon = $daemonTask
                ios_driver = $driverTask
                legacy_sidetap = $legacyTask
                legacy_sidetap_watchdog = $legacyWatchdogTask
            }
        }
    } catch {
        $failureCount++
        $sample = [pscustomobject]@{
            checked_at = $checkedAt.ToString('o')
            sample = $sampleCount
            pass = $false
            gap_seconds = $gapSeconds
            interval_pass = $intervalPass
            error = $_.Exception.Message
        }
    }

    Add-Content -LiteralPath $OutputPath -Value ($sample | ConvertTo-Json -Compress -Depth 12) -Encoding utf8
    if ((Get-Date) -ge $deadline) {
        break
    }
    Start-Sleep -Seconds $IntervalSeconds
}

$finishedAt = Get-Date
$actualDurationSeconds = [math]::Round(($finishedAt - $startedAt).TotalSeconds, 2)
$continuousSampling = (
    $gapViolationCount -eq 0 -and
    $sampleCount -ge $minimumSamples -and
    $actualDurationSeconds -ge ($DurationMinutes * 60)
)
$summary = [pscustomobject]@{
    status = if ($failureCount -eq 0 -and $contract.pass -and $continuousSampling) { 'PASS' } else { 'DEFECT' }
    started_at = $startedAt.ToString('o')
    finished_at = $finishedAt.ToString('o')
    requested_duration_minutes = $DurationMinutes
    actual_duration_seconds = $actualDurationSeconds
    interval_seconds = $IntervalSeconds
    samples = $sampleCount
    minimum_samples = $minimumSamples
    failures = $failureCount
    max_allowed_gap_seconds = $maxAllowedGapSeconds
    max_observed_gap_seconds = [math]::Round($maxObservedGapSeconds, 3)
    gap_violations = $gapViolationCount
    continuous_sampling = $continuousSampling
    contract = $contract
    evidence_jsonl = $OutputPath
}
$summary | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $summaryPath -Encoding utf8
$summary | ConvertTo-Json -Depth 12

if ($summary.status -ne 'PASS') {
    exit 1
}
