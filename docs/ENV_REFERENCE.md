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
| `KB_MCP_BEARER` | *(none)* | Optional `Bearer <token>` for the KB endpoint.  Hosted KBs gate access.  Read the existing token from `~/.claude.json` → `mcpServers.agora-trading.headers.Authorization` and strip the `Bearer ` prefix. |
| `KB_PROJECT` | `sirin` | Default project slug for KB writes from runtime (convergence guard raw notes, etc).  Reads pass project explicitly. |

**Quick setup against the hosted KB:**
```bash
# .env
KB_ENABLED=1
KB_MCP_URL=https://agoramarketapi.purrtechllc.com/api/mcp
KB_MCP_BEARER=<paste-token-from-~/.claude.json>
```

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
| `FOLLOWUP_INTERVAL_SECS` | `20` | Follow-up worker polling interval |

## Browser / Test Runner

| Variable | Default | Description |
|----------|---------|-------------|
| `SIRIN_BROWSER_HEADLESS` | `true` | Chrome mode. Set `false` / `0` / `no` to run visible — **required for Flutter CanvasKit / WebGL apps** which won't paint headless. **As of cb49ea5 all 22 Agora YAML tests have removed their per-test `browser_headless` field — set this once in `.env` instead.** Per-test YAML `browser_headless` still overrides at the `TestGoal` level if explicitly set. |
| `BATCH_START_STAGGER_MS` | `5000` | Delay (ms) between consecutive `run_test_batch` test starts.  Each test waits `idx * stagger_ms` before acquiring the concurrency semaphore.  Mitigates Chrome-process state races when multiple batch tests share localStorage / cookies / `?__test_role=` SPA flags.  Default raised 2s → 5s after batch 7-9 showed Flutter SPA auto-login needed ~3-5s to settle before the next test could safely navigate to the same auth-required URL.  In addition, batch start now does a synchronous `about:blank` pre-warm so Chrome is fully cold-launched before idx=0 begins.  Set `0` to disable for tests on independent domains. |
| `SIRIN_RPC_PORT` | `7700` | Port for the WebSocket + MCP HTTP server. Change when port 7700 is stuck in TCP TIME_WAIT from a recently-killed Sirin, or when running multiple instances. |
| `SIRIN_REPO_BACKEND` | `~/IdeaProjects/AgoraMarketAPI` | Repo path for auto-fix spawning (backend bugs) |
| `SIRIN_REPO_FRONTEND` | `~/IdeaProjects/AgoraMarketFlutter` | Repo path for auto-fix (frontend bugs) |
| `SIRIN_REPO_SIRIN` | *(this repo)* | Repo path for auto-fix (Sirin itself) |
| `SIRIN_CLAUDE_BIN` | *(auto-detect)* | Override path to `claude` CLI binary |

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
