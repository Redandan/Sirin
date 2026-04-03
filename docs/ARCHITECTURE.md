# Sirin 架構說明（Current Architecture）

本文件描述目前 `Sirin` 專案「已經存在且可對應到程式碼」的架構，不含未落地的目標設計。

## 1. 系統總覽

Sirin 是一個 **Tauri 2 桌面殼 + Next.js UI + Rust 背景服務** 的混合式本機代理。

- UI 層：Next.js 任務看板（Task Board）
- 應用層：Tauri commands 作為前後端橋接
- 背景層：Telegram listener、Follow-up worker、Research pipeline
- 儲存層：JSONL 日誌 + LanceDB 向量記憶（本機）

## 2. 執行拓樸

### 2.1 前端

- 入口：`app/page.tsx`
- 核心元件：`components/task-board.tsx`
- 前端以 `@tauri-apps/api/core` 的 `invoke()` 呼叫 Rust commands。

### 2.2 Tauri Host（Rust）

- 入口：`src/main.rs`
- 啟動時註冊 command：
  - `read_tasks`
  - `list_skills`
  - `approve_task`
  - `search_web`
  - `get_context`
  - `clear_context`
  - `start_research`
  - `get_research_status`
  - `list_research_tasks`
- 啟動後常駐背景任務：
  - `background_loop()`（每 60 秒 heartbeat）
  - `telegram::run_listener(...)`
  - `followup::run_worker(...)`

### 2.3 UI 與桌面殼連線

- Tauri dev：`tauri.conf.json`
  - `devUrl = http://localhost:3000`
  - `beforeDevCommand = npm run dev`
- Tauri build：
  - `beforeBuildCommand = npm run build`
  - `frontendDist = dist`
- Next.js 設定：`next.config.mjs`
  - `output = export`
  - `distDir = dist`

## 3. 模組責任

### 3.1 `src/persona.rs`

- 載入 `config/persona.yaml`
- 定義 persona 身分、目標、ROI 閥值、回覆語氣
- 提供 `BehaviorEngine` 判斷 Action Tier：
  - `Ignore`
  - `LocalProcess`
  - `Escalate`
- 提供 `TaskTracker` 對 `task.jsonl` 讀寫與狀態更新

### 3.2 `src/telegram.rs`

- 透過 `grammers` 連線 Telegram（MTProto）
- 解析 `.env`（`TG_*`）控制監聽、回覆策略、啟動訊息
- 內建訊息處理能力：
  - 建立待辦（PENDING）
  - 查詢待辦
  - 完成最新待辦
  - 偵測調研意圖（例如「調研 ...」）
- AI 回覆後端：
  - `LLM_PROVIDER=ollama`（`/api/generate`）
  - `LLM_PROVIDER=lmstudio`（OpenAI-compatible `/chat/completions`）
- 具備對話 context 附帶與中英文修正策略（必要時強制繁中）

### 3.3 `src/followup.rs`

- 週期性掃描最近任務（預設每 30 分鐘）
- 針對 `PENDING` / `FOLLOWING` 建 prompt 丟本機 LLM
- 若模型回應 `FOLLOWUP_NEEDED`，將對應任務狀態改為 `FOLLOWUP_NEEDED`
- 週期可由 `FOLLOWUP_INTERVAL_SECS` 覆蓋

### 3.4 `src/researcher.rs`

- 背景調研 pipeline（多階段）
  1. fetch URL 內容（可選）
  2. overview
  3. 產生 4 個研究問題
  4. 每題做 DDG 搜尋與 LLM 回答
  5. synthesis 最終報告
- 任務狀態持久化於 `research.jsonl`
- 前端可透過 command 輪詢進度與最終報告

### 3.5 `src/skills.rs`

- 提供零金鑰 DDG 搜尋能力 `ddg_search`
- 註冊可執行技能清單
- `approve_task` 後以 Tauri event 發送 `skill:<id>`

### 3.6 `src/memory.rs`

- LanceDB 本地向量記憶：
  - `add_to_memory`
  - `search_memory`
- JSONL 對話上下文：
  - `append_context`
  - `load_recent_context`
  - `clear_context`

## 4. 資料儲存與路徑策略

優先使用 `%LOCALAPPDATA%/Sirin/...`；若環境不可得則 fallback 到工作目錄 `data/...`。

- Task log：`tracking/task.jsonl`
- Research log：`tracking/research.jsonl`
- Conversation context：`tracking/sirin_context.jsonl`
- Telegram session：`sirin.session`
- 向量記憶：`data/sirin_memory`（LanceDB）

## 5. 關鍵資料流

### 5.1 UI Task Board 資料流

1. 前端定期呼叫：`read_tasks` + `list_research_tasks`
2. Rust 回傳最新資料
3. 前端依狀態分區顯示（待處理、調研中、活動流）
4. 若按「快速核准」，呼叫 `approve_task`
5. Rust 更新狀態為 `DONE` 並觸發 skill event

### 5.2 Telegram 訊息資料流

1. listener 收到訊息
2. 解析是否為指令/待辦/調研請求
3. 必要時寫入 `task.jsonl` 或建立 research 任務
4. 透過 LLM 生成回覆（含 context 與可選 web search 摘要）
5. 回覆發送至 Telegram

### 5.3 Follow-up 資料流

1. worker 讀最近 N 筆任務
2. 過濾 `PENDING/FOLLOWING`
3. LLM 判斷是否需跟進
4. 回寫任務狀態

## 6. 執行緒與非同步模型

- Rust runtime：Tokio
- 長時任務（listener/worker/research）以 `spawn` 背景執行
- Tauri command 以同步/非同步函式混合提供
- UI 輪詢採 adaptive interval（調研進行中 2 秒，否則 5 秒）

## 7. 已知邊界與限制（現況）

- 純瀏覽器跑 Next.js 時，涉及 `invoke()` 的互動功能不等同桌面模式。
- Telegram / LLM 功能依賴 `.env` 與本機模型服務狀態。
- `skills` 目前為有限集合，事件驅動能力已建立但仍偏輕量。

## 8. 主要檔案索引

- `src/main.rs`：應用入口、command 註冊、背景任務啟動
- `src/persona.rs`：persona 與 task tracker
- `src/telegram.rs`：Telegram listener 與回覆策略
- `src/followup.rs`：跟進判斷 worker
- `src/researcher.rs`：背景調研 pipeline
- `src/memory.rs`：向量記憶與 context log
- `src/skills.rs`：skill registry 與 web search
- `components/task-board.tsx`：任務看板 UI
- `config/persona.yaml`：persona 設定
- `tauri.conf.json`：Tauri build/dev 串接設定
