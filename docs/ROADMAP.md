# Sirin 開發路線圖

本文件記錄所有待實現的功能目標，依優先級與依賴關係排序。
分析基礎：當前程式碼審查 + AI Agent 能力缺口評估。

---

## 終極目標

> **打造一個能持續自我優化的本機 AI Agent——不需要人工介入，它能從每次互動中學習、評估自身表現、調整行為策略，並主動追蹤目標。**

### 自我優化的完整定義

「自我優化」不只是「會調研」，而是一個閉合的學習迴圈：

```
行動
  → 觀察結果（回覆被接受？任務完成？用戶滿意？）
  → 評估品質（量化打分）
  → 更新知識（記住什麼有效）
  → 調整策略（改變 prompt、工具選擇、目標優先序）
  → 更好的下次行動
```

### 終態能力清單

| 層次 | 能力 | 說明 |
|------|------|------|
| **感知** | 持久語意記憶 | 記得所有過去的研究、對話、學習；跨 session 不遺忘 |
| **感知** | 環境監控 | 主動監測目標（如 Agora）的變化，不需被動等訊息 |
| **決策** | 結構化工具選擇 | LLM 自主決定用哪個工具，而非 Rust 硬編碼猜意圖 |
| **決策** | 多步驟規劃 | 複雜任務先分解計劃再執行，而非直接輸出一個答案 |
| **執行** | 豐富工具集 | 搜尋、抓頁面、讀寫檔案、執行調研、記憶存取 |
| **執行** | 並行任務 | 同時執行多個研究任務，不互相阻塞 |
| **學習** | 回覆品質自評 | 每次回覆後自動評分，識別哪種回答是好的 |
| **學習** | 失敗模式識別 | 分析低分回覆的共同原因，避免重蹈覆轍 |
| **進化** | Persona 自我更新 | 根據互動歷史自動調整目標優先序與回覆風格 |
| **進化** | Prompt 自我改進 | 偵測 prompt 缺陷，提出修改建議並測試效果 |

### 與當前版本的差距

```
現在：  用戶說「調研 X」→ Sirin 研究 → 報告存檔 → 下次忘了
終態：  Sirin 主動發現「X 有新動態」→ 自行研究 → 吸收進記憶
        → 回覆相關問題時自動引用 → 評估回覆是否有幫助
        → 若無幫助，重新研究或調整研究策略
```

### 不依賴雲端的原則

Sirin 的所有自我優化能力必須在**本機模型（Ollama / LM Studio）上運作**，
不強制依賴 OpenAI / Anthropic API。
這意味著每個優化步驟的設計都要考慮小模型（3B-7B）的能力邊界。

---

## 核心約束：每次優化都需要證據

> **任何對 Sirin 自身行為、策略或設定的修改，必須有可追溯的量化或質性證據支撐。無法驗證的優化不得自動套用。**

### 驗證層級

每一個優化動作在執行前，必須先確認能在哪個層級完成驗證：

```
層級 1 — 本機自動驗證（可直接執行）
層級 2 — 需要人工確認（暫停等待）
層級 3 — 需要雲端 AI 協助（發送審查請求）
層級 0 — 無法驗證（禁止執行，記錄原因）
```

### 各層級判斷標準

#### 層級 1：本機可自動驗證

滿足以下**全部**條件才屬於此層級：

- 有**量化指標**作為前後對比基準（例如：回覆評分、任務完成率、研究步驟數）
- 優化範圍**可逆**（例如：可以備份後覆寫，失敗時可還原）
- 影響範圍**有限**（只改變一個行為，不影響其他模組）
- 本機小模型有能力評估這類結果（不需要複雜推理或主觀判斷）

**典型例子**：
- 調整調研冷卻時間（前後比較任務重複率）
- 更新 `response_style.voice`（前後比較同類問題的評分分布）

#### 層級 2：需要人工確認

有以下任何一項就需要人工確認：

- 涉及**策略方向**的改變（例如：更換核心 `objectives`）
- 量化指標存在但**解讀有歧義**（例如：回覆更短但不確定是更好還是更差）
- 優化**不可逆**或影響範圍**跨模組**
- 本機模型對此類判斷**歷史準確率低**（根據 feedback.jsonl 記錄）

