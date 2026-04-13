Print the current UI structure by reading function signatures from all ui_egui/*.rs files.
Show as a tree:

```
ui_egui/
├── mod.rs:    App + update() + toast overlay
├── sidebar:   show() → [AGENTS, SYSTEM, COLLAB sections]
├── workspace: show() → [overview, thinking, pending, settings tabs]
├── settings:  show() → [system: TG/LLM/MCP/Skills]
├── log_view:  show() → [filter + cached lines]
├── workflow:  show() → [empty/active pipeline + advance]
├── meeting:   show() → [invite/active room + send]
└── theme:     [colours + card/badge/section/info_row helpers]
```

Also show `wc -l` for each file. Keep response under 20 lines.
