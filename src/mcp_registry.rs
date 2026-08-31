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
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema for the tool's `arguments` payload.  Lives as a
    /// `Value` (not a struct) so we don't have to standardise the
    /// schema shape across all 92 tools — each registration can pass
    /// whatever shape the existing literal had.
    pub input_schema: Value,
    pub handler: ToolHandler,
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

        // Keep safety declarations explicit and narrow. Observation-only
        // tools are safe for approval=never clients. KB writes are mutations,
        // but topicKey upsert is idempotent and the Sirin implementation is
        // restricted to a fixed server-local kbWrite path.
        match self.name {
            "codex_supervisor_snapshot" => {
                definition["annotations"] = json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                });
            }
            "codex_supervisor_report" | "codex_supervisor_complete_action" => {
                definition["annotations"] = json!({
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                });
            }
            "codex_supervisor_claim" => {
                definition["annotations"] = json!({
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "idempotentHint": false,
                    "openWorldHint": false
                });
            }
            "ai_monitor_speed_test" => {
                definition["annotations"] = json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": false,
                    "openWorldHint": true
                });
            }
            "ios_device_status" | "ios_control_session_status" => {
                definition["annotations"] = json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                });
            }
            "kb_get" | "kb_search" | "kb_stats" => {
                definition["annotations"] = json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": true
                });
            }
            "kb_write" => {
                definition["annotations"] = json!({
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": true
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
            Self::SyncJson(f) => f(args).map(wrap_json),
            Self::AsyncJson(f) => f(args).await.map(wrap_json),
            Self::SyncText(f) => f(args).map(wrap_text),
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
        description: "讀取 Sirin 本機 Codex 工作督導快照。顯示證據分類、驗收面缺口、掃描計數與可安全跳過的未變更終態 task key；不讀 prompt/回覆全文、不傳送訊息、不授權、不 fork。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler: ToolHandler::SyncJson(crate::codex_supervisor::call_snapshot),
    });

    m.insert("codex_supervisor_report", ToolDef {
        name: "codex_supervisor_report",
        description: "由 Codex heartbeat 回報最小化結構證據。一般 threadId 用於任務分類；保留 threadId=sirin-supervisor-scan 用於每輪掃描 freshness，成功且無候選時顯示 HEALTHY_IDLE。只接受不含對話原文的 opaque key；不傳送任務訊息。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "threadId": {"type": "string", "description": "Codex task/thread id；掃描心跳固定使用 sirin-supervisor-scan"},
                "hostId": {"type": "string", "description": "可選：Codex host id"},
                "listingCursorKey": {"type": "string", "description": "可選：由 list_threads updatedAt/cursor 產生的 opaque key，用於跳過未變更終態 task"},
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
                },
                "scanMetrics": {
                    "type": "object",
                    "description": "只供 sirin-supervisor-scan 使用的 count-only telemetry",
                    "properties": {
                        "listedCount": {"type": "integer", "minimum": 0, "maximum": 500},
                        "strongCandidateCount": {"type": "integer", "minimum": 0, "maximum": 500},
                        "compactProbeCount": {"type": "integer", "minimum": 0, "maximum": 500},
                        "deepReadCount": {"type": "integer", "minimum": 0, "maximum": 2},
                        "skippedUnchangedCount": {"type": "integer", "minimum": 0, "maximum": 500}
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

    m.insert("ai_monitor_speed_test", ToolDef {
        name: "ai_monitor_speed_test",
        description: "AI Work Monitor 手動下載測速。只有明確呼叫時才從 Cloudflare speed endpoint 下載 5 MB；不會背景執行、不改路由、不讀憑證。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler: ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_ai_monitor_speed_test(args))),
    });

    // Domain: AI-facing discovery — first thing other AIs see when they
    // call `tools/list`, so the description spells out what Sirin is + the
    // help URL.  Returning the URL inline lets curl-only callers skip the
    // browser entirely.
    m.insert("help", ToolDef {
        name:         "help",
        description: "📖 What is Sirin? — AI-driven browser test daemon. Returns the full tools/list count, optional discovery profiles, connection methods, tool categories, and the help URL (http://127.0.0.1:7700/help). Other AIs (Claude / Cline / Cursor / curl) should call this FIRST. No args required.",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_help),
    });

    // Domain: skills + teams (text-mode handlers).
    m.insert(
        "skill_list",
        ToolDef {
            name: "skill_list",
            description: "列出 Sirin 所有可用技能（含 YAML 動態技能）。",
            input_schema: json!({"type": "object", "properties": {}}),
            // Wrap the zero-arg handler into a `fn(Value) -> Result<String, String>`
            // shape via a non-capturing closure.  Closures without captures coerce
            // to fn pointers, which is what the enum variant requires.
            handler: ToolHandler::SyncText(|_args| Ok(crate::mcp_server::call_skill_list())),
        },
    );

    m.insert(
        "teams_pending",
        ToolDef {
            name: "teams_pending",
            description: "取得 Teams 待確認回覆草稿列表。",
            input_schema: json!({"type": "object", "properties": {}}),
            handler: ToolHandler::SyncText(|_args| Ok(crate::mcp_server::call_teams_pending())),
        },
    );

    // Domain: discovery (structured-JSON handler with no args).
    m.insert(
        "discovery_status",
        ToolDef {
            name: "discovery_status",
            description: "查詢 discovery crawler 的目前狀態（NotRun / Crawling / Done + 元數據）。",
            input_schema: json!({"type": "object", "properties": {}}),
            handler: ToolHandler::SyncJson(|_args| crate::mcp_server::call_discovery_status()),
        },
    );

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

    m.insert(
        "list_hooks",
        ToolDef {
            name: "list_hooks",
            description: "#225 — 列出 ~/.claude/settings.json hooks 設定（event → command 清單）。",
            input_schema: json!({"type": "object", "properties": {}}),
            handler: ToolHandler::SyncJson(|_args| crate::mcp_server::call_list_hooks()),
        },
    );

    m.insert(
        "list_intents",
        ToolDef {
            name: "list_intents",
            description: "#233 — 列出 ~/.claude/llm_intents.json 中的所有 intent → LLM 路由規則。",
            input_schema: json!({"type": "object", "properties": {}}),
            handler: ToolHandler::SyncJson(|_args| crate::mcp_server::call_list_intents()),
        },
    );

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
        description: "將 repo 的 config/tests/ 同步到 %LOCALAPPDATA%\\Sirin\\config\\tests/（Sirin 執行時讀取的位置）。\n\n每次修改 YAML 測試檔後必須呼叫，否則 Sirin 跑的是舊版 YAML。若傳 prune_stale_tests=true，會刪除 LOCALAPPDATA 裡 repo 已不存在的 config/tests/**/*.yaml，避免舊測試污染 list_tests/test_summary。\n\n返回：synced=true, files_copied=N, stale_tests_removed=[...]。\n\n關閉 #187。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "prune_stale_tests": {
                    "type": "boolean",
                    "description": "預設 false；true 時刪除本機 config/tests 內 repo 已不存在的 YAML"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_sync_config),
    });

    m.insert(
        "agent_queue_status",
        ToolDef {
            name: "agent_queue_status",
            description: "查看 AI 小隊任務佇列現況（所有任務的 id / status / 結果摘要）。",
            input_schema: json!({"type": "object", "properties": {}}),
            handler: ToolHandler::SyncJson(|_args| crate::mcp_server::call_agent_queue_status()),
        },
    );

    m.insert(
        "agent_clear_completed",
        ToolDef {
            name: "agent_clear_completed",
            description: "清除任務佇列中所有已完成（done / failed）的任務，保留 queued / running。",
            input_schema: json!({"type": "object", "properties": {}}),
            handler: ToolHandler::SyncJson(|_args| crate::mcp_server::call_agent_clear_completed()),
        },
    );

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

    m.insert(
        "restore_point",
        ToolDef {
            name: "restore_point",
            description: "#227 — 讀取指定 save point 的 summary，用於恢復上下文。",
            input_schema: json!({
                "type": "object",
                "required": ["label"],
                "properties": {
                    "label": { "type": "string", "description": "要恢復的 save point label" }
                }
            }),
            handler: ToolHandler::SyncJson(crate::mcp_server::call_restore_point),
        },
    );

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

    m.insert(
        "link_task",
        ToolDef {
            name: "link_task",
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
            handler: ToolHandler::SyncJson(crate::mcp_server::call_link_task),
        },
    );

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
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::mcp_server::call_create_handoff(args))),
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

    m.insert(
        "list_handoff_history",
        ToolDef {
            name: "list_handoff_history",
            description: "#224 — 列出最近 N 筆 handoff 記錄（最新在前）。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "project slug，預設 sirin" },
                    "limit":   { "type": "number",  "description": "筆數上限，預設 10" }
                }
            }),
            handler: ToolHandler::SyncJson(crate::mcp_server::call_list_handoff_history),
        },
    );

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

    m.insert(
        "list_tests",
        ToolDef {
            name: "list_tests",
            description: "列出 config/tests/ 目錄下 YAML 測試 goal。預設列全部；可用 suite=active-smoke / deep-regression / obsolete / all，或 tags/include_tags/exclude_tags 篩選，讓外部 AI 排程前避開 mutating / requires-operator-approval 測試。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "suite": { "type": "string", "description": "選填：active-smoke | deep-regression | obsolete | all；不傳則列全部" },
                    "tag": { "type": "string", "description": "選填：單一 tag filter" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "選填：任一 tag 命中即納入" },
                    "include_tags": { "type": "array", "items": { "type": "string" }, "description": "選填：任一 tag 命中即納入" },
                    "exclude_tags": { "type": "array", "items": { "type": "string" }, "description": "選填：排除任一 tag 命中的測試" }
                }
            }),
            handler: ToolHandler::SyncJson(crate::mcp_server::call_list_tests),
        },
    );

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
        description: "一次呼叫取得最近一批測試的完整摘要：pass/fail counts + console_errors 統計 + 建議動作。預設 suite=active-smoke，只看已驗證、可快速判斷目前 AgoraMarket 健康狀態的 deterministic smoke；可改 suite=deep-regression / obsolete / all，或用 tags/exclude_tags 自訂篩選。零筆符合資料時 status=no_data、has_data=false，不會誤報為 passed。\n\n回傳: { suite, include_tags, status, has_data, passed, failed, console_errors_total, console_warnings_total, results: [{test_id, status, run_mode, fixture_only, tags, console_errors, console_warnings, flag}] }",
        input_schema: json!({
            "type": "object",
            "properties": {
                "since":  { "type": "string", "description": "只看此時間（RFC3339 或 HH:MM）之後的 runs，預設最近 24 小時" },
                "limit":  { "type": "number", "description": "最多看幾個不重複的 test，預設 31" },
                "suite":  { "type": "string", "description": "active-smoke（預設）| deep-regression | obsolete | all" },
                "tag":    { "type": "string", "description": "選填：只看單一 tag，會覆蓋 suite 的預設 tag" },
                "tags":   { "type": "array", "items": { "type": "string" }, "description": "選填：任一 tag 命中即納入，會覆蓋 suite 的預設 tag" },
                "exclude_tags": { "type": "array", "items": { "type": "string" }, "description": "選填：排除任一 tag 命中的測試" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_test_summary),
    });

    m.insert("agora_health_check", ToolDef {
        name:         "agora_health_check",
        description: "外部 AI 的 AgoraMarket 單一健康檢查入口。整合 active-smoke 的 test_summary + script_health + 最新 run freshness，回傳 status/confidence/healthy/evidence/next_action。預設只讀不跑測試；傳 rerun_if_stale=true 會在資料過期或缺失時排 active-smoke，force_rerun=true 則直接重跑。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "stale_after_hours": {
                    "type": "number",
                    "description": "active-smoke 最新證據超過幾小時視為 stale，預設 6"
                },
                "rerun_if_stale": {
                    "type": "boolean",
                    "description": "預設 false；true 時資料 stale/no_data 會自動 queue active-smoke"
                },
                "force_rerun": {
                    "type": "boolean",
                    "description": "預設 false；true 時不管新鮮度都 queue active-smoke"
                },
                "max_concurrency": {
                    "type": "number",
                    "description": "保留相容；agora_health_check 目前固定 serial concurrency=1，以避免 Flutter semantics/profile 互相干擾"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_agora_health_check),
    });

    m.insert("agora_health_triage", ToolDef {
        name:         "agora_health_triage",
        description: "AgoraMarket active-smoke 失敗後的一鍵診斷入口。預設讀最新 active-smoke 非 passed run；也可傳 run_ids 或 test_ids 指定診斷。彙整 get_test_result / trace / console / screenshot 線索，輸出 failure_type、root_cause_hint、recommended_action、safe_to_escalate_to_codex。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "選填：指定要診斷的 run_id 清單"
                },
                "test_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "選填：指定 test_id，會診斷各 test 最新 run"
                },
                "include_passed": {
                    "type": "boolean",
                    "description": "預設 false；true 時也把 passed run 納入輸出，方便驗證分類"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::mcp_server::call_agora_health_triage),
    });

    m.insert("agora_explore_audit", ToolDef {
        name:         "agora_explore_audit",
        description: "AgoraMarket 自動探索稽核入口。Sirin 會依 config/audits/agoramarket.yaml 序列巡檢 buyer/seller/admin 主要頁與 seller 商品/訂單/錢包 surface，收集 final_url/title/Flutter semantics/shadow DOM/console 線索，偵測白屏、卡 login、role 錯誤、console error、核心文案缺失等明顯問題，輸出 issue_candidates 交給 Codex 驗收。每次 run 會回傳 run_id/evidence_path，並把 JSON evidence 持久化到 app data tracking/audits。這是 deterministic audit，不會修改 AgoraMarket。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "surfaces": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["buyer_home", "seller_home", "admin_home", "seller_products", "seller_orders", "seller_wallet"]
                    },
                    "description": "選填：只稽核指定 surface；預設全部"
                },
                "include_screenshots": {
                    "type": "boolean",
                    "description": "預設 false；true 時每個 surface 會附 screenshot base64，payload 較大"
                },
                "stop_on_first_critical": {
                    "type": "boolean",
                    "description": "預設 false；true 時遇到 high severity issue 立即停止"
                },
                "timeout_ms": {
                    "type": "number",
                    "description": "等待 Flutter semantics 的 timeout，預設 15000"
                }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::audit_engine::call_agora_explore_audit(args))),
    });

    m.insert("audit_history", ToolDef {
        name:         "audit_history",
        description: "列出最近 persisted audit runs。預設 profile_id=agoramarket，讀取 app data tracking/audits/<profile_id>/*/result.json，只做 read-only history 查詢，不啟動瀏覽器。回傳 run_id/status/issue_count/critical_count/evidence_path/issue_summary。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "audit profile id，預設 agoramarket"
                },
                "limit": {
                    "type": "number",
                    "description": "最多回傳幾筆，預設 10，範圍 1-100"
                },
                "include_issues": {
                    "type": "boolean",
                    "description": "預設 true；false 時省略 issue_summary，適合只看 run list"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::audit_engine::call_audit_history),
    });

    m.insert("audit_get_result", ToolDef {
        name:         "audit_get_result",
        description: "依 run_id 讀取 persisted audit evidence 完整 JSON。預設 profile_id=agoramarket，只讀 app data tracking/audits/<profile_id>/<run_id>/result.json，不啟動瀏覽器。",
        input_schema: json!({
            "type": "object",
            "required": ["run_id"],
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "audit profile id，預設 agoramarket"
                },
                "run_id": {
                    "type": "string",
                    "description": "agora_explore_audit 回傳的 run_id"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::audit_engine::call_audit_get_result),
    });

    m.insert("agoramarket_audit_latest", ToolDef {
        name:         "agoramarket_audit_latest",
        description: "快速取得最新 AgoraMarket audit summary。讀取 persisted evidence，不啟動瀏覽器。預設只回 summary；include_full=true 時附完整 result JSON。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "include_full": {
                    "type": "boolean",
                    "description": "預設 false；true 時附完整 persisted result JSON"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::audit_engine::call_agoramarket_audit_latest),
    });

    m.insert("handoff_list", ToolDef {
        name:         "handoff_list",
        description: "列出 Sirin audit 產生的 Codex handoff inbox。預設 profile_id=agoramarket、status=active，只讀 app data tracking/handoffs，不啟動瀏覽器。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "handoff profile id，預設 agoramarket"
                },
                "status": {
                    "type": "string",
                    "enum": ["active", "open", "acknowledged", "resolved", "reopened", "all"],
                    "description": "預設 active；active 會列出所有非 resolved handoff"
                },
                "limit": {
                    "type": "number",
                    "description": "最多回傳幾筆，預設 20，範圍 1-100"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::audit_engine::call_handoff_list),
    });

    m.insert("handoff_get", ToolDef {
        name:         "handoff_get",
        description: "讀取單一 Codex handoff record 完整內容。預設 profile_id=agoramarket，只讀 app data tracking/handoffs。",
        input_schema: json!({
            "type": "object",
            "required": ["handoff_id"],
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "handoff profile id，預設 agoramarket"
                },
                "handoff_id": {
                    "type": "string",
                    "description": "handoff_list 或 agora_explore_audit.handoff_inbox 回傳的 handoff_id"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::audit_engine::call_handoff_get),
    });

    m.insert("handoff_ack", ToolDef {
        name:         "handoff_ack",
        description: "把 Codex handoff 標記為 acknowledged，代表 Codex 已接手。只更新 Sirin handoff inbox 狀態，不啟動瀏覽器。",
        input_schema: json!({
            "type": "object",
            "required": ["handoff_id"],
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "handoff profile id，預設 agoramarket"
                },
                "handoff_id": {
                    "type": "string",
                    "description": "要接手的 handoff_id"
                },
                "actor": {
                    "type": "string",
                    "description": "操作者，預設 codex"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::audit_engine::call_handoff_ack),
    });

    m.insert("handoff_resolve", ToolDef {
        name:         "handoff_resolve",
        description: "把 Codex handoff 標記為 resolved，代表 Codex 已處理完成。record 內保留 recheck_hint 供後續 Sirin 重驗。",
        input_schema: json!({
            "type": "object",
            "required": ["handoff_id"],
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "handoff profile id，預設 agoramarket"
                },
                "handoff_id": {
                    "type": "string",
                    "description": "要完成的 handoff_id"
                },
                "actor": {
                    "type": "string",
                    "description": "操作者，預設 codex"
                },
                "note": {
                    "type": "string",
                    "description": "選填 resolve note，例如修復 PR 或驗收備註"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::audit_engine::call_handoff_resolve),
    });

    m.insert("handoff_recheck", ToolDef {
        name:         "handoff_recheck",
        description: "依 handoff_id 讀取 recheck_hint，只重跑該 handoff 對應 AgoraMarket surface。若原 fingerprint 消失則標記 verified_resolved；若仍出現則標記 reopened/open，並回傳 recheck run/evidence。",
        input_schema: json!({
            "type": "object",
            "required": ["handoff_id"],
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "handoff profile id，預設 agoramarket"
                },
                "handoff_id": {
                    "type": "string",
                    "description": "要重驗的 handoff_id"
                },
                "timeout_ms": {
                    "type": "number",
                    "description": "等待 Flutter semantics 的 timeout，預設 12000"
                },
                "include_screenshots": {
                    "type": "boolean",
                    "description": "預設 false；true 時 recheck audit 附 screenshot base64，payload 較大"
                }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::audit_engine::call_handoff_recheck(args))),
    });

    m.insert("test_analytics", ToolDef {
        name:         "test_analytics",
        description: "聚合測試健康指標：pass rate (近 10 / 30 runs)、flaky 標記、avg iterations、avg duration、最常見 failure_category。預設只看目前 config/tests 清單且限近 30 天，使用單次批次查詢；可明確要求歷史 test id 或完整時間範圍。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "test_id": { "type": "string", "description": "選填：只看特定測試；明確指定時保留該測試最近 30 runs 的歷史行為" },
                "window_days": { "type": "integer", "minimum": 0, "maximum": 3650, "default": 30, "description": "批次統計時間範圍；0=不限時間" },
                "include_historical": { "type": "boolean", "default": false, "description": "true 時納入已不在目前 config/tests 的歷史 test id" }
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
        description: "#266 — 回傳 saved_scripts / fixture_only deterministic 健康度指標，用於 Dashboard KPI / Coverage column / cron 偵測 stale-script drift。預設 suite=active-smoke，只看可快速判斷目前 AgoraMarket 健康狀態的 deterministic smoke；可改 suite=deep-regression / obsolete / all，或用 tags/exclude_tags 自訂篩選。\n\n回傳兩部份：\n- `aggregate`: 篩選後 tests 在時間窗內的實際 runs：total_runs / passed_via_replay / passed_via_fixture_only / passed_deterministic / passed_via_llm / failed / success_rate_replay / success_rate_deterministic + 上一週期比較、drift、latest_run_at、has_recent_data\n- `per_test`: 每個 test 的 status (healthy|fixture_only|stale_fallback|llm_only|untested) + tags + 最近 3 次 is_replay 歷史\n\n預設 window=14 天（前 7 天 vs 後 7 天比較）。若 drift < -0.10（10 個百分點），代表 replay 成功率明顯下滑，建議 audit。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "window_days": {
                    "type": "number",
                    "description": "比較窗（會被切兩半做 week-over-week drift），預設 14"
                },
                "suite":  { "type": "string", "description": "active-smoke（預設）| deep-regression | obsolete | all" },
                "tag":    { "type": "string", "description": "選填：只看單一 tag，會覆蓋 suite 的預設 tag" },
                "tags":   { "type": "array", "items": { "type": "string" }, "description": "選填：任一 tag 命中即納入，會覆蓋 suite 的預設 tag" },
                "exclude_tags": { "type": "array", "items": { "type": "string" }, "description": "選填：排除任一 tag 命中的測試" }
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
        description: "批次啟動多個 YAML test，立即返回 N 個 run_id；實際執行會經 TEST_RUN_LOCK 序列化，避免多個 Flutter/Chrome 測試同時撞 Sirin 的 process-wide Chrome singleton。\n\n適用場景：smoke suite / nightly regression / 一次排完多個 tag。\n\n限制：effective_concurrency 固定 1；不會自動 triage 或 auto_fix（失敗請用個別 run_test_async 重跑）；任一 test_id 找不到就整批拒絕。",
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
                    "description": "相容舊客戶端；目前會被 clamped to 1，因 Sirin Chrome singleton 必須序列執行"
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
        description: "#226 — KB 深度統計（補強 kbHealth）。透過 Sirin 共用 KB client 與 Agora OPS read authorization 取得指定 project 的條目分布：by domain / by status / by layer / by tag（若上游提供）/ draft ratio / stale ratio。\n\n另外包含 #219 的 brief 長度治理 breakdown：可檢查指定 topicKey 的字數是否超過上限（預設 3000）。上游健康檢查不可用、空白或無法解析時會回報 unavailable/unknown，不會顯示 healthy。",
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
        description: "語意搜尋 AgoraMarket Knowledge Base，返回最相關的條目內容。KB 必須啟用（KB_ENABLED=1）。Sirin 優先使用既有 Agora OPS read authorization；憑證不會進入瀏覽器。無結果會回傳 found=false，不會把提示文字當內容。",
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
        description: "依 topicKey 取得 Knowledge Base 單一條目。KB 必須啟用（KB_ENABLED=1）。Sirin 優先使用既有 Agora OPS read authorization；not-found 文字不會被當成內容或寫入快取。",
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
        description: "Upsert an explicitly requested Knowledge Base entry through Sirin's fixed server-local kbWrite service path. No external-AI bearer or per-call Telegram approval is used. The remote credential never leaves the AgoraMarketAPI host. Projects, topic keys, sizes, layer, and status are allowlisted; source is always sirin. KB_ENABLED=1 plus AGORA_SSH_HOST/AGORA_SSH_KEY (or SIRIN_KB_SSH_HOST/SIRIN_KB_SSH_KEY) are required. This manual tool is independent of KB_WRITE_TELEMETRY.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "topic_key": { "type": "string", "description": "recognized domain-prefixed lowercase kebab-case key, e.g. 'sirin-kb-local-write'" },
                "title":     { "type": "string", "description": "human-readable title" },
                "content":   { "type": "string", "description": "entry body, Markdown allowed, maximum 3500 characters" },
                "domain":    { "type": "string", "description": "e.g. 'tooling', 'cic', 'session-task'" },
                "tags":      { "type": "string", "description": "comma-separated, e.g. 'cic-task,result'" },
                "file_refs": { "type": "string", "description": "optional comma-separated file refs" },
                "project":   { "type": "string", "enum": ["sirin", "agora-backend", "flutter"], "description": "KB project slug (default reads KB_PROJECT env)" },
                "layer":     { "type": "string", "enum": ["raw", "topic", "brief"], "default": "topic" },
                "status":    { "type": "string", "enum": ["draft", "confirmed"], "default": "confirmed" },
                "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0, "default": 0.9 }
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

    m.insert(
        "memory_search",
        ToolDef {
            name: "memory_search",
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
            handler: ToolHandler::AsyncText(|args| {
                Box::pin(crate::mcp_server::call_memory_search(args))
            }),
        },
    );

    m.insert(
        "teams_approve",
        ToolDef {
            name: "teams_approve",
            description: "核准指定的 Teams 草稿，觸發送出。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "PendingReply ID" }
                },
                "required": ["id"]
            }),
            handler: ToolHandler::SyncText(crate::mcp_server::call_teams_approve),
        },
    );

    m.insert(
        "trigger_research",
        ToolDef {
            name: "trigger_research",
            description: "觸發 Sirin 對指定主題進行調研。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "topic": { "type": "string", "description": "調研主題" },
                    "url":   { "type": "string", "description": "參考 URL（選填）" }
                },
                "required": ["topic"]
            }),
            handler: ToolHandler::SyncText(crate::mcp_server::call_trigger_research),
        },
    );

    m.insert("research_sentinel_goal_create", ToolDef {
        name:         "research_sentinel_goal_create",
        description: "建立 Codex 定義的官方/投資資料研究目標。Sirin 只讀配置來源與本地 inbox；不讀 AgoraMarketAPI 交易狀態、不下單、不改 OCO。",
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
        description: "依 Codex 定義的 goal 或 inline topic 執行一次 Official + Investment Intelligence 研究。只讀 configured official/investment_data RSS 來源，整理 event score、Codex recommendation、來源 URL 與風險關聯；不使用一般新聞/搜尋 fallback，不輸出買賣建議。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "goal_id": { "type": "string", "description": "既有 research_sentinel goal id；未提供時使用 inline topic/focus/keywords" },
                "topic": { "type": "string", "description": "inline 研究主題" },
                "focus": { "type": "string", "description": "inline 研究焦點" },
                "keywords": { "type": "array", "items": { "type": "string" }, "description": "inline 搜尋關鍵字" },
                "lookback_hours": { "type": "integer", "description": "回看小時，預設 goal 值或 24，範圍 1..168" },
                "rss_feeds": { "type": "array", "items": { "type": "string" }, "description": "臨時 RSS 來源；未提供時使用 config/research_sentinel.yaml 的 official/investment_data sources" },
                "max_results": { "type": "integer", "description": "最多保留結果，預設 12，範圍 1..30" },
                "create_inbox": { "type": "boolean", "description": "是否寫入 inbox，預設 true" },
                "create_report": { "description": "HTML 報告產生策略：true/always 強制產生；false/never 不產生；auto 只在 new convert_to_issue、escalated、source_errors 時產生。未提供時手動 run 預設產生。" },
                "publish_to_kb": { "type": "boolean", "description": "是否對符合條件的新事件/升級事件嘗試寫入 KB；仍受 KB_ENABLED/KB_WRITE_TELEMETRY 保護，預設 false" },
                "deep_research": { "type": "boolean", "description": "是否啟用可選 LLM deep research 層；LLM 不可用時會保留 deterministic 結果並標記 unavailable，預設 false" },
                "deep_research_mode": { "type": "string", "enum": ["off", "auto", "required"], "description": "deep research 模式；auto 允許安全降級，required 會把 LLM 失敗標成 failed" },
                "max_deep_events": { "type": "integer", "description": "最多交給 deep research 分析的 Codex-review events，預設 4，範圍 1..8" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::research_sentinel::call_research_sentinel_run_once(args))),
    });

    m.insert(
        "research_sentinel_inbox",
        ToolDef {
            name: "research_sentinel_inbox",
            description: "列出 Sirin 本地 News Research Sentinel inbox，供 Codex 驗收研究結果。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "unread_only": { "type": "boolean", "description": "只列 unread，預設 true" },
                    "limit": { "type": "integer", "description": "最多筆數，預設 20，範圍 1..100" }
                }
            }),
            handler: ToolHandler::SyncJson(crate::research_sentinel::call_research_sentinel_inbox),
        },
    );

    m.insert("research_sentinel_monitor_status", ToolDef {
        name:         "research_sentinel_monitor_status",
        description: "查看 Research Sentinel 背景 monitor loop 狀態：enabled、interval、last_run、next_run、new/escalated counts、HTML report path、KB publish count。",
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
        handler:      ToolHandler::SyncJson(crate::research_sentinel::call_research_sentinel_monitor_status),
    });

    m.insert("a2a_worker_status", ToolDef {
        name:         "a2a_worker_status",
        description: "查看 Sirin server-side A2A worker 狀態。此 worker 只負責從外部任務服務 claim Sirin 可執行的工作、執行本地 research sentinel / AgoraMarket issue scout / first-order activation / Sirin test-health scout，再回報 artifacts；任務管理與驗收仍在服務端/Codex。",
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_worker_status),
    });

    m.insert("a2a_issue_candidates", ToolDef {
        name:         "a2a_issue_candidates",
        description: "列出 Sirin 本地保存的 ops scout 候選問題 inbox。候選由 Sirin worker 從 read-only evidence 產生並存於本機 app data，包括 AGORA_MARKET_ISSUE_SCOUT 與 SIRIN_AGORA_TEST_HEALTH_SCOUT；不依賴 AgoraMarketAPI 新增 review API。可用 task_id/status/severity/limit 篩選。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "可選：只列指定 A2A task id" },
                "status": { "type": "string", "description": "可選：CANDIDATE/CONVERT_TO_ISSUE/NEEDS_MORE_EVIDENCE/REJECTED/ACCEPTED" },
                "severity": { "type": "string", "description": "可選：HIGH/MEDIUM/LOW" },
                "limit": { "type": "integer", "description": "最多筆數，預設 20，範圍 1..100" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_issue_candidates),
    });

    m.insert("ops_event_inbox", ToolDef {
        name:         "ops_event_inbox",
        description: "列出 Sirin 本地 AgoraMarket 商城運營事件 inbox。這是 a2a_issue_candidates 的運營事件視角，可用 eventType/evidenceLevel/allowedAction/targetProject/status/severity/limit 篩選；只讀 Sirin 本機 app data，不建立 issue、不改 AgoraMarket/API。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "可選：只列指定 A2A task id" },
                "status": { "type": "string", "description": "可選：CANDIDATE/CONVERT_TO_ISSUE/NEEDS_MORE_EVIDENCE/REJECTED/ACCEPTED" },
                "severity": { "type": "string", "description": "可選：HIGH/MEDIUM/LOW" },
                "eventType": { "type": "string", "description": "可選：service_health/order_risk/growth_signal/test_coverage_gap 等 opsEvent.eventType" },
                "evidenceLevel": { "type": "string", "description": "可選：suspected/observed/needs_verification/hypothesis/confirmed" },
                "allowedAction": { "type": "string", "description": "可選：SIRIN_BACKLOG_ONLY/CODEX_VERIFY_INTEGRATION/HANDOFF_REVIEW_ONLY/OPERATOR_REVIEW/OPEN_PRODUCT_ISSUE_AFTER_REVIEW" },
                "targetProject": { "type": "string", "description": "可選：Sirin/AgoraMarket/AgoraMarketAPI" },
                "limit": { "type": "integer", "description": "最多筆數，預設 20，範圍 1..100" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_ops_event_inbox),
    });

    m.insert("a2a_review_issue_candidate", ToolDef {
        name:         "a2a_review_issue_candidate",
        description: "在 Sirin 本地 review 一筆 ops event/候選問題，寫入 review_status/review_note/reviewed_by 並回傳 handoff。reviewStatus=CONVERT_TO_ISSUE 只有 allowedAction=OPEN_PRODUCT_ISSUE_AFTER_REVIEW 時允許；其他事件只能 ACCEPTED/NEEDS_MORE_EVIDENCE/REJECTED。這只更新 Sirin 本機 review inbox；不建立 GitHub issue、不修改 AgoraMarketAPI、不改訂單/資金/交易狀態。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "dedupeKey": { "type": "string", "description": "候選問題 dedupeKey" },
                "id": { "type": "string", "description": "可選 alias：候選 record_id 或 dedupeKey" },
                "recordId": { "type": "string", "description": "可選 alias：候選 record_id" },
                "task_id": { "type": "string", "description": "可選：同一 dedupeKey 多 task 時指定 task id" },
                "reviewStatus": {
                    "type": "string",
                    "enum": ["ACCEPTED", "REJECTED", "NEEDS_MORE_EVIDENCE", "NEEDS_MORE_SOURCES", "CONVERT_TO_ISSUE"],
                    "description": "候選審核狀態"
                },
                "reviewNote": { "type": "string", "description": "可選：審核備註" },
                "reviewedBy": { "type": "string", "description": "可選：審核者，預設 codex" },
                "externalIssueUrl": { "type": "string", "description": "可選：已開外部 issue URL，例如 AgoraMarketAPI GitHub issue" },
                "externalIssueProject": { "type": "string", "description": "可選：外部 issue 所屬專案，例如 AgoraMarketAPI" },
                "externalIssueNumber": { "type": "integer", "description": "可選：外部 issue 編號" }
            },
            "required": ["reviewStatus"]
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_review_issue_candidate),
    });

    m.insert("a2a_handoff_queue", ToolDef {
        name:         "a2a_handoff_queue",
        description: "列出 Sirin 本地 approved handoff queue。當 issue candidate 被審核為 CONVERT_TO_ISSUE 或 ACCEPTED 時會入隊，供 operator 交給 ChatGPT/Codex；工具本身不呼叫外部 AI、不建立 GitHub issue、不改 AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "description": "READY/ACKED/DONE/CANCELLED/ALL，預設 READY" },
                "limit": { "type": "integer", "description": "最多筆數，預設 50，範圍 1..200" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_handoff_queue),
    });

    m.insert("a2a_handoff_approval_packets", ToolDef {
        name:         "a2a_handoff_approval_packets",
        description: "只讀產生 READY handoff 的 operator approval packets，包含可複製的 a2a_update_handoff ACK template。用於把 Sirin 發現的運營問題安全交給 ChatGPT/Codex 複查；不 ACK、不呼叫外部 AI、不改 GitHub/AgoraMarket/AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "handoffId": { "type": "string", "description": "可選：只產生指定 handoff 的 approval packet" },
                "limit": { "type": "integer", "description": "最多筆數，預設 20，範圍 1..100" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_handoff_approval_packets),
    });

    m.insert("a2a_request_handoff_approval", ToolDef {
        name:         "a2a_request_handoff_approval",
        description: "為 READY handoff 建立或更新 Sirin 本地 operator approval request。這代表 Sirin 正式提請審批，但不 ACK handoff、不呼叫 ChatGPT/Codex、不改 GitHub/AgoraMarket/AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "handoffId": { "type": "string", "description": "READY handoff queue item id" },
                "requestedBy": { "type": "string", "description": "可選：提出申請者，預設 sirin" },
                "note": { "type": "string", "description": "可選：審批申請備註" }
            },
            "required": ["handoffId"]
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_request_handoff_approval),
    });

    m.insert("a2a_handoff_approval_requests", ToolDef {
        name:         "a2a_handoff_approval_requests",
        description: "列出 Sirin 本地 handoff approval request queue。預設只看 PENDING_OPERATOR_APPROVAL；只讀，不 ACK handoff、不呼叫 ChatGPT/Codex、不改外部專案。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "handoffId": { "type": "string", "description": "可選：只列指定 handoff 的 approval request" },
                "status": { "type": "string", "description": "PENDING_OPERATOR_APPROVAL/ALL，預設 PENDING_OPERATOR_APPROVAL" },
                "limit": { "type": "integer", "description": "最多筆數，預設 50，範圍 1..200" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_handoff_approval_requests),
    });

    m.insert("a2a_update_handoff_approval_request", ToolDef {
        name:         "a2a_update_handoff_approval_request",
        description: "更新 Sirin 本地 handoff approval request 狀態，例如 APPROVED/REJECTED/CANCELLED。只記錄 operator 對申請的決定；不 ACK handoff、不呼叫 ChatGPT/Codex、不改 GitHub/AgoraMarket/AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "requestId": { "type": "string", "description": "approval request id；可用 handoffId 代替" },
                "handoffId": { "type": "string", "description": "approval request 對應 handoff id；可用 requestId 代替" },
                "status": {
                    "type": "string",
                    "enum": ["PENDING_OPERATOR_APPROVAL", "APPROVED", "REJECTED", "CANCELLED"],
                    "description": "新的 approval request 狀態"
                },
                "reviewer": { "type": "string", "description": "可選：operator/reviewer id；預設 operator" },
                "reviewedBy": { "type": "string", "description": "可選：reviewer alias" },
                "decision": { "type": "string", "description": "可選：operator decision；預設等於 status" },
                "note": { "type": "string", "description": "可選：review note" },
                "approvedBoundary": { "type": "string", "description": "可選：批准邊界；預設只允許 Sirin-local metadata update" }
            },
            "required": ["status"]
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_update_handoff_approval_request),
    });

    m.insert("a2a_handoff_review_inbox", ToolDef {
        name:         "a2a_handoff_review_inbox",
        description: "只讀列出 handoff 下游 review inbox：已 ACK 且有 executionPackage 的項目可交 ChatGPT/Codex 複查，READY handoff 會以 approval_required 顯示並附 approval packet；不 ACK、不呼叫外部 AI、不改 GitHub/AgoraMarket/AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "actor": { "type": "string", "description": "可選：CHATGPT/CODEX" },
                "status": { "type": "string", "description": "可選：READY_FOR_REVIEW/APPROVAL_REQUIRED/LEGACY_ACK_MISSING_EXECUTION_PACKAGE" },
                "limit": { "type": "integer", "description": "最多筆數，預設 50，範圍 1..200" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_handoff_review_inbox),
    });

    m.insert("a2a_update_handoff", ToolDef {
        name:         "a2a_update_handoff",
        description: "更新 Sirin 本地 approved handoff 狀態，例如 ACKED/DONE/CANCELLED，並可記錄 operator review decision/boundary。ACKED 會產生本地 executionPackage 供 ChatGPT/Codex 複查，但只更新本地 handoff queue metadata，不呼叫 ChatGPT/Codex/GitHub/AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "handoffId": { "type": "string", "description": "handoff queue item id" },
                "status": {
                    "type": "string",
                    "enum": ["READY", "ACKED", "DONE", "CANCELLED"],
                    "description": "新的 handoff 狀態"
                },
                "note": { "type": "string", "description": "可選：狀態更新備註" },
                "reviewer": { "type": "string", "description": "可選：reviewer/operator id；預設 operator" },
                "reviewedBy": { "type": "string", "description": "可選：reviewer/operator id alias" },
                "decision": { "type": "string", "description": "可選：operator/Codex review decision；預設等於 status" },
                "approvedBoundary": {
                    "type": "string",
                    "description": "可選：明確批准邊界；預設只允許 Sirin-local metadata update"
                },
                "approvedActions": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "可選：本次 review 批准的 Sirin-local actions"
                },
                "blockedMutations": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "可選：本次 review 仍禁止的 external/production mutations"
                }
            },
            "required": ["handoffId", "status"]
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_update_handoff),
    });

    m.insert("a2a_prepare_handoff_execution_packages", ToolDef {
        name:         "a2a_prepare_handoff_execution_packages",
        description: "為舊版 ACKED handoff 補齊 Sirin 本地 executionPackage，讓已審核 handoff 可被 ChatGPT/Codex 複查。預設 dryRun=true；dryRun=false 只寫 Sirin 本地 handoff queue，不呼叫 ChatGPT/Codex/GitHub/AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "dryRun": {
                    "type": "boolean",
                    "description": "預設 true；false 才會寫入 Sirin 本地 handoff queue"
                },
                "limit": {
                    "type": "integer",
                    "description": "最多準備幾筆 ACKED handoff，預設 50，範圍 1..200"
                }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::a2a_worker::call_a2a_prepare_handoff_execution_packages),
    });

    m.insert("ops_schedule_create", ToolDef {
        name:         "ops_schedule_create",
        description: "建立 Sirin 本地運營 AI 排期任務。任務資料存於 Sirin app data tracking/ops_schedule.jsonl，可排 SIRIN_AGORA_TEST_HEALTH_SCOUT、AGORA_MARKET_ISSUE_SCOUT、AGORA_MARKET_FIRST_ORDER_ACTIVATION 等 Sirin 可執行檢查；不寫 AgoraMarketAPI、不建立遠端任務。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "排期任務標題" },
                "description": { "type": "string", "description": "可選：任務說明" },
                "taskType": { "type": "string", "description": "預設 SIRIN_AGORA_TEST_HEALTH_SCOUT；外部 read-only ops MCP 巡檢可用 AGORA_MARKET_ISSUE_SCOUT；第一單成交 playbook 可用 AGORA_MARKET_FIRST_ORDER_ACTIVATION" },
                "params": { "type": "object", "description": "傳給 Sirin 檢查任務的本地參數" },
                "cadence": { "type": "string", "description": "manual/daily/weekly/once，第一版只保存排期語義" },
                "priority": { "type": "string", "description": "P0/P1/P2，預設 P1" },
                "scheduledFor": { "type": "string", "description": "可選：RFC3339 或人類可讀時間字串" }
            },
            "required": ["title"]
        }),
        handler:      ToolHandler::SyncJson(crate::ops_schedule::call_ops_schedule_create),
    });

    m.insert("ops_review_queue_list", ToolDef {
        name:         "ops_review_queue_list",
        description: "列出 Sirin 本地 ops_action_ledger 的 Codex 複查任務。這是 Sirin 發起、Codex 複查的入口；只讀本地 tracking/ops_action_ledger.jsonl，不呼叫 Codex/GitHub/AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "description": "PENDING_CODEX_REVIEW/CLAIMED/IN_PROGRESS/NEEDS_MORE_EVIDENCE/BLOCKED/VERIFIED/DONE/REJECTED/ALL，預設 PENDING_CODEX_REVIEW" },
                "actor": { "type": "string", "description": "可選：通常是 CODEX" },
                "targetProject": { "type": "string", "description": "可選：例如 Sirin/AgoraMarket/AgoraMarketAPI" },
                "limit": { "type": "integer", "description": "最多筆數，預設 50，範圍 1..200" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ops_action_ledger::call_ops_review_queue_list),
    });

    m.insert("ops_review_claim", ToolDef {
        name:         "ops_review_claim",
        description: "讓 Codex claim 一筆 Sirin 本地 CODEX_REVIEW 任務。只更新 Sirin 本地 ledger 狀態，不修改外部專案。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "actionId": { "type": "string", "description": "OPS-ACTION-YYYYMMDD-NNNN" },
                "id": { "type": "string", "description": "actionId alias" },
                "claimedBy": { "type": "string", "description": "預設 codex" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ops_action_ledger::call_ops_review_claim),
    });

    m.insert("ops_review_update", ToolDef {
        name:         "ops_review_update",
        description: "更新 Sirin 本地 CODEX_REVIEW 任務進度，例如 IN_PROGRESS、NEEDS_MORE_EVIDENCE、BLOCKED。只寫本地 ledger。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "actionId": { "type": "string", "description": "OPS-ACTION-YYYYMMDD-NNNN" },
                "id": { "type": "string", "description": "actionId alias" },
                "status": { "type": "string", "enum": ["PENDING_CODEX_REVIEW", "CLAIMED", "IN_PROGRESS", "NEEDS_MORE_EVIDENCE", "BLOCKED"] },
                "note": { "type": "string", "description": "可選：進度備註" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ops_action_ledger::call_ops_review_update),
    });

    m.insert("ops_review_complete", ToolDef {
        name:         "ops_review_complete",
        description: "Codex 完成複查後把結論回寫 Sirin 本地 action ledger。可帶 result 物件或 finding/evidence/recommendedNextAction/issueToOpenOrUpdate 欄位；不建立 issue、不改 AgoraMarket/API。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "actionId": { "type": "string", "description": "OPS-ACTION-YYYYMMDD-NNNN" },
                "id": { "type": "string", "description": "actionId alias" },
                "status": { "type": "string", "enum": ["VERIFIED", "DONE", "REJECTED", "NEEDS_MORE_EVIDENCE"], "description": "完成狀態，預設 VERIFIED" },
                "result": { "type": "object", "description": "Codex 複查結論，例如 is_real_issue/evidence/recommended_next_action/issue_to_open_or_update" },
                "finding": { "type": "string", "description": "未提供 result 時的簡短結論" },
                "evidence": { "type": "string", "description": "未提供 result 時的證據摘要" },
                "recommendedNextAction": { "type": "string", "description": "未提供 result 時的下一步" },
                "issueToOpenOrUpdate": { "type": "string", "description": "未提供 result 時的 issue URL 或建議" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ops_action_ledger::call_ops_review_complete),
    });

    m.insert("ops_schedule_list", ToolDef {
        name:         "ops_schedule_list",
        description: "列出 Sirin 本地運營 AI 排期任務。可依 status/taskType/priority 篩選；資料只來自本機 tracking/ops_schedule.jsonl。成功執行後可看 last_result_status 判斷報告是 READY_FOR_REVIEW 或 BLOCKED_INTEGRATION。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "description": "SCHEDULED/RUNNING/PAUSED/DONE/CANCELLED/FAILED" },
                "taskType": { "type": "string", "description": "可選：任務類型" },
                "priority": { "type": "string", "description": "可選：P0/P1/P2" },
                "limit": { "type": "integer", "description": "最多筆數，預設 50，範圍 1..200" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ops_schedule::call_ops_schedule_list),
    });

    m.insert("ops_schedule_digest", ToolDef {
        name:         "ops_schedule_digest",
        description: "彙總最近 Sirin 本地運營 AI 排期結果，輸出跨輪 actionSummary、已追蹤 issue/handoff、剩餘待處理項與下一個建議排程；只讀本地 tracking/ops_schedule.jsonl，不執行任務也不修改外部系統。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "taskType": { "type": "string", "description": "可選：只彙總指定任務類型，例如 AGORA_MARKET_ISSUE_SCOUT" },
                "limit": { "type": "integer", "description": "最近幾筆 schedule，預設 10，範圍 1..50" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ops_schedule::call_ops_schedule_digest),
    });

    m.insert("ops_external_issue_sync", ToolDef {
        name:         "ops_external_issue_sync",
        description: "只讀同步 Sirin 本地運營排期中追蹤的 GitHub issue 狀態，將 open/closed 寫回本機 tracking/ops_schedule.jsonl，供 externalIssueStateSync 決定是否可重跑 checkout dry-run；不修改 GitHub、AgoraMarket 或 AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "taskType": { "type": "string", "description": "可選：只同步指定任務類型，預設 AGORA_MARKET_FIRST_ORDER_ACTIVATION" },
                "limit": { "type": "integer", "description": "最多掃描幾筆 schedule，預設 50，範圍 1..200" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ops_schedule::call_ops_external_issue_sync),
    });

    m.insert("ops_schedule_update", ToolDef {
        name:         "ops_schedule_update",
        description: "更新 Sirin 本地運營 AI 排期狀態，例如 PAUSED/DONE/CANCELLED。只更新 Sirin 本地 metadata，不觸發 AgoraMarketAPI side effect。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "scheduleId": { "type": "string", "description": "OPS-YYYYMMDD-NNNN" },
                "status": {
                    "type": "string",
                    "enum": ["SCHEDULED", "RUNNING", "PAUSED", "DONE", "CANCELLED", "FAILED"],
                    "description": "新的排期狀態"
                },
                "note": { "type": "string", "description": "可選：狀態更新備註" }
            },
            "required": ["scheduleId", "status"]
        }),
        handler:      ToolHandler::SyncJson(crate::ops_schedule::call_ops_schedule_update),
    });

    m.insert("ops_schedule_run_due", ToolDef {
        name:         "ops_schedule_run_due",
        description: "執行 Sirin 本地 due 的運營 AI 排期任務。會把 SCHEDULED 且到期的任務標記 RUNNING，呼叫 Sirin 既有 inspection 能力，成功後將 issue scout candidates 寫回 Sirin 本地 inbox，並在本輪結束後只讀同步 tracked GitHub issue state 到本地 snapshot；不修改 GitHub、AgoraMarket 或 AgoraMarketAPI。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "scheduleId": { "type": "string", "description": "可選：只執行指定 OPS schedule" },
                "limit": { "type": "integer", "description": "最多執行幾筆 due schedule，預設 1，範圍 1..10" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ops_schedule::call_ops_schedule_run_due(args))),
    });

    m.insert("device_list", ToolDef {
        name:         "device_list",
        description: "列出本機 Android SDK、ADB devices 與 Android Emulator AVD。只讀本機狀態；用於確認 Sirin 是否能控制 emulator。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_list),
    });

    m.insert("device_emulator_start", ToolDef {
        name:         "device_emulator_start",
        description: "啟動 Android Emulator AVD 並等待 adb boot_completed。預設 AVD=Sirin_Pixel_7_API_35，使用 D:\\Android\\Sdk / D:\\Android\\Avd；若 emulator 已在跑則直接回傳 already_running。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "avd": { "type": "string", "description": "AVD 名稱，預設 Sirin_Pixel_7_API_35" },
                "timeoutSeconds": { "type": "integer", "description": "等待 boot completed 秒數，預設 180" },
                "noWindow": { "type": "boolean", "description": "是否無視窗啟動，預設 true" },
                "gpu": { "type": "string", "description": "emulator -gpu 參數，預設 swiftshader_indirect" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_emulator_start),
    });

    m.insert("device_emulator_status", ToolDef {
        name:         "device_emulator_status",
        description: "查詢 Android Emulator/ADB 狀態，包含 SDK/AVD 路徑、AVD 清單、device、bootCompleted、foregroundPackage。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_emulator_status),
    });

    m.insert("device_screenshot", ToolDef {
        name:         "device_screenshot",
        description: "擷取 Android Emulator 目前畫面 PNG，寫入 Sirin app data/device_screenshots，回傳 path、size、sha256。用於 GPS 路線後的 App 畫面證據閉環；預設只允許 emulator-*。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "device": { "type": "string", "description": "ADB serial，例如 emulator-5554；省略時自動選第一台 emulator" },
                "label": { "type": "string", "description": "可選：檔名標籤，會自動清理不安全字元" },
                "allowPhysicalDevice": { "type": "boolean", "description": "預設 false；true 才允許非 emulator device" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_screenshot),
    });

    m.insert("device_app_launch", ToolDef {
        name:         "device_app_launch",
        description: "在 Android Emulator 啟動 App。可用 package 走 monkey launcher，或 package+activity 走 am start。預設只允許 emulator-*。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "device": { "type": "string", "description": "ADB serial，例如 emulator-5554；省略時自動選第一台 emulator" },
                "package": { "type": "string", "description": "Android package name" },
                "activity": { "type": "string", "description": "可選：Activity class/path；提供時使用 am start -n package/activity" },
                "allowPhysicalDevice": { "type": "boolean", "description": "預設 false；true 才允許非 emulator device" }
            },
            "required": ["package"]
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_app_launch),
    });

    m.insert("device_operator_login_start", ToolDef {
        name:         "device_operator_login_start",
        description: "建立本地等待用戶手動登入的狀態。Sirin 不輸入帳密；用戶在 emulator 內登入後呼叫 device_operator_login_confirm。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "device": { "type": "string", "description": "ADB serial，例如 emulator-5554；省略時自動選第一台 emulator" },
                "loginId": { "type": "string", "description": "可選：自訂 login wait id" },
                "package": { "type": "string", "description": "可選：正在等待登入的 App package" },
                "timeoutSeconds": { "type": "integer", "description": "登入等待語義秒數，預設 300" },
                "note": { "type": "string", "description": "可選：操作提示" },
                "allowPhysicalDevice": { "type": "boolean", "description": "預設 false；true 才允許非 emulator device" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_operator_login_start),
    });

    m.insert("device_operator_login_status", ToolDef {
        name:         "device_operator_login_status",
        description: "查詢 Sirin 本地用戶手動登入等待狀態。可指定 loginId；省略則列出所有本程序內登入等待。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "loginId": { "type": "string", "description": "可選：login wait id" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_operator_login_status),
    });

    m.insert("device_operator_login_confirm", ToolDef {
        name:         "device_operator_login_confirm",
        description: "由用戶或操作員確認 emulator 內帳號已登入完成。只更新 Sirin 本地狀態，不讀取或保存帳密。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "loginId": { "type": "string", "description": "login wait id" },
                "note": { "type": "string", "description": "可選：確認備註" }
            },
            "required": ["loginId"]
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_operator_login_confirm),
    });

    m.insert("device_geo_fix", ToolDef {
        name:         "device_geo_fix",
        description: "對 Android Emulator 注入單次 GPS 定位，底層使用 adb -s <device> emu geo fix <lng> <lat>。預設只允許 emulator-*，真機必須顯式 allowPhysicalDevice=true。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "device": { "type": "string", "description": "ADB serial，例如 emulator-5554；省略時自動選第一台 emulator" },
                "lat": { "type": "number", "description": "緯度" },
                "lng": { "type": "number", "description": "經度" },
                "point": { "type": "object", "description": "可選：{lat,lng}" },
                "allowPhysicalDevice": { "type": "boolean", "description": "預設 false；true 才允許非 emulator device" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_geo_fix),
    });

    m.insert("device_geo_route_plan", ToolDef {
        name:         "device_geo_route_plan",
        description: "規劃 Android Emulator GPS 路線點。輸入起終點/多點、speedKmh、durationSeconds、tickSeconds，回傳每個 tick 的 lat/lng；不執行 ADB。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "route": { "type": "object", "description": "{start:{lat,lng}, end:{lat,lng}} 或 {points:[{lat,lng},...]}" },
                "start": { "type": "object", "description": "可選：起點 {lat,lng}" },
                "end": { "type": "object", "description": "可選：終點 {lat,lng}" },
                "points": { "type": "array", "items": { "type": "object" }, "description": "可選：多點路線" },
                "speedKmh": { "type": "number", "description": "預設 20.0" },
                "durationSeconds": { "type": "integer", "description": "預設 600" },
                "tickSeconds": { "type": "integer", "description": "預設 1" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_geo_route_plan),
    });

    m.insert("device_geo_route_start", ToolDef {
        name:         "device_geo_route_start",
        description: "啟動 Android Emulator GPS 路線注入背景任務。典型用法：speedKmh=20、durationSeconds=600、tickSeconds=1，Sirin 每秒注入一次定位。預設只允許 emulator-*。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "routeId": { "type": "string", "description": "可選：自訂 route id" },
                "device": { "type": "string", "description": "ADB serial，例如 emulator-5554；省略時自動選第一台 emulator" },
                "route": { "type": "object", "description": "{start:{lat,lng}, end:{lat,lng}} 或 {points:[{lat,lng},...]}" },
                "start": { "type": "object", "description": "可選：起點 {lat,lng}" },
                "end": { "type": "object", "description": "可選：終點 {lat,lng}" },
                "points": { "type": "array", "items": { "type": "object" }, "description": "可選：多點路線" },
                "speedKmh": { "type": "number", "description": "預設 20.0" },
                "durationSeconds": { "type": "integer", "description": "預設 600" },
                "tickSeconds": { "type": "integer", "description": "預設 1" },
                "screenMonitor": { "type": "boolean", "description": "預設 false；true 時路線執行中定期擷取 emulator 畫面，狀態回傳 screenshot path/hash/evidence" },
                "screenSampleEveryTicks": { "type": "integer", "description": "screenMonitor=true 時每 N 個 tick 擷取一次，預設 5" },
                "screenRequireChange": { "type": "boolean", "description": "預設 false；true 時連續截圖 hash 不變達 maxStaleScreenTicks 會判定 FAILED" },
                "maxStaleScreenTicks": { "type": "integer", "description": "screenRequireChange=true 時允許連續不變樣本數，預設 3" },
                "allowPhysicalDevice": { "type": "boolean", "description": "預設 false；true 才允許非 emulator device" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_geo_route_start),
    });

    m.insert("device_geo_roam_plan", ToolDef {
        name:         "device_geo_roam_plan",
        description: "自動產生道路型城市漫遊路線。預設台北中心、半徑 1500m、20km/h、10 分鐘、每秒 tick；先產生 non-repeating seed waypoints，再用 OSRM-compatible road router 取得道路 geometry；只規劃不執行 ADB。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "city": { "type": "string", "description": "可選：taipei/new_taipei/taichung/kaohsiung；省略預設 taipei" },
                "center": { "type": "object", "description": "可選：覆蓋城市中心 {lat,lng}" },
                "radiusMeters": { "type": "number", "description": "漫遊半徑，預設 1500，範圍 100..10000" },
                "waypoints": { "type": "integer", "description": "路線 waypoint 數，預設 8，範圍 4..32" },
                "roadProvider": { "type": "string", "description": "預設 osrm；offline/unit test 可用 synthetic" },
                "roadRouterUrl": { "type": "string", "description": "OSRM-compatible base URL，預設 https://router.project-osrm.org" },
                "roadProfile": { "type": "string", "description": "OSRM profile，預設 driving" },
                "roadTimeoutSeconds": { "type": "integer", "description": "road router timeout，預設 15" },
                "allowSyntheticFallback": { "type": "boolean", "description": "預設 false；road routing 失敗時是否允許退回非道路 synthetic route" },
                "speedKmh": { "type": "number", "description": "預設 20.0" },
                "durationSeconds": { "type": "integer", "description": "預設 600" },
                "tickSeconds": { "type": "integer", "description": "預設 1" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_geo_roam_plan),
    });

    m.insert("device_geo_roam_start", ToolDef {
        name:         "device_geo_roam_start",
        description: "自動產生道路型城市漫遊路線並啟動 Android Emulator GPS 注入。典型流程：device_emulator_start → device_app_launch → device_operator_login_start/confirm → device_geo_roam_start。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "device": { "type": "string", "description": "ADB serial，例如 emulator-5554；省略時自動選第一台 emulator" },
                "city": { "type": "string", "description": "可選：taipei/new_taipei/taichung/kaohsiung；省略預設 taipei" },
                "center": { "type": "object", "description": "可選：覆蓋城市中心 {lat,lng}" },
                "radiusMeters": { "type": "number", "description": "漫遊半徑，預設 1500，範圍 100..10000" },
                "waypoints": { "type": "integer", "description": "路線 waypoint 數，預設 8，範圍 4..32" },
                "roadProvider": { "type": "string", "description": "預設 osrm；offline/unit test 可用 synthetic" },
                "roadRouterUrl": { "type": "string", "description": "OSRM-compatible base URL，預設 https://router.project-osrm.org" },
                "roadProfile": { "type": "string", "description": "OSRM profile，預設 driving" },
                "roadTimeoutSeconds": { "type": "integer", "description": "road router timeout，預設 15" },
                "allowSyntheticFallback": { "type": "boolean", "description": "預設 false；road routing 失敗時是否允許退回非道路 synthetic route" },
                "speedKmh": { "type": "number", "description": "預設 20.0" },
                "durationSeconds": { "type": "integer", "description": "預設 600" },
                "tickSeconds": { "type": "integer", "description": "預設 1" },
                "screenMonitor": { "type": "boolean", "description": "預設 false；true 時路線執行中定期擷取 emulator 畫面，狀態回傳 screenshot path/hash/evidence" },
                "screenSampleEveryTicks": { "type": "integer", "description": "screenMonitor=true 時每 N 個 tick 擷取一次，預設 5" },
                "screenRequireChange": { "type": "boolean", "description": "預設 false；true 時連續截圖 hash 不變達 maxStaleScreenTicks 會判定 FAILED" },
                "maxStaleScreenTicks": { "type": "integer", "description": "screenRequireChange=true 時允許連續不變樣本數，預設 3" },
                "allowPhysicalDevice": { "type": "boolean", "description": "預設 false；true 才允許非 emulator device" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_geo_roam_start),
    });

    m.insert("device_geo_route_status", ToolDef {
        name:         "device_geo_route_status",
        description: "查詢 Sirin 目前 Android Emulator GPS 路線注入任務狀態。可指定 routeId；省略則列出所有本程序內任務。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "routeId": { "type": "string", "description": "可選：route id" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::device_geo::call_device_geo_route_status),
    });

    m.insert(
        "device_geo_route_stop",
        ToolDef {
            name: "device_geo_route_stop",
            description: "停止 Sirin Android Emulator GPS 路線注入背景任務。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "routeId": { "type": "string", "description": "route id" }
                },
                "required": ["routeId"]
            }),
            handler: ToolHandler::SyncJson(crate::device_geo::call_device_geo_route_stop),
        },
    );

    m.insert("ios_3utools_status", ToolDef {
        name:         "ios_3utools_status",
        description: "Deprecated：舊 3uTools GUI PoC 狀態工具。新 iOS 定位方案請使用 ios_dev_location_status；此工具只保留相容，不作為主路線。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_3utools_status),
    });

    m.insert("ios_3utools_start", ToolDef {
        name:         "ios_3utools_start",
        description: "Deprecated：舊 3uTools GUI PoC 啟動工具。新 iOS 定位方案不使用 3uTools，請使用 ios_dev_location_*。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_3utools_start),
    });

    m.insert("ios_dev_location_status", ToolDef {
        name:         "ios_dev_location_status",
        description: "檢查不依賴 3uTools 的 iOS Developer Location Driver：偵測 pymobiledevice3 / libimobiledevice(idevicesetlocation)、Apple Mobile Device Service、連線裝置摘要與安全邊界。只讀且會遮罩裝置識別碼。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_status),
    });

    m.insert("ios_dev_location_current", ToolDef {
        name:         "ios_dev_location_current",
        description: "嘗試讀取 iOS 真機當前 GPS 位置。若 pymobiledevice3/lockdown/developer services 不提供當前位置，回 UNAVAILABLE_REQUIRES_APP_REPORT 並寫 evidence，要求由受信任 App/Web 回報座標。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "requestId": { "type": "string", "description": "可選：自訂 current-location probe id" },
                "note": { "type": "string", "description": "可選：operator 備註" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_current),
    });

    m.insert("ios_location_report_latest", ToolDef {
        name:         "ios_location_report_latest",
        description: "讀取 iPhone Safari/受信任瀏覽器透過 /ios-location-report 回報給 Sirin 的最新 GPS 座標；用於 iOS 不允許直接讀取目前位置時，作為當前城市漫遊起點。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_location_report_latest),
    });

    m.insert("ios_dev_location_set", ToolDef {
        name:         "ios_dev_location_set",
        description: "透過 iOS developer location simulation CLI backend 設定單點定位。不使用 3uTools；backend=auto 時優先 pymobiledevice3，否則 idevicesetlocation。會寫 Sirin 本地 evidence。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "string", "description": "可選：自訂 iOS developer location session id" },
                "backend": { "type": "string", "description": "auto/pymobiledevice3/libimobiledevice，預設 auto" },
                "lat": { "type": "number", "description": "緯度" },
                "lng": { "type": "number", "description": "經度" },
                "point": { "type": "object", "description": "可選：{lat,lng}" },
                "targetApp": { "type": "string", "description": "可選：正在測試的 app 名稱/package/bundle id" },
                "note": { "type": "string", "description": "可選：operator 備註" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_set),
    });

    m.insert("ios_dev_location_reset", ToolDef {
        name:         "ios_dev_location_reset",
        description: "透過 iOS developer location simulation CLI backend 重置/清除模擬定位。不使用 3uTools；會寫 Sirin 本地 evidence。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "string", "description": "可選：自訂 reset session id" },
                "backend": { "type": "string", "description": "auto/pymobiledevice3/libimobiledevice，預設 auto" },
                "note": { "type": "string", "description": "可選：operator 備註" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_reset),
    });

    m.insert("ios_dev_location_session_status", ToolDef {
        name:         "ios_dev_location_session_status",
        description: "查詢 Sirin 本程序內 iOS developer location session。可指定 sessionId；省略列全部。只讀。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "string", "description": "可選：iOS developer location session id" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_session_status),
    });

    m.insert("ios_dev_location_route_plan", ToolDef {
        name:         "ios_dev_location_route_plan",
        description: "建立不依賴 3uTools 的 iOS developer-location 道路路線計畫。可提供道路 polyline points；若省略 points，會依 center/currentLocation/city 自動產生當前城市道路漫遊。預設 20km/h、10 分鐘、1 秒 tick。只規劃與寫 evidence，不需要真機。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "routeId": { "type": "string", "description": "可選：自訂 route id" },
                "points": { "type": "array", "description": "道路 polyline 點陣列，至少兩點；每點 {lat,lng}" },
                "roadPolyline": { "type": "array", "description": "同 points，語意上表示已由道路/地圖來源產生" },
                "route": { "type": "object", "description": "可選：{points:[{lat,lng},...]}" },
                "center": { "type": "object", "description": "省略 points 時使用的漫遊中心點 {lat,lng}" },
                "currentLocation": { "type": "object", "description": "省略 points 時使用的目前位置 {lat,lng}" },
                "city": { "type": "string", "description": "內建城市 fallback：taipei/new_taipei/taichung/kaohsiung；預設 taipei" },
                "radiusMeters": { "type": "number", "description": "城市漫遊半徑，預設 1500" },
                "waypoints": { "type": "integer", "description": "城市漫遊 seed waypoint 數，預設 8" },
                "roadProvider": { "type": "string", "description": "osrm/synthetic/none，預設 osrm" },
                "roadRouterUrl": { "type": "string", "description": "OSRM-compatible router base URL，可用 SIRIN_OSRM_BASE_URL 覆蓋" },
                "roadProfile": { "type": "string", "description": "OSRM profile，預設 driving" },
                "allowSyntheticFallback": { "type": "boolean", "description": "road router 失敗時是否允許退回非道路 synthetic；預設 false" },
                "routeSource": { "type": "string", "description": "可選：道路來源，例如 app_screen_road_trace/osrm/manual_road_polyline" },
                "movementProfile": { "type": "string", "description": "移動模型：roam（預設）或 bicycle。bicycle 會加入起步、巡航波動、轉彎減速與短停，且單段速度不超過 20.5km/h" },
                "speedKmh": { "type": "number", "description": "預設 20，範圍 1-130" },
                "durationSeconds": { "type": "integer", "description": "預設 600" },
                "tickSeconds": { "type": "integer", "description": "預設 1，範圍 1-60" },
                "includeSchedule": { "type": "boolean", "description": "是否回傳完整 tick 點；預設 false，只回 preview" },
                "note": { "type": "string", "description": "可選：operator 備註" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_route_plan),
    });

    m.insert("ios_dev_location_roam_plan", ToolDef {
        name:         "ios_dev_location_roam_plan",
        description: "依目前位置或城市中心自動產生 iOS developer-location 當前城市道路漫遊路線。預設 OSRM 道路路由、20km/h、10 分鐘、1 秒 tick；只規劃不寫入真機。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "routeId": { "type": "string", "description": "可選：自訂 route id" },
                "center": { "type": "object", "description": "漫遊中心點 {lat,lng}；優先使用" },
                "currentLocation": { "type": "object", "description": "目前位置 {lat,lng}" },
                "city": { "type": "string", "description": "內建城市 fallback：taipei/new_taipei/taichung/kaohsiung；預設 taipei" },
                "radiusMeters": { "type": "number", "description": "城市漫遊半徑，預設 1500" },
                "waypoints": { "type": "integer", "description": "seed waypoint 數，預設 8" },
                "roadProvider": { "type": "string", "description": "osrm/synthetic/none，預設 osrm" },
                "roadRouterUrl": { "type": "string", "description": "OSRM-compatible router base URL，可用 SIRIN_OSRM_BASE_URL 覆蓋" },
                "roadProfile": { "type": "string", "description": "OSRM profile，預設 driving" },
                "allowSyntheticFallback": { "type": "boolean", "description": "road router 失敗時是否允許退回非道路 synthetic；預設 false" },
                "movementProfile": { "type": "string", "description": "移動模型：roam（預設）或 bicycle。bicycle 會加入起步、巡航波動、轉彎減速與短停，且單段速度不超過 20.5km/h" },
                "speedKmh": { "type": "number", "description": "預設 20，範圍 1-130" },
                "durationSeconds": { "type": "integer", "description": "預設 600" },
                "tickSeconds": { "type": "integer", "description": "預設 1，範圍 1-60" },
                "includeSchedule": { "type": "boolean", "description": "是否回傳完整 tick 點；預設 false，只回 preview" },
                "note": { "type": "string", "description": "可選：operator 備註" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_roam_plan),
    });

    m.insert("ios_dev_location_route_status", ToolDef {
        name:         "ios_dev_location_route_status",
        description: "查詢 Sirin 本程序內 iOS developer-location road route plan。可指定 routeId；省略列全部。只讀。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "routeId": { "type": "string", "description": "可選：iOS developer location route id" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_route_status),
    });

    m.insert("ios_dev_location_route_start", ToolDef {
        name:         "ios_dev_location_route_start",
        description: "啟動已規劃的 iOS developer-location GPX 路線播放。使用 pymobiledevice3 simulate-location play，不依賴 3uTools；會產生 GPX、stdout/stderr log 與 evidence，立即回傳 pid。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "routeId": { "type": "string", "description": "必填：由 ios_dev_location_route_plan / roam_plan 回傳的 routeId" },
                "backend": { "type": "string", "description": "目前僅支援 pymobiledevice3，預設 pymobiledevice3" },
                "userspace": { "type": "boolean", "description": "是否加 --userspace，預設 true" }
            },
            "required": ["routeId"]
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_route_start),
    });

    m.insert("ios_dev_location_route_run_status", ToolDef {
        name:         "ios_dev_location_route_run_status",
        description: "讀取 iOS developer-location route_start 本地 run log，回傳 set-location 次數、第一/最後座標、累計移動距離、stdout/stderr 與 evidence 路徑。省略 routeRunId 時讀最新 run。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "routeRunId": { "type": "string", "description": "可選：route_start 回傳的 routeRunId；省略讀最新 route run" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_route_run_status),
    });

    m.insert("ios_dev_location_route_stop", ToolDef {
        name:         "ios_dev_location_route_stop",
        description: "停止 iOS developer-location route_start 啟動的 GPX 播放程序。可指定 routeRunId 或 pid；省略 routeRunId 時嘗試使用最新 route run evidence。停止後仍建議呼叫 ios_dev_location_reset 清除 developer simulated location。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "routeRunId": { "type": "string", "description": "可選：route_start 回傳的 routeRunId；省略時讀最新 route run" },
                "pid": { "type": "integer", "description": "可選：route_start 回傳的播放程序 pid；優先於 routeRunId evidence" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_dev_location_route_stop),
    });

    m.insert("ios_location_set_prepare", ToolDef {
        name:         "ios_location_set_prepare",
        description: "建立 iOS/3uTools 單點虛擬定位操作包。Sirin 產生 lat/lng、manual steps 與 evidence JSON；operator 在 3uTools Virtual Location 點 Modify virtual location 後再 confirm。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "string", "description": "可選：自訂 iOS location session id" },
                "lat": { "type": "number", "description": "緯度" },
                "lng": { "type": "number", "description": "經度" },
                "point": { "type": "object", "description": "可選：{lat,lng}" },
                "targetApp": { "type": "string", "description": "可選：正在測試的 app 名稱/package/bundle id" },
                "note": { "type": "string", "description": "可選：operator 備註" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_location_set_prepare),
    });

    m.insert("ios_location_restore_prepare", ToolDef {
        name:         "ios_location_restore_prepare",
        description: "建立 iOS/3uTools 恢復真實定位操作包。operator 在 3uTools Virtual Location 點 Restore true location；必要時重啟 iPhone。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "string", "description": "可選：自訂 restore session id" },
                "note": { "type": "string", "description": "可選：operator 備註" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_location_restore_prepare),
    });

    m.insert("ios_location_confirm", ToolDef {
        name:         "ios_location_confirm",
        description: "由 operator 回填 iOS/3uTools 定位操作結果。status 可用 APPLIED/RESTORED/FAILED/CANCELLED；只更新 Sirin 本地 session/evidence。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "string", "description": "iOS location session id" },
                "status": { "type": "string", "description": "APPLIED/RESTORED/FAILED/CANCELLED" },
                "note": { "type": "string", "description": "可選：驗證/錯誤備註" }
            },
            "required": ["sessionId"]
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_location_confirm),
    });

    m.insert("ios_location_session_status", ToolDef {
        name:         "ios_location_session_status",
        description: "查詢 Sirin 本程序內 iOS/3uTools 定位 session。可指定 sessionId；省略列全部。只讀。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "string", "description": "可選：iOS location session id" }
            }
        }),
        handler:      ToolHandler::SyncJson(crate::ios_location::call_ios_location_session_status),
    });

    // ── Physical iPhone control — Sirin-owned facade over the internal iOS
    // Driver.  These tools intentionally keep credentials, unlock, signing,
    // arbitrary URLs, messaging, orders, and payments outside the MCP surface.

    m.insert("ios_device_status", ToolDef {
        name:         "ios_device_status",
        description: "唯讀檢查 Sirin 的實體 iPhone provider，分開回報 DEVICE_DETECTED、INFO_READABLE、SCREEN_CAPTURE、SCREEN_CONTROL；不把連線或截圖誤當成控制證據。",
        input_schema: json!({"type": "object", "properties": {}}),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_device_status(args))),
    });

    m.insert("ios_screen_capture", ToolDef {
        name:         "ios_screen_capture",
        description: "透過 Sirin 擷取一張新鮮的實體 iPhone 畫面並存入 Sirin evidence。只證明 SCREEN_CAPTURE，不證明 SCREEN_CONTROL。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "label": { "type": "string", "description": "證據檔名標籤；選填" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_screen_capture(args))),
    });

    m.insert("ios_agoramarket_webview_evidence", ToolDef {
        name:         "ios_agoramarket_webview_evidence",
        description: "唯讀擷取目前前景 Telegram Mini App 中 AgoraMarket 正式站的實機 WebView 證據：綁定新鮮截圖、固定正式網域、版本 meta、啟動版本閘與已載入 Flutter runtime。不得傳入任意 JavaScript，不讀憑證，不改頁面；缺少已知舊快取前提時不宣稱快取更新 PASS。",
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_agoramarket_webview_evidence(args))),
    });

    m.insert("ios_control_session_start", ToolDef {
        name:         "ios_control_session_start",
        description: "取得 Sirin 管理的單一 iPhone 控制租約。預設 acceptance_browse_only；不授權密碼、信任、簽章、訊息、訂單或付款。",
        input_schema: json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": "工作階段標籤，例如 codex-agoramarket" },
                "policy": { "type": "string", "enum": ["acceptance_browse_only", "supervised_control"], "description": "預設 acceptance_browse_only" },
                "ttl_secs": { "type": "integer", "minimum": 30, "maximum": 900, "description": "租約秒數，預設 300" }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_control_session_start(args))),
    });

    m.insert(
        "ios_control_session_status",
        ToolDef {
            name: "ios_control_session_status",
            description: "唯讀查詢目前 iPhone 控制租約與最近一次 SCREEN_CONTROL 證據。",
            input_schema: json!({"type": "object", "properties": {}}),
            handler: ToolHandler::SyncJson(crate::ios_device::call_ios_control_session_status),
        },
    );

    m.insert(
        "ios_control_session_stop",
        ToolDef {
            name: "ios_control_session_stop",
            description:
                "釋放由 session_id 持有的 iPhone 控制租約，並強制停止及驗證 Sirin 自己建立的暫時 tunnel/WDA/forward 程序。Driver 服務本身保持被動常駐。",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["session_id"],
                "properties": {
                    "session_id": { "type": "string" }
                }
            }),
            handler: ToolHandler::AsyncJson(|args| {
                Box::pin(crate::ios_device::call_ios_control_session_stop(args))
            }),
        },
    );

    m.insert("ios_swipe", ToolDef {
        name:         "ios_swipe",
        description: "由 Sirin 操作實體 iPhone 滑動。acceptance_browse_only 只允許垂直捲動；動作前後各擷取新鮮畫面，排除狀態列與瀏覽器工具列後，只有內容區出現實質垂直位移才回報 SCREEN_CONTROL=PASS。",
        input_schema: json!({
            "type": "object",
            "required": ["session_id", "action_id", "x1", "y1", "x2", "y2"],
            "properties": {
                "session_id": { "type": "string" },
                "action_id": { "type": "string", "description": "重試時必須沿用同一 id，避免重複手勢" },
                "x1": { "type": "number" }, "y1": { "type": "number" },
                "x2": { "type": "number" }, "y2": { "type": "number" },
                "seconds": { "type": "number", "minimum": 0.05, "maximum": 3.0 },
                "settle_ms": { "type": "integer", "minimum": 200, "maximum": 10000 }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_swipe(args))),
    });

    m.insert("ios_home", ToolDef {
        name:         "ios_home",
        description: "由 Sirin 執行實體 iPhone Home 動作並以前後新鮮畫面驗證控制；需要有效租約與 action_id。",
        input_schema: action_schema(json!({}), &[]),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_home(args))),
    });

    m.insert("ios_recover_home", ToolDef {
        name:         "ios_recover_home",
        description: "受限的 iPhone WDA 恢復：只在使用者明確核准、無有效租約、WDA 已 down/wedged 且 Sirin Driver 為 loopback acceptance-only 時，透過 USB 回到 SpringBoard 並請求重啟 WDA。不能點擊、輸入、開 App 或網址。",
        input_schema: json!({
            "type": "object",
            "required": ["action_id"],
            "properties": {
                "action_id": { "type": "string", "description": "冪等鍵；回覆不明時必須沿用同一值" },
                "settle_ms": { "type": "integer", "minimum": 200, "maximum": 10000 }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_recover_home(args))),
    });

    m.insert("ios_open_app", ToolDef {
        name:         "ios_open_app",
        description: "由 Sirin 在實體 iPhone 開啟 Safari 或 Telegram；P0 不允許其他 App。需要有效租約與 action_id。",
        input_schema: action_schema(json!({
            "app": { "type": "string", "enum": ["safari", "telegram", "com.apple.mobilesafari", "ph.telegra.telegraph"] }
        }), &["app"]),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_open_app(args))),
    });

    m.insert("ios_open_route", ToolDef {
        name:         "ios_open_route",
        description: "由 Sirin 開啟固定驗收入口：AgoraMarket Safari、Telegram @agora_login_bot 或 Codex iPhone Remote PWA；不接受任意 URL，不送出 Telegram 訊息。",
        input_schema: action_schema(json!({
            "route": { "type": "string", "enum": ["safari_store", "telegram_bot", "codex_remote"] }
        }), &["route"]),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_open_route(args))),
    });

    m.insert("ios_acceptance_run", ToolDef {
        name:         "ios_acceptance_run",
        description: "以一次 Sirin 呼叫完成固定 iPhone 驗收入口：按需啟動被動鏈路、取得 acceptance_browse_only 租約、保存動作前後證據，最後驗證租約與 Sirin 暫時鏈路程序已釋放。只接受固定 route；run_id 是目前 Sirin process 內、綁定同一 route 的重試冪等鍵。",
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["run_id", "route"],
            "properties": {
                "run_id": { "type": "string", "description": "1-120 字元冪等鍵；回覆不明時必須沿用" },
                "route": { "type": "string", "enum": ["safari_store", "telegram_bot", "codex_remote"] },
                "owner": { "type": "string", "description": "選填工作標籤" },
                "ttl_secs": { "type": "integer", "minimum": 30, "maximum": 900 },
                "settle_ms": { "type": "integer", "minimum": 200, "maximum": 10000 }
            }
        }),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_acceptance_run(args))),
    });

    m.insert("ios_tap", ToolDef {
        name:         "ios_tap",
        description: "由 Sirin 點擊實體 iPhone。只在 supervised_control 明確啟用且 provider 非 acceptance-only 時可用；必須提供最新 expected_frame_sha256，避免點擊舊畫面。",
        input_schema: action_schema(json!({
            "x": { "type": "number" },
            "y": { "type": "number" },
            "expected_frame_sha256": { "type": "string", "description": "最近 ios_screen_capture 的 sha256" }
        }), &["x", "y", "expected_frame_sha256"]),
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::ios_device::call_ios_tap(args))),
    });

    m.insert(
        "research_sentinel_ack",
        ToolDef {
            name: "research_sentinel_ack",
            description: "將指定 News Research Sentinel inbox item 標記為 read。",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "inbox item id" }
                },
                "required": ["id"]
            }),
            handler: ToolHandler::SyncJson(crate::research_sentinel::call_research_sentinel_ack),
        },
    );

    m.insert("research_sentinel_review", ToolDef {
        name:         "research_sentinel_review",
        description: "回寫 Codex 對 News Research Sentinel inbox item 的驗收結果，形成研究閉環。accepted/convert_to_issue 會對符合條件事件嘗試寫入 KB；狀態可為 accepted、ignored、needs_more_sources、convert_to_issue。",
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
        handler:      ToolHandler::AsyncJson(|args| Box::pin(crate::research_sentinel::call_research_sentinel_review(args))),
    });
}

