#!/usr/bin/env bash
# scripts/squad-restart.sh — kill → rebuild → relaunch → start N squad workers
#
# Usage:
#   bash scripts/squad-restart.sh          # default 2 workers, port 7710
#   bash scripts/squad-restart.sh 4        # 4 workers
#   SIRIN_RPC_PORT=7712 bash scripts/squad-restart.sh 3
#
# Exits non-zero on build failure or if Sirin doesn't come up within 30s.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# ── Args ──────────────────────────────────────────────────────────────────────
N="${1:-2}"
# Clamp N to [1, 8]
N=$(( N < 1 ? 1 : N > 8 ? 8 : N ))

BASE_PORT="${SIRIN_RPC_PORT:-7710}"

echo "============================================================"
echo "  squad-restart.sh  workers=$N  base_port=$BASE_PORT"
echo "============================================================"

# ── Step 1: Kill running sirin ────────────────────────────────────────────────
echo ""
echo "[1/5] Stopping any running sirin.exe ..."
if command -v powershell >/dev/null 2>&1; then
    powershell -NoProfile -Command \
        "Get-Process sirin -ErrorAction SilentlyContinue | Stop-Process -Force" \
        2>/dev/null || true
elif command -v pkill >/dev/null 2>&1; then
    pkill -f sirin || true
fi

echo "      Waiting 3s for port release ..."
sleep 3

# ── Step 2: Build release binary into isolated swap dir ───────────────────────
echo ""
echo "[2/5] Building release binary (CARGO_TARGET_DIR=target_release_swap) ..."
if ! CARGO_TARGET_DIR=target_release_swap cargo build --release --bin sirin 2>&1; then
    echo ""
    echo "ERROR: cargo build failed — aborting. No binary swapped." >&2
    exit 1
fi
echo "      Build succeeded."

# ── Step 3: Swap binary ───────────────────────────────────────────────────────
echo ""
echo "[3/5] Swapping binary ..."

SWAP_BIN="target_release_swap/release/sirin.exe"
DEST_BIN="target/release/sirin.exe"

# Fallback for Linux/macOS
if [[ ! -f "$SWAP_BIN" ]]; then
    SWAP_BIN="target_release_swap/release/sirin"
    DEST_BIN="target/release/sirin"
fi

if [[ ! -f "$SWAP_BIN" ]]; then
    echo "ERROR: built binary not found at $SWAP_BIN" >&2
    exit 1
fi

mkdir -p "$(dirname "$DEST_BIN")"
cp -f "$SWAP_BIN" "$DEST_BIN"
echo "      $SWAP_BIN → $DEST_BIN"
echo "      binary mtime: $(stat -c %y "$DEST_BIN" 2>/dev/null || stat -f %Sm "$DEST_BIN" 2>/dev/null || echo '?')"
echo "      last commit:  $(git log -1 --format='%ai %h %s')"

# ── Step 4: Launch Sirin in background ────────────────────────────────────────
echo ""
echo "[4/5] Launching Sirin (headless) ..."

# Pick first free port in [BASE_PORT, BASE_PORT+3]
ACTUAL_PORT=""
for offset in 0 1 2 3; do
    TRY=$(( BASE_PORT + offset ))
    if command -v powershell >/dev/null 2>&1; then
        FREE=$(powershell -NoProfile -Command \
            "if ((Get-NetTCPConnection -LocalPort $TRY -State Listen -ErrorAction SilentlyContinue) -eq \$null) { 'free' } else { 'busy' }" \
            2>/dev/null | tr -d '\r\n ')
        [[ "$FREE" == "free" ]] && { ACTUAL_PORT=$TRY; break; }
    else
        ACTUAL_PORT=$TRY
        break
    fi
done

if [[ -z "$ACTUAL_PORT" ]]; then
    echo "ERROR: ports ${BASE_PORT}..$(( BASE_PORT+3 )) all busy." >&2
    exit 1
fi

[[ "$ACTUAL_PORT" != "$BASE_PORT" ]] && \
    echo "      WARN: port $BASE_PORT busy → using $ACTUAL_PORT"

LOG_FILE=".claude/tmp/sirin_relaunch.log"
mkdir -p ".claude/tmp"

SIRIN_RPC_PORT="$ACTUAL_PORT" \
SIRIN_BROWSER_HEADLESS=true \
    "$DEST_BIN" --headless >> "$LOG_FILE" 2>&1 &
SIRIN_PID=$!
echo "      PID=$SIRIN_PID  port=$ACTUAL_PORT  log=$LOG_FILE"

# ── Step 5: Wait for Sirin to be healthy, then start workers ─────────────────
echo ""
echo "[5/5] Waiting for Sirin to be ready (up to 30s) ..."

MCP_URL="http://127.0.0.1:${ACTUAL_PORT}/mcp"
DEADLINE=$(( $(date +%s) + 30 ))
READY=0

while [[ $(date +%s) -lt $DEADLINE ]]; do
    HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 2 \
        -X POST "$MCP_URL" \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
        2>/dev/null || echo "000")
    if [[ "$HTTP_CODE" == "200" ]]; then
        READY=1
        break
    fi
    printf "."
    sleep 2
done
echo ""

if [[ $READY -eq 0 ]]; then
    echo "ERROR: Sirin did not respond within 30s on port $ACTUAL_PORT." >&2
    echo "       Check log: $LOG_FILE" >&2
    exit 1
fi

echo "      Sirin is up on :${ACTUAL_PORT}"

# Start N workers
echo "      Starting $N squad worker(s) ..."
START_RESP=$(curl -s --max-time 10 \
    -X POST "$MCP_URL" \
    -H "Content-Type: application/json" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"agent_start_worker\",\"arguments\":{\"n\":$N}}}" \
    2>/dev/null || echo "")

# ── Final status ──────────────────────────────────────────────────────────────
echo ""
echo "------------------------------------------------------------"
echo "  STATUS REPORT"
echo "------------------------------------------------------------"

STATUS_RESP=$(curl -s --max-time 5 \
    -X POST "$MCP_URL" \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"agent_queue_status","arguments":{}}}' \
    2>/dev/null || echo "")

printf '%s' "$STATUS_RESP" | node -e "
const chunks = [];
process.stdin.on('data', c => chunks.push(c));
process.stdin.on('end', () => {
  const raw = Buffer.concat(chunks).toString('utf8');
  const port = '${ACTUAL_PORT}';
  const workers = ${N};
  try {
    const outer = JSON.parse(raw);
    let payload = outer;
    if (outer.result !== undefined) payload = outer.result;
    if (payload && payload.content && Array.isArray(payload.content)) {
      payload = JSON.parse(payload.content[0].text);
    }
    const tasks = Array.isArray(payload) ? payload : (payload.tasks || []);
    const counts = { done:0, queued:0, running:0, failed:0 };
    for (const t of tasks) { const s = t.status || 'unknown'; counts[s] = (counts[s]||0) + 1; }
    console.log('  PORT     : ' + port);
    console.log('  WORKERS  : ' + workers + ' started');
    console.log('  QUEUE    : queued=' + (counts.queued||0)
                          + '  running=' + (counts.running||0)
                          + '  done=' + (counts.done||0)
                          + '  failed=' + (counts.failed||0));
    console.log('  TOTAL    : ' + tasks.length + ' tasks');
  } catch(e) {
    console.log('  PORT     : ' + port);
    console.log('  WORKERS  : ' + workers + ' started');
    console.log('  QUEUE    : (could not parse status — ' + e.message + ')');
  }
});
" 2>/dev/null || {
    echo "  PORT     : $ACTUAL_PORT"
    echo "  WORKERS  : $N started"
    echo "  QUEUE    : (node not available for status parse)"
}

echo "------------------------------------------------------------"
echo "  Done.  Re-run is idempotent — kills + restarts cleanly."
echo "============================================================"
