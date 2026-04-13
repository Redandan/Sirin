# Sirin 架構說明

本文件描述目前程式碼實際對應的架構。

## 1. 系統總覽

Sirin 是一個**單一 Rust binary**，整合 egui 0.31 immediate mode UI 與 Tokio 非同步後端。沒有 WebView、沒有 Node.js、沒有 IPC 序列化層。

Agent layer 使用 ADK-RUST 風格：
- `src/adk/`：`Agent` / `AgentContext` / `ToolRegistry` / `AgentRuntime`
- `src/agents/`：具體 agent（`planner_agent`、`router_agent`、`chat_agent`、`coding_agent`、`research_agent`）

```
┌──────────────────────────────────────────────────────────┐
│                    sirin.exe（單一進程）                  │
│                                                          │
│  ┌──────────────┐      ┌──────────────────────────────┐  │
│  │ Dioxus UI    │      │ ADK Runtime                  │  │
│  │ + Telegram   │ ───→ │ - AgentRuntime              │  │
│  │ + Teams      │      │ - AgentContext              │  │
│  │ + MCP Client │      │ - ToolRegistry（26+ 內建     │  │
│  └──────────────┘      │   + 動態 MCP 外部工具）      │  │
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

## 2. UI 層（egui 0.31 immediate mode）

```
src/ui_egui/
├── mod.rs          App root + eframe context + view 切換邏輯
├── sidebar.rs      Agent 列表 + 導航按鈕
├── workspace.rs    Agent 工作區（概覽 + 待審核）
├── settings.rs     Agent 設定 + 系統面板入口
├── log_view.rs     日誌（filter + 版本快取）
├── workflow.rs     Skill 開發 6 階段 pipeline
├── meeting.rs      多 Agent 會議室
└── theme.rs        配色方案（#1A1A1A + #00FFA3）
```

**狀態管理**：
- 本地結構體狀態（無 Signal）
- `AppService` trait 提供後端訪問
- 事件監聽通過 tokio channels
- 磁碟 I/O 在背景執行緒執行，不阻塞 UI

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
