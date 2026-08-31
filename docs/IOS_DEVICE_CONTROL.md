# iPhone 實機控制收編計畫

## 目標

Windows 實體 iPhone 的偵測、資訊讀取、畫面擷取、控制、驗收、MCP 與
Codex 使用入口，最終全部由 Sirin 維護。Codex 與其他本地用戶端只依賴
Sirin 的 MCP 合約，不直接依賴 SideTap、WebDriverAgent 或 `go-ios`。

低階 Driver 原始碼由 SideTap 的 MIT 授權版本衍生，但現在由 Sirin 儲存庫
內的 `integrations/ios-driver` 維護。其 API、安裝位置、程序管理與錯誤細節
不得成為外部合約；安裝、升級、啟動與監控都由 Sirin 腳本與 daemon 處理。

## 能力證據

四個層級必須分開回報，不能用較低層級代替較高層級：

| 能力 | PASS 所需證據 |
|---|---|
| `DEVICE_DETECTED` | 當下取得裝置資訊，或 WDA 連線當下為 `up` |
| `INFO_READABLE` | 當下成功讀取手機狀態 |
| `SCREEN_CAPTURE` | 當下取得有效、非佔位圖的畫面並保存雜湊 |
| `SCREEN_CONTROL` | 同一動作前後取得新畫面，且畫面確實改變 |

若證據不足，一律回報 `MISSING_PROOF`。手機鎖定時回報
`NEEDS_HUMAN_UNLOCK`，由使用者親自在手機上解鎖。Sirin 不輸入密碼、
不按「信任」、不修復簽章、不越獄，也不處理登入、訊息、訂單或付款。

## 對外合約

Sirin MCP 提供以下工具：

- `ios_device_status`、`ios_screen_capture`
- `ios_control_session_start`、`ios_control_session_status`、
  `ios_control_session_stop`
- `ios_swipe`、`ios_home`、`ios_open_app`、`ios_open_route`、`ios_tap`
- `ios_acceptance_run`：以單次呼叫完成固定 route、前後證據與租約釋放
- `ios_recover_home`：只在 WDA 已確認為 `down`／`wedged` 時，透過 USB
  回到 SpringBoard 並請求重啟 WDA

所有控制動作必須先取得單一租約，並攜帶 `session_id` 與唯一
`action_id`。同一 `action_id` 重試只回放第一次結果，不會再次操作手機。
租約建立時會排定逾時清理；TTL 到期後不只清除記憶體租約，也會停止並驗證
Sirin 建立的暫時鏈路。釋放無法證明時會標記 `link_cleanup_required` 並阻擋新
租約；第二個用戶端在租約存續時會收到 `DEVICE_BUSY`。
`ios_acceptance_run` 由 Sirin 內部建立並在所有結果路徑嘗試釋放短租約與 Sirin
建立的暫時 forwards／WDA／tunnel 程序，呼叫端只提供冪等 `run_id`，不需要再做
多次 lease/status 輪詢；私有 Driver 服務本身保持被動常駐。`run_id` 在目前
Sirin process 生命週期內冪等，且綁定固定 route；重複 ID 搭配不同 route 會回報
`IDEMPOTENCY_CONFLICT`，Sirin 重啟後仍應把不明回覆視為 `MISSING_PROOF`，不可
假定跨重啟已保存結果。

唯一例外是使用者另行明確核准的 `ios_recover_home`。它不取得一般控制租約，
且只有在無有效租約、Sirin 私有 Driver 已確認 loopback／acceptance-only、
實機存在、WDA 已 `down`／`wedged` 時才可執行。此工具只有固定 Home 恢復，
不能點擊、輸入、開 App 或網址；WDA 健康、狀態不明或 Driver 正在啟動時
一律拒絕。Home 後會先重建本機轉送並等待既有 WDA Runner 恢復，避免直接
在卡住的 Runner 上重啟而觸發 XCTest 103；只有等待仍無回應才啟動新 Runner。
它仍需要 `action_id`，並保留前後畫面及冪等結果。

