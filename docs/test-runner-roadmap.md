# Sirin Test Runner — Improvement Roadmap

> **Status update 2026-04-16 PM:** P1 all done ✅. P2 item 2.1 + 2.3 done ✅.
> P3 items still pending. Plus several unlisted extras shipped: ad-hoc tests,
> config_diagnostics MCP, browser_exec MCP, auto-fix verification loop,
> Flutter vision testing playbook.
>
> v1 — 2026-04-16 AM, revised after author's review.
> Analysis based on `6860a82 feat: test_runner external MCP API + async polling + resilience`.

---

## P1 — Blocks non-Chinese users (1-3 hr each)

### 1.1 Locale-aware prompts ✅ DONE (a38f26f)

**現狀**: `executor.rs` 的 ReAct prompt、`triage.rs` failure classifier prompt、可能還有 success evaluator prompt 全寫死繁中。英文 user 拿到 `final_analysis` 是完整繁中句子(非亂碼,只是不懂),LLM 母語非中文也可能降智。

**為什麼重要**: Sirin 要當 general-purpose testing tool,不能假設 user 讀得懂中文。

**改法**:
```rust
// src/test_runner/executor.rs
enum Locale { ZhTw, En, ZhCn }

fn build_prompt(goal: &TestGoal, locale: Locale) -> String {
    match locale {
        Locale::En => EN_PROMPT_TEMPLATE,
        Locale::ZhTw => ZH_TW_PROMPT_TEMPLATE,  // 現況
        Locale::ZhCn => ZH_CN_PROMPT_TEMPLATE,
    }
}
```

YAML 加 `locale: en`,預設維持 `zh-TW` 不影響既有測試。

**估時**: 3-4hr(3 個 prompt template + enum + YAML parse + 各 prompt 的 fallback/heuristic 英文版)

---

### 1.2 `retry_on_parse_error` YAML 欄位 ✅ DONE (a38f26f)

**現狀更正**: `max_iterations` 和 `timeout_secs` 在 `parser.rs:12-15` **已經是 YAML 欄位**(`#[serde(default)]`)。真正還 hardcoded 的只剩 `executor.rs:38 const MAX_PARSE_ERRORS: usize = 3`。

**為什麼重要**: 低 token-budget 測試想 fail fast(retry=1),高穩定度測試想多試幾次(retry=5)。

**改法**:
```rust
// parser.rs
#[serde(default = "default_retry_parse")]
pub retry_on_parse_error: u32,

fn default_retry_parse() -> u32 { 3 }

// executor.rs
if parse_error_count >= test.retry_on_parse_error as usize { ... }
```

**估時**: 0.5hr(一個欄位 + default fn + 替換 const)

---

### 1.3 `.claude/skills/sirin-test.md` 說明 ✅ DONE (a38f26f + 39aa036 launch skill)

**現狀**: Sirin repo 有 README,但 Claude Code 使用者不知道怎麼從 CLI session 用它。沒有 skill doc → Claude 不會主動觸發測試。

**為什麼重要**: `6860a82` 把 MCP API 做好了但 UX 還缺最後一哩。有 skill doc,Claude Code 才會在 user 說「測試買家流程」時知道要調 `sirin.run_test_async(...)`。

**改法**: 在 Sirin repo 加 `.claude/skills/sirin-test.md`,內容:
- 什麼時機用 Sirin(E2E、視覺驗證、失敗自動診斷)
- 前置:Sirin running + MCP 註冊
- 3 個常見流程(run single test / run tag / debug failed run)
- Example YAML test goals

**估時**: 2hr 寫 + 修

---

## P2 — 可靠性升級(各 3-6 hr)

### 2.1 Observation truncation — 讓 LLM 自主 pull 完整內容 ✅ DONE (7a94b46)

**現狀**: 觀察 > 800 char 截斷,附 hint `use get_full_observation(run_id, step=N)`。但 LLM 看到 hint 後也沒 tool 可以主動 call — `get_full_observation` 是 HTTP API,在 ReAct loop 內不是 browser tool。

**為什麼重要**: 抓 console error 或 network response 時經常 > 800 char。LLM 只看前 800 char 做決策常誤判。

