# Sirin — Claude Code guidelines

## Efficiency rules (reduce token / tool-call waste)

- **Read large sections in one call.** When debugging structure in a file over 300 lines, read the full relevant section (e.g. lines 1000–end) in a single `Read` call rather than multiple 30–90-line slices.
- **Brace analysis before editing.** When a brace mismatch is reported, run a quick brace-count first:
  ```bash
  awk '{o+=gsub("{",""); c+=gsub("}","")} END{print "open:"o" close:"c}' src/ui.rs
  ```
  or use grep with line numbers to find all top-level `}` positions before touching the file.
- **One cargo check per iteration.** Only run `cargo check` after an edit, not speculatively.
- **Parallel reads.** When needing context from multiple files at once, issue all `Read`/`Grep` calls in the same message.

## Project layout shortcuts

- Main GUI: `src/ui.rs` — egui immediate-mode, `fn show_chat` owns the input area.
- Agent pipeline: `src/agents/` — planner → router → chat / coding / research / followup.
- Persona config: `config/persona.yaml` — `auto_approve_writes` controls coding dry-run.
- Telegram: `src/telegram/mod.rs` + `src/telegram/handler.rs`.

## Build

```bash
cargo check          # fast type-check
cargo build --release
cargo test           # unit + integration (slow: --ignored for live tests)
```
