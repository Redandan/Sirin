# scripts/

Helper scripts for development, deployment, and ops. Most are bash for
the cross-platform happy path (Windows users use Git Bash); two are
PowerShell (`*.ps1`) for callers that need pure-pwsh — same contracts
where they overlap.

| Script | What it does | When to run |
|---|---|---|
| **`preflight.sh`** | 6-section sanity check: LLM keys / gateway up / vision smoke / config sync / Chrome stability / action-registry consistency. Exits non-zero on FAIL. | Before any benchmark or LLM-comparison session. |
| **`dev-relaunch.sh`** | Safe rebuild loop: kill running Sirin → free port → `cargo build --release` → relaunch. Falls through to alt port (#14 zombie defence). | Inner dev loop after a code change. |
| **`watch-sirin.ps1`** | PowerShell watchdog. Polls every 5s; relaunches if binary is newer than the running process. | Long-lived dev sessions where you want auto-restart on rebuild. |
| **`kill-port.sh`** / **`kill-port.ps1`** | Find and kill whoever owns the TCP LISTEN socket on `<port>`, regardless of process name. Silent if free; best-effort on permission errors. | Manually clearing a stuck port without restarting Sirin. Run by `dev-relaunch.sh` automatically on 7700-7703. |
| **`fetch-handoff.sh.example`** | Reference copy of `~/.claude/scripts/fetch-handoff.sh` — SessionStart hook that injects the latest mid-session handoff into a new Claude Code session. Includes hardened token handling (`umask 077` + 600 tempfile + `curl -K`). | One-time install: `cp scripts/fetch-handoff.sh.example ~/.claude/scripts/fetch-handoff.sh`. |
| **`check_kb_freshness.sh`** | Post-commit hook: scans files changed in the last commit and marks any KB entries whose `fileRefs` overlap as `STALE` via `kbMarkStale`. | Auto-installed by `install_kb_freshness_hook.sh`. |
| **`install_kb_freshness_hook.sh`** | Wires `check_kb_freshness.sh` into `.git/hooks/post-commit`. Idempotent; `--uninstall` to remove. | One-time setup per checkout. |
| **`squad-restart.sh`** | Kill → rebuild → relaunch → start N multi-agent squad workers. Default 2 workers on port 7710. | Bringing the dev squad back online after a code change to `multi_agent/`. |
| **`squad-status.sh`** | Pretty-print the multi-agent task queue (queued / running / done / failed counts + last 10 entries). | Monitoring squad progress without opening the web UI. |
| **`squad-cleanup-old-done.sh`** | Prune `done`/`failed` tasks older than N days from the queue (default 7, clamped 1–90). `SQUAD_CLEANUP_DRY_RUN=1` for preview. | Periodic maintenance — queue files grow unbounded otherwise. |
| **`verify-new-actions.sh`** | Smoke test for the MCP actions added 2026-04-17 (`page_state`, `ax_find` regex flags, `ax_snapshot`/`ax_diff`, `authz deny`). Runs against live AgoraMarket. | After landing changes that touch the action registry — quick "did I break the public surface" check. |
| **`ocr_windows_find_text.ps1`** | PowerShell helper that runs Windows OCR on a screenshot and returns the bounding boxes of `<Needle>`. Used by the `browser_exec action=ocr_find_text` MCP tool. | Don't invoke directly — Sirin spawns it. |

## Conventions

- **Exit codes**: 0 = success, non-zero = caller should see the error.
- **Idempotent**: re-running a script must not corrupt state. `dev-relaunch.sh`, `install_kb_freshness_hook.sh`, `squad-restart.sh` all conform.
- **Cross-platform paths**: scripts that read or write user data go through `$LOCALAPPDATA/Sirin/` on Windows or the equivalent (see `src/platform.rs::app_data_dir`). No hard-coded `~/IdeaProjects/Sirin` paths.
- **Quiet by default**: ops scripts suppress noise on the happy path. Use `-v` / set `DEBUG=1` for verbose output (when supported).

## Adding a new script

1. Drop the file into `scripts/`.
2. `chmod +x scripts/your-script.sh`.
3. Add a row above with what / when. The `scripts_index` test in `src/test_runner/parser.rs` walks the directory and panics if any file is undocumented.
