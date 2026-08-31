use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditProfile {
    id: String,
    target: String,
    #[serde(default = "default_timeout_ms")]
    default_timeout_ms: u64,
    surfaces: Vec<AuditSurface>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditSurface {
    id: String,
    role: String,
    url: String,
    #[serde(default)]
    nav_target: Option<String>,
    viewport: AuditViewport,
    #[serde(default)]
    expected_any: Vec<String>,
    #[serde(default)]
    forbidden_any: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditViewport {
    width: u64,
    height: u64,
    #[serde(default)]
    mobile: bool,
}

struct AuditSurfaceRun {
    result: Value,
    issues: Vec<Value>,
}

fn default_timeout_ms() -> u64 {
    15_000
}

fn load_profile(profile_id: &str) -> Result<(AuditProfile, String), String> {
    let rel = format!("audits/{profile_id}.yaml");
    let candidates = [
        crate::platform::config_path(&rel),
        std::env::current_dir()
            .map_err(|e| format!("cwd: {e}"))?
            .join("config")
            .join(&rel),
    ];

    for path in candidates {
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("read audit profile {}: {e}", path.display()))?;
            let profile = serde_yaml::from_str::<AuditProfile>(&text)
                .map_err(|e| format!("parse audit profile {}: {e}", path.display()))?;
            return Ok((profile, path.to_string_lossy().into_owned()));
        }
    }

    if profile_id == "agoramarket" {
        let text = include_str!("../config/audits/agoramarket.yaml");
        let profile = serde_yaml::from_str::<AuditProfile>(text)
            .map_err(|e| format!("parse embedded agoramarket audit profile: {e}"))?;
        return Ok((
            profile,
            "embedded:config/audits/agoramarket.yaml".to_string(),
        ));
    }

    Err(format!("audit profile {profile_id:?} not found"))
}

async fn browser_action(args: Value) -> (Option<Value>, Option<String>) {
    match crate::mcp_server::call_browser_exec(args, "sirin-audit-engine").await {
        Ok(value) => (Some(value), None),
        Err(err) => (None, Some(err)),
    }
}

async fn browser_action_in_session(
    session_id: &str,
    mut args: Value,
) -> (Option<Value>, Option<String>) {
    if let Some(obj) = args.as_object_mut() {
        obj.insert("session_id".to_string(), json!(session_id));
    }
    browser_action(args).await
}

fn text_contains_any(haystack: &str, needles: &[String]) -> Vec<String> {
    needles
        .iter()
        .filter(|needle| haystack.contains(needle.as_str()))
        .cloned()
        .collect()
}

