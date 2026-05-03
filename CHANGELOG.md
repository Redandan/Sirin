# Changelog

All notable changes to Sirin are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [0.5.6] — 2026-05-03

### Changed
- **docs**: Final cleanup pass after v0.5.0 web UI migration — removed
  stale `egui` / `eframe` / `ui_egui` / `winit` references from README,
  ARCHITECTURE, ROADMAP, MCP_API, QUICKSTART, sirin-dev / sirin-launch
  skills, and the `/status` + `/ui-map` slash commands.  `Cargo.toml`
  description rewritten to reflect plain-HTML web UI on `:7700/ui/`.
  `docs/architecture/ui_egui.md` deleted; `docs/DESIGN_MONITOR.md`
  marked partially superseded.
- **codebase**: `src/memory/codebase.rs` file descriptions updated for
  `web/index.html` / `web/app.js` / `web/style.css`; chat_agent test
  fixtures no longer reference deleted `ui_egui/*` paths.

### Removed
- Obsolete unit tests in `multi_agent::mod` and `multi_agent::queue`
  that depended on `src/ui_egui/workspace.rs` (deleted in v0.5.0).

No functional changes — docs / metadata only.  All 632+ tests still pass.

---

## [0.5.0–0.5.5] — 2026-05-02

### Added
- **web**: Plain HTML + Alpine.js web UI served at `:7700/ui/`,
  bundled via `include_bytes!` for single-binary distribution.
  Replaces egui (see migration notes in `web/DESIGN.md`).
- **api**: `GET /api/snapshot` (read), `POST /mcp` (write — existing),
  `GET /api/browser_screenshot`, `POST /api/chat`, `GET /api/agent/{id}`,
  `POST /api/pending/{agent_id}`, `POST /api/persona/name`,
  `GET /api/health`, `GET /api/logs`, `GET /api/team_dashboard`,
  `WS /ws` (2 s push, replaces 5 s HTTP polling).
- **chat**: Persistent chat history via SQLite `chat_messages` table
  (shares the `__shared_db()` connection with `test_runner::store`).
- **dashboard**: Composable widget grid — 4-col layout with localStorage
  persistence, 10-widget catalog including 6 KPI cards (pass rate /
  runs today / avg duration / cost-per-hour / active agents / running
  tests).
- **workspace**: Per-agent detail view with 4 sub-tabs (對話 / 概覽 /
  待確認 / 設定).
- **mcp playground**: Inline tool-call tester with JSON request/response
  preview.

### Changed
- **bin**: Closing the browser tab no longer kills the daemon — RPC,
  Telegram listener, scheduler, and screenshot pump keep running.
  Re-open `:7700/ui/` any time; `taskkill /F /IM sirin.exe` (or Ctrl-C
  in the terminal) is the only way to fully stop.
- **headless**: `--headless` / `SIRIN_HEADLESS=1` now skips the
  auto-open of the user's default browser only — RPC/MCP server,
  browser singleton, telegram listeners, test_runner all start
  normally; the web UI is still reachable at `:7700/ui/` if you
  navigate there manually.

### Removed
- `src/ui_egui/` and the `eframe` / `egui` / `winit` dependency tree
  (Phase 7 of the migration, commit 0690a77).

---

## [0.4.4] — 2026-04-25

### Added
- **browser**: `go_back` action — Chrome history back via `window.history.back()`
  with navigation settle wait.  Closes #28.  Exposed through `web_navigate`
  (internal agents) and `browser_exec` (external MCP).
- **ui**: Test Dashboard panel (`測試儀表板`) — live `Active runs` (in-memory
  registry, 3s refresh, pulse animation) + `History` (last 30 from SQLite
  store).  Each row shows status badge (PASS/FAIL/TIME/ERR/RUN/WAIT) +
  test_id + AI analysis snippet.  New `View::TestRuns` + `TestRunnerService`
  sub-trait (8th sub-trait of `AppService`).
- **llm**: `GEMINI_CONCURRENCY` env var (default 3) — process-wide
  `tokio::sync::Semaphore` caps concurrent Gemini API requests to prevent
  the silent "200 + empty content" responses Gemini's free tier returns
  when batch tests fan out 8 parallel `screenshot_analyze` calls.
