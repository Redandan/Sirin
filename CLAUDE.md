# Sirin — Claude Code guidelines

## Architecture decisions (DO NOT revisit)

- **GUI**: plain HTML/CSS/JS web app served by `mcp_server` at
  `http://127.0.0.1:7700/ui/`. Single-file Alpine.js. Bundled into
  the binary via `include_bytes!` so single-binary distribution still
  holds. **Migrated from egui in v0.5.0 (2026-05-02)** — egui's
  immediate-mode + child↔parent layout negotiation was hostile to
  AI-driven development. See `web/DESIGN.md` for the full story.
- **Theme**: 極簡硬核風（加密終端/系統監控器風格）— 不要用 Catppuccin
- **Backend access**: ALL UI goes through `AppService` trait. The web
  UI talks to it via `GET /api/snapshot` (read) and `POST /mcp` (write,
  via existing tool-call protocol). A global
  `OnceLock<Arc<dyn AppService>>` in `mcp_server.rs` is set by
  `main.rs` at startup; `register_app_service()` is the entry point.
- **Daemon-style**: Closing the browser tab does NOT kill sirin. RPC
  server, Telegram listener, scheduler, screenshot pump all keep
  running. Re-open `http://127.0.0.1:7700/ui/` any time. Killing the
  daemon needs Ctrl-C in the terminal or `taskkill /F /IM sirin.exe`.
- **No WASM, no Tauri, no Electron**: Just a real browser tab. The
  user already has Chrome (Sirin tests Chrome apps), so no extra
  runtime to ship.
- **Persona**: Use `Persona::cached()` not `Persona::load()` in hot paths
- **Mutex**: Always `unwrap_or_else(|e| e.into_inner())`, never `.unwrap()`

## UI/UX 規範 (嚴格遵守)

**Source of truth**: [`web/DESIGN.md`](web/DESIGN.md) documents
competitor inspiration (Linear / Playwright Test UI / GitHub Actions
/ Vercel / Sentry / Codecov / Datadog), color semantics, font
hierarchy, and "do not adopt" anti-patterns. Read it before designing
new panels.

### 視覺語調
- 風格：極簡、硬核、高性能感（Linear / Vercel dashboard 風）
- 配色固定，意義穩定（accent=running，danger=failed，info=link/scripted）

### 配色方案 (CSS variables in `web/style.css`)
```
--bg:        #1A1A1A
--card:      #222222
--hover:     #2A2A2A
--border:    #333333
--text:      #E0E0E0
--text-dim:  #808080
--accent:    #00FFA3   (running / passed / 主要 CTA)
--danger:    #FF4B4B   (failed / stopped)
--yellow:    #FFD93D   (partial / timeout / warning)
--info:      #4DA6FF   (scripted / link / neutral status)
--value:     #FFFFFF   (numbers, monospace)
```

### 字型階層
```
24px → H1 (DASHBOARD / TESTING / WORKSPACE)
15px → modal section title
13px → body / button text
11.5px MONO 大寫 → section label
10px  → caption / timestamp
```
數字一律 `var(--font-mono)` + `font-variant-numeric: tabular-nums`.

### 組件規範
- **Card**: `background: var(--card); border-radius: 4px; padding: 12px`
- **Status dot**: 7-8px `<span class="dot dot-ok|dot-bad">`
- **Pulse dot**: live indicator with CSS keyframe animation
- **Button hover**: bg → `var(--hover)`, do NOT animate transform/scale
- **Active row**: 2px `var(--accent)` left bar (Linear convention)
- **Section label**: `text-transform: uppercase`, mono, 0.04em letter-spacing

### AI 新頁面流程
1. **Read web/DESIGN.md** — pick competitor reference for the new view
2. **Add HTML to `web/index.html`** — single-file UI, all views in there
3. **Add state to `web/app.js`** — extend `sirin()` factory
4. **Add CSS classes to `web/style.css`** — reuse existing tokens
5. **Verify in browser** — no rebuild needed for UI-only changes;
   F5 refresh is the entire iteration loop. Backend changes still
   need `cargo build --release` + restart.

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

