# Multi-Agent Squad Architecture

> Source: `src/multi_agent/`
> Cross-references: [../squad-roadmap.md](../squad-roadmap.md),
> [./test_runner.md](./test_runner.md), [./llm.md](./llm.md)

---

## 1. Purpose

Sirin embeds an **autonomous development squad** — three persistent Claude Code sessions
that collaborate to execute coding tasks without human supervision.  Tasks are pushed into
a JSONL queue (via MCP, the UI, or tests); workers continuously pull, execute, and verify.

Each role maintains a persistent `claude --continue` session across tasks, giving it
memory of past work within the same session lifetime.

**Execution flow:**

```
Caller (MCP / UI / test)
        |
        v
  task_queue.jsonl     <- append-only JSONL, %LOCALAPPDATA%\Sirin\data\multi_agent\
        |
        v
  Worker(s) [N OS threads]
        |
        +-- PM session      -- decompose task, write Engineer instructions
        |       |
        |       v
        +-- Engineer session -- read code, make changes, report back
        |       |
        |       v
        +-- PM review (APPROVED / NEEDS_FIX, up to 5 iterations)
        |
        +-- Tester session  -- cargo check; if fail Engineer fixes
```

---

## 2. Module Map

| File | Lines | Responsibility |
|---|---|---|
| `mod.rs` | ~413 | `AgentTeam` orchestration: `assign_task()` loop, `test_cycle()`, global singleton, `TeamStatus` types |
| `queue.rs` | ~341 | JSONL task queue: enqueue, `take_next_queued()` (atomic), `update_status()`, priority sort |
| `session.rs` | ~129 | `PersistentSession`: wraps `claude_session::run_one_turn()`, persists `session_id` + turn count to JSON |
| `worker.rs` | ~147 | Background threads: `spawn_n()`, `run_loop()`, auto-retry on failure, Engineer context reset |
| `roles.rs` | ~58 | System prompts for PM, Engineer, Tester — including VERDICT token format |

---

## 3. Task Lifecycle

### `TaskStatus` enum (`queue.rs:17`)

```rust
pub enum TaskStatus { Queued, Running, Done, Failed }
```

### State transitions

```
       enqueue()
           |
           v
        Queued  <---------- auto-retry re-enqueue (retry_count=1)
           |
           | take_next_queued() [atomic]
           v
        Running
           |
      +----+--------+
      |             |
      v             v
     Done         Failed --> if retry_count==0: re-enqueue [auto-retry]
```

### `TeamTask` fields (`queue.rs:38`)

| Field | Type | Notes |
|---|---|---|
| `id` | `String` | Millisecond timestamp at enqueue time |
| `description` | `String` | Full task text passed to PM |
| `created_at` | `String` | RFC-3339 |
| `status` | `TaskStatus` | Current state |
| `result` | `Option<String>` | PM final review (Done) or error message (Failed) |
| `finished_at` | `Option<String>` | Set when transitioning to Done/Failed |
| `retry_count` | `u8` | 0=original; 1=auto-retry; max 1 retry |
| `priority` | `u8` | 0=urgent, 50=normal (default), 255=lowest |

---

## 4. Persistent Sessions

Each role's session state is stored as a JSON file on disk:

```
%LOCALAPPDATA%\Sirin\data\multi_agent\
    pm.json           <- worker 0 PM   (legacy path)
    engineer.json     <- worker 0 Engineer
    tester.json       <- worker 0 Tester
    w1_pm.json        <- worker 1 PM
    w1_engineer.json  <- worker 1 Engineer
    w1_tester.json    <- worker 1 Tester
    w2_pm.json        <- worker 2 ...
```

**`SessionFile` (`session.rs:17`):**

```rust
struct SessionFile {
    session_id: Option<String>,   // Claude session ID from first run
    role:       String,
    started_at: String,
    turns:      u32,
}
```

**`PersistentSession::send()` flow (`session.rs:63`):**

1. `session_id` is `None` (new session): prepend system prompt to message, call
   `claude_session::run_one_turn(..., continuation=false)`.
2. `session_id` is `Some`: call `run_one_turn(..., continuation=true)` — maps to
   `claude -p <message> --continue`, resuming the existing session.
3. On first turn: capture and store the returned `session_id`.
4. Increment `turns`, call `save()` to write JSON to disk.

**Why `--continue` not `--resume <id>`:**
`--continue` resumes the most recent session for the given working directory.
The stored `session_id` is surfaced in `TeamStatus.resume_cmd` so users can inspect
conversations manually with `claude --resume <id>`.

**Session reset:** `PersistentSession::reset()` deletes the JSON file and clears
in-memory state.  The next `send()` starts a new session.  Worker resets Engineer after
40 turns (`worker.rs:122`) to prevent context window overflow between tasks.

---

## 5. JSONL Queue

### File location

```
%LOCALAPPDATA%\Sirin\data\multi_agent\task_queue.jsonl
```

Each line is one `TeamTask` serialised as JSON.  Append-only for new tasks; mutations
(status updates) rewrite the whole file via `rewrite_unlocked()`.

### Global lock (`queue.rs:53`)

```rust
static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
```

Every queue function acquires this mutex before any disk I/O, serialising all queue
operations across threads within the same process.

### Atomic take (`queue.rs:132`)

```rust
pub fn take_next_queued() -> Option<TeamTask>
```

The read → select → mark-Running → rewrite sequence happens inside one `LOCK` guard.
This prevents two workers from racing to claim the same task.  `next_queued()` is a
non-atomic peek kept for single-worker backwards compat only.

### Priority sort (`queue.rs:137`)

