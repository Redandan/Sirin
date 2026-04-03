# Sirin QUICKSTART

這份文件提供最短路徑，讓你在本機快速把 Sirin 跑起來。

## 1. 前置需求

### 必要

- Node.js 20+
- npm

### 若要跑桌面版（建議）

- Rust toolchain（含 `cargo`）
- Tauri CLI（`cargo tauri` 或 `npx tauri`）
- Windows: Microsoft C++ Build Tools、WebView2 Runtime

## 2. 安裝

在專案根目錄執行：

```powershell
npm install
```

## 3. 最快驗證路徑

### 路徑 A：先確認前端可啟動

```powershell
npm run dev
```

開啟 `http://localhost:3000`。

注意：這只驗證 Next.js；涉及 `invoke()` 的桌面互動仍需 Tauri 模式。

### 路徑 B：跑完整桌面版（推薦）

若已有 `cargo tauri`：

```powershell
cargo tauri dev
```

若使用 npm 本地 CLI：

```powershell
npx tauri dev
```

## 4. 最小 `.env` 範例

若你要啟用 Telegram 與本機 LLM，請在專案根目錄建立 `.env`：

```env
TG_API_ID=
TG_API_HASH=
TG_PHONE=
TG_GROUP_IDS=
TG_AUTO_REPLY=true
TG_REPLY_PRIVATE=true
TG_REPLY_GROUPS=true

LLM_PROVIDER=ollama
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_MODEL=llama3.2
```

如果改用 LM Studio：

```env
LLM_PROVIDER=lmstudio
LM_STUDIO_BASE_URL=http://localhost:1234/v1
LM_STUDIO_MODEL=local-model-name
# LM_STUDIO_API_KEY=optional
```

## 5. 基本操作驗收

啟動後可快速驗證下列流程：

1. UI 任務板可顯示並定期更新。
2. Telegram 收到訊息後，`task.jsonl` 有新紀錄。
3. 在 Telegram 輸入「調研 <主題或URL>」，任務板可看到 research 任務。
4. 點選「快速核准」可將可行動任務更新為 `DONE`。

## 6. 產出建置

### 前端匯出

```powershell
npm run build
```

輸出到 `dist/`（由 `next.config.mjs` 設定）。

### 打包桌面應用

```powershell
cargo tauri build
```

或：

```powershell
npx tauri build
```

## 7. 常見問題（快速排查）

### `cargo` 或 `cargo tauri` 找不到

先確認 Rust toolchain 與 Tauri CLI 已安裝並可在 PATH 使用。

### 前端可開但按鈕/互動失效

這通常是瀏覽器模式沒有 Tauri IPC。請改用 `cargo tauri dev` 或 `npx tauri dev`。

### Telegram 無回覆

- 檢查 `.env` 是否完整（`TG_API_ID`、`TG_API_HASH`、`TG_GROUP_IDS`）
- 檢查首次登入驗證流程是否完成
- 檢查 `LLM_PROVIDER` 對應的本機模型服務是否真的在執行

## 8. 你下一步可能會做的事

- 調整 `config/persona.yaml` 的語氣與目標
- 將 `FOLLOWUP_INTERVAL_SECS` 調短做壓測
- 增加更多 `skills` 並在 `approve_task` 串接
