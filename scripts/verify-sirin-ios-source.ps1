#requires -Version 5.1
<#
.SYNOPSIS
Verifies Sirin's low-overhead iPhone source contracts without contacting a phone.

.DESCRIPTION
Runs only source, parser, Python mock/unit, and Rust unit checks. It never calls
the Sirin/Driver HTTP endpoints, inspects a connected device, changes firewall or
phone settings, starts/stops a service, or installs dependencies. The JSON report
records the exact source hashes, commands, and requirement-to-test evidence.
#>

[CmdletBinding()]
param(
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$Python = '',
    [string]$OutputPath = '',
    [switch]$FullRust
)

$ErrorActionPreference = 'Stop'
$Repo = [System.IO.Path]::GetFullPath($Repo)
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
    $OutputPath = Join-Path $env:TEMP "sirin-ios-source-verification-$stamp.json"
}
$OutputPath = [System.IO.Path]::GetFullPath($OutputPath)

function Resolve-NativeTool([string]$Name, [string]$ExplicitPath = '') {
    if (-not [string]::IsNullOrWhiteSpace($ExplicitPath)) {
        $resolved = [System.IO.Path]::GetFullPath($ExplicitPath)
        if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
            throw "$Name is missing: $resolved"
        }
        return $resolved
    }
    $command = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $command) { throw "$Name is unavailable on PATH" }
    return $command.Source
}

function Resolve-PythonWithPytest([string]$ExplicitPath = '') {
    $candidates = @()
    if (-not [string]::IsNullOrWhiteSpace($ExplicitPath)) {
        $candidates += Resolve-NativeTool 'python' $ExplicitPath
    }
    else {
        $candidates += @(
            (Join-Path $Repo '.venv\Scripts\python.exe'),
            (Join-Path $Repo 'integrations\ios-driver\.venv\Scripts\python.exe')
        )
        $pathPython = Get-Command python -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($pathPython) { $candidates += $pathPython.Source }
    }
    foreach ($candidate in @($candidates | Select-Object -Unique)) {
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) { continue }
        $previousErrorActionPreference = $ErrorActionPreference
        try {
            $ErrorActionPreference = 'Continue'
            & $candidate -c 'import pytest' 1>$null 2>$null
            $probeExitCode = $LASTEXITCODE
        }
        finally {
            $ErrorActionPreference = $previousErrorActionPreference
        }
        if ($probeExitCode -eq 0) { return [System.IO.Path]::GetFullPath($candidate) }
    }
    throw ('No existing Python environment with pytest was found. ' +
        'Install integrations/ios-driver/requirements-dev.txt in an isolated environment ' +
        'and pass its python.exe with -Python. This verifier never installs dependencies.')
}

function Get-OutputTail([string]$Path, [int]$Lines = 100) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return '' }
    return (@(Get-Content -LiteralPath $Path -ErrorAction SilentlyContinue |
        Select-Object -Last $Lines) -join "`n")
}

function Invoke-NativeVerification(
    [string]$Name,
    [string]$FilePath,
    [string[]]$Arguments
) {
    $stdoutPath = [System.IO.Path]::GetTempFileName()
    $stderrPath = [System.IO.Path]::GetTempFileName()
    $started = [DateTime]::UtcNow
    $exitCode = 1
    $errorText = $null
    try {
        Push-Location $Repo
        try {
            # Windows PowerShell 5.1 promotes redirected native stderr to a
            # NativeCommandError record. Start-Process keeps cargo/pytest
            # progress as ordinary file output and makes the real process exit
            # code the only pass/fail contract.
            $process = Start-Process `
                -FilePath $FilePath `
                -ArgumentList $Arguments `
                -WorkingDirectory $Repo `
                -RedirectStandardOutput $stdoutPath `
                -RedirectStandardError $stderrPath `
                -WindowStyle Hidden `
                -Wait `
                -PassThru
            $exitCode = [int]$process.ExitCode
        }
        finally {
            Pop-Location
        }
    }
    catch {
        $errorText = $_.Exception.Message
    }
    $finished = [DateTime]::UtcNow
    $stdoutTail = Get-OutputTail $stdoutPath
    $stderrTail = Get-OutputTail $stderrPath
    Remove-Item -LiteralPath $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
    [pscustomobject]@{
        name = $Name
        ok = ($exitCode -eq 0 -and [string]::IsNullOrWhiteSpace($errorText))
        exit_code = $exitCode
        command = (@($FilePath) + $Arguments) -join ' '
        duration_ms = [int][Math]::Round(($finished - $started).TotalMilliseconds)
        stdout_tail = $stdoutTail
        stderr_tail = $stderrTail
        error = $errorText
    }
}

