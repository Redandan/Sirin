[CmdletBinding()]
param(
    [ValidateSet('Status', 'Migrate', 'Rollback')]
    [string]$Action = 'Status',
    [string]$SirinBaseUrl = 'http://127.0.0.1:7700',
    [string]$BackupPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RunKeyPath = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunValueName = 'ChatGPTKeepAwake'
$LegacyScript = Join-Path $env:USERPROFILE '.codex\scripts\keep-chatgpt-awake.ps1'
$WindowsPowerShell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$BackupDirectory = Join-Path $env:LOCALAPPDATA 'Sirin\releases\backups'

function Get-LegacyRunValue {
    $item = Get-ItemProperty -LiteralPath $RunKeyPath -ErrorAction SilentlyContinue
    if ($null -eq $item) {
        return $null
    }
    $property = $item.PSObject.Properties[$RunValueName]
    if ($null -eq $property) {
        return $null
    }
    return [string]$property.Value
}

function Get-LegacyProcesses {
    $escapedPath = [regex]::Escape($LegacyScript)
    @(Get-CimInstance Win32_Process -Filter "Name='powershell.exe' OR Name='pwsh.exe'" |
        Where-Object { $_.CommandLine -and $_.CommandLine -match $escapedPath })
}

function Get-SirinMonitor {
    Invoke-RestMethod -Method Get -Uri ($SirinBaseUrl.TrimEnd('/') + '/api/ai-monitor') -TimeoutSec 15
}

function Get-GuardSnapshot {
    param([Parameter(Mandatory = $true)]$Monitor)

    $powerProperty = $Monitor.PSObject.Properties['power']
    if ($null -eq $powerProperty -or $null -eq $powerProperty.Value) {
        return $null
    }
    $guardProperty = $powerProperty.Value.PSObject.Properties['awake_guard']
    if ($null -eq $guardProperty) {
        return $null
    }
    return $guardProperty.Value
}

function Test-GuardReady {
    param([Parameter(Mandatory = $true)]$Monitor)

    $guard = Get-GuardSnapshot -Monitor $Monitor
    if ($null -eq $guard) {
        return $false
    }
    return (
        $guard.enabled -eq $true -and
        $guard.chatgpt_running -eq $true -and
        $guard.request_active -eq $true -and
        $guard.system_required -eq $true -and
        $guard.display_required -eq $true -and
        [string]$guard.evidence -eq 'MEASURED'
    )
}

function Get-GuardReadyMonitor {
    for ($attempt = 1; $attempt -le 4; $attempt++) {
        $monitor = Get-SirinMonitor
        if (Test-GuardReady -Monitor $monitor) {
            return $monitor
        }
        if ($attempt -lt 4) {
            Start-Sleep -Seconds 5
        }
    }
    throw 'Sirin awake guard is not MEASURED and active for the running ChatGPT process.'
}

function Restore-LegacyGuard {
    param([Parameter(Mandatory = $true)]$Backup)

    if ($null -ne $Backup.run_value -and -not [string]::IsNullOrWhiteSpace([string]$Backup.run_value)) {
        New-ItemProperty -LiteralPath $RunKeyPath -Name $RunValueName -Value ([string]$Backup.run_value) -PropertyType String -Force | Out-Null
    }
    $processes = @(Get-LegacyProcesses)
    if ($processes.Count -eq 0 -and (Test-Path -LiteralPath $LegacyScript -PathType Leaf)) {
        Start-Process -FilePath $WindowsPowerShell -ArgumentList @(
            '-NoProfile',
            '-WindowStyle', 'Hidden',
            '-ExecutionPolicy', 'Bypass',
            '-File', ('"' + $LegacyScript + '"')
        ) -WindowStyle Hidden | Out-Null
    }
}

function Show-Status {
    $monitor = $null
    $monitorError = $null
    try {
        $monitor = Get-SirinMonitor
    }
    catch {
        $monitorError = $_.Exception.Message
    }
    $legacyProcesses = @(Get-LegacyProcesses)
    $runValue = Get-LegacyRunValue
    $guard = if ($null -ne $monitor) { Get-GuardSnapshot -Monitor $monitor } else { $null }
    [pscustomobject]@{
        action = 'Status'
        sirin_reachable = $null -ne $monitor
        sirin_error = $monitorError
        sirin_guard_ready = if ($null -ne $monitor) { Test-GuardReady -Monitor $monitor } else { $false }
        sirin_guard = $guard
        registry_startup_present = $null -ne $runValue
        legacy_processes = @($legacyProcesses | ForEach-Object {
            [pscustomobject]@{
                pid = [int]$_.ProcessId
                created_at = ([datetime]$_.CreationDate).ToString('o')
            }
        })
        legacy_script_present = Test-Path -LiteralPath $LegacyScript -PathType Leaf
    }
}

switch ($Action) {
    'Status' {
        Show-Status | ConvertTo-Json -Depth 8
    }
    'Migrate' {
        $null = Get-GuardReadyMonitor
        $legacyProcesses = @(Get-LegacyProcesses)
        $runValue = Get-LegacyRunValue
        New-Item -ItemType Directory -Path $BackupDirectory -Force | Out-Null
        $timestamp = Get-Date -Format 'yyyyMMdd-HHmmss'
        $resolvedBackup = Join-Path $BackupDirectory "chatgpt-awake-guard-$timestamp.json"
        $backup = [pscustomobject]@{
            schema_version = 1
            created_at = (Get-Date).ToString('o')
            run_key = $RunKeyPath
            run_value_name = $RunValueName
            run_value = $runValue
            legacy_script = $LegacyScript
            legacy_pids = @($legacyProcesses | ForEach-Object { [int]$_.ProcessId })
        }
        $backup | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $resolvedBackup -Encoding UTF8

        try {
            if ($null -ne $runValue) {
                Remove-ItemProperty -LiteralPath $RunKeyPath -Name $RunValueName
            }
            foreach ($process in $legacyProcesses) {
                $current = Get-CimInstance Win32_Process -Filter ("ProcessId=" + [int]$process.ProcessId) -ErrorAction SilentlyContinue
                if ($null -ne $current -and $current.CommandLine -and $current.CommandLine -match [regex]::Escape($LegacyScript)) {
                    Stop-Process -Id ([int]$process.ProcessId) -Force
                }
            }
            Start-Sleep -Seconds 3
            $null = Get-GuardReadyMonitor
        }
        catch {
            Restore-LegacyGuard -Backup $backup
            throw "Migration failed and the legacy guard was restored. Backup: $resolvedBackup. $($_.Exception.Message)"
        }

        [pscustomobject]@{
            action = 'Migrate'
            status = 'MIGRATED'
            backup_path = $resolvedBackup
            registry_startup_present = $null -ne (Get-LegacyRunValue)
            legacy_process_count = @(Get-LegacyProcesses).Count
            sirin_guard_ready = Test-GuardReady -Monitor (Get-SirinMonitor)
            legacy_script_retained_for_rollback = Test-Path -LiteralPath $LegacyScript -PathType Leaf
        } | ConvertTo-Json -Depth 5
    }
    'Rollback' {
        if ([string]::IsNullOrWhiteSpace($BackupPath)) {
            throw 'Rollback requires -BackupPath pointing to the exact migration backup JSON.'
        }
        $resolvedBackup = (Resolve-Path -LiteralPath $BackupPath).Path
        if ([System.IO.Path]::GetDirectoryName($resolvedBackup) -ne (Resolve-Path -LiteralPath $BackupDirectory).Path) {
            throw "Backup must be inside the Sirin backup directory: $BackupDirectory"
        }
        $backup = Get-Content -LiteralPath $resolvedBackup -Raw | ConvertFrom-Json
        if ([int]$backup.schema_version -ne 1 -or [string]$backup.run_value_name -ne $RunValueName -or [string]$backup.legacy_script -ne $LegacyScript) {
            throw 'Backup identity does not match the managed ChatGPT awake guard.'
        }
        Restore-LegacyGuard -Backup $backup
        [pscustomobject]@{
            action = 'Rollback'
            status = 'ROLLED_BACK'
            backup_path = $resolvedBackup
            registry_startup_present = $null -ne (Get-LegacyRunValue)
            legacy_process_count = @(Get-LegacyProcesses).Count
        } | ConvertTo-Json -Depth 5
    }
}
