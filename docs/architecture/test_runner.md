# Test Runner — Architecture Overview

> Source: `src/test_runner/` (7 files, ~2 400 lines total)
> Related: [`docs/test-runner-roadmap.md`](../test-runner-roadmap.md) · [`docs/MCP_API.md`](../MCP_API.md) · [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md)

---

## 1. Purpose

The test runner replaces scripted browser automation (Puppeteer / Playwright) with
an AI agent that receives a **goal in plain text** and figures out how to achieve it
using the same 45+ Chrome CDP actions available to the coding agent.

Three capabilities distinguish it from script-based tools:

| Capability | Scripted tools | Sirin test runner |
|------------|:-:|:-:|
| Define test as a goal, not a script | ❌ | ✅ |
| Classify failures and trigger auto-fix | ❌ | ✅ |
| Remember flaky history across runs | ❌ | ✅ |

**When to use this vs unit tests:** unit tests verify pure logic in isolation;
the test runner verifies end-to-end user flows against a live browser.  It is
intentionally slow and LLM-heavy — use it for smoke tests and regression checks,
not for hot-loop TDD.

---

## 2. Module Map

| File | Lines | Responsibility |
|------|------:|----------------|
| `mod.rs` | 913 | Public API: `run_test`, `spawn_run_async`, `spawn_adhoc_run`, `spawn_batch_run`, `run_all`, `persist_adhoc_run`; internal `run_test_with_run_id` orchestrator |
| `parser.rs` | 267 | `TestGoal` struct; YAML deserialization; `Fixture` setup/cleanup steps; `full_url()` query-string merge; `load_all()` / `find()` |
| `executor.rs` | 650 | `execute_test_tracked()` — ReAct loop driving `web_navigate`; `build_prompt()`, `parse_step()`, `evaluate_success()`; fixture lifecycle |
| `triage.rs` | 439 | `FailureCategory` enum; `triage()` 3-step classifier; `trigger_auto_fix()` with dedup + 3-failure cap |
| `store.rs` | 659 | SQLite `test_runs` + `test_knowledge` + `auto_fix_history`; `is_flaky()` logic; `find_run_by_run_id()` slow-path |
| `runs.rs` | 252 | In-memory async run registry (`RunPhase` state machine, 1-hour TTL prune) |
| `i18n.rs` | 167 | `Locale` enum (ZhTw / En / ZhCn); per-locale prompt strings |

---

## 3. Run Lifecycle

```
Caller (MCP / agent tool / UI)
        │  test_id: &str
        ▼
mod.rs: run_test() / spawn_run_async() / spawn_batch_run()
        │
        ├── parser::find(test_id)               parser.rs:246
        │     └── scans config/tests/*.yaml
        │           returns TestGoal or error
        │
        ├── runs::new_run(run_id)                runs.rs:87
        │     └── inserts RunPhase::Queued into in-memory registry
        │           prunes Complete/Error runs older than 1 hour
        │
        ▼
run_test_with_run_id(ctx, test, run_id, session_id)
        │
        ├── executor::execute_test_tracked(...)  executor.rs:58
        │     │
        │     ├── headless mode check
        │     │     (if browser_headless override set, toggle Chrome headless)
        │     │
        │     ├── goto test.full_url()           parser.rs:108
        │     │     (merges url_query BTreeMap → ?key=val query string)
        │     │
        │     ├── install_capture                (network req+res body recording)
        │     │
        │     ├── fixture setup steps            parser.rs:FixtureStep
        │     │     (ABORT on failure — marks run Error before entering loop)
        │     │
        │     ├── ReAct loop (max_iterations, default 20)
        │     │     │
        │     │     ├── runs::update_phase(RunPhase::Running{step, action})
        │     │     │
        │     │     ├── build_prompt(test, history, locale)   executor.rs:310
        │     │     │     ├── goal + success_criteria
        │     │     │     ├── available browser actions list
        │     │     │     │   (goto / click / type / read / eval / wait /
        │     │     │     │    exists / count / attr / value / scroll / key /
        │     │     │     │    screenshot_analyze / console / network /
        │     │     │     │    ax_* / expand_observation / clear_state /
        │     │     │     │    wait_new_tab / wait_request)
        │     │     │     └── history (observations truncated to OBS_TRUNCATE_CHARS=800)
        │     │     │
        │     │     ├── call_coding_prompt(prompt)   llm/mod.rs:561
        │     │     │
        │     │     ├── parse_step(raw)              executor.rs:438
        │     │     │     └── strips markdown fences → JSON
        │     │     │           {thought, action_input, done, final_answer}
        │     │     │
        │     │     ├── if done=true → break to evaluate_success
        │     │     │
        │     │     └── inject_session(action_input, session_id)
        │     │           → call_tool("web_navigate", action_input)
        │     │             store full obs in runs registry
        │     │             store truncated obs in history for next prompt
        │     │
        │     ├── evaluate_success(ctx, test, history)  executor.rs:530
        │     │     └── second LLM call: criteria list → {passed, reason}
        │     │
        │     └── fixture cleanup steps
        │           (always run; errors logged, not propagated)
        │
        ├── triage::triage(ctx, test, &result)   (if status != Passed)
        │     └── returns (FailureCategory, analysis_string)
        │
        ├── triage::trigger_auto_fix(...)        (if UiBug or ApiBug)
        │
        ├── store::record_run(test_id, &result)  store.rs:136
        │
        └── runs::update_phase(RunPhase::Complete(result))
```