預設策略為 `acceptance_browse_only`：只允許既定 AgoraMarket Safari、
Telegram 與 Codex iPhone Remote PWA 入口、返回首頁與垂直捲動。
`codex_remote` 只會從 SpringBoard 精確尋找並開啟 `Remote Desktop` 圖示，
再以相同前景 App 的唯一 Web Inspector 頁面確認固定
`https://remote.purrtechllc.com/` origin。圖示不存在時回報人工 Add to Home
Screen 邊界，不會退回 Safari、任意座標或任意 URL。任意座標點擊只在
`supervised_control` 可用，而且必須
提供剛取得畫面的 SHA-256，以阻止過期座標點擊；此策略還需要 Sirin 主機端
明確開關，並受內部 Driver 的 acceptance-only 設定限制。

## 安全與部署邊界

- iOS Driver 只能綁定 loopback；Sirin 拒絕 LAN、HTTPS 代理 URL、URL
  帳密或 query string。
- Sirin 是唯一對外控制面；Driver 不註冊成全域 Codex MCP。
- 排程常駐使用 `--ios-driver-autostart` 啟用 Sirin 的 start-only 生命週期管理；
- 手動／開發啟動也可使用 `SIRIN_IOS_DRIVER_AUTOSTART=1`；
  預設關閉，且不包含停止、安裝、簽章、信任、解鎖或防火牆修改。
- `scripts/install-ios-driver-task.ps1` 建立無登入觸發器的私有
  `Sirin iOS Driver` 任務；只有 Sirin daemon 會要求它啟動。runtime 位於
  `%LOCALAPPDATA%\Sirin\ios-driver`，並使用固定雜湊驗證的官方 go-ios 套件；
  Appium 官方 WDA Runner ZIP 也以固定 release 與 SHA-256 驗證，原樣重包為
  `operator-assets` 下的未簽署 IPA，並附上游 BSD license；
  不依賴 SideTap checkout，Codex client 也不知道其路徑或排程名稱。
- 控制動作序列化，避免兩個工作階段同時操作同一台手機。
- 每個動作保留前後畫面、時間、雜湊與結果；Driver 回覆不明時禁止換新的
  `action_id` 盲目重試。
- 實機驗收只瀏覽；登入、授權、訊息、訂單、付款都不是驗收工具能力。

## 遷移階段

1. **P0：穩定合約** — Sirin DeviceManager、private provider、MCP 工具、
   租約、冪等與證據分級。
2. **P1：唯一入口** — 所有本地 Codex 工作階段只設定 Sirin MCP；安裝技能
   只說明 Sirin 合約，不再教用戶端直接呼叫 SideTap。
3. **P2：生命週期收編** — SideTap 安裝、啟停、健康檢查、WDA 更新與恢復
   由 Sirin 管理，保留 Apple 必須由人完成的解鎖／信任／簽章步驟。
4. **P3：單一發行物** — 將 Driver 打包為 Sirin 私有元件，完成長時間 soak
   後封存獨立 SideTap 專案與舊排程；歷史證據保留唯讀。

截至 2026-08-23，P0、P1 與 P2 已完成實機證據。P1 已由另一個全新的
Codex CLI session 在 `approval=never` 與唯讀 sandbox 下，透過共用 `sirin`
MCP 成功且只呼叫一次 `ios_device_status`；回報 provider=`sirin-ios-driver`、
`DEVICE_DETECTED=PASS`、`INFO_READABLE=PASS`、無有效租約。這次驗證沒有呼叫
擷取或控制工具。為支援標準 MCP client，Sirin 對 JSON-RPC notification 回覆
HTTP 202 空內容，並只將 `ios_device_status` 與 `ios_control_session_status`
標註為唯讀；截圖、租約變更及所有控制工具仍保留 client 核准門檻。

P3 已完成 Sirin 私有 source/runtime 與官方雜湊來源，舊 SideTap 排程已停用
但保留為回復路徑；重開機驗證與長時間 soak 完成前不刪除。

## 跨工作階段知識與權限邊界

正式 KB 條目是 `sirin/sirin-windows-iphone-single-control-plane`。其他本地
Codex 工作階段可透過共用的嚴格唯讀 `agora_kb_readonly` MCP 搜尋這個決策，
再依共用的 `iphone-usb-validation` Skill 呼叫 Sirin。2026-08-23 已由生產
`kbGet` 證實該條目為 `confirmed` v1。

