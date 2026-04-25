#!/bin/bash
# scripts/dev-relaunch.sh
#
# Safe rebuild + relaunch loop for Sirin dev.  Solves three Windows pitfalls:
#
#   1. Stale .exe — Windows holds sirin.exe open while a process is running,
#      so `cargo build --release` fails (sometimes silently) and you end up
#      running yesterday's binary against today's source.
#   2. Port zombie (issue #14) — old TCP listener may still be bound after
#      the process exits.  We auto-fall-through to a +1 port when that happens.
#   3. Forgetting to rebuild before smoke test — bit us today (eab8537 commit
#      had robustness actions but the .exe was 10h older → "Unknown action").
#
# Usage:
#
#   ./scripts/dev-relaunch.sh                       # default port 7700, headless
#   SIRIN_RPC_PORT=7702 ./scripts/dev-relaunch.sh   # custom port
#   SIRIN_BROWSER_HEADLESS=false ./scripts/dev-relaunch.sh   # for Flutter
#   ./scripts/dev-relaunch.sh --build-only          # build, do not launch
#
# Exits non-zero if cargo build fails (does NOT launch a stale binary).

set -e
set -o pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

BUILD_ONLY=0
[[ "$1" == "--build-only" ]] && { BUILD_ONLY=1; shift; }

PORT="${SIRIN_RPC_PORT:-7700}"
HEADLESS="${SIRIN_BROWSER_HEADLESS:-true}"
KILL_ZOMBIE="${SIRIN_KILL_ZOMBIE_PORTS:-1}"

# ── 1. Kill any running sirin (Windows .exe lock + port reuse) ─────────────
echo "[1/4] Killing any running sirin.exe..."
if command -v powershell >/dev/null 2>&1; then
    powershell -c "Get-Process sirin -ErrorAction SilentlyContinue | Stop-Process -Force" 2>/dev/null || true
elif command -v pkill >/dev/null 2>&1; then
    pkill -f sirin || true
fi
sleep 1

# ── 1b. Sweep zombie TCP listeners on candidate ports (issue #14) ──────────
# Windows sometimes leaves the listen socket bound to a PID that tasklist
# can no longer see — killing "by process name" in step 1 doesn't catch
# those.  Walk the candidate port range and kill whichever process owns
# each LISTEN socket.  Opt-out:  SIRIN_KILL_ZOMBIE_PORTS=0
if [[ "$KILL_ZOMBIE" == "1" ]]; then
    echo "[1b/4] Sweeping zombie listeners on ports $PORT..$((PORT+3))..."
    for offset in 0 1 2 3; do
        bash "$REPO_ROOT/scripts/kill-port.sh" "$((PORT + offset))" || true
    done
else
    echo "[1b/4] SIRIN_KILL_ZOMBIE_PORTS=0 — skipping zombie sweep"
fi

# ── 2. Build release ────────────────────────────────────────────────────────
echo "[2/4] cargo build --release..."
cargo build --release

# ── 2b. Sync test YAMLs from repo → LOCALAPPDATA (reads AppData at runtime) ─
# Binary reads %LOCALAPPDATA%\Sirin\config\tests\ — not the repo's config/.
# After any YAML edit, we must sync or the binary uses a stale version.
if command -v powershell >/dev/null 2>&1; then
    WIN_APPDATA=$(powershell -NoProfile -Command '$env:LOCALAPPDATA' 2>/dev/null | tr -d '\r\n')
    if [[ -n "$WIN_APPDATA" ]]; then
        DEST="$WIN_APPDATA/Sirin/config/tests"
        # Convert Windows path to bash path for cp
        DEST_BASH=$(echo "$DEST" | sed 's|\\|/|g' | sed 's|^\([A-Za-z]\):|/\L\1|')
        if [[ -d "$DEST_BASH" ]]; then
            echo "[2b/4] Syncing config/tests → $DEST ..."
            find config/tests -name "*.yaml" | while read -r f; do
                rel="${f#config/tests/}"
                dir_part=$(dirname "$rel")
                mkdir -p "$DEST_BASH/$dir_part"
                cp -f "$f" "$DEST_BASH/$rel"
            done
            echo "[2b/4] Sync done."
        else
            echo "[2b/4] WARN: $DEST not found, skipping YAML sync."
        fi
    fi
else
    echo "[2b/4] WARN: powershell not available, skipping YAML sync."
fi

# ── 3. Sanity: binary mtime vs latest commit ────────────────────────────────
echo "[3/4] Binary check:"
if [[ -f target/release/sirin.exe ]]; then
    BIN=target/release/sirin.exe
elif [[ -f target/release/sirin ]]; then
    BIN=target/release/sirin
else
    echo "  ERROR: no release binary found" >&2
    exit 1
fi
echo "  binary:        $BIN ($(stat -c %y "$BIN" 2>/dev/null || stat -f %Sm "$BIN"))"
echo "  latest commit: $(git log -1 --format='%ai %h %s')"

[[ $BUILD_ONLY -eq 1 ]] && { echo "[4/4] --build-only set, exiting."; exit 0; }

# ── 4. Probe port; fall through to +1 if zombie-occupied ────────────────────
PROBED=""
for offset in 0 1 2 3; do
    TRY=$((PORT + offset))
    if command -v powershell >/dev/null 2>&1; then
        # Use Test-NetConnection-equivalent check.  Get-NetTCPConnection
        # returns nothing when free → echoed "True" by `-eq $null`.
        FREE=$(powershell -NoProfile -Command \
            "if ((Get-NetTCPConnection -LocalPort $TRY -State Listen -ErrorAction SilentlyContinue) -eq \$null) { 'free' } else { 'busy' }" 2>/dev/null | tr -d '\r\n ')
        if [[ "$FREE" == "free" ]]; then
            PROBED=$TRY
            break
        fi
    else
        # POSIX: just trust the env var
        PROBED=$TRY
        break
    fi
done
if [[ -z "$PROBED" ]]; then
    echo "  ERROR: ports $PORT..$((PORT+3)) all occupied" >&2
    exit 1
fi
[[ "$PROBED" != "$PORT" ]] && echo "  WARN: port $PORT busy, falling through to $PROBED"

echo "[4/4] Launching: SIRIN_RPC_PORT=$PROBED SIRIN_BROWSER_HEADLESS=$HEADLESS $BIN $@"
echo
SIRIN_RPC_PORT="$PROBED" \
    SIRIN_BROWSER_HEADLESS="$HEADLESS" \
    exec "$BIN" "$@"