**處理方式**：
1. 在 Telegram 發送優化提案，附上證據摘要
2. 等待用戶確認（`同意` / `拒絕` / `要更多資料`）
3. 超時未回應 → 視為拒絕，記錄後暫停

#### 層級 3：需要雲端 AI 協助驗證

本機模型無法可靠評估，但問題有明確答案（非純主觀）：

- 優化涉及**語言品質判斷**（例如：哪個 prompt 寫法更清晰）
- 優化涉及**邏輯正確性**（例如：新的研究問題生成策略是否合理）
- 需要比較兩個版本的**語意差異**

**處理方式**：
1. 構建評估 prompt，發送至雲端模型（Claude / GPT）
2. 記錄雲端模型的評估結果與理由到 `data/tracking/validation.jsonl`
3. 根據結果決定是否套用

**注意**：雲端驗證需設定 `CLOUD_AI_API_KEY`；若未設定，自動降級為層級 2（人工確認）。

#### 層級 0：無法驗證，禁止執行

- 無法定義成功標準
- 無量化指標可追蹤
- 影響範圍無法預測
- 本機和雲端模型都沒有足夠能力評估

**處理方式**：
1. 記錄到 `data/tracking/blocked_optimizations.jsonl`，包含：
   - 提案內容
   - 無法驗證的原因
   - 需要什麼條件才能重新評估
2. 通知用戶（Telegram）：「發現潛在優化，但無法驗證，已暫停」
3. **不做任何修改**

---

### 優化執行流程（強制）

```
發現潛在優化點
  ↓
收集證據（前置指標快照）
  ↓
判斷驗證層級
  ↓
  ├─ 層級 1 → 執行 → 收集後置指標 → 對比 → 若退步則還原
  ├─ 層級 2 → 暫停 → 發 Telegram 提案 → 等待人工確認
  ├─ 層級 3 → 暫停 → 發雲端 AI 審查 → 等結果 → 按結果執行
  └─ 層級 0 → 禁止執行 → 記錄 → 通知
```

### 證據儲存規範

每次優化（無論是否執行）必須在 `data/tracking/optimization_log.jsonl` 留下完整記錄：

```jsonc
{
  "id": "opt-1234567890",
  "timestamp": "2025-04-04T10:00:00Z",
  "type": "persona_update",           // 優化類型
  "proposal": "...",                  // 提案內容
  "evidence": {
    "before": { "metric": "value" },  // 前置指標
    "after":  { "metric": "value" }   // 後置指標（若有執行）
  },
  "validation_level": 2,              // 驗證層級
  "validation_result": "approved",    // approved / rejected / pending / blocked
  "validator": "human",               // local / human / cloud_ai / none
  "applied": true,                    // 是否實際套用
  "rollback": false,                  // 是否已還原
  "notes": "..."
}
```

---

## 現況摘要

| 能力 | 現況 |
|------|------|
| Telegram 自動回覆 | ✅ 運作中 |
| 多步驟調研 pipeline | ✅ 運作中 |
| 調研完成 Telegram 通知 | ✅ 運作中 |
| 調研結果注入回覆 prompt | ✅ keyword match（有限） |
| 自動調研排程（follow-up worker） | ✅ 運作中 |
| 跨 session 記憶 | ❌ LanceDB 死碼，重啟清空 |
| 結構化 Tool Use | ❌ 僅 regex 意圖偵測 |
| 規劃能力（Planning） | ❌ 無 |
| 回覆品質評估 | ❌ 無 feedback 機制 |
| persona 動態更新 | ❌ 靜態 YAML |
| 系統托盤 | ❌ 無（v0.5.0 改 web UI 後不再需要 — daemon-style，關 tab 不殺 daemon）|

---

## Phase 1 — 補強記憶系統

> **目標**：讓 Sirin 記得過去發生的事，跨 session 可檢索。
> **依賴**：無（可獨立開發）

### T-01：啟用持久語意記憶（BM25 全文索引）

**問題**：`src/memory.rs` 的 LanceDB `add_to_memory` / `search_memory` 是死碼，
需要外部 embedding 模型，目前無可用路徑。

