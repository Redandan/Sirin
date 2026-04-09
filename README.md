# Sirin

純 Rust 桌面 AI Agent 平台。Tokio 背景任務負責多平台訊息監聽、任務追蹤、LLM 自動回覆與多階段調研 pipeline；前端使用 egui 原生 GUI，零 WebView、零 Node.js、零外部 C 依賴（SQLite 靜態編入）。

---

## 架構總覽

```
egui (UI thread)
   │
   ├── 側欄：助手清單（雙擊改名）
   │
   ├── Agent Workspace
   │       └── 思考流 / 待確認 / ⚙設定 tab
   │
   ├── Telegram Worker (src/telegram/)
   │       └── handler → intent classify → LLM reply / research trigger
   │
   ├── Teams Worker (src/teams/)
   │       └── 可見 Chrome → MutationObserver → CDP event
   │               → 自動「稍等」+ PendingReply 草稿（即時核准送出）
   │
   ├── Follow-up Worker (src/followup.rs)
   │       └── periodic LLM evaluation of tracked tasks
   │
   ├── Events Bus (src/events.rs)     (broadcast channel，跨模組通訊)
   │
   ├── RPC Server  ws://127.0.0.1:7700/
   └── MCP Server  http://127.0.0.1:7700/mcp
```

**Agent pipeline**：`Planner` 分析意圖 → `Router` 按 `IntentFamily` 派發 → 對應 agent 執行並發佈事件。

---

## 功能模組

### 多助手管理
- `config/agents.yaml` 定義多個助手實例，預設 `助手1`（Telegram）、`助手2`（Teams）
- **側欄雙擊名稱即可改名**，Enter 確認立即寫入 yaml，Esc 取消
- 每個助手有獨立工作台（思考流 / 待確認 / ⚙設定）
- ⚙設定 → 身分 tab 可改語氣、查看內部 ID
- 仿人類行為引擎：隨機延遲、每小時/每日訊息上限、工作時間排程

### YAML 外掛技能
- `config/skills/*.yaml` 新增技能，無需修改主程式
- `refresh()` 時熱重載，`list_skills()` 自動合併硬編碼（13 個）+ YAML 動態技能
- Coding Agent 可透過 `skill_catalog` 工具感知新技能

### Headless 瀏覽器自動化
- `src/browser.rs`：`BrowserSession` 封裝 headless_chrome
- `web_navigate` ADK 工具：支援 goto / screenshot 動作
- 截圖自動解碼並顯示在思考流 tab 底部

### Teams 整合（事件驅動，手動啟動）
- **Settings → 系統 → 開始連線 Teams** 按鈕手動觸發，不自動開啟
- 開啟可見 Chrome，第一次需手動完成學校/公司 SSO；後續重啟免登入（`data/teams_profile` 持久化 cookie）
- JavaScript `MutationObserver` + CDP `Runtime.addBinding`，新訊息 < 100ms 偵測
- 自動回「稍等」模板（無需確認）→ 實質草稿進「待確認」tab
- 點「✅ 確認發送」立即送出（P1 即時核准，無需等 60 秒）
- 訊息發送使用 JS `execCommand('insertText')` + Send 按鈕，不依賴 UI 焦點

| Teams 狀態 | 顯示 |
|-----------|------|
| 未連線 | 流程說明 + 「開始連線」按鈕 |
| 等待登入 | 黃色提示，說明 SSO 步驟 |
| 監聽中 | 綠色確認 + 機制說明 |
| 錯誤 | 錯誤訊息 + 重新連線按鈕 |

### WebSocket RPC
- `ws://127.0.0.1:7700`
- 支援：`memory_search` / `call_graph_query` / `trigger_research` / `skill_list`

