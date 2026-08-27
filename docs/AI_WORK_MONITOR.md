# AI Work Monitor

Windows 本機 AI 工作監控，整合在 Sirin 既有 daemon、tray、`/ui/`，並提供 Windows 11 Widgets Board 原生小工具。

## 開啟方式

- Sirin UI 側欄：`AI 工作監控`
- tray：`Open AI Work Monitor`
- URL：`http://127.0.0.1:7700/ui/?view=ai-monitor`
- Windows 小工具：安裝後按 `Win + W`，在小工具選擇器搜尋 `Sirin AI 工作監控`

直接開啟監控頁時，不啟動原本每 2 秒的全域 snapshot/WebSocket；切換到其他 Sirin 頁面後才會啟動。Sirin 每 60 秒唯讀採樣一次 Codex token 與程序資訊，保留最近 60 點；網路路由與 ping 仍只在 `/api/ai-monitor` 被讀取時執行，且共用 60 秒快取。監控頁每 15 秒讀取 API 快照，Windows 小工具在可見/啟用期間最多每 60 秒讀取一次。

## 證據來源

| 資料 | 來源 | 標示 |
|---|---|---|
| IPv4/IPv6 預設路由、各自的 interface metric、介面 | Windows `netsh` active store | `MEASURED` |
| Wi-Fi signal、link rate、radio、channel | Windows `netsh wlan` | `MEASURED` |
| RSSI | signal quality 換算近似值 | `INFERRED` |
| IPv4/IPv6 latency | 各一次低成本 ICMP probe | 有回覆為 `MEASURED`；無回覆為 `MISSING_PROOF` |
| ChatGPT/Codex/Sirin 程序與 working set | Windows `tasklist` | `MEASURED` |
| Codex task token | `%USERPROFILE%\.codex\state_5.sqlite` 的 `threads.tokens_used`，唯讀開啟 | `MEASURED` |
| Codex token 趨勢 | 兩次重疊的近期工作快照相減 | `INFERRED` |
| Codex 活動狀態 | `updated_at_ms` 的時間新鮮度 | `INFERRED` |
| Sirin 小隊 token | 既有 `multi_agent::usage` 近 5 分鐘資料 | `MEASURED` |
| Windows 鎖定狀態 | WTS current-session `SessionFlags` | `MEASURED` 或 `MISSING_PROOF` |
| Modern Standby | System event log 的 Kernel-Power 506 / 507 | 成功讀取事件記錄為 `MEASURED` |
| AI 待機防護 | Sirin sampler thread 的 Windows execution-state 呼叫結果 | 成功設定／清除並與 ChatGPT 程序狀態一致為 `MEASURED` |
| 監控 thread 成本 | sampler thread 的 wall time 與 Windows thread CPU time | `MEASURED`；只代表監控 thread |
| Sirin 程序資源 | Windows process CPU time、working set、private bytes | `MEASURED`；明確標示為整個 Sirin，不冒充監控模組獨占成本 |
| Codex 本機資源健康 | Windows `GetSystemTimes`、`GlobalMemoryStatusEx`、`GetDiskFreeSpaceExW` 與既有程序 working set | 每 60 秒本機取樣；CPU 使用跨取樣區間，RAM／系統碟為即時容量，不啟動 PowerShell/WMI |
| Codex 進度健康 | thread 更新時間、Token 增量、Codex 程序存在與上述資源壓力 | 有增量可標示 `HEALTHY_ACTIVITY`；沒有更新只能是 `WAITING_OR_IDLE`／`MISSING_PROOF`，不能單獨判成卡住 |
| 本機 I/O 成本 | collector/cache/write 計數器與 Token 趨勢檔 metadata | `MEASURED`；程序重啟後重新計數 |
| 狀態週期驗收 | 固定事件種類的首次／最後時間與次數；鎖定使用 WTS、待機使用 Kernel-Power 與取樣 GAP 關聯 | 有完整週期才 `PASS`；其餘保持 `PARTIAL`／`MISSING_PROOF` |

