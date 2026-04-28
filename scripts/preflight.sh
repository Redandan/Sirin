#!/usr/bin/env bash
# Sirin Pre-flight Checklist
# Run this before any benchmark / LLM comparison
# Usage: bash scripts/preflight.sh
set -euo pipefail

ENV_FILE="$LOCALAPPDATA/Sirin/.env"
PASS=0; FAIL=0; WARN=0

ok()   { echo "  ✅ $1"; PASS=$((PASS+1)); }
fail() { echo "  ❌ FAIL: $1"; FAIL=$((FAIL+1)); }
warn() { echo "  ⚠️  WARN: $1"; WARN=$((WARN+1)); }

echo "======================================="
echo "  Sirin Pre-flight Check ($(date +%H:%M))"
echo "======================================="

# ── 1. Core LLM Keys ─────────────────────────────────────────────────────────
echo
echo "1. Core LLM Keys"

check_key() {
  local var=$1; local min_len=${2:-20}
  local val=$(grep "^${var}=" "$ENV_FILE" 2>/dev/null | head -1 | cut -d= -f2-)
  local len=${#val}
  if [ -z "$val" ]; then
    fail "$var is EMPTY (not set)"
  elif [ "$len" -lt "$min_len" ]; then
    fail "$var too short (${len} chars, expected >=${min_len})"
  else
    ok "$var set (${len} chars)"
  fi
}

LLM_PROVIDER=$(grep "^LLM_PROVIDER=" "$ENV_FILE" 2>/dev/null | cut -d= -f2-)
ok "LLM_PROVIDER=${LLM_PROVIDER}"

case "$LLM_PROVIDER" in
  gemini)  check_key "GEMINI_API_KEY" 30 ;;
  anthropic) check_key "ANTHROPIC_API_KEY" 50 ;;
  lmstudio) check_key "LM_STUDIO_API_KEY" 20 ;;
  *) warn "Unknown LLM_PROVIDER='$LLM_PROVIDER'" ;;
esac

# Fallback LLM (429 auto-switch)
FALLBACK_URL=$(grep "^LLM_FALLBACK_BASE_URL=" "$ENV_FILE" 2>/dev/null | cut -d= -f2-)
FALLBACK_KEY=$(grep "^LLM_FALLBACK_API_KEY=" "$ENV_FILE" 2>/dev/null | cut -d= -f2-)
FALLBACK_MODEL=$(grep "^LLM_FALLBACK_MODEL=" "$ENV_FILE" 2>/dev/null | cut -d= -f2-)

if [ -z "$FALLBACK_URL" ]; then
  warn "LLM_FALLBACK_BASE_URL not set → no 429 auto-switch (Gemini rate-limit will stall tests 35s+ per retry)"
elif [ -z "$FALLBACK_KEY" ] || [ "${#FALLBACK_KEY}" -lt 20 ]; then
  fail "LLM_FALLBACK_BASE_URL set but LLM_FALLBACK_API_KEY empty/short → fallback DISABLED"
else
  ok "Fallback LLM: ${FALLBACK_MODEL} @ ${FALLBACK_URL} (key ${#FALLBACK_KEY} chars)"
fi

# Vision specialist
VISION_URL=$(grep "^LLM_VISION_BASE_URL=" "$ENV_FILE" 2>/dev/null | cut -d= -f2-)
VISION_KEY=$(grep "^LLM_VISION_API_KEY=" "$ENV_FILE" 2>/dev/null | cut -d= -f2-)
VISION_MODEL=$(grep "^LLM_VISION_MODEL=" "$ENV_FILE" 2>/dev/null | cut -d= -f2-)

if [ -n "$VISION_URL" ] && [ -z "$VISION_KEY" ]; then
  fail "LLM_VISION_BASE_URL set but LLM_VISION_API_KEY EMPTY → vision specialist DISABLED"
elif [ -n "$VISION_URL" ] && [ "${#VISION_KEY}" -lt 20 ]; then
  fail "LLM_VISION_API_KEY too short (${#VISION_KEY} chars) → may be truncated"
elif [ -n "$VISION_URL" ]; then
  ok "Vision specialist: ${VISION_MODEL} (key ${#VISION_KEY} chars)"
else
  warn "LLM_VISION_BASE_URL not set → vision specialist disabled (fallback to main LLM)"
fi

# ── 2. Sirin Running ──────────────────────────────────────────────────────────
echo
echo "2. Sirin Process"

HTTP=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:7700/gateway --max-time 3 2>/dev/null)
if [ "$HTTP" = "200" ]; then
  ok "Sirin gateway responding (HTTP 200)"
else
  fail "Sirin NOT running (HTTP ${HTTP})"
fi

# ── 3. Vision Specialist Quick Smoke ─────────────────────────────────────────
if [ -n "$VISION_URL" ] && [ "${#VISION_KEY}" -ge 20 ]; then
  echo
  echo "3. Vision Specialist Smoke Test"
  SMOKE=$(curl -s -o /tmp/pf_vision_smoke.json -w "%{http_code}" \
    -X POST "${VISION_URL}/chat/completions" \
    -H "Authorization: Bearer ${VISION_KEY}" \
    -H "Content-Type: application/json" \
    -H "HTTP-Referer: https://sirin.local" \
    -H "X-Title: Sirin Preflight" \
    -d "{\"model\":\"${VISION_MODEL}\",\"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"reply OK\"},{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==\"}}]}],\"max_tokens\":5}" \
    --max-time 15 2>/dev/null)
  if [ "$SMOKE" = "200" ]; then
    ok "Vision model ${VISION_MODEL} accepts image input (HTTP 200)"
  elif [ "$SMOKE" = "402" ]; then
    fail "Vision model returned 402 — OpenRouter free-tier IMAGE quota exhausted (text-only OK)"
  elif [ "$SMOKE" = "401" ]; then
    fail "Vision model returned 401 — API key invalid/empty"
  else
    warn "Vision model returned HTTP ${SMOKE} (may still work for text-only)"
  fi
