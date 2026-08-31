//! Server-side A2A task worker.
//!
//! Sirin stays a runner: it polls an external MCP task service, claims work it
//! can execute locally, runs the matching Sirin capability, then reports the
//! outcome and artifacts back to the server.  Task ownership, retries, review
//! state, and cross-agent orchestration remain on the server side.

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const CONFIG_FILE: &str = "a2a_worker.yaml";
const DEFAULT_CLAIM_TOOL: &str = "claimAiTask";
const DEFAULT_UPDATE_TOOL: &str = "updateAiTaskStatus";
const RESEARCH_SENTINEL_TASK_TYPE: &str = "SIRIN_RESEARCH_SENTINEL_RUN_ONCE";
const AGORA_MARKET_OPS_DAILY_BRIEF_TASK_TYPE: &str = "AGORA_MARKET_OPS_DAILY_BRIEF";
const AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE: &str = "AGORA_MARKET_ISSUE_SCOUT";
const AGORA_MARKET_FIRST_ORDER_ACTIVATION_TASK_TYPE: &str = "AGORA_MARKET_FIRST_ORDER_ACTIVATION";
const SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE: &str = "SIRIN_AGORA_TEST_HEALTH_SCOUT";
const ISSUE_CANDIDATES_FILE: &str = "a2a_issue_candidates.jsonl";
const HANDOFF_QUEUE_FILE: &str = "a2a_handoff_queue.jsonl";
const HANDOFF_APPROVAL_REQUESTS_FILE: &str = "a2a_handoff_approval_requests.jsonl";
const DEFAULT_TEST_HEALTH_EXCLUDE_TAGS: &[&str] = &[
    "obsolete",
    "mutating",
    "requires-operator-approval",
    "interactive-media",
    "selector-risk",
    "needs-manual-verification",
    "external-issue",
];

#[derive(Debug, Clone, Deserialize)]
struct A2aWorkerFile {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_worker_id")]
    worker_id: String,
    #[serde(default)]
    server_url: String,
    #[serde(default = "default_bearer_env")]
    bearer_env: String,
    #[serde(default = "default_poll_interval_seconds")]
    poll_interval_seconds: u64,
    #[serde(default = "default_assignee_type")]
    assignee_type: String,
    #[serde(default = "default_claim_tool")]
    claim_tool: String,
    #[serde(default = "default_update_tool")]
    update_tool: String,
}

impl Default for A2aWorkerFile {
    fn default() -> Self {
        Self {
            enabled: false,
            worker_id: default_worker_id(),
            server_url: String::new(),
            bearer_env: default_bearer_env(),
            poll_interval_seconds: default_poll_interval_seconds(),
            assignee_type: default_assignee_type(),
            claim_tool: default_claim_tool(),
            update_tool: default_update_tool(),
        }
    }
}

#[derive(Debug, Clone)]
struct A2aWorkerConfig {
    enabled: bool,
    worker_id: String,
    server_url: String,
    bearer: Option<String>,
    agora_ops_authorization: Option<String>,
    bearer_env: String,
    poll_interval_seconds: u64,
    assignee_type: String,
    claim_tool: String,
    update_tool: String,
}

#[derive(Debug, Clone, Default, Serialize)]
struct A2aWorkerRuntimeStatus {
    enabled: bool,
    worker_id: String,
    server_url_configured: bool,
    poll_interval_seconds: u64,
    assignee_type: String,
    claim_tool: String,
    update_tool: String,
    capabilities: Vec<String>,
    last_poll_at: Option<String>,
    last_task_id: Option<String>,
    last_task_type: Option<String>,
    last_status: Option<String>,
    last_error: Option<String>,
    last_result_summary: Option<String>,
}

#[derive(Debug, Clone)]
struct A2aTask {
    id: String,
    task_type: String,
    params: Value,
}

static STATUS: OnceLock<Mutex<A2aWorkerRuntimeStatus>> = OnceLock::new();

fn status_cell() -> &'static Mutex<A2aWorkerRuntimeStatus> {
    STATUS.get_or_init(|| Mutex::new(A2aWorkerRuntimeStatus::default()))
}

fn default_worker_id() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .map(|host| format!("sirin-{}", host.to_lowercase()))
        .unwrap_or_else(|_| "sirin-local".into())
}

fn default_bearer_env() -> String {
    "KB_MCP_BEARER".into()
}

fn default_poll_interval_seconds() -> u64 {
    60
}

fn default_assignee_type() -> String {
    "SIRIN".into()
}

fn default_claim_tool() -> String {
    DEFAULT_CLAIM_TOOL.into()
}

fn default_update_tool() -> String {
    DEFAULT_UPDATE_TOOL.into()
}

fn env_bool(name: &str) -> Option<bool> {
    let v = std::env::var(name).ok()?.trim().to_lowercase();
    Some(matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

fn load_worker_file() -> A2aWorkerFile {
    let path = crate::platform::config_path(CONFIG_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_yaml::from_str(&content).unwrap_or_else(|e| {
            crate::sirin_log!("[a2a_worker] Failed to parse {CONFIG_FILE}: {e}");
            A2aWorkerFile::default()
        }),
        Err(_) => A2aWorkerFile::default(),
    }
}

fn load_config() -> A2aWorkerConfig {
    let file = load_worker_file();
    let bearer_env = std::env::var("SIRIN_A2A_BEARER_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(file.bearer_env);
    let bearer = std::env::var("SIRIN_A2A_BEARER")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var(&bearer_env)
                .ok()
                .filter(|v| !v.trim().is_empty())
        });
    let server_url = std::env::var("SIRIN_A2A_SERVER_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            if file.server_url.trim().is_empty() {
                None
            } else {
                Some(file.server_url)
            }
        })
        .or_else(|| {
            std::env::var("KB_MCP_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_default();
    let poll_interval_seconds = std::env::var("SIRIN_A2A_POLL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(file.poll_interval_seconds)
        .clamp(10, 3600);

    A2aWorkerConfig {
        enabled: env_bool("SIRIN_A2A_WORKER_ENABLED").unwrap_or(file.enabled),
        worker_id: std::env::var("SIRIN_A2A_WORKER_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(file.worker_id),
        server_url,
        bearer,
        agora_ops_authorization: load_agora_ops_authorization(),
        bearer_env,
        poll_interval_seconds,
        assignee_type: file.assignee_type,
        claim_tool: file.claim_tool,
        update_tool: file.update_tool,
    }
}

fn load_agora_ops_authorization() -> Option<String> {
    read_nonempty_env("SIRIN_AGORA_OPS_AUTHORIZATION")
        .or_else(|| read_nonempty_env("MCP_OPS_KEY"))
        .or_else(|| read_claude_mcp_ops_authorization("agora-ops"))
        .map(|value| normalize_ops_authorization(&value))
}

fn read_nonempty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn read_claude_mcp_ops_authorization(server: &str) -> Option<String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from)?;
    let src = std::fs::read_to_string(home.join(".claude.json")).ok()?;
    let value: Value = serde_json::from_str(&src).ok()?;
    claude_mcp_ops_authorization(&value, server)
}

fn claude_mcp_ops_authorization(value: &Value, server: &str) -> Option<String> {
    let headers = &value["mcpServers"][server]["headers"];
    [
        "X-OPS-Authorization",
        "X-Ops-Authorization",
        "x-ops-authorization",
    ]
    .iter()
    .find_map(|header| {
        headers[*header]
            .as_str()
            .filter(|token| !token.trim().is_empty())
            .map(str::to_owned)
    })
}

fn normalize_ops_authorization(value: &str) -> String {
    let trimmed = value.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let scheme = parts.next().unwrap_or_default();
    if scheme.eq_ignore_ascii_case("bearer") {
        return parts.next().unwrap_or_default().trim().to_string();
    }
    trimmed.to_string()
}

fn update_status(f: impl FnOnce(&mut A2aWorkerRuntimeStatus)) {
    let mut guard = status_cell().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard);
}

fn current_status_with_config() -> A2aWorkerRuntimeStatus {
    let cfg = load_config();
    let mut status = status_cell()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    status.enabled = cfg.enabled;
    status.worker_id = cfg.worker_id;
    status.server_url_configured = !cfg.server_url.trim().is_empty();
    status.poll_interval_seconds = cfg.poll_interval_seconds;
    status.assignee_type = cfg.assignee_type;
    status.claim_tool = cfg.claim_tool;
    status.update_tool = cfg.update_tool;
    status.capabilities = worker_capabilities();
    status
}

fn worker_capabilities() -> Vec<String> {
    vec![
        RESEARCH_SENTINEL_TASK_TYPE.into(),
        AGORA_MARKET_OPS_DAILY_BRIEF_TASK_TYPE.into(),
        AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE.into(),
        AGORA_MARKET_FIRST_ORDER_ACTIVATION_TASK_TYPE.into(),
        SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE.into(),
    ]
}

pub(crate) fn spawn_worker_loop() {
    let cfg = load_config();
    update_status(|status| {
        status.enabled = cfg.enabled;
        status.worker_id = cfg.worker_id.clone();
        status.server_url_configured = !cfg.server_url.trim().is_empty();
        status.poll_interval_seconds = cfg.poll_interval_seconds;
        status.assignee_type = cfg.assignee_type.clone();
        status.claim_tool = cfg.claim_tool.clone();
        status.update_tool = cfg.update_tool.clone();
        status.capabilities = worker_capabilities();
        if !cfg.enabled {
            status.last_status = Some("disabled".into());
        } else if cfg.server_url.trim().is_empty() {
            status.last_status = Some("not_configured".into());
            status.last_error =
                Some("SIRIN_A2A_SERVER_URL or a2a_worker.yaml server_url is required".into());
        }
    });

    if !cfg.enabled {
        crate::sirin_log!("[a2a_worker] disabled");
        return;
    }
    if cfg.server_url.trim().is_empty() {
        crate::sirin_log!("[a2a_worker] enabled but no server_url configured");
        return;
    }

    tokio::spawn(async move {
        crate::sirin_log!(
            "[a2a_worker] enabled: worker_id={} poll={}s tool={}",
            cfg.worker_id,
            cfg.poll_interval_seconds,
            cfg.claim_tool
        );
        loop {
            run_one_poll(&cfg).await;
            tokio::time::sleep(Duration::from_secs(cfg.poll_interval_seconds)).await;
        }
    });
}

async fn run_one_poll(cfg: &A2aWorkerConfig) {
    update_status(|status| {
        status.last_poll_at = Some(Utc::now().to_rfc3339());
        status.last_status = Some("polling".into());
        status.last_error = None;
    });

    let claim_args = json!({
        "workerId": cfg.worker_id,
        "assigneeType": cfg.assignee_type,
    });
    let claim = crate::mcp_client::call_tool_with_bearer(
        &cfg.server_url,
        &cfg.claim_tool,
        claim_args,
        cfg.bearer.as_deref(),
    )
    .await;

    let task = match claim {
        Ok(value) => match extract_task(&value) {
            Ok(task) => task,
            Err(e) => {
                update_status(|status| {
                    status.last_status = Some("idle".into());
                    status.last_error = Some(e);
                });
                return;
            }
        },
        Err(e) => {
            update_status(|status| {
                status.last_status = Some("poll_failed".into());
                status.last_error = Some(e);
            });
            return;
        }
    };

    let Some(task) = task else {
        update_status(|status| {
            status.last_status = Some("idle".into());
            status.last_task_id = None;
            status.last_task_type = None;
        });
        return;
    };

    update_status(|status| {
        status.last_task_id = Some(task.id.clone());
        status.last_task_type = Some(task.task_type.clone());
        status.last_status = Some("running".into());
    });

    let _ = report_task_status(cfg, &task.id, "RUNNING", None, json!([]), json!(null)).await;

    let outcome = execute_task(cfg, &task).await;
    match outcome {
        Ok(result) => {
            if result
                .get("issue_candidates")
                .and_then(Value::as_array)
                .is_some()
            {
                if let Err(e) = persist_issue_candidates_for_task(&task.id, &result) {
                    crate::sirin_log!("[a2a_worker] failed to persist issue candidates: {e}");
                }
            }
            let artifacts = artifacts_from_result(&result);
            let summary = result
                .get("summary")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(240).collect::<String>());
            let report =
                report_task_status(cfg, &task.id, "NEEDS_REVIEW", None, artifacts, result).await;
            update_status(|status| {
                status.last_status = Some(
                    if report.is_ok() {
                        "reported"
                    } else {
                        "report_failed"
                    }
                    .into(),
                );
                status.last_error = report.err();
                status.last_result_summary = summary;
            });
        }
        Err(e) => {
            let _ = report_task_status(
                cfg,
                &task.id,
                "FAILED",
                Some(e.clone()),
                json!([]),
                json!(null),
            )
            .await;
            update_status(|status| {
                status.last_status = Some("failed".into());
                status.last_error = Some(e);
            });
        }
    }
}

async fn execute_task(cfg: &A2aWorkerConfig, task: &A2aTask) -> Result<Value, String> {
    match task.task_type.as_str() {
        RESEARCH_SENTINEL_TASK_TYPE => {
            let mut args = task.params.clone();
            if !args.is_object() {
                args = json!({});
            }
            normalize_research_params(&mut args);
            if args.get("create_inbox").is_none() {
                args["create_inbox"] = json!(true);
            }
            if args.get("create_report").is_none() {
                args["create_report"] = json!("auto");
            }
            crate::research_sentinel::call_research_sentinel_run_once(args).await
        }
        AGORA_MARKET_OPS_DAILY_BRIEF_TASK_TYPE => run_agora_market_ops_daily_brief(cfg, task).await,
        AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE => run_agora_market_issue_scout(cfg, task).await,
        AGORA_MARKET_FIRST_ORDER_ACTIVATION_TASK_TYPE => {
            run_agora_market_first_order_activation(cfg, task).await
        }
        SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE => run_sirin_agora_test_health_scout(task).await,
        other => Err(format!("Unsupported A2A task_type: {other}")),
    }
}

async fn run_agora_market_ops_daily_brief(
    cfg: &A2aWorkerConfig,
    task: &A2aTask,
) -> Result<Value, String> {
    if cfg.server_url.trim().is_empty() {
        return Err("AGORA_MARKET_OPS_DAILY_BRIEF requires SIRIN_A2A_SERVER_URL or a2a_worker.yaml server_url".into());
    }

    let days = bounded_u64_param(&task.params, "days", 7, 1, 90) as i64;
    let overdue_hours = bounded_u64_param(&task.params, "hoursOverdue", 12, 1, 168) as i64;
    let include_customer_support = bool_param(&task.params, "includeCustomerSupport", true);
    let include_sourcing = bool_param(&task.params, "includeSourcing", true);

    let mut tool_calls = vec![
        OpsToolCall::new("storeHealthCheck", json!({})),
        OpsToolCall::new("getOpsBacklog", json!({})),
        OpsToolCall::new("listPendingOrders", json!({ "days": days })),
        OpsToolCall::new("getDisputeBacklog", json!({ "days": days })),
        OpsToolCall::new("getOrderFlowFunnel", json!({ "days": days })),
        OpsToolCall::new("getGroupAttributionReport", json!({ "days": days })),
    ];
    if include_customer_support {
        tool_calls.push(OpsToolCall::new("getBotKnowledgeGaps", json!({})));
        tool_calls.push(OpsToolCall::new("getKnowledgeBaseStats", json!({})));
    }
    if include_sourcing {
        tool_calls.push(OpsToolCall::new(
            "listPendingFulfillmentOrders",
            json!({ "hoursOverdue": overdue_hours }),
        ));
        tool_calls.push(OpsToolCall::new("listSuppliers", json!({})));
    }

    let mut sections = Vec::new();
    let mut failures = Vec::new();
    let auth_probe = agora_ops_direct_auth_probe(cfg, &tool_calls);
    if cfg.agora_ops_authorization.is_none() {
        let error = "Sirin OPS MCP authorization is missing; configure SIRIN_AGORA_OPS_AUTHORIZATION, MCP_OPS_KEY, or ~/.claude.json mcpServers.agora-ops.headers.X-OPS-Authorization.";
        failures.push(json!({
            "tool": "opsAuthorization",
            "error": error,
            "authProbe": auth_probe,
        }));
        sections.push(json!({
            "tool": "opsAuthorization",
            "arguments": { "requestedTools": tool_calls.iter().map(|call| call.name).collect::<Vec<_>>() },
            "ok": false,
            "error": error,
            "authProbe": auth_probe,
            "evidenceType": "mcp_auth_required",
        }));
        let summary = build_ops_summary(days, overdue_hours, &sections, failures.len());
        return Ok(json!({
            "task_type": AGORA_MARKET_OPS_DAILY_BRIEF_TASK_TYPE,
            "summary": summary,
            "boundary": "READ_ONLY; no Telegram sends, no order/product/fund/wallet/trading mutations.",
            "window": {
                "days": days,
                "hours_overdue": overdue_hours,
            },
            "auth_probe": auth_probe,
            "sections": sections,
            "failures": failures,
            "generated_at": Utc::now().to_rfc3339(),
        }));
    }
    for call in tool_calls {
        let value = crate::mcp_client::call_tool_with_auth_headers(
            &cfg.server_url,
            call.name,
            call.arguments.clone(),
            None,
            cfg.agora_ops_authorization.as_deref(),
        )
        .await;
        match value {
            Ok(v) => sections.push(json!({
                "tool": call.name,
                "arguments": call.arguments,
                "ok": true,
                "text": tool_text(&v),
                "raw": v,
            })),
            Err(e) => {
                let error_data = mcp_error_data_from_text(&e);
                failures.push(json!({
                    "tool": call.name,
                    "error": e,
                    "errorData": error_data.clone().unwrap_or(Value::Null),
                }));
                sections.push(json!({
                    "tool": call.name,
                    "arguments": call.arguments,
                    "ok": false,
                    "error": failures.last().and_then(|f| f.get("error")).cloned().unwrap_or(json!("unknown error")),
                    "errorData": error_data.unwrap_or(Value::Null),
                }));
            }
        }
    }

    let summary = build_ops_summary(days, overdue_hours, &sections, failures.len());
    Ok(json!({
        "task_type": AGORA_MARKET_OPS_DAILY_BRIEF_TASK_TYPE,
        "summary": summary,
        "boundary": "READ_ONLY; no Telegram sends, no order/product/fund/wallet/trading mutations.",
        "window": {
            "days": days,
            "hours_overdue": overdue_hours,
        },
        "auth_probe": auth_probe,
        "sections": sections,
        "failures": failures,
        "generated_at": Utc::now().to_rfc3339(),
    }))
}

