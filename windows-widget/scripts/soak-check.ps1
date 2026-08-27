[CmdletBinding()]
param(
    [string]$EvidenceRoot = 'D:\IdeaProjects\Sirin\target-ai-work-widget\widget-soak-v0111',
    [string]$ExpectedWidgetVersion = '0.1.11.0',
    [string]$ApiUri = 'http://127.0.0.1:7700/api/ai-monitor',
    [int]$DurationHours = 24,
    [int]$MinimumFinalCheckpoints = 40
)

$ErrorActionPreference = 'Stop'
$packageName = 'Redan.SirinAIWorkWidget'
$providerStatePath = Join-Path $env:LOCALAPPDATA 'Sirin\widget-provider-state.json'
$logPath = Join-Path $EvidenceRoot 'checkpoints.jsonl'
$latestPath = Join-Path $EvidenceRoot 'latest.json'
$runPath = Join-Path $EvidenceRoot 'run.json'

New-Item -ItemType Directory -Path $EvidenceRoot -Force | Out-Null

$now = [DateTimeOffset]::Now
if (-not (Test-Path -LiteralPath $runPath)) {
    $run = [ordered]@{
        schema_version = 1
        run_id = 'sirin-widget-v0111-' + $now.ToString('yyyyMMdd-HHmmss')
        started_at = $now.ToString('o')
        deadline_at = $now.AddHours($DurationHours).ToString('o')
        duration_hours = $DurationHours
        expected_widget_version = $ExpectedWidgetVersion
        api_uri = $ApiUri
        mutation_policy = 'READ_ONLY_OBSERVATION'
    }
    $run | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $runPath -Encoding UTF8
} else {
    $run = Get-Content -LiteralPath $runPath -Raw -Encoding UTF8 | ConvertFrom-Json
}

$failures = [Collections.Generic.List[string]]::new()
$warnings = [Collections.Generic.List[string]]::new()
$missingProof = [Collections.Generic.List[string]]::new()

$package = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue
if (-not $package) {
    $failures.Add('WIDGET_PACKAGE_MISSING')
}

$installedProviderPath = if ($package) {
    Join-Path ([string]$package.InstallLocation) 'SirinWidgetProvider\SirinWidgetProvider.exe'
} else {
    $null
}
$provider = Get-CimInstance Win32_Process -Filter "Name = 'SirinWidgetProvider.exe'" -ErrorAction SilentlyContinue |
    Where-Object {
        -not $installedProviderPath -or
        [string]::Equals(
            [string]$_.ExecutablePath,
            [IO.Path]::GetFullPath($installedProviderPath),
            [StringComparison]::OrdinalIgnoreCase
        )
    } |
    Select-Object -First 1

if ($package -and [string]$package.Version -ne $ExpectedWidgetVersion) {
    $failures.Add('WIDGET_VERSION_MISMATCH')
}
if ($package -and -not (Test-Path -LiteralPath ([string]$package.InstallLocation))) {
    $failures.Add('WIDGET_INSTALL_LOCATION_MISSING')
}
if (-not $provider) {
    $failures.Add('WIDGET_PROVIDER_NOT_RUNNING')
}

$providerState = $null
$providerStateItem = $null
if (Test-Path -LiteralPath $providerStatePath) {
    try {
        $providerStateItem = Get-Item -LiteralPath $providerStatePath
        $providerState = Get-Content -LiteralPath $providerStatePath -Raw -Encoding UTF8 | ConvertFrom-Json
    } catch {
        $failures.Add('PROVIDER_STATE_INVALID')
    }
} else {
    $failures.Add('PROVIDER_STATE_MISSING')
}

if ($providerState) {
    if ([string]$providerState.version -ne $ExpectedWidgetVersion) {
        $failures.Add('PROVIDER_STATE_VERSION_MISMATCH')
    }
    if ([string]$providerState.event -ne 'UPDATE_OK' -or [int]$providerState.hresult -ne 0) {
        $failures.Add('PROVIDER_LAST_UPDATE_FAILED')
    }
    if ([int]$providerState.widget_count -lt 1) {
        $failures.Add('PINNED_WIDGET_NOT_OBSERVED')
    }
    $providerStateAgeSeconds = [math]::Max(
        0,
        [int](($now.UtcDateTime - $providerStateItem.LastWriteTimeUtc).TotalSeconds)
    )
    if ($providerStateAgeSeconds -gt 300) {
        $warnings.Add('PROVIDER_STATE_STALE_WHILE_WIDGET_MAY_BE_HIDDEN')
    }
} else {
    $providerStateAgeSeconds = $null
}

