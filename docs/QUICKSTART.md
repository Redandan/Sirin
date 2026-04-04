# Sirin QUICKSTART

## 1. 前置需求

| 必要 | 說明 |
|------|------|
| Rust toolchain | `rustup` 安裝，確認 `cargo --version` 可用 |
| Microsoft C++ Build Tools | Windows 編譯 Rust 必要 |
| 本機 LLM 服務 | Ollama 或 LM Studio（任一即可）|

Node.js / npm 不再需要。

## 2. 設定 `.env`

在專案根目錄建立 `.env`（可複製下方範例）：

```env
# Telegram
TG_API_ID=your_api_id
TG_API_HASH=your_api_hash
TG_PHONE=+886...
TG_AUTO_REPLY=true
TG_REPLY_PRIVATE=true
TG_REQUIRE_LOGIN=1

# LM Studio（推薦）
LLM_PROVIDER=lmstudio
LM_STUDIO_BASE_URL=http://localhost:1234/v1
LM_STUDIO_MODEL=llama-3.2-3b-instruct-uncensored

# 或改用 Ollama
# LLM_PROVIDER=ollama
# OLLAMA_BASE_URL=http://localhost:11434
# OLLAMA_MODEL=llama3.2
```

沒有 `.env` 也可以啟動，但 Telegram 與 LLM 功能會停用。

## 3. 啟動

```powershell
cargo run
```

首次執行會下載並編譯依賴，需要幾分鐘。之後的啟動很快。

### Release 版本

```powershell
cargo build --release
.\target\release\sirin.exe
```

## 4. 首次 Telegram 登入

1. 啟動後切換到 **Telegram** tab
2. 狀態顯示「需要驗證碼」時，輸入手機收到的 code
3. 若帳號有 2FA，再輸入密碼
4. 成功後 session 持久化，下次啟動自動連線

## 5. 基本驗收

| 功能 | 驗證方式 |
|------|----------|
| GUI 正常顯示 | 啟動後四個 tab 可切換，中文不亂碼 |
| 本地 AI 對話 | 切換到「💬 對話」tab，輸入訊息按 Enter 送出，收到 AI 回覆 |
| Log 面板 | 底部 Log 區域顯示彩色日誌，可拖拉調整高度 |
| Telegram 連線 | Telegram tab 顯示「已連線」|
| 任務記錄 | 傳訊息給自己，任務板出現新紀錄 |
| 調研 | 發「調研 Rust async runtime」，調研 tab 出現進行中任務 |
| LLM 回覆 | 私訊帳號，收到 AI 自動回覆 |

## 6. GUI 操作說明

### 對話 Tab（💬 對話）
- **Enter**：送出訊息
- **Shift+Enter**：換行
- 對話歷史會作為 context 注入下一次回覆（最近 5 輪）

### Log 面板
- 底部面板顯示即時日誌，顏色分類：
  - 🔵 藍色：Telegram 相關
  - 🟢 綠色：調研 pipeline
  - 🟡 黃色：Follow-up worker
  - 🔴 紅色：錯誤訊息
- 右上角「📋 隱藏/顯示 Log」可切換
- 面板可拖拉調整高度（80~300px）

## 7. 調整人格

編輯 `config/persona.yaml`：

```yaml
identity:
  name: Sirin
response_style:
  voice: 自然、簡潔、像真人朋友
  ack_prefix: 收到
  compliance_line: 我會按照你說的做
```

改完後重啟即生效（每次回覆時重新載入）。

## 8. 常見問題

**中文顯示為方框**
egui 字型載入失敗。確認 `C:/Windows/Fonts/msjh.ttc`（微軟正黑體）或 `msyh.ttc`（微軟雅黑）存在。

**對話 Tab 沒有回覆**
- 確認 `.env` 的 LLM 設定正確
- 確認 LLM 服務正在執行（LM Studio server 已啟動，或 Ollama 已啟動）
- 查看底部 Log 面板是否有錯誤訊息

**Telegram 沒有自動回覆**
- 確認 `.env` 的 `TG_AUTO_REPLY=true` 與 `TG_REPLY_PRIVATE=true`
- 確認 LLM 服務正在執行
- 查看 Log 面板是否有 `[telegram]` 錯誤訊息

**`cargo build` 失敗**
確認已安裝 Microsoft C++ Build Tools，並且 Rust toolchain 是最新版（`rustup update`）。