function Test-PowerShellSources([string[]]$RelativePaths) {
    $started = [DateTime]::UtcNow
    $failures = @()
    foreach ($relative in $RelativePaths) {
        $path = Join-Path $Repo $relative
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            $failures += "$relative`: missing"
            continue
        }
        $tokens = $null
        $errors = $null
        [System.Management.Automation.Language.Parser]::ParseFile(
            $path,
            [ref]$tokens,
            [ref]$errors
        ) | Out-Null
        if (@($errors).Count -gt 0) {
            $failures += "$relative`: $($errors[0].Message)"
        }
    }
    [pscustomobject]@{
        name = 'powershell_source_parse'
        ok = ($failures.Count -eq 0)
        exit_code = if ($failures.Count -eq 0) { 0 } else { 1 }
        command = 'PowerShell AST parse only'
        duration_ms = [int][Math]::Round(([DateTime]::UtcNow - $started).TotalMilliseconds)
        stdout_tail = if ($failures.Count -eq 0) {
            "Parsed $($RelativePaths.Count) iPhone PowerShell sources"
        }
        else { '' }
        stderr_tail = $failures -join "`n"
        error = $null
    }
}

$requiredSources = @(
    'src\ios_device.rs',
    'src\mcp_registry.rs',
    'integrations\ios-driver\src\phone_harness\device.py',
    'integrations\ios-driver\scripts\unattended_host.py',
    'integrations\ios-driver\tests\test_device.py',
    'integrations\ios-driver\tests\test_provider.py',
    'scripts\install-sirin-ios.ps1',
    'scripts\switch-sirin-daemon.ps1',
    'scripts\verify-sirin-ios-physical.ps1',
    'scripts\verify-sirin-ios-source.ps1',
    'docs\IOS_DEVICE_CONTROL.md',
    'docs\MCP_API.md',
    'integrations\codex\skills\iphone-usb-validation\SKILL.md',
    '.github\workflows\release.yml',
    'sirin.iss'
)
$powerShellSources = @(
    'scripts\install-sirin-ios.ps1',
    'scripts\install-ios-driver-runtime.ps1',
    'scripts\install-ios-driver-task.ps1',
    'scripts\install-sirin-daemon-task.ps1',
    'scripts\install-codex-ios-integration.ps1',
    'scripts\soak-ios-device.ps1',
    'scripts\switch-sirin-daemon.ps1',
    'scripts\verify-sirin-ios-physical.ps1'
)

