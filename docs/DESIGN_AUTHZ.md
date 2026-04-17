# Design: Pre-Authorization Engine

**Status:** Proposed (design only, no implementation yet)
**Target tier:** Tier 1 (safety / blast-radius)
**Depends on:** none
**Enables:** `DESIGN_MONITOR.md` (authz_ask prompts surface in GUI)

---

## 1. Why

外部 AI(Claude Desktop / Claude Code / Cursor / 第三方 LLM)透過 MCP 呼叫 Sirin 的 `browser_exec`,目前**完全沒 gate**。LLM 幻覺出:

```jsonc
// 在使用者自己的 Chrome 裡執行
{ "action": "ax_type", "backend_id": 42, "text": "<credit card number>" }
{ "action": "goto", "target": "https://phishing.example/" }
{ "action": "eval", "target": "navigator.credentials.get(...)" }
```

這些都會照做。需要一層像 Claude Code `settings.json` 的 **allow / deny / ask** 閘,但要 **URL-scoped** + **client-aware** + **learn-able**。

## 2. Goals / Non-goals

### Goals

- 每個 `browser_exec` call 在執行前過一道 policy engine
- 規則以 YAML 聲明,支援 URL glob、action name、JS 內容子字串、a11y label 子字串
- 支援 `deny`(硬拒絕)、`allow`(白名單通過)、`ask`(彈 prompt 給人類)
- 不同 MCP client 可以套不同 policy
- 新規則 on-the-fly 學習(彈「Allow always」後寫回 yaml)
- 所有決定寫 audit log

### Non-goals

- **不做身份認證**:連進 MCP 的都視為「已通過 transport 層認證」。authz 只管「能不能做這件事」,不管「你是誰」(client id 只用來差異化 policy,不做 verify)
- **不取代 transport 安全**:Sirin 仍只聽 `127.0.0.1`,跨機存取要用 SSH tunnel / reverse proxy 自己顧
- **不限制讀操作的輸出**:`ax_tree` 回完整 tree,不做 PII redaction(redact 屬於 observability / compliance 另一塊)

## 3. Threat model

| 情境 | 有無 authz 的差別 |
|---|---|
| LLM 亂下 `goto https://paypal.com` 後 `ax_type` | ❌ 無 authz:照做。✅ 有 authz:`paypal.com` 在 deny,整串拒絕 |
| LLM 亂下 `eval "document.cookie"` 外洩 cookie | ❌ 無:cookie 出去。✅ 有:eval js_contains `document.cookie` 在 deny |
| LLM 亂打 `ax_type` 到密碼欄 | ❌ 無:密碼寫進去。✅ 有:該 input label 含「password / 密碼」在 deny |
| 正常測試 `goto redandan.github.io` → click → type | ✅ 有:allowlist 匹配 URL pattern,順暢跑 |
| 測試新 URL 第一次碰到 | `mode=selective` 時彈 ask,用戶一鍵「Allow URL pattern」寫回 yaml |

## 4. Configuration file

### Location

- **Repo-local**(優先):`<repo>/.sirin/authz.yaml` — 每個 repo 自己的規則,可 commit
- **User-global**(fallback):`~/.sirin/authz.yaml` — 跨所有 repo 的 baseline
- **Built-in default**:hard-coded strict defaults in Rust,用戶刪檔仍有基本防護

Repo 覆寫 user,user 覆寫 default,**合併規則**:allow / deny / ask 三個 array 做 union;`mode` 取最後載入的值。

### Schema