**改法**: 把 `get_full_observation` 暴露成 ReAct tool:
```
tool name: expand_observation
args: step_number
```
LLM 看 hint → call `expand_observation(step=3)` → 拿完整內容 → 重新判斷。

**估時**: 3hr

---

### ~~2.2 Bayesian flakiness detection~~ ❌ 撤回

**作者否決(我同意)**: 10 sample 的 Beta(α,β) 95% CI 寬到實用失效。
例:7 pass / 3 fail → Beta(8,4) 的 95% CI ≈ [0.37, 0.88]。判定「95% 下界 < 70%」幾乎對任何不完美 test 都成立 → 所有 test 都變 flaky,分類失去意義。

要窄的 CI 至少需 n>30,但通常 test 跑不到。

現有 `固定 70% + 10-run window` 簡單、可解釋、夠用。這是「看起來科學但沒實務價值」的過度工程典型。

---

### 2.3 Auto-fix feedback loop ✅ DONE — plus verification loop (7a94b46 + 3800b25)

**現狀**: `auto_fix=true` spawn Claude Code 修 bug 是 fire-and-forget。沒記錄 spawn 結果,下次同 test fail 時 LLM 不知道上次試過什麼。

**為什麼重要**:
- 同 bug 被 auto-fix 觸發 10 次,每次都是重新分析(浪費 LLM token)
- 修完若有 regression,triage 看不到「這是 regression,不是新 bug」

**改法**: 新表 `auto_fix_history`:
```sql
CREATE TABLE auto_fix_history (
  test_id TEXT,
  triggered_at TEXT,
  claude_session_id TEXT,
  fix_outcome TEXT, -- pending / merged / rejected / regressed
  related_run_id TEXT,
  analysis TEXT
);
```

Triage 時查這表:
- 若 `pending` 有未完成 fix → 不再 spawn
- 若最近 5 次 fix 都 `regressed` → 通知 user「這 test 可能有其他隱形問題」

**估時**: 5-6hr

---

## P3 — 架構級(1-2 day 各)

### 3.1 Multi-browser pool 並行

**現狀**: `ensure_open()` 是全 global singleton。`run_all()` 只能序列跑 N 個 test = N × 120s。

**為什麼重要**: Sirin 宣傳「全套 E2E」,但 30 個 test 要跑 1hr,user 會嫌慢。並行 3 個 browser 能縮到 20min。

**改法**: 把 `global()` 改成 `Vec<BrowserInner>`,加 tab pool:
```rust
pub fn acquire_tab() -> TabHandle  // 拿一個空閒 tab
pub fn release_tab(h: TabHandle)   // 還回去

// run_all:
for test in tests {
    let tab = acquire_tab();
    spawn(async move { execute_on_tab(tab, test).await; release_tab(tab); });
}
```

難點:headless_chrome crate 的 Browser 不是 `Send` 友好;要包裝或改用多 process。

**估時**: 1-2 day

---

### 3.2 Backend-agnostic browser trait

**現狀**: 全用 `headless_chrome` crate。此 crate 社群小、CDP 新 feature 更新慢。若 crate bug 遇到就沒救。

**為什麼重要**: 單點依賴風險。production 用 crate 卡住時,可切 Playwright 或 Puppeteer(透過 Node sidecar)。

**改法**:
```rust
trait BrowserBackend {
    fn navigate(&self, url: &str) -> Result<(), String>;
    fn click(&self, selector: &str) -> Result<(), String>;
    // ... 30 個方法
}

struct HeadlessChromeBackend { ... }
struct PlaywrightNodeBackend { ... }  // 透過 IPC 調 node playwright
struct CDPDirectBackend { ... }        // 自己講 CDP websocket
```

Browser module 暴露 `Box<dyn BrowserBackend>`,依 config 選。

**估時**: 1 day(trait 抽象) + 1 day(Playwright backend PoC)

---

### 3.3 Test recording mode(LLM 學習 workflow)

**現狀**: 測試寫 YAML 要人工定義 success_criteria。非 tech user 不會寫。

**為什麼重要**: 讓 QA 或 PM 手動操作一次,Sirin 錄下步驟 → 自動生成 YAML test。降低門檻,擴大 user base。