`MISSING_PROOF` 不會被轉寫成離線、故障或零用量。

`codex_health` 把狀態拆成進度、本機資源、網路與遠端限制。CPU 最近區間達 90%、可用 RAM 低於 15%／commit 達 90%，或系統碟低於 15 GB／8% 時顯示警告；更嚴格的 98%、8%／95%、5 GB／3% 顯示嚴重。這些門檻只產生本機警示，不會終止程序、清理檔案或修改路由。遠端模型限流與帳號額度除非有明確結構化錯誤，固定保持 `MISSING_PROOF`。

Token 趨勢的 `lifecycle` 會明確區分 `BASELINE`、`ACTIVE`、`IDLE`、`APPS_CLOSED`、`SOURCE_MISSING` 與 `GAP`。Codex SQLite 暫時不可讀時，Sirin 會保留上一個有效基線、記錄缺證據點且停止推算；來源恢復但間隔超過 90 秒時只重建基線，不把中斷期間的差額換算成速率。AI 程式關閉但資料來源仍可讀時則記為 `APPS_CLOSED`，不與來源故障混淆。

`acceptance` 會把 `IDLE`、程式關閉後恢復、鎖定後解鎖、Modern Standby 後取樣重建、Sirin 重啟後歷史恢復列為必要週期，另把 Token 來源中斷後恢復列為診斷週期。只看到單邊事件時是 `PARTIAL`，沒有事件則是 `MISSING_PROOF`；不會為了讓畫面變綠而自動鎖定、睡眠、重啟或關閉 AI 程式。

## 安全邊界

- Codex 查詢只讀取工作 ID、`tokens_used`、更新時間；不查詢或顯示標題、prompt、訊息內容。
- 不讀或顯示 API key、登入憑證、程序 command line。
- 不改路由、不終止 task/process、不部署、不設定開機自啟。
- 不送 telemetry；最近一小時的 Token 統計與固定 9 種狀態事件的首次／最後時間及次數，以原子替換方式保存在 Sirin 本機 tracking 目錄，內容不含工作標題或訊息。
- 背景 sampler 不執行網路探測或下載測速；API 觸發的路由／ICMP 收集最多每 60 秒一次，手動下載位元組另行計數。
- 背景 sampler 平常不查詢事件記錄；只有取樣間隔超過 90 秒形成 `GAP` 時，才讀一次本機 Kernel-Power 記錄來保存待機／恢復關聯，次數會列在 overhead。
- 下載測速是 `ai_monitor_speed_test` MCP action，只有按下按鈕才會向 Cloudflare endpoint 下載 5 MB；不會背景執行。

## 電源、鎖定與恢復

當 ChatGPT 程序存在時，Sirin 的每分鐘 sampler thread 會維持 Windows `SYSTEM_REQUIRED` 與 `DISPLAY_REQUIRED` execution request；ChatGPT 關閉或 Sirin 結束時會在同一 thread 清除。它不模擬滑鼠／鍵盤、不使用 away mode，也不解鎖 Windows。這代表防止閒置待機與螢幕自動關閉，不代表已鎖定的桌面仍可被 UI automation 操作。

`/api/ai-monitor` 同時回傳目前 Windows session 是否鎖定、最近一次 Modern Standby 進入／恢復時間、sampler 取樣年齡、趨勢是否從磁碟恢復，以及具體的本機告警。Kernel-Power 查詢快取 60 秒；網路探測與下載測速不會因此增加背景流量。

正式 daemon 驗證新守護後，可用可回滾腳本移除舊的 `ChatGPTKeepAwake` 登入啟動值與精確匹配的舊 PowerShell 程序；舊腳本檔會保留作回滾：

