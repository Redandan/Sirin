# Sirin 開發路線圖

本文件記錄所有待實現的功能目標，依優先級與依賴關係排序。
分析基礎：當前程式碼審查 + AI Agent 能力缺口評估。

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
| egui 系統托盤 | ❌ 只有最小化 |

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

**問題**：目前沒有任何機制知道哪個回覆是好的，
自我優化循環缺少「評估」這一步。

**方案 A（用戶評分）**：在 Telegram 回覆後加 👍/👎 按鈕（Telegram inline keyboard），
用戶點擊後記錄到 `data/tracking/feedback.jsonl`。

**方案 B（LLM 自評）**：回覆發出後，用另一個 LLM call 評估回覆品質（1-5 分），
自動記錄，不需用戶操作。

**建議**：先實作方案 B（零用戶摩擦），日後加方案 A。

**實作位置**：
- 新增 `src/feedback.rs`
- 修改 `src/telegram/mod.rs`（回覆後觸發自評）

**驗收條件**：
- 每次回覆後記錄 `{timestamp, user_msg, reply, score, reason}` 到 feedback.jsonl
- 低分回覆（< 3）自動建立 `self_improvement_request` 任務

---

### T-08：Follow-up Worker 品質提升

**問題**：`followup.rs` 的 LLM 判斷只問「FOLLOWUP_NEEDED 還是 NO_FOLLOWUP」，
context 嚴重不足，決策品質低。

**改進點**：
1. Prompt 加入任務的完整 `message_preview` 和 `reason`（現在只有 event/status）
2. 從記憶庫注入相關歷史知識（依賴 T-01）
3. 分離「判斷需要跟進」和「決定如何跟進」兩個 LLM call
4. 結果記錄到 feedback.jsonl 形成優化 loop

**實作位置**：`src/followup.rs` 的 `build_prompt()` + `run_once()`

---

### T-09：Persona 動態更新

**問題**：`config/persona.yaml` 的 `objectives` 是靜態的，
agent 學到的東西不會改變未來的行為方向。

**方案**：follow-up worker 定期（例如每天）讓 LLM 根據：
- 近期 feedback.jsonl 的高低分模式
- 近期研究報告的主題分布
- 用戶頻繁問的問題類型

…建議更新 `objectives` 或 `response_style`，
由 agent 寫入 persona.yaml 並記錄變更原因。

**實作位置**：新增 `src/persona_updater.rs`

**安全限制**：
- 只能修改白名單欄位（`objectives`、`response_style.voice`）
- 每次修改前備份舊版 persona.yaml
- 修改內容記錄到 task.jsonl（`persona_updated` 事件）

---

## Phase 5 — GUI 完善

> **目標**：補強 egui UI 使其與後端能力對稱。

### T-10：系統托盤（System Tray）

**問題**：目前關閉視窗只能最小化，沒有系統托盤圖示。

**方案**：加入 `tray-icon` crate，在主執行緒事件迴圈整合。

**驗收條件**：
- 系統托盤顯示 Sirin 圖示
- 右鍵選單：Show / Quit
- 關閉視窗 → 最小化到托盤（現有行為 + 托盤圖示）
- 雙擊托盤圖示 → 還原視窗

---

### T-11：Memory 瀏覽 Tab

**依賴**：T-01

新增第四個 tab「記憶庫」：
- 顯示所有儲存的記憶條目（分頁）
- 搜尋框：輸入關鍵字即時過濾
- 每筆顯示：來源（research / conversation）、時間、內容摘要
- 「刪除」按鈕（單筆）

---

### T-12：Feedback 瀏覽 Tab

**依賴**：T-07

新增「品質追蹤」tab：
- 近期回覆品質分數趨勢圖（egui plot）
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
