# Sirin Windows Widget

真正註冊在 Windows 11 Widgets Board 的本機小工具，不是浮動瀏覽器視窗。

## 功能

- 首頁優先顯示 Codex 推定工作中、最近活動、待命或閒置，以及判斷依據。
- 顯示兩分鐘內活動工作數、最近工作更新時間與 Token 增量趨勢。
- 顯示最近 12 次取樣的 Token/min 迷你趨勢圖、區間總增量與峰值；只存在記憶體，不寫入磁碟。
- 大型版補充 ChatGPT、Codex、Sirin 執行環境狀態、程序數與 working set。
- 網路縮成次要摘要；大型版仍保留 IPv4 / IPv6 分流證據。
- Sirin 離線或探測無回覆時顯示「缺證據」，不推斷為零或故障。
- 狀態來自本機時間戳與 Token 變化，屬明確標示的推定；不把「等待」誤報成「卡住」。

Widget Provider 每 60 秒只在 Widget 可見時讀取一次
`http://127.0.0.1:7700/api/ai-monitor`。按「立即更新」可手動刷新。
不執行下載測速、不改網路、不終止程序、不顯示 Codex 工作標題或訊息內容。

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


