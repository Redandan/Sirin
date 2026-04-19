# claude_session Architecture
> Source: `src/claude_session.rs`
> Cross-references: [multi_agent.md](./multi_agent.md), [mcp_server.md](./mcp_server.md)
> Postmortem: `docs/postmortem/2026-04-19-silent-crash.md`

## 1. Purpose

`claude_session` is Sirin's interface to the Claude Code CLI (`claude -p`).
It spawns Claude as an external subprocess, drives it non-interactively
(print mode, no API key required — uses the operator's Max plan), and returns
the output to callers.

Three call patterns are supported:

| Pattern | Entry point | Used by |
|---|---|---|
| Fire-and-forget | `run_sync` / `run_async` | One-shot bug-fix tasks |
| Single turn (squad) | `run_one_turn` → `run_one_turn_scoped` | Multi-agent squad workers |
| Supervised loop | `run_supervised` | Agentic loops with auto-approval or consultant |

## 2. Module Map

`src/claude_session.rs` is a single file (~943 lines).

| Section | Lines | Responsibility |
|---|---|---|
| `SessionResult` | 30–34 | Return type for sync/async calls |
| `run_sync` / `run_async` | 39–70 | Blocking + background one-shot spawns |
| `build_bug_prompt` | 73–97 | Prompt builder for browser-test failures |
| `SupervisionPolicy` / `SupervisionEvent` | 105–132 | Types for supervised mode |
| `consult` | 143–162 | Spawn a consultant session to answer a question |
| `run_supervised` | 173–240 | Outer supervision loop (up to 5 rounds) |
| `run_one_round` | 247–309 | Inner streaming loop used by supervision |
| `looks_like_question` | 312–320 | Heuristic: does this text look like a pause? |
| `run_one_turn` | 331–337 | Thin wrapper → `run_one_turn_scoped(None)` |
| `run_one_turn_scoped` | 346–454 | **Squad worker entry point — Fix B lives here** |
| `extract_assistant_text` | 456–462 | Parse assistant text blocks from stream-json |
| `repo_path` | 465–481 | Resolve well-known repo paths by alias |
| `claude_bin` | 484–505 | Locate `claude` / `claude.cmd` on PATH |
| `run_claude` | 510–512 | Thin 600s-timeout wrapper |
| `run_claude_with_timeout` | 524–569 | **Fix A — timed subprocess + OS-level pipe drain** |
| `wait_child_with_timeout` | 571–616 | Helper: poll child, kill on deadline |
| `resolve_claude_node_script` | 618–640 | Resolve Node.js entry point for `claude.cmd` |
| `cli_available` / `cli_version` | 643–651 | Probe CLI availability |

## 3. Run Path Taxonomy

Two distinct code paths handle stdout, chosen by caller:

```
run_sync / run_supervised
    └── run_claude()                   ← Fix A path
            └── run_claude_with_timeout()
                    └── wait_child_with_timeout()
                            read_to_end() on background threads
                            poll child.try_wait() with deadline
                            kill() on timeout

run_one_turn / run_one_turn_scoped
    └── cmd.spawn() inline             ← Fix B path
            BufReader::lines() stream
            watchdog thread (600s kill)
```

The paths differ intentionally:
- **Fix A path** (`run_claude`) is used by `run_sync`, `consult`, and
  `run_one_round` (the supervision inner loop). It materialises stdout but
  adds a timeout and proper pipe draining to prevent OS-buffer deadlocks.
- **Fix B path** (`run_one_turn_scoped`) never buffers stdout at all.
  It is used by all squad worker calls where the 80–100 MB stream-json
  payload would otherwise cause OOM at N≥2 concurrency.

## 4. Fix B — Streaming Refactor (core subject of this doc)

### Background

Before commit `6f75e31`, `run_one_turn_scoped` used `run_claude()` which
called `cmd.output()`. `cmd.output()` buffers **all** of stdout into a
`Vec<u8>` before returning.

Problem: Claude's `--output-format stream-json` generates 20–100 MB of
streaming JSON for a complex multi-file session. At N=4 squad workers, each
making multiple sequential calls per task:

```
4 workers × 80 MB peak stdout buffer = 320 MB simultaneous heap allocation
```

With 5 PM/Engineer/review iterations per task (observed: w2 ran 39 min),
the cumulative heap pressure caused a silent OOM kill — no Windows Event Log
entry, no Rust panic, just process death. See postmortem for full timeline.

### What Fix B does

`run_one_turn_scoped` now spawns the child directly and reads its stdout
**one line at a time** via `BufReader::lines()`:

```rust
// Before (Fix B — OLD):
let raw    = run_claude(&args, Some(cwd_path))?;
let stdout = String::from_utf8_lossy(&raw.stdout);
for line in stdout.lines() { ... }

// After (Fix B — NEW):
cmd.stdout(Stdio::piped()).stderr(Stdio::null());
let mut child = cmd.spawn()?;
let stdout    = child.stdout.take().ok_or("no stdout")?;

for line in BufReader::new(stdout).lines() {
    let Ok(line) = line else { continue };
    let Ok(val)  = serde_json::from_str::<serde_json::Value>(&line) else { continue };
    match val["type"].as_str() {
        Some("assistant") => {
            if let Some(t) = extract_assistant_text(&val) { output = t; }
        }
        Some("result") => {
            if let Some(r) = val["result"].as_str()     { output     = r.to_string(); }
            if let Some(s) = val["session_id"].as_str() { session_id = s.to_string(); }
        }
        _ => {}
    }
    // `line` and `val` drop here — freed immediately
}
```

**Key properties after Fix B:**

| Property | Before | After |
|---|---|---|
| Peak per-call heap | ~80–100 MB (full stream-json) | ~max(one line) + len(final output) |
| N=4 peak total | ~320 MB | < 1 MB (+ final outputs) |
| Stderr | Captured into Vec | `Stdio::null()` — dropped at OS level |
| Data kept | Everything | Only: final assistant text + `session_id` |

Stderr is intentionally dropped at the OS level (`Stdio::null()`). Callers
never inspect it, and capturing it would re-introduce buffering pressure.

### Watchdog thread

Because `run_one_turn_scoped` no longer goes through `wait_child_with_timeout`,
it needs its own timeout mechanism. A watchdog thread is spawned alongside
the child:

```
main thread: BufReader::lines() loop (may block on slow network)
watchdog:    sleep loop → kill child after 600s

When BufReader hits EOF (normal exit or kill):
    main thread: exits loop, signals watchdog via AtomicBool
    child.wait(): reaps child exit status
    watchdog.join(): joins cleanly (already exited)
```

On Windows, `taskkill /F /T /PID <pid>` is used to kill the child and its
entire process tree (node.exe spawned by claude.cmd). On non-Windows, `kill -9`.

If the child is killed by the watchdog, `BufReader::lines()` hits EOF
naturally (the write end of the pipe closes when the process dies). The
stream-json parse loop exits, `output` / `session_id` contain whatever was
collected before the kill, and the caller receives a partial but non-panicking
result.

### Why `run_one_round` (supervision) was not changed

`run_one_round` already used BufReader streaming (it predates Fix B). It is
only called by `run_supervised`, which is not used by squad workers. The
supervision path uses `run_claude` → `wait_child_with_timeout` for everything
else (consult, run_sync). Mixing streaming and buffered paths in supervision
is acceptable because supervision typically runs at N=1 (no concurrency).

## 5. Fix A — Subprocess Timeout (context)

Fix A addresses the second failure mode: the subprocess blocking indefinitely
when the Anthropic API drops a connection.

`run_claude()` (used by `run_sync`, `consult`, `run_one_round`) now delegates
to `run_claude_with_timeout()`:

```rust
fn run_claude(args, cwd) -> Result<Output> {
    run_claude_with_timeout(args, cwd, Duration::from_secs(600))
}
```

`wait_child_with_timeout` drains stdout and stderr on background threads
(required — OS pipe buffers are ~64 KB; if not drained, the child blocks on
write and never exits). It polls `child.try_wait()` every 500ms and kills
the child if the deadline passes.

Fix A is **insufficient alone** for the OOM problem (see postmortem §UPDATE).
It prevents indefinite blocking but does not reduce per-call heap because
`wait_child_with_timeout` still calls `read_to_end()`. Fix B is required to
address the heap pressure.

## 6. `run_one_turn_scoped` — Tool Whitelist

Added in the same commit as Fix B, `run_one_turn_scoped` accepts an optional
tool whitelist:

```rust
pub fn run_one_turn_scoped(
    cwd: &str, prompt: &str, continuation: bool,
    allowed_tools: Option<&[&str]>,
) -> Result<(String, String), String>
```

- `None` → `--dangerously-skip-permissions` (god mode, backward-compatible)
- `Some(&["Read", "Grep", "Glob"])` → `--allowedTools "Read,Grep,Glob"`

Used by `multi_agent::PersistentSession` to restrict each role (PM, Engineer,
Reviewer) to only the tools it actually needs, reducing blast radius from a
misbehaving role. The existing `run_one_turn` is a thin wrapper passing `None`.

## 7. Windows `.cmd` Handling

`claude` is installed via npm as `claude.cmd` on Windows. Invoking it through
`cmd /c claude.cmd` breaks multi-line prompts: cmd.exe treats embedded newlines
as command separators, stripping flags and truncating prompts.

`run_claude_with_timeout` resolves this by detecting the `.cmd` extension and
locating the backing Node.js entry point:

```
%APPDATA%\npm\node_modules\@anthropic-ai\claude-code\cli.js
```

Then invokes `node cli.js` directly, bypassing cmd.exe entirely. See
`resolve_claude_node_script()` for the resolution logic.

`run_one_turn_scoped` (Fix B path) currently uses `cmd /c` as a fallback
(line 385–391). This is a known inconsistency — if multi-line prompts are
needed in squad worker calls, the node-direct invocation should be applied
there too. Tracked as a future cleanup.

## 8. Design Decisions

**Why external subprocess, not API?**
Claude Code CLI uses the operator's Max plan — no API key required, no token
quota management, full tool access. The subprocess model lets Sirin act as an
outer supervisor without implementing its own tool execution loop.

**Why stream-json format?**
`--output-format stream-json` emits one JSON object per line as Claude works.
This enables Fix B (line-by-line processing) and allows `run_supervised` to
observe tool calls in real time (via `UsingTool` events). The alternative
`--output-format text` loses structured metadata and session_id.

**Why `Stdio::null()` for stderr in Fix B?**
Capturing stderr would require either a background drain thread (adding
back memory pressure) or a synchronous drain (blocking the parser). Since
no caller of `run_one_turn_scoped` reads stderr, dropping it at the OS level
is the correct choice. Diagnostic output from the Claude CLI appears in
its own log, not in Sirin's stderr.

**Why a watchdog thread instead of `child.try_wait()` loop?**
The BufReader loop blocks in `next()` while waiting for data. A poll loop
cannot run simultaneously with the BufReader read. A dedicated watchdog thread
runs concurrently and kills the child when the deadline passes, which causes
BufReader to hit EOF and unblock the main thread naturally.

## 9. Known Limits / Future Work

- **Fix B path uses `cmd /c` fallback** for the Windows `.cmd` case
  (not the `node cli.js` direct invocation used by Fix A path). Multi-line
  prompts in squad worker calls could be truncated if they contain newlines.
  Low risk today (prompts are typically single-paragraph), but worth aligning.

- **Partial output on watchdog kill**: if the 600s watchdog fires, the caller
  receives whatever text was streamed before the kill. There is no explicit
  error returned — the function returns `Ok((partial_output, partial_session_id))`.
  Callers should check session_id emptiness as a proxy for incomplete execution.

- **No per-line size guard**: a single malformed stream-json line could
  theoretically be arbitrarily large (e.g. a tool result with a huge embedded
  file). BufReader has no line-length limit. In practice, Claude Code's JSON
  emitter chunks large payloads across multiple lines, so this is not observed
  in practice.

- **Memory probe missing**: the postmortem recommended adding a 60s RSS probe
  to worker.rs to graph Sirin's own memory over time. This has not been
  implemented. Without it, future slow leaks (outside of claude_session) will
  again be invisible until crash.

- **Uptime-bound crash pattern (unresolved)**: crash #4 (postmortem §UPDATE)
  occurred ~3 min after workers spawned but ~45 min after Sirin itself started —
  the same 38–45 min uptime window as crashes #1–#3. This suggests a slow
  baseline leak in the Sirin process (LLM fleet probe, Telegram listener buffers,
  codebase index) that is independent of claude_session. Fix B eliminates the
  per-call subprocess OOM spike but does not address the baseline leak. The idle
  soak test (run Sirin with zero workers, monitor RSS) recommended in the postmortem
  has not been performed.
