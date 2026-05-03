# Proposal: Macro-based MCP Tool Registry (#257)

Status: **draft, awaiting decision**
Author: AI session, 2026-05-03
Related: [#257](https://github.com/Redandan/Sirin/issues/257)

## tl;dr

Issue #257 asks for a `#[mcp_tool]` proc macro that auto-collects 65+ tool
definitions, replacing the current 3-place split (schema literal /
dispatch arm / handler fn).  This proposal lays out the actual scope,
risk, and three options:

| Option | Effort | Risk | ROI |
|--------|--------|------|-----|
| **A** Status quo + better docs | 0 | nil | low |
| **B** `register!` lookup table (no proc macro) | 1 day | low | ~70% |
| **C** Full proc macro (issue spec) | 3-5 days | high | 100% |

**Recommendation: Option B.** Reason: it captures the bulk of the pain
(adding a tool = 1 place instead of 3) without the proc-macro / inventory
linker risk, and stays a clean stepping stone if Option C ever becomes
worth its cost.

---

## 1. Current state (audited 2026-05-03)

`src/mcp_server.rs` is 6613 lines.  Every tool is defined in 3 places:

1. **Schema literal** in `handle_tools_list` (`tools/list` response).
   ~108 `"name":` entries, of which ~92 are real tools (some are nested
   schema fields).
2. **Dispatch arm** in `handle_tools_call` (`tools/call` route).
   86 `match` arms.
3. **Handler fn** elsewhere in the file.

### 1.1 Handler signature distribution

| Signature | Count | Examples |
|-----------|-------|----------|
| `fn call_X(args: Value) -> Result<Value, String>` | ~75 | most |
| `async fn call_X(args: Value) -> Result<Value, String>` | ~17 | `kb_search`, `route_query`, `explain_failure` |
| `fn call_X() -> Result<Value, String>` | ~15 | `discovery_status`, `list_allowlist` |
| `fn call_X(args: Value) -> Result<String, String>` | 4 | `memory_search` (text), `teams_approve`, `trigger_research` |
| `fn call_X() -> String` | 2 | `skill_list`, `teams_pending` |
| `async fn call_X(args: Value, user_agent: &str) -> Result<Value, String>` | **1** | `call_browser_exec` (authz needs caller identity) |

### 1.2 Two dispatch paths

The `handle_tools_call` function has *two* paths:

- **JSON path** (~87 tools): `call_X(args).await.map(wrap_json)` → wraps
  `Value` in `{"content": [{"type": "text", "text": pretty_json}]}`.
- **Text path** (5 tools): `match name { "memory_search" => call_X(args)?,
  ... }` → wraps `String` in `{"content": [{"type": "text", "text": s}]}`.

Both paths produce the same MCP envelope shape but call different
handlers.  The split exists because some tools naturally produce text
(memory search results) and some structured data (test runs).

### 1.3 Schema description richness

Every `inputSchema` JSON literal has hand-written Chinese descriptions
with concrete defaults and cross-references:

```json
"description": "聚合測試健康指標：pass rate (近 10 / 30 runs)、flaky 標記、avg iterations、avg duration、最常見 failure_category。不指定 test_id 時返回全部測試（依 pass_rate_7d 升序，最差優先）+ summary 區塊。",
```

These don't translate cleanly to `#[derive(JsonSchema)]` on an Args struct
— schemars takes Rust doc comments verbatim, but the call-site context
("不指定 test_id 時返回全部測試") doesn't belong on a struct field.

### 1.4 Pain quantified

Adding a new tool today touches **3 places** in one file:
1. Insert ~10-line schema literal in `handle_tools_list` (alphabetised
   by section, no enforced sort).
2. Insert 1-line dispatch arm in `handle_tools_call`.
3. Write the `call_X` handler.

Common bugs:
- **Silent drop**: schema literal added but dispatch arm missing.
  `tools/list` advertises the tool, `tools/call` returns "Unknown tool".
  Caught only at runtime by the consumer.
- **Schema drift**: handler accepts a field the schema doesn't declare,
  or vice versa.  No compile-time link between them.
- **Sort drift**: the schema literal block has no canonical order,
  making PR diffs noisy when two contributors add tools simultaneously.

ROI of fixing this is real but bounded — adding a tool costs ~5 minutes
of friction, not 5 hours.  The bigger payoff is regression prevention.

---

## 2. Goal

What we want to optimize for, in order:

1. **Adding a tool = 1 place change.**  Schema, dispatch, and handler
   should all flow from a single source.
2. **Compile-time link between schema and handler.**  If the handler
   reads `args["foo"].as_str()` but the schema doesn't declare `foo`,
   we want a build-time signal.
3. **Description richness preserved.**  No regression in the user-facing
   text on `tools/list`.
4. **Migration is gradual.**  Both old and new must coexist during the
   transition.  No big-bang rewrite.
5. **Stays buildable on Sirin's existing toolchain.**  No new procedural
   macro crate unless the ROI clearly justifies it.

---

## 3. Three options

### Option A — Status quo + lint test

Don't refactor.  Just add a unit test that walks `handle_tools_list`,
extracts every `"name":` value, walks `handle_tools_call`'s match arms,
and panics if either set has an entry the other doesn't.  This catches
the "silent drop" bug without restructuring code.

**Effort**: 1-2h.
**Risk**: nil.
**ROI**: small but real — catches the most common bug.

This is the **minimum-viable improvement**.  Worth landing regardless of
which option we pick for #257 long-term.

---

### Option B — `register!` lookup-table refactor (no proc macro)

Replace the 86-arm `match` and the schema-literal block with a single
`OnceLock<HashMap<&'static str, ToolDef>>` populated by a `register!`
macro_rules! macro at startup.

```rust
struct ToolDef {
    name:        &'static str,
    description: &'static str,
    input_schema: Value,
    handler:     ToolHandler,  // boxed enum over the 4 variants
}

enum ToolHandler {
    SyncJson(fn(Value) -> Result<Value, String>),
    AsyncJson(fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>),
    SyncText(fn(Value) -> Result<String, String>),
    AsyncText(fn(Value) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>),
    BrowserExec, // sentinel — special-case for the user_agent variant
}

macro_rules! register {
    ($name:literal, $desc:expr, $schema:tt, $handler:expr) => {
        TOOL_REGISTRY.with(|r| r.insert($name, ToolDef {
            name: $name,
            description: $desc,
            input_schema: serde_json::json!($schema),
            handler: $handler,
        }))
    };
}
```

Adding a new tool becomes:

```rust
// Single place: where the handler lives.
fn call_my_new_tool(args: Value) -> Result<Value, String> {
    /* ... */
}

inventory::submit!(ToolDef {
    name: "my_new_tool",
    description: "What it does. 預設 N。",
    input_schema: json!({"type": "object", "properties": {...}}),
    handler: ToolHandler::SyncJson(call_my_new_tool),
});
```

`tools/list` becomes a 5-line iteration over the registry.
`tools/call` becomes a 5-line lookup + dispatch.
86-arm `match` deleted.  Schema-literal block deleted.

**Effort**: 1 day.
- Half-day to write the registry + register! macro.
- Half-day to migrate the 92 tools (mechanical: cut schema literal, paste
  into `register!` next to the handler).

**Risk**: low.
- No new crate, no proc macros, no `inventory`/`linkme` linker magic.
- The `inventory::submit!` line above is for illustration — Option B
  uses `static REGISTRY: OnceLock<...>` populated by a `ctor`-style
  initialiser or just by calling `register_all()` once at startup with
  every tool listed in one place.  Either way, Rust-checks the handler
  signature against the enum variant.

**Subtle gotcha**: `ToolHandler::SyncJson(fn ptr)` only works for plain
function pointers.  Closures capturing state can't be put in a `static`.
This is fine for our 92 handlers — they're all top-level fns.

**Limitation**: `input_schema` is still a hand-written `json!()` literal,
not derived from a struct.  Goal #2 (compile-time schema↔handler link)
isn't met — but the other 4 goals are.

**Migration**: incremental.  Day 1: introduce `ToolDef` + registry.  Day
2-3 (later PRs): convert 5-10 tools per PR.  Old `match` and new registry
coexist via `dispatch_old_first(name)` + `dispatch_new(name)` fallthrough.

---

### Option C — Full `#[mcp_tool]` proc macro (issue spec)

```rust
#[derive(JsonSchema, Deserialize)]
struct RunTestArgs {
    /// 測試 id（config/tests/*.yaml 中的 id 欄位）
    test_id: String,
    /// 失敗時自動 spawn claude_session 修 bug（預設 false）
    #[serde(default)]
    auto_fix: bool,
}

#[mcp_tool(
    name = "run_test_async",
    description = "非同步啟動測試；立即返回 run_id。用 get_test_result 輪詢狀態。"
)]
async fn run_test_async(args: RunTestArgs) -> Result<Value, String> {
    /* ... */
}
```

The proc macro:
1. Reads attributes (name, description).
2. Generates schema by calling `schema_for!(RunTestArgs)` at runtime
   on first registry build.
3. Submits a `ToolDef` to a global `inventory::submit!`.
4. Wraps the handler so `Value`-shaped input is `serde_json::from_value`'d
   into `RunTestArgs` and any deserialisation error becomes a clean
   "missing/invalid field" message.

**Effort**: 3-5 days.
- Day 1: new `crates/sirin-macros/` proc-macro crate (this is fine, proc
  macros must be separate crates).  Hand-write a minimal `mcp_tool!`
  attribute macro using `syn` + `quote` + `inventory`.
- Day 2: decide canonical handler signature.  Currently 6 variants;
  proc macro must support at least: `Args → Value`, `Args → String`,
  `Args + user_agent → Value`.  Either rewrite the 6 variants down to
  1-2 (touching 92 fns), or make the macro accept all 6 (complex macro).
- Day 3: convert ~10 tools per spec.  Each conversion: define `Args`
  struct + `JsonSchema` derive + move docstrings from JSON literal to
  Rust doc comments.
- Day 4-5: handle the long tail.  Tools without args (`call_skill_list`)
  need an empty-args struct.  `call_browser_exec`'s `user_agent` needs
  a special attribute (`#[mcp_tool(needs_caller_identity)]`).  Schema
  descriptions that reference call-site context (#1.3) need rewording
  or a separate `#[mcp_tool(extra_desc = "...")]` field.

**Risks** (real, not hypothetical):

1. **`inventory` cross-crate collection in a binary-only project**.
   Sirin has no `lib.rs`.  `inventory::submit!` works inside binaries
   but the linker behaviour around `inventory::collect!` from a binary
   crate vs a library crate is platform-specific.  Documented edge cases
   on Windows + LLVM thin-LTO (Sirin's `[profile.release]`) may require
   `linkme` instead of `inventory`, and `linkme` has different
   ergonomics.  This is the kind of issue that surfaces *only* in
   release builds, after the migration is half-done.

2. **`schemars` description richness regression**.
   Current schema descriptions are call-site-aware: "不指定 test_id 時
   返回全部測試（依 pass_rate_7d 升序）".  With `#[derive(JsonSchema)]`
   on `Args`, that description would land on the `test_id` field, which
   is grammatically wrong ("if not specified" is a *response* behaviour,
   not a field property).  Fix requires either:
   - Rewriting all 92 descriptions to fit the field/struct grammar.
   - Adding a `#[mcp_tool(input_schema_extra = "...")]` escape hatch
     that bypasses derive.

3. **Hybrid coexistence is the danger window**.
   Per spec, Phase 1 is "10 tools converted".  That means for 1-3 weeks
   (until all 92 land), `tools/list` and `tools/call` must read from
   *both* the new registry *and* the old `match` block.  Forgetting to
   add a tool to one or the other = silent drop, exactly the bug we're
   trying to fix.

4. **Async + Send + 'static** in the function pointer.  The macro
   needs to handle the existing 17 async handlers, some of which capture
   `&'static` shared state via `OnceLock`.  Stuffing them into a
   `Box<dyn Fn(Value) -> Pin<Box<dyn Future + Send>>>` is doable but
   adds 1-2 levels of indirection that the current direct `match` arm
   doesn't have.  Performance is fine (this is JSON-RPC, not a hot
   loop); it's just extra cognitive cost.

**Acceptance criteria from the issue**:
- [x] proc macro crate
- [ ] ≥ 10 tools converted
- [ ] Manual `register` block deleted
- [ ] `cargo test` passes
- [ ] CLAUDE.md "how to add MCP tool" section

If we ship Option C, Phase 1 (10 tools) leaves us in the "hybrid danger
window" indefinitely unless someone commits to finishing the migration.

---

## 4. Decision matrix

| Goal | A: status quo | B: lookup-table | C: proc macro |
|------|---------------|-----------------|---------------|
| 1 place to add tool | ❌ (3 places) | ✅ | ✅ |
| Compile-time schema↔handler link | ❌ | ❌ | ✅ |
| Description richness | ✅ | ✅ | ⚠️ (regression risk) |
| Gradual migration | n/a | ✅ | ⚠️ (hybrid window) |
| No new build-time deps | ✅ | ✅ | ❌ (`syn`, `quote`, `inventory`, `schemars` heavier use) |
| **Total** | 2/5 | 4/5 | 3.5/5 |

Option B scores best on the criteria we actually care about.  Option C
scores higher on the literal issue spec but the marginal improvement
(goal 2: compile-time schema↔handler) costs the most risk.

---

## 5. Recommendation

**Pick Option B.**

If after Option B lands the team still feels the schema↔handler gap
is painful (real instances of "I added a field to the handler but
forgot the schema"), revisit Option C with the lookup-table already in
place — at that point migrating to proc-macro is mechanical (replace
`register!` with `#[mcp_tool]`).

### Sub-recommendation regardless of A/B/C

Land the **lint test from Option A** *first*, in its own ~50-line PR.
It catches the "silent drop" bug today, runs in CI, and remains useful
forever — even after a future Option B/C refactor.

```rust
#[test]
fn tools_list_and_dispatch_in_sync() {
    let listed = extract_tool_names_from(handle_tools_list());
    let dispatched = extract_match_arms_from_source(...);
    let only_listed: Vec<_> = listed.difference(&dispatched).collect();
    let only_dispatched: Vec<_> = dispatched.difference(&listed).collect();
    assert!(only_listed.is_empty(), "advertised but undispatched: {:?}", only_listed);
    assert!(only_dispatched.is_empty(), "dispatched but unadvertised: {:?}", only_dispatched);
}
```

(Implementation note: `extract_match_arms_from_source` reads `mcp_server.rs`
as text and grep-parses the match arms.  Less elegant than reflection
but doesn't need any compile-time setup.)

---

## 6. Open questions for the user

Before either Option B or C ships, decide:

1. **Canonical handler signature.**
   Today: `fn`/`async fn`, `Value`/`String` return, `(Value)`/`()`/`(Value, &str)`.
   - Pick one for new tools?  Most natural: `async fn (Value) -> Result<Value, String>` (super-set of all current variants except `user_agent`).
   - Migrate existing tools to canonical, or keep variants?

2. **`browser_exec`'s `user_agent`.**
   Only 1 of 92 handlers needs it (for authz).  Options:
   - Thread `user_agent` into a request-scoped `tokio::task_local!` so
     all handlers can call `current_user_agent()` if they want.  Eliminates
     the special signature.  Cleanest.
   - Keep the special signature and special-case it in the registry.
     Less clean but zero-impact on the other 91 handlers.

3. **Schema description grammar.**
   Several current descriptions read "if X is unset, the tool does Y" —
   that's response behaviour, not field property.  Rewrite to fit a
   field-grammar?  Or keep an `extra_desc` escape hatch?

4. **`linkme` vs `inventory` vs explicit `register_all()`** (only relevant for Option C).
   Recommendation: **explicit `register_all()`** called once at server
   startup.  Listed in one place, no linker magic, easier to debug.
   Costs us "auto-collect" but the savings (a 1-line registration vs a
   3-line scattered diff) are the win, not the auto part.

---

## 7. If "yes go", what's the next concrete step

For **Option B**: I'd open a PR titled `refactor(mcp): introduce
ToolDef registry — preparation for #257`, ~300 lines of net change,
converting 5 tools as proof.  Then a follow-up issue tracks the
remaining 87 conversions (mechanical, can be split across many PRs).

For **Option C**: I'd open a PR titled `chore(macros): scaffold
sirin-macros crate for #257` that *only* lands the proc-macro crate
+ converts 1 tool end-to-end.  Get the linker / inventory wiring right
on a known-isolated change before touching any other tool.

For **Option A** (lint only): I'd open `test(mcp): add tools_list ↔
dispatch parity test (#257)` as a 50-line PR, no behaviour change.

---

## 8. Out of scope (don't bundle)

- Splitting `mcp_server.rs` into per-domain modules (test_runner tools,
  KB tools, agent tools).  That's #256-style structural cleanup, separate
  proposal.
- Generating an OpenAPI / TS-types contract from the schemas.  Useful but
  orthogonal.
- Auto-doc generation for `MCP_API.md`.  Same.