fn console_error_items(console: &Value) -> Vec<Value> {
    console["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| {
                    let compact = message.to_string().to_lowercase();
                    compact.contains("\"error\"")
                        || compact.contains("uncaught")
                        || compact.contains("exception")
                        || compact.contains("failed to load")
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn issue_origin(rule: &str) -> &'static str {
    match rule {
        "browser_viewport_error"
        | "browser_url_error"
        | "browser_title_error"
        | "shadow_dump_error"
        | "console_capture_error"
        | "flutter_semantics_missing"
        | "blank_or_no_shadow_dom" => "sirin_or_browser_bootstrap",
        "navigation_target_missing" | "missing_core_surface_text" => "needs_review",
        "unexpected_login_route"
        | "route_role_mismatch"
        | "console_error"
        | "wrong_role_or_surface_content" => "target_app_or_fixture",
        _ => "unknown",
    }
}

fn issue(
    surface_id: &str,
    rule: &str,
    severity: &str,
    confidence: &str,
    evidence: impl Into<String>,
    recommended_action: &str,
) -> Value {
    json!({
        "surface_id": surface_id,
        "type": rule,
        "rule": rule,
        "origin": issue_origin(rule),
        "severity": severity,
        "confidence": confidence,
        "final_url": null,
        "evidence": evidence.into(),
        "recommended_action": recommended_action,
        "screenshot_hint": "rerun agora_explore_audit with include_screenshots=true for this surface",
    })
}

fn enrich_issues(issues: &mut [Value], final_url: &str) {
    for item in issues {
        if let Some(obj) = item.as_object_mut() {
            obj.insert("final_url".to_string(), json!(final_url));
        }
    }
}

fn route_role_mismatch(role: &str, final_url: &str) -> Option<String> {
    let lower_url = final_url.to_lowercase();
    match role {
        "buyer" if lower_url.contains("#/seller") || lower_url.contains("#/admin") => {
            Some(format!("buyer role landed on non-buyer route: {final_url}"))
        }
        "seller" if lower_url.contains("#/admin") || lower_url.contains("#/buyer") => {
            Some(format!("seller role landed on wrong route: {final_url}"))
        }
        "admin" if lower_url.contains("#/seller") || lower_url.contains("#/buyer") => {
            Some(format!("admin role landed on wrong route: {final_url}"))
        }
        _ => None,
    }
}

fn audit_run_id(profile_id: &str) -> String {
    let now = chrono::Local::now();
    format!("audit_{}_{}", profile_id, now.format("%Y%m%d_%H%M%S_%3f"))
}

fn audit_run_dir(profile_id: &str, run_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("tracking")
        .join("audits")
        .join(profile_id)
        .join(run_id)
}

fn audit_profile_dir(profile_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("tracking")
        .join("audits")
        .join(profile_id)
}

fn audit_result_path(profile_id: &str, run_id: &str) -> PathBuf {
    audit_run_dir(profile_id, run_id).join("result.json")
}

fn handoff_profile_dir(profile_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("tracking")
        .join("handoffs")
        .join(profile_id)
}

fn handoff_record_path(profile_id: &str, handoff_id: &str) -> PathBuf {
    handoff_profile_dir(profile_id).join(format!("{handoff_id}.json"))
}

fn persist_audit_result(profile_id: &str, run_id: &str, result: &Value) -> Result<String, String> {
    let dir = audit_run_dir(profile_id, run_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create audit evidence dir {}: {e}", dir.display()))?;
    let path = audit_result_path(profile_id, run_id);
    let text =
        serde_json::to_string_pretty(result).map_err(|e| format!("serialize audit result: {e}"))?;
    std::fs::write(&path, text)
        .map_err(|e| format!("write audit evidence {}: {e}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}

fn read_audit_result(profile_id: &str, run_id: &str) -> Result<(Value, String), String> {
    let path = audit_result_path(profile_id, run_id);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("read audit evidence {}: {e}", path.display()))?;
    let mut value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("parse audit evidence {}: {e}", path.display()))?;
    if value.get("evidence_path").is_none() {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("evidence_path".to_string(), json!(path.to_string_lossy()));
        }
    }
    Ok((value, path.to_string_lossy().into_owned()))
}

fn summarize_audit_result(result: &Value, evidence_path: impl Into<String>) -> Value {
    let issue_candidates = result["issue_candidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let issue_summary: Vec<Value> = issue_candidates
        .iter()
        .map(|item| {
            json!({
                "surface_id": item["surface_id"],
                "rule": item["rule"],
                "origin": item["origin"],
                "severity": item["severity"],
                "confidence": item["confidence"],
                "final_url": item["final_url"],
                "recommended_action": item["recommended_action"],
                "evidence": item["evidence"],
            })
        })
        .collect();

    json!({
        "run_id": result["run_id"],
        "profile_id": result["profile_id"],
        "target": result["target"],
        "status": result["status"],
        "completed_surfaces": result["completed_surfaces"],
        "issue_count": result["issue_count"],
        "critical_count": result["critical_count"],
        "recommended_action": result["recommended_action"],
        "acceptance_summary": result["acceptance_summary"],
        "codex_handoff": result["codex_handoff"],
        "config_source": result["config_source"],
        "evidence_path": evidence_path.into(),
        "issue_summary": issue_summary,
    })
}

fn suggested_codex_action(rule: &str) -> &'static str {
    match rule {
        "route_role_mismatch" => {
            "inspect AgoraMarket role bootstrap, route guard, and test fixture role query handling"
        }
        "unexpected_login_route" => "inspect AgoraMarket auth bootstrap and fixture login bypass",
        "console_error" => {
            "inspect AgoraMarket browser console stack and failing frontend code path"
        }
        "wrong_role_or_surface_content" => {
            "inspect AgoraMarket role-specific navigation and content gating"
        }
        _ => "inspect AgoraMarket target app behavior using the linked Sirin evidence",
    }
}

fn codex_handoff(result: &Value, evidence_path: impl Into<String>) -> Value {
    let evidence_path = evidence_path.into();
    let target_candidates = result["acceptance_summary"]["target_app_candidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let issues: Vec<Value> = target_candidates
        .iter()
        .map(|item| {
            let rule = item["rule"].as_str().unwrap_or("unknown");
            json!({
                "surface_id": item["surface_id"],
                "rule": item["rule"],
                "origin": item["origin"],
                "severity": item["severity"],
                "confidence": item["confidence"],
                "final_url": item["final_url"],
                "evidence": item["evidence"],
                "evidence_path": evidence_path.clone(),
                "original_recommended_action": item["recommended_action"],
                "suggested_codex_action": suggested_codex_action(rule),
            })
        })
        .collect();
    let safe_to_escalate = result["acceptance_summary"]["safe_to_escalate_to_codex"]
        .as_bool()
        .unwrap_or(false);
    let recommended_next_action = if safe_to_escalate {
        "codex_triage_target_app_candidates"
    } else if result["acceptance_summary"]["needs_rerun_count"]
        .as_u64()
        .unwrap_or(0)
        > 0
    {
        "do_not_escalate_until_sirin_flakes_are_rechecked"
    } else {
        "no_codex_handoff_needed"
    };

    json!({
        "handoff_type": "codex_target_app_triage",
        "target": result["target"],
        "target_repo": "AgoraMarket",
        "repo_hint": "C:\\Users\\Redan\\IdeaProjects\\AgoraMarket",
        "generated_from_tool": result["tool"],
        "generated_from_run_id": result["run_id"],
        "safe_to_escalate": safe_to_escalate,
        "issue_count": issues.len(),
        "evidence_path": evidence_path,
        "issues": issues,
        "recommended_next_action": recommended_next_action,
    })
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

fn handoff_fingerprint(target_repo: &str, issue: &Value) -> String {
    let surface_id = issue["surface_id"].as_str().unwrap_or("");
    let rule = issue["rule"].as_str().unwrap_or("");
    let final_url = issue["final_url"].as_str().unwrap_or("");
    let raw = format!("{target_repo}|{surface_id}|{rule}|{final_url}");
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn handoff_id_from_fingerprint(profile_id: &str, fingerprint: &str) -> String {
    let short = fingerprint.get(0..16).unwrap_or(fingerprint);
    format!("{profile_id}_{short}")
}

fn read_handoff_record(profile_id: &str, handoff_id: &str) -> Result<Value, String> {
    let path = handoff_record_path(profile_id, handoff_id);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("read handoff record {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse handoff record {}: {e}", path.display()))
}

fn write_handoff_record(
    profile_id: &str,
    handoff_id: &str,
    record: &Value,
) -> Result<String, String> {
    let dir = handoff_profile_dir(profile_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create handoff inbox dir {}: {e}", dir.display()))?;
    let path = handoff_record_path(profile_id, handoff_id);
    let text = serde_json::to_string_pretty(record)
        .map_err(|e| format!("serialize handoff record: {e}"))?;
    std::fs::write(&path, text)
        .map_err(|e| format!("write handoff record {}: {e}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}

fn handoff_record_summary(record: &Value) -> Value {
    json!({
        "handoff_id": record["handoff_id"],
        "fingerprint": record["fingerprint"],
        "target_repo": record["target_repo"],
        "status": record["status"],
        "surface_id": record["issue"]["surface_id"],
        "rule": record["issue"]["rule"],
        "severity": record["issue"]["severity"],
        "confidence": record["issue"]["confidence"],
        "final_url": record["issue"]["final_url"],
        "source_run_id": record["source_run_id"],
        "evidence_path": record["evidence_path"],
        "occurrence_count": record["occurrence_count"],
        "created_at": record["created_at"],
        "updated_at": record["updated_at"],
        "last_seen_at": record["last_seen_at"],
        "acknowledged_at": record["acknowledged_at"],
        "resolved_at": record["resolved_at"],
        "recheck_hint": record["recheck_hint"],
    })
}

fn upsert_handoff_records(profile_id: &str, handoff: &Value) -> Result<Value, String> {
    let target_repo = handoff["target_repo"].as_str().unwrap_or("AgoraMarket");
    let source_run_id = handoff["generated_from_run_id"].as_str().unwrap_or("");
    let evidence_path = handoff["evidence_path"].as_str().unwrap_or("");
    let now = now_rfc3339();
    let mut records = Vec::new();

    for issue in handoff["issues"].as_array().into_iter().flatten() {
        let fingerprint = handoff_fingerprint(target_repo, issue);
        let handoff_id = handoff_id_from_fingerprint(profile_id, &fingerprint);
        let mut record = read_handoff_record(profile_id, &handoff_id).unwrap_or_else(|_| {
            json!({
                "handoff_id": handoff_id.clone(),
                "fingerprint": fingerprint.clone(),
                "handoff_type": handoff["handoff_type"],
                "target": handoff["target"],
                "target_repo": target_repo,
                "repo_hint": handoff["repo_hint"],
                "status": "open",
                "source_run_id": source_run_id,
                "first_source_run_id": source_run_id,
                "evidence_path": evidence_path,
                "issue": issue,
                "occurrence_count": 0,
                "created_at": now.clone(),
                "updated_at": now.clone(),
                "last_seen_at": now.clone(),
                "acknowledged_at": Value::Null,
                "acknowledged_by": Value::Null,
                "resolved_at": Value::Null,
                "resolved_by": Value::Null,
                "resolution_note": Value::Null,
                "recheck_hint": {
                    "tool": "agora_explore_audit",
                    "arguments": {
                        "surfaces": [issue["surface_id"].as_str().unwrap_or("")],
                        "timeout_ms": 12000,
                        "include_screenshots": false
                    }
                }
            })
        });

        let previous_status = record["status"].as_str().unwrap_or("open").to_string();
        let next_status = if previous_status == "resolved" {
            "reopened"
        } else {
            previous_status.as_str()
        };
        let occurrence_count = record["occurrence_count"].as_u64().unwrap_or(0) + 1;
        if let Some(obj) = record.as_object_mut() {
            obj.insert("status".to_string(), json!(next_status));
            obj.insert("source_run_id".to_string(), json!(source_run_id));
            obj.insert("evidence_path".to_string(), json!(evidence_path));
            obj.insert("issue".to_string(), issue.clone());
            obj.insert("occurrence_count".to_string(), json!(occurrence_count));
            obj.insert("updated_at".to_string(), json!(now));
            obj.insert("last_seen_at".to_string(), json!(now));
        }
        let record_path = write_handoff_record(profile_id, &handoff_id, &record)?;
        if let Some(obj) = record.as_object_mut() {
            obj.insert("record_path".to_string(), json!(record_path));
        }
        records.push(handoff_record_summary(&record));
    }

    Ok(json!({
        "profile_id": profile_id,
        "persisted_count": records.len(),
        "records": records,
    }))
}

fn read_handoff_records(profile_id: &str) -> Result<Vec<Value>, String> {
    let dir = handoff_profile_dir(profile_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| format!("read handoff inbox dir {}: {e}", dir.display()))?
    {
        let entry =
            entry.map_err(|e| format!("read handoff inbox entry {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let mut record: Value = match serde_json::from_str(&text) {
            Ok(record) => record,
            Err(_) => continue,
        };
        if let Some(obj) = record.as_object_mut() {
            obj.insert("record_path".to_string(), json!(path.to_string_lossy()));
        }
        records.push(record);
    }
    records.sort_by(|a, b| {
        b["updated_at"]
            .as_str()
            .unwrap_or("")
            .cmp(a["updated_at"].as_str().unwrap_or(""))
            .then_with(|| {
                b["handoff_id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(a["handoff_id"].as_str().unwrap_or(""))
            })
    });
    Ok(records)
}

fn recheck_next_status(previous_status: &str, passed: bool) -> &'static str {
    if passed {
        "verified_resolved"
    } else if matches!(previous_status, "resolved" | "verified_resolved") {
        "reopened"
    } else {
        "open"
    }
}

fn acceptance_summary(issue_candidates: &[Value]) -> Value {
    let target_app_candidates: Vec<Value> = issue_candidates
        .iter()
        .filter(|item| item["origin"].as_str() == Some("target_app_or_fixture"))
        .cloned()
        .collect();
    let sirin_flake_candidates: Vec<Value> = issue_candidates
        .iter()
        .filter(|item| item["origin"].as_str() == Some("sirin_or_browser_bootstrap"))
        .cloned()
        .collect();
    let needs_review_candidates: Vec<Value> = issue_candidates
        .iter()
        .filter(|item| item["origin"].as_str() == Some("needs_review"))
        .cloned()
        .collect();
    let needs_rerun_count = sirin_flake_candidates.len();
    let safe_to_escalate_to_codex = !target_app_candidates.is_empty() && needs_rerun_count == 0;
    let recommended_next_action = if needs_rerun_count > 0 {
        "rerun_or_inspect_sirin_bootstrap"
    } else if !target_app_candidates.is_empty() {
        "escalate_target_app_candidates_to_codex"
    } else if !needs_review_candidates.is_empty() {
        "review_low_confidence_candidates"
    } else {
        "none"
    };

    json!({
        "accepted_issue_count": target_app_candidates.len(),
        "target_app_candidates": target_app_candidates,
        "sirin_flake_candidates": sirin_flake_candidates,
        "needs_review_candidates": needs_review_candidates,
        "needs_rerun_count": needs_rerun_count,
        "safe_to_escalate_to_codex": safe_to_escalate_to_codex,
        "recommended_next_action": recommended_next_action,
    })
}

fn only_bootstrap_issues(issues: &[Value]) -> bool {
    !issues.is_empty()
        && issues
            .iter()
            .all(|item| item["origin"].as_str() == Some("sirin_or_browser_bootstrap"))
}

fn audit_run_entries(
    profile_id: &str,
) -> Result<Vec<(String, std::time::SystemTime, PathBuf)>, String> {
    let dir = audit_profile_dir(profile_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in
        std::fs::read_dir(&dir).map_err(|e| format!("read audit dir {}: {e}", dir.display()))?
    {
        let entry = entry.map_err(|e| format!("read audit dir entry {}: {e}", dir.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(run_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let result_path = path.join("result.json");
        if !result_path.exists() {
            continue;
        }
        let modified = std::fs::metadata(&result_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        entries.push((run_id, modified, result_path));
    }
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    Ok(entries)
}

/// List recent persisted audit runs. Read-only; does not start a browser.
pub(crate) fn call_audit_history(args: Value) -> Result<Value, String> {
    let profile_id = args
        .get("profile_id")
        .and_then(Value::as_str)
        .unwrap_or("agoramarket");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 100) as usize;
    let include_issues = args
        .get("include_issues")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let mut runs = Vec::new();
    for (run_id, _, path) in audit_run_entries(profile_id)?.into_iter().take(limit) {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                runs.push(json!({
                    "run_id": run_id,
                    "evidence_path": path.to_string_lossy(),
                    "status": "read_error",
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        let result: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                runs.push(json!({
                    "run_id": run_id,
                    "evidence_path": path.to_string_lossy(),
                    "status": "parse_error",
                    "error": err.to_string(),
                }));
                continue;
            }
        };
        let mut summary = summarize_audit_result(&result, path.to_string_lossy());
        if !include_issues {
            if let Some(obj) = summary.as_object_mut() {
                obj.remove("issue_summary");
            }
        }
        runs.push(summary);
    }

    Ok(json!({
        "profile_id": profile_id,
        "total_returned": runs.len(),
        "runs": runs,
    }))
}

/// Read a persisted audit run by run_id. Read-only; does not start a browser.
pub(crate) fn call_audit_get_result(args: Value) -> Result<Value, String> {
    let profile_id = args
        .get("profile_id")
        .and_then(Value::as_str)
        .unwrap_or("agoramarket");
    let run_id = args
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or("audit_get_result requires run_id")?;
    let (result, evidence_path) = read_audit_result(profile_id, run_id)?;
    Ok(json!({
        "profile_id": profile_id,
        "run_id": run_id,
        "evidence_path": evidence_path,
        "result": result,
    }))
}

/// Return the latest AgoraMarket audit summary. Read-only; does not start a browser.
pub(crate) fn call_agoramarket_audit_latest(args: Value) -> Result<Value, String> {
    let include_full = args
        .get("include_full")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some((run_id, _, path)) = audit_run_entries("agoramarket")?.into_iter().next() else {
        return Ok(json!({
            "profile_id": "agoramarket",
            "status": "no_runs",
            "run_id": null,
            "summary": null,
            "result": null,
        }));
    };
    let (result, evidence_path) = read_audit_result("agoramarket", &run_id)?;
    let summary = summarize_audit_result(&result, evidence_path);
    Ok(json!({
        "profile_id": "agoramarket",
        "status": "ok",
        "run_id": run_id,
        "evidence_path": path.to_string_lossy(),
        "summary": summary,
        "result": if include_full { result } else { Value::Null },
    }))
}

pub(crate) fn call_handoff_list(args: Value) -> Result<Value, String> {
    let profile_id = args
        .get("profile_id")
        .and_then(Value::as_str)
        .unwrap_or("agoramarket");
    let status = args
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("active");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 100) as usize;
    let records = read_handoff_records(profile_id)?;
    let records: Vec<Value> = records
        .into_iter()
        .filter(|record| {
            let record_status = record["status"].as_str().unwrap_or("open");
            match status {
                "all" => true,
                "active" => record_status != "resolved",
                other => record_status == other,
            }
        })
        .take(limit)
        .map(|record| handoff_record_summary(&record))
        .collect();
    Ok(json!({
        "profile_id": profile_id,
        "status_filter": status,
        "count": records.len(),
        "handoffs": records,
    }))
}

pub(crate) fn call_handoff_get(args: Value) -> Result<Value, String> {
    let profile_id = args
        .get("profile_id")
        .and_then(Value::as_str)
        .unwrap_or("agoramarket");
    let handoff_id = args
        .get("handoff_id")
        .and_then(Value::as_str)
        .ok_or("handoff_get requires handoff_id")?;
    let record = read_handoff_record(profile_id, handoff_id)?;
    Ok(json!({
        "profile_id": profile_id,
        "handoff_id": handoff_id,
        "record": record,
    }))
}

fn update_handoff_status(args: Value, next_status: &str) -> Result<Value, String> {
    let profile_id = args
        .get("profile_id")
        .and_then(Value::as_str)
        .unwrap_or("agoramarket");
    let handoff_id = args
        .get("handoff_id")
        .and_then(Value::as_str)
        .ok_or("handoff status update requires handoff_id")?;
    let actor = args.get("actor").and_then(Value::as_str).unwrap_or("codex");
    let note = args.get("note").and_then(Value::as_str);
    let now = now_rfc3339();
    let mut record = read_handoff_record(profile_id, handoff_id)?;
    let previous_status = record["status"].as_str().unwrap_or("open").to_string();
    if let Some(obj) = record.as_object_mut() {
        obj.insert("status".to_string(), json!(next_status));
        obj.insert("updated_at".to_string(), json!(now));
        match next_status {
            "acknowledged" => {
                obj.insert("acknowledged_at".to_string(), json!(now));
                obj.insert("acknowledged_by".to_string(), json!(actor));
            }
            "resolved" => {
                obj.insert("resolved_at".to_string(), json!(now));
                obj.insert("resolved_by".to_string(), json!(actor));
                if let Some(note) = note {
                    obj.insert("resolution_note".to_string(), json!(note));
                }
            }
            _ => {}
        }
    }
    let record_path = write_handoff_record(profile_id, handoff_id, &record)?;
    Ok(json!({
        "profile_id": profile_id,
        "handoff_id": handoff_id,
        "previous_status": previous_status,
        "status": next_status,
        "record_path": record_path,
        "record": record,
    }))
}

pub(crate) fn call_handoff_ack(args: Value) -> Result<Value, String> {
    update_handoff_status(args, "acknowledged")
}

pub(crate) fn call_handoff_resolve(args: Value) -> Result<Value, String> {
    update_handoff_status(args, "resolved")
}

pub(crate) async fn call_handoff_recheck(args: Value) -> Result<Value, String> {
    let profile_id = args
        .get("profile_id")
        .and_then(Value::as_str)
        .unwrap_or("agoramarket");
    let handoff_id = args
        .get("handoff_id")
        .and_then(Value::as_str)
        .ok_or("handoff_recheck requires handoff_id")?;
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(12_000);
    let include_screenshots = args
        .get("include_screenshots")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let original = read_handoff_record(profile_id, handoff_id)?;
    let previous_status = original["status"].as_str().unwrap_or("open").to_string();
    let fingerprint = original["fingerprint"]
        .as_str()
        .ok_or("handoff record missing fingerprint")?
        .to_string();
    let target_repo = original["target_repo"]
        .as_str()
        .unwrap_or("AgoraMarket")
        .to_string();
    let surface_id = original["recheck_hint"]["arguments"]["surfaces"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_str)
        .or_else(|| original["issue"]["surface_id"].as_str())
        .ok_or("handoff record missing recheck surface")?
        .to_string();

    let audit = call_agora_explore_audit(json!({
        "surfaces": [surface_id],
        "timeout_ms": timeout_ms,
        "include_screenshots": include_screenshots,
    }))
    .await?;

    let matched_issue = audit["issue_candidates"].as_array().and_then(|items| {
        items
            .iter()
            .find(|issue| handoff_fingerprint(&target_repo, issue) == fingerprint)
            .cloned()
    });
    let passed = matched_issue.is_none();
    let next_status = recheck_next_status(&previous_status, passed);
    let now = now_rfc3339();
    let mut record = read_handoff_record(profile_id, handoff_id).unwrap_or(original);
    if let Some(obj) = record.as_object_mut() {
        obj.insert("status".to_string(), json!(next_status));
        obj.insert("updated_at".to_string(), json!(now));
        obj.insert("last_rechecked_at".to_string(), json!(now));
        obj.insert("last_recheck_run_id".to_string(), audit["run_id"].clone());
        obj.insert("last_recheck_passed".to_string(), json!(passed));
        obj.insert(
            "last_recheck_evidence_path".to_string(),
            audit["evidence_path"].clone(),
        );
        obj.insert("source_run_id".to_string(), audit["run_id"].clone());
        obj.insert("evidence_path".to_string(), audit["evidence_path"].clone());
        if let Some(issue) = &matched_issue {
            obj.insert("issue".to_string(), issue.clone());
        }
    }
    let record_path = write_handoff_record(profile_id, handoff_id, &record)?;

    Ok(json!({
        "profile_id": profile_id,
        "handoff_id": handoff_id,
        "previous_status": previous_status,
        "status": next_status,
        "passed": passed,
        "matched_issue": matched_issue.unwrap_or(Value::Null),
        "recheck_run_id": audit["run_id"],
        "evidence_path": audit["evidence_path"],
        "record_path": record_path,
        "record": record,
    }))
}

async fn run_audit_surface(
    surface: &AuditSurface,
    run_id: &str,
    surface_index: usize,
    timeout_ms: u64,
    include_screenshots: bool,
    attempt: u64,
) -> AuditSurfaceRun {
    let session_id = format!(
        "{}-{}-{}-attempt{}",
        run_id, surface.id, surface_index, attempt
    );
    let mut steps = Vec::new();
    let mut surface_issues = Vec::new();
    let mut screenshot = Value::Null;
    let mut nav_error_for_surface: Option<String> = None;

    let (_, viewport_error) = browser_action_in_session(
        &session_id,
        json!({
            "action": "viewport",
            "width": surface.viewport.width,
            "height": surface.viewport.height,
            "mobile": surface.viewport.mobile,
            "scale": 1.0,
        }),
    )
    .await;
    if let Some(err) = viewport_error {
        surface_issues.push(issue(
            &surface.id,
            "browser_viewport_error",
            "medium",
            "medium",
            err,
            "rerun_audit",
        ));
    }

    let (goto_result, goto_error) = browser_action_in_session(
        &session_id,
        json!({
            "action": "goto",
            "target": surface.url.clone(),
            "browser_headless": true,
        }),
    )
    .await;
    steps.push(json!({ "action": "goto", "ok": goto_error.is_none(), "result": goto_result, "error": goto_error }));

    let (_, wait_error) =
        browser_action_in_session(&session_id, json!({ "action": "wait", "ms": 800 })).await;
    steps.push(json!({ "action": "wait", "ok": wait_error.is_none(), "error": wait_error }));

    let (semantics_result, semantics_error) = browser_action_in_session(
        &session_id,
        json!({
            "action": "wait_for_flutter_semantics",
            "min_roles": 8,
            "timeout_ms": timeout_ms,
        }),
    )
    .await;
    if let Some(err) = semantics_error.clone() {
        surface_issues.push(issue(
            &surface.id,
            "flutter_semantics_missing",
            "high",
            "medium",
            err,
            "rerun_active_smoke",
        ));
    }
    steps.push(json!({ "action": "wait_for_flutter_semantics", "ok": semantics_error.is_none(), "result": semantics_result, "error": semantics_error }));

    let (dismiss_result, dismiss_error) =
        browser_action_in_session(&session_id, json!({ "action": "dismiss_passkey_prompt" })).await;
    steps.push(json!({ "action": "dismiss_passkey_prompt", "ok": dismiss_error.is_none(), "result": dismiss_result, "error": dismiss_error }));

    if let Some(nav_target) = &surface.nav_target {
        let (nav_result, nav_error) = browser_action_in_session(
            &session_id,
            json!({
                "action": "agora_nav_click",
                "target": nav_target,
            }),
        )
        .await;
        if let Some(err) = nav_error.clone() {
            nav_error_for_surface = Some(err);
        }
        steps.push(json!({ "action": "agora_nav_click", "target": nav_target, "ok": nav_error.is_none(), "result": nav_result, "error": nav_error }));

        let (_, post_nav_wait_error) =
            browser_action_in_session(&session_id, json!({ "action": "wait", "ms": 700 })).await;
        steps.push(json!({ "action": "post_nav_wait", "ok": post_nav_wait_error.is_none(), "error": post_nav_wait_error }));

        let (mut post_nav_semantics_result, mut post_nav_semantics_error) =
            browser_action_in_session(
                &session_id,
                json!({
                    "action": "wait_for_flutter_semantics",
                    "min_roles": 8,
                    "timeout_ms": timeout_ms,
                }),
            )
            .await;
        steps.push(json!({ "action": "post_nav_wait_for_flutter_semantics", "ok": post_nav_semantics_error.is_none(), "result": post_nav_semantics_result, "error": post_nav_semantics_error }));

        if post_nav_semantics_error
            .as_deref()
            .map(|err| err.contains("about:blank"))
            .unwrap_or(false)
        {
            let (recover_goto_result, recover_goto_error) = browser_action_in_session(
                &session_id,
                json!({
                    "action": "goto",
                    "target": surface.url.clone(),
                    "browser_headless": true,
                }),
            )
            .await;
            steps.push(json!({ "action": "recover_about_blank_goto", "ok": recover_goto_error.is_none(), "result": recover_goto_result, "error": recover_goto_error }));

            let (_, recover_wait_error) =
                browser_action_in_session(&session_id, json!({ "action": "wait", "ms": 800 }))
                    .await;
            steps.push(json!({ "action": "recover_wait", "ok": recover_wait_error.is_none(), "error": recover_wait_error }));

            let (recover_semantics_result, recover_semantics_error) = browser_action_in_session(
                &session_id,
                json!({
                    "action": "wait_for_flutter_semantics",
                    "min_roles": 8,
                    "timeout_ms": timeout_ms,
                }),
            )
            .await;
            steps.push(json!({ "action": "recover_wait_for_flutter_semantics", "ok": recover_semantics_error.is_none(), "result": recover_semantics_result, "error": recover_semantics_error }));

            let (recover_dismiss_result, recover_dismiss_error) = browser_action_in_session(
                &session_id,
                json!({ "action": "dismiss_passkey_prompt" }),
            )
            .await;
            steps.push(json!({ "action": "recover_dismiss_passkey_prompt", "ok": recover_dismiss_error.is_none(), "result": recover_dismiss_result, "error": recover_dismiss_error }));

            let (retry_nav_result, retry_nav_error) = browser_action_in_session(
                &session_id,
                json!({
                    "action": "agora_nav_click",
                    "target": nav_target,
                }),
            )
            .await;
            steps.push(json!({ "action": "retry_agora_nav_click", "target": nav_target, "ok": retry_nav_error.is_none(), "result": retry_nav_result, "error": retry_nav_error }));

            let (_, retry_wait_error) =
                browser_action_in_session(&session_id, json!({ "action": "wait", "ms": 700 }))
                    .await;
            steps.push(json!({ "action": "retry_post_nav_wait", "ok": retry_wait_error.is_none(), "error": retry_wait_error }));

            let retry = browser_action_in_session(
                &session_id,
                json!({
                    "action": "wait_for_flutter_semantics",
                    "min_roles": 8,
                    "timeout_ms": timeout_ms,
                }),
            )
            .await;
            post_nav_semantics_result = retry.0;
            post_nav_semantics_error = retry.1;
            steps.push(json!({ "action": "retry_post_nav_wait_for_flutter_semantics", "ok": post_nav_semantics_error.is_none(), "result": post_nav_semantics_result, "error": post_nav_semantics_error }));
        }

        if let Some(err) = post_nav_semantics_error.clone() {
            surface_issues.push(issue(
                &surface.id,
                "flutter_semantics_missing",
                "high",
                "medium",
                err,
                "rerun_active_smoke",
            ));
        }
    }

    let (url_result, url_error) =
        browser_action_in_session(&session_id, json!({ "action": "url" })).await;
    let (title_result, title_error) =
        browser_action_in_session(&session_id, json!({ "action": "title" })).await;
    let (shadow_result, shadow_error) =
        browser_action_in_session(&session_id, json!({ "action": "shadow_dump" })).await;
    let (console_result, console_error) =
        browser_action_in_session(&session_id, json!({ "action": "console", "limit": 40 })).await;
    if include_screenshots {
        let (screenshot_result, screenshot_error) =
            browser_action_in_session(&session_id, json!({ "action": "screenshot" })).await;
        screenshot = screenshot_result.unwrap_or_else(|| json!({ "error": screenshot_error }));
    }

    let final_url = url_result
        .as_ref()
        .and_then(|v| v["url"].as_str())
        .unwrap_or("")
        .to_string();
    let title = title_result
        .as_ref()
        .and_then(|v| v["title"].as_str())
        .unwrap_or("")
        .to_string();
    let shadow_count = shadow_result
        .as_ref()
        .and_then(|v| v["count"].as_u64())
        .unwrap_or(0);
    let shadow_text = shadow_result
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_default();
    let console_errors = console_result
        .as_ref()
        .map(console_error_items)
        .unwrap_or_default();
    let searchable = format!("{final_url}\n{title}\n{shadow_text}");

    if let Some(err) = url_error {
        surface_issues.push(issue(
            &surface.id,
            "browser_url_error",
            "medium",
            "medium",
            err,
            "rerun_audit",
        ));
    }
    if let Some(err) = title_error {
        surface_issues.push(issue(
            &surface.id,
            "browser_title_error",
            "low",
            "medium",
            err,
            "rerun_audit",
        ));
    }
    if let Some(err) = shadow_error {
        surface_issues.push(issue(
            &surface.id,
            "shadow_dump_error",
            "high",
            "medium",
            err,
            "rerun_active_smoke",
        ));
    }
    if let Some(err) = console_error {
        surface_issues.push(issue(
            &surface.id,
            "console_capture_error",
            "medium",
            "medium",
            err,
            "rerun_audit",
        ));
    }
    if final_url.contains("#/login") || final_url.to_lowercase().contains("login") {
        surface_issues.push(issue(
            &surface.id,
            "unexpected_login_route",
            "high",
            "high",
            format!("final_url={final_url}"),
            "inspect_fixture_or_auth_bootstrap",
        ));
    }
    if let Some(evidence) = route_role_mismatch(&surface.role, &final_url) {
        surface_issues.push(issue(
            &surface.id,
            "route_role_mismatch",
            "high",
            "high",
            evidence,
            "inspect_role_bootstrap_or_router",
        ));
    }
    if shadow_count == 0 {
        surface_issues.push(issue(
            &surface.id,
            "blank_or_no_shadow_dom",
            "high",
            "medium",
            "shadow_dump returned 0 elements",
            "rerun_active_smoke",
        ));
    }
    if !console_errors.is_empty() {
        surface_issues.push(issue(
            &surface.id,
            "console_error",
            "high",
            "high",
            format!(
                "{} console error-like messages captured",
                console_errors.len()
            ),
            "inspect_console_and_target_app",
        ));
    }

    let expected_hits = text_contains_any(&searchable, &surface.expected_any);
    let forbidden_hits = text_contains_any(&searchable, &surface.forbidden_any);
    if let Some(err) = nav_error_for_surface {
        if expected_hits.is_empty() {
            surface_issues.push(issue(
                &surface.id,
                "navigation_target_missing",
                "medium",
                "medium",
                err,
                "inspect_surface_navigation",
            ));
        }
    }
    if expected_hits.is_empty() {
        surface_issues.push(issue(
            &surface.id,
            "missing_core_surface_text",
            "medium",
            "medium",
            format!(
                "none of {:?} was visible in URL/title/shadow_dump",
                surface.expected_any
            ),
            "inspect_surface_screenshot",
        ));
    }
    if !forbidden_hits.is_empty() {
        surface_issues.push(issue(
            &surface.id,
            "wrong_role_or_surface_content",
            "high",
            "medium",
            format!(
                "unexpected terms visible for {}: {:?}",
                surface.role, forbidden_hits
            ),
            "inspect_role_bootstrap",
        ));
    }

    enrich_issues(&mut surface_issues, &final_url);
    let (close_result, close_error) = browser_action(json!({
        "action": "close_session",
        "target": session_id,
    }))
    .await;
    steps.push(json!({ "action": "close_session", "ok": close_error.is_none(), "result": close_result, "error": close_error }));

    let result = json!({
        "id": surface.id.clone(),
        "session_id": session_id,
        "role": surface.role.clone(),
        "url": surface.url.clone(),
        "nav_target": surface.nav_target.clone(),
        "final_url": final_url,
        "title": title,
        "viewport": {
            "width": surface.viewport.width,
            "height": surface.viewport.height,
            "mobile": surface.viewport.mobile,
        },
        "shadow_count": shadow_count,
        "expected_hits": expected_hits,
        "forbidden_hits": forbidden_hits,
        "console_errors": console_errors,
        "issues": surface_issues,
        "steps": steps,
        "screenshot": screenshot,
        "attempt": attempt,
        "rerun_reason": null,
    });
    let issues = result["issues"].as_array().cloned().unwrap_or_default();
    AuditSurfaceRun { result, issues }
}

/// Deterministic AgoraMarket exploration audit for external AI callers.
pub(crate) async fn call_agora_explore_audit(args: Value) -> Result<Value, String> {
    let include_screenshots = args
        .get("include_screenshots")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stop_on_first_critical = args
        .get("stop_on_first_critical")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let requested: Option<Vec<String>> =
        args.get("surfaces").and_then(Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        });

    let (profile, config_source) = load_profile("agoramarket")?;
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(profile.default_timeout_ms);
    let run_id = audit_run_id(&profile.id);

    let mut surfaces = Vec::new();
    for surface in profile.surfaces {
        if requested
            .as_ref()
            .map(|ids| ids.iter().any(|id| id == &surface.id))
            .unwrap_or(true)
        {
            surfaces.push(surface);
        }
    }
    if surfaces.is_empty() {
        return Err("agora_explore_audit: no matching surfaces".into());
    }

    let mut surface_results = Vec::new();
    let mut issue_candidates = Vec::new();
    let mut completed = 0usize;

    for surface in surfaces {
        let mut run = run_audit_surface(
            &surface,
            &run_id,
            completed,
            timeout_ms,
            include_screenshots,
            1,
        )
        .await;

        if only_bootstrap_issues(&run.issues) {
            let first_attempt = run.result.clone();
            let mut retry = run_audit_surface(
                &surface,
                &run_id,
                completed,
                timeout_ms,
                include_screenshots,
                2,
            )
            .await;
            if let Some(obj) = retry.result.as_object_mut() {
                obj.insert(
                    "rerun_reason".to_string(),
                    json!("bootstrap_only_first_attempt"),
                );
                obj.insert("first_attempt".to_string(), first_attempt);
            }
            run = retry;
        }

        issue_candidates.extend(run.issues.iter().cloned());
        completed += 1;
        surface_results.push(run.result);

        if stop_on_first_critical
            && issue_candidates
                .iter()
                .any(|item| item["severity"].as_str() == Some("high"))
        {
            break;
        }
    }

    let critical_count = issue_candidates
        .iter()
        .filter(|item| item["severity"].as_str() == Some("high"))
        .count();
    let status = if issue_candidates.is_empty() {
        "clean"
    } else if critical_count > 0 {
        "issues_found"
    } else {
        "warnings_found"
    };
    let recommended_action = match status {
        "clean" => "none",
        "issues_found" => "review_issue_candidates_then_run_agora_health_triage",
        _ => "review_issue_candidates",
    };
    let acceptance = acceptance_summary(&issue_candidates);

    let evidence_path = audit_result_path("agoramarket", &run_id)
        .to_string_lossy()
        .into_owned();
    let mut result = json!({
        "target": profile.target,
        "profile_id": profile.id,
        "config_source": config_source,
        "run_id": run_id,
        "tool": "agora_explore_audit",
        "status": status,
        "completed_surfaces": completed,
        "issue_count": issue_candidates.len(),
        "critical_count": critical_count,
        "recommended_action": recommended_action,
        "acceptance_summary": acceptance,
        "issue_candidates": issue_candidates,
        "surfaces": surface_results,
        "evidence_path": evidence_path,
    });
    let handoff = codex_handoff(&result, evidence_path.clone());
    let handoff_inbox = upsert_handoff_records("agoramarket", &handoff)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("codex_handoff".to_string(), handoff);
        obj.insert("handoff_inbox".to_string(), handoff_inbox);
    }
    let evidence_path = persist_audit_result(
        "agoramarket",
        result["run_id"].as_str().unwrap_or("unknown"),
        &result,
    )?;
    debug_assert_eq!(
        result["evidence_path"].as_str(),
        Some(evidence_path.as_str())
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_agoramarket_profile_parses() {
        let (profile, source) = load_profile("agoramarket").expect("profile parses");
        assert_eq!(profile.id, "agoramarket");
        assert!(source.contains("agoramarket.yaml"));
        assert!(profile.surfaces.iter().any(|s| s.id == "buyer_home"));
        assert!(profile.surfaces.iter().any(|s| s.id == "seller_wallet"));
    }

    #[test]
    fn issue_candidate_contract_has_required_fields() {
        let item = issue(
            "buyer_home",
            "route_role_mismatch",
            "high",
            "high",
            "x",
            "inspect",
        );
        for key in [
            "surface_id",
            "type",
            "rule",
            "origin",
            "severity",
            "confidence",
            "final_url",
            "evidence",
            "recommended_action",
            "screenshot_hint",
        ] {
            assert!(item.get(key).is_some(), "missing {key}");
        }
        assert_eq!(item["origin"], "target_app_or_fixture");
    }

    #[test]
    fn route_role_mismatch_detects_wrong_hash_route() {
        let mismatch = route_role_mismatch(
            "buyer",
            "https://redandan.github.io/?__test_role=buyer#/seller",
        );
        assert!(mismatch.unwrap().contains("buyer role"));
        assert!(route_role_mismatch(
            "seller",
            "https://redandan.github.io/?__test_role=seller#/seller"
        )
        .is_none());
    }

    #[test]
    fn audit_summary_contract_has_history_fields() {
        let result = json!({
            "run_id": "audit_agoramarket_test",
            "profile_id": "agoramarket",
            "target": "AgoraMarket",
            "status": "issues_found",
            "completed_surfaces": 6,
            "issue_count": 1,
            "critical_count": 1,
            "recommended_action": "review",
            "config_source": "embedded",
            "tool": "agora_explore_audit",
            "acceptance_summary": {
                "accepted_issue_count": 1,
                "target_app_candidates": [],
                "sirin_flake_candidates": [],
                "needs_review_candidates": [],
                "needs_rerun_count": 0,
                "safe_to_escalate_to_codex": true,
                "recommended_next_action": "escalate_target_app_candidates_to_codex"
            },
            "codex_handoff": {
                "handoff_type": "codex_target_app_triage",
                "target_repo": "AgoraMarket",
                "safe_to_escalate": true,
                "issues": []
            },
            "issue_candidates": [{
                "surface_id": "buyer_home",
                "rule": "route_role_mismatch",
                "origin": "target_app_or_fixture",
                "severity": "high",
                "confidence": "high",
                "final_url": "https://example/#/seller",
                "recommended_action": "inspect",
                "evidence": "wrong route"
            }]
        });
        let summary = summarize_audit_result(&result, "x/result.json");
        for key in [
            "run_id",
            "profile_id",
            "target",
            "status",
            "completed_surfaces",
            "issue_count",
            "critical_count",
            "recommended_action",
            "acceptance_summary",
            "codex_handoff",
            "evidence_path",
            "issue_summary",
        ] {
            assert!(summary.get(key).is_some(), "missing {key}");
        }
        assert_eq!(summary["issue_summary"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn codex_handoff_contains_only_escalatable_target_candidates() {
        let target = issue(
            "buyer_home",
            "route_role_mismatch",
            "high",
            "high",
            "buyer role landed on non-buyer route",
            "inspect",
        );
        let mut bootstrap = issue(
            "seller_orders",
            "flutter_semantics_missing",
            "high",
            "medium",
            "about:blank",
            "rerun",
        );
        bootstrap["origin"] = json!("sirin_or_browser_bootstrap");
        let issues = vec![target, bootstrap];
        let result = json!({
            "target": "AgoraMarket",
            "tool": "agora_explore_audit",
            "run_id": "audit_agoramarket_test",
            "acceptance_summary": acceptance_summary(&issues),
        });

        let handoff = codex_handoff(&result, "x/result.json");

        assert_eq!(handoff["handoff_type"], "codex_target_app_triage");
        assert_eq!(handoff["target_repo"], "AgoraMarket");
        assert_eq!(
            handoff["repo_hint"],
            "C:\\Users\\Redan\\IdeaProjects\\AgoraMarket"
        );
        assert_eq!(handoff["safe_to_escalate"], false);
        assert_eq!(handoff["issue_count"], 1);
        assert_eq!(handoff["issues"].as_array().unwrap().len(), 1);
        assert_eq!(handoff["issues"][0]["surface_id"], "buyer_home");
        assert_eq!(handoff["issues"][0]["evidence_path"], "x/result.json");
        assert_eq!(
            handoff["issues"][0]["suggested_codex_action"],
            "inspect AgoraMarket role bootstrap, route guard, and test fixture role query handling"
        );
    }

    #[test]
    fn handoff_fingerprint_is_stable_for_same_issue_key() {
        let issue_a = json!({
            "surface_id": "buyer_home",
            "rule": "route_role_mismatch",
            "final_url": "https://example/#/seller",
            "evidence": "first"
        });
        let issue_b = json!({
            "surface_id": "buyer_home",
            "rule": "route_role_mismatch",
            "final_url": "https://example/#/seller",
            "evidence": "second"
        });
        let issue_c = json!({
            "surface_id": "admin_home",
            "rule": "route_role_mismatch",
            "final_url": "https://example/#/seller"
        });

        assert_eq!(
            handoff_fingerprint("AgoraMarket", &issue_a),
            handoff_fingerprint("AgoraMarket", &issue_b)
        );
        assert_ne!(
            handoff_fingerprint("AgoraMarket", &issue_a),
            handoff_fingerprint("AgoraMarket", &issue_c)
        );
    }

    #[test]
    fn handoff_upsert_deduplicates_and_preserves_status_lifecycle() {
        let profile_id = format!("test_handoff_{}", std::process::id());
        let _ = std::fs::remove_dir_all(handoff_profile_dir(&profile_id));
        let issue = json!({
            "surface_id": "buyer_home",
            "rule": "route_role_mismatch",
            "origin": "target_app_or_fixture",
            "severity": "high",
            "confidence": "high",
            "final_url": "https://example/#/seller",
            "evidence": "wrong route",
            "suggested_codex_action": "inspect"
        });
        let handoff = json!({
            "handoff_type": "codex_target_app_triage",
            "target": "AgoraMarket",
            "target_repo": "AgoraMarket",
            "repo_hint": "C:\\Users\\Redan\\IdeaProjects\\AgoraMarket",
            "generated_from_run_id": "run_1",
            "evidence_path": "x/result.json",
            "issues": [issue]
        });

        let first = upsert_handoff_records(&profile_id, &handoff).expect("first upsert");
        let handoff_id = first["records"][0]["handoff_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(first["persisted_count"], 1);
        assert_eq!(first["records"][0]["status"], "open");
        assert_eq!(first["records"][0]["occurrence_count"], 1);

        let ack = update_handoff_status(
            json!({
                "profile_id": profile_id.clone(),
                "handoff_id": handoff_id.clone(),
                "actor": "test"
            }),
            "acknowledged",
        )
        .expect("ack");
        assert_eq!(ack["status"], "acknowledged");

        let second = upsert_handoff_records(&profile_id, &handoff).expect("second upsert");
        assert_eq!(second["persisted_count"], 1);
        assert_eq!(second["records"][0]["status"], "acknowledged");
        assert_eq!(second["records"][0]["occurrence_count"], 2);

        let _ = std::fs::remove_dir_all(handoff_profile_dir(&profile_id));
    }

    #[test]
    fn recheck_status_transition_matches_passed_and_previous_state() {
        assert_eq!(recheck_next_status("resolved", true), "verified_resolved");
        assert_eq!(recheck_next_status("verified_resolved", false), "reopened");
        assert_eq!(recheck_next_status("resolved", false), "reopened");
        assert_eq!(recheck_next_status("acknowledged", false), "open");
        assert_eq!(recheck_next_status("open", false), "open");
    }

    #[test]
    fn recheck_fingerprint_detects_matching_issue_candidate() {
        let original = json!({
            "surface_id": "admin_home",
            "rule": "route_role_mismatch",
            "final_url": "https://example/#/seller"
        });
        let same = json!({
            "surface_id": "admin_home",
            "rule": "route_role_mismatch",
            "final_url": "https://example/#/seller",
            "evidence": "same problem"
        });
        let different = json!({
            "surface_id": "admin_home",
            "rule": "wrong_role_or_surface_content",
            "final_url": "https://example/#/seller"
        });
        let fingerprint = handoff_fingerprint("AgoraMarket", &original);

        assert_eq!(handoff_fingerprint("AgoraMarket", &same), fingerprint);
        assert_ne!(handoff_fingerprint("AgoraMarket", &different), fingerprint);
    }

    #[test]
    fn acceptance_summary_splits_target_and_bootstrap_candidates() {
        let target = issue(
            "buyer_home",
            "route_role_mismatch",
            "high",
            "high",
            "wrong route",
            "inspect",
        );
        let mut bootstrap = issue(
            "seller_orders",
            "flutter_semantics_missing",
            "high",
            "medium",
            "about:blank",
            "rerun",
        );
        bootstrap["origin"] = json!("sirin_or_browser_bootstrap");

        let summary = acceptance_summary(&[target, bootstrap]);

        assert_eq!(summary["accepted_issue_count"], 1);
        assert_eq!(
            summary["target_app_candidates"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            summary["sirin_flake_candidates"].as_array().unwrap().len(),
            1
        );
        assert_eq!(summary["needs_rerun_count"], 1);
        assert_eq!(summary["safe_to_escalate_to_codex"], false);
        assert_eq!(
            summary["recommended_next_action"],
            "rerun_or_inspect_sirin_bootstrap"
        );
    }

    #[test]
    fn only_bootstrap_issues_requires_non_empty_all_bootstrap() {
        assert!(!only_bootstrap_issues(&[]));

        let mut bootstrap = issue(
            "seller_orders",
            "shadow_dump_error",
            "high",
            "medium",
            "about:blank",
            "rerun",
        );
        bootstrap["origin"] = json!("sirin_or_browser_bootstrap");
        assert!(only_bootstrap_issues(&[bootstrap.clone()]));

        let target = issue(
            "buyer_home",
            "route_role_mismatch",
            "high",
            "high",
            "wrong route",
            "inspect",
        );
        assert!(!only_bootstrap_issues(&[bootstrap, target]));
    }
}
