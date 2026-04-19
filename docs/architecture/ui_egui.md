# UI Layer — `src/ui_egui/`

> Source: `src/ui_egui/`
> Cross-references: [./mcp_server.md](./mcp_server.md), [./multi_agent.md](./multi_agent.md)

---

## 1. Purpose

Sirin's desktop GUI is built on **egui 0.31** (immediate-mode Rust UI framework).
Every frame the entire UI is re-evaluated — there is no retained DOM or component
tree. The UI is intentionally kept thin: it reads state from `AppService` and
calls its mutating methods; it never imports backend modules directly.

The visual language is deliberately **極簡硬核** (minimalist, hardcore):
crypto-terminal / kernel-monitor aesthetic, dark background, spring-green accents.

---

## 2. Why egui

egui's immediate-mode model has a non-obvious benefit for AI-assisted development:
**the UI state is the code, not a runtime DOM**.

| Property | egui (Sirin) | Browser-based UI (React/Flutter) |
|---|---|---|
| UI state location | Rust structs in source | Runtime DOM / widget tree |
| AI reads UI by... | `read_file src/ui_egui/` | Must run browser + screenshot |
| Audit trail | `git diff` | Inspector + browser recording |
| Cross-platform | Single binary | Electron / WebView dependency |
| CJK rendering | System font, one `setup_fonts()` call | Font loading pipeline |

Because the entire UI is expressed as Rust code that is re-executed every frame,
an AI session can understand the full UI structure — layout, routing, data
bindings, event handling — by reading the source files.  No DOM, no VDOM diffing,
no component lifecycle to trace.

The trade-off: no hot-reload (must recompile), no CSS, and the widget set is
smaller than browser UI libraries.  These are acceptable for a single-user desktop
tool where iteration speed matters less than auditability.

---

## 3. Module Map

**Active modules** (declared in `mod.rs`, compiled):

```
src/ui_egui/
├── mod.rs          SirinApp (eframe::App), View enum, toast overlay, font setup
├── theme.rs        Colour palette, spacing constants, shared widget helpers
├── sidebar.rs      Collapsible left panel — agent list + nav items
├── workspace.rs    Per-agent conversation + task view (View::Workspace(idx))
├── settings.rs     System settings panel (View::Settings)
├── log_view.rs     Streaming log tail (View::Log)
├── browser.rs      Embedded browser control panel (View::Browser)
├── monitor/        Screenshot/monitor view (View::Monitor) — subdirectory module
└── team_panel.rs   Dev-squad queue UI (View::Team)
```

**Orphaned files on disk** (NOT in `mod.rs`, not compiled):

```
src/ui_egui/workflow.rs   — workflow stage tracker panel (superseded by team_panel)
src/ui_egui/meeting.rs    — meeting room panel (multi-participant chat simulation)
```

These files exist on disk but are not referenced from `mod.rs` and are therefore
dead code.  Do not import them; add `pub mod workflow;` / `pub mod meeting;` to
`mod.rs` first if you want to revive them.

**Rule**: zero backend imports inside `src/ui_egui/`. All data flows through
`Arc<dyn AppService>` from `src/ui_service.rs`.

---

## 4. View Routing

```rust
#[derive(PartialEq, Clone)]
enum View {
    Workspace(usize),  // index into agents list
    Settings,
    Log,
    Browser,
    Monitor,
    Team,
}
```

`sidebar::show()` mutates the `view` field; `CentralPanel` routes to the
matching module:

```
View::Workspace(idx) → workspace::show(...)
View::Settings       → settings::show(...)
View::Log            → log_view::show(...)
View::Browser        → browser::show(...)
View::Monitor        → monitor::show(...)
View::Team           → team_panel::show(...)
```

---

## 5. SirinApp Struct

```rust
pub struct SirinApp {
    svc: Arc<dyn AppService>,       // backend boundary — only thing UI imports
    view: View,
    agents: Vec<AgentSummary>,
    tasks: Vec<TaskView>,
    pending_counts: HashMap<String, usize>,
    last_refresh: Instant,
    toasts: VecDeque<Toast>,
    renaming: Option<(usize, String)>,   // inline rename state in sidebar

    sidebar_collapsed: bool,
    log_state:       log_view::LogState,
    workspace_state: workspace::WorkspaceState,
    settings_state:  settings::SettingsState,
    browser_state:   browser::BrowserUiState,
    monitor_state:   monitor::MonitorViewState,
    team_state:      team_panel::TeamPanelState,
    update_banner_dismissed: bool,
}
```

Each panel module owns its own `*State` struct; `SirinApp` stores them so state
survives panel switches.

### Frame Loop (`eframe::App::update`)

1. Auto-refresh every 5 s: `svc.list_agents()`, `svc.recent_tasks(200)`, pending counts.
2. Deactivate screenshot pump when `Monitor` is not the active view.
3. Drain `svc.poll_toasts()` into the `toasts` deque; expire stale entries.
4. Render sidebar.
5. Optionally render update banner (top panel).
6. Render central panel → dispatch to active module.
7. Render toast overlay (right-bottom `egui::Area`, last 3 toasts).

