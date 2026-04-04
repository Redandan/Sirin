# Sirin 架構說明

本文件描述目前程式碼實際對應的架構。

## 1. 系統總覽

Sirin 是一個**單一 Rust binary**，整合 egui 原生 GUI 與 Tokio 非同步後端。沒有 WebView、沒有 Node.js、沒有 IPC 序列化層。

```
┌─────────────────────────────────────────────┐
│               sirin.exe（單一進程）           │
│                                             │
│  ┌──────────────┐   直接呼叫   ┌──────────┐ │
│  │  egui GUI    │ ←─────────→ │  共享狀態 │ │
│  │  (主執行緒)   │             │  Arc/Mutex│ │
│  └──────────────┘             └──────────┘ │
│                                     ↑       │
│  ┌──────────────────────────────────┘       │
│  │  Tokio 背景任務（spawn）                  │
│  │  - telegram::run_listener                │
│  │  - followup::run_worker                  │
│  │  - background_loop（heartbeat）          │
│  └──────────────────────────────────────────│
└─────────────────────────────────────────────┘
```

## 2. 執行緒模型

| 執行緒 | 負責 |
|--------|------|
| 主執行緒 | eframe 事件迴圈（egui 渲染） |
| Tokio worker pool | Telegram listener、follow-up worker、research pipeline |

egui 的 `update()` 每 5 秒從 JSONL 檔案讀取最新狀態並重繪，無需跨執行緒 channel。

## 3. 模組責任

### `src/main.rs`
- 載入 `.env`
- 建立共享狀態（`TaskTracker`、`TelegramAuthState`）
- 建立 `tokio::runtime::Runtime`，spawn 背景任務
- 呼叫 `eframe::run_native`（接管主執行緒）

### `src/ui.rs`
- egui App，三個 tab：
  - **任務板**：讀取最近 200 筆 `task.jsonl`，顯示事件 / 狀態
  - **調研**：啟動新調研、顯示 `research.jsonl` 歷史
  - **Telegram**：顯示連線狀態，CodeRequired / PasswordRequired 時顯示輸入框
- 每 5 秒自動刷新，關閉視窗改為最小化（背景持續運行）

### `src/telegram/`
拆分為四個子模組：

| 子模組 | 內容 |
|--------|------|
| `mod.rs` | 連線、授權流程、主訊息迴圈 |
| `llm.rs` | Prompt 建構、AI 回覆生成 |
| `commands.rs` | 指令解析（任務建立、調研意圖偵測）|
| `language.rs` | CJK 偵測、混語判斷、中文保底回覆 |
| `config.rs` | `TelegramConfig` 與 `.env` 解析 |

**回覆策略（三層）：**
1. LLM 生成 1-3 句自然回覆（同語言）
2. 若使用者用中文但 AI 回非中文 → 強制繁中重試
3. 仍失敗 → 中文保底句型

**調研閉環（新）：**
- 偵測「調研/研究」意圖 → 背景 spawn `researcher::run_research`
- 完成後透過同一 Telegram 連線回報摘要給原發訊者
- 相關研究結果（keyword match）會注入後續回覆的 prompt

### `src/followup.rs`
- 週期性（預設 30 分鐘）掃描近期 `PENDING` 任務
- 透過本機 LLM 判斷是否需跟進
- 每 10 個週期自動 trim `task.jsonl`（上限由 `TASK_LOG_MAX_LINES` 控制）

### `src/researcher.rs`
多階段背景調研 pipeline：
1. Fetch URL 頁面內容（可選）
2. LLM overview 分析
3. 生成 4 個研究問題
4. 每題 DDG 搜尋 + LLM 解答
5. Synthesis 最終報告

任務狀態持久化至 `research.jsonl`；GUI 每 5 秒讀取最新狀態。

### `src/llm.rs`
共用 LLM 呼叫層：
- `LlmBackend` enum：`Ollama` | `LmStudio`
- `LlmConfig::from_env()` 解析 `.env`
- `call_prompt(client, llm, prompt)` 統一入口

### `src/memory.rs`
- LanceDB 本地向量記憶：`add_to_memory` / `search_memory`（目前為保留能力，尚未串入主流程）
- JSONL 對話 context：`append_context` / `load_recent_context`

### `src/persona.rs`
- `Persona`：載入 `config/persona.yaml`
- `TaskTracker`：`task.jsonl` 讀寫、狀態更新、`trim_to_max`

### `src/skills.rs`
- `ddg_search`：零金鑰 DuckDuckGo 搜尋
- `execute_skill`：skill 執行（目前無事件廣播，直接回傳結果）

## 4. 資料儲存

優先使用 `%LOCALAPPDATA%/Sirin/tracking/`；fallback 到工作目錄 `data/tracking/`。

| 檔案 | 說明 |
|------|------|
| `task.jsonl` | 任務追蹤（append-only，定期 trim）|
| `research.jsonl` | 調研任務與報告 |
| `sirin_context.jsonl` | 對話 context（最近 N 輪）|
| `sirin.session` | Telegram MTProto session |
| `data/sirin_memory` | LanceDB 向量記憶 |

## 5. 主要資料流

### Telegram 訊息處理
```
收到訊息
  → 解析指令 / 調研意圖 / 一般對話
  → 載入對話 context + web search（可選）
  → 載入相關研究結果（keyword match）
  → LLM 生成回覆
  → 語言修正（必要時）
  → 發送回覆 + 記錄 context
```

### 調研任務閉環
```
使用者發「調研 <主題>」
  → spawn researcher::run_research（背景）
  → 立即回覆「已啟動調研任務」
  → 研究完成後 → 透過 Telegram 回報摘要
  → 後續對話可引用研究結果
```

## 6. 已知限制

- LanceDB 向量記憶尚未串入回覆流程（用 keyword match 替代）
- skills 為輕量實作，無 event bus
- egui 無系統托盤（tray），關閉視窗改為最小化
