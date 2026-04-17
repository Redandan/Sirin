# AgoraMarket — Sirin Cheatsheet

Canonical reference for driving Sirin against **AgoraMarket**, a Flutter Web PWA
at `https://redandan.github.io/`. Target audience: an external Claude Code (or
Desktop) session that already has Sirin's MCP tools registered.

This is **AgoraMarket-specific application knowledge** (verified 2026-04-17).
For generic API shapes / semantics of every tool see
[`docs/MCP_API.md`](../docs/MCP_API.md). For all-purpose test-author patterns
see `.claude/skills/sirin-test/SKILL.md`.

> Why a dedicated cheatsheet? AgoraMarket uses Flutter **CanvasKit** — the DOM
> is essentially an empty `<canvas>`. CSS selectors / `querySelector` /
> `exists` / `attr` / `click(target:"#...")` **all silently fail**. The only
> reliable handle is the a11y tree, and that's dormant until explicitly woken.

---

## 1. Quick start

```bash
# 1. Launch Sirin with a headed browser (so you can watch)
SIRIN_BROWSER_HEADLESS=false target/release/sirin.exe
# (on macOS / Linux: .../sirin. Port defaults 7700; override via SIRIN_RPC_PORT.)

# 2. From the external agent side:
#    - goto AgoraMarket (gives Flutter ~5 s to hydrate)
#    - enable_a11y (internally clicks flt-semantics-placeholder + Tab×2)
#    - you now have ~50 a11y nodes to query

browser_exec goto https://redandan.github.io/
# (sleep ~5 s for Flutter bootstrap)
browser_exec enable_a11y
browser_exec ax_tree     # sanity-check: count should be ~50, not 1
```

Skipping step 2's `enable_a11y` leaves Flutter's semantics tree at **1 node
(RootWebArea only)** — `ax_find` will match nothing and everything downstream
breaks. This is the single most common failure mode on this site.

---

## 2. Login page a11y snapshot (verified 2026-04-17)

Freshly-loaded `https://redandan.github.io/` (unauthenticated) produces **~50
a11y nodes**. Key ones:

```text
[RootWebArea]    backend_id=2     "Agora Market"
[image]          backend_id=105   "Agora Market"              # logo
[textbox]        backend_id=3     ""                          # search/email field
[group]          backend_id=103   ""                          # button cluster wrapper
[button]         backend_id=6     ""                          # unlabelled icon button
[button]         backend_id=9     ""                          # unlabelled icon button
[button]         backend_id=98    "提交"
[StaticText]     backend_id=136   "提交"
[button]         backend_id=119   "使用 Google 登入"
[button]         backend_id=120   "使用冷錢包登入"
[button]         backend_id=123   "測試買家"                   # buyer test login
[button]         backend_id=124   "測試賣家"                   # seller test login
[button]         backend_id=125   "測試送貨員"                 # delivery test login
[button]         backend_id=126   "測試管理員"                 # admin test login
[button]         backend_id=129   "立即註冊"
[StaticText]     backend_id=137   "Agora Market"
[StaticText]     backend_id=138   "去中心化電商市集,連接買家、賣家與數位錢包,..."
```

### ⚠️ `backend_id` is **NOT stable**

Every page reload (or route change that rebuilds the widget tree) reassigns
backend IDs. **Never hardcode them** in trace files. The (`role`, `name`) pair
is the stable identifier — resolve it to a live backend_id right before each
click:

```jsonc
// DO:
{"name":"browser_exec","arguments":{
  "action":"ax_find","role":"button","name":"測試買家"
}}
// → nodes[0].backend_id → pass to ax_click

// DON'T:
{"name":"browser_exec","arguments":{
  "action":"ax_click","backend_id":123  // stale after reload
}}
```

---

## 3. Canonical workflows

### 3a. Log in as one of the four test roles

```jsonc
// 1. Find the button by role + name
{"name":"browser_exec","arguments":{
  "action":"ax_find","role":"button","name":"測試買家"
}}
// → {found:true, count:1, nodes:[{backend_id:123, role:"button", name:"測試買家"}]}

// 2. Click it
{"name":"browser_exec","arguments":{
  "action":"ax_click","backend_id":123
}}

// 3. Wait ~6 s for Flutter Navigator.push + auth API round-trip, THEN
//    re-enable a11y on the new route (fresh semantics tree).
{"name":"browser_exec","arguments":{"action":"enable_a11y"}}
```

Swap `name` to `"測試賣家"` / `"測試送貨員"` / `"測試管理員"` for the other
three roles.

### 3b. Pull all four test-login buttons in one call