**方案**：在 LanceDB 之前加一層 BM25 全文搜尋（零外部依賴），
作為可用的記憶後端；LanceDB 保留供日後向量搜尋升級。

**實作位置**：`src/memory.rs`（新增 `src/memory_bm25.rs`）

**驗收條件**：
- `memory_store(text: &str)` 將文字持久化到 `data/memory/index.jsonl`
- `memory_search(query: &str, limit: usize) -> Vec<String>` 用 BM25 返回最相關結果
- 重啟後仍可查詢之前儲存的內容

---

### T-02：研究報告自動入庫

**問題**：研究完成後報告只存在 `research.jsonl`，不會進入任何可搜尋的記憶。

**實作位置**：`src/researcher.rs` 的 `run_research()` 結尾

**驗收條件**：
- `ResearchStatus::Done` 時，自動呼叫 `memory_store(final_report)`
- 同時儲存 topic 作為 metadata
- 查詢「調研過的話題」可列出歷史記錄

---

### T-03：回覆前語意記憶檢索

**問題**：目前用 keyword match 找研究結果（`src/telegram/mod.rs`），
只能找到 topic 關鍵字完全相符的研究。

**實作位置**：`src/telegram/mod.rs`（替換現有 `memory_context` 邏輯）

**驗收條件**：
- 回覆前呼叫 `memory_search(user_message, 3)`
- 返回結果注入 prompt 的 `memory_block`
- 語意相關（非字面相同）的過去研究也能被引用

---

## Phase 2 — 結構化 Tool Use

> **目標**：讓 LLM 自己決定要用哪個工具，而非 Rust 用 regex 猜意圖。
> **依賴**：T-01 完成後效果更好，但可先獨立開發

### T-04：定義 Tool Call 協議

**問題**：目前意圖偵測邏輯散落在 `src/telegram/commands.rs`，
用 `starts_with("調研")` 等硬編碼 regex，LLM 完全不參與工具選擇。

**方案**：在 prompt 中定義 JSON 工具呼叫格式，LLM 輸出工具指令，
Rust 解析後執行，再把結果餵回 LLM 生成最終回覆。

**協議格式**：
```json
{"tool": "reply",    "text": "..."}
{"tool": "search",   "query": "..."}
{"tool": "research", "topic": "...", "url": "optional"}
{"tool": "remember", "text": "..."}
{"tool": "recall",   "query": "..."}
{"tool": "todo",     "action": "create|list|done", "detail": "..."}
```

**實作位置**：
- 新增 `src/tool_call.rs`（協議定義 + 解析）
- 修改 `src/telegram/llm.rs`（prompt 加工具清單說明）
- 修改 `src/telegram/mod.rs`（工具執行 + 結果回注）

**驗收條件**：
- LLM 輸出 `{"tool": "search", "query": "rust async"}` 時自動執行 DDG 搜尋
- LLM 輸出 `{"tool": "research", "topic": "..."}` 時啟動研究任務
- 若輸出不是有效 JSON，fallback 為純文字回覆（向下相容）

---

### T-05：擴充工具集

**依賴**：T-04

| 工具 | 說明 | 實作位置 |
|------|------|----------|
| `fetch_url` | 抓取任意 URL 頁面文字 | 已有邏輯在 `researcher.rs`，抽成獨立 skill |
| `read_file` | 讀取本地檔案內容 | 新增 `src/skills.rs` |
| `write_file` | 寫入本地檔案 | 新增 `src/skills.rs`（需路徑白名單限制）|
| `list_tasks` | 查詢任務板 | 已有邏輯，接入 tool call 協議 |
| `memory_recall` | 搜尋記憶庫 | 依賴 T-01 |

---

## Phase 3 — 規劃與推理能力

> **目標**：對複雜請求，先規劃再執行，而非直接輸出一個回覆。
> **依賴**：T-04（Tool Use 協議）

### T-06：ReAct 推理迴圈

**問題**：收到複雜請求時，Sirin 做一次 LLM call 就回覆，
沒有分解任務、執行步驟、觀察結果的能力。

