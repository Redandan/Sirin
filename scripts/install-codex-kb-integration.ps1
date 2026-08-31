#requires -Version 5.1
<#
Install or inspect Sirin's shared Codex KB integration.

Sirin owns the canonical skill under integrations\codex. The copy under the
Codex home is generated so desktop, CLI, and IDE sessions use the same
Sirin-routed KB workflow.
#>

[CmdletBinding()]
param(
    [ValidateSet('Install', 'Status')]
    [string]$Action = 'Status',
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$CodexHome = (Join-Path $env:USERPROFILE '.codex')
)

$ErrorActionPreference = 'Stop'

$sourceSkill = Join-Path $Repo 'integrations\codex\skills\kb-mcp'
$targetSkill = Join-Path $CodexHome 'skills\kb-mcp'
$managedFiles = @(
    'SKILL.md',
    'agents\openai.yaml',
    'references\tool-map.md'
)

function Get-FileHashValue([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

if (-not (Test-Path -LiteralPath $sourceSkill -PathType Container)) {
    throw "canonical Sirin KB skill is missing: $sourceSkill"
}

if ($Action -eq 'Install') {
    foreach ($relative in $managedFiles) {
        $source = Join-Path $sourceSkill $relative
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "canonical Sirin KB skill file is missing: $source"
        }
        $target = Join-Path $targetSkill $relative
        $targetDir = Split-Path -Parent $target
        New-Item -ItemType Directory -Force -Path $targetDir | Out-Null
        Copy-Item -LiteralPath $source -Destination $target -Force
    }
}

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

[pscustomobject]@{
    status = if (@($fileStates | Where-Object { -not $_.in_sync }).Count -eq 0) { 'READY' } else { 'NEEDS_INSTALL' }
    source = $sourceSkill
    target = $targetSkill
    files = @($fileStates)
    restart_required = $true
} | ConvertTo-Json -Depth 5
