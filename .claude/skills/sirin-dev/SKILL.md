---
name: sirin-dev
description: Use this skill when developing on the Sirin project itself (not when using Sirin to test other apps) — adding a new browser action, MCP endpoint, agent skill, test_runner feature, or fixing a bug in the Rust code.  Trigger phrases include "add a Sirin action", "fix Sirin's X", "extend Sirin", "modify Sirin", "Sirin internals", "how does Sirin X work", or any task that involves editing files under `~/IdeaProjects/Sirin/src/`.  This skill is for AI sessions picking up Sirin development cold — covers architecture, common workflows, conventions, and the gotchas that have already cost us time.
version: 1.5.0
---

# Sirin Development Skill

Onboarding for AI sessions developing on Sirin itself.  If you're using
Sirin's MCP API to test other apps, see `sirin-test` instead.

## When This Skill Applies

- Editing Rust source under `~/IdeaProjects/Sirin/src/`
- Adding a browser action, MCP endpoint, test_runner field, agent skill
- Investigating Sirin internals to answer "how does X work?"
- Fixing a bug reported against Sirin (vs in a target app)

## ⛔ 執行任何 config/tests/*.yaml 前的強制 checklist

**跳過這步 = 預期浪費一次空跑（帳號/viewport/flow 錯誤）**

1. 讀 `list_tests` 輸出，看 `docs_refs` 欄位有沒有列文件
2. 把每個 `docs_refs` 裡的文件讀完（帳號密碼、viewport 要求、登入 flow、AC）
3. 確認以下三項再跑：
   - 測試帳號 username / password ✓
   - 是否需要特定 viewport（Flutter app 通常需要 1280×1600）✓
   - 登入 flow（test button? username/password form? ax_find 哪個 button?）✓
4. **確認後才呼叫 run_test_async / run_adhoc_test**

`run_test_async` 回應會包含 `docs_refs` + `warning` 欄位 — 若看到這個欄位卻沒讀，
下一個 session 接手時必須重跑，成本加倍。

## Read these FIRST (in order)

1. **`CLAUDE.md`** at repo root — architecture decisions, project layout,
   efficiency rules.  Contains DO-NOT-revisit decisions (egui, no WASM,
   theme colors).  If you contradict it without explicit user request,
   you are wrong.
2. **`docs/ARCHITECTURE.md`** — module relationships
3. **`docs/test-runner-roadmap.md`** — what's done, what's planned, what
   has been explicitly rejected (e.g. Bayesian flakiness, Backend trait)
4. **Latest broadcast** at `~/.claude/broadcasts/2026-04-*-sirin-*.md` —
   most recent state, what last session shipped

Failing to read these = duplicating work or contradicting decisions.

## Architecture map (key files)

