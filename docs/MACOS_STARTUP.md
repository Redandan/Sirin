# Sirin — macOS 開發者啟動指南

> 此文件針對首次在 **macOS** 上建置 Sirin 的開發者。  
> 原始 README.md 以 Windows 為主，請以本文件為準。

---

## 前置需求

### 1. Xcode Command Line Tools

Rust 編譯器需要系統 C 工具鏈（clang、libc）：

```bash
xcode-select --install
```

安裝完成後驗證：

```bash
clang --version
# Apple clang version 15.x.x ...
```

### 2. Rust Toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# 選擇 default installation (1)

# 重新載入 shell 環境
source "$HOME/.cargo/env"

# 確認版本
rustc --version
cargo --version
```

### 3. 本機 LLM 服務（二擇一）

**Ollama（推薦 Mac 開發者）**

```bash
# 官網 https://ollama.com 下載 .dmg 安裝，或用 Homebrew：
brew install ollama

# 啟動服務（背景執行）
ollama serve &

# 下載模型（選一個即可）
ollama pull llama3.2
```

**LM Studio**

至 [lmstudio.ai](https://lmstudio.ai) 下載 macOS 版本，啟動後開啟 Local Server（預設 `http://localhost:1234`）。

---

## 建置與執行

### 首次建置

```bash
git clone <repo-url>
cd Sirin

# 初次編譯需要 3–10 分鐘（下載並編譯依賴）
cargo build
```

> **常見編譯問題**：若出現 `openssl` 相關錯誤，安裝：
> ```bash
> brew install openssl
> export OPENSSL_DIR=$(brew --prefix openssl)
> ```

### 開發模式啟動

```bash
cargo run
```

### Release 版本

```bash
cargo build --release
./target/release/sirin
```

---

## 設定 `.env`

在專案根目錄建立 `.env`：

```env
# ── LLM（Ollama 範例）──────────────────────────────
LLM_PROVIDER=ollama
OLLAMA_BASE_URL=http://localhost:11434
OLLAMA_MODEL=llama3.2

# ── LLM（LM Studio 範例）──────────────────────────
# LLM_PROVIDER=lmstudio
# LM_STUDIO_BASE_URL=http://localhost:1234/v1
# LM_STUDIO_MODEL=llama3.2

# ── Claude（會議室用，選填）────────────────────────
# ANTHROPIC_API_KEY=sk-ant-...

# ── Telegram（選填）────────────────────────────────
# TG_API_ID=
# TG_API_HASH=
# TG_PHONE=+886...
# TG_AUTO_REPLY=true
# TG_REPLY_PRIVATE=true
```

沒有 `.env` 也能啟動，Telegram 和自動回覆功能會停用。

---

## Mac 專屬設定

### Telegram Session 路徑

`config/agents.yaml` 預設使用 Windows 的 `${LOCALAPPDATA}`，在 Mac 上需要改為本機路徑。
開啟 `config/agents.yaml`，將 `session_file` 改為：

```yaml
# 原本（Windows）：
session_file: ${LOCALAPPDATA}/sirin/assistant_1.session

# 改為（Mac）：
session_file: /Users/<你的帳號>/Library/Application Support/sirin/assistant_1.session
# 或使用相對路徑：
session_file: data/sessions/assistant_1.session
```

Session 目錄不存在時 Sirin 會自動建立。

### 中文字體

`setup_fonts` 目前只查找 Windows 字體路徑，在 Mac 上中文會顯示為方框。
短期 workaround：在 `src/ui.rs` 的 `setup_fonts` 函式加入 Mac 路徑：

```rust
// 在 let font_path = ... 之前加入 macOS 路徑
#[cfg(target_os = "macos")]
let font_path = std::path::Path::new("/System/Library/Fonts/STHeiti Medium.ttc");
#[cfg(target_os = "macos")]
let fallback  = std::path::Path::new("/Library/Fonts/Arial Unicode MS.ttf");

#[cfg(not(target_os = "macos"))]
let font_path = std::path::Path::new("C:/Windows/Fonts/msjh.ttc");
#[cfg(not(target_os = "macos"))]
let fallback  = std::path::Path::new("C:/Windows/Fonts/msyh.ttc");
```

> macOS 內建的中文字體路徑（任一）：
> - `/System/Library/Fonts/STHeiti Medium.ttc`  
> - `/System/Library/Fonts/PingFang.ttc`（macOS 10.11+）

---

## 資料儲存路徑（macOS）

| 內容 | 路徑 |
|------|------|
| 記憶 SQLite | `~/Library/Application Support/Sirin/memory.db` |
| 任務追蹤 | `~/Library/Application Support/Sirin/tracking/task.jsonl` |
| 調研記錄 | `~/Library/Application Support/Sirin/tracking/research.jsonl` |
| 程式碼 Call Graph | `~/Library/Application Support/Sirin/call_graph.jsonl` |
| Teams 草稿 | `data/pending_replies/teams.jsonl` |

---

## 首次啟動驗收

| 項目 | 正常狀態 |
|------|----------|
| GUI 開啟 | 視窗正常顯示，tab 可切換 |
| 中文字體 | 介面中文不亂碼（需套用 font patch） |
| AI 對話 | 切到「💬 對話」tab，送訊息後收到 LLM 回覆 |
| Log 面板 | 底部顯示彩色 log，`[llm]` 行顯示 backend 與 model |
| MCP 服務 | `curl http://127.0.0.1:7700/mcp` 回應 JSON |

---

## 常見問題

**`cargo build` 報 `linker 'cc' not found`**

```bash
xcode-select --install
```

**`openssl` 或 `pkg-config` 找不到**

```bash
brew install openssl pkg-config
export PKG_CONFIG_PATH="$(brew --prefix openssl)/lib/pkgconfig"
```

**對話 Tab 沒有 LLM 回覆**

1. 確認 LLM 服務正在執行：
   ```bash
   # Ollama
   curl http://localhost:11434/api/tags
   # LM Studio
   curl http://localhost:1234/v1/models
   ```
2. 檢查 `.env` 的 `LLM_PROVIDER` 與 `*_MODEL` 是否填寫
3. 查看 Sirin 底部 Log 面板的 `[llm]` 錯誤行

**Teams 功能**

Teams 整合使用 headless Chrome，Mac 上需安裝 Google Chrome：

```bash
brew install --cask google-chrome
```

Chrome 路徑在 Mac 為 `/Applications/Google Chrome.app`，`headless_chrome` crate 會自動偵測。

**Telegram 登入後 session 消失**

確認 `config/agents.yaml` 的 `session_file` 路徑存在且有寫入權限。

---

## 功能對照（Windows vs macOS）

| 功能 | Windows | macOS |
|------|---------|-------|
| 編譯建置 | MSVC | Xcode CLT + clang |
| 資料路徑 | `%LOCALAPPDATA%\Sirin` | `~/Library/Application Support/Sirin` |
| 中文字體 | 自動偵測 msjh.ttc | 需手動 patch `setup_fonts` |
| Teams 瀏覽器 | `C:\Program Files\Google\Chrome` | `/Applications/Google Chrome.app` |
| LLM 推薦 | Ollama / LM Studio | Ollama（有原生 .dmg） |
| 記憶體效能 | x86-64 | Apple Silicon (arm64) 原生支援 |
