# Sirin

純 Rust 跨平台 AI Agent 平台。Tokio 非同步後端負責多平台訊息監聽、任務追蹤、LLM 自動回覆與多階段調研；前端使用 egui 0.31 immediate mode UI（Desktop 原生），零 WebView、零 Node.js、零外部 C 依賴（SQLite 靜態編入）。

---

## 架構總覽

```
egui Immediate Mode UI (Desktop native)
   │
   ├── Sidebar：助手清單 + 導航
   ├── Agent Workspace（概覽 / 待確認 tabs）
   ├── Settings（Agent 設定 + 系統診斷 + AI 修復）
   ├── Log（即時日誌 + filter）
   ├── Workflow（Skill 開發 pipeline）
   ├── Meeting（多 Agent 會議室）
   └── Browser（即席瀏覽器控制 + 截圖 + JS eval）

Tokio Background Tasks
   │
   ├── Telegram Worker (src/telegram/)
   ├── Teams Worker (src/teams/)
   ├── Follow-up Worker (src/followup.rs)
   ├── Test Runner (src/test_runner/)        ← AI 驅動瀏覽器測試
   ├── MCP Client (src/mcp_client.rs)
   ├── Events Bus (src/events.rs)
   │
   ├── RPC Server  ws://127.0.0.1:7700/
   └── MCP Server  http://127.0.0.1:7700/mcp  ← 14 個 tools 對外
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

### AI 瀏覽器測試（test_runner）
- **Goal-driven**：YAML 寫高階目標，LLM 用 ReAct loop 驅動瀏覽器達成
- **35+ 瀏覽器動作**：navigate / click / type / read / eval / wait / scroll / coordinate-click / hover / cookies / network / console / multi-tab
- **Vision 整合**：Gemini multimodal 可讀截圖 — 即使 Flutter CanvasKit 無 DOM 也能測
- **自動三角分類**：失敗自動歸類為 ui_bug / api_bug / flaky / env / obsolete
- **Auto-fix 驗證迴圈**：spawn `claude` CLI 修代碼 → **重跑測試** → 標 verified / regressed
- **SQLite 歷史**：test_runs + test_knowledge + auto_fix_history（含 verification）
- **i18n**：zh-TW / en / zh-CN 三語 prompt
- **去重斷路器**：30 分鐘內重複失敗不再 spawn Claude；連續 3 次修復失敗自動停手

### 瀏覽器自動化（browser.rs）
- 持久化 Chrome session（singleton + auto-recover from dead connection）
- Tier 1：DOM 操作（click/type/read/wait/exists/attr/scroll/keyboard/select）
- Tier 2：座標操作（click_point/hover_point/screenshot_element）— Flutter Canvas 適用
- Tier 3：multi-tab、cookies、network intercept、file upload、iframe、drag、PDF、HTTP auth、localStorage

### MCP Server
- `http://127.0.0.1:7700/mcp`（MCP 2024-11-05 Streamable HTTP）
- Claude Desktop 可直接連接，14 個 tools

| 工具 | 說明 |
|------|------|
| `memory_search` | 搜尋記憶庫 |
| `skill_list` | 列出所有技能 |
| `teams_pending` / `teams_approve` | Teams 草稿管理 |
| `trigger_research` | 啟動調研任務 |
| `list_tests[tag?]` | 列出 `config/tests/` 下所有測試 |
| `run_test_async(test_id, auto_fix?)` | 非同步啟動 YAML 測試 |
| **`run_adhoc_test(url, goal, ...)`** | 即席測試任意 URL，無需建 YAML |
| `get_test_result(run_id)` | 輪詢測試狀態 |
| `get_screenshot(run_id)` | 失敗截圖（base64 PNG）|
| `get_full_observation(run_id, step)` | 完整未截斷的 tool output |
| `list_recent_runs(test_id?)` | 歷史測試執行 |
| `list_fixes(test_id?)` | auto-fix 歷史含 verification 結果 |
| `config_diagnostics()` | Sirin 自我健康檢查 |
| `browser_exec(action, ...)` | 即席瀏覽器操作（不必走完整 goal）|

外部 Claude Code 可透過 `.claude/skills/sirin-launch/SKILL.md` + `sirin-test/SKILL.md` 自主啟動 + 使用。

**完整 MCP API 參考：** [`docs/MCP_API.md`](docs/MCP_API.md)

## 給開發者（包含 AI session 接手開發）

如果你要修改 Sirin 本身（不是用 Sirin 測別的 app），讀：

1. [`CLAUDE.md`](CLAUDE.md) — 架構決策、不可重啟議的選擇
2. [`.claude/skills/sirin-dev/SKILL.md`](.claude/skills/sirin-dev/SKILL.md) — 開發工作流、加新動作的 4 處改動清單、已踩過的雷
3. [`docs/test-runner-roadmap.md`](docs/test-runner-roadmap.md) — 進度與已拒絕的提案
4. [`~/.claude/broadcasts/2026-04-*-sirin-*.md`](~/.claude/broadcasts/) — 上一個 session 的最新狀態