```
src/
├── browser.rs              Persistent Chrome singleton (CDP wrapper)
│                           — 45+ actions: navigate/click/type/wait/scroll/...
│                           — auto-reconnect, hash-route fast path,
│                             headless mode switching, settle delay,
│                             nav retry, network capture (req+res body),
│                             clear_browser_state, wait_for_new_tab,
│                             wait_for_request, multi-tab management,
│                             condition waits (wait_for_url,
│                             wait_for_network_idle), named session mgmt
│                             (session_switch / list_sessions / close_session)
├── browser_ax.rs           CDP Accessibility tree (literal text — for
│                           K14-style exact assertions; uses raw JSON
│                           Method to bypass headless_chrome strict
│                           enum bug; poll_tree_recovery (3×400ms wait
│                           before bootstrap — allows Flutter self-recover);
│                           wait_for_ax_ready (min-node count poll);
│                           find_scrolling_by_role_and_name (scroll-to-find);
│                           Tab×2 permanently removed (Issue #20 URL reset)
├── platform.rs             Cross-platform path helpers (v0.2.0+)
│                           — app_data_dir() → %LOCALAPPDATA%\Sirin (Win)
│                           — config_dir()   → app_data_dir()/config (prod)
│                                              ./config in #[cfg(test)]
│                           — config_path("x.yaml") → config_dir().join("x.yaml")
│                           ⚠ NEVER use "config/foo.yaml" literals
│                           ⚠ NEVER use "data/..." literals
│                           Always go through platform:: helpers.
├── updater.rs              Auto-update via GitHub Releases API (pure reqwest,
│                           no self_update crate — removed v0.4.1)
│                           — spawn_check() → background thread on startup
│                           — get_status() → UpdateStatus enum (UI polls this)
│                           — apply_update(ver) → Windows: downloads SirinSetup-{ver}.exe
│                             from GH Releases, spawns with /SILENT /SUPPRESSMSGBOXES
│                             (UAC auto-triggers via Inno Setup manifest), self-exits 2s
│                           — Non-Windows: opens browser to releases page (no self-replace)
│                           — Tag vX.Y.Z push triggers release CI
├── bin/sirin_call.rs       Thin CLI wrapper for the MCP API — avoids bash
│                           shell-escaping pain with CJK/Unicode payloads;
│                           `key=value` syntax (auto-typed JSON) or stdin JSON;
│                           `--list` enumerates tools; reqwest blocking HTTP.
│                           Build: `cargo build --release`
├── claude_session.rs       Spawn `claude` CLI cross-repo bug fixing
├── config_check.rs         Diagnostics + AI fix proposal (dual-confirm)
├── perception/             Pre-LLM observation layer (added v0.4.3)
│   ├── mod.rs              PerceptionMode (Text/Vision/Auto), PagePerception,
│   │                       perceive(ctx, mode).  Text mode short-circuits —
│   │                       zero overhead for legacy tests.
│   ├── canvas_detect.rs    One JS eval: URL+title+canvas flag
│   │                       (window.flutter || flt-glass-pane || >=50% canvas)
│   ├── capture.rs          screenshot_b64() — base64 PNG for vision LLM
│   └── ocr.rs              Windows-local OCR (browser_exec ocr_find_text)
│                           as cheap no-token locator alternative
├── integrations/           Third-party integrations (NOT core test runner)
│   └── open_claude/        Chrome extension + native messaging bridge.
│                           Reserved for Assistant mode; test_runner must
│                           not depend on this.
├── assistant/              Assistant mode scaffold (empty stub) — populate
│                           when adding Google Maps / FB farm style tasks
├── test_runner/            AI test runner (browser, not unit tests)
│   ├── parser.rs           YAML TestGoal (locale, retry, url_query,
│   │                       browser_headless, llm_backend, success_criteria,
│   │                       tags, **perception**). `llm_backend: claude_cli`
│   │                       switches a single test to the claude subprocess
│   │                       (00c0bc2) — but see "claude_cli ReAct hang" gotcha;
│   │                       default is Gemini.  `perception: vision|auto` opts
│   │                       into screenshot-as-primary-observation mode.
│   ├── executor.rs         ReAct loop driving web_navigate;
│   │                       ALSO contains the LLM prompt — when adding
│   │                       a new web_navigate action, advertise it
│   │                       in the prompt's "Available actions" list
│   │                       (both `build_prompt` and `build_prompt_vision`).
│   │                       `resolve_llm_backend()` + `call_test_llm()`
│   │                       dispatch per-test backend; `call_claude_cli()`
│   │                       wraps `claude_session::run_sync` in
│   │                       `tokio::task::spawn_blocking`.
│   │                       **Perception dispatch**: if perception.resolved_mode
│   │                       == Vision and screenshot present → call_vision;
│   │                       otherwise falls through to legacy text path.
│   ├── triage.rs           Failure → ui_bug/api_bug/flaky/env/obsolete
│   │                       → spawn claude_session + verification re-run
│   ├── store.rs            SQLite test_runs + auto_fix_history
│   ├── runs.rs             In-memory async run registry
│   └── i18n.rs             Locale strings for 3 prompts
├── adk/tool/builtins.rs    ToolRegistry — all agent-callable tools
│                           live here.  When adding a browser action,
│                           you MUST also add to this file's
│                           web_navigate match arm OR register a new
│                           top-level tool (e.g. expand_observation).
├── mcp_server.rs           HTTP MCP server on :7700/mcp (18 tools).  When
│                           adding a browser action that should be externally
│                           callable, ALSO add to this file's
│                           call_browser_exec match arm AND the
│                           tools/list schema.
│                           Direct browser/observability tools that bypass the
│                           ReAct loop entirely (use these to verify state when
│                           an AI test fails ambiguously):
│                             • `page_state` — JSON with title/URL/text excerpt
│                             • `get_screenshot` — PNG base64 from last failure
│                             • `get_full_observation` — combined snapshot
│                             • `browser_exec` — fire any single browser action
├── llm/                    Multi-backend LLM (Ollama/LMStudio/Gemini/
│                           Claude) + vision multimodal
├── ui_egui/                egui UI — sidebar, settings, browser panel,
│                           workflow, meeting, test_dashboard (3908b2d),
│                           team_panel; reads ONLY through AppService
├── ui_service.rs           AppService trait — UI ↔ backend boundary
│                           (8 sub-traits: AgentService, PendingReplyService,
│                           WorkflowService, IntegrationService, SystemService,
│                           MultiAgentService, BrowserService,
│                           TestRunnerService [3908b2d]).
│                           Don't import backend modules directly from ui_egui.
├── ui_service_impl/        RealService impl of AppService (7 submodules,
│                           TestRunnerService implemented inline in mod.rs)
└── persona/                Behavior engine + ROI thresholds + cached
                            persona (use Persona::cached() in hot paths,
                            never Persona::load())

config/
├── tests/*.yaml            Browser test goals (NOT unit tests)
├── skills/*.yaml           YAML skills (planner-recommended)
└── (others)                agents.yaml, persona.yaml, llm.yaml, mcp_servers.yaml

.claude/skills/
├── sirin-launch/           For external Claude sessions starting Sirin
├── sirin-test/             For external Claude sessions running tests
└── sirin-dev/              ← THIS skill
```

## Build / test commands

```bash
cargo check          # 0 errors required before commit
# Tests — MUST use file-redirect form; pipe | tail kills cargo in background mode
cargo test --bin sirin > /tmp/sirin_test.txt 2>&1 ; tail -8 /tmp/sirin_test.txt
# (384 passed, 18 ignored as of v0.4.2)
cargo build --release     # ~2-4 min incremental (see benchmarks below)
./target/release/sirin.exe                       # launch GUI on port 7700
SIRIN_RPC_PORT=7701 ./target/release/sirin.exe   # alt port if 7700 stuck
SIRIN_BROWSER_HEADLESS=false ./target/release/... # for Flutter / WebGL
./target/release/sirin.exe --headless            # no GUI (server / SSH / CI)
SIRIN_HEADLESS=1 ./target/release/sirin.exe      # equivalent env-var form
```