---

## 6. Theme (`theme.rs`)

### Colour Palette

| Constant     | Hex       | Usage                                   |
|--------------|-----------|----------------------------------------|
| `BG`         | `#1A1A1A` | Window / panel background               |
| `CARD`       | `#222222` | Card / section background               |
| `HOVER`      | `#2A2A2A` | Hovered row / button background         |
| `BORDER`     | `#333333` | Card stroke, separator lines            |
| `TEXT`       | `#E0E0E0` | Primary text                            |
| `TEXT_DIM`   | `#808080` | Secondary / muted text                  |
| `ACCENT`     | `#00FFA3` | Running state, selected indicator, dots |
| `DANGER`     | `#FF4B4B` | Error, disabled state                   |
| `INFO`       | `#4DA6FF` | Links, informational labels             |
| `YELLOW`     | `#FFD93D` | Warning / reconnecting state            |
| `VALUE`      | `WHITE`   | Numeric values (monospace)              |

### Typography (`FONT_*` constants, in points)

| Constant        | Size  | Typical Use              |
|-----------------|-------|--------------------------|
| `FONT_TITLE`    | 18.0  | Page/section title       |
| `FONT_HEADING`  | 15.0  | Card heading             |
| `FONT_BODY`     | 13.0  | Body text, nav labels    |
| `FONT_SMALL`    | 11.5  | Secondary info           |
| `FONT_CAPTION`  | 10.0  | Sidebar group labels, dots |

### Spacing (`SP_*` constants, in points)

| Constant | Value | Usage                          |
|----------|-------|--------------------------------|
| `SP_XS`  | 4.0   | Tiny gaps                      |
| `SP_SM`  | 8.0   | Component internal padding     |
| `SP_MD`  | 12.0  | Central panel inner margin     |
| `SP_LG`  | 20.0  | Central panel outer margin     |
| `SP_XL`  | 32.0  | Section breaks                 |

### Widget Helpers (`theme.rs` public functions)

```rust
theme::apply(ctx)         // install palette + global style once at startup
theme::thin_separator(ui) // 1px #333333 horizontal rule
theme::section(ui, label, |ui| {...})   // labelled section with divider
theme::card(ui, |ui| {...})             // Frame + Rounding(4) + Stroke(1,BORDER)
theme::badge(ui, text, color)          // pill label
theme::tab_bar(ui, labels, selected)   // horizontal tab selector, returns new idx
theme::info_row(ui, label, value)      // "Label   value" with dim/white contrast
theme::status_row(ui, label, status, ok) // "Label   ● status" ACCENT/DANGER dot
theme::status_color(status)              // maps status string to Color32
theme::log_color(level)                  // maps LogLevel to Color32 (for log_view)
```

---

## 7. Sidebar (`sidebar.rs`)

Two modes controlled by `sidebar_collapsed: bool`:

### Expanded (210 px)
- AGENTS section with scroll area (height = available − 180 px)
  - Each agent row: status dot + name + pending-count badge
  - Click → `View::Workspace(idx)`, double-click → inline rename
  - Active item: `HOVER` fill + 3 px `ACCENT` left bar
  - Dot colours: `ACCENT`=connected, `YELLOW`=reconnecting/waiting, `DANGER`=error
- SYSTEM group: 系統設定, Log
- TOOLS group: Browser, Monitor, 開發小隊
- Bottom strip: TG ● RPC ● status indicators

### Collapsed (36 px)
- Pending dot (ACCENT) if any agent has pending messages
- `›` expand button
- TG / RPC status dots at bottom

---

## 8. CJK Font Setup

`setup_fonts()` loads the first available system font from:
- Windows: `C:\Windows\Fonts\msjh.ttc` (Microsoft JhengHei) or `msyh.ttc`
- macOS: `/System/Library/Fonts/PingFang.ttc`
- Linux: `/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc`

The loaded font is inserted at index 0 of both `Proportional` and `Monospace`
families, ensuring CJK characters render correctly in all text widgets.

---

## 9. Update Banner

A slim `TopBottomPanel::top("update_banner")` is rendered when
`updater::get_status()` returns an actionable state:

| State            | Banner text             | Buttons           |
|------------------|-------------------------|-------------------|
| `Available(v)`   | 🆕 Sirin vX.Y.Z 可用    | 立即更新, 📥手動下載 |
| `Applying`       | ⏳ 下載更新中…           | —                 |
| `RestartRequired`| ✅ 更新完成 — 重啟生效   | —                 |
| `ApplyFailed(e)` | ❌ 更新失敗: reason       | 📥手動下載         |

The banner has a `✕` dismiss button that sets `update_banner_dismissed = true`
(session-scoped; resets on restart). `Idle / Checking / UpToDate` suppress the
banner entirely.

---

## 10. Key Panels

