# Sirin

純 Rust 跨平台 AI Agent 平台。Tokio 非同步後端負責多平台訊息監聽、任務追蹤、LLM 自動回覆與多階段調研；前端使用 Dioxus 0.7 跨平台 UI（Desktop / Web / Mobile），零 WebView、零 Node.js、零外部 C 依賴（SQLite 靜態編入）。

---

## 架構總覽

```
Dioxus UI (Desktop / Web)
   │
   ├── Sidebar：助手清單 + 導航
   │
   ├── Agent Workspace
   │       └── 概覽 / 待確認 tabs
   │
   ├── Settings（Agent 設定 + 系統面板）
   ├── Log（即時日誌 + filter）
   ├── Workflow（Skill 開發 pipeline）
   └── Meeting（多 Agent 會議室）

Tokio Background Tasks
   │
   ├── Telegram Worker (src/telegram/)
   │       └── handler → intent classify → LLM reply / research trigger
   │
   ├── Teams Worker (src/teams/)
   │       └── Chrome CDP → MutationObserver → AI 草稿 → PendingReply
   │
   ├── Follow-up Worker (src/followup.rs)
   │       └── periodic evaluation of tracked tasks
   │
   ├── MCP Client (src/mcp_client.rs)
   │       └── 連接外部 MCP Server，發現並代理工具
   │
   ├── Events Bus (src/events.rs)
   │
   ├── RPC Server  ws://127.0.0.1:7700/
   └── MCP Server  http://127.0.0.1:7700/mcp
```

**Agent pipeline**：`Planner` 分析意圖 → `Router` 按 `IntentFamily` 派發 → 對應 agent 執行並發佈事件。

---

## 功能模組

### 多助手管理
- `config/agents.yaml` 定義多個助手實例
- 每個助手可獨立配置：身分、通訊平台、目標、人性化行為、KPI
- 仿人類行為引擎：隨機延遲、每小時/每日訊息上限、工作時間排程

### YAML 外掛技能
- `config/skills/*.yaml` 新增技能，無需修改主程式
- 熱重載：`refresh()` 時自動合併硬編碼（13 個）+ YAML 動態技能
- Rhai 腳本引擎支援（`config/scripts/*.rhai`）

### MCP Client（外部工具代理）
- `config/mcp_servers.yaml` 配置外部 MCP Server
- 啟動時自動發現工具並註冊到 Agent ToolRegistry
- Agent 可透明呼叫外部工具（如 `mcp_agora-trading_getBalance`）

### Telegram 整合
- MTProto 原生協議（grammers-client）
- 群組/私訊監聽，AI 自動回覆
- 人工審核閘門（PendingReply）
- 斷線自動重連（指數退避 + jitter）

### Teams 整合（事件驅動）
- Chrome CDP + MutationObserver，新訊息 < 100ms 偵測
- AI 驅動的草稿生成（ChatAgent pipeline）
- 自動回「稍等」+ 實質草稿進「待確認」

### MCP Server
- `http://127.0.0.1:7700/mcp`（MCP 2024-11-05 Streamable HTTP）
- Claude Desktop 可直接連接

| 工具 | 說明 |
|------|------|
| `memory_search` | 搜尋記憶庫 |
| `skill_list` | 列出所有技能 |
| `teams_pending` | 查看 Teams 待確認草稿 |
| `teams_approve` | 核准並送出指定草稿 |
| `trigger_research` | 啟動調研任務 |

---

## 技術堆疊

| 層 | 技術 |
|---|---|
| GUI | Dioxus 0.7（跨平台：Desktop / Web / Mobile） |
| 非同步 | Tokio 1.37（full features） |
| LLM | Ollama / LM Studio / Gemini / Anthropic Claude |
| Telegram | grammers-client 0.9（MTProto） |
| Teams | headless_chrome 1.0（CDP，事件驅動） |
| HTTP/WS 服務 | axum 0.8 |
| 記憶 | SQLite FTS5（rusqlite bundled）+ JSONL ring-log |
| 錯誤處理 | thiserror 統一錯誤類型 |
| Web 搜尋 | DuckDuckGo + SearxNG fallback |
| 程式碼解析 | tree-sitter + tree-sitter-rust |
| 序列化 | serde / serde_json / serde_yaml |

