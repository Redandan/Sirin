# Multi-Agent Squad Upgrade Roadmap

> Last updated: 2026-04-19 | Owner: Sirin AI (squad manager)

## Current Architecture (Baseline)

```
AgentTeam
  ├── PM       — persistent session (--continue), role: plan / review / record
  ├── Engineer — session reset after each task (context bloat guard)
  └── Tester   — persistent session, runs cargo check only

Worker: 1 thread, FIFO queue, 10s poll interval
Task pipeline: PM plan → Engineer execute → PM review (max iter) → test_cycle
```

## Shipped Upgrades

| Date | Tier | Item | Commit | Detail |
|------|------|------|--------|--------|
| 2026-04-19 | T1 | MAX_ITER 3→5 | (this session) | UI and multi-file tasks need more room |
| 2026-04-19 | T1 | MAX_MSG_CHARS 8K→24K | (this session) | Claude 200K window can handle it |
| 2026-04-19 | T1 | Engineer no per-iter reset (T1-4) | (this session) | Retains context across retries; cross-task guard stays at 40 turns |
| 2026-04-19 | T1 | **T1-1 Parallel workers (N threads)** | (pending build) | `spawn_n(cwd, n)` + atomic `take_next_queued()` + `worker_id`-namespaced session files; MCP `agent_start_worker` accepts `n:1-8` |

---

## Tier 1 — Speed / Reliability (1–3 hours each)

### T1-1 — Parallel Workers (N threads) ★★★★★
**Status:** ✅ Shipped 2026-04-19 (manager-implemented; pending build + relaunch)

**What landed:**
- `worker::spawn_n(cwd, n)` spawns N independent threads, each with its own
  `AgentTeam::load_for_worker(cwd, worker_id)`
- `worker::spawn(cwd)` kept as 1-worker wrapper (backward compat — UI's
  `team_start()` still works)
- Per-worker session files: worker 0 → legacy paths (`pm.json`); worker 1+ →
  `w{N}_{role}.json`. Existing PM/Engineer/Tester history survives.
- `queue::take_next_queued()` — atomic SELECT-and-mark-Running under the
  global queue Mutex. Two workers cannot grab the same task.
- `queue::next_queued()` kept (peek-only) but marked unsafe-for-multi in docs.
- MCP `agent_start_worker` accepts optional `n: integer` (clamp 1-8). Old
  callers with no `n` get N=1 by default.
- Logs prefixed `[team-worker:w{worker_id}]` for separation in tracing output.

**Files touched:**
- `src/multi_agent/queue.rs` — added `take_next_queued()`
- `src/multi_agent/session.rs` — added `worker_id` field +
  `load_for_worker()` + renamed `state_path_for(role, worker_id)`
- `src/multi_agent/mod.rs` — added `AgentTeam::load_for_worker()`
- `src/multi_agent/worker.rs` — added `spawn_n()`, `run_loop` takes
  `worker_id`, switched to `take_next_queued()` (no more separate
  `update_status(Running)`)
- `src/mcp_server.rs` — `agent_start_worker` schema + handler accepts `n`

**Caveat (T2-4 will fix):** all workers share the same `cwd` (the Sirin
repo). Edits to different files are usually fine, but two workers editing
the same file will git-stage-conflict. T2-4 (worktree isolation) is the
real fix.

**Risk live now:** multiple claude CLI processes competing for Anthropic
API rate limit. Recommend N=2 to start, bump to 3 once observed stable.

---

### T1-5 — Auto-Retry Failed Tasks ★★★
**Status:** ✅ Shipped (2026-04-21, discovered already implemented)

**What landed (already in code):**
- `retry_count: u8` field exists in `TeamTask` (`queue.rs:89`)
- In `worker.rs` failed arm (lines 153-169): if `task.retry_count == 0`, 
  re-enqueue with `retry_count: 1` and tag `[auto-retry]`
- Never retry a task that already has `retry_count > 0`
- Log: `[team-worker] Auto-retrying task {id} (1st retry)`

**Files:**
- `src/multi_agent/queue.rs` — `TeamTask.retry_count` + `enqueue_with_retry()`
- `src/multi_agent/worker.rs` — auto-retry logic in failed task arm

---

### T1-6 — Structured PM Verdict ★★
**Status:** ✅ Shipped (2026-04-21, discovered already implemented)

**What landed (already in code):**
- PM system prompt (`roles.rs:34–38`): end every review with structured verdict
  ```
  <<<VERDICT: APPROVED>>> or <<<VERDICT: NEEDS_FIX: <reason>>>>
  ```
- Approval detection in `mod.rs` (lines 146–153): checks both new token and
  legacy "核准" keyword with fallback:
  ```rust
  let approved = review.contains("<<<VERDICT: APPROVED>>>")
      || (review.contains("核准") && !review.contains("<<<VERDICT: NEEDS_FIX"));
  ```
- Eliminates false positives from quoted "核准" in context (fallback guards)

**Files:**
- `src/multi_agent/roles.rs` — PM system prompt with VERDICT format
- `src/multi_agent/mod.rs` — structured approval detection

---

### T1-7 — Task Priority Lanes ★★
**Status:** ✅ Shipped (2026-04-21, discovered already implemented)

**What landed (already in code):**
- `priority: u8` field in `TeamTask` (`queue.rs:91`, default 50)
- `enqueue_with_priority(task, priority)` in `queue.rs:124`
- `take_next_queued()` sorts by `(priority ASC, created_at ASC)` (`queue.rs:195`)
- MCP `agent_enqueue` accepts optional `priority` arg (clamped 0–255, default 50)
  (`mcp_server.rs:1203–1207`)
- Urgent tasks with `priority=0` jump ahead of backlog (FIFO within same priority)