## YAML 測試 — IDE 自動補全 / Schema 驗證 (Issue #255)

每個 `config/tests/*.yaml` 第一行加：

```yaml
# yaml-language-server: $schema=../test-schema.json
```

VS Code 裝 [`redhat.vscode-yaml`](https://marketplace.visualstudio.com/items?itemName=redhat.vscode-yaml) 後就有：
- 全部欄位的 auto-complete（max_iterations / viewport / perception / 等）
- enum 限定（`perception: text|vision|auto`、`viewport.mobile: bool`）
- 拼錯欄位即時紅線

**Schema 怎麼更新**：

```bash
# 改完 src/test_runner/parser.rs 的 #[derive(JsonSchema)] struct 後
cargo run --bin gen-schema
# 把更新後的 config/test-schema.json 一起 commit
```

`cargo test schema_has_no_drift` 會在 CI / 本機自動擋住忘記重生的情況。

`scaffold_test` MCP tool 產生的新 YAML 自動帶 schema header。

## AgoraMarket 網頁架構（必讀，開測試前先確認）

| 端 | 類型 | 正確 viewport | URL 特徵 |
|---|---|---|---|
| **Buyer（會員/購物）** | **H5 手機版** | `390×844 scale=2 mobile=true` | `__test_role=buyer` |
| **Seller（商家後台）** | **H5 手機版** | `390×844 scale=2 mobile=true` | `__test_role=seller` |
| **Delivery（外送員）** | **H5 手機版** | `390×844 scale=2 mobile=true` | `__test_role=delivery` |
| **Admin（管理後台）** | PC 桌面版 | `1280×900 scale=1 mobile=false` | `__test_role=admin` |

⚠️ **新增 buyer/seller/delivery 測試必須加 viewport block：**
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

### Flutter 測試關鍵模式（2026-04-29 更新）

| 操作 | 正確方式 | 錯誤方式 |
|---|---|---|
| off-screen textbox 輸入 | `ax_find` → `ax_focus` → `flutter_type` | `ax_click`（不 scroll）|
| scroll 後找按鈕 | `scroll` → `wait 800` → **再 shadow_dump** | 用舊 shadow_dump 結果 |
| Flutter tab 切換（PageView） | `shadow_click role=tab` → `wait 2000` → `enable_a11y` → `wait 1000` | 切換後立即操作（PageView 動畫未完成）|
| Flutter tab 切換（auto_route） | `shadow_click role=tab` | `click_point`（CDP 座標）|
| 確認輸入值 | `ax_value backend_id=<id>` | `screenshot_analyze`（近似）|
| CJK/中文輸入 | `flutter_type text="你好"` — 自動路由到 JS ClipboardEvent paste | `shadow_type`（InsertText 不保證 Flutter 接收）|
| ExpansionTile 點擊 | `shadow_click role=group name_regex="..."` | `shadow_click role=button`（ExpansionTile AX role=group）|
| ExpansionTile 展開後操作 | 展開後加 `wait 1500` + `enable_a11y` + `wait 800` | 立即截圖（動畫 ~300ms 未完成）|

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
web/                         Web UI (v0.5.0+) — single-binary embedded via
                             include_bytes! in mcp_server.rs:
                             • index.html  — Alpine.js x-data root, ~6 views
                             • style.css   — design tokens + components
                             • app.js      — sirin() factory: state + fetch
                             • alpine.min.js — bundled v3 runtime
                             • DESIGN.md   — competitor inspiration map
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
src/mcp_server.rs            MCP HTTP server (:7700/mcp) — 65+ tools exposed
                             (run_test_batch added v0.4.0; v0.4.6 added 22 new
                             tools: coverage, replay_last_failure, shadow_dump_diff,
                             suggest_allowlist, list/add/remove_allow, save_point,
                             create/list/mark_done task, session_cost,
                             create_handoff, get_latest_handoff, kb_stats, kb_diff…)
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
cargo test --bin sirin   # 632+ passed, 17 ignored
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