async fn run_agora_market_issue_scout(
    cfg: &A2aWorkerConfig,
    task: &A2aTask,
) -> Result<Value, String> {
    let brief = run_agora_market_ops_daily_brief(cfg, task).await?;
    let max_candidates = bounded_u64_param(&task.params, "maxCandidates", 12, 1, 50) as usize;
    let mut candidates = build_issue_candidates(&brief, max_candidates);
    let local_test_health = if bool_param(&task.params, "includeTestHealth", true) {
        Some(run_embedded_test_health_scout(task, max_candidates).await)
    } else {
        None
    };
    if let Some(Ok(health)) = local_test_health.as_ref() {
        if let Some(local_candidates) = health.get("issue_candidates").and_then(Value::as_array) {
            candidates.extend(local_candidates.iter().cloned());
        }
    }
    let local_health_report = match local_test_health.as_ref() {
        Some(Ok(value)) => Some(value.clone()),
        Some(Err(error)) => Some(json!({
            "task_type": SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE,
            "summary": format!("Sirin local test health scout failed: {error}"),
            "error": error,
            "candidate_count": 0,
            "issue_candidates": [],
            "generated_at": Utc::now().to_rfc3339(),
        })),
        None => None,
    };
    let persisted_records = load_issue_candidate_records().unwrap_or_default();
    let (report_candidates, current_tracked_records) =
        split_current_candidates_with_tracked_reviews(&task.id, &candidates, &persisted_records);
    let mut inbox_records = load_report_inbox_records(50).unwrap_or_default();
    if local_test_health_has_no_current_candidates(local_health_report.as_ref()) {
        inbox_records.retain(|record| !is_local_test_health_inbox_record(record));
    }
    let current_tracked_dedupe_keys: HashSet<String> = current_tracked_records
        .iter()
        .filter_map(|record| {
            record
                .get("dedupe_key")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    let current_tracked_operation_keys: HashSet<String> = current_tracked_records
        .iter()
        .map(record_operation_key)
        .filter(|key| !key.is_empty())
        .collect();
    if !current_tracked_dedupe_keys.is_empty() || !current_tracked_operation_keys.is_empty() {
        inbox_records.retain(|record| {
            !record
                .get("dedupe_key")
                .and_then(Value::as_str)
                .is_some_and(|key| current_tracked_dedupe_keys.contains(key))
                && !current_tracked_operation_keys.contains(&record_operation_key(record))
        });
    }
    inbox_records.extend(current_tracked_records);
    let ops_report = build_ops_report(
        &brief,
        &report_candidates,
        local_health_report.as_ref(),
        &inbox_records,
    );
    let summary = build_issue_scout_summary(&candidates);
    Ok(json!({
        "task_type": AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE,
        "summary": summary,
        "boundary": "READ_ONLY; Sirin only scouts evidence and proposes candidates. ChatGPT/Codex/Human must verify before action.",
        "candidate_count": candidates.len(),
        "ops_report": ops_report,
        "issue_candidates": candidates,
        "source_brief": brief,
        "source_test_health": local_health_report,
        "generated_at": Utc::now().to_rfc3339(),
    }))
}

async fn run_agora_market_first_order_activation(
    cfg: &A2aWorkerConfig,
    task: &A2aTask,
) -> Result<Value, String> {
    let mut scout_task = task.clone();
    scout_task.task_type = AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE.into();
    if !scout_task.params.is_object() {
        scout_task.params = json!({});
    }
    if scout_task.params.get("includeTestHealth").is_none() {
        scout_task.params["includeTestHealth"] = json!(true);
    }
    if scout_task.params.get("maxCandidates").is_none() {
        scout_task.params["maxCandidates"] = json!(12);
    }

    let scout = run_agora_market_issue_scout(cfg, &scout_task).await?;
    let activation = build_first_order_activation_playbook(&scout, &task.params);
    let mut issue_candidates = scout
        .get("issue_candidates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    issue_candidates.extend(build_activation_handoff_candidates(&activation));
    let codex_review_actions = crate::ops_action_ledger::persist_codex_review_actions(
        &task.id,
        task.params
            .get("sourceScheduleId")
            .or_else(|| task.params.get("source_schedule_id"))
            .and_then(Value::as_str),
        &activation,
    )?;
    let summary = activation
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("AgoraMarket first-order activation playbook generated.")
        .to_string();

    Ok(json!({
        "task_type": AGORA_MARKET_FIRST_ORDER_ACTIVATION_TASK_TYPE,
        "summary": summary,
        "boundary": "READ_ONLY; Sirin creates an activation playbook and handoff plan only. It does not place orders, send messages, mutate products, or modify AgoraMarket/API state.",
        "candidate_count": issue_candidates.len(),
        "first_order_activation": activation,
        "codex_review_actions": codex_review_actions,
        "ops_report": scout.get("ops_report").cloned().unwrap_or(Value::Null),
        "issue_candidates": issue_candidates,
        "source_issue_scout": scout,
        "generated_at": Utc::now().to_rfc3339(),
    }))
}

fn local_test_health_has_no_current_candidates(local_test_health: Option<&Value>) -> bool {
    let Some(health) = local_test_health else {
        return false;
    };
    if health.get("error").is_some() {
        return false;
    }
    health
        .get("candidate_count")
        .and_then(Value::as_u64)
        .is_some_and(|count| count == 0)
}

fn build_first_order_activation_playbook(scout: &Value, params: &Value) -> Value {
    let ops_report = scout.get("ops_report").unwrap_or(&Value::Null);
    let source_brief = scout.get("source_brief").unwrap_or(&Value::Null);
    let source_test_health = scout.get("source_test_health").unwrap_or(&Value::Null);
    let store_health = section_text(source_brief, "storeHealthCheck");
    let order_funnel = section_text(source_brief, "getOrderFlowFunnel");
    let pending_orders = section_text(source_brief, "listPendingOrders");
    let support_gaps = section_text(source_brief, "getBotKnowledgeGaps");
    let product_counts = parse_store_health_counts(&store_health);
    let production_orders = product_counts
        .get("productionOrders30d")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let production_gmv = product_counts
        .get("productionGmv30d")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let goal = params
        .get("goal")
        .and_then(Value::as_str)
        .unwrap_or("FIRST_PRODUCTION_ORDER")
        .trim()
        .to_ascii_uppercase();
    let repeatable_order_goal =
        goal == "FIRST_FIVE_PRODUCTION_ORDERS" || goal == "REPEATABLE_PRODUCTION_ORDERS";
    let target_production_orders = if repeatable_order_goal { 5 } else { 1 };
    let buyer_path_ready = source_test_health
        .pointer("/source_health/aggregate/failed")
        .and_then(Value::as_u64)
        .is_some_and(|failed| failed == 0);
    let tracked_external = ops_report
        .get("trackedExternalIssues")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut tracked_issue_urls: Vec<String> = tracked_external
        .iter()
        .filter_map(|issue| issue.pointer("/externalIssue/url").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    tracked_issue_urls.extend(extract_issue_url_params(params, "trackedIssues"));
    tracked_issue_urls.extend(extract_issue_url_params(params, "tracked_issues"));
    dedupe_strings_preserve_order(&mut tracked_issue_urls);
    let tracked_issue_values: Vec<Value> = tracked_issue_urls
        .iter()
        .cloned()
        .map(Value::String)
        .collect();
    let known_blocker = params
        .get("knownBlocker")
        .or_else(|| params.get("known_blocker"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let active_known_blocker = known_blocker
        .as_ref()
        .filter(|blocker| !is_resolved_known_blocker(blocker));
    let required_verification = extract_string_params(params, "requiredVerification")
        .into_iter()
        .chain(extract_string_params(params, "required_verification"))
        .collect::<Vec<_>>();
    let mut required_verification = required_verification;
    dedupe_strings_preserve_order(&mut required_verification);
    let support_gap_items = extract_bot_knowledge_gap_questions(&support_gaps);
    let pending_shipment_count = pending_orders.matches("PENDING_SHIPMENT").count() as u64;
    let success = if repeatable_order_goal {
        production_orders >= target_production_orders
            && production_gmv > 0.0
            && active_known_blocker.is_none()
    } else {
        production_orders > 0 || production_gmv > 0.0
    };

    let mut current_state = json!({
        "successMetric": params.get("successMetric").and_then(Value::as_str).unwrap_or("production_orders >= 1 OR production_gmv > 0"),
        "targetProductionOrders": target_production_orders,
        "productionOrders30d": production_orders,
        "productionGmv30d": production_gmv,
        "remainingProductionOrdersToTarget": target_production_orders.saturating_sub(production_orders),
        "pendingShipmentCount": pending_shipment_count,
        "onSaleProducts": product_counts.get("onSaleProducts").cloned().unwrap_or(Value::Null),
        "totalProducts": product_counts.get("totalProducts").cloned().unwrap_or(Value::Null),
        "buyerPathReady": buyer_path_ready,
        "opsChecksReady": ops_report.pointer("/readOnlyChecks/failed").and_then(Value::as_u64).is_some_and(|failed| failed == 0),
        "trackedIssues": tracked_issue_values,
        "orderFunnelExcerpt": evidence_excerpt(&order_funnel),
        "pendingOrdersExcerpt": evidence_excerpt(&pending_orders),
    });
    if let Some(blocker) = &known_blocker {
        current_state["knownBlocker"] = json!(blocker);
        current_state["knownBlockerStatus"] = json!(if active_known_blocker.is_some() {
            "active"
        } else {
            "resolved"
        });
    }
    if !required_verification.is_empty() {
        current_state["requiredVerification"] = json!(required_verification);
    }

    let blockers = if success {
        vec![json!({
            "type": "goal_reached",
            "severity": "INFO",
            "summary": if repeatable_order_goal {
                "Repeatable-order target is reached for the current window."
            } else {
                "At least one production order/GMV is present."
            },
            "nextActor": "SIRIN",
            "nextAction": if repeatable_order_goal {
                "Prepare repeatable-order retrospective and keep monitoring retention, fulfillment, and support."
            } else {
                "Prepare first-order retrospective and move to repeatable first 5 orders."
            }
        })]
    } else {
        let mut items = Vec::new();
        if let Some(blocker) = active_known_blocker {
            items.push(json!({
                "type": "known_blocker",
                "severity": "P0",
                "summary": blocker,
                "trackedIssues": current_state["trackedIssues"].clone(),
                "requiredVerification": current_state.get("requiredVerification").cloned().unwrap_or_else(|| json!([])),
                "nextActor": "CODEX",
                "nextAction": "Track the known blocker issue(s), then rerun first-order verification after the blocker is fixed."
            }));
        }
        if repeatable_order_goal && production_orders < target_production_orders {
            items.push(json!({
                "type": "repeatable_order_gap",
                "severity": "P0",
                "summary": format!(
                    "Production orders are {production_orders}/{target_production_orders}; orders #2-#5 still need a repeatable acquisition and checkout path."
                ),
                "evidence": evidence_excerpt(&store_health),
                "nextActor": "SIRIN",
                "nextAction": "Keep the daily repeatable-order patrol active and rank checkout/cart, fulfillment, support, supplier, and acquisition blockers."
            }));
        }
        if production_orders == 0 && production_gmv == 0.0 {
            items.push(json!({
                "type": "conversion_gap",
                "severity": "P0",
                "summary": "Products are on sale but production orders/GMV are still zero.",
                "evidence": evidence_excerpt(&store_health),
                "trackedIssue": current_state["trackedIssues"].as_array().and_then(|items| items.first()).cloned().unwrap_or(Value::Null),
                "nextActor": "OPERATOR",
                "nextAction": "Use the tracked issue to drive a first-order activation action today."
            }));
        }
        if !buyer_path_ready {
            items.push(json!({
                "type": "buyer_path_risk",
                "severity": "P0",
                "summary": "Buyer path/test health is not confirmed healthy.",
                "nextActor": "CODEX",
                "nextAction": "Run or fix buyer path smoke before sending traffic."
            }));
        }
        if repeatable_order_goal && pending_shipment_count > 0 {
            items.push(json!({
                "type": "fulfillment_followup",
                "severity": "P1",
                "summary": format!("{pending_shipment_count} pending shipment order(s) need fulfillment follow-up before repeat purchases can be trusted."),
                "evidence": evidence_excerpt(&pending_orders),
                "nextActor": "CHATGPT",
                "nextAction": "Triage pending shipment evidence into an operator follow-up; do not message customers or mutate orders without approval."
            }));
        }
        if !support_gap_items.is_empty() {
            items.push(json!({
                "type": "support_readiness_gap",
                "severity": "P1",
                "summary": "Bot knowledge gaps include buyer onboarding/support content that can reduce first-order friction.",
                "questions": support_gap_items,
                "nextActor": "CHATGPT",
                "nextAction": "Draft buyer-facing FAQ/CTA answers for operator review."
            }));
        }
        items
    };

    let today_actions = if success {
        vec![json!({
            "actor": "SIRIN",
            "action": if repeatable_order_goal {
                "Generate a repeatable-order retrospective and keep the daily monitor scheduled."
            } else {
                "Mark the P0 first-order schedule DONE and generate a first-order retrospective."
            },
            "expectedResult": if repeatable_order_goal {
                "Repeatable-order evidence is preserved for the next growth cycle."
            } else {
                "First-order evidence is preserved for next growth target."
            },
            "verification": if repeatable_order_goal {
                "storeHealthCheck production_orders_30d >= 5 and production_gmv_30d > 0."
            } else {
                "storeHealthCheck production_orders >= 1 or production_gmv > 0."
            }
        })]
    } else if repeatable_order_goal {
        vec![
            json!({
                "actor": "SIRIN",
                "action": "Keep the P0 repeatable-order schedule active and preserve the current orders/GMV baseline.",
                "expectedResult": "Each run shows progress toward 5 production orders or a ranked blocker list.",
                "verification": "Next run compares productionOrders30d, productionGmv30d, pending shipments, and issue candidates."
            }),
            json!({
                "actor": "CHATGPT",
                "action": "Turn pending shipment and support-gap evidence into operator-facing triage copy.",
                "expectedResult": "Operator can decide fulfillment/support actions without Sirin mutating business state.",
                "verification": "Handoff or issue candidate is reviewed before external action."
            }),
            json!({
                "actor": "CODEX",
                "action": "Track checkout/cart blocker issues and rerun dry-run only after the external blocker changes.",
                "expectedResult": "Sirin does not treat first-order success as completion of repeatable conversion.",
                "verification": "agora_makro_th_checkout_dry stops before payment confirmation when rerun."
            }),
        ]
    } else {
        vec![
            json!({
                "actor": "OPERATOR",
                "action": "Pick 1-3 ON_SALE products from the current catalog and publish a first-order CTA to the highest-intent channel.",
                "expectedResult": "At least one real buyer attempts checkout.",
                "verification": "Next Sirin run sees production order/GMV or a concrete checkout/support blocker."
            }),
            json!({
                "actor": "CODEX",
                "action": "Verify buyer path for the selected product(s): product detail -> checkout -> payment/order attempt, read-only until explicit operator approval.",
                "expectedResult": "Buyer path is confirmed or a specific blocker issue is opened.",
                "verification": "Sirin active-smoke remains healthy and any blocker has issue/handoff evidence."
            }),
            json!({
                "actor": "CHATGPT",
                "action": "Turn support knowledge gaps into short buyer FAQ and CTA copy.",
                "expectedResult": "Operator has copy to answer first-buyer objections.",
                "verification": "Handoff is ACKED/DONE and support gaps are reduced or explicitly classified."
            }),
        ]
    };

    let boundary = params
        .get("boundary")
        .and_then(Value::as_str)
        .unwrap_or("Sirin produces a read-only activation playbook. Human/operator or downstream agents must execute any outreach, product, code, or production-data changes.");

    let summary = if success {
        if repeatable_order_goal {
            format!(
                "Repeatable-order target reached: {production_orders}/{target_production_orders} production orders in 30d."
            )
        } else {
            "First production order/GMV evidence is present; prepare retrospective.".to_string()
        }
    } else if repeatable_order_goal {
        format!(
            "Repeatable-order target is not reached yet: {production_orders}/{target_production_orders} production orders in 30d; keep daily ops patrol active."
        )
    } else if let Some(blocker) = active_known_blocker {
        format!("First production order is not achieved yet; active blocker: {blocker}")
    } else {
        "First production order is not achieved yet; execute the activation actions and rerun Sirin.".to_string()
    };
    let operations_cycle_report = if repeatable_order_goal {
        json!({
            "reportType": "AGORA_MARKET_REPEATABLE_ORDER_CYCLE",
            "status": if success { "target_reached" } else { "needs_operational_followup" },
            "orderProgress": {
                "productionOrders30d": production_orders,
                "targetProductionOrders": target_production_orders,
                "remainingProductionOrdersToTarget": target_production_orders.saturating_sub(production_orders),
                "productionGmv30d": production_gmv,
                "firstOrderEvidence": if production_orders > 0 || production_gmv > 0.0 {
                    "observed"
                } else {
                    "missing"
                }
            },
            "largestBlockers": blockers.clone(),
            "handoffPlan": {
                "sirin": [
                    "Keep the local daily repeatable-order schedule active.",
                    "Preserve each run's order/GMV, pending-shipment, support-gap, and issue-blocker evidence.",
                    "Fix Sirin-local false positives or report-shape gaps directly."
                ],
                "codex": [
                    "Verify checkout/cart blocker evidence before opening or updating external issues.",
                    "Rerun checkout dry-runs only after tracked external blockers change, and stop before payment confirmation.",
                    "Modify Sirin only; AgoraMarket/AgoraMarketAPI work remains issue or handoff unless explicitly approved."
                ],
                "chatgpt": [
                    "Draft operator-facing fulfillment follow-up from pending-shipment evidence.",
                    "Turn support knowledge gaps into FAQ/CTA candidates for operator review."
                ],
                "operator": [
                    "Approve any customer message, seller action, order mutation, payment action, or production business change.",
                    "Choose acquisition/CTA actions for orders #2-#5 after reviewing Sirin evidence."
                ]
            },
            "localSchedule": {
                "canContinue": true,
                "recommendedCadence": "daily",
                "nextCheck": "Wait for the next scheduled Sirin-local repeatable-order run unless #257 or another blocker changes.",
                "requiredBoundary": boundary
            },
            "sirinCapabilityGaps": [
                "External issue status is still supplied through tracked issue context; Sirin should sync issue state before deciding whether to rerun checkout dry-runs.",
                "Fulfillment and support follow-up are represented as review tasks; Sirin still needs an approval-gated handoff execution path before it can contact users or sellers.",
                "Acquisition quality is inferred from attribution and order counts; Sirin still needs richer source/channel attribution to choose traffic actions without operator judgment."
            ]
        })
    } else {
        Value::Null
    };

    json!({
        "schemaVersion": 1,
        "goal": if repeatable_order_goal { goal.as_str() } else { "FIRST_PRODUCTION_ORDER" },
        "status": if success { "goal_reached" } else { "activation_required" },
        "summary": summary,
        "boundary": boundary,
        "currentState": current_state,
        "topBlockers": blockers,
        "todayActions": today_actions,
        "operationsCycleReport": operations_cycle_report,
        "verification": {
            "successCondition": if repeatable_order_goal {
                "storeHealthCheck shows production_orders_30d >= 5, production_gmv_30d > 0, and no active checkout/cart blocker"
            } else {
                "storeHealthCheck shows production_orders >= 1 or production_gmv > 0"
            },
            "nextCheck": "Rerun AGORA_MARKET_FIRST_ORDER_ACTIVATION after operator/ChatGPT/Codex actions.",
            "failureEscalation": "If still zero after 3 daily runs, split tracked issue into exposure, buyer-path, checkout/payment, and trust/support blockers.",
            "requiredVerification": current_state.get("requiredVerification").cloned().unwrap_or_else(|| json!([]))
        }
    })
}

fn build_activation_handoff_candidates(activation: &Value) -> Vec<Value> {
    if activation.get("goal").and_then(Value::as_str) != Some("FIRST_FIVE_PRODUCTION_ORDERS") {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    let Some(blockers) = activation.get("topBlockers").and_then(Value::as_array) else {
        return candidates;
    };
    for blocker in blockers {
        let blocker_type = blocker
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match blocker_type {
            "fulfillment_followup" => {
                let evidence = blocker
                    .get("evidence")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if evidence.trim().is_empty() {
                    continue;
                }
                candidates.push(issue_candidate_with_source(
                    AGORA_MARKET_FIRST_ORDER_ACTIVATION_TASK_TYPE,
                    "FULFILLMENT_FOLLOWUP",
                    blocker
                        .get("severity")
                        .and_then(Value::as_str)
                        .unwrap_or("P1"),
                    0.78,
                    "Review pending shipment follow-up for repeatable orders".to_string(),
                    "listPendingOrders",
                    "mcp_output",
                    evidence,
                    "CHATGPT",
                    "draft_operator_fulfillment_followup",
                ));
            }
            "support_readiness_gap" => {
                let questions = blocker
                    .get("questions")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|question| format!("- {question}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if questions.trim().is_empty() {
                    continue;
                }
                let evidence = format!(
                    "Support knowledge gaps can reduce repeatable-order conversion:\n{questions}"
                );
                candidates.push(issue_candidate_with_source(
                    AGORA_MARKET_FIRST_ORDER_ACTIVATION_TASK_TYPE,
                    "GROWTH_GAP",
                    blocker
                        .get("severity")
                        .and_then(Value::as_str)
                        .unwrap_or("P1"),
                    0.72,
                    "Convert repeatable-order support gaps into FAQ/CTA handoff".to_string(),
                    "getBotKnowledgeGaps",
                    "activation_playbook",
                    &evidence,
                    "CHATGPT",
                    "draft_kb_or_faq",
                ));
            }
            _ => {}
        }
    }
    candidates
}

fn is_resolved_known_blocker(blocker: &str) -> bool {
    let normalized = blocker.trim().to_ascii_lowercase();
    normalized.starts_with("resolved:")
        || normalized.starts_with("fixed:")
        || normalized.starts_with("closed:")
        || normalized.starts_with("done:")
        || normalized.starts_with("resolved -")
        || normalized.starts_with("fixed -")
        || normalized.starts_with("closed -")
        || normalized.starts_with("done -")
}

fn extract_issue_url_params(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .or_else(|| item.pointer("/externalIssue/url").and_then(Value::as_str))
                        .or_else(|| item.get("url").and_then(Value::as_str))
                })
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn extract_string_params(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn dedupe_strings_preserve_order(items: &mut Vec<String>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.clone()));
}

fn section_text(brief: &Value, tool_name: &str) -> String {
    let text = brief
        .get("sections")
        .and_then(Value::as_array)
        .and_then(|sections| {
            sections
                .iter()
                .find(|section| section.get("tool").and_then(Value::as_str) == Some(tool_name))
        })
        .and_then(|section| {
            section
                .get("text")
                .or_else(|| section.get("error"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_string();
    normalize_tool_text_for_ops(&text)
}

fn normalize_tool_text_for_ops(text: &str) -> String {
    let trimmed = text.trim();
    if let Ok(decoded) = serde_json::from_str::<String>(trimmed) {
        return decoded;
    }
    trimmed.replace("\\n", "\n")
}

fn parse_store_health_counts(text: &str) -> Value {
    let production_line = text
        .lines()
        .filter(|line| line.contains("production:"))
        .last()
        .unwrap_or_default();
    let product_line = text
        .lines()
        .find(|line| line.contains("商品:"))
        .unwrap_or_default();
    let production_orders =
        first_capture_u64(production_line, "production: ", " 訂單").unwrap_or(0);
    let production_gmv = first_capture_f64(production_line, "GMV ", " USDT").unwrap_or(0.0);
    let total_products = first_capture_u64(product_line, "商品: ", " 總").unwrap_or(0);
    let on_sale_products = first_capture_u64(product_line, " | ", " ON_SALE").unwrap_or(0);
    json!({
        "productionOrders30d": production_orders,
        "productionGmv30d": production_gmv,
        "totalProducts": total_products,
        "onSaleProducts": on_sale_products,
    })
}

fn first_capture_u64(text: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let after = text.split(prefix).nth(1)?;
    let raw = after.split(suffix).next()?.trim();
    raw.parse::<u64>().ok()
}

fn first_capture_f64(text: &str, prefix: &str, suffix: &str) -> Option<f64> {
    let after = text.split(prefix).nth(1)?;
    let raw = after.split(suffix).next()?.trim();
    raw.parse::<f64>().ok()
}

fn extract_bot_knowledge_gap_questions(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let question = trimmed.strip_prefix("📝 ")?;
            Some(json!(question.trim()))
        })
        .collect()
}

fn is_local_test_health_inbox_record(record: &Value) -> bool {
    let candidate = record.get("candidate").unwrap_or(&Value::Null);
    candidate
        .pointer("/opsEvent/source")
        .and_then(Value::as_str)
        == Some("sirin_test_health")
        || (candidate.get("allowedAction").and_then(Value::as_str) == Some("SIRIN_BACKLOG_ONLY")
            && candidate
                .get("evidence")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.get("tool").and_then(Value::as_str) == Some("script_health")
                    })
                }))
}

fn split_current_candidates_with_tracked_reviews(
    task_id: &str,
    candidates: &[Value],
    persisted_records: &[Value],
) -> (Vec<Value>, Vec<Value>) {
    let mut report_candidates = Vec::new();
    let mut tracked_records = Vec::new();
    let now = Utc::now().to_rfc3339();
    for candidate in candidates {
        let dedupe_key = candidate
            .get("dedupeKey")
            .or_else(|| candidate.get("dedupe_key"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let previous_review = if dedupe_key.is_empty() {
            None
        } else {
            find_previous_review_for_candidate(persisted_records, "", dedupe_key, candidate)
        };
        if previous_review
            .as_ref()
            .is_some_and(|record| is_tracked_external_record(record))
        {
            tracked_records.push(json!({
                "record_id": format!("{task_id}:{dedupe_key}"),
                "task_id": task_id,
                "task_type": AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE,
                "dedupe_key": dedupe_key,
                "candidate": candidate,
                "review_status": previous_review.as_ref().and_then(|r| r.get("review_status")).cloned().unwrap_or(Value::Null),
                "review_note": previous_review.as_ref().and_then(|r| r.get("review_note")).cloned().unwrap_or(Value::Null),
                "reviewed_by": previous_review.as_ref().and_then(|r| r.get("reviewed_by")).cloned().unwrap_or(Value::Null),
                "external_issue": previous_review.as_ref().and_then(|r| r.get("external_issue")).cloned().unwrap_or(Value::Null),
                "created_at": now,
                "updated_at": now,
                "source_summary": "Current candidate matched an already tracked external issue.",
                "boundary": "Sirin-local issue candidate; read-only scout output."
            }));
        } else {
            report_candidates.push(candidate.clone());
        }
    }
    (report_candidates, tracked_records)
}

async fn run_embedded_test_health_scout(
    task: &A2aTask,
    default_max_candidates: usize,
) -> Result<Value, String> {
    let suite = arg_text(&task.params, "testHealthSuite").unwrap_or_else(|| "active-smoke".into());
    let window_days = bounded_u64_param(&task.params, "testHealthWindowDays", 14, 2, 90);
    let max_candidates = bounded_u64_param(
        &task.params,
        "maxTestHealthCandidates",
        default_max_candidates as u64,
        1,
        50,
    );
    let mut params = json!({
        "suite": suite,
        "windowDays": window_days,
        "maxCandidates": max_candidates,
        "includeLlmOnly": bool_param(&task.params, "includeLlmOnly", true),
        "includeUntested": bool_param(&task.params, "includeUntested", true),
        "includeObsolete": bool_param(&task.params, "includeObsolete", false),
    });
    if let Some(tags) = string_array_param(&task.params, "excludeTags") {
        params["excludeTags"] = json!(tags);
    }
    let health_task = A2aTask {
        id: format!("{}:test_health", task.id),
        task_type: SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE.into(),
        params,
    };
    run_sirin_agora_test_health_scout(&health_task).await
}

async fn run_sirin_agora_test_health_scout(task: &A2aTask) -> Result<Value, String> {
    let suite = arg_text(&task.params, "suite").unwrap_or_else(|| "active-smoke".into());
    let window_days = bounded_u64_param(&task.params, "windowDays", 14, 2, 90);
    let max_candidates = bounded_u64_param(&task.params, "maxCandidates", 12, 1, 50) as usize;
    let include_llm_only = bool_param(&task.params, "includeLlmOnly", true);
    let include_untested = bool_param(&task.params, "includeUntested", true);
    let include_obsolete = bool_param(&task.params, "includeObsolete", false);
    let exclude_tags = test_health_exclude_tags(&task.params, &suite, include_obsolete);
    let mut script_health_args = json!({
        "suite": suite,
        "window_days": window_days,
    });
    if let Some(tags) = exclude_tags {
        script_health_args["exclude_tags"] = json!(tags);
    }
    let script_health = crate::mcp_server::call_script_health(script_health_args)?;
    let failed_tests = latest_failed_test_details(&script_health, window_days);
    let candidates = build_test_health_candidates(
        &script_health,
        &failed_tests,
        max_candidates,
        include_llm_only,
        include_untested,
    );
    let summary = build_test_health_summary(&script_health, &candidates);
    Ok(json!({
        "task_type": SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE,
        "summary": summary,
        "boundary": "READ_ONLY; Sirin only inspects local test/replay health and proposes candidates. ChatGPT/Codex/Human must verify before action.",
        "candidate_count": candidates.len(),
        "issue_candidates": candidates,
        "failed_tests": failed_tests,
        "source_health": script_health,
        "generated_at": Utc::now().to_rfc3339(),
    }))
}

fn test_health_exclude_tags(
    params: &Value,
    suite: &str,
    include_obsolete: bool,
) -> Option<Vec<String>> {
    if let Some(explicit) = string_array_param(params, "excludeTags") {
        return Some(explicit);
    }
    if include_obsolete || matches!(suite, "obsolete" | "legacy") {
        return None;
    }
    Some(
        DEFAULT_TEST_HEALTH_EXCLUDE_TAGS
            .iter()
            .map(|tag| (*tag).to_owned())
            .collect(),
    )
}

async fn report_task_status(
    cfg: &A2aWorkerConfig,
    task_id: &str,
    status: &str,
    error: Option<String>,
    artifacts: Value,
    result: Value,
) -> Result<Value, String> {
    let summary = if let Some(error) = error.as_deref() {
        status_message(status, Some(error))
    } else {
        result
            .get("summary")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(1000).collect::<String>())
            .unwrap_or_else(|| status_message(status, None))
    };
    let artifacts_json = serde_json::to_string(&artifacts)
        .map_err(|e| format!("serialize A2A artifacts failed: {e}"))?;
    let args = json!({
        "taskId": task_id,
        "status": status,
        "summary": summary,
        "errorMessage": error.unwrap_or_default(),
        "artifactsJson": artifacts_json,
    });
    crate::mcp_client::call_tool_with_bearer(
        &cfg.server_url,
        &cfg.update_tool,
        args,
        cfg.bearer.as_deref(),
    )
    .await
}

fn status_message(status: &str, error: Option<&str>) -> String {
    match (status, error) {
        ("RUNNING", _) => "Sirin accepted the task and started local execution.".into(),
        ("NEEDS_REVIEW", _) => {
            "Sirin completed local research; Codex/server review is required.".into()
        }
        ("FAILED", Some(e)) => format!("Sirin task failed: {e}"),
        _ => status.into(),
    }
}

fn artifacts_from_result(result: &Value) -> Value {
    let mut artifacts = Vec::new();
    if result.get("task_type").and_then(Value::as_str)
        == Some(AGORA_MARKET_FIRST_ORDER_ACTIVATION_TASK_TYPE)
    {
        if let Some(summary) = result.get("summary").and_then(Value::as_str) {
            artifacts.push(json!({
                "artifactType": "LOG",
                "uriOrValue": summary,
                "metadata": { "name": "agora_market_first_order_activation_summary" },
            }));
        }
        if let Some(playbook) = result.get("first_order_activation") {
            artifacts.push(json!({
                "artifactType": "JSON_PAYLOAD",
                "uriOrValue": playbook.to_string(),
                "metadata": { "name": "agora_market_first_order_activation_playbook" },
            }));
        }
        if let Some(report) = result.get("ops_report") {
            artifacts.push(json!({
                "artifactType": "JSON_PAYLOAD",
                "uriOrValue": report.to_string(),
                "metadata": { "name": "agora_market_ops_report" },
            }));
        }
        artifacts.push(json!({
            "artifactType": "JSON_PAYLOAD",
            "uriOrValue": result.to_string(),
            "metadata": { "name": "agora_market_first_order_activation_result" },
        }));
        return json!(artifacts);
    }
    if result.get("task_type").and_then(Value::as_str) == Some(AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE) {
        if let Some(summary) = result.get("summary").and_then(Value::as_str) {
            artifacts.push(json!({
                "artifactType": "LOG",
                "uriOrValue": summary,
                "metadata": { "name": "agora_market_issue_scout_summary" },
            }));
        }
        if let Some(candidates) = result.get("issue_candidates") {
            artifacts.push(json!({
                "artifactType": "JSON_PAYLOAD",
                "uriOrValue": candidates.to_string(),
                "metadata": { "name": "agora_market_issue_candidates" },
            }));
        }
        if let Some(report) = result.get("ops_report") {
            artifacts.push(json!({
                "artifactType": "JSON_PAYLOAD",
                "uriOrValue": report.to_string(),
                "metadata": { "name": "agora_market_ops_report" },
            }));
        }
        artifacts.push(json!({
            "artifactType": "JSON_PAYLOAD",
            "uriOrValue": result.to_string(),
            "metadata": { "name": "agora_market_issue_scout_result" },
        }));
        return json!(artifacts);
    }
    if result.get("task_type").and_then(Value::as_str)
        == Some(AGORA_MARKET_OPS_DAILY_BRIEF_TASK_TYPE)
    {
        if let Some(summary) = result.get("summary").and_then(Value::as_str) {
            artifacts.push(json!({
                "artifactType": "LOG",
                "uriOrValue": summary,
                "metadata": { "name": "agora_market_ops_daily_brief_summary" },
            }));
        }
        artifacts.push(json!({
            "artifactType": "JSON_PAYLOAD",
            "uriOrValue": result.to_string(),
            "metadata": { "name": "agora_market_ops_daily_brief_result" },
        }));
        return json!(artifacts);
    }
    if let Some(inbox_id) = result.get("inbox_id").and_then(Value::as_str) {
        artifacts.push(json!({
            "artifactType": "INBOX_ID",
            "uriOrValue": inbox_id,
            "metadata": { "name": "research_sentinel_inbox" },
        }));
    }
    if let Some(report_path) = result.get("report_path").and_then(Value::as_str) {
        artifacts.push(json!({
            "artifactType": "HTML_REPORT",
            "uriOrValue": report_path,
            "metadata": { "name": "human_readable_report" },
        }));
    }
    if let Some(run_id) = result.get("run_id").and_then(Value::as_str) {
        artifacts.push(json!({
            "artifactType": "LOG",
            "uriOrValue": run_id,
            "metadata": { "name": "research_sentinel_run" },
        }));
    }
    if let Some(deep) = result
        .get("deep_research")
        .or_else(|| {
            result
                .get("entry")
                .and_then(|entry| entry.get("deep_research"))
        })
        .filter(|value| !value.is_null())
    {
        artifacts.push(json!({
            "artifactType": "JSON_PAYLOAD",
            "uriOrValue": deep.to_string(),
            "metadata": { "name": "deep_research_result" },
        }));
        if let Some(issue) = deep
            .get("issue_draft")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            artifacts.push(json!({
                "artifactType": "LOG",
                "uriOrValue": issue,
                "metadata": { "name": "deep_research_issue_draft" },
            }));
        }
        if let Some(kb) = deep
            .get("kb_draft")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            artifacts.push(json!({
                "artifactType": "LOG",
                "uriOrValue": kb,
                "metadata": { "name": "deep_research_kb_draft" },
            }));
        }
    }
    artifacts.push(json!({
        "artifactType": "JSON_PAYLOAD",
        "uriOrValue": result.to_string(),
        "metadata": { "name": "research_sentinel_result" },
    }));
    json!(artifacts)
}

fn agora_ops_direct_auth_probe(cfg: &A2aWorkerConfig, tool_calls: &[OpsToolCall]) -> Value {
    let requested_tools: Vec<Value> = tool_calls
        .iter()
        .map(|call| Value::String(call.name.to_owned()))
        .collect();
    let ops_present = cfg.agora_ops_authorization.is_some();
    json!({
        "status": if ops_present { "OPS_DIRECT" } else { "MISSING_OPS_AUTHORIZATION" },
        "authorized": ops_present,
        "requestedToolsCovered": ops_present,
        "requestedTools": requested_tools,
        "sirinAuthMode": if ops_present { "OPS_HEADER" } else { "OPS_HEADER_MISSING" },
        "opsAuthorizationPresent": ops_present,
        "nextAction": if ops_present { "call_requested_ops_tools_directly" } else { "configure_ops_authorization" },
        "boundary": "Sirin uses OPS direct authorization for AgoraMarket read-only operations tools; it does not request external-AI Telegram approval.",
    })
}

struct OpsToolCall {
    name: &'static str,
    arguments: Value,
}

impl OpsToolCall {
    fn new(name: &'static str, arguments: Value) -> Self {
        Self { name, arguments }
    }
}

fn bounded_u64_param(value: &Value, key: &str, default: u64, min: u64, max: u64) -> u64 {
    value
        .get(key)
        .or_else(|| value.get(to_snake_case(key).as_str()))
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .clamp(min, max)
}

fn bool_param(value: &Value, key: &str, default: bool) -> bool {
    value
        .get(key)
        .or_else(|| value.get(to_snake_case(key).as_str()))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn string_array_param(value: &Value, key: &str) -> Option<Vec<String>> {
    let arr = value
        .get(key)
        .or_else(|| value.get(to_snake_case(key).as_str()))?
        .as_array()?;
    Some(
        arr.iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    )
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        if ch.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn tool_text(value: &Value) -> String {
    value
        .get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(6000).collect())
        .unwrap_or_else(|| value.to_string().chars().take(6000).collect())
}

fn mcp_error_data_from_text(error: &str) -> Option<Value> {
    let marker = "; data=";
    let data = error.split_once(marker)?.1.trim();
    serde_json::from_str::<Value>(data).ok()
}

fn build_ops_summary(
    days: i64,
    overdue_hours: i64,
    sections: &[Value],
    failure_count: usize,
) -> String {
    let ok_count = sections
        .iter()
        .filter(|section| section.get("ok").and_then(Value::as_bool).unwrap_or(false))
        .count();
    let mut lines = vec![
        format!(
            "AgoraMarket ops daily brief: {ok_count}/{} read-only MCP checks completed.",
            sections.len()
        ),
        format!("Window: last {days} days; fulfillment overdue threshold: {overdue_hours}h."),
        "Boundary: READ_ONLY; no Telegram sends, no order/product/fund/wallet/trading mutations."
            .into(),
    ];
    if failure_count > 0 {
        lines.push(format!("Attention: {failure_count} check(s) failed; inspect failures before relying on the brief."));
    }
    lines.push("Recommended review order: ops backlog -> pending orders -> disputes -> group attribution -> knowledge gaps -> sourcing.".into());
    lines.join("\n")
}

fn build_issue_candidates(brief: &Value, max_candidates: usize) -> Vec<Value> {
    let Some(sections) = brief.get("sections").and_then(Value::as_array) else {
        return Vec::new();
    };
    let has_bot_gap_detail = sections.iter().any(|section| {
        section.get("tool").and_then(Value::as_str) == Some("getBotKnowledgeGaps")
            && section.get("ok").and_then(Value::as_bool).unwrap_or(false)
            && section
                .get("text")
                .and_then(Value::as_str)
                .map(|text| {
                    !is_healthy_ops_section("getBotKnowledgeGaps", text)
                        && contains_attention_signal(text)
                })
                .unwrap_or(false)
    });
    let mut candidates = Vec::new();
    for section in sections {
        if candidates.len() >= max_candidates {
            break;
        }
        let tool = section
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let ok = section.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if !ok {
            let error = section
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("MCP check failed");
            let (candidate_type, title, evidence_type) = if section
                .get("evidenceType")
                .and_then(Value::as_str)
                == Some("mcp_auth_required")
                || error.contains("BATCH_PLAN_REQUIRED")
                || error.contains("batch authorization not ready")
                || error.contains("OPS MCP authorization not ready")
            {
                let title = if is_external_mcp_contract_blocker("mcp_auth_required", error) {
                    format!("AgoraMarketAPI MCP ops tool contract blocks Sirin evidence collection: {tool}")
                } else {
                    "Sirin AgoraMarket OPS MCP authorization is not ready".to_string()
                };
                ("INTEGRATION_BLOCKER", title, "mcp_auth_required")
            } else {
                (
                    "INTEGRATION_BLOCKER",
                    format!("Sirin AgoraMarket MCP check failed: {tool}"),
                    "mcp_error",
                )
            };
            candidates.push(issue_candidate(
                candidate_type,
                "MEDIUM",
                0.65,
                title,
                tool,
                evidence_type,
                error,
                "CODEX",
                "verify",
            ));
            continue;
        }

        let text = section
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if is_healthy_ops_section(tool, text) {
            continue;
        }
        if tool == "getKnowledgeBaseStats" && has_bot_gap_detail {
            continue;
        }
        if tool == "storeHealthCheck" {
            let counts = parse_store_health_counts(text);
            let production_orders = counts
                .get("productionOrders30d")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let production_gmv = counts
                .get("productionGmv30d")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let on_sale_products = counts
                .get("onSaleProducts")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if production_orders > 0 || production_gmv > 0.0 {
                continue;
            }
            if on_sale_products > 0 {
                let (candidate_type, severity, confidence, actor, action, title) =
                    classify_ops_section(tool, text);
                candidates.push(issue_candidate(
                    candidate_type,
                    severity,
                    confidence,
                    title,
                    tool,
                    "mcp_output",
                    text,
                    actor,
                    action,
                ));
                continue;
            }
        }
        if !contains_attention_signal(text) {
            continue;
        }
        let (candidate_type, severity, confidence, actor, action, title) =
            classify_ops_section(tool, text);
        candidates.push(issue_candidate(
            candidate_type,
            severity,
            confidence,
            title,
            tool,
            "mcp_output",
            text,
            actor,
            action,
        ));
    }
    candidates
}

fn build_ops_report(
    brief: &Value,
    candidates: &[Value],
    local_test_health: Option<&Value>,
    inbox_records: &[Value],
) -> Value {
    let sections = brief
        .get("sections")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ok_count = sections
        .iter()
        .filter(|section| section.get("ok").and_then(Value::as_bool).unwrap_or(false))
        .count();
    let failure_count = sections.len().saturating_sub(ok_count);
    let marketplace_evidence_ready = ok_count > 0
        && !sections.iter().any(|section| {
            section.get("evidenceType").and_then(Value::as_str) == Some("mcp_auth_required")
        });
    let local_test_health_ready = local_test_health
        .map(|health| health.get("error").is_none())
        .unwrap_or(false);
    let auth_probe = brief.get("auth_probe").cloned().unwrap_or(Value::Null);
    let report_status = build_ops_report_status(marketplace_evidence_ready, &auth_probe);
    let blocked_actor = report_auth_blocker_actor(&auth_probe);
    let blocked_allowed_action = report_auth_blocker_allowed_action(&auth_probe);
    let mut categories = vec![
        report_category("service", "Marketplace service health and MCP readiness"),
        report_category("product", "Product/catalog review and listing health"),
        report_category("seller", "Seller fulfillment, supplier, and sourcing ops"),
        report_category("order", "Order flow, pending orders, and checkout risk"),
        report_category("growth", "Growth, attribution, and acquisition signals"),
        report_category("support", "Support, disputes, trust, and KB gaps"),
    ];

    for section in &sections {
        let tool = section
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let category = report_category_for_tool(tool);
        if let Some(bucket) = categories
            .iter_mut()
            .find(|item| item.get("category").and_then(Value::as_str) == Some(category))
        {
            let checks = bucket
                .get_mut("checks")
                .and_then(Value::as_array_mut)
                .expect("report checks stay arrays");
            checks.push(report_check_from_section(section));
        }
    }

    if let Some(health) = local_test_health {
        if let Some(bucket) = categories
            .iter_mut()
            .find(|item| item.get("category").and_then(Value::as_str) == Some("service"))
        {
            let checks = bucket
                .get_mut("checks")
                .and_then(Value::as_array_mut)
                .expect("report checks stay arrays");
            checks.push(report_check_from_test_health(health));
        }
    }

    for candidate in candidates {
        let category = report_category_for_candidate(candidate);
        if let Some(bucket) = categories
            .iter_mut()
            .find(|item| item.get("category").and_then(Value::as_str) == Some(category))
        {
            let issues = bucket
                .get_mut("issues")
                .and_then(Value::as_array_mut)
                .expect("report issues stay arrays");
            issues.push(report_issue_from_candidate(candidate));
        }
    }

    for record in inbox_records {
        let Some(candidate) = record.get("candidate") else {
            continue;
        };
        let category = report_category_for_candidate(candidate);
        if let Some(bucket) = categories
            .iter_mut()
            .find(|item| item.get("category").and_then(Value::as_str) == Some(category))
        {
            let events = bucket
                .get_mut("inboxEvents")
                .and_then(Value::as_array_mut)
                .expect("report inboxEvents stay arrays");
            events.push(report_inbox_event_from_record(record));
        }
    }

    for bucket in &mut categories {
        let issue_count = bucket
            .get("issues")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let inbox_events = bucket
            .get("inboxEvents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let active_inbox_count = inbox_events
            .iter()
            .filter(|event| !is_tracked_external_inbox_event(event))
            .count();
        let tracked_external_count = inbox_events
            .iter()
            .filter(|event| is_tracked_external_inbox_event(event))
            .count();
        let failed_check_count = bucket
            .get("checks")
            .and_then(Value::as_array)
            .map(|checks| {
                checks
                    .iter()
                    .filter(|check| check.get("status").and_then(Value::as_str) != Some("ok"))
                    .count()
            })
            .unwrap_or(0);
        let has_checks = bucket
            .get("checks")
            .and_then(Value::as_array)
            .is_some_and(|checks| !checks.is_empty());
        let category_name = bucket
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let mut status = if issue_count > 0 || active_inbox_count > 0 {
            "needs_review"
        } else if failed_check_count > 0 && tracked_external_count > 0 {
            "blocked_external_issue_tracked"
        } else if tracked_external_count > 0 {
            "tracked_external_issue"
        } else if failed_check_count > 0 {
            "blocked_or_incomplete"
        } else if has_checks {
            "observed_no_attention_signal"
        } else {
            "not_checked"
        };
        if !marketplace_evidence_ready
            && !has_checks
            && issue_count == 0
            && active_inbox_count == 0
            && tracked_external_count == 0
        {
            status = "blocked_pending_marketplace_evidence";
            bucket["coverageGap"] = json!({
                "category": category_name,
                "reason": "Marketplace read-only MCP evidence was not collected for this category.",
                "blockedBy": "Sirin OPS MCP authorization/integration",
                "evidenceLevel": "needs_verification",
                "targetProject": "Sirin",
                "nextActor": blocked_actor,
                "allowedAction": blocked_allowed_action,
                "nextAction": report_auth_blocker_next_action(&auth_probe, Some(category_name)),
            });
        }
        bucket["status"] = json!(status);
    }

    let coverage_gaps = report_coverage_gaps(&categories);
    let tracked_external_issues = report_tracked_external_issues(&categories);
    let next_actions =
        build_report_next_actions(&categories, marketplace_evidence_ready, &auth_probe);
    let action_summary = build_action_summary(
        &categories,
        &next_actions,
        &tracked_external_issues,
        marketplace_evidence_ready,
    );

    json!({
        "schemaVersion": 1,
        "reportType": "AGORA_MARKET_OPERATIONS_REPORT",
        "reportStatus": report_status,
        "boundary": "READ_ONLY evidence report; no AgoraMarket/API/GitHub mutation.",
        "marketplaceEvidenceReady": marketplace_evidence_ready,
        "localTestHealthReady": local_test_health_ready,
        "readOnlyChecks": {
            "total": sections.len(),
            "ok": ok_count,
            "failed": failure_count,
        },
        "localTestHealth": local_test_health.cloned().unwrap_or(Value::Null),
        "opsEventInbox": {
            "included": true,
            "count": inbox_records.len(),
            "trackedExternalIssueCount": tracked_external_issues.len(),
            "storagePath": issue_candidates_path(),
        },
        "auth": auth_probe,
        "window": brief.get("window").cloned().unwrap_or_else(|| json!({})),
        "summary": build_ops_report_summary(&categories, marketplace_evidence_ready, local_test_health_ready, ok_count, failure_count),
        "actionSummary": action_summary,
        "coverageGaps": coverage_gaps,
        "trackedExternalIssues": tracked_external_issues,
        "categories": categories,
        "nextActions": next_actions,
    })
}

fn report_category(category: &str, description: &str) -> Value {
    json!({
        "category": category,
        "description": description,
        "status": "not_checked",
        "coverageGap": null,
        "checks": [],
        "issues": [],
        "inboxEvents": [],
    })
}

fn report_check_from_section(section: &Value) -> Value {
    let ok = section.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let status = if ok { "ok" } else { "failed_or_blocked" };
    let evidence = section
        .get("text")
        .or_else(|| section.get("error"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "tool": section.get("tool").cloned().unwrap_or_else(|| json!("unknown")),
        "status": status,
        "evidenceType": section.get("evidenceType").and_then(Value::as_str).unwrap_or(if ok { "mcp_output" } else { "mcp_error" }),
        "evidenceExcerpt": evidence_excerpt(evidence),
        "errorData": section.get("errorData").cloned().unwrap_or(Value::Null),
    })
}

fn report_check_from_test_health(health: &Value) -> Value {
    if let Some(error) = health.get("error").and_then(Value::as_str) {
        return json!({
            "tool": "script_health",
            "status": "failed_or_blocked",
            "evidenceType": "script_health_error",
            "evidenceExcerpt": evidence_excerpt(error),
        });
    }
    let summary = health
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("Sirin local test health scout completed.");
    let candidate_count = health
        .get("candidate_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "tool": "script_health",
        "status": if candidate_count > 0 { "attention_signal" } else { "ok" },
        "evidenceType": "sirin_local_test_health",
        "evidenceExcerpt": evidence_excerpt(summary),
    })
}

fn report_issue_from_candidate(candidate: &Value) -> Value {
    json!({
        "title": candidate.get("title").cloned().unwrap_or(Value::Null),
        "candidateType": candidate.get("candidateType").cloned().unwrap_or(Value::Null),
        "severity": candidate.get("severity").cloned().unwrap_or(Value::Null),
        "evidenceLevel": candidate.get("evidenceLevel").cloned().unwrap_or(Value::Null),
        "allowedAction": candidate.get("allowedAction").cloned().unwrap_or(Value::Null),
        "nextActor": candidate.get("suggestedNextActor").cloned().unwrap_or(Value::Null),
        "nextAction": candidate.get("suggestedAction").cloned().unwrap_or(Value::Null),
        "targetProject": candidate.pointer("/opsEvent/targetProject").cloned().unwrap_or(Value::Null),
        "mayOpenProductIssue": candidate.pointer("/opsEvent/productIssueGate/mayOpenProductIssue").cloned().unwrap_or(Value::Bool(false)),
        "dedupeKey": candidate.get("dedupeKey").cloned().unwrap_or(Value::Null),
        "evidence": candidate.get("evidence").cloned().unwrap_or_else(|| json!([])),
    })
}

fn report_inbox_event_from_record(record: &Value) -> Value {
    let candidate = record.get("candidate").unwrap_or(&Value::Null);
    let mut event = report_issue_from_candidate(candidate);
    if let Some(obj) = event.as_object_mut() {
        obj.insert(
            "recordId".into(),
            record.get("record_id").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "taskId".into(),
            record.get("task_id").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "reviewStatus".into(),
            record.get("review_status").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "externalIssue".into(),
            record.get("external_issue").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "createdAt".into(),
            record.get("created_at").cloned().unwrap_or(Value::Null),
        );
        obj.insert("source".into(), json!("ops_event_inbox"));
    }
    event
}

fn report_coverage_gaps(categories: &[Value]) -> Vec<Value> {
    categories
        .iter()
        .filter_map(|category| {
            let gap = category.get("coverageGap")?;
            if gap.is_null() {
                None
            } else {
                Some(gap.clone())
            }
        })
        .collect()
}

fn report_tracked_external_issues(categories: &[Value]) -> Vec<Value> {
    categories
        .iter()
        .flat_map(|category| {
            let category_name = category.get("category").cloned().unwrap_or(Value::Null);
            category
                .get("inboxEvents")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|event| is_tracked_external_inbox_event(event))
                .map(move |event| {
                    json!({
                        "category": category_name.clone(),
                        "title": event.get("title").cloned().unwrap_or(Value::Null),
                        "recordId": event.get("recordId").cloned().unwrap_or(Value::Null),
                        "targetProject": event.get("targetProject").cloned().unwrap_or(Value::Null),
                        "allowedAction": event.get("allowedAction").cloned().unwrap_or(Value::Null),
                        "reviewStatus": event.get("reviewStatus").cloned().unwrap_or(Value::Null),
                        "externalIssue": event.get("externalIssue").cloned().unwrap_or(Value::Null),
                    })
                })
        })
        .collect()
}

fn is_tracked_external_inbox_event(event: &Value) -> bool {
    matches!(
        event.get("reviewStatus").and_then(Value::as_str),
        Some("ACCEPTED" | "CONVERT_TO_ISSUE")
    ) && external_issue_has_reference(event.get("externalIssue").unwrap_or(&Value::Null))
}

fn inbox_event_needs_review(event: &Value) -> bool {
    if is_tracked_external_inbox_event(event) {
        return false;
    }
    matches!(
        event.get("reviewStatus").and_then(Value::as_str),
        None | Some("CANDIDATE" | "NEEDS_MORE_EVIDENCE" | "NEEDS_MORE_SOURCES")
    )
}

fn external_issue_has_reference(external_issue: &Value) -> bool {
    external_issue
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|url| !url.trim().is_empty())
        || !external_issue
            .get("number")
            .unwrap_or(&Value::Null)
            .is_null()
}

fn build_ops_report_status(marketplace_evidence_ready: bool, _auth_probe: &Value) -> &'static str {
    if marketplace_evidence_ready {
        "READY_FOR_REVIEW"
    } else {
        "BLOCKED_INTEGRATION"
    }
}

