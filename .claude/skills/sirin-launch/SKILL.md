---
name: sirin-launch
description: This skill should be used when the user wants to start, stop, restart, or check the status of Sirin (the AI browser testing service). Trigger phrases include "start Sirin", "launch Sirin", "Sirin 起來", "開啟 Sirin", "is Sirin running", "stop Sirin", "restart Sirin", or before any sirin-test workflow when Sirin's MCP endpoint might not be up.
version: 1.0.0
---

# Sirin Lifecycle Skill

Manage the Sirin process from an external Claude Code session. Required
before `sirin-test` can be used — Sirin's MCP endpoint on
`http://127.0.0.1:7700/mcp` is only live while the process is running.

## When This Skill Applies

- User asks to start, stop, or restart Sirin
- User asks "is Sirin up?" / "check Sirin status"
- About to use the `sirin-test` skill and unsure whether Sirin is running
- User reports "connection refused" or similar when calling Sirin's MCP

## Prerequisites

- Sirin repo cloned at a known path (default: `~/IdeaProjects/Sirin`)
- `cargo` in PATH
- Chrome installed (for browser automation)
- `.env` configured with `GEMINI_API_KEY` (or other LLM provider)

## Status Check (always run first)

Before launching, always probe the MCP endpoint:

```bash
curl -s -X POST http://127.0.0.1:7700/mcp \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  --max-time 3
```

**Interpret response:**
- Returns JSON with `"tools": [...]` → Sirin is up, skip launch
- `curl: (7) Failed to connect` → Sirin not running, proceed to launch
- Returns empty / timeout → Sirin in bad state, restart recommended

## Launch Workflow

### Step 1 — Build (if binary missing or stale)

```bash
cd ~/IdeaProjects/Sirin
ls target/release/sirin.exe 2>/dev/null || ls target/release/sirin 2>/dev/null
```

If binary absent OR source newer than binary:
```bash
cargo build --release 2>&1 | tail -5
```
(Takes 2-6 minutes on cold cache. Warn the user.)

### Step 2 — Launch in background

**Windows (with GUI):**
```bash
cd ~/IdeaProjects/Sirin
# Detach so claude's terminal isn't held
start /b "" target/release/sirin.exe > sirin.log 2>&1
```

**Unix-like:**
```bash
cd ~/IdeaProjects/Sirin
nohup ./target/release/sirin > sirin.log 2>&1 &
disown
```

**Important:** Sirin opens an egui window. On systems without a display,
the launch will fail silently. Warn the user if no display is available.

### Step 3 — Wait for MCP readiness (up to 15s)

```bash
for i in 1 2 3 4 5 6 7 8 9 10; do
  sleep 1.5
  if curl -s -X POST http://127.0.0.1:7700/mcp \
       -H "Content-Type: application/json" \
       -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
       --max-time 2 > /dev/null; then
    echo "✓ Sirin ready after ${i}s"
    exit 0
  fi
done
echo "✗ Sirin did not become ready within 15s"
```

If ready, report the available MCP tools count and continue with the
user's actual request.

If not ready after 15s, read `sirin.log` to diagnose. Common causes:
- `GEMINI_API_KEY not set` → tell user to add to `.env`
- `address already in use` → port 7700 conflict, or previous Sirin still running
- Chrome launch failure → verify Chrome installed

## Stop Workflow

Sirin has no clean-shutdown CLI flag. Find and kill the process:

**Windows:**
```bash
taskkill /F /IM sirin.exe
```

**Unix-like:**
```bash
pkill -f "target/release/sirin$"
# or if launched from dev:
pkill -f "cargo run"
```

Verify it's gone:
```bash
curl -s -X POST http://127.0.0.1:7700/mcp \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  --max-time 2 && echo "still up" || echo "stopped"
```

## Restart Workflow

Stop then launch. The GUI will close and reopen. Any in-progress test
runs lose their in-memory registry (SQLite history persists).

## Anti-patterns

❌ **Don't launch with `cargo run` (non-release build)** — startup is
slow and LLM calls may time out.

❌ **Don't launch synchronously** — Sirin's eframe::run_native blocks
until the window closes. Always detach (`start /b` / `nohup &`).

❌ **Don't loop-poll faster than 1s** — MCP server binds to port 7700
in one of the earlier startup phases; first successful response may
take 3-8s on a warm binary. Be patient.

❌ **Don't assume Sirin is still up between sessions** — user may have
closed the window. Always status-check first.

## Example Session Fragment

```
User: "Run the Agora login smoke test"

Claude Code:
1. Status check → connection refused, Sirin not running
2. Apply this skill:
   a. Binary exists at target/release/sirin.exe → skip build
   b. Launch detached: start /b ... 
   c. Poll until ready (took 5s)
3. Now invoke sirin-test skill workflow:
   a. list_tests(tag="smoke") → found "agora_login"
   b. run_test_async(test_id="agora_login")
   c. poll get_test_result until terminal
4. Report result to user
```

## Troubleshooting Quick Reference

| Symptom | Cause | Fix |
|---------|-------|-----|
| connection refused | Sirin not running | Launch (Step 2) |
| connection timeout | Sirin hung during startup | Stop, check log, restart |
| "No LLM models discovered" in log | API key missing/invalid | Fix `.env`, restart |
| Window opens, closes immediately | Missing display / driver issue | Use `--help` flag for diagnostics |
| Port 7700 in use | Orphaned Sirin process | Kill all `sirin` processes |

## Related Skills

- `sirin-test` — actual E2E testing workflows (requires Sirin running)