```yaml
# .sirin/authz.yaml

# mode: selective | strict | permissive | plan
#   selective   : deny-by-default + allow-list + ask-list(預設)
#   strict      : 所有 mutating action 都 ask
#   permissive  : 只擋 deny,其他全 allow(for dev / CI)
#   plan        : 禁止所有 mutating action(只讓 AI 計畫,不讓它動)
mode: selective

# 完全零風險的 action,任何 client / URL 都直接過
# 這組是 Sirin 內建的 hard-coded default,yaml 只能 *擴充* 不能收斂
readonly_allow:
  - ax_tree
  - ax_find
  - ax_value
  - screenshot
  - url
  - title
  - console
  - network
  - exists
  - attr
  - read

# 每個 MCP client 的 policy group(client 名稱 match MCP initialize clientInfo.name)
# 沒 match 時 fallback "*"
clients:
  "claude-code@*":
    mode: permissive      # 開發場景,本地 Claude Code 完全信任
  "claude-desktop@*":
    mode: selective
  "cursor@*":
    mode: selective
  "*":
    mode: selective        # fallback

# URL × action 允許規則(用於 mode=selective)
# 欄位:
#   action            action 名(支援 * wildcard)
#   url_pattern       URL glob(支援 * 和 **)
#   js_contains       (只用於 eval action)JS 字串需含此子字串
#   name_substring    (用於 ax_* action)a11y name / value 需含此子字串
#   name_regex        (用於 ax_* action)a11y name 正則(比 substring 強)
#   not_name_matches  a11y name 不能含某些字(例如 password / 密碼)
allow:
  # 一般瀏覽
  - { action: goto, url_pattern: "https://redandan.github.io/**" }
  - { action: goto, url_pattern: "http://localhost:*/**" }
  - { action: goto, url_pattern: "http://127.0.0.1:*/**" }

  # a11y 互動(Flutter Web)
  - { action: "ax_*", url_pattern: "https://redandan.github.io/**" }
  - { action: "ax_*", url_pattern: "http://localhost:*/**" }

  # 座標 click / type(兜底,CanvasKit 無 a11y 時)
  - { action: click_point, url_pattern: "https://redandan.github.io/**" }
  - { action: type,        url_pattern: "https://redandan.github.io/**" }

  # 瀏覽器控制(不碰敏感內容)
  - { action: set_viewport }
  - { action: clear_browser_state }
  - { action: wait_for_request }
  - { action: wait_for_new_tab }
  - { action: scroll, url_pattern: "**" }
  - { action: key,    url_pattern: "**" }

# 硬拒絕(永遠優先於 allow,即使規則重疊)
deny:
  # 敏感域名
  - { url_pattern: "https://**paypal**/**" }
  - { url_pattern: "https://**bank**/**" }
  - { url_pattern: "https://*.stripe.com/**" }
  - { url_pattern: "https://*.coinbase.com/**" }

  # 敏感 JS
  - { action: eval, js_contains: "document.cookie" }
  - { action: eval, js_contains: "window.ethereum" }
  - { action: eval, js_contains: "navigator.credentials" }
  - { action: eval, js_contains: "chrome.storage" }

  # 敏感輸入
  - { action: "ax_type*",
      not_name_matches: ["password", "密碼", "private key", "seed phrase", "助記詞", "ssn", "社會安全"] }
  - { action: type,
      url_pattern: "https://**login**/**",
      js_contains: "password" }

# 彈 GUI 問人類(一次性決定)
ask:
  - { action: goto, url_pattern: "https://**.google.com/**" }
  - { action: goto, url_pattern: "https://**github.com/settings/**" }
  - { action: goto, url_pattern: "https://**github.com/*/settings/**" }

# Learn mode 設定
learn:
  # 開啟 learn mode:未知 pattern 第一次碰到時彈 ask,給「Allow always」選項
  enabled: true
  # 新學到的 allow 寫回哪:repo 或 user
  write_back_to: repo    # repo | user | memory_only
  # 最多彈幾次 ask 後就不再學(防 AI 瘋狂 spam)
  max_asks_per_session: 20

# 設定本身的驗證
audit:
  # 所有決定(allow / deny / ask 結果)寫入檔案(NDJSON,每行一個 event)
  # 路徑相對 .sirin/ 目錄
  log_path: audit.ndjson
  # log rotation:超過 10 MB 自動 rotate(.1, .2, ...)
  max_size_mb: 10
  # 最多保留幾個 rotated file
  max_backups: 5
```