生產 KB 的 OPS credential 只由遠端執行中的服務與唯讀橋接使用；不得複製到
Sirin `.env`、安裝包或 Driver。Sirin 是唯一的手機控制面，但不因此取得 KB
管理員權限。舊的 Sirin `kb_get`/`kb_stats` Bearer 直連目前不能作為生產 KB
可用性證據；KB 發布或讀取必須以受保護 connector 的實際結果為準。

## P0 驗收門檻

- Sirin 可在不操作手機的情況下分別回報偵測與資訊讀取。
- 當下截圖可驗證尺寸、檔案與 SHA-256。
- 一次安全垂直捲動必須在排除狀態列與瀏覽器工具列後，證明內容區有實質
  垂直位移，才能宣告 `SCREEN_CONTROL=PASS`。整張 PNG 的 SHA-256 不同本身
  不足以證明控制成功，因為時鐘與瀏覽器 chrome 也會自行變化。
- acceptance-only 的任意 tap 在碰到 Driver 前即被拒絕。
- 重複 `action_id` 不會再次執行動作。
- Rust 編譯與相關單元測試通過。

在接觸真機或替換常駐服務前，先用同一個只讀來源驗證入口產生 JSON 證據：

```powershell
.\scripts\verify-sirin-ios-source.ps1 -FullRust `
  -OutputPath "$env:TEMP\sirin-ios-source-verification.json"
```

這個入口只執行 PowerShell 語法、Python mock/unit、Rust unit 與編譯檢查；
不呼叫 `:7700`/`:8770`、不探測 USB 手機、不安裝依賴、不啟停服務，也不修改
防火牆、DDI 或手機設定。報告包含來源 SHA-256、實際指令、結果摘要，以及本節
各安全契約對應的測試名稱。Release CI 會執行相同入口並上傳報告。

P0 不代表所有 Codex 工作階段已經能呼叫新工具。只有新版本 Sirin 啟動、
全域 MCP 指向 Sirin，並由另一個用戶端完成 `tools/list` 與實際呼叫後，才可
宣告 P1 完成。

## 候選版本隔離驗證

工作樹有其他尚未發布的變更時，不得直接覆蓋常駐 release。先用不同連接埠
啟動候選版本的 MCP-only 模式：

```powershell
$env:SIRIN_RPC_PORT = "17700"
.\target\debug\sirin.exe --mcp-only
```

此模式只啟動 loopback RPC/MCP transport，不啟動 LLM 探測、Telegram、
背景 worker、運營排程、瀏覽器、更新檢查或 UI service。驗證端至少要完成：

1. `initialize` 回傳 Sirin 的 iPhone 安全 instructions。
2. `tools/list` 包含完整 iPhone 工具集合。
3. `ios_device_status` 取得當下唯讀狀態，並維持四層證據分離。

驗證程序結束後停止候選程序；不得因候選驗證而終止或取代 7700 的現行
Sirin。正式切換仍需備份舊 binary、健康檢查及自動回復路徑。

`switch-sirin-daemon.ps1 -Action Verify` 會把候選的
`SIRIN_IOS_DRIVER_URL` 強制指向未使用的 loopback 測試埠（預設 `18770`），
並關閉候選的 Driver autostart。候選只有在必要 iPhone 工具完整，且 Driver
不可達時仍回報 `STALE_OR_UNPROVEN`、四層 `MISSING_PROOF`、無 lease、無 cleanup
blocker、無 supervisor 啟動，才會回傳 `VERIFIED_NOT_DEPLOYED`。這個 smoke 不會
呼叫目前 `8770` 的 Driver，也不會探測或操作手機。若候選 MCP 工具數少於
目前 7700 常駐版本，驗證仍保留上述 iPhone fail-closed 證據，但狀態改為
`BLOCKED_TOOL_REGRESSION`、`candidate_deployable=false`；`Deploy` 會拒絕替換。

安全來源驗證完成後，真機驗收使用獨立的可重現腳本。預設 `Status` 只啟動
alternate-port passive Driver/MCP 並呼叫 `ios_device_status`；不接觸目前
7700/8770，也不建立手機鏈路：

```powershell
.\scripts\verify-sirin-ios-physical.ps1 -Action Status `
  -CandidateBinary .\target\debug\sirin.exe
```