fi

# ── 4. YAML Config Check ──────────────────────────────────────────────────────
echo
echo "4. YAML Test Health"

LOCALAPP_YAML="$LOCALAPPDATA/Sirin/config/tests/agora_regression/agora_pickup_time_picker.yaml"
REPO_YAML="config/tests/agora_regression/agora_pickup_time_picker.yaml"
if [ -f "$REPO_YAML" ] && [ -f "$LOCALAPP_YAML" ]; then
  if diff -q "$REPO_YAML" "$LOCALAPP_YAML" > /dev/null 2>&1; then
    ok "pickup_time_picker YAML: repo == LOCALAPPDATA (in sync)"
  else
    fail "pickup_time_picker YAML: repo != LOCALAPPDATA → run: cp $REPO_YAML $LOCALAPP_YAML"
  fi
fi

# Viewport check — buyer H5 tests must specify 390×844 mobile viewport
BUYER_MISSING_VP=0
for f in $(find config/tests -name "*.yaml" 2>/dev/null | head -30); do
  if grep -q "__test_role=buyer" "$f" 2>/dev/null; then
    if ! grep -q "^viewport:" "$f" 2>/dev/null; then
      warn "$(basename $f .yaml): __test_role=buyer but missing viewport block (should be 390×844)"
      BUYER_MISSING_VP=$((BUYER_MISSING_VP+1))
    fi
  fi
done
if [ "$BUYER_MISSING_VP" -eq 0 ]; then
  ok "All buyer-side tests have correct H5 viewport (390×844)"
fi

# Check for lenient acceptance patterns
for f in $(find config/tests -name "*.yaml" 2>/dev/null | head -10); do
  name=$(basename "$f" .yaml)
  if grep -q "不論任何步驟失敗\|regardless.*fail\|continue regardless" "$f" 2>/dev/null; then
    warn "$name: lenient acceptance (不論失敗都繼續) — may mask real failures"
  fi
done

# ── 5. Recent Chrome Stability ────────────────────────────────────────────────
echo
echo "5. Recent Chrome Stability"
if [ -f "sirin.err.log" ]; then
  CRASHES=$(grep -cE "Chrome crashed|connection closed.*recovering" sirin.err.log 2>/dev/null | tr -d "
" || echo 0)
  LAUNCHES=$(grep -c "launched Chrome" sirin.err.log 2>/dev/null || echo 0)
  if [ "$CRASHES" -gt 5 ]; then
    warn "sirin.err.log shows ${CRASHES} Chrome crashes — check SIRIN_PERSISTENT_PROFILE"
  elif [ "$CRASHES" -gt 0 ]; then
    warn "sirin.err.log shows ${CRASHES} Chrome crashes (acceptable)"
  else
    ok "No Chrome crashes in sirin.err.log"
  fi
  ok "Chrome launches: ${LAUNCHES}"
fi

# ── 6. Action Registry Consistency ────────────────────────────────────────────
echo
echo "6. Action Registry Consistency (mcp_server vs builtins)"
if [ -f "src/mcp_server.rs" ] && [ -f "src/adk/tool/builtins.rs" ]; then
  # High-risk browser actions that should be in both callers.
  # Since Issue #115 the shared dispatch lives in src/browser_exec.rs; actions
  # defined there count as "registered in builtins" because builtins delegates
  # to browser_exec::dispatch().  We search both files.
  BROWSER_ACTIONS="goto screenshot screenshot_analyze click click_point type read eval wait exists attr scroll key console network url title close set_viewport enable_a11y ax_tree ax_find ax_value ax_click ax_focus ax_type ax_type_verified ax_snapshot ax_diff wait_for_ax_change wait_for_url wait_for_ax_ready wait_for_network_idle assert_ax_contains assert_url_matches shadow_find shadow_click shadow_dump flutter_type flutter_enter shadow_type_flutter go_back clear_state wait_new_tab wait_request list_sessions close_session dom_snapshot"
  MISSING_COUNT=0
  for action in $BROWSER_ACTIONS; do
    if ! grep -q "\"$action\"" src/adk/tool/builtins.rs src/browser_exec.rs 2>/dev/null; then
      warn "browser action '$action' missing from builtins.rs and browser_exec.rs"
      MISSING_COUNT=$((MISSING_COUNT+1))
    fi
  done
  if [ "$MISSING_COUNT" -eq 0 ]; then
    ok "all browser actions registered in builtins.rs (or shared browser_exec.rs)"
  fi
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo
echo "======================================="
echo "  Result: ${PASS} OK | ${WARN} WARN | ${FAIL} FAIL"
echo "======================================="

if [ "$FAIL" -gt 0 ]; then
  echo "  ❌ PREFLIGHT FAILED — fix the issues above before running benchmarks"
  exit 1
elif [ "$WARN" -gt 0 ]; then
  echo "  ⚠️  PREFLIGHT PASSED with warnings"
  exit 0
else
  echo "  ✅ PREFLIGHT PASSED — ready to run"
  exit 0
fi
