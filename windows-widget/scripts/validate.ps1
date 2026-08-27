[CmdletBinding()]
param(
    [string]$BuildRoot = ''
)

$ErrorActionPreference = 'Stop'
$widgetRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$BuildRoot = if ([string]::IsNullOrWhiteSpace($BuildRoot)) {
    $widgetRoot
} else {
    [IO.Path]::GetFullPath($BuildRoot)
}
$templatePath = Join-Path $widgetRoot 'Templates\SirinAiMonitorWidget.json'
$manifestPath = Join-Path $widgetRoot 'Package.appxmanifest'
$builtPreviewPath = Join-Path $BuildRoot 'out\layout\ProviderAssets\SirinAIWork_Screenshot.png'

$templateText = Get-Content -LiteralPath $templatePath -Raw -Encoding UTF8
$template = $templateText | ConvertFrom-Json
[xml]$manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8

$forbidden = @('task.title', 'message', 'prompt', 'command_line', 'credentials')
$hits = @($forbidden | Where-Object { $templateText -match [regex]::Escape($_) })
if ($hits.Count -gt 0) {
    throw "Forbidden content bindings found: $($hits -join ', ')"
}
if ($template.type -ne 'AdaptiveCard' -or $template.version -ne '1.5') {
    throw 'Unexpected Adaptive Card type or version.'
}
$requiredBindings = @(
    '${workState}',
    '${workDetail}',
    '${activeSummary}',
    '${tokenActivity}',
    '${tokenSparkline}',
    '${tokenWindowTotal}',
    '${task1Meta}',
    '${powerStatus}',
    '${sessionStatus}',
    '${healthAlert}',
    '${networkSummary}'
)
$missingBindings = @($requiredBindings | Where-Object { -not $templateText.Contains($_) })
if ($missingBindings.Count -gt 0) {
    throw "Required work-status bindings missing: $($missingBindings -join ', ')"
}
if ($null -eq $manifest.Package.Applications.Application.Extensions) {
    throw 'Widget package extensions are missing.'
}
$previewReady = $false
if (Test-Path -LiteralPath $builtPreviewPath) {
    Add-Type -AssemblyName System.Drawing
    $preview = [System.Drawing.Bitmap]::FromFile($builtPreviewPath)
    try {
        $previewReady = $preview.Width -eq 300 -and
            $preview.Height -eq 304 -and
            $preview.GetPixel(0, 0).A -eq 0
    } finally {
        $preview.Dispose()
    }
}

[pscustomobject]@{
    status = 'VALID'
    adaptive_card_version = $template.version
    action_count = @($template.actions).Count
    forbidden_binding_hits = $hits.Count
    work_status_bindings = $requiredBindings.Count
    package_name = $manifest.Package.Identity.Name
    publisher = $manifest.Package.Identity.Publisher
    picker_preview_ready = $previewReady
} | ConvertTo-Json -Compress
