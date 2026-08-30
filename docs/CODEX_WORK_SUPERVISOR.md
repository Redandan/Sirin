# Codex Work Supervisor

Sirin 的低開銷、證據導向 Codex 工作督導器。它解決的是「任務在奇怪位置停止」與「驗收範圍沒有走完」，不是用 Token 變化猜測 Codex 內部狀態。

分類核心、本機 ledger、MCP 回報／快照／claim／完成介面與 AI Monitor 快照都由同一個 Sirin binary 提供。Daemon 與 Codex heartbeat 是否正在執行屬於部署狀態，不寫死在原始碼文件；驗收時必須即時確認正式 binary、MCP tool count、heartbeat 狀態與最新 supervisor snapshot。收到第一份結構化回報前，UI 會顯示 `WAITING_FOR_HEARTBEAT`。

## 架構

```text
Codex heartbeat（預設每 30 分鐘）
  list_threads → cheap filter → read_thread（最多 2 個）
        │
        ├─ 結構化證據 → codex_supervisor_report
        │                    │
        │                    └─ Sirin 本機分類 + coverage + dedupe ledger
        │
        └─ codex_supervisor_claim（每輪最多 1 個）
                 │
                 ├─ NO_ACTION → 結束，不通知
                 └─ CLAIMED → 重新讀 target → send_message_to_thread
                                      │
                                      └─ codex_supervisor_complete_action
```

Sirin daemon 不直接呼叫 Codex desktop task API。Codex heartbeat 負責讀取最新回合及發送續推；Sirin 只保存最小化結構證據、執行安全分類、產生一次性 claim 並做去重。

## 分類與動作門檻

| 分類 | 必要證據 | 動作 |
|---|---|---|
| `HEALTHY_RUNNING` | 新進度或仍開啟的工具操作 | 靜默 |
| `COMPLETED` | 最新 final 符合要求，且必要驗收面無缺口 | 靜默 |
| `WAITING_CORRECT_TIME` | 明確未到期的排程、外部事件或正確等待 | 靜默 |
| `USER_DECISION_REQUIRED` | 需要選擇、新核准、憑證、附件或人工驗收 | 通知，不續推 |
| `HIGH_RISK_AUTH_UNCLEAR` | 下一步涉及部署、push、刪除、正式環境、外部人員等，且精確授權不明 | 通知，不續推 |
| `SUSPECTED_STALL` | 只有靜止或慢，沒有中斷證據 | 下輪再觀察 |
| `RECOVERABLE_INTERRUPTION` | 原目標未完成、失敗／中斷／tool-result gap／驗收缺口，且不需新權限 | 去重後續推一次 |
| `CONTROL_STATE_MISMATCH` | 最新讀取與續推工具對 active turn 判斷矛盾 | 停止重試 |
| `UNREADABLE_OR_TIMEOUT` | 任務讀取、等待或解析失敗 | fail closed |

`active`、`idle`、`notLoaded`、Token 沒增加、任務標題或舊錯誤都不能單獨構成可恢復證據。

## 任務契約

Heartbeat 從最新使用者要求建立最小任務契約：

- `intent`：`ANALYZE`、`CHANGE`、`DEEP_CHECK` 或 `UNKNOWN`
- `authorizedActions`：`READ`、`EDIT`、`TEST`、`COMMIT`、`PUSH`、`DEPLOY`、`DEVICE_INSPECT`、`DEVICE_CONTROL`、`EXTERNAL_MESSAGE`
- `deepCheckRequested`
- `readOnly`

「分析／診斷／檢查」沒有自動修正權；「修正／處理／完成並驗證」若停在診斷才屬於未完成。缺少的 action 不可由 Sirin 或 heartbeat 自行補成授權。

## 驗收矩陣

可要求的 surface：

- `CODE`
- `UNIT_TEST`
- `INTEGRATION`
- `RUNTIME`
- `WEB_UI`
- `MOBILE_UI`
- `SIMULATOR`
- `PHYSICAL_DEVICE`
- `DEPLOYMENT`

