# AgoraMarket 已知 Issues 追蹤文件

此文件追蹤 AgoraMarket 主要已知問題的驗收標準、背景與後續行動建議。

---

## Issue #34 — 質押功能驗收標準

**GitHub**: https://github.com/Redandan/AgoraMarket/issues/34  
**狀態**: 長期未解決（追蹤 9+ 個月）  
**報告者**: Ruiuiuiuiui

---

### 1. 問題分析 — 為何雙方認知不一致

此 issue 反覆出現「未修正 → 已處理 → 還是沒處理 → 真的好了 → 假的沒有好」的循環，根本原因是：

- **沒有明確 AC（Acceptance Criteria）**：developer 與 reporter 對「修好」的定義不同。
- **缺乏可重現的步驟**：reporter 提供的問題描述模糊，無法確認是否在相同情境下驗證。
- **沒有測試環境規格**：瀏覽器版本、OS、是否透過 Telegram Mini App 未知，行為可能因環境而異。
- **沒有自動化回歸**：修復後沒有測試保護，容易在後續重構中被意外破壞。

---

### 2. 建議的驗收標準（Acceptance Criteria）

以下每條標準均可觀測、可截圖驗證：

#### AC-1：質押頁面載入
- 質押頁面可正常開啟，無白屏或 JS error
- 顯示質押利率（數值非 `0`、非 `N/A`、非空白）
- 頁面顯示用戶當前可質押金額

#### AC-2：質押發起流程
- 輸入質押金額（有效數值）
- 點擊「確認/質押」按鈕
- 系統顯示確認對話框或 loading 狀態
- 操作成功後：餘額扣款反映正確，質押狀態變更為「質押中」或「進行中」
- 不出現 500 error 或 "Something went wrong" 訊息

#### AC-3：解質押流程
- 解質押按鈕可見且可點擊（非 disabled）
- 點擊後顯示確認對話框
- 確認後質押狀態更新為「已解質押」或「結算中」
- 不出現未預期的錯誤

#### AC-4：質押歷史記錄
- 歷史記錄頁面可正常開啟
- 能分別看到以下三種狀態的記錄（若有）：
  - 進行中（Active）
  - 已結算（Settled）
  - 已解質押（Unstaked）
- 各記錄顯示：金額、開始日期、利率、狀態

#### AC-5：金額顯示精度
- 質押金額與後端 API 回傳的小數位一致
- 不出現浮點精度問題（例如 `0.10000000001` 或 `0.0999999999`）

---

### 3. 需要 Reporter 提供的資訊

在 issue 回覆時，請使用以下模板向 reporter 要求補充：

```
感謝回報！為了精準重現問題，麻煩提供以下資訊：

**環境**
- 瀏覽器與版本（例：Chrome 124）
- 作業系統（Windows / macOS / iOS / Android）
- 是否透過 Telegram Mini App 使用？

**問題步驟**
- 進入質押頁面的路徑（哪個選單/按鈕）
- 具體在哪個操作失敗（發起質押 / 解質押 / 查看歷史）
- 輸入的金額（如適用）

**截圖**
- 失敗時的截圖或錄影
- Console 的錯誤訊息（F12 → Console tab 的截圖）

謝謝！
```

---

### 4. 後續行動建議

#### 短期
- [ ] 依上述 AC 在 staging/prod 手動驗收一輪，截圖留存
- [ ] 在 issue 上回覆確認每條 AC 的狀態（通過/失敗）

#### 中期
- [ ] 建立 `config/tests/agora_staking.yaml` 自動化瀏覽器測試，覆蓋 AC-1 至 AC-4
  - 作為回歸保護，防止後續重構意外破壞質押功能
- [ ] 若質押功能已確認正常，於 issue 中提供截圖佐證後關閉

#### 長期
- [ ] 若質押涉及多個獨立功能點（發起、解質押、歷史、利率計算），考慮拆成子 issue 分別追蹤

---

### 5. 與 Issue #70 的關係

Issue #70 是 2026-04-17 全日重構（-29,133 LOC）後的回歸驗收，目前回歸測試清單（`config/tests/agora_regression/`）覆蓋：

| YAML 檔案 | 功能 |
|-----------|------|
| `agora_webrtc_permission.yaml` | WebRTC 通話 permission |
| `agora_notification_delete.yaml` | 通知刪除 |
| `agora_pickup_service_default.yaml` | Pickup service type 預設值 |
| `agora_admin_category_filter.yaml` | Admin category filter |
| `agora_pickup_checkboxes_restore.yaml` | Pickup checkboxes 還原 |

**質押功能（Issue #34）目前未包含在 Issue #70 回歸清單中。**

建議：待 Issue #34 AC 確認後，若質押功能屬於本次重構的影響範圍，應新增 `agora_staking.yaml` 並加入回歸測試批次。

---

*文件建立：2026-04-19 | 維護者：Sirin AI*
