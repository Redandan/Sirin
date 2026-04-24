# Design: Live GUI Monitor

**Status:** Implemented — see `docs/MCP_API.md § Live Monitor` for the live reference
**Target tier:** Tier 1 (observability / debuggability)
**Depends on:** `DESIGN_AUTHZ.md`(AuthZ ask prompts 在此 UI 浮現,但可獨立先上)
**Enables:** trace viewer replay mode(歷史 ndjson replay 重用同個 UI)

---

## 1. Why

現狀:外部 AI(Claude Desktop / Code / Cursor)用 MCP 呼叫 Sirin 時,**人類完全看不到**正在發生什麼:

- 哪個 client 在跑
- 跑了哪些 action / 結果是什麼
- 當前 browser 頁面長怎樣
- 有沒有 authz ask 等我回應
- 出錯在哪一步

要查只能事後翻 log。需要**實時 GUI**,滿足:

- **看**:即時看 browser 螢幕 + action feed + console / network
- **審**:authz ask 浮出來,一鍵 Allow/Deny
- **控**:Pause / Step-through / Abort
- **溯**:過去 N 分鐘的 trace 可以 replay

## 2. Goals / Non-goals

### Goals

- 加入 Sirin 既有 egui 主 UI(`src/ui_egui/`)作為新的 view,**零新 GUI 框架**
- Screenshot stream(取樣 500ms)+ action feed(即時)+ authz ask 浮層
- Pause / Step / Abort 控制能中斷外部 AI 的操作流
- Trace ndjson 寫檔,另一個 replay UI(同個元件,改 source)能看歷史
- 可選 WebSocket server,讓遠端觀察者(瀏覽器 / 第二台機器)也能接

### Non-goals

- **不重寫既有 UI**:`ui_egui` 架構保留,Monitor 只是新 tab / view
- **不做 Multi-user auth**:UI 本地 127.0.0.1 only,不做登入
- **不取代 audit log**:UI 是**視覺化**audit log 的即時版,不是替代;log 仍是 source of truth
- **Phase 1 不做 Systray**:系統列 icon 放 Phase 2,core feature 先出

## 3. Architecture

### 整合既有架構

Sirin 現有(`docs/ARCHITECTURE.md`):

```
sirin.exe (單一進程)
├── eframe + egui 0.31 (immediate mode UI)
│   └── ui_egui/ {mod, sidebar, workspace, browser, log_view, settings, ...}
├── ADK Runtime + Agents
├── Memory / Events / MCP server (axum) / RPC
└── Tokio async backend
```

Monitor 加入方式:

```
ui_egui/
├── mod.rs                ← 加 MonitorView enum + routing
├── sidebar.rs            ← 加 "Monitor" 選項
└── monitor/              ← 【新】
    ├── mod.rs            view 進入點
    ├── action_feed.rs    動作 feed panel
    ├── screenshot_pane.rs 即時截圖 panel
    ├── control_bar.rs    Pause / Step / Abort + status
    ├── authz_modal.rs    authz ask 浮層
    ├── network_pane.rs   network 請求 list(折疊)
    ├── console_pane.rs   console log(折疊)
    └── state.rs          MonitorState(共享 state + event queue)
```

### Event flow

```
┌──────────────┐      mpsc::Sender<MonitorEvent>      ┌──────────────┐
│   mcp_server │─────────────────────────────────────▶│              │
│  / browser / │                                      │ MonitorState │
│  authz / cdp │                                      │   (Arc<RwLock>)
└──────────────┘                                      │              │
                                                      └──────┬───────┘
                                                             │ (egui polls every frame)
                                                             ▼
                                                      ┌──────────────┐
                                                      │  ui_egui     │
                                                      │  /monitor/*  │
                                                      └──────────────┘
                                                             │
                                                             │ user interaction
                                                             ▼
                                                      ┌──────────────┐
                                                      │ control send │
                                                      │  ControlCmd  │
                                                      └──────┬───────┘
                                                             │ mpsc
                                                             ▼
                                                      ┌──────────────┐
                                                      │ authz / exec │
                                                      │  awaits cmd  │
                                                      └──────────────┘
```

### Screenshot pump

獨立 tokio task,**只在有 MonitorView active 時才跑**:

```rust
async fn screenshot_pump(
    state: Arc<RwLock<MonitorState>>,
    browser: Arc<BrowserHandle>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    loop {
        interval.tick().await;
        if !state.read().await.view_active { continue; }
        if state.read().await.paused_stream { continue; }
        match browser.screenshot_jpeg(80).await {
            Ok(bytes) => state.write().await.push_screenshot(bytes),
            Err(e)    => state.write().await.push_error(e),
        }
    }
}
```