**方案**：對觸發 planning 條件的請求（多步驟、複雜問題），
先執行 `Thought → Action → Observation` 迴圈，最多 N 輪，
再輸出最終回覆。

**實作位置**：新增 `src/planner.rs`

**流程**：
```
用戶輸入
  → 判斷是否需要 planning（複雜度偵測）
  → [Thought] LLM 分析：需要哪些步驟？
  → [Action]  執行工具（tool call）
  → [Observation] 把工具結果加進 context
  → 重複最多 5 輪，或直到 LLM 輸出 {"tool": "reply", ...}
  → 最終回覆
```

**驗收條件**：
- 「幫我查一下 X 然後寫成摘要」類請求能自動分解執行
- 每輪 Thought/Action/Observation 記錄進 task.jsonl 供追蹤
- 最大輪數與 timeout 可由 env var 設定

---

## Phase 4 — 自我優化閉環

> **目標**：讓 Sirin 能根據互動結果改善自身行為。
> **依賴**：T-01（記憶）、T-04（Tool Use）

### T-07：回覆品質評分機制

**問題**：目前沒有任何機制知道哪個回覆是好的，自我優化循環缺少「評估」這一步。

**雙軌評分**：

- **方案 A（用戶評分）**：Telegram 回覆後加 👍/👎 inline button，記錄到 `feedback.jsonl`。這是最可靠的證據，屬於**層級 1 驗證**。
- **方案 B（LLM 自評）**：回覆發出後，用另一個 LLM call 評估品質（1-5 分）。本機小模型自評準確率有限，所有自評結果必須標記為**待驗證**，累積足夠樣本後才能作為優化依據。

**重要**：方案 B 的自評分數**不得單獨作為優化觸發條件**，必須與方案 A 的用戶評分或雲端 AI 校準結合使用，否則屬於層級 0（無法驗證）。

**實作位置**：新增 `src/feedback.rs`，修改 `src/telegram/mod.rs`

**驗收條件**：
- 每次回覆記錄 `{user_msg, reply, local_score, user_score, cloud_score}` 到 `feedback.jsonl`
- 三個分數欄位均可為 null（未收集）
- 低分回覆（用戶評 👎 或用戶+雲端均 < 3）才建立 `self_improvement_request`
- 純本機自評低分不觸發優化，只記錄待觀察

---

### T-08：Follow-up Worker 品質提升

**問題**：`followup.rs` 的 LLM 判斷只問「FOLLOWUP_NEEDED 還是 NO_FOLLOWUP」，context 嚴重不足。

**改進點**：
1. Prompt 加入任務完整 `message_preview` 和 `reason`
2. 從記憶庫注入相關歷史知識（依賴 T-01）
3. 分離「判斷」和「決定如何跟進」兩個 LLM call

**驗證要求**：每次 worker 決策必須記錄到 `optimization_log.jsonl`，包含：
- 判斷依據（哪些任務、哪些指標）
- 決策結果
- 後續追蹤（任務最終是否確實需要跟進）

**準確率追蹤**：每 50 次決策計算一次「worker 判斷 vs 實際結果」的準確率。
若準確率 < 60%，worker 自動降級為**層級 2**（每次決策需人工確認），
直到準確率回升才恢復自動模式。

**實作位置**：`src/followup.rs` 的 `build_prompt()` + `run_once()`

---

### T-09：Persona 動態更新

**問題**：`config/persona.yaml` 的 `objectives` 是靜態的，agent 學不到東西。

**方案**：follow-up worker 定期（每天）讓 LLM 分析近期數據，提出 persona 更新提案。

**驗證層級規則**：

| 修改類型 | 驗證層級 | 原因 |
|----------|----------|------|
| `response_style.voice` 微調 | 層級 1 | 可用前後評分對比量化 |
| `objectives` 新增一條 | 層級 2 | 策略方向，需人工確認 |
| `objectives` 刪除或修改 | 層級 2 | 不可輕易自動決定 |
| `response_style` 大幅重寫 | 層級 3 | 需雲端 AI 評估語意合理性 |
| 任何其他欄位 | 層級 0 | 禁止，不在白名單內 |