只有人員已接上、喚醒並解鎖測試機，而且被動證據通過後，才可明確開啟一次
固定路線動作。`RunId` 必須由呼叫者保存；回覆不明時沿用同一值：

```powershell
.\scripts\verify-sirin-ios-physical.ps1 -Action Acceptance `
  -CandidateBinary .\target\debug\sirin.exe `
  -Route codex_remote `
  -RunId operator-reviewed-20260827-1 `
  -AllowPhoneAction
```

腳本只經 Sirin MCP 呼叫手機工具，完成後驗證租約、link cleanup、PID records
與 17700/18770/8100/9100 listeners；若無法證明清理，不刪除暫時 runtime
ownership evidence，並以非零狀態結束。

正式候選通過完整測試與 SHA-256 審查後，使用 Sirin 自己的切換腳本；部署
前會再次執行隔離 MCP smoke，失敗時自動恢復舊 binary：

```powershell
.\scripts\switch-sirin-daemon.ps1 -Action Deploy `
  -CandidateBinary <reviewed-sirin.exe> `
  -ExpectedCandidateSha256 <sha256>
```

若排程透過 Windows Junction 指向同一份 Sirin binary，切換器以磁碟序號與
檔案 ID 驗證實體檔案身分，不以路徑字串或相同雜湊冒充所有權；不同實體檔案
仍會 fail closed。

`Rollback` 必須指定確切的備份檔，不會猜測「最新」版本。

Sirin 內的 Codex Skill 是唯一來源，安裝到共用 Codex 設定：

```powershell
.\scripts\install-codex-ios-integration.ps1 -Action Install
```

安裝後需重啟 ChatGPT/Codex 用戶端，新的本地工作階段才會重新讀取共用 MCP
與 Skill。可用 `-Action Status` 比對來源／安裝副本的 SHA-256 及 Sirin MCP
URL；全域副本不是另一個需要獨立維護的專案。

## 單一安裝與維護入口

Windows installer 的「實體 iPhone USB 驗收能力」工作會一起安裝 Sirin
binary、私有 Driver 原始碼、固定依賴、Codex Skill 與內部安裝腳本，並自動
呼叫唯一公開入口：

```powershell
.\scripts\install-sirin-ios.ps1 -Action Install -RunNow -MigrateLegacySideTap
.\scripts\install-sirin-ios.ps1 -Action Status
```

`Install` 與 `Upgrade` 使用相同的冪等流程。它會自動尋找 Python 3.10+、建立
`%LOCALAPPDATA%\Sirin\ios-driver` 私有 runtime、安裝兩個 Sirin 排程、同步
共用 Codex MCP/Skill，並只對外暴露 `http://127.0.0.1:7700/mcp`。安裝前會
備份 Sirin 與 legacy SideTap 排程定義，失敗時自動還原；不複製憑證、不處理
簽章、安裝到手機、信任、解鎖或防火牆。它只會準備可稽核的未簽署 WDA
operator asset；Apple ID、Sideloadly Start 與手機信任仍由使用者親自完成。

Driver 排程固定使用 `attach_mode=passive`。手機插著時，背景 supervisor 只做
便宜的程序存活檢查；`ios_device_status` 只觀察一次 USB／既有鏈路，不會因此
啟動 tunnel、掛載 DDI、啟動 WDA 或建立 8100/9100 轉送。驗收也不會自動掛載
DDI：缺少時固定回報 `NEEDS_HUMAN_DDI`，避免 iOS 更新後首次掛載在背景連線
Apple TSS 或改變手機 Developer Image 狀態。固定路線驗收優先用
單一 `ios_acceptance_run`：它在明確操作時才按需啟動鏈路，保存前後證據，並在
成功或失敗後釋放租約與 Sirin 建立的暫時鏈路程序。

`-RunNow` 是明確的維護操作，因此會透過 Sirin 公開 MCP 建立一個短暫驗收租約，
按需證明鏈路後立即釋放並驗證暫時程序已停止；一般排程啟動不做這個明確驗證。
安裝檔、
排程、程序來源或 Codex 整合等 control-plane gate 未通過時才回復備份；若控制面
已正確安裝，但手機鎖定、未連線或 WDA 尚不可用，則保留已安裝的 Sirin 狀態並
回報對應的 `next_safe_action`，不會因被動鏈路為 down 就把 control plane 判成
故障。Driver 已停止啟動且出現明確 `NEEDS_HUMAN_*` 或
`SECURITY_BLOCK_*` 時會提前結束等待。外部手機狀態不會再讓安裝器復原成舊
SideTap 控制面。

