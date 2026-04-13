# Sirin — Claude Code guidelines

## Efficiency rules (reduce token / tool-call waste)

- **Read large sections in one call.** When debugging structure in a file over 300 lines, read the full relevant section in a single `Read` call rather than multiple 30-90-line slices.
- **One cargo check per iteration.** Only run `cargo check` after an edit, not speculatively.
- **Parallel reads.** When needing context from multiple files at once, issue all `Read`/`Grep` calls in the same message.

## Project layout shortcuts

- UI: `src/ui_dx/` — Dioxus 0.7 cross-platform (mod.rs, sidebar, workspace, settings, log, workflow, meeting).
- Agent pipeline: `src/agents/` — planner → router → chat / coding / research.
- MCP Client: `src/mcp_client.rs` — connects to external MCP servers, proxies tools.
- MCP Server: `src/mcp_server.rs` — exposes Sirin tools to external AI (Claude Desktop).
- Persona config: `config/persona.yaml` — use `Persona::cached()` not `Persona::load()` in hot paths.
- Error type: `src/error.rs` — `SirinError` enum (thiserror).
- Telegram: `src/telegram/mod.rs` + `src/telegram/handler.rs`.
- Teams: `src/teams/mod.rs` — AI-powered draft via ChatAgent.

## Build

```bash
cargo check          # fast type-check
cargo build --release
cargo test           # 177 tests (5 ignored need live LLM)
```