### 內建 hard-coded default

Rust side `authz::defaults()` 回傳:

```yaml
mode: selective
readonly_allow: [ax_tree, ax_find, ax_value, screenshot, url, title, console, network, exists, attr, read]
allow: []
deny:
  - { url_pattern: "file:///**" }                      # 本地 file:// 拒絕讀
  - { url_pattern: "chrome://**" }                     # Chrome internal
  - { url_pattern: "chrome-extension://**" }           # 擴充 internal
  - { action: eval, js_contains: "document.cookie" }
  - { action: eval, js_contains: "window.ethereum" }
  - { action: eval, js_contains: "navigator.credentials" }
  - { action: eval, js_contains: "indexedDB.open" }    # IDB 內容外洩路徑
  - { action: "ax_type*",
      not_name_matches: ["password", "密碼", "private key", "seed phrase", "助記詞"] }
ask: []
```

刪 yaml 仍有這組 baseline。

## 5. Decision engine

### Algorithm

```rust
// src/authz/engine.rs (pseudo)
pub fn decide(
    client_id: &str,
    action: &str,
    args: &serde_json::Value,
    current_url: &Option<String>,
    config: &AuthzConfig,
) -> Decision {
    let policy = config.resolve_client_policy(client_id);

    // 1. readonly_allow 直通
    if config.readonly_allow.contains(action) {
        return Decision::Allow("readonly");
    }

    // 2. deny 永遠最高優先
    for rule in &config.deny {
        if rule.matches(action, args, current_url) {
            return Decision::Deny(rule.describe());
        }
    }

    // 3. mode 簡單路徑
    match policy.mode {
        Mode::Permissive  => return Decision::Allow("permissive mode"),
        Mode::Plan        => {
            if is_mutating(action) {
                return Decision::Deny("plan mode — mutating disabled");
            }
            return Decision::Allow("plan mode readonly");
        }
        Mode::Strict      => return Decision::Ask("strict mode — all mutating asks"),
        Mode::Selective   => { /* continue */ }
    }

    // 4. allow 檢查
    for rule in &config.allow {
        if rule.matches(action, args, current_url) {
            return Decision::Allow(rule.describe());
        }
    }

    // 5. ask 檢查
    for rule in &config.ask {
        if rule.matches(action, args, current_url) {
            return Decision::Ask(rule.describe());
        }
    }

    // 6. Learn mode 路徑
    if config.learn.enabled && !config.learn.exhausted() {
        return Decision::AskWithLearn;
    }

    // 7. Default deny
    Decision::Deny("no matching rule")
}
```

### Pattern matching 細節

- **URL glob**:用 `globset` crate(`*` 一段,`**` 任意段),不用自己寫 regex
- **action name**:支援 `*` 前/後綴(`ax_*` 配 `ax_tree`, `ax_click` 等;`*` 配所有)
- **js_contains / name_substring**:純 `String::contains`,case-insensitive
- **name_regex**:用 `regex` crate,Rust-flavour
- **not_name_matches**:array of substrings,**任何一個** match 就算違反(用於 deny)

### Rule ordering

- `deny` 比 `allow` 先過,即使是同 action/URL,deny 贏
- `allow` 內部按 yaml 順序,第一個 match 就決定
- `ask` 同樣按 yaml 順序

## 6. MCP integration

### 入口 gate

`src/mcp_server.rs::call_browser_exec` 開頭加:

```rust
let decision = authz::decide(
    &session.client_id,
    &action,
    &args,
    &browser::current_url(),
    &authz::config(),
);

match decision {
    Decision::Allow(reason) => {
        authz::audit::log_allow(&session, &action, &args, reason);
    }
    Decision::Deny(reason) => {
        authz::audit::log_deny(&session, &action, &args, &reason);
        return mcp_error(
            format!("authz deny: {}", reason),
            json!({ "action": action, "reason": reason, "hint": "add to .sirin/authz.yaml allow[]" })
        );
    }
    Decision::Ask(reason) | Decision::AskWithLearn => {
        // 推給 monitor GUI(見 DESIGN_MONITOR.md)
        // 沒 GUI 時(headless mode)→ fallback to deny with reason
        let resp = authz::ask_human(&session, &action, &args, reason, learn: matches!(decision, AskWithLearn)).await?;
        match resp {
            AskResponse::AllowOnce       => { authz::audit::log_allow_once(...); /* continue */ }
            AskResponse::AllowAlways(rule) => { authz::learn::persist(rule)?; /* continue */ }
            AskResponse::Deny            => {
                authz::audit::log_deny(...);
                return mcp_error("authz deny by human", ...);
            }
            AskResponse::Timeout         => {
                authz::audit::log_timeout(...);
                return mcp_error("authz ask timeout — treated as deny", ...);
            }
        }
    }
}
```

### Client id resolution

MCP `initialize` 的 `clientInfo`:

```json
{ "clientInfo": { "name": "claude-code", "version": "0.3.2" } }
```

→ Sirin 組出 client_id `claude-code@0.3.2`。沒傳 clientInfo 則 `unknown@unknown`。

每個 MCP session 的 client_id 存進 `SessionContext`,每個 call 都用它查 policy group。

## 7. Audit log format

NDJSON,每行一個 event,寫入 `.sirin/audit.ndjson`:

```json
{"ts":"2026-04-17T03:14:15.123Z","type":"allow","client":"claude-code@0.3.2","action":"ax_click","args":{"backend_id":42},"url":"https://redandan.github.io/#/wallet/withdraw","rule":"ax_* url=redandan.github.io/**"}
{"ts":"...","type":"deny","client":"claude-desktop@1.2.0","action":"goto","args":{"target":"https://paypal.com/login"},"url":"about:blank","rule":"url=**paypal**"}
{"ts":"...","type":"ask","client":"cursor@0.50","action":"goto","args":{"target":"https://google.com/oauth"},"url":"about:blank","decision":"allow_once","human_ts":"2026-04-17T03:14:22.456Z","wait_ms":7333}
{"ts":"...","type":"learn","client":"claude-code@0.3.2","new_rule":{"action":"goto","url_pattern":"https://docs.flutter.dev/**"},"written_to":".sirin/authz.yaml"}
```

Rotation:超過 `max_size_mb` 時 rename 為 `audit.ndjson.1`,現有 `.1` → `.2`,超過 `max_backups` 的刪除。

## 8. Learn mode details

### 彈 prompt 的選項

當 `AskWithLearn` 時,GUI 顯示:

```
┌─ Sirin AuthZ ──────────────────────────────────┐
│ claude-desktop@1.2.0 requests:                 │
│                                                │
│   action: goto                                 │
│   target: https://docs.flutter.dev/test       │
│                                                │
│ Not covered by any rule.                       │
│                                                │
│ [ Allow once                                 ] │
│ [ Allow always for https://docs.flutter.dev/** ] │
│ [ Allow always for any goto under this domain ] │
│ [ Allow always for this action ]              │
│ [ Deny once                                  ] │
│ [ Deny + block this URL pattern              ] │
└────────────────────────────────────────────────┘
```

### "Allow always" 寫回

選擇後 append 到 target yaml(`.sirin/authz.yaml` 或 `~/.sirin/authz.yaml`,由 `learn.write_back_to` 決定)的 `allow:` 或 `deny:` list 末尾。寫入前:

1. 讀 yaml → parse → insert → serialize → atomic rename(write-then-rename 確保不損壞)
2. 重新載入 in-memory config
3. audit log 寫一筆 `{type:"learn",...}`

### 防 spam

- `max_asks_per_session` 到上限後,新 ask 改成 auto-deny + warn,等 session restart 才 reset
- 同一 rule pattern 1 秒內重複 ask 直接用上次決定

## 9. Implementation plan

### Files

