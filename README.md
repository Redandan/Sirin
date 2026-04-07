# Sirin

Sirin 是一個純 Rust 桌面 AI Agent。Tokio 背景任務負責 Telegram 監聽、任務追蹤、LLM 自動回覆與多階段調研 pipeline；前端使用 egui 原生 GUI，零 WebView、零 Node.js、零外部 C 依賴（SQLite 靜態編入）。

---

## 架構總覽

```
egui (UI thread)
   │
   ├── AgentConsole (src/ui.rs)
   │       └── AgentRuntime ──► Planner → Router ──► Chat Agent
   │                                               ├── Coding Agent  (ReAct loop)
   │                                               └── Research Agent
   │
   ├── Telegram Worker (src/telegram/)
   │       └── handler → intent classify → LLM reply / research trigger
   │
   ├── Follow-up Worker (src/followup.rs)
   │       └── periodic LLM evaluation of tracked tasks
   │
   └── Events Bus (src/events.rs)          (broadcast channel, inter-agent)
```

**Agent pipeline**：`Planner` 分析意圖 → `Router` 按 `IntentFamily` 派發 → 對應 agent 執行並發佈事件。

---

## 技術堆疊

| 層 | 技術 |
|---|---|
| GUI | egui 0.31 / eframe（原生，無瀏覽器） |
| 非同步 | Tokio 1.37（full features） |
| LLM | Ollama 或 LM Studio（本機，OpenAI-compatible API） |
| Telegram | grammers-client 0.9（MTProto） |
| 記憶體 | SQLite FTS5（rusqlite bundled）+ JSONL ring-log + tree-sitter 程式碼索引 |
| Web 搜尋 | DuckDuckGo HTML scraping + Instant Answer + SearxNG fallback |
| 程式碼解析 | tree-sitter + tree-sitter-rust（call graph、符號索引） |
| 序列化 | serde / serde_json / serde_yaml |
| 平台 | Windows 11（MSVC toolchain） |

---

## 前置需求

- Rust toolchain（`rustup` 安裝，stable channel）
- Microsoft C++ Build Tools（Windows）
- 本機 LLM 服務：Ollama 或 LM Studio

> Telegram 與 LLM 功能可選；沒有 `.env` 或未設定相關變數時，對應功能自動停用，GUI 仍可正常啟動。

---

## 快速開始

```powershell
# 開發模式
cargo run

# Release 版本（LTO + strip，體積最小）
cargo build --release
.\target\release\sirin.exe
```

---

## 環境變數

在專案根目錄建立 `.env`（或設定系統環境變數）：

### Telegram

```env
TG_API_ID=            # 從 https://my.telegram.org 取得
TG_API_HASH=
TG_PHONE=             # 國際格式，例如 +886912345678
TG_GROUP_IDS=         # 監聽的群組 ID，逗號分隔（留空只回覆私訊）
TG_AUTO_REPLY=true    # 啟用 AI 自動回覆
TG_REPLY_PRIVATE=true
TG_REPLY_GROUPS=false
TG_STARTUP_MSG=Sirin 已啟動 — {time}
TG_STARTUP_TARGET=    # 啟動通知目標 username（留空發給自己）
TG_REQUIRE_LOGIN=1    # 設為 1 才啟用 OTP 登入流程（有 session 後可移除）
```

### LLM

```env
LLM_PROVIDER=lmstudio     # ollama | lmstudio

# Ollama
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_MODEL=llama3.2

# LM Studio（OpenAI-compatible）
LM_STUDIO_BASE_URL=http://localhost:1234/v1
LM_STUDIO_MODEL=gemma-4-e4b-it
# LM_STUDIO_API_KEY=optional

# 可選：為不同用途配置獨立模型
LLM_ROUTER_MODEL=          # Planner / Router 用（輕量模型加速路由）
LLM_CODING_MODEL=          # Coding Agent 專用
LLM_LARGE_MODEL=           # 需要高推理能力時使用
```

### Web 搜尋

```env
SEARXNG_BASE_URL=http://localhost:8080   # 可選；未設定則用 DuckDuckGo
```

### 其他

```env
FOLLOWUP_INTERVAL_SECS=1800   # follow-up worker 週期（秒，預設 1800）
TASK_LOG_MAX_LINES=2000        # task.jsonl 上限行數
```

---

## Persona 設定

編輯 `config/persona.yaml` 調整 AI 人格與 Coding Agent 行為：

```yaml
identity:
  name: Sirin

response_style:
  voice: 自然、親切、口吻

objectives:
  - Monitor Agora
  - Maintain VIPs

coding_agent:
  enabled: true
  auto_approve_reads: true
  auto_approve_writes: true   # false = dry-run 模式，不寫入磁碟
  allowed_commands:
    - "cargo check"
    - "cargo test"
    - "cargo build --release"
  max_iterations: 10
  max_file_write_bytes: 102400
```

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
| `src/main.rs` | 程式入口：啟動 Tokio runtime、egui 視窗、程式碼索引初始化 |
| `src/adk/` | ADK-RUST 核心：`Agent` trait、`AgentContext`、`ToolRegistry`、`AgentRuntime` |
| `src/agents/` | 六個 agent：planner / router / chat / coding / research / followup |
| `src/ui.rs` | egui App（任務板、調研、Telegram、對話四 tab），Agent Console |
| `src/telegram/` | Telegram listener / handler / reply / language 分層 |
| `src/telegram_auth.rs` | OTP 登入流程狀態機 |
| `src/skills.rs` | Web search：DDG HTML + Instant Answer + SearxNG |
| `src/researcher.rs` | 五階段背景調研 pipeline（fetch → overview → questions → search → synthesis） |
| `src/code_graph.rs` | tree-sitter call graph：符號解析、反向索引、跨重啟 JSONL 持久化 |
| `src/memory.rs` | 三層記憶：SQLite FTS5 / 程式碼索引 / per-peer JSONL ring-log |
| `src/llm.rs` | 共用 LLM 呼叫層（Ollama / LM Studio，支援 streaming，無逾時限制） |
| `src/persona.rs` | Persona 載入、TaskTracker、CodingAgentConfig |
| `src/events.rs` | Broadcast event bus（ResearchCompleted / CodingAgentCompleted 等） |
| `src/followup.rs` | Follow-up worker（定期 LLM 評估追蹤中的任務） |
| `config/persona.yaml` | 人格與 Coding Agent 設定 |

---

## 資料儲存路徑（Windows）

| 內容 | 路徑 |
|------|------|
| Telegram session | `%LOCALAPPDATA%\Sirin\sirin.session` |
| 任務追蹤 JSONL | `%LOCALAPPDATA%\Sirin\tracking\task.jsonl` |
| 調研記錄 JSONL | `%LOCALAPPDATA%\Sirin\tracking\research.jsonl` |
| 記憶 SQLite | `%LOCALAPPDATA%\Sirin\memory.db` |
| 程式碼 call graph | `%LOCALAPPDATA%\Sirin\call_graph.jsonl` |
| per-peer 對話 | `%LOCALAPPDATA%\Sirin\context\<peer_id>.jsonl` |