**安全機制**：
- 白名單欄位：僅 `objectives`（新增）、`response_style.voice`
- 每次修改前自動備份 `persona.yaml` → `persona.yaml.bak.{timestamp}`
- 修改後 48 小時內持續追蹤評分，若退步自動還原並升級驗證層級
- 所有提案與結果記錄到 `optimization_log.jsonl`

**實作位置**：新增 `src/persona_updater.rs`

---

## Phase 5 — UI 完善（已大幅推進，v0.5.0+ 為 web UI）

> **目標**：補強 web UI 使其與後端能力對稱。
> **狀態**：v0.5.0 砍掉 egui shell 改 plain HTML web UI；v0.5.5 已支援
> 自訂 widget 排版 + 6 個 KPI cards。下面三個 task 對 web UI 的對應實作。

### T-10：System Tray（已不適用）

v0.5.0 之後 daemon-style：關 tab 不殺 daemon，UI 想看就開瀏覽器
`http://127.0.0.1:7700/ui/`。System tray 的問題（關視窗 = 殺進程）已從根本解決。

---

### T-11：Memory 瀏覽 view（待做）

**依賴**：T-01

加一個 view（在 sidebar VIEWS section 下方）：
- 顯示所有儲存的記憶條目（虛擬列表）
- 搜尋框 → POST `/api/memory_search`
- 每筆顯示：來源（research / conversation）、時間、內容摘要
- 「刪除」按鈕（單筆）

實作上 backend 已有 `svc.search_memory`，UI 只需新 view + 一個 endpoint。

---

### T-12：Feedback 瀏覽 view（待做）

**依賴**：T-07

新增「品質追蹤」view：
- 近期回覆品質分數趨勢圖（內嵌 SVG 或 chart.js）
- 低分回覆列表（可點開看完整對話）
- 自我優化任務清單

---

## Phase 6 — 效能與可靠性

### T-13：LLM 回覆串流（Streaming）

**問題**：目前 LLM call 是阻塞等待，長回覆時 UI 無反應。

**方案**：`src/llm.rs` 加 `call_prompt_stream()` 函數，
透過 `tokio::sync::mpsc` channel 把 token 串流送到 UI。

**實作位置**：`src/llm.rs` + `src/ui.rs`

---

### T-14：Research Pipeline 並行化

**問題**：`researcher.rs` Phase 4（每題搜尋+分析）是依序執行，
4 題依序跑很慢。

**方案**：用 `futures::join_all` 並行執行 4 個 Q&A 任務。

**實作位置**：`src/researcher.rs` 的 Phase 4 迴圈

**驗收條件**：研究時間從 ~4x 縮短到 ~1x（受限於 local LLM 並發能力）

---

### T-15：Telegram Session 自動恢復

**問題**：Session 失效後需要用戶手動操作 UI 重新登入，
若 session 在無人看管的情況下失效，agent 就停止運作。

**方案**：
- 偵測 session 失效後發 email / ntfy 通知（可選 webhook）
- 提供 CLI flag `--reauth` 強制重新登入流程

---

## 依賴關係圖

```
T-01 (BM25記憶)
  ├─→ T-02 (研究入庫)
  ├─→ T-03 (語意檢索回覆)
  ├─→ T-08 (follow-up 品質)
  └─→ T-11 (記憶瀏覽 Tab)

T-04 (Tool Use協議)
  ├─→ T-05 (工具集擴充)
  ├─→ T-06 (ReAct規劃)
  └─→ T-09 (persona更新)

T-07 (品質評分)
  ├─→ T-08 (follow-up 改進)
  ├─→ T-09 (persona更新)
  └─→ T-12 (feedback Tab)

T-13 (串流) — 獨立
T-14 (並行研究) — 獨立
T-15 (session恢復) — 獨立
T-10 (系統托盤) — 獨立
```

---

## 優先順序建議

