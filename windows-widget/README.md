# Sirin Windows Widget

真正註冊在 Windows 11 Widgets Board 的本機小工具，不是浮動瀏覽器視窗。

## 功能

- 首頁優先顯示 Codex 推定工作中、最近活動、待命或閒置，以及判斷依據。
- 顯示兩分鐘內活動工作數、最近工作更新時間與 Token 增量趨勢。
- 顯示最近一小時的 Token/min 迷你趨勢圖、區間總增量與峰值；每格代表 2 分鐘，取樣中斷會顯示空格。
- 大型版補充 ChatGPT、Codex、Sirin 執行環境狀態、程序數與 working set。
- 顯示 Sirin AI 待機防護、Windows 鎖定狀態、最近 Modern Standby 與重啟恢復證據；異常時給出可執行的本機提示。
- 大型版顯示整個 Sirin 程序 CPU／記憶體、監控 sampler thread 耗時／CPU 與趨勢檔大小；不同 scope 不混稱。
- 網路縮成次要摘要；大型版仍保留 IPv4 / IPv6 分流證據。
- Sirin 離線或探測無回覆時顯示「缺證據」，不推斷為零或故障。
- 狀態來自本機時間戳與 Token 變化，屬明確標示的推定；不把「等待」誤報成「卡住」。
- Token lifecycle 分開顯示 `IDLE`、`APPS_CLOSED`、`SOURCE_MISSING` 與 `GAP`，來源缺失時不顯示假速率。
- 顯示持久化實機驗收摘要；只有完整觀察閒置、程式關閉／恢復、鎖定／解鎖、待機／恢復與重啟接續週期才標示 `PASS`，不會自動觸發這些狀態。

Sirin 每 60 秒在本機記憶體保留一次輕量 Token／程序取樣；不執行網路探測。Widget Provider 每 60 秒只在 Widget 可見時讀取一次
`http://127.0.0.1:7700/api/ai-monitor`。按「立即更新」可手動刷新。
不執行下載測速、不改網路、不終止程序、不顯示 Codex 工作標題或訊息內容。Sirin daemon 會在 ChatGPT 執行期間管理 Windows execution request；Widget 本身只顯示結果，不持有電源要求。

## 建置與安裝

```powershell
.\scripts\validate.ps1
.\scripts\build.ps1
.\scripts\manage.ps1 -Action Install
.\scripts\manage.ps1 -Action OpenBoard
```

日常更新可直接執行 `manage.ps1 -Action Install -Build`。管理腳本會停止舊版
Provider、完成建置與登記、重新載入 Windows Widget 平台並要求開啟面板；既有
釘選狀態會保留。僅在除錯且不想重新載入平台時使用 `-SkipReload`。

Windows 不提供第三方程式自動 pin Widget 的正式 API。安裝後請在 Widgets
Board 的「新增小工具」選擇 `Sirin AI 工作監控`。開發版使用 Developer Mode
loose-package registration，不安裝憑證；`manage.ps1 -Action Remove` 可移除。

Picker 預覽資產遵循 Windows 規格：`300×304` PNG，四角透明。每次建置都會
驗證尺寸與透明角，避免套件已註冊但未出現在「新增小工具」清單。

Provider 架構依 Microsoft WindowsAppSDK Widgets C++ sample（MIT）實作。
