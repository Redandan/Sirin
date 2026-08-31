#requires -Version 5.1
<#
Run a bounded physical-iPhone verification through an alternate Sirin MCP and
an alternate passive Sirin iOS Driver. The current 7700/8770 services are never
stopped, replaced, or called.

Status is passive and performs only ios_device_status. Acceptance additionally
requires -AllowPhoneAction plus a caller-chosen stable -RunId, then invokes one
fixed ios_acceptance_run and verifies lease/link cleanup afterward.
#>

[CmdletBinding()]
param(
    [ValidateSet('Status', 'Acceptance')]
    [string]$Action = 'Status',
    [string]$Repo = '',
    [string]$CandidateBinary = '',
    [string]$Python = '',
    [string]$GoIos = '',
    [ValidateSet('safari_store', 'telegram_bot', 'codex_remote')]
    [string]$Route = 'codex_remote',
    [string]$RunId = '',
    [switch]$AllowPhoneAction,
    [int]$McpPort = 17700,
    [int]$DriverPort = 18770,
    [ValidateRange(5, 120)]
    [int]$TimeoutSeconds = 30,
    [string]$OutputPath = ''
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($Repo)) {
    $Repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
}
$Repo = [System.IO.Path]::GetFullPath($Repo)
if ([string]::IsNullOrWhiteSpace($CandidateBinary)) {
    $CandidateBinary = Join-Path $Repo 'target\debug\sirin.exe'
}
if ([string]::IsNullOrWhiteSpace($Python)) {
    $bundled = Join-Path $env:TEMP 'sirin-ios-verifier-venv\Scripts\python.exe'
    if (Test-Path -LiteralPath $bundled -PathType Leaf) {
        $Python = $bundled
    }
    else {
        $command = Get-Command python -ErrorAction SilentlyContinue
        if ($command) { $Python = $command.Source }
    }
}
if ([string]::IsNullOrWhiteSpace($GoIos)) {
    $GoIos = Join-Path $env:LOCALAPPDATA 'Sirin\ios-driver\bin\ios.exe'
}
if ($Action -eq 'Acceptance') {
    if (-not $AllowPhoneAction) {
        throw 'Acceptance requires the explicit -AllowPhoneAction switch.'
    }
    if ([string]::IsNullOrWhiteSpace($RunId)) {
        throw 'Acceptance requires a stable caller-chosen -RunId for idempotent retry.'
    }
}
foreach ($required in @($CandidateBinary, $Python, $GoIos)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "required file is missing: $required"
    }
}
if ($McpPort -eq $DriverPort -or $McpPort -in @(7700, 8770) -or $DriverPort -in @(7700, 8770)) {
    throw 'Alternate MCP/Driver ports must be distinct and cannot be 7700 or 8770.'
}

$nonce = [Guid]::NewGuid().ToString('N')
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $OutputPath = Join-Path $env:TEMP "sirin-ios-physical-verification-$nonce.json"
}
$OutputPath = [System.IO.Path]::GetFullPath($OutputPath)
$runtimeRoot = Join-Path $env:TEMP "sirin-ios-physical-runtime-$nonce"
$driverSource = Join-Path $Repo 'integrations\ios-driver\scripts\unattended_host.py'
$driverStdout = Join-Path $env:TEMP "sirin-ios-physical-$nonce.driver.out.log"
$driverStderr = Join-Path $env:TEMP "sirin-ios-physical-$nonce.driver.err.log"
$mcpStdout = Join-Path $env:TEMP "sirin-ios-physical-$nonce.mcp.out.log"
$mcpStderr = Join-Path $env:TEMP "sirin-ios-physical-$nonce.mcp.err.log"

function Get-Listener([int]$Port) {
    Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue |
        Select-Object -First 1
}

function Wait-Listener([int]$Port, [System.Diagnostics.Process]$Process) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $listener = Get-Listener $Port
        if ($listener) { return $listener }
        if ($Process.HasExited) {
            throw "process exited before port $Port became ready (exit=$($Process.ExitCode))"
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    throw "port $Port did not become ready within $TimeoutSeconds seconds"
}