Tasks sorted by `(priority ASC, created_at ASC)` — lowest number wins.
Default `priority = 50`.  Urgent tasks use `enqueue_with_priority(desc, 0)`.

### API summary

| Function | Description |
|---|---|
| `enqueue(desc)` | Append Queued task, priority 50 |
| `enqueue_with_priority(desc, p)` | Append Queued task, explicit priority |
| `enqueue_with_retry(desc, n)` | Internal: re-enqueue failed task with `retry_count=1` |
| `take_next_queued()` | **Atomic**: claim next task, mark Running, return it |
| `next_queued()` | Non-atomic peek — single-worker only |
| `update_status(id, status, result)` | Transition status, write `finished_at` on terminal states |
| `list_all()` | All tasks, newest first |
| `list_by_status(status)` | Filtered view |
| `clear_completed()` | Remove Done/Failed, keep Queued/Running |

---

## 6. Multi-Worker (N Parallel)

### Spawn (`worker.rs:44`)

```rust
pub fn spawn_n(cwd: &str, n: usize)
```

- `static STARTED: AtomicBool` guard — idempotent, second call is a no-op.
- Resets any stale `Running` tasks to `Queued` before spawning (crash recovery).
- Spawns `n` OS threads named `multi-agent-worker-0`, `-1`, …
- Recommended range: 2–4 workers. More than 8 tends to hit Anthropic API rate limits.

### Why OS threads not tokio tasks

`claude_session::run_one_turn()` shells out to the `claude` CLI and blocks for 30–120s.
Tokio async tasks would block the runtime during that wait.  OS threads are the correct
primitive — each worker blocks independently without starving other async work.

### Worker namespacing (`session.rs:118`)

Each worker `w` calls `AgentTeam::load_for_worker(&cwd, w)`, giving it three private
session files: `w{w}_pm.json`, `w{w}_engineer.json`, `w{w}_tester.json`.  Workers never
share session state.  Worker 0 uses legacy non-namespaced filenames for backwards
compatibility with sessions created before T1-1.

### Shared working directory

Each task gets its **own isolated git worktree** at `../sirin-task-{id}/` (T2-4, shipped 2026-04-25).
`worker.rs` calls `create_worktree()` before `assign_task()` and `cleanup_worktree()` after — 
`team.engineer.cwd` and `team.tester.cwd` are updated to the worktree path.
Git-stage conflicts between concurrent workers are eliminated.

---

## 7. PM Verdict + Auto-retry

### VERDICT token (`roles.rs:21`, `mod.rs:105`)

The PM system prompt requires every review reply to end with exactly one of:

```
<<<VERDICT: APPROVED>>>
<<<VERDICT: NEEDS_FIX: <one-line reason>>>
```

`assign_task()` checks this token first.  A legacy keyword fallback handles pre-T1-6
sessions:

```rust
let approved = review.contains("<<<VERDICT: APPROVED>>>")
    || (review.contains("核准") && !review.contains("<<<VERDICT: NEEDS_FIX"));
```

### Iteration loop (`mod.rs:79`)

```
for iter in 0..5 (MAX_ITER):
    Engineer.send(task + PM plan [+ PM feedback if iter > 0])
    review = PM.send(engineer output)
    if APPROVED -> return Ok(review)
    last_review = review   <- fed back to Engineer next iteration
return Err(...)            <- exhausted iterations
```

Engineer retains context **within** a task (T1-4): no reset between iterations.
Cross-task context bloat is managed by the 40-turn worker reset.

### Auto-retry on failure (T1-5, `worker.rs:99`)

When `assign_task()` returns `Err`:

1. Task marked `Failed`.
2. If `retry_count == 0`: new task enqueued with description prefixed `[auto-retry]`
   and `retry_count = 1`.
3. If `retry_count == 1`: no further retry — permanent failure.

Every task gets exactly **one second chance**.

---

## 8. Priority Lanes (T1-7)

`TeamTask.priority: u8` stored in JSONL.
`#[serde(default = "default_priority")]` = 50 for tasks written by older code.

| Value | Meaning |
|---|---|
| 0–10 | Urgent — jump the queue |
| 50 | Normal (default) |
| 100–255 | Background / low-priority |

`take_next_queued()` sorts by `(priority ASC, created_at ASC)`.  A priority-0 task
enqueued after 10 normal tasks is picked before any of them.

---

## 9. Known Limits / Future Work

### ~~Shared `cwd` — no worktree isolation~~ (T2-4 shipped 2026-04-25)

✅ Each task now runs in its own `../sirin-task-{id}/` worktree.  Git-stage conflicts
between concurrent workers are eliminated.  See `src/multi_agent/worktree.rs`.

### No dashboard UI

`TeamStatus` (session IDs, turn counts, resume commands) is exposed via MCP
`agent_team_status` and logged, but there is no live egui panel showing per-worker
session state.

### No auto-decomposition

Tasks are pushed verbatim.  PM decomposes them conversationally inside the Claude
session — not structurally (no child tasks in the queue).

### Engineer context bound is heuristic

The 40-turn threshold was chosen empirically.  A 5-iteration task uses 5 turns; a long
debug loop may use 15+.  A task needing >40 Engineer turns will be reset mid-task.

### Single auto-retry, no backoff

Failed tasks get exactly one retry with no delay and no modified strategy.  Systematic
failures (wrong API key, impossible task) will fail again.

### JSONL rewrite on every mutation

`update_status()` rewrites the entire JSONL on every status change — O(n) I/O.
Acceptable for tens of tasks; a SQLite backend would be needed at larger scale.

### No task cancellation API

Once `Running`, a task can only reach `Failed` via natural completion.  Cancellation
requires killing the worker thread or restarting Sirin.
