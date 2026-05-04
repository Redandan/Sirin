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
        json!({
            "name":        self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        })
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

        // Un-migrated tools must NOT be in the registry yet.
        assert!(get("run_test_async").is_none());
        assert!(get("memory_search").is_none());
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
