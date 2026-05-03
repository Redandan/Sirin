Quick project health check. Use Bash tool with `timeout: 120000`.

Run these commands — cargo check ONCE (pipe to two greps), git and wc in parallel:

```bash
# Single cargo check, capture output once
CARGO_OUT=$(cargo check 2>&1)
WARN=$(echo "$CARGO_OUT" | grep -c "warning:" || echo 0)
ERR=$(echo "$CARGO_OUT" | grep -c "^error" || echo 0)
UNCOMMITTED=$(git status -s 2>/dev/null | grep -v ".claude/worktrees" | wc -l)
UI=$(wc -l web/index.html web/app.js web/style.css src/ui_service*.rs 2>/dev/null | tail -1 | awk '{print $1}')
echo "Errors: $ERR | Warnings: $WARN | Uncommitted: $UNCOMMITTED | UI: $UI lines"
```

**NEVER** use `run_in_background=true` for cargo commands.

Report the single output line verbatim.
