# Sirin — Claude Code guidelines

## Architecture decisions (DO NOT revisit)

- **GUI**: egui 0.31 immediate mode — AI reads code to "see" UI
- **Theme**: 極簡硬核風（加密終端/系統監控器風格）— 不要用 Catppuccin
- **Backend access**: ALL UI goes through `AppService` trait — zero backend imports in `src/ui_egui/`
- **Desktop only**: No WASM, no web mode, no Dioxus, no Tauri
- **Persona**: Use `Persona::cached()` not `Persona::load()` in hot paths
- **Mutex**: Always `unwrap_or_else(|e| e.into_inner())`, never `.unwrap()`

## UI/UX 規範 (嚴格遵守)

### 視覺語調
- 風格：極簡、硬核、高性能感（加密貨幣終端 / 系統內核監控器）
- **不要使用 egui 預設配色**

### 配色方案
```
背景:       #1A1A1A (深灰)
卡片/面板:  #222222
Hover:      #2A2A2A
邊框:       #333333
主文字:     #E0E0E0
副文字:     #808080
強調(運行): #00FFA3 (Spring Green)
警告(停用): #FF4B4B (Red)
資訊/連結:  #4DA6FF (Blue)
數值:       #FFFFFF (White, monospace)
```

### 佈局規則
```
Top Panel:      高度 32pt, 項目名稱 + 版本 + 全局狀態指標
Side Panel:     寬度 200pt, 導航按鈕
Central Panel:  ScrollArea 包裹, 內邊距 12pt
組件間距:       8pt
內部邊距:       12pt
```

### 組件規範
- **Card**: `egui::Frame` + `Rounding(4.0)` + `Stroke(1.0, #333333)`
- **Status**: `[圓點] + 文字` 格式，#00FFA3=運行 #FF4B4B=停用
- **Button hover**: 邊框變亮的視覺反饋
- **數值**: 等寬字體 `egui::TextStyle::Monospace`
- **對齊**: 所有數據左對齊

### AI 新頁面流程
1. State Mapping — 列出 UiState 字段
2. Mockup Outline — Markdown 樹狀結構描述層級
3. Code Gen — egui 代碼 + 關鍵佈局註釋

## Efficiency rules

- **Batch edits.** Make all related edits before cargo check.
- **One cargo check per iteration.** Not speculatively.
- **Parallel reads.** Multiple files in one message.
- **Never re-explore.** Don't analyze code twice in same session.
- **No architecture detours.** STOP and ask before switching frameworks.

## Project layout

```
src/ui_egui/                 egui UI (8 modules + theme — incl. browser panel)
src/ui_service.rs            AppService trait — UI↔backend boundary (6 sub-traits
                             incl. BrowserService)
src/ui_service_impl/         RealService (6 domain submodules: agents, pending,
                             workflow, integrations, system, browser)
src/agents/                  Planner → Router → Chat / Coding / Research
  coding_agent/              mod (orchestration) + react (ReAct loop) +
                             verify (auto-fix+rollback) + finalize (epilogue) +
                             state (RunState) + prompt + verdict + rollback +
                             helpers
  chat_agent/                mod + dispatch + intent + context + format
  planner_agent.rs +
  planner_intent.rs          intent classification split out
src/adk/                     Agent trait, AgentRuntime
  tool/                      mod (ToolRegistry) + builtins (35+ tool impls
                             incl. web_navigate / run_test / claude_session /
                             expand_observation) + fs_helpers
src/browser.rs               Persistent Chrome session (singleton + auto-recover)
                             — 35+ CDP actions, vision-ready
src/test_runner/             AI-driven browser testing
  mod.rs                     Public API (run_test / spawn_run_async /
                             spawn_adhoc_run / run_all)
  parser.rs                  YAML TestGoal (locale, retry, url_query, criteria)
  executor.rs                ReAct loop driving web_navigate
  triage.rs                  Failure classification + auto-fix + verification loop
  store.rs                   SQLite test_runs + auto_fix_history (with verification)
  runs.rs                    In-memory async run registry
  i18n.rs                    Locale (zh-TW / en / zh-CN) prompt strings
src/claude_session.rs        Spawn `claude` CLI for cross-repo bug fixing
src/config_check.rs          Diagnostics + AI fix proposal (dual-stage confirm)
src/mcp_client.rs            External MCP server proxy
src/mcp_server.rs            MCP HTTP server (:7700/mcp) — 14 tools exposed
src/telegram/                MTProto listener — mod + filter + handler +
                             reply + commands + config + language + llm
src/teams/                   Chrome CDP (Teams MutationObserver)
src/memory/                  mod (FTS5 SQL store) + codebase (project index) +
                             context (per-peer ring-log)
src/llm/                     mod (core types + public call API + vision) +
                             backends (Ollama/OpenAI HTTP, multimodal) +
                             probe (fleet discovery)
src/persona/                 mod (config types) + behavior (decision engine) +
                             task_tracker (event log)
src/researcher/              mod + fetch + persistence + pipeline
src/followup/                mod (worker loop) + candidates (self-assign)
```

## Build

```bash
cargo check          # 0 errors
cargo test --bin sirin   # 174 passed, 5 ignored (need LM Studio)
cargo clippy             # 11 warnings (false positives + architectural)
cargo build --release
```
