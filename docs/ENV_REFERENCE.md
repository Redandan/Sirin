# Sirin Environment Variables Reference

All variables are optional unless marked **(required)**.
Set in `.env` or system environment.

## LLM Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `LLM_PROVIDER` | `ollama` | Backend: `ollama`, `lmstudio`/`openai`, `gemini`, `anthropic`/`claude` |
| `OLLAMA_BASE_URL` | `http://localhost:11434` | Ollama API endpoint |
| `OLLAMA_MODEL` | `llama3.2` | Default model name |
| `LM_STUDIO_BASE_URL` | `http://localhost:1234/v1` | LM Studio / OpenAI-compatible endpoint |
| `LM_STUDIO_MODEL` | `llama3.2` | Model name for LM Studio |
| `LM_STUDIO_API_KEY` | *(empty)* | Optional Bearer token |
| `SIRIN_LMSTUDIO_AUTOSTART` | `1` | When LM Studio is configured on localhost but `/v1/models` is unreachable, Sirin tries `lms server start` and then the LM Studio app before retrying. Set `0`/`false`/`off` to disable. |
| `SIRIN_LMSTUDIO_AUTOSTART_WAIT_SECS` | `20` | Max seconds to wait for the LM Studio local server after auto-start. |
| `LMS_CLI` / `LM_STUDIO_CLI` | `lms` | Optional explicit path to the LM Studio CLI used for `lms server start`. |
| `LM_STUDIO_APP_PATH` | auto-detect | Optional explicit path to `LM Studio.exe` when the CLI is not on `PATH`. |
| `GEMINI_API_KEY` | | Google Gemini API key |
| `GEMINI_MODEL` | `gemini-2.0-flash` | Gemini model |
| `ANTHROPIC_API_KEY` | | Anthropic Claude API key |
| `ANTHROPIC_MODEL` | `claude-sonnet-4-6` | Claude model |
| `ROUTER_MODEL` | *(falls back to main)* | Small model for Planner/Router (kept resident) |
| `ROUTER_LLM_PROVIDER` | *(falls back to main)* | Separate provider for Router (keep local) |
| `CODING_MODEL` | *(falls back to main)* | Dedicated coding model |
| `LARGE_MODEL` | *(falls back to main)* | Large model for deep reasoning |
| `GEMINI_CONCURRENCY` | `3` | Max in-flight concurrent Gemini API calls (process-wide semaphore in `src/llm/backends.rs::gemini_semaphore`).  Lower this to 2 if batch test runs still see empty responses; raising above 5 risks 429 / empty-content storms on Gemini's free tier.  Free tier may also return HTTP 200 + empty `choices[0].message.content` instead of 429 when over budget — Sirin auto-retries those 2× with 2 s / 4 s backoff (Gemini-only; `GEMINI_EMPTY_MAX_RETRIES` const, not env-tunable). |

## Knowledge Base (Sirin × KB integration)

| Variable | Default | Description |
|----------|---------|-------------|
| `KB_ENABLED` | `0` | Master switch for KB integration.  Set `1`/`true`/`yes`/`on` to enable.  When off, all `kb_client` helpers short-circuit (no MCP traffic).  Off by default so dev setups without the agora-trading MCP service reachable don't see error spam. |
| `KB_MCP_URL` | `http://localhost:3001/mcp` | MCP endpoint for the KB.  For the **hosted agora-trading service** (the one Claude Code already uses) set to `https://agoramarketapi.purrtechllc.com/api/mcp`. |
| `KB_MCP_BEARER` | *(none)* | Legacy bearer fallback for KB reads when no OPS authorization is available. Sirin manual and telemetry writes do not use it. |
| `KB_PROJECT` | `sirin` | Default project slug for KB writes from runtime (convergence guard raw notes, etc).  Reads pass project explicitly. |
| `SIRIN_AGORA_OPS_AUTHORIZATION` | *(none)* | Optional explicit OPS authorization value for AgoraMarket read-only operations and KB reads. Sirin strips a leading `Bearer ` before sending this as `X-OPS-Authorization`, so read-only checks do not require external-AI Telegram approval. If unset, Sirin tries `MCP_OPS_KEY`, then `~/.claude.json` `mcpServers.agora-ops.headers.X-OPS-Authorization`. It intentionally does not use `mcpServers.agora-ops.headers.Authorization` as an OPS fallback because that value may be an external-AI bearer. |
| `SIRIN_KB_SSH_HOST` / `AGORA_SSH_HOST` | *(none)* | SSH host used only by Sirin's fixed server-local `kbWrite` path. The Sirin-specific name wins. |
| `SIRIN_KB_SSH_KEY` / `AGORA_SSH_KEY` | *(none)* | Existing absolute SSH private-key path for the fixed KB write path. The key stays on Windows; the AgoraMarketAPI MCP credential stays on the remote host. |

