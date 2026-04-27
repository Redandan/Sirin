# Claude in Chrome vs Sirin — 網頁操作能力評測

> Issue: [#59](https://github.com/Redandan/Sirin/issues/59) (closed via PR #73 doc-only) → [#97](https://github.com/Redandan/Sirin/issues/97) (pilot 實機 benchmark)
> 相關 PR: [#72](https://github.com/Redandan/Sirin/pull/72) (KB MCP 整合) → [#91](https://github.com/Redandan/Sirin/pull/91)/[#92](https://github.com/Redandan/Sirin/pull/92)/[#94](https://github.com/Redandan/Sirin/pull/94) (gateway + helper) → [#96](https://github.com/Redandan/Sirin/pull/96) (kb_write)
> Benchmark 對象: AgoraMarket Flutter CanvasKit PWA + 校準 / 跨站
> 狀態: 架構對比 + **Pilot #001 實機 benchmark 完成**（5 test，KB report: `cic-bench-pilot-001-report`）

---

## 0. TL;DR

兩者**互補不互斥**，但**權重要調**（Pilot #001 實證後修正）。

- **Sirin** = 自動化測試引擎（regression / batch / CI），能直驅 Flutter CanvasKit semantics tree。
- **Claude in Chrome (Beta)** = 互動式探索瀏覽器 + **vision-driven coordinate click 在 Flutter canvas 比預期有效**（Pilot 實測 H4 駁斥）。原生 DOM 仍是強項，跨站通用性高。
- **#54 / PR #72** 完成後，CiC 可透過 Sirin MCP（`kb_search`/`kb_get`/`kb_write`）取得 KB；後續 PR #91/#92/#94 以 `/gateway` + `window.sirin` helper 補強到實際可用，PR #96 加 `kb_write` 完整化 round-trip。
- **真正不可替代的 Sirin 角色**: parallel batch 執行 + structured regression history（CiC 一次只能 1 task；Pilot 證實 Sirin 並行多 test 還有 [#98 race bug](https://github.com/Redandan/Sirin/issues/98) 待修）。

---

## 1. 為什麼要做這個對比

隨著 Anthropic 釋出 Claude in Chrome (Beta)，一個合理的疑問浮現：

> 既然 Anthropic 官方做了 Chrome agent，Sirin 的 value-add 還在哪？

回答這個問題的關鍵是：**AgoraMarket 全是 Flutter CanvasKit，不是傳統 DOM 應用**。
通用 Chrome agent 對 canvas-rendered 應用的支援度，是這個對比的核心未知數。

---

## 2. 受測平台特性 — AgoraMarket Flutter CanvasKit

所有 17 個現有 Sirin regression 測試都跑在 Flutter CanvasKit 渲染：

| 特性 | 對 agent 的影響 |
|------|----------------|
| UI 渲染在 WebGL `<canvas>` | 標準 DOM `querySelector` 完全無效 — 沒有按鈕可選 |
| Accessibility tree 是 Flutter 自製 semantic tree | 非標準，需要 CDP 的 `Accessibility.getFullAXTree`（且要 `RawGetFullAxTree` workaround 繞過 headless_chrome 的 strict-enum bug） |
| 點擊需要 `Input.dispatchMouseEvent` 模擬 `PointerDown` | 標準 `element.click()` 被 Flutter 攔截無效 |
| 文字輸入需要 `Input.insertText` CDP 命令 | `flutter_type` 限 ASCII；CJK 必須走 IME 路徑 |
| Hash-route 切換中 CDP 容易 30s silence timeout | headless_chrome 連線會掉 — 需要 `wait_for_url` + auto-recover |

Sirin 為此實作了 `shadow_dump` / `shadow_click` / `shadow_find` / `flutter_type` / `RawGetFullAxTree` 等專用 action（見 `src/browser_ax.rs`、`src/adk/tool/builtins.rs`）。

---

## 3. 能力對比表

### 3.1 渲染層存取

| 能力 | Claude in Chrome | Sirin |
|------|-----------------|-------|
| 標準 DOM `querySelector` | ✅ 原生 | ✅ via `Runtime.evaluate` |
| `getComputedStyle` / form fill | ✅ 原生強項 | ✅ |
| Flutter semantics tree | ❌ 無公開 API | ✅ `RawGetFullAxTree` + recovery |
| Canvas pixel-level 點擊 | ❌（無 PointerDown 路徑）| ✅ `shadow_click` via `Input.dispatchMouseEvent` |
| 視覺截圖 + LLM 判讀 | ✅ 內建 | ✅ `screenshot_b64` + vision LLM (perception layer) |
| OCR fallback | ❌ | ✅ Windows OCR (`src/perception/ocr.rs`) |
| Network capture (req+res body) | 🟡 部分 (`read_network_requests`) | ✅ 全量 + persist |

### 3.2 工作流層

| 能力 | Claude in Chrome | Sirin |
|------|-----------------|-------|
| Scriptable test goal (YAML) | ❌ — prompt 驅動 | ✅ `src/test_runner/parser.rs` |
| Batch / 並行執行 | ❌ 單 session | ✅ `run_test_batch`（max 8 tabs，per-test session_id） |
| Headless / CI | ❌ — 須開 Chrome 視窗 | ✅ `--headless` + `SIRIN_BROWSER_HEADLESS` |
| 失敗 auto-triage + auto-fix | ❌ 靠對話 | ✅ `src/test_runner/triage.rs` + verification loop |
| Persistent KB（跨 session 學習） | 🟡 透過 Sirin MCP（PR #72） | ✅ SQLite + KB write-back on test failure |
| Run history / regression 比對 | ❌ | ✅ `test_runs` SQLite |
| 多瀏覽器 / 跨 OS | ✅ 用 Chrome | 🟡 Chrome only (CDP) |
| 用戶不需安裝額外工具 | ✅ | ❌ 需要 Sirin binary + MCP server |
| 互動式探索（用戶 + agent 共駕） | ✅ 強項 | 🟡 Assistant mode 才支援（scaffold 未完整） |

### 3.3 Authentication / 隔離

| 能力 | Claude in Chrome | Sirin |
|------|-----------------|-------|
| 用戶 cookies / login state | ✅ 原生 — 跑在用戶 Chrome | 🟡 `SIRIN_PERSISTENT_PROFILE` 開啟才行 |
| 防止測試污染用戶資料 | ❌ — 同一個 profile | ✅ 預設 fresh profile / named session |
| Bearer token 不洩漏到瀏覽器 | ✅（透過 Sirin MCP，token 留在 `.env`）| ✅ |

---

## 4. 針對 Flutter CanvasKit 的對比（核心結論）

### 4.1 點擊

- **Sirin**：`shadow_click` 走 `Input.dispatchMouseEvent` 直接送 `PointerDown` → `PointerUp`，繞過 DOM 完全成功。
- **Claude in Chrome**：原生 `computer` / `find` 工具假設 DOM tree 存在；對 canvas 上的「商品卡」沒有可選 element。預期成功率低於 30%（對應 issue #59 H4 假設）。

### 4.2 元素查找

- **Sirin**：`shadow_find` + `RawGetFullAxTree` 直接從 Flutter semantic tree 找按鈕名稱（即使 Tab×2 觸發 semantics 重啟也能 recover — 見 `src/browser_ax.rs::poll_tree_recovery`）。
- **Claude in Chrome**：`get_page_text` / `read_page` 對 canvas 應用基本回空字串。截圖 + vision LLM 是唯一路徑，但對 Flutter 特有 UI（chip、bottom-tab、time-picker）的辨識精度未知。

### 4.3 文字輸入

- **Sirin**：`flutter_type` (`Input.insertText`) 直接寫入 Flutter focused field；CJK 要走 IME 但路徑明確。
- **Claude in Chrome**：依賴鍵盤事件 → Flutter 的 hardware keyboard 路徑 — 對 IME 候選字、長按手勢的支援度未知。

### 4.4 Hash-route / SPA 導航

- **Sirin**：`wait_for_url` + `wait_for_network_idle` + 連線掉了會 `with_tab` recover（保留 cookies via `SIRIN_PERSISTENT_PROFILE`）。
- **Claude in Chrome**：navigate 後沒有「等 Flutter render 完」的 condition wait；通常要靠 polling screenshot。

### 4.5 推論：H4 假設 — **Pilot #001 實測駁斥**

> **更新（2026-04-27 Pilot #001）**：原預測「CiC Flutter 點擊成功率 < 30%」**錯**。實測 CiC 在 Flutter CanvasKit 上 **vision + coordinate click 戰略**有效：
>
> - `agora_buyer_wallet`: CiC **18 秒** completeness=3 — 用螢幕座標點 Flutter bottom nav，讀全部錢包資料（USDT 7376.81 等）；同題 Sirin **失敗 57 秒** (shadow_click 找不到按鈕)
> - `agora_pickup_time_picker`: CiC completeness=2，25 actions 完成（switch role + scroll + 找到 picker + 設 14:00）；Sirin error 0 iter
>
> 真實 CiC Flutter 成功率估 **60-80%**。失敗 case 的根因不是 LLM 能力（座標點 work），而是「真實後端 auth 缺失」（`agora_admin_category_filter` 的 `__test_role=admin` query param 不夠完成完整登入）+「native browser dialog CDP 看不到」（`agora_webrtc_permission`）。

**修正後結論**：`browser_exec` 整合仍有價值（CiC + Sirin shadow_click 可覆蓋 admin 那種 complex auth 場景），**但不是必要條件**。CiC 純 vision-coordinate 對日常 Flutter UI 已夠。

---

## 5. 各自最強場景

### Sirin 強項（無可替代）

1. **大量並行 regression** — `run_test_batch` 一次跑 17 個 YAML 測試。
2. **CI / headless server** — 沒有用戶在的場景。
3. **Flutter 特化操作** — `shadow_click` / `flutter_type` / AX tree recovery。
4. **Persistent learning** — 失敗自動寫回 KB（`feat(kb): auto write-back on test failure`），下次同類失敗少跑 2 步。
5. **Auto-triage + auto-fix + verification** — `src/test_runner/triage.rs` 三階段循環。
6. **Trial isolation** — fresh profile 預設，測試不污染用戶 cookies。

### Claude in Chrome 強項

1. **零安裝** — 用戶開 Chrome 就有，不需 Sirin binary。
2. **跑在用戶自己的 session** — 已登入的內網系統 / SaaS 可直接操作。
3. **互動式 debug** — 用戶可以手動接管、agent 解釋當前畫面。
4. **跨瀏覽器** — 任何 site 都能用，不限定 AgoraMarket。
5. **Browser permission dialogs** — WebRTC / clipboard / notification 等原生彈窗（issue #59 H3 假設）。
6. **官方支援** — Anthropic 直接維護，bug fix 跟 Claude 模型升級同步。

---

## 6. 整合場景：兩者一起用

PR #72 (#54) 已實作 Claude in Chrome 透過 MCP 連接 Sirin (`http://127.0.0.1:7700/mcp`)：
- CORS layer + `chrome-extension://<id>` Origin 驗證 — 設備綁定 + extension 綁定。
- 暴露 `kb_search` / `kb_get` — Claude in Chrome 在任何網站都能查 AgoraMarket KB。
- KB Bearer token 留在 Sirin `.env`，永不進瀏覽器。

### 推薦工作流

```
人在用戶端：Claude in Chrome (探索 / 互動 / debug)
    ↓ MCP (127.0.0.1:7700)
Sirin (KB lookup, 未來: browser_exec → shadow_click)
    ↓ CDP
Flutter CanvasKit page
```

具體場景：
1. **探索→寫測試**：用戶用 Claude in Chrome 操作 AgoraMarket，發現 bug → Claude 呼叫 Sirin MCP 查 KB → 找到該頁的 AX tree 結構 → 產出 YAML test goal → Sirin batch 跑 regression。
2. **CI 失敗→debug**：Sirin batch run 失敗 → 用戶開 Claude in Chrome 連到同一頁 → Claude 透過 MCP 取得失敗 trace + KB → 互動式定位問題。
3. **混合測試**：Sirin 跑 100 條 Flutter regression（高速 batch），Claude in Chrome 跑 5 條跨瀏覽器或需要真人 cookie 的長尾場景。

---

## 7. Pilot #001 實機 Benchmark 結果（2026-04-27）

執行條件：PR #72 + #91 + #92 + #94 + #96 全 merge 後，CiC ↔ KB ↔ Sirin round-trip 已驗證。

### 7.1 方法

- 5 個代表性 test 涵蓋 Issue #59 規劃的 H1-H4
- Sirin 側: 5 × `run_test_async` 並行，從外部 Claude Code session 直接 curl `/mcp`
- CiC 側: 1 個 batch task 寫進 KB，user paste 1 次 prompt 觸發 CiC 連跑 5 個 + `kb_write` 結果回 KB
- Aggregate: 我從 KB 拉 10 個 result + 計分

### 7.2 戰績

| test_id | Sirin status | Sirin iters | Sirin dur(s) | CiC complete | CiC dur(s) | CiC actions | 勝者 |
|---|---|---|---|---|---|---|---|
| `wiki_smoke` | failed | 4 | 32 | 3 | 45 | 6 | **CiC** |
| `agora_buyer_wallet` | failed | 7 | 57 | 3 | 18 | 5 | **CiC** |
| `agora_pickup_time_picker` | error | 0 | 0 | 2 | 90 | 25 | **CiC** |
| `agora_admin_category_filter` | error | 0 | 0 | 1 | 60 | 12 | tie (兩邊各自失敗) |
| `agora_webrtc_permission` | error | 0 | 0 | 1 | 30 | 5 | tie (CiC 自知 CDP 限制) |
| **總計** | **0/5 success** | | **89s** (only 2 ran) | **10/15 + 5/5 self-aware** | **243s** | **CiC sweep** |

### 7.3 假設驗證

| H | 預測 | 實測 | 結論 |
|---|------|------|------|
| H0 | calibration tied | Sirin URL stuck on AgoraMarket（profile leak）→ failed; CiC win | ❌ Sirin profile 隔離 bug |
| H1 | Flutter 基礎 Sirin win | CiC 18s 完美讀錢包 vs Sirin 57s 失敗 | ❌ **DRAMATIC REJECT** |
| H3 | WebRTC permission CiC win | CiC 觸發 dialog 但承認 CDP 限制 | 🟡 部分（自知力 win）|
| H4 | CiC Flutter < 30% | CiC 60-80% (3 個 Flutter test 全有實質進展) | ❌ **REJECTED** |

### 7.4 三個重大發現

#### Finding 1 — CiC 的 vision + coordinate-click 戰略

`agora_buyer_wallet` notes（CiC 自述）：
> "Flutter CanvasKit app rendered visually. Bottom nav bar was clickable via coordinate. Clicked 錢包 tab successfully."

CiC LLM 看到 canvas 後**主動 fall back 到截圖 + 螢幕座標點擊**，繞過「沒 DOM 就沒辦法」的限制。Issue #59 規劃時完全沒預期這條路徑。**對日常 Flutter UI（tab、button、表單欄位）這個戰略涵蓋率高。**

#### Finding 2 — Sirin 並行多 test 撞 singleton Chrome

5 個 `run_test_async` 並行 → **3 個 hit `net::ERR_ABORTED` at iter 0 + dur 0ms**。Sirin 的 singleton Chrome session 在 CDP `Page.navigate` 並發下 race。已開 [Issue #98](https://github.com/Redandan/Sirin/issues/98)。短期修法 = `tokio::sync::Mutex` 包整個 test 執行段（serial queue）。

#### Finding 3 — KB read 路徑差異

CiC 用 `sirin.tools.kb_write` 寫進 KB 後，Sirin 自己的 `/mcp kb_get` 對該 topic_key 持續返回 "Not found" ≥ 5min；外部 Claude Code session 用 `mcp__agora-trading__kbGet` 直接路徑馬上看得到。建議外部 session pull KB result 走直接路徑，Sirin /mcp kb_get 適合 stable 條目。

### 7.5 為什麼不擴 full 19

Pilot 5 已涵蓋 H1-H4，趨勢明朗。增量 14 test 邊際 ROI 低。Round 2（CiC + Sirin MCP 整合）優先級反而更高 — 看 CiC 拉 sirin.tools.shadow_click 能不能補強 admin auth 那類 complex case。

---

## 8. 後續行動建議（Pilot #001 後修正）

1. **修 [Issue #98](https://github.com/Redandan/Sirin/issues/98) Sirin 並行多 test race**：短期 `tokio::sync::Mutex` queue，長期 multi-session 隔離 profile。**這是 Sirin batch 角色的根基**，比 #54 後半重要。
2. **`browser_exec` MCP tool 仍值得做但降一格**：CiC 純 vision-coordinate 對 Flutter 60-80%已 work，shadow_click 整合是 "coverage 補強" 而非「必要」。
3. **保留 Sirin batch / CI 路徑** — Pilot 證實 Sirin 真正不可替代角色就在 parallel + 結構化 history。CiC 一次只能 1 task。
4. **互動式 Assistant mode**（`src/assistant/` scaffold）優先級可降 — 這個生態位已被 Claude in Chrome 占住，Pilot 也證實 CiC vision-driven 探索能力勝過架構推理預期。
5. **KB 持續累積 + Sirin /mcp kb_get 讀延遲修**：Pilot 發現 Sirin client kb_get 跟直接 KB API 一致性問題。可能要 invalidate cache on kb_write，或加 `?bypass_cache=1` query。
6. **Round 2 (CiC + Sirin MCP integration test)** 比 full 19 test 更值得：用 CiC LLM + sirin.tools.browser_exec 看能不能補 admin auth / time picker 那類 case。

---

## 9. 結論（Pilot #001 後修正）

> **Sirin = parallel batch + 結構化 regression history（這是 CiC 一次只能 1 task 永遠補不到的）**
> **Claude in Chrome = vision-driven adaptive agent，包含 Flutter CanvasKit coordinate-click（比預期強）+ 跨網站通用 + 自知力高**

修正前認為 Sirin 的 Flutter 必殺優勢，Pilot 後權重要降——CiC vision-coordinate 已涵蓋 60-80% 日常 Flutter UI。**Sirin 真正不可替代的不是 Flutter 專長，是「能 batch 跑 100 條 regression 不需要人類介入」**。

兩者透過 KB-as-message-queue（PR #91/#92/#94/#96 串起來的 gateway + helper + kb_write）已實證可串成 round-trip 工作流：外部 Claude Code session 派 task 進 KB → user 1 次 paste 給 CiC → CiC 自動跑 + 寫 result 回 KB → 外部 session 拉 result aggregate。Pilot #001 用此 pattern 完成 5 test benchmark + 自動 aggregate report，總共 1 次人類介入。