策略:
- JPEG quality 80,壓低傳輸成本(egui texture upload)
- `view_active` + `paused_stream` 兩層 gate,不看就不花 CPU
- 失敗一次不重啟(log 就好),避免雪崩

### Control state

```rust
pub struct ControlState {
    pub paused:   AtomicBool,    // 暫停所有 action,等 resume
    pub step:     AtomicBool,    // 跑下一個 action 後自動 paused=true
    pub aborted:  AtomicBool,    // 立即拒絕所有後續 action
}

impl ControlState {
    /// 每個 action 進入時 gate 一下。paused 時輪詢等,aborted 直接 error。
    pub async fn gate(&self) -> Result<(), AbortError> {
        if self.aborted.load(Relaxed) { return Err(AbortError); }
        while self.paused.load(Relaxed) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.aborted.load(Relaxed) { return Err(AbortError); }
        }
        if self.step.swap(false, Relaxed) {
            self.paused.store(true, Relaxed);
        }
        Ok(())
    }
}
```

MCP server 的 `call_browser_exec` 在 authz 之後、executor 之前插入 `control.gate().await`。

## 4. UI spec

### Main layout(egui)

```
┌─ Sirin ────────────────────────────────────────────┐
│ [Workspace] [Browser] [Monitor●] [Settings] [Logs] │ ← sidebar,● 表示 active session 中
├─ Monitor ──────────────────────────────────────────┤
│                                                    │
│ Status: 🟢 Active  Port 7703  │ clients:           │
│                                │ • claude-desktop  │
│                                │ • claude-code     │
│                                                    │
│ ┌─────────────────────────┐ ┌─ CONTROL ──────────┐ │
│ │                         │ │ ▶ Running          │ │
│ │  [screenshot ~640x400]  │ │ [Pause] [Step]     │ │
│ │                         │ │ [Abort]            │ │
│ │  URL: /wallet/withdraw  │ │                    │ │
│ └─────────────────────────┘ └────────────────────┘ │
│                                                    │
│ ┌─ ACTION FEED ───────────────────────────────────┐│
│ │ 12:34:10.123  claude-desktop                    ││
│ │   ax_find role=button, name=確認提款             ││
│ │   ↳ {backend_id:42, name:"確認提款"}            ││ ← expandable row
│ │ 12:34:10.456  claude-desktop                    ││
│ │   ax_click 42   ✓ (38ms)                        ││
│ │ ─────────────────────────────────────────────── ││
│ │ ⚠ 12:34:11.789  claude-desktop  AUTHZ ASK       ││
│ │   goto https://docs.flutter.dev/test            ││
│ │   not covered by any rule                       ││
│ │   [Allow once] [Allow URL*] [Allow action*]     ││
│ │   [Deny] [Deny + block URL]                     ││ ← 點擊送 AuthzDecision
│ │ ─────────────────────────────────────────────── ││
│ │ 12:34:13.002  claude-code                       ││
│ │   ax_tree → 55 nodes                            ││
│ │ ...                                             ││
│ └─────────────────────────────────────────────────┘│
│                                                    │
│ [Console 0 err] [Network 5 req] [A11y 55 nodes]   │ ← 折疊 panel
│                                                    │
│ [Export trace.ndjson] [Clear] [Replay mode...]    │
└────────────────────────────────────────────────────┘
```

### Action feed row

每行:

- **時間戳**(`HH:MM:SS.mmm`)
- **client id**(彩色 chip 區分)
- **action + 關鍵 args**(一行摘要)
- **結果 icon**(✓ 綠,✗ 紅,⚠ 黃 ask,⏸ 灰 paused)
- **耗時**(action_done 回來才填)
- **展開區**(點擊):完整 args json、完整 result json、關聯 screenshot(前後對比)

AuthZ ask row 特殊:按鈕內嵌在 row 裡,點擊送 `AuthzDecision` 給等待的 task。超時計時器(預設 30s)顯示在 row 尾。

### Screenshot pane

- 頂部顯示當前 URL(點擊 → 複製到 clipboard)
- 圖片 fit-height-aspect-preserve
- 底部 toolbar:
  - `⏸ Pause stream` / `▶ Resume stream`(不影響 action gate)
  - `💾 Save PNG`(存當前 frame)
  - `🔍 Open full` (egui window)
- Stream 斷線時顯示「reconnecting…」並持續重試

### Control bar

- Status dot:🟢 active / 🟡 paused / 🔴 aborted / ⚪ idle (no recent activity)
- Pause:set `paused=true`,in-flight action 仍會做完,下一個開始前卡住
- Step:先 set `paused=false` 讓當前通過,下一個 action 自動 pause(UI icon 變「⏭ 1 queued」)
- Abort:set `aborted=true`,popup 確認「This will reject all further actions. Continue?」
- 視覺:pause 時整個 UI 淡黃底色,abort 時紅底色,一眼看出來

