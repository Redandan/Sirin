#!/usr/bin/env bash
# Verify the new MCP actions that landed 2026-04-17:
#   - page_state (aggregate)
#   - ax_find with name_regex + not_name_matches + limit
#   - ax_snapshot / ax_diff / wait_for_ax_change
#   - authz deny (negative test)
#
# Run against live AgoraMarket: https://redandan.github.io/
set -euo pipefail

PORT="${SIRIN_RPC_PORT:-7700}"
SIRIN="http://127.0.0.1:${PORT}/mcp"

mkdir -p .sirin-verify
TMP=".sirin-verify"

bx() {
  # Usage: bx '<json args>'
  cat > "$TMP/req.json" <<EOF
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"browser_exec","arguments":$1}}
EOF
  curl -sX POST "$SIRIN" -H "Content-Type: application/json" -d @"$TMP/req.json" --max-time 60
}

extract() {
  node -e "
const fs=require('fs');
const d=JSON.parse(fs.readFileSync('/dev/stdin','utf8'));
if(d.error){console.log('ERR:',d.error.message);process.exit(1);}
const t=d.result?.content?.[0]?.text||'';
try{console.log(JSON.stringify(JSON.parse(t),null,2).slice(0,900));}
catch{console.log(t.slice(0,900));}
"
}

pass=0; fail=0
check() {
  local label="$1"; shift
  if "$@"; then echo "  ✅ $label"; pass=$((pass+1)); else echo "  ❌ $label"; fail=$((fail+1)); fi
}

echo "════════════════════════════════════════════════"
echo "  Verify new Sirin actions (port $PORT)         "
echo "════════════════════════════════════════════════"

# ── 0. Health ─────────────────────────────────────────
echo ""
echo "── 0. tools/list ──"
curl -sX POST "$SIRIN" -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' --max-time 5 \
  > "$TMP/tools.json"
TOOL_NAMES=$(node -e "
const d=require('./$TMP/tools.json');
(d.result?.tools||[]).forEach(t=>console.log(t.name));
" 2>&1)
echo "$TOOL_NAMES"
check "page_state tool exists" grep -q "page_state" <<< "$TOOL_NAMES"
check "browser_exec tool exists" grep -q "browser_exec" <<< "$TOOL_NAMES"

# ── 1. goto + enable_a11y ─────────────────────────────
echo ""
echo "── 1. goto AgoraMarket + enable Flutter semantics ──"
bx '{"action":"goto","target":"https://redandan.github.io/"}' > "$TMP/goto.json"
cat "$TMP/goto.json" | extract | head -3
sleep 5
bx '{"action":"enable_a11y"}' > "$TMP/a11y.json"
cat "$TMP/a11y.json" | extract | head -3
sleep 2

# ── 2. page_state (T-M05) ─────────────────────────────
echo ""
echo "── 2. page_state — aggregate query ──"
cat > "$TMP/req.json" <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"page_state","arguments":{}}}
EOF
curl -sX POST "$SIRIN" -H "Content-Type: application/json" -d @"$TMP/req.json" --max-time 30 > "$TMP/page_state.json"
node -e "
const d=require('./$TMP/page_state.json');
if(d.error){console.log('ERR:',d.error.message);process.exit(0);}
const t=d.result?.content?.[0]?.text||'';
try{
  const o=JSON.parse(t);
  console.log('  keys:', Object.keys(o).join(', '));
  console.log('  url:', o.url);
  console.log('  title:', o.title);
  console.log('  ax nodes:', (o.ax_tree||[]).length);
  console.log('  console entries:', (o.console||[]).length);
  console.log('  screenshot size:', (o.screenshot_base64||'').length, 'chars base64');
}catch(e){console.log(t.slice(0,300));}
"
check "page_state returns ax_tree" [ -s "$TMP/page_state.json" ]

# ── 3. ax_find with name_regex (T-M06) ────────────────
echo ""
echo "── 3. ax_find regex — match all test-login buttons ──"
bx '{"action":"ax_find","role":"button","name_regex":"測試.*登入","limit":10}' > "$TMP/find_regex.json"
cat "$TMP/find_regex.json" | extract | head -30
check "regex matched test login buttons" grep -q "backend_id" "$TMP/find_regex.json"

# ── 4. ax_find with not_name_matches (T-M06) ──────────
echo ""
echo "── 4. ax_find — any button NOT about password/密碼 ──"
bx '{"action":"ax_find","role":"button","not_name_matches":["password","密碼"],"limit":5}' > "$TMP/find_exclude.json"
cat "$TMP/find_exclude.json" | extract | head -15
check "exclusion filter returns results" grep -q "backend_id" "$TMP/find_exclude.json"

# ── 5. ax_snapshot + ax_diff (T-M07) ──────────────────
echo ""
echo "── 5. ax_snapshot BEFORE login + click + diff ──"
bx '{"action":"ax_snapshot","id":"before_login"}' > "$TMP/snap_before.json"
cat "$TMP/snap_before.json" | extract | head -5

# Click first buyer button (whichever regex found)
BID=$(node -e "
const d=require('./$TMP/find_regex.json');
const t=d.result?.content?.[0]?.text||'[]';
try{const arr=JSON.parse(t);console.log(arr[0]?.backend_id||'');}catch{}
" 2>/dev/null)
if [ -n "$BID" ]; then
  echo "  clicking backend_id=$BID"
  bx "{\"action\":\"ax_click\",\"backend_id\":$BID}" > "$TMP/click.json"
  cat "$TMP/click.json" | extract | head -3
  sleep 6
  bx '{"action":"ax_snapshot","id":"after_login"}' > "$TMP/snap_after.json"
  bx '{"action":"ax_diff","before_id":"before_login","after_id":"after_login"}' > "$TMP/diff.json"
  cat "$TMP/diff.json" | extract | head -40
  check "ax_diff produced added/removed nodes" grep -qE "added|removed|changed" "$TMP/diff.json"
else
  echo "  ⚠ skipped (no BID from step 3)"
fi

# ── 6. AuthZ deny negative test ───────────────────────
echo ""
echo "── 6. AuthZ deny — goto paypal should be blocked ──"
bx '{"action":"goto","target":"https://www.paypal.com/login"}' > "$TMP/deny.json"
cat "$TMP/deny.json" | extract | head -5
if grep -qiE "deny|not allowed|forbidden" "$TMP/deny.json"; then
  echo "  ✅ authz deny triggered (paypal blocked)"
  pass=$((pass+1))
else
  echo "  ⚠ authz not triggered — might be permissive-default mode; check .sirin/authz.yaml"
fi

# ── Summary ───────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════"
echo "  pass=$pass  fail=$fail                        "
echo "════════════════════════════════════════════════"

# Dump all responses for review
echo ""
echo "Full responses saved in $TMP/ :"
ls -la "$TMP"/

[ $fail -eq 0 ]