**改法**:
```
UI: "Start Recording" button
  → 觸發 browser.rs install_action_log()
  → 使用者手動操作(click/type/scroll)
  → "Stop Recording" → YAML generator 把動作序列變成 success_criteria
```

Claude 擴充有做類似功能(record workflow),可參考其 UX。

**估時**: 2-3 day

---

## 優先排序(修正版)

| 順位 | 項目 | 估時 | ROI |
|---|---|---|---|
| 🔴 P1 | 1.3 Claude Code skill doc | 2hr | **最高** — 不用 code 改動就解鎖一票 user |
| 🔴 P1 | 1.1 Locale 參數化 | 3-4hr | 擋住英文 user |
| 🔴 P1 | 1.2 `retry_on_parse_error` YAML | 0.5hr | 幾乎白撿 |
| 🟡 P2 | 2.3 Auto-fix history | 4-6hr | 避免重複 spawn Claude 浪費 token |
| 🟡 P2 | 2.1 Expand observation(有條件) | 3hr | 抓 console/network 品質升級,需控管 token cost |
| ❌ 撤回 | ~~2.2 Bayesian flakiness~~ | — | 過度工程,10-sample Beta CI 太寬 |
| 🟢 P3 | 3.1 Multi-browser pool | 1-2 day | 30x test 才顯著 |
| 🟢 P3 | 3.3 Recording mode | 2-3 day | Growth 加速器 |
| 🟢 P3 | 3.2 Backend trait | 2 day | YAGNI(headless_chrome 穩定) |

---

## 修正後 sprint 建議

**Week 1 — P1 全部(~6hr,一天內搞定)**:
- 1.3 skill doc(2hr)→ 解鎖 Claude Code 使用者路徑
- 1.1 locale 參數化(3-4hr)→ 國際化
- 1.2 `retry_on_parse_error`(0.5hr)→ 白撿

**Week 2 — 兩個真痛點(~9hr)**:
- 2.3 Auto-fix history(4-6hr)→ 避免重複 spawn
- 2.1 Expand observation with token-cost guardrail(3hr)→ 只在 truncation 發生 AND 下一步需要 expand 時允許 call

**後續**:
- Multi-browser pool:等 user 實際抱怨速度再做
- Recording mode:Growth feature,core 穩了再考慮
- Backend trait:YAGNI,除非 headless_chrome crate 出事

### 作者 review 採納記錄

| Issue | Feedback | 處理 |
|---|---|---|
| 1.2 「全 hardcoded」 | **事實錯誤** — parser.rs 已有 max_iterations/timeout_secs YAML 欄位 | 改成「只補 retry_on_parse_error」,估時 1.5hr → 0.5hr |
| 1.1 估時 | 2hr 偏樂觀(3 prompts + fallback) | 改 3-4hr |
| 1.1 「亂碼」用詞 | 繁中是正常顯示,只是英文 user 不懂 | 文字修正 |
| 2.2 Bayesian | 10-sample Beta CI 太寬 → 分類失效 | **撤回** |
| 2.1 Expand obs | token cost 要權衡 | 加「限制條件:truncation 發生 AND 下一步需要」 |

---

*Prepared in response to AgoraMarket session's Puppeteer vs Sirin evaluation, 2026-04-16.*

---

## 2026-04-16 PM 實作追記

### 已完成（與 roadmap 一致）

- 1.1 locale：`src/test_runner/i18n.rs` + 3 個 prompt 切換（執行/評估/triage）
- 1.2 `retry_on_parse_error`：YAML 欄位 + executor 支援
- 1.3 skill docs：`.claude/skills/sirin-test/SKILL.md` + `sirin-launch/SKILL.md`
- 2.1 expand_observation：新 ctx_fn tool + executor dispatch
- 2.3 auto_fix history：dedupe + 3-fail circuit breaker

### 非 roadmap 但有做（來自「外部 AI 用戶缺啥」分析）