### AuthZ modal vs inline

兩種呈現可切換(settings):

- **Inline**(預設):ask 直接插進 action feed,feed 繼續滾
- **Modal**:浮出 window 擋住 UI,強制先回應。適合獨立 display

## 5. WebSocket server(optional)

給遠端觀察:

```
listen on 127.0.0.1:<SIRIN_WS_PORT || 7703 + 1>
path: /monitor/ws
protocol: JSON text frames (not binary)
```

### 訊息 schema

```typescript
// Server → Client
type ServerEvent =
  | { type:"hello", ts, sirin_version, clients:string[] }
  | { type:"action_start", id, ts, client, action, args }
  | { type:"action_done",  id, ts, result, duration_ms }
  | { type:"action_error", id, ts, error }
  | { type:"authz_ask",    request_id, client, action, args, url, timeout_ms, learn:bool }
  | { type:"authz_resolved", request_id, decision }
  | { type:"screenshot",   ts, jpeg_base64 }                        // 500ms tick
  | { type:"url_change",   ts, url }
  | { type:"console",      ts, level, text }
  | { type:"network",      ts, url, method, status, size, req_body_preview, res_body_preview }
  | { type:"state",        paused, step, aborted, view_active }
  | { type:"goodbye" }

// Client → Server
type ClientCommand =
  | { type:"authz_response", request_id, decision:"allow_once"|"allow_always_url"|"allow_always_action"|"deny"|"deny_block" }
  | { type:"pause" }
  | { type:"resume" }
  | { type:"step" }
  | { type:"abort" }
  | { type:"subscribe", channels:("screenshot"|"action"|"network"|"console")[] }
```

### 安全

- **只綁 127.0.0.1**,外網存取要自己 SSH tunnel
- 可選 token:啟動時生成 random token,WS URL 要 `?token=<>`,錯 → 401 close
- Origin check:只接 `http://localhost:*` / `http://127.0.0.1:*`

### 關係

egui 主 UI 是 first-class(共享 process memory,免序列化)。WS 是**額外**的觀察 channel,schema 跟 egui 看的是同一份 `MonitorState` 轉 JSON,不分兩套邏輯。

## 6. Trace ndjson

### 寫檔

每個 Sirin session 開啟時建立 `<repo>/.sirin/trace-<ISO8601>.ndjson`,與 audit log 分開(audit 是 authz 決定,trace 是**全部**action 時序)。

Event 同 WS server 的 `ServerEvent` schema,序列化一樣。

### Rotation

- 新 session 新檔
- 單檔超過 100 MB auto-rotate(.1, .2),保留最多 20 個

### Replay mode

egui 新的 `Replay` view:

- 選擇 ndjson 檔 / 拖拉進來
- UI 用**同一組 component** render,但 event source 從 mpsc channel 變 file iterator
- 時間軸:拉 scrubber 到任何時點,state 重建到那個 moment
- 速度:0.5x / 1x / 2x / 5x / instant seek
- Filter:只看某個 client、某種 event type

這直接實現 ROADMAP Tier 1 T2(Trace viewer),不用寫第二套 UI。

## 7. Implementation plan

### Files

```
src/
├── monitor/
│   ├── mod.rs              模組 API:init()/emit_*(event) helpers
│   ├── state.rs            MonitorState(Arc<RwLock<>>) + event queue
│   ├── events.rs           ServerEvent / ClientCommand enum
│   ├── screenshot_pump.rs  tokio task
│   ├── trace_writer.rs     NDJSON writer + rotation
│   └── ws.rs               【optional】axum WebSocket handler
│
├── ui_egui/
│   ├── mod.rs              加 MonitorView 路由
│   ├── sidebar.rs          加 "Monitor" 入口
│   └── monitor/
│       ├── mod.rs          view::show(ui, state)
│       ├── action_feed.rs
│       ├── screenshot_pane.rs
│       ├── control_bar.rs
│       ├── authz_modal.rs
│       ├── network_pane.rs
│       └── console_pane.rs
│
└── mcp_server.rs           加 control.gate() + emit events
```

### PRs

