# Sirin — Project Layout

Module-level map of the Sirin codebase.  Cross-reference for `CLAUDE.md`'s
high-level architecture summary.  Updated alongside meaningful module
additions / re-organisations so a new AI session can grep this file
instead of `find src/`.

```
web/                         Web UI (v0.5.0+) — single-binary embedded via
                             include_bytes! in mcp_server.rs:
                             • index.html  — Alpine.js x-data root, ~6 views
                             • style.css   — design tokens + components
                             • app.js      — sirin() factory: state + fetch
                             • alpine.min.js — bundled v3 runtime
                             • help.html   — Chinese HTML docs for other
                                             AI clients (served at /help)
                             • root.html   — landing page served at /welcome
                                             (4 cards + AI-discovery meta tags)
                             • llms.txt    — markdown for LLM crawlers
                                             (llmstxt.org convention)
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
  idle_close.rs              Background watcher — closes the test Chrome after
                             SIRIN_BROWSER_IDLE_CLOSE_SECS (default 60s) of
                             no active runs.  Exposes idle_close_in_secs() so
                             /api/snapshot can render the live countdown.
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
src/ai_monitor.rs            Local AI activity, resource, power, recovery, and
                             acceptance snapshot exposed through `/api/snapshot`.
src/codex_supervisor.rs      Codex desktop task supervision — structured
                             evidence classification, task contract, coverage
                             matrix, local dedupe ledger, and claim protocol.
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
src/doctor.rs                `sirin doctor` subcommand (Issue #261) — pure-Rust
                             6-section preflight (LLM keys / gateway / vision /
                             YAML sync / Chrome stability / action registry).
                             Text + JSON outputs.  Exits non-zero on FAIL.
src/tray.rs                  System tray icon (Issue #272) — Windows-only MVP.
                             Shows status text + Open Dashboard / Quit menu.
                             Tooltip surfaces #279 sub-phase / queue ETA /
                             browser idle countdown.  Skipped in --headless.
```

For deeper architecture rationale (egui→web migration, ADK-RUST agent layer,
LLM caller indirection) see [`docs/ARCHITECTURE.md`](ARCHITECTURE.md).