**Quick setup against the hosted KB:**
```bash
# .env
KB_ENABLED=1
KB_MCP_URL=https://agoramarketapi.purrtechllc.com/api/mcp
# Read-only kb_get/kb_search prefer the existing agora-ops X-OPS authorization.
AGORA_SSH_HOST=<existing-ssh-host>
AGORA_SSH_KEY=<existing-absolute-key-path>
```

Local AI sessions must use Sirin's `kb_write` for explicitly requested KB
updates. Sirin validates the live upstream `kbWrite` policy, obtains the
running service credential only inside the remote host, and calls the fixed
server-local MCP endpoint. It never routes local writes through the broad
external-AI connector and therefore does not request per-call Telegram
approval. `KB_WRITE_TELEMETRY` controls only automatic raw/draft notes; it does
not disable manual `kb_write` calls.

**Auto-features when enabled:**
- `TestGoal.docs_refs` entries are auto-resolved at run start: filesystem paths read with `std::fs`; kebab-case keys (no `/` `\` and no extension) fetched via `kbGet`.  Content is spliced into the LLM prompt under "Required reading".
- `TestGoal.kb_refs` entries are always treated as KB topicKeys (no path heuristic) — explicit form for KB-only references.
- Convergence guard / error-ratio guard fires write a `layer=raw` note via `kbWrite` capturing the test_id, action signature, and observation snippet — searchable as `stuck-{test_id_slug}-{action_sig_slug}`.
- `claude_session::run_sync` triage spawns prepend up to 3 `kbSearch` hits from the project inferred via `project_from_cwd` (agora-backend / flutter / sirin).
- The post-commit hook (`scripts/check_kb_freshness.sh`, install via `scripts/install_kb_freshness_hook.sh`) marks KB entries stale when the commit changes their `fileRefs`.  Honours `KB_MCP_BEARER` for hosted endpoints.

## Telegram

| Variable | Default | Description |
|----------|---------|-------------|
| `TG_API_ID` | **(required)** | App API ID from https://my.telegram.org |
| `TG_API_HASH` | **(required)** | App API hash |
| `TG_PHONE` | | Phone number (international format) for automated login |
| `TG_AUTO_REPLY` | `false` | Enable automatic replies |
| `TG_REPLY_PRIVATE` | `true` | Reply to private DMs |
| `TG_REPLY_GROUPS` | `false` | Reply in groups |
| `TG_GROUP_IDS` | | Comma-separated group chat IDs to monitor |
| `TG_STARTUP_MSG` | | Send this message on startup (health check) |
| `TG_DEBUG_UPDATES` | `true` | Verbose update logging |

## Project / Tools

| Variable | Default | Description |
|----------|---------|-------------|
| `SIRIN_PROJECT_ROOT` | *(cwd)* | Root directory for file operations (path traversal guard) |
| `SIRIN_ALLOWED_COMMANDS` | | Additional shell commands (comma-separated) added to coding agent whitelist |
| `SIRIN_MCP_TOOL_PROFILE` | `full` | Default `tools/list` discovery profile: `full`, `core`, `testing`, `ios`, or `ops`. An explicit `tools/list` `params.profile` overrides it. Compact profiles reduce AI context only; they do not restrict `tools/call` or change authorization. |
| `FOLLOWUP_INTERVAL_SECS` | `20` | Follow-up worker polling interval |

## Browser / Test Runner

| Variable | Default | Description |
|----------|---------|-------------|
| `SIRIN_BROWSER_HEADLESS` | `true` | Chrome mode. Set `false` / `0` / `no` to run visible — **required for Flutter CanvasKit / WebGL apps** which won't paint headless. **As of cb49ea5 all 22 Agora YAML tests have removed their per-test `browser_headless` field — set this once in `.env` instead.** Per-test YAML `browser_headless` still overrides at the `TestGoal` level if explicitly set. |
| `SIRIN_BROWSER_OPERATION_TIMEOUT_SECS` | `120` | Hard deadline for one synchronous CDP operation; values are clamped to 10–600 seconds. A timeout returns an explicit error and invalidates only the stale browser-session generation so the next operation can rebuild Chrome. This is separate from headless_chrome's shared idle/event-loop timeout. |
| `BATCH_START_STAGGER_MS` | `5000` | Delay (ms) between consecutive `run_test_batch` test starts.  Each test waits `idx * stagger_ms` before acquiring the concurrency semaphore.  Mitigates Chrome-process state races when multiple batch tests share localStorage / cookies / `?__test_role=` SPA flags.  Default raised 2s → 5s after batch 7-9 showed Flutter SPA auto-login needed ~3-5s to settle before the next test could safely navigate to the same auth-required URL.  In addition, batch start now does a synchronous `about:blank` pre-warm so Chrome is fully cold-launched before idx=0 begins.  Set `0` to disable for tests on independent domains. |
| `SIRIN_RPC_PORT` | `7700` | Port for the WebSocket + MCP HTTP server. Change when port 7700 is stuck in TCP TIME_WAIT from a recently-killed Sirin, or when running multiple instances. |
| `SIRIN_REPO_BACKEND` | `~/IdeaProjects/AgoraMarketAPI` | Repo path for auto-fix spawning (backend bugs) |
| `SIRIN_REPO_FRONTEND` | `~/IdeaProjects/AgoraMarketFlutter` | Repo path for auto-fix (frontend bugs) |
| `SIRIN_REPO_SIRIN` | *(this repo)* | Repo path for auto-fix (Sirin itself) |
| `SIRIN_CLAUDE_BIN` | *(auto-detect)* | Override path to `claude` CLI binary |

## AI Work Monitor

| Variable | Default | Description |
|----------|---------|-------------|
| `SIRIN_AI_MONITOR_TREND_PATH` | `%LOCALAPPDATA%\Sirin\tracking\ai_monitor_token_trend.jsonl` | Internal test/deployment override for the local Token trend snapshot. Production should normally leave it unset. |
| `SIRIN_AI_MONITOR_DISABLE_AWAKE_GUARD` | `0` | Set to `1` only for isolated candidate smoke tests or troubleshooting. Disables Sirin's conditional Windows execution request without disabling observation. |
| `SIRIN_CODEX_SUPERVISOR_STATE_PATH` | `%LOCALAPPDATA%\Sirin\tracking\codex_supervisor.json` | Internal test/deployment override for the Codex supervisor evidence and dedupe ledger. Production should normally leave it unset. |

## Data Paths (Windows)

| Path | Content |
|------|---------|
| `%LOCALAPPDATA%\Sirin\tracking\` | task.jsonl, research.jsonl |
| `%LOCALAPPDATA%\Sirin\memory\` | memories.db (SQLite FTS5) + test_memory.db |
| `%LOCALAPPDATA%\Sirin\code_graph\` | graph.jsonl |
| `%LOCALAPPDATA%\Sirin\context\` | Per-peer conversation logs |
| `data/pending_replies/` | JSONL draft files per agent |
| `data/sessions/` | Telegram session files |
| `data/test_failures/` | Per-run failure screenshots (PNG, gitignored) |
| `data/teams_profile/` | Chrome user data dir for Teams (gitignored) |
| `config/` | agents.yaml, persona.yaml, llm.yaml, skills/, scripts/, tests/ |
| `config/tests/` | AI browser test goal YAMLs |
| `config/mcp_servers.yaml` | External MCP server connections |