### Workspace (`workspace.rs`)
- Shows conversation history for a selected agent workspace
- Contains task-status filter tabs: 全部 / 執行中 / 完成 / 失敗
- State: `WorkspaceState { overview_filter: usize, ... }`

### Team Panel (`team_panel.rs`)
- Multi-agent squad queue view
- Lists tasks by status (Queued / Running / Done / Failed)
- Provides enqueue input + start/stop worker controls
- Calls `svc.team_*()` methods from `MultiAgentService`

### Browser Panel (`browser.rs`)
- Controls the persistent Chrome CDP session
- Navigate URL bar, screenshot display, action log
- Calls `svc.browser_*()` from `BrowserService`

### Monitor (`monitor.rs`)
- Periodic screenshot viewer for the Teams / Chrome window
- Deactivates screenshot pump when panel not active (guards CPU)
- State: `MonitorViewState`

---

## 11. AppService Boundary

The UI exclusively uses `Arc<dyn AppService>` (defined in `src/ui_service.rs`).
No `ui_egui` module may import a backend crate directly.

### Example violation (DO NOT write this):

```rust
// src/ui_egui/settings.rs  ← WRONG
use crate::memory::MemoryStore;   // ← violates boundary; backend module

fn show_memory(ui: &mut egui::Ui, store: &MemoryStore) {
    let results = store.search("query", 10);  // direct backend call
    ...
}
```

### Correct pattern (go through AppService):

```rust
// src/ui_egui/settings.rs  ← CORRECT
// No backend imports — only ui_service types

fn show_memory(ui: &mut egui::Ui, svc: &Arc<dyn AppService>, query: &str) {
    let results = svc.search_memory(query, 10);  // through service boundary
    ...
}
```

If the data you need is not yet exposed by `AppService`, add a method to the
appropriate sub-trait in `src/ui_service.rs` and implement it in
`src/ui_service_impl/` first.  Never reach through the boundary directly.

Seven sub-traits are aggregated:

| Sub-trait | Key methods |
|---|---|
| `AgentService` | `list_agents`, `agent_detail`, `create_agent`, `rename_agent`, `toggle_agent`, `delete_agent`, `add_objective`, `set_behavior`, `toggle_skill` |
| `PendingReplyService` | `pending_count`, `load_pending`, `approve_reply`, `reject_reply`, `edit_draft` |
| `WorkflowService` | `workflow_state`, `workflow_create`, `workflow_advance`, `workflow_generate`, `workflow_save_output` |
| `IntegrationService` | `tg_submit_code`, `teams_running`, `mcp_tools`, `mcp_call`, `meeting_start/end/send`, `chat_send`, `trigger_research`, `execute_skill` |
| `SystemService` | `recent_tasks`, `log_recent`, `system_status`, `search_memory`, `persona_*`, `available_models`, `config_check`, `config_ai_analyze`, `export_config`, `poll_toasts` |
| `MultiAgentService` | `team_dashboard`, `team_queue`, `team_enqueue`, `team_start_worker`, `team_clear_completed`, `team_reset_member` |
| `BrowserService` | `browser_open`, `browser_navigate`, `browser_click`, `browser_type`, `browser_screenshot`, `browser_eval`, `browser_read`, `browser_url`, `browser_console`, `browser_tab_count` (20 methods) |

`RealService` in `src/ui_service_impl/` implements all seven.

---

## 12. Layout Constants (from `CLAUDE.md`)

```
Top Panel:    32 pt high — title + version + global status indicators
Side Panel:   210 pt wide (expanded), 36 pt (collapsed)
Central Panel: ScrollArea, inner_margin 12 pt
Component gap: 8 pt
Inner padding: 12 pt
```

Card style: `egui::Frame` + `Rounding(4.0)` + `Stroke(1.0, BORDER)`.

---

## 13. Known Limits / Future Work

### No headless UI test harness

egui has no equivalent of testing-library or Playwright.  The only way to verify
UI behaviour is to run the binary and interact visually.  Automated UI regression
is not possible without a screen-capture + vision pipeline.  `cargo test` covers
service logic but cannot exercise the egui widgets.

### No multi-window support

egui's `eframe` runner manages a single native window.  Detaching a panel into a
second window (e.g. a floating browser panel) would require `egui_multiwin` or a
custom approach.

### No virtual list for large datasets

`ScrollArea` + repeated `ui.label()` calls render every row.  The task log panel
calls `svc.recent_tasks(200)` and renders all 200 rows unconditionally.  For very
long logs (thousands of entries) this causes frame time spikes.  A virtual list
(only render visible rows) would fix this but is not implemented in egui 0.31
without manual clipping.

### AppService boundary violation risk

Nothing in the Rust type system prevents a UI module from importing a backend
crate directly — the rule is enforced by convention and CI linting.  Adding a
`use crate::memory::` import in `src/ui_egui/settings.rs` would compile fine but
violates the architecture.  A `forbid(unsafe_code)` style lint module alias
(re-exporting only via `AppService`) would make this enforced at compile time.