$sourceEvidence = @($requiredSources | ForEach-Object {
    $path = Join-Path $Repo $_
    [pscustomobject]@{
        path = $_.Replace('\', '/')
        exists = Test-Path -LiteralPath $path -PathType Leaf
        sha256 = if (Test-Path -LiteralPath $path -PathType Leaf) {
            (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        }
        else { $null }
    }
})

$results = @()
$results += Test-PowerShellSources $powerShellSources

$pythonPath = $null
$cargoPath = $null
try { $pythonPath = Resolve-PythonWithPytest $Python }
catch {
    $results += [pscustomobject]@{
        name = 'python_driver_tests'; ok = $false; exit_code = 1
        command = 'python -m pytest integrations/ios-driver/tests'
        duration_ms = 0; stdout_tail = ''; stderr_tail = ''; error = $_.Exception.Message
    }
}
try { $cargoPath = Resolve-NativeTool 'cargo' }
catch {
    $results += [pscustomobject]@{
        name = 'rust_toolchain'; ok = $false; exit_code = 1
        command = 'cargo'; duration_ms = 0; stdout_tail = ''; stderr_tail = ''
        error = $_.Exception.Message
    }
}

$oldNoBytecode = $env:PYTHONDONTWRITEBYTECODE
$oldPluginAutoload = $env:PYTEST_DISABLE_PLUGIN_AUTOLOAD
$oldCargoColor = $env:CARGO_TERM_COLOR
try {
    $env:PYTHONDONTWRITEBYTECODE = '1'
    $env:PYTEST_DISABLE_PLUGIN_AUTOLOAD = '1'
    $env:CARGO_TERM_COLOR = 'never'
    if ($pythonPath) {
        $results += Invoke-NativeVerification 'python_driver_contracts' $pythonPath @(
            '-m', 'pytest', '-vv', '-p', 'no:cacheprovider',
            'integrations/ios-driver/tests/test_device.py::test_safe_kill_requires_successful_exit_proof',
            'integrations/ios-driver/tests/test_device.py::test_stop_all_verified_keeps_pid_record_when_termination_is_unproven',
            'integrations/ios-driver/tests/test_device.py::test_free_port_refuses_untracked_ios_listener',
            'integrations/ios-driver/tests/test_device.py::test_free_port_stops_only_matching_tracked_forward',
            'integrations/ios-driver/tests/test_device.py::test_lan_block_rule_requires_exact_fail_closed_rule_shape',
            'integrations/ios-driver/tests/test_provider.py::test_passive_status_does_not_probe_wda_when_usb_is_absent',
            'integrations/ios-driver/tests/test_provider.py::test_device_probe_failure_is_not_misreported_as_missing_usb',
            'integrations/ios-driver/tests/test_provider.py::test_link_activation_refuses_before_phone_or_network_changes_without_lan_block',
            'integrations/ios-driver/tests/test_provider.py::test_link_activation_never_auto_mounts_developer_image',
            'integrations/ios-driver/tests/test_provider.py::test_release_link_stops_only_transient_sirin_phone_processes',
            'integrations/ios-driver/tests/test_provider.py::test_release_link_fails_closed_when_process_or_listener_remains'
        )
        $results += Invoke-NativeVerification 'python_driver_tests' $pythonPath @(
            '-m', 'pytest', 'integrations/ios-driver/tests', '-q', '-p', 'no:cacheprovider'
        )
    }
    if ($cargoPath) {
        $results += Invoke-NativeVerification 'rust_ios_contracts' $cargoPath @(
            'test', '--bin', 'sirin', 'ios_device::', '--', '--test-threads=1'
        )
        $results += Invoke-NativeVerification 'rust_mcp_contracts' $cargoPath @(
            'test', '--bin', 'sirin', 'mcp_registry::tests::iphone', '--', '--test-threads=1'
        )
        $results += Invoke-NativeVerification 'cargo_check' $cargoPath @(
            'check', '--message-format=short'
        )
        if ($FullRust) {
            $results += Invoke-NativeVerification 'rust_full_regression' $cargoPath @(
                'test', '--bin', 'sirin', '--no-fail-fast', '--message-format=short',
                '--', '--test-threads=1'
            )
        }
    }
}
finally {
    if ($null -eq $oldNoBytecode) { Remove-Item Env:PYTHONDONTWRITEBYTECODE -ErrorAction SilentlyContinue }
    else { $env:PYTHONDONTWRITEBYTECODE = $oldNoBytecode }
    if ($null -eq $oldPluginAutoload) { Remove-Item Env:PYTEST_DISABLE_PLUGIN_AUTOLOAD -ErrorAction SilentlyContinue }
    else { $env:PYTEST_DISABLE_PLUGIN_AUTOLOAD = $oldPluginAutoload }
    if ($null -eq $oldCargoColor) { Remove-Item Env:CARGO_TERM_COLOR -ErrorAction SilentlyContinue }
    else { $env:CARGO_TERM_COLOR = $oldCargoColor }
}

$resultByName = @{}
foreach ($result in $results) { $resultByName[$result.name] = $result }
$switcherSource = Get-Content -LiteralPath (
    Join-Path $Repo 'scripts\switch-sirin-daemon.ps1'
) -Raw
$physicalVerifierSource = Get-Content -LiteralPath (
    Join-Path $Repo 'scripts\verify-sirin-ios-physical.ps1'
) -Raw
function Test-StepsPassed([string[]]$Names) {
    foreach ($name in $Names) {
        if (-not $resultByName.ContainsKey($name) -or -not $resultByName[$name].ok) {
            return $false
        }
    }
    return $true
}

function Test-StepEvidence([string]$Name, [string[]]$Patterns) {
    if (-not (Test-StepsPassed @($Name))) { return $false }
    $output = [string]$resultByName[$Name].stdout_tail
    foreach ($pattern in $Patterns) {
        if ($output -notmatch [Regex]::Escape($pattern)) { return $false }
    }
    return $true
}

$contracts = @(
    [pscustomobject]@{
        id = 'link_release_trust'
        ok = ((Test-StepEvidence 'python_driver_contracts' @(
            'test_safe_kill_requires_successful_exit_proof',
            'test_release_link_fails_closed_when_process_or_listener_remains'
        )) -and (Test-StepEvidence 'rust_ios_contracts' @(
            'expired_lease_posts_verified_link_release',
            'failed_start_releases_a_preexisting_provider_link'
        )))
        evidence = @(
            'test_safe_kill_requires_successful_exit_proof',
            'test_release_link_fails_closed_when_process_or_listener_remains',
            'expired_lease_posts_verified_link_release',
            'failed_start_releases_a_preexisting_provider_link'
        )
    },
    [pscustomobject]@{
        id = 'acceptance_success_judgement'
        ok = Test-StepEvidence 'rust_ios_contracts' @('provider_false_result_never_confirms_an_action')
        evidence = @('provider_false_result_never_confirms_an_action')
    },
    [pscustomobject]@{
        id = 'lease_ttl_cleanup'
        ok = Test-StepEvidence 'rust_ios_contracts' @(
            'expired_lease_is_taken_for_link_cleanup',
            'expired_lease_posts_verified_link_release'
        )
        evidence = @('expired_lease_is_taken_for_link_cleanup', 'expired_lease_posts_verified_link_release')
    },
    [pscustomobject]@{
        id = 'passive_health_model'
        ok = ((Test-StepEvidence 'python_driver_contracts' @(
            'test_passive_status_does_not_probe_wda_when_usb_is_absent'
        )) -and (Test-StepEvidence 'rust_ios_contracts' @(
            'provider_health_requires_current_passive_safety_contract',
            'passive_link_is_not_misreported_as_a_human_blocker'
        )))
        evidence = @(
            'test_passive_status_does_not_probe_wda_when_usb_is_absent',
            'provider_health_requires_current_passive_safety_contract',
            'passive_link_is_not_misreported_as_a_human_blocker'
        )
    },
    [pscustomobject]@{
        id = 'lan_protection_validation'
        ok = Test-StepEvidence 'python_driver_contracts' @(
            'test_lan_block_rule_requires_exact_fail_closed_rule_shape',
            'test_link_activation_refuses_before_phone_or_network_changes_without_lan_block'
        )
        evidence = @(
            'test_lan_block_rule_requires_exact_fail_closed_rule_shape',
            'test_link_activation_refuses_before_phone_or_network_changes_without_lan_block'
        )
    },
    [pscustomobject]@{
        id = 'ddi_and_network_transparency'
        ok = ((Test-StepEvidence 'python_driver_contracts' @(
            'test_link_activation_never_auto_mounts_developer_image'
        )) -and (Test-StepsPassed @('powershell_source_parse')))
        evidence = @('test_link_activation_never_auto_mounts_developer_image', 'NEEDS_HUMAN_DDI contract')
    },
    [pscustomobject]@{
        id = 'route_bound_idempotency'
        ok = ((Test-StepEvidence 'rust_ios_contracts' @(
            'acceptance_run_id_rejects_a_different_route'
        )) -and (Test-StepsPassed @('rust_mcp_contracts')))
        evidence = @('acceptance_run_id_rejects_a_different_route', 'process-lifetime run_id contract')
    },
    [pscustomobject]@{
        id = 'public_stop_cannot_keep_warm_link'
        ok = ((Test-StepEvidence 'rust_ios_contracts' @(
            'public_stop_rejects_retaining_a_warm_link'
        )) -and (Test-StepEvidence 'rust_mcp_contracts' @(
            'iphone_session_stop_cannot_retain_a_warm_link'
        )))
        evidence = @('public_stop_rejects_retaining_a_warm_link', 'iphone_session_stop_cannot_retain_a_warm_link')
    },
    [pscustomobject]@{
        id = 'candidate_smoke_isolation'
        ok = (
            $switcherSource -match [Regex]::Escape("-ArgumentList @('--mcp-only')") -and
            $switcherSource -match [Regex]::Escape('$env:SIRIN_IOS_DRIVER_URL = "http://127.0.0.1:$IosDriverSmokePort"') -and
            $switcherSource -match [Regex]::Escape("`$env:SIRIN_IOS_DRIVER_AUTOSTART = '0'") -and
            $switcherSource -match [Regex]::Escape('Test-IosMcpFailClosedContract') -and
            $switcherSource -match [Regex]::Escape("'BLOCKED_TOOL_REGRESSION'") -and
            $switcherSource -match [Regex]::Escape('candidate_deployable = -not $toolRegression') -and
            $switcherSource -notmatch [Regex]::Escape('--ai-monitor-only')
        )
        evidence = @(
            '--mcp-only candidate process',
            'unused loopback iPhone Driver port',
            'Driver autostart disabled',
            'iPhone unavailable-provider fail-closed contract',
            'tool regression blocks deployment'
        )
    },
    [pscustomobject]@{
        id = 'immutable_daemon_deployment'
        ok = (
            $switcherSource -match [Regex]::Escape('Stage-ImmutableCandidate') -and
            $switcherSource -match [Regex]::Escape('"sirin-$($Sha256.Substring(0, 12))"') -and
            $switcherSource -match [Regex]::Escape('$liveBinary = Get-TaskBinary $taskAction') -and
            $switcherSource -match [Regex]::Escape('Set-TaskActionFromSnapshot $newTaskAction') -and
            $switcherSource -match [Regex]::Escape('Write-DeploymentManifest') -and
            $switcherSource -match [Regex]::Escape('Restore-Backup $backup') -and
            $switcherSource -match [Regex]::Escape('listener_path_matches_task = $true') -and
            $switcherSource -notmatch [Regex]::Escape("`$liveBinary = Join-Path `$Repo 'target\release\sirin.exe'")
        )
        evidence = @(
            'reviewed-SHA deployment directory',
            'task-owned live path discovery',
            'single scheduled-task action switch',
            'deployment manifest',
            'automatic rollback',
            'listener path and artifact hash verification',
            'repository build output is not the live path'
        )
    },
    [pscustomobject]@{
        id = 'physical_verifier_fail_closed'
        ok = (
            $physicalVerifierSource -match [Regex]::Escape("[string]`$Action = 'Status'") -and
            $physicalVerifierSource -match [Regex]::Escape('Acceptance requires the explicit -AllowPhoneAction switch.') -and
            $physicalVerifierSource -match [Regex]::Escape('Acceptance requires a stable caller-chosen -RunId') -and
            $physicalVerifierSource -match [Regex]::Escape("-ArgumentList @('--mcp-only')") -and
            $physicalVerifierSource -match [Regex]::Escape("'--attach-mode', 'passive'") -and
            $physicalVerifierSource -match [Regex]::Escape('current_services_replaced = $false') -and
            $physicalVerifierSource.IndexOf("`$initialStatus = Invoke-IosTool 2 'ios_device_status'", [System.StringComparison]::Ordinal) -lt
                $physicalVerifierSource.IndexOf("`$acceptance = Invoke-IosTool 3 'ios_acceptance_run'", [System.StringComparison]::Ordinal) -and
            $physicalVerifierSource -match [Regex]::Escape('cleanup.proven')
        )
        evidence = @(
            'passive Status default',
            'explicit phone-action and stable run-id gates',
            'alternate MCP/Driver only',
            'status-before-action order',
            'post-run process/listener cleanup proof'
        )
    }
)

$gitHead = $null
$gitBranch = $null
$gitDirtyCount = $null
try {
    Push-Location $Repo
    try {
        $gitHead = (& git rev-parse HEAD 2>$null | Select-Object -First 1)
        $gitBranch = (& git branch --show-current 2>$null | Select-Object -First 1)
        $gitDirtyCount = @(& git status --porcelain 2>$null).Count
    }
    finally { Pop-Location }
}
catch {}

$allSourcesPresent = @($sourceEvidence | Where-Object { -not $_.exists }).Count -eq 0
$allStepsPassed = @($results | Where-Object { -not $_.ok }).Count -eq 0
$allContractsPassed = @($contracts | Where-Object { -not $_.ok }).Count -eq 0
$report = [ordered]@{
    schema = 'sirin-ios-source-verification/v1'
    generated_at = [DateTime]::UtcNow.ToString('o')
    ok = ($allSourcesPresent -and $allStepsPassed -and $allContractsPassed)
    scope = [ordered]@{
        source_and_mock_tests_only = $true
        phone_contacted = $false
        driver_http_contacted = $false
        firewall_changed = $false
        phone_settings_changed = $false
        services_changed = $false
        dependencies_installed = $false
        full_rust_requested = [bool]$FullRust
    }
    source_identity = [ordered]@{
        repo = $Repo
        git_head = $gitHead
        git_branch = $gitBranch
        git_dirty_entry_count = $gitDirtyCount
    }
    source_files = $sourceEvidence
    steps = $results
    contracts = $contracts
}

$parent = Split-Path -Parent $OutputPath
if (-not (Test-Path -LiteralPath $parent -PathType Container)) {
    [void](New-Item -ItemType Directory -Path $parent -Force)
}
$report | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $OutputPath -Encoding UTF8
$report | ConvertTo-Json -Depth 10
Write-Host "Sirin iOS source verification report: $OutputPath"
if (-not $report.ok) { exit 1 }