$widgetSession = $null
if ($package) {
    $sessionRoot = Join-Path $env:LOCALAPPDATA (
        'Packages\Microsoft.WidgetsPlatformRuntime_8wekyb3d8bbwe\LocalState\WidgetSessions\' +
        [string]$package.PackageFamilyName + '!App!!SirinAIWorkWidgetProvider'
    )
    $widgetSession = Get-ChildItem -LiteralPath $sessionRoot -Filter '*.dat' -File -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if (-not $widgetSession) {
        $failures.Add('WIDGET_SESSION_MISSING')
    }
}

$snapshot = $null
try {
    $snapshot = Invoke-RestMethod -Uri $ApiUri -TimeoutSec 15
} catch {
    $failures.Add('SIRIN_AI_MONITOR_UNREACHABLE')
}

$sampleAgeSeconds = $null
$requiredModes = @()
$missingRequiredModes = @()
if ($snapshot) {
    $nowUnixMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $lastSampledAtMs = [int64]$snapshot.ai_work.codex_token_trend.last_sampled_at_ms
    if ($lastSampledAtMs -gt 0) {
        $sampleAgeSeconds = [math]::Max(0, [int](($nowUnixMs - $lastSampledAtMs) / 1000))
    }
    if ($null -eq $sampleAgeSeconds -or $sampleAgeSeconds -gt 180) {
        $failures.Add('TOKEN_SAMPLER_STALE')
    }
    if (-not [bool]$snapshot.ai_work.codex_token_trend.history_persisted) {
        $failures.Add('TOKEN_HISTORY_NOT_PERSISTED')
    }
    if (-not [bool]$snapshot.ai_work.codex_token_trend.history_restored) {
        $failures.Add('TOKEN_HISTORY_NOT_RESTORED')
    }
    if ([string]$snapshot.recovery.status -ne 'RESTORED') {
        $failures.Add('SIRIN_MONITOR_NOT_RESTORED')
    }
    if ([string]$snapshot.ai_work.system_resources.pressure -eq 'CRITICAL') {
        $failures.Add('LOCAL_RESOURCE_PRESSURE_CRITICAL')
    } elseif ([string]$snapshot.ai_work.system_resources.pressure -ne 'HEALTHY') {
        $warnings.Add('LOCAL_RESOURCE_PRESSURE_ELEVATED')
    }
    if ([double]$snapshot.overhead.process.cpu_percent_recent -gt 10) {
        $warnings.Add('SIRIN_PROCESS_CPU_OVER_10_PERCENT')
    }
    if ([double]$snapshot.overhead.process.working_set_mb -gt 250) {
        $warnings.Add('SIRIN_PROCESS_MEMORY_OVER_250_MB')
    }
    if ([bool]$snapshot.network.split_default_route) {
        $warnings.Add('KNOWN_IPV4_IPV6_SPLIT_ROUTE')
    }
    $requiredModes = @($snapshot.acceptance.modes | Where-Object required)
    $missingRequiredModes = @($requiredModes | Where-Object status -ne 'PASS' | ForEach-Object mode)
    foreach ($mode in $missingRequiredModes) {
        $missingProof.Add('CYCLE_' + $mode)
    }
}

$previousRecords = @()
if (Test-Path -LiteralPath $logPath) {
    $previousRecords = @(Get-Content -LiteralPath $logPath -Encoding UTF8 | ForEach-Object {
        try { $_ | ConvertFrom-Json } catch { $null }
    } | Where-Object { $null -ne $_ })
}
$previousFailureCount = @($previousRecords | Where-Object { $_.failure_count -gt 0 }).Count
$checkpointNumber = $previousRecords.Count + 1
$deadline = [DateTimeOffset]::Parse([string]$run.deadline_at)
$deadlineReached = $now -ge $deadline