---

## 前置需求

- Rust toolchain（`rustup` 安裝，stable channel）
- Microsoft C++ Build Tools（Windows）
- 本機 LLM 服務：Ollama 或 LM Studio
- Teams 功能：Google Chrome

---

## 快速開始

```powershell
# 開發模式
cargo run

# Release 版本
cargo build --release
.\target\release\sirin.exe

# 測試
cargo test
```

---

## 環境變數

在專案根目錄建立 `.env`（完整列表見 [docs/ENV_REFERENCE.md](docs/ENV_REFERENCE.md)）：

```env
# LLM
LLM_PROVIDER=ollama              # ollama | lmstudio | gemini | anthropic
OLLAMA_MODEL=llama3.2

# Telegram
TG_API_ID=                       # 從 https://my.telegram.org 取得
TG_API_HASH=
TG_AUTO_REPLY=true

# 可選：獨立模型
ROUTER_MODEL=                    # Planner / Router（輕量模型）
LARGE_MODEL=                     # 高推理能力任務
```

---

## 設定檔

| 檔案 | 說明 |
|------|------|
| `config/agents.yaml` | 多助手實例設定（身分、通道、目標、行為） |
| `config/persona.yaml` | 全局人格與 Coding Agent 設定 |
| `config/skills/*.yaml` | YAML 自訂技能目錄 |
| `config/scripts/*.rhai` | Rhai 自動化腳本 |
| `config/mcp_servers.yaml` | 外部 MCP Server 連接配置 |
| `config/llm.yaml` | UI 儲存的模型選擇（覆蓋環境變數） |

---

## MCP 整合

### 作為 Server（讓外部 AI 呼叫 Sirin）

在 `claude_desktop_config.json` 加入：

```json
{
  "mcpServers": {
    "sirin": { "url": "http://127.0.0.1:7700/mcp" }
  }
}
```

### 作為 Client（Sirin 呼叫外部工具）

在 `config/mcp_servers.yaml` 配置：

```yaml
servers:
  - name: agora-trading
    url: "http://localhost:3001/mcp"
    enabled: true
```

啟動後 Agent 自動發現並使用外部工具。

---

## 重要檔案

| 路徑 | 說明 |
|------|------|
| `src/main.rs` | 程式入口：Tokio runtime、Dioxus 視窗、背景任務啟動 |
| `src/ui_dx/` | Dioxus UI：sidebar、workspace、settings、log、workflow、meeting |
| `src/agents/` | Planner / Router / Chat / Coding / Research agent |
| `src/adk/` | ADK 核心：Agent trait、ToolRegistry、AgentRuntime |
| `src/telegram/` | Telegram listener / handler / reply |
| `src/teams/` | Teams CDP 事件驅動 + AI 草稿生成 |
| `src/mcp_client.rs` | MCP Client：連接外部 Server、發現工具、代理呼叫 |
| `src/mcp_server.rs` | MCP Server（/mcp endpoint） |
| `src/memory.rs` | 三層記憶：SQLite FTS5 / 程式碼索引 / JSONL |
| `src/llm.rs` | LLM 呼叫層（Ollama / LM Studio / Gemini / Claude） |
| `src/persona.rs` | 人格設定 + 快取 + ROI 行為引擎 |
| `src/pending_reply.rs` | 人工確認流程（併發安全） |
| `src/error.rs` | 統一錯誤類型（thiserror） |
| `src/events.rs` | Broadcast event bus |

---

## 資料儲存路徑（Windows）

| 內容 | 路徑 |
|------|------|
| 任務追蹤 | `%LOCALAPPDATA%\Sirin\tracking\task.jsonl` |
| 調研記錄 | `%LOCALAPPDATA%\Sirin\tracking\research.jsonl` |
| 記憶 SQLite | `%LOCALAPPDATA%\Sirin\memory\memories.db` |
| per-peer 對話 | `%LOCALAPPDATA%\Sirin\context\<peer_id>.jsonl` |
| 待確認草稿 | `data/pending_replies/<agent_id>.jsonl` |
| Telegram session | `data/sessions/<agent_id>.session` |
| Teams Chrome profile | `data/teams_profile/` |