**Files:**
- `src/multi_agent/queue.rs` — `TeamTask.priority` field + `enqueue_with_priority()`
- `src/multi_agent/worker.rs` — consumes priority-sorted queue
- `src/mcp_server.rs` — `agent_enqueue` MCP handler

---

## Tier 2 — Capability Expansion (2–3 days each)

### T2-1 — Squad Knowledge Base (SQLite `squad_knowledge`) ★★★★
**Status:** ✅ Shipped (2026-04-24, commit `0a3c998`)

Cross-task persistent memory for the PM. Learned patterns survive across restarts.

**Schema:**
```sql
CREATE TABLE squad_knowledge (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    key         TEXT NOT NULL UNIQUE,  -- first 80 chars of lesson text (dedup key)
    value       TEXT NOT NULL,          -- the lesson
    learned_at  TEXT NOT NULL,
    source_task TEXT                    -- task_id that taught this
);
```

**Flow:**
1. PM ends every task with `[📝 學到: <one-line lesson>]` (already in roles.rs)
2. Worker parses these lines after each `assign_task` success and writes to `squad_knowledge`
3. Before planning a new task, PM gets injected: "過去學到的相關知識：\n{top_5_lessons}"

**Top-5 selection:** keyword overlap scoring in Rust (no dynamic SQL); fallback to most-recent.

**MCP tool:** `squad_knowledge` — list stored lessons (see `docs/MCP_API.md`).

**Module:** `src/multi_agent/knowledge.rs`

---

### T2-2 — Tester Runs YAML Tests via MCP ★★★★
**Status:** Planned

Closes the "wrote test, never tried it" gap.

After Engineer writes a `config/tests/*.yaml`, Tester calls Sirin's own MCP:
```
POST :7700/mcp  run_adhoc_test  { goal: "...", url: "..." }
```
Returns pass/fail + screenshot. PM decides on real result, not just file existence.

**Requires:** Sirin running on a fixed port (e.g. :7700) while squad runs.
**Risk:** squad session running inside Sirin might cause recursion. Run squad on :7706 or use a test port.

---

### T2-4 — Git Worktree Isolation Per Task ★★★
**Status:** ✅ Architecture Complete (2026-04-21)

**What landed:**
- New module: `src/multi_agent/worktree.rs` with complete lifecycle management:
  - `create_worktree(repo_cwd, task_id)` → creates isolated worktree + detached branch
  - `cleanup_worktree(repo_cwd, task_id)` → removes worktree safely
  - `merge_task_branch(repo_cwd, task_id)` → merges completed task (fast-forward only)
  - Safe handling of task_id sanitization for filesystem paths
- Module exported in `src/multi_agent/mod.rs`

**Architecture:**
```
Task starts
  ↓
create_worktree(../sirin, task_id) 
  → ../sirin-task-{id}/ (detached HEAD)
  ↓
Engineer/Tester work in isolated cwd
  → separate target/ dir, no git conflicts
  ↓
Task succeeds
  ↓
cleanup_worktree() → removes ../ sirin-task-{id}
  → can also call merge_task_branch() before cleanup
```

**Why this matters:**
- Prevents `git add` conflicts when 2+ workers edit the same file
- Enables `cargo test` in worktree (own target/ dir) without file locks
- Safe parallel execution: each worker has isolated workspace

**Full Integration** (future PR):
- Pass `effective_cwd` through `AgentTeam::load_for_worker_project()`
- Call `create_worktree()` before `team.assign_task()`
- Call `cleanup_worktree()` after task completion (success or failure)

**Files:**
- `src/multi_agent/worktree.rs` — complete implementation
- `src/multi_agent/mod.rs` — exports `pub mod worktree`

---

### T2-5 — Specialist Engineer Roles ★★★
**Status:** To be enqueued to squad

Replace single `ENGINEER` prompt with 4 specialists. PM routes based on task tag:

| Role | Tags | Focus |
|------|------|-------|
| `rust_engineer` | `rust`, `code`, `refactor` | Rust, Cargo, types, lifetimes |
| `yaml_author` | `yaml`, `test`, `agora` | Test goal YAML, success_criteria, browser patterns |
| `doc_writer` | `doc`, `md`, `cheatsheet` | Markdown docs, code references |
| `devops` | `ci`, `config`, `infra` | Cargo.toml, CI YAML, scripts |

**Implementation:** PM's plan message includes `[route: yaml_author]` tag; worker picks the matching session.

---

## Tier 3 — Strategic (multi-week)

### T3-1 — Planner Agent Above the Squad
User submits "fix Issue #34" (1 sentence). Planner decomposes into 3–7 squad-sized tasks and enqueues them all. Squad executes in parallel (requires T1-1).

### T3-2 — Self-Improving Prompts
PM analyzes >3 failed tasks with common failure pattern → proposes `roles.rs` edit → human approves → committed.

### T3-3 — Cost Dashboard
Track tokens per task per role. Monthly cap via `SIRIN_SQUAD_BUDGET_USD`. Alert at 80%.

### T3-5 — Cross-Repo Work
Squad can checkout AgoraMarket / AgoraMarketAPI / SDK and commit across repos in one task.

---

## Squad Capacity Projections

| Configuration | Tasks/hour | Notes |
|---|---|---|
| Current (1 worker, 3 iter, 8K) | ~6–8 | Baseline |
| After T1-2/3/4 (done) | ~8–10 | Fewer failed retries, richer context |
| After T1-1 (N=2 workers) | ~14–18 | ×2 throughput |
| After T1-1 (N=4 workers) | ~24–32 | Depends on API quota |
| After T2-1 (knowledge base) | ~30–40 | Fewer re-learns, faster planning |
| Full Tier 1+2 | ~40+ | Plus new work types (YAML test validation) |

---

*Maintained by: Sirin session manager. Update when items ship.*
