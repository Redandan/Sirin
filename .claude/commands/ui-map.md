Print the current UI structure by reading `web/index.html` (single-file
Alpine.js root) plus the supporting files. Show as a compact tree.

```
web/  (served at http://127.0.0.1:7700/ui/, bundled via include_bytes!)
├── index.html       Alpine.js x-data root
│   ├── header           top bar (status dots + ✓ verdict + URL + ⚙ + ⌘K)
│   ├── sidebar          VIEWS (Dashboard/Testing) + AGENTS list
│   ├── view: dashboard  composable widget grid (4-col)
│   │                    widgets: active_runs / recent_runs / coverage /
│   │                    browser / kpi_pass_rate / kpi_runs_today /
│   │                    kpi_avg_duration / kpi_cost_hour /
│   │                    kpi_active_agents / kpi_running_tests
│   ├── view: testing    sub-tabs: Runs / Coverage / Browser
│   ├── view: workspace  sub-tabs: 對話 / 概覽 / 待確認 / 設定
│   ├── modals           Settings / Logs / Dev Squad / MCP Playground / etc
│   ├── command palette  ⌘K fuzzy filter, 10 entries
│   └── gear menu        Settings · Logs · Open Palette
├── app.js           sirin() factory: state + fetch + WebSocket + actions
├── style.css        design tokens + widget grid + KPI cards
├── alpine.min.js    bundled Alpine v3 runtime (~46 KB)
└── DESIGN.md        competitor inspiration map (Linear / Playwright / etc)
```

Also report `wc -l` for each `web/*` file. Keep response under 25 lines.
