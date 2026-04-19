# Changelog

All notable changes to Sirin are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
