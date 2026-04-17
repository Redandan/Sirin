Run the test suite using the Bash tool with these STRICT rules:

## Execution rules (NEVER violate)
- **NEVER** use `run_in_background=true` for cargo commands — Cargo uses an exclusive file lock on `target/`; background tasks queue indefinitely and cause deadlocks
- **ALWAYS** set `timeout: 600000` (10 min) on the Bash tool call
- **NEVER** launch a second `cargo` command while one is still running

## Command
```bash
cargo test --bin sirin 2>&1 | tail -8
```

If the Cargo lock is already held (error contains "waiting for file lock"), wait 30s then retry once. If still locked, report: "⚠️ Cargo file lock held by another process — kill it first"

## Output format
Report ONLY the final result line:
- `✅ X passed, Y failed, Z ignored`
- `❌ FAILED: [list failed test names]`

Keep response under 5 lines.
