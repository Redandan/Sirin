# Sirin

Sirin 是一個純 Rust 桌面 AI 代理。後端 Tokio 背景任務處理 Telegram 監聽、任務追蹤、LLM 自動回覆與調研 pipeline；前端使用 egui 原生 GUI，無 WebView、無 Node.js。

## 文件導覽

- [架構說明](docs/ARCHITECTURE.md)
- [快速啟動](docs/QUICKSTART.md)
- [開發路線圖](docs/ROADMAP.md)

## 技術組成

- **GUI**：egui / eframe（原生 Rust，無瀏覽器）
- **後端**：Rust + Tokio（非同步背景任務）
- **Telegram**：grammers-client（MTProto）
- **LLM**：Ollama 或 LM Studio（本機模型）
- **記憶**：JSONL 全文索引（`memory_store` / `memory_search`，零外部依賴）
- **對話 context**：per-peer JSONL ring-log

## 前置需求

- Rust toolchain（`rustup` 安裝）
- 本機 LLM 服務：Ollama 或 LM Studio

Windows 上需要 Microsoft C++ Build Tools。

## 啟動

```powershell
cargo run
```

Release 版本：

```powershell
cargo build --release
.\target\release\sirin.exe
```

## 環境變數

在專案根目錄建立 `.env`，沒有 `.env` 不會中止，但 Telegram 與 LLM 功能會停用。

### Telegram

```env
TG_API_ID=            # 從 https://my.telegram.org 取得
TG_API_HASH=
TG_PHONE=             # 國際格式，例如 +886...
TG_GROUP_IDS=         # 要監聽的群組 ID，逗號分隔（留空只回覆私訊）
TG_AUTO_REPLY=true
TG_REPLY_PRIVATE=true
TG_REPLY_GROUPS=false
TG_STARTUP_MSG=Sirin 已啟動 — {time}
TG_STARTUP_TARGET=    # 啟動通知發送的 username（留空發給自己）
TG_REQUIRE_LOGIN=1    # 設為 1 才啟用登入流程
```

### LLM

```env
LLM_PROVIDER=lmstudio     # ollama | lmstudio

# Ollama
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_MODEL=llama3.2

# LM Studio（OpenAI-compatible）
LM_STUDIO_BASE_URL=http://localhost:1234/v1
LM_STUDIO_MODEL=llama-3.2-3b-instruct-uncensored
# LM_STUDIO_API_KEY=optional
```

### 其他

```env
FOLLOWUP_INTERVAL_SECS=1800   # follow-up worker 週期（秒）
TASK_LOG_MAX_LINES=2000        # task.jsonl 上限行數
```

## 重要檔案

| 路徑 | 說明 |
|------|------|
| `src/main.rs` | 程式入口，啟動 Tokio runtime 與 egui 視窗 |
| `src/ui.rs` | egui App（四個 tab：任務板、調研、Telegram、對話）|
| `src/log_buffer.rs` | 全域 log 環形緩衝，供 GUI 底部 Log 面板讀取 |
| `src/telegram/` | Telegram listener、AI 回覆、語言修正 |
| `src/followup.rs` | LLM follow-up worker |
| `src/researcher.rs` | 多階段背景調研 pipeline |
| `src/llm.rs` | 共用 LLM 呼叫層（Ollama / LM Studio）|
| `src/memory.rs` | 全文記憶索引 + per-peer 對話 context |
| `src/persona.rs` | Persona 載入與 TaskTracker |
| `config/persona.yaml` | 人格設定 |