| # | 內容 | 規模 | 相依 |
|---|---|---|---|
| M1 | `src/monitor/state.rs` + `events.rs` + emit helpers | 1/2 day | - |
| M2 | `mcp_server.rs` hook:每個 action 前後 emit event | 1/2 day | M1 |
| M3 | `src/monitor/screenshot_pump.rs` + browser JPEG API | 1 day | M1 |
| M4 | `src/monitor/trace_writer.rs` + rotation | 1/2 day | M1 |
| M5 | `src/ui_egui/monitor/` — action_feed + screenshot_pane 雛形 | 1-2 days | M1, M3 |
| M6 | `src/monitor/control.rs` + Pause/Step/Abort + UI control_bar | 1 day | M5 |
| M7 | `src/ui_egui/monitor/authz_modal.rs` + 整合 `DESIGN_AUTHZ` ask | 1/2 day | M5, AUTHZ PR 5 |
| M8 | `src/monitor/ws.rs`(WS server)+ auth token | 1 day | M1, M2 |
| M9 | Replay mode(load ndjson,重用 monitor UI) | 1-2 days | M4, M5 |

**Phase 1(核心 UI,無 WS,無 replay)**:M1+M2+M3+M5+M6 ≈ 4 days
**Phase 2(+ authz 整合 + WS + replay)**:M7+M8+M9 ≈ 2.5 days
**Total**:~7 days full time

### Dependencies (Cargo.toml)

全部現有 deps 夠用:

- `eframe 0.31` / `egui 0.31` ✓ 已有
- `axum 0.8` with `ws` feature ✓ 已有
- `tokio 1.37` ✓ 已有
- `serde / serde_json` ✓ 已有
- `image 0.25` ← 新增,egui JPEG decode(或用 `egui_extras`)

## 8. Performance budget

- Screenshot pump:500ms interval,JPEG 80% 640×400 典型 ~50 KB → 100 KB/s peak
- Action feed:每個 event ~500 bytes,100 events/s 也才 50 KB/s
- UI frame:egui repaint 60 FPS,只 repaint 變更區域,當無新 event 時背壓 10 FPS
- 記憶體:MonitorState 保留最近 1000 個 action + 50 個 screenshot frame(~5 MB),超過淘汰

目標:Monitor active 時 Sirin 整體 CPU <5%、額外 RAM <50 MB。

## 9. Test plan

### Unit

- `MonitorState` event queue 滿時淘汰邏輯
- `trace_writer` rotation 邊界
- `control.gate()` 對 pause / step / abort 的各組合

### Integration

- 起 Sirin,從 egui 進 Monitor view,發 MCP call,驗 feed 有 row
- Pause 後發 action,驗 action 卡住;Resume 後完成
- Abort 後發 action,MCP 直接 error
- WS client 接 ws://127.0.0.1:7704/monitor/ws,驗 event 收到

### E2E

- AgoraMarket phase3-atomic.sh 跑一遍,Monitor 從頭看到尾,screenshot 同步 + 所有 action feed 有記錄 + trace ndjson 完整

## 10. Open questions

1. **Sirin 已跑 no-UI 模式(CLI / headless)時 Monitor 如何?** → 建議自動停用 GUI 部分,保留 trace ndjson + WS server(讓遠端 UI 接)。
2. **多 Sirin instance 時 WS 埠怎麼分?** → `SIRIN_RPC_PORT=7703` 時 WS 用 `7703 + 1000 = 8703`,避開常用 port
3. **Screenshot 對 CanvasKit 負擔**:CDP `Page.captureScreenshot` 每 500ms 對 Flutter GPU pipeline 有無明顯影響?→ 需 benchmark,必要時降到 1s。
4. **Trace ndjson 隱私**:包含所有 URL / ax_tree / 可能包含 console 機敏文字。→ `.sirin/` 要 gitignore;可選 `redact_patterns` 在寫檔前做
5. **egui 能 render JPEG base64 嗎?** → 需 `egui_extras::install_image_loaders` + `image` crate decode,egui 0.31 有。

## 11. 跟 DESIGN_AUTHZ 的合體

兩個設計獨立,但有整合點:

- **AUTHZ PR 5**(`ask_human()` 實作):有 Monitor active 時推送 `authz_ask` event,等 `AuthzDecision` 回來;無 Monitor 時立即 fallback 成 deny
- **Monitor M7**(authz_modal):訂閱 `authz_ask` event,渲染 ask row / modal,收集用戶點擊回送 decision

沒 Monitor 也能用 AuthZ(fallback deny),沒 AuthZ 也能用 Monitor(只有 action feed 沒 ask),兩者是 additive。

## 12. 長遠 vision

Phase 3(不在本 PR):

- **Systray**:`tray-icon` crate,icon 顏色反映 state,右鍵菜單 pause all / abort all / open monitor
- **Multi-session view**:一個 Sirin instance 同時服務多個 MCP client,UI 加 tab 切換看每個 client 各自的 feed
- **Bookmark 時點**:看到有趣 event 按 B 標記,之後 replay 時快速跳
- **Export HTML report**:從 trace ndjson 生成靜態 HTML,分享給同事看 bug 復現

這些都是 Phase 1 完成後的 nice-to-have,不擋核心交付。
