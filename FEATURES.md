# Sirin — 功能說明書

> 版本：v0.1  
> 更新：2026-04-06

---

## 目錄

1. [GUI 介面](#1-gui-介面)
2. [ADK Agent 架構](#2-adk-agent-架構)
3. [Planner Agent](#3-planner-agent)
4. [Router Agent](#4-router-agent)
5. [Chat Agent](#5-chat-agent)
6. [Coding Agent](#6-coding-agent)
7. [Research Agent](#7-research-agent)
8. [Follow-up Worker](#8-follow-up-worker)
9. [Telegram 整合](#9-telegram-整合)
10. [三層記憶系統](#10-三層記憶系統)
11. [Call Graph 分析](#11-call-graph-分析)
12. [Web 搜尋技能](#12-web-搜尋技能)
13. [LLM 呼叫層](#13-llm-呼叫層)
14. [事件總線](#14-事件總線)
15. [Persona 設定系統](#15-persona-設定系統)
16. [任務追蹤](#16-任務追蹤)

---

## 1. GUI 介面

**入口**：`src/ui.rs`  
框架：egui 0.31 / eframe（原生 Rust，無 WebView）

### 四個主 Tab

| Tab | 功能 |
|-----|------|
| **任務板** | 顯示所有追蹤中的任務，支援新增 / 完成 / 刪除，顯示優先度與截止日 |
| **調研** | 觸發調研 pipeline，顯示進度步驟與最終報告，支援 URL 或純主題輸入 |
| **Telegram** | 顯示連線狀態、已監聽群組列表、最近訊息與 AI 回覆紀錄 |
| **對話** | Agent Console：直接輸入任務給 AI Agent，即時顯示 streaming 回覆 |

### Agent Console

- 直接發送自然語言任務給 `AgentRuntime`
- 支援 streaming token 顯示（LLM 邊生成邊顯示）
- 自動路由到 Chat / Coding / Research Agent
- 顯示 Coding Agent 的 ReAct 迴圈過程（工具呼叫 + 觀察結果）
- 底部 Log 面板：顯示全域 `sirin_log!` 輸出（環形緩衝，最近 500 條）

### Persona 目標更新通知

當 researcher 完成第 5 個任務的倍數時，LLM 會建議更新 persona objectives，UI 顯示確認對話框讓使用者審閱後決定是否採用。

---

## 2. ADK Agent 架構

**路徑**：`src/adk/`  
靈感來自 Google ADK（Agent Development Kit），以純 Rust 實作。

### 核心元件

```
Agent trait          — 定義 agent 的 name / description / run()
AgentContext         — 請求上下文（user input、LLM config、tool registry 引用）
ToolRegistry         — 唯讀工具登錄表，agent 透過名稱查找並執行工具
AgentRuntime         — 管理 agent 生命週期，協調 context 與 agent 執行
```

### Agent trait 介面

```rust
pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    async fn run(&self, ctx: &AgentContext) -> AgentResponse;
}
```

### ToolRegistry（唯讀）

Agent 運行時無法修改工具登錄表，確保安全性。工具以 `Arc<dyn Fn(Value) -> BoxFuture<Value>>` 形式儲存，支援非同步執行。

---

## 3. Planner Agent

**路徑**：`src/agents/planner_agent.rs`

### 功能

接收使用者原始輸入，透過單次 LLM 呼叫（使用 router model）產生結構化意圖分析：

```rust
pub struct PlannerOutput {
    pub intent_family: IntentFamily,
    pub recommended_skills: Vec<String>,
    pub task_summary: String,
    pub confidence: f32,
}
```

### IntentFamily 分類

| 分類 | 觸發條件 | 派發目標 |
|------|----------|----------|
| `Capability` | 詢問 AI 能做什麼 | Chat Agent |
| `LocalFile` | 要求讀取 / 查看特定檔案 | Chat Agent（file view mode） |
| `ProjectOverview` | 詢問專案結構 / 架構 | Chat Agent（code context） |
| `SkillArchitecture` | 詢問技能 / 工具設計 | Chat Agent |
| `CodeAnalysis` | 要求分析、修改程式碼 | Coding Agent |
| `Research` | 調研主題或 URL | Research Agent |
| `GeneralChat` | 一般對話 | Chat Agent |

### 快速路徑

兩個無 LLM 呼叫的快速判斷：
1. **關鍵字比對**：偵測 `幫我看`、`show`、`read` → `LocalFile`
2. **問題模式**：偵測 `為什麼`、`如何`、`what` → 非 file view

---

## 4. Router Agent

**路徑**：`src/agents/router_agent.rs`

### 功能

接收 `PlannerOutput`，依 `IntentFamily` 選擇並執行對應 agent，回傳統一的 `RouterResponse`。

### 路由邏輯

```
IntentFamily::CodeAnalysis  ──► CodingAgent
IntentFamily::Research      ──► ResearchAgent（spawn background task）
其他                         ──► ChatAgent
```

Research 任務以 `tokio::spawn` 背景執行，Router 立即回覆「已啟動調研」讓 UI 不阻塞。

---

## 5. Chat Agent

**路徑**：`src/agents/chat_agent/`  
分為三個子模組：`mod.rs`、`intent.rs`、`dispatch.rs`、`context.rs`

### 功能

- 載入 per-peer 對話 context（近期 N 輪）
- 依 intent 選擇上下文策略（是否注入程式碼摘要、記憶搜尋結果）
- 呼叫 LLM 生成回覆（支援 streaming）
- 將回覆寫回對話 context
- 發佈 `ChatAgentReplied` 事件

### Intent 子分類

| Intent | 對應策略 |
|--------|----------|
| `LocalFile` | 讀取目標檔案內容，注入 prompt |
| `ProjectOverview` | 注入 `project_overview()` 摘要 |
| `CodeAnalysis` | 注入 codebase search 結果 |
| `CapabilityQuery` | 靜態能力清單，無 LLM 呼叫 |
| `Correction` | 帶入上一輪回覆進行修正 |
| `WebSearch` | 呼叫 `ddg_search`，注入搜尋結果 |
| `General` | 純對話，附帶記憶搜尋 |

---

## 6. Coding Agent

**路徑**：`src/agents/coding_agent.rs`

### 工作流程

```
1. project_overview() + codebase_search()  ← 理解現有程式碼
2. 單次 LLM 呼叫：產生帶編號步驟的計劃
3. ReAct 迴圈（最多 max_iterations 輪）：
   ├── LLM 輸出 JSON：{ "thought", "action", "action_input" }
   ├── 執行工具（見下表）
   ├── 觀察結果附加到 history
   └── action == "DONE" → 跳出，回傳 final_answer
4. cargo check 驗證（如果 allowed_commands 包含）
5. git diff HEAD 收集變更摘要
```

### 可用工具

| 工具 | 功能 |
|------|------|
| `file_read` | 讀取檔案全文或指定行範圍 |
| `file_write` | 寫入檔案（支援 dry_run 模式） |
| `codebase_search` | 搜尋程式碼符號 / 關鍵字 |
| `project_overview` | 取得專案頂層結構摘要 |
| `run_command` | 執行 allowed_commands 清單中的命令 |
| `git_stash` / `git_stash_pop` | 建立還原點（任務失敗時自動回滾） |
| `inspect_range` | 讀取特定行範圍（精確查看） |

### 安全機制

- `auto_approve_writes: false`（persona.yaml）→ dry_run 模式，生成 diff 但不寫入磁碟
- `allowed_commands` 白名單限制可執行命令
- `max_file_write_bytes: 102400` 限制單次寫入大小
- Git stash baseline：任務開始前自動 stash，失敗時 stash pop 回滾
- `max_iterations` 防止無限迴圈

### 輸出格式

```rust
CodingAgentResponse {
    outcome: String,           // 人類可讀摘要
    files_modified: Vec<String>,
    iterations_used: usize,
    diff: Option<String>,      // git diff HEAD
    verified: bool,            // cargo check 是否通過
    verification_output: Option<String>,
}
```

---

## 7. Research Agent

**路徑**：`src/researcher.rs`、`src/agents/research_agent.rs`

### 五階段 Pipeline

```
Phase 1: Fetch（如有 URL）
         └── scraping_http client（60s timeout）抓取並萃取 HTML 文字
Phase 2: Overview Analysis
         └── LLM 分析：【是什麼】【主要功能】【關鍵技術/實體】
Phase 3: Generate Questions
         └── LLM 產生 4 個延伸研究問題
Phase 4: Parallel Search & Answer（4 問題並行）
         ├── DDG 搜尋每個問題（最多 3 條結果）
         └── LLM 依搜尋結果逐一回答
Phase 5: Synthesis
         └── LLM 生成完整報告：【執行摘要】【核心發現】【詳細分析】【結論與建議】
```

> **設計要點**：Phase 1 使用 `scraping_http`（有 timeout）；Phase 2-5 所有 LLM 呼叫使用 `shared_http`（無 timeout），確保長推理不被中斷。

### 進度持久化

每個階段完成後即時寫入 `research.jsonl`（原子 rename），UI 可輪詢即時顯示進度。

### Persona 反思

每完成第 5 個調研任務，pipeline 會額外呼叫 LLM 建議更新 `persona objectives`，結果存入待審閱槽位，不直接寫入。

### 失敗處理

- Phase 1 失敗（頁面抓取錯誤）→ 改以純 topic 繼續，不中止
- Phase 4 單一問題失敗 → 跳過，記錄警告；只要有一個成功即繼續
- Phase 2/3/5 失敗 → 整個任務標記 `Failed`，記錄錯誤原因

---

## 8. Follow-up Worker

**路徑**：`src/followup.rs`

### 功能

定期（`FOLLOWUP_INTERVAL_SECS`，預設 30 分鐘）執行：

1. 讀取所有 `Running` 狀態的任務
2. 呼叫 LLM 評估每個任務是否需要跟進
3. 若需要：透過 Telegram 或 UI 發送提醒
4. 發佈 `FollowupTriggered` 事件

---

## 9. Telegram 整合

**路徑**：`src/telegram/`  
協議：MTProto（grammers-client 0.9）

### 模組結構

| 模組 | 說明 |
|------|------|
| `mod.rs` | 啟動 MTProto client，管理連線生命週期 |
| `handler.rs` | 訊息過濾（群組/私訊）、意圖判斷、轉發給 LLM |
| `reply.rs` | 發送回覆，支援長訊息分段，streaming edit |
| `language.rs` | CJK 語言偵測、意圖分類（是否問題 / 身份詢問 / 程式碼查詢） |
| `commands.rs` | 指令處理：任務 CRUD、調研觸發、搜尋 |
| `config.rs` | 從環境變數載入 Telegram 設定 |
| `llm.rs` | Telegram 專用 LLM 呼叫包裝 |

### 自動回覆邏輯

```
收到訊息
  │
  ├── TG_AUTO_REPLY=false → 忽略
  ├── 群組訊息 & TG_REPLY_GROUPS=false → 忽略
  ├── should_search() → 先做 DDG 搜尋，結果注入 LLM prompt
  ├── detect_research_intent() → 觸發背景調研 pipeline
  └── 一般訊息 → 直接 LLM 回覆
```

### 調研指令前綴

| 前綴 | 說明 |
|------|------|
| `研究 <主題>` | 純主題調研 |
| `調查 <主題>` | 同上 |
| `分析 <主題或URL>` | 支援 URL |
| `research <topic>` | 英文版 |
| `investigate <topic>` | 英文版 |
| `analyze <url/topic>` | 英文版 |
| `幫我研究 ...` | 繁體中文完整句 |
| `幫我調查 ...` | 繁體中文完整句 |

### 連線管理

- session 持久化於 `%LOCALAPPDATA%\Sirin\sirin.session`
- 有 session 且未設定 `TG_REQUIRE_LOGIN=1` → 自動重連，無需 OTP
- 斷線自動重試（exponential backoff）

---

## 10. 三層記憶系統

**路徑**：`src/memory.rs`

### 第一層：全文記憶（SQLite FTS5）

```rust
memory_store(content: &str, tag: &str)  // 儲存
memory_search(query: &str) -> Vec<MemoryEntry>  // 語意全文搜尋
```

- 儲存位置：`%LOCALAPPDATA%\Sirin\memory.db`
- FTS5 全文索引，支援中英文
- 每條記憶含 `tag`（research / chat / coding）與時間戳
- Agent 在生成回覆前自動搜尋相關記憶注入 context

### 第二層：程式碼索引（tree-sitter）

```rust
refresh_index()  // 掃描 .rs 檔案，提取符號
project_overview() -> String  // 頂層結構摘要
codebase_search(query) -> Vec<SymbolMatch>  // 搜尋函數 / 結構體
```

- 排除 `target/`、`.git/`、`node_modules/`
- 提取：`fn`、`struct`、`enum`、`trait`、`impl`、`async fn`、`pub fn`
- 每個符號最多 12 個，避免過度索引
- 程式啟動時自動建立索引

### 第三層：per-peer 對話 context（JSONL ring-log）

```rust
append_context(peer_id, role, content)  // 新增一輪
load_recent_context(peer_id, n) -> Vec<Message>  // 載入近 n 輪
```

- 每個 peer（Telegram chat id 或 UI session 0）獨立檔案
- Ring-log：超過上限時自動截斷舊記錄
- 儲存位置：`%LOCALAPPDATA%\Sirin\context\<peer_id>.jsonl`

---

## 11. Call Graph 分析

**路徑**：`src/code_graph.rs`

### 功能

- 使用 tree-sitter 解析所有 `.rs` 檔案的函數定義與呼叫關係
- 建立正向（callee）與反向（caller）索引
- 支援跨函數的鏈式查詢

### 持久化

```rust
refresh_call_graph()  // 重新掃描並寫入 call_graph.jsonl
query_callers(symbol) -> Vec<String>   // 誰呼叫了這個函數？
query_callees(symbol) -> Vec<String>  // 這個函數呼叫了什麼？
```

- JSONL 格式儲存，跨重啟無需重新解析（有增量失效機制）
- 程式啟動時若 `.rs` 檔案有修改則自動重建

---

## 12. Web 搜尋技能

**路徑**：`src/skills.rs`

### 搜尋策略（優先順序）

1. **SearxNG**（若設定 `SEARXNG_BASE_URL`）：自架搜尋引擎，隱私優先
2. **DuckDuckGo HTML scraping**：解析 DDG 搜尋結果頁面
3. **DuckDuckGo Instant Answer API**：快速摘要型回答

### 回傳格式

```rust
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}
```

---

## 13. LLM 呼叫層

**路徑**：`src/llm.rs`

### 支援後端

| 後端 | 協議 | 環境變數 |
|------|------|----------|
| Ollama | `/api/generate` | `OLLAMA_BASE_URL` |
| LM Studio | OpenAI `/v1/chat/completions` | `LM_STUDIO_BASE_URL` |

### 呼叫函數

| 函數 | 用途 | 模型 |
|------|------|------|
| `call_prompt` | 一般用途 | 主模型 |
| `call_coding_prompt` | Coding Agent | `LLM_CODING_MODEL`（fallback 主模型）|
| `call_router_prompt` | Planner/Router | `LLM_ROUTER_MODEL`（fallback 主模型）|
| `call_large_prompt` | 需要高推理能力 | `LLM_LARGE_MODEL`（fallback 主模型）|
| `call_prompt_stream` | Streaming token 輸出 | 主模型 |
| `call_prompt_messages` | 多輪對話 | 主模型 |

### 共用 HTTP Client

```rust
shared_http()   // reqwest::Client，無 timeout，供 LLM 呼叫
scraping_http() // 60s timeout + custom User-Agent，供網頁抓取
```

### Probing 機制

啟動時 `probe_and_configure()` 嘗試連接 LLM 後端，自動選擇可用服務與模型，失敗則 fallback 到環境變數設定。

---

## 14. 事件總線

**路徑**：`src/events.rs`

Tokio broadcast channel，容量 64 個事件。

### 事件類型

| 事件 | 發佈者 | 訂閱者 |
|------|--------|--------|
| `ResearchCompleted` | researcher | followup, UI |
| `ResearchRequested` | router | researcher |
| `FollowupTriggered` | followup worker | UI |
| `PersonaUpdated` | researcher | UI（顯示確認對話框）|
| `CodingAgentCompleted` | coding agent | UI, telegram |
| `ChatAgentReplied` | chat agent | telegram reply |

---

## 15. Persona 設定系統

**路徑**：`src/persona.rs`、`config/persona.yaml`

### 結構

```yaml
identity:
  name: Sirin

response_style:
  voice: 自然、親切、年輕女生口吻
  ack_prefix: 收到你的訊息。

objectives:          # LLM 任務導向目標，影響所有 agent 的 system prompt
  - Monitor Agora

roi_thresholds:      # 成本控制
  min_usd_to_notify: 5.0
  min_usd_to_call_remote_llm: 25.0

coding_agent:        # Coding Agent 行為設定
  enabled: true
  auto_approve_reads: true
  auto_approve_writes: true
  allowed_commands: [...]
  max_iterations: 10
  max_file_write_bytes: 102400
```

### Persona 反思

每完成 5 個調研任務，LLM 自動評估 `objectives` 是否應更新，建議儲存在 `pending_objectives_slot`，使用者在 UI 確認後才生效。

---

## 16. 任務追蹤

**路徑**：`src/persona.rs`（`TaskTracker`）

### 功能

- JSONL 格式儲存於 `%LOCALAPPDATA%\Sirin\tracking\task.jsonl`
- 支援：新增、完成、刪除、更新狀態
- 欄位：`id`、`title`、`status`（pending/running/done/failed）、`priority`、`due_date`、`created_at`
- `trim_to_max(n)`：保留最新 n 條，自動清理舊記錄
- `find_by_timestamp(ts)`：精確查找特定時間戳的任務
- Follow-up worker 定期讀取 `Running` 任務進行 LLM 評估

---

*本文件由 Claude Code 自動生成，對應 Sirin v0.1 代碼庫。*