**Headless mode** (added v0.4.0): Skip `eframe::run_native()` entirely;
keep RPC/MCP server, browser singleton, telegram listeners, and test_runner
running. Triggered by either the `--headless` CLI flag or `SIRIN_HEADLESS=1`
env var. Process parks the main thread until SIGINT/SIGTERM. Useful for:
servers without a display, Docker containers, CI pipelines invoking via MCP
only, and benchmarking the MCP API without UI overhead.

Avoid `cargo run` (debug build, slow startup, LLM calls may time out).

### Preferred: `./scripts/dev-relaunch.sh`

For any **smoke test or live-debug session**, use the helper script
instead of the raw commands:

```bash
./scripts/dev-relaunch.sh                        # default 7700, headless
SIRIN_RPC_PORT=7702 ./scripts/dev-relaunch.sh    # alt port
SIRIN_BROWSER_HEADLESS=false ./scripts/dev-relaunch.sh  # for Flutter
./scripts/dev-relaunch.sh --build-only           # just build, no launch
```

It chains: **kill old sirin → cargo build → [2b] auto-rsync
`config/tests/*.yaml` → `%LOCALAPPDATA%\Sirin\config\tests\` (added 9b880ac
so installed-mode reads pick up YAML edits without manual copy) → print
binary mtime + latest commit → fall-through to +1 port if zombie → exec**.

Why it matters: bypassing this chain causes the "stale .exe" bug —
running yesterday's binary against today's source.  This bit us when
testing the eab8537 robustness actions (binary 10h older than commit
returned `Unknown action` for everything).  Always use the script when
the actions you're testing came from a recent commit.

### Build time benchmarks (v0.4.1→v0.4.2 optimization history)

Actual measurements on this machine (Ryzen 7700-class):

| Scenario | Time | Notes |
|----------|------|-------|
| Baseline: fat LTO, 1 crate changed | **8m 31s** | LTO merges all 570 crates in 1 thread |
| Round 1: thin LTO (cold rebuild forced) | **5m 44s** | Profile change → full cold |
| Round 1: remove self_update + parking_lot | **4m 40s** | Cargo.lock changed → partial cold |
| Round 1: tighten tokio features | **3m 08s** | Only 14 crates rebuilt |
| Round 2: remove scraper (html5ever C gone) | **3m 47s** | First cold after dep removal |
| Round 2: futures→futures-util, version pin cleanup | **~2m** | Incremental |
| Incremental single `.rs` change (steady state) | **~1m 50s** | Normal dev cycle |
| `--profile dev-fast` cold | **~2-3 min** | Local iteration profile |

**Dep optimizations applied (v0.4.1 → v0.4.2):**

1. `lto = "thin"` (was fat) + `codegen-units = 4` — 3-4x faster link
2. Removed `self_update 0.42` (pulled in reqwest 0.12 duplicate)
3. Removed `parking_lot 0.12` → `std::sync` everywhere
4. Tightened `tokio` from `features = ["full"]` to explicit list
5. Removed `scraper 0.19` (html5ever C parser + cssparser + selectors eliminated)
   - `researcher/fetch.rs`: regex-based tag stripping
   - `skills.rs DDG HTML`: string-split fallback parser
6. `futures = "0.3"` → `futures-util = "0.3"` (deduplicate with grammers dep)
7. Version pins loosened: serde/serde_json/reqwest/regex/chrono/dotenvy

**`dev-fast` profile** — for local dev loops:

```bash
cargo build --profile dev-fast   # ~2-3 min cold, ~30-60s incremental
./target/dev-fast/sirin.exe
```

Settings: `opt-level=1, lto=false, codegen-units=16`.  Fast enough for real LLM calls
(debug build times out LLM calls on slow machines).  Not for production distribution.

**reqwest 0.13 pitfall**: Feature `rustls-tls-ring` does **not exist** in reqwest 0.13.
Use `"rustls"` (defaults to aws-lc-rs).  `aws-lc-rs` has C++ build deps (bundled) —
unavoidable until reqwest exposes a pure-Rust TLS option.

**parking_lot → std::sync migration rule** (CLAUDE.md enforced):
`parking_lot` is infallible (returns guard directly).  `std::sync` returns `LockResult`.
When migrating, add `unwrap_or_else(|e| e.into_inner())` to **all** `.lock()`, `.read()`,
`.write()` calls.  Missing one = compile error ("expected `MutexGuard`, got `LockResult`").

## Common workflows

### Add a new browser action (e.g. `clear_state`)

Three files MUST be touched:

1. **`src/browser.rs`** — add the actual function:
   ```rust
   pub fn clear_browser_state() -> Result<(), String> {
       use headless_chrome::protocol::cdp::Network;
       with_tab(|tab| {
           tab.call_method(Network::ClearBrowserCookies(None))?;
           Ok(())
       })?;
       let _ = evaluate_js("localStorage.clear(); ...");
       Ok(())
   }
   ```

2. **`src/adk/tool/builtins.rs`** — add `web_navigate` action arm
   so internal Sirin agents can call it:
   ```rust
   "clear_state" => {
       browser::clear_browser_state()?;
       Ok(json!({ "status": "cleared" }))
   }
   ```

3. **`src/mcp_server.rs`** — add `browser_exec` action arm so
   external Claude Code sessions can call it via MCP:
   ```rust
   "clear_state" => {
       browser::clear_browser_state()?;
       Ok(json!({ "status": "cleared" }))
   }
   ```
   AND update the schema description string in the `tools/list`
   handler so MCP clients see it.

4. **`src/test_runner/executor.rs`** — extend the ReAct prompt's
   "Available actions" list so the LLM knows the action exists.
   Easy to forget; LLM won't use what you don't advertise.

5. **`.claude/skills/sirin-test/SKILL.md`** + **`docs/MCP_API.md`** —
   document for external sessions.  Same week, same commit.

### Add a new MCP endpoint (e.g. `list_recent_runs`)

`src/mcp_server.rs` only:
- Add a tool definition in the `tools/list` handler (JSON schema)
- Add an arm to the `tools/call` dispatcher in `call_browser_exec`
  or add a new `call_<name>` async fn called from there
- Update `docs/MCP_API.md`

### Add a new YAML field to TestGoal (e.g. `browser_headless`)

1. `src/test_runner/parser.rs` — add field with `#[serde(default = "...")]`
2. `src/test_runner/executor.rs` — read it before navigate / loop
3. `src/test_runner/mod.rs::spawn_adhoc_run` — accept as param,
   pass through to TestGoal struct construction
