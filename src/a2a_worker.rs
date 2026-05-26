//! Server-side A2A task worker.
//!
//! Sirin stays a runner: it polls an external MCP task service, claims work it
//! can execute locally, runs the matching Sirin capability, then reports the
//! outcome and artifacts back to the server.  Task ownership, retries, review
//! state, and cross-agent orchestration remain on the server side.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const CONFIG_FILE: &str = "a2a_worker.yaml";
const DEFAULT_CLAIM_TOOL: &str = "claimAiTask";
const DEFAULT_UPDATE_TOOL: &str = "updateAiTaskStatus";
const DEFAULT_TASK_TYPE: &str = "SIRIN_RESEARCH_SENTINEL_RUN_ONCE";

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
        .or_else(|| std::env::var(&bearer_env).ok().filter(|v| !v.trim().is_empty()));
    let server_url = std::env::var("SIRIN_A2A_SERVER_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| if file.server_url.trim().is_empty() { None } else { Some(file.server_url) })
        .or_else(|| std::env::var("KB_MCP_URL").ok().filter(|v| !v.trim().is_empty()))
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
        bearer_env,
        poll_interval_seconds,
        assignee_type: file.assignee_type,
        claim_tool: file.claim_tool,
        update_tool: file.update_tool,
    }
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
    status.capabilities = vec![DEFAULT_TASK_TYPE.into()];
    status
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
        status.capabilities = vec![DEFAULT_TASK_TYPE.into()];
        if !cfg.enabled {
            status.last_status = Some("disabled".into());
        } else if cfg.server_url.trim().is_empty() {
            status.last_status = Some("not_configured".into());
            status.last_error = Some("SIRIN_A2A_SERVER_URL or a2a_worker.yaml server_url is required".into());
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

    let outcome = execute_task(&task).await;
    match outcome {
        Ok(result) => {
            let artifacts = artifacts_from_result(&result);
            let summary = result
                .get("summary")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(240).collect::<String>());
            let report = report_task_status(cfg, &task.id, "NEEDS_REVIEW", None, artifacts, result).await;
            update_status(|status| {
                status.last_status = Some(if report.is_ok() { "reported" } else { "report_failed" }.into());
                status.last_error = report.err();
                status.last_result_summary = summary;
            });
        }
        Err(e) => {
            let _ = report_task_status(cfg, &task.id, "FAILED", Some(e.clone()), json!([]), json!(null)).await;
            update_status(|status| {
                status.last_status = Some("failed".into());
                status.last_error = Some(e);
            });
        }
    }
}

async fn execute_task(task: &A2aTask) -> Result<Value, String> {
    match task.task_type.as_str() {
        DEFAULT_TASK_TYPE => {
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
        other => Err(format!("Unsupported A2A task_type: {other}")),
    }
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
        ("NEEDS_REVIEW", _) => "Sirin completed local research; Codex/server review is required.".into(),
        ("FAILED", Some(e)) => format!("Sirin task failed: {e}"),
        _ => status.into(),
    }
}

fn artifacts_from_result(result: &Value) -> Value {
    let mut artifacts = Vec::new();
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
    artifacts.push(json!({
        "artifactType": "JSON_PAYLOAD",
        "uriOrValue": result.to_string(),
        "metadata": { "name": "research_sentinel_result" },
    }));
    json!(artifacts)
}

fn normalize_research_params(args: &mut Value) {
    let Some(obj) = args.as_object_mut() else { return };
    for (camel, snake) in [
        ("lookbackHours", "lookback_hours"),
        ("maxResults", "max_results"),
        ("createInbox", "create_inbox"),
        ("createReport", "create_report"),
        ("publishToKb", "publish_to_kb"),
        ("rssFeeds", "rss_feeds"),
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
    Ok(Some(A2aTask { id, task_type, params }))
}

pub(crate) fn call_a2a_worker_status(_args: Value) -> Result<Value, String> {
    Ok(json!({
        "config_file": crate::platform::config_path(CONFIG_FILE),
        "status": current_status_with_config(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_task_from_mcp_text_result() {
        let value = json!({
            "result": "{\"task\":{\"taskId\":\"t-1\",\"taskType\":\"SIRIN_RESEARCH_SENTINEL_RUN_ONCE\",\"params\":{\"topic\":\"BTC\"}}}"
        });
        let task = extract_task(&value).expect("valid payload").expect("task");
        assert_eq!(task.id, "t-1");
        assert_eq!(task.task_type, DEFAULT_TASK_TYPE);
        assert_eq!(task.params["topic"], "BTC");
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
        assert!(extract_task(&json!({"task": null})).expect("valid").is_none());
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
            "createReport": "auto"
        });
        normalize_research_params(&mut args);
        assert_eq!(args["lookback_hours"], 6);
        assert_eq!(args["max_results"], 3);
        assert_eq!(args["publish_to_kb"], false);
        assert_eq!(args["create_report"], "auto");
    }

    #[test]
    fn artifacts_match_server_contract() {
        let artifacts = artifacts_from_result(&json!({
            "run_id": "rsr-1",
            "inbox_id": "rsn-1",
            "report_path": "C:/tmp/report.html",
            "summary": "done"
        }));
        let first = artifacts.as_array().expect("array").first().expect("artifact");
        assert!(first.get("artifactType").is_some());
        assert!(first.get("uriOrValue").is_some());
        assert!(first.get("metadata").is_some());
    }
}
