[CmdletBinding()]
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release',
    [string]$BuildRoot = ''
)

$ErrorActionPreference = 'Stop'
$widgetRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$projectRoot = Join-Path $widgetRoot 'SirinWidgetProvider'
$project = Join-Path $projectRoot 'SirinWidgetProvider.vcxproj'
$BuildRoot = if ([string]::IsNullOrWhiteSpace($BuildRoot)) {
    $widgetRoot
} else {
    [IO.Path]::GetFullPath($BuildRoot)
}
$packagesRoot = [IO.Path]::GetFullPath((Join-Path $BuildRoot 'packages'))
$intermediateRoot = [IO.Path]::GetFullPath((Join-Path $BuildRoot "obj\$Configuration")) + [IO.Path]::DirectorySeparatorChar
$buildOutput = [IO.Path]::GetFullPath((Join-Path $BuildRoot "bin\$Configuration")) + [IO.Path]::DirectorySeparatorChar
$outRoot = [IO.Path]::GetFullPath((Join-Path $BuildRoot 'out'))
$layout = [IO.Path]::GetFullPath((Join-Path $outRoot 'layout'))

if (-not $layout.StartsWith($outRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Unsafe layout path: $layout"
}

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path -LiteralPath $vswhere)) {
    throw 'Visual Studio Installer discovery tool was not found.'
}
$installation = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $installation) {
    throw 'Visual Studio C++ Build Tools are not installed.'
}
$msbuild = Join-Path $installation 'MSBuild\Current\Bin\amd64\MSBuild.exe'
if (-not (Test-Path -LiteralPath $msbuild)) {
    throw "MSBuild was not found: $msbuild"
}

& $msbuild $project `
    /restore `
    /p:RestorePackagesConfig=true `
    /p:RestorePackagesPath=$packagesRoot `
    /p:RestoreRepositoryPath=$packagesRoot `
    /p:NugetPackageDirectory=$packagesRoot `
    /p:IntDir=$intermediateRoot `
    /p:OutDir=$buildOutput `
    /p:Configuration=$Configuration `
    /p:Platform=x64 `
    /p:EnableCoreMrtTooling=false `
    /m:1 `
    /verbosity:minimal
if ($LASTEXITCODE -ne 0) {
    throw "Sirin Widget provider build failed with exit code $LASTEXITCODE"
}

if (Test-Path -LiteralPath $layout) {
    Remove-Item -LiteralPath $layout -Recurse -Force
}
$providerLayout = Join-Path $layout 'SirinWidgetProvider'
$templateLayout = Join-Path $layout 'Templates'
$imagesLayout = Join-Path $layout 'Images'
$providerAssetsLayout = Join-Path $layout 'ProviderAssets'
@($layout, $providerLayout, $templateLayout, $imagesLayout, $providerAssetsLayout) |
    ForEach-Object { New-Item -ItemType Directory -Path $_ -Force | Out-Null }

$providerExe = Join-Path $buildOutput 'SirinWidgetProvider.exe'
if (-not (Test-Path -LiteralPath $providerExe)) {
    throw "Provider executable was not produced: $providerExe"
}
Copy-Item -LiteralPath $providerExe -Destination $providerLayout
Get-ChildItem -LiteralPath $buildOutput -Filter '*.dll' -File -ErrorAction SilentlyContinue |
    ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $providerLayout }
Copy-Item -LiteralPath (Join-Path $widgetRoot 'Package.appxmanifest') -Destination (Join-Path $layout 'AppxManifest.xml')
Copy-Item -LiteralPath (Join-Path $widgetRoot 'Templates\SirinAiMonitorWidget.json') -Destination $templateLayout