### Batch runs (`spawn_batch_run`)

```
spawn_batch_run(ctx, test_ids, max_concurrency)
        │
        ├── tokio::sync::Semaphore(max_concurrency)
        │
        └── for each test_id:
              acquire permit → spawn async task
                session_id = "batch_{batch_id}_{idx:02}"
                run_test_with_run_id(ctx, test, run_id, session_id)
                after completion: close dedicated browser tab
              release permit
```

`session_id` is passed through `inject_session()` into every `web_navigate` call
so concurrent batch tests run in isolated browser tabs without cross-contamination.
Maximum 8 concurrent tabs (MCP server enforces this via its own semaphore).

---

## 4. Triage Flow

### 4a. `triage()` — three-step classifier

```
triage(ctx, test, result)                        triage.rs:68
        │
        ├── Step 1 — flakiness history
        │     store::is_flaky(test_id)
        │       → true if <70% pass rate in last 10 runs
        │              (requires at least 1 pass AND 1 fail)
        │     → returns FailureCategory::Flaky immediately
        │
        ├── Step 2 — env quick-check
        │     if result.steps_taken < 2 AND result contains "timeout"
        │       → returns FailureCategory::Env immediately
        │       (no LLM call — Chrome / network infrastructure failure)
        │
        └── Step 3 — LLM classification
              context = last_screenshot + network_log + console_errors + history
              prompt includes i18n::triage_categories_doc(locale)
              LLM outputs JSON: {category, reason, suggested_repo}
              → maps to FailureCategory::{UiBug|ApiBug|Flaky|Env|Obsolete|Unknown}
```

### 4b. `trigger_auto_fix()` — dedup + state machine

```
trigger_auto_fix(test, result, category, run_id)  triage.rs:224
        │
        ├── dedup: pending fix within 30 min?
        │     store::last_pending_fix_age(test_id) < 30 min
        │       → record_auto_fix(outcome="skipped_dedupe") and return
        │
        ├── 3-failure cap: last 3 attempts all outcome=failed?
        │     store::last_n_fix_outcomes(test_id, 3) all == "failed"
        │       → log and return (no new attempt)
        │
        ├── store::record_pending_fix(test_id, run_id)
        │     (outcome = "pending")
        │
        ├── determine repo:
        │     UiBug → "frontend"   (claude_session repo_path)
        │     ApiBug → "backend"
        │
        ├── claude_session::build_bug_prompt(...)
        │     inputs: test name + reason + url + error + network_log + screenshot
        │
        └── std::thread::spawn → claude_session::run_sync(cwd, prompt)
              on completion:
                if exit==0:
                  store::complete_fix(fix_id, outcome="fix_attempted")
                  run verification: run_test() again
                    if Passed  → store::record_verification(outcome="verified")
                    if !Passed → store::record_verification(outcome="regressed")
                else:
                  store::complete_fix(fix_id, outcome="failed")
```

### 4c. Auto-fix outcome state machine

```
pending
  └─→ fix_attempted
        ├─→ verified    (re-run passed)
        └─→ regressed   (re-run failed)
  └─→ failed           (claude_session exit != 0)
  └─→ skipped_dedupe   (30-min dedup guard)
```

---

## 5. Storage

DB path: `{app_data}/memory/test_memory.db`

```sql
-- One row per test execution
CREATE TABLE IF NOT EXISTS test_runs (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    test_id          TEXT    NOT NULL,
    run_id           TEXT    NOT NULL,
    started_at       TEXT    NOT NULL,          -- RFC 3339
    duration_ms      INTEGER,
    status           TEXT    NOT NULL,          -- passed | failed | timeout | error
    failure_category TEXT,                      -- ui_bug | api_bug | flaky | env | obsolete | unknown
    ai_analysis      TEXT,                      -- triage LLM output
    screenshot_path  TEXT,
    history_json     TEXT                       -- full step history (not truncated)
);
CREATE INDEX IF NOT EXISTS idx_tr_test ON test_runs(test_id, started_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tr_run_id ON test_runs(run_id);

-- Per-test learned facts (selector updates, known workarounds, etc.)
CREATE TABLE IF NOT EXISTS test_knowledge (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    test_id    TEXT NOT NULL,
    key        TEXT NOT NULL,                   -- e.g. "selector_login_btn"
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(test_id, key)
);

-- Auto-fix attempt history
CREATE TABLE IF NOT EXISTS auto_fix_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    test_id     TEXT NOT NULL,
    run_id      TEXT NOT NULL,
    started_at  TEXT NOT NULL,
    outcome     TEXT NOT NULL,                  -- see §4c state machine
    fix_summary TEXT
);
CREATE INDEX IF NOT EXISTS idx_afh_test ON auto_fix_history(test_id, started_at);
```

### Key queries