- **scripts**: `dev-relaunch.sh` step `[2b]` — auto-rsync
  `config/tests/*.yaml` → `%LOCALAPPDATA%\Sirin\config\tests\` after build,
  so YAML edits take effect without manual copy.

### Fixed
- **test_runner**: `parse_step` now recovers from Gemini's plaintext
  "label: {json}" drift after a parse retry — three-pronged fix: root-action
  recovery (treats root as action_input when wrapper missing), brace-depth
  plaintext fallback parser, and stricter parse_error_hint with explicit
  schema example.  Resolves "too many invalid LLM responses (5)" failures
  on `agora_pickup_checkboxes_restore` and similar tests where Gemini
  drifted into plain-text format and never recovered.
- **test_runner**: Fire `notify_failure` only from `runs::set_phase()` —
  removed duplicate call in `store::record_run` that caused two Telegram
  notifications per failed run.
- **llm**: HTTP 200 + empty `choices[0].message.content` from Gemini now
  retries 2× with 2 s / 4 s backoff (Gemini-only; local backends unaffected).
- **tests**: All 22 Agora YAMLs no longer hardcode `browser_headless: false`
  — centralized via `.env SIRIN_BROWSER_HEADLESS=false` (per-test YAML
  override still supported but discouraged).

### Internal
- 8 sub-traits in `AppService` (added `TestRunnerService`).
- 11 modules in `src/ui_egui/` (added `test_dashboard.rs`, `team_panel.rs`).
- 479 unit tests passing (was 469 in v0.4.3).

---

## [0.4.3] — 2026-04-19

### Added
- **multi_agent**: Parallel worker pool (T1-1) — multiple squad workers run
  concurrently via Tokio tasks; controlled by configurable `max_workers`.
- **multi_agent**: Auto-retry on failure (T1-5) — failed tasks are automatically
  re-queued once before being marked as permanently failed.
- **multi_agent**: Persistent task queue + background worker; tasks survive
  process restarts via SQLite; UI supports role/status filtering.
- **ui**: Squad monitor panel (`開發小隊監控面板`) — live view of all agent
  roles, task queue, and worker health.
- **test_runner**: Telegram failure notification hook — failed test runs post a
  summary message to the configured Telegram chat.
- **process_group**: Windows `JobObject` with `KILL_ON_JOB_CLOSE` — child
  processes (Chrome, `claude` CLI) are reliably killed when Sirin exits.

### Fixed
- **claude_session**: **Fix A** — subprocess hard wall-clock timeout (600 s)
  via watchdog thread; prevents squad worker threads blocking indefinitely
  when the Anthropic API hangs at high concurrency. Insufficient alone — see
  Fix B and `docs/postmortem/2026-04-19-silent-crash.md`.
- **claude_session**: **Fix B** — stream `claude` stdout line-by-line via
  `BufReader::new(stdout).lines()` instead of buffering the full stream-json
  via `read_to_end()`. Drops per-call peak memory from 80–100 MB to ~1 MB
  and resolves the silent-OOM crash at 38–45 min uptime under N≥2 workers.
- **claude_session**: **Fix C** — extract `build_claude_command()` helper so
  all three spawn sites (`run_one_round`, `run_one_turn_scoped`,
  `run_claude_with_timeout`) bypass `cmd.exe` on Windows uniformly. Fix B's
  new spawn site copied the legacy `cmd /c claude.cmd` pattern, which
  silently truncated multi-line squad prompts at the first `\n` and broke
  every PM↔Engineer round — manifesting as "PM 5 輪後仍未核准" failures.
- **multi_agent**: `assign_task` context overflow prevention — truncates LLM
  context before it hits the token limit.
- **multi_agent**: P0 — Tester cargo lock + `assign_task` iteration loop fix.
- **mcp_server**: Safe UTF-8 truncation for CJK task descriptions — prevents
  panic when truncating multi-byte sequences at byte boundary.
- **platform**: Isolate `app_data_dir()` in test builds to avoid polluting
  `%LOCALAPPDATA%` during `cargo test`.
- **triage**: Thread `run_id` through `trigger_auto_fix` for correct run
  attribution in auto-fix history.
- **test(authz)**: Fix `audit_test` parallel flakiness from Windows PID reuse.

---

## [0.4.2] — 2026-04-18

### Changed
- **updater**: Replace `self_update` crate with a direct GitHub Releases API
  call — removes a heavy transitive dependency tree, shrinks binary ~400 KB.
- **deps**: Remove `parking_lot`; tighten `tokio` feature flags to only what is
  used; clean `eframe`/`egui` version pins.
- **build**: Enable thin LTO + `codegen-units = 4` in release profile — ~15 %
  smaller binary, ~8 % faster cold-start.

---

## [0.4.1] — 2026-04-18

### Fixed
- **updater**: Installer-download flow on Windows was silently failing when the
  asset zip contained a path component; now extracts `sirin.exe` correctly.
- Residual Issue #22 regressions cleaned up (pointer-event sequencing edge
  cases in headless Chrome).

### Added
- **build**: `dev-fast` Cargo profile — `opt-level = 1`, no LTO — for faster
  incremental builds during development.

---

## [0.4.0] — 2026-04-18

### Added
- **headless mode**: `--headless` flag (or `SIRIN_HEADLESS=1`) skips
  `eframe::run_native()`. MCP server, browser singleton, Telegram listeners,
  and test_runner all start normally; main thread parks until SIGINT/SIGTERM.
  Enables running Sirin as a background service / in CI / over SSH.
- **mcp**: `run_test_batch` — parallel YAML test fan-out via Tokio `Semaphore`,
  up to 8 concurrent Chrome tabs, per-test `session_id` isolation.
- **test_runner**: `persist_adhoc_run` — ad-hoc explore results are written to
  SQLite so they survive process restarts.

---

## [0.3.3] — 2026-04-18

### Added
- **mcp**: `persist_adhoc_run` tool — promotes a successful ad-hoc explore into
  a permanent regression YAML in `config/tests/`.

### Fixed
- **diagnose**: Chrome and LLM status now report truthfully; port-fallback logic
  tightened; error filter no longer swallows meaningful failures.
- **browser**: Stale URL / title after Flutter navigation (Issue #23) — now
  resolved via raw `Runtime.evaluate` CDP instead of the cached property.

---

## [0.3.2] — 2026-04-18

### Added
- **mcp**: `diagnose` tool — two-tier external-AI bug triage: fast heuristic
  pass followed by an LLM explanation with suggested fix.

### Fixed
- **browser_ax**: `ax_click` now emits the full 5-event `PointerEvent` sequence
  required by Flutter web (Issue #22-3).
- **mcp**: Per-session `client_id` assignment prevents cross-session state
  bleed; panic recovery middleware added to all handlers.
- **mcp**: 180 s `TimeoutLayer` prevents `CLOSE_WAIT` socket buildup under load.
- **browser_ax**: `Accessibility.Enable` is now idempotent — calling it a
  second time no longer resets the page URL (Issue #21).

### Changed
- Resolved all 33 Clippy warnings — codebase is now `cargo clippy`-clean.

---

## [0.3.0] — 2026-04-17

### Added
- **browser**: Condition waits — `wait_for_url`, `wait_for_network_idle`.
- **browser**: Multi-session management — `session_switch`, `list_sessions`,
  `close_session`.
- **browser_ax**: `find_scrolling_by_role_and_name` — scroll-to-find for
  off-screen AX nodes.
- **adk**: `assert` tool for inline test assertions in ReAct loops.
- **bin**: `sirin_call` — thin CLI wrapper for the MCP API; avoids shell
  escaping pain with CJK/Unicode payloads; `key=value` or stdin JSON syntax.

### Fixed
- **browser_ax**: Issue #20 — remove `Tab×2` fallback that was causing
  spurious focus events; add teardown-recovery poll in `get_full_tree`.

### Changed
- **ui**: Collapsible sidebar (`‹` / `›` toggle).
- **ui**: Version shown in sidebar header.

---

## [0.2.0] — 2026-04-17

### Added
- **installer**: Inno Setup 6 script (`sirin.iss`) — produces
  `SirinSetup-X.Y.Z.exe`; default-enables autostart on fresh install.
- **updater**: Auto-update via GitHub Releases (`src/updater.rs`);
  `spawn_check()` on startup, `apply_update(ver)` downloads zip and replaces
  binary; status banner in UI.
- **platform**: `src/platform.rs` — cross-platform data/config dir resolution.
  `config_dir()` returns `./config` in `#[cfg(test)]`, production path in
  release builds. **All path literals migrated; never use `"config/foo.yaml"`
  directly — always call `platform::config_path()`.**
- **ci**: `.github/workflows/release.yml` — build + publish on `v*.*.*` tag push.
- **scripts**: `dev-relaunch.sh` (kill → build → launch), `verify-new-actions.sh`
  (MCP smoke test).

---

[0.4.3]: https://github.com/Redandan/sirin/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/Redandan/sirin/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/Redandan/sirin/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/Redandan/sirin/compare/v0.3.3...v0.4.0
[0.3.3]: https://github.com/Redandan/sirin/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/Redandan/sirin/compare/v0.3.0...v0.3.2
[0.3.0]: https://github.com/Redandan/sirin/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Redandan/sirin/releases/tag/v0.2.0
