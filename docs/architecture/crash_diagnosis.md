# Sirin Silent Crash — Diagnosis Report

> **Incident:** Two silent crashes at N=4 workers, both ~40 min after startup.  
> **Analyzed:** Crash #2 (sirin_crash_2.log, 48 lines).  
> **Date:** 2026-04-19

---

## 1. Windows Event Log Findings

**Query window:** Application + System logs, Level=Error/Critical,  
09:00–10:10 UTC (17:30–18:00 local).

**Result: No entries found.**

This rules out:
- Unhandled C++ / SEH exception (would generate WER entry)
- Stack overflow (0xC00000FD would appear in System log)
- .NET / CLR crash

The absence of Event Log entries means the process either **exited cleanly** (voluntary or Job Object cascade) or was **killed at OS level below WER's detection threshold** (e.g., OOM Hard-Kill or Job Object termination by parent).

---

## 2. Crash Log Timeline Analysis

### Full timeline (Crash #2)

| UTC Time     | Event |
|--------------|-------|
| 09:10:17     | Sirin started; Job Object installed |
| 09:11:34     | 4 workers spawned; tasks assigned |
| 09:11:34     | **w0**: researcher.md · **w1**: telegram.md · **w2**: agora_admin_status_chip · **w3**: persona.md |
| 09:14–09:27  | w1, w3, w0 complete tasks rapidly (3–7 min each); pick up new tasks |
| 09:27:44     | w1 starts ui_egui.md |
| 09:37:20     | w0 starts mcp_client.md |
| 09:40:49     | External MCP enqueue (user adds token-usage task) |
| 09:42:02     | w3 starts agora_search_keyword.yaml — **last worker START logged** |
| 09:50:35     | External MCP enqueue (last ANY log line) |
| **~09:51+**  | **Process exits silently — no further log output** |

### Worker state at crash time

| Worker | Task | Running duration at crash |
|--------|------|--------------------------|
| w0     | mcp_client.md (started 09:37:20) | ~14 min — no "done" logged |
| w1     | ui_egui.md (started 09:27:44) | **~23 min** — no "done" logged |
| w2     | agora_admin_status_chip (started **09:11:34**) | **~39 min** — no "done" logged |
| w3     | agora_search_keyword (started 09:42:02) | ~9 min — no "done" logged |

### The 8-minute gap

From `09:42:02` (last worker START) to `09:50:35` (last MCP log) = **8 minutes with zero worker activity**.  
Typical task duration in this run: 3–7 minutes.  
At 09:42:02 all 4 workers were mid-task (no idle 10s polls logged).  
This means **all 4 worker threads were blocked** — none completed a task or logged anything for 8+ minutes before the process died.

**Critical anomaly:** Worker w2 ran the *same* task for **39 minutes** — 5–13× longer than any other task in the session. This is the likely trigger.

---

## 3. Ranked Root Causes

### #1 — Claude CLI subprocess hang + no timeout (probability: ~55%)

**What happens:**  
`run_claude()` calls `cmd.output()` — a synchronous blocking call with **no timeout**.  
If the Claude CLI subprocess hangs (network stall, Anthropic API rate-limit with no retry, or the CLI is waiting on an interactive prompt that never comes), `cmd.output()` blocks the worker thread **indefinitely**.

**Evidence:**
- w2 blocked for 39 min on a task that similar workers finish in 3–7 min.
- w1 also blocked 23 min without completion.
- Once 2+ of the 4 worker threads are hung in `cmd.output()`, the Claude processes pile up (each hold a `Stdio::piped()` stdout handle + Node.js memory).
- With 4 workers potentially spawning 4 hung Node.js processes simultaneously, available memory and file descriptors are consumed.

**File:** `src/claude_session.rs:485`
```rust
cmd.output().map_err(|e| format!("claude failed ({bin:?}): {e}"))
// ^^^^ No timeout — hangs forever if subprocess stalls
```

**Also in `run_one_round` (used by supervised sessions):** `child.wait()` on line 302 — same issue.

**Recommended fix:**
```rust
// Spawn child + watchdog thread that kills after N minutes
let mut child = cmd.spawn()?;
let child_id = child.id();
let deadline = Duration::from_secs(10 * 60); // 10 min max per turn
let handle = std::thread::spawn(move || {
    std::thread::sleep(deadline);
    // Kill the child process by PID if still running
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &child_id.to_string()])
        .output();
});
let output = child.wait_with_output()?;
drop(handle); // cleanup watchdog if done in time
```

---

### #2 — Resource exhaustion → OS kills Job Object (probability: ~30%)

**What happens:**  
Sirin installs a Windows Job Object at startup so all child processes (Node.js) die when Sirin exits. The flip side: if Windows kills the Job Object (due to memory/handle limit), **all children die simultaneously** and then Sirin's stdout pipe reads return EOF → Sirin may exit cleanly.

