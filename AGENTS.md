# Sirin — Codex guidelines

Lean session-bootstrap.  Every section here is the **rule** — every link
points at the **detail**.  Read the linked file when you need to actually
work in that area, not before.

## 🚫 Architecture decisions (DO NOT revisit)

- **GUI**: plain HTML/CSS/JS web app served by `mcp_server` at
  `http://127.0.0.1:7700/ui/`.  Single-file Alpine.js, bundled via
  `include_bytes!` so single-binary distribution still holds.
  **Migrated from egui in v0.5.0 (2026-05-02)** — see
  [`web/DESIGN.md`](web/DESIGN.md) for the full why.
- **Theme**: 極簡硬核風(加密終端/系統監控器風格)— 不要用 Catppuccin
- **Backend access**: ALL UI goes through `AppService` trait via
  `GET /api/snapshot` (read) + `POST /mcp` (write).  Global
  `OnceLock<Arc<dyn AppService>>` in `mcp_server.rs`, set by `main.rs`.
- **Daemon-style**: Closing the browser tab does NOT kill sirin.
  Re-open `http://127.0.0.1:7700/ui/` any time.  Kill with Ctrl-C in
  the terminal, `taskkill /F /IM sirin.exe`, or the system tray icon.
- **No WASM, no Tauri, no Electron**: just a real browser tab.
- **Persona**: Use `Persona::cached()` not `Persona::load()` in hot paths
- **Mutex**: Always `unwrap_or_else(|e| e.into_inner())`, never `.unwrap()`

## 📂 Project layout

[`docs/PROJECT_LAYOUT.md`](docs/PROJECT_LAYOUT.md) — module-level map.
Read it when you need to find which file owns a concept.  High-level
architecture rationale lives in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## 🎨 UI / UX 規範 (嚴格遵守)

[`docs/UI_TOKENS.md`](docs/UI_TOKENS.md) — colour palette, font hierarchy,
component rules, AI new-page workflow.  Source-of-truth for inspiration is
[`web/DESIGN.md`](web/DESIGN.md).  **Read those before building any new view.**

## ⚡ Efficiency rules

- **Batch edits.** Make all related edits before cargo check.
- **One cargo check per iteration.** Not speculatively.
- **Parallel reads.** Multiple files in one message.
- **Never re-explore.** Don't analyze code twice in same session.
- **No architecture detours.** STOP and ask before switching frameworks.

## 🛠️ Cargo / build rules (CRITICAL — prevent 2h deadlocks)

- **NEVER** run `cargo` with `run_in_background=true` — Cargo uses an
  exclusive file lock on `target/`; background tasks queue forever.
- **ALWAYS** set `timeout: 600000` (10 min) on any `cargo test` Bash call.
- **One cargo at a time.** Never launch a second `cargo` while one is running.
- **cargo check once.** Capture output, grep twice — never run check twice.
- **Pipe-to-tail trap (2026-05-06).** `cargo test ... 2>&1 | tail -50`
  with the *no-filter* full suite triggers the Bash tool's auto-background
  AND silently drops output (0 bytes forever, no cargo in tasklist) —
  the wrapper shell + pipe interaction loses stdout in bg mode.
  **Workaround:** redirect to a file, then read it.
  ```bash
  cargo test --bin sirin --message-format=short --no-fail-fast > /tmp/sirin_test.log 2>&1
  tail -20 /tmp/sirin_test.log
  ```
  Confirmed `cargo test` no-filter finishes 881 passed / 28 ignored in
  ~6 s when redirected; same command via `| tail` produces 0-byte zombie.
- **Deadlock signal:** if a cargo output file stays at 0 bytes for >30s
  AND no cargo/rustc process appears in tasklist, you're hitting the
  pipe-to-tail trap — kill the task and switch to file redirect.

```bash
cargo check                                        # 0 errors
cargo test --bin sirin                             # 632+ passed, 17 ignored
cargo build --release                              # release binary
./target/release/sirin.exe --headless              # no-GUI mode (server / CI / SSH)
./target/release/sirin.exe doctor                  # 6-section preflight (#261)
./target/release/sirin.exe doctor --format=json    # CI-friendly
```

Headless mode skips `eframe::run_native()` only — RPC/MCP server, browser
singleton, telegram listeners, test_runner all start normally.

Windows installer + release CI:

```bash
& 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe' /DMyAppVersion=X.Y.Z sirin.iss
git tag vX.Y.Z && git push origin vX.Y.Z   # → GitHub Actions builds + publishes
```

## 📝 YAML 測試 — IDE 自動補全 / Schema 驗證 (Issue #255)

每個 `config/tests/*.yaml` 第一行加:

```yaml
# yaml-language-server: $schema=../test-schema.json
```