```jsonc
{"name":"browser_exec","arguments":{
  "action":"ax_find",
  "role":"button",
  "name_regex":"^測試",
  "limit":10
}}
// → count:4, nodes:[測試買家 / 測試賣家 / 測試送貨員 / 測試管理員]
```

Note `name_regex` is a Rust regex with **no implicit anchoring** — add `^` / `$`
explicitly.

### 3c. Aggregate one-call orientation with `page_state`

```jsonc
{"name":"page_state","arguments":{}}
// → {
//     url, title, ax_node_count,
//     ax_summary,          // compact one-liner preview
//     ax_tree_text,        // full pretty-printed tree
//     screenshot_jpeg_b64, // visual ground truth
//     screenshot_size_bytes,
//     console, network
//   }
```

Use this at the start of any exploration instead of firing four separate
`browser_exec` calls (goto+enable_a11y+ax_tree+screenshot). One round-trip,
complete picture.

### 3d. Snapshot + diff (measure what changed after a click)

```jsonc
// Before
{"name":"browser_exec","arguments":{"action":"ax_snapshot","id":"before_click"}}

// Action
{"name":"browser_exec","arguments":{"action":"ax_click","backend_id":123}}
// wait ~5 s for Navigator.push to settle

// After
{"name":"browser_exec","arguments":{"action":"enable_a11y"}}
{"name":"browser_exec","arguments":{"action":"ax_snapshot","id":"after_click"}}

// Delta
{"name":"browser_exec","arguments":{
  "action":"ax_diff","before_id":"before_click","after_id":"after_click"
}}
// → {added:[...], removed:[...], changed:[{node_id, before_name, after_name}]}
```

Snapshot IDs live in the Sirin process for the current session only — they
don't persist across Sirin restart. Safe to reuse the same ID (overwrites).

### 3e. Fill a text field with **verified** typing

```jsonc
// 1. Find the input
{"name":"browser_exec","arguments":{
  "action":"ax_find","role":"textbox","limit":5
}}

// 2. Type, wait 300 ms, read back
{"name":"browser_exec","arguments":{
  "action":"ax_type_verified","backend_id":141,"text":"100.50"
}}
// → {backend_id, typed:"100.50", actual:"100.50 USDT", matched:true}
```

`matched:false` means Flutter rejected / transformed the input (wrong focus,
input mask, IME race) — treat as a test failure, don't proceed.

### 3f. Submit + wait on API request

```jsonc
// 1. Click submit
{"name":"browser_exec","arguments":{"action":"ax_click","backend_id":98}}

// 2. Block until the POST lands (or 5 s timeout)
{"name":"browser_exec","arguments":{
  "action":"wait_request",
  "target":"/api/withdraw/submit",
  "timeout_ms":5000
}}
// → {request:{method, url, status, req_body, body, ts, ...}}
```

`wait_request` auto-installs network capture — no need to pre-arm it.
`target` matches as URL substring (`/api/withdraw/submit` matches the full
`https://api.purrtechllc.com/api/withdraw/submit`).

---

## 4. Route cheatsheet (hash URLs)

Extracted from `lib/core/router/app_router.dart` and live-verified. AgoraMarket
uses hash routing (`/#/...`), not pushState.

| Route | Purpose |
|---|---|
| `/#/` or `/` | Login page (unauth) / landing (auth) |
| `/#/home` | Buyer home |
| `/#/wallet/withdraw` | Withdraw form |
| `/#/wallet/deposit` | Deposit |
| `/#/wallet/stake-form` | Stake form — **not `/#/stake`!** |
| `/#/messages` | Message centre |
| `/#/delivery` | Delivery rider home |
| `/#/admin/statistics` | Admin default landing after login |
| `/#/admin/tg/group-monitor` | TG group monitor |

Programmatic hash navigation — must go through `eval`:

```jsonc
{"name":"browser_exec","arguments":{
  "action":"eval",
  "target":"location.hash='#/wallet/withdraw'; 'ok'"
}}
```

`eval` returns the last expression as a string, hence the trailing `'ok'`.
The legacy `evaluate_js` action **does not exist** — calling it fails silently
(see pitfall §6.4).

---

## 5. Known test-fixture state

Long-lived records in the shared dev backend that affect test planning:

| Role | Fixture | Effect |
|---|---|---|
| **buyer** | Pending withdrawal `W2508060354120001T9LQ` ($119 USDT TRC20, created 2025-08-06) | `/#/wallet/withdraw` renders **"in-progress withdrawal"** card, not a new-withdraw form. K3/K4 withdraw trace cases must assert the in-progress view. |