安裝包只把 `.env.example`、`agents.yaml` 與 `persona.yaml` 放在 Sirin 的
`defaults` 目錄；安裝後由同一入口以原始使用者身分，只在使用者設定缺少時
複製到 `%LOCALAPPDATA%\Sirin`。升級不覆蓋既有設定，系統管理員安裝階段也
不直接寫入 HKCU 或使用者目錄。子程序非零結束碼會使安裝器明確報錯，而不是
顯示假成功。

portable zip 也包含相同入口與 integrations，不需要 SideTap checkout。細部的
`install-ios-driver-*` 與 `install-codex-ios-integration.ps1` 是內部元件，不是
另一套使用者操作流程。Release workflow 會在乾淨的 Windows runner 從
`integrations/ios-driver/requirements-dev.txt` 安裝測試相依，執行全部 Driver
測試後才建置發行物；發行驗證不依賴 `D:\SideTap` 或其虛擬環境。

`-Action Status` 的 `migration` 區塊會另外回報現行依賴是否仍指向 SideTap、
Sirin 畫面擷取／操作能力證據、合格八小時 soak 證據與舊任務退休門檻；
這些遷移證據不會被混入目前服務的 `READY` 判定。`READY` 代表 Sirin binary、
排程、MCP 工具、目前版本的 loopback acceptance-only Driver、`attach_mode=passive`
與 LAN 啟動防護都可用；它刻意不要求平時維持 WDA link=`up`。當下
`DEVICE_DETECTED`、`INFO_READABLE`、WDA/input 另由 `live_ready` 回報；
`SCREEN_CAPTURE` 與 `SCREEN_CONTROL` 仍是另外的能力證據，不能由 readiness
推定。Provider 另回報有限狀態機值 `last_link_result`；它用來區分
`NEEDS_HUMAN_WAKE_UNLOCK` 等明確人工邊界與 `WDA_START_TIMEOUT` 等維護故障，
不包含原始工具輸出或裝置識別碼，也不能單獨證明較高層能力。重開機恢復有實際重啟
證據前固定回報 `MISSING_PROOF`。`next_safe_action` 只會按照 soak、耐久排程、
readiness、能力證據、使用者核准重啟與 legacy 退休的固定順序前進；前一關
未通過時不建議後續破壞性較高的動作。

LAN 安全狀態不是常數：Driver 會區分沒有 listener、僅 loopback、已有防火牆
阻擋的 LAN listener、未阻擋的 LAN listener，以及無法判定。由於固定版 go-ios
轉送沒有 bind-address 選項，冷啟動前必須先證明既有 inbound block 已啟用；
否則回報 `SECURITY_BLOCK_LAN_ACTIVATION_UNPROTECTED`，並在 tunnel、DDI、WDA
或 forwards 之前停止。Sirin 不會自行新增／修改防火牆規則，也不會修改手機
信任、解鎖、簽章或網路設定。

若完整已安裝 App 清單不再包含 WDA runner，Driver 回報
`NEEDS_HUMAN_WDA_INSTALL`。Sirin 不會把保存的舊 bundle 名稱當成仍已安裝的證據，
也不會自行簽章或重新安裝。`Status.runtime.operator_handoff` 會提供 Sirin 自己
管理且雜湊已驗證的未簽署 IPA 路徑；使用者以該檔走 Apple/Sideloadly 流程後
再驗證，不再需要 `D:\SideTap\wda`。

完成一次由 Sirin MCP 管理的安全垂直捲動並釋放 lease 後，使用同一入口記錄
最近十五分鐘內的證據：

```powershell
.\scripts\install-sirin-ios.ps1 -Action RecordCapabilityProof
```

這個動作不會再操作手機；它只接受 provider=`sirin-ios-driver`、
acceptance-only、loopback、無有效 lease、有效截圖檔與雜湊，以及排除狀態列
與瀏覽器工具列後，內容區確有實質垂直位移的捲動。僅有整張截圖雜湊不同不
合格。證據綁定目前 Sirin binary 與 Driver source 雜湊；升級
任一元件後必須重新驗證，不能沿用舊證據。

