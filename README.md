# Sirin

Sirin 是一個以 Tauri 2 + Next.js 15 建成的桌面代理原型。前端提供 Live Task Board，後端以 Rust 負責背景工作、任務追蹤、Telegram 監聽與後續 Ollama 判斷流程。

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
- 背景功能：Telegram listener、task tracking、Ollama follow-up worker

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
TG_GROUP_IDS=
```

- `TG_API_ID`：Telegram App API ID
- `TG_API_HASH`：Telegram App API hash
- `TG_GROUP_IDS`：要監聽的群組 ID，使用逗號分隔

如果這些變數缺失，Telegram listener 會啟動失敗，但不一定會讓整個桌面應用直接退出。

### Ollama follow-up worker 可選

```env
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_MODEL=llama3.2
```

- `OLLAMA_BASE_URL` 預設為 `http://localhost:11434`
- `OLLAMA_MODEL` 預設為 `llama3.2`

如果沒有本機 Ollama，只有在 follow-up worker 真正需要呼叫模型時才會出現錯誤訊息。

## 重要檔案

- `src/main.rs`：Tauri app 入口，註冊 commands、system tray、background workers
- `src/telegram.rs`：Telegram listener
- `src/followup.rs`：Ollama follow-up worker
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