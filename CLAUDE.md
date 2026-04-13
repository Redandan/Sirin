# Sirin — Claude Code guidelines

## Architecture decisions (DO NOT revisit — these are final)

- **GUI**: egui 0.31 immediate mode — AI reads code to "see" UI, no screenshots needed
- **Theme**: Catppuccin Mocha via `catppuccin-egui`
- **Backend access**: ALL UI goes through `AppService` trait (`src/ui_service.rs`) — zero direct backend imports in `src/ui_egui/`
- **Desktop only**: No WASM, no web mode, no Dioxus, no Tauri
- **Persona**: Use `Persona::cached()` not `Persona::load()` in hot paths
- **Mutex**: Always `unwrap_or_else(|e| e.into_inner())`, never `.unwrap()`

## Efficiency rules (reduce token / tool-call waste)

- **Read large sections in one call.** 300+ line files: read the full section, not 30-line slices.
- **One cargo check per edit.** Never speculatively. Use `/check` slash command.
- **Parallel reads.** When needing multiple files, issue all Read/Grep in one message.
- **Never re-explore.** If code was analyzed this session, don't Explore it again.
- **No architecture detours.** If a change requires switching frameworks or adding WASM, STOP and ask the user first.
- **Batch edits.** Make all related edits before running cargo check, not one-at-a-time.

## Project layout

```
src/ui_egui/          egui UI (7 modules + theme)
  mod.rs              App + toast + top bar + router
  theme.rs            Catppuccin colours + card/badge/section helpers
  sidebar.rs          Agent list (rename, status, badges) + grouped nav
  workspace.rs        4 tabs: overview, thinking, pending, per-agent settings
  settings.rs         System-only: LLM, TG auth, MCP tools, skills
  log_view.rs         3-level filter + version cache
  workflow.rs         6-stage pipeline + AI prompt + advance
  meeting.rs          Multi-agent room with start/end/send

src/ui_service.rs     AppService trait (20+ methods) — UI↔backend boundary
src/ui_service_impl.rs RealService wrapping all backend calls

src/agents/           Planner → Router → Chat / Coding / Research
src/adk/              Agent trait, ToolRegistry (26+ tools), AgentRuntime
src/mcp_client.rs     Connect to external MCP servers, proxy tools
src/mcp_server.rs     Expose Sirin tools via MCP HTTP (:7700/mcp)
src/telegram/         MTProto listener + handler + reply
src/teams/            Chrome CDP + AI draft generation
src/memory.rs         SQLite FTS5 + context JSONL
src/llm.rs            Ollama / LM Studio / Gemini / Claude backends
src/persona.rs        Identity + ROI + behavior engine + cached()
src/pending_reply.rs  Human-in-the-loop with FILE_LOCK
src/error.rs          SirinError enum (thiserror)
```

## Build & verify

```bash
cargo check          # fast type-check (should be 0 errors, 0 warnings)
cargo test           # 177 tests (5 ignored need live LLM)
cargo build --release
```

## Current status

- 0 errors, 0 warnings, 177 tests passed
- AppService V3: 20+ methods (read, write, auth, meeting, workflow, MCP)
- All features wired to UI (Teams, Meeting, Workflow AI)
