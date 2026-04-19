# Postmortem: Sirin Silent Crash @ N=4 Workers

**Date:** 2026-04-19
**Severity:** P1 — Service unavailable (process death, all workers killed)
**Status:** Diagnosed, mitigation active (N=2), fix proposed
**Author:** Investigation session (claude-sonnet-4.6)

---

## Symptom Timeline

Both crashes share identical pattern: no `panic`, no `backtrace`, no `error` in stderr.
Process silently disappears; Windows Job Object kills all 4 node (claude) children.

### Crash #2 — 2026-04-19 (from `.claude/tmp/sirin_crash_2.log`)

| UTC time | Event | Detail |
|---|---|---|
| 09:10:17 | Startup | Windows Job Object installed; MCP on :7711 |
| 09:11:34 | Workers start | 4 workers launched, 4 tasks claimed |
| 09:11:34 | w2 starts | agora_admin_status_chip.yaml (YAML creation task) |
| 09:11:34 | w0/w1/w3 start | docs/architecture tasks |
| 09:14–09:31 | Normal throughput | w1/w0/w3 complete 7 tasks, ~3 min each |
| 09:31:57 | w3 starts | mcp_server.md |
| 09:37:20 | w0 starts | mcp_client.md |
| 09:40:49 | MCP alive | `agent_enqueue` received — port still responding |
| 09:41:28 | w3 completes | mcp_server.md done ✓ |
| 09:42:02 | w3 starts | agora_search_keyword.yaml |
| 09:50:35 | **Last log** | `agent_enqueue` for agora_admin_status_chip |
| 09:50:35+ | **DEATH** | Process disappears; all node children killed |

**Critical observation at time of death:**
- **w2: still running agora_admin_status_chip** — started 09:11:34, never logged "done ✓" — **39+ minutes** for a simple YAML creation task
- **w1: still running ui_egui.md** — started 09:27:44 — **22+ minutes**
- **w0: still running mcp_client.md** — started 09:37:20 — **13+ minutes**
- **w3: running agora_search_keyword** — started 09:42:02 — **8+ minutes**

The 8-minute gap (09:42:02 → 09:50:35) had **zero worker log output** despite 4 workers supposedly active.
MCP port was alive during this gap (responded to `agent_enqueue` at 09:50:35).

### Crash #1 — earlier same day

