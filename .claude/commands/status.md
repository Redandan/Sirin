Quick project health check. Run these in parallel and report a compact summary:
1. `cargo check 2>&1 | grep -c "warning:"` → warning count
2. `cargo check 2>&1 | grep "^error" | wc -l` → error count
3. `git status -s | grep -v ".claude/worktrees" | wc -l` → uncommitted files
4. `wc -l src/ui_egui/*.rs src/ui_service*.rs` → UI line counts

Format as a single compact block:
```
Errors: 0 | Warnings: 0 | Uncommitted: 0 | UI: 1636 lines
```