長時間 soak 完成後，先建立不會自行重啟電腦的檢查點；只有使用者另行核准並
完成實際重啟後，才執行驗證：

合格 soak 不只看牆鐘時間或 summary 的 `PASS`。`Status` 會重新解析原始 JSONL，
要求每筆為 PASS、序號連續、實際觀測跨度至少八小時、樣本數至少符合設定
interval 的 1.5 倍容忍值，且任兩筆最大間隔不得超過 interval 三倍或 180 秒
（取較大者）。Windows 睡眠或長時間停止取樣不能冒充連續 soak。

```powershell
.\scripts\install-sirin-ios.ps1 -Action PrepareRebootProof
# 使用者明確核准後，才由使用者或另行授權的維護流程重啟 Windows
.\scripts\install-sirin-ios.ps1 -Action VerifyRebootProof
```

`VerifyRebootProof` 只讀取開機時間、排程執行時間、Sirin readiness 與
`ios_device_status`；不擷取或操作手機。只有 Sirin 擷取／操作能力、合格八小時
soak 與同一 binary 的重啟恢復證據都為 `PASS`，`RetireLegacy` 才能移除兩個
已停用的 SideTap 排程：

```powershell
.\scripts\install-sirin-ios.ps1 -Action RetireLegacy
```

這個動作只移除舊排程並保留備份，不刪除 `D:\SideTap` 或歷史證據。

目前 unattended 契約是 `logged_in_user_session`：使用者可離開，Sirin、Driver
與 Codex 在已登入且鎖定的 Windows 工作階段持續運行。Windows 冷啟動後仍需
使用者登入一次；不設定自動登入，也不索取或保存 Windows 密碼。S4U 雖不保存
密碼，但沒有網路與加密檔案存取，不能承載完整 Sirin/Codex 能力。因此重啟證據
明確記錄 `cold_boot_without_user_logon_proven=false`，不能把登入後恢復宣稱成
無登入服務能力。

兩個 Sirin 排程必須使用無執行時間上限 (`PT0S`)、一分鐘重啟間隔、非零重啟
次數及 `StartWhenAvailable=true`。Windows 預設的 72 小時上限或零重啟次數
都不符合 unattended readiness；`-Action Status` 會分別以
`daemon_task_durable`、`driver_task_durable` fail closed。
Daemon 任務同時有登入觸發器與五分鐘安全喚醒；其動作直接執行 `sirin.exe`，
搭配 `MultipleInstances=IgnoreNew`，不會啟動第二個實例或顯示 PowerShell 視窗。
Driver 任務沒有任何觸發器，只能由 Sirin 的 start-only supervisor 啟動。

安裝或升級會先備份兩個 Sirin 任務與 legacy 任務的 XML／運行狀態，再進入短暫
維護切換：停止目前 Sirin/Driver 任務、更新 runtime 與定義、啟動新 daemon。
失敗時除回復 XML 外，也會重新啟動原本正在運行的任務。`Status` 同時比對
7700 的實際 process binary 與 8770 的實際 Driver source/runtime；只改排程文字
而舊程序仍佔用 port 時不能得到 `READY`。

`-MigrateLegacySideTap` 也會盤點執行檔實際位於 legacy task 工作目錄下的存活
程序；即使舊任務已停用，遺留的 tunnel／syslog 仍算 active dependency，
`Status` 必須 fail closed。遷移只終止經執行檔路徑再次確認的 legacy 程序，再
由 Sirin 私有 runtime 建立 tunnel。WDA bundle 選擇第一次匯入 Sirin state 後
永久優先保留，後續升級不再重讀或覆寫 SideTap `.env`。

升級同時會回收執行檔精確等於 Sirin runtime `ios.exe` 的既有 detached stack，
避免舊 forward 讓新 Driver 把已斷線的 WDA 誤判為 wedged；回收後由同一 runtime
重建 tunnel、WDA 與 forwards。其他路徑啟動的 tunnel／runwda／forward／syslog
不會被自動終止，但會列為 foreign control process 並使 readiness fail closed。