### MCP Server
- `http://127.0.0.1:7700/mcp`（Streamable HTTP transport，MCP 2024-11-05）
- Claude Desktop 可直接連接，呼叫 Sirin 工具

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
| GUI | egui 0.31 / eframe（原生，無瀏覽器） |
| 非同步 | Tokio 1.37（full features） |
| LLM | Ollama / LM Studio / Gemini（OpenAI-compatible API） |
| Telegram | grammers-client 0.9（MTProto） |
| Teams | headless_chrome 1.0（CDP，事件驅動） |
| HTTP/WS 服務 | axum 0.8 |
| 記憶體 | SQLite FTS5（rusqlite bundled）+ JSONL ring-log + tree-sitter 程式碼索引 |
| 圖片解碼 | image 0.25（PNG） |
| Web 搜尋 | DuckDuckGo HTML scraping + Instant Answer + SearxNG fallback |
| 程式碼解析 | tree-sitter + tree-sitter-rust（call graph、符號索引） |
| 序列化 | serde / serde_json / serde_yaml |
| 平台 | Windows 11（MSVC toolchain） |

---

## 前置需求

- Rust toolchain（`rustup` 安裝，stable channel）
- Microsoft C++ Build Tools（Windows）
- 本機 LLM 服務：Ollama 或 LM Studio
- Teams 功能：Google Chrome（預設路徑 `C:\Program Files\Google\Chrome\`）

---

## 快速開始

```powershell
# 開發模式
cargo run

# Release 版本
cargo build --release
.\target\release\sirin.exe
```

---

## 環境變數

在專案根目錄建立 `.env`：

### Telegram

```env
TG_API_ID=            # 從 https://my.telegram.org 取得
TG_API_HASH=
TG_PHONE=             # 國際格式，例如 +886912345678
TG_GROUP_IDS=         # 監聽的群組 ID，逗號分隔（留空只回覆私訊）
TG_AUTO_REPLY=true
TG_REPLY_PRIVATE=true
TG_REPLY_GROUPS=false
TG_STARTUP_MSG=已啟動 — {time}
```

### LLM

```env
LLM_PROVIDER=lmstudio     # ollama | lmstudio | gemini

# Ollama
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_MODEL=llama3.2

# LM Studio（OpenAI-compatible）
LM_STUDIO_BASE_URL=http://localhost:1234/v1
LM_STUDIO_MODEL=gemma-4-e4b-it

# Gemini
GEMINI_API_KEY=
GEMINI_MODEL=gemini-2.0-flash

# 可選：為不同用途配置獨立模型
ROUTER_MODEL=    # Planner / Router（輕量模型）
CODING_MODEL=    # Coding Agent 專用
LARGE_MODEL=     # 高推理能力任務
```

> **注意**：Settings UI 中選擇的模型會儲存至 `config/llm.yaml`，啟動時自動覆蓋環境變數。

### Web 搜尋

```env
SEARXNG_BASE_URL=http://localhost:8080   # 可選；未設定則用 DuckDuckGo
```

---

## 設定檔

### `config/agents.yaml` — 多助手設定

預設包含兩個助手，名稱可在 UI 側欄雙擊直接改名：

```yaml
agents:
  - id: assistant_1          # 內部 ID，不隨改名變動
    enabled: true
    identity:
      name: 助手1            # 顯示名稱（UI 可直接編輯）
    channel:
      telegram:
        api_id: "${TG_API_ID}"
        api_hash: "${TG_API_HASH}"
        phone: "${TG_PHONE}"
        session_file: "${LOCALAPPDATA}/sirin/assistant_1.session"
        auto_reply: false
        require_confirmation: false
    human_behavior:
      enabled: false
      min_reply_delay_secs: 30
      max_reply_delay_secs: 180

  - id: assistant_2
    enabled: false
    identity:
      name: 助手2
    channel:
      teams:
        webhook_url: "${TEAMS_WEBHOOK_URL}"
    human_behavior:
      enabled: true
      work_schedule:
        utc_offset_hours: 8
        work_start: "09:00"
        work_end: "18:00"
        work_days: [1, 2, 3, 4, 5]
```

### `config/persona.yaml` — 全局人格

```yaml
identity:
  name: 助手1

coding_agent:
  enabled: true
  auto_approve_writes: true
  allowed_commands:
    - "cargo check"
    - "cargo test"
  max_iterations: 10
