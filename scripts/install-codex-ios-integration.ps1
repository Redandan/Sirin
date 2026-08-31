#requires -Version 5.1
<#
Install or inspect Sirin's shared Codex iPhone integration.

Sirin owns the canonical skill under integrations\codex. The copy under the
Codex home is generated so local desktop, CLI, and IDE sessions use the same
Sirin-only workflow.
#>

[CmdletBinding()]
param(
    [ValidateSet('Install', 'Status')]
    [string]$Action = 'Status',
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$CodexHome = (Join-Path $env:USERPROFILE '.codex'),
    [bool]$ManageProjectConfig = $true
)

$ErrorActionPreference = 'Stop'

$sourceSkill = Join-Path $Repo 'integrations\codex\skills\iphone-usb-validation'
$targetSkill = Join-Path $CodexHome 'skills\iphone-usb-validation'
$configPath = Join-Path $CodexHome 'config.toml'
$projectConfigPath = Join-Path $Repo '.codex\config.toml'
$managedFiles = @(
    'SKILL.md',
    'agents\openai.yaml',
    'references\agoramarket-acceptance.md',
    'references\codex-iphone-remote-acceptance.md'
)

function Get-FileHashValue([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-SirinConfigState([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return 'MISSING_CONFIG'
    }
    $raw = Get-Content -LiteralPath $Path -Raw
    $match = [regex]::Match(
        $raw,
        '(?ms)^\[mcp_servers\.sirin\]\s*(?<body>.*?)(?=^\[|\z)'
    )
    if (-not $match.Success) {
        return 'MISSING_MCP'
    }
    if ($match.Groups['body'].Value -notmatch '(?m)^url\s*=\s*["'']http://127\.0\.0\.1:7700/mcp["'']\s*$') {
        return 'WRONG_URL'
    }
    'READY'
}

function Ensure-ProjectSirinConfig {
    $projectConfigDir = Split-Path -Parent $projectConfigPath
    New-Item -ItemType Directory -Force -Path $projectConfigDir | Out-Null
    if (-not (Test-Path -LiteralPath $projectConfigPath -PathType Leaf)) {
        New-Item -ItemType File -Path $projectConfigPath | Out-Null
    }
    $raw = Get-Content -LiteralPath $projectConfigPath -Raw
    $match = [regex]::Match(
        $raw,
        '(?ms)^\[mcp_servers\.sirin\]\s*(?<body>.*?)(?=^\[|\z)'
    )
    if (-not $match.Success) {
        Add-Content -LiteralPath $projectConfigPath -Encoding UTF8 -Value @'

[mcp_servers.sirin]
url = "http://127.0.0.1:7700/mcp"
'@
        return
    }
    if ($match.Groups['body'].Value -match '(?m)^url\s*=\s*["'']http://127\.0\.0\.1:7700/mcp["'']\s*$') {
        return
    }
    $block = $match.Value
    if ($block -notmatch '(?m)^url\s*=') {
        throw 'Sirin project MCP block has no URL and requires manual review'
    }
    $updatedBlock = [regex]::Replace(
        $block,
        '(?m)^url\s*=.*$',
        'url = "http://127.0.0.1:7700/mcp"',
        1
    )
    $updated = $raw.Remove($match.Index, $match.Length).Insert($match.Index, $updatedBlock)
    Set-Content -LiteralPath $projectConfigPath -Encoding UTF8 -Value $updated
}

function Get-IntegrationStatus {
    $fileStates = foreach ($relative in $managedFiles) {
        $source = Join-Path $sourceSkill $relative
        $target = Join-Path $targetSkill $relative
        $sourceHash = Get-FileHashValue $source
        $targetHash = Get-FileHashValue $target
        [pscustomobject]@{
            file = $relative
            source_exists = $null -ne $sourceHash
            target_exists = $null -ne $targetHash
            in_sync = $null -ne $sourceHash -and $sourceHash -eq $targetHash
        }
    }
    $configState = Get-SirinConfigState $configPath
    $projectConfigState = if ($ManageProjectConfig) { Get-SirinConfigState $projectConfigPath } else { 'NOT_MANAGED' }
    [pscustomobject]@{
        status = if (@($fileStates | Where-Object { -not $_.in_sync }).Count -eq 0 -and
            $configState -eq 'READY' -and
            (-not $ManageProjectConfig -or $projectConfigState -eq 'READY')) { 'READY' } else { 'NEEDS_INSTALL' }
        mcp_config = $configState
        project_mcp_config = $projectConfigState
        project_config = $projectConfigPath
        source = $sourceSkill
        target = $targetSkill
        files = @($fileStates)
        restart_required = $true
    }
}

if (-not (Test-Path -LiteralPath $sourceSkill -PathType Container)) {
    throw "canonical Sirin skill is missing: $sourceSkill"
}

if ($Action -eq 'Install') {
    foreach ($relative in $managedFiles) {
        $source = Join-Path $sourceSkill $relative
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "canonical skill file is missing: $source"
        }
        $target = Join-Path $targetSkill $relative
        $targetDir = Split-Path -Parent $target
        New-Item -ItemType Directory -Force -Path $targetDir | Out-Null
        Copy-Item -LiteralPath $source -Destination $target -Force
    }

    $configState = Get-SirinConfigState $configPath
    if ($configState -eq 'MISSING_CONFIG') {
        New-Item -ItemType Directory -Force -Path $CodexHome | Out-Null
        New-Item -ItemType File -Path $configPath | Out-Null
        $configState = 'MISSING_MCP'
    }
    if ($configState -eq 'MISSING_MCP') {
        Add-Content -LiteralPath $configPath -Encoding UTF8 -Value @'

[mcp_servers.sirin]
url = "http://127.0.0.1:7700/mcp"
startup_timeout_sec = 10
tool_timeout_sec = 60
enabled = true
'@
    }
    elseif ($configState -ne 'READY') {
        throw "Codex Sirin MCP config requires manual review: $configState"
    }
    if ($ManageProjectConfig) {
        Ensure-ProjectSirinConfig
    }
}

Get-IntegrationStatus | ConvertTo-Json -Depth 5
