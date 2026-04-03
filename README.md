# Sirin

Sirin 是一個以 Tauri 2 + Next.js 15 建成的桌面代理原型。前端提供 Live Task Board，後端以 Rust 負責背景工作、任務追蹤、Telegram 監聽與本機 LLM 回覆流程（Ollama / LM Studio）。

## 文件導覽

- [架構說明（Current Architecture）](docs/ARCHITECTURE.md)
- [快速啟動（QUICKSTART）](docs/QUICKSTART.md)

## 目前已確認的狀態

以下流程已在目前專案上驗證過：

- `npm install` 可以成功安裝前端依賴。
- `npm run build` 可以成功完成 Next.js 靜態匯出，輸出到 `dist/`。
- `npm run dev` 可以成功啟動開發伺服器，預設在 `http://localhost:3000`。

以下流程目前無法在這台機器上直接驗證：

- `cargo tauri dev`
- `cargo tauri build`

原因是目前環境中沒有可用的 `cargo` / `cargo tauri` 指令，因此桌面殼啟動仍需要先安裝 Rust toolchain 與 Tauri CLI。

## 技術組成

- 前端：Next.js 15、React 18、Tailwind CSS
- 桌面殼：Tauri 2
- 後端：Rust + Tokio
- 背景功能：Telegram listener、task tracking、LLM follow-up worker（Ollama / LM Studio）
- 回覆策略：AI 優先回覆、語言不一致自動重試（繁中）、最後才走中文保底
- 內嵌向量記憶：LanceDB（本地 `data/sirin_memory`，無需外部服務）

## 執行前置條件

### 必要

- Node.js 20+
- npm

### 如果要跑 Tauri 桌面版

除了 Node.js 之外，還需要：

- Rust toolchain（包含 `cargo`）
- Tauri CLI

Windows 上通常還需要：

- Microsoft C++ Build Tools
- WebView2 Runtime

Rust 可用 `rustup` 安裝，Tauri CLI 可用下列其中一種方式安裝：

```powershell
cargo install tauri-cli --version "^2"
```

或：

```powershell
npm install -D @tauri-apps/cli
```

如果你選擇用本地 npm 套件安裝 Tauri CLI，啟動命令請改用 `npx tauri dev` / `npx tauri build`。

## 安裝

在專案根目錄執行：

```powershell
npm install
```

## 啟動方式

### 1. 只驗證前端開發伺服器

```powershell
npm run dev
```

啟動後可開啟：

```text
http://localhost:3000
```

注意：這個專案的 Task Board 會直接呼叫 Tauri `invoke()`，所以純瀏覽器模式雖然能把 Next.js 頁面跑起來，但不代表完整功能可在瀏覽器內正常操作。真正的互動流程仍應以 Tauri 桌面模式為準。

### 2. 跑完整 Tauri 桌面版

如果系統已經有 `cargo` 與 Tauri CLI：

```powershell
cargo tauri dev
```

如果你是用 npm 安裝本地版 CLI：

```powershell
npx tauri dev
```

Tauri 會先啟動 Next.js 開發伺服器，再載入桌面視窗。

## 建置

### 前端靜態匯出

```powershell
npm run build
```

這會依照 `next.config.mjs` 的設定，把輸出寫到 `dist/`，供 Tauri 打包時使用。

### 打包桌面應用程式

```powershell
cargo tauri build
```

或：

```powershell
npx tauri build
```

## 環境變數

專案會在啟動時讀取根目錄下的 `.env`。沒有 `.env` 時不會直接中止，但某些背景功能會失效。

### Telegram 監聽需要

```env
TG_API_ID=
TG_API_HASH=
TG_PHONE=
TG_GROUP_IDS=
TG_AUTO_REPLY=true
TG_AUTO_REPLY_TEXT=收到你的訊息，我先幫你整理重點。
TG_REPLY_PRIVATE=true
TG_REPLY_GROUPS=true
TG_STARTUP_MSG=Sirin started at {time}
TG_STARTUP_TARGET=
TG_DEBUG_UPDATES=true
```

- `TG_API_ID`：Telegram App API ID
- `TG_API_HASH`：Telegram App API hash
- `TG_PHONE`：可選，使用者手機號碼（國際格式，例如 `+886...`）。若未設定，啟動時會在終端機提示輸入
- `TG_GROUP_IDS`：要監聽的群組 ID，使用逗號分隔
- `TG_AUTO_REPLY`：是否自動回覆收到的訊息（`true/false`）
- `TG_AUTO_REPLY_TEXT`：AI 回覆失敗時的模板保底內容。
	可用佔位符：`{persona}`、`{profit}`、`{voice}`、`{ack_prefix}`、`{compliance}`
