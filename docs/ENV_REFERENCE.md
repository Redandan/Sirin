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
| `GEMINI_CONCURRENCY` | `3` | Max in-flight concurrent Gemini API calls (process-wide semaphore in `src/llm/backends.rs::gemini_semaphore`).  Lower this to 2 if batch test runs still see empty responses; raising above 5 risks 429 / empty-content storms on Gemini's free tier. |

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