```
src/authz/
├── mod.rs          模組入口 + public API
├── config.rs       yaml loader + schema structs
├── engine.rs       decide() / rule matching
├── audit.rs        NDJSON writer + rotation
└── learn.rs        rule write-back + atomic rename

.sirin/
└── authz.yaml.example

docs/
└── AUTHZ.md        end-user facing(不是 design doc;用法範例 + troubleshooting)

tests/
├── authz_engine_test.rs      decide() 各 case
├── authz_config_test.rs      yaml loader
└── authz_learn_test.rs       write-back 不破壞檔案
```

### PRs

| # | 內容 | 規模 |
|---|---|---|
| 1 | `src/authz/config.rs` + schema structs + loader | 1/2 day |
| 2 | `src/authz/engine.rs` + matching + `decide()` + 單測 | 1 day |
| 3 | `src/mcp_server.rs` 加 gate + client_id 解析 | 1/2 day |
| 4 | `src/authz/audit.rs` + NDJSON writer | 1/2 day |
| 5 | `ask_human()` fallback:沒 GUI 時 deny(獨立於 DESIGN_MONITOR) | 1/2 day |
| 6 | `src/authz/learn.rs` + atomic write-back | 1/2 day |
| 7 | `docs/AUTHZ.md` + `authz.yaml.example` + e2e smoke | 1/2 day |

PR 1–5 可獨立上,無 GUI 時 `ask` 全部 fallback 成 `deny`,不卡 DESIGN_MONITOR。

## 10. Test plan

### Unit

- 每個 `Decision::*` 分支都要有 case
- Deny 比 allow 優先
- readonly_allow 直通
- client policy resolution(wildcard / exact / fallback)
- glob pattern edge case(`**` 尾端 / query string / fragment)
- name_regex / not_name_matches
- Learn mode write-back 不破壞既有 yaml 順序 / comments(用 `serde_yaml` + 註解保留策略)
- Audit log rotation 邊界

### Integration

- 起 Sirin + 送 MCP call,驗 audit log
- 送 deny 會拿到 MCP error,`content` 帶 rule reason
- 送 ask(用 mock GUI)會阻塞到決定回來
- `max_asks_per_session` 到上限後 auto-deny

### E2E(對 Agora Market)

- `goto https://redandan.github.io/` + `ax_click` 登入按鈕,驗 log 是 `allow`
- `goto https://paypal.com/` 驗 log 是 `deny`
- `eval "document.cookie"` 驗 log 是 `deny`

## 11. Open questions

1. **yaml 順序對 learn 的影響**:`serde_yaml` 序列化會 reorder keys 嗎?要用 `yaml-rust2` 保留順序 + 註解嗎?
2. **子 tab / iframe 的 URL 怎麼算**:`current_url` 指 top frame。iframe 內的 ax_click 要看 top 還是 iframe?建議用 top(攻擊面是 outer 網站)。
3. **MCP session 結束時 audit flush**:`SessionStop` hook 保證 log buffer flush 到 disk
4. **Learn mode 跟 version control**:repo `.sirin/authz.yaml` 被 learn 改了,是否該自動 `git add`?不該(侵入 VCS);只寫檔,由用戶決定
5. **跨 Sirin instance 的 audit 合併**:多個 Sirin(多 port)同時跑時,各寫各的 `audit.ndjson`,path 按 port 區分:`audit-7700.ndjson`

## 12. Backward compatibility

- 舊 Sirin 沒 authz.yaml → 載入 built-in defaults,行為等同 `mode: selective` + 內建 deny list
- 既有 MCP call 第一次試著做 `goto https://redandan.github.io` 會:
  - 如果 yaml 有 allowlist → 直接過
  - 如果 yaml 無 allowlist 且 `learn.enabled=true` → 彈 ask
  - 如果 yaml 無 allowlist 且 `learn.enabled=false` → deny
- 部署建議:第一次上線時 `learn.enabled=true` 跑一遍典型 workflow,讓它把 allowlist 自己學起來,然後手動 review `.sirin/authz.yaml` 確認規則合理,再 commit
