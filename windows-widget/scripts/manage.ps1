[CmdletBinding()]
param(
    [ValidateSet('Plan', 'Install', 'Status', 'OpenBoard', 'Remove')]
    [string]$Action = 'Plan',
    [switch]$Build,
    [switch]$SkipReload,
    [string]$BuildRoot = ''
)

$ErrorActionPreference = 'Stop'
$packageName = 'Redan.SirinAIWorkWidget'
$widgetRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$BuildRoot = if ([string]::IsNullOrWhiteSpace($BuildRoot)) {
    $widgetRoot
} else {
    [IO.Path]::GetFullPath($BuildRoot)
}
$layout = Join-Path $BuildRoot 'out\layout'
$manifest = Join-Path $layout 'AppxManifest.xml'
$providerPath = Join-Path $layout 'SirinWidgetProvider\SirinWidgetProvider.exe'

function Get-SirinWidgetProviderProcess {
    Get-CimInstance Win32_Process -Filter "Name = 'SirinWidgetProvider.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -eq $providerPath } |
        Select-Object -First 1
}

function Stop-SirinWidgetProvider {
    $provider = Get-SirinWidgetProviderProcess
    if ($provider) {
        Stop-Process -Id $provider.ProcessId -Force -ErrorAction Stop
    }
}

function Restart-WindowsWidgetHost {
    $widgetProcesses = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object {
            ($_.Name -in @('WidgetBoard.exe', 'Widgets.exe') -and
                $_.ExecutablePath -like 'C:\Program Files\WindowsApps\MicrosoftWindows.Client.WebExperience_*') -or
            ($_.Name -eq 'WidgetService.exe' -and
                $_.ExecutablePath -like 'C:\Program Files\WindowsApps\Microsoft.WidgetsPlatformRuntime_*')
        }

    foreach ($widgetProcess in $widgetProcesses) {
        Stop-Process -Id $widgetProcess.ProcessId -Force -ErrorAction SilentlyContinue
    }

    Start-Sleep -Milliseconds 750
    Start-Process 'ms-widgets:'

    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        Start-Sleep -Milliseconds 250
        $widgetService = Get-Process WidgetService -ErrorAction SilentlyContinue | Select-Object -First 1
    } until ($widgetService -or [DateTime]::UtcNow -ge $deadline)

    [pscustomobject]@{
        host_restarted = $true
        board_open_requested = $true
        widget_service_running = $null -ne $widgetService
    }
}

function Get-SirinWidgetStatus {
    $package = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue
    $provider = Get-SirinWidgetProviderProcess
    $widgetService = Get-Process WidgetService -ErrorAction SilentlyContinue | Select-Object -First 1
    [pscustomobject]@{
        package_name = $packageName
        installed = $null -ne $package
        package_full_name = if ($package) { $package.PackageFullName } else { $null }
        package_version = if ($package) { [string]$package.Version } else { $null }
        install_location = if ($package) { $package.InstallLocation } else { $null }
        provider_running = $null -ne $provider
        provider_pid = if ($provider) { $provider.ProcessId } else { $null }
        widget_service_running = $null -ne $widgetService
        manifest_ready = Test-Path -LiteralPath $manifest
    }
}

if ($Action -eq 'Plan') {
    [pscustomobject]@{
        action = 'PLAN_ONLY'
        package_name = $packageName
        registration = 'Developer-mode loose package'
        auto_pin = $false
        note = 'Windows requires the user to pin the widget from the Widgets picker.'
    } | ConvertTo-Json -Compress
    exit 0
}

if ($Action -eq 'Status') {
    Get-SirinWidgetStatus | ConvertTo-Json -Compress
    exit 0
}

if ($Action -eq 'OpenBoard') {
    Start-Process 'ms-widgets:'
    Get-SirinWidgetStatus | ConvertTo-Json -Compress
    exit 0
}

if ($Action -eq 'Remove') {
    $package = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue
    if ($package) {
        Remove-AppxPackage -Package $package.PackageFullName
    }
    Get-SirinWidgetStatus | ConvertTo-Json -Compress
    exit 0
}

if ($Build) {
    Stop-SirinWidgetProvider
    & (Join-Path $PSScriptRoot 'build.ps1') -BuildRoot $BuildRoot
    & (Join-Path $PSScriptRoot 'validate.ps1') -BuildRoot $BuildRoot
}
if (-not (Test-Path -LiteralPath $manifest)) {
    throw "Build the widget before installing it: $manifest"
}

[xml]$desiredManifest = Get-Content -LiteralPath $manifest -Raw -Encoding UTF8
$desiredVersion = [version]([string]$desiredManifest.Package.Identity.Version)
$installedPackage = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue
$sameRegistration = $false
if ($installedPackage) {
    $installedVersion = [version]([string]$installedPackage.Version)
    $installedLocation = [IO.Path]::GetFullPath([string]$installedPackage.InstallLocation)
    $desiredLocation = [IO.Path]::GetFullPath($layout)
    $sameLocation = [string]::Equals(
        $installedLocation,
        $desiredLocation,
        [StringComparison]::OrdinalIgnoreCase
    )
    if ($installedVersion -gt $desiredVersion) {
        throw "Refusing to downgrade Widget $installedVersion to $desiredVersion"
    }
    if ($installedVersion -eq $desiredVersion -and -not $sameLocation) {
        throw "Widget $desiredVersion is already registered from another location: $installedLocation"
    }
    $sameRegistration = $installedVersion -eq $desiredVersion -and $sameLocation
}
if (-not $sameRegistration) {
    Add-AppxPackage -Register $manifest -ForceApplicationShutdown
}
$reload = if ($SkipReload) {
    [pscustomobject]@{
        host_restarted = $false
        board_open_requested = $false
        widget_service_running = (Get-Process WidgetService -ErrorAction SilentlyContinue) -ne $null
    }
} else {
    Restart-WindowsWidgetHost
}

Start-Sleep -Milliseconds 500
$status = Get-SirinWidgetStatus
[pscustomobject]@{
    package_name = $status.package_name
    installed = $status.installed
    package_full_name = $status.package_full_name
    package_version = $status.package_version
    install_location = $status.install_location
    provider_running = $status.provider_running
    provider_pid = $status.provider_pid
    widget_service_running = $status.widget_service_running
    manifest_ready = $status.manifest_ready
    registration_updated = -not $sameRegistration
    host_restarted = $reload.host_restarted
    board_open_requested = $reload.board_open_requested
} | ConvertTo-Json -Compress