| Function | Query |
|----------|-------|
| `is_flaky(test_id)` | Last 10 runs: count passes + fails → flaky if passes > 0 AND fails > 0 AND pass\_rate < 0.70 |
| `success_rate(test_id, days)` | Runs in last N days grouped by status |
| `recent_runs(test_id, limit)` | `ORDER BY started_at DESC LIMIT ?` |
| `find_run_by_run_id(run_id)` | Unique index lookup — used by `persist_adhoc_run` slow path when in-memory registry has pruned the entry |
| `store_knowledge` / `get_knowledge` | `INSERT OR REPLACE` on UNIQUE(test_id, key) |

---

## 6. Notable Design Decisions

| Decision | Alternative considered | Reason |
|----------|------------------------|--------|
| **ReAct loop over scripted selectors** | Playwright script generation | LLM adapts to UI changes without selector maintenance; `screenshot_analyze` handles dynamic/visual assertions that CSS selectors can't express |
| **Per-test `session_id` for batch isolation** | Sequential batch execution | N=8 concurrent tabs without cross-contamination; each session\_id routes CDP commands to its dedicated tab via `browser::session_switch` |
| **Two-stage evaluation** (executor decides `done=true`, then `evaluate_success()` makes a separate LLM call) | Single-pass done+evaluate | Separation of concerns: the driving agent focuses on navigation; the evaluator focuses on assertion — avoids prompt confusion between "what to do next" and "did we succeed" |
| **`OBS_TRUNCATE_CHARS = 800` in history + `expand_observation` tool** | Full observations in every prompt | Long DOM dumps or network logs fill the context window; 800-char summary keeps prompt short; `expand_observation(obs_id)` retrieves the full content on demand |
| **Fixture setup/cleanup lifecycle** | Inline setup steps in goal text | Explicit abort-on-failure for setup (missing fixture = broken test, not flaky); cleanup always runs regardless of test outcome so browser state stays clean for next run |
| **`persist_adhoc_run` two-tier recovery** (in-memory first, SQLite fallback for pruned runs) | In-memory only | Ad-hoc runs promoted to permanent YAML long after completion; 1-hour TTL prune means the in-memory entry may be gone; SQLite `run_id` unique index is authoritative |
| **`is_flaky` threshold: <70% pass in last 10 runs** | Fixed failure count | Rate-based threshold tolerates intermittent infrastructure noise; requiring at least one pass AND one fail avoids classifying a reliably broken test as flaky |
| **Dedup: skip auto-fix if pending within 30 min** | No dedup | Prevents N parallel failures from spawning N concurrent `claude_session` processes against the same repo |
| **3-failure cap on auto-fix** | Unlimited retries | Avoids infinite repair loops when a bug is beyond the coding agent's reach; operator must intervene manually |
| **`Locale` enum for prompt language** | Always Chinese | International projects need English reasoning; CJK reasoning tokens are longer, raising cost; locale is set per-test in YAML |

---

## 7. Known Limits / Future Work

For the full roadmap see [`docs/test-runner-roadmap.md`](../test-runner-roadmap.md).

### Current limits

- **No incremental YAML discovery** — `load_all()` scans `config/tests/*.yaml` on
  every call; not cached. On large test suites (>100 YAMLs) this is a noticeable
  I/O hit on every `list_tests()` call.

- **`OBS_TRUNCATE_CHARS = 800` is a blunt instrument** — long JSON API responses
  or verbose DOM trees are cut at a character boundary, which may split a token or
  truncate a closing brace. `expand_observation` mitigates this but requires the
  LLM to decide to call it.

- **Two-stage evaluation doubles LLM cost per test** — `evaluate_success()` is a
  separate `call_coding_prompt` round-trip. Cheap success criteria (URL check,
  text presence) could be evaluated locally without a second LLM call.

- **Auto-fix runs in a detached thread** — `trigger_auto_fix` spawns
  `std::thread::spawn`; there is no cancellation, no timeout, and no back-pressure
  if many fixes are queued simultaneously.

- **`persist_adhoc_run` two-tier recovery adds latency** — if the in-memory run
  has been pruned, `find_run_by_run_id()` does a full SQLite row fetch; this is
  acceptable for a rare code path but makes the function's latency non-deterministic.

- **`session_id` injection is shallow** — `inject_session` merges the field at the
  top level of the action JSON object. Nested CDP commands that open new contexts
  (e.g., `wait_new_tab`) do not propagate the session to the child tab.

- **`is_flaky` requires exactly ≥1 pass AND ≥1 fail** — a test that has only ever
  failed (all 10 of 10) is not flagged flaky; it may actually be a dead test rather
  than flaky, but the distinction is not surfaced to the operator.

- **No visual regression baseline** — `screenshot_analyze` uses the LLM's
  open-ended vision judgment; there is no pixel-diff against a stored golden image.
  Visual regressions rely on the goal text being specific enough to catch them.

- **`collect_reply_samples` in context.rs is O(file size)** — not directly
  related to test_runner, but `executor.rs` calls `load_recent_context` at prompt
  build time; per-peer context files grow unboundedly (see `docs/architecture/memory.md`
  §6).