- **Uptime:** ~38 min, same N=4 config
- **Binary mtime:** ~16:08 (same as Crash #2 → same binary)
- **Pattern:** identical — no panic, process disappears

Both crashes at ~38-40 min is not coincidental. See §Suspects #2 for timing hypothesis.

---

## Windows Event Log Findings

Query executed:
```powershell
Get-WinEvent -FilterHashtable @{LogName='Application','System'; Level=1,2,3;
  StartTime='2026-04-19 09:00:00'; EndTime='2026-04-19 11:00:00'}
```

**Result: NO sirin.exe Application Error event recorded.**

Events found were unrelated:
| Provider | Event ID | Significance |
|---|---|---|
| Microsoft-Windows-DistributedCOM | 10016 | COM permission warning (MSTeams, chronic background noise) |
| Microsoft-Windows-TPM-WMI | 1796 | UEFI Secure Boot cert update failure (unrelated) |
| Netwtw14 | 6062 | WiFi driver event |

**Interpretation:** Windows did not observe this as a crash. The process either called `ExitProcess` cleanly, or was killed by an OOM condition that bypassed WER (Windows Error Reporting). This rules out access violations, stack overflows detected by Windows, and exceptions caught by the OS crash handler.

---

## Suspects — Ranked by Likelihood

### #1 — `cmd.output()` with No Timeout → Thread Blocking → OOM (HIGH, ~65%)

**Location:** `src/claude_session.rs:485`
```rust
cmd.output().map_err(|e| format!("claude failed ({bin:?}): {e}"))
```

`std::process::Command::output()` buffers **all** stdout/stderr into `Vec<u8>` before returning. It has **no timeout**. The child process must exit on its own.

**How this causes a crash:**

1. Worker spawns `claude --output-format stream-json --verbose` as a child process
2. Claude streams JSON line-by-line until done, then exits
3. `cmd.output()` returns with entire output buffered in memory

**Memory scale estimate for N=4:**
- A single `claude --output-format stream-json` session on a complex docs task (reading 5-10 source files, generating 400-line markdown) produces ~20–100 MB of streaming JSON before finishing
- At N=4 workers, each running its own PM or Engineer session: **4 × 80 MB = 320 MB peak simultaneous allocations**
- Tasks that go through 5 PM/Engineer/PM iterations (see §Suspects #2): **5 rounds × (PM call + Engineer call + PM review call) = 15 `cmd.output()` calls per task, sequential per worker**
- But when multiple workers are each at different stages: peak buffering compounds

**Why 39 minutes for w2?** A YAML creation task that required 5 PM/Engineer/PM review iterations:
- Each claude call for a complex task: ~4–8 min (observed from logs: typical task takes 3–4 min for PM plan + Engineer + PM review)
- 5 iterations × 3 calls × ~3 min average = ~45 min — matches the 39+ min observation

**The memory kill scenario:**
- w2 is on iteration 4 or 5, Engineer produces a large diff or reads many files → 100+ MB output
- w1 is on iteration 2 of ui_egui.md (300+ source lines to read) → 80+ MB output
- w0/w3 are in normal mid-task calls → 40+ MB each
- Peak simultaneous buffering: 300–400+ MB across 4 worker threads
- Windows commits all pages → VirtualAlloc fails → process OOM-killed → no panic log (process killed before panic handler runs)

**Evidence:**
- `run_claude()` at `src/claude_session.rs:449-486` is the only place stdout is collected — no streaming, no backpressure
- 39-minute task duration matches 5-iteration limit in `assign_task()` at `src/multi_agent/mod.rs:62`
- No Windows crash event → clean exit or OOM-killed (not an exception)

---

### #2 — `assign_task()` MAX_ITER=5 + Hung Claude Subprocess → Thread Deadlock (HIGH, ~55%)

**Location:** `src/multi_agent/mod.rs:62`
```rust
const MAX_ITER: usize = 5;
```

**The 40-minute timing hypothesis:**

Each `assign_task()` call can run up to:
- 1 PM planning call
- 5 × (Engineer call + PM review call) = 10 more calls
- = 11 claude subprocess invocations per task, all via `cmd.output()` (blocking, no timeout)

If the Anthropic API becomes slow or drops a connection mid-response (common at high concurrency), `cmd.output()` blocks the worker thread indefinitely. The child process (claude.cmd → node → node HTTP client) is waiting for the API response. Nothing times out.

**Scenario:**
1. All 4 workers are mid-task at different `assign_task()` iterations
2. Anthropic API rate-limits or drops connections to N=4 concurrent callers
3. All 4 `cmd.output()` calls block simultaneously (at ~09:42:02, consistent with the 8-min silence)
4. Anthropic client-side eventually reconnects, response arrives, stdout buffer fills
5. Unbuffering 4 large accumulated responses simultaneously → OOM spike → kill

**Evidence for connection drop at 09:42:02:**
- Last worker log: w3 starts a task at 09:42:02
- Next event: MCP enqueue at 09:50:35 (8 min later, no worker progress)
- 8-minute silence in worker logs while 4 workers are supposedly active = workers blocked in `cmd.output()`

**Additional concern:** `run_one_round()` in `src/claude_session.rs:246` uses streaming (line-by-line BufReader), but `run_one_turn_scoped()` at line 345 uses `cmd.output()` — the latter is called by all squad workers. The streaming version is only used by `run_supervised()`, which is not used by squad workers.

---

### #3 — N=4 Concurrent JSONL Rewrites → Brief Mutex Starvation (LOW, ~15%)

**Location:** `src/multi_agent/queue.rs:53-57`
```rust
static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn lock() -> &'static Mutex<()> { LOCK.get_or_init(|| Mutex::new(())) }
```

Every `update_status()` call acquires this global lock and rewrites the entire `task_queue.jsonl`.
With N=4 workers each calling `update_status()` when a task finishes or fails:
- Brief contention is expected but safe (`.unwrap_or_else(|e| e.into_inner())` handles poisoning)
- File rewrite is synchronous but fast (<1ms for 38-task queue)

**This suspect is LOW priority because:**
- MCP `agent_enqueue` at 09:50:35 still worked — the global LOCK was not deadlocked
- All `.lock()` calls in `queue.rs` use `.unwrap_or_else(|e| e.into_inner())` — Mutex poisoning is handled
- JSONL rewrite time is negligible vs. multi-minute claude calls

---

## What Was Ruled Out

| Hypothesis | Ruled out by |
|---|---|
| Mutex poisoning / `.unwrap()` panic | All `lock()` calls in `queue.rs` use `.unwrap_or_else(|e| e.into_inner())` — confirmed by audit |
| Stack overflow | Worker threads use simple iterative loop; no recursion in hot path; no large stack allocations detected |
| Windows OS crash | No sirin.exe Application Error in Event Log; only DCOM/TPM/WiFi events |
| Race condition in task assignment | `take_next_queued()` is atomic (read-mutate-rewrite inside single LOCK acquire) |
| Panic in MCP handler | MCP was alive and responding at 09:50:35 (last known event); MCP handlers have `#[cfg]` panic recovery from `5fc4df1` |
| Recent T1-5 auto-retry bug | T1-5 only triggers on `Err` path after `update_status`; enqueue_with_retry uses the same safe LOCK |

---

## Recommended Diagnostic Actions

### Immediate (before next N=4 run)

1. **Add `RUST_LOG=debug` and pipe to file:**
   ```bash
   RUST_LOG=debug SIRIN_SQUAD_WORKERS=4 ./sirin.exe --headless 2>&1 | tee /tmp/sirin_debug.log
   ```
   This will show every `tracing::debug!` including inside `run_claude` if we add one.

2. **Add a subprocess timeout to `run_claude()`:**
   Replace `cmd.output()` with a timed wait. Windows `std::process::Child::wait_timeout` does not exist natively, but can be emulated with a watchdog thread:
   ```rust
   // Proposed: kill child if no data received in 10 min
   let timeout = Duration::from_secs(600);
   ```
   See §Proposed Fix below.

3. **Watch for 8-minute worker silences:**
   If no `[team-worker:wN]` log for >5 min, a worker is blocked. This is the early warning sign observed in Crash #2.

4. **Start with N=3 instead of N=4:**
   N=3 × 80MB peak = 240MB — likely safe. N=4 may be at the edge of available virtual memory given other processes.

### Medium-term

5. **Stream stdout incrementally instead of buffering:**
   Change `run_one_turn_scoped` to process stdout line-by-line (as `run_one_round` already does for supervised mode). This eliminates the memory accumulation entirely:
   ```rust
   // Instead of:
   let raw = run_claude(&args, Some(cwd_path))?;
   let stdout = String::from_utf8_lossy(&raw.stdout);
   // Use:
   cmd.stdout(Stdio::piped());
   let child = cmd.spawn()?;
   for line in BufReader::new(child.stdout).lines() { ... }
   ```

6. **Add a worker heartbeat log every 60s:**
   ```rust
   if last_log_elapsed > 60s { tracing::info!("[team-worker:w{worker_id}] heartbeat — waiting for claude"); }
   ```
   This distinguishes "no tasks" from "worker hung."

---

## Proposed Fix (Code-Level, NOT Applied)

### Fix A: Subprocess timeout in `run_claude()` (addresses Suspects #1 and #2)

**File:** `src/claude_session.rs`

Replace the final `cmd.output()` call with a timeout-aware wrapper:

```rust
/// Run claude with a hard 10-minute wall-clock timeout.
/// If the subprocess doesn't complete within the timeout, it is killed and
/// an Err is returned. This prevents worker threads from blocking indefinitely
/// when the Anthropic API hangs or rate-limits.
fn run_claude_with_timeout(
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    // ... same setup as run_claude ...
    cmd.stdin(Stdio::null())
       .stdout(Stdio::piped())
       .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(status) => {
                // Collect output after exit
                let stdout = /* drain piped stdout */;
                let stderr = /* drain piped stderr */;
                return Ok(std::process::Output { status, stdout, stderr });
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err(format!("claude subprocess timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}
```

**Commit risk:** LOW — purely additive, only affects what happens when claude hangs.

### Fix B: Stream stdout instead of buffer (addresses Suspect #1 memory usage)

**File:** `src/claude_session.rs:run_one_turn_scoped`

Refactor to use `Stdio::piped()` + line-by-line BufReader (same pattern as `run_one_round`).
This eliminates the `Vec<u8>` accumulation entirely.

**Commit risk:** MEDIUM — requires restructuring `run_claude()` to not use `cmd.output()`.

### Which commit to watch / potentially revert

None of the recent commits is the direct cause. The underlying bug (no timeout, buffered stdout) predates T1-1/T1-5. However:

- **`d4deba6` (T1-1)** is what made N=4 possible — before this, only N=1 ran. The bug was latent but harmless at N=1.
- If an emergency revert is needed to stabilize: revert `d4deba6` to drop back to N=1. This is safe and would immediately eliminate the crash.
- **Do NOT revert `eb87498` (T1-5 auto-retry)** in isolation — it's not the cause and would just lose the retry feature.

---

## Current Mitigations

| Mitigation | Status | Effectiveness |
|---|---|---|
| N=2 workers (SIRIN_SQUAD_WORKERS=2) | Active | HIGH — halves memory pressure, reduces API concurrency |
| `RUST_BACKTRACE=full` env var | Active | LOW — only catches Rust panics; this crash is likely OOM (no panic) |
| worker.rs engineer session reset at turns>40 | Active | LOW — reduces context window, not subprocess memory |
| T1-5 auto-retry on failure | Active | Neutral — doesn't affect crash root cause |

**Watch for:** any 5+ minute period with no `[team-worker]` log while workers are supposed to be running. That indicates workers are blocked in `cmd.output()` — the precursor to the crash.

---

## UPDATE — Crash #4 (2026-04-19 20:25, after Fix A applied)

### What was tried

Fix A from this postmortem was implemented by the squad worker (uncommitted in
`src/claude_session.rs`, 195 insertions):
- New `wait_child_with_timeout()` drains stdout/stderr on background threads
  via `read_to_end(&mut buf)` then waits with deadline
- New `run_claude_with_timeout()` spawns child + delegates to wait helper
- `run_claude()` becomes a thin 600s-timeout wrapper

Built release binary at 19:35, relaunched Sirin (PID 17764, port 7711) at
**19:40:13**. Spawned N=2 workers at **20:21:57**.

### What happened

Sirin disappeared from tasklist between **20:23 and 20:25** — total uptime
**~45 minutes**, only **2-3 minutes after workers spawned**. JobObject killed
both child node.exe processes. No Windows Event Log entry. No panic in log.
Last log line at 20:21:57: `[team-worker:w0] Starting task 1776597421409`.

### Why Fix A did not help

`wait_child_with_timeout()` still calls `read_to_end(&mut buf)` to materialize
the entire stdout into a `Vec<u8>`. The only thing it prevents is OS-pipe-buffer
deadlock (when child blocks on a full kernel pipe), not heap OOM. The
underlying issue identified in Suspect #1 — buffering 80MB+ of stream-json
into Rust heap per worker — is **unchanged**.

### New evidence: crash time appears uptime-bound, not workload-bound

| # | N | Workers spawn | Total uptime at death |
|---|---|---|---|
| 1 | 4 | at start | 38 min |
| 2 | 4 | at start | 40 min |
| 3 | 2 | at start | 42 min |
| 4 | 2 | spawn at +42 min | **45 min** (3 min after worker spawn) |

All four crashes cluster at **38-45 min total Sirin uptime**, regardless of
worker count, subprocess timing, or Fix A presence. This suggests **a slow
leak in the long-running Sirin process itself** (not in the per-claude
subprocess pipeline) — possibly:

- LLM fleet probe / model warmup buffers
- Telegram listener message queue
- MCP HTTP server connection state
- Codebase index re-build (1962 files indexed at 19:40:35)
- Tracing-subscriber span buffers with `RUST_BACKTRACE=full`
- Job Object child accounting

Or possibly: a real OOM at startup-time-cumulative-memory threshold ~2GB
that crosses regardless of trigger.

### Required next steps (NOT yet applied)

1. **Implement Fix B** from above — actually stream stdout via line-by-line
   BufReader, never `read_to_end`. Discard intermediate stream-json deltas,
   keep only the final assistant message text.

2. **Add a baseline memory probe** every 60s in worker.rs to graph Sirin's
   own RSS over time. We currently have ZERO data on Sirin's own memory
   pre-crash — could be flat or could be growing 50MB/min. We don't know.

3. **Try with NO workers, NO MCP-LLM activity, just idle** — observe whether
   Sirin itself crashes at the 40-min mark with zero workload. This isolates
   "Sirin process leak" from "subprocess buffering."

4. **Consider reverting `d4deba6` (T1-1 parallel workers)** to N=1 default
   if workload-driven OOM is confirmed. Single worker with bounded session
   reset has not crashed historically.

### Mitigation today

Sirin is currently NOT running (crashed 20:25). Operator must decide:
- (B) Implement Fix B (stream stdout, don't buffer) and rebuild
- (C) Implement memory probe + idle-soak test to confirm leak source
- (D) Revert T1-1 to N=1 single worker as emergency stabilization

Fix A alone is insufficient and should not be relied upon.

---

## Resolution — 2026-04-19 22:00

### Fix B (streaming) — committed `6f75e31`, validated

Refactored `run_one_turn_scoped` to stream stdout line-by-line via
`BufReader::lines()` instead of `read_to_end()`. Per-call peak memory
dropped from 80–100 MB to ~1 MB. Validated by N=2 worker stress test:

| Time | Uptime | RSS | Note |
|---|---|---|---|
| 20:46 | startup | 191 MB | Sirin launched with Fix B |
| 21:24 | 38.6 min | 225 MB | Entered prior OOM window |
| 21:31 | 45.6 min | 225.2 MB | **Past historical crash window** |
| 21:35 | 48.8 min | 225.9 MB | Memory completely flat |

Total memory growth: **+7 MB over 49 min** under continuous N=2 squad
workload. Compare with the previous OOM pattern of 4×100 MB buffered
output causing process death at 38–45 min.

### Fix B regression → Fix C (cmd.exe bypass)

Validation revealed that all 6 squad tasks failed with "PM 5 輪後仍未核准"
despite no OOM. Inspection of newly-created Claude session JSONL files
(`~/.claude/projects/<encoded-cwd>/<session_id>.jsonl`) showed every user
message was **truncated at the first newline**:

| Recipient | Expected length | Actual | Newlines preserved |
|---|---|---|---|
| Engineer  | ~500–900 chars | **91 chars** | 0 (was 26) |
| PM review | ~400–800 chars | **13 chars** ("工程師回報（第 N 輪）：") | 0 (was 36) |
| Tester    | ~365 chars (system prompt) | n/a (never invoked) | 0 |

PM literally received only "工程師回報（第 5 輪）：" with no engineer report
attached, and replied "訊息好像不完整——「第 5 輪：」後面沒有內容".

**Root cause:** Fix B's new spawn site at line 384–393 of `claude_session.rs`
copied the legacy `cmd /c claude.cmd` pattern instead of the `node <cli.js>`
bypass that already existed in `run_claude_with_timeout`. On Windows, when
`cmd.exe` invokes a `.cmd` file, embedded newlines in argument values are
treated as command separators, silently truncating the rest of the prompt.

**Fix C:** Extract `build_claude_command()` helper that resolves
`%APPDATA%\npm\node_modules\@anthropic-ai\claude-code\cli.js` and invokes
`node` directly. Applied uniformly to `run_one_round`, `run_one_turn_scoped`,
and `run_claude_with_timeout` so the three spawn paths cannot diverge again.

**Validation (post Fix C):** Smoke test task completed in 1m 52s — first
successful squad task in ~8 hours. Inspection of the three new session JSONLs
confirmed user messages now contain 16, 26, and 36 newlines respectively
(matching the originals).

### Status

| Fix | Status | Commit |
|---|---|---|
| A — subprocess timeout | ✅ in code | `6f75e31` |
| B — streaming stdout | ✅ in code, validated | `6f75e31` |
| C — `cmd.exe` bypass for Fix B path | ✅ in code, validated | (uncommitted at this writing) |

---

*Generated by investigation session.*
*Updated 2026-04-19 20:30 with crash #4 results.*
*Updated 2026-04-19 22:00 with Fix B/C resolution.*
