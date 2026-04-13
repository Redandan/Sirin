Stage all changed src/ and config/ files (NOT .claude/worktrees/), generate a conventional commit message from the diff, commit and push to origin main.

Steps:
1. `git add src/ config/ Cargo.toml Cargo.lock CLAUDE.md README.md docs/`
2. `git diff --cached --stat` to see what changed
3. Generate commit message (conventional commits format, 繁體中文 body)
4. `git commit -m "..."` with Co-Authored-By
5. `git push origin main`

Keep response under 10 lines.