VS Code 裝 [`redhat.vscode-yaml`](https://marketplace.visualstudio.com/items?itemName=redhat.vscode-yaml) 後就有
auto-complete + enum 限定 + 拼錯欄位即時紅線。

**Schema 怎麼更新**:
```bash
# 改完 src/test_runner/parser.rs 的 #[derive(JsonSchema)] struct 後
cargo run --bin gen-schema
# 把更新後的 config/test-schema.json 一起 commit
```

`cargo test schema_has_no_drift` 在 CI / 本機自動擋住忘記重生.
`scaffold_test` MCP tool 產生的新 YAML 自動帶 schema header.

## 🧪 Test runner — replay health metrics (Issue #266)

Saved-script replay 是主要的 cost-saving 機制 — script PASS = 0 LLM 成本。
但 UI 改版會讓 saved_scripts 過時,悄悄退回 LLM ReAct(成本+延遲都漲)。

| 來源 | 指標 |
|------|------|
| Dashboard KPI「REPLAY RATE」 | 7-day:passed_via_replay / total_runs;< prior 7d -10pp 變黃 |
| Coverage chips | ✅ healthy / ⚠️ stale_fallback / 🤖 llm_only / ○ untested |
| MCP `script_health` | JSON 介面(`window_days` 預設 14) |

該行動的訊號:KPI 變黃 → 查 `script_health` → 跑 LLM 重錄。
寫 test 時:YAML 越 deterministic, script auto-record 越穩定。

## 🌐 AgoraMarket viewport(寫 buyer/seller/delivery 測試前必讀)

[`docs/AGORA_VIEWPORT.md`](docs/AGORA_VIEWPORT.md) — 各 role viewport 對照
+ 跨 role 切換 SOP + 設錯的視覺症狀。

最常見坑:**buyer/seller/delivery 必須加 H5 viewport**(390×844 mobile=true)。
忘記加 → 截圖 > 800KB,看到桌面版面,selector 整批失準。

## 🤖 Flutter 測試模式 (2026-04-29 更新)

[`docs/FLUTTER_TEST_PATTERNS.md`](docs/FLUTTER_TEST_PATTERNS.md) — 對照表:
off-screen textbox / scroll 後找按鈕 / Flutter tab 切換 / CJK 輸入 /
ExpansionTile / headless_chrome 已知修復 (PR #118 / #124)。

## 🔬 Benchmark / LLM 比較 SOP(跑前必做)

```bash
bash scripts/preflight.sh   # 6-section 驗證,FAIL 則修完再跑
# 或在 Windows / 沒 bash 環境用純 Rust 版:
./target/release/sirin.exe doctor
```

**LLM fallback chain(必須設定)**:
```env
LLM_FALLBACK_BASE_URL=https://api.deepseek.com/v1
LLM_FALLBACK_API_KEY=sk-...       # platform.deepseek.com 新帳號 $5 免費
LLM_FALLBACK_MODEL=deepseek-chat  # = DeepSeek V4(自動 alias)
```

Primary 429 → 立即切 fallback(0s 等待)。不設定 → Gemini 429 時測試卡 35s+。

## 🌍 Env vars that affect the browser / LLM

[`docs/ENV_REFERENCE.md`](docs/ENV_REFERENCE.md) — full reference.

Quick-glance for daily work:

| Var | Values | Purpose |
|---|---|---|
| `SIRIN_BROWSER_HEADLESS` | bool | Process-wide Chrome headless default |
| `SIRIN_PERSISTENT_PROFILE` | unset / `1` / path | Persist Chrome user-data-dir between launches |
| `SIRIN_BROWSER_IDLE_CLOSE_SECS` | int (default 60) | Auto-close test Chrome after N s of no active runs (0 disables) |
| `GEMINI_CONCURRENCY` | int (default 3) | Cap in-flight Gemini calls — fixes silent empty-response on burst |
| `LLM_PROMPT_CACHE` | bool (default off) | Anthropic native cache_control on system prompt (Issue #260) |

## 🗂️ Where user data lives (installed vs dev)

| Mode | Binary | Config | Data | .env |
|------|--------|--------|------|------|
| Installed (Windows) | `C:\Program Files\Sirin\` | `%LOCALAPPDATA%\Sirin\config\` | `%LOCALAPPDATA%\Sirin\` | `%LOCALAPPDATA%\Sirin\.env` |
| Dev (`cargo build`) | `target/release/` | `%LOCALAPPDATA%\Sirin\config\` | `%LOCALAPPDATA%\Sirin\` | `%LOCALAPPDATA%\Sirin\.env` or CWD fallback |
| Test (`cargo test`) | — | `./config/` (repo) | — | — |

Always use `platform::config_path()` / `platform::app_data_dir()` —
**never** hardcode `"config/foo.yaml"` literals.

## 👥 Multi-agent spawn rules

KB: [`sirin/trap-parallel-agent-worktree-contention`](https://github.com/Redandan/Sirin/issues/76)

- ✅ 同一 session 內 spawn 多個 Agent 並行 → OK,但**只能跨 repo**
- ❌ 同 repo 多 agent 並行 → working tree 互踩 (`isolation: "worktree"` 不
  是真隔離,會 checkout 切走別 agent 的未 commit 改動),必須**嚴格序列**
- ✅ `general-purpose` 可遞迴 spawn 子 agent 做 nested 平行
- ⚠️ Edit/Write/Glob 在 worktree 內路徑解析會跑到主 repo — 用絕對路徑

## 🐛 Issue creation rules

- ❌ 不要用 `gh issue create &`(並行)— issue ID 順序不可控
- ✅ 永遠序列 `gh issue create` 後立刻 `gh issue view N --json title` 驗證對應