**Evidence:**
- "所有 node subprocess 同時死亡 (Job Object)" matches Job Object termination exactly.
- No WER event = not an unhandled exception; consistent with OS-level kill.
- 4 workers × potentially 3 PM/Engineer/Tester Claude sessions active = up to **12 concurrent Node.js processes** at peak (each ~150–200 MB) = **1.8–2.4 GB** for subprocesses alone, plus Sirin itself + Chrome.

**Recommended fix:**
- Enforce a max-concurrent-subprocess limit in `run_claude()`.
- Add a subprocess pool / semaphore: at most `N_WORKERS × 2` Claude CLI processes at once.

---

### #3 — Orphaned handles prevent subprocess EOF (probability: ~10%)

**What happens:**  
`run_one_round` (supervised sessions) uses `Stdio::piped()` + `BufReader::new(stdout).lines()`.  
If the child process forks additional Node.js worker processes that **inherit the pipe handle**, the pipe stays open until all descendants exit. The `BufReader` loop never reaches EOF → `child.wait()` blocks.

**Evidence:**
- Claude CLI (Node.js) may spawn child workers that inherit stdio pipes.
- This would cause `run_one_round` to hang indefinitely, similar to #1.

**Recommended fix:**  
Use `Stdio::null()` for stdin (already done) and explicitly set `HANDLE_INHERITANCE = false` on the stdout pipe, or add a timeout as in Fix #1.

---

## 4. Recommended Fixes (Ranked by Impact)

| Priority | Fix | File | Effort |
|----------|-----|------|--------|
| **P0** | Add 10-min subprocess timeout to `run_claude()` + `run_one_round()` | `claude_session.rs` | Medium |
| **P1** | Semaphore: max `N×2` concurrent Claude CLI processes | `claude_session.rs` or `worker.rs` | Medium |
| **P2** | Per-worker task timeout: if `assign_task()` > 15 min, abort + mark Failed | `worker.rs` | Small |
| **P3** | Watchdog thread in `run_loop`: log a heartbeat every 2 min, detect stuck workers | `worker.rs` | Small |

---

## 5. Code Audit Results

### Mutex `.lock()` audit — CLEAN ✅

All `.lock()` calls in `multi_agent/` use `.unwrap_or_else(|e| e.into_inner())`:

| File | Call sites | Pattern |
|------|-----------|---------|
| `queue.rs` | Lines 77, 95, 117, 133, 152, 168, 181, 232, 248, 257, 281 | ✅ `unwrap_or_else` |
| `mod.rs` | Line 225 | ✅ `unwrap_or_else` |

**Mutex poisoning is NOT the crash cause.** No `.unwrap()` on any Mutex in `multi_agent/`.

### Recursion / stack overflow audit — CLEAN ✅

No recursive functions in `src/multi_agent/`. All loops are iterative.  
`assign_task()` uses a `for iter in 0..MAX_ITER` loop.  
Stack overflow is ruled out (also confirmed by no Event Log entry for 0xC00000FD).

### File handle lifecycle — ACCEPTABLE

Per worker task iteration:
- `task_queue.jsonl`: `fs::read_to_string` → `fs::write` — opened/closed per call ✅
- Session `.json`: `fs::read_to_string` + `fs::write` — opened/closed per call ✅
- Claude subprocess: 1× `stdout` pipe — held open until `cmd.output()` returns

**Peak fd estimate at N=4:** 4 workers × 1 open subprocess pipe = 4 FDs.  
If subprocesses hang: 4 hung pipes + accumulated retries = potentially 10–20 FDs.  
Windows limit is 2048 per process — fd exhaustion alone is unlikely to crash.

---

## 6. Current Mitigations + Watch-For

### In effect (N=2, RUST_BACKTRACE=full)

| Mitigation | Effect |
|------------|--------|
| N=2 workers instead of N=4 | Halves peak subprocess count; reduces memory pressure |
| `RUST_BACKTRACE=full` | Captures Rust panic backtraces to stderr (helpful if panic occurs) |
| Engineer context reset at >40 turns | Prevents `--continue` history bloat from growing session files |
| `take_next_queued()` atomic op | Prevents double-assignment at N>1 |

### What to watch for

1. **Any task running > 15 min** — immediately suspicious for subprocess hang.
2. **Worker logs going silent for > 5 min** while queue shows tasks Running — all workers blocked.
3. **Memory growth** in Task Manager: if Sirin + children exceed 3 GB, abort before OOM.
4. **Event ID 10016 or 1000** in Event Log after a crash — would confirm WER capture.
5. **`sirin_relaunch.log`** timestamp vs last crash log timestamp — gap indicates how long the process was dead before restart.

### Next diagnostic step

To confirm Hypothesis #1 (subprocess hang), add this to `run_loop` temporarily:

```rust
tracing::info!(target: "sirin",
    "[team-worker:w{worker_id}] Calling Claude CLI for task {}", task.id);
// ... call assign_task ...
tracing::info!(target: "sirin",
    "[team-worker:w{worker_id}] Claude CLI returned for task {}", task.id);
```

If the second log never appears after the first, `cmd.output()` is hanging.