Don't try to cancel / clean this — it's been used as a stable fixture across
multiple E2E runs. Add to this table as new long-lived fixtures appear.

---

## 6. Common pitfalls

### 6.1 Semantics tree starts dormant

Without `enable_a11y`, `ax_tree` returns **1 node** (`RootWebArea` only).
Flutter lazily hydrates the tree when assistive tech asks for it — Sirin's
`enable_a11y` fakes that by clicking the `flt-semantics-placeholder` div and
sending Tab×2. Always call it:

- Once after initial `goto`
- Again after any route change that rebuilds the widget subtree

### 6.2 `backend_id` is not stable

See §2 warning. Re-resolve via `ax_find(role, name)` before every click that
happens after a reload, route change, or dialog open/close.

### 6.3 CanvasKit has no DOM

For AgoraMarket, these are **broken** (they silently no-op or return empty):

- `browser_exec({action:"exists", target:"#login-btn"})`
- `browser_exec({action:"attr", target:".class", name:"..."})`
- `browser_exec({action:"click", target:"#submit"})` — CSS selector form

Use a11y tree (`ax_find` → `ax_click`) or coordinate-based `click_point`
instead.

### 6.4 `evaluate_js` is not a thing

Old traces occasionally show `{action:"evaluate_js", ...}` — that action name
**does not exist** in Sirin. It fails as an unknown action and the runner
moves on, often hiding real bugs. The JS-eval action is **`eval`**.

### 6.5 Session bleed between roles

Switching from (say) buyer to admin in the same Sirin session: a simple
reload is **not** enough. localStorage + cookies + IndexedDB all retain auth
state. Use:

```jsonc
{"name":"browser_exec","arguments":{"action":"clear_state"}}
```

before the second role's login. Full scrub, then `goto` + `enable_a11y` +
`ax_click` on the new test-login button.

---

## 7. End-to-end bash example — buyer withdraw

Complete executable script: launch Sirin (external), log in as buyer, navigate
to the withdraw page, assert the in-progress fixture is visible.

```bash
#!/usr/bin/env bash
# buyer-withdraw-smoke.sh — assumes Sirin running on :7700 with headed browser
set -euo pipefail
MCP=http://127.0.0.1:7700/mcp

call() {
  curl -s "$MCP" -X POST -H 'Content-Type: application/json' -d "$1"
}

wrap() {  # $1=tool $2=args-json
  printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"%s","arguments":%s}}' "$1" "$2"
}

# 1. Navigate + wake semantics
call "$(wrap browser_exec '{"action":"goto","target":"https://redandan.github.io/"}')" > /dev/null
sleep 5
call "$(wrap browser_exec '{"action":"enable_a11y"}')" > /dev/null

# 2. Resolve "測試買家" → backend_id → click
BUYER_ID=$(call "$(wrap browser_exec '{"action":"ax_find","role":"button","name":"測試買家"}')" \
  | python -c 'import sys,json; r=json.load(sys.stdin); print(json.loads(r["result"]["content"][0]["text"])["nodes"][0]["backend_id"])')

call "$(wrap browser_exec "{\"action\":\"ax_click\",\"backend_id\":$BUYER_ID}")" > /dev/null
sleep 6  # Navigator push + auth API

# 3. Hash-nav to withdraw form + re-enable a11y
call "$(wrap browser_exec '{"action":"eval","target":"location.hash=\"#/wallet/withdraw\"; \"ok\""}')" > /dev/null
sleep 3
call "$(wrap browser_exec '{"action":"enable_a11y"}')" > /dev/null

# 4. Assert in-progress fixture visible
RESULT=$(call "$(wrap browser_exec '{"action":"ax_find","role":"text","name_regex":"W2508.*T9LQ","limit":1}')")
echo "$RESULT" | grep -q '"found":true' && echo "PASS: pending withdraw visible" || { echo "FAIL"; exit 1; }
```

Run: `bash buyer-withdraw-smoke.sh`. Exit 0 if the W2508…T9LQ reference shows
up on the withdraw page, non-zero otherwise.

---

## Appendix — related docs

- [`docs/MCP_API.md`](../docs/MCP_API.md) — every tool, every argument (generic)
- [`docs/ROADMAP.md`](../docs/ROADMAP.md) — T-M14 is the backlog entry for this file
- `.claude/skills/sirin-test/SKILL.md` — trace authoring patterns
- `.claude/skills/agora-market-e2e/SKILL.md` (in `AgoraMarket` repo) — upstream
  project's view of the same workflows
