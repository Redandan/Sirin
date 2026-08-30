//! MCP tool registry — Option B from #257 proposal.
//!
//! Replaces the 3-place split (schema literal in `handle_tools_list`,
//! dispatch arm in `handle_tools_call`, handler `fn call_X`) with one
//! [`ToolDef`] entry per tool.  Adding a tool becomes a single
//! `register_tool!` call near its handler.
//!
//! ## Coexistence with the legacy path
//!
//! This module is additive — existing match-arm dispatched tools keep
//! working unchanged.  `handle_tools_list` and `handle_tools_call` in
//! `mcp_server.rs` consult the registry FIRST, then fall through to
//! the legacy literals / arms for un-migrated tools.
//!
//! Migrating a tool means:
//! 1. Delete its schema literal from `handle_tools_list`.
//! 2. Delete its match arm from `handle_tools_call`.
//! 3. Add a `register_tool!(...)` call inside [`seed_default`] below.
//!
//! Goal #1 from the proposal — "1 place to add a tool" — is met for
//! migrated tools.  Schema↔handler compile-time link (goal #2) is
//! NOT met by this design; that needs a proc macro (Option C).
//!
//! ## Handler signature variants
//!
//! Sirin's 92 tool handlers use 5 distinct signatures (the 6th —
//! `browser_exec` with `user_agent` — stays in the legacy path because
//! it's the sole authz exception).  [`ToolHandler`] is a tagged union
//! over the 5 sane variants; non-capturing closures coerce zero-arg
//! handlers (`fn () -> ...`) into the `(Value) -> ...` variants.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use serde_json::{json, Value};

// ── Public types ────────────────────────────────────────────────────────────

/// One tool's definition: name + schema + dispatch handler.  Inserted
/// into the global registry once at startup.
pub struct ToolDef {
    pub name:         &'static str,
    pub description: &'static str,
    /// JSON Schema for the tool's `arguments` payload.  Lives as a
    /// `Value` (not a struct) so we don't have to standardise the
    /// schema shape across all 92 tools — each registration can pass
    /// whatever shape the existing literal had.
    pub input_schema: Value,
    pub handler:      ToolHandler,
}

impl ToolDef {
    /// Render the tool definition into the JSON shape that
    /// `tools/list` returns.
    pub fn to_json(&self) -> Value {
        let mut definition = json!({
            "name":        self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        });

        match self.name {
            "codex_supervisor_snapshot" => {
                definition["annotations"] = json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                });
            }
            "codex_supervisor_report" => {
                definition["annotations"] = json!({
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                });
            }
            "codex_supervisor_claim" | "codex_supervisor_complete_action" => {
                definition["annotations"] = json!({
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "idempotentHint": false,
                    "openWorldHint": false
                });
            }
            _ => {}
        }

        definition
    }
}

/// Tagged union over Sirin's tool-handler signature variants.
///
/// **Why not one canonical signature**: rewriting 92 handlers down to
/// `async fn(Value) -> Result<Value, String>` would touch ~75 sync fns
/// and mass-async-ify them — a bigger churn than the registry refactor
/// itself.  The enum lets us migrate handlers as-is.
pub enum ToolHandler {
    /// Sync handler returning structured JSON.  Most common variant.
    SyncJson(fn(Value) -> Result<Value, String>),

    /// Async handler returning structured JSON.  Used by handlers that
    /// hit the LLM / KB / network.  The fn-pointer must be a NON-capturing
    /// closure (or a top-level fn) — closure variables cannot be smuggled
    /// into a `static`.
    AsyncJson(fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'static>>),

    /// Sync handler returning plain text (memory_search, skill_list, etc).
    /// The text wrap (`{"content":[{"type":"text","text":...}]}`) is
    /// applied uniformly inside [`ToolHandler::invoke`].
    SyncText(fn(Value) -> Result<String, String>),

    /// Async text handler.
    AsyncText(fn(Value) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>>),
}

impl ToolHandler {
    /// Run the handler and return the MCP-envelope-shaped JSON
    /// (`{"content":[...]}` for text mode, raw JSON wrapped via
    /// `wrap_json` for structured mode).  Always returns the same
    /// envelope shape regardless of handler variant — callers don't
    /// need to know which variant ran.
    pub async fn invoke(&self, args: Value) -> Result<Value, String> {
        match self {
            Self::SyncJson(f)  => f(args).map(wrap_json),
            Self::AsyncJson(f) => f(args).await.map(wrap_json),
            Self::SyncText(f)  => f(args).map(wrap_text),
            Self::AsyncText(f) => f(args).await.map(wrap_text),
        }
    }
}

// MCP-envelope helpers, mirrored from `mcp_server::wrap_json`.  Local
// copies so `mcp_registry` doesn't depend on private mcp_server items.
fn wrap_json(payload: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
        }]
    })
}

fn wrap_text(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }]
    })
}

// ── Global registry ─────────────────────────────────────────────────────────

/// Sorted by tool name so `tools/list` output is stable across runs.
type RegistryMap = BTreeMap<&'static str, ToolDef>;

static REGISTRY: OnceLock<RegistryMap> = OnceLock::new();

/// Initialise + return the global registry.  Idempotent — first caller
/// wins; subsequent callers get the cached map.
pub fn registry() -> &'static RegistryMap {
    REGISTRY.get_or_init(|| {
        let mut m = BTreeMap::new();
        seed_default(&mut m);
        m
    })
}

/// Look up a tool by name.  Returns `None` for un-migrated tools
/// (the caller should fall through to the legacy match path).
pub fn get(name: &str) -> Option<&'static ToolDef> {
    registry().get(name)
}

/// Iterate the registry's tool definitions in name order.
pub fn iter() -> impl Iterator<Item = &'static ToolDef> {
    registry().values()
}

/// `true` when the registry has at least one tool.
#[allow(dead_code)]
pub fn is_populated() -> bool {
    !registry().is_empty()
}

// ── Seed: hand-listed registrations ─────────────────────────────────────────
//
// Each migrated tool gets one entry here.  The order doesn't matter (the
// BTreeMap sorts on insert), but grouping by domain makes review easier.
//
// Migrations land in batches — see #257 for the running checklist.

