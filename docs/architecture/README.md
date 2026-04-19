# Sirin Architecture Docs

This folder contains one deep-dive document per major Sirin subsystem.
Each doc is written from source — not aspirational — and is intended for
engineers (human or AI) joining the project cold who need to understand
how a subsystem works before touching it.

See [`CLAUDE.md`](../../CLAUDE.md) at the repo root for build rules,
UI/UX conventions, and the project layout overview that ties everything
together.

---

## Subsystem Index

| Subsystem | Source path(s) | Doc | Key concepts |
|---|---|---|---|
| **Telegram** | `src/telegram/` | [telegram.md](./telegram.md) | MTProto, filter chain, LLM dispatch, reply queue |
| **Researcher** | `src/researcher/` | [researcher.md](./researcher.md) | fetch pipeline, persistence, candidate scoring |
| **Persona** | `src/persona/` | [persona.md](./persona.md) | `OnceLock<RwLock>`, behavior engine, task tracker |
| **Memory** | `src/memory/` | [memory.md](./memory.md) | FTS5 SQLite, codebase index, per-peer ring-log |
| **Test Runner** | `src/test_runner/` | [test_runner.md](./test_runner.md) | ReAct loop, triage, auto-fix, SQLite store |
| **LLM** | `src/llm/` | [llm.md](./llm.md) | multi-backend, fleet probe, role slots, vision |
| **Browser** | `src/browser.rs`, `src/browser_ax.rs` | [browser.md](./browser.md) | CDP singleton, 45+ actions, a11y tree, named sessions |
| **UI (egui)** | `src/ui_egui/` | [ui_egui.md](./ui_egui.md) | immediate-mode, AppService trait, theme tokens |
| **Multi-agent** | `src/multi_agent/` | [multi_agent.md](./multi_agent.md) | squad workers, task queue, priority lanes, roles |
| **MCP Server** | `src/mcp_server.rs` | [mcp_server.md](./mcp_server.md) | HTTP `:7700/mcp`, 28 tools, browser_exec, squad API |
| **MCP Client** | `src/mcp_client.rs` | [mcp_client.md](./mcp_client.md) | outbound MCP proxy, tool namespacing, `mcp_{server}_{tool}` |
| **Claude Session** | `src/claude_session.rs` | [claude_session.md](./claude_session.md) | CLI subprocess, Fix B streaming, supervision loop, tool whitelist |

---

## Recommended Reading Order

**New contributor — start here:**

1. [`../../CLAUDE.md`](../../CLAUDE.md) — project layout, architecture decisions, build rules
2. [`llm.md`](./llm.md) — all agents depend on this; understand it first
3. [`persona.md`](./persona.md) — shapes every agent decision
4. [`browser.md`](./browser.md) — most non-trivial tests and tools go through here
5. [`test_runner.md`](./test_runner.md) — how AI-driven browser tests work end-to-end

**If you're touching the agent pipeline:**

- [`memory.md`](./memory.md) — context injection
- [`researcher.md`](./researcher.md) — background information gathering
- [`multi_agent.md`](./multi_agent.md) — task queue and squad coordination

**If you're debugging a Telegram interaction:**

- [`telegram.md`](./telegram.md) — filter chain and LLM dispatch

---

## How to Add a New Subsystem Doc

1. **Read the source first.** Open every `.rs` file in the subsystem.
   Note: public structs, key singletons, entry points, and any
   `// TODO` / `// FIXME` comments worth calling out.

2. **Create `docs/architecture/<subsystem>.md`** following this section structure:

   ```
   # <Subsystem> Architecture
   > Source: `src/<path>/`
   > Cross-references: <links to related docs>

   ## 1. Purpose
   ## 2. Module Map         ← table: file | line count | one-line responsibility
   ## 3. <Core mechanism>   ← e.g. "Singleton + lifecycle", "Pipeline stages"
   ## 4. Design Decisions   ← why, not what — non-obvious choices
   ## 5. Known Limits / Future Work
   ```

3. **Length target:** 150–450 lines.  Index files (like this one) are shorter;
   complex subsystems (browser, test_runner) may run longer.

4. **Cross-link expectations:**
   - Link to source files with backtick paths, not URLs.
   - Reference related architecture docs with relative Markdown links.
   - Mention the relevant MCP tool names if the subsystem exposes any.

5. **Update this README:** add a row to the Subsystem Index table and change
   `*(planned)*` to `[subsystem.md](./subsystem.md)`.

---

## Cross-References

| Doc | What it covers |
|---|---|
| [`../../CLAUDE.md`](../../CLAUDE.md) | Build rules, UI conventions, project layout, DO-NOT-revisit decisions |
| [`../MCP_API.md`](../MCP_API.md) | Full reference for all 18 MCP tools exposed on `:7700/mcp` |
| [`../test-runner-roadmap.md`](../test-runner-roadmap.md) | Planned test_runner features and K-series milestones |
| [`../squad-roadmap.md`](../squad-roadmap.md) | Multi-agent squad upgrade roadmap (T1-x tasks) |
| [`../ARCHITECTURE.md`](../ARCHITECTURE.md) | High-level system diagram and component overview |