Add-Type -AssemblyName System.Drawing
function New-SirinAsset {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][int]$Width,
        [Parameter(Mandatory = $true)][int]$Height,
        [switch]$Preview
    )
    $bitmap = New-Object System.Drawing.Bitmap $Width, $Height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $accent = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(0, 255, 163))
        $white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(245, 245, 245))
        $dim = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(145, 145, 145))
        $background = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 26, 26, 26))
        try {
            if ($Preview) {
                $graphics.Clear([System.Drawing.Color]::Transparent)
                $corner = 22
                $diameter = $corner * 2
                $cardPath = New-Object System.Drawing.Drawing2D.GraphicsPath
                try {
                    $cardPath.AddArc(0, 0, $diameter, $diameter, 180, 90)
                    $cardPath.AddArc($Width - $diameter - 1, 0, $diameter, $diameter, 270, 90)
                    $cardPath.AddArc($Width - $diameter - 1, $Height - $diameter - 1, $diameter, $diameter, 0, 90)
                    $cardPath.AddArc(0, $Height - $diameter - 1, $diameter, $diameter, 90, 90)
                    $cardPath.CloseFigure()
                    $graphics.FillPath($background, $cardPath)
                    $graphics.SetClip($cardPath)
                } finally {
                    $cardPath.Dispose()
                }
                $graphics.FillRectangle($accent, 18, 18, 8, 8)
                $titleFont = New-Object System.Drawing.Font 'Segoe UI', 16, ([System.Drawing.FontStyle]::Bold)
                $valueFont = New-Object System.Drawing.Font 'Consolas', 25, ([System.Drawing.FontStyle]::Bold)
                $chartFont = New-Object System.Drawing.Font 'Segoe UI Symbol', 23, ([System.Drawing.FontStyle]::Bold)
                $smallFont = New-Object System.Drawing.Font 'Segoe UI', 9
                try {
                    $graphics.DrawString('AI WORK STATUS', $titleFont, $white, 34, 12)
                    $graphics.DrawString('LIKELY WORKING', $valueFont, $accent, 18, 54)
                    $graphics.DrawString('3 CODEX TASKS UPDATED < 2 MIN', $smallFont, $white, 20, 101)
                    $graphics.DrawString('····▁▂▅▃▇█', $chartFont, $accent, 18, 119)
                    $graphics.DrawString('+1.1M TOKENS · PEAK 836K/MIN', $smallFont, $dim, 20, 158)
                    $graphics.DrawLine((New-Object System.Drawing.Pen $accent, 3), 18, 187, $Width - 18, 187)
                    $graphics.DrawString('NETWORK  Wi-Fi 2 · 13 ms', $smallFont, $white, 18, 201)
                    $graphics.DrawString('LOCAL · READ ONLY · NO CONTENT', $smallFont, $dim, 20, $Height - 34)
                } finally {
                    $titleFont.Dispose(); $valueFont.Dispose(); $chartFont.Dispose(); $smallFont.Dispose()
                }
            } else {
                $graphics.Clear([System.Drawing.Color]::FromArgb(26, 26, 26))
                $size = [Math]::Max(12, [Math]::Floor([Math]::Min($Width, $Height) * 0.48))
                $font = New-Object System.Drawing.Font 'Consolas', $size, ([System.Drawing.FontStyle]::Bold), ([System.Drawing.GraphicsUnit]::Pixel)
                try {
                    $format = New-Object System.Drawing.StringFormat
                    $format.Alignment = [System.Drawing.StringAlignment]::Center
                    $format.LineAlignment = [System.Drawing.StringAlignment]::Center
                    $graphics.FillEllipse($accent, [int]($Width * 0.08), [int]($Height * 0.08), [int]($Width * 0.12), [int]($Height * 0.12))
                    $graphics.DrawString('S', $font, $white, (New-Object System.Drawing.RectangleF 0, 0, $Width, $Height), $format)
                    $format.Dispose()
                } finally {
                    $font.Dispose()
                }
            }
        } finally {
            $accent.Dispose(); $white.Dispose(); $dim.Dispose(); $background.Dispose()
        }
        $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

New-SirinAsset -Path (Join-Path $imagesLayout 'StoreLogo.png') -Width 50 -Height 50
New-SirinAsset -Path (Join-Path $imagesLayout 'Square44x44Logo.png') -Width 44 -Height 44
New-SirinAsset -Path (Join-Path $imagesLayout 'Square150x150Logo.png') -Width 150 -Height 150
New-SirinAsset -Path (Join-Path $imagesLayout 'Wide310x150Logo.png') -Width 310 -Height 150
New-SirinAsset -Path (Join-Path $providerAssetsLayout 'SirinAIWork_Icon.png') -Width 64 -Height 64
New-SirinAsset -Path (Join-Path $providerAssetsLayout 'SirinAIWork_Screenshot.png') -Width 300 -Height 304 -Preview

$previewPath = Join-Path $providerAssetsLayout 'SirinAIWork_Screenshot.png'
$previewImage = [System.Drawing.Bitmap]::FromFile($previewPath)
try {
    if ($previewImage.Width -ne 300 -or $previewImage.Height -ne 304) {
        throw "Widget picker preview must be exactly 300x304: $previewPath"
    }
    if ($previewImage.GetPixel(0, 0).A -ne 0) {
        throw "Widget picker preview must have transparent rounded corners: $previewPath"
    }
} finally {
    $previewImage.Dispose()
}

$makeAppx = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\makeappx.exe'
$packageManifest = [xml](Get-Content -LiteralPath (Join-Path $widgetRoot 'Package.appxmanifest') -Raw -Encoding UTF8)
$packageVersion = [string]$packageManifest.Package.Identity.Version
$msix = Join-Path $outRoot "SirinAIWorkWidget_${packageVersion}_x64_unsigned.msix"
if (Test-Path -LiteralPath $makeAppx) {
    & $makeAppx pack /d $layout /p $msix /o | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "MakeAppx failed with exit code $LASTEXITCODE"
    }
}

[pscustomobject]@{
    status = 'BUILT'
    configuration = $Configuration
    build_root = $BuildRoot
    provider = $providerExe
    layout = $layout
    unsigned_msix = if (Test-Path -LiteralPath $msix) { $msix } else { $null }
} | ConvertTo-Json -Compress
