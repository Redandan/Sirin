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

## Cargo / build rules (CRITICAL — prevent 2h deadlocks)

- **NEVER** run `cargo` with `run_in_background=true` — Cargo uses an exclusive
  file lock on `target/`; background tasks queue forever and cause deadlocks.
- **ALWAYS** set `timeout: 600000` (10 min) on any `cargo test` Bash call.
- **One cargo at a time.** Never launch a second `cargo` command while one is running.
- **cargo check once.** If you need both error count and warning count, capture
  output to a variable and grep twice — never run cargo check twice.
- **Deadlock signal:** if a cargo output file stays at 0 bytes for >30s, the
  process is waiting for the lock — kill it and retry.

## AgoraMarket 網頁架構（必讀，開測試前先確認）

| 端 | 類型 | 正確 viewport | URL 特徵 |
|---|---|---|---|
| **Buyer（會員/購物）** | **H5 手機版** | `390×844 scale=2 mobile=true` | `__test_role=buyer` |
| **Seller（商家後台）** | PC 桌面版 | `1280×900 scale=1 mobile=false` | `__test_role=seller` |

⚠️ **新增 buyer 測試必須加 viewport block：**
```yaml
viewport:
  width: 390
  height: 844
  scale: 2.0
  mobile: true
```

⚠️ **viewport 設錯的症狀**：截圖 > 800KB（應為 ~500KB），看到兩欄寬桌面版面而非手機單欄。
用 `browser_exec action=set_viewport + screenshot` 先驗證再寫 YAML。

KB: `trap-agoramarket-buyer-h5-viewport`

## Benchmark / LLM 比較 SOP（跑前必做）

```bash
bash scripts/preflight.sh   # 6-section 驗證，FAIL 則修完再跑
```

preflight 檢查項目：
1. **Core LLM Keys** — primary + fallback LLM key 長度驗證
2. **Sirin gateway** — http://127.0.0.1:7700/gateway 必須 200
3. **Vision smoke** — vision model 接受 image input
4. **YAML health** — repo == LOCALAPPDATA + lenient acceptance warning
5. **Chrome stability** — sirin.err.log crash 統計
6. **Action registry** — builtins.rs vs browser_exec.rs 一致性

### LLM fallback chain（必須設定）

```env
LLM_FALLBACK_BASE_URL=https://api.deepseek.com/v1
LLM_FALLBACK_API_KEY=sk-...       # platform.deepseek.com，新帳號 $5 免費
LLM_FALLBACK_MODEL=deepseek-chat  # = DeepSeek V4（自動 alias）
```

Primary 429 → 立即切 fallback（0s 等待）。不設定 → Gemini 429 時測試卡 35s+。

### Flutter 測試關鍵模式（2026-04-28 確認）

| 操作 | 正確方式 | 錯誤方式 |
|---|---|---|
| off-screen textbox 輸入 | `ax_find` → `ax_focus` → `flutter_type` | `ax_click`（不 scroll）|
| scroll 後找按鈕 | `scroll` → `wait 800` → **再 shadow_dump** | 用舊 shadow_dump 結果 |
| Flutter tab 切換 | `shadow_click role=tab` | `click_point`（CDP 座標）|
| 確認輸入值 | `ax_value backend_id=<id>` | `screenshot_analyze`（近似）|

### headless_chrome 已知修復

- **TargetInfoChanged cascade crash**（PR #118）：`transport/mod.rs break→continue`，git fork: `Redandan/rust-headless-chrome@sirin-transport-fix`
- **idle_browser_timeout**（PR #124）：300s→1800s，避免 browser event loop 超時
- **YAML config 自動同步**（PR #124）：啟動時自動 diff+copy `./config/` → LOCALAPPDATA

## Multi-agent spawn rules