- `TG_REPLY_PRIVATE`：是否回覆私聊訊息（預設 `true`）
- `TG_REPLY_GROUPS`：是否回覆群組/頻道訊息（預設 `true`）
- `TG_STARTUP_MSG`：啟動後發送一則自檢訊息；可使用 `{time}`。留空可關閉
- `TG_STARTUP_TARGET`：啟動訊息目標 username（不含 `@` 也可）
- `TG_DEBUG_UPDATES`：輸出 Telegram update 診斷 log（預設 `true`）

首次啟動若尚未授權，程式會走「使用者帳號登入」流程：送出驗證碼、輸入 code，若帳號有 2FA 會再要求密碼。成功後 session 會持久化，下次啟動通常不需重登。

### 自動回覆自然度策略

目前 Telegram 自動回覆採用三層機制：

1. AI 優先：先用本機 LLM 生成 1-3 句自然回覆（同語言、簡潔、人類口吻）
2. 語言修正：若使用者訊息含中文、但 AI 回覆非中文，會再發一次「強制繁中」請求
3. 最終保底：若仍失敗，才使用中文保底句型

Prompt 已內建限制，降低「像系統提示」的語氣：

- 非使用者詢問時，不主動自我介紹
- 非使用者詢問時，不主動提 ROI / profit
- 避免 policy/system 口吻

建議搭配 [config/persona.yaml](config/persona.yaml) 以繁中設定：

- `response_style.voice`
- `response_style.ack_prefix`
- `response_style.compliance_line`

如果你希望對話更像真人，而不是客服模板，優先調整 `voice` 與 `ack_prefix`。

### LLM follow-up worker 可選（支援 Ollama / LM Studio）

```env
# Provider: ollama | lmstudio
LLM_PROVIDER=ollama

# Ollama backend
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_MODEL=llama3.2

# LM Studio backend (OpenAI-compatible API)
LM_STUDIO_BASE_URL=http://localhost:1234/v1
LM_STUDIO_MODEL=local-model-name
# LM_STUDIO_API_KEY=optional

# Optional: follow-up worker interval override (seconds)
# FOLLOWUP_INTERVAL_SECS=30
```

- `LLM_PROVIDER` 預設為 `ollama`
- 當 `LLM_PROVIDER=ollama`：使用 `OLLAMA_BASE_URL` 與 `OLLAMA_MODEL`
- 當 `LLM_PROVIDER=lmstudio`：使用 `LM_STUDIO_BASE_URL` 與 `LM_STUDIO_MODEL`
- `LM_STUDIO_BASE_URL` 可直接設為 `http://localhost:1234/v1`

注意：`LLM_PROVIDER` 同時影響 Telegram 自動回覆與 follow-up worker 的模型後端。

如果本機模型服務未啟動，只有在 follow-up worker 真正需要呼叫模型時才會出現錯誤訊息。

## 重要檔案

- `src/main.rs`：Tauri app 入口，註冊 commands、system tray、background workers
- `src/telegram.rs`：Telegram listener
- `src/telegram.rs`：Telegram listener + AI 回覆生成 + 語言修正/保底機制
- `src/followup.rs`：LLM follow-up worker（Ollama / LM Studio）
- `src/memory.rs`：LanceDB 本地向量記憶（`add_to_memory` / `search_memory`）
- `src/persona.rs`：persona 載入與 task log 存取
- `components/task-board.tsx`：前端任務看板
- `config/persona.yaml`：persona 設定
- `data/tracking/task.jsonl`：任務追蹤紀錄

## 常見問題

### `cargo` 找不到

代表 Rust toolchain 尚未安裝，先安裝 `rustup`，再確認 `cargo --version` 可用。

### `npm run dev` 成功，但畫面互動失敗

這通常不是 Next.js 問題，而是因為頁面在瀏覽器中無法取得 Tauri IPC。請改用 `cargo tauri dev` 或 `npx tauri dev`。

### `npm run build` 成功，但桌面版仍跑不起來

前端 build 成功只代表靜態匯出沒問題，不代表 Windows 上的 Rust/Tauri 原生建置環境已經完整。