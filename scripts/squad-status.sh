#!/usr/bin/env bash
# squad-status.sh — pretty-print the multi-agent task queue
# Usage: bash scripts/squad-status.sh
#        SIRIN_RPC_PORT=7705 bash scripts/squad-status.sh

PORT="${SIRIN_RPC_PORT:-7700}"
URL="http://127.0.0.1:${PORT}/mcp"

# ── fetch ──────────────────────────────────────────────────────────────────────
RESPONSE=$(curl -s --max-time 5 -X POST "$URL" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agent_queue_status","arguments":{}}}' \
  2>/dev/null)

if [ -z "$RESPONSE" ]; then
  echo "❌ Sirin not reachable on :${PORT}"
  exit 1
fi

# ── parse + render via node (stdin — avoids Windows /tmp path issues) ──────────
printf '%s' "$RESPONSE" | node -e "
const chunks = [];
process.stdin.on('data', c => chunks.push(c));
process.stdin.on('end', () => {
  const raw = Buffer.concat(chunks).toString('utf8');

  let tasks = [];
  try {
    const outer = JSON.parse(raw);
    let payload = outer;
    if (outer.result !== undefined) payload = outer.result;
    // MCP tools/call wraps result in content[0].text
    if (payload && payload.content && Array.isArray(payload.content)) {
      payload = JSON.parse(payload.content[0].text);
    }
    tasks = Array.isArray(payload) ? payload : (payload.tasks || []);
  } catch(e) {
    console.error('❌ Failed to parse response:', e.message);
    console.error('Raw:', raw.slice(0, 300));
    process.exit(1);
  }

  // summary counts
  const counts = { done:0, queued:0, running:0, failed:0 };
  for (const t of tasks) { const s = t.status || 'unknown'; counts[s] = (counts[s]||0) + 1; }
  const port = '${PORT}';
  console.log('Sirin squad @ 127.0.0.1:' + port);
  console.log(
    'done='    + (counts.done||0)    +
    ' queued=' + (counts.queued||0)  +
    ' running='+ (counts.running||0) +
    ' failed=' + (counts.failed||0)  +
    ' (total ' + tasks.length + ')'
  );
  console.log('');

  // ANSI colors
  const C = { reset:'\x1b[0m', cyan:'\x1b[36m', yellow:'\x1b[33m', green:'\x1b[32m', red:'\x1b[31m' };
  const colorOf = s => ({ running:C.cyan, queued:C.yellow, done:C.green, failed:C.red }[s] || '');

  // human-readable age
  function age(iso) {
    if (!iso) return '?';
    const secs = Math.max(0, Math.floor((Date.now() - new Date(iso).getTime()) / 1000));
    if (secs < 60) return secs + 's';
    const m = Math.floor(secs / 60) % 60;
    const h = Math.floor(secs / 3600);
    return h > 0 ? h + 'h' + String(m).padStart(2,'0') + 'm' : m + 'm';
  }

  const trunc = (s, n) => s.length > n ? s.slice(0, n) + '\u2026' : s;

  // newest first, cap at 8
  const rows = [...tasks].reverse().slice(0, 8);

  console.log('STATUS    | ID       | AGE    | DESCRIPTION');
  console.log('----------+----------+--------+' + '-'.repeat(72));

  for (const t of rows) {
    const status = (t.status || 'unknown').padEnd(9);
    const id     = (t.id || '').slice(-8).padEnd(8);
    const a      = age(t.created_at).padEnd(7);
    const desc   = trunc((t.description || '').replace(/\n/g, ' '), 70);
    const col    = colorOf(t.status);
    console.log(col + status + C.reset + ' | ' + id + ' | ' + a + '| ' + desc);
  }
});
" 2>&1
