# Sirin 架構說明

本文件描述目前程式碼實際對應的架構。

## 1. 系統總覽

Sirin 是一個**單一 Rust binary** + **plain HTML 網頁 UI**：UI 由內建的
axum HTTP server（`mcp_server.rs`）在 `:7700/ui/` serve，前端三個檔案
（`index.html` / `app.js` / `style.css` + `alpine.min.js`）透過
`include_bytes!` 編進 binary，所以仍然是單檔分發。沒有 WebView、沒有
Node.js、沒有 IPC 序列化層；UI 跟後端在同一 process 透過 HTTP +
WebSocket 溝通。

> **v0.5.0 (2026-05-02) 之前** UI 是 `egui 0.31` immediate mode 桌面
> 視窗。改成 web UI 的主因：egui 的 child↔parent layout 協商對 AI 開發
> 不友善（每次改要 5 分鐘 cargo build），而 web 有 DevTools / hot reload
> / 海量訓練資料。詳見 `web/DESIGN.md`。

Agent layer 使用 ADK-RUST 風格：
- `src/adk/`：`Agent` / `AgentContext` / `ToolRegistry` / `AgentRuntime`
- `src/agents/`：具體 agent（`planner_agent`、`router_agent`、`chat_agent`、`coding_agent`、`research_agent`）

```
┌──────────────────────────────────────────────────────────┐
│                    sirin.exe（單一進程）                  │
│                                                          │
│  ┌──────────────┐      ┌──────────────────────────────┐  │
│  │ Web UI       │      │ ADK Runtime                  │  │
│  │  (:7700/ui/) │ ───→ │ - AgentRuntime              │  │
│  │ + Telegram   │      │ - AgentContext              │  │
│  │ + Teams      │      │ - ToolRegistry（90+ 內建    │  │
│  │ + MCP Client │      │   + 動態 MCP 外部工具）      │  │
│  └──────────────┘      │                              │  │
│                         └──────────────┬──────────────┘  │
│                                        │                 │
│                         ┌──────────────▼──────────────┐  │
│                         │ Agents                      │  │
│                         │ - planner_agent             │  │
│                         │ - router_agent              │  │
│                         │ - chat_agent                │  │
│                         │ - coding_agent              │  │
│                         │ - research_agent            │  │
│                         └─────────────────────────────┘  │
│                                                          │
│  ┌────────────┐  ┌──────────┐  ┌───────────────────┐    │
│  │ Memory     │  │ Events   │  │ MCP Server        │    │
│  │ (SQLite    │  │ (broadcast│  │ (axum HTTP)       │    │
│  │  FTS5)     │  │  channel) │  │ + RPC (WebSocket) │    │
│  └────────────┘  └──────────┘  └───────────────────┘    │
└──────────────────────────────────────────────────────────┘
```

## 2. UI 層（plain HTML / Alpine.js, served at :7700/ui/）

```
web/
├── index.html        Single-page Alpine.js root — Dashboard / Testing /
│                     Workspace / 5 modals / ⌘K palette
├── app.js            sirin() factory: state + fetch + WebSocket + actions
├── style.css         Design tokens (#1A1A1A + #00FFA3) + widget grid
├── alpine.min.js     Bundled Alpine.js v3 runtime (~46 KB)
└── DESIGN.md         Competitor inspiration map (Linear / Playwright / GH Actions)
```

**渲染流程**：
- Browser opens `http://127.0.0.1:7700/ui/` (auto-launched by `main.rs`)
- `mcp_server.rs` serves `web/*` via `include_bytes!` (single-binary distribution)
- WebSocket connection to `/ws` pushes snapshot every 2 s (fallback to 5 s polling)
- Slow data (config_check / log_recent / team_dashboard) lives in dedicated
  on-demand endpoints fetched only when corresponding modal opens

**狀態管理**：
- Frontend state lives in Alpine factory (`sirin()` in `app.js`)
- `AppService` trait still provides backend access; web UI consumes via:
    - `GET /api/snapshot` — combined dashboard JSON
    - `POST /mcp` — existing MCP tool-call protocol (also drives the
      MCP Playground modal that lists all 90+ tools)
    - `GET /api/browser_screenshot` — live PNG of controlled Chrome
    - 5 mutating endpoints for agent edit / pending review / chat / persona
- Daemon-style: closing the browser tab does NOT kill sirin. Re-open
  the URL anytime to see live state.
- Dashboard layout customizable via Alpine `dashboard_layout` array
  persisted in localStorage

## 3. Agent Pipeline

```
用戶訊息 → Planner（意圖分類）→ Router（路由分派）
              │                        │
              │  IntentFamily:         ├── Chat → ChatAgent（本地 LLM）
              │  GeneralChat           ├── Research → ResearchAgent
              │  Research              ├── LargeModel → ChatAgent（遠端 LLM）
              │  CodeAnalysis          └── Coding → CodingAgent
              │  SkillArchitecture
              │  ...
```

Router 和 Planner 固定使用本地小模型（`ROUTER_MODEL`），不消耗遠端 API。

## 4. MCP 雙向整合

```
外部 AI (Claude Desktop) ──→ Sirin MCP Server (:7700/mcp)
                              ├── memory_search
                              ├── skill_list
                              ├── teams_pending / approve
                              └── trigger_research

Sirin MCP Client ──→ 外部 MCP Server (config/mcp_servers.yaml)
                      └── 動態發現工具 → 註冊到 ToolRegistry
                          → Agent 可透明呼叫
```

## 5. 記憶系統

| 層 | 儲存 | 用途 |
|---|---|---|
| SQLite FTS5 | `memories.db` | 長期記憶，全文搜尋 |
| 對話上下文 | `context/<peer_id>.jsonl` | 最近 N 輪對話 |
| 任務追蹤 | `tracking/task.jsonl` | 行為引擎評估 |
| 程式碼索引 | `code_graph/graph.jsonl` | tree-sitter call graph |

## 6. 錯誤處理

統一錯誤類型 `SirinError`（`src/error.rs`）：
- `thiserror` 派生
- 支援 IO / JSON / YAML / HTTP / SQLite / LLM / Tool / Config
- `From<String>` 向後相容舊程式碼

## 7. 效能設計

- **Persona 快取**：`Persona::cached()` 用 OnceLock 避免重複讀 YAML
- **Log 版本快取**：只在 buffer 變化時重新過濾和著色
- **背景 Refresh**：磁碟 I/O 在 `std::thread::spawn` 執行，不阻塞 UI
- **Mutex poison 安全**：所有 `.lock()` 使用 `unwrap_or_else(|e| e.into_inner())`
- **PendingReply 併發鎖**：`FILE_LOCK` 防止多 task 同時讀寫
- **HTTP Client 共享**：`shared_http()` 全域連接池
- **重連 jitter**：Telegram 斷線重連加入隨機抖動，防雷群效應
