Clean up disk space by removing build caches. Run these commands:

1. `rm -rf target/debug/incremental` — remove incremental compilation cache
2. `rm -rf .claude/worktrees/*/target` — remove worktree build artifacts
3. `df -h /c` — show remaining disk space

Report: "Freed Xg, now Yg free"
