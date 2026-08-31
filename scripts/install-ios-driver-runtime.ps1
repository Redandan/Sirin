#requires -Version 5.1
<#
Install or inspect Sirin's private iOS Driver runtime.

The source stays in this repository. Generated Python, go-ios files and the
verified unsigned WDA operator handoff stay under
%LOCALAPPDATA%\Sirin\ios-driver. No certificate, provisioning profile, Apple
credential, signed WDA asset, device identifier, or phone setting is copied or
changed. Sirin never signs or installs WDA.
#>

[CmdletBinding()]
param(
    [ValidateSet('Install', 'Status')]
    [string]$Action = 'Status',
    [string]$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$RuntimeRoot = (Join-Path $env:LOCALAPPDATA 'Sirin\ios-driver'),
    [string]$Python = '',
    [string]$GoIosSource = '',
    [string]$GoIosPackageUrl = 'https://registry.npmjs.org/go-ios/-/go-ios-1.3.2.tgz',
    [string]$ExpectedGoIosPackageSha512 = '64b4cbec3d8dc4ab600142b734e2ce3f96e4de2aa445b02f4a88c7f9b303ec1dea64cda3f46c53f16c916f0e3e1f94fa60f807c5ea61d6e65c9f59b94f2fc89c',
    [string]$ExpectedGoIosSha256 = 'c99b04f1d615fa716637efae457d5c554f32259f9d249375de0086e3cc1a1df5',
    [string]$WdaReleaseTag = 'v16.5.1',
    [string]$WdaRunnerZipUrl = 'https://github.com/appium/WebDriverAgent/releases/download/v16.5.1/WebDriverAgentRunner-Runner.zip',
    [string]$ExpectedWdaRunnerZipSha256 = '6aa026a7938ec66d7d0452b12e210ac5c3a99b5b1528111cee1b690285f6efe9',
    [string]$WdaLicenseUrl = 'https://raw.githubusercontent.com/appium/WebDriverAgent/v16.5.1/LICENSE',
    [string]$ExpectedWdaLicenseSha256 = 'd9910c6ba5e4c29ae415ee3ce875c9e18a60d8bc4d7fe2c2d104db2a718b1bb4'
)

$ErrorActionPreference = 'Stop'

