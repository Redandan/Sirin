#!/bin/bash
# scripts/kill-port.sh
#
# Cross-platform helper: find whoever owns the TCP listener on <port> and
# kill -9 that PID, regardless of what the process is called.  This is the
# defense against Windows zombie listener sockets (issue #14) — sometimes
# after sirin.exe dies the TCP stack keeps the listen socket bound under
# a stale PID that tasklist can't even see, so killing "by name" isn't
# enough.
#
# Usage:
#   bash scripts/kill-port.sh 7700
#
# Semantics:
#   - No listener on <port>   → silent, exit 0
#   - Listener found, killed  → prints one-liner, exit 0
#   - Listener found, kill failed (permissions etc.) → warn on stderr, exit 0
#     (caller — typically dev-relaunch.sh — still has its port-fallback
#      safety net, so we don't want to abort the whole launch here)
#
# Detects OS: uses PowerShell Get-NetTCPConnection on Windows/Git-Bash,
# otherwise falls back to lsof (macOS / Linux).

set -e
set -o pipefail

PORT="${1:-}"
if [[ -z "$PORT" ]]; then
    echo "usage: $0 <port>" >&2
    exit 2
fi

# ── Windows / Git Bash path ─────────────────────────────────────────────────
if command -v powershell >/dev/null 2>&1; then
    # Grab OwningProcess (PID) of the LISTEN socket on $PORT, if any.
    # Redirect all error streams so a missing socket doesn't spew red.
    PID_RAW=$(powershell -NoProfile -Command \
        "(Get-NetTCPConnection -LocalPort $PORT -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty OwningProcess) 2>\$null" \
        2>/dev/null | tr -d '\r\n ')

    if [[ -z "$PID_RAW" || "$PID_RAW" == "0" ]]; then
        # No listener — silent success.
        exit 0
    fi

    echo "[kill-port] killing PID $PID_RAW on port $PORT"
    KILL_OUT=$(powershell -NoProfile -Command \
        "Stop-Process -Id $PID_RAW -Force -ErrorAction SilentlyContinue 2>\$null; exit 0" \
        2>&1 || true)

    # Best-effort: if PID still listening we warn but don't fail.
    STILL=$(powershell -NoProfile -Command \
        "if ((Get-NetTCPConnection -LocalPort $PORT -State Listen -ErrorAction SilentlyContinue) -eq \$null) { 'gone' } else { 'stuck' }" \
        2>/dev/null | tr -d '\r\n ')
    if [[ "$STILL" != "gone" ]]; then
        echo "[kill-port] warning: port $PORT still LISTENING after kill attempt (PID $PID_RAW, need admin?)" >&2
    fi
    exit 0
fi

# ── macOS / Linux path ──────────────────────────────────────────────────────
if command -v lsof >/dev/null 2>&1; then
    PID_RAW=$(lsof -i :"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -n1 || true)
    if [[ -z "$PID_RAW" ]]; then
        # No listener — silent success.
        exit 0
    fi
    echo "[kill-port] killing PID $PID_RAW on port $PORT"
    kill -9 "$PID_RAW" 2>/dev/null || {
        echo "[kill-port] warning: kill -9 $PID_RAW failed (permissions?)" >&2
    }
    exit 0
fi

# ── No known tool available ────────────────────────────────────────────────
echo "[kill-port] warning: neither powershell nor lsof available; cannot probe port $PORT" >&2
exit 0
