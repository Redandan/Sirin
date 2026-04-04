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

egui 的 `update()` 每 5 秒從 JSONL 檔案讀取最新狀態並重繪。
Chat tab 透過 `std::sync::mpsc` channel 從 Tokio 任務接收 LLM 回覆。

## 3. 模組責任

### `src/main.rs`
- 載入 `.env`
- 建立共享狀態（`TaskTracker`、`TelegramAuthState`）
- 建立 `tokio::runtime::Runtime`，spawn 背景任務
- 呼叫 `eframe::run_native`（接管主執行緒）

### `src/ui.rs`
- egui App，四個 tab：
  - **任務板**：讀取最近 200 筆 `task.jsonl`，顯示事件 / 狀態
  - **調研**：啟動新調研、顯示 `research.jsonl` 歷史
  - **Telegram**：顯示連線狀態，CodeRequired / PasswordRequired 時顯示輸入框
  - **對話**：直接與本機 LLM 對話（Enter 送出，Shift+Enter 換行）
- 底部 **Log 面板**：即時顯示後端日誌，顏色按模組分類
- 每 5 秒自動刷新，關閉視窗改為最小化（背景持續運行）

### `src/log_buffer.rs`
- 全域靜態環形緩衝（最多 300 行）
- `sirin_log!(...)` macro：同時寫入 stderr 和緩衝
- GUI 每幀呼叫 `log_buffer::recent(200)` 渲染 Log 面板
- telegram、researcher、followup 模組均使用 `sirin_log!` 取代 `eprintln!`

### `src/telegram/`
拆分為四個子模組：

| 子模組 | 內容 |
|--------|------|
| `mod.rs` | 連線、授權流程、主訊息迴圈 |
| `llm.rs` | Prompt 建構、AI 回覆生成 |
| `commands.rs` | 指令解析（任務建立、調研意圖偵測、LLM 搜尋 query 提取）|
| `language.rs` | CJK 偵測、混語判斷、中文保底回覆 |
| `config.rs` | `TelegramConfig` 與 `.env` 解析 |

**回覆策略（三層）：**
1. LLM 生成 1-3 句自然回覆（同語言）
2. 若使用者用中文但 AI 回非中文 → 強制繁中重試
3. 仍失敗 → 中文保底句型

**搜尋 query 最佳化：**
- 收到疑問句時，先呼叫 LLM 提取簡潔 query（至多 8 個字），再送 DDG 搜尋
- 搜尋結果注入回覆 prompt

**對話 context 隔離：**
- 每個 peer（Telegram 用戶或群組）有獨立的 context 檔案
- 檔案命名：`sirin_context_{peer_id}.jsonl`
- GUI 對話 tab 使用 `peer_id=0`

### `src/followup.rs`
- 週期性掃描近期 `PENDING` 任務
- Prompt 包含 persona objectives 與任務內容（`message_preview`）
- 透過本機 LLM 判斷是否需跟進
- 每 10 個週期自動 trim `task.jsonl`

### `src/researcher.rs`
多階段背景調研 pipeline：
1. Fetch URL 頁面內容（可選）
2. LLM overview 分析
3. 生成 4 個研究問題
4. 每題 DDG 搜尋 + LLM 解答
5. Synthesis 最終報告

調研完成後透過 Telegram 發送摘要給原發訊者。

### `src/llm.rs`
共用 LLM 呼叫層：
- `LlmBackend` enum：`Ollama` | `LmStudio`
- `LlmConfig::from_env()` 解析 `.env`
- `call_prompt(client, llm, prompt)` 統一入口

### `src/memory.rs`
**全文記憶索引（TF 評分）：**
- `memory_store(text, source)` → 寫入 `data/memory/index.jsonl`
- `memory_search(query, limit)` → TF 評分，返回最相關結果
- 支援 CJK 逐字 tokenization + ASCII 單字 tokenization
- 零外部依賴（純 Rust + JSONL）

**per-peer 對話 context：**
- `append_context(user_msg, reply, peer_id)` → 寫入 per-peer JSONL
- `load_recent_context(limit, peer_id)` → 讀取最近 N 輪

### `src/persona.rs`
- `Persona`：載入 `config/persona.yaml`
- `TaskTracker`：`task.jsonl` 讀寫、狀態更新、`trim_to_max`

### `src/skills.rs`
- `ddg_search`：零金鑰 DuckDuckGo 搜尋

## 4. 資料儲存

優先使用 `%LOCALAPPDATA%/Sirin/`；fallback 到工作目錄。

| 檔案 | 說明 |
|------|------|
| `tracking/task.jsonl` | 任務追蹤（append-only，定期 trim）|
| `tracking/research.jsonl` | 調研任務與報告 |
| `tracking/sirin_context_{id}.jsonl` | per-peer 對話 context（最近 N 輪）|
| `tracking/sirin.session` | Telegram MTProto session |
| `memory/index.jsonl` | 全文記憶索引 |

## 5. 主要資料流

### Telegram 訊息處理
```
收到訊息
  → 解析指令 / 調研意圖 / 一般對話
  → 載入 per-peer 對話 context（最近 5 輪）
  → LLM 提取搜尋 query → DDG 搜尋（可選）
  → keyword match 注入研究報告（可選）
  → LLM 生成回覆
  → 語言修正（必要時）
  → 發送回覆 + 記錄 per-peer context
```

### 調研任務閉環
```
使用者發「調研 <主題>」
  → spawn researcher::run_research（背景）
  → 立即回覆「已啟動調研任務」
  → 研究完成後 → 透過 Telegram 回報摘要
  → 後續對話可引用研究結果
```

### GUI 對話 Tab
```
用戶在 Chat tab 輸入 → Enter 送出
  → push user message 到 chat_messages
  → spawn tokio task（call_prompt）
  → mpsc::SyncSender 傳回 reply
  → 主執行緒 try_recv() 接收 → push assistant message
```

## 6. 已知限制

- `memory_search` 使用 TF 評分（非語意向量），語意相似但字面不同的查詢匹配率有限
- skills 為輕量實作（僅 ddg_search）
- egui 無系統托盤（tray），關閉視窗改為最小化