4. `src/mcp_server.rs::call_run_adhoc_test` — accept from MCP args
5. `src/mcp_server.rs` schema — add to inputSchema
6. Add a unit test for parsing
7. Document in skills + MCP_API

### Release a new version (Windows installer + auto-update)

1. Bump `version` in `Cargo.toml` (e.g. `0.2.0` → `0.2.1`)
2. `cargo build --release` locally to verify
3. Build installer locally to smoke-test:
   ```powershell
   & 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe' /DMyAppVersion=0.2.1 sirin.iss
   # → Output\SirinSetup-0.2.1.exe
   ```
4. Commit + push main
5. Push a tag → **GitHub Actions auto-builds + publishes Release**:
   ```bash
   git tag v0.2.1 && git push origin v0.2.1
   ```
6. CI (`.github/workflows/release.yml`) creates:
   - `Output/SirinSetup-0.2.1.exe` (Inno Setup installer)
   - `Output/sirin-windows-x86_64.zip` (portable, used by `apply_update()`)
   - GitHub Release: "Sirin v0.2.1" with both assets

Users running old versions will see the update banner on next launch.

**ISS gotchas (paid for):**
- Use `{localappdata}` NOT `{userappdata}` — they're different folders on Windows
  (`%LOCALAPPDATA%` ≠ `%APPDATA%`). Rust uses `%LOCALAPPDATA%`.