fn action_schema(extra_properties: Value, extra_required: &[&str]) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert("session_id".into(), json!({"type": "string"}));
    properties.insert(
        "action_id".into(),
        json!({
            "type": "string",
            "description": "idempotency key; retries must reuse the same value"
        }),
    );
    properties.insert(
        "settle_ms".into(),
        json!({"type": "integer", "minimum": 200, "maximum": 10000}),
    );
    if let Some(extra) = extra_properties.as_object() {
        properties.extend(extra.clone());
    }
    let mut required = vec!["session_id", "action_id"];
    required.extend_from_slice(extra_required);
    json!({
        "type": "object",
        "required": required,
        "properties": properties,
    })
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
        assert!(get("research_sentinel_monitor_status").is_some());
        assert!(get("a2a_worker_status").is_some());
        assert!(get("a2a_issue_candidates").is_some());
        assert!(get("ops_event_inbox").is_some());
        assert!(get("a2a_review_issue_candidate").is_some());
        assert!(get("a2a_handoff_queue").is_some());
        assert!(get("a2a_handoff_approval_packets").is_some());
        assert!(get("a2a_request_handoff_approval").is_some());
        assert!(get("a2a_handoff_approval_requests").is_some());
        assert!(get("a2a_update_handoff_approval_request").is_some());
        assert!(get("a2a_handoff_review_inbox").is_some());
        assert!(get("a2a_update_handoff").is_some());
        assert!(get("a2a_prepare_handoff_execution_packages").is_some());
        assert!(get("codex_supervisor_snapshot").is_some());
        assert!(get("codex_supervisor_report").is_some());
        assert!(get("codex_supervisor_claim").is_some());
        assert!(get("codex_supervisor_complete_action").is_some());
        assert!(get("ops_schedule_create").is_some());
        assert!(get("ops_review_queue_list").is_some());
        assert!(get("ops_review_claim").is_some());
        assert!(get("ops_review_update").is_some());
        assert!(get("ops_review_complete").is_some());
        assert!(get("ops_schedule_list").is_some());
        assert!(get("ops_schedule_digest").is_some());
        assert!(get("ops_schedule_update").is_some());
        assert!(get("ops_schedule_run_due").is_some());
        assert!(get("device_list").is_some());
        assert!(get("device_emulator_start").is_some());
        assert!(get("device_emulator_status").is_some());
        assert!(get("device_screenshot").is_some());
        assert!(get("device_app_launch").is_some());
        assert!(get("device_operator_login_start").is_some());
        assert!(get("device_operator_login_status").is_some());
        assert!(get("device_operator_login_confirm").is_some());
        assert!(get("device_geo_fix").is_some());
        assert!(get("device_geo_route_plan").is_some());
        assert!(get("device_geo_route_start").is_some());
        assert!(get("device_geo_roam_plan").is_some());
        assert!(get("device_geo_roam_start").is_some());
        assert!(get("device_geo_route_status").is_some());
        assert!(get("device_geo_route_stop").is_some());
        assert!(get("ios_3utools_status").is_some());
        assert!(get("ios_3utools_start").is_some());
        assert!(get("ios_dev_location_status").is_some());
        assert!(get("ios_dev_location_current").is_some());
        assert!(get("ios_location_report_latest").is_some());
        assert!(get("ios_dev_location_set").is_some());
        assert!(get("ios_dev_location_reset").is_some());
        assert!(get("ios_dev_location_session_status").is_some());
        assert!(get("ios_dev_location_route_plan").is_some());
        assert!(get("ios_dev_location_roam_plan").is_some());
        assert!(get("ios_dev_location_route_status").is_some());
        assert!(get("ios_dev_location_route_start").is_some());
        assert!(get("ios_dev_location_route_run_status").is_some());
        assert!(get("ios_device_status").is_some());
        assert!(get("ios_screen_capture").is_some());
        assert!(get("ios_agoramarket_webview_evidence").is_some());
        assert!(get("ios_control_session_start").is_some());
        assert!(get("ios_control_session_status").is_some());
        assert!(get("ios_control_session_stop").is_some());
        assert!(get("ios_swipe").is_some());
        assert!(get("ios_home").is_some());
        assert!(get("ios_recover_home").is_some());
        assert!(get("ios_open_app").is_some());
        assert!(get("ios_open_route").is_some());
        assert!(get("ios_acceptance_run").is_some());
        assert!(get("ios_tap").is_some());
        assert!(get("ios_dev_location_route_stop").is_some());
        assert!(get("ios_location_set_prepare").is_some());
        assert!(get("ios_location_restore_prepare").is_some());
        assert!(get("ios_location_confirm").is_some());
        assert!(get("ios_location_session_status").is_some());
        assert!(get("research_sentinel_ack").is_some());
        assert!(get("research_sentinel_review").is_some());

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

        for name in [
            "codex_supervisor_report",
            "codex_supervisor_claim",
            "codex_supervisor_complete_action",
        ] {
            let tool = get(name)
                .expect("seeded supervisor mutation tool")
                .to_json();
            assert_eq!(tool["annotations"]["readOnlyHint"], false);
            assert_eq!(tool["annotations"]["destructiveHint"], false);
            assert_eq!(tool["annotations"]["openWorldHint"], false);
        }
    }

    #[test]
    fn tool_def_renders_to_expected_json_shape() {
        let def = get("skill_list").expect("seeded");
        let json = def.to_json();
        assert_eq!(json["name"], "skill_list");
        assert_eq!(
            json["description"],
            "列出 Sirin 所有可用技能（含 YAML 動態技能）。"
        );
        assert!(json["inputSchema"].is_object());
    }

    #[test]
    fn iphone_observation_tools_are_explicitly_read_only() {
        for name in ["ios_device_status", "ios_control_session_status"] {
            let json = get(name).expect("seeded iPhone observation tool").to_json();
            assert_eq!(json["annotations"]["readOnlyHint"], true);
            assert_eq!(json["annotations"]["destructiveHint"], false);
            assert_eq!(json["annotations"]["idempotentHint"], true);
            assert_eq!(json["annotations"]["openWorldHint"], false);
        }

        assert!(
            get("ios_screen_capture")
                .expect("seeded iPhone capture tool")
                .to_json()["annotations"]
                .is_null(),
            "capture must retain the client's normal approval gate"
        );
        assert!(
            get("ios_swipe")
                .expect("seeded iPhone control tool")
                .to_json()["annotations"]
                .is_null(),
            "control must retain the client's normal approval gate"
        );
        assert!(
            get("ios_recover_home")
                .expect("seeded iPhone recovery tool")
                .to_json()["annotations"]
                .is_null(),
            "recovery must retain the client's normal approval gate"
        );
    }

    #[test]
    fn codex_remote_is_a_discoverable_fixed_iphone_route() {
        let json = get("ios_open_route")
            .expect("seeded iPhone fixed-route tool")
            .to_json();
        let routes = json["inputSchema"]["properties"]["route"]["enum"]
            .as_array()
            .expect("fixed route enum");
        assert!(routes.iter().any(|route| route == "codex_remote"));
        assert!(!routes
            .iter()
            .any(|route| route == "https://remote.purrtechllc.com/"));
    }

    #[test]
    fn iphone_acceptance_run_is_fixed_route_and_not_read_only() {
        let json = get("ios_acceptance_run")
            .expect("seeded iPhone composite acceptance tool")
            .to_json();
        assert_eq!(json["inputSchema"]["additionalProperties"], false);
        assert!(json["inputSchema"]["required"]
            .as_array()
            .expect("required fields")
            .iter()
            .any(|field| field == "run_id"));
        assert!(json["annotations"].is_null());
    }

    #[test]
    fn iphone_session_stop_cannot_retain_a_warm_link() {
        let json = get("ios_control_session_stop")
            .expect("seeded iPhone stop tool")
            .to_json();
        assert_eq!(json["inputSchema"]["additionalProperties"], false);
        assert!(json["inputSchema"]["properties"]["release_link"].is_null());
    }

    #[test]
    fn kb_tools_declare_read_and_idempotent_write_semantics() {
        for name in ["kb_get", "kb_search", "kb_stats"] {
            let json = get(name).expect("seeded KB read tool").to_json();
            assert_eq!(json["annotations"]["readOnlyHint"], true);
            assert_eq!(json["annotations"]["destructiveHint"], false);
            assert_eq!(json["annotations"]["idempotentHint"], true);
        }

        let write = get("kb_write").expect("seeded KB write tool").to_json();
        assert_eq!(write["annotations"]["readOnlyHint"], false);
        assert_eq!(write["annotations"]["destructiveHint"], false);
        assert_eq!(write["annotations"]["idempotentHint"], true);
        assert_eq!(
            write["inputSchema"]["properties"]["layer"]["default"],
            "topic"
        );
        assert_eq!(
            write["inputSchema"]["properties"]["status"]["default"],
            "confirmed"
        );
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
