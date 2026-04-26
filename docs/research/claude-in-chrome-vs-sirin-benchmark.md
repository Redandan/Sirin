# Claude in Chrome vs Sirin — 網頁操作能力評測

> Issue: [#59](https://github.com/Redandan/Sirin/issues/59)
> 相關 PR: [#72](https://github.com/Redandan/Sirin/pull/72) — Claude in Chrome MCP 整合 (#54)
> Benchmark 對象: AgoraMarket Flutter CanvasKit PWA
> 狀態: 架構/能力對比研究（無實機 benchmark 數據）

---

## 0. TL;DR

兩者**互補不互斥**。

- **Sirin** = 自動化測試引擎（regression / batch / CI），能直驅 Flutter CanvasKit semantics tree。
- **Claude in Chrome (Beta)** = 互動式探索瀏覽器（debug / 人工驅動），原生 DOM 強項，但對 Flutter canvas 限制大。
- **#54 / PR #72** 完成後，Claude in Chrome 可透過 Sirin MCP（`kb_search`、`kb_get`，未來 `browser_exec`）取得 Sirin 的 Flutter 操作能力 — 這是「兩個工具一個工作流」的關鍵橋。

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

### 4.5 推論：H4 假設驗證

Issue #59 的 **H4：CiC 無 shadow_click → Flutter 點擊成功率 < 30%** 在架構上幾乎一定成立。
這代表 **PR #72 的 `browser_exec` 整合（未來）不是 nice-to-have，是 Claude in Chrome 操作 AgoraMarket 的必要條件**。

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

## 7. 假設未測 — Round 2 待跑

下列項目需要實機 benchmark 才能定論（issue #59 提到的執行階段）：

| 假設 | 狀態 | 預計驗證方式 |
|------|------|--------------|
| H1: Flutter 互動 Sirin 勝率 ≥ 80% | 待測 | 19 個測試案例對跑 |
| H2: 純視覺判斷兩者無顯著差異 | 待測 | `cal_*` 校準 + `agora_admin_status_chip` |
| H3: WebRTC permission CiC 勝 | 待測 | `agora_webrtc_permission` |
| H4: CiC Flutter 點擊成功率 < 30% | **架構上幾乎必成立** | `agora_navigation_breadcrumb` 一個就能驗 |

實機 benchmark 待 Round 2（PR #72 merge 後 + Claude in Chrome Beta access 確認）執行。

---

## 8. 後續行動建議

1. **加速 #54 的後半 — `browser_exec` MCP tool**：PR #72 只暴露 KB，下一步把 `shadow_click` / `shadow_find` / `flutter_type` 也透過 MCP 暴露。這是 Claude in Chrome 操作 AgoraMarket 的必要橋樑。
2. **保留 Sirin batch / CI 路徑** — 不要為了「整合」削弱 Sirin 自身的 standalone 能力。
3. **互動式 Assistant mode**（`src/assistant/` scaffold）優先級可降 — 這個生態位已被 Claude in Chrome 占住。
4. **KB 持續累積** — Sirin 的 KB 是兩者都在吃的共享資產，是未來 moat。

---

## 9. 結論

> **Sirin = 自動化測試 / regression / 大量 batch / Flutter CanvasKit 專家**
> **Claude in Chrome = 互動式 debug / 探索 / 用戶 driven / 跨網站通用**

兩者透過 MCP 串成一個工作流：人在 Chrome 探索與決策，Sirin 在背景做高速 regression 和 KB 知識累積。
這個分工讓 Sirin 不需要追平 Anthropic 官方 agent 的通用性，而是專注在 Flutter / 自動化 / 並行這幾個 Anthropic 不會做的方向。