fn report_category_for_tool(tool: &str) -> &'static str {
    match affected_surface_for_tool(tool) {
        "marketplace_ops" | "unknown" => "service",
        "sourcing" => "seller",
        "orders" => "order",
        "disputes" | "customer_support" => "support",
        "growth" => "growth",
        _ => "service",
    }
}

fn report_category_for_candidate(candidate: &Value) -> &'static str {
    if is_mcp_batch_auth_candidate(candidate) {
        return "service";
    }
    let event_type = candidate
        .pointer("/opsEvent/eventType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "mcp_integration" | "service_health" | "service_health_validation" => "service",
        "seller_fulfillment" => "seller",
        "order_risk" => "order",
        "trust_support" | "support_content" => "support",
        "growth_signal" => "growth",
        _ => match candidate
            .get("affectedSurface")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "sourcing" => "seller",
            "orders" => "order",
            "disputes" | "customer_support" => "support",
            "growth" => "growth",
            _ => "service",
        },
    }
}

fn is_mcp_batch_auth_candidate(candidate: &Value) -> bool {
    let evidence = candidate
        .get("evidence")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .unwrap_or(&Value::Null);
    let evidence_type = evidence
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let evidence_text = evidence
        .get("excerpt")
        .and_then(Value::as_str)
        .unwrap_or_default();
    evidence_type == "mcp_auth_required"
        || evidence_text.contains("BATCH_PLAN_REQUIRED")
        || evidence_text.contains("batch authorization not ready")
}

fn build_ops_report_summary(
    categories: &[Value],
    marketplace_evidence_ready: bool,
    local_test_health_ready: bool,
    ok_count: usize,
    failure_count: usize,
) -> String {
    let issue_count: usize = categories
        .iter()
        .filter_map(|category| category.get("issues").and_then(Value::as_array))
        .map(Vec::len)
        .sum();
    if !marketplace_evidence_ready {
        let local_note = if local_test_health_ready {
            " Local Sirin test_health evidence was included."
        } else {
            ""
        };
        return format!(
            "AgoraMarket ops report is not ready for marketplace conclusions: {ok_count} read-only checks completed, {failure_count} blocked/failed. Resolve Sirin/MCP integration gates before treating this as product evidence.{local_note}"
        );
    }
    if issue_count == 0 {
        return format!(
            "AgoraMarket ops report collected {ok_count} read-only checks and found no attention signal requiring review."
        );
    }
    format!(
        "AgoraMarket ops report collected {ok_count} read-only checks and found {issue_count} attention signal(s) requiring review."
    )
}

fn build_action_summary(
    categories: &[Value],
    next_actions: &[Value],
    tracked_external_issues: &[Value],
    marketplace_evidence_ready: bool,
) -> Value {
    let mut new_issue_candidates = 0usize;
    let mut handoff_only_candidates = 0usize;
    let mut inbox_needing_review = 0usize;
    let mut healthy_checks = 0usize;
    let mut blocked_checks = 0usize;

    for category in categories {
        for check in category
            .get("checks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if check.get("status").and_then(Value::as_str) == Some("ok") {
                healthy_checks += 1;
            } else {
                blocked_checks += 1;
            }
        }

        for issue in category
            .get("issues")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            match issue.get("allowedAction").and_then(Value::as_str) {
                Some("OPEN_PRODUCT_ISSUE_AFTER_REVIEW") => new_issue_candidates += 1,
                Some("HANDOFF_REVIEW_ONLY" | "OPERATOR_REVIEW") => handoff_only_candidates += 1,
                _ => {}
            }
        }

        for event in category
            .get("inboxEvents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if inbox_event_needs_review(&event) {
                inbox_needing_review += 1;
            }
        }
    }

    let status = if !marketplace_evidence_ready {
        "blocked_integration"
    } else if new_issue_candidates > 0 {
        "ready_for_issue_review"
    } else if handoff_only_candidates > 0 || inbox_needing_review > 0 {
        "ready_for_handoff_review"
    } else {
        "no_action_required"
    };
    let recommended_next_step = if !marketplace_evidence_ready {
        "Resolve Sirin/OPS MCP integration, then rerun the issue scout."
    } else if new_issue_candidates > 0 {
        "Review OPEN_PRODUCT_ISSUE_AFTER_REVIEW candidates first; create or update external issues only after verification."
    } else if handoff_only_candidates > 0 {
        "Review handoff-only candidates with ChatGPT/operator; do not open product issues yet."
    } else if inbox_needing_review > 0 {
        "Review pending Sirin-local inbox events before creating new work."
    } else {
        "No immediate operations action; keep scheduled monitoring running."
    };

    json!({
        "status": status,
        "newIssueCandidates": new_issue_candidates,
        "handoffOnlyCandidates": handoff_only_candidates,
        "trackedExternalIssues": tracked_external_issues.len(),
        "inboxEventsNeedingReview": inbox_needing_review,
        "healthyChecks": healthy_checks,
        "blockedChecks": blocked_checks,
        "nextActionCount": next_actions.len(),
        "recommendedNextStep": recommended_next_step,
    })
}

fn auth_waits_for_operator_approval(auth_probe: &Value) -> bool {
    let _ = auth_probe;
    false
}

fn report_auth_blocker_actor(auth_probe: &Value) -> &'static str {
    if auth_waits_for_operator_approval(auth_probe) {
        "HUMAN"
    } else {
        "CODEX"
    }
}

fn report_auth_blocker_allowed_action(auth_probe: &Value) -> &'static str {
    if auth_waits_for_operator_approval(auth_probe) {
        "OPERATOR_REVIEW"
    } else {
        "CODEX_VERIFY_INTEGRATION"
    }
}

fn report_auth_blocker_next_action(auth_probe: &Value, category: Option<&str>) -> String {
    let _ = auth_probe;
    let scope = category
        .map(|name| format!(" before drawing {name} conclusions"))
        .unwrap_or_default();
    format!("Resolve Sirin OPS MCP authorization/integration, then rerun AGORA_MARKET_ISSUE_SCOUT{scope}.")
}

fn auth_approval_next_action(auth_probe: &Value) -> Value {
    if auth_waits_for_operator_approval(auth_probe) {
        json!({
            "actor": report_auth_blocker_actor(auth_probe),
            "action": report_auth_blocker_next_action(auth_probe, None),
            "allowedAction": report_auth_blocker_allowed_action(auth_probe),
            "source": "auth",
            "authNextAction": auth_probe.get("nextAction").cloned().unwrap_or(Value::Null),
            "grantRequestId": auth_probe.get("grantRequestId").cloned().unwrap_or(Value::Null),
            "requestedTools": auth_probe.get("requestedTools").cloned().unwrap_or_else(|| json!([])),
        })
    } else {
        json!({
            "actor": "CODEX",
            "action": "Resolve Sirin OPS MCP authorization/integration, then rerun AGORA_MARKET_ISSUE_SCOUT.",
            "allowedAction": "CODEX_VERIFY_INTEGRATION",
            "source": "auth",
            "authNextAction": auth_probe.get("nextAction").cloned().unwrap_or(Value::Null),
            "grantRequestId": auth_probe.get("grantRequestId").cloned().unwrap_or(Value::Null),
            "requestedTools": auth_probe.get("requestedTools").cloned().unwrap_or_else(|| json!([])),
        })
    }
}