fn seed_default(m: &mut RegistryMap) {
    m.insert("codex_supervisor_snapshot", ToolDef {
        name: "codex_supervisor_snapshot",
        description: "讀取 Sirin 本機 Codex 工作督導快照。顯示證據分類、驗收面缺口、可安全續推候選與人工介入項；不讀 prompt/回覆全文、不傳送訊息、不授權、不 fork。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler: ToolHandler::SyncJson(crate::codex_supervisor::call_snapshot),
    });

    m.insert("codex_supervisor_report", ToolDef {
        name: "codex_supervisor_report",
        description: "由 Codex heartbeat 回報一個任務的最小化結構證據，讓 Sirin 本機分類健康、完成、正常等待、需決策、授權不明、疑似停滯或可恢復中斷。只接受不含對話原文的 opaque key；不傳送任務訊息。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "threadId": {"type": "string", "description": "Codex task/thread id"},
                "hostId": {"type": "string", "description": "可選：Codex host id"},
                "latestUserTurnKey": {"type": "string", "description": "最新 user turn 的 hash/cursor；禁止放原文"},
                "unfinishedScopeKey": {"type": "string", "description": "未完成範圍的 hash/key；禁止放原文"},
                "latestTurnStatus": {"type": "string", "enum": ["ACTIVE", "COMPLETED", "FAILED", "INTERRUPTED", "WAITING", "UNKNOWN"]},
                "readable": {"type": "boolean"},
                "freshProgress": {"type": "boolean"},
                "activeOperation": {"type": "boolean"},
                "finalAnswerFulfills": {"type": "boolean"},
                "objectiveUnfinished": {"type": "boolean"},
                "waitingCorrectTime": {"type": "boolean"},
                "needsUserDecision": {"type": "boolean"},
                "needsNewApproval": {"type": "boolean"},
                "highRiskNextAction": {"type": "boolean"},
                "exactAuthorityPresent": {"type": "boolean"},
                "boundedObservationUnchanged": {"type": "boolean"},
                "toolResultGap": {"type": "boolean"},
                "assistantDeclaredIncomplete": {"type": "boolean"},
                "continuationAlreadyVisible": {"type": "boolean"},
                "controlStateMismatch": {"type": "boolean"},
                "readErrorCode": {"type": "string", "description": "可選：穩定錯誤碼，不得放 raw output"},
                "contract": {
                    "type": "object",
                    "properties": {
                        "intent": {"type": "string", "enum": ["ANALYZE", "CHANGE", "DEEP_CHECK", "UNKNOWN"]},
                        "authorizedActions": {"type": "array", "items": {"type": "string", "enum": ["READ", "EDIT", "TEST", "COMMIT", "PUSH", "DEPLOY", "DEVICE_INSPECT", "DEVICE_CONTROL", "EXTERNAL_MESSAGE"]}},
                        "deepCheckRequested": {"type": "boolean"},
                        "readOnly": {"type": "boolean"}
                    }
                },
                "coverage": {
                    "type": "array",
                    "maxItems": 16,
                    "items": {
                        "type": "object",
                        "properties": {
                            "surface": {"type": "string", "enum": ["CODE", "UNIT_TEST", "INTEGRATION", "RUNTIME", "WEB_UI", "MOBILE_UI", "SIMULATOR", "PHYSICAL_DEVICE", "DEPLOYMENT"]},
                            "status": {"type": "string", "enum": ["PASS", "FAIL", "PENDING", "MISSING_PROOF", "NOT_APPLICABLE", "UNKNOWN"]},
                            "required": {"type": "boolean"},
                            "humanRequired": {"type": "boolean"},
                            "observedAtMs": {"type": "integer"}
                        },
                        "required": ["surface", "status", "required"]
                    }
                }
            },
            "required": ["threadId", "latestUserTurnKey", "latestTurnStatus"]
        }),
        handler: ToolHandler::SyncJson(crate::codex_supervisor::call_report),
    });

    m.insert("codex_supervisor_claim", ToolDef {
        name: "codex_supervisor_claim",
        description: "領取至多一筆仍新鮮、通過權限與去重門檻的 Codex 續推候選。只建立 10 分鐘 Sirin 本機 claim 並回傳 scope-preserving prompt；不會直接呼叫 Codex 或傳送訊息。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "threadId": {"type": "string", "description": "可選：只領取指定 task"}
            }
        }),
        handler: ToolHandler::SyncJson(crate::codex_supervisor::call_claim),
    });

    m.insert("codex_supervisor_complete_action", ToolDef {
        name: "codex_supervisor_complete_action",
        description: "Codex heartbeat 嘗試續推後，將 ACCEPTED/NOT_SENT/FAILED/TARGET_RUNNING 回寫 Sirin 本機去重 ledger。只更新本機 metadata。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "claimId": {"type": "string"},
                "outcome": {"type": "string", "enum": ["ACCEPTED", "NOT_SENT", "FAILED", "TARGET_RUNNING"]}
            },
            "required": ["claimId", "outcome"]
        }),
        handler: ToolHandler::SyncJson(crate::codex_supervisor::call_complete_action),
    });

    // Domain: AI-facing discovery — first thing other AIs see when they
    // call `tools/list`, so the description spells out what Sirin is + the
    // help URL.  Returning the URL inline lets curl-only callers skip the
    // browser entirely.
    m.insert("help", ToolDef {
        name:         "help",
        description: "📖 What is Sirin? — AI-driven browser test daemon. Returns connection methods + tool categories + the help URL (http://127.0.0.1:7700/help) for full HTML docs. Other AIs (Claude / Cline / Cursor / curl) should call this FIRST to learn what 65+ tools are available and how to use them. No args required.",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_help),
    });

    m.insert("ai_monitor_speed_test", ToolDef {
        name:         "ai_monitor_speed_test",
        description: "AI Work Monitor manual download test. Downloads 5 MB only after an explicit call; it does not run in the background, change routes, or access credentials.",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_ai_monitor_speed_test(args))),
    });

    // Domain: skills + teams (text-mode handlers).
    m.insert("skill_list", ToolDef {
        name:         "skill_list",
        description: "列出 Sirin 所有可用技能（含 YAML 動態技能）。",
        input_schema: json!({"type": "object", "properties": {}}),
        // Wrap the zero-arg handler into a `fn(Value) -> Result<String, String>`
        // shape via a non-capturing closure.  Closures without captures coerce
        // to fn pointers, which is what the enum variant requires.
        handler:      ToolHandler::SyncText(|_args| Ok(crate::mcp_server::call_skill_list())),
    });

    m.insert("teams_pending", ToolDef {
        name:         "teams_pending",
        description: "取得 Teams 待確認回覆草稿列表。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncText(|_args| Ok(crate::mcp_server::call_teams_pending())),
    });

    // Domain: discovery (structured-JSON handler with no args).
    m.insert("discovery_status", ToolDef {
        name:         "discovery_status",
        description: "查詢 discovery crawler 的目前狀態（NotRun / Crawling / Done + 元數據）。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_discovery_status()),
    });

    // Domain: settings/allowlist + KB + replay scripts (zero-arg SyncJson handlers).
    // 6-tool batch migrated 2026-05-04 — see #257 follow-up commits.

    m.insert("list_redundant_allow", ToolDef {
        name:         "list_redundant_allow",
        description: "#229 — 找出 ~/.claude/settings.json allowlist 中的冗餘項目。\n\n冗餘定義：如果 pattern A 的前綴已被 wildcard pattern B 涵蓋（例如 Bash(git commit:*) 已被 Bash(git:*) 涵蓋），則 A 是冗餘的。\n\n回傳建議刪除的列表。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_list_redundant_allow()),
    });

    m.insert("list_allowlist", ToolDef {
        name:         "list_allowlist",
        description: "#225 — 列出 ~/.claude/settings.json permissions.allow 中所有項目。同時計算 Bash wildcard 和 exact 項目數量。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_list_allowlist()),
    });

    m.insert("list_slash_commands", ToolDef {
        name:         "list_slash_commands",
        description: "#225 — 列出 ~/.claude/commands/*.md 中所有 slash commands（name + 前 3 行 description）。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_list_slash_commands()),
    });

    m.insert("list_hooks", ToolDef {
        name:         "list_hooks",
        description: "#225 — 列出 ~/.claude/settings.json hooks 設定（event → command 清單）。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_list_hooks()),
    });

    m.insert("list_intents", ToolDef {
        name:         "list_intents",
        description: "#233 — 列出 ~/.claude/llm_intents.json 中的所有 intent → LLM 路由規則。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_list_intents()),
    });

    m.insert("list_saved_scripts", ToolDef {
        name:         "list_saved_scripts",
        description: "列出所有已儲存的確定性重播腳本（deterministic replay scripts）。顯示 test_id、儲存時間、成功/失敗次數、腳本 action 數量。用於管理腳本庫。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_list_saved_scripts()),
    });

    // Domain: ops + diagnostics + multi-agent queue (zero-arg SyncJson handlers).
    // 7-tool batch migrated 2026-05-04 — see #257 follow-up commits.

    m.insert("expire_points", ToolDef {
        name:         "expire_points",
        description: "#227 — 清除 ~/.claude/session_points.json 中已過期的 save points（saved_at + ttl_days < now）。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_expire_points()),
    });

    m.insert("config_diagnostics", ToolDef {
        name:         "config_diagnostics",
        description: "回傳 Sirin 當前配置診斷（LLM backend 連通、router 狀態、vision 可用性、Chrome/Claude CLI 等）。遇到測試全部失敗時用來自我檢查。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_config_diagnostics()),
    });

    m.insert("diagnose", ToolDef {
        name:         "diagnose",
        description: "Sirin 自我診斷快照 — 回傳 version / git commit / build date / platform / uptime / Chrome 狀態 / LLM provider+model / update 狀態 / 最近 ERROR/WARN log，外加一個預先填好環境資訊的 GitHub issue 模板（report_issue_template.body）。\n\n用法：外部 AI 在 Sirin MCP 操作遇到 bug 時，先呼叫 diagnose 拿快照，據此判斷：(1) 重試（transient）；(2) 提示用戶升級（你在 0.3.0 但 0.3.2 修了這個）；(3) 用 report_issue_template 開 issue（環境區塊已填好，用戶只要補 reproduction）。\n\n成本：~5–20 ms（一次 CDP getVersion + log tail）。安全在 caller 的 error path 每次呼叫。",
        input_schema: json!({"type": "object", "properties": {}}),
        // diagnose::snapshot returns Value (not Result) — wrap in Ok inside the closure.
        handler:      ToolHandler::SyncJson(|_args| Ok(crate::diagnose::snapshot())),
    });

    m.insert("browser_status", ToolDef {
        name:         "browser_status",
        description: "查詢 Sirin 目前開啟的 Chrome 瀏覽器狀態。\n\n返回：\n- is_open: Chrome 是否已啟動\n- tab_count: 總 tab 數\n- active_tab: 目前 active tab 的 index + URL\n- tabs: 每個 tab 的 index、URL、session_id（若有 named session）\n- named_sessions: 所有 named session 的 id → tab_index → URL 清單\n\n用途：排查 batch 測試後殘留的 tab、確認 session 是否正確關閉、即時查看瀏覽器狀態。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_browser_status()),
    });

    // Phase A diagnostics — added 2026-05-05.

    m.insert("tail_app_log", ToolDef {
        name:         "tail_app_log",
        description: "回傳 Sirin runtime log 環形 buffer 的最近 N 行（LogBufferLayer 自動 capture 所有 tracing 事件，buffer 上限 300 行）。Sirin 預設 stderr 不寫檔案，這是查 mid-run 訊息的唯一管道。\n\n參數：\n- lines (預設 100, max 300)\n- grep (substring，case-sensitive，可選)\n- level (\"ERROR\"|\"WARN\"|\"INFO\"，可選)\n\n用途：debug stuck test、看 watchdog/timeout/convergence guard 是否觸發、追 LLM error。\n\n範例：tail_app_log lines=50 grep=\"watchdog\"",
        input_schema: json!({
            "type": "object",
            "properties": {
                "lines": { "type": "number", "description": "回傳行數（預設 100，clamp 1..300）" },
                "grep":  { "type": "string", "description": "substring filter（case-sensitive）" },
                "level": { "type": "string", "enum": ["ERROR", "WARN", "INFO"], "description": "等級過濾" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_tail_app_log),
    });

    m.insert("queue_status", ToolDef {
        name:         "queue_status",
        description: "Test runner 佇列+並行度快照。Sirin 用 process-wide TEST_RUN_LOCK 強制 concurrency=1，所以 run_test_batch N 個 test 在 dashboard 看起來像 N 個同時跑（實際 serial）。本工具拆解：running=1 + queued=N，含 oldest_queued_for_secs（解掉「卡 50 分鐘」誤判）+ estimated_drain_secs（用最近 50 筆 median 推估）。\n\n用途：判斷「現在卡住嗎」、看排隊還要多久、確認 LLM ReAct mode 是否拖慢整批。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_queue_status),
    });

    m.insert("live_trace", ToolDef {
        name:         "live_trace",
        description: "Phase B — running test 的最近 N 個 TestStep（mid-run 可拿，不用等 test 完成）+ 當前 sub-phase（llm_call/browser_action/replay/verify）+ subphase_age_ms。專治「84s/iter 不知道在等 LLM 還是 browser」這類謎題。\n\n參數：\n- run_id (必填)\n- last_n (預設 5, max 30)\n\n回傳含每步 thought / action / observation（裁切到 1KB） / llm_model / llm_latency_ms / parse_errors / ts。Action input 內的 string > 200 chars 自動裁切（screenshot base64 不會炸 response）。\n\n用法：當 get_test_result 顯示 idle_secs > 30 時，呼叫此工具看 recent_steps + current_subphase 馬上判斷瓶頸在哪。",
        input_schema: json!({
            "type": "object",
            "required": ["run_id"],
            "properties": {
                "run_id": { "type": "string", "description": "spawned-run id from run_test_async" },
                "last_n": { "type": "number", "description": "回傳最近幾步（預設 5，clamp 1..30）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_live_trace),
    });

    m.insert("sync_config", ToolDef {
        name:         "sync_config",
        description: "將 repo 的 config/tests/ 同步到 %LOCALAPPDATA%\\Sirin\\config\\tests/（Sirin 執行時讀取的位置）。\n\n每次修改 YAML 測試檔後必須呼叫，否則 Sirin 跑的是舊版 YAML。\n\n返回：synced=true, files_copied=N。\n\n關閉 #187。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_sync_config()),
    });

    m.insert("agent_queue_status", ToolDef {
        name:         "agent_queue_status",
        description: "查看 AI 小隊任務佇列現況（所有任務的 id / status / 結果摘要）。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_agent_queue_status()),
    });

    m.insert("agent_clear_completed", ToolDef {
        name:         "agent_clear_completed",
        description: "清除任務佇列中所有已完成（done / failed）的任務，保留 queued / running。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(|_args| crate::mcp_server::call_agent_clear_completed()),
    });

    m.insert("cleanup_stale_worktrees", ToolDef {
        name:         "cleanup_stale_worktrees",
        description: "Issue #242 — Reap abandoned `sirin-task-*` worktrees older than `older_than_hours` whose task is no longer Queued/Running.\n\nSafe by default: pass `dry_run=true` to preview what would be deleted without touching the filesystem. With `dry_run=false`, removes the worktree via `git worktree remove --force` (fallback `rm -rf`) and drops the matching `task/<id>` branch.\n\nArgs:\n  older_than_hours (default 24, min 1, max 24*30)\n  dry_run          (default true)\n\nReturns: { reaped: [{ path, branch, age_hours, deleted, error? }, ...], total: N, dry_run, older_than_hours }.\n\nCron-friendly — per-row failures are recorded in `error` rather than aborting the sweep.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "older_than_hours": {
                    "type":        "integer",
                    "default":     24,
                    "minimum":     1,
                    "maximum":     720,
                    "description": "Only worktrees older than this many hours are candidates."
                },
                "dry_run": {
                    "type":        "boolean",
                    "default":     true,
                    "description": "When true (default), report-only — no filesystem mutation."
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_cleanup_stale_worktrees),
    });

    // Domain: session-memory + task-tracker + handoff (sync arg-taking handlers).
    // 10-tool batch migrated 2026-05-04 — same pattern as zero-arg, but the
    // handler's `fn(Value) -> Result<Value, String>` signature already matches
    // SyncJson directly, so no closure wrapper needed.

    // ── #227 session-memory (3 of 4 — expire_points already in this file) ──

    m.insert("save_point", ToolDef {
        name:         "save_point",
        description: "#227 — 在 ~/.claude/session_points.json 儲存一個進度記錄點（save point）。可在 session 內或跨 session 快速恢復上下文，比 handoff 更輕量。\n\nttl_days 預設 7 天後自動過期。",
        input_schema: json!({
            "type": "object",
            "required": ["label"],
            "properties": {
                "label":    { "type": "string", "description": "唯一識別名稱，例如 debug-issue-230" },
                "summary":  { "type": "string", "description": "進度摘要（自由文字）" },
                "ttl_days": { "type": "number", "description": "存活天數，預設 7" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_save_point),
    });

    m.insert("list_points", ToolDef {
        name:         "list_points",
        description: "#227 — 列出所有未過期的 save points（最新在前）。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "label_contains": { "type": "string", "description": "過濾：label 包含此字串（選填）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_list_points),
    });

    m.insert("restore_point", ToolDef {
        name:         "restore_point",
        description: "#227 — 讀取指定 save point 的 summary，用於恢復上下文。",
        input_schema: json!({
            "type": "object",
            "required": ["label"],
            "properties": {
                "label": { "type": "string", "description": "要恢復的 save point label" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_restore_point),
    });

    // ── #228 task-tracker (4 tools, full domain) ──

    m.insert("create_task", ToolDef {
        name:         "create_task",
        description: "#228 — 在 ~/.claude/tasks.json 建立一個 task 追蹤項目，取代 MEMORY.md 手動維護的 backlog。\n\n`priority`: P0=緊急 / P1=重要 / P2=一般。回傳 task_id（格式 T-YYYYMMDD-NNNN）。",
        input_schema: json!({
            "type": "object",
            "required": ["project", "description"],
            "properties": {
                "project":     { "type": "string", "description": "project 識別碼，例如 sirin / agora-backend / flutter" },
                "description": { "type": "string", "description": "任務描述" },
                "priority":    { "type": "string", "description": "P0 / P1 / P2，預設 P1" },
                "kb_refs":     { "type": "string", "description": "相關 KB topicKey（逗號分隔，選填）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_create_task),
    });

    m.insert("list_tasks", ToolDef {
        name:         "list_tasks",
        description: "#228 — 列出 ~/.claude/tasks.json 中的 tasks（預設只列 open）。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "project":  { "type": "string",  "description": "過濾 project（選填）" },
                "status":   { "type": "string",  "description": "open / done / all，預設 open" },
                "priority": { "type": "string",  "description": "過濾 priority（P0/P1/P2，選填）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_list_tasks),
    });

    m.insert("mark_task_done", ToolDef {
        name:         "mark_task_done",
        description: "#228 — 將 task 標為 done，附帶 resolution 說明。",
        input_schema: json!({
            "type": "object",
            "required": ["task_id", "resolution"],
            "properties": {
                "task_id":    { "type": "string", "description": "T-YYYYMMDD-NNNN 格式的 task ID" },
                "resolution": { "type": "string", "description": "完成說明或 commit hash" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_mark_task_done),
    });

    m.insert("link_task", ToolDef {
        name:         "link_task",
        description: "#228 — 把 task 和 GitHub issue URL 或 KB topicKey 連結。",
        input_schema: json!({
            "type": "object",
            "required": ["task_id"],
            "properties": {
                "task_id":     { "type": "string", "description": "T-YYYYMMDD-NNNN" },
                "github_url":  { "type": "string", "description": "GitHub issue URL（選填）" },
                "kb_topickey": { "type": "string", "description": "KB topicKey（選填）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_link_task),
    });

    // ── #224 handoff (3 tools, full domain) ──

    m.insert("create_handoff", ToolDef {
        name:         "create_handoff",
        description: "#224 — 建立一個 session 交接記錄，存到 ~/.claude/handoff_history.json。\n\n比 kbWrite 更簡潔：不需要 domain/layer/tags/fileRefs 等樣板參數，專門為 session bridge 設計。\n內容自動 unescape，get_latest_handoff 直接回傳 markdown，不需要 Python unescape。\n同時寫入 KB（topicKey=sirin-handoff-latest）供 SessionStart hook 使用。",
        input_schema: json!({
            "type": "object",
            "required": ["reason", "content"],
            "properties": {
                "reason":    { "type": "string", "description": "交接原因，例如「加了新 MCP 需重啟」" },
                "content":   { "type": "string", "description": "Markdown 格式的交接內容（接手 prompt + 完成事項等）" },
                "project":   { "type": "string", "description": "project slug，預設 sirin" },
                "file_refs": { "type": "string", "description": "動到的檔案（逗號分隔，選填）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_create_handoff),
    });

    m.insert("get_latest_handoff", ToolDef {
        name:         "get_latest_handoff",
        description: "#224 — 讀取最新的 handoff 記錄，直接回傳 markdown 內容（已 unescape）。\n\n比 fetch-handoff.sh 更簡單：不需要 agora-trading auth，不需要 Python unescape，直接呼叫 Sirin MCP。\n可用於 SessionStart hook 的簡化版 fetch-handoff.sh。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "project": { "type": "string", "description": "project slug，預設 sirin" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_get_latest_handoff),
    });

    m.insert("list_handoff_history", ToolDef {
        name:         "list_handoff_history",
        description: "#224 — 列出最近 N 筆 handoff 記錄（最新在前）。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "project": { "type": "string", "description": "project slug，預設 sirin" },
                "limit":   { "type": "number",  "description": "筆數上限，預設 10" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_list_handoff_history),
    });

    // Domain: replay scripts + auto-fix history + #225 claude-config + #232
    // session-cost + #233 register_intent (sync arg-taking handlers).
    // 8-tool batch migrated 2026-05-04 — closes #225 and #232 sync subsets.

    m.insert("delete_saved_script", ToolDef {
        name:         "delete_saved_script",
        description: "刪除指定測試的已儲存腳本，迫使下次跑 LLM ReAct loop 重新生成。當 UI 改版導致腳本失效時使用。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "test_id": { "type": "string", "description": "要刪除腳本的 test_id" }
            },
            "required": ["test_id"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_delete_saved_script),
    });

    m.insert("list_fixes", ToolDef {
        name:         "list_fixes",
        description: "查詢 auto-fix 歷史（claude_session spawn 記錄）。能看到哪些 test 觸發過自動修復、結果如何。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "test_id": { "type": "string", "description": "選填" },
                "limit":   { "type": "number", "description": "預設 20" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_list_fixes),
    });

    // ── #225 claude-config (3 final tools — joins list_allowlist /
    //    list_slash_commands / list_hooks / list_redundant_allow already
    //    in this file → full sync subset of #225 now in registry) ──

    m.insert("suggest_allowlist", ToolDef {
        name:         "suggest_allowlist",
        description: "#229 — 掃描 ~/.claude/projects/**/*.jsonl 找出高頻 tool-call pattern，建議加到 Claude Code allowlist。\n\n`threshold`：同一 pattern 出現 N 次以上才建議（預設 2）。\n`sessions`：掃最近 N 個 session（預設 10）。\n`project_key`：限制掃指定 project 目錄名稱（例如 C--Users-Redan-IdeaProjects-Sirin），不指定則掃全部。\n\n回傳結果排除已在 allow list 中的 pattern，只顯示新增建議。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "threshold":   { "type": "number", "description": "出現次數閾值，預設 2" },
                "sessions":    { "type": "number", "description": "掃最近 N 個 session，預設 10" },
                "project_key": { "type": "string", "description": "限定 project 目錄名稱（可選）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_suggest_allowlist),
    });

    m.insert("add_allow", ToolDef {
        name:         "add_allow",
        description: "#225 — 向 ~/.claude/settings.json permissions.allow 新增一條 pattern。\n\n原子寫入：讀取 → 修改 → 寫回（JSON 格式化，4-space indent）。重複 pattern 自動去重。",
        input_schema: json!({
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": { "type": "string", "description": "例如 Bash(cargo:*) 或 Read" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_add_allow),
    });

    m.insert("remove_allow", ToolDef {
        name:         "remove_allow",
        description: "#225 — 從 ~/.claude/settings.json permissions.allow 刪除指定 pattern（精確匹配）。",
        input_schema: json!({
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": { "type": "string", "description": "要刪除的 pattern（完整字串匹配）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_remove_allow),
    });

    // ── #232 session-cost (2 tools, full domain) ──

    m.insert("session_cost", ToolDef {
        name:         "session_cost",
        description: "#232 — 解析 ~/.claude/projects/**/*.jsonl 計算指定 session（或最新 session）的 token 使用量和 API 費用估算。\n\n包含：input/output/cache tokens、USD 費用估算（用 Anthropic 公定價）、cache hit rate、最高成本 tool 排行。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "session_id":  { "type": "string", "description": "JSONL 檔案名稱（不含 .jsonl），選填，預設最新" },
                "project_key": { "type": "string", "description": "限定 project 目錄，選填" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_session_cost),
    });

    m.insert("list_expensive_sessions", ToolDef {
        name:         "list_expensive_sessions",
        description: "#232 — 列出最貴的 N 個 sessions（依 USD 成本降序）。\n\n`top` 預設 10。`project_key` 可限定 project 目錄。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "top":         { "type": "number", "description": "回傳數量，預設 10" },
                "project_key": { "type": "string", "description": "限定 project 目錄，選填" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_list_expensive_sessions),
    });

    // ── #233 cross-ai-router-mcp (sync subset — register_intent joins
    //    already-migrated list_intents) ──

    m.insert("register_intent", ToolDef {
        name:         "register_intent",
        description: "#233 — 在 ~/.claude/llm_intents.json 新增或更新一條 intent → LLM 路由規則。",
        input_schema: json!({
            "type": "object",
            "required": ["name", "backend"],
            "properties": {
                "name":    { "type": "string", "description": "intent 名稱，例如 indicator-design" },
                "backend": { "type": "string", "description": "LLM backend：gemini / deepseek / claude / ollama" },
                "model":   { "type": "string", "description": "指定 model（選填）" },
                "reason":  { "type": "string", "description": "路由原因備注（選填）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_register_intent),
    });

    // Domain: read-only test_runner queries (sync arg-taking handlers).
    // 14-tool batch migrated 2026-05-04 — picks every test_runner read with
    // no side effects (results, runs, traces, screenshots, analytics, replay
    // diagnostics).  The action-taking ones (run_test_async / run_test_batch
    // / run_test_pipeline / run_adhoc_test / kill_run / scaffold_test /
    // lint_test / persist_adhoc_run / discover_app / discovery_features)
    // stay on the legacy path — same shape but riskier to batch in one go.

    m.insert("list_tests", ToolDef {
        name:         "list_tests",
        description: "列出 config/tests/ 目錄下所有 YAML 測試 goal。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "tag": { "type": "string", "description": "選填：tag filter" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_list_tests),
    });

    m.insert("get_test_result", ToolDef {
        name:         "get_test_result",
        description: "依 run_id 取得測試狀態。可能狀態：queued | running | passed | failed | timeout | error。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "spawn_run_async 返回的 run_id" }
            },
            "required": ["run_id"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_get_test_result),
    });

    m.insert("get_screenshot", ToolDef {
        name:         "get_screenshot",
        description: "依 run_id 取得失敗截圖（base64 PNG）。若 bytes 為 null，screenshot_error 說明為何失敗。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string" }
            },
            "required": ["run_id"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_get_screenshot),
    });

    m.insert("get_full_observation", ToolDef {
        name:         "get_full_observation",
        description: "取得某步驟的完整（未截斷）browser tool observation。LLM 歷史中的 observation 被截斷時，可用這個抓完整內容。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string" },
                "step":   { "type": "number", "description": "0-indexed 步驟" }
            },
            "required": ["run_id", "step"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_get_full_observation),
    });

    m.insert("get_run_trace", ToolDef {
        name:         "get_run_trace",
        description: "依 run_id 取得每個 step 的 trace 元資料：LLM model/latency、KB injects、parse errors、timestamp。debug 失敗用。\n\n- `steps`: 陣列，每筆有 ts/llm_model/llm_latency_ms/parse_errors/kb_hits/action（簡化）\n- `summary`: total_steps、kb_hits（去重）、avg_latency_ms",
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "已完成的 run_id（從 SQLite test_runs 表讀取）" }
            },
            "required": ["run_id"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_get_run_trace),
    });

    m.insert("list_recent_runs", ToolDef {
        name:         "list_recent_runs",
        description: "查詢歷史測試執行記錄。不指定 test_id 時列所有測試的近期 runs。用來看 pattern / flakiness / 最近失敗原因。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "test_id": { "type": "string", "description": "選填：只看特定測試" },
                "limit":   { "type": "number", "description": "筆數（預設 20，最多 100）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_list_recent_runs),
    });

    m.insert("test_summary", ToolDef {
        name:         "test_summary",
        description: "一次呼叫取得最近一批測試的完整摘要：pass/fail counts + console_errors 統計 + 建議動作。適合每次 regression suite 跑完後立刻查看結果。\n\n回傳: { passed, failed, console_errors_total, console_warnings_total, results: [{test_id, status, console_errors, console_warnings, flag}] }",
        input_schema: json!({
            "type": "object",
            "properties": {
                "since":  { "type": "string", "description": "只看此時間（HH:MM）之後的 runs，預設最近 1 小時" },
                "limit":  { "type": "number", "description": "最多看幾個不重複的 test，預設 31" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_test_summary),
    });

    m.insert("test_analytics", ToolDef {
        name:         "test_analytics",
        description: "聚合測試健康指標：pass rate (近 10 / 30 runs)、flaky 標記、avg iterations、avg duration、最常見 failure_category。不指定 test_id 時返回全部測試（依 pass_rate_7d 升序，最差優先）+ summary 區塊。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "test_id": { "type": "string", "description": "選填：只看特定測試" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_test_analytics),
    });

    m.insert("test_coverage", ToolDef {
        name:         "test_coverage",
        description: "AgoraMarket 功能地圖覆蓋率報告。\n\n讀取 config/coverage/agora_market.yaml 功能地圖，交叉比對現有測試和 saved scripts，輸出：\n• 每個功能模組的覆蓋率（%）\n• 各功能點的狀態（confirmed/partial/missing）\n• 哪些測試覆蓋該功能 + 是否有 replay script\n• 覆蓋缺口（missing features）清單\n• 建議下一步新增哪個測試",
        input_schema: json!({
            "type": "object",
            "properties": {
                "group_id": { "type": "string", "description": "選填：只看特定 feature group（如 'buyer_browse'）" },
                "show_missing_only": { "type": "boolean", "description": "只顯示 missing features，預設 false" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_test_coverage),
    });

    m.insert("list_flaky_tests", ToolDef {
        name:         "list_flaky_tests",
        description: "列出歷史上不穩定（flaky）的測試，依 pass rate 升序（最差優先）。\n\n等同 test_analytics 但只回傳 flaky 條目，方便快速定位需要重點關注的測試。\n\nflaky 定義：近 10 次中 pass rate < threshold（預設 70%）且至少 3 次 runs。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "threshold": { "type": "number", "description": "flaky 閾值，0.0-1.0，預設 0.70" },
                "limit":     { "type": "number", "description": "回傳上限，預設 20" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_list_flaky_tests),
    });

    m.insert("script_health", ToolDef {
        name:         "script_health",
        description: "#266 — 回傳 saved_scripts 健康度指標，用於 Dashboard KPI / Coverage column / cron 偵測 stale-script drift。\n\n回傳兩部份：\n- `aggregate`: window 內 total_runs / passed_via_replay / passed_via_llm / failed / success_rate_replay + 上一週期比較 + drift\n- `per_test`: 每個 test 的 status (healthy|stale_fallback|llm_only|untested) + 最近 3 次 is_replay 歷史\n\n預設 window=14 天（前 7 天 vs 後 7 天比較）。若 drift < -0.10（10 個百分點），代表 replay 成功率明顯下滑，建議 audit。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "window_days": {
                    "type": "number",
                    "description": "比較窗（會被切兩半做 week-over-week drift），預設 14"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_script_health),
    });

    m.insert("replay_last_failure", ToolDef {
        name:         "replay_last_failure",
        description: "讀取某個測試最近一次失敗 run 的逐步執行記錄（step-by-step inspection）。\n\n不重新執行測試 — 直接從 SQLite history_json 取出每一個 action 和 LLM observation，讓你像翻日誌一樣審查失敗過程。\n\n`break_at` 可限制只看前 N 步，適合定位哪一步開始出錯。",
        input_schema: json!({
            "type": "object",
            "required": ["test_id"],
            "properties": {
                "test_id":  { "type": "string", "description": "YAML test_id，例如 agora_cart_add_remove" },
                "break_at": { "type": "number",  "description": "只返回前 N 步，0=全部（預設）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_replay_last_failure),
    });

    m.insert("shadow_dump_diff", ToolDef {
        name:         "shadow_dump_diff",
        description: "對比一次失敗 run 中第 A 步和第 B 步的 LLM observation（AX tree 快照）。以行為單位的 unified diff 格式輸出，方便定位 ExpansionTile 動畫、tab 切換等 UI 狀態變化。",
        input_schema: json!({
            "type": "object",
            "required": ["test_id", "step_a", "step_b"],
            "properties": {
                "test_id": { "type": "string", "description": "YAML test_id" },
                "step_a":  { "type": "number",  "description": "第一個步驟編號（1-based）" },
                "step_b":  { "type": "number",  "description": "第二個步驟編號（1-based）" },
                "run_id":  { "type": "string",  "description": "指定 run_id（選填，預設用最新失敗 run）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_shadow_dump_diff),
    });

    m.insert("compare_with_replay", ToolDef {
        name:         "compare_with_replay",
        description: "#230 — 對比同一 test_id 的最近一次 script replay run 和最近一次 LLM run 的結果差異。\n\n用於評估 replay 模式是否可靠：比較兩者的 status / duration / iterations / failure_category。\n若只有其中一種 run 存在，也會回傳現有資料並標注缺少哪一種。",
        input_schema: json!({
            "type": "object",
            "required": ["test_id"],
            "properties": {
                "test_id": { "type": "string", "description": "YAML test_id" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_compare_with_replay),
    });

    // Domain: action-taking test_runner tools + discover_app + scaffold/lint
    // (sync arg-taking handlers).  10-tool batch migrated 2026-05-04 — closes
    // the test_runner sync subset (only AsyncJson explain_failure remains).

    m.insert("run_test_async", ToolDef {
        name:         "run_test_async",
        description: "非同步啟動測試；立即返回 run_id。用 get_test_result 輪詢狀態。\n\n#269 — 可選 llm_override 跑單個 test 在 process-wide LLM_PROVIDER 之外的後端（split parallel cloud + local，避免 contention；compare backends 並排）。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "test_id":  { "type": "string", "description": "測試 id（config/tests/*.yaml 中的 id 欄位）" },
                "auto_fix": { "type": "boolean", "description": "失敗時自動 spawn claude_session 修 bug（預設 false）" },
                "llm_override": {
                    "type": "object",
                    "description": "#269 per-test LLM override；空 → 用 process-wide config",
                    "properties": {
                        "provider": { "type": "string", "description": "lmstudio | gemini | anthropic | ollama" },
                        "model":    { "type": "string", "description": "model id at the chosen provider" },
                        "base_url": { "type": "string", "description": "可選；空 → provider 預設 base_url" },
                        "api_key":  { "type": "string", "description": "可選；空 → provider env API key" }
                    },
                    "required": ["provider", "model"]
                }
            },
            "required": ["test_id"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_run_test_async),
    });

    m.insert("run_test_batch", ToolDef {
        name:         "run_test_batch",
        description: "並行啟動多個 YAML test，每個跑在獨立 chrome tab（session_id 自動分配）。立即返回 N 個 run_id。\n\n適用場景：smoke suite / nightly regression / 一次跑完多個 tag。\n\n限制：max_concurrency 最大 8（避免 CDP 連線過載）；不會自動 triage 或 auto_fix（失敗請用個別 run_test_async 重跑）；任一 test_id 找不到就整批拒絕。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "test_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "要並行跑的 test_id 清單（必須都存在於 config/tests/*.yaml）"
                },
                "max_concurrency": {
                    "type": "number",
                    "description": "最大同時執行數量；預設 3，最大 8"
                }
            },
            "required": ["test_ids"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_run_test_batch),
    });

    m.insert("run_test_pipeline", ToolDef {
        name:         "run_test_pipeline",
        description: "以管線方式循序執行多個 YAML test，組成粗粒度的交易流程測試。\n\n與 run_test_batch（並行）不同，pipeline 保證執行順序：stage1 完成後才跑 stage2。\n適用場景：C2C 交易生命週期（買家下單→賣家出貨→買家確認）等有依賴順序的流程。\n\n返回：pipeline_id（識別整批）+ 每個 stage 的 run_id（可分別 poll）。\nstop_on_failure=true 時，某 stage 失敗後跳過後續 stage。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "stage_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "按執行順序的 test_id 清單（每個都要存在於 config/tests/）"
                },
                "stop_on_failure": {
                    "type": "boolean",
                    "description": "某 stage 失敗後是否停止後續（預設 false = 繼續跑完所有）"
                }
            },
            "required": ["stage_ids"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_run_test_pipeline),
    });

    m.insert("kill_run", ToolDef {
        name:         "kill_run",
        description: "強制終止卡住的 in-memory test run（zombie）。\n\n當 run_test_async 啟動的 run 因 LLM call 阻塞而超過 timeout_secs 仍顯示 running 時使用。\n設為 error 狀態，讓後續 run_test_async 呼叫可以正常取得 TEST_RUN_LOCK。\n⚠️ 只能終止 Running/Queued 狀態的 run；已完成的 run 無法覆蓋。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "要強制終止的 run_id" }
            },
            "required": ["run_id"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_kill_run),
    });

    m.insert("run_adhoc_test", ToolDef {
        name:         "run_adhoc_test",
        description: "即席啟動測試 — 不需預先建立 YAML。外部 AI 收到用戶要求『測 <URL> 的 <流程>』時用這個。立即返回 run_id，用 get_test_result 輪詢。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "url":  { "type": "string", "description": "要測試的起始 URL" },
                "goal": { "type": "string", "description": "高階測試目標（自然語言描述）" },
                "success_criteria": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "通過條件（1-3 條；空陣列會用預設的『目標達成』判斷）"
                },
                "locale":           { "type": "string", "description": "zh-TW / en / zh-CN（預設 zh-TW）" },
                "max_iterations":   { "type": "number", "description": "預設 15" },
                "timeout_secs":     { "type": "number", "description": "預設 120" },
                "browser_headless": { "type": "boolean", "description": "Flutter CanvasKit/WebGL 必須設 false 才能 paint。預設讀 SIRIN_BROWSER_HEADLESS env（預設 true）" },
                "llm_backend":      { "type": "string", "description": "可選 LLM backend override：'claude_cli'/'claude' = 用 claude -p subprocess（Max plan、JSON 輸出最穩、~3-5s/呼叫 overhead）；省略或其他值 = 用 Sirin 主 LLM 設定（Gemini/LM Studio 等）。優先順序：此參數 > TEST_RUNNER_LLM_BACKEND env > 主設定" },
                "fixture": {
                    "type": "object",
                    "description": "可選的 fixture：setup（測試前執行）和 cleanup（測試後執行，無論成敗）。",
                    "properties": {
                        "setup": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "action":     { "type": "string" },
                                    "target":     { "type": "string" },
                                    "text":       { "type": "string" },
                                    "timeout_ms": { "type": "number" }
                                },
                                "required": ["action"]
                            }
                        },
                        "cleanup": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "action":     { "type": "string" },
                                    "target":     { "type": "string" },
                                    "text":       { "type": "string" },
                                    "timeout_ms": { "type": "number" }
                                },
                                "required": ["action"]
                            }
                        }
                    }
                }
            },
            "required": ["url", "goal"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_run_adhoc_test),
    });

    m.insert("persist_adhoc_run", ToolDef {
        name:         "persist_adhoc_run",
        description: "把一次成功的 ad-hoc 探索升級為永久 regression test。\n\n工作流：\n1. AI 用 run_adhoc_test 探索 → 拿 run_id\n2. 用 get_test_result 確認 status=passed\n3. 用 persist_adhoc_run(run_id, test_id='login_flow') 寫出 config/tests/login_flow.yaml\n4. 之後 run_test_async + test_id='login_flow' 就是 regression test\n\n會拒絕：失敗/未完成的 run、test_id 含大寫或連字符、test_id 以 'adhoc_' 開頭、檔案已存在（除非 overwrite=true）、run 超過 1 小時被 prune。\n\n預設行為：strip 'Ad-hoc: ' 前綴 / 把 'adhoc' tag 換成 'adhoc-derived' / max_iterations 提升到 max(used+5, original) 以容忍 regression 變異。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_id":  { "type": "string", "description": "run_adhoc_test 返回的 run_id（必須在 1 小時內、且狀態為 passed）" },
                "test_id": { "type": "string", "description": "新測試的永久 id，必須符合 [a-z0-9_]+，不能以 adhoc_ 開頭。會變成 config/tests/<test_id>.yaml" },
                "name":    { "type": "string", "description": "可選的人類可讀名稱；省略時沿用 ad-hoc 名稱（自動 strip 'Ad-hoc: ' 前綴）" },
                "tags":    {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "可選 tags 覆蓋；省略時沿用原 tags（'adhoc' 會被換成 'adhoc-derived'）"
                },
                "bump_iterations": { "type": "boolean", "description": "true 時將 max_iterations 提升到 max(used+5, original)，給 regression 留 slack；預設 true" },
                "overwrite":       { "type": "boolean", "description": "覆蓋現存檔案；預設 false（避免誤刪手寫測試）" }
            },
            "required": ["run_id", "test_id"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_persist_adhoc_run),
    });

    m.insert("discover_app", ToolDef {
        name:         "discover_app",
        description: "Issue #247 — 啟動自動探索爬蟲。Sirin 開瀏覽器到 seed_url，用 dom_snapshot 列舉可互動元件（button/link/input/tab），存到 SQLite discovered_features 表。\n\n用於 Coverage 3-tier funnel 的「探索」層 — 補 YAML coverage map 沒列到的功能。\n\n回傳 run_id（立即返回，crawl 在背景跑）。用 discovery_status 查狀態。",
        input_schema: json!({
            "type": "object",
            "required": ["seed_url"],
            "properties": {
                "seed_url":  { "type": "string", "description": "起始 URL（爬蟲第一個導覽到的頁面）" },
                "max_depth": { "type": "number", "description": "遞迴深度上限（iter 2 只支援 1，更深需後續 PR）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_discover_app),
    });

    m.insert("discovery_features", ToolDef {
        name:         "discovery_features",
        description: "列出 discovery 爬蟲找到的所有 features（route/label/kind/selector/last_seen）。可用 limit + kind 過濾。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "limit": { "type": "number", "description": "回傳上限，預設 100" },
                "kind":  { "type": "string", "description": "選填：只看某個 kind（button/link/form_input/tab/menuitem）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_discovery_features),
    });

    m.insert("scaffold_test", ToolDef {
        name:         "scaffold_test",
        description: "產生一份新的 AgoraMarket 測試 YAML 樣板（Issue #239）。\n\n根據 role（buyer / seller / delivery / admin）自動填入正確的 viewport、init 序列、role-pinning goto 跟 success_criteria placeholder。輸出可直接寫入 config/tests/agora_regression/<id>.yaml，確保跑 lint 全部通過（不會踩到常見的 5 個 trap）。\n\n回傳 yaml string + 建議的檔案路徑；不會自動寫檔（避免覆蓋）。",
        input_schema: json!({
            "type": "object",
            "required": ["role", "id"],
            "properties": {
                "role": { "type": "string", "enum": ["buyer", "seller", "delivery", "admin"], "description": "測試角色" },
                "id":   { "type": "string", "description": "test_id（同檔名，不含 .yaml），慣例：agora_<role>_<feature>" },
                "name": { "type": "string", "description": "可讀的測試名稱（預設用 placeholder）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_scaffold_test),
    });

    m.insert("lint_test", ToolDef {
        name:         "lint_test",
        description: "對單一 test_id 跑全部 5 條 YAML lint 規則 (Issue #239)，回傳所有 issues。\n\n規則：clear_state_reauth / h5_viewport_missing / double_enable_a11y / success_criteria_positive / iterations_ratio。\n\n所有規則都是 warn-level — 不會阻擋測試執行；用來定位常見的 5 個 trap。",
        input_schema: json!({
            "type": "object",
            "required": ["test_id"],
            "properties": {
                "test_id": { "type": "string", "description": "要 lint 的 test_id（YAML 檔名不含 .yaml）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_lint_test),
    });

    // Domain: multi-agent + dev_team + cross-session helpers (sync arg-taking).
    // 14-tool batch migrated 2026-05-04 — closes the entire sync agent /
    // dev_team / consult / supervised_run subset.  Only assistant_task
    // (AsyncJson) remains in this cluster.

    m.insert("agent_team_status", ToolDef {
        name:         "agent_team_status",
        description: "查看 PM / Engineer / Tester 三個 session 的目前狀態。回傳每個 session 的 session_id 和 resume 指令（可直接在 terminal 執行 `claude --resume <id>` 查看對話歷史）。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_agent_team_status),
    });

    m.insert("agent_team_task", ToolDef {
        name:         "agent_team_task",
        description: "派一個任務給 AI 團隊：PM 拆解 → Engineer 執行 → PM review。回傳 PM 的最終 review 結果。每個角色的對話歷史都會自動保留，可用 agent_team_status 取得 resume 指令查看。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "任務描述" },
                "cwd":  { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
            },
            "required": ["task"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_agent_team_task),
    });

    m.insert("agent_team_test", ToolDef {
        name:         "agent_team_test",
        description: "觸發測試循環：Tester 跑測試 → 失敗則 Engineer 修 → PM 記錄學習。回傳最終測試結果摘要。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_agent_team_test),
    });

    m.insert("agent_send", ToolDef {
        name:         "agent_send",
        description: "直接送一條訊息給指定角色（pm / engineer / tester），取得回覆。對話歷史自動延續。適合：查詢 PM 的學習記錄、問工程師問題、請測試 session 執行特定測試。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "role":    { "type": "string", "enum": ["pm", "engineer", "tester"], "description": "目標角色" },
                "message": { "type": "string", "description": "要送的訊息" },
                "cwd":     { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
            },
            "required": ["role", "message"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_agent_send),
    });

    m.insert("agent_reset", ToolDef {
        name:         "agent_reset",
        description: "重置指定角色的 session（清除對話歷史，開新對話）。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "role": { "type": "string", "enum": ["pm", "engineer", "tester", "all"], "description": "要重置的角色" },
                "cwd":  { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" }
            },
            "required": ["role"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_agent_reset),
    });

    m.insert("agent_enqueue", ToolDef {
        name:         "agent_enqueue",
        description: "把一個任務加入 AI 小隊的任務佇列。Worker 執行緒會自動依序執行（PM→Engineer→PM review）。回傳任務 ID，可用 agent_queue_status 查詢進度。\n\nT2-2：傳入 yaml_test_id 後，Engineer 完成任務時 Sirin 會自動執行該 YAML test 驗證，失敗則讓 Engineer 修 YAML 再試一次。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "task":         { "type": "string", "description": "任務描述（越具體越好）" },
                "cwd":          { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" },
                "priority":     { "type": "integer", "minimum": 0, "maximum": 255, "description": "任務優先級：0=緊急，50=正常（預設），255=最低" },
                "yaml_test_id": { "type": "string", "description": "（T2-2）Engineer 完成後，Sirin 自動跑這個 YAML test 驗證。傳 test_id（不含 .yaml）；系統從 Sirin repo 的 config/tests/ 遞迴搜尋，失敗則讓 Engineer 修 YAML 再試。" }
            },
            "required": ["task"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_agent_enqueue),
    });

    m.insert("agent_start_worker", ToolDef {
        name:         "agent_start_worker",
        description: "啟動 AI 小隊的背景工作執行緒（若尚未啟動）。啟動後持續消費佇列直到進程結束。可選 n 啟動多個平行 worker（T1-1）。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "description": "工作目錄（repo 路徑）。省略時用 sirin repo" },
                "n":   { "type": "integer", "description": "Worker 執行緒數量（預設 1，最大 8；建議 2-3）。每 worker 有獨立的 PM/Engineer/Tester session。", "minimum": 1, "maximum": 8 }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_agent_start_worker),
    });

    m.insert("squad_knowledge", ToolDef {
        name:         "squad_knowledge",
        description: "查看 Squad PM 積累的學習記錄（squad_knowledge.db）。列出 PM 在任務 review 中記錄的 [📝 學到:] 條目，這些知識會自動注入到下一個相關任務的規劃階段。可用於確認 PM 是否正確學到教訓、或除錯「為何 PM 一直犯同樣的錯」。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "最多回傳幾條（預設 20，最大 100）" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_squad_knowledge),
    });

    m.insert("dev_team_enqueue_issue", ToolDef {
        name:         "dev_team_enqueue_issue",
        description: "從 GitHub issue 直接餵任務給 Sirin Dev Team。讀取 issue 標題+內文+labels，包成 task 後放進佇列；任務完成後 system 會自動把 PM 的最終 review 貼回 issue 留言（除非 dry_run=true）。\n\n預設 dry_run=true（驗證模式）— PM/Engineer/Tester 會收到一段禁止 gh issue comment / git push 的系統提示，task 完成時 review 會存到 data/multi_agent/preview_comments.jsonl 而非貼到 GitHub。要真的跑（會留言/可能 push）就明確傳 dry_run=false。\n\nproject_key 例如 'agora_market' / 'sirin'，會決定 cwd（透過 claude_session::repo_path）；gh_repo 是 GitHub 的 owner/name 字串（例如 'Redandan/AgoraMarket'）。需要 gh CLI 已認證（`gh auth status` 通過）。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "project_key":  { "type": "string", "description": "邏輯專案名稱（決定 cwd 與 session 命名空間）。例：'agora_market', 'sirin', 'agora_api'" },
                "gh_repo":      { "type": "string", "description": "GitHub repo（owner/name 格式）。例：'Redandan/AgoraMarket'" },
                "issue_number": { "type": "integer", "minimum": 1, "description": "Issue 編號" },
                "dry_run":      { "type": "boolean", "description": "驗證模式（預設 true）。true=不會貼 GitHub 留言、不會 git push；false=正常跑會留言。", "default": true },
                "priority":     { "type": "integer", "minimum": 0, "maximum": 255, "description": "任務優先級（預設 50）" }
            },
            "required": ["project_key", "gh_repo", "issue_number"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_dev_team_enqueue_issue),
    });

    m.insert("dev_team_list_previews", ToolDef {
        name:         "dev_team_list_previews",
        description: "列出所有 dry-run 任務存下來的 preview comments（位於 data/multi_agent/preview_comments.jsonl）。每筆包含 task_id / issue_url / success / body / saved_at — 給人類 review 後決定要不要用 dev_team_replay_preview 真的貼到 GitHub。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "limit":     { "type": "integer", "minimum": 1, "maximum": 200, "description": "最多回傳幾筆（預設 20，由新到舊）" },
                "issue_url": { "type": "string", "description": "選填：只列出這個 issue 的 preview" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_dev_team_list_previews),
    });

    m.insert("dev_team_replay_preview", ToolDef {
        name:         "dev_team_replay_preview",
        description: "把指定 task_id 的 dry-run preview 真的貼到 GitHub issue 留言。用於人類 review 過 preview 內容、確認 OK 後手動觸發 — 等同於把 dry-run 模式跑出來的結果「approve + post」。留言會加 'replayed from dry-run' 標記，與一般 worker 自動貼的格式有所區別。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "要 replay 的 task_id（從 dev_team_list_previews 取得）" }
            },
            "required": ["task_id"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_dev_team_replay_preview),
    });

    m.insert("dev_team_read_issue", ToolDef {
        name:         "dev_team_read_issue",
        description: "用 gh CLI 讀單一 issue 的 title / body / labels（不會把它放進 task 佇列）。適合：先看內容判斷要不要餵給 dev team、或除錯 enqueue 失敗時拿原始資料。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "gh_repo":      { "type": "string", "description": "GitHub repo（owner/name）" },
                "issue_number": { "type": "integer", "minimum": 1 }
            },
            "required": ["gh_repo", "issue_number"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_dev_team_read_issue),
    });

    m.insert("consult", ToolDef {
        name:         "consult",
        description: "把一個問題交給另一個 Claude Code session 回答。Sirin 會在指定工作目錄（可以是另一個 repo）啟動一個顧問 session，讓它讀取程式碼後給出簡潔可執行的建議，再把答案帶回來。適合：「這個 API 格式對嗎？」「後端怎麼實作這個？」等需要跨 repo 判斷的問題。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "question":   { "type": "string", "description": "要問顧問的問題" },
                "context":    { "type": "string", "description": "背景說明（目前在做什麼）" },
                "cwd":        { "type": "string", "description": "顧問 session 的工作目錄（repo 路徑）。省略時用 sirin 自身目錄" }
            },
            "required": ["question"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_consult),
    });

    // ── First AsyncJson migration (probe — verifies the variant works
    //    before bulk-migrating the rest of the async cluster) ──

    m.insert("kb_stats", ToolDef {
        name:         "kb_stats",
        description: "#226 — KB 深度統計（補強 kbHealth）。透過 agora-trading MCP 取得指定 project 的條目分布：by domain / by status / by layer / by tag（若上游提供）/ draft ratio / stale ratio。\n\n另外包含 #219 的 brief 長度治理 breakdown：可檢查指定 topicKey 的字數是否超過上限（預設 3000）。\n\n需要 agora-trading + agora-ops token 在 ~/.claude.json 中設定。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "project": { "type": "string", "description": "project slug：sirin / agora-backend / flutter，預設 sirin" },
                "brief_limit": { "type": "integer", "description": "brief 長度上限（預設 3000）" },
                "brief_topics": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "要檢查字數上限的 topicKey 清單；省略時 sirin 預設檢查 sirin-overview 與 sirin-playbook-lhi-marathon"
                }
            }
        }),
        // AsyncJson takes `fn(Value) -> Pin<Box<dyn Future<...> + Send + 'static>>`.
        // The handler `async fn call_X(args) -> Result<Value, String>` returns
        // an opaque future; wrap with Box::pin in a non-capturing closure to
        // coerce to the fn pointer the variant requires.
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_kb_stats(args))),
    });

    m.insert("supervised_run", ToolDef {
        name:         "supervised_run",
        description: "在指定 repo 啟動一個受監督的 Claude Code session。當主 session 停下來（問問題 / 達到輪次上限），Sirin 自動決定怎麼回應：\n- policy=auto：直接回「yes, continue」\n- policy=consult：把問題轉給另一個 Claude session 取得建議再回答\n最多執行 5 輪，全部完成後回傳結果摘要（含每輪事件）。\n注意：可能需要 1-5 分鐘，視任務複雜度而定。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "cwd":            { "type": "string", "description": "主 session 工作目錄（repo 路徑）" },
                "prompt":         { "type": "string", "description": "給 Claude Code 的任務描述" },
                "policy":         { "type": "string", "enum": ["auto", "consult"], "description": "auto=直接回 yes；consult=問另一個 session（預設 auto）" },
                "consultant_cwd": { "type": "string", "description": "顧問 session 的工作目錄，policy=consult 時有效（省略則與 cwd 相同）" }
            },
            "required": ["cwd", "prompt"]
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_supervised_run),
    });

    // ── AsyncJson cluster — full async subset (16 tools).  Pattern proven
    //    by the kb_stats probe earlier in this file: a non-capturing closure
    //    that boxes the future coerces to the fn-pointer the variant requires.
    //    16-tool batch migrated 2026-05-04 — closes #257 to ~95%.

    // #226 kb-lifecycle (async subset)
    m.insert("kb_diff", ToolDef {
        name:         "kb_diff",
        description: "#226 — 對比兩個 KB 條目的內容差異（行級 unified diff）。適合追蹤同一 topicKey 在不同版本的演進，或對比兩個相關條目的內容重疊度。\n\n需要 agora-trading token 在 ~/.claude.json 中設定。",
        input_schema: json!({
            "type": "object",
            "required": ["topic_a", "topic_b"],
            "properties": {
                "topic_a":  { "type": "string", "description": "第一個 topicKey" },
                "topic_b":  { "type": "string", "description": "第二個 topicKey" },
                "project_a": { "type": "string", "description": "topic_a 的 project，預設 sirin" },
                "project_b": { "type": "string", "description": "topic_b 的 project，預設同 project_a" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_kb_diff(args))),
    });

    m.insert("kb_duplicate_check", ToolDef {
        name:         "kb_duplicate_check",
        description: "#226 — 找出 KB 中內容高度重疊的條目（Jaccard 文字相似度），不需要 Chroma embedding。\n\n`threshold`：0.0-1.0，預設 0.7（70% 重疊才算重複）。\n`topic_keys`：逗號分隔，指定要比較的 topicKey 清單；不指定則比較所有傳入的候選集。\n`project`：預設 sirin。\n\n回傳：相似對（pair_a, pair_b, jaccard_score）清單，score 高 → 越相似。",
        input_schema: json!({
            "type": "object",
            "required": ["topic_keys"],
            "properties": {
                "topic_keys": { "type": "string", "description": "逗號分隔的 topicKey 清單（至少 2 個）" },
                "project":   { "type": "string", "description": "project slug，預設 sirin" },
                "threshold": { "type": "number", "description": "Jaccard 相似度閾值，預設 0.7" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_kb_duplicate_check(args))),
    });

    m.insert("kb_merge", ToolDef {
        name:         "kb_merge",
        description: "#226 — 合併多個 KB 條目到一個目標 key。\n\n`strategy`：\n- concat：直接合併所有 src 內容（預設）\n- llm：呼叫 LLM 智慧整合，去重複，保留精華\n合併後將 src 條目標為 stale。",
        input_schema: json!({
            "type": "object",
            "required": ["src_keys", "dst_key"],
            "properties": {
                "src_keys":  { "type": "string",  "description": "逗號分隔的來源 topicKey 列表" },
                "dst_key":   { "type": "string",  "description": "目標 topicKey（若已存在則追加）" },
                "project":   { "type": "string",  "description": "project slug，預設 sirin" },
                "strategy":  { "type": "string",  "description": "concat（預設）| llm" },
                "dry_run":   { "type": "boolean", "description": "true=只預覽不寫入，預設 false" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_kb_merge(args))),
    });

    // #231 agora-daily-brief
    m.insert("generate_daily_brief", ToolDef {
        name:         "generate_daily_brief",
        description: "#231 — 自動聚合 agora-trading 多個工具生成每日 ops 摘要（markdown）。\n\n一次呼叫取代手動跑 getMarketSnapshot + getOpenPositions + getShadowSignalStats 等。\n`sections`：逗號分隔，預設 market,portfolio,ml,ops。\n結果同時寫入 KB（topicKey=agora-daily-brief-YYYYMMDD）供下次 session 參考。\n\n需要 agora-trading + agora-ops token 設定。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "sections": { "type": "string", "description": "逗號分隔：market,portfolio,ml,ops（預設全部）" },
                "date":     { "type": "string", "description": "YYYY-MM-DD，預設今天" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_generate_daily_brief(args))),
    });

    // #233 cross-ai-router (async subset)
    m.insert("route_query", ToolDef {
        name:         "route_query",
        description: "#233 — 依 intent registry 自動選擇 LLM 並呼叫。\n\n查 ~/.claude/llm_intents.json 找到 intent 對應的 backend+model，呼叫並回傳結果。\nintent 例：indicator-design(→deepseek), code-review(→claude), vision(→gemini)。\n找不到 intent → 使用 primary LLM（Gemini/Claude）。",
        input_schema: json!({
            "type": "object",
            "required": ["intent", "prompt"],
            "properties": {
                "intent": { "type": "string", "description": "意圖名稱，例如 indicator-design / code-review / translate-zh" },
                "prompt": { "type": "string", "description": "要發送給 LLM 的 prompt" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_route_query(args))),
    });

    m.insert("query_llm", ToolDef {
        name:         "query_llm",
        description: "#233 — 直接呼叫指定 LLM backend（跳過 intent routing）。\n\n`backend`: gemini / deepseek / claude / ollama。\n`model`: 選填，不指定則用 backend 預設值或 .env 設定。\n`api_key`: 選填，不指定則從 .env 讀取。",
        input_schema: json!({
            "type": "object",
            "required": ["backend", "prompt"],
            "properties": {
                "backend": { "type": "string", "description": "gemini / deepseek / claude / ollama" },
                "model":   { "type": "string", "description": "模型名稱（選填）" },
                "api_key": { "type": "string", "description": "API key（選填，不填從 .env 讀）" },
                "prompt":  { "type": "string", "description": "prompt 內容" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_query_llm(args))),
    });

    m.insert("fallback_chain", ToolDef {
        name:         "fallback_chain",
        description: "#233 — 依序嘗試 LLM 列表，第一個成功的回傳結果。\n\n`backends`：逗號分隔，例如 \"gemini,deepseek,claude\"。\n任一 backend 失敗（429/error）自動切下一個，< 1s latency。",
        input_schema: json!({
            "type": "object",
            "required": ["prompt", "backends"],
            "properties": {
                "prompt":   { "type": "string", "description": "prompt 內容" },
                "backends": { "type": "string", "description": "逗號分隔的 backend 列表，例如 gemini,deepseek,claude" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_fallback_chain(args))),
    });

    m.insert("benchmark_llms", ToolDef {
        name:         "benchmark_llms",
        description: "#233 — 同一 prompt 同時發給多個 LLM，比較回應速度和內容。\n\n`backends`：逗號分隔，預設 \"gemini,deepseek\"。結果含每個 backend 的耗時 ms 和前 200 字回應。",
        input_schema: json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt":   { "type": "string", "description": "benchmark 用的 prompt" },
                "backends": { "type": "string", "description": "逗號分隔，預設 gemini,deepseek" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_benchmark_llms(args))),
    });

    m.insert("benchmark_test_override", ToolDef {
        name:         "benchmark_test_override",
        description: "#275 — benchmark per-test llm_override latency without running full browser E2E.\n\nProvide test_ids + provider/model; Sirin builds short prompts from each test goal and measures LLM round-trip latency (avg/p95). Useful to compare candidate small models before committing to full run_test_async matrices.",
        input_schema: json!({
            "type": "object",
            "required": ["test_ids", "provider", "model"],
            "properties": {
                "test_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "YAML test ids to benchmark against prompt context"
                },
                "provider": {
                    "type": "string",
                    "description": "lmstudio | gemini | anthropic | ollama"
                },
                "model": {
                    "type": "string",
                    "description": "Model id at provider"
                },
                "base_url": {
                    "type": "string",
                    "description": "Optional override base URL"
                },
                "api_key": {
                    "type": "string",
                    "description": "Optional override API key"
                },
                "runs_per_test": {
                    "type": "integer",
                    "description": "How many latency samples per test (default 3, clamp 1..5)"
                }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_benchmark_test_override(args))),
    });

    m.insert("benchmark_test_override_matrix", ToolDef {
        name:         "benchmark_test_override_matrix",
        description: "#275 — one-shot matrix benchmark: compare baseline vs candidate llm_override across test_ids.\n\nReturns per-test side-by-side metrics + computed speedup/pass-rate-drop + recommendation (adopt candidate / keep baseline / per-test mix).",
        input_schema: json!({
            "type": "object",
            "required": ["test_ids", "baseline", "candidate"],
            "properties": {
                "test_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "YAML test ids to benchmark"
                },
                "baseline": {
                    "type": "object",
                    "required": ["provider", "model"],
                    "properties": {
                        "provider": { "type": "string" },
                        "model": { "type": "string" },
                        "base_url": { "type": "string" },
                        "api_key": { "type": "string" }
                    }
                },
                "candidate": {
                    "type": "object",
                    "required": ["provider", "model"],
                    "properties": {
                        "provider": { "type": "string" },
                        "model": { "type": "string" },
                        "base_url": { "type": "string" },
                        "api_key": { "type": "string" }
                    }
                },
                "runs_per_test": {
                    "type": "integer",
                    "description": "Latency samples per test (default 3, clamp 1..5)"
                },
                "min_speedup_pct": {
                    "type": "integer",
                    "description": "Adoption threshold for average speedup (default 30)"
                },
                "max_pass_rate_drop_pct": {
                    "type": "integer",
                    "description": "Reject candidate when worst pass-rate drop exceeds this (default 10)"
                }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_benchmark_test_override_matrix(args))),
    });

    // Triage / browser snapshots / regression suite
    m.insert("explain_failure", ToolDef {
        name:         "explain_failure",
        description: "用 LLM 解釋某次測試失敗的根因。整合截圖分析、console errors、歷史步驟、AI analysis，產生人類可讀的診斷報告。\n\n適合 debug 時快速理解失敗原因，不需要手動翻 history_json。",
        input_schema: json!({
            "type": "object",
            "required": ["run_id"],
            "properties": {
                "run_id": { "type": "string", "description": "要解釋的測試 run_id" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_explain_failure(args))),
    });

    m.insert("page_state", ToolDef {
        name:         "page_state",
        description: "一次回傳當前瀏覽器頁面的完整狀態 — URL、title、ax_tree 文字片段、JPEG 截圖（Base64）、console 錯誤、最近網路請求。比分別呼叫多個 browser_exec 動作更快，適合 AI agent 做 situational awareness。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "include_screenshot": { "type": "boolean", "description": "是否包含截圖（預設 true）。截圖為 JPEG 80% Base64" },
                "include_ax":         { "type": "boolean", "description": "是否包含 ax_tree 文字摘要（預設 true）" },
                "max_ax_nodes":       { "type": "number",  "description": "ax_tree 最多返回幾個節點（預設 50）" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_page_state(args))),
    });

    m.insert("run_regression_suite", ToolDef {
        name:         "run_regression_suite",
        description: "一鍵執行 config/tests/agora_regression/ 下所有（或指定 tag 的）regression tests，等全部完成後回傳摘要報告。\n\n返回：total/passed/failed/timeout/duration_secs + 每個測試的 status/duration_ms/error。\n\n可選參數：\n- tag: 只跑含此 tag 的測試（如 'c2c'）\n- timeout_secs: 整體 timeout（預設 3600）\n\n關閉 #188。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "tag": { "type": "string", "description": "只跑含此 tag 的測試，空字串=全部" },
                "timeout_secs": { "type": "number", "description": "整體 timeout 秒數（預設 3600）" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_run_regression_suite(args))),
    });

    m.insert("sirin_preflight", ToolDef {
        name:         "sirin_preflight",
        description: "Session 開始前驗證環境就緒：Sirin MCP、config sync 狀態、redandan.github.io 版本、API healthy。\n\n返回：ready=true/false + 各項檢查結果 + warnings 清單。\n\n關閉 #193。",
        input_schema: json!({"type": "object", "properties": {}}),
        // Zero-arg async — wrap with `_args` placeholder closure, drop the args.
        handler:      ToolHandler::AsyncJson(|_args| Box::pin(crate::mcp_server::call_sirin_preflight())),
    });

    // Vision-driven assistant (the big one — uses screenshot_analyze internally)
    m.insert("assistant_task", ToolDef {
        name:         "assistant_task",
        description: "用自然語言請求執行一般網頁任務（非 Flutter 測試）。Sirin 透過視覺驅動的 ReAct loop 操作瀏覽器完成任務，回傳結果摘要。\n適用場景：\n- 在 Google Maps 找附近餐廳並過濾評分\n- 在 Facebook 查詢修車廠聯絡資訊\n- 翻譯外語頁面內容（支援泰文等）\n- 在任意網頁填表、搜尋、提取資料\n注意：使用 Sirin 的 Chrome session（需先啟動 Sirin），最多執行 25 步。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "request": { "type": "string", "description": "自然語言任務描述，例如：在 Google Maps 找附近評分 4 星以上的泰式餐廳" },
                "url":     { "type": "string", "description": "選填：起始 URL，例如 https://maps.google.com" }
            },
            "required": ["request"]
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_assistant_task(args))),
    });

    // KB direct access (Sirin's own search/get/write to the AgoraMarket KB)
    m.insert("kb_search", ToolDef {
        name:         "kb_search",
        description: "語意搜尋 AgoraMarket Knowledge Base，返回最相關的條目內容。KB 必須啟用（KB_ENABLED=1）。Bearer token 留在 Sirin 端，瀏覽器永遠看不到。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "query":   { "type": "string",  "description": "搜尋查詢文字" },
                "project": { "type": "string",  "description": "KB project slug（預設讀 KB_PROJECT env，通常是 'agora_market'）" },
                "limit":   { "type": "integer", "description": "最多返回幾筆（預設 5，最大 20）" }
            },
            "required": ["query"]
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_kb_search(args))),
    });

    m.insert("kb_get", ToolDef {
        name:         "kb_get",
        description: "依 topicKey 取得 Knowledge Base 單一條目。KB 必須啟用（KB_ENABLED=1）。Bearer token 留在 Sirin 端。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "topic_key": { "type": "string", "description": "KB topicKey（例如 'agora-pickup-flow'）" },
                "project":   { "type": "string", "description": "KB project slug（預設讀 KB_PROJECT env）" }
            },
            "required": ["topic_key"]
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_kb_get(args))),
    });

    m.insert("kb_write", ToolDef {
        name:         "kb_write",
        description: "Write a note to AgoraMarket Knowledge Base. Stored as layer=raw, status=draft, source=sirin. KB must be enabled (KB_ENABLED=1).",
        input_schema: json!({
            "type": "object",
            "properties": {
                "topic_key": { "type": "string", "description": "unique kebab-case key, e.g. 'cic-result-demo-001'" },
                "title":     { "type": "string", "description": "human-readable title" },
                "content":   { "type": "string", "description": "note body (Markdown OK)" },
                "domain":    { "type": "string", "description": "e.g. 'tooling', 'cic', 'session-task'" },
                "tags":      { "type": "string", "description": "comma-separated, e.g. 'cic-task,result'" },
                "file_refs": { "type": "string", "description": "optional comma-separated file refs" },
                "project":   { "type": "string", "description": "KB project slug (default reads KB_PROJECT env)" }
            },
            "required": ["topic_key", "title", "content", "domain"]
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_kb_write(args))),
    });

    // ── Final cluster — text-envelope tools (3 tools).  These return a
    //    `Result<String, String>` instead of `Result<Value, String>`, which
    //    is why they sat in a separate `let text = match name { ... }` block
    //    in mcp_server.rs.  The registry's SyncText / AsyncText variants
    //    handle the `{"content":[{"type":"text","text":...}]}` envelope
    //    automatically, so migration is the same fn-pointer pattern.
    //
    //    With this batch, every JSON tool except browser_exec is in the
    //    registry.  browser_exec stays legacy permanently — it's the sole
    //    handler that needs `user_agent` for authz, and the ToolHandler
    //    enum doesn't carry that channel.

    m.insert("memory_search", ToolDef {
        name:         "memory_search",
        description: "搜尋 Sirin 的記憶庫與對話歷史。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "搜尋關鍵字" },
                "limit": { "type": "number", "description": "最多返回幾筆（預設 5）" }
            },
            "required": ["query"]
        }),
        // First AsyncText migration — same Box::pin pattern as AsyncJson, just
        // wraps a `Result<String, String>` future instead of `Result<Value, ..>`.
        handler:      ToolHandler::AsyncText(|args| Box::pin(crate::mcp_server::call_memory_search(args))),
    });

    m.insert("teams_approve", ToolDef {
        name:         "teams_approve",
        description: "核准指定的 Teams 草稿，觸發送出。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "PendingReply ID" }
            },
            "required": ["id"]
        }),
        handler:      ToolHandler::SyncText(crate::mcp_server::call_teams_approve),
    });

    m.insert("trigger_research", ToolDef {
        name:         "trigger_research",
        description: "觸發 Sirin 對指定主題進行調研。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "調研主題" },
                "url":   { "type": "string", "description": "參考 URL（選填）" }
            },
            "required": ["topic"]
        }),
        handler:      ToolHandler::SyncText(crate::mcp_server::call_trigger_research),
    });

    m.insert("research_sentinel_goal_create", ToolDef {
        name:         "research_sentinel_goal_create",
        description: "建立 Codex 定義的新聞/事件研究目標。Sirin 只做外部研究與本地 inbox 保存；不讀 AgoraMarketAPI 交易狀態、不下單、不改 OCO。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "研究主題，例如 BTCUSDT automated trading risk news" },
                "focus": { "type": "string", "description": "研究焦點，例如 volatility/liquidity/exchange/regulation/macro risk" },
                "keywords": { "type": "array", "items": { "type": "string" }, "description": "搜尋關鍵字" },
                "lookback_hours": { "type": "integer", "description": "回看小時，預設 24，範圍 1..168" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::research_sentinel::call_research_sentinel_goal_create),
    });

    m.insert("research_sentinel_run_once", ToolDef {
        name:         "research_sentinel_run_once",
        description: "依 Codex 定義的 goal 或 inline topic 執行一次新聞/事件研究，整理來源 URL、事件類型、資產、relevance、confidence，並保存到 Codex 可讀 inbox。研究結果不是買賣建議。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "goal_id": { "type": "string", "description": "既有 research_sentinel goal id；未提供時使用 inline topic/focus/keywords" },
                "topic": { "type": "string", "description": "inline 研究主題" },
                "focus": { "type": "string", "description": "inline 研究焦點" },
                "keywords": { "type": "array", "items": { "type": "string" }, "description": "inline 搜尋關鍵字" },
                "lookback_hours": { "type": "integer", "description": "回看小時，預設 goal 值或 24，範圍 1..168" },
                "rss_feeds": { "type": "array", "items": { "type": "string" }, "description": "RSS fallback feeds；預設 CoinDesk + Cointelegraph" },
                "max_queries": { "type": "integer", "description": "最多搜尋 query 數，預設 4，範圍 1..8" },
                "max_results": { "type": "integer", "description": "最多保留結果，預設 12，範圍 1..30" },
                "create_inbox": { "type": "boolean", "description": "是否寫入 inbox，預設 true" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::research_sentinel::call_research_sentinel_run_once(args))),
    });

    m.insert("research_sentinel_inbox", ToolDef {
        name:         "research_sentinel_inbox",
        description: "列出 Sirin 本地 News Research Sentinel inbox，供 Codex 驗收研究結果。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "unread_only": { "type": "boolean", "description": "只列 unread，預設 true" },
                "limit": { "type": "integer", "description": "最多筆數，預設 20，範圍 1..100" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::research_sentinel::call_research_sentinel_inbox),
    });

    m.insert("research_sentinel_ack", ToolDef {
        name:         "research_sentinel_ack",
        description: "將指定 News Research Sentinel inbox item 標記為 read。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "inbox item id" }
            },
            "required": ["id"]
        }),
        handler:      ToolHandler::SyncJson(crate::research_sentinel::call_research_sentinel_ack),
    });

    m.insert("research_sentinel_review", ToolDef {
        name:         "research_sentinel_review",
        description: "回寫 Codex 對 News Research Sentinel inbox item 的驗收結果，形成研究閉環。狀態可為 accepted、ignored、needs_more_sources、convert_to_issue。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "inbox item id" },
                "review_status": {
                    "type": "string",
                    "enum": ["accepted", "ignored", "needs_more_sources", "convert_to_issue"],
                    "description": "Codex 驗收狀態"
                },
                "review_note": { "type": "string", "description": "Codex 驗收備註" }
            },
            "required": ["id", "review_status"]
        }),
        handler:      ToolHandler::SyncJson(crate::research_sentinel::call_research_sentinel_review),
    });
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_seeded_tools() {
        // Seeded tools must be in the registry.
        assert!(get("skill_list").is_some());
        assert!(get("teams_pending").is_some());
        assert!(get("discovery_status").is_some());
        assert!(get("benchmark_test_override").is_some());
        assert!(get("benchmark_test_override_matrix").is_some());
        assert!(get("research_sentinel_goal_create").is_some());
        assert!(get("research_sentinel_run_once").is_some());
        assert!(get("research_sentinel_inbox").is_some());
        assert!(get("research_sentinel_ack").is_some());
        assert!(get("research_sentinel_review").is_some());
        assert!(get("codex_supervisor_snapshot").is_some());
        assert!(get("codex_supervisor_report").is_some());
        assert!(get("codex_supervisor_claim").is_some());
        assert!(get("codex_supervisor_complete_action").is_some());

        // Un-migrated tools must NOT be in the registry yet.  Only one tool
        // is permanently un-migrated: `browser_exec`.  It's the sole authz
        // exception (needs user_agent for audit), pinned to the legacy
        // dispatch path forever per CLAUDE.md.  Asserting it stays out is a
        // structural guarantee — won't go stale on future refactors.
        //
        // We also assert a non-existent name to verify `get` returns None
        // (rather than e.g. always returning a default).
        assert!(get("browser_exec").is_none());
        assert!(get("definitely_not_a_real_tool").is_none());
    }

    #[test]
    fn codex_supervisor_tools_are_seeded_with_narrow_safety_annotations() {
        let snapshot = get("codex_supervisor_snapshot")
            .expect("seeded supervisor snapshot")
            .to_json();
        assert_eq!(snapshot["annotations"]["readOnlyHint"], true);
        assert_eq!(snapshot["annotations"]["openWorldHint"], false);

        let report = get("codex_supervisor_report")
            .expect("seeded supervisor report")
            .to_json();
        assert_eq!(report["annotations"]["idempotentHint"], true);

        for name in ["codex_supervisor_claim", "codex_supervisor_complete_action"] {
            let tool = get(name).expect("seeded supervisor mutation tool").to_json();
            assert_eq!(tool["annotations"]["readOnlyHint"], false);
            assert_eq!(tool["annotations"]["destructiveHint"], false);
            assert_eq!(tool["annotations"]["idempotentHint"], false);
            assert_eq!(tool["annotations"]["openWorldHint"], false);
        }
    }

    #[test]
    fn tool_def_renders_to_expected_json_shape() {
        let def = get("skill_list").expect("seeded");
        let json = def.to_json();
        assert_eq!(json["name"],        "skill_list");
        assert_eq!(json["description"], "列出 Sirin 所有可用技能（含 YAML 動態技能）。");
        assert!(json["inputSchema"].is_object());
    }

    #[test]
    fn iter_returns_alphabetised_by_name() {
        // BTreeMap preserves insertion-key order — our seeded entries
        // are skill_list / teams_pending / discovery_status in source
        // order, but iter() returns them sorted: discovery_status comes
        // first alphabetically.
        let names: Vec<&str> = iter().map(|d| d.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "iter() must be alphabetical");
    }

    #[tokio::test]
    async fn sync_text_handler_round_trip_through_envelope() {
        let def = get("skill_list").expect("seeded");
        let envelope = def.handler.invoke(json!({})).await.expect("call");
        // The envelope must be `{"content":[{"type":"text","text":...}]}`.
        assert_eq!(envelope["content"][0]["type"], "text");
        assert!(envelope["content"][0]["text"].is_string());
    }

    #[test]
    fn wrap_json_helper_produces_text_envelope_with_pretty_payload() {
        let payload = json!({"k": "v"});
        let env = wrap_json(payload);
        let text = env["content"][0]["text"].as_str().unwrap();
        // Pretty printer adds a newline after the opening brace.
        assert!(text.contains("\"k\": \"v\""));
    }
}