| 階段 | 任務 | 理由 |
|------|------|------|
| 立即 | T-04 Tool Use 協議 | 最高槓桿，解決意圖識別根本問題 |
| 立即 | T-01 BM25 記憶 | agent 最基礎缺口，零外部依賴可實作 |
| 短期 | T-02 研究入庫 | T-01 完成後 5 分鐘內可做 |
| 短期 | T-07 品質評分 | 自我優化的前提，越早收集數據越好 |
| 中期 | T-06 ReAct 規劃 | 顯著提升複雜任務處理能力 |
| 中期 | T-08 Follow-up 改進 | 改善現有自主循環品質 |
| 長期 | T-09 Persona 更新 | 真正的自我優化閉環 |
| 長期 | T-10 系統托盤 | UX 完善 |

---

# 附錄:MCP / Testing Roadmap(2026-04-17 起)

Sirin 除了原本的 self-optimizing agent 方向,也變成**外部 AI 透過 MCP 驅動瀏覽器**的基礎設施(Claude Code / Desktop / Cursor 都在用)。這條線有獨立的演化方向 —— 重點不是「Sirin 更聰明」,是「Sirin 更安全、更穩、更可觀察」。

## Tier 1 — 止血(痛點立即感受)

| # | 項目 | 規模 | 文件 |
|---|---|---|---|
| **T-M01** | Windows zombie port wrapper(issue #14 自修復) | 1/2 day | — |
| **T-M02 ★A** | **Pre-Authorization engine**(外部 AI gate,防幻覺 `eval document.cookie` 等) | 3–5 day | `docs/DESIGN_AUTHZ.md` |
| **T-M03 ★B** | **Live web UI Monitor**(瀏覽器 :7700/ui/ Browser tab 內的即時 screenshot / action feed / authz ask + Pause/Step/Abort) | 5–7 day | `docs/DESIGN_MONITOR.md`(部分過時，v0.5.0 之後 consumer 換成 web UI) |
| **T-M04** | Trace NDJSON + Replay mode(★B UI 重用) | 1–2 day(在 ★B 之上) | `DESIGN_MONITOR.md` §6 |

## Tier 2 — 擴能

| # | 項目 | 規模 |
|---|---|---|
| T-M05 | `page_state` 聚合查詢(url+ax+screenshot+console+net 一次回) | 1 day |
| T-M06 | `ax_find` 加 regex + `not_name_matches` | 1/2 day |
| T-M07 | `ax_diff(before, after)` + `wait_for_ax_change` | 1 day |
| T-M08 | Fixture 管理(`with_fixture { setup, cleanup }`) | 1 day |
| T-M09 | Parallel test execution(多 Chrome profile 同時跑) | 2–3 day |

## Tier 3 — 生態

| # | 項目 | 規模 |
|---|---|---|
| T-M10 | Playwright protocol bridge(接 Playwright trace viewer / codegen) | 1–2 weeks |
| T-M11 | VSCode extension(即時 screenshot + ax tree panel) | 1 week |
| T-M12 | CLI / REPL mode(`sirin exec goto …` 免 curl-heredoc) | 2–3 day |
| T-M13 | Systray helper(tray-icon,★B 第二階段) | 2 day |

## Tier 4 — 社群

| # | 項目 | 規模 |
|---|---|---|
| T-M14 | `examples/agora-market.md`(固化已驗 a11y 節點 cheatsheet) | 1/2 day |
| T-M15 | `cargo build` emit `schema.json`(外部 LLM tool-use 可消費) | 1/2 day |
| T-M16 | Benchmark harness(`cargo bench` 跑典型 action 組合) | 1 day |

## 優先序建議

```
立即 │ T-M01 zombie port → T-M02 AuthZ → T-M03 Monitor
短期 │ T-M04 trace replay → T-M05 page_state → T-M14 agora cheatsheet
中期 │ T-M07/M08 ax_diff / fixture → T-M12 CLI → T-M13 systray
長期 │ T-M09 parallel → T-M10 Playwright bridge → T-M11 VSCode ext
```

## 跟原 T-01..T-15 Agent 方向的關係

- **獨立**:MCP/Testing 方向不動 agent core(memory / persona / follow-up)
- **共用**:同個 `sirin.exe` 進程、同個 web UI(`:7700/ui/`)、同個 tokio runtime
- **互補**:Monitor 的 trace ndjson 之後也能給 agent 做 self-optimize 的 training data(T-07 品質評分的一個新 input source)