```powershell
.\scripts\migrate-chatgpt-awake-guard.ps1 -Action Status
.\scripts\migrate-chatgpt-awake-guard.ps1 -Action Migrate
.\scripts\migrate-chatgpt-awake-guard.ps1 -Action Rollback -BackupPath <exact-backup-json>
```

## Windows 11 小工具

`windows-widget/` 是 Windows App SDK C++/WinRT 的 packaged Widget Provider。卡片使用 Adaptive Cards，僅連到 `http://127.0.0.1:7700/api/ai-monitor`，不經系統 proxy、不啟動測速，也不包含工作標題或訊息內容。Token 圖固定為最近 60 分鐘、每格 2 分鐘；睡眠、重啟或取樣中斷會保留空格，不把不等長區間畫成等距連續資料。

Token 趨勢會以單一、原子替換的 JSONL 快照保存在 `platform::app_data_dir()/tracking/ai_monitor_token_trend.jsonl`，只包含最近 60 個統計點、最多 8 個不透明工作 ID 與其累積計數，以及固定 9 種狀態事件的時間／次數，不保存工作標題、訊息、提示詞、回覆或憑證。啟動時只恢復最近一小時內的趨勢資料；驗收事件保留首末時間，避免真實鎖定或程式關閉證據在一小時後消失。超過 90 秒的睡眠、停機或取樣中斷會重新建立基線，該區段不計入速率與圖表總量。

候選版本的隔離驗證會暫時設定 `SIRIN_AI_MONITOR_TREND_PATH`，讓 alternate-port smoke test 使用獨立的暫存快照，並設定 `SIRIN_AI_MONITOR_DISABLE_AWAKE_GUARD=1`，避免候選程序額外持有 execution request。正式與候選程序不會同時改寫正式趨勢檔或同時管理待機防護。這兩個變數只供部署守門與測試使用。

```powershell
.\windows-widget\scripts\build.ps1
.\windows-widget\scripts\manage.ps1 -Action Install
.\windows-widget\scripts\manage.ps1 -Action OpenBoard
```

若 C 槽空間有限，可把 NuGet、編譯中間檔與安裝 layout 全部放到 D 槽：

```powershell
.\windows-widget\scripts\build.ps1 -BuildRoot D:\SirinWidgetBuild
.\windows-widget\scripts\validate.ps1 -BuildRoot D:\SirinWidgetBuild
.\windows-widget\scripts\manage.ps1 -Action Install -BuildRoot D:\SirinWidgetBuild
```

開發環境用 Developer Mode 的 loose package registration，不需把自簽憑證加入受信任根憑證。Windows 沒有支援的第三方自動釘選 API，第一次需由使用者在 Widgets picker 按一次 `+`。

## 隔離驗收模式

`sirin.exe --ai-monitor-only` 只註冊本機 UI/AppService 並啟動 loopback server，不啟動 Telegram、排程、LLM discovery、browser、updater 或其他背景 worker。此模式只供候選版本 UI 驗收，不取代正式 daemon。

## 安全更新 Sirin daemon

`scripts/switch-sirin-daemon.ps1` 不會直接啟動第二個正式 Sirin。它先在替代 loopback port 以 `--ai-monitor-only` 驗證候選檔、要求明確 SHA-256，並拒絕 MCP tool 數量倒退；正式切換前保存執行檔與完整排程 action，失敗會自動還原。

```powershell
.\scripts\switch-sirin-daemon.ps1 -Action Status
.\scripts\switch-sirin-daemon.ps1 -Action Verify -CandidateBinary <path> -ExpectedCandidateSha256 <sha256>
.\scripts\switch-sirin-daemon.ps1 -Action Deploy -CandidateBinary <path> -ExpectedCandidateSha256 <sha256>
```

排程原有參數與 working directory 會原樣保留；工具不會擅自移除其他 Sirin supervisor 功能。Windows PowerShell 5.1 的 repo 預設路徑會在參數完成綁定後解析，避免 `$PSScriptRoot` 在預設參數表達式中為空。