$sourceRoot = Join-Path $Repo 'integrations\ios-driver'
$hostScript = Join-Path $sourceRoot 'scripts\unattended_host.py'
$requirements = Join-Path $sourceRoot 'requirements.txt'
$allowedRoot = [System.IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'Sirin')).TrimEnd('\')
$resolvedRuntime = [System.IO.Path]::GetFullPath($RuntimeRoot).TrimEnd('\')
if (-not $resolvedRuntime.StartsWith(
    $allowedRoot + '\',
    [System.StringComparison]::OrdinalIgnoreCase
)) {
    throw "RuntimeRoot must remain below $allowedRoot"
}

$venvRoot = Join-Path $resolvedRuntime 'venv'
$venvPython = Join-Path $venvRoot 'Scripts\python.exe'
$venvPythonw = Join-Path $venvRoot 'Scripts\pythonw.exe'
$binRoot = Join-Path $resolvedRuntime 'bin'
$goIos = Join-Path $binRoot 'ios.exe'
$downloadRoot = Join-Path $resolvedRuntime 'downloads'
$goIosPackage = Join-Path $downloadRoot 'go-ios-1.3.2.tgz'
$operatorAssetsRoot = Join-Path $resolvedRuntime 'operator-assets'
$wdaRunnerZip = Join-Path $downloadRoot "WebDriverAgentRunner-Runner-$WdaReleaseTag.zip"
$wdaUnsignedIpa = Join-Path $operatorAssetsRoot "WebDriverAgent-$WdaReleaseTag-unsigned.ipa"
$wdaLicense = Join-Path $operatorAssetsRoot 'WebDriverAgent-LICENSE.txt'
$wdaOperatorReadme = Join-Path $operatorAssetsRoot 'WDA-OPERATOR-README.txt'
$stateRoot = Join-Path $resolvedRuntime 'state'
$logRoot = Join-Path $resolvedRuntime 'logs'
$manifest = Join-Path $resolvedRuntime 'runtime-manifest.json'

function Get-HashOrNull([string]$Path, [string]$Algorithm = 'SHA256') {
    if (Test-Path -LiteralPath $Path -PathType Leaf) {
        return (Get-FileHash -LiteralPath $Path -Algorithm $Algorithm).Hash.ToLowerInvariant()
    }
    return $null
}

function Get-ManifestOrNull {
    if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
        return $null
    }
    try {
        return Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json
    }
    catch {
        return $null
    }
}

function Test-UnsignedWdaIpa([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $false
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = $null
    try {
        $archive = [System.IO.Compression.ZipFile]::OpenRead($Path)
        $entries = @($archive.Entries | ForEach-Object { $_.FullName.Replace('\\', '/') })
        $files = @($entries | Where-Object { -not $_.EndsWith('/') })
        $expectedPrefix = 'Payload/WebDriverAgentRunner-Runner.app/'
        $hasInfo = $files -contains ($expectedPrefix + 'Info.plist')
        $hasRunner = $files -contains ($expectedPrefix + 'WebDriverAgentRunner-Runner')
        $hasUnexpectedPath = @($files | Where-Object { -not $_.StartsWith($expectedPrefix) }).Count -gt 0
        $hasSigningMaterial = @($files | Where-Object {
            $_ -match '/_CodeSignature/' -or $_ -match '/embedded\.mobileprovision$'
        }).Count -gt 0
        return $hasInfo -and $hasRunner -and -not $hasUnexpectedPath -and -not $hasSigningMaterial
    }
    catch {
        return $false
    }
    finally {
        if ($null -ne $archive) {
            $archive.Dispose()
        }
    }
}

function Save-VerifiedDownload(
    [string]$Uri,
    [string]$Path,
    [string]$ExpectedHash,
    [ValidateSet('SHA256', 'SHA512')]
    [string]$Algorithm = 'SHA256'
) {
    $expected = $ExpectedHash.ToLowerInvariant()
    if ((Get-HashOrNull $Path $Algorithm) -eq $expected) {
        return 'existing_hash_verified_file'
    }

    $pending = "$Path.pending"
    $webRequest = @{
        Uri = $Uri
        OutFile = $pending
        TimeoutSec = 120
    }
    if ($PSVersionTable.PSVersion.Major -le 5) {
        $webRequest.UseBasicParsing = $true
    }
    Invoke-WebRequest @webRequest
    $downloadedHash = (Get-FileHash -LiteralPath $pending -Algorithm $Algorithm).Hash.ToLowerInvariant()
    if ($downloadedHash -ne $expected) {
        Remove-Item -LiteralPath $pending -Force
        throw "download hash mismatch for $Uri`: expected $expected, got $downloadedHash"
    }
    Move-Item -LiteralPath $pending -Destination $Path -Force
    return 'official_url_hash_verified'
}

function Test-CompatiblePython([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path) -or
        -not (Test-Path -LiteralPath $Path -PathType Leaf) -or
        $Path -match '\\WindowsApps\\') {
        return $null
    }
    try {
        $versionText = (& $Path -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")' 2>$null | Out-String).Trim()
        $version = [version]$versionText
        if ($version.Major -eq 3 -and $version.Minor -ge 10) {
            return [pscustomobject]@{ path = [System.IO.Path]::GetFullPath($Path); version = $versionText }
        }
    }
    catch {}
    return $null
}

function Resolve-BootstrapPython {
    $candidates = [System.Collections.Generic.List[string]]::new()
    if (-not [string]::IsNullOrWhiteSpace($Python)) {
        $candidates.Add($Python)
    }
    foreach ($root in @(
        (Join-Path $env:LOCALAPPDATA 'Programs\Python'),
        $env:ProgramFiles,
        ${env:ProgramFiles(x86)}
    )) {
        if ([string]::IsNullOrWhiteSpace($root) -or -not (Test-Path -LiteralPath $root -PathType Container)) {
            continue
        }
        $pattern = if ($root -like '*Programs\Python') { 'Python*\python.exe' } else { 'Python*\python.exe' }
        Get-ChildItem -Path (Join-Path $root $pattern) -File -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending |
            ForEach-Object { $candidates.Add($_.FullName) }
    }
    $command = Get-Command python.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($command) {
        $candidates.Add($command.Source)
    }

    foreach ($candidate in @($candidates | Select-Object -Unique)) {
        $compatible = Test-CompatiblePython $candidate
        if ($compatible) {
            return $compatible
        }
    }
    throw 'Python 3.10 or newer is required to install Sirin iOS Driver; no compatible python.exe was found.'
}

function Get-RuntimeStatus {
    $goIosHash = Get-HashOrNull $goIos
    $packageHash = Get-HashOrNull $goIosPackage 'SHA512'
    $hostHash = Get-HashOrNull $hostScript
    $manifestValue = Get-ManifestOrNull
    $wdaRunnerZipHash = Get-HashOrNull $wdaRunnerZip
    $wdaLicenseHash = Get-HashOrNull $wdaLicense
    $wdaUnsignedIpaHash = Get-HashOrNull $wdaUnsignedIpa
    $recordedWdaIpaHash = if ($null -ne $manifestValue) {
        [string]$manifestValue.wda_unsigned_ipa_sha256
    } else { '' }
    $wdaUnsignedIpaReady = (
        $wdaRunnerZipHash -eq $ExpectedWdaRunnerZipSha256.ToLowerInvariant() -and
        $wdaLicenseHash -eq $ExpectedWdaLicenseSha256.ToLowerInvariant() -and
        -not [string]::IsNullOrWhiteSpace($recordedWdaIpaHash) -and
        $wdaUnsignedIpaHash -eq $recordedWdaIpaHash.ToLowerInvariant() -and
        (Test-UnsignedWdaIpa $wdaUnsignedIpa)
    )
    $requestsVersion = $null
    if (Test-Path -LiteralPath $venvPython -PathType Leaf) {
        try {
            $requestsVersion = (& $venvPython -c 'import requests; print(requests.__version__)' 2>$null | Out-String).Trim()
        }
        catch {}
    }
    [pscustomobject]@{
        owner = 'sirin'
        source_root = $sourceRoot
        source_ready = (Test-Path -LiteralPath $hostScript -PathType Leaf)
        source_sha256 = $hostHash
        runtime_root = $resolvedRuntime
        runtime_within_sirin = $resolvedRuntime.StartsWith($allowedRoot + '\', [System.StringComparison]::OrdinalIgnoreCase)
        python = $venvPython
        python_ready = (Test-Path -LiteralPath $venvPython -PathType Leaf)
        pythonw_ready = (Test-Path -LiteralPath $venvPythonw -PathType Leaf)
        requests_version = $requestsVersion
        go_ios = $goIos
        go_ios_ready = ($goIosHash -eq $ExpectedGoIosSha256.ToLowerInvariant())
        go_ios_sha256 = $goIosHash
        expected_go_ios_sha256 = $ExpectedGoIosSha256.ToLowerInvariant()
        official_package_url = $GoIosPackageUrl
        official_package_sha512 = $ExpectedGoIosPackageSha512.ToLowerInvariant()
        official_package_cached = ($packageHash -eq $ExpectedGoIosPackageSha512.ToLowerInvariant())
        cached_package_sha512 = $packageHash
        wda_release_tag = $WdaReleaseTag
        wda_official_runner_zip_url = $WdaRunnerZipUrl
        wda_official_runner_zip_sha256 = $ExpectedWdaRunnerZipSha256.ToLowerInvariant()
        wda_official_runner_zip_cached = ($wdaRunnerZipHash -eq $ExpectedWdaRunnerZipSha256.ToLowerInvariant())
        wda_unsigned_ipa = $wdaUnsignedIpa
        wda_unsigned_ipa_sha256 = $wdaUnsignedIpaHash
        wda_unsigned_ipa_ready = $wdaUnsignedIpaReady
        wda_license = $wdaLicense
        wda_license_ready = ($wdaLicenseHash -eq $ExpectedWdaLicenseSha256.ToLowerInvariant())
        wda_operator_readme = $wdaOperatorReadme
        operator_handoff = [pscustomobject]@{
            action = 'USER_SIGN_AND_INSTALL_UNSIGNED_WDA_WITH_SIDELOADLY'
            ipa = $wdaUnsignedIpa
            license = $wdaLicense
            ready = $wdaUnsignedIpaReady
            human_only = $true
        }
        manifest = $manifest
        manifest_exists = (Test-Path -LiteralPath $manifest -PathType Leaf)
        certificates_copied = $false
        signing_supported = $false
        install_to_phone_supported = $false
    }
}

if ($Action -eq 'Status') {
    Get-RuntimeStatus | ConvertTo-Json -Depth 5
    return
}

$bootstrapPython = Resolve-BootstrapPython
foreach ($required in @($hostScript, $requirements, $bootstrapPython.path)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "required installer input is missing: $required"
    }
}

foreach ($directory in @($resolvedRuntime, $binRoot, $downloadRoot, $operatorAssetsRoot, $stateRoot, $logRoot)) {
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
}

$wdaRunnerZipSourceMode = Save-VerifiedDownload `
    -Uri $WdaRunnerZipUrl `
    -Path $wdaRunnerZip `
    -ExpectedHash $ExpectedWdaRunnerZipSha256 `
    -Algorithm SHA256
$wdaLicenseSourceMode = Save-VerifiedDownload `
    -Uri $WdaLicenseUrl `
    -Path $wdaLicense `
    -ExpectedHash $ExpectedWdaLicenseSha256 `
    -Algorithm SHA256

$priorManifest = Get-ManifestOrNull
$recordedWdaIpaHash = if ($null -ne $priorManifest) {
    [string]$priorManifest.wda_unsigned_ipa_sha256
} else { '' }
$currentWdaIpaHash = Get-HashOrNull $wdaUnsignedIpa
$wdaIpaSourceMode = 'existing_manifest_and_content_verified'
if ([string]::IsNullOrWhiteSpace($recordedWdaIpaHash) -or
    $currentWdaIpaHash -ne $recordedWdaIpaHash.ToLowerInvariant() -or
    -not (Test-UnsignedWdaIpa $wdaUnsignedIpa)) {
    $stageRoot = Join-Path $downloadRoot ('wda-stage-' + [guid]::NewGuid().ToString('N'))
    $extractRoot = Join-Path $stageRoot 'extract'
    $payloadRoot = Join-Path $stageRoot 'Payload'
    $pendingIpaZip = Join-Path $stageRoot 'WebDriverAgent-unsigned.zip'
    $resolvedStage = [System.IO.Path]::GetFullPath($stageRoot)
    if (-not $resolvedStage.StartsWith(
        ([System.IO.Path]::GetFullPath($downloadRoot).TrimEnd('\') + '\'),
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        throw 'WDA staging path escaped Sirin runtime downloads root.'
    }
    try {
        New-Item -ItemType Directory -Path $extractRoot, $payloadRoot -Force | Out-Null
        Expand-Archive -LiteralPath $wdaRunnerZip -DestinationPath $extractRoot -Force
        $topLevel = @(Get-ChildItem -LiteralPath $extractRoot -Force)
        $wdaApp = Join-Path $extractRoot 'WebDriverAgentRunner-Runner.app'
        if ($topLevel.Count -ne 1 -or
            -not (Test-Path -LiteralPath $wdaApp -PathType Container)) {
            throw 'Verified WDA runner archive did not contain exactly the expected app bundle.'
        }
        Move-Item -LiteralPath $wdaApp -Destination $payloadRoot
        Compress-Archive `
            -LiteralPath $payloadRoot `
            -DestinationPath $pendingIpaZip `
            -CompressionLevel Optimal `
            -Force
        if (-not (Test-UnsignedWdaIpa $pendingIpaZip)) {
            throw 'Generated WDA operator IPA failed unsigned structure validation.'
        }
        Move-Item -LiteralPath $pendingIpaZip -Destination $wdaUnsignedIpa -Force
        $wdaIpaSourceMode = 'repacked_from_official_runner_zip_hash_verified'
    }
    finally {
        if (Test-Path -LiteralPath $stageRoot -PathType Container) {
            [System.IO.Directory]::Delete($resolvedStage, $true)
        }
    }
}

@"
Sirin iPhone operator handoff

Unsigned WDA IPA: $wdaUnsignedIpa
Source: $WdaRunnerZipUrl
Pinned release: $WdaReleaseTag
Source ZIP SHA-256: $($ExpectedWdaRunnerZipSha256.ToLowerInvariant())

This IPA contains only the hash-verified Appium WebDriverAgent release payload.
Sirin does not sign or install it. A human operator must use Sideloadly, handle
Apple ID/signing prompts, then trust the developer profile on the iPhone.
Never place a signed IPA, certificate, provisioning profile or Apple credential
inside the Sirin repository or installer.
"@ | Set-Content -LiteralPath $wdaOperatorReadme -Encoding utf8
if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
    & $bootstrapPython.path -m venv $venvRoot
    if ($LASTEXITCODE -ne 0) {
        throw "failed to create Sirin iOS Driver venv: exit $LASTEXITCODE"
    }
}
$pipOutput = @(& $venvPython -m pip install --disable-pip-version-check --requirement $requirements 2>&1)
if ($LASTEXITCODE -ne 0) {
    $detail = ($pipOutput | Select-Object -Last 20 | Out-String).Trim()
    throw "failed to install Sirin iOS Driver requirements: exit $LASTEXITCODE; $detail"
}

$goIosSourceMode = 'existing_verified_runtime'
$currentGoIosHash = Get-HashOrNull $goIos
if ($currentGoIosHash -ne $ExpectedGoIosSha256.ToLowerInvariant()) {
    $pendingGoIos = Join-Path $binRoot 'ios.exe.pending'
    if (-not [string]::IsNullOrWhiteSpace($GoIosSource)) {
        if (-not (Test-Path -LiteralPath $GoIosSource -PathType Leaf)) {
            throw "explicit go-ios source is missing: $GoIosSource"
        }
        Copy-Item -LiteralPath $GoIosSource -Destination $pendingGoIos -Force
        $goIosSourceMode = 'explicit_hash_verified_file'
    }
    else {
        $packageHash = Get-HashOrNull $goIosPackage 'SHA512'
        if ($packageHash -ne $ExpectedGoIosPackageSha512.ToLowerInvariant()) {
            $pendingPackage = "$goIosPackage.pending"
            $webRequest = @{
                Uri = $GoIosPackageUrl
                OutFile = $pendingPackage
                TimeoutSec = 120
            }
            if ($PSVersionTable.PSVersion.Major -le 5) {
                $webRequest.UseBasicParsing = $true
            }
            Invoke-WebRequest @webRequest
            $downloadedHash = (Get-FileHash -LiteralPath $pendingPackage -Algorithm SHA512).Hash.ToLowerInvariant()
            if ($downloadedHash -ne $ExpectedGoIosPackageSha512.ToLowerInvariant()) {
                Remove-Item -LiteralPath $pendingPackage -Force
                throw "go-ios package hash mismatch: expected $ExpectedGoIosPackageSha512, got $downloadedHash"
            }
            Move-Item -LiteralPath $pendingPackage -Destination $goIosPackage -Force
        }
        $tarError = Join-Path $downloadRoot 'go-ios-tar.stderr.log'
        $tarProcess = Start-Process `
            -FilePath 'tar.exe' `
            -ArgumentList @(
                '-xOf',
                "`"$goIosPackage`"",
                'package/dist/go-ios-windows-amd64_windows_amd64/ios.exe'
            ) `
            -RedirectStandardOutput $pendingGoIos `
            -RedirectStandardError $tarError `
            -WindowStyle Hidden `
            -Wait `
            -PassThru
        if ($tarProcess.ExitCode -ne 0) {
            $detail = if (Test-Path -LiteralPath $tarError) {
                (Get-Content -LiteralPath $tarError -Raw).Trim()
            } else { 'no tar stderr' }
            throw "failed to extract the official go-ios Windows binary: $detail"
        }
        $goIosSourceMode = 'official_npm_package_hash_verified'
    }
    $pendingHash = (Get-FileHash -LiteralPath $pendingGoIos -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($pendingHash -ne $ExpectedGoIosSha256.ToLowerInvariant()) {
        Remove-Item -LiteralPath $pendingGoIos -Force
        throw "go-ios binary hash mismatch: expected $ExpectedGoIosSha256, got $pendingHash"
    }
    Move-Item -LiteralPath $pendingGoIos -Destination $goIos -Force
}

$validationOutput = @(& $venvPython $hostScript --runtime-root $resolvedRuntime --validate-only 2>&1)
if ($LASTEXITCODE -ne 0) {
    $detail = ($validationOutput | Select-Object -Last 20 | Out-String).Trim()
    throw "Sirin iOS Driver validate-only failed: exit $LASTEXITCODE; $detail"
}

$manifestValue = [ordered]@{
    owner = 'sirin'
    installed_at = (Get-Date).ToUniversalTime().ToString('o')
    source_root = $sourceRoot
    source_sha256 = Get-HashOrNull $hostScript
    requirements_sha256 = Get-HashOrNull $requirements
    bootstrap_python = $bootstrapPython.path
    bootstrap_python_version = $bootstrapPython.version
    go_ios_version = '1.3.2'
    go_ios_sha256 = Get-HashOrNull $goIos
    go_ios_source_mode = $goIosSourceMode
    go_ios_package_url = $GoIosPackageUrl
    go_ios_package_sha512 = $ExpectedGoIosPackageSha512.ToLowerInvariant()
    wda_release_tag = $WdaReleaseTag
    wda_runner_zip_url = $WdaRunnerZipUrl
    wda_runner_zip_sha256 = Get-HashOrNull $wdaRunnerZip
    wda_runner_zip_source_mode = $wdaRunnerZipSourceMode
    wda_license_url = $WdaLicenseUrl
    wda_license_sha256 = Get-HashOrNull $wdaLicense
    wda_license_source_mode = $wdaLicenseSourceMode
    wda_unsigned_ipa = $wdaUnsignedIpa
    wda_unsigned_ipa_sha256 = Get-HashOrNull $wdaUnsignedIpa
    wda_unsigned_ipa_source_mode = $wdaIpaSourceMode
    safety = [ordered]@{
        loopback_only = $true
        acceptance_only_default = $true
        credentials_copied = $false
        signing_supported = $false
        install_to_phone_supported = $false
        unsigned_wda_operator_handoff_only = $true
    }
}
$manifestValue | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $manifest -Encoding utf8
Get-RuntimeStatus | ConvertTo-Json -Depth 5
