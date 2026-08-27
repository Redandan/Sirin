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
$providerStatePath = Join-Path $env:LOCALAPPDATA 'Sirin\widget-provider-state.json'

if ($null -eq ('SirinWidgetWindow' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class SirinWidgetWindow
{
    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr windowHandle);
}
'@
}

function Get-SirinWidgetProviderProcess {
    param([string]$ExpectedPath = '')

    Get-CimInstance Win32_Process -Filter "Name = 'SirinWidgetProvider.exe'" -ErrorAction SilentlyContinue |
        Where-Object {
            [string]::IsNullOrWhiteSpace($ExpectedPath) -or
            [string]::Equals(
                [string]$_.ExecutablePath,
                [IO.Path]::GetFullPath($ExpectedPath),
                [StringComparison]::OrdinalIgnoreCase
            )
        } |
        Select-Object -First 1
}

function Stop-SirinWidgetProvider {
    param([string]$ExpectedPath = $providerPath)

    $provider = Get-SirinWidgetProviderProcess -ExpectedPath $ExpectedPath
    if ($provider) {
        Stop-Process -Id $provider.ProcessId -Force -ErrorAction Stop
        Wait-Process -Id $provider.ProcessId -Timeout 5 -ErrorAction SilentlyContinue
    }
}

function Open-WindowsWidgetBoard {
    $widgets = Get-Process Widgets -ErrorAction SilentlyContinue | Select-Object -First 1
    $alreadyVisible = $widgets -and
        $widgets.MainWindowHandle -ne 0 -and
        [SirinWidgetWindow]::IsWindowVisible([IntPtr]$widgets.MainWindowHandle)
    if (-not $alreadyVisible) {
        Start-Process 'ms-widgets:'
    }

    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        Start-Sleep -Milliseconds 250
        $widgetService = Get-Process WidgetService -ErrorAction SilentlyContinue | Select-Object -First 1
        $currentWidgets = Get-Process Widgets -ErrorAction SilentlyContinue | Select-Object -First 1
        $boardVisible = $currentWidgets -and
            $currentWidgets.MainWindowHandle -ne 0 -and
            [SirinWidgetWindow]::IsWindowVisible([IntPtr]$currentWidgets.MainWindowHandle)
    } until (($widgetService -and $boardVisible) -or [DateTime]::UtcNow -ge $deadline)

    [pscustomobject]@{
        host_restarted = $false
        board_open_requested = -not $alreadyVisible
        board_visible = [bool]$boardVisible
        widget_service_running = $null -ne $widgetService
    }
}

function Get-ProviderState {
    if (-not (Test-Path -LiteralPath $providerStatePath)) {
        return $null
    }
    try {
        $item = Get-Item -LiteralPath $providerStatePath
        $data = Get-Content -LiteralPath $providerStatePath -Raw -Encoding UTF8 | ConvertFrom-Json
        [pscustomobject]@{
            version = [string]$data.version
            event = [string]$data.event
            widget_count = [int]$data.widget_count
            timestamp_unix_ms = [int64]$data.timestamp_unix_ms
            hresult = [int]$data.hresult
            last_write_utc = $item.LastWriteTimeUtc.ToString('o')
            last_write_ticks = $item.LastWriteTimeUtc.Ticks
        }
    } catch {
        $null
    }
}

function Get-SirinWidgetSession {
    param([string]$PackageFamilyName = '')

    if ([string]::IsNullOrWhiteSpace($PackageFamilyName)) {
        return $null
    }
    $sessionRoot = Join-Path $env:LOCALAPPDATA (
        'Packages\Microsoft.WidgetsPlatformRuntime_8wekyb3d8bbwe\LocalState\WidgetSessions\' +
        $PackageFamilyName + '!App!!SirinAIWorkWidgetProvider'
    )
    $latest = Get-ChildItem -LiteralPath $sessionRoot -Filter '*.dat' -File -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if ($latest) {
        [pscustomobject]@{
            path = $latest.FullName
            last_write_utc = $latest.LastWriteTimeUtc.ToString('o')
            last_write_ticks = $latest.LastWriteTimeUtc.Ticks
        }
    }
}