每面狀態為 `PASS`、`FAIL`、`PENDING`、`MISSING_PROOF`、`NOT_APPLICABLE` 或 `UNKNOWN`。必要面仍為 `PENDING`／`MISSING_PROOF`／`UNKNOWN` 時，不能判定完整完成；若該面需要人類操作，分類為 `USER_DECISION_REQUIRED`，不會不停重試手機、信任、密碼或簽名流程。

## 本機 MCP 介面

### `codex_supervisor_report`

Codex heartbeat 回報一個任務的結構證據。`latestUserTurnKey` 與 `unfinishedScopeKey` 必須是 hash、cursor 或 opaque key，只允許安全鍵字元；放入自然語言會被拒絕。

### `codex_supervisor_snapshot`

讀取分類、驗收缺口、介入數量、可續推數量、最近派送結果與安全邊界。超過 15 分鐘的舊回報仍可保留在 ledger 供歷史與去重使用，但不再算作目前快照證據；若沒有新鮮回報，UI 回到 `WAITING_FOR_HEARTBEAT`。

### `codex_supervisor_claim`

每次最多領取一筆新鮮的 `RECOVERABLE_INTERRUPTION`。claim 有 10 分鐘 lease，只回傳 scope-preserving continuation prompt，不會傳送訊息或授權。

### `codex_supervisor_complete_action`

Codex 端嘗試 `send_message_to_thread` 後回寫：

- `ACCEPTED`
- `NOT_SENT`
- `FAILED`
- `TARGET_RUNNING`

只有 `ACCEPTED` 會啟動相同 fingerprint 的六小時去重窗。

## Heartbeat 執行預算

- 建議週期：30 分鐘，不跟目前 60 秒 Token sampler 綁在一起。這使例行模型喚醒從每天 288 次降為 48 次；若實際漏接中斷，再以證據調短，而不是先提高頻率。
- 每輪列出最多 50 個近期非 pinned 任務，加上全部 pinned。
- routine mode 最多深讀 2 個強候選。
- 每個候選先讀最近 3 回合；只有授權／完成狀態不清楚時才擴到 6 回合。
- 活躍但不明的任務只做一次 bounded wait snapshot。
- 每輪最多 claim／續推 1 個任務。
- 沒有恢復、人工介入或掃描錯誤時保持靜默。
- 不自動 fork，不建立平行任務，不讓兩個任務同時修改同一工作樹。

## 隱私與安全

- 本機檔案：`platform::app_data_dir()/tracking/codex_supervisor.json`
- 隔離驗收可設定 `SIRIN_CODEX_SUPERVISOR_STATE_PATH`；正式環境通常不設定。
- 不保存 prompt、回覆、任務標題、command output、憑證或 API key。
- 只保存 thread/host ID、opaque turn/scope key、布林證據、enum、coverage 與 dispatch metadata。
- Sirin 不能授予 Codex approval，也不能直接向 Codex 任務送訊息。
- claim 不代表使用者核准；Codex heartbeat 派送前仍必須重新讀取目標任務。
- 高風險授權不明、人工步驟、不可讀資料與控制狀態矛盾一律 fail closed。

## 必要驗收案例

1. 新鮮 active task → `HEALTHY_RUNNING`，不送訊息。
2. 舊 interrupted、但較新回合已完成 → `COMPLETED`。
3. 最新回合明確 interrupted，原目標仍授權且未完成 → 只續推一次。
4. 只要求分析，已交付根因但沒有修正 → `COMPLETED`，不擅自 edit。
5. 要求修正並驗證，卻停在根因 → `RECOVERABLE_INTERRUPTION`。
6. 等待 commit/deploy 新核准 → `USER_DECISION_REQUIRED` 或 `HIGH_RISK_AUTH_UNCLEAR`。
7. 深度檢查只有 Rust 測試，Web UI／手機仍缺證據 → 不得判定完整完成。
8. 真機需要 passcode、trust 或其他人類步驟 → 通知，不代操作。
9. 同 fingerprint 六小時內 → 不重複續推。
10. 一個不可讀任務不影響其他候選，該項本身 fail closed。
