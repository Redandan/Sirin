Run the test suite using the Bash tool with these STRICT rules:

## Execution rules (NEVER violate)
- **NEVER** use `run_in_background=true` for cargo commands — Cargo uses an exclusive file lock on `target/`; two concurrent cargo processes deadlock
- **ALWAYS** set `timeout: 600000` (10 min) on the Bash tool call
- **NEVER** launch a second `cargo` command while one is still running
- **The Bash tool will auto-background long-running commands regardless of `run_in_background`.  This is OK — handle it correctly (see below).**

## Command
```bash
cargo test --bin sirin > /tmp/sirin_test.txt 2>&1 ; tail -8 /tmp/sirin_test.txt
```

**Why this form?**  `| tail` in a pipe causes cargo to silently die in background mode.
Redirecting to a file + separate `tail` keeps cargo in foreground and always produces output.

## If the command auto-backgrounds (returns a task ID immediately)
The Bash tool shows:
> Command running in background with ID: <id>. Output is being written to: <path>

**DO NOT call TaskOutput** — it blocks for the full timeout duration (= wasted minutes).

Instead:
1. Tell the user "tests running in background, waiting for completion…"
2. Wait for the `<task-notification>` system message (fires automatically when done)
3. Use **Read** on the output file path from the initial response to get the last lines
4. Report result

## If Cargo lock is held
Error contains "waiting for file lock" → wait 30 s, retry once. If still locked: "⚠️ Cargo file lock held — kill other cargo process first"

## Output format
Report ONLY the final result line:
- `✅ X passed, Y failed, Z ignored`
- `❌ FAILED: [list failed test names]`

Keep response under 5 lines.