function Invoke-Mcp([int]$Id, [string]$Method, $Params) {
    $body = [ordered]@{ jsonrpc = '2.0'; id = $Id; method = $Method }
    if ($null -ne $Params) { $body.params = $Params }
    Invoke-RestMethod `
        -Uri "http://127.0.0.1:$McpPort/mcp" `
        -Method Post `
        -ContentType 'application/json' `
        -Body ($body | ConvertTo-Json -Depth 12 -Compress) `
        -TimeoutSec $TimeoutSeconds
}

function Wait-Mcp {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        try {
            return Invoke-Mcp 1 'tools/list' $null
        }
        catch {
            Start-Sleep -Milliseconds 250
        }
    } while ((Get-Date) -lt $deadline)
    throw "alternate Sirin MCP did not become ready on port $McpPort"
}

function Invoke-IosTool([int]$Id, [string]$Name, $Arguments) {
    $response = Invoke-Mcp $Id 'tools/call' @{
        name = $Name
        arguments = $Arguments
    }
    if ($response.error) {
        throw "Sirin MCP $Name failed: $($response.error.message)"
    }
    $text = @($response.result.content | Where-Object { $_.type -eq 'text' } |
        Select-Object -First 1).text
    if ([string]::IsNullOrWhiteSpace([string]$text)) {
        throw "Sirin MCP $Name omitted its structured text result"
    }
    try { $text | ConvertFrom-Json }
    catch { throw "Sirin MCP $Name returned invalid JSON: $($_.Exception.Message)" }
}

function Get-PassiveDecision($Status) {
    $contractReady = (
        [string]$Status.provider_contract_status -eq 'PASS' -and
        [bool]$Status.provider.provider_reachable -and
        [string]$Status.provider.provider -eq 'sirin-ios-driver' -and
        [bool]$Status.provider.acceptance_only -and
        [string]$Status.provider.attach_mode -eq 'passive' -and
        $Status.provider.lan_exposed -eq $false -and
        $null -eq $Status.active_lease -and
        -not [bool]$Status.link_cleanup_required -and
        -not [bool]$Status.lifecycle.manager_loop_started
    )
    $deviceReady = [string]$Status.capabilities.DEVICE_DETECTED.status -eq 'PASS'
    $infoReady = [string]$Status.capabilities.INFO_READABLE.status -eq 'PASS'
    [pscustomobject]@{
        contract_ready = $contractReady
        device_ready = $deviceReady
        info_ready = $infoReady
        status_ready = $contractReady -and $deviceReady -and $infoReady
        acceptance_preconditions_ready = $contractReady -and $deviceReady
    }
}

function Stop-StartedProcess([System.Diagnostics.Process]$Process) {
    if ($null -eq $Process) { return }
    $current = Get-Process -Id $Process.Id -ErrorAction SilentlyContinue
    if ($current) {
        Stop-Process -Id $current.Id -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $current.Id -Timeout 10 -ErrorAction SilentlyContinue
    }
}

foreach ($port in @($McpPort, $DriverPort)) {
    if (Get-Listener $port) { throw "alternate port is already in use: $port" }
}
if ($Action -eq 'Acceptance') {
    foreach ($port in @(8100, 9100)) {
        if (Get-Listener $port) {
            throw "phone forward port is already in use; refusing alternate acceptance: $port"
        }
    }
}

$previous = [ordered]@{
    go_ios = $env:SIRIN_IOS_GO_IOS_PATH
    env_file = $env:SIRIN_ENV_FILE
    rpc_port = $env:SIRIN_RPC_PORT
    driver_url = $env:SIRIN_IOS_DRIVER_URL
    autostart = $env:SIRIN_IOS_DRIVER_AUTOSTART
}
$driverProcess = $null
$mcpProcess = $null
$initialStatus = $null
$finalStatus = $null
$decision = $null
$acceptance = $null
$failure = $null
$cleanup = $null

try {
    $env:SIRIN_IOS_GO_IOS_PATH = (Resolve-Path -LiteralPath $GoIos).Path
    $env:SIRIN_ENV_FILE = Join-Path $runtimeRoot 'no-user-env'
    $driverArgs = @(
        "`"$driverSource`"",
        '--port', [string]$DriverPort,
        '--runtime-root', "`"$runtimeRoot`"",
        '--attach-mode', 'passive',
        '--health-poll-seconds', '3600'
    )
    $driverProcess = Start-Process `
        -FilePath $Python `
        -ArgumentList $driverArgs `
        -WorkingDirectory $Repo `
        -RedirectStandardOutput $driverStdout `
        -RedirectStandardError $driverStderr `
        -WindowStyle Hidden `
        -PassThru
    Wait-Listener $DriverPort $driverProcess | Out-Null

    $env:SIRIN_RPC_PORT = [string]$McpPort
    $env:SIRIN_IOS_DRIVER_URL = "http://127.0.0.1:$DriverPort"
    $env:SIRIN_IOS_DRIVER_AUTOSTART = '0'
    $mcpProcess = Start-Process `
        -FilePath $CandidateBinary `
        -ArgumentList @('--mcp-only') `
        -WorkingDirectory $Repo `
        -RedirectStandardOutput $mcpStdout `
        -RedirectStandardError $mcpStderr `
        -WindowStyle Hidden `
        -PassThru
    Wait-Mcp | Out-Null

    $initialStatus = Invoke-IosTool 2 'ios_device_status' @{}
    $decision = Get-PassiveDecision $initialStatus

    if ($Action -eq 'Acceptance') {
        if (-not $decision.acceptance_preconditions_ready) {
            $acceptance = [pscustomobject]@{
                ok = $false
                error = 'MISSING_PROOF: passive provider/device preconditions are incomplete'
            }
        }
        else {
            $acceptance = Invoke-IosTool 3 'ios_acceptance_run' @{
                run_id = $RunId
                route = $Route
                owner = 'sirin-physical-verifier'
                ttl_secs = 120
                settle_ms = 3000
            }
        }
        $finalStatus = Invoke-IosTool 4 'ios_device_status' @{}
    }
}
catch {
    $failure = $_.Exception.Message
}
finally {
    Stop-StartedProcess $mcpProcess
    Stop-StartedProcess $driverProcess

    $env:SIRIN_IOS_GO_IOS_PATH = $previous.go_ios
    $env:SIRIN_ENV_FILE = $previous.env_file
    $env:SIRIN_RPC_PORT = $previous.rpc_port
    $env:SIRIN_IOS_DRIVER_URL = $previous.driver_url
    $env:SIRIN_IOS_DRIVER_AUTOSTART = $previous.autostart

    $leftoverPorts = @(($McpPort, $DriverPort, 8100, 9100) | Where-Object {
        $null -ne (Get-Listener $_)
    })
    $livePidRecords = @()
    $stateDir = Join-Path $runtimeRoot 'state'
    if (Test-Path -LiteralPath $stateDir -PathType Container) {
        foreach ($file in Get-ChildItem -LiteralPath $stateDir -Filter '*.pid' -File) {
            $rawPid = Get-Content -LiteralPath $file.FullName -Raw -ErrorAction SilentlyContinue
            $parsedPid = 0
            if ([int]::TryParse(([string]$rawPid).Trim(), [ref]$parsedPid) -and
                (Get-Process -Id $parsedPid -ErrorAction SilentlyContinue)) {
                $livePidRecords += [pscustomobject]@{ path = $file.FullName; pid = $parsedPid }
            }
        }
    }
    $cleanupProven = $leftoverPorts.Count -eq 0 -and $livePidRecords.Count -eq 0
    $runtimeRemoved = $false
    if ($cleanupProven -and (Test-Path -LiteralPath $runtimeRoot)) {
        $tempPrefix = [System.IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
        $resolvedRuntime = [System.IO.Path]::GetFullPath($runtimeRoot)
        if ($resolvedRuntime.StartsWith(
            $tempPrefix,
            [System.StringComparison]::OrdinalIgnoreCase
        )) {
            Remove-Item -LiteralPath $resolvedRuntime -Recurse -Force
            $runtimeRemoved = -not (Test-Path -LiteralPath $resolvedRuntime)
        }
    }
    $cleanup = [pscustomobject]@{
        proven = $cleanupProven
        leftover_ports = $leftoverPorts
        live_pid_records = $livePidRecords
        runtime_root = $runtimeRoot
        runtime_removed = $runtimeRemoved
    }
}

$acceptancePass = (
    $Action -eq 'Acceptance' -and
    $null -eq $failure -and
    $acceptance.ok -eq $true -and
    $acceptance.lease_release.ok -eq $true -and
    $null -ne $finalStatus -and
    $null -eq $finalStatus.active_lease -and
    -not [bool]$finalStatus.link_cleanup_required -and
    $cleanup.proven
)
$reportStatus = if ($failure) {
    'ERROR'
}
elseif ($Action -eq 'Acceptance') {
    if ($acceptancePass) { 'PASS' } else { 'MISSING_PROOF' }
}
elseif ($decision.status_ready) {
    'READY'
}
else {
    'MISSING_PROOF'
}

$report = [ordered]@{
    schema = 'sirin-ios-physical-verification/v1'
    generated_at = [DateTime]::UtcNow.ToString('o')
    status = $reportStatus
    action = $Action
    route = if ($Action -eq 'Acceptance') { $Route } else { $null }
    run_id = if ($Action -eq 'Acceptance') { $RunId } else { $null }
    identity = [ordered]@{
        repo = $Repo
        candidate = (Resolve-Path -LiteralPath $CandidateBinary).Path
        candidate_sha256 = (Get-FileHash -LiteralPath $CandidateBinary -Algorithm SHA256).Hash.ToLowerInvariant()
        driver_source_sha256 = (Get-FileHash -LiteralPath $driverSource -Algorithm SHA256).Hash.ToLowerInvariant()
        go_ios_sha256 = (Get-FileHash -LiteralPath $GoIos -Algorithm SHA256).Hash.ToLowerInvariant()
        mcp_port = $McpPort
        driver_port = $DriverPort
    }
    safety = [ordered]@{
        alternate_services_only = $true
        current_services_replaced = $false
        firewall_changed = $false
        phone_settings_changed = $false
        signing_supported = $false
        install_to_phone_supported = $false
        driver_attach_mode = 'passive'
        phone_action_explicitly_enabled = [bool]$AllowPhoneAction
    }
    decision = $decision
    initial_status = $initialStatus
    acceptance = $acceptance
    final_status = $finalStatus
    cleanup = $cleanup
    error = $failure
    logs = [ordered]@{
        driver_stdout = $driverStdout
        driver_stderr = $driverStderr
        mcp_stdout = $mcpStdout
        mcp_stderr = $mcpStderr
    }
}
$json = $report | ConvertTo-Json -Depth 14
[System.IO.File]::WriteAllText($OutputPath, $json, [System.Text.UTF8Encoding]::new($false))
$json

if ($reportStatus -eq 'ERROR' -or -not $cleanup.proven) { exit 1 }
if ($Action -eq 'Acceptance' -and -not $acceptancePass) { exit 2 }
