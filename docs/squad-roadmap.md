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
**Status:** To be enqueued to squad

- Add `retry_count: u8` field to `TaskItem` in `queue.rs`
- In `worker.rs` failed arm: if `task.retry_count == 0`, re-enqueue with `retry_count: 1` and tag `[auto-retry]`
- Never retry a task that already has `retry_count > 0`
- Log: `[team-worker] Auto-retrying task {id} (1st retry)`

---

### T1-6 — Structured PM Verdict ★★
**Status:** To be enqueued to squad

- Change PM system prompt (`roles.rs`): end every review with a structured verdict block:
  ```
  <<<VERDICT: APPROVED>>> or <<<VERDICT: NEEDS_FIX: <reason>>>>
  ```
- Change approval detection in `mod.rs` from substring match to:
  ```rust
  let approved = review.contains("<<<VERDICT: APPROVED>>>");
  ```
- Eliminates false positives from quoted "核准" in context.

---

### T1-7 — Task Priority Lanes ★★
**Status:** To be enqueued to squad

- Add `priority: u8` field to `TaskItem` (default 50; 0=highest, 255=lowest)
- `enqueue()` MCP arg accepts optional `priority`
- `next_queued()` ORDER BY priority ASC, created_at ASC
- Useful for: urgent hotfix jumps the YAML-writing backlog

---

## Tier 2 — Capability Expansion (2–3 days each)

### T2-1 — Squad Knowledge Base (SQLite `squad_knowledge`) ★★★★
**Status:** Planned

Cross-task persistent memory for the PM. Learned patterns survive across restarts.

**Schema:**
```sql
CREATE TABLE squad_knowledge (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    key         TEXT NOT NULL UNIQUE,  -- e.g. "flutter_headless_always_false"
    value       TEXT NOT NULL,          -- the lesson
    learned_at  TEXT NOT NULL,
    source_task TEXT                    -- task_id that taught this
);
```

**Flow:**
1. PM ends every task with `[📝 學到: <one-line lesson>]` (already in roles.rs)
2. Worker parses these lines after each `assign_task` success and writes to `squad_knowledge`
3. Before planning a new task, PM gets injected: "過去學到的相關知識：\n{top_5_lessons}"

**Top-5 selection:** simple LIKE match on task description keywords.

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
**Status:** Planned (prerequisite for safe T1-1 parallel)

Each task: `git worktree add ../sirin-w{id} -b task/{task_id}`
- Engineer works in the worktree (own target/ dir → cargo test allowed)
- PM merges to main on signoff: `git merge --ff-only task/{task_id}`
- Cleanup: `git worktree remove ../sirin-w{id}`

Unblocks: running `cargo test` (not just `cargo check`) in Tester.

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