fn build_report_next_actions(
    categories: &[Value],
    marketplace_evidence_ready: bool,
    auth_probe: &Value,
) -> Vec<Value> {
    if !marketplace_evidence_ready {
        let mut actions = vec![auth_approval_next_action(auth_probe)];
        let coverage_gap_categories: Vec<_> = report_coverage_gaps(categories)
            .into_iter()
            .filter_map(|gap| {
                gap.get("category")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        if !coverage_gap_categories.is_empty() {
            actions.push(json!({
                "actor": report_auth_blocker_actor(auth_probe),
                "action": "Restore marketplace read-only evidence coverage for blocked report categories, then rerun AGORA_MARKET_ISSUE_SCOUT.",
                "allowedAction": report_auth_blocker_allowed_action(auth_probe),
                "source": "coverageGaps",
                "categories": coverage_gap_categories,
                "grantRequestId": auth_probe.get("grantRequestId").cloned().unwrap_or(Value::Null),
            }));
        }
        actions.extend(report_inbox_next_actions(categories).into_iter().take(5));
        return actions;
    }
    let mut actions = Vec::new();
    for category in categories {
        let Some(issues) = category.get("issues").and_then(Value::as_array) else {
            continue;
        };
        for issue in issues {
            actions.push(json!({
                "category": category.get("category").cloned().unwrap_or(Value::Null),
                "actor": issue.get("nextActor").cloned().unwrap_or(Value::Null),
                "action": issue.get("nextAction").cloned().unwrap_or(Value::Null),
                "allowedAction": issue.get("allowedAction").cloned().unwrap_or(Value::Null),
                "title": issue.get("title").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    actions.extend(report_inbox_next_actions(categories));
    actions
}

fn report_inbox_next_actions(categories: &[Value]) -> Vec<Value> {
    let mut tracked_actions = Vec::new();
    let mut actions = Vec::new();
    for category in categories {
        let Some(events) = category.get("inboxEvents").and_then(Value::as_array) else {
            continue;
        };
        for event in events {
            if is_tracked_external_inbox_event(event) {
                let external_issue = event.get("externalIssue").cloned().unwrap_or(Value::Null);
                tracked_actions.push(json!({
                    "category": category.get("category").cloned().unwrap_or(Value::Null),
                    "actor": event.get("nextActor").cloned().unwrap_or(Value::Null),
                    "action": "Track the external issue until resolved, then rerun AGORA_MARKET_ISSUE_SCOUT.",
                    "allowedAction": event.get("allowedAction").cloned().unwrap_or(Value::Null),
                    "title": event.get("title").cloned().unwrap_or(Value::Null),
                    "source": "tracked_external_issue",
                    "recordId": event.get("recordId").cloned().unwrap_or(Value::Null),
                    "targetProject": event.get("targetProject").cloned().unwrap_or(Value::Null),
                    "externalIssue": external_issue,
                }));
                continue;
            }
            actions.push(json!({
                "category": category.get("category").cloned().unwrap_or(Value::Null),
                "actor": event.get("nextActor").cloned().unwrap_or(Value::Null),
                "action": event.get("nextAction").cloned().unwrap_or(Value::Null),
                "allowedAction": event.get("allowedAction").cloned().unwrap_or(Value::Null),
                "title": event.get("title").cloned().unwrap_or(Value::Null),
                "source": "ops_event_inbox",
                "recordId": event.get("recordId").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    tracked_actions.extend(actions);
    let actions = tracked_actions;
    actions
}

fn load_report_inbox_records(limit: usize) -> Result<Vec<Value>, String> {
    let mut records: Vec<Value> = load_issue_candidate_records()?
        .into_iter()
        .map(ensure_ops_event_record)
        .filter(|record| {
            !matches!(
                candidate_record_status(record),
                Some("REJECTED" | "DONE" | "CANCELLED")
            )
        })
        .collect();
    collapse_report_inbox_records(&mut records, limit);
    Ok(records)
}

fn collapse_report_inbox_records(records: &mut Vec<Value>, limit: usize) {
    records.sort_by(|a, b| {
        let priority_cmp = record_review_priority(b).cmp(&record_review_priority(a));
        if priority_cmp != std::cmp::Ordering::Equal {
            return priority_cmp;
        }
        b.get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(
                a.get("updated_at")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
    });
    let mut seen = HashSet::new();
    records.retain(|record| {
        let key = record
            .get("dedupe_key")
            .or_else(|| {
                record
                    .get("candidate")
                    .and_then(|candidate| candidate.get("dedupeKey"))
            })
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        key.is_empty() || seen.insert(key)
    });
    let mut seen_operation = HashSet::new();
    records.retain(|record| {
        let key = record_operation_key(record);
        key.is_empty() || seen_operation.insert(key)
    });
    let mut kept_batch_auth = false;
    records.retain(|record| {
        let is_batch_auth = record
            .get("candidate")
            .is_some_and(is_mcp_batch_auth_candidate);
        if !is_batch_auth {
            return true;
        }
        if kept_batch_auth {
            return false;
        }
        kept_batch_auth = true;
        true
    });
    records.truncate(limit);
}

fn record_review_priority(record: &Value) -> u8 {
    let event = report_inbox_event_from_record(record);
    if is_tracked_external_inbox_event(&event) {
        return 4;
    }
    match candidate_record_status(record) {
        Some("CONVERT_TO_ISSUE" | "ACCEPTED") => 3,
        Some("NEEDS_MORE_EVIDENCE" | "NEEDS_MORE_SOURCES") => 2,
        Some("CANDIDATE") | None => 1,
        _ => 0,
    }
}

fn record_operation_key(record: &Value) -> String {
    let Some(candidate) = record.get("candidate") else {
        return String::new();
    };
    let tool = candidate
        .get("evidence")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = candidate
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let allowed_action = candidate
        .get("allowedAction")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let target_project = candidate
        .pointer("/opsEvent/targetProject")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let candidate_type = candidate
        .get("candidateType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if title.is_empty() && tool.is_empty() {
        return String::new();
    }
    format!("{target_project}|{allowed_action}|{candidate_type}|{tool}|{title}")
}

fn build_test_health_candidates(
    health: &Value,
    failed_tests: &[Value],
    max_candidates: usize,
    include_llm_only: bool,
    include_untested: bool,
) -> Vec<Value> {
    let mut candidates = Vec::new();
    let aggregate = health.get("aggregate").unwrap_or(&Value::Null);
    let suite = health
        .get("suite")
        .and_then(Value::as_str)
        .unwrap_or("active-smoke");
    let failed = aggregate.get("failed").and_then(Value::as_u64).unwrap_or(0);
    if failed > 0 && candidates.len() < max_candidates {
        let evidence = compact_failed_tests_evidence(aggregate, failed_tests);
        candidates.push(test_health_candidate(
            "TEST_REGRESSION",
            "HIGH",
            0.82,
            format!("Investigate {failed} failing AgoraMarket {suite} test(s)"),
            "script_health",
            "script_health_failed_tests",
            &evidence,
            "CODEX",
            "verify_failed_tests",
        ));
    }

    let drift = aggregate
        .get("drift")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if drift <= -0.10 && candidates.len() < max_candidates {
        candidates.push(test_health_candidate(
            "REPLAY_DRIFT",
            "MEDIUM",
            0.76,
            format!(
                "Replay rate drifted down by {:.1}pp for AgoraMarket {suite}",
                drift * 100.0
            ),
            "script_health",
            "script_health_aggregate",
            &aggregate.to_string(),
            "CODEX",
            "refresh_replay_scripts",
        ));
    }

    let Some(per_test) = health.get("per_test").and_then(Value::as_array) else {
        return candidates;
    };
    for test in per_test {
        if candidates.len() >= max_candidates {
            break;
        }
        let test_id = test
            .get("test_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown_test");
        let status = test
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match status {
            "stale_fallback" => candidates.push(test_health_candidate(
                "REPLAY_STALE",
                "MEDIUM",
                0.74,
                format!("Refresh stale replay script for {test_id}"),
                "script_health",
                "per_test_replay_status",
                &test.to_string(),
                "CODEX",
                "rerun_and_rerecord",
            )),
            "llm_only" if include_llm_only => candidates.push(test_health_candidate(
                "COVERAGE_GAP",
                "LOW",
                0.62,
                format!("Add deterministic replay coverage for {test_id}"),
                "script_health",
                "per_test_replay_status",
                &test.to_string(),
                "CODEX",
                "record_replay_script",
            )),
            "untested" if include_untested => candidates.push(test_health_candidate(
                "COVERAGE_GAP",
                "LOW",
                0.58,
                format!("Run or classify untested AgoraMarket check {test_id}"),
                "script_health",
                "per_test_replay_status",
                &test.to_string(),
                "CODEX",
                "verify_test_relevance",
            )),
            _ => {}
        }
    }
    candidates
}

fn compact_failed_tests_evidence(aggregate: &Value, failed_tests: &[Value]) -> String {
    let mut limit = failed_tests.len().min(5);
    loop {
        let failed_test_ids: Vec<Value> = failed_tests
            .iter()
            .filter_map(|test| test.get("test_id").cloned())
            .collect();
        let compact_tests: Vec<Value> = failed_tests
            .iter()
            .take(limit)
            .map(compact_failed_test)
            .collect();
        let evidence = json!({
            "failed_test_ids": failed_test_ids,
            "aggregate": {
                "window_days": aggregate.get("window_days").cloned().unwrap_or(Value::Null),
                "total_runs": aggregate.get("total_runs").cloned().unwrap_or(Value::Null),
                "failed": aggregate.get("failed").cloned().unwrap_or(Value::Null),
                "deterministic_rate": aggregate.get("success_rate_deterministic").cloned().unwrap_or(Value::Null),
            },
            "failed_tests": compact_tests,
            "omitted_failed_tests": failed_tests.len().saturating_sub(limit),
        });
        let text = evidence.to_string();
        if text.chars().count() <= 1190 || limit <= 1 {
            return text;
        }
        limit -= 1;
    }
}

fn compact_failed_test(test: &Value) -> Value {
    let latest = test.get("latest_run").unwrap_or(&Value::Null);
    let tags = test
        .get("test_tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .take(8)
                .map(|tag| Value::String(tag.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let status = latest.get("status").cloned().unwrap_or(Value::Null);
    let timeout = status.as_str() == Some("timeout");
    json!({
        "test_id": test.get("test_id").cloned().unwrap_or(Value::Null),
        "tags": tags,
        "replay_status": test.get("replay_status").cloned().unwrap_or(Value::Null),
        "latest_run": {
            "run_id": latest.get("run_id").cloned().unwrap_or(Value::Null),
            "at": latest.get("started_at").cloned().unwrap_or(Value::Null),
            "status": status,
            "duration_ms": latest.get("duration_ms").cloned().unwrap_or(Value::Null),
            "is_replay": latest.get("is_replay").cloned().unwrap_or(Value::Null),
            "timeout": timeout,
            "screenshot_path": latest.get("screenshot_path").cloned().unwrap_or(Value::Null),
            "failure_category": latest.get("failure_category").cloned().unwrap_or(Value::Null),
        }
    })
}

fn latest_failed_test_details(health: &Value, window_days: u64) -> Vec<Value> {
    let half_days = std::cmp::max(1, window_days as i64 / 2);
    let now = Utc::now();
    let cutoff = now - chrono::Duration::days(half_days);
    let Some(per_test) = health.get("per_test").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut failed = Vec::new();
    for test in per_test {
        let Some(test_id) = test.get("test_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(run) = crate::test_runner::store::recent_runs(test_id, 50)
            .into_iter()
            .find(|run| {
                chrono::DateTime::parse_from_rfc3339(&run.started_at)
                    .map(|dt| {
                        let started = dt.with_timezone(&Utc);
                        started >= cutoff && started < now
                    })
                    .unwrap_or(false)
            })
        else {
            continue;
        };
        if run.status == "passed" {
            continue;
        }
        failed.push(json!({
            "test_id": test_id,
            "test_tags": test.get("tags").cloned().unwrap_or_else(|| json!([])),
            "replay_status": test.get("status").cloned().unwrap_or(Value::Null),
            "has_script": test.get("has_script").cloned().unwrap_or(Value::Null),
            "recent_modes": test.get("recent_modes").cloned().unwrap_or_else(|| json!([])),
            "latest_run": {
                "id": run.id,
                "run_id": run.run_id,
                "started_at": run.started_at,
                "duration_ms": run.duration_ms,
                "status": run.status,
                "failure_category": run.failure_category,
                "ai_analysis": run.ai_analysis,
                "screenshot_path": run.screenshot_path,
                "iterations": run.iterations,
                "is_replay": run.is_replay,
                "console_errors": run.console_errors,
                "console_warnings": run.console_warnings,
                "llm_prompt_tokens": run.prompt_tokens,
                "llm_completion_tokens": run.completion_tokens,
                "llm_cached_tokens": run.cached_tokens,
                "cost_micro_usd": run.cost_micro_usd,
            }
        }));
    }
    failed
}

fn test_health_candidate(
    candidate_type: &str,
    severity: &str,
    confidence: f64,
    title: String,
    tool: &str,
    evidence_type: &str,
    evidence_text: &str,
    suggested_next_actor: &str,
    suggested_action: &str,
) -> Value {
    issue_candidate_with_source(
        SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE,
        candidate_type,
        severity,
        confidence,
        title,
        tool,
        evidence_type,
        evidence_text,
        suggested_next_actor,
        suggested_action,
    )
}

fn contains_attention_signal(text: &str) -> bool {
    text.lines().any(line_contains_attention_signal)
}

fn line_contains_attention_signal(line: &str) -> bool {
    let lower = line.to_lowercase();
    let has_signal = [
        "⚠",
        "❌",
        "待處理",
        "逾時",
        "overdue",
        "pending",
        "backlog",
        "failed",
        "失敗",
    ]
    .iter()
    .any(|needle| lower.contains(&needle.to_lowercase()));
    has_signal && !is_benign_zero_attention_line(&lower)
}

fn is_benign_zero_attention_line(lower: &str) -> bool {
    if lower.contains("pending_review") && lower.contains("0 pending_review") {
        return true;
    }
    if lower.contains("爭議")
        && (lower.contains("0 筆")
            || lower.contains("0 production active")
            || lower.contains("爭議率: 0.0%")
            || lower.contains("0.0%"))
    {
        return true;
    }
    lower.contains("退款") && lower.contains("0 usdt")
}

fn is_healthy_ops_section(tool: &str, text: &str) -> bool {
    let lower = text.to_lowercase();
    match tool {
        "getOpsBacklog" => {
            text.contains("✅")
                && (text.contains("全部清空")
                    || text.contains("沒事做")
                    || (text.contains("待履約訂單:")
                        && text.contains("0 production")
                        && text.contains("待處理爭議:")
                        && text.contains("0 production")
                        && text.contains("商品待審:")
                        && text.contains('0')))
        }
        "listPendingOrders" => {
            text.contains("✅")
                && (text.contains("無待處理訂單")
                    || lower.contains("no pending orders")
                    || lower.contains("0 pending orders"))
        }
        "getDisputeBacklog" => {
            text.contains("✅")
                && (text.contains("無爭議 backlog")
                    || lower.contains("no dispute backlog")
                    || lower.contains("0 dispute"))
        }
        "listPendingFulfillmentOrders" => {
            text.contains("✅")
                && (text.contains("無待履約")
                    || lower.contains("no pending fulfillment")
                    || lower.contains("0 pending fulfillment"))
        }
        "getKnowledgeBaseStats" => {
            text.contains("✅")
                && (lower.contains("pending 0")
                    || text.contains("PENDING 待補答問題: 0")
                    || text.contains("⏳ PENDING 待補答問題: 0"))
        }
        _ => false,
    }
}

fn classify_ops_section(
    tool: &str,
    text: &str,
) -> (
    &'static str,
    &'static str,
    f64,
    &'static str,
    &'static str,
    String,
) {
    let lower = text.to_lowercase();
    match tool {
        "getDisputeBacklog" => (
            "OPS_RISK",
            "HIGH",
            0.78,
            "CHATGPT",
            "verify",
            "Review AgoraMarket dispute backlog".into(),
        ),
        "listPendingFulfillmentOrders" => (
            "OPS_RISK",
            "HIGH",
            0.8,
            "HUMAN",
            "operator_review",
            "Review overdue fulfillment orders".into(),
        ),
        "getBotKnowledgeGaps" | "getLowQualityBotResponses" => (
            "GROWTH_GAP",
            "MEDIUM",
            0.72,
            "CHATGPT",
            "draft_kb_or_faq",
            "Convert bot knowledge gaps into FAQ candidates".into(),
        ),
        "getOpsBacklog" | "listPendingOrders" => (
            "OPS_RISK",
            if lower.contains("failed") || lower.contains("失敗") {
                "HIGH"
            } else {
                "MEDIUM"
            },
            0.74,
            "CHATGPT",
            "triage",
            format!("Triage AgoraMarket ops signal from {tool}"),
        ),
        "getGroupAttributionReport" => (
            "GROWTH_GAP",
            "LOW",
            0.58,
            "CHATGPT",
            "analyze_growth",
            "Review TG group attribution signal".into(),
        ),
        _ => (
            "DATA_ANOMALY",
            "LOW",
            0.55,
            "CODEX",
            "verify",
            format!("Review AgoraMarket signal from {tool}"),
        ),
    }
}

fn issue_candidate(
    candidate_type: &str,
    severity: &str,
    confidence: f64,
    title: String,
    tool: &str,
    evidence_type: &str,
    evidence_text: &str,
    suggested_next_actor: &str,
    suggested_action: &str,
) -> Value {
    issue_candidate_with_source(
        AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE,
        candidate_type,
        severity,
        confidence,
        title,
        tool,
        evidence_type,
        evidence_text,
        suggested_next_actor,
        suggested_action,
    )
}

fn issue_candidate_with_source(
    source_task_type: &str,
    candidate_type: &str,
    severity: &str,
    confidence: f64,
    title: String,
    tool: &str,
    evidence_type: &str,
    evidence_text: &str,
    suggested_next_actor: &str,
    suggested_action: &str,
) -> Value {
    let affected_surface = affected_surface_for_tool(tool);
    let excerpt = evidence_excerpt(evidence_text);
    let dedupe_key = dedupe_key(affected_surface, tool, candidate_type, &excerpt);
    let ops_event = ops_event_for_candidate(
        source_task_type,
        candidate_type,
        tool,
        evidence_type,
        evidence_text,
        suggested_next_actor,
        suggested_action,
    );
    let evidence_level = ops_event["evidenceLevel"]
        .as_str()
        .unwrap_or("suspected")
        .to_string();
    let allowed_action = ops_event["allowedAction"]
        .as_str()
        .unwrap_or("HANDOFF_REVIEW_ONLY")
        .to_string();
    let handoff = build_candidate_handoff(
        source_task_type,
        candidate_type,
        severity,
        confidence,
        &title,
        tool,
        affected_surface,
        &ops_event,
        &excerpt,
        suggested_next_actor,
        suggested_action,
        &dedupe_key,
    );
    json!({
        "schemaVersion": 1,
        "status": "CANDIDATE",
        "candidateType": candidate_type,
        "severity": severity,
        "confidence": confidence,
        "title": title,
        "dedupeKey": dedupe_key,
        "affectedSurface": affected_surface,
        "opsEvent": ops_event,
        "evidenceLevel": evidence_level,
        "allowedAction": allowed_action,
        "evidence": [{
            "type": evidence_type,
            "tool": tool,
            "excerpt": excerpt,
            "errorData": mcp_error_data_from_text(evidence_text).unwrap_or(Value::Null),
        }],
        "suggestedNextActor": suggested_next_actor,
        "suggestedAction": suggested_action,
        "handoff": handoff,
        "boundary": "READ_ONLY evidence candidate; verify before any mutation or external message.",
    })
}

fn affected_surface_for_tool(tool: &str) -> &'static str {
    match tool {
        "getBotKnowledgeGaps" | "getKnowledgeBaseStats" | "getLowQualityBotResponses" => {
            "customer_support"
        }
        "getGroupAttributionReport" | "getBotConversationStats" => "growth",
        "listPendingFulfillmentOrders" | "listSuppliers" | "getProductSourcingHealth" => "sourcing",
        "getDisputeBacklog" => "disputes",
        "listPendingOrders" | "getOrderFlowFunnel" => "orders",
        "storeHealthCheck" | "getOpsBacklog" => "marketplace_ops",
        "script_health" | "test_runner" => "test_automation",
        _ => "unknown",
    }
}

fn ops_event_for_candidate(
    source_task_type: &str,
    candidate_type: &str,
    tool: &str,
    evidence_type: &str,
    evidence_text: &str,
    suggested_next_actor: &str,
    suggested_action: &str,
) -> Value {
    let integration_signal = matches!(evidence_type, "mcp_error" | "mcp_auth_required");
    let external_mcp_contract_blocker =
        is_external_mcp_contract_blocker(evidence_type, evidence_text);
    let source = if integration_signal {
        if external_mcp_contract_blocker {
            "agoramarketapi_mcp_auth"
        } else {
            "sirin_ops_integration"
        }
    } else if source_task_type == SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE {
        "sirin_test_health"
    } else {
        "agoramarket_ops_mcp"
    };
    let target_project = if external_mcp_contract_blocker {
        "AgoraMarketAPI"
    } else if source_task_type == SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE || integration_signal {
        "Sirin"
    } else {
        "AgoraMarket"
    };
    let event_type = ops_event_type(tool, candidate_type);
    let evidence_level = evidence_level_for(source_task_type, candidate_type, evidence_type);
    let allowed_action = allowed_action_for(
        source_task_type,
        candidate_type,
        evidence_type,
        suggested_next_actor,
        suggested_action,
    );
    json!({
        "eventType": event_type,
        "source": source,
        "targetProject": target_project,
        "evidenceLevel": evidence_level,
        "allowedAction": allowed_action,
        "requiresReview": true,
        "productIssueGate": {
            "mayOpenProductIssue": allowed_action == "OPEN_PRODUCT_ISSUE_AFTER_REVIEW",
            "reason": product_issue_gate_reason(source_task_type, candidate_type, evidence_type, evidence_text),
        }
    })
}

fn is_external_mcp_contract_blocker(evidence_type: &str, evidence_text: &str) -> bool {
    matches!(evidence_type, "mcp_error" | "mcp_auth_required")
        && evidence_text.contains("BATCH_PLAN_REQUIRED")
        && (evidence_text.contains("deniedTool") || evidence_text.contains("requiredNextAction"))
}

fn ops_event_type(tool: &str, candidate_type: &str) -> &'static str {
    if candidate_type == "INTEGRATION_BLOCKER" {
        return "mcp_integration";
    }
    match tool {
        "storeHealthCheck" | "getOpsBacklog" => "service_health",
        "listPendingOrders" | "getOrderFlowFunnel" => "order_risk",
        "getDisputeBacklog" => "trust_support",
        "listPendingFulfillmentOrders" | "listSuppliers" | "getProductSourcingHealth" => {
            "seller_fulfillment"
        }
        "getBotKnowledgeGaps" | "getKnowledgeBaseStats" | "getLowQualityBotResponses" => {
            "support_content"
        }
        "getGroupAttributionReport" | "getBotConversationStats" => "growth_signal",
        "script_health" | "test_runner" => match candidate_type {
            "TEST_REGRESSION" => "service_health_validation",
            "REPLAY_DRIFT" | "REPLAY_STALE" => "test_replay_health",
            "COVERAGE_GAP" => "test_coverage_gap",
            _ => "test_automation",
        },
        _ => "ops_signal",
    }
}

fn evidence_level_for(
    source_task_type: &str,
    candidate_type: &str,
    evidence_type: &str,
) -> &'static str {
    if evidence_type == "mcp_auth_required" {
        return "needs_verification";
    }
    if evidence_type == "mcp_error" {
        return "suspected";
    }
    if source_task_type == SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE {
        return match candidate_type {
            "TEST_REGRESSION" => "needs_verification",
            "COVERAGE_GAP" | "REPLAY_DRIFT" | "REPLAY_STALE" => "observed",
            _ => "suspected",
        };
    }
    match candidate_type {
        "FULFILLMENT_FOLLOWUP" => {
            if evidence_type == "mcp_output" {
                "observed"
            } else {
                "suspected"
            }
        }
        "GROWTH_GAP" => "hypothesis",
        "OPS_RISK" | "DATA_ANOMALY" => "observed",
        _ => "suspected",
    }
}

fn allowed_action_for(
    source_task_type: &str,
    candidate_type: &str,
    evidence_type: &str,
    suggested_next_actor: &str,
    suggested_action: &str,
) -> &'static str {
    if source_task_type == SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE {
        return "SIRIN_BACKLOG_ONLY";
    }
    if matches!(evidence_type, "mcp_error" | "mcp_auth_required") {
        return "CODEX_VERIFY_INTEGRATION";
    }
    if suggested_next_actor == "HUMAN" || suggested_action == "operator_review" {
        return "OPERATOR_REVIEW";
    }
    match candidate_type {
        "OPS_RISK" | "DATA_ANOMALY" => "OPEN_PRODUCT_ISSUE_AFTER_REVIEW",
        "GROWTH_GAP" => "HANDOFF_REVIEW_ONLY",
        _ => "HANDOFF_REVIEW_ONLY",
    }
}

fn product_issue_gate_reason(
    source_task_type: &str,
    candidate_type: &str,
    evidence_type: &str,
    evidence_text: &str,
) -> &'static str {
    if source_task_type == SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE {
        return "Sirin test health signals create Sirin backlog/handoff only; they are not product issue evidence by themselves.";
    }
    if is_external_mcp_contract_blocker(evidence_type, evidence_text) {
        return "AgoraMarketAPI MCP auth/session contract must be fixed or documented before Sirin can collect marketplace ops evidence; open/update an AgoraMarketAPI issue or handoff, not an AgoraMarket product issue.";
    }
    if evidence_type == "mcp_auth_required" {
        return "Sirin OPS MCP authorization/configuration must be fixed before Sirin can collect marketplace ops evidence; this is a Sirin integration/backlog item, not product issue evidence.";
    }
    if evidence_type == "mcp_error" {
        return "MCP/tool failures need Codex integration verification before becoming AgoraMarket service issues.";
    }
    if candidate_type == "FULFILLMENT_FOLLOWUP" {
        return "Pending fulfillment evidence creates a handoff-only operations follow-up; it should not open a product issue without operator review.";
    }
    if candidate_type == "GROWTH_GAP" {
        return "Growth signals are hypotheses and need ChatGPT/operator analysis before product issue creation.";
    }
    "Read-only AgoraMarket ops evidence may become a product issue only after review verifies user/service impact."
}

fn evidence_excerpt(text: &str) -> String {
    text.trim()
        .chars()
        .take(1200)
        .collect::<String>()
        .replace('\n', "\\n")
}

fn dedupe_key(surface: &str, tool: &str, candidate_type: &str, excerpt: &str) -> String {
    let signature = attention_signature(excerpt);
    let signature = if signature.trim().is_empty() {
        excerpt.chars().take(500).collect::<String>()
    } else {
        signature
    };
    format!(
        "agora-market:{surface}:{tool}:{candidate_type}:{}",
        stable_hash(&signature)
    )
    .to_lowercase()
}

fn attention_signature(excerpt: &str) -> String {
    excerpt
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| contains_attention_signal(line))
        .take(5)
        .collect::<Vec<_>>()
        .join("|")
        .chars()
        .take(500)
        .collect()
}

fn stable_hash(value: &str) -> String {
    // FNV-1a 64-bit: deterministic across platforms and enough for dedupe keys.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn build_candidate_handoff(
    source_task_type: &str,
    candidate_type: &str,
    severity: &str,
    confidence: f64,
    title: &str,
    tool: &str,
    affected_surface: &str,
    ops_event: &Value,
    excerpt: &str,
    suggested_next_actor: &str,
    suggested_action: &str,
    dedupe_key: &str,
) -> Value {
    let source_slug = source_task_type.to_ascii_lowercase();
    let event_type = ops_event["eventType"].as_str().unwrap_or("ops_signal");
    let evidence_level = ops_event["evidenceLevel"].as_str().unwrap_or("suspected");
    let allowed_action = ops_event["allowedAction"]
        .as_str()
        .unwrap_or("HANDOFF_REVIEW_ONLY");
    let product_issue_gate = ops_event["productIssueGate"]["reason"]
        .as_str()
        .unwrap_or("Review required before external action.");
    let issue_body = format!(
        "## Summary\n{title}\n\n## Ops event\n- eventType: `{event_type}`\n- evidenceLevel: `{evidence_level}`\n- allowedAction: `{allowed_action}`\n- productIssueGate: {product_issue_gate}\n\n## Evidence\n- source: Sirin {source_task_type}\n- tool: `{tool}`\n- affectedSurface: `{affected_surface}`\n- severity: `{severity}`\n- confidence: `{confidence:.2}`\n- dedupeKey: `{dedupe_key}`\n\n```text\n{excerpt}\n```\n\n## Boundary\nREAD_ONLY evidence only. Verify before any mutation, external message, deploy, issue creation, or issue closure.\n\n## Suggested next step\n{suggested_next_actor}: {suggested_action}\n"
    );
    json!({
        "reviewStatusHint": if allowed_action == "OPEN_PRODUCT_ISSUE_AFTER_REVIEW" { "CONVERT_TO_ISSUE" } else { "NEEDS_MORE_EVIDENCE" },
        "chatgptReviewPrompt": format!(
            "Review this AgoraMarket ops event from Sirin {source_task_type}. Decide whether it is an operational issue, a growth hypothesis, test automation backlog, or noise. Keep all actions read-only unless explicitly authorized.\n\nTitle: {title}\nType: {candidate_type}\nEvent type: {event_type}\nEvidence level: {evidence_level}\nAllowed action: {allowed_action}\nSeverity: {severity}\nConfidence: {confidence:.2}\nTool: {tool}\nProduct issue gate: {product_issue_gate}\nEvidence:\n{excerpt}"
        ),
        "codexTaskPayload": {
            "objective": format!("Verify AgoraMarket ops event: {title}"),
            "dedupeKey": dedupe_key,
            "source": format!("sirin.{source_slug}"),
            "requiredBoundary": "read-only verification first; no production mutation without explicit approval",
            "opsEvent": ops_event,
            "evidence": {
                "tool": tool,
                "affectedSurface": affected_surface,
                "excerpt": excerpt
            }
        },
        "githubIssueDraft": {
            "title": title,
            "labels": ["sirin-scout", "needs-verification", affected_surface, severity],
            "body": issue_body
        }
    })
}

fn build_issue_scout_summary(candidates: &[Value]) -> String {
    if candidates.is_empty() {
        return "AgoraMarket issue scout found no candidate requiring review. Boundary: READ_ONLY."
            .into();
    }
    let high = candidates
        .iter()
        .filter(|c| c.get("severity").and_then(Value::as_str) == Some("HIGH"))
        .count();
    let medium = candidates
        .iter()
        .filter(|c| c.get("severity").and_then(Value::as_str) == Some("MEDIUM"))
        .count();
    let low = candidates.len().saturating_sub(high + medium);
    format!(
        "AgoraMarket issue scout found {} candidate(s): HIGH={}, MEDIUM={}, LOW={}. Boundary: READ_ONLY; ChatGPT/Codex/Human must verify before action.",
        candidates.len(),
        high,
        medium,
        low
    )
}

fn build_test_health_summary(health: &Value, candidates: &[Value]) -> String {
    let suite = health
        .get("suite")
        .and_then(Value::as_str)
        .unwrap_or("active-smoke");
    let aggregate = health.get("aggregate").unwrap_or(&Value::Null);
    let total = aggregate
        .get("total_runs")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let failed = aggregate.get("failed").and_then(Value::as_u64).unwrap_or(0);
    let deterministic = aggregate
        .get("success_rate_deterministic")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if candidates.is_empty() {
        return format!(
            "Sirin AgoraMarket test health scout found no candidate requiring review for {suite}. total_runs={total}, failed={failed}, deterministic_rate={:.1}%. Boundary: READ_ONLY.",
            deterministic * 100.0
        );
    }
    let high = candidates
        .iter()
        .filter(|c| c.get("severity").and_then(Value::as_str) == Some("HIGH"))
        .count();
    let medium = candidates
        .iter()
        .filter(|c| c.get("severity").and_then(Value::as_str) == Some("MEDIUM"))
        .count();
    let low = candidates.len().saturating_sub(high + medium);
    format!(
        "Sirin AgoraMarket test health scout found {} candidate(s) for {suite}: HIGH={}, MEDIUM={}, LOW={}. total_runs={total}, failed={failed}, deterministic_rate={:.1}%. Boundary: READ_ONLY; ChatGPT/Codex/Human must verify before action.",
        candidates.len(),
        high,
        medium,
        low,
        deterministic * 100.0
    )
}

fn normalize_research_params(args: &mut Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    for (camel, snake) in [
        ("lookbackHours", "lookback_hours"),
        ("maxResults", "max_results"),
        ("createInbox", "create_inbox"),
        ("createReport", "create_report"),
        ("publishToKb", "publish_to_kb"),
        ("rssFeeds", "rss_feeds"),
        ("deepResearch", "deep_research"),
        ("deepResearchMode", "deep_research_mode"),
        ("maxDeepEvents", "max_deep_events"),
        ("llmAnalysis", "llm_analysis"),
    ] {
        if !obj.contains_key(snake) {
            if let Some(value) = obj.get(camel).cloned() {
                obj.insert(snake.into(), value);
            }
        }
    }
}

fn normalize_result(value: &Value) -> Result<Value, String> {
    if let Some(text) = value.get("result").and_then(Value::as_str) {
        return parse_nested_json_text(text);
    }
    if let Some(result) = value.get("result") {
        return Ok(result.clone());
    }
    Ok(value.clone())
}

fn parse_nested_json_text(text: &str) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Value::Null);
    }
    let mut parsed: Value = serde_json::from_str(text)
        .map_err(|e| format!("A2A task service returned non-JSON text result: {e}"))?;
    for _ in 0..2 {
        let Some(inner) = parsed.as_str().map(str::trim) else {
            return Ok(parsed);
        };
        if !(inner.starts_with('{') || inner.starts_with('[') || inner == "null") {
            return Ok(parsed);
        }
        parsed = serde_json::from_str(inner)
            .map_err(|e| format!("A2A task service returned invalid nested JSON text: {e}"))?;
    }
    Ok(parsed)
}

fn extract_task(value: &Value) -> Result<Option<A2aTask>, String> {
    let root = normalize_result(value)?;
    if root.is_null() {
        return Ok(None);
    }
    if root.get("task").is_some_and(Value::is_null) {
        return Ok(None);
    }
    let task_value = root.get("task").unwrap_or(&root);
    let id = task_value
        .get("taskId")
        .or_else(|| task_value.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if id.is_empty() {
        if task_value == &root {
            return Ok(None);
        }
        return Err("A2A task payload missing taskId/id".into());
    }
    let task_type = task_value
        .get("taskType")
        .or_else(|| task_value.get("type"))
        .or_else(|| task_value.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if task_type.is_empty() {
        return Err(format!("A2A task {id} missing taskType/type"));
    }
    let params = task_value
        .get("params")
        .or_else(|| task_value.get("payload"))
        .or_else(|| task_value.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(Some(A2aTask {
        id,
        task_type,
        params,
    }))
}

pub(crate) fn call_a2a_worker_status(_args: Value) -> Result<Value, String> {
    Ok(json!({
        "config_file": crate::platform::config_path(CONFIG_FILE),
        "status": current_status_with_config(),
    }))
}

pub(crate) async fn execute_local_ops_task(
    task_id: String,
    task_type: String,
    params: Value,
) -> Result<Value, String> {
    let cfg = load_config();
    let task = A2aTask {
        id: task_id.clone(),
        task_type,
        params,
    };
    let result = execute_task(&cfg, &task).await?;
    if result
        .get("issue_candidates")
        .and_then(Value::as_array)
        .is_some()
    {
        persist_issue_candidates_for_task(&task_id, &result)?;
    }
    Ok(result)
}

pub(crate) fn call_a2a_issue_candidates(args: Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 100) as usize;
    let candidates = load_filtered_issue_candidate_records(&args, limit, false)?;
    Ok(json!({
        "count": candidates.len(),
        "candidates": candidates,
        "storage_path": issue_candidates_path(),
        "boundary": "Sirin-local review inbox only; no AgoraMarketAPI code or business-state mutation."
    }))
}

pub(crate) fn call_ops_event_inbox(args: Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 100) as usize;
    let events = load_filtered_issue_candidate_records(&args, limit, true)?;
    Ok(json!({
        "count": events.len(),
        "events": events,
        "candidates": events,
        "storage_path": issue_candidates_path(),
        "boundary": "Sirin-local ops event inbox only; Sirin classifies and routes events, but does not mutate AgoraMarket/API/GitHub state."
    }))
}

fn load_filtered_issue_candidate_records(
    args: &Value,
    limit: usize,
    include_ops_filters: bool,
) -> Result<Vec<Value>, String> {
    let task_id = arg_text(args, "task_id").or_else(|| arg_text(args, "taskId"));
    let status = arg_text(args, "status").map(|s| s.to_ascii_uppercase());
    let severity = arg_text(args, "severity").map(|s| s.to_ascii_uppercase());
    let event_type = if include_ops_filters {
        arg_text(args, "event_type").or_else(|| arg_text(args, "eventType"))
    } else {
        None
    };
    let evidence_level = if include_ops_filters {
        arg_text(args, "evidence_level").or_else(|| arg_text(args, "evidenceLevel"))
    } else {
        None
    };
    let allowed_action = if include_ops_filters {
        arg_text(args, "allowed_action").or_else(|| arg_text(args, "allowedAction"))
    } else {
        None
    };
    let target_project = if include_ops_filters {
        arg_text(args, "target_project").or_else(|| arg_text(args, "targetProject"))
    } else {
        None
    };
    let mut records = load_issue_candidate_records()?;
    records.sort_by(|a, b| {
        b.get("updated_at")
            .and_then(Value::as_str)
            .cmp(&a.get("updated_at").and_then(Value::as_str))
    });
    let candidates = records
        .into_iter()
        .map(ensure_ops_event_record)
        .filter(|record| {
            task_id.as_deref().is_none_or(|expected| {
                record
                    .get("task_id")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual == expected)
            })
        })
        .filter(|record| {
            status.as_deref().is_none_or(|expected| {
                candidate_record_status(record)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
        })
        .filter(|record| {
            severity.as_deref().is_none_or(|expected| {
                record
                    .get("candidate")
                    .and_then(|candidate| candidate.get("severity"))
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
        })
        .filter(|record| {
            event_type.as_deref().is_none_or(|expected| {
                candidate_ops_field(record, "eventType")
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
        })
        .filter(|record| {
            evidence_level.as_deref().is_none_or(|expected| {
                candidate_ops_field(record, "evidenceLevel")
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
        })
        .filter(|record| {
            allowed_action.as_deref().is_none_or(|expected| {
                candidate_ops_field(record, "allowedAction")
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
        })
        .filter(|record| {
            target_project.as_deref().is_none_or(|expected| {
                candidate_ops_field(record, "targetProject")
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
        })
        .take(limit)
        .collect();
    Ok(candidates)
}

fn ensure_ops_event_record(mut record: Value) -> Value {
    let Some(candidate) = record.get("candidate").filter(|v| v.is_object()) else {
        return record;
    };
    if candidate.get("opsEvent").is_some()
        && candidate.get("evidenceLevel").is_some()
        && candidate.get("allowedAction").is_some()
    {
        return record;
    }

    let source_task_type = record
        .get("task_type")
        .and_then(Value::as_str)
        .unwrap_or(AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE);
    let candidate_type = candidate
        .get("candidateType")
        .and_then(Value::as_str)
        .unwrap_or("OPS_SIGNAL");
    let evidence = candidate
        .get("evidence")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .unwrap_or(&Value::Null);
    let tool = evidence
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let evidence_type = evidence
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("legacy_record");
    let evidence_text = evidence
        .get("excerpt")
        .or_else(|| evidence.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let suggested_next_actor = candidate
        .get("suggestedNextActor")
        .and_then(Value::as_str)
        .unwrap_or("CODEX");
    let suggested_action = candidate
        .get("suggestedAction")
        .and_then(Value::as_str)
        .unwrap_or("verify");
    let ops_event = ops_event_for_candidate(
        source_task_type,
        candidate_type,
        tool,
        evidence_type,
        evidence_text,
        suggested_next_actor,
        suggested_action,
    );
    let evidence_level = ops_event["evidenceLevel"]
        .as_str()
        .unwrap_or("suspected")
        .to_string();
    let allowed_action = ops_event["allowedAction"]
        .as_str()
        .unwrap_or("HANDOFF_REVIEW_ONLY")
        .to_string();
    if let Some(candidate) = record.get_mut("candidate").and_then(Value::as_object_mut) {
        candidate
            .entry("opsEvent")
            .or_insert_with(|| ops_event.clone());
        candidate
            .entry("evidenceLevel")
            .or_insert_with(|| json!(evidence_level));
        candidate
            .entry("allowedAction")
            .or_insert_with(|| json!(allowed_action));
    }
    record
}

fn candidate_ops_field<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    let candidate = record.get("candidate")?;
    candidate
        .get("opsEvent")
        .and_then(|event| event.get(key))
        .or_else(|| candidate.get(key))
        .and_then(Value::as_str)
}

pub(crate) fn call_a2a_review_issue_candidate(args: Value) -> Result<Value, String> {
    let candidate_id = arg_text(&args, "dedupe_key")
        .or_else(|| arg_text(&args, "dedupeKey"))
        .or_else(|| arg_text(&args, "record_id"))
        .or_else(|| arg_text(&args, "recordId"))
        .or_else(|| arg_text(&args, "id"))
        .ok_or_else(|| "dedupeKey or id is required".to_string())?;
    let review_status = arg_text(&args, "review_status")
        .or_else(|| arg_text(&args, "reviewStatus"))
        .ok_or_else(|| "reviewStatus is required".to_string())?
        .to_ascii_uppercase();
    if !matches!(
        review_status.as_str(),
        "ACCEPTED" | "REJECTED" | "NEEDS_MORE_EVIDENCE" | "NEEDS_MORE_SOURCES" | "CONVERT_TO_ISSUE"
    ) {
        return Err("reviewStatus must be ACCEPTED, REJECTED, NEEDS_MORE_EVIDENCE, NEEDS_MORE_SOURCES, or CONVERT_TO_ISSUE".into());
    }
    let task_id = arg_text(&args, "task_id").or_else(|| arg_text(&args, "taskId"));
    let reviewed_by = arg_text(&args, "reviewed_by")
        .or_else(|| arg_text(&args, "reviewedBy"))
        .unwrap_or_else(|| "codex".into());
    let review_note = arg_text(&args, "review_note").or_else(|| arg_text(&args, "reviewNote"));
    let external_issue_url =
        arg_text(&args, "external_issue_url").or_else(|| arg_text(&args, "externalIssueUrl"));
    let external_issue_project = arg_text(&args, "external_issue_project")
        .or_else(|| arg_text(&args, "externalIssueProject"));
    let external_issue_number = args
        .get("external_issue_number")
        .or_else(|| args.get("externalIssueNumber"))
        .and_then(|value| {
            value.as_u64().map(|n| json!(n)).or_else(|| {
                value
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|n| json!(n))
            })
        });

    let mut records = load_issue_candidate_records()?;
    let mut match_index = None;
    for (idx, record) in records.iter().enumerate().rev() {
        let dedupe_matches = record
            .get("dedupe_key")
            .and_then(Value::as_str)
            .is_some_and(|actual| actual == candidate_id);
        let record_id_matches = record
            .get("record_id")
            .and_then(Value::as_str)
            .is_some_and(|actual| actual == candidate_id);
        let task_matches = task_id.as_deref().is_none_or(|expected| {
            record
                .get("task_id")
                .and_then(Value::as_str)
                .is_some_and(|actual| actual == expected)
        });
        if (dedupe_matches || record_id_matches) && task_matches {
            match_index = Some(idx);
            break;
        }
    }
    let idx = match_index.ok_or_else(|| format!("issue candidate not found: {candidate_id}"))?;
    let now = Utc::now().to_rfc3339();
    let record = records
        .get_mut(idx)
        .ok_or_else(|| "matched issue candidate disappeared".to_string())?;
    let normalized = ensure_ops_event_record(record.clone());
    validate_review_status_allowed(&normalized, &review_status)?;
    *record = normalized;
    record["review_status"] = json!(review_status);
    record["review_note"] = review_note.map(Value::String).unwrap_or(Value::Null);
    record["reviewed_by"] = json!(reviewed_by);
    let default_external_issue_project = record
        .pointer("/candidate/opsEvent/targetProject")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    if external_issue_url.is_some()
        || external_issue_project.is_some()
        || external_issue_number.is_some()
    {
        record["external_issue"] = json!({
            "url": external_issue_url.unwrap_or_default(),
            "project": external_issue_project.unwrap_or(default_external_issue_project),
            "number": external_issue_number.unwrap_or(Value::Null),
            "linked_at": now,
            "linked_by": record.get("reviewed_by").cloned().unwrap_or(Value::Null),
        });
    }
    record["updated_at"] = json!(now);
    let candidate_status = record
        .get("review_status")
        .cloned()
        .unwrap_or_else(|| json!("CANDIDATE"));
    if let Some(candidate) = record.get_mut("candidate").and_then(Value::as_object_mut) {
        candidate.insert("status".into(), candidate_status);
    }
    let response = record.clone();
    write_issue_candidate_records(&records)?;
    let queued_handoff = if matches!(review_status.as_str(), "CONVERT_TO_ISSUE" | "ACCEPTED") {
        Some(upsert_handoff_queue_from_candidate(
            &response,
            &review_status,
        )?)
    } else {
        None
    };
    Ok(json!({
        "updated": true,
        "candidate": response,
        "handoff": response.get("candidate").and_then(|candidate| candidate.get("handoff")).cloned().unwrap_or(Value::Null),
        "queued_handoff": queued_handoff,
        "boundary": "Review metadata stored in Sirin only; no GitHub issue or AgoraMarketAPI mutation was performed."
    }))
}

fn validate_review_status_allowed(record: &Value, review_status: &str) -> Result<(), String> {
    let allowed_action = record
        .get("candidate")
        .and_then(|candidate| candidate.get("allowedAction"))
        .and_then(Value::as_str)
        .unwrap_or("HANDOFF_REVIEW_ONLY");
    if review_status == "CONVERT_TO_ISSUE" && allowed_action != "OPEN_PRODUCT_ISSUE_AFTER_REVIEW" {
        return Err(format!(
            "reviewStatus=CONVERT_TO_ISSUE is blocked for allowedAction={allowed_action}; use ACCEPTED/NEEDS_MORE_EVIDENCE/REJECTED or verify the ops event until allowedAction=OPEN_PRODUCT_ISSUE_AFTER_REVIEW"
        ));
    }
    Ok(())
}

pub(crate) fn call_a2a_handoff_queue(args: Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let status = arg_text(&args, "status")
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_else(|| "READY".into());
    let mut records = load_handoff_queue_records()?;
    records.sort_by(|a, b| {
        b.get("updated_at")
            .and_then(Value::as_str)
            .cmp(&a.get("updated_at").and_then(Value::as_str))
    });
    let handoffs: Vec<Value> = records
        .into_iter()
        .filter(|record| {
            status == "ALL"
                || record
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(&status))
        })
        .take(limit)
        .collect();
    Ok(json!({
        "count": handoffs.len(),
        "handoffs": handoffs,
        "storage_path": handoff_queue_path(),
        "boundary": "Sirin-local approved handoff queue only; no ChatGPT/Codex/GitHub/AgoraMarketAPI call was made."
    }))
}

pub(crate) fn call_a2a_handoff_approval_packets(args: Value) -> Result<Value, String> {
    let limit = bounded_u64_param(&args, "limit", 20, 1, 100) as usize;
    let handoff_id_filter = arg_text(&args, "handoffId").or_else(|| arg_text(&args, "handoff_id"));
    let mut records = load_handoff_queue_records()?;
    records.sort_by(|a, b| {
        handoff_approval_sort_key(a)
            .cmp(&handoff_approval_sort_key(b))
            .then_with(|| {
                b.get("updated_at")
                    .and_then(Value::as_str)
                    .cmp(&a.get("updated_at").and_then(Value::as_str))
            })
    });
    let packets: Vec<Value> = records
        .iter()
        .filter(|record| {
            record
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status.eq_ignore_ascii_case("READY"))
        })
        .filter(|record| {
            handoff_id_filter.as_ref().is_none_or(|wanted| {
                record
                    .get("handoff_id")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual == wanted)
            })
        })
        .take(limit)
        .map(build_handoff_approval_packet)
        .collect();

    Ok(json!({
        "count": packets.len(),
        "nextPacket": packets.first().cloned().unwrap_or(Value::Null),
        "packets": packets,
        "storage_path": handoff_queue_path(),
        "boundary": "Read-only operator approval packets only; Sirin did not ACK handoffs, call ChatGPT/Codex/GitHub, or mutate AgoraMarket/AgoraMarketAPI."
    }))
}

pub(crate) fn call_a2a_request_handoff_approval(args: Value) -> Result<Value, String> {
    let handoff_id = arg_text(&args, "handoffId")
        .or_else(|| arg_text(&args, "handoff_id"))
        .ok_or_else(|| "handoffId is required".to_string())?;
    let requested_by = arg_text(&args, "requestedBy")
        .or_else(|| arg_text(&args, "requested_by"))
        .unwrap_or_else(|| "sirin".into());
    let note = arg_text(&args, "note");
    let handoffs = load_handoff_queue_records()?;
    let record = handoffs
        .iter()
        .find(|record| {
            record.get("handoff_id").and_then(Value::as_str) == Some(handoff_id.as_str())
        })
        .ok_or_else(|| format!("handoff not found: {handoff_id}"))?;
    let handoff_status = record
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !handoff_status.eq_ignore_ascii_case("READY") {
        return Err(format!(
            "handoff must be READY to request operator approval; current status={handoff_status}"
        ));
    }

    let packet = build_handoff_approval_packet(record);
    let request_id = approval_request_id_for_handoff(&handoff_id);
    let now = Utc::now().to_rfc3339();
    let mut requests = load_handoff_approval_request_records()?;
    let existing_idx = requests.iter().position(|request| {
        request.get("requestId").and_then(Value::as_str) == Some(request_id.as_str())
            || request.get("handoffId").and_then(Value::as_str) == Some(handoff_id.as_str())
    });
    let previous = existing_idx.and_then(|idx| requests.get(idx).cloned());
    let request = json!({
        "schemaVersion": 1,
        "requestId": request_id,
        "handoffId": handoff_id,
        "status": previous
            .as_ref()
            .and_then(|value| value.get("status"))
            .filter(|status| status.as_str() != Some("APPROVED"))
            .cloned()
            .unwrap_or_else(|| json!("PENDING_OPERATOR_APPROVAL")),
        "title": record.get("title").cloned().unwrap_or(Value::Null),
        "targetActor": packet.get("targetActor").cloned().unwrap_or(Value::Null),
        "track": packet.get("track").cloned().unwrap_or(Value::Null),
        "approvalDecision": packet.get("approvalDecision").cloned().unwrap_or(Value::Null),
        "approvalPacket": packet,
        "requestedBy": requested_by,
        "note": note.clone().unwrap_or_default(),
        "createdAt": previous
            .as_ref()
            .and_then(|value| value.get("createdAt"))
            .cloned()
            .unwrap_or_else(|| json!(now.clone())),
        "updatedAt": now,
        "boundary": "Sirin-local approval request only; no ACK, external AI call, GitHub issue mutation, AgoraMarket/AgoraMarketAPI mutation, customer/seller message, order update, or payment action was executed."
    });
    if let Some(idx) = existing_idx {
        requests[idx] = request.clone();
    } else {
        requests.push(request.clone());
    }
    write_handoff_approval_request_records(&requests)?;

    Ok(json!({
        "createdOrUpdated": true,
        "request": request,
        "nextAction": "operator_review_required; if approved, operator may call the approvalPacket.toolCallTemplate manually.",
        "storage_path": handoff_approval_requests_path(),
        "boundary": "Only Sirin-local approval request metadata was written."
    }))
}

pub(crate) fn call_a2a_handoff_approval_requests(args: Value) -> Result<Value, String> {
    let limit = bounded_u64_param(&args, "limit", 50, 1, 200) as usize;
    let status = arg_text(&args, "status")
        .map(|value| value.to_ascii_uppercase())
        .unwrap_or_else(|| "PENDING_OPERATOR_APPROVAL".into());
    let handoff_id_filter = arg_text(&args, "handoffId").or_else(|| arg_text(&args, "handoff_id"));
    let mut requests = load_handoff_approval_request_records()?;
    requests.sort_by(|a, b| {
        b.get("updatedAt")
            .and_then(Value::as_str)
            .cmp(&a.get("updatedAt").and_then(Value::as_str))
    });
    let requests: Vec<Value> = requests
        .into_iter()
        .filter(|request| {
            status == "ALL"
                || request
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(&status))
        })
        .filter(|request| {
            handoff_id_filter.as_ref().is_none_or(|wanted| {
                request
                    .get("handoffId")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual == wanted)
            })
        })
        .take(limit)
        .collect();

    Ok(json!({
        "count": requests.len(),
        "nextRequest": requests.first().cloned().unwrap_or(Value::Null),
        "requests": requests,
        "storage_path": handoff_approval_requests_path(),
        "boundary": "Read-only Sirin-local handoff approval requests; listing does not ACK handoffs, call ChatGPT/Codex/GitHub, or mutate AgoraMarket/AgoraMarketAPI."
    }))
}

pub(crate) fn call_a2a_update_handoff_approval_request(args: Value) -> Result<Value, String> {
    let request_id = arg_text(&args, "requestId").or_else(|| arg_text(&args, "request_id"));
    let handoff_id = arg_text(&args, "handoffId").or_else(|| arg_text(&args, "handoff_id"));
    if request_id.is_none() && handoff_id.is_none() {
        return Err("requestId or handoffId is required".into());
    }
    let status = arg_text(&args, "status")
        .ok_or_else(|| "status is required".to_string())?
        .to_ascii_uppercase();
    if !matches!(
        status.as_str(),
        "PENDING_OPERATOR_APPROVAL" | "APPROVED" | "REJECTED" | "CANCELLED"
    ) {
        return Err(
            "status must be PENDING_OPERATOR_APPROVAL, APPROVED, REJECTED, or CANCELLED".into(),
        );
    }
    let reviewer = arg_text(&args, "reviewer")
        .or_else(|| arg_text(&args, "reviewedBy"))
        .or_else(|| arg_text(&args, "operator"))
        .unwrap_or_else(|| "operator".into());
    let decision = arg_text(&args, "decision").unwrap_or_else(|| status.clone());
    let note = arg_text(&args, "note");
    let approved_boundary = arg_text(&args, "approvedBoundary")
        .or_else(|| arg_text(&args, "approved_boundary"))
        .unwrap_or_else(|| {
            "Sirin-local metadata update only; no customer, seller, order, payment, FAQ, product, GitHub, AgoraMarket, or AgoraMarketAPI mutation approved."
                .into()
        });

    let mut requests = load_handoff_approval_request_records()?;
    let idx = requests
        .iter()
        .position(|request| {
            request_id.as_ref().is_some_and(|wanted| {
                request.get("requestId").and_then(Value::as_str) == Some(wanted.as_str())
            }) || handoff_id.as_ref().is_some_and(|wanted| {
                request.get("handoffId").and_then(Value::as_str) == Some(wanted.as_str())
            })
        })
        .ok_or_else(|| {
            request_id
                .as_ref()
                .map(|id| format!("approval request not found: {id}"))
                .or_else(|| {
                    handoff_id
                        .as_ref()
                        .map(|id| format!("approval request not found for handoff: {id}"))
                })
                .unwrap_or_else(|| "approval request not found".to_string())
        })?;
    let now = Utc::now().to_rfc3339();
    let request = requests
        .get_mut(idx)
        .ok_or_else(|| "matched approval request disappeared".to_string())?;
    request["status"] = json!(status.clone());
    request["updatedAt"] = json!(now);
    let review_entry = json!({
        "at": now,
        "reviewer": reviewer,
        "decision": decision,
        "status": request.get("status").cloned().unwrap_or(Value::Null),
        "note": note.clone().unwrap_or_default(),
        "approvedBoundary": approved_boundary,
        "mutationApproved": false,
        "schemaVersion": 1,
    });
    request["operatorReview"] = review_entry.clone();
    if let Some(history) = request
        .get_mut("operatorReviewHistory")
        .and_then(Value::as_array_mut)
    {
        history.push(review_entry.clone());
    } else if let Some(obj) = request.as_object_mut() {
        obj.insert(
            "operatorReviewHistory".into(),
            json!([review_entry.clone()]),
        );
    }
    let next_tool = if status == "APPROVED" {
        "a2a_update_handoff"
    } else {
        "a2a_handoff_approval_requests"
    };
    let response = request.clone();
    write_handoff_approval_request_records(&requests)?;

    Ok(json!({
        "updated": true,
        "request": response,
        "nextLocalTool": next_tool,
        "nextAction": if status == "APPROVED" {
            "operator approval recorded; local handoff may be ACKED with the embedded approvalPacket.toolCallTemplate, still without external mutation"
        } else {
            "approval request status recorded; do not ACK the handoff unless a later APPROVED status is present"
        },
        "storage_path": handoff_approval_requests_path(),
        "boundary": "Only Sirin-local approval request metadata was updated; handoff status was not ACKED and no external system was mutated."
    }))
}

pub(crate) fn call_a2a_handoff_review_inbox(args: Value) -> Result<Value, String> {
    let limit = bounded_u64_param(&args, "limit", 50, 1, 200) as usize;
    let actor_filter = arg_text(&args, "actor").map(|actor| actor.to_ascii_uppercase());
    let status_filter = arg_text(&args, "status").map(|status| status.to_ascii_uppercase());
    let mut records = load_handoff_queue_records()?;
    let approval_requests = load_handoff_approval_request_records()?;
    records.sort_by(|a, b| {
        handoff_review_inbox_sort_key(a)
            .cmp(&handoff_review_inbox_sort_key(b))
            .then_with(|| {
                b.get("updated_at")
                    .and_then(Value::as_str)
                    .cmp(&a.get("updated_at").and_then(Value::as_str))
            })
    });

    let mut ready_for_review = 0usize;
    let mut approval_required = 0usize;
    let mut legacy_ack_missing_package = 0usize;
    let mut items = Vec::new();

    for record in &records {
        if items.len() >= limit {
            break;
        }
        let Some(item) = build_handoff_review_inbox_item(record, &approval_requests) else {
            continue;
        };
        let actor_matches = actor_filter.as_ref().is_none_or(|wanted| {
            item.get("targetActor")
                .and_then(Value::as_str)
                .is_some_and(|actual| actual.eq_ignore_ascii_case(wanted))
        });
        let status_matches = status_filter.as_ref().is_none_or(|wanted| {
            item.get("status")
                .and_then(Value::as_str)
                .is_some_and(|actual| actual.eq_ignore_ascii_case(wanted))
        });
        if !actor_matches || !status_matches {
            continue;
        }
        match item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "READY_FOR_REVIEW" => ready_for_review += 1,
            "APPROVAL_REQUIRED" => approval_required += 1,
            "LEGACY_ACK_MISSING_EXECUTION_PACKAGE" => legacy_ack_missing_package += 1,
            _ => {}
        }
        items.push(item);
    }

    Ok(json!({
        "count": items.len(),
        "readyForReview": ready_for_review,
        "approvalRequired": approval_required,
        "legacyAckMissingExecutionPackage": legacy_ack_missing_package,
        "nextItem": items.first().cloned().unwrap_or(Value::Null),
        "items": items,
        "storage_path": handoff_queue_path(),
        "boundary": "Read-only handoff review inbox only; Sirin did not ACK handoffs, call ChatGPT/Codex/GitHub, or mutate AgoraMarket/AgoraMarketAPI."
    }))
}

pub(crate) fn call_a2a_update_handoff(args: Value) -> Result<Value, String> {
    let handoff_id = arg_text(&args, "handoff_id")
        .or_else(|| arg_text(&args, "handoffId"))
        .ok_or_else(|| "handoffId is required".to_string())?;
    let status = arg_text(&args, "status")
        .ok_or_else(|| "status is required".to_string())?
        .to_ascii_uppercase();
    if !matches!(status.as_str(), "READY" | "ACKED" | "DONE" | "CANCELLED") {
        return Err("status must be READY, ACKED, DONE, or CANCELLED".into());
    }
    let note = arg_text(&args, "note");
    let reviewer = arg_text(&args, "reviewer")
        .or_else(|| arg_text(&args, "reviewedBy"))
        .or_else(|| arg_text(&args, "operator"))
        .unwrap_or_else(|| "operator".into());
    let decision = arg_text(&args, "decision").unwrap_or_else(|| status.clone());
    let approved_boundary = arg_text(&args, "approvedBoundary")
        .or_else(|| arg_text(&args, "approved_boundary"))
        .unwrap_or_else(|| {
            "Sirin-local metadata update only; no external message, order, payment, product, GitHub, AgoraMarket, or AgoraMarketAPI mutation was approved."
                .into()
        });
    let blocked_mutations = string_array_param(&args, "blockedMutations")
        .or_else(|| string_array_param(&args, "blocked_mutations"))
        .unwrap_or_else(|| {
            vec![
                "customer_message".into(),
                "seller_message".into(),
                "order_update".into(),
                "payment_action".into(),
                "product_change".into(),
                "github_issue_mutation".into(),
                "agoramarket_code_change".into(),
                "agoramarketapi_code_change".into(),
            ]
        });
    let approved_actions = string_array_param(&args, "approvedActions")
        .or_else(|| string_array_param(&args, "approved_actions"))
        .unwrap_or_else(|| vec!["sirin_local_metadata_update".into()]);
    let mut records = load_handoff_queue_records()?;
    let idx = records
        .iter()
        .position(|record| {
            record.get("handoff_id").and_then(Value::as_str) == Some(handoff_id.as_str())
        })
        .ok_or_else(|| format!("handoff not found: {handoff_id}"))?;
    let now = Utc::now().to_rfc3339();
    let record = records
        .get_mut(idx)
        .ok_or_else(|| "matched handoff disappeared".to_string())?;
    record["status"] = json!(status);
    record["updated_at"] = json!(now);
    let review_entry = json!({
        "at": now,
        "reviewer": reviewer.clone(),
        "decision": decision.clone(),
        "status": record.get("status").cloned().unwrap_or(Value::Null),
        "note": note.clone().unwrap_or_default(),
        "approvedBoundary": approved_boundary.clone(),
        "approvedActions": approved_actions.clone(),
        "blockedMutations": blocked_mutations.clone(),
        "mutationApproved": false,
        "schemaVersion": 1,
    });
    record["operator_review"] = review_entry.clone();
    if let Some(reviews) = record
        .get_mut("operator_review_history")
        .and_then(Value::as_array_mut)
    {
        reviews.push(review_entry.clone());
    } else if let Some(obj) = record.as_object_mut() {
        obj.insert(
            "operator_review_history".into(),
            json!([review_entry.clone()]),
        );
    }
    if let Some(note) = note {
        let entry = json!({
            "at": Utc::now().to_rfc3339(),
            "note": note,
            "reviewer": reviewer,
            "decision": decision,
            "approvedBoundary": approved_boundary,
            "blockedMutations": blocked_mutations,
            "mutationApproved": false
        });
        if let Some(notes) = record.get_mut("notes").and_then(Value::as_array_mut) {
            notes.push(entry);
        } else if let Some(obj) = record.as_object_mut() {
            obj.insert("notes".into(), json!([entry]));
        }
    }
    if status == "ACKED" {
        record["executionPackage"] = build_handoff_execution_package(record, &review_entry, &now);
    } else if matches!(status.as_str(), "DONE" | "CANCELLED") {
        if let Some(package) = record
            .get_mut("executionPackage")
            .and_then(Value::as_object_mut)
        {
            package.insert(
                "status".into(),
                json!(if status == "DONE" {
                    "DONE"
                } else {
                    "CANCELLED"
                }),
            );
            package.insert("updatedAt".into(), json!(now));
        }
    }
    let response = record.clone();
    write_handoff_queue_records(&records)?;
    Ok(json!({
        "updated": true,
        "handoff": response,
        "boundary": "Only Sirin-local handoff metadata was updated."
    }))
}

fn handoff_approval_sort_key(record: &Value) -> usize {
    match handoff_candidate_type(record).as_str() {
        "FULFILLMENT_FOLLOWUP" => 0,
        "GROWTH_GAP" => 1,
        _ => 2,
    }
}

fn handoff_review_inbox_sort_key(record: &Value) -> usize {
    if record
        .pointer("/executionPackage/status")
        .and_then(Value::as_str)
        .is_some_and(|status| status.starts_with("READY_FOR_"))
    {
        return 0;
    }
    if record
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status.eq_ignore_ascii_case("READY"))
    {
        return 1 + handoff_approval_sort_key(record);
    }
    if record
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status.eq_ignore_ascii_case("ACKED"))
        && record.get("executionPackage").is_none_or(Value::is_null)
    {
        return 10;
    }
    99
}

fn handoff_candidate_type(record: &Value) -> String {
    record
        .pointer("/candidate/candidateType")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN")
        .to_ascii_uppercase()
}

fn build_handoff_review_inbox_item(record: &Value, approval_requests: &[Value]) -> Option<Value> {
    let record_status = record
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    if let Some(package) = record
        .get("executionPackage")
        .filter(|value| value.is_object())
    {
        let package_status = package
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if package_status.starts_with("READY_FOR_") {
            return Some(json!({
                "status": "READY_FOR_REVIEW",
                "handoffId": record.get("handoff_id").cloned().unwrap_or(Value::Null),
                "title": record.get("title").cloned().unwrap_or(Value::Null),
                "targetActor": package.get("targetActor").cloned().unwrap_or(Value::Null),
                "reviewStatus": package.get("status").cloned().unwrap_or(Value::Null),
                "allowedAction": package.get("allowedAction").cloned().unwrap_or(Value::Null),
                "executionPackage": package.clone(),
                "handoffPayloadPreview": package.get("handoffPayload").cloned().unwrap_or(Value::Null),
                "nextLocalTool": "a2a_update_handoff",
                "nextExternalActorInstruction": "External actor may review this local package only; no customer/seller/order/payment/product/GitHub/AgoraMarket/AgoraMarketAPI mutation is approved.",
                "boundary": package.get("boundary").cloned().unwrap_or_else(|| json!("Local review package only."))
            }));
        }
    }

    if record_status == "READY" {
        let packet = build_handoff_approval_packet(record);
        let approval_request =
            record
                .get("handoff_id")
                .and_then(Value::as_str)
                .and_then(|handoff_id| {
                    approval_requests.iter().find(|request| {
                        request.get("handoffId").and_then(Value::as_str) == Some(handoff_id)
                    })
                });
        let approval_request_status = approval_request
            .and_then(|request| request.get("status"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase();
        let next_local_tool = match approval_request_status.as_str() {
            "APPROVED" => "a2a_update_handoff",
            "PENDING_OPERATOR_APPROVAL" | "REJECTED" | "CANCELLED" => {
                "a2a_handoff_approval_requests"
            }
            _ => "a2a_request_handoff_approval",
        };
        let next_action = match approval_request_status.as_str() {
            "APPROVED" => "operator_approved_local_ack_allowed",
            "PENDING_OPERATOR_APPROVAL" => "wait_for_operator_approval",
            "REJECTED" | "CANCELLED" => "approval_request_closed_do_not_ack_handoff",
            _ => "request_operator_approval_before_chatgpt_or_codex_review",
        };
        return Some(json!({
            "status": "APPROVAL_REQUIRED",
            "handoffId": record.get("handoff_id").cloned().unwrap_or(Value::Null),
            "title": record.get("title").cloned().unwrap_or(Value::Null),
            "targetActor": packet.get("targetActor").cloned().unwrap_or(Value::Null),
            "track": packet.get("track").cloned().unwrap_or(Value::Null),
            "approvalRequestId": approval_request
                .and_then(|request| request.get("requestId"))
                .cloned()
                .unwrap_or(Value::Null),
            "approvalRequestStatus": approval_request
                .and_then(|request| request.get("status"))
                .cloned()
                .unwrap_or(Value::Null),
            "approvalPacket": packet,
            "nextLocalTool": next_local_tool,
            "nextAction": next_action,
            "boundary": "Approval required; Sirin did not ACK or call any external actor."
        }));
    }

    if record_status == "ACKED" && record.get("executionPackage").is_none_or(Value::is_null) {
        return Some(json!({
            "status": "LEGACY_ACK_MISSING_EXECUTION_PACKAGE",
            "handoffId": record.get("handoff_id").cloned().unwrap_or(Value::Null),
            "title": record.get("title").cloned().unwrap_or(Value::Null),
            "targetActor": record.get("target_actor").cloned().unwrap_or(Value::Null),
            "nextLocalTool": "a2a_prepare_handoff_execution_packages",
            "nextAction": "backfill_possible_only_if_structured_operator_review_exists",
            "hasOperatorReview": record.get("operator_review").is_some_and(Value::is_object),
            "boundary": "Legacy ACK metadata only; Sirin did not infer or fabricate approval evidence."
        }));
    }

    None
}

fn handoff_track_for_candidate(candidate_type: &str) -> &'static str {
    match candidate_type {
        "FULFILLMENT_FOLLOWUP" => "fulfillment_followup",
        "GROWTH_GAP" => "support_growth_review",
        _ => "handoff_review",
    }
}

fn blocked_mutations_for_handoff_candidate(candidate_type: &str) -> Vec<&'static str> {
    match candidate_type {
        "FULFILLMENT_FOLLOWUP" => vec![
            "customer_message",
            "seller_message",
            "order_update",
            "payment_action",
        ],
        "GROWTH_GAP" => vec![
            "customer_message",
            "public_faq_publish",
            "product_issue_creation",
        ],
        _ => vec![
            "external_message",
            "product_issue_creation",
            "production_mutation",
        ],
    }
}

fn handoff_decision_for_actor(actor: &str) -> &'static str {
    if actor.eq_ignore_ascii_case("CODEX") {
        "ACK_FOR_CODEX_REVIEW"
    } else {
        "ACK_FOR_CHATGPT_REVIEW"
    }
}

fn expected_execution_package_status_for_actor(actor: &str) -> &'static str {
    if actor.eq_ignore_ascii_case("CODEX") {
        "READY_FOR_CODEX_REVIEW"
    } else {
        "READY_FOR_CHATGPT_REVIEW"
    }
}

fn build_handoff_approval_packet(record: &Value) -> Value {
    let handoff_id = record.get("handoff_id").cloned().unwrap_or(Value::Null);
    let candidate_type = handoff_candidate_type(record);
    let track = handoff_track_for_candidate(&candidate_type);
    let blocked_mutations = blocked_mutations_for_handoff_candidate(&candidate_type);
    let target_actor = record
        .get("target_actor")
        .or_else(|| record.pointer("/candidate/suggestedNextActor"))
        .and_then(Value::as_str)
        .unwrap_or("CHATGPT")
        .to_ascii_uppercase();
    let decision = handoff_decision_for_actor(&target_actor);
    let expected_package_status = expected_execution_package_status_for_actor(&target_actor);
    let approved_boundary = "Sirin-local metadata update only; no customer, seller, order, payment, FAQ, product, GitHub, AgoraMarket, or AgoraMarketAPI mutation approved.";
    let review_prompt = record
        .pointer("/handoff/chatgptReviewPrompt")
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "handoffId": handoff_id,
        "title": record.get("title").cloned().unwrap_or(Value::Null),
        "track": track,
        "targetActor": target_actor,
        "candidateType": candidate_type,
        "allowedAction": record.pointer("/candidate/allowedAction").cloned().unwrap_or(Value::Null),
        "approvalDecision": decision,
        "localTool": "a2a_update_handoff",
        "toolCallTemplate": {
            "name": "a2a_update_handoff",
            "arguments": {
                "handoffId": record.get("handoff_id").cloned().unwrap_or(Value::Null),
                "status": "ACKED",
                "note": "operator reviewed; no external mutation executed by Sirin",
                "reviewer": "<operator-id>",
                "decision": decision,
                "approvedBoundary": approved_boundary,
                "approvedActions": ["sirin_local_metadata_update"],
                "blockedMutations": blocked_mutations.clone()
            }
        },
        "expectedExecutionPackage": {
            "willBeCreatedOnAck": true,
            "status": expected_package_status,
            "allowedAction": "HANDOFF_REVIEW_ONLY",
            "targetActor": target_actor,
            "blockedMutations": blocked_mutations.clone(),
        },
        "handoffPayloadPreview": {
            "chatgptReviewPrompt": review_prompt,
            "codexTaskPayload": record.pointer("/handoff/codexTaskPayload").cloned().unwrap_or(Value::Null),
            "githubIssueDraft": record.pointer("/handoff/githubIssueDraft").cloned().unwrap_or(Value::Null),
            "externalIssue": record.get("external_issue").cloned().unwrap_or(Value::Null),
        },
        "operatorChecklist": [
            "Confirm the handoff is still relevant to repeatable orders, trust, conversion, or fulfillment.",
            "Confirm this approval only permits Sirin-local metadata ACK and executionPackage creation.",
            "Confirm no customer/seller/order/payment/product/GitHub/AgoraMarket/AgoraMarketAPI mutation is approved.",
            "If approved, replace <operator-id> and call the provided a2a_update_handoff template."
        ],
        "boundary": "Approval packet only; Sirin did not ACK, message users, publish FAQ, create issues, update orders, touch payment, call ChatGPT, call Codex, or mutate AgoraMarket/AgoraMarketAPI."
    })
}

pub(crate) fn call_a2a_prepare_handoff_execution_packages(args: Value) -> Result<Value, String> {
    let limit = bounded_u64_param(&args, "limit", 50, 1, 200) as usize;
    let dry_run = bool_param(&args, "dryRun", true);
    let mut records = load_handoff_queue_records()?;
    let now = Utc::now().to_rfc3339();
    let mut prepared = Vec::new();
    let mut skipped = Vec::new();
    let mut changed = 0usize;

    for record in records.iter_mut() {
        if prepared.len() >= limit {
            break;
        }
        let handoff_id = record
            .get("handoff_id")
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_string();
        let status = record
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !status.eq_ignore_ascii_case("ACKED") {
            continue;
        }
        if record
            .get("executionPackage")
            .is_some_and(|package| package.is_object())
        {
            skipped.push(json!({
                "handoffId": handoff_id,
                "reason": "execution_package_already_exists"
            }));
            continue;
        }
        let Some(review_entry) = record.get("operator_review").cloned() else {
            skipped.push(json!({
                "handoffId": handoff_id,
                "reason": "missing_operator_review"
            }));
            continue;
        };
        if !review_entry.is_object() {
            skipped.push(json!({
                "handoffId": handoff_id,
                "reason": "invalid_operator_review"
            }));
            continue;
        }
        let package = build_handoff_execution_package(record, &review_entry, &now);
        prepared.push(json!({
            "handoffId": handoff_id,
            "title": record.get("title").cloned().unwrap_or(Value::Null),
            "targetActor": package.get("targetActor").cloned().unwrap_or(Value::Null),
            "status": package.get("status").cloned().unwrap_or(Value::Null),
            "allowedAction": package.get("allowedAction").cloned().unwrap_or(Value::Null),
        }));
        if !dry_run {
            record["executionPackage"] = package;
            record["updated_at"] = json!(now);
            changed += 1;
        }
    }

    if !dry_run && changed > 0 {
        write_handoff_queue_records(&records)?;
    }

    Ok(json!({
        "dryRun": dry_run,
        "preparedCount": prepared.len(),
        "changedCount": changed,
        "prepared": prepared,
        "skipped": skipped,
        "storage_path": handoff_queue_path(),
        "boundary": "Sirin-local handoff execution package preparation only; no ChatGPT/Codex/GitHub/AgoraMarketAPI call was made."
    }))
}

fn build_handoff_execution_package(record: &Value, review_entry: &Value, now: &str) -> Value {
    let handoff = record.get("handoff").unwrap_or(&Value::Null);
    let candidate = record.get("candidate").unwrap_or(&Value::Null);
    let decision = review_entry
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    let target_actor = if decision.contains("CHATGPT") {
        "CHATGPT".to_string()
    } else if decision.contains("CODEX") {
        "CODEX".to_string()
    } else {
        record
            .get("target_actor")
            .or_else(|| candidate.get("suggestedNextActor"))
            .and_then(Value::as_str)
            .unwrap_or("CHATGPT")
            .to_ascii_uppercase()
    };
    let blocked_mutations = review_entry
        .get("blockedMutations")
        .cloned()
        .unwrap_or_else(default_blocked_handoff_mutations);
    let approved_actions = review_entry
        .get("approvedActions")
        .cloned()
        .unwrap_or_else(|| json!(["sirin_local_metadata_update"]));
    let approved_boundary = review_entry
        .get("approvedBoundary")
        .and_then(Value::as_str)
        .unwrap_or("Sirin-local metadata update only; no external mutation was approved.");

    json!({
        "schemaVersion": 1,
        "status": if target_actor == "CODEX" { "READY_FOR_CODEX_REVIEW" } else { "READY_FOR_CHATGPT_REVIEW" },
        "createdAt": now,
        "updatedAt": now,
        "sourceHandoffId": record.get("handoff_id").cloned().unwrap_or(Value::Null),
        "sourceRecordId": record.get("source_record_id").cloned().unwrap_or(Value::Null),
        "dedupeKey": record.get("dedupe_key").cloned().unwrap_or(Value::Null),
        "title": record.get("title").cloned().unwrap_or(Value::Null),
        "targetActor": target_actor,
        "allowedAction": "HANDOFF_REVIEW_ONLY",
        "approval": {
            "reviewer": review_entry.get("reviewer").cloned().unwrap_or(Value::Null),
            "decision": review_entry.get("decision").cloned().unwrap_or(Value::Null),
            "reviewedAt": review_entry.get("at").cloned().unwrap_or(Value::Null),
            "approvedBoundary": approved_boundary,
            "approvedActions": approved_actions,
            "blockedMutations": blocked_mutations,
            "mutationApproved": false
        },
        "handoffPayload": {
            "chatgptReviewPrompt": handoff.get("chatgptReviewPrompt").cloned().unwrap_or(Value::Null),
            "codexTaskPayload": handoff.get("codexTaskPayload").cloned().unwrap_or(Value::Null),
            "githubIssueDraft": handoff.get("githubIssueDraft").cloned().unwrap_or(Value::Null),
            "externalIssue": record.get("external_issue").cloned().unwrap_or(Value::Null),
            "candidateSummary": {
                "candidateType": candidate.get("candidateType").cloned().unwrap_or(Value::Null),
                "severity": candidate.get("severity").cloned().unwrap_or(Value::Null),
                "allowedAction": candidate.get("allowedAction").cloned().unwrap_or(Value::Null),
                "suggestedAction": candidate.get("suggestedAction").cloned().unwrap_or(Value::Null)
            }
        },
        "boundary": "Local review package only. Do not message customers or sellers, update orders, touch payments, publish product changes, create GitHub issues, or modify AgoraMarket/AgoraMarketAPI from this package without a separate explicit approval.",
    })
}

fn default_blocked_handoff_mutations() -> Value {
    json!([
        "customer_message",
        "seller_message",
        "order_update",
        "payment_action",
        "product_change",
        "github_issue_mutation",
        "agoramarket_code_change",
        "agoramarketapi_code_change"
    ])
}

fn persist_issue_candidates_for_task(task_id: &str, result: &Value) -> Result<usize, String> {
    let Some(candidates) = result.get("issue_candidates").and_then(Value::as_array) else {
        return Ok(0);
    };
    let mut records = load_issue_candidate_records()?;
    let now = Utc::now().to_rfc3339();
    let mut persisted = 0usize;
    for candidate in candidates {
        if !candidate.is_object() {
            continue;
        }
        let dedupe_key = candidate
            .get("dedupeKey")
            .or_else(|| candidate.get("dedupe_key"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if dedupe_key.is_empty() {
            continue;
        }
        let record_id = format!("{task_id}:{dedupe_key}");
        let existing = records.iter().position(|record| {
            record
                .get("record_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == record_id)
        });
        let previous_record = existing.and_then(|idx| records.get(idx).cloned());
        let previous_review =
            find_previous_review_for_candidate(&records, &record_id, dedupe_key, candidate);
        let record = json!({
            "record_id": record_id,
            "task_id": task_id,
            "task_type": result.get("task_type").and_then(Value::as_str).unwrap_or(AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE),
            "dedupe_key": dedupe_key,
            "candidate": candidate,
            "review_status": previous_review.as_ref().and_then(|r| r.get("review_status")).cloned().unwrap_or(Value::Null),
            "review_note": previous_review.as_ref().and_then(|r| r.get("review_note")).cloned().unwrap_or(Value::Null),
            "reviewed_by": previous_review.as_ref().and_then(|r| r.get("reviewed_by")).cloned().unwrap_or(Value::Null),
            "external_issue": previous_review.as_ref().and_then(|r| r.get("external_issue")).cloned().unwrap_or(Value::Null),
            "created_at": previous_record.as_ref().and_then(|r| r.get("created_at")).cloned().unwrap_or_else(|| json!(now)),
            "updated_at": now,
            "source_summary": result.get("summary").cloned().unwrap_or(Value::Null),
            "boundary": "Sirin-local issue candidate; read-only scout output."
        });
        let should_queue_inherited_handoff = matches!(
            record.get("review_status").and_then(Value::as_str),
            Some("ACCEPTED" | "CONVERT_TO_ISSUE")
        );
        let queued_record = if should_queue_inherited_handoff {
            Some(record.clone())
        } else {
            None
        };
        if let Some(idx) = existing {
            records[idx] = record;
        } else {
            records.push(record);
        }
        if let Some(record) = queued_record {
            let review_status = record
                .get("review_status")
                .and_then(Value::as_str)
                .unwrap_or("ACCEPTED");
            upsert_handoff_queue_from_candidate(&record, review_status)?;
        }
        persisted += 1;
    }
    write_issue_candidate_records(&records)?;
    Ok(persisted)
}

fn find_previous_review_for_dedupe(
    records: &[Value],
    record_id: &str,
    dedupe_key: &str,
) -> Option<Value> {
    records
        .iter()
        .find(|record| {
            record.get("record_id").and_then(Value::as_str) == Some(record_id)
                && record_has_review_metadata(record)
        })
        .cloned()
        .or_else(|| {
            records
                .iter()
                .find(|record| {
                    record.get("dedupe_key").and_then(Value::as_str) == Some(dedupe_key)
                        && record_has_review_metadata(record)
                })
                .cloned()
        })
}

fn find_previous_review_for_candidate(
    records: &[Value],
    record_id: &str,
    dedupe_key: &str,
    candidate: &Value,
) -> Option<Value> {
    find_previous_review_for_dedupe(records, record_id, dedupe_key).or_else(|| {
        let operation_key = candidate_operation_key(candidate);
        if operation_key.is_empty() {
            return None;
        }
        records
            .iter()
            .rev()
            .find(|record| {
                record_has_review_metadata(record) && record_operation_key(record) == operation_key
            })
            .cloned()
    })
}

fn candidate_operation_key(candidate: &Value) -> String {
    record_operation_key(&json!({ "candidate": candidate }))
}

fn record_has_review_metadata(record: &Value) -> bool {
    record
        .get("review_status")
        .is_some_and(|status| !status.is_null())
        || record
            .get("external_issue")
            .is_some_and(external_issue_has_reference)
}

fn is_tracked_external_record(record: &Value) -> bool {
    matches!(
        record.get("review_status").and_then(Value::as_str),
        Some("ACCEPTED" | "CONVERT_TO_ISSUE")
    ) && record
        .get("external_issue")
        .is_some_and(external_issue_has_reference)
}

fn issue_candidates_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("tracking")
        .join(ISSUE_CANDIDATES_FILE)
}

fn load_issue_candidate_records() -> Result<Vec<Value>, String> {
    let path = issue_candidates_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("read issue candidates {} failed: {e}", path.display()))?;
    let mut rows = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "parse issue candidates {} line {} failed: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        rows.push(value);
    }
    Ok(rows)
}

fn write_issue_candidate_records(records: &[Value]) -> Result<(), String> {
    let path = issue_candidates_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create issue candidate dir {} failed: {e}",
                parent.display()
            )
        })?;
    }
    let tmp = unique_tmp_path(&path);
    let mut file = std::fs::File::create(&tmp)
        .map_err(|e| format!("create issue candidates {} failed: {e}", tmp.display()))?;
    for record in records {
        let line = serde_json::to_string(record)
            .map_err(|e| format!("serialize issue candidate failed: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("write issue candidate failed: {e}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        format!(
            "replace issue candidates {} with {} failed: {e}",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

fn candidate_record_status(record: &Value) -> Option<&str> {
    record
        .get("review_status")
        .and_then(Value::as_str)
        .or_else(|| {
            record
                .get("candidate")
                .and_then(|candidate| candidate.get("status"))
                .and_then(Value::as_str)
        })
}

fn upsert_handoff_queue_from_candidate(
    record: &Value,
    review_status: &str,
) -> Result<Value, String> {
    let record_id = record
        .get("record_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "candidate record missing record_id".to_string())?;
    let candidate = record
        .get("candidate")
        .cloned()
        .ok_or_else(|| "candidate record missing candidate".to_string())?;
    let handoff = candidate
        .get("handoff")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let title = candidate
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| {
            handoff
                .get("githubIssueDraft")
                .and_then(|draft| draft.get("title"))
                .and_then(Value::as_str)
        })
        .unwrap_or("Untitled ops handoff")
        .to_string();
    let now = Utc::now().to_rfc3339();
    let handoff_id = format!("handoff:{record_id}");
    let dedupe_key = record
        .get("dedupe_key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut records = load_handoff_queue_records()?;
    let existing = records.iter().position(|item| {
        item.get("handoff_id")
            .and_then(Value::as_str)
            .is_some_and(|id| id == handoff_id)
            || (!dedupe_key.is_empty()
                && item
                    .get("dedupe_key")
                    .and_then(Value::as_str)
                    .is_some_and(|key| key == dedupe_key))
    });
    let previous = existing.and_then(|idx| records.get(idx).cloned());
    let queued = json!({
        "handoff_id": handoff_id,
        "source_record_id": record_id,
        "task_id": record.get("task_id").cloned().unwrap_or(Value::Null),
        "dedupe_key": record.get("dedupe_key").cloned().unwrap_or(Value::Null),
        "title": title,
        "target_actor": candidate.get("suggestedNextActor").cloned().unwrap_or_else(|| json!("CODEX")),
        "review_status": review_status,
        "status": previous.as_ref().and_then(|p| p.get("status")).cloned().unwrap_or_else(|| json!("READY")),
        "external_issue": record.get("external_issue").cloned().unwrap_or(Value::Null),
        "candidate": candidate,
        "handoff": handoff,
        "created_at": previous.as_ref().and_then(|p| p.get("created_at")).cloned().unwrap_or_else(|| json!(now)),
        "updated_at": now,
        "notes": previous.as_ref().and_then(|p| p.get("notes")).cloned().unwrap_or_else(|| json!([])),
        "boundary": "Sirin-local approved handoff; operator must explicitly hand to ChatGPT/Codex."
    });
    if let Some(idx) = existing {
        records[idx] = queued.clone();
    } else {
        records.push(queued.clone());
    }
    write_handoff_queue_records(&records)?;
    Ok(queued)
}

fn handoff_queue_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("tracking")
        .join(HANDOFF_QUEUE_FILE)
}

fn handoff_approval_requests_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("tracking")
        .join(HANDOFF_APPROVAL_REQUESTS_FILE)
}

fn approval_request_id_for_handoff(handoff_id: &str) -> String {
    format!("approval:{handoff_id}")
}

fn unique_tmp_path(path: &std::path::Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("sirin.jsonl");
    let suffix = format!(
        "{}-{:?}-{}",
        std::process::id(),
        std::thread::current().id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
    .replace(['(', ')', ' '], "_");
    path.with_file_name(format!("{file_name}.{suffix}.tmp"))
}

fn load_handoff_queue_records() -> Result<Vec<Value>, String> {
    let path = handoff_queue_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("read handoff queue {} failed: {e}", path.display()))?;
    let mut rows = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "parse handoff queue {} line {} failed: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        rows.push(value);
    }
    Ok(rows)
}

fn write_handoff_queue_records(records: &[Value]) -> Result<(), String> {
    let path = handoff_queue_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create handoff queue dir {} failed: {e}", parent.display()))?;
    }
    let tmp = unique_tmp_path(&path);
    let mut file = std::fs::File::create(&tmp)
        .map_err(|e| format!("create handoff queue {} failed: {e}", tmp.display()))?;
    for record in records {
        let line =
            serde_json::to_string(record).map_err(|e| format!("serialize handoff failed: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("write handoff failed: {e}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        format!(
            "replace handoff queue {} with {} failed: {e}",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

fn load_handoff_approval_request_records() -> Result<Vec<Value>, String> {
    let path = handoff_approval_requests_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "read handoff approval requests {} failed: {e}",
            path.display()
        )
    })?;
    let mut rows = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "parse handoff approval requests {} line {} failed: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        rows.push(value);
    }
    Ok(rows)
}

fn write_handoff_approval_request_records(records: &[Value]) -> Result<(), String> {
    let path = handoff_approval_requests_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create handoff approval requests dir {} failed: {e}",
                parent.display()
            )
        })?;
    }
    let tmp = unique_tmp_path(&path);
    let mut file = std::fs::File::create(&tmp).map_err(|e| {
        format!(
            "create handoff approval requests {} failed: {e}",
            tmp.display()
        )
    })?;
    for record in records {
        let line = serde_json::to_string(record)
            .map_err(|e| format!("serialize handoff approval request failed: {e}"))?;
        writeln!(file, "{line}")
            .map_err(|e| format!("write handoff approval request failed: {e}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        format!(
            "replace handoff approval requests {} with {} failed: {e}",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

fn arg_text(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, OnceLock};

    use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
    use tokio::net::TcpListener;

    #[test]
    fn extracts_task_from_mcp_text_result() {
        let value = json!({
            "result": "{\"task\":{\"taskId\":\"t-1\",\"taskType\":\"SIRIN_RESEARCH_SENTINEL_RUN_ONCE\",\"params\":{\"topic\":\"BTC\"}}}"
        });
        let task = extract_task(&value).expect("valid payload").expect("task");
        assert_eq!(task.id, "t-1");
        assert_eq!(task.task_type, RESEARCH_SENTINEL_TASK_TYPE);
        assert_eq!(task.params["topic"], "BTC");
    }

    #[test]
    fn normalize_ops_authorization_strips_bearer_prefix() {
        assert_eq!(
            normalize_ops_authorization("Bearer ops-secret"),
            "ops-secret"
        );
        assert_eq!(
            normalize_ops_authorization(" bearer ops-secret "),
            "ops-secret"
        );
        assert_eq!(
            normalize_ops_authorization("Bearer\t  ops-secret"),
            "ops-secret"
        );
        assert_eq!(normalize_ops_authorization("ops-secret"), "ops-secret");
    }

    #[test]
    fn claude_mcp_ops_authorization_reads_x_ops_header() {
        let value = json!({
            "mcpServers": {
                "agora-ops": {
                    "headers": {
                        "Authorization": "Bearer external-ai-token",
                        "X-OPS-Authorization": "ops-secret"
                    }
                }
            }
        });

        assert_eq!(
            claude_mcp_ops_authorization(&value, "agora-ops").as_deref(),
            Some("ops-secret")
        );
    }

    #[test]
    fn claude_mcp_ops_authorization_does_not_use_external_ai_authorization() {
        let value = json!({
            "mcpServers": {
                "agora-ops": {
                    "headers": {
                        "Authorization": "Bearer external-ai-token"
                    }
                }
            }
        });

        assert_eq!(claude_mcp_ops_authorization(&value, "agora-ops"), None);
    }

    #[test]
    fn parses_mcp_error_data_from_error_text() {
        let error =
            "MCP tool error: BATCH_PLAN_REQUIRED; code=-32004; data={\"deniedTool\":\"storeHealthCheck\",\"planTool\":\"getMcpAuthProbe\"}";
        let data = mcp_error_data_from_text(error).expect("error data");
        assert_eq!(data["deniedTool"], "storeHealthCheck");
        assert_eq!(data["planTool"], "getMcpAuthProbe");
    }

    #[test]
    fn batch_plan_tool_contract_blocker_targets_agoramarketapi() {
        let candidate = issue_candidate(
            "INTEGRATION_BLOCKER",
            "MEDIUM",
            0.65,
            "AgoraMarketAPI MCP ops tool contract blocks Sirin evidence collection".into(),
            "storeHealthCheck",
            "mcp_auth_required",
            "MCP tool error: BATCH_PLAN_REQUIRED; code=-32004; data={\"deniedTool\":\"storeHealthCheck\",\"planTool\":\"getMcpAuthProbe\",\"requiredNextAction\":\"Call getMcpAuthProbe with arguments.requestedTools before protected tool calls\"}",
            "CODEX",
            "verify",
        );
        assert_eq!(candidate["opsEvent"]["source"], "agoramarketapi_mcp_auth");
        assert_eq!(candidate["opsEvent"]["targetProject"], "AgoraMarketAPI");
        assert_eq!(candidate["allowedAction"], "CODEX_VERIFY_INTEGRATION");
        assert!(candidate["opsEvent"]["productIssueGate"]["reason"]
            .as_str()
            .expect("gate reason")
            .contains("AgoraMarketAPI MCP auth/session contract"));
    }

    #[test]
    fn extracts_task_from_double_encoded_mcp_text_result() {
        let inner = json!({
            "claimed": true,
            "task": {
                "taskId": "t-1",
                "taskType": "SIRIN_RESEARCH_SENTINEL_RUN_ONCE",
                "params": { "topic": "BTC" }
            }
        })
        .to_string();
        let value = json!({ "result": json!(inner).to_string() });
        let task = extract_task(&value).expect("valid payload").expect("task");
        assert_eq!(task.id, "t-1");
        assert_eq!(task.params["topic"], "BTC");
    }

    #[test]
    fn empty_claim_result_is_idle() {
        assert!(extract_task(&json!({"task": null}))
            .expect("valid")
            .is_none());
        assert!(extract_task(&json!({})).expect("valid").is_none());
    }

    #[test]
    fn missing_task_type_is_error() {
        let err = extract_task(&json!({"taskId":"t-2"})).expect_err("missing type");
        assert!(err.contains("missing taskType"));
    }

    #[test]
    fn normalizes_server_camel_case_params() {
        let mut args = json!({
            "lookbackHours": 6,
            "maxResults": 3,
            "publishToKb": false,
            "createReport": "auto",
            "deepResearch": true,
            "deepResearchMode": "auto",
            "maxDeepEvents": 2
        });
        normalize_research_params(&mut args);
        assert_eq!(args["lookback_hours"], 6);
        assert_eq!(args["max_results"], 3);
        assert_eq!(args["publish_to_kb"], false);
        assert_eq!(args["create_report"], "auto");
        assert_eq!(args["deep_research"], true);
        assert_eq!(args["deep_research_mode"], "auto");
        assert_eq!(args["max_deep_events"], 2);
    }

    #[test]
    fn exposes_research_and_agora_ops_capabilities() {
        let caps = worker_capabilities();
        assert!(caps.contains(&RESEARCH_SENTINEL_TASK_TYPE.to_string()));
        assert!(caps.contains(&AGORA_MARKET_OPS_DAILY_BRIEF_TASK_TYPE.to_string()));
        assert!(caps.contains(&AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE.to_string()));
        assert!(caps.contains(&AGORA_MARKET_FIRST_ORDER_ACTIVATION_TASK_TYPE.to_string()));
        assert!(caps.contains(&SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE.to_string()));
    }

    #[test]
    fn first_order_activation_playbook_outputs_operator_actions_when_zero_orders() {
        let store_health = serde_json::to_string(
            "=== 🏪 Store Health Check ===\n📅 今日: 0 訂單 | GMV 0 USDT\n   production: 0 訂單 | GMV 0 USDT | excludedTest 0\n📊 近 7d: 0 訂單 | GMV 0 USDT\n   production: 0 訂單 | GMV 0 USDT | excludedTest 0\n📈 近 30d: 0 訂單 | GMV 0 USDT\n   production: 0 訂單 | GMV 0 USDT | excludedTest 0\n📦 商品: 368 總 | 357 ON_SALE | 0 PENDING_REVIEW",
        )
        .expect("json string");
        let scout = json!({
            "source_brief": {
                "sections": [
                    {
                        "tool": "storeHealthCheck",
                        "text": store_health
                    },
                    {"tool": "getOrderFlowFunnel", "text": "✅ 無訂單資料 (近 7d)"},
                    {"tool": "listPendingOrders", "text": "✅ 無待處理訂單"},
                    {
                        "tool": "getBotKnowledgeGaps",
                        "text": "❓ #1\n   📝 @agora_login_bot 有人聽說過agora 商城嗎\n❓ #2\n   📝 手機端tg不會自動登入"
                    }
                ]
            },
            "source_test_health": {
                "source_health": {
                    "aggregate": {
                        "failed": 0
                    }
                }
            },
            "ops_report": {
                "readOnlyChecks": {"failed": 0},
                "trackedExternalIssues": [{
                    "externalIssue": {
                        "url": "https://github.com/Redandan/AgoraMarket/issues/254"
                    }
                }]
            }
        });

        let playbook = build_first_order_activation_playbook(&scout, &json!({}));

        assert_eq!(playbook["status"], "activation_required");
        assert_eq!(playbook["currentState"]["productionOrders30d"], 0);
        assert_eq!(playbook["currentState"]["onSaleProducts"], 357);
        assert_eq!(playbook["currentState"]["buyerPathReady"], true);
        assert!(playbook["topBlockers"]
            .as_array()
            .expect("blockers")
            .iter()
            .any(|blocker| blocker["type"] == "conversion_gap"));
        assert!(playbook["todayActions"]
            .as_array()
            .expect("actions")
            .iter()
            .any(|action| action["actor"] == "OPERATOR"));
    }

    #[test]
    fn first_order_activation_playbook_preserves_schedule_blocker_params() {
        let scout = json!({
            "source_brief": {
                "sections": [
                    {
                        "tool": "storeHealthCheck",
                        "text": "=== 🏪 Store Health Check ===\n📅 今日: 0 訂單 | GMV 0 USDT\n   production: 0 訂單 | GMV 0 USDT | excludedTest 0\n📊 近 7d: 0 訂單 | GMV 0 USDT\n   production: 0 訂單 | GMV 0 USDT | excludedTest 0\n📈 近 30d: 0 訂單 | GMV 0 USDT\n   production: 0 訂單 | GMV 0 USDT | excludedTest 0\n📦 商品: 368 總 | 357 ON_SALE | 0 PENDING_REVIEW"
                    },
                    {"tool": "getOrderFlowFunnel", "text": "✅ 無訂單資料 (近 7d)"},
                    {"tool": "listPendingOrders", "text": "✅ 無待處理訂單"}
                ]
            },
            "source_test_health": {
                "source_health": {
                    "aggregate": {
                        "failed": 0
                    }
                }
            },
            "ops_report": {
                "readOnlyChecks": {"failed": 0},
                "trackedExternalIssues": [{
                    "externalIssue": {
                        "url": "https://github.com/Redandan/AgoraMarket/issues/254"
                    }
                }]
            }
        });
        let params = json!({
            "trackedIssues": [
                "https://github.com/Redandan/AgoraMarket/issues/256",
                "https://github.com/Redandan/AgoraMarketAPI/issues/564"
            ],
            "knownBlocker": "TH HOME_DELIVERY address creation fails with backend DATABASE_ERROR.",
            "requiredVerification": [
                "Create/read back TH HOME_DELIVERY buyer address for userId=2",
                "Rerun Makro checkout path"
            ],
            "boundary": "Sirin may inspect and report only."
        });

        let playbook = build_first_order_activation_playbook(&scout, &params);

        assert_eq!(playbook["status"], "activation_required");
        assert_eq!(playbook["boundary"], "Sirin may inspect and report only.");
        assert!(playbook["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("DATABASE_ERROR"));
        assert_eq!(
            playbook["currentState"]["trackedIssues"],
            json!([
                "https://github.com/Redandan/AgoraMarket/issues/254",
                "https://github.com/Redandan/AgoraMarket/issues/256",
                "https://github.com/Redandan/AgoraMarketAPI/issues/564"
            ])
        );
        assert_eq!(
            playbook["currentState"]["knownBlocker"],
            "TH HOME_DELIVERY address creation fails with backend DATABASE_ERROR."
        );
        assert!(playbook["topBlockers"]
            .as_array()
            .expect("blockers")
            .iter()
            .any(|blocker| blocker["type"] == "known_blocker"
                && blocker["trackedIssues"]
                    .as_array()
                    .expect("tracked issues")
                    .len()
                    == 3));
        assert_eq!(
            playbook["verification"]["requiredVerification"],
            json!([
                "Create/read back TH HOME_DELIVERY buyer address for userId=2",
                "Rerun Makro checkout path"
            ])
        );
    }

    #[test]
    fn first_order_activation_playbook_does_not_treat_resolved_blocker_as_active() {
        let scout = json!({
            "source_brief": {
                "sections": [{
                    "tool": "storeHealthCheck",
                    "text": "=== Store Health Check ===\nproduction: 0 訂單 | GMV 0 USDT\n近 30d: 0 訂單 | GMV 0 USDT\n商品: 368 總 | 357 ON_SALE"
                }]
            },
            "source_test_health": {
                "source_health": {
                    "aggregate": {
                        "failed": 0
                    }
                }
            },
            "ops_report": {
                "readOnlyChecks": {"failed": 0},
                "trackedExternalIssues": []
            }
        });
        let params = json!({
            "knownBlocker": "RESOLVED: AgoraMarketAPI#564 TH HOME_DELIVERY address create fixed."
        });

        let playbook = build_first_order_activation_playbook(&scout, &params);

        assert_eq!(
            playbook["currentState"]["knownBlocker"],
            "RESOLVED: AgoraMarketAPI#564 TH HOME_DELIVERY address create fixed."
        );
        assert_eq!(playbook["currentState"]["knownBlockerStatus"], "resolved");
        assert!(!playbook["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("active blocker"));
        assert!(!playbook["topBlockers"]
            .as_array()
            .expect("blockers")
            .iter()
            .any(|blocker| blocker["type"] == "known_blocker"));
        assert!(playbook["topBlockers"]
            .as_array()
            .expect("blockers")
            .iter()
            .any(|blocker| blocker["type"] == "conversion_gap"));
    }

    #[test]
    fn first_order_activation_playbook_marks_goal_reached_from_30d_production_order() {
        let scout = json!({
            "source_brief": {
                "sections": [{
                    "tool": "storeHealthCheck",
                    "text": "=== 🏪 Store Health Check ===\n📅 今日: 0 訂單 | GMV 0 USDT\n   production: 0 訂單 | GMV 0 USDT | excludedTest 0\n📊 近 7d: 1 訂單 | GMV 12 USDT\n   production: 1 訂單 | GMV 12 USDT | excludedTest 0\n📈 近 30d: 1 訂單 | GMV 12 USDT\n   production: 1 訂單 | GMV 12 USDT | excludedTest 0\n📦 商品: 368 總 | 357 ON_SALE | 0 PENDING_REVIEW"
                }]
            },
            "source_test_health": {
                "source_health": {
                    "aggregate": {
                        "failed": 0
                    }
                }
            },
            "ops_report": {
                "readOnlyChecks": {"failed": 0},
                "trackedExternalIssues": []
            }
        });

        let playbook = build_first_order_activation_playbook(&scout, &json!({}));

        assert_eq!(playbook["status"], "goal_reached");
        assert_eq!(playbook["currentState"]["productionOrders30d"], 1);
        assert_eq!(playbook["currentState"]["productionGmv30d"], 12.0);
        assert!(playbook["todayActions"][0]["action"]
            .as_str()
            .unwrap_or_default()
            .contains("Mark the P0 first-order schedule DONE"));
    }

    #[test]
    fn first_order_activation_playbook_tracks_first_five_order_goal() {
        let scout = json!({
            "source_brief": {
                "sections": [
                    {
                        "tool": "storeHealthCheck",
                        "text": "=== Store Health Check ===\n📅 今日: 0 訂單 | GMV 0 USDT\n   production: 0 訂單 | GMV 0 USDT | excludedTest 0\n📊 近 7d: 1 訂單 | GMV 12 USDT\n   production: 1 訂單 | GMV 12 USDT | excludedTest 0\n📈 近 30d: 1 訂單 | GMV 12 USDT\n   production: 1 訂單 | GMV 12 USDT | excludedTest 0\n商品: 368 總 | 357 ON_SALE"
                    },
                    {
                        "tool": "listPendingOrders",
                        "text": "=== 待處理訂單 (1 筆) ===\nO1 | status=PENDING_SHIPMENT | $12 USDT"
                    },
                    {
                        "tool": "getBotKnowledgeGaps",
                        "text": "❓ #1\n   📝 手機端tg不會自動登入"
                    }
                ]
            },
            "source_test_health": {
                "source_health": {
                    "aggregate": {
                        "failed": 0
                    }
                }
            },
            "ops_report": {
                "readOnlyChecks": {"failed": 0},
                "trackedExternalIssues": []
            }
        });
        let params = json!({
            "goal": "FIRST_FIVE_PRODUCTION_ORDERS",
            "successMetric": "production_orders_30d >= 5",
            "knownBlocker": "Track AgoraMarket#257 as separate cart/checkout blocker."
        });

        let playbook = build_first_order_activation_playbook(&scout, &params);

        assert_eq!(playbook["goal"], "FIRST_FIVE_PRODUCTION_ORDERS");
        assert_eq!(playbook["status"], "activation_required");
        assert_eq!(playbook["currentState"]["targetProductionOrders"], 5);
        assert_eq!(playbook["currentState"]["productionOrders30d"], 1);
        assert_eq!(
            playbook["currentState"]["remainingProductionOrdersToTarget"],
            4
        );
        assert_eq!(playbook["currentState"]["pendingShipmentCount"], 1);
        assert!(playbook["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("1/5 production orders"));
        assert!(playbook["topBlockers"]
            .as_array()
            .expect("blockers")
            .iter()
            .any(|blocker| blocker["type"] == "repeatable_order_gap"));
        assert!(playbook["topBlockers"]
            .as_array()
            .expect("blockers")
            .iter()
            .any(|blocker| blocker["type"] == "fulfillment_followup"));
        assert!(playbook["todayActions"]
            .as_array()
            .expect("actions")
            .iter()
            .any(|action| action["action"]
                .as_str()
                .unwrap_or_default()
                .contains("repeatable-order schedule")));
        assert_eq!(
            playbook["operationsCycleReport"]["reportType"],
            "AGORA_MARKET_REPEATABLE_ORDER_CYCLE"
        );
        assert_eq!(
            playbook["operationsCycleReport"]["orderProgress"]["remainingProductionOrdersToTarget"],
            4
        );
        assert_eq!(
            playbook["operationsCycleReport"]["localSchedule"]["canContinue"],
            true
        );
        assert!(playbook["operationsCycleReport"]["handoffPlan"]["codex"]
            .as_array()
            .expect("codex handoff")
            .iter()
            .any(|item| item
                .as_str()
                .unwrap_or_default()
                .contains("stop before payment confirmation")));
        assert!(playbook["operationsCycleReport"]["sirinCapabilityGaps"]
            .as_array()
            .expect("capability gaps")
            .iter()
            .any(|item| item
                .as_str()
                .unwrap_or_default()
                .contains("External issue status")));
        let handoff_candidates = build_activation_handoff_candidates(&playbook);
        assert_eq!(handoff_candidates.len(), 2);
        assert!(handoff_candidates.iter().any(|candidate| {
            candidate["candidateType"] == "FULFILLMENT_FOLLOWUP"
                && candidate["allowedAction"] == "HANDOFF_REVIEW_ONLY"
                && candidate["evidenceLevel"] == "observed"
                && candidate["suggestedNextActor"] == "CHATGPT"
                && candidate["opsEvent"]["productIssueGate"]["mayOpenProductIssue"] == false
                && candidate["opsEvent"]["productIssueGate"]["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("handoff-only operations follow-up")
        }));
        assert!(handoff_candidates.iter().any(|candidate| {
            candidate["candidateType"] == "GROWTH_GAP"
                && candidate["allowedAction"] == "HANDOFF_REVIEW_ONLY"
                && candidate["suggestedAction"] == "draft_kb_or_faq"
        }));
    }

    #[test]
    fn repeatable_order_handoff_candidates_enter_review_queue_before_handoff() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = issue_candidates_path();
        let handoff_path = handoff_queue_path();
        let original = std::fs::read_to_string(&path).ok();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&handoff_path);

        let activation = json!({
            "goal": "FIRST_FIVE_PRODUCTION_ORDERS",
            "topBlockers": [
                {
                    "type": "fulfillment_followup",
                    "severity": "P1",
                    "evidence": "=== 待處理訂單 (1 筆) ===\nO1 | status=PENDING_SHIPMENT | $12 USDT"
                },
                {
                    "type": "support_readiness_gap",
                    "severity": "P1",
                    "questions": ["手機端tg不會自動登入"]
                }
            ]
        });
        let result = json!({
            "task_type": AGORA_MARKET_FIRST_ORDER_ACTIVATION_TASK_TYPE,
            "summary": "repeatable order handoff candidates",
            "issue_candidates": build_activation_handoff_candidates(&activation)
        });

        assert_eq!(
            persist_issue_candidates_for_task("repeatable-order-test", &result)
                .expect("persist activation candidates"),
            2
        );
        let listed = call_a2a_issue_candidates(json!({
            "task_id": "repeatable-order-test",
            "status": "CANDIDATE"
        }))
        .expect("list candidates");
        assert_eq!(listed["count"], 2);
        let handoffs_before =
            call_a2a_handoff_queue(json!({ "status": "READY" })).expect("handoffs before review");
        assert_eq!(handoffs_before["count"], 0);

        let fulfillment_key = listed["candidates"]
            .as_array()
            .expect("candidates")
            .iter()
            .find(|record| record["candidate"]["candidateType"] == "FULFILLMENT_FOLLOWUP")
            .and_then(|record| record["dedupe_key"].as_str())
            .expect("fulfillment dedupe")
            .to_string();
        let reviewed = call_a2a_review_issue_candidate(json!({
            "dedupeKey": fulfillment_key,
            "taskId": "repeatable-order-test",
            "reviewStatus": "ACCEPTED",
            "reviewNote": "verified handoff-only fulfillment follow-up"
        }))
        .expect("accept fulfillment candidate");
        assert_eq!(reviewed["queued_handoff"]["status"], "READY");
        assert_eq!(reviewed["queued_handoff"]["target_actor"], "CHATGPT");
        assert_eq!(
            reviewed["queued_handoff"]["candidate"]["allowedAction"],
            "HANDOFF_REVIEW_ONLY"
        );

        let handoffs_after =
            call_a2a_handoff_queue(json!({ "status": "READY" })).expect("handoffs after review");
        assert_eq!(handoffs_after["count"], 1);

        if let Some(content) = original {
            std::fs::write(&path, content).expect("restore issue candidates");
        } else {
            let _ = std::fs::remove_file(&path);
        }
        if let Some(content) = original_handoff {
            std::fs::write(&handoff_path, content).expect("restore handoff queue");
        } else {
            let _ = std::fs::remove_file(&handoff_path);
        }
    }

    #[test]
    fn extracts_agora_ops_task_from_claim_payload() {
        let value = json!({
            "task": {
                "taskId": "ait-1",
                "taskType": "AGORA_MARKET_OPS_DAILY_BRIEF",
                "params": {
                    "days": 14,
                    "hoursOverdue": 24,
                    "includeCustomerSupport": false
                }
            }
        });
        let task = extract_task(&value).expect("valid payload").expect("task");
        assert_eq!(task.id, "ait-1");
        assert_eq!(task.task_type, AGORA_MARKET_OPS_DAILY_BRIEF_TASK_TYPE);
        assert_eq!(
            bounded_u64_param(&task.params, "hoursOverdue", 12, 1, 168),
            24
        );
        assert!(!bool_param(&task.params, "includeCustomerSupport", true));
    }

    #[test]
    fn ops_artifacts_use_ops_names() {
        let artifacts = artifacts_from_result(&json!({
            "task_type": "AGORA_MARKET_OPS_DAILY_BRIEF",
            "summary": "brief",
            "sections": []
        }));
        let names: Vec<_> = artifacts
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|item| item.get("metadata")?.get("name")?.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "agora_market_ops_daily_brief_summary",
                "agora_market_ops_daily_brief_result"
            ]
        );
    }

    #[test]
    fn ops_summary_keeps_read_only_boundary_visible() {
        let sections = vec![json!({"ok": true}), json!({"ok": false})];
        let summary = build_ops_summary(7, 12, &sections, 1);
        assert!(summary.contains("1/2 read-only MCP checks completed"));
        assert!(summary.contains("READ_ONLY"));
        assert!(summary.contains("1 check(s) failed"));
    }

    #[test]
    fn issue_scout_builds_structured_candidates_from_attention_signals() {
        let brief = json!({
            "sections": [
                {
                    "tool": "getDisputeBacklog",
                    "ok": true,
                    "text": "⚠️ 待處理爭議 backlog: 3"
                },
                {
                    "tool": "listPendingFulfillmentOrders",
                    "ok": true,
                    "text": "overdue orders: 2"
                },
                {
                    "tool": "getKnowledgeBaseStats",
                    "ok": false,
                    "error": "MCP tool error: timeout"
                }
            ]
        });
        let candidates = build_issue_candidates(&brief, 10);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0]["candidateType"], "OPS_RISK");
        assert_eq!(candidates[0]["severity"], "HIGH");
        assert_eq!(candidates[0]["affectedSurface"], "disputes");
        assert_eq!(candidates[0]["opsEvent"]["eventType"], "trust_support");
        assert_eq!(candidates[0]["evidenceLevel"], "observed");
        assert_eq!(
            candidates[0]["allowedAction"],
            "OPEN_PRODUCT_ISSUE_AFTER_REVIEW"
        );
        assert_eq!(
            candidates[0]["opsEvent"]["productIssueGate"]["mayOpenProductIssue"],
            true
        );
        assert_eq!(candidates[1]["suggestedNextActor"], "HUMAN");
        assert_eq!(candidates[1]["allowedAction"], "OPERATOR_REVIEW");
        assert_eq!(candidates[2]["suggestedNextActor"], "CODEX");
        assert_eq!(candidates[2]["evidenceLevel"], "suspected");
        assert_eq!(candidates[2]["allowedAction"], "CODEX_VERIFY_INTEGRATION");
        assert!(candidates[2]["dedupeKey"]
            .as_str()
            .expect("dedupe")
            .contains("getknowledgebasestats"));
        assert!(candidates[0]["boundary"]
            .as_str()
            .expect("boundary")
            .contains("READ_ONLY"));
        assert_eq!(
            candidates[0]["handoff"]["reviewStatusHint"],
            "CONVERT_TO_ISSUE"
        );
        assert!(candidates[0]["handoff"]["chatgptReviewPrompt"]
            .as_str()
            .expect("chatgpt prompt")
            .contains("Review this AgoraMarket ops event"));
        assert!(
            candidates[0]["handoff"]["codexTaskPayload"]["requiredBoundary"]
                .as_str()
                .expect("codex boundary")
                .contains("read-only verification")
        );
        assert!(candidates[0]["handoff"]["githubIssueDraft"]["body"]
            .as_str()
            .expect("issue body")
            .contains("Sirin AGORA_MARKET_ISSUE_SCOUT"));
        assert!(candidates[0]["handoff"]["githubIssueDraft"]["body"]
            .as_str()
            .expect("issue body")
            .contains("allowedAction"));
        let report = build_ops_report(
            &brief,
            &candidates,
            Some(&json!({
                "summary": "Sirin local test health scout found no candidate requiring review.",
                "candidate_count": 0,
                "issue_candidates": [],
            })),
            &[],
        );
        assert_eq!(report["reportType"], "AGORA_MARKET_OPERATIONS_REPORT");
        assert_eq!(report["reportStatus"], "READY_FOR_REVIEW");
        assert_eq!(report["marketplaceEvidenceReady"], true);
        assert_eq!(report["localTestHealthReady"], true);
        assert_eq!(report["actionSummary"]["status"], "ready_for_issue_review");
        assert_eq!(report["actionSummary"]["newIssueCandidates"], 1);
        assert_eq!(report["actionSummary"]["handoffOnlyCandidates"], 1);
        assert_eq!(report["actionSummary"]["healthyChecks"], 3);
        assert_eq!(report["actionSummary"]["blockedChecks"], 1);
        let categories = report["categories"].as_array().expect("categories");
        assert!(categories
            .iter()
            .any(|category| category["category"] == "support"
                && category["issues"].as_array().expect("issues").len() == 1));
        assert!(categories
            .iter()
            .any(|category| category["category"] == "seller"
                && category["issues"].as_array().expect("issues").len() == 1));
        assert!(categories
            .iter()
            .any(|category| category["category"] == "service"
                && category["issues"].as_array().expect("issues").len() == 1
                && category["checks"].as_array().expect("checks").len() == 1));
        assert!(categories.iter().any(
            |category| category["category"] == "product" && category["status"] == "not_checked"
        ));
    }

    #[test]
    fn issue_scout_ignores_explicitly_healthy_ops_sections() {
        let brief = json!({
            "sections": [
                {
                    "tool": "getOpsBacklog",
                    "ok": true,
                    "text": "=== Ops Backlog ===\n待履約訂單: 0 production | raw 8 | excludedTest 8\n待處理爭議: 0 production active | rawActive 1 | excludedTest 1\n商品待審: 0\n\n✅ 全部清空,沒事做。"
                },
                {
                    "tool": "listPendingOrders",
                    "ok": true,
                    "text": "✅ 無待處理訂單 (近 7d, status=PENDING/IN_PROGRESS)"
                },
                {
                    "tool": "getDisputeBacklog",
                    "ok": true,
                    "text": "✅ 無爭議 backlog"
                },
                {
                    "tool": "getKnowledgeBaseStats",
                    "ok": true,
                    "text": "=== Knowledge Base Stats ===\n✅ 已收錄知識文件: 45 筆\n⏳ PENDING 待補答問題: 0 筆"
                }
            ]
        });

        let candidates = build_issue_candidates(&brief, 10);
        assert!(candidates.is_empty());
    }

    #[test]
    fn issue_scout_keeps_zero_gmv_store_health_signal() {
        let brief = json!({
            "sections": [
                {
                    "tool": "storeHealthCheck",
                    "ok": true,
                    "text": "=== Store Health Check ===\n今日: 0 訂單 | GMV 0 USDT\n近 7d: 0 訂單 | GMV 0 USDT\n近 30d: 0 訂單 | GMV 0 USDT\n商品: 368 總 | 357 ON_SALE | 0 PENDING_REVIEW\n⚠️ 爭議 (近 30d): 0 筆 | 退款總額 0 USDT"
                }
            ]
        });

        let candidates = build_issue_candidates(&brief, 10);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0]["candidateType"], "DATA_ANOMALY");
        assert_eq!(candidates[0]["affectedSurface"], "marketplace_ops");
    }

    #[test]
    fn issue_scout_ignores_store_health_after_first_production_order() {
        let brief = json!({
            "sections": [
                {
                    "tool": "storeHealthCheck",
                    "ok": true,
                    "text": "=== Store Health Check (06-19 01:54) ===\n今日: 2 訂單 | GMV 51.7396 USDT\n   production: 0 訂單 | GMV 0 USDT | excludedTest 2 / 51.7396 USDT\n近 7d: 7 訂單 | GMV 172.7126 USDT\n   production: 1 訂單 | GMV 33.8301 USDT | excludedTest 6 / 138.8825 USDT\n近 30d: 7 訂單 | GMV 172.7126 USDT\n   production: 1 訂單 | GMV 33.8301 USDT | excludedTest 6 / 138.8825 USDT\n商品: 368 總 | 357 ON_SALE | 0 PENDING_REVIEW\n⚠️ 爭議 (近 30d): 0 筆 | 退款總額 0 USDT | 平均處理 0.0 hr"
                }
            ]
        });

        let candidates = build_issue_candidates(&brief, 10);
        assert!(candidates.is_empty());
    }

    #[test]
    fn issue_scout_prefers_detailed_bot_gaps_over_kb_stats_summary() {
        let brief = json!({
            "sections": [
                {
                    "tool": "getBotKnowledgeGaps",
                    "ok": true,
                    "text": "=== Bot 知識庫缺口 (PENDING 5 筆) ===\n❓ #5 幫我檢查倉位\n❓ #4 說個笑話"
                },
                {
                    "tool": "getKnowledgeBaseStats",
                    "ok": true,
                    "text": "=== Knowledge Base Stats ===\n✅ 已收錄知識文件: 45 筆\n⏳ PENDING 待補答問題: 5 筆"
                }
            ]
        });

        let candidates = build_issue_candidates(&brief, 10);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0]["candidateType"], "GROWTH_GAP");
        assert_eq!(candidates[0]["affectedSurface"], "customer_support");
        assert_eq!(candidates[0]["evidence"][0]["tool"], "getBotKnowledgeGaps");
    }

    #[test]
    fn issue_candidate_dedupe_key_is_stable_and_specific() {
        let a = issue_candidate(
            "OPS_RISK",
            "HIGH",
            0.8,
            "Review overdue fulfillment orders".into(),
            "listPendingFulfillmentOrders",
            "mcp_output",
            "overdue orders: 2",
            "HUMAN",
            "operator_review",
        );
        let b = issue_candidate(
            "OPS_RISK",
            "HIGH",
            0.8,
            "Review overdue fulfillment orders".into(),
            "listPendingFulfillmentOrders",
            "mcp_output",
            "overdue orders: 2",
            "HUMAN",
            "operator_review",
        );
        let c = issue_candidate(
            "OPS_RISK",
            "HIGH",
            0.8,
            "Review overdue fulfillment orders".into(),
            "listPendingFulfillmentOrders",
            "mcp_output",
            "overdue orders: 9",
            "HUMAN",
            "operator_review",
        );
        assert_eq!(a["dedupeKey"], b["dedupeKey"]);
        assert_ne!(a["dedupeKey"], c["dedupeKey"]);
        assert!(a["dedupeKey"]
            .as_str()
            .expect("dedupe")
            .starts_with("agora-market:sourcing:listpendingfulfillmentorders:ops_risk:"));
    }

    #[test]
    fn issue_scout_artifacts_expose_candidates_separately() {
        let artifacts = artifacts_from_result(&json!({
            "task_type": "AGORA_MARKET_ISSUE_SCOUT",
            "summary": "scout",
            "ops_report": {
                "reportType": "AGORA_MARKET_OPERATIONS_REPORT"
            },
            "issue_candidates": [{
                "candidateType": "OPS_RISK",
                "severity": "HIGH"
            }]
        }));
        let names: Vec<_> = artifacts
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|item| item.get("metadata")?.get("name")?.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "agora_market_issue_scout_summary",
                "agora_market_issue_candidates",
                "agora_market_ops_report",
                "agora_market_issue_scout_result"
            ]
        );
    }

    #[test]
    fn ops_report_includes_existing_ops_event_inbox_records() {
        let inbox_candidate = issue_candidate(
            "OPS_RISK",
            "HIGH",
            0.78,
            "Review AgoraMarket dispute backlog".into(),
            "getDisputeBacklog",
            "mcp_output",
            "待處理爭議 backlog: 3",
            "CHATGPT",
            "verify",
        );
        let report = build_ops_report(
            &json!({
                "sections": [],
                "window": { "days": 7, "hours_overdue": 12 }
            }),
            &[],
            None,
            &[json!({
                "record_id": "inbox-1",
                "task_id": "ait-inbox",
                "candidate": inbox_candidate,
                "review_status": "CANDIDATE",
                "created_at": "2026-06-14T00:00:00Z",
                "updated_at": "2026-06-14T00:00:00Z"
            })],
        );
        assert_eq!(report["opsEventInbox"]["count"], 1);
        let support = report["categories"]
            .as_array()
            .expect("categories")
            .iter()
            .find(|category| category["category"] == "support")
            .expect("support category");
        assert_eq!(support["status"], "needs_review");
        assert_eq!(support["inboxEvents"][0]["source"], "ops_event_inbox");
        assert_eq!(support["inboxEvents"][0]["recordId"], "inbox-1");
        assert!(
            report["nextActions"]
                .as_array()
                .expect("next actions")
                .iter()
                .any(|action| action["source"] == "ops_event_inbox"
                    && action["recordId"] == "inbox-1")
        );
    }

    #[test]
    fn ops_report_marks_accepted_external_issue_as_tracked() {
        let inbox_candidate = issue_candidate(
            "OPS_RISK",
            "MEDIUM",
            0.65,
            "AgoraMarketAPI MCP ops tool contract blocks Sirin evidence collection: storeHealthCheck"
                .into(),
            "storeHealthCheck",
            "mcp_auth_required",
            "MCP tool error: BATCH_PLAN_REQUIRED",
            "CODEX",
            "verify integration contract",
        );
        let report = build_ops_report(
            &json!({
                "sections": [],
                "window": { "days": 7, "hours_overdue": 12 }
            }),
            &[],
            None,
            &[json!({
                "record_id": "tracked-1",
                "task_id": "OPS-20260615-0001",
                "candidate": inbox_candidate,
                "review_status": "ACCEPTED",
                "external_issue": {
                    "url": "https://github.com/Redandan/AgoraMarketAPI/issues/560",
                    "project": "AgoraMarketAPI",
                    "number": 560
                },
                "created_at": "2026-06-15T00:00:00Z",
                "updated_at": "2026-06-15T00:00:00Z"
            })],
        );
        assert_eq!(report["opsEventInbox"]["trackedExternalIssueCount"], 1);
        assert_eq!(report["actionSummary"]["trackedExternalIssues"], 1);
        assert_eq!(report["actionSummary"]["inboxEventsNeedingReview"], 0);
        assert_eq!(
            report["trackedExternalIssues"][0]["externalIssue"]["url"],
            "https://github.com/Redandan/AgoraMarketAPI/issues/560"
        );

        let service = report["categories"]
            .as_array()
            .expect("categories")
            .iter()
            .find(|category| category["category"] == "service")
            .expect("service category");
        assert_eq!(service["status"], "tracked_external_issue");
        assert_eq!(service["inboxEvents"][0]["externalIssue"]["number"], 560);
        assert!(report["nextActions"]
            .as_array()
            .expect("next actions")
            .iter()
            .any(|action| action["source"] == "tracked_external_issue"
                && action["recordId"] == "tracked-1"
                && action["externalIssue"]["number"] == 560));
        assert!(!report["nextActions"]
            .as_array()
            .expect("next actions")
            .iter()
            .any(
                |action| action["source"] == "ops_event_inbox" && action["recordId"] == "tracked-1"
            ));
    }

    #[test]
    fn ops_report_marks_converted_external_issue_as_tracked_not_pending_review() {
        let inbox_candidate = issue_candidate(
            "DATA_ANOMALY",
            "LOW",
            0.55,
            "Review zero GMV signal".into(),
            "storeHealthCheck",
            "mcp_output",
            "30d GMV 0 USDT with 357 ON_SALE products",
            "CODEX",
            "verify",
        );
        let report = build_ops_report(
            &json!({
                "sections": [
                    {"tool": "storeHealthCheck", "ok": true, "text": "30d GMV 0 USDT with 357 ON_SALE products"}
                ]
            }),
            &[],
            None,
            &[json!({
                "record_id": "converted-1",
                "task_id": "OPS-20260615-0010",
                "candidate": inbox_candidate,
                "review_status": "CONVERT_TO_ISSUE",
                "external_issue": {
                    "url": "https://github.com/Redandan/AgoraMarket/issues/254",
                    "project": "AgoraMarket",
                    "number": 254
                },
                "created_at": "2026-06-15T00:00:00Z",
                "updated_at": "2026-06-15T00:00:00Z"
            })],
        );

        assert_eq!(report["actionSummary"]["trackedExternalIssues"], 1);
        assert_eq!(report["actionSummary"]["inboxEventsNeedingReview"], 0);
        assert_eq!(
            report["trackedExternalIssues"][0]["externalIssue"]["number"],
            254
        );
        assert!(report["nextActions"]
            .as_array()
            .expect("next actions")
            .iter()
            .any(|action| action["source"] == "tracked_external_issue"
                && action["recordId"] == "converted-1"
                && action["externalIssue"]["number"] == 254));
    }

    #[test]
    fn current_candidate_matches_tracked_external_issue_by_operation_key_when_dedupe_changes() {
        let old_candidate = issue_candidate(
            "DATA_ANOMALY",
            "LOW",
            0.55,
            "Review AgoraMarket signal from storeHealthCheck".into(),
            "storeHealthCheck",
            "mcp_output",
            "30d GMV 0 USDT at 04:13",
            "CODEX",
            "verify",
        );
        let mut new_candidate = issue_candidate(
            "DATA_ANOMALY",
            "LOW",
            0.55,
            "Review AgoraMarket signal from storeHealthCheck".into(),
            "storeHealthCheck",
            "mcp_output",
            "30d GMV 0 USDT at 05:03",
            "CODEX",
            "verify",
        );
        new_candidate["dedupeKey"] = json!("new-dedupe-key");

        let persisted = vec![json!({
            "record_id": "old-record",
            "task_id": "OPS-20260615-0015",
            "dedupe_key": "old-dedupe-key",
            "candidate": old_candidate,
            "review_status": "CONVERT_TO_ISSUE",
            "review_note": "Tracked by AgoraMarket #254",
            "reviewed_by": "codex",
            "external_issue": {
                "url": "https://github.com/Redandan/AgoraMarket/issues/254",
                "project": "AgoraMarket",
                "number": 254
            },
            "created_at": "2026-06-15T04:13:00Z",
            "updated_at": "2026-06-15T04:13:00Z"
        })];

        let (report_candidates, tracked_records) = split_current_candidates_with_tracked_reviews(
            "OPS-20260615-0016",
            &[new_candidate],
            &persisted,
        );

        assert!(report_candidates.is_empty());
        assert_eq!(tracked_records.len(), 1);
        assert_eq!(tracked_records[0]["review_status"], "CONVERT_TO_ISSUE");
        assert_eq!(tracked_records[0]["external_issue"]["number"], 254);
        assert_eq!(
            tracked_records[0]["source_summary"],
            "Current candidate matched an already tracked external issue."
        );
    }

    #[test]
    fn report_inbox_collapse_prefers_tracked_external_over_new_duplicate() {
        let tracked_candidate = issue_candidate(
            "DATA_ANOMALY",
            "LOW",
            0.55,
            "Review zero GMV signal".into(),
            "storeHealthCheck",
            "mcp_output",
            "30d GMV 0 USDT with 357 ON_SALE products",
            "CODEX",
            "verify",
        );
        let duplicate_candidate = issue_candidate(
            "DATA_ANOMALY",
            "LOW",
            0.55,
            "Review zero GMV signal".into(),
            "storeHealthCheck",
            "mcp_output",
            "30d GMV 0 USDT with 357 ON_SALE products at a later timestamp",
            "CODEX",
            "verify",
        );
        let mut records = vec![
            json!({
                "record_id": "duplicate-newer",
                "candidate": duplicate_candidate,
                "review_status": "CANDIDATE",
                "updated_at": "2026-06-15T02:00:00Z"
            }),
            json!({
                "record_id": "tracked-older",
                "candidate": tracked_candidate,
                "review_status": "CONVERT_TO_ISSUE",
                "external_issue": {
                    "url": "https://github.com/Redandan/AgoraMarket/issues/254",
                    "project": "AgoraMarket",
                    "number": 254
                },
                "updated_at": "2026-06-15T01:00:00Z"
            }),
        ];

        collapse_report_inbox_records(&mut records, 20);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["record_id"], "tracked-older");
    }

    #[test]
    fn ops_report_action_summary_separates_issue_and_handoff_candidates() {
        let product_candidate = issue_candidate(
            "DATA_ANOMALY",
            "LOW",
            0.55,
            "Review zero GMV signal".into(),
            "storeHealthCheck",
            "mcp_output",
            "30d GMV 0 USDT with 357 ON_SALE products",
            "CODEX",
            "verify",
        );
        let handoff_candidate = issue_candidate(
            "GROWTH_GAP",
            "MEDIUM",
            0.72,
            "Convert bot knowledge gaps into FAQ candidates".into(),
            "getBotKnowledgeGaps",
            "mcp_output",
            "PENDING 5 knowledge gaps",
            "CHATGPT",
            "draft_kb_or_faq",
        );
        let report = build_ops_report(
            &json!({
                "sections": [
                    {"tool": "storeHealthCheck", "ok": true, "text": "30d GMV 0 USDT with 357 ON_SALE products"},
                    {"tool": "getOpsBacklog", "ok": true, "text": "✅ 全部清空,沒事做。"}
                ]
            }),
            &[product_candidate, handoff_candidate],
            None,
            &[],
        );

        assert_eq!(report["actionSummary"]["status"], "ready_for_issue_review");
        assert_eq!(report["actionSummary"]["newIssueCandidates"], 1);
        assert_eq!(report["actionSummary"]["handoffOnlyCandidates"], 1);
        assert_eq!(report["actionSummary"]["trackedExternalIssues"], 0);
        assert_eq!(report["actionSummary"]["inboxEventsNeedingReview"], 0);
        assert_eq!(report["actionSummary"]["healthyChecks"], 2);
    }

    #[test]
    fn ops_report_routes_legacy_batch_plan_errors_to_service() {
        let legacy_candidate = issue_candidate(
            "OPS_RISK",
            "MEDIUM",
            0.65,
            "AgoraMarket MCP check failed: listPendingOrders".into(),
            "listPendingOrders",
            "mcp_error",
            "MCP tool error: BATCH_PLAN_REQUIRED",
            "CODEX",
            "verify",
        );
        assert_eq!(report_category_for_candidate(&legacy_candidate), "service");
        let report = build_ops_report(
            &json!({ "sections": [] }),
            &[],
            None,
            &[json!({
                "record_id": "legacy-batch-plan",
                "task_id": "ait-old",
                "candidate": legacy_candidate,
                "review_status": "CANDIDATE",
                "created_at": "2026-06-14T00:00:00Z",
                "updated_at": "2026-06-14T00:00:00Z"
            })],
        );
        let categories = report["categories"].as_array().expect("categories");
        let service = categories
            .iter()
            .find(|category| category["category"] == "service")
            .expect("service");
        let order = categories
            .iter()
            .find(|category| category["category"] == "order")
            .expect("order");
        assert_eq!(service["inboxEvents"].as_array().expect("events").len(), 1);
        assert_eq!(order["inboxEvents"].as_array().expect("events").len(), 0);
    }

    #[test]
    fn report_inbox_records_collapse_repeated_batch_auth_noise() {
        let batch_a = issue_candidate(
            "OPS_RISK",
            "MEDIUM",
            0.65,
            "AgoraMarket MCP check failed: listPendingOrders".into(),
            "listPendingOrders",
            "mcp_error",
            "MCP tool error: BATCH_PLAN_REQUIRED",
            "CODEX",
            "verify",
        );
        let batch_b = issue_candidate(
            "OPS_RISK",
            "MEDIUM",
            0.65,
            "AgoraMarket MCP check failed: getDisputeBacklog".into(),
            "getDisputeBacklog",
            "mcp_error",
            "MCP tool error: BATCH_PLAN_REQUIRED",
            "CODEX",
            "verify",
        );
        let dispute = issue_candidate(
            "OPS_RISK",
            "HIGH",
            0.78,
            "Review AgoraMarket dispute backlog".into(),
            "getDisputeBacklog",
            "mcp_output",
            "待處理爭議 backlog: 3",
            "CHATGPT",
            "verify",
        );
        let mut records = vec![
            json!({
                "record_id": "older-batch",
                "dedupe_key": "older-batch-key",
                "candidate": batch_a,
                "review_status": "CANDIDATE",
                "updated_at": "2026-06-14T00:00:00Z"
            }),
            json!({
                "record_id": "newer-batch",
                "dedupe_key": "newer-batch-key",
                "candidate": batch_b,
                "review_status": "CANDIDATE",
                "updated_at": "2026-06-14T01:00:00Z"
            }),
            json!({
                "record_id": "real-dispute",
                "dedupe_key": "real-dispute-key",
                "candidate": dispute,
                "review_status": "CANDIDATE",
                "updated_at": "2026-06-14T00:30:00Z"
            }),
        ];
        collapse_report_inbox_records(&mut records, 50);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["record_id"], "newer-batch");
        assert!(records
            .iter()
            .any(|record| record["record_id"] == "real-dispute"));
    }

    #[test]
    fn issue_scout_summary_counts_severity() {
        let summary = build_issue_scout_summary(&[
            json!({"severity": "HIGH"}),
            json!({"severity": "MEDIUM"}),
            json!({"severity": "LOW"}),
        ]);
        assert!(summary.contains("HIGH=1"));
        assert!(summary.contains("MEDIUM=1"));
        assert!(summary.contains("LOW=1"));
        assert!(summary.contains("READ_ONLY"));
    }

    #[test]
    fn test_health_default_excludes_routine_risk_tags() {
        let tags = test_health_exclude_tags(&json!({}), "active-smoke", false)
            .expect("routine scout should exclude risk tags by default");
        let expected: Vec<String> = DEFAULT_TEST_HEALTH_EXCLUDE_TAGS
            .iter()
            .map(|tag| (*tag).to_owned())
            .collect();
        assert_eq!(tags, expected);
    }

    #[test]
    fn test_health_explicit_exclude_tags_override_default() {
        let tags = test_health_exclude_tags(
            &json!({
                "excludeTags": ["obsolete", "custom-risk"]
            }),
            "active-smoke",
            false,
        )
        .expect("explicit excludeTags should be preserved");
        assert_eq!(tags, vec!["obsolete".to_owned(), "custom-risk".to_owned()]);
    }

    #[test]
    fn test_health_include_obsolete_or_legacy_suite_disables_default_excludes() {
        assert!(test_health_exclude_tags(&json!({}), "active-smoke", true).is_none());
        assert!(test_health_exclude_tags(&json!({}), "obsolete", false).is_none());
        assert!(test_health_exclude_tags(&json!({}), "legacy", false).is_none());
    }

    #[test]
    fn test_health_scout_builds_candidates_from_local_script_health() {
        let health = json!({
            "suite": "active-smoke",
            "aggregate": {
                "total_runs": 6,
                "failed": 1,
                "success_rate_deterministic": 0.5,
                "drift": -0.25
            },
            "per_test": [
                {
                    "test_id": "agora_c2c_seller_orders",
                    "status": "stale_fallback",
                    "has_script": true,
                    "recent_modes": [false, true, true]
                },
                {
                    "test_id": "agora_chat_sse",
                    "status": "llm_only",
                    "has_script": false,
                    "recent_modes": [false, false]
                },
                {
                    "test_id": "agora_webrtc_voice_call",
                    "status": "untested",
                    "has_script": false,
                    "recent_modes": []
                }
            ]
        });
        let failed_tests = vec![
            json!({
                "test_id": "agora_market_smoke",
                "latest_run": {
                    "run_id": "run_smoke_1",
                    "status": "timeout",
                    "duration_ms": 181211,
                    "is_replay": false,
                    "screenshot_path": "C:\\Users\\Redan\\AppData\\Local\\Sirin\\test_failures\\agora_market_smoke.png"
                }
            }),
            json!({
                "test_id": "agora_search_keyword",
                "latest_run": {
                    "run_id": "run_search_1",
                    "status": "timeout",
                    "duration_ms": 361539,
                    "is_replay": false,
                    "screenshot_path": "C:\\Users\\Redan\\AppData\\Local\\Sirin\\test_failures\\agora_search_keyword.png"
                }
            }),
        ];
        let candidates = build_test_health_candidates(&health, &failed_tests, 10, true, true);
        assert_eq!(candidates.len(), 5);
        assert_eq!(candidates[0]["candidateType"], "TEST_REGRESSION");
        assert_eq!(candidates[1]["candidateType"], "REPLAY_DRIFT");
        assert_eq!(candidates[2]["candidateType"], "REPLAY_STALE");
        assert_eq!(candidates[3]["candidateType"], "COVERAGE_GAP");
        assert_eq!(candidates[4]["candidateType"], "COVERAGE_GAP");
        assert_eq!(candidates[0]["affectedSurface"], "test_automation");
        assert_eq!(
            candidates[0]["opsEvent"]["eventType"],
            "service_health_validation"
        );
        assert_eq!(candidates[0]["evidenceLevel"], "needs_verification");
        assert_eq!(candidates[0]["allowedAction"], "SIRIN_BACKLOG_ONLY");
        assert_eq!(candidates[3]["opsEvent"]["eventType"], "test_coverage_gap");
        assert_eq!(candidates[3]["evidenceLevel"], "observed");
        assert_eq!(candidates[3]["allowedAction"], "SIRIN_BACKLOG_ONLY");
        assert_eq!(
            candidates[3]["opsEvent"]["productIssueGate"]["mayOpenProductIssue"],
            false
        );
        assert_eq!(
            candidates[3]["handoff"]["reviewStatusHint"],
            "NEEDS_MORE_EVIDENCE"
        );
        assert!(candidates[0]["handoff"]["codexTaskPayload"]["source"]
            .as_str()
            .expect("source")
            .contains("sirin.sirin_agora_test_health_scout"));
        assert!(candidates[0]["handoff"]["githubIssueDraft"]["body"]
            .as_str()
            .expect("issue body")
            .contains("SIRIN_AGORA_TEST_HEALTH_SCOUT"));
        let regression_evidence = &candidates[0]["evidence"][0]["excerpt"];
        assert!(regression_evidence
            .as_str()
            .expect("evidence excerpt")
            .contains("agora_market_smoke"));
        let parsed_regression_evidence: Value =
            serde_json::from_str(regression_evidence.as_str().expect("evidence excerpt"))
                .expect("regression evidence stays valid JSON after excerpting");
        assert_eq!(
            parsed_regression_evidence["failed_tests"][0]["latest_run"]["status"],
            "timeout"
        );
        assert_eq!(
            parsed_regression_evidence["failed_tests"][0]["latest_run"]["timeout"],
            true
        );
        let failed_ids = parsed_regression_evidence["failed_test_ids"]
            .as_array()
            .expect("failed ids");
        assert_eq!(failed_ids.len(), 2);
        assert_eq!(failed_ids[1], "agora_search_keyword");
        assert!(
            regression_evidence
                .as_str()
                .expect("evidence excerpt")
                .chars()
                .count()
                <= 1200
        );
        assert_eq!(
            candidates[0]["evidence"][0]["type"],
            "script_health_failed_tests"
        );
        let summary = build_test_health_summary(&health, &candidates);
        assert!(summary.contains("HIGH=1"));
        assert!(summary.contains("MEDIUM=2"));
        assert!(summary.contains("LOW=2"));
        assert!(summary.contains("READ_ONLY"));
    }

    #[test]
    fn test_health_dedupe_keys_include_per_test_identity() {
        let health = json!({
            "suite": "all",
            "aggregate": {
                "total_runs": 0,
                "failed": 0,
                "success_rate_deterministic": 0.0,
                "drift": null
            },
            "per_test": [
                {
                    "test_id": "agora_pickup_time_picker",
                    "status": "llm_only",
                    "has_script": false,
                    "recent_modes": [false]
                },
                {
                    "test_id": "agora_chat_sse",
                    "status": "llm_only",
                    "has_script": false,
                    "recent_modes": [false]
                }
            ]
        });
        let candidates = build_test_health_candidates(&health, &[], 10, true, true);
        assert_eq!(candidates.len(), 2);
        assert_ne!(candidates[0]["dedupeKey"], candidates[1]["dedupeKey"]);
    }

    #[test]
    fn issue_candidates_persist_and_review_in_sirin_local_inbox() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = issue_candidates_path();
        let handoff_path = handoff_queue_path();
        let original = std::fs::read_to_string(&path).ok();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&handoff_path);

        let result = json!({
            "task_type": "AGORA_MARKET_ISSUE_SCOUT",
            "summary": "scout found one",
            "issue_candidates": [{
                "status": "CANDIDATE",
                "candidateType": "OPS_RISK",
                "severity": "HIGH",
                "dedupeKey": "agora-market:test:tool:ops_risk:abc",
                "title": "Test candidate",
                "handoff": {
                    "chatgptReviewPrompt": "review",
                    "codexTaskPayload": {"objective": "verify"},
                    "githubIssueDraft": {"title": "draft"}
                }
            }]
        });

        assert_eq!(
            persist_issue_candidates_for_task("ait-test-1", &result).expect("persist"),
            1
        );
        let listed = call_a2a_issue_candidates(json!({
            "task_id": "ait-test-1",
            "severity": "HIGH"
        }))
        .expect("list");
        assert_eq!(listed["count"], 1);
        assert_eq!(
            listed["candidates"][0]["dedupe_key"],
            "agora-market:test:tool:ops_risk:abc"
        );
        let record_id = listed["candidates"][0]["record_id"]
            .as_str()
            .expect("record id")
            .to_string();

        let reviewed = call_a2a_review_issue_candidate(json!({
            "task_id": "ait-test-1",
            "id": record_id,
            "reviewStatus": "CONVERT_TO_ISSUE",
            "reviewNote": "verified enough",
            "reviewedBy": "codex",
            "externalIssueUrl": "https://github.com/Redandan/AgoraMarketAPI/issues/560",
            "externalIssueProject": "AgoraMarketAPI",
            "externalIssueNumber": 560
        }))
        .expect("review");
        assert_eq!(reviewed["updated"], true);
        assert_eq!(reviewed["candidate"]["review_status"], "CONVERT_TO_ISSUE");
        assert_eq!(
            reviewed["candidate"]["external_issue"]["url"],
            "https://github.com/Redandan/AgoraMarketAPI/issues/560"
        );
        assert_eq!(reviewed["handoff"]["githubIssueDraft"]["title"], "draft");
        assert_eq!(reviewed["queued_handoff"]["status"], "READY");
        assert_eq!(
            reviewed["queued_handoff"]["external_issue"]["project"],
            "AgoraMarketAPI"
        );

        let converted = call_a2a_issue_candidates(json!({
            "status": "CONVERT_TO_ISSUE"
        }))
        .expect("list converted");
        assert_eq!(converted["count"], 1);
        assert_eq!(
            converted["candidates"][0]["candidate"]["status"],
            "CONVERT_TO_ISSUE"
        );
        let handoffs = call_a2a_handoff_queue(json!({ "status": "READY" })).expect("handoffs");
        assert_eq!(handoffs["count"], 1);
        assert_eq!(handoffs["handoffs"][0]["title"], "Test candidate");
        assert_eq!(handoffs["handoffs"][0]["external_issue"]["number"], 560);
        let handoff_id = handoffs["handoffs"][0]["handoff_id"]
            .as_str()
            .expect("handoff id")
            .to_string();
        let acked = call_a2a_update_handoff(json!({
            "handoffId": handoff_id,
            "status": "ACKED",
            "note": "operator reviewed payload",
            "reviewer": "operator-redan",
            "decision": "ACK_FOR_CHATGPT_REVIEW",
            "approvedBoundary": "Sirin-local metadata update only; no customer or seller message approved.",
            "approvedActions": ["sirin_local_metadata_update"],
            "blockedMutations": ["customer_message", "seller_message", "order_update", "payment_action"]
        }))
        .expect("ack handoff");
        assert_eq!(acked["handoff"]["status"], "ACKED");
        assert_eq!(
            acked["handoff"]["operator_review"]["reviewer"],
            "operator-redan"
        );
        assert_eq!(
            acked["handoff"]["operator_review"]["decision"],
            "ACK_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(
            acked["handoff"]["operator_review"]["mutationApproved"],
            false
        );
        assert_eq!(
            acked["handoff"]["operator_review"]["approvedBoundary"],
            "Sirin-local metadata update only; no customer or seller message approved."
        );
        assert_eq!(
            acked["handoff"]["operator_review"]["blockedMutations"][0],
            "customer_message"
        );
        assert_eq!(
            acked["handoff"]["operator_review_history"][0]["decision"],
            "ACK_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(acked["handoff"]["notes"][0]["reviewer"], "operator-redan");
        assert_eq!(
            acked["handoff"]["executionPackage"]["status"],
            "READY_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(
            acked["handoff"]["executionPackage"]["targetActor"],
            "CHATGPT"
        );
        assert_eq!(
            acked["handoff"]["executionPackage"]["allowedAction"],
            "HANDOFF_REVIEW_ONLY"
        );
        assert_eq!(
            acked["handoff"]["executionPackage"]["approval"]["mutationApproved"],
            false
        );
        assert_eq!(
            acked["handoff"]["executionPackage"]["approval"]["blockedMutations"][2],
            "order_update"
        );
        assert_eq!(
            acked["handoff"]["executionPackage"]["handoffPayload"]["chatgptReviewPrompt"],
            "review"
        );
        assert!(acked["handoff"]["executionPackage"]["boundary"]
            .as_str()
            .unwrap()
            .contains("Do not message customers or sellers"));

        let all_handoffs =
            call_a2a_handoff_queue(json!({ "status": "ALL" })).expect("all handoffs");
        assert_eq!(
            all_handoffs["handoffs"][0]["executionPackage"]["allowedAction"],
            "HANDOFF_REVIEW_ONLY"
        );

        if let Some(content) = original {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, content);
        } else {
            let _ = std::fs::remove_file(&path);
        }
        if let Some(content) = original_handoff {
            if let Some(parent) = handoff_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&handoff_path, content);
        } else {
            let _ = std::fs::remove_file(&handoff_path);
        }
    }

    #[test]
    fn handoff_execution_package_backfill_is_local_and_dry_run_by_default() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let handoff_path = handoff_queue_path();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let _ = std::fs::remove_file(&handoff_path);

        let record = json!({
            "handoff_id": "handoff:OPS-TEST-BACKFILL:abc",
            "source_record_id": "OPS-TEST-BACKFILL:abc",
            "dedupe_key": "agora-market:orders:test-backfill",
            "title": "Backfill reviewed handoff",
            "target_actor": "CHATGPT",
            "status": "ACKED",
            "updated_at": "2026-06-19T10:25:00Z",
            "operator_review": {
                "at": "2026-06-19T10:24:00Z",
                "reviewer": "operator-redan",
                "decision": "ACK_FOR_CHATGPT_REVIEW",
                "approvedBoundary": "Sirin-local metadata update only; no customer or seller message approved.",
                "approvedActions": ["sirin_local_metadata_update"],
                "blockedMutations": ["customer_message", "seller_message", "order_update"],
                "mutationApproved": false,
                "schemaVersion": 1
            },
            "candidate": {
                "allowedAction": "HANDOFF_REVIEW_ONLY",
                "candidateType": "FULFILLMENT_FOLLOWUP",
                "severity": "P1",
                "suggestedAction": "draft_operator_followup"
            },
            "handoff": {
                "chatgptReviewPrompt": "review prompt",
                "codexTaskPayload": {"objective": "verify"},
                "githubIssueDraft": {"title": "draft"}
            }
        });
        write_handoff_queue_records(&[record]).expect("write legacy handoff");

        let dry_run = call_a2a_prepare_handoff_execution_packages(json!({})).expect("dry run");
        assert_eq!(dry_run["dryRun"], true);
        assert_eq!(dry_run["preparedCount"], 1);
        let still_legacy =
            call_a2a_handoff_queue(json!({ "status": "ALL" })).expect("list after dry run");
        assert!(still_legacy["handoffs"][0]["executionPackage"].is_null());

        let backfilled = call_a2a_prepare_handoff_execution_packages(json!({ "dryRun": false }))
            .expect("backfill");
        assert_eq!(backfilled["dryRun"], false);
        assert_eq!(backfilled["changedCount"], 1);
        assert_eq!(
            backfilled["prepared"][0]["allowedAction"],
            "HANDOFF_REVIEW_ONLY"
        );

        let listed =
            call_a2a_handoff_queue(json!({ "status": "ALL" })).expect("list after backfill");
        assert_eq!(
            listed["handoffs"][0]["executionPackage"]["status"],
            "READY_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(
            listed["handoffs"][0]["executionPackage"]["handoffPayload"]["chatgptReviewPrompt"],
            "review prompt"
        );

        if let Some(content) = original_handoff {
            if let Some(parent) = handoff_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&handoff_path, content);
        } else {
            let _ = std::fs::remove_file(&handoff_path);
        }
    }

    #[test]
    fn handoff_approval_packets_are_read_only_ack_templates() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let handoff_path = handoff_queue_path();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let _ = std::fs::remove_file(&handoff_path);

        let support_handoff = json!({
            "handoff_id": "handoff:OPS-TEST-PACKET:support",
            "dedupe_key": "agora-market:support:test",
            "title": "Convert support gaps into FAQ",
            "target_actor": "CHATGPT",
            "status": "READY",
            "updated_at": "2026-06-19T10:20:00Z",
            "candidate": {
                "allowedAction": "HANDOFF_REVIEW_ONLY",
                "candidateType": "GROWTH_GAP",
                "suggestedAction": "draft_faq"
            },
            "handoff": {
                "chatgptReviewPrompt": "support review",
                "codexTaskPayload": {"objective": "review support gaps"},
                "githubIssueDraft": {"title": "support draft"}
            }
        });
        let fulfillment_handoff = json!({
            "handoff_id": "handoff:OPS-TEST-PACKET:fulfillment",
            "dedupe_key": "agora-market:orders:test",
            "title": "Review pending shipment follow-up",
            "target_actor": "CHATGPT",
            "status": "READY",
            "updated_at": "2026-06-19T10:25:00Z",
            "candidate": {
                "allowedAction": "HANDOFF_REVIEW_ONLY",
                "candidateType": "FULFILLMENT_FOLLOWUP",
                "suggestedAction": "draft_operator_followup"
            },
            "handoff": {
                "chatgptReviewPrompt": "fulfillment review",
                "codexTaskPayload": {"objective": "review fulfillment"},
                "githubIssueDraft": {"title": "fulfillment draft"}
            }
        });
        write_handoff_queue_records(&[support_handoff, fulfillment_handoff])
            .expect("write ready handoffs");

        let packets = call_a2a_handoff_approval_packets(json!({ "limit": 10 })).expect("packets");
        assert_eq!(packets["count"], 2);
        assert_eq!(
            packets["packets"][0]["handoffId"],
            "handoff:OPS-TEST-PACKET:fulfillment"
        );
        assert_eq!(packets["packets"][0]["track"], "fulfillment_followup");
        assert_eq!(
            packets["packets"][0]["toolCallTemplate"]["arguments"]["status"],
            "ACKED"
        );
        assert_eq!(
            packets["packets"][0]["toolCallTemplate"]["arguments"]["blockedMutations"][2],
            "order_update"
        );
        assert_eq!(
            packets["packets"][0]["expectedExecutionPackage"]["status"],
            "READY_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(
            packets["packets"][0]["expectedExecutionPackage"]["allowedAction"],
            "HANDOFF_REVIEW_ONLY"
        );
        assert_eq!(
            packets["packets"][0]["handoffPayloadPreview"]["chatgptReviewPrompt"],
            "fulfillment review"
        );

        let listed = call_a2a_handoff_queue(json!({ "status": "READY" })).expect("list");
        assert_eq!(listed["count"], 2);
        assert!(listed["handoffs"][0]["executionPackage"].is_null());

        if let Some(content) = original_handoff {
            if let Some(parent) = handoff_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&handoff_path, content);
        } else {
            let _ = std::fs::remove_file(&handoff_path);
        }
    }

    #[test]
    fn handoff_approval_request_is_local_and_does_not_ack_handoff() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let handoff_path = handoff_queue_path();
        let request_path = handoff_approval_requests_path();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let original_request = std::fs::read_to_string(&request_path).ok();
        let _ = std::fs::remove_file(&handoff_path);
        let _ = std::fs::remove_file(&request_path);

        let handoff = json!({
            "handoff_id": "handoff:OPS-TEST-APPROVAL-REQUEST:fulfillment",
            "dedupe_key": "agora-market:orders:approval-request",
            "title": "Request fulfillment follow-up approval",
            "target_actor": "CHATGPT",
            "status": "READY",
            "updated_at": "2026-06-19T10:40:00Z",
            "candidate": {
                "allowedAction": "HANDOFF_REVIEW_ONLY",
                "candidateType": "FULFILLMENT_FOLLOWUP",
                "suggestedAction": "draft_operator_followup"
            },
            "handoff": {
                "chatgptReviewPrompt": "review pending shipment follow-up",
                "codexTaskPayload": {"objective": "review fulfillment"},
                "githubIssueDraft": {"title": "fulfillment draft"}
            }
        });
        write_handoff_queue_records(&[handoff]).expect("write ready handoff");

        let requested = call_a2a_request_handoff_approval(json!({
            "handoffId": "handoff:OPS-TEST-APPROVAL-REQUEST:fulfillment",
            "requestedBy": "codex",
            "note": "Ask operator to approve local review package only."
        }))
        .expect("request approval");
        assert_eq!(requested["createdOrUpdated"], true);
        assert_eq!(requested["request"]["status"], "PENDING_OPERATOR_APPROVAL");
        assert_eq!(requested["request"]["requestedBy"], "codex");
        assert_eq!(
            requested["request"]["approvalPacket"]["toolCallTemplate"]["arguments"]["status"],
            "ACKED"
        );
        assert!(requested["request"]["boundary"]
            .as_str()
            .expect("boundary")
            .contains("no ACK"));

        let requests =
            call_a2a_handoff_approval_requests(json!({ "status": "ALL" })).expect("list");
        assert_eq!(requests["count"], 1);
        assert_eq!(
            requests["requests"][0]["handoffId"],
            "handoff:OPS-TEST-APPROVAL-REQUEST:fulfillment"
        );

        let listed = call_a2a_handoff_queue(json!({ "status": "ALL" })).expect("handoffs");
        assert_eq!(listed["handoffs"][0]["status"], "READY");
        assert!(listed["handoffs"][0]["executionPackage"].is_null());

        let approved = call_a2a_update_handoff_approval_request(json!({
            "handoffId": "handoff:OPS-TEST-APPROVAL-REQUEST:fulfillment",
            "status": "APPROVED",
            "reviewer": "operator-redan",
            "decision": "APPROVE_LOCAL_ACK_ONLY",
            "note": "Approve local ACK package only."
        }))
        .expect("approve request");
        assert_eq!(approved["updated"], true);
        assert_eq!(approved["request"]["status"], "APPROVED");
        assert_eq!(
            approved["request"]["operatorReview"]["reviewer"],
            "operator-redan"
        );
        assert_eq!(approved["nextLocalTool"], "a2a_update_handoff");

        let after_approval =
            call_a2a_handoff_queue(json!({ "status": "ALL" })).expect("handoffs after approval");
        assert_eq!(after_approval["handoffs"][0]["status"], "READY");
        assert!(after_approval["handoffs"][0]["executionPackage"].is_null());

        let inbox = call_a2a_handoff_review_inbox(json!({
            "status": "APPROVAL_REQUIRED",
            "limit": 10
        }))
        .expect("handoff inbox");
        assert_eq!(inbox["items"][0]["approvalRequestStatus"], "APPROVED");
        assert_eq!(
            inbox["items"][0]["nextAction"],
            "operator_approved_local_ack_allowed"
        );
        assert_eq!(inbox["items"][0]["nextLocalTool"], "a2a_update_handoff");

        restore_test_file(&handoff_path, original_handoff);
        restore_test_file(&request_path, original_request);
    }

    #[test]
    fn handoff_review_inbox_classifies_reviewable_approval_and_legacy_items() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let handoff_path = handoff_queue_path();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let _ = std::fs::remove_file(&handoff_path);

        let reviewable = json!({
            "handoff_id": "handoff:OPS-TEST-INBOX:reviewable",
            "dedupe_key": "agora-market:orders:reviewable",
            "title": "Ready package for ChatGPT",
            "target_actor": "CHATGPT",
            "status": "ACKED",
            "updated_at": "2026-06-19T10:30:00Z",
            "executionPackage": {
                "status": "READY_FOR_CHATGPT_REVIEW",
                "targetActor": "CHATGPT",
                "allowedAction": "HANDOFF_REVIEW_ONLY",
                "handoffPayload": {"chatgptReviewPrompt": "review package"},
                "boundary": "Local review package only."
            }
        });
        let approval_required = json!({
            "handoff_id": "handoff:OPS-TEST-INBOX:approval",
            "dedupe_key": "agora-market:orders:approval",
            "title": "Approval required handoff",
            "target_actor": "CHATGPT",
            "status": "READY",
            "updated_at": "2026-06-19T10:29:00Z",
            "candidate": {
                "allowedAction": "HANDOFF_REVIEW_ONLY",
                "candidateType": "FULFILLMENT_FOLLOWUP"
            },
            "handoff": {
                "chatgptReviewPrompt": "needs approval",
                "codexTaskPayload": {"objective": "review"},
                "githubIssueDraft": {"title": "draft"}
            }
        });
        let legacy = json!({
            "handoff_id": "handoff:OPS-TEST-INBOX:legacy",
            "dedupe_key": "agora-market:orders:legacy",
            "title": "Legacy ack without package",
            "target_actor": "CHATGPT",
            "status": "ACKED",
            "updated_at": "2026-06-19T10:28:00Z"
        });
        write_handoff_queue_records(&[legacy, approval_required, reviewable])
            .expect("write inbox records");

        let inbox = call_a2a_handoff_review_inbox(json!({ "limit": 10 })).expect("inbox");
        assert_eq!(inbox["count"], 3);
        assert_eq!(inbox["readyForReview"], 1);
        assert_eq!(inbox["approvalRequired"], 1);
        assert_eq!(inbox["legacyAckMissingExecutionPackage"], 1);
        assert_eq!(inbox["items"][0]["status"], "READY_FOR_REVIEW");
        assert_eq!(
            inbox["items"][0]["executionPackage"]["status"],
            "READY_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(inbox["items"][1]["status"], "APPROVAL_REQUIRED");
        assert_eq!(
            inbox["items"][1]["approvalPacket"]["toolCallTemplate"]["arguments"]["status"],
            "ACKED"
        );
        assert_eq!(
            inbox["items"][2]["status"],
            "LEGACY_ACK_MISSING_EXECUTION_PACKAGE"
        );
        assert_eq!(inbox["items"][2]["hasOperatorReview"], false);

        let filtered = call_a2a_handoff_review_inbox(json!({
            "status": "READY_FOR_REVIEW",
            "actor": "CHATGPT"
        }))
        .expect("filtered inbox");
        assert_eq!(filtered["count"], 1);
        assert_eq!(
            filtered["items"][0]["handoffId"],
            "handoff:OPS-TEST-INBOX:reviewable"
        );

        if let Some(content) = original_handoff {
            if let Some(parent) = handoff_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&handoff_path, content);
        } else {
            let _ = std::fs::remove_file(&handoff_path);
        }
    }

    #[test]
    fn issue_candidate_persist_inherits_review_metadata_by_dedupe_key() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = issue_candidates_path();
        let handoff_path = handoff_queue_path();
        let original = std::fs::read_to_string(&path).ok();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&handoff_path);

        let candidate = json!({
            "status": "CANDIDATE",
            "candidateType": "INTEGRATION_BLOCKER",
            "severity": "MEDIUM",
            "dedupeKey": "agora-market:marketplace_ops:storehealthcheck:integration_blocker:abc",
            "title": "AgoraMarketAPI MCP ops tool contract blocks Sirin evidence collection: storeHealthCheck",
            "allowedAction": "CODEX_VERIFY_INTEGRATION",
            "suggestedNextActor": "CODEX",
            "suggestedAction": "verify",
            "opsEvent": {
                "targetProject": "AgoraMarketAPI",
                "allowedAction": "CODEX_VERIFY_INTEGRATION"
            },
            "handoff": {
                "chatgptReviewPrompt": "review",
                "codexTaskPayload": {"objective": "verify"},
                "githubIssueDraft": {"title": "draft"}
            }
        });

        persist_issue_candidates_for_task(
            "OPS-1",
            &json!({
                "task_type": AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE,
                "summary": "first run",
                "issue_candidates": [candidate.clone()],
            }),
        )
        .expect("persist first");
        call_a2a_review_issue_candidate(json!({
            "task_id": "OPS-1",
            "dedupeKey": "agora-market:marketplace_ops:storehealthcheck:integration_blocker:abc",
            "reviewStatus": "ACCEPTED",
            "reviewNote": "Tracked externally",
            "reviewedBy": "codex",
            "externalIssueUrl": "https://github.com/Redandan/AgoraMarketAPI/issues/560",
            "externalIssueProject": "AgoraMarketAPI",
            "externalIssueNumber": 560
        }))
        .expect("accept first");
        let first_handoffs =
            call_a2a_handoff_queue(json!({ "status": "ALL" })).expect("first handoff list");
        assert_eq!(first_handoffs["count"], 1);
        let first_handoff_id = first_handoffs["handoffs"][0]["handoff_id"]
            .as_str()
            .expect("handoff id")
            .to_string();
        call_a2a_update_handoff(json!({
            "handoffId": first_handoff_id,
            "status": "ACKED",
            "note": "already handed off"
        }))
        .expect("ack first handoff");

        persist_issue_candidates_for_task(
            "OPS-2",
            &json!({
                "task_type": AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE,
                "summary": "second run",
                "issue_candidates": [candidate],
            }),
        )
        .expect("persist second");

        let listed = call_a2a_issue_candidates(json!({
            "task_id": "OPS-2",
            "status": "ACCEPTED"
        }))
        .expect("list inherited accepted");
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["candidates"][0]["review_status"], "ACCEPTED");
        assert_eq!(
            listed["candidates"][0]["external_issue"]["url"],
            "https://github.com/Redandan/AgoraMarketAPI/issues/560"
        );
        let second_handoffs =
            call_a2a_handoff_queue(json!({ "status": "ALL" })).expect("second handoff list");
        assert_eq!(second_handoffs["count"], 1);
        assert_eq!(second_handoffs["handoffs"][0]["status"], "ACKED");
        assert_eq!(
            second_handoffs["handoffs"][0]["source_record_id"],
            "OPS-2:agora-market:marketplace_ops:storehealthcheck:integration_blocker:abc"
        );
        assert_eq!(
            second_handoffs["handoffs"][0]["dedupe_key"],
            "agora-market:marketplace_ops:storehealthcheck:integration_blocker:abc"
        );

        restore_test_file(&path, original);
        restore_test_file(&handoff_path, original_handoff);
    }

    #[test]
    fn current_report_candidates_with_accepted_external_review_become_tracked_events() {
        let candidate = json!({
            "status": "CANDIDATE",
            "candidateType": "INTEGRATION_BLOCKER",
            "severity": "MEDIUM",
            "dedupeKey": "agora-market:marketplace_ops:storehealthcheck:integration_blocker:abc",
            "title": "AgoraMarketAPI MCP ops tool contract blocks Sirin evidence collection: storeHealthCheck",
            "allowedAction": "CODEX_VERIFY_INTEGRATION",
            "suggestedNextActor": "CODEX",
            "suggestedAction": "verify",
            "opsEvent": {
                "eventType": "mcp_integration",
                "targetProject": "AgoraMarketAPI",
                "allowedAction": "CODEX_VERIFY_INTEGRATION"
            }
        });
        let persisted = vec![json!({
            "record_id": "OPS-old:agora-market:marketplace_ops:storehealthcheck:integration_blocker:abc",
            "dedupe_key": "agora-market:marketplace_ops:storehealthcheck:integration_blocker:abc",
            "candidate": candidate.clone(),
            "review_status": "ACCEPTED",
            "review_note": "Tracked externally",
            "reviewed_by": "codex",
            "external_issue": {
                "url": "https://github.com/Redandan/AgoraMarketAPI/issues/560",
                "project": "AgoraMarketAPI",
                "number": 560
            }
        })];

        let (report_candidates, tracked_records) =
            split_current_candidates_with_tracked_reviews("OPS-new", &[candidate], &persisted);
        assert!(report_candidates.is_empty());
        assert_eq!(tracked_records.len(), 1);
        assert_eq!(tracked_records[0]["review_status"], "ACCEPTED");
        assert_eq!(tracked_records[0]["external_issue"]["number"], 560);

        let report = build_ops_report(
            &json!({ "sections": [] }),
            &report_candidates,
            None,
            &tracked_records,
        );
        assert_eq!(
            report["trackedExternalIssues"][0]["externalIssue"]["number"],
            560
        );
        assert!(report["nextActions"]
            .as_array()
            .expect("next actions")
            .iter()
            .any(|action| action["source"] == "tracked_external_issue"));
    }

    #[test]
    fn healthy_current_test_health_suppresses_stale_test_health_inbox_in_report() {
        let stale_record = json!({
            "record_id": "old-test-health",
            "task_id": "OPS-old",
            "candidate": test_health_candidate(
                "TEST_REGRESSION",
                "HIGH",
                0.8,
                "Investigate 33 failing AgoraMarket active-smoke test(s)".into(),
                "script_health",
                "script_health_failed_tests",
                "{\"failed\":33}",
                "CODEX",
                "verify_failed_tests",
            ),
            "review_status": "CONVERT_TO_ISSUE",
            "created_at": "2026-06-14T00:00:00Z",
            "updated_at": "2026-06-14T00:00:00Z"
        });
        assert!(is_local_test_health_inbox_record(&stale_record));
        let healthy = json!({
            "summary": "Sirin local test health scout found no candidate requiring review.",
            "candidate_count": 0,
            "issue_candidates": [],
        });
        assert!(local_test_health_has_no_current_candidates(Some(&healthy)));

        let mut inbox_records = vec![stale_record];
        if local_test_health_has_no_current_candidates(Some(&healthy)) {
            inbox_records.retain(|record| !is_local_test_health_inbox_record(record));
        }
        let report = build_ops_report(
            &json!({ "sections": [] }),
            &[],
            Some(&healthy),
            &inbox_records,
        );
        assert_eq!(report["opsEventInbox"]["count"], 0);
        assert!(!report["nextActions"]
            .as_array()
            .expect("next actions")
            .iter()
            .any(|action| action["recordId"] == "old-test-health"));
    }

    #[test]
    fn ops_event_inbox_filters_by_event_gate_fields() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = issue_candidates_path();
        let original = std::fs::read_to_string(&path).ok();
        let _ = std::fs::remove_file(&path);

        let product_event = issue_candidate(
            "OPS_RISK",
            "HIGH",
            0.78,
            "Review AgoraMarket dispute backlog".into(),
            "getDisputeBacklog",
            "mcp_output",
            "⚠️ 待處理爭議 backlog: 3",
            "CHATGPT",
            "verify",
        );
        let sirin_event = test_health_candidate(
            "COVERAGE_GAP",
            "LOW",
            0.58,
            "Run or classify untested AgoraMarket check agora_webrtc_voice_call".into(),
            "script_health",
            "per_test_replay_status",
            r#"{"test_id":"agora_webrtc_voice_call","status":"untested"}"#,
            "CODEX",
            "verify_test_relevance",
        );

        persist_issue_candidates_for_task(
            "ait-ops-events",
            &json!({
                "task_type": AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE,
                "summary": "ops event test",
                "issue_candidates": [product_event, sirin_event],
            }),
        )
        .expect("persist ops events");

        let product_only = call_ops_event_inbox(json!({
            "eventType": "trust_support",
            "allowedAction": "OPEN_PRODUCT_ISSUE_AFTER_REVIEW",
            "targetProject": "AgoraMarket",
            "limit": 10
        }))
        .expect("filter product events");
        assert_eq!(product_only["count"], 1);
        assert_eq!(
            product_only["events"][0]["candidate"]["opsEvent"]["eventType"],
            "trust_support"
        );

        let sirin_only = call_ops_event_inbox(json!({
            "allowedAction": "SIRIN_BACKLOG_ONLY",
            "targetProject": "Sirin",
            "limit": 10
        }))
        .expect("filter sirin backlog events");
        assert_eq!(sirin_only["count"], 1);
        assert_eq!(
            sirin_only["events"][0]["candidate"]["allowedAction"],
            "SIRIN_BACKLOG_ONLY"
        );
        assert!(sirin_only["boundary"]
            .as_str()
            .expect("boundary")
            .contains("ops event inbox"));

        if let Some(content) = original {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, content);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn ops_event_inbox_normalizes_legacy_records_without_ops_event() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = issue_candidates_path();
        let original = std::fs::read_to_string(&path).ok();
        let _ = std::fs::remove_file(&path);

        persist_issue_candidates_for_task(
            "ait-legacy-health",
            &json!({
                "task_type": SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE,
                "summary": "legacy test-health candidate",
                "issue_candidates": [{
                    "status": "CANDIDATE",
                    "candidateType": "REPLAY_STALE",
                    "severity": "MEDIUM",
                    "dedupeKey": "agora-market:test_automation:script_health:replay_stale:legacy",
                    "affectedSurface": "test_automation",
                    "suggestedNextActor": "CODEX",
                    "suggestedAction": "rerun_and_rerecord",
                    "evidence": [{
                        "type": "per_test_replay_status",
                        "tool": "script_health",
                        "excerpt": "{\"test_id\":\"agora_buyer_wallet\"}"
                    }]
                }]
            }),
        )
        .expect("persist legacy record");

        let result = call_ops_event_inbox(json!({
            "allowedAction": "SIRIN_BACKLOG_ONLY",
            "targetProject": "Sirin",
            "limit": 10
        }))
        .expect("legacy records should be normalized on read");
        assert_eq!(result["count"], 1);
        assert_eq!(
            result["events"][0]["candidate"]["opsEvent"]["eventType"],
            "test_replay_health"
        );
        assert_eq!(
            result["events"][0]["candidate"]["allowedAction"],
            "SIRIN_BACKLOG_ONLY"
        );

        if let Some(content) = original {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, content);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn review_gate_blocks_product_issue_conversion_for_sirin_backlog_events() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = issue_candidates_path();
        let handoff_path = handoff_queue_path();
        let original = std::fs::read_to_string(&path).ok();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&handoff_path);

        let sirin_event = test_health_candidate(
            "COVERAGE_GAP",
            "LOW",
            0.58,
            "Run or classify untested AgoraMarket check agora_webrtc_voice_call".into(),
            "script_health",
            "per_test_replay_status",
            r#"{"test_id":"agora_webrtc_voice_call","status":"untested"}"#,
            "CODEX",
            "verify_test_relevance",
        );
        let dedupe_key = sirin_event["dedupeKey"]
            .as_str()
            .expect("dedupe key")
            .to_string();
        persist_issue_candidates_for_task(
            "ait-review-gate",
            &json!({
                "task_type": SIRIN_AGORA_TEST_HEALTH_SCOUT_TASK_TYPE,
                "summary": "review gate test",
                "issue_candidates": [sirin_event],
            }),
        )
        .expect("persist sirin event");

        let blocked = call_a2a_review_issue_candidate(json!({
            "task_id": "ait-review-gate",
            "dedupeKey": dedupe_key.clone(),
            "reviewStatus": "CONVERT_TO_ISSUE",
        }))
        .expect_err("Sirin backlog events must not convert to product issue");
        assert!(blocked.contains("allowedAction=SIRIN_BACKLOG_ONLY"));

        let accepted = call_a2a_review_issue_candidate(json!({
            "task_id": "ait-review-gate",
            "dedupeKey": dedupe_key,
            "reviewStatus": "ACCEPTED",
        }))
        .expect("accepted should still queue Sirin handoff/backlog");
        assert_eq!(accepted["candidate"]["review_status"], "ACCEPTED");
        assert_eq!(
            accepted["queued_handoff"]["candidate"]["allowedAction"],
            "SIRIN_BACKLOG_ONLY"
        );

        restore_test_file(&path, original);
        restore_test_file(&handoff_path, original_handoff);
    }

    #[test]
    fn review_gate_allows_product_issue_conversion_for_product_ops_events() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = issue_candidates_path();
        let handoff_path = handoff_queue_path();
        let original = std::fs::read_to_string(&path).ok();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&handoff_path);

        let product_event = issue_candidate(
            "OPS_RISK",
            "HIGH",
            0.78,
            "Review AgoraMarket dispute backlog".into(),
            "getDisputeBacklog",
            "mcp_output",
            "⚠️ 待處理爭議 backlog: 3",
            "CHATGPT",
            "verify",
        );
        let dedupe_key = product_event["dedupeKey"]
            .as_str()
            .expect("dedupe key")
            .to_string();
        persist_issue_candidates_for_task(
            "ait-review-product",
            &json!({
                "task_type": AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE,
                "summary": "review product gate test",
                "issue_candidates": [product_event],
            }),
        )
        .expect("persist product event");

        let reviewed = call_a2a_review_issue_candidate(json!({
            "task_id": "ait-review-product",
            "dedupeKey": dedupe_key,
            "reviewStatus": "CONVERT_TO_ISSUE",
        }))
        .expect("product ops events may convert after review");
        assert_eq!(reviewed["candidate"]["review_status"], "CONVERT_TO_ISSUE");
        assert_eq!(
            reviewed["queued_handoff"]["candidate"]["allowedAction"],
            "OPEN_PRODUCT_ISSUE_AFTER_REVIEW"
        );

        restore_test_file(&path, original);
        restore_test_file(&handoff_path, original_handoff);
    }

    #[tokio::test]
    async fn issue_scout_stops_at_pending_mcp_batch_auth() {
        let mock = start_mock_agora_mcp_with_auth(false).await;
        let cfg = A2aWorkerConfig {
            enabled: true,
            worker_id: "sirin-test-worker".into(),
            server_url: mock.url.clone(),
            bearer: None,
            agora_ops_authorization: None,
            bearer_env: "KB_MCP_BEARER".into(),
            poll_interval_seconds: 60,
            assignee_type: "SIRIN".into(),
            claim_tool: DEFAULT_CLAIM_TOOL.into(),
            update_tool: DEFAULT_UPDATE_TOOL.into(),
        };
        let task = A2aTask {
            id: "ait-auth-pending".into(),
            task_type: AGORA_MARKET_ISSUE_SCOUT_TASK_TYPE.into(),
            params: json!({
                "days": 7,
                "hoursOverdue": 12,
                "maxCandidates": 10
            }),
        };

        let result = run_agora_market_issue_scout(&cfg, &task)
            .await
            .expect("scout result");
        let candidates = result["issue_candidates"]
            .as_array()
            .expect("candidate array");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0]["candidateType"], "INTEGRATION_BLOCKER");
        assert_eq!(candidates[0]["affectedSurface"], "unknown");
        assert_eq!(candidates[0]["opsEvent"]["source"], "sirin_ops_integration");
        assert_eq!(candidates[0]["opsEvent"]["targetProject"], "Sirin");
        assert_eq!(candidates[0]["allowedAction"], "CODEX_VERIFY_INTEGRATION");
        assert_eq!(result["ops_report"]["reportStatus"], "BLOCKED_INTEGRATION");
        assert_eq!(result["ops_report"]["marketplaceEvidenceReady"], false);
        assert_eq!(result["ops_report"]["localTestHealthReady"], true);
        assert!(result.get("source_test_health").is_some());
        assert!(result["ops_report"]["summary"]
            .as_str()
            .expect("report summary")
            .contains("not ready for marketplace conclusions"));
        assert_eq!(
            result["ops_report"]["nextActions"][0]["allowedAction"],
            "CODEX_VERIFY_INTEGRATION"
        );
        assert_eq!(result["ops_report"]["nextActions"][0]["actor"], "CODEX");
        assert_eq!(result["ops_report"]["nextActions"][0]["source"], "auth");
        assert_eq!(
            result["ops_report"]["nextActions"][0]["authNextAction"],
            "configure_ops_authorization"
        );
        let product_category = result["ops_report"]["categories"]
            .as_array()
            .expect("report categories")
            .iter()
            .find(|category| category["category"] == "product")
            .expect("product category");
        assert_eq!(
            product_category["status"],
            "blocked_pending_marketplace_evidence"
        );
        assert_eq!(
            product_category["coverageGap"]["allowedAction"],
            "CODEX_VERIFY_INTEGRATION"
        );
        assert_eq!(product_category["coverageGap"]["nextActor"], "CODEX");
        assert!(product_category["coverageGap"]["nextAction"]
            .as_str()
            .expect("coverage next action")
            .contains("Resolve Sirin OPS MCP authorization/integration"));
        assert_eq!(
            result["ops_report"]["nextActions"][1]["allowedAction"],
            "CODEX_VERIFY_INTEGRATION"
        );
        assert_eq!(
            product_category["coverageGap"]["evidenceLevel"],
            "needs_verification"
        );
        assert!(result["ops_report"]["coverageGaps"]
            .as_array()
            .expect("coverage gaps")
            .iter()
            .any(|gap| gap["category"] == "product"
                && gap["allowedAction"] == "CODEX_VERIFY_INTEGRATION"));
        assert!(result["ops_report"]["nextActions"]
            .as_array()
            .expect("next actions")
            .iter()
            .any(|action| action["source"] == "coverageGaps"
                && action["categories"]
                    .as_array()
                    .expect("coverage action categories")
                    .iter()
                    .any(|category| category == "product")));
        assert_eq!(
            candidates[0]["opsEvent"]["productIssueGate"]["mayOpenProductIssue"],
            false
        );
        assert_eq!(
            result["source_brief"]["sections"]
                .as_array()
                .expect("sections")
                .len(),
            1
        );
        assert_eq!(
            result["source_brief"]["sections"][0]["tool"],
            "opsAuthorization"
        );
    }

    fn restore_test_file(path: &PathBuf, original: Option<String>) {
        if let Some(content) = original {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, content);
        } else {
            let _ = std::fs::remove_file(path);
        }
    }

    fn issue_candidate_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[tokio::test]
    async fn issue_scout_worker_e2e_dry_run_claims_executes_and_reports_artifacts() {
        let _guard = issue_candidate_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = issue_candidates_path();
        let handoff_path = handoff_queue_path();
        let original = std::fs::read_to_string(&path).ok();
        let original_handoff = std::fs::read_to_string(&handoff_path).ok();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&handoff_path);

        let mock = start_mock_agora_mcp().await;
        let cfg = A2aWorkerConfig {
            enabled: true,
            worker_id: "sirin-test-worker".into(),
            server_url: mock.url.clone(),
            bearer: None,
            agora_ops_authorization: Some("test-ops-token".into()),
            bearer_env: "KB_MCP_BEARER".into(),
            poll_interval_seconds: 60,
            assignee_type: "SIRIN".into(),
            claim_tool: DEFAULT_CLAIM_TOOL.into(),
            update_tool: DEFAULT_UPDATE_TOOL.into(),
        };

        run_one_poll(&cfg).await;

        let updates = mock
            .updates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let ops_authorizations = mock
            .ops_authorizations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert!(
            ops_authorizations
                .iter()
                .any(|value| value == "test-ops-token"),
            "Agora ops scout must call protected ops tools with X-OPS-Authorization"
        );
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0]["status"], "RUNNING");
        assert_eq!(updates[1]["status"], "NEEDS_REVIEW");
        assert!(updates[1]["summary"]
            .as_str()
            .expect("summary")
            .contains("AgoraMarket issue scout found"));

        let artifacts: Value =
            serde_json::from_str(updates[1]["artifactsJson"].as_str().expect("artifactsJson"))
                .expect("valid artifacts JSON");
        let names: Vec<_> = artifacts
            .as_array()
            .expect("artifact array")
            .iter()
            .filter_map(|item| item.get("metadata")?.get("name")?.as_str())
            .collect();
        assert!(names.contains(&"agora_market_issue_candidates"));

        let candidates_artifact = artifacts
            .as_array()
            .expect("artifact array")
            .iter()
            .find(|item| item["metadata"]["name"] == "agora_market_issue_candidates")
            .expect("candidate artifact");
        let candidates: Value = serde_json::from_str(
            candidates_artifact["uriOrValue"]
                .as_str()
                .expect("candidate payload"),
        )
        .expect("valid candidates JSON");
        let first = candidates
            .as_array()
            .expect("candidate array")
            .first()
            .expect("at least one candidate");
        assert!(first.get("dedupeKey").is_some());
        assert!(first["handoff"]["chatgptReviewPrompt"]
            .as_str()
            .expect("chatgpt handoff")
            .contains("Review this AgoraMarket ops event"));
        assert!(first["handoff"]["codexTaskPayload"]["requiredBoundary"]
            .as_str()
            .expect("codex boundary")
            .contains("read-only verification"));
        assert!(first["handoff"]["githubIssueDraft"]["body"]
            .as_str()
            .expect("issue draft")
            .contains("READ_ONLY evidence only"));

        if let Some(content) = original {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, content);
        } else {
            let _ = std::fs::remove_file(&path);
        }
        if let Some(content) = original_handoff {
            if let Some(parent) = handoff_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&handoff_path, content);
        } else {
            let _ = std::fs::remove_file(&handoff_path);
        }
    }

    #[test]
    fn artifacts_match_server_contract() {
        let artifacts = artifacts_from_result(&json!({
            "run_id": "rsr-1",
            "inbox_id": "rsn-1",
            "report_path": "C:/tmp/report.html",
            "summary": "done",
            "deep_research": {
                "enabled": true,
                "status": "completed",
                "issue_draft": "Open an issue",
                "kb_draft": "Write KB"
            }
        }));
        let first = artifacts
            .as_array()
            .expect("array")
            .first()
            .expect("artifact");
        assert!(first.get("artifactType").is_some());
        assert!(first.get("uriOrValue").is_some());
        assert!(first.get("metadata").is_some());
        let names: Vec<_> = artifacts
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|item| item.get("metadata")?.get("name")?.as_str())
            .collect();
        assert!(names.contains(&"deep_research_result"));
        assert!(names.contains(&"deep_research_issue_draft"));
        assert!(names.contains(&"deep_research_kb_draft"));
    }

    #[derive(Clone)]
    struct MockAgoraMcp {
        url: String,
        updates: Arc<Mutex<Vec<Value>>>,
        ops_authorizations: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Clone, Default)]
    struct MockAgoraMcpState {
        updates: Arc<Mutex<Vec<Value>>>,
        ops_authorizations: Arc<Mutex<Vec<String>>>,
        auth_covered: bool,
    }

    async fn start_mock_agora_mcp() -> MockAgoraMcp {
        start_mock_agora_mcp_with_auth(false).await
    }

    async fn start_mock_agora_mcp_with_auth(auth_covered: bool) -> MockAgoraMcp {
        let state = MockAgoraMcpState::default();
        let state = MockAgoraMcpState {
            auth_covered,
            ..state
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = Router::new()
            .route("/mcp", post(mock_mcp_handler))
            .with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        MockAgoraMcp {
            url: format!("http://{addr}/mcp"),
            updates: state.updates,
            ops_authorizations: state.ops_authorizations,
        }
    }

    async fn mock_mcp_handler(
        State(state): State<MockAgoraMcpState>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let ops_authorization = headers
            .get("X-OPS-Authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if let Some(value) = ops_authorization.as_ref() {
            state
                .ops_authorizations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(value.clone());
        }
        let ops_auth_covered = state.auth_covered || ops_authorization.is_some();
        let tool = body
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = body
            .pointer("/params/arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let text = match tool {
            "claimAiTask" => json!({
                "claimed": true,
                "task": {
                    "taskId": "ait-99",
                    "taskType": "AGORA_MARKET_ISSUE_SCOUT",
                    "params": {
                        "days": 7,
                        "hoursOverdue": 12,
                        "maxCandidates": 8
                    }
                }
            })
            .to_string(),
            "updateAiTaskStatus" => {
                state
                    .updates
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(args);
                "{\"ok\":true}".into()
            }
            "getMcpAuthProbe" => json!({
                "toolName": "storeHealthCheck",
                "approvalMode": "SESSION_BATCH",
                "requestedToolsCovered": ops_auth_covered,
                "requestedToolsStatus": args
                    .get("requestedTools")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|tool| json!({
                        "toolName": tool,
                        "covered": ops_auth_covered
                    }))
                    .collect::<Vec<_>>(),
                "status": if ops_auth_covered { "APPROVED" } else { "PENDING" },
                "nextAction": if ops_auth_covered { "retry_requested_tools" } else { "wait_for_tg_approval" },
                "grantRequestId": if ops_auth_covered { Value::Null } else { json!("mcp_test_grant") },
                "boundary": "READ_ONLY",
            })
            .to_string(),
            "storeHealthCheck" => "Store health normal. GMV stable.".into(),
            "getOpsBacklog" => "待處理 backlog: 商品審核 2, 訂單 1".into(),
            "listPendingOrders" => "pending orders: 1".into(),
            "getDisputeBacklog" => "⚠️ 待處理爭議 backlog: 3".into(),
            "getOrderFlowFunnel" => "Order funnel normal.".into(),
            "getGroupAttributionReport" => "TG attribution report: no alert.".into(),
            "getBotKnowledgeGaps" => "PENDING knowledge gaps: 4".into(),
            "getKnowledgeBaseStats" => "KB stats normal.".into(),
            "listPendingFulfillmentOrders" => "overdue fulfillment orders: 2".into(),
            "listSuppliers" => "supplier pool normal.".into(),
            other => format!("unknown mock tool: {other}"),
        };
        Json(json!({
            "jsonrpc": "2.0",
            "id": body.get("id").cloned().unwrap_or(json!(1)),
            "result": {
                "content": [{ "type": "text", "text": text }]
            }
        }))
    }
}