```

### `config/skills/*.yaml` — 自訂技能

```yaml
id: analyze_pr_diff
name: "PR Diff 分析"
description: "讀取最近的 git diff 並生成摘要"
category: "coding"
enabled: true
requires_approval: false
backed_by_tools:
  - git_log
  - local_file_read
prompt_template: |
  使用 git_log 取得最近 5 個 commit，再生成改動摘要。
```

---

## MCP 整合（Claude Desktop）

Sirin 啟動後自動在 `http://127.0.0.1:7700/mcp` 提供 MCP 服務。

在 `claude_desktop_config.json` 加入：

```json
{
  "mcpServers": {
    "sirin": {
      "url": "http://127.0.0.1:7700/mcp"
    }
  }
}
```

---

## Teams 連線流程

1. **Settings → 系統 → 點「開始連線 Teams」**
2. Chrome 視窗跳出，導覽至 `https://teams.microsoft.com`
3. **有 `data/teams_profile/` cookie** → 自動登入，< 5 秒進入監聽
4. **第一次 / session 過期** → 手動完成學校 SSO / MFA（最多等 5 分鐘）
5. 登入後狀態顯示「● 監聽中」，cookie 永久保存
6. 收到新訊息 → 自動回「稍等」→「待確認」tab 出現草稿
7. 點「✅ 確認發送」→ 立即送出至 Teams

---

## 測試

```bash
# 純單元測試（快，無外部依賴）
cargo test

# 需要 LM Studio 的 live 整合測試
cargo test -- --ignored --nocapture
```

---

## 重要檔案

| 路徑 | 說明 |
|------|------|
| `src/main.rs` | 程式入口：Tokio runtime、egui 視窗、各背景任務啟動 |
| `src/ui.rs` | egui App：多助手工作台、側欄改名、系統設定、Log tab |
| `src/agents/` | Planner / Router / Chat / Coding / Research / Followup |
| `src/adk/` | ADK 核心：Agent trait、ToolRegistry（24 個工具）、AgentRuntime |
| `src/telegram/` | Telegram listener / handler / reply |
| `src/teams/mod.rs` | Teams 事件驅動主循環（即時核准通道）|
| `src/teams/browser_client.rs` | Chrome CDP 封裝（profile 持久化、JS 發送）|
| `src/browser.rs` | headless_chrome BrowserSession 封裝 |
| `src/skills.rs` | 硬編碼技能（13 個）+ Web 搜尋引擎 |
| `src/skill_loader.rs` | YAML 動態技能掃描與快取 |
| `src/rpc_server.rs` | WebSocket RPC server |
| `src/mcp_server.rs` | MCP HTTP server（/mcp endpoint） |
| `src/researcher.rs` | 五階段背景調研 pipeline |
| `src/code_graph.rs` | tree-sitter call graph |
| `src/memory.rs` | 三層記憶：SQLite FTS5 / 程式碼索引 / JSONL |
| `src/llm.rs` | LLM 呼叫層 + `LlmUiConfig`（config/llm.yaml） |
| `src/events.rs` | Broadcast event bus |
| `src/pending_reply.rs` | 人工確認流程（Telegram + Teams 共用）|
| `src/human_behavior.rs` | 仿人類行為引擎 |
| `src/agent_config.rs` | AgentsFile / AgentConfig（YAML 序列化）|
| `config/agents.yaml` | 多助手實例設定 |
| `config/persona.yaml` | 全局人格與 Coding Agent 設定 |
| `config/skills/` | YAML 自訂技能目錄 |
| `config/llm.yaml` | UI 儲存的模型選擇（覆蓋環境變數）|

---

## 資料儲存路徑（Windows）

| 內容 | 路徑 |
|------|------|
| 任務追蹤 JSONL | `%LOCALAPPDATA%\Sirin\tracking\task.jsonl` |
| 調研記錄 JSONL | `%LOCALAPPDATA%\Sirin\tracking\research.jsonl` |
| 記憶 SQLite | `%LOCALAPPDATA%\Sirin\memory.db` |
| 程式碼 call graph | `%LOCALAPPDATA%\Sirin\call_graph.jsonl` |
| per-peer 對話 | `%LOCALAPPDATA%\Sirin\context\<peer_id>.jsonl` |
| Teams 草稿 | `data/pending_replies/teams.jsonl` |
| Telegram 草稿 | `data/pending_replies/<agent_id>.jsonl` |
| Telegram session | `data/sessions/<agent_id>.session` 或 `%LOCALAPPDATA%\sirin\<agent_id>.session` |
| Teams Chrome profile | `data/teams_profile/` |