`sirin-dev` skill 會在 Claude session 進入 Sirin repo 時自動載入，不必手動讀。

---

## 技術堆疊

| 層 | 技術 |
|---|---|
| GUI | egui 0.31（immediate mode，Desktop 原生） |
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
| `config/tests/*.yaml` | AI 瀏覽器測試 goal 定義 |
| `.claude/skills/sirin-*/SKILL.md` | 給外部 Claude Code 用的 skill 文件 |

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
| `src/main.rs` | 程式入口：Tokio runtime、egui 視窗、背景任務啟動 |
| `src/ui_egui/` | egui UI：sidebar、workspace、settings、log、workflow、meeting、browser |
| `src/agents/` | Planner / Router / Chat / Coding / Research agent |
| `src/adk/` | ADK 核心：Agent trait、ToolRegistry、AgentRuntime |
| `src/telegram/` | Telegram listener / handler / reply |
| `src/teams/` | Teams CDP 事件驅動 + AI 草稿生成 |
| `src/browser.rs` | 持久化 Chrome session（35+ CDP 操作 + auto-recover）|
| `src/test_runner/` | AI 測試 runner（parser / executor / triage / store / runs / i18n）|
| `src/claude_session.rs` | spawn `claude` CLI 修 bug |
| `src/config_check.rs` | 配置診斷 + AI 修復（dual-confirm）|
| `src/mcp_client.rs` | MCP Client：連接外部 Server、發現工具、代理呼叫 |
| `src/mcp_server.rs` | MCP Server（/mcp endpoint，14 tools） |
| `src/memory/` | SQLite FTS5 + 程式碼索引 + per-peer 對話 |
| `src/llm/` | LLM 抽象層（Ollama/LM Studio/Gemini/Claude）+ vision multimodal |
| `src/persona/` | 人格設定 + ROI 行為引擎 + TaskTracker |
| `src/pending_reply.rs` | 人工確認流程（併發安全） |
| `src/events.rs` | Broadcast event bus |
| `src/error.rs` | 統一錯誤類型（thiserror） |

---

## 資料儲存路徑（Windows）

| 內容 | 路徑 |
|------|------|
| 任務追蹤 | `%LOCALAPPDATA%\Sirin\tracking\task.jsonl` |
| 調研記錄 | `%LOCALAPPDATA%\Sirin\tracking\research.jsonl` |
| 記憶 SQLite | `%LOCALAPPDATA%\Sirin\memory\memories.db` |
| 測試歷史 SQLite | `%LOCALAPPDATA%\Sirin\memory\test_memory.db` |
| per-peer 對話 | `%LOCALAPPDATA%\Sirin\context\<peer_id>.jsonl` |
| 待確認草稿 | `data/pending_replies/<agent_id>.jsonl` |
| Telegram session | `data/sessions/<agent_id>.session` |
| Teams Chrome profile | `data/teams_profile/` |
| 測試失敗截圖 | `data/test_failures/<test_id>_<timestamp>.png` |

---

## AI 瀏覽器測試 — 快速範例

### 寫一個測試 goal

`config/tests/login_smoke.yaml`：
```yaml
id: login_smoke
name: "Login flow smoke test"
url: "https://app.example.com/login"
goal: |
  測試使用者可以用 email 註冊並進入 dashboard
locale: zh-TW           # 或 en / zh-CN
max_iterations: 15
timeout_secs: 120
url_query:              # 選填 — Flutter HTML renderer 等
  flutter-web-renderer: html
success_criteria:
  - "URL 含 /dashboard"
  - "頁面顯示歡迎訊息"
tags: [smoke, auth]
```

### 透過 MCP 觸發（外部 Claude Code）

```bash
# 1. 啟動測試（非阻塞）
curl -s http://127.0.0.1:7700/mcp -X POST \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"run_test_async",
                 "arguments":{"test_id":"login_smoke","auto_fix":true}}}'

# 2. 輪詢狀態
curl ... -d '{"jsonrpc":"2.0","id":2,"method":"tools/call",
              "params":{"name":"get_test_result",
                        "arguments":{"run_id":"run_..."}}}'

# 3. 失敗時抓截圖
curl ... -d '{"params":{"name":"get_screenshot","arguments":{"run_id":"..."}}}'
```

### Ad-hoc 測試（不必先建 YAML）

```bash
curl ... -d '{"params":{"name":"run_adhoc_test",
              "arguments":{
                "url":"https://example.com",
                "goal":"驗證頁面顯示 Example Domain 標題",
                "success_criteria":["頁面包含 Example Domain"]
              }}}'
```

### Flutter CanvasKit 測試

CanvasKit 應用 DOM 是空的 — 改用 vision：
```yaml
goal: |
  ⚠️ Flutter CanvasKit app — DOM 是空的
  改用 screenshot_analyze action，讓 vision LLM 直接讀截圖
success_criteria:
  - "Vision 確認頁面顯示登入表單"
```
詳見 `.claude/skills/sirin-test/SKILL.md` 的「Flutter Web apps」章節。