function Get-SirinWidgetStatus {
    $package = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue
    $installedProviderPath = if ($package) {
        Join-Path ([string]$package.InstallLocation) 'SirinWidgetProvider\SirinWidgetProvider.exe'
    } else {
        $providerPath
    }
    $provider = Get-SirinWidgetProviderProcess -ExpectedPath $installedProviderPath
    $widgetService = Get-Process WidgetService -ErrorAction SilentlyContinue | Select-Object -First 1
    $providerState = Get-ProviderState
    $widgetSession = Get-SirinWidgetSession -PackageFamilyName $(if ($package) { [string]$package.PackageFamilyName } else { '' })
    [pscustomobject]@{
        package_name = $packageName
        installed = $null -ne $package
        package_full_name = if ($package) { $package.PackageFullName } else { $null }
        package_version = if ($package) { [string]$package.Version } else { $null }
        install_location = if ($package) { $package.InstallLocation } else { $null }
        installed_provider_path = if ($package) { $installedProviderPath } else { $null }
        provider_running = $null -ne $provider
        provider_pid = if ($provider) { $provider.ProcessId } else { $null }
        provider_state_path = $providerStatePath
        provider_state_version = if ($providerState) { $providerState.version } else { $null }
        provider_state_event = if ($providerState) { $providerState.event } else { $null }
        provider_state_widget_count = if ($providerState) { $providerState.widget_count } else { $null }
        provider_state_hresult = if ($providerState) { $providerState.hresult } else { $null }
        provider_state_last_write_utc = if ($providerState) { $providerState.last_write_utc } else { $null }
        widget_session_path = if ($widgetSession) { $widgetSession.path } else { $null }
        widget_session_last_write_utc = if ($widgetSession) { $widgetSession.last_write_utc } else { $null }
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
    Open-WindowsWidgetBoard | Out-Null
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
    Stop-SirinWidgetProvider -ExpectedPath $providerPath
    & (Join-Path $PSScriptRoot 'build.ps1') -BuildRoot $BuildRoot
    & (Join-Path $PSScriptRoot 'validate.ps1') -BuildRoot $BuildRoot
}
if (-not (Test-Path -LiteralPath $manifest)) {
    throw "Build the widget before installing it: $manifest"
}

[xml]$desiredManifest = Get-Content -LiteralPath $manifest -Raw -Encoding UTF8
$desiredVersion = [version]([string]$desiredManifest.Package.Identity.Version)
$installedPackage = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue
$stateBefore = Get-ProviderState
$stateTicksBefore = if ($stateBefore) { $stateBefore.last_write_ticks } else { 0 }
$sessionBefore = Get-SirinWidgetSession -PackageFamilyName $(if ($installedPackage) { [string]$installedPackage.PackageFamilyName } else { '' })
$sessionTicksBefore = if ($sessionBefore) { $sessionBefore.last_write_ticks } else { 0 }
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
    if ($installedPackage) {
        $oldProviderPath = Join-Path ([string]$installedPackage.InstallLocation) 'SirinWidgetProvider\SirinWidgetProvider.exe'
        Stop-SirinWidgetProvider -ExpectedPath $oldProviderPath
    }
    Add-AppxPackage -Register $manifest -ForceApplicationShutdown
}
$reload = if ($SkipReload) {
    [pscustomobject]@{
        host_restarted = $false
        board_open_requested = $false
        widget_service_running = (Get-Process WidgetService -ErrorAction SilentlyContinue) -ne $null
    }
} else {
    Open-WindowsWidgetBoard
}

$registeredPackage = Get-AppxPackage -Name $packageName -ErrorAction Stop
$registeredProviderPath = Join-Path ([string]$registeredPackage.InstallLocation) 'SirinWidgetProvider\SirinWidgetProvider.exe'
Stop-SirinWidgetProvider -ExpectedPath $registeredProviderPath
Start-Process explorer.exe -ArgumentList (
    'shell:AppsFolder\' + [string]$registeredPackage.PackageFamilyName + '!App'
)

$providerUpdateConfirmed = $false
$deadline = [DateTime]::UtcNow.AddSeconds(20)
do {
    Start-Sleep -Milliseconds 500
    $providerState = Get-ProviderState
    $stateAdvanced = $providerState -and $providerState.last_write_ticks -gt $stateTicksBefore
    $providerUpdateConfirmed = $stateAdvanced -and
        $providerState.version -eq [string]$desiredVersion -and
        $providerState.event -eq 'UPDATE_OK' -and
        $providerState.hresult -eq 0
} until ($providerUpdateConfirmed -or [DateTime]::UtcNow -ge $deadline)

$status = Get-SirinWidgetStatus
$sessionAfter = Get-SirinWidgetSession -PackageFamilyName ([string]$registeredPackage.PackageFamilyName)
$sessionAdvanced = $sessionAfter -and $sessionAfter.last_write_ticks -gt $sessionTicksBefore
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
    provider_bootstrap_requested = $true
    provider_update_confirmed = $providerUpdateConfirmed
    provider_state_path = $status.provider_state_path
    provider_state_version = $status.provider_state_version
    provider_state_event = $status.provider_state_event
    provider_state_widget_count = $status.provider_state_widget_count
    provider_state_hresult = $status.provider_state_hresult
    provider_state_last_write_utc = $status.provider_state_last_write_utc
    widget_session_path = $status.widget_session_path
    widget_session_last_write_utc = $status.widget_session_last_write_utc
    widget_session_advanced = [bool]$sessionAdvanced
} | ConvertTo-Json -Compress
