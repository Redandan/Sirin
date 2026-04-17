# scripts/kill-port.ps1
#
# Pure-PowerShell sibling of kill-port.sh, for callers invoking from
# cmd.exe / pwsh without Git Bash.  Same contract:
#
#   - Silent if no LISTEN socket on $Port.
#   - Best-effort Stop-Process -Force; warns on stderr but exits 0
#     so a caller's port-fallback safety net still runs.
#   - Defense against Windows zombie listener sockets (issue #14).
#
# Usage:
#   powershell -File scripts/kill-port.ps1 7700
#   pwsh      -File scripts/kill-port.ps1 -Port 7700

param(
    [Parameter(Mandatory = $true, Position = 0)]
    [int]$Port
)

$ErrorActionPreference = 'SilentlyContinue'

# Find the LISTEN owner (if any). Any errors go to null.
$conn = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue 2>$null |
    Select-Object -First 1

if ($null -eq $conn -or $null -eq $conn.OwningProcess -or $conn.OwningProcess -eq 0) {
    # No listener — silent success.
    exit 0
}

$targetPid = $conn.OwningProcess
Write-Host "[kill-port] killing PID $targetPid on port $Port"

# Best-effort kill; swallow all errors so the caller can still fallback.
Stop-Process -Id $targetPid -Force -ErrorAction SilentlyContinue 2>$null

# Verify.  If the socket is still bound, warn but do not fail.
$still = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue 2>$null
if ($null -ne $still) {
    [Console]::Error.WriteLine("[kill-port] warning: port $Port still LISTENING after kill attempt (PID $targetPid, need admin?)")
}

exit 0