KB: [`sirin/trap-parallel-agent-worktree-contention`](https://github.com/Redandan/Sirin/issues/76)

- ✅ 同一 session 內 spawn 多個 Agent 並行 → OK，但**只能跨 repo**
- ❌ 同 repo 多 agent 並行 → working tree 會互踩（`isolation: "worktree"` 不是真隔離，會 checkout 切走別的 agent 的未 commit 改動），必須**嚴格序列**
- ✅ `general-purpose` 可遞迴 spawn 子 agent 做 nested 平行
- ⚠️ Edit/Write/Glob 在 worktree 內路徑解析會跑到主 repo — 用絕對路徑

## Issue creation rules

- ❌ 不要用 `gh issue create &`（並行）— issue ID 順序不可控
- ✅ 永遠序列 `gh issue create` 後立刻 `gh issue view N --json title` 驗證對應

## Project layout

```
src/ui_egui/                 egui UI (9 modules + theme — incl. browser panel,
                             test_dashboard added 3908b2d, team_panel)
src/ui_service.rs            AppService trait — UI↔backend boundary (8 sub-traits:
                             AgentService, PendingReplyService, WorkflowService,
                             IntegrationService, SystemService, MultiAgentService,
                             BrowserService, TestRunnerService [3908b2d])
src/ui_service_impl/         RealService (7 domain submodules: agents, pending,
                             workflow, integrations, system, browser, team —
                             TestRunnerService is implemented inline in mod.rs)
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
src/bin/sirin_call.rs        Thin CLI wrapper for Sirin MCP API — avoids
                             bash shell-escaping pain with CJK/Unicode payloads;
                             key=value syntax or stdin JSON; `--list` to
                             enumerate tools. Build: `cargo build --release`.
src/browser.rs               Persistent Chrome session (singleton + auto-recover)
                             — 45+ CDP actions, vision-ready, network capture
                             with req+res body, clear_browser_state,
                             wait_for_new_tab, wait_for_request, hash-route
                             fast path, mode-switch settle delay + nav retry,
                             condition waits (wait_for_url / wait_for_network_idle),
                             named session mgmt (session_switch / list_sessions /
                             close_session)
src/browser_ax.rs            CDP Accessibility tree (literal text — for K14
                             exact assertions; RawGetFullAxTree workaround
                             for headless_chrome strict-enum bug;
                             poll_tree_recovery + auto-retriggers Flutter
                             semantics on collapse (Tab×2 removed — Issue #20);
                             wait_for_ax_ready (min-node count poll);
                             find_scrolling_by_role_and_name (scroll-to-find)
src/perception/              Pre-LLM "how we see the page" layer (added v0.4.3)
  mod.rs                     PerceptionMode (Text/Vision/Auto) + perceive()
  canvas_detect.rs           1-JS-eval probe: URL + title + canvas flag
                             (window.flutter || flt-glass-pane || big <canvas>)
  capture.rs                 screenshot_b64() — base64 PNG for vision LLM
  ocr.rs                     Local Windows OCR via PowerShell (browser_exec
                             action=ocr_find_text); cheap alternative to
                             vision LLM when tokens are tight
src/integrations/            Third-party integrations, not core testing
  open_claude/               Chrome extension + native-messaging bridge — for
                             future Assistant mode (driving the user's own
                             Chrome window for Google Maps scraping / FB farm
                             tasks).  NOT used by the test runner.
src/assistant/               Scaffold for Assistant mode (empty — populate
                             with task modules as they are added)
src/test_runner/             AI-driven browser testing
  mod.rs                     Public API (run_test / spawn_run_async /
                             spawn_adhoc_run / run_all)
  parser.rs                  YAML TestGoal (locale, retry, url_query,
                             browser_headless, criteria, perception)
  executor.rs                ReAct loop driving web_navigate; LLM prompt
                             advertises web_navigate/ax_*/robustness actions.
                             Perception-aware: text mode = legacy AX-tree
                             observations; vision mode attaches screenshot
                             to every LLM turn via crate::llm::call_vision.
  triage.rs                  Failure classification + auto-fix + verification loop
  store.rs                   SQLite test_runs + auto_fix_history (with verification)
  runs.rs                    In-memory async run registry
  i18n.rs                    Locale (zh-TW / en / zh-CN) prompt strings
src/platform.rs              Cross-platform data/config dir resolution
                             — app_data_dir() → %LOCALAPPDATA%\Sirin (Win)
                               ~/Library/Application Support/Sirin (mac)
                               ~/.local/share/sirin (Linux)
                             — config_dir() → app_data_dir()/config (prod)
                               OR ./config in #[cfg(test)] builds
                             — config_path("foo.yaml") → shorthand
                             RULE: NEVER use "config/foo.yaml" literals.
                             ALWAYS call platform::config_path() / app_data_dir()
src/updater.rs               Auto-update via GitHub Releases (self_update crate)
                             — spawn_check() called once on startup
                             — get_status() → UpdateStatus for UI banner
                             — apply_update(ver) downloads zip, replaces binary
                             — GitHub asset: sirin-windows-x86_64.zip / sirin.exe
                             — Tag format: v0.2.0 triggers release CI
src/claude_session.rs        Spawn `claude` CLI for cross-repo bug fixing
src/config_check.rs          Diagnostics + AI fix proposal (dual-stage confirm)
src/mcp_client.rs            External MCP server proxy
src/mcp_server.rs            MCP HTTP server (:7700/mcp) — 18 tools exposed
                             (run_test_batch added v0.4.0 — parallel YAML
                             test fan-out via tokio Semaphore + per-test
                             session_id; max 8 concurrent tabs)
src/multi_agent/             PM/Engineer/Tester squad — persistent sessions via
                             `claude --continue`; SQLite task queue (JSONL);
                             multi-worker pool (spawn_n); GitHub issue loop-closure;
                             dry-run mode; per-project session isolation
  mod.rs                     AgentTeam: assign_task() + test_cycle()
  worker.rs                  Background worker loop; reset_stale_running
  queue.rs                   Atomic take_next_queued + JSONL persistence
  roles.rs                   PM / Engineer / Tester system prompts
  knowledge.rs               Squad Knowledge Base (T2-1): parse_lessons,
                             store_lessons, relevant_lessons — SQLite at
                             <app_data_dir>/memory/squad_knowledge.db;
                             dedup by 80-char key; keyword-overlap scoring
  github_adapter.rs          Post review comments to GitHub issues via gh CLI
  worktree.rs                Git worktree isolation scaffold (T2-4, not yet wired)
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
cargo test --bin sirin   # 398+ passed, 17 ignored
cargo clippy             # warnings (false positives + architectural)
cargo build --release
./target/release/sirin.exe --headless        # no-GUI mode (server / CI / SSH)
SIRIN_HEADLESS=1 ./target/release/sirin.exe  # equivalent env-var form

# Windows installer (requires Inno Setup 6 installed)
& 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe' /DMyAppVersion=X.Y.Z sirin.iss
# → Output\SirinSetup-X.Y.Z.exe

# Release CI: push a semver tag → GitHub Actions builds + publishes
git tag vX.Y.Z && git push origin vX.Y.Z
```

Headless mode (added v0.4.0) skips `eframe::run_native()` only — RPC/MCP
server, browser singleton, telegram listeners, and test_runner all start
normally. Process parks the main thread until SIGINT/SIGTERM.

## Env vars that affect the browser / LLM

| Var | Values | Purpose |
|---|---|---|
| `SIRIN_BROWSER_HEADLESS` | `true`/`false`/`0`/`1` | Process-wide Chrome headless default.  As of cb49ea5 all 22 Agora YAML tests removed their per-test `browser_headless` field — set this once in `.env` instead.  Per-test YAML field still overrides if explicitly set. |
| `SIRIN_PERSISTENT_PROFILE` | unset / `1` / absolute path | `1` → Chrome `--user-data-dir=<app_data_dir>/chrome-profile`.  A path → use that dir.  Unset = fresh profile per launch (default).  When on, cookies / localStorage / IndexedDB survive a `with_tab` recovery — essential for Flutter hash-route tests where a CDP transport race can force a Chrome relaunch mid-test. |
| `GEMINI_CONCURRENCY` | positive integer (default `3`) | Caps the number of in-flight Gemini API calls process-wide via `tokio::sync::Semaphore` in `src/llm/backends.rs::gemini_semaphore`.  Added bd9cafb to fix batch test flakiness — Gemini's free tier silently returns 200 + empty content (not 429) when several requests arrive in the same second.  Lower to 2 if batch runs still see empty responses; raising above 5 risks 429 storms. |

## Where user data lives (installed vs dev)

| Mode | Binary | Config | Data | .env |
|------|--------|--------|------|------|
| Installed (Windows) | `C:\Program Files\Sirin\` | `%LOCALAPPDATA%\Sirin\config\` | `%LOCALAPPDATA%\Sirin\` | `%LOCALAPPDATA%\Sirin\.env` |
| Dev (`cargo build`) | `target/release/` | `%LOCALAPPDATA%\Sirin\config\` | `%LOCALAPPDATA%\Sirin\` | `%LOCALAPPDATA%\Sirin\.env` or CWD fallback |
| Test (`cargo test`) | — | `./config/` (repo) | — | — |

Always use `platform::config_path()` / `platform::app_data_dir()` — never hardcode.