$classification = if ($failures.Count -gt 0) {
    'FAIL_CHECKPOINT'
} elseif ($missingProof.Count -gt 0) {
    'CORE_PASS_MISSING_CYCLE_PROOF'
} else {
    'PASS_CHECKPOINT'
}

$finalClassification = $null
if ($deadlineReached) {
    if ($previousFailureCount + $failures.Count -gt 0) {
        $finalClassification = 'SOAK_FAIL'
    } elseif ($checkpointNumber -lt $MinimumFinalCheckpoints -or $missingProof.Count -gt 0) {
        $finalClassification = 'MISSING_PROOF'
    } else {
        $finalClassification = 'SOAK_PASS'
    }
}

$record = [ordered]@{
    schema_version = 1
    run_id = [string]$run.run_id
    checkpoint = $checkpointNumber
    captured_at = $now.ToString('o')
    deadline_at = $deadline.ToString('o')
    deadline_reached = $deadlineReached
    classification = $classification
    final_classification = $finalClassification
    failure_count = $failures.Count
    failures = @($failures)
    warning_count = $warnings.Count
    warnings = @($warnings)
    missing_proof = @($missingProof)
    widget = [ordered]@{
        package_version = if ($package) { [string]$package.Version } else { $null }
        install_location = if ($package) { [string]$package.InstallLocation } else { $null }
        provider_running = $null -ne $provider
        provider_pid = if ($provider) { [int]$provider.ProcessId } else { $null }
        state_event = if ($providerState) { [string]$providerState.event } else { $null }
        state_hresult = if ($providerState) { [int]$providerState.hresult } else { $null }
        widget_count = if ($providerState) { [int]$providerState.widget_count } else { $null }
        provider_state_age_secs = $providerStateAgeSeconds
        session_path = if ($widgetSession) { $widgetSession.FullName } else { $null }
        session_last_write_utc = if ($widgetSession) { $widgetSession.LastWriteTimeUtc.ToString('o') } else { $null }
    }
    sirin = [ordered]@{
        api_version = if ($snapshot) { [string]$snapshot.version } else { $null }
        token_sample_age_secs = $sampleAgeSeconds
        recovery_status = if ($snapshot) { [string]$snapshot.recovery.status } else { $null }
        monitor_uptime_secs = if ($snapshot) { [int64]$snapshot.recovery.uptime_secs } else { $null }
        sampler_runs_total = if ($snapshot) { [int64]$snapshot.overhead.sampler_runs_total } else { $null }
        sampler_wall_ms = if ($snapshot) { [double]$snapshot.overhead.last_sampler_wall_ms } else { $null }
        sampler_cpu_ms = if ($snapshot) { [double]$snapshot.overhead.last_sampler_cpu_ms } else { $null }
        process_cpu_percent = if ($snapshot) { [double]$snapshot.overhead.process.cpu_percent_recent } else { $null }
        process_working_set_mb = if ($snapshot) { [double]$snapshot.overhead.process.working_set_mb } else { $null }
        resource_pressure = if ($snapshot) { [string]$snapshot.ai_work.system_resources.pressure } else { $null }
        c_free_gb = if ($snapshot) { [double]$snapshot.ai_work.system_resources.system_drive_free_gb } else { $null }
        session_locked = if ($snapshot) { [bool]$snapshot.power.session_locked } else { $null }
        system_required = if ($snapshot) { [bool]$snapshot.power.awake_guard.system_required } else { $null }
        standby_entered_since_monitor_start = if ($snapshot) { [bool]$snapshot.power.modern_standby.entered_since_monitor_start } else { $null }
        acceptance_status = if ($snapshot) { [string]$snapshot.acceptance.status } else { $null }
        missing_required_modes = @($missingRequiredModes)
    }
}

$json = $record | ConvertTo-Json -Depth 8 -Compress
Add-Content -LiteralPath $logPath -Value $json -Encoding UTF8
$temporaryLatestPath = $latestPath + '.tmp'
$record | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $temporaryLatestPath -Encoding UTF8
Move-Item -LiteralPath $temporaryLatestPath -Destination $latestPath -Force

$json
if ($classification -eq 'FAIL_CHECKPOINT') {
    exit 1
}
