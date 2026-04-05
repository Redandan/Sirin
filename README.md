# Sirin

Sirin 是一個純 Rust 桌面 AI 代理。後端 Tokio 背景任務處理 Telegram 監聽、任務追蹤、LLM 自動回覆與調研 pipeline；前端使用 egui 原生 GUI，無 WebView、無 Node.js。

## 技術組成

- **GUI**：egui / eframe（原生 Rust，無瀏覽器）
- **後端**：Rust + Tokio（非同步背景任務）
- **Agent Runtime**：ADK-RUST 風格 `Agent / Context / Tool / Runner` 分層（`src/adk/`, `src/agents/`）
  - **Planner** → 分析使用者意圖，產生 `IntentFamily`（Capability / LocalFile / CodeAnalysis / Research / GeneralChat 等）與推薦技能清單
  - **Router** → 依 Planner 結果將請求分派給 Chat、Research 或 Coding agent
  - **Chat Agent** → 持有 planner 提示、記憶與對話 context，產生回覆
  - **Coding Agent** → ReAct 迴圈（Reason + Act）讀取、修改、驗證程式碼，支援 `cargo check`
  - **Research Agent** → 多階段背景調研，可結合網路搜尋
  - **Follow-up Agent** → 定期觸發 LLM follow-up
- **Telegram**：grammers-client（MTProto），含 `handler` / `reply` 分層與語言偵測
- **LLM**：Ollama 或 LM Studio（本機模型，支援 streaming）
- **Web Search**：DuckDuckGo HTML / Instant Answer + SearxNG fallback（`src/skills.rs`）
- **記憶（三層）**：
  1. 全文記憶索引（JSONL，`memory_store` / `memory_search`，零外部依賴）
  2. 專案程式碼索引（tree-sitter 掃描 `.rs`，提供架構感知摘要，`src/memory.rs`）
  3. per-peer 對話 context（JSONL ring-log，`append_context` / `load_recent_context`）
- **Call Graph**：tree-sitter 解析 Rust 符號與呼叫關係，支援跨檔案反向查詢（`src/code_graph.rs`）

## 前置需求

- Rust toolchain（`rustup` 安裝）
- 本機 LLM 服務：Ollama 或 LM Studio

Windows 上需要 Microsoft C++ Build Tools。

## 啟動

```powershell
cargo run
```

Release 版本：

```powershell
cargo build --release
.\target\release\sirin.exe
```

## 環境變數

在專案根目錄建立 `.env`，沒有 `.env` 不會中止，但 Telegram 與 LLM 功能會停用。

### Telegram

```env
TG_API_ID=            # 從 https://my.telegram.org 取得
TG_API_HASH=
TG_PHONE=             # 國際格式，例如 +886...
TG_GROUP_IDS=         # 要監聽的群組 ID，逗號分隔（留空只回覆私訊）
TG_AUTO_REPLY=true
TG_REPLY_PRIVATE=true
TG_REPLY_GROUPS=false
TG_STARTUP_MSG=Sirin 已啟動 — {time}
TG_STARTUP_TARGET=    # 啟動通知發送的 username（留空發給自己）
TG_REQUIRE_LOGIN=1    # 設為 1 才啟用登入流程
```

### LLM

```env
LLM_PROVIDER=lmstudio     # ollama | lmstudio

# Ollama
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_MODEL=llama3.2

# LM Studio（OpenAI-compatible）
LM_STUDIO_BASE_URL=http://localhost:1234/v1
LM_STUDIO_MODEL=llama-3.2-3b-instruct-uncensored
# LM_STUDIO_API_KEY=optional
```

### Web Search

```env
SEARXNG_BASE_URL=http://localhost:8080   # 可選，設定後優先使用 SearxNG；未設定則用 DuckDuckGo
```

### 其他

```env
FOLLOWUP_INTERVAL_SECS=1800   # follow-up worker 週期（秒）
TASK_LOG_MAX_LINES=2000        # task.jsonl 上限行數
```

## 重要檔案

| 路徑 | 說明 |
|------|------|
| `src/main.rs` | 程式入口，啟動 Tokio runtime 與 egui 視窗，初始化程式碼索引 |
| `src/adk/` | ADK-RUST 核心：`Agent`、`Context`、`ToolRegistry`、`AgentRuntime` |
| `src/agents/` | 六個 agent 實作：planner / router / chat / coding / research / followup |
| `src/ui.rs` | egui App（四個 tab：任務板、調研、Telegram、對話），含 Agent Console |
| `src/log_buffer.rs` | 全域 log 環形緩衝，供 GUI 底部 Log 面板讀取 |
| `src/telegram/` | Telegram listener、handler/reply 分層、AI 回覆、語言偵測（CJK / 意圖分類）|
| `src/telegram_auth.rs` | Telegram 登入流程狀態機（OTP 輸入、session 管理）|
| `src/skills.rs` | Web search 實作：DuckDuckGo HTML scraping + Instant Answer + SearxNG |
| `src/code_graph.rs` | tree-sitter Rust call graph：符號解析、反向索引、跨重啟持久化 |
| `src/followup.rs` | LLM follow-up worker（直接呼叫，非 ADK 路徑）|
| `src/researcher.rs` | 多階段背景調研 pipeline |
| `src/llm.rs` | 共用 LLM 呼叫層（Ollama / LM Studio，支援 streaming）|
| `src/memory.rs` | 三層記憶：全文索引、程式碼索引、per-peer 對話 context |
| `src/persona.rs` | Persona 載入、TaskTracker、CodingAgentConfig |
| `config/persona.yaml` | 人格設定 |
