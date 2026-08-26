# AI Work Monitor

Windows 本機 AI 工作監控，整合在 Sirin 既有 daemon、tray、`/ui/`，並提供 Windows 11 Widgets Board 原生小工具。

## 開啟方式

- Sirin UI 側欄：`AI 工作監控`
- tray：`Open AI Work Monitor`
- URL：`http://127.0.0.1:7700/ui/?view=ai-monitor`
- Windows 小工具：安裝後按 `Win + W`，在小工具選擇器搜尋 `Sirin AI 工作監控`

直接開啟監控頁時，不啟動原本每 2 秒的全域 snapshot/WebSocket；切換到其他 Sirin 頁面後才會啟動。Sirin 每 60 秒唯讀採樣一次 Codex token 與程序資訊，保留最近 60 點；網路路由與 ping 仍只在 `/api/ai-monitor` 被讀取時執行。監控頁每 15 秒讀取快取，Windows 小工具在可見/啟用期間最多每 60 秒讀取一次。

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

`MISSING_PROOF` 不會被轉寫成離線、故障或零用量。

## 安全邊界

- Codex 查詢只讀取工作 ID、`tokens_used`、更新時間；不查詢或顯示標題、prompt、訊息內容。
- 不讀或顯示 API key、登入憑證、程序 command line。
- 不改路由、不終止 task/process、不部署、不設定開機自啟。
- 不送 telemetry；最近一小時的採樣只保存在 Sirin 記憶體，重啟後清空。
- 下載測速是 `ai_monitor_speed_test` MCP action，只有按下按鈕才會向 Cloudflare endpoint 下載 5 MB；不會背景執行。

## Windows 11 小工具

`windows-widget/` 是 Windows App SDK C++/WinRT 的 packaged Widget Provider。卡片使用 Adaptive Cards，僅連到 `http://127.0.0.1:7700/api/ai-monitor`，不經系統 proxy、不啟動測速，也不包含工作標題或訊息內容。Token 圖固定為最近 60 分鐘、每格 2 分鐘；睡眠、重啟或取樣中斷會保留空格，不把不等長區間畫成等距連續資料。

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
