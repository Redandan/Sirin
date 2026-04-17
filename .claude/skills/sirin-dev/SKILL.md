---
name: sirin-dev
description: Use this skill when developing on the Sirin project itself (not when using Sirin to test other apps) — adding a new browser action, MCP endpoint, agent skill, test_runner feature, or fixing a bug in the Rust code.  Trigger phrases include "add a Sirin action", "fix Sirin's X", "extend Sirin", "modify Sirin", "Sirin internals", "how does Sirin X work", or any task that involves editing files under `~/IdeaProjects/Sirin/src/`.  This skill is for AI sessions picking up Sirin development cold — covers architecture, common workflows, conventions, and the gotchas that have already cost us time.
version: 1.0.0
---

# Sirin Development Skill

Onboarding for AI sessions developing on Sirin itself.  If you're using
Sirin's MCP API to test other apps, see `sirin-test` instead.

## When This Skill Applies

- Editing Rust source under `~/IdeaProjects/Sirin/src/`
- Adding a browser action, MCP endpoint, test_runner field, agent skill
- Investigating Sirin internals to answer "how does X work?"
- Fixing a bug reported against Sirin (vs in a target app)

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
│                           — 30+ actions: navigate/click/type/wait/scroll/...
│                           — auto-reconnect, hash-route fast path,
│                             headless mode switching, settle delay,
│                             nav retry, network capture (req+res body),
│                             clear_browser_state, wait_for_new_tab,
│                             wait_for_request, multi-tab management
├── browser_ax.rs           CDP Accessibility tree (literal text — for
│                           K14-style exact assertions; uses raw JSON
│                           Method to bypass headless_chrome strict
│                           enum bug; auto-retriggers Flutter semantics)
├── claude_session.rs       Spawn `claude` CLI cross-repo bug fixing
├── config_check.rs         Diagnostics + AI fix proposal (dual-confirm)
├── test_runner/            AI test runner (browser, not unit tests)
│   ├── parser.rs           YAML TestGoal (locale, retry, url_query,
│   │                       browser_headless, success_criteria, tags)
│   ├── executor.rs         ReAct loop driving web_navigate;
│   │                       ALSO contains the LLM prompt — when adding
│   │                       a new web_navigate action, advertise it
│   │                       in the prompt's "Available actions" list
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
├── mcp_server.rs           HTTP MCP server on :7700/mcp.  When adding
│                           a browser action that should be externally
│                           callable, ALSO add to this file's
│                           call_browser_exec match arm AND the
│                           tools/list schema.
├── llm/                    Multi-backend LLM (Ollama/LMStudio/Gemini/
│                           Claude) + vision multimodal
├── ui_egui/                egui UI — sidebar, settings, browser panel,
│                           workflow, meeting; reads ONLY through AppService
├── ui_service.rs           AppService trait — UI ↔ backend boundary
│                           (6 sub-traits). Don't import backend modules
│                           directly from ui_egui.
├── ui_service_impl/        RealService impl of AppService
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
cargo test --bin sirin    # currently 257 tests, all should pass
cargo build --release     # ~5-7 min cold
./target/release/sirin.exe                       # launch GUI on port 7700
SIRIN_RPC_PORT=7701 ./target/release/sirin.exe   # alt port if 7700 stuck
SIRIN_BROWSER_HEADLESS=false ./target/release/... # for Flutter / WebGL
```

Avoid `cargo run` (debug build, slow startup, LLM calls may time out).

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
**Fix** (already in `browser_ax::get_full_tree`): detect ≤2 nodes,
call `enable_flutter_semantics` (placeholder click + Tab×2), retry once.

### Flutter CanvasKit + headless = blank
WebGL doesn't paint in Chrome headless mode.  Set `browser_headless:
false` per-test, or `SIRIN_BROWSER_HEADLESS=false` env globally.
The `agora_market_smoke.yaml` example doc tests this.

### Hash-only navigation hangs
`tab.navigate_to(url).wait_until_navigated()` waits for
`Page.frameNavigated` which Chrome **does not** emit for fragment
changes.  `browser::navigate` auto-detects same-origin same-path
hash-only changes and uses `location.hash =` instead.  Already done.

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
keeping a stale binary.

### `cargo run` vs `./target/release/sirin.exe`
Always run release for any real work.  Debug build's LLM calls take
2-3× longer and time out.

### Python on this Windows machine
Both `python` and `python3` resolve to the Microsoft Store stub
("No installed Python found").  Use `node`, `jq`, or shell tools
instead — don't rely on Python for testing or scripting.

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
| Test history | SQLite `data/test_memory.db` (gitignored) |
| Failure screenshots | `data/test_failures/<id>_<ts>.png` (gitignored) |
| LLM config | `.env` (gitignored) + `config/llm.yaml` (override) |
| Skill registry | `OnceLock` cache + `config/skills/*.yaml` |
| LLM fleet | `OnceLock<Arc<AgentFleet>>` in `llm/probe.rs` |
| MCP server bind | `:7700` (or `SIRIN_RPC_PORT` override) |

## When you're done

Before declaring "done" on any change:

1. `cargo check` → 0 errors, 0 warnings
2. `cargo test --bin sirin` → all pass (currently 257)
3. Updated docs (`SKILL.md` + `MCP_API.md` if user-facing)
4. Conventional commit message
5. Push to `main` (no PR workflow currently)
6. Optional: smoke test against a real page if you touched
   `browser.rs` or `browser_ax.rs`

## Related skills

- `sirin-launch` — for starting/stopping Sirin from another session
- `sirin-test` — for using Sirin to test apps (not develop on Sirin)