- Registry entries spanning lines need `\` line continuation or it parses
  the second line as a new incomplete entry ("Root not specified" error).
- `LicenseFile=LICENSE` → must have `LICENSE` file in repo root.

### Add a built-in skill (Sirin's own agents)

1. `config/skills/<id>.yaml` — declare with `id`, `name`, `description`,
   `trigger_keywords`, `example_prompts`, `category`
2. `src/skills.rs::execute_skill` — short-circuit branch BEFORE the
   `script_file.as_deref().ok_or(...)` line, like `config-check` does
3. Implement the helper at module top (parallel to `execute_browser_test`)
4. Unit tests in `mod tests`

## Code conventions (enforced by reviewer = future you)

- **Mutex always**: `lock().unwrap_or_else(|e| e.into_inner())` —
  never `.unwrap()` (poisoned locks should not crash; we keep going)
- **Persona reads**: use `Persona::cached()` in hot paths, never
  `Persona::load()` (the latter hits disk every call)
- **UI**: never import `crate::backend_module` from `ui_egui/*` —
  go through `AppService` trait; if you need new data, add a sub-trait
  method first
- **Theme**: stick to constants from `ui_egui::theme` (BG, ACCENT,
  TEXT_DIM, SP_SM, SP_MD, FONT_SMALL, etc).  No raw `Color32::from_rgb`.
- **Error type**: `Result<T, String>` for browser/test_runner
  (Box<dyn Error> creates Send+Sync headaches with our async usage)
- **Format strings in raw docs**: `{role}` → `{{role}}` to escape!
  Lost time twice on this. Even in r#"..."# raw strings, `format!`
  parses braces.
- **CRLF git warnings on Windows**: cosmetic, ignore them
- **Don't `unwrap()` on env vars or std::env::var**: returns Err if
  unset, we want graceful default
- **`cargo test` pipe bug**: `cargo test ... | tail -8` silently kills
  cargo when Bash tool auto-backgrounds the command. Always use:
  `cargo test --bin sirin > /tmp/sirin_test.txt 2>&1 ; tail -8 /tmp/sirin_test.txt`
- **Bash tool auto-backgrounds long commands**: can't be prevented.
  When it happens, do NOT call TaskOutput (blocks for timeout duration).
  Just wait for the `<task-notification>` event, then Read the output file.

## Gotchas already paid for

### `headless_chrome` 1.0.21 strict enum panic
Newer Chrome versions emit AXPropertyName values the crate doesn't
know (`uninteresting`, etc).  Strict serde fails the whole call.
**Workaround pattern** (in `browser_ax.rs`):
```rust
struct RawGetFullAxTree {}
impl Method for RawGetFullAxTree {
    const NAME: &'static str = "Accessibility.getFullAXTree";
    type ReturnObject = serde_json::Value;  // ← raw, parse loosely
}
```
Use this pattern for any other CDP method that crashes on enum mismatch.

### Flutter Web semantics tree collapse
After `Accessibility.enable` + first `getFullAXTree`, Flutter often
silently collapses the tree to a single `RootWebArea` if it doesn't
detect ongoing AT activity.  Symptom: 1-2 nodes returned.

**Two distinct situations** look identical:
1. **Cold start** — tree never populated; needs bootstrap (placeholder click)
2. **Post-navigation teardown** — Flutter rebuilding; self-recovers in ~1s

**Fix** (already in `browser_ax::get_full_tree`): detect ≤2 nodes, first
**poll 3×400ms** (`poll_tree_recovery`) to allow self-recovery (situation 2),
then call `enable_flutter_semantics` (placeholder click only — **Tab×2
permanently removed** in Issue #20, as it triggered URL resets on hash-route
Flutter apps via keyboard event delivery).

If you need to wait for tree readiness explicitly, use `wait_for_ax_ready`:
```rust
browser_ax::wait_for_ax_ready(20, 5000)?;  // block ≤5s until ≥20 nodes
```
or via MCP: `{"action":"wait_for_ax_ready","min_nodes":20,"timeout_ms":5000}`

### flutter_type is ASCII-only — CJK silently fails

`flutter_type` calls `press_key()` per `char`, but `press_key` sends
`Input.dispatchKeyEvent` which requires a standard keycode.  CJK chars
like `你`, `好` have no keycode → the key event is dropped silently, the
Flutter textbox stays empty.

**Workaround**: use ASCII for test messages in YAML goals (e.g.
`flutter_type text="hello"` instead of `flutter_type text="你好"`).

### shadow_click uses JS PointerEvent, NOT CDP Input.dispatchMouseEvent

`browser::shadow_click()` was rewritten to dispatch `PointerEvent` via JS
directly on the `flt-semantics` element.  The old CDP `Input.dispatchMouseEvent`
caused `about:blank` when clicking Flutter navigation buttons (flt-semantics
overlay intercepts and re-routes the CDP event as a top-level navigation).

**Never use `click_point` for Flutter nav buttons** — use `shadow_click`.

### flutter_enter: the reliable way to submit Flutter chat/forms

`browser::flutter_enter()` dispatches Enter `keydown`/`keyup` on
`document.querySelector('.flt-text-editing')`.  Use after `flutter_type`
to submit a message.  Icon-only send buttons (no aria-label) cannot be
found by `shadow_click name_regex=...` — this is the workaround.

Registered in builtins.rs + mcp_server.rs + executor.rs prompt (d87c3c0).

### Flutter CanvasKit + headless = blank
WebGL doesn't paint in Chrome headless mode.  Set `browser_headless:
false` per-test, or `SIRIN_BROWSER_HEADLESS=false` env globally.

### Flutter + SwiftShader: HTML renderer is correct, CanvasKit is not

Chrome crashes 3×/test run on native GPU (Flutter CanvasKit + Windows GPU).
Fixed by `--use-angle=swiftshader` in `browser.rs` `launch_with_mode()`.

#### What SwiftShader actually does to Flutter

| Flag combination | Flutter mode | Result |
|---|---|---|
| Native GPU (no flags) | CanvasKit | Works — but Chrome crashes 3×/run (GPU driver) |
| `--use-angle=swiftshader` alone | HTML renderer | Chrome still crashes (~30s CDP timeout during Flutter JS init) |
| `--use-angle=swiftshader` + `--ignore-gpu-blocklist` | CanvasKit attempted | ❌ All-black screen (CanvasKit fails on SwiftShader) |
| `--disable-gpu` | HTML renderer | ✅ **Stable — current approach** |

**Current approach: `--disable-gpu`** (commit `b2e6xxx`):
- No GPU or WebGL at all → Flutter unconditionally uses HTML renderer
- No SwiftShader WebGL processing → no 30-second CDP event silence → no timeouts
- `--disable-gpu` is the standard Puppeteer/Playwright CI flag

**`--use-angle=swiftshader`** was tried:
- Even without `--ignore-gpu-blocklist`, Chrome timed out (~30s) during Flutter JS init
- SwiftShader still processes WebGL calls, causing headless_chrome's event loop to time out
- Removed

#### What this means for tests

With `--disable-gpu` (HTML renderer mode):
- Flutter renders as real HTML DOM — CSS selectors, `click`, `type`, `find` all work
- Semantics tree is NOT available — `ax_find`/`ax_click` will not work
- Use `screenshot_analyze` for visual state, `click`/`type`/`find` for interaction
- Keep `browser_headless: false` per-test (HTML renderer + visible window for CDP)
- Add `url_query: {flutter-web-renderer: html}` to YAML as belt+suspenders
- The executor waits 5 s after `goto` before first screenshot check (Flutter init time)

### Chrome recovery re-launches in wrong headless mode
`with_tab()` recovery used to call `ensure_open_reusing()` → `default_headless()`
(always `true`). Fixed by `static TEST_DESIRED_HEADLESS: AtomicBool` in `browser.rs`.

- Executor calls `crate::browser::set_test_headless_mode(want_headless)` before `ensure_open()`
- Recovery calls `ensure_open(TEST_DESIRED_HEADLESS.load(...))` — not `ensure_open_reusing()`

### Black screen detection (`is_all_black_screenshot`)
Heuristic in `executor.rs`: if screenshot `size_bytes < 8_000` → all-black/blank.
Real rendered pages ≥ 15 KB; all-black PNG ≈ 2 KB.

Two check points:
1. After initial `goto` — if black: close + reopen + re-navigate
2. After every `screenshot`/`screenshot_analyze` in ReAct loop — same recovery
   + inject `"⚠️ 螢幕全黑...請重新執行 semantics bootstrap"` into history

### `rendering_failure` triage category
When `triage.rs` finds `screenshot_path` file < 8 KB, it immediately returns
`RenderingFailure` — **no auto-fix triggered**, no `claude_session` spawned.
This prevents wasting tokens fixing non-existent frontend bugs.
Category string: `"rendering_failure"` (serde snake_case).

### Hash-only navigation hangs
`tab.navigate_to(url).wait_until_navigated()` waits for
`Page.frameNavigated` which Chrome **does not** emit for fragment
changes.  `browser::navigate` auto-detects same-origin same-path
hash-only changes and uses `location.hash =` instead.  Already done.

### Perception layer — opt-in vision for Flutter Canvas (v0.4.3)
`TestGoal.perception` YAML field (`text` default / `vision` / `auto`)
controls how the ReAct loop observes the page before each LLM turn:

- `text` — legacy path. **Zero overhead** (perceive() short-circuits).
  Don't remove this branch casually; all existing YAML tests rely on it.
- `vision` — `perception::capture::screenshot_b64()` + `llm::call_vision`
  with `build_prompt_vision` (lean action list, 3-step history, 200-char
  observations).  Image is primary observation.
- `auto` — JS-eval probe (`canvas_detect::probe_page`) detects
  `window.flutter` / `flt-glass-pane` / `>=50% viewport canvas` and
  upgrades to vision only when true.  Recommended default for new
  Flutter tests.

**When to opt into vision:** only when AX tree is unavailable (no
`enable_a11y` fixture, or Flutter semantics refuses to bootstrap).  On
healthy pages, text mode + AX tree is faster and doesn't use more tokens
overall — measured 34 s / 8 calls (text) vs 126 s / 12 calls (vision)
on `redandan.github.io/#/login` 2026-04-22.

**Gotcha:** vision mode still requires a vision-capable LLM backend.
If the configured model is text-only, `call_test_llm` falls back to
the text prompt (with a `tracing::warn!`).

### Flutter hash-route change tears down CDP connection
Observed 2026-04-22 on `redandan.github.io/#/login` → `#/home`: after
a successful click, Flutter navigates hash, emits
`Target.targetInfoChanged`, and `headless_chrome`'s transport loop
receives `SendError { .. }` → `[browser] mid-call connection closed —
attempting one-shot recovery`.  Chrome gets relaunched and login state
is wiped.

**Why:** `headless_chrome` has a 30-second "no event" timeout on the
WebSocket.  Flutter's hash-route doesn't fire `Page.frameNavigated`, and
the Target event races the reconnect logic.

**No fix yet.**  If you're debugging a Flutter test that "almost works"
then resets to `about:blank`, this is likely the cause — not perception,
not the LLM prompt.  Candidate fixes: periodic `Runtime.evaluate('1')`
heartbeat during expected route changes, or switching to `chromiumoxide`.

### Mode-switch race after Chrome relaunch
First `navigate` after a freshly-launched Chrome can fail with
"wait: The event waited for never came".  600ms settle delay +
1-step retry already in `browser.rs`.  If you see this in a new
context, increase the settle.

### Port 7700 zombie sockets on Windows
After `Stop-Process -Force sirin`, the listening socket lingers
~2 minutes in TIME_WAIT.  Sirin auto-retries bind 3× × 2s.  If
still failing, `SIRIN_RPC_PORT=7701` is the escape hatch.

### Killing Sirin from Claude
`taskkill /F /IM sirin.exe` from Bash sometimes hits encoding issues
(it interprets `/F` as a path).  Use:
```bash
powershell -c "Get-Process sirin -ErrorAction SilentlyContinue | Stop-Process -Force"
```

### Can't rebuild while Sirin is running
Windows holds the .exe open.  Kill Sirin (above) before
`cargo build --release`, otherwise you get "access denied" silently
keeping a stale binary.  **Use `./scripts/dev-relaunch.sh`** to do
this automatically (kill → build → relaunch in one shot).

### Stale binary even when build "succeeded"
Worse failure mode: you forget to kill, build appears to succeed (it
re-uses .o files, only the final link fails on Windows file-lock,
sometimes silently), and you launch the *previous* exe against new
source.  Symptom: actions you just added in source return
`Unknown browser_exec action: <name>` from MCP.  The script catches
this by killing pre-build + verifying mtime post-build.

### `cargo run` vs `./target/release/sirin.exe`
Always run release for any real work.  Debug build's LLM calls take
2-3× longer and time out.

### Python on this Windows machine
Both `python` and `python3` resolve to the Microsoft Store stub
("No installed Python found").  Use `node`, `jq`, or shell tools
instead — don't rely on Python for testing or scripting.

### Browser singleton hangs forever when CDP disconnects mid-call
**Symptom (2026-04-20):** two parallel `run_test_async` runs against a
Flutter SPA. Chrome crashed ~35s after launch (`TargetDestroyed` fired
in CDP logs). Both runs got stuck at `step:0, current_action:"goto"`
forever; `get_test_result` polling never showed progress; the watchdog
on the Tokio task didn't fire because `executor.rs` `await`s a sync
`navigate()` call whose underlying CDP socket is already dead.
**Diagnosis:** `browser::with_tab` returns the cached `Tab` handle
without health-checking the WebSocket. When the Chrome target dies, the
next CDP method call blocks until OS-level TCP timeout (effectively never).
**Workarounds:**
- Don't fire concurrent `run_test_async` against the same singleton on
  Flutter targets — serialize them, or use `run_test_batch` (uses
  per-run `session_id` so each gets its own tab).
- If a run is "queued" or "running, step 0" for >60s, kill Sirin +
  Chrome and restart. The in-memory run will be lost; SQLite has nothing.
**Fix needed:** wrap each CDP call in a tokio timeout; on timeout, mark
the singleton dead so the next call relaunches Chrome instead of
silently reusing the corpse. Tracked as task chip "Fix browser singleton
hang on CDP disconnect".

### `claude_cli` LLM backend hangs on big ReAct prompts
**Symptom (2026-04-20):** YAML `llm_backend: claude_cli` switches a test
to spawning `claude -p` per LLM call. Iteration 1 works (~6s, small
prompt). Iteration 2's prompt grows to 10-20KB (history + screenshot
data url) and the subprocess never returns — hits the 600s watchdog kill
in `claude_session::run_claude_with_timeout`. The outer test then errors
with `claude subprocess timed out after 600s`.
**Confirmed:** dispatcher (`resolve_llm_backend` → `call_claude_cli` →
`spawn_blocking` → `run_sync`) routes correctly. stdin is already
`Stdio::null()` (line 566 of `claude_session.rs`). Node-direct on
Windows. The hang is inside the `claude` CLI itself, not in our wrapper.
**Workaround applied:** YAML files revert to default Gemini (commit
`cd5f2f7`). Comments left in `agora_chat_sse.yaml` and
`agora_staking.yaml` explaining the revert and how to re-enable.
**Fix needed:** investigate whether `claude` CLI has its own bug with
big prompts on Windows, or whether we need to chunk the ReAct context
before sending. Tracked as task chip "Investigate claude_cli ReAct hang
in test_runner".

### Flutter blank screenshot from headless ping-pong
Even with `browser_headless: false` in YAML, if Sirin's singleton was
launched in headless mode for an earlier call (say a wiki smoke test),
the Chrome process stays headless until killed. The Flutter test then
runs against a CanvasKit page that has never painted — failure
screenshot is solid dark gray.
**Detection:** read `failed.png`; if it's near-uniform color, your
"AI couldn't find the button" error is actually "page never rendered".
**Fix:** kill Chrome before the Flutter run so Sirin re-launches with
the YAML's headless preference, OR set `SIRIN_BROWSER_HEADLESS=false`
globally before any test runs.

### Pivot to direct MCP when AI test loop is unreliable
When a test fails with `too many invalid LLM responses` AND the
screenshot is blank/ambiguous, **don't** keep retrying the AI loop —
pivot to direct verification:
```bash
sirin_call browser_exec action=goto target=https://app.example.com
sirin_call page_state                    # title + url + text excerpt
sirin_call browser_exec action=ax_tree   # accessibility nodes — Flutter-friendly
sirin_call browser_exec action=eval target='fetch("/version.json").then(r=>r.text())'
```
This was how Issue #34 (staking N/A) was verified on prod 1.0.991+992 on
2026-04-20 after both Gemini and claude_cli AI loops failed. ax_tree
returned 7 nodes including the expected "提交" button — proof the page
renders even though the AI loop couldn't see it. Save the AI loop for
multi-step exploratory flows; use direct MCP for single-shot verification.

### AgoraMarket Flutter AX tree patterns (2026-04-24)

**商品卡** — the product card widget:
```
role=button, name = "<multi-line string>\n<price> USDT"
```
- ✅ `shadow_click role=button name_regex="USDT"` — matches any product card
- ❌ `shadow_click role=button name_regex=".+"` — fails, `.+` doesn't match `\n` in names

**底部導航 Tab**:
```
role=tab, name = "商品" | "訂單" | "錢包" | "我的"
```
- ✅ `shadow_click role=tab name_regex="^我的$"` — exact match required

**登出按鈕位置**: 在「我的」頁最底部，需先 `scroll y=600` 才可見

### JSON syntax in YAML goal text causes LLM parse failures

**Problem**: Writing `scroll {"direction":"down","amount":500}` in a YAML goal
teaches the LLM to use that exact format in its JSON response — which doesn't
match the executor's expected schema `{"action":"scroll","y":500}`.
The LLM then produces non-parseable JSON and the test fails with
`too many invalid LLM responses (N)`.

**Fix**: Always use plain-text descriptions in goal text:
```yaml
# ❌ Breaks LLM JSON output format
4. scroll {"direction":"down","amount":500}

# ✅ Correct — describes intent without JSON fragment
4. 向下捲動 500px（scroll y=500）
```

The correct scroll action schema (for LLM reference):
```json
{"action": "scroll", "y": 500}
```

### ?__test_role= URL auto-login (AgoraMarket)

AgoraMarket `login_page.dart` reads `Uri.base.queryParameters['__test_role']`
and calls `_handleDemoLogin(username)` automatically on test domains.

Sirin executor **must** do `Storage.clearDataForOrigin` BEFORE navigation to
wipe any stale Flutter session from the profile DB — `localStorage.clear()` is
insufficient because Flutter has already read the session into memory.

In `executor.rs`, the trigger condition:
```rust
if test.fixture.is_some() || nav_url.contains("__test_role=") {
    clear_origin_storage(&nav_url);
    wait(8000);      // Flutter needs ~6-8s to complete auto-login
    enable_a11y();
}
```

Without this, tests using `?__test_role=` URLs run with the PREVIOUS test's
session — a subtle contamination bug that makes test results non-repeatable.

### Flutter AppBar back button has no accessible name

Flutter Material AppBar's back arrow button emits **no accessible name** in the
AX/semantics tree.  `shadow_click role=button name_regex="Back|返回|navigate back"`
will silently fail to find the element — confirmed on AgoraMarket 2026-04-24.

**Workaround**: use the browser history API instead:
```yaml
eval target='window.history.back()'
wait 2000
screenshot_analyze "是否回到上一頁？"
```

`eval` dispatches a real JS call; Flutter's hash-route router picks it up and
navigates back correctly.  Tested on `agora_navigation_breadcrumb` (2026-04-24).

### YAML goal design: linear steps beat conditional branches

**Symptom**: test exhausts `max_iterations` without ever outputting `done=true`.
The LLM keeps retrying or trying new approaches instead of terminating.

**Root cause**: YAML goals with `if/else` branches confuse the LLM.  It sees
partial progress on a branch and keeps exploring instead of recognising the
exit condition.

**Rules**:
1. Write steps as a flat numbered list — no nested `if`/`else`
2. Put `done=true` at the last step, **unconditionally** (e.g. step 9)
3. Let `success_criteria` decide pass/fail — not the LLM's `done=true` decision
4. Keep `max_iterations` ≤ 2× the number of steps (not ≤ 40 "just in case")
5. Add `⚠️ 即使某個步驟找不到元素也繼續往下，不要重試` at the goal header

**Bad (loops forever)**:
```yaml
goal: |
  4. shadow_click role=button name_regex="Buy"
     若找不到 → screenshot_analyze → done=true
  5. wait 3000 → done=true
  # LLM never hits done=true because it keeps retrying step 4
```

**Good (always terminates)**:
```yaml
goal: |
  ⚠️ 即使某個步驟找不到元素也繼續往下，不要重試同一步驟。
  4. shadow_click role=button name_regex="Buy"（找不到也繼續）
  5. wait 3000
  6. screenshot_analyze "目前頁面狀態？"
  7. done=true   ← 無條件，永遠執行到這裡
```

### YAML sync: repo → %LOCALAPPDATA%\Sirin\config\tests

Release binary reads YAML from `%LOCALAPPDATA%\Sirin\config\tests\`,
NOT from `./config/tests/` in the repo. After editing any YAML:
```bash
cp config/tests/agora_regression/*.yaml "$LOCALAPPDATA/Sirin/config/tests/agora_regression/"
```

## Useful test commands

```bash
# Single test
cargo test --bin sirin browser_ax::tests::ax_node_matches_by_role

# All browser_ax tests
cargo test --bin sirin browser_ax

# Show printlns
cargo test --bin sirin <name> -- --nocapture

# Ignored (E2E) tests — need Chrome + LLM
cargo test --bin sirin browser_lifecycle -- --ignored --nocapture
```

## Commit conventions

- **Conventional Commits**: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`
- Body explains **why**, not just what
- Co-author trailer for AI authorship:
  ```
  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
  ```
- Push to `main` directly (small project, no PR workflow yet)
- After every feature commit: update `.claude/skills/sirin-*/SKILL.md`
  + `docs/MCP_API.md` in the same session.  Drift kills discoverability.

## Don't do these (already considered + rejected)

- ❌ Bayesian flakiness detection (10-sample CI is too wide; current
  70%/10-run threshold is sufficient — see roadmap)
- ❌ Backend trait abstraction over headless_chrome (YAGNI; the crate
  is stable; no real backend swap need)
- ❌ Switching from egui to anything (decided long ago; AI reads code
  to "see" UI, this is the point)
- ❌ Adding Node.js/Python sidecars (zero non-Rust deps in core; CDP
  goes direct)
- ❌ HTML test report generator (CLI + SQLite is enough until proven
  otherwise)
- ❌ SO_REUSEADDR on Windows (different semantics than Unix; security
  anti-pattern; we use port retry + escape hatch instead)

## Where state lives

| What | Where |
|------|-------|
| Process-wide Chrome session | `OnceLock<Mutex<Option<BrowserInner>>>` in `browser.rs` |
| Active test runs | `OnceLock<Mutex<HashMap<String, RunState>>>` in `test_runner/runs.rs` |
| Test history | SQLite `%LOCALAPPDATA%\Sirin\data\test_memory.db` |
| Failure screenshots | `%LOCALAPPDATA%\Sirin\test_failures\<id>_<ts>.png` |
| LLM config | `%LOCALAPPDATA%\Sirin\.env` + `config/llm.yaml` (override) |
| Skill registry | `OnceLock` cache + `config/skills/*.yaml` |
| LLM fleet | `OnceLock<Arc<AgentFleet>>` in `llm/probe.rs` |
| MCP server bind | `:7700` (or `SIRIN_RPC_PORT` override) |
| Update status | `OnceLock<Mutex<UpdateStatus>>` in `updater.rs` |
| Installed binary | `C:\Program Files\Sirin\sirin.exe` |
| User data root | `%LOCALAPPDATA%\Sirin\` (all modes except `#[cfg(test)]`) |

## When you're done

Before declaring "done" on any change:

1. `cargo check` → 0 errors, 0 warnings
2. `cargo test --bin sirin` → all pass (currently 468)
3. Updated docs (`SKILL.md` + `MCP_API.md` if user-facing)
4. Conventional commit message
5. Push to `main` (no PR workflow currently)
6. Optional: smoke test against a real page if you touched
   `browser.rs` or `browser_ax.rs`

## Related skills

- `sirin-launch` — for starting/stopping Sirin from another session
- `sirin-test` — for using Sirin to test apps (not develop on Sirin)