| 項目 | commit | 原因 |
|------|:---:|------|
| `run_adhoc_test` MCP | 3bee071 | 外部 AI 被 YAML 綁手腳 |
| `config_diagnostics` MCP | 3bee071 | 外部 AI 無法自我診斷 |
| `list_recent_runs` + `list_fixes` MCP | 3bee071 | 歷史查詢需求 |
| `browser_exec` MCP | 3bee071 | 即席 debug 需求 |
| **auto-fix 驗證迴圈** | 3800b25 | Claude 說修好 ≠ 真的修好 |
| `url_query` TestGoal 欄位 | 3800b25 | Flutter renderer flag |
| `config/tests/agora_market_smoke.yaml` | 56aed9b | Vision-based Flutter 測試驗證 |
| 狀態機 demo test | 8a29d73 | living documentation |
| **Telegram 失敗通知 hook** | b8ac3b8 + 17e1c22 | `runs::set_phase()` 在 Complete 非 passed 狀態 fire-and-forget notify_failure；store::record_run 的重複通知 bug 一併修掉 |
| **UI Test Dashboard** | 3908b2d | 新 `View::TestRuns` egui 面板 + `TestRunnerService` sub-trait + sidebar "測試儀表板" 按鈕。Active runs 走 in-memory registry，History 走 SQLite store(30) |
| **`go_back` browser action** | 206c661 | 補上瀏覽器歷史返回（closes #28）— 解 Flutter SPA hash route 切換後回上一頁需求 |
| **browser_headless 集中化** | cb49ea5 | 移除 22 個 Agora YAML 的 `browser_headless: false` — 改由 `.env SIRIN_BROWSER_HEADLESS=false` 一處設定，per-test override 仍保留但實務不用 |
| **dev-relaunch.sh YAML auto-sync** | 9b880ac | 加 step `[2b]`：cargo build 後自動 rsync `config/tests/*.yaml` → `%LOCALAPPDATA%\Sirin\config\tests\`，避免改 YAML 後 installed-mode 讀不到 |
| **Gemini concurrency limiter + empty-retry** | bd9cafb | `src/llm/backends.rs` 加 process-wide Semaphore (`GEMINI_CONCURRENCY=3`)；HTTP 200 + 空 content 自動 retry 2 次 (2s/4s)。解 batch 跑 8 個 test 同時打 Gemini 偶爾失敗的根因（Free tier 並發回空，不回 429） |
| **parse_step robustness (Gemini plaintext drift)** | b97df97 | `src/test_runner/executor.rs::parse_step` — root-action recovery（root 有 `action` 但無 `thought`/`action_input` wrapper 時當 action_input）+ plaintext fallback（brace-depth 從 `thought: ...\naction_input: {...}` extract）+ parse_error_hint 加完整 schema 範例。解 "too many invalid LLM responses (5)" 失敗模式 |
| **shadow_find Flutter auto-bootstrap** | 119efdc | `src/browser.rs::shadow_find` — 5×600ms host-empty retry 後自動呼 `enable_flutter_semantics()` + 800ms wait + 一次 retry。對齊 `get_full_tree()` (ax_* path) 既有 fallback。Test author 不需手動加 enable_a11y preamble |
| **Test Dashboard launcher + filter + sparkline** | 79eaf19 | `src/ui_egui/test_dashboard.rs` — Run launcher（YAML 下拉 + ▶ Run）、filter（All/Passed/Failed tabs + text 搜尋）、12-cell pass-rate sparkline 在 header。`TestRunnerService` trait 增 2 method：`list_test_ids` + `launch_test_run` |
| **v0.4.4 release** | 6795897 | tag + GH release CI 自動建 Windows installer + ZIP，包含 go_back / dashboard / Gemini limiter / parser fix / shadow_find auto-bootstrap |

### 仍為 P3 / 未做

- 3.1 multi-browser pool：平行測試（部分由 `run_test_batch` 解決，使用 chrome tabs + Semaphore，但仍非完整 browser pool）
- 3.2 backend-agnostic browser trait：YAGNI
- 3.3 recording mode：growth feature

### 新發現的 TODO（從實測中）

1. ~~**headless daemon mode**~~ — `sirin --headless` 已 ship (v0.4.0)
2. ~~**`persist_adhoc_run(run_id, test_id)` MCP**~~ — 已 ship
3. ~~**UI test dashboard**~~ — 已 ship 3908b2d
4. ~~**通知 hooks**~~ — Telegram failure notify 已 ship b8ac3b8 + 17e1c22
