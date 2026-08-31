//! Sirin-local operations task schedule.
//!
//! This is intentionally separate from the legacy `~/.claude/tasks.json`
//! session tracker. Ops schedules are Sirin product state and live under
//! `platform::app_data_dir()/tracking`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::platform::NoWindow;

use chrono::{Duration, Local, Utc};
use serde_json::{json, Value};

const OPS_SCHEDULE_FILE: &str = "ops_schedule.jsonl";
const RUNNER_INTERVAL_SECONDS: u64 = 60;

#[derive(Debug)]
struct OpsScheduleClaim {
    task: Option<Value>,
    schedule: Option<Value>,
}

pub(crate) fn spawn_runner_loop() {
    tokio::spawn(async {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(RUNNER_INTERVAL_SECONDS));
        interval.tick().await;
        loop {
            interval.tick().await;
            match call_ops_schedule_run_due(json!({ "limit": 1 })).await {
                Ok(value) => {
                    if value.get("ran").and_then(Value::as_u64).unwrap_or(0) > 0 {
                        tracing::info!(target: "sirin", "[ops_schedule] executed due schedule: {value}");
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "sirin", "[ops_schedule] runner failed: {e}");
                }
            }
        }
    });
}

pub(crate) fn call_ops_schedule_create(args: Value) -> Result<Value, String> {
    let title = arg_text(&args, "title").ok_or_else(|| "title is required".to_string())?;
    let task_type = arg_text(&args, "task_type")
        .or_else(|| arg_text(&args, "taskType"))
        .unwrap_or_else(|| "SIRIN_AGORA_TEST_HEALTH_SCOUT".to_string());
    let cadence = arg_text(&args, "cadence").unwrap_or_else(|| "manual".into());
    let priority = arg_text(&args, "priority").unwrap_or_else(|| "P1".into());
    let scheduled_for =
        arg_text(&args, "scheduled_for").or_else(|| arg_text(&args, "scheduledFor"));
    let description = arg_text(&args, "description").unwrap_or_default();
    let params = args
        .get("params")
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));

    let record = mutate_ops_schedule_records(|records| {
        let id = next_schedule_id(records);
        let now = Utc::now().to_rfc3339();
        let record = json!({
            "id": id,
            "title": title,
            "description": description,
            "task_type": task_type,
            "params": params,
            "cadence": cadence,
            "priority": priority,
            "status": "SCHEDULED",
            "scheduled_for": scheduled_for.map(Value::String).unwrap_or(Value::Null),
            "last_run_at": null,
            "next_run_at": null,
            "created_at": now,
            "updated_at": now,
            "notes": [],
            "boundary": "Sirin-local ops schedule; no AgoraMarketAPI mutation."
        });
        records.push(record.clone());
        Ok((record, true))
    })?;
    Ok(json!({
        "created": true,
        "schedule": record,
        "storage_path": ops_schedule_path(),
        "boundary": "Stored in Sirin app data only."
    }))
}

pub(crate) fn call_ops_schedule_list(args: Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let status = arg_text(&args, "status").map(|s| s.to_ascii_uppercase());
    let task_type = arg_text(&args, "task_type").or_else(|| arg_text(&args, "taskType"));
    let priority = arg_text(&args, "priority");

    let mut records = load_ops_schedule_records()?;
    records.sort_by(|a, b| {
        b.get("updated_at")
            .and_then(Value::as_str)
            .cmp(&a.get("updated_at").and_then(Value::as_str))
    });
    let schedules: Vec<Value> = records
        .into_iter()
        .filter(|record| {
            status.as_deref().is_none_or(|expected| {
                record
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
        })
        .filter(|record| {
            task_type.as_deref().is_none_or(|expected| {
                record
                    .get("task_type")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual == expected)
            })
        })
        .filter(|record| {
            priority.as_deref().is_none_or(|expected| {
                record
                    .get("priority")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual == expected)
            })
        })
        .take(limit)
        .collect();
    let schedules = enrich_schedules_with_action_statuses(schedules)?;

    Ok(json!({
        "count": schedules.len(),
        "schedules": schedules,
        "storage_path": ops_schedule_path(),
        "boundary": "Sirin-local ops schedule only."
    }))
}

pub(crate) fn call_ops_schedule_digest(args: Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 50) as usize;
    let task_type = arg_text(&args, "task_type").or_else(|| arg_text(&args, "taskType"));

    let mut records = load_ops_schedule_records()?;
    records.sort_by(|a, b| {
        b.get("updated_at")
            .and_then(Value::as_str)
            .cmp(&a.get("updated_at").and_then(Value::as_str))
    });
    let filtered_records: Vec<Value> = records
        .into_iter()
        .filter(|record| {
            task_type.as_deref().is_none_or(|expected| {
                record
                    .get("task_type")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual == expected)
            })
        })
        .collect();
    let scheduled_total = filtered_records
        .iter()
        .filter(|record| {
            record
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status == "SCHEDULED")
        })
        .count();
    let schedules: Vec<Value> = filtered_records.into_iter().take(limit).collect();
    let schedules = enrich_schedules_with_action_statuses(schedules)?;
    let review_queues = current_ops_review_queues()?;
    let digest = build_ops_schedule_digest(&schedules, &review_queues, Some(scheduled_total));
    Ok(json!({
        "digest": digest,
        "count": schedules.len(),
        "schedules": schedules,
        "storage_path": ops_schedule_path(),
        "boundary": "Sirin-local ops schedule digest only; no task was executed and no external system was mutated."
    }))
}

pub(crate) fn call_ops_external_issue_sync(args: Value) -> Result<Value, String> {
    let task_type = arg_text(&args, "task_type")
        .or_else(|| arg_text(&args, "taskType"))
        .unwrap_or_else(|| "AGORA_MARKET_FIRST_ORDER_ACTIVATION".to_string());
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;

    let records = load_ops_schedule_records()?;
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for record in records
        .iter()
        .filter(|record| {
            record
                .get("task_type")
                .and_then(Value::as_str)
                .is_some_and(|actual| actual == task_type)
        })
        .take(limit)
    {
        for issue in tracked_issue_values_from_record(record) {
            if let Some(reference) = GitHubIssueRef::from_value(issue) {
                if seen.insert(reference.key()) {
                    targets.push(reference);
                }
            }
        }
    }

    let mut snapshots = Vec::new();
    let mut errors = Vec::new();
    for target in &targets {
        match fetch_github_issue_snapshot(target) {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(e) => errors.push(json!({
                "key": target.key(),
                "url": target.url(),
                "error": e,
            })),
        }
    }

    let updated_issue_values = mutate_ops_schedule_records(|records| {
        let updated_issue_values = apply_external_issue_snapshots(records, &snapshots);
        Ok((updated_issue_values, updated_issue_values > 0))
    })?;
    let updated = updated_issue_values > 0;

    Ok(json!({
        "synced": updated,
        "targetTaskType": task_type,
        "trackedIssueTargets": targets.len(),
        "fetchedIssueStates": snapshots.len(),
        "updatedIssueValues": updated_issue_values,
        "issues": snapshots.iter().map(GitHubIssueSnapshot::to_value).collect::<Vec<_>>(),
        "errors": errors,
        "storage_path": ops_schedule_path(),
        "boundary": "Read-only GitHub issue state sync into Sirin-local ops schedule snapshots only; no GitHub issue, AgoraMarket, AgoraMarketAPI, order, payment, product, customer, or seller state was mutated."
    }))
}

fn auto_sync_external_issues_after_run_due(task_types: HashSet<String>) -> Value {
    if task_types.is_empty() {
        return json!({
            "attempted": false,
            "reason": "no schedule ran",
            "boundary": "No external issue sync was attempted."
        });
    }

    let mut results = Vec::new();
    for task_type in task_types {
        match call_ops_external_issue_sync(json!({
            "taskType": task_type,
            "limit": 200
        })) {
            Ok(result) => results.push(json!({
                "taskType": result.get("targetTaskType").cloned().unwrap_or(Value::Null),
                "synced": result.get("synced").cloned().unwrap_or(Value::Null),
                "trackedIssueTargets": result.get("trackedIssueTargets").cloned().unwrap_or(Value::Null),
                "fetchedIssueStates": result.get("fetchedIssueStates").cloned().unwrap_or(Value::Null),
                "updatedIssueValues": result.get("updatedIssueValues").cloned().unwrap_or(Value::Null),
                "errorCount": result
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0),
                "status": "ok",
            })),
            Err(error) => results.push(json!({
                "taskType": task_type,
                "status": "failed",
                "error": error,
            })),
        }
    }

    json!({
        "attempted": true,
        "results": results,
        "boundary": "Post-run external issue sync is read-only toward GitHub and only updates Sirin-local schedule snapshots."
    })
}

pub(crate) fn call_ops_schedule_update(args: Value) -> Result<Value, String> {
    let schedule_id = arg_text(&args, "schedule_id")
        .or_else(|| arg_text(&args, "scheduleId"))
        .ok_or_else(|| "scheduleId is required".to_string())?;
    let status = arg_text(&args, "status")
        .map(|s| s.to_ascii_uppercase())
        .ok_or_else(|| "status is required".to_string())?;
    if !matches!(
        status.as_str(),
        "SCHEDULED" | "RUNNING" | "PAUSED" | "DONE" | "CANCELLED" | "FAILED"
    ) {
        return Err("status must be SCHEDULED, RUNNING, PAUSED, DONE, CANCELLED, or FAILED".into());
    }
    let note = arg_text(&args, "note");
    let response = mutate_ops_schedule_records(|records| {
        let idx = records
            .iter()
            .position(|record| {
                record.get("id").and_then(Value::as_str) == Some(schedule_id.as_str())
            })
            .ok_or_else(|| format!("ops schedule not found: {schedule_id}"))?;
        let now = Utc::now().to_rfc3339();
        let record = records
            .get_mut(idx)
            .ok_or_else(|| "matched ops schedule disappeared".to_string())?;
        record["status"] = json!(status);
        record["updated_at"] = json!(now);
        if let Some(note) = note {
            let entry = json!({
                "at": Utc::now().to_rfc3339(),
                "note": note,
            });
            if let Some(notes) = record.get_mut("notes").and_then(Value::as_array_mut) {
                notes.push(entry);
            } else if let Some(obj) = record.as_object_mut() {
                obj.insert("notes".into(), json!([entry]));
            }
        }
        Ok((record.clone(), true))
    })?;
    Ok(json!({
        "updated": true,
        "schedule": response,
        "boundary": "Only Sirin-local schedule metadata was updated."
    }))
}

pub(crate) async fn call_ops_schedule_run_due(args: Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 10) as usize;
    let schedule_id = arg_text(&args, "schedule_id").or_else(|| arg_text(&args, "scheduleId"));

    let mut results = Vec::new();
    let mut executed_task_types = HashSet::new();
    let mut schedule_snapshot = None;
    for _ in 0..limit {
        let claim = claim_next_due_schedule(schedule_id.as_deref())?;
        let Some(task) = claim.task else {
            schedule_snapshot = claim.schedule;
            break;
        };
        let schedule_id = task
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("OPS-UNKNOWN")
            .to_string();
        let task_type = task
            .get("task_type")
            .and_then(Value::as_str)
            .unwrap_or("AGORA_MARKET_ISSUE_SCOUT")
            .to_string();
        executed_task_types.insert(task_type.clone());
        let mut params = task
            .get("params")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        params["sourceScheduleId"] = json!(schedule_id.clone());

        let task_id = format!("{schedule_id}:{}", Utc::now().timestamp());
        let outcome =
            crate::a2a_worker::execute_local_ops_task(task_id.clone(), task_type.clone(), params)
                .await;
        let finished_at = Utc::now().to_rfc3339();
        match outcome {
            Ok(result) => {
                let candidate_count = result
                    .get("candidate_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let result_status = ops_result_status(&result);
                finish_claimed_schedule(
                    &schedule_id,
                    "DONE",
                    &finished_at,
                    Some(&result),
                    None,
                    candidate_count,
                )?;
                results.push(json!({
                    "schedule_id": schedule_id,
                    "task_id": task_id,
                    "task_type": task_type,
                    "status": "DONE",
                    "result_status": result_status,
                    "candidate_count": candidate_count,
                    "summary": result.get("summary").cloned().unwrap_or(Value::Null),
                }));
            }
            Err(error) => {
                finish_claimed_schedule(
                    &schedule_id,
                    "FAILED",
                    &finished_at,
                    None,
                    Some(&error),
                    0,
                )?;
                results.push(json!({
                    "schedule_id": schedule_id,
                    "task_id": task_id,
                    "task_type": task_type,
                    "status": "FAILED",
                    "error": error,
                }));
            }
        }
    }

    if results.is_empty() {
        return Ok(json!({
            "ran": 0,
            "results": [],
            "schedule": schedule_snapshot.unwrap_or(Value::Null),
            "storage_path": ops_schedule_path(),
            "boundary": if schedule_id.is_some() {
                "Specified Sirin-local ops schedule was not due or was already handled."
            } else {
                "No due Sirin-local ops schedule was executed."
            }
        }));
    }

    let external_issue_sync = auto_sync_external_issues_after_run_due(executed_task_types);

    Ok(json!({
        "ran": results.len(),
        "results": results,
        "externalIssueSync": external_issue_sync,
        "storage_path": ops_schedule_path(),
        "boundary": "Executed Sirin-local ops schedules only; no AgoraMarketAPI mutation was performed."
    }))
}

fn is_due(record: &Value) -> bool {
    let Some(raw) = record.get("scheduled_for").and_then(Value::as_str) else {
        return true;
    };
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc) <= Utc::now())
        .unwrap_or(true)
}

fn claim_next_due_schedule(schedule_id: Option<&str>) -> Result<OpsScheduleClaim, String> {
    mutate_ops_schedule_records(|records| {
        let due_idx = records.iter().position(|record| {
            schedule_id.is_none_or(|expected| {
                record
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual == expected)
            }) && record
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status.eq_ignore_ascii_case("SCHEDULED"))
                && is_due(record)
        });

        let Some(idx) = due_idx else {
            let schedule = schedule_id.and_then(|expected| {
                records
                    .iter()
                    .find(|record| {
                        record
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|actual| actual == expected)
                    })
                    .cloned()
            });
            return Ok((
                OpsScheduleClaim {
                    task: None,
                    schedule,
                },
                false,
            ));
        };

        let run_started_at = Utc::now().to_rfc3339();
        set_schedule_running(&mut records[idx], &run_started_at);
        Ok((
            OpsScheduleClaim {
                task: Some(records[idx].clone()),
                schedule: None,
            },
            true,
        ))
    })
}

fn finish_claimed_schedule(
    schedule_id: &str,
    status: &str,
    finished_at: &str,
    result: Option<&Value>,
    error: Option<&str>,
    candidate_count: u64,
) -> Result<(), String> {
    mutate_ops_schedule_records(|records| {
        let record = records
            .iter_mut()
            .find(|record| record.get("id").and_then(Value::as_str) == Some(schedule_id))
            .ok_or_else(|| format!("claimed ops schedule disappeared: {schedule_id}"))?;
        let current_status = record
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN");
        if !current_status.eq_ignore_ascii_case("RUNNING") {
            return Err(format!(
                "claimed ops schedule {schedule_id} changed to {current_status}; result was not applied"
            ));
        }
        set_schedule_finished(record, status, finished_at, result, error, candidate_count);
        Ok(((), true))
    })
}

fn set_schedule_running(record: &mut Value, now: &str) {
    record["status"] = json!("RUNNING");
    record["updated_at"] = json!(now);
    record["last_run_at"] = json!(now);
}

fn set_schedule_finished(
    record: &mut Value,
    status: &str,
    now: &str,
    result: Option<&Value>,
    error: Option<&str>,
    candidate_count: u64,
) {
    record["updated_at"] = json!(now);
    record["last_finished_at"] = json!(now);
    record["last_run_status"] = json!(status);
    record["last_candidate_count"] = json!(candidate_count);
    record["run_count"] = json!(record.get("run_count").and_then(Value::as_u64).unwrap_or(0) + 1);
    record["last_error"] = error
        .map(|message| Value::String(message.to_string()))
        .unwrap_or(Value::Null);
    record["last_result_summary"] = result
        .and_then(|value| value.get("summary"))
        .cloned()
        .unwrap_or(Value::Null);
    record["last_result_status"] = result
        .map(ops_result_status)
        .map(Value::String)
        .unwrap_or(Value::Null);
    if let Some(value) = result {
        record["last_result"] = value.clone();
    }
    if status == "DONE" {
        match record
            .get("cadence")
            .and_then(Value::as_str)
            .unwrap_or("manual")
        {
            "daily" => {
                let next = (Utc::now() + Duration::days(1)).to_rfc3339();
                record["status"] = json!("SCHEDULED");
                record["scheduled_for"] = json!(next);
                record["next_run_at"] = record["scheduled_for"].clone();
            }
            "weekly" => {
                let next = (Utc::now() + Duration::weeks(1)).to_rfc3339();
                record["status"] = json!("SCHEDULED");
                record["scheduled_for"] = json!(next);
                record["next_run_at"] = record["scheduled_for"].clone();
            }
            _ => {
                record["status"] = json!("DONE");
            }
        }
    } else {
        record["status"] = json!(status);
    }
}

fn ops_result_status(result: &Value) -> String {
    result
        .pointer("/ops_report/reportStatus")
        .or_else(|| result.get("reportStatus"))
        .and_then(Value::as_str)
        .unwrap_or("COMPLETED")
        .to_string()
}

#[derive(Clone, Debug, Default)]
struct OpsReviewQueues {
    pending_issue_candidates: u64,
    pending_codex_review_actions: u64,
    ready_handoff_items: u64,
    pending_operator_approval_requests: u64,
    pending_codex_review_summary: Vec<Value>,
    ready_handoff_summary: Vec<Value>,
    pending_operator_approval_summary: Vec<Value>,
    handoff_status_summary: Value,
    recently_drained_handoff_summary: Vec<Value>,
    script_health_summary: Value,
}

fn current_ops_review_queues() -> Result<OpsReviewQueues, String> {
    let issue_candidates = crate::a2a_worker::call_a2a_issue_candidates(json!({
        "status": "CANDIDATE",
        "limit": 100
    }))?;
    let codex = crate::ops_action_ledger::call_ops_review_queue_list(json!({
        "status": "PENDING",
        "limit": 200
    }))?;
    let handoffs = crate::a2a_worker::call_a2a_handoff_queue(json!({
        "status": "READY",
        "limit": 200
    }))?;
    let all_handoffs = crate::a2a_worker::call_a2a_handoff_queue(json!({
        "status": "ALL",
        "limit": 200
    }))?;
    let approval_requests = crate::a2a_worker::call_a2a_handoff_approval_requests(json!({
        "status": "ALL",
        "limit": 200
    }))?;
    let script_health_summary = match crate::mcp_server::call_script_health(json!({
        "suite": "active-smoke",
        "window_days": 14
    })) {
        Ok(value) => summarize_script_health(&value),
        Err(e) => json!({
            "status": "unavailable",
            "suite": "active-smoke",
            "windowDays": 14,
            "needsAttention": true,
            "error": e,
        }),
    };
    Ok(OpsReviewQueues {
        pending_issue_candidates: issue_candidates
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        pending_codex_review_actions: codex.get("count").and_then(Value::as_u64).unwrap_or(0),
        ready_handoff_items: handoffs.get("count").and_then(Value::as_u64).unwrap_or(0),
        pending_operator_approval_requests: count_approval_requests_with_status(
            &approval_requests,
            "PENDING_OPERATOR_APPROVAL",
        ),
        pending_codex_review_summary: summarize_pending_codex_reviews(&codex, 5),
        ready_handoff_summary: summarize_ready_handoffs_with_approval_requests(
            &handoffs,
            &approval_requests,
            5,
        ),
        pending_operator_approval_summary: summarize_pending_approval_requests(
            &approval_requests,
            5,
        ),
        handoff_status_summary: summarize_handoff_status_counts(&all_handoffs),
        recently_drained_handoff_summary: summarize_drained_handoffs(&all_handoffs, 5),
        script_health_summary,
    })
}

fn count_approval_requests_with_status(approval_requests: &Value, status: &str) -> u64 {
    approval_requests
        .get("requests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|request| {
            request
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|actual| actual.eq_ignore_ascii_case(status))
        })
        .count() as u64
}

fn summarize_pending_codex_reviews(codex: &Value, limit: usize) -> Vec<Value> {
    codex
        .get("actions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|request| {
                    request
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| {
                            status.eq_ignore_ascii_case("PENDING_OPERATOR_APPROVAL")
                        })
                })
                .take(limit)
                .map(|action| {
                    json!({
                        "id": action.get("id").cloned().unwrap_or(Value::Null),
                        "title": action.get("title").cloned().unwrap_or(Value::Null),
                        "action": action.get("action").cloned().unwrap_or(Value::Null),
                        "actor": action.get("actor").cloned().unwrap_or(Value::Null),
                        "status": action.get("status").cloned().unwrap_or(Value::Null),
                        "targetProject": action.get("targetProject").cloned().unwrap_or(Value::Null),
                        "sourceScheduleId": action.get("sourceScheduleId").cloned().unwrap_or(Value::Null),
                        "sourceTaskId": action.get("sourceTaskId").cloned().unwrap_or(Value::Null),
                        "expectedResult": action.get("expectedResult").cloned().unwrap_or(Value::Null),
                        "verification": action.get("verification").cloned().unwrap_or(Value::Null),
                        "boundary": action.get("boundary").cloned().unwrap_or(Value::Null),
                        "updatedAt": action.get("updatedAt").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn summarize_ready_handoffs_with_approval_requests(
    handoffs: &Value,
    approval_requests: &Value,
    limit: usize,
) -> Vec<Value> {
    let approval_by_handoff: HashMap<String, Value> = approval_requests
        .get("requests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|request| {
            let handoff_id = request.get("handoffId").and_then(Value::as_str)?;
            Some((handoff_id.to_string(), request.clone()))
        })
        .collect();
    handoffs
        .get("handoffs")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(limit)
                .map(|handoff| {
                    let candidate = handoff.get("candidate").unwrap_or(&Value::Null);
                    let operator_review = handoff.get("operator_review").unwrap_or(&Value::Null);
                    let execution_package =
                        handoff.get("executionPackage").unwrap_or(&Value::Null);
                    let approval_request = handoff
                        .get("handoff_id")
                        .and_then(Value::as_str)
                        .and_then(|handoff_id| approval_by_handoff.get(handoff_id));
                    json!({
                        "handoffId": handoff.get("handoff_id").cloned().unwrap_or(Value::Null),
                        "dedupeKey": handoff.get("dedupe_key").cloned().unwrap_or(Value::Null),
                        "title": handoff.get("title").cloned().unwrap_or(Value::Null),
                        "targetActor": handoff.get("target_actor").cloned().unwrap_or(Value::Null),
                        "status": handoff.get("status").cloned().unwrap_or(Value::Null),
                        "approvalRequestId": approval_request
                            .and_then(|request| request.get("requestId"))
                            .cloned()
                            .unwrap_or(Value::Null),
                        "approvalRequestStatus": approval_request
                            .and_then(|request| request.get("status"))
                            .cloned()
                            .unwrap_or(Value::Null),
                        "approvalRequestedAt": approval_request
                            .and_then(|request| request.get("createdAt"))
                            .cloned()
                            .unwrap_or(Value::Null),
                        "allowedAction": candidate.get("allowedAction").cloned().unwrap_or(Value::Null),
                        "candidateType": candidate.get("candidateType").cloned().unwrap_or(Value::Null),
                        "evidenceLevel": candidate
                            .get("evidenceLevel")
                            .or_else(|| candidate.pointer("/opsEvent/evidenceLevel"))
                            .cloned()
                            .unwrap_or(Value::Null),
                        "productIssueGate": candidate
                            .pointer("/opsEvent/productIssueGate/reason")
                            .cloned()
                            .unwrap_or(Value::Null),
                        "operatorReview": if operator_review.is_object() {
                            json!({
                                "reviewer": operator_review.get("reviewer").cloned().unwrap_or(Value::Null),
                                "decision": operator_review.get("decision").cloned().unwrap_or(Value::Null),
                                "approvedBoundary": operator_review.get("approvedBoundary").cloned().unwrap_or(Value::Null),
                                "approvedActions": operator_review.get("approvedActions").cloned().unwrap_or_else(|| json!([])),
                                "blockedMutations": operator_review.get("blockedMutations").cloned().unwrap_or_else(|| json!([])),
                                "mutationApproved": operator_review.get("mutationApproved").cloned().unwrap_or(Value::Null),
                                "reviewedAt": operator_review.get("at").cloned().unwrap_or(Value::Null),
                                "schemaVersion": operator_review.get("schemaVersion").cloned().unwrap_or(Value::Null),
                            })
                        } else {
                            Value::Null
                        },
                        "executionPackage": if execution_package.is_object() {
                            json!({
                                "status": execution_package.get("status").cloned().unwrap_or(Value::Null),
                                "targetActor": execution_package.get("targetActor").cloned().unwrap_or(Value::Null),
                                "allowedAction": execution_package.get("allowedAction").cloned().unwrap_or(Value::Null),
                                "boundary": execution_package.get("boundary").cloned().unwrap_or(Value::Null),
                                "createdAt": execution_package.get("createdAt").cloned().unwrap_or(Value::Null),
                                "updatedAt": execution_package.get("updatedAt").cloned().unwrap_or(Value::Null),
                            })
                        } else {
                            Value::Null
                        },
                        "updatedAt": handoff.get("updated_at").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn summarize_ready_handoffs(handoffs: &Value, limit: usize) -> Vec<Value> {
    summarize_ready_handoffs_with_approval_requests(handoffs, &json!({"requests": []}), limit)
}

fn summarize_pending_approval_requests(approval_requests: &Value, limit: usize) -> Vec<Value> {
    approval_requests
        .get("requests")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(limit)
                .map(|request| {
                    json!({
                        "requestId": request.get("requestId").cloned().unwrap_or(Value::Null),
                        "handoffId": request.get("handoffId").cloned().unwrap_or(Value::Null),
                        "status": request.get("status").cloned().unwrap_or(Value::Null),
                        "title": request.get("title").cloned().unwrap_or(Value::Null),
                        "targetActor": request.get("targetActor").cloned().unwrap_or(Value::Null),
                        "track": request.get("track").cloned().unwrap_or(Value::Null),
                        "approvalDecision": request.get("approvalDecision").cloned().unwrap_or(Value::Null),
                        "requestedBy": request.get("requestedBy").cloned().unwrap_or(Value::Null),
                        "createdAt": request.get("createdAt").cloned().unwrap_or(Value::Null),
                        "updatedAt": request.get("updatedAt").cloned().unwrap_or(Value::Null),
                        "boundary": request.get("boundary").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn summarize_handoff_status_counts(handoffs: &Value) -> Value {
    let mut ready = 0u64;
    let mut acked = 0u64;
    let mut done = 0u64;
    let mut cancelled = 0u64;
    let mut other = 0u64;
    let mut ready_execution_packages = 0u64;
    for handoff in handoffs
        .get("handoffs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if handoff
            .pointer("/executionPackage/status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.starts_with("READY_FOR_"))
        {
            ready_execution_packages += 1;
        }
        match handoff
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase()
            .as_str()
        {
            "READY" => ready += 1,
            "ACKED" => acked += 1,
            "DONE" => done += 1,
            "CANCELLED" => cancelled += 1,
            _ => other += 1,
        }
    }
    json!({
        "ready": ready,
        "acked": acked,
        "done": done,
        "cancelled": cancelled,
        "other": other,
        "drained": acked + done + cancelled,
        "readyExecutionPackages": ready_execution_packages,
        "total": ready + acked + done + cancelled + other,
    })
}

fn summarize_drained_handoffs(handoffs: &Value, limit: usize) -> Vec<Value> {
    summarize_ready_handoffs(handoffs, 200)
        .into_iter()
        .filter(|handoff| {
            handoff
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| {
                    matches!(
                        status.to_ascii_uppercase().as_str(),
                        "ACKED" | "DONE" | "CANCELLED"
                    )
                })
        })
        .take(limit)
        .collect()
}

fn summarize_script_health(script_health: &Value) -> Value {
    let aggregate = script_health.get("aggregate").unwrap_or(&Value::Null);
    let failed = aggregate.get("failed").and_then(Value::as_u64).unwrap_or(0);
    let passed_via_llm = aggregate
        .get("passed_via_llm")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let drift = aggregate.get("drift").and_then(Value::as_f64);
    let drift_warning_threshold = script_health
        .get("drift_warning_threshold")
        .and_then(Value::as_f64)
        .unwrap_or(-0.10);
    let drift_warning = drift.is_some_and(|value| value < drift_warning_threshold);
    let needs_attention = failed > 0 || passed_via_llm > 0 || drift_warning;
    let status = if drift_warning {
        "attention_replay_drift"
    } else if failed > 0 {
        "attention_failed_or_untested"
    } else if passed_via_llm > 0 {
        "attention_llm_fallback"
    } else {
        "healthy_deterministic"
    };
    let stale_or_nondeterministic_tests: Vec<Value> = script_health
        .get("per_test")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| {
                            matches!(status, "stale_fallback" | "llm_only" | "untested")
                        })
                })
                .take(5)
                .map(|item| {
                    json!({
                        "testId": item.get("test_id").cloned().unwrap_or(Value::Null),
                        "status": item.get("status").cloned().unwrap_or(Value::Null),
                        "hasScript": item.get("has_script").cloned().unwrap_or(Value::Null),
                        "tags": item.get("tags").cloned().unwrap_or_else(|| json!([])),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "status": status,
        "suite": script_health.get("suite").cloned().unwrap_or_else(|| json!("active-smoke")),
        "windowDays": aggregate.get("window_days").cloned().unwrap_or_else(|| json!(14)),
        "totalTests": aggregate.get("total_runs").cloned().unwrap_or(Value::Null),
        "passedViaReplay": aggregate.get("passed_via_replay").cloned().unwrap_or(Value::Null),
        "passedViaFixtureOnly": aggregate.get("passed_via_fixture_only").cloned().unwrap_or(Value::Null),
        "passedViaLlm": aggregate.get("passed_via_llm").cloned().unwrap_or(Value::Null),
        "failed": aggregate.get("failed").cloned().unwrap_or(Value::Null),
        "successRateReplay": aggregate.get("success_rate_replay").cloned().unwrap_or(Value::Null),
        "successRateDeterministic": aggregate.get("success_rate_deterministic").cloned().unwrap_or(Value::Null),
        "drift": aggregate.get("drift").cloned().unwrap_or(Value::Null),
        "driftWarningThreshold": drift_warning_threshold,
        "needsAttention": needs_attention,
        "staleOrNonDeterministicTests": stale_or_nondeterministic_tests,
    })
}

fn operations_action_queue_from_digest(
    progress: &Value,
    review_queues: &OpsReviewQueues,
) -> (Value, Vec<Value>) {
    let remaining_orders = progress
        .get("remainingProductionOrdersToTarget")
        .and_then(Value::as_u64);
    let pending_shipments = progress
        .get("pendingShipmentCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let repeatable_target_met = remaining_orders.is_some_and(|remaining| remaining == 0);
    let fulfillment_handoffs =
        handoff_summary_by_candidate_type(review_queues, "FULFILLMENT_FOLLOWUP");
    let growth_handoffs = handoff_summary_by_candidate_type(review_queues, "GROWTH_GAP");
    let fulfillment_pending_approvals = handoffs_with_pending_approval(&fulfillment_handoffs);
    let growth_pending_approvals = handoffs_with_pending_approval(&growth_handoffs);
    let checkout_codex_reviews =
        codex_summary_matching(review_queues, &["checkout", "cart", "buyer path"]);
    let script_needs_attention = review_queues
        .script_health_summary
        .get("needsAttention")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut next_action_queue = Vec::new();
    if pending_shipments > 0 || !fulfillment_handoffs.is_empty() {
        let has_pending_approval = !fulfillment_pending_approvals.is_empty();
        next_action_queue.push(json!({
            "priority": "P0",
            "track": "fulfillment_followup",
            "actor": "CHATGPT",
            "status": if has_pending_approval { "PENDING_OPERATOR_APPROVAL" } else { "READY_FOR_APPROVAL_REQUEST" },
            "executionGate": {
                "canAutoExecute": false,
                "requiredApproval": "OPERATOR",
                "allowedAction": "HANDOFF_REVIEW_ONLY",
                "approvalAlreadyRequested": has_pending_approval,
                "pendingApprovalRequestCount": fulfillment_pending_approvals.len(),
                "blockedMutations": ["customer_message", "seller_message", "order_update", "payment_action"],
                "reason": "Pending fulfillment follow-up can affect customers, sellers, and orders."
            },
            "evidence": {
                "pendingShipmentCount": pending_shipments,
                "readyHandoffs": fulfillment_handoffs
            },
            "action": "Review pending shipments and prepare an operator-approved seller/customer follow-up."
        }));
    }
    if !checkout_codex_reviews.is_empty() {
        next_action_queue.push(json!({
            "priority": "P0",
            "track": "checkout_cart_verification",
            "actor": "CODEX",
            "status": "PENDING_CODEX_REVIEW",
            "executionGate": {
                "canAutoExecute": false,
                "requiredApproval": "CODEX_REVIEW",
                "allowedAction": "READ_ONLY_VERIFICATION",
                "blockedMutations": ["checkout_submit", "payment_confirmation", "cart_mutation", "order_creation"],
                "reason": "Checkout/cart dry-runs must stop before payment confirmation and wait for external blocker changes."
            },
            "evidence": checkout_codex_reviews,
            "action": "Re-verify checkout/cart blockers read-only and rerun dry-run only after the external blocker changes."
        }));
    }
    if !growth_handoffs.is_empty() {
        let has_pending_approval = !growth_pending_approvals.is_empty();
        next_action_queue.push(json!({
            "priority": "P1",
            "track": "support_growth_review",
            "actor": "CHATGPT",
            "status": if has_pending_approval { "PENDING_OPERATOR_APPROVAL" } else { "READY_FOR_APPROVAL_REQUEST" },
            "executionGate": {
                "canAutoExecute": false,
                "requiredApproval": "OPERATOR",
                "allowedAction": "HANDOFF_REVIEW_ONLY",
                "approvalAlreadyRequested": has_pending_approval,
                "pendingApprovalRequestCount": growth_pending_approvals.len(),
                "blockedMutations": ["customer_message", "public_faq_publish", "product_issue_creation"],
                "reason": "Growth and support signals are hypotheses until reviewed."
            },
            "evidence": growth_handoffs,
            "action": "Turn support and CTA gaps into an operator-reviewed FAQ or conversion handoff."
        }));
    }
    if script_needs_attention {
        next_action_queue.push(json!({
            "priority": "P1",
            "track": "test_replay_health",
            "actor": "SIRIN",
            "status": "NEEDS_SCRIPT_HEALTH_REVIEW",
            "executionGate": {
                "canAutoExecute": true,
                "requiredApproval": "NONE",
                "allowedAction": "SIRIN_LOCAL_TEST_HEALTH_REVIEW",
                "blockedMutations": ["agoramarket_code_change", "agoramarketapi_code_change"],
                "reason": "Script-health review is Sirin-local and read-only for target projects."
            },
            "evidence": review_queues.script_health_summary,
            "action": "Refresh stale deterministic coverage before expanding repeatable-order automation."
        }));
    }
    if !repeatable_target_met {
        next_action_queue.push(json!({
            "priority": "P1",
            "track": "repeatable_order_gap",
            "actor": "SIRIN",
            "status": "ACTIVE_MONITORING",
            "executionGate": {
                "canAutoExecute": true,
                "requiredApproval": "NONE",
                "allowedAction": "SIRIN_LOCAL_MONITORING",
                "blockedMutations": ["order_update", "payment_action", "customer_message", "seller_message", "agoramarket_code_change", "agoramarketapi_code_change"],
                "reason": "Sirin may continue local monitoring and issue/handoff preparation without mutating production commerce state."
            },
            "evidence": progress,
            "action": "Keep ranking checkout, fulfillment, support, supplier, and acquisition blockers until the first five production orders are reached."
        }));
    }

    let next_focus = next_action_queue
        .first()
        .and_then(|item| item.get("track"))
        .cloned()
        .unwrap_or_else(|| json!("monitoring"));
    let operator_approval_required =
        !fulfillment_handoffs.is_empty() || !growth_handoffs.is_empty() || pending_shipments > 0;
    let auto_executable_count = next_action_queue
        .iter()
        .filter(|item| {
            item.pointer("/executionGate/canAutoExecute")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();

    let readiness = json!({
        "repeatableOrderTargetMet": repeatable_target_met,
        "remainingProductionOrdersToTarget": remaining_orders.map(Value::from).unwrap_or(Value::Null),
        "pendingShipmentCount": pending_shipments,
        "fulfillmentFollowupReady": !fulfillment_handoffs.is_empty(),
        "growthSupportReviewReady": !growth_handoffs.is_empty(),
        "checkoutCodexReviewPending": !checkout_codex_reviews.is_empty(),
        "scriptHealthNeedsAttention": script_needs_attention,
        "operatorApprovalRequired": operator_approval_required,
        "pendingOperatorApprovalRequests": review_queues.pending_operator_approval_requests,
        "autoExecutableActionCount": auto_executable_count,
        "approvalGatedActionCount": next_action_queue.len().saturating_sub(auto_executable_count),
        "nextFocus": next_focus,
    });
    (readiness, next_action_queue)
}

fn handoff_summary_by_candidate_type(
    review_queues: &OpsReviewQueues,
    candidate_type: &str,
) -> Vec<Value> {
    review_queues
        .ready_handoff_summary
        .iter()
        .filter(|item| {
            item.get("candidateType")
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(candidate_type))
        })
        .cloned()
        .collect()
}

fn handoffs_with_pending_approval(handoffs: &[Value]) -> Vec<Value> {
    handoffs
        .iter()
        .filter(|item| {
            item.get("approvalRequestStatus")
                .and_then(Value::as_str)
                .is_some_and(|status| status.eq_ignore_ascii_case("PENDING_OPERATOR_APPROVAL"))
        })
        .cloned()
        .collect()
}

fn codex_summary_matching(review_queues: &OpsReviewQueues, needles: &[&str]) -> Vec<Value> {
    review_queues
        .pending_codex_review_summary
        .iter()
        .filter(|item| {
            let text = [
                item.get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                item.get("action")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                item.get("verification")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ]
            .join(" ")
            .to_ascii_lowercase();
            needles
                .iter()
                .any(|needle| text.contains(&needle.to_ascii_lowercase()))
        })
        .cloned()
        .collect()
}

fn review_queue_pressure_from_digest(review_queues: &OpsReviewQueues) -> Value {
    let chatgpt_ready = review_queues
        .ready_handoff_summary
        .iter()
        .filter(|item| {
            item.get("targetActor")
                .and_then(Value::as_str)
                .is_some_and(|actor| actor.eq_ignore_ascii_case("CHATGPT"))
        })
        .count();
    let codex_ready = review_queues
        .ready_handoff_summary
        .iter()
        .filter(|item| {
            item.get("targetActor")
                .and_then(Value::as_str)
                .is_some_and(|actor| actor.eq_ignore_ascii_case("CODEX"))
        })
        .count();
    let total_open_review_items = review_queues.pending_issue_candidates
        + review_queues.pending_codex_review_actions
        + review_queues.ready_handoff_items;
    let drain_before_next_scout = total_open_review_items > 0;
    let recommended_drain_order = if review_queues.ready_handoff_items > 0 {
        "ready_handoffs"
    } else if review_queues.pending_codex_review_actions > 0 {
        "pending_codex_review"
    } else if review_queues.pending_issue_candidates > 0 {
        "issue_candidates"
    } else {
        "none"
    };

    json!({
        "totalOpenReviewItems": total_open_review_items,
        "readyHandoffItems": review_queues.ready_handoff_items,
        "pendingOperatorApprovalRequests": review_queues.pending_operator_approval_requests,
        "pendingCodexReviewActions": review_queues.pending_codex_review_actions,
        "pendingIssueCandidates": review_queues.pending_issue_candidates,
        "readyHandoffsByActor": {
            "chatgpt": chatgpt_ready,
            "codex": codex_ready,
            "other": review_queues
                .ready_handoff_summary
                .len()
                .saturating_sub(chatgpt_ready + codex_ready),
        },
        "firstReadyHandoff": review_queues
            .ready_handoff_summary
            .first()
            .cloned()
            .unwrap_or(Value::Null),
        "firstPendingOperatorApproval": review_queues
            .pending_operator_approval_summary
            .first()
            .cloned()
            .unwrap_or(Value::Null),
        "firstCodexReview": review_queues
            .pending_codex_review_summary
            .first()
            .cloned()
            .unwrap_or(Value::Null),
        "drainBeforeNextScout": drain_before_next_scout,
        "recommendedDrainOrder": recommended_drain_order,
        "reason": if drain_before_next_scout {
            "Resolve ready handoffs, Codex review actions, or issue candidates before creating more ops noise."
        } else {
            "No active review queue pressure; the next scheduled ops scout can continue normally."
        }
    })
}

fn handoff_track(candidate_type: &str) -> &'static str {
    match candidate_type {
        "FULFILLMENT_FOLLOWUP" => "fulfillment_followup",
        "GROWTH_GAP" => "support_growth_review",
        _ => "handoff_review",
    }
}

fn handoff_blocked_mutations(candidate_type: &str) -> Vec<&'static str> {
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

fn review_drain_plan_from_digest(review_queues: &OpsReviewQueues) -> Value {
    let mut steps = Vec::new();
    let mut order = 1usize;
    let mut handoffs = review_queues.ready_handoff_summary.clone();
    handoffs.sort_by_key(|item| {
        match item
            .get("candidateType")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "FULFILLMENT_FOLLOWUP" => 0,
            "GROWTH_GAP" => 1,
            _ => 2,
        }
    });

    for handoff in handoffs {
        let candidate_type = handoff
            .get("candidateType")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN");
        let blocked_mutations = handoff_blocked_mutations(candidate_type);
        let handoff_id = handoff.get("handoffId").cloned().unwrap_or(Value::Null);
        let title = handoff.get("title").cloned().unwrap_or(Value::Null);
        let approval_request_status = handoff
            .get("approvalRequestStatus")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let approval_already_requested =
            approval_request_status.eq_ignore_ascii_case("PENDING_OPERATOR_APPROVAL");
        let approval_satisfied = approval_request_status.eq_ignore_ascii_case("APPROVED");
        let approval_closed_without_ack = matches!(
            approval_request_status.to_ascii_uppercase().as_str(),
            "REJECTED" | "CANCELLED"
        );
        let local_tool = if approval_satisfied {
            "a2a_update_handoff"
        } else if approval_already_requested || approval_closed_without_ack {
            "a2a_handoff_approval_requests"
        } else {
            "a2a_request_handoff_approval"
        };
        steps.push(json!({
            "order": order,
            "queue": "a2a_handoff_queue",
            "itemId": handoff_id,
            "title": title,
            "track": handoff_track(candidate_type),
            "actor": handoff.get("targetActor").cloned().unwrap_or(Value::Null),
            "localTool": local_tool,
            "allowedAction": handoff.get("allowedAction").cloned().unwrap_or(Value::Null),
            "statusTransition": if approval_satisfied {
                "APPROVED -> ACKED via a2a_update_handoff, creating a local executionPackage only"
            } else if approval_already_requested {
                "PENDING_OPERATOR_APPROVAL -> ACKED, DONE, or CANCELLED only after operator approval"
            } else if approval_closed_without_ack {
                "REJECTED/CANCELLED -> no ACK unless a new approval request is created and approved"
            } else {
                "READY -> PENDING_OPERATOR_APPROVAL via a2a_request_handoff_approval, then ACKED only after operator approval"
            },
            "canAutoExecute": approval_satisfied,
            "requiredApproval": if approval_satisfied { "APPROVED_OPERATOR_REQUEST" } else { "OPERATOR" },
            "approvalAlreadyRequested": approval_already_requested,
            "approvalSatisfied": approval_satisfied,
            "approvalClosedWithoutAck": approval_closed_without_ack,
            "approvalRequestId": handoff.get("approvalRequestId").cloned().unwrap_or(Value::Null),
            "approvalRequestStatus": handoff.get("approvalRequestStatus").cloned().unwrap_or(Value::Null),
            "blockedMutations": blocked_mutations.clone(),
            "toolCallTemplate": {
                "requestApproval": {
                    "name": "a2a_request_handoff_approval",
                    "arguments": {
                        "handoffId": handoff.get("handoffId").cloned().unwrap_or(Value::Null),
                        "requestedBy": "sirin",
                        "note": "Request operator approval for local handoff ACK package only; no external mutation."
                    }
                },
                "name": "a2a_update_handoff",
                "arguments": {
                    "handoffId": handoff.get("handoffId").cloned().unwrap_or(Value::Null),
                    "status": "ACKED",
                    "note": "operator reviewed; no external mutation executed by Sirin",
                    "reviewer": "<operator-id>",
                    "decision": "ACK_FOR_CHATGPT_REVIEW",
                    "approvedBoundary": "Sirin-local metadata update only; no customer, seller, order, payment, FAQ, product, GitHub, AgoraMarket, or AgoraMarketAPI mutation approved.",
                    "approvedActions": ["sirin_local_metadata_update"],
                    "blockedMutations": blocked_mutations
                }
            },
            "boundary": if approval_satisfied {
                "Operator approval is recorded; Sirin may only ACK local handoff metadata and create a local executionPackage. External mutations remain blocked."
            } else if approval_already_requested {
                "Operator approval has already been requested; Sirin may only wait/list local approval requests until an operator explicitly approves or rejects."
            } else if approval_closed_without_ack {
                "Operator approval request is closed without approval; Sirin must not ACK this handoff unless a new request is approved."
            } else {
                "Sirin may create a local approval request, but may not ACK handoff metadata or approve any message, issue creation, FAQ publish, order update, or payment action."
            }
        }));
        order += 1;
    }

    for action in &review_queues.pending_codex_review_summary {
        steps.push(json!({
            "order": order,
            "queue": "ops_review_queue",
            "itemId": action.get("id").cloned().unwrap_or(Value::Null),
            "title": action.get("title").cloned().unwrap_or(Value::Null),
            "track": "checkout_cart_verification",
            "actor": "CODEX",
            "localTool": "ops_review_claim/ops_review_complete",
            "allowedAction": "READ_ONLY_VERIFICATION",
            "statusTransition": "PENDING_CODEX_REVIEW -> CLAIMED -> VERIFIED, DONE, REJECTED, or NEEDS_MORE_EVIDENCE",
            "canAutoExecute": false,
            "requiredApproval": "CODEX_REVIEW",
            "blockedMutations": [
                "checkout_submit",
                "payment_confirmation",
                "cart_mutation",
                "order_creation",
                "agoramarket_code_change",
                "agoramarketapi_code_change"
            ],
            "toolCallTemplate": {
                "claim": {
                    "name": "ops_review_claim",
                    "arguments": {
                        "actionId": action.get("id").cloned().unwrap_or(Value::Null),
                        "claimedBy": "codex"
                    }
                },
                "complete": {
                    "name": "ops_review_complete",
                    "arguments": {
                        "actionId": action.get("id").cloned().unwrap_or(Value::Null),
                        "status": "VERIFIED",
                        "finding": "read-only verification complete",
                        "recommendedNextAction": "update Sirin-local ops report or open/update external issue only if evidence proves a product/API problem"
                    }
                }
            },
            "boundary": "Codex review is read-only first; AgoraMarket/AgoraMarketAPI code changes or production mutations require explicit approval."
        }));
        order += 1;
    }

    if review_queues.pending_issue_candidates > 0 {
        steps.push(json!({
            "order": order,
            "queue": "a2a_issue_candidates",
            "itemId": Value::Null,
            "title": "Review pending issue candidates",
            "track": "issue_candidate_triage",
            "actor": "CODEX",
            "localTool": "a2a_review_issue_candidate",
            "allowedAction": "ISSUE_OR_HANDOFF_REVIEW_ONLY",
            "statusTransition": "CANDIDATE -> ACCEPTED, REJECTED, NEEDS_MORE_EVIDENCE, or CONVERT_TO_ISSUE when allowed",
            "canAutoExecute": false,
            "requiredApproval": "CODEX_REVIEW",
            "blockedMutations": [
                "external_issue_creation_without_review",
                "agoramarket_code_change",
                "agoramarketapi_code_change",
                "production_mutation"
            ],
            "toolCallTemplate": {
                "name": "a2a_review_issue_candidate",
                "arguments": {
                    "dedupeKey": "<candidate dedupeKey>",
                    "reviewStatus": "NEEDS_MORE_EVIDENCE",
                    "reviewNote": "read-only triage result"
                }
            },
            "boundary": "Issue candidates must be reviewed locally before any GitHub issue or external handoff is created."
        }));
    }

    let status = if steps.is_empty() {
        "clear"
    } else {
        "drain_required"
    };
    let next_step = steps.first().cloned().unwrap_or(Value::Null);

    json!({
        "status": status,
        "totalItems": steps.len(),
        "nextStep": next_step,
        "steps": steps,
        "boundary": "Sirin-local drain plan only; it does not invoke ChatGPT, Codex, GitHub, AgoraMarket, AgoraMarketAPI, order, payment, product, customer, or seller mutation."
    })
}

fn review_drain_progress_from_plan(
    review_drain_plan: &Value,
    review_queues: &OpsReviewQueues,
) -> Value {
    let steps = review_drain_plan
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total_items = steps.len();
    let auto_executable_count = steps
        .iter()
        .filter(|step| {
            step.get("canAutoExecute")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    let operator_approval_count = steps
        .iter()
        .filter(|step| {
            step.get("requiredApproval")
                .and_then(Value::as_str)
                .is_some_and(|approval| approval.eq_ignore_ascii_case("OPERATOR"))
        })
        .count();
    let codex_review_count = steps
        .iter()
        .filter(|step| {
            step.get("requiredApproval")
                .and_then(Value::as_str)
                .is_some_and(|approval| approval.eq_ignore_ascii_case("CODEX_REVIEW"))
        })
        .count();
    let first_pending = steps.first().cloned().unwrap_or(Value::Null);
    let next_required_approval = first_pending
        .get("requiredApproval")
        .and_then(Value::as_str)
        .unwrap_or("NONE");
    let next_local_tool = first_pending
        .get("localTool")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let can_advance_without_approval = total_items == 0 || auto_executable_count > 0;
    let drained_handoff_items = review_queues
        .handoff_status_summary
        .get("drained")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let known_handoff_items = review_queues
        .handoff_status_summary
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let reviewed_drained_handoff_count = review_queues
        .recently_drained_handoff_summary
        .iter()
        .filter(|handoff| {
            let review = handoff.get("operatorReview").unwrap_or(&Value::Null);
            review
                .get("reviewer")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
                && review
                    .get("decision")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                && review
                    .get("mutationApproved")
                    .and_then(Value::as_bool)
                    .is_some_and(|approved| !approved)
        })
        .count();
    let has_review_evidence_for_recent_drains =
        drained_handoff_items == 0 || reviewed_drained_handoff_count > 0;
    let legacy_drained_handoff_items =
        drained_handoff_items.saturating_sub(reviewed_drained_handoff_count);
    let structured_review_coverage = if drained_handoff_items == 0 {
        1.0
    } else {
        reviewed_drained_handoff_count as f64 / drained_handoff_items as f64
    };
    let review_evidence_status = if drained_handoff_items == 0 {
        "no_drained_handoffs"
    } else if reviewed_drained_handoff_count == 0 {
        "legacy_gap"
    } else if reviewed_drained_handoff_count < drained_handoff_items {
        "partial_structured_review"
    } else {
        "structured_review_complete"
    };
    let denominator = total_items + drained_handoff_items;
    let drain_completion_ratio = if denominator == 0 {
        1.0
    } else {
        drained_handoff_items as f64 / denominator as f64
    };
    let status = if total_items == 0 {
        if drained_handoff_items > 0 {
            "drained"
        } else {
            "clear"
        }
    } else if can_advance_without_approval {
        "local_progress_available"
    } else {
        "waiting_for_review"
    };

    json!({
        "status": status,
        "totalItems": total_items,
        "openItems": total_items,
        "autoExecutableItems": auto_executable_count,
        "approvalGatedItems": total_items.saturating_sub(auto_executable_count),
        "operatorApprovalItems": operator_approval_count,
        "codexReviewItems": codex_review_count,
        "drainedHandoffItems": drained_handoff_items,
        "knownHandoffItems": known_handoff_items,
        "reviewedDrainedHandoffItems": reviewed_drained_handoff_count,
        "legacyDrainedHandoffItems": legacy_drained_handoff_items,
        "structuredReviewCoverage": structured_review_coverage,
        "reviewEvidenceStatus": review_evidence_status,
        "hasReviewEvidenceForRecentDrains": has_review_evidence_for_recent_drains,
        "handoffStatusSummary": review_queues.handoff_status_summary.clone(),
        "recentlyDrainedHandoffs": review_queues.recently_drained_handoff_summary.clone(),
        "canAdvanceWithoutApproval": can_advance_without_approval,
        "nextRequiredApproval": next_required_approval,
        "nextLocalTool": next_local_tool,
        "nextStep": first_pending,
        "drainCompletionRatio": drain_completion_ratio,
        "recommendation": if total_items == 0 {
            if drained_handoff_items > 0 {
                "Ready handoffs have been drained locally; Sirin may continue scheduled repeatable-order monitoring after checking Codex reviews and issue candidates."
            } else {
                "Review queue is clear; Sirin may continue scheduled repeatable-order monitoring."
            }
        } else if can_advance_without_approval {
            "Run the next Sirin-local metadata-only drain step before creating more ops noise."
        } else if review_evidence_status == "legacy_gap" {
            "Current queue needs review, and legacy drained handoffs lack structured review evidence; future handoff updates should use structured reviewer, decision, approvedBoundary, and blockedMutations fields."
        } else {
            "Wait for the required operator or Codex review before mutating local queue status."
        },
        "boundary": "Progress is computed from Sirin-local queue metadata only; no external action was executed."
    })
}

fn operator_review_readiness_from_report(
    review_drain_plan: &Value,
    review_drain_progress: &Value,
) -> Value {
    let next_step = review_drain_plan.get("nextStep").unwrap_or(&Value::Null);
    if next_step.is_null() {
        return json!({
            "status": "not_required",
            "ready": true,
            "nextTrack": Value::Null,
            "nextLocalTool": Value::Null,
            "nextRequiredApproval": "NONE",
            "handoffId": Value::Null,
            "template": Value::Null,
            "reviewEvidenceStatus": review_drain_progress.get("reviewEvidenceStatus").cloned().unwrap_or(Value::Null),
            "checks": [],
            "boundary": "Readiness only; Sirin did not mutate queue state or call external services."
        });
    }

    let next_required_approval = next_step
        .get("requiredApproval")
        .and_then(Value::as_str)
        .unwrap_or("NONE");
    if !next_required_approval.eq_ignore_ascii_case("OPERATOR") {
        return json!({
            "status": "not_required",
            "ready": true,
            "nextTrack": next_step.get("track").cloned().unwrap_or(Value::Null),
            "nextLocalTool": next_step.get("localTool").cloned().unwrap_or(Value::Null),
            "nextRequiredApproval": next_required_approval,
            "handoffId": next_step.get("itemId").cloned().unwrap_or(Value::Null),
            "approvalAlreadyRequested": next_step.get("approvalAlreadyRequested").cloned().unwrap_or(Value::Bool(false)),
            "approvalRequestId": next_step.get("approvalRequestId").cloned().unwrap_or(Value::Null),
            "approvalRequestStatus": next_step.get("approvalRequestStatus").cloned().unwrap_or(Value::Null),
            "template": next_step.get("toolCallTemplate").cloned().unwrap_or(Value::Null),
            "reviewEvidenceStatus": review_drain_progress.get("reviewEvidenceStatus").cloned().unwrap_or(Value::Null),
            "checks": [],
            "boundary": "Readiness only; Sirin did not mutate queue state or call external services."
        });
    }

    let args = next_step
        .pointer("/toolCallTemplate/arguments")
        .unwrap_or(&Value::Null);
    let approved_boundary = args
        .get("approvedBoundary")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let has_template = next_step
        .pointer("/toolCallTemplate/name")
        .and_then(Value::as_str)
        .is_some_and(|name| name == "a2a_update_handoff");
    let has_reviewer_placeholder = args
        .get("reviewer")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let has_decision = args
        .get("decision")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let has_boundary = approved_boundary.contains("metadata update only");
    let has_approved_actions = args
        .get("approvedActions")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    let has_blocked_mutations = args
        .get("blockedMutations")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    let external_mutation_blocked = approved_boundary.contains("no customer")
        && approved_boundary.contains("AgoraMarketAPI")
        && approved_boundary.contains("mutation approved");
    let checks = vec![
        json!({"name": "template_present", "passed": has_template}),
        json!({"name": "requires_operator", "passed": true}),
        json!({"name": "reviewer_placeholder", "passed": has_reviewer_placeholder}),
        json!({"name": "decision_present", "passed": has_decision}),
        json!({"name": "boundary_metadata_only", "passed": has_boundary}),
        json!({"name": "approved_actions_present", "passed": has_approved_actions}),
        json!({"name": "blocked_mutations_present", "passed": has_blocked_mutations}),
        json!({"name": "external_mutation_blocked", "passed": external_mutation_blocked}),
    ];
    let ready = checks.iter().all(|check| {
        check
            .get("passed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    });

    json!({
        "status": if ready { "ready_for_operator_review" } else { "not_ready" },
        "ready": ready,
        "nextTrack": next_step.get("track").cloned().unwrap_or(Value::Null),
        "nextLocalTool": next_step.get("localTool").cloned().unwrap_or(Value::Null),
        "nextRequiredApproval": next_required_approval,
        "handoffId": next_step.get("itemId").cloned().unwrap_or(Value::Null),
        "approvalAlreadyRequested": next_step.get("approvalAlreadyRequested").cloned().unwrap_or(Value::Bool(false)),
        "approvalRequestId": next_step.get("approvalRequestId").cloned().unwrap_or(Value::Null),
        "approvalRequestStatus": next_step.get("approvalRequestStatus").cloned().unwrap_or(Value::Null),
        "template": next_step.get("toolCallTemplate").cloned().unwrap_or(Value::Null),
        "reviewEvidenceStatus": review_drain_progress.get("reviewEvidenceStatus").cloned().unwrap_or(Value::Null),
        "checks": checks,
        "boundary": "Readiness only; Sirin did not mutate queue state or call external services."
    })
}

fn codex_review_readiness_from_report(
    review_drain_plan: &Value,
    review_drain_progress: &Value,
) -> Value {
    let codex_step = review_drain_plan
        .get("steps")
        .and_then(Value::as_array)
        .and_then(|steps| {
            steps.iter().find(|step| {
                step.get("requiredApproval")
                    .and_then(Value::as_str)
                    .is_some_and(|approval| approval.eq_ignore_ascii_case("CODEX_REVIEW"))
            })
        });
    let Some(step) = codex_step else {
        return json!({
            "status": "not_required",
            "ready": true,
            "nextTrack": Value::Null,
            "nextLocalTool": Value::Null,
            "actionId": Value::Null,
            "claimTemplate": Value::Null,
            "completeTemplate": Value::Null,
            "reviewEvidenceStatus": review_drain_progress.get("reviewEvidenceStatus").cloned().unwrap_or(Value::Null),
            "checks": [],
            "boundary": "Readiness only; Sirin did not claim, complete, or mutate any review action."
        });
    };

    let claim_template = step
        .pointer("/toolCallTemplate/claim")
        .unwrap_or(&Value::Null);
    let complete_template = step
        .pointer("/toolCallTemplate/complete")
        .unwrap_or(&Value::Null);
    let claim_action_id = claim_template.pointer("/arguments/actionId");
    let complete_action_id = complete_template.pointer("/arguments/actionId");
    let item_id = step.get("itemId").unwrap_or(&Value::Null);
    let boundary = step
        .get("boundary")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let has_claim_template = claim_template
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name == "ops_review_claim");
    let has_complete_template = complete_template
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name == "ops_review_complete");
    let action_ids_match = claim_action_id.is_some()
        && complete_action_id.is_some()
        && claim_action_id == complete_action_id
        && claim_action_id == Some(item_id);
    let has_read_only_action = step
        .get("allowedAction")
        .and_then(Value::as_str)
        .is_some_and(|action| action == "READ_ONLY_VERIFICATION");
    let has_blocked_mutations = step
        .get("blockedMutations")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| item == "payment_confirmation")
                && items.iter().any(|item| item == "agoramarket_code_change")
                && items
                    .iter()
                    .any(|item| item == "agoramarketapi_code_change")
        });
    let external_mutation_blocked =
        boundary.contains("read-only") && boundary.contains("require explicit approval");
    let completion_requires_result = complete_template
        .pointer("/arguments/recommendedNextAction")
        .and_then(Value::as_str)
        .is_some_and(|value| value.contains("evidence proves"));
    let checks = vec![
        json!({"name": "claim_template_present", "passed": has_claim_template}),
        json!({"name": "complete_template_present", "passed": has_complete_template}),
        json!({"name": "action_ids_match", "passed": action_ids_match}),
        json!({"name": "read_only_action", "passed": has_read_only_action}),
        json!({"name": "blocked_mutations_present", "passed": has_blocked_mutations}),
        json!({"name": "external_mutation_blocked", "passed": external_mutation_blocked}),
        json!({"name": "completion_requires_evidence", "passed": completion_requires_result}),
    ];
    let ready = checks.iter().all(|check| {
        check
            .get("passed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    });

    json!({
        "status": if ready { "ready_for_codex_review" } else { "not_ready" },
        "ready": ready,
        "nextTrack": step.get("track").cloned().unwrap_or(Value::Null),
        "nextLocalTool": step.get("localTool").cloned().unwrap_or(Value::Null),
        "actionId": item_id.clone(),
        "claimTemplate": claim_template.clone(),
        "completeTemplate": complete_template.clone(),
        "reviewEvidenceStatus": review_drain_progress.get("reviewEvidenceStatus").cloned().unwrap_or(Value::Null),
        "checks": checks,
        "boundary": "Readiness only; Sirin did not claim, complete, or mutate any review action."
    })
}

fn review_execution_matrix_from_report(review_drain_plan: &Value) -> Value {
    let steps = review_drain_plan
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut operator_required = Vec::new();
    let mut codex_reviewable = Vec::new();
    let mut chatgpt_handoff_only = Vec::new();
    let mut local_auto_executable = Vec::new();

    for step in &steps {
        let required_approval = step
            .get("requiredApproval")
            .and_then(Value::as_str)
            .unwrap_or("NONE");
        let actor = step
            .get("actor")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let can_auto_execute = step
            .get("canAutoExecute")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let summary = json!({
            "queue": step.get("queue").cloned().unwrap_or(Value::Null),
            "itemId": step.get("itemId").cloned().unwrap_or(Value::Null),
            "title": step.get("title").cloned().unwrap_or(Value::Null),
            "track": step.get("track").cloned().unwrap_or(Value::Null),
            "actor": step.get("actor").cloned().unwrap_or(Value::Null),
            "localTool": step.get("localTool").cloned().unwrap_or(Value::Null),
            "allowedAction": step.get("allowedAction").cloned().unwrap_or(Value::Null),
            "requiredApproval": required_approval,
            "canAutoExecute": can_auto_execute,
            "approvalAlreadyRequested": step.get("approvalAlreadyRequested").cloned().unwrap_or(Value::Bool(false)),
            "approvalRequestId": step.get("approvalRequestId").cloned().unwrap_or(Value::Null),
            "approvalRequestStatus": step.get("approvalRequestStatus").cloned().unwrap_or(Value::Null),
            "blockedMutations": step.get("blockedMutations").cloned().unwrap_or_else(|| json!([])),
            "toolCallTemplate": step.get("toolCallTemplate").cloned().unwrap_or(Value::Null),
            "boundary": step.get("boundary").cloned().unwrap_or(Value::Null),
        });

        if can_auto_execute {
            local_auto_executable.push(summary.clone());
        }
        if required_approval.eq_ignore_ascii_case("OPERATOR") {
            operator_required.push(summary.clone());
            if actor.eq_ignore_ascii_case("CHATGPT") {
                chatgpt_handoff_only.push(summary.clone());
            }
        } else if required_approval.eq_ignore_ascii_case("CODEX_REVIEW") {
            codex_reviewable.push(summary.clone());
        }
    }

    let next_recommended_lane = if !operator_required.is_empty() {
        "operator_approval_required"
    } else if !codex_reviewable.is_empty() {
        "codex_reviewable"
    } else if !local_auto_executable.is_empty() {
        "sirin_local_auto_executable"
    } else {
        "clear"
    };
    let next_item = match next_recommended_lane {
        "operator_approval_required" => operator_required.first(),
        "codex_reviewable" => codex_reviewable.first(),
        "sirin_local_auto_executable" => local_auto_executable.first(),
        _ => None,
    }
    .cloned()
    .unwrap_or(Value::Null);

    json!({
        "status": if steps.is_empty() { "clear" } else { "classified" },
        "totalOpenItems": steps.len(),
        "operatorApprovalRequiredCount": operator_required.len(),
        "codexReviewableCount": codex_reviewable.len(),
        "chatgptHandoffOnlyCount": chatgpt_handoff_only.len(),
        "sirinLocalAutoExecutableCount": local_auto_executable.len(),
        "nextRecommendedLane": next_recommended_lane,
        "nextItem": next_item,
        "operatorApprovalRequired": operator_required,
        "codexReviewable": codex_reviewable,
        "chatgptHandoffOnly": chatgpt_handoff_only,
        "sirinLocalAutoExecutable": local_auto_executable,
        "boundary": "Classification only; Sirin did not update handoff, review, issue, product, order, payment, customer, seller, GitHub, AgoraMarket, or AgoraMarketAPI state."
    })
}

fn operator_approval_packets_from_matrix(review_execution_matrix: &Value) -> Value {
    let items = review_execution_matrix
        .get("operatorApprovalRequired")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let packets: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let track = item
                .get("track")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let handoff_id = item.get("itemId").cloned().unwrap_or(Value::Null);
            let title = item.get("title").cloned().unwrap_or(Value::Null);
            let blocked_mutations = item
                .get("blockedMutations")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let chatgpt_prompt = match track {
                "fulfillment_followup" => {
                    "Review the pending-shipment evidence and draft an operator-approved seller/customer follow-up plan. Do not send messages, mutate orders, change payment state, or contact users; return only recommended wording, escalation criteria, and evidence needed for approval."
                }
                "support_growth_review" => {
                    "Review the buyer onboarding/support knowledge gaps and draft FAQ/CTA copy for operator approval. Do not publish FAQ content, create product issues, message customers, or change AgoraMarket/AgoraMarketAPI; return only draft copy, target surfaces, and validation criteria."
                }
                _ => {
                    "Review this Sirin handoff and prepare a recommendation for operator approval. Do not mutate external systems; return only a structured review note and required evidence."
                }
            };
            json!({
                "order": index + 1,
                "handoffId": handoff_id,
                "title": title,
                "track": track,
                "targetActor": item.get("actor").cloned().unwrap_or(Value::Null),
                "approvalDecision": "ACK_FOR_CHATGPT_REVIEW",
                "approvalAlreadyRequested": item.get("approvalAlreadyRequested").cloned().unwrap_or(Value::Bool(false)),
                "approvalRequestId": item.get("approvalRequestId").cloned().unwrap_or(Value::Null),
                "approvalRequestStatus": item.get("approvalRequestStatus").cloned().unwrap_or(Value::Null),
                "localTool": item.get("localTool").cloned().unwrap_or(Value::Null),
                "toolCallTemplate": item.get("toolCallTemplate").cloned().unwrap_or(Value::Null),
                "chatgptPromptDraft": chatgpt_prompt,
                "operatorChecklist": [
                    "Confirm the handoff is still relevant to repeatable orders or trust/conversion.",
                    "Confirm the approval is only for Sirin-local metadata ACK.",
                    "Confirm no customer/seller/order/payment/product/GitHub/AgoraMarket/AgoraMarketAPI mutation is approved.",
                    "If approved, run the provided a2a_update_handoff template with a real reviewer id."
                ],
                "blockedMutations": blocked_mutations,
                "boundary": "Operator packet only; Sirin did not approve, ACK, message, publish, create issues, update orders, touch payment, or call ChatGPT."
            })
        })
        .collect();
    let pending_request_count = packets
        .iter()
        .filter(|packet| {
            packet
                .get("approvalRequestStatus")
                .and_then(Value::as_str)
                .is_some_and(|status| status.eq_ignore_ascii_case("PENDING_OPERATOR_APPROVAL"))
        })
        .count();

    json!({
        "status": if packets.is_empty() { "clear" } else { "ready_for_operator" },
        "packetCount": packets.len(),
        "pendingRequestCount": pending_request_count,
        "nextPacket": packets.first().cloned().unwrap_or(Value::Null),
        "packets": packets,
        "boundary": "Approval packets are local review material only; execution requires explicit operator action."
    })
}

fn handoff_review_inbox_from_report(
    review_drain_progress: &Value,
    operator_approval_packets: &Value,
    review_execution_matrix: &Value,
) -> Value {
    let mut items = Vec::new();
    let mut ready_for_review = 0usize;
    let mut approved_local_ack = 0usize;
    let mut approval_required = 0usize;
    let mut legacy_ack_missing_execution_package = 0usize;

    for handoff in review_drain_progress
        .get("recentlyDrainedHandoffs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if handoff
            .pointer("/executionPackage/status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.starts_with("READY_FOR_"))
        {
            ready_for_review += 1;
            items.push(json!({
                "status": "READY_FOR_REVIEW",
                "handoffId": handoff.get("handoffId").cloned().unwrap_or(Value::Null),
                "title": handoff.get("title").cloned().unwrap_or(Value::Null),
                "targetActor": handoff.pointer("/executionPackage/targetActor").cloned().unwrap_or(Value::Null),
                "reviewStatus": handoff.pointer("/executionPackage/status").cloned().unwrap_or(Value::Null),
                "allowedAction": handoff.pointer("/executionPackage/allowedAction").cloned().unwrap_or(Value::Null),
                "executionPackage": handoff.get("executionPackage").cloned().unwrap_or(Value::Null),
                "nextAction": "chatgpt_or_codex_review_local_package",
                "boundary": "Local review package only; no customer/seller/order/payment/product/GitHub/AgoraMarket/AgoraMarketAPI mutation is approved."
            }));
        } else if handoff
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("ACKED"))
        {
            legacy_ack_missing_execution_package += 1;
            items.push(json!({
                "status": "LEGACY_ACK_MISSING_EXECUTION_PACKAGE",
                "handoffId": handoff.get("handoffId").cloned().unwrap_or(Value::Null),
                "title": handoff.get("title").cloned().unwrap_or(Value::Null),
                "targetActor": handoff.get("targetActor").cloned().unwrap_or(Value::Null),
                "hasOperatorReview": handoff.get("operatorReview").is_some_and(Value::is_object),
                "nextAction": "backfill_possible_only_if_structured_operator_review_exists",
                "boundary": "Legacy ACK metadata only; Sirin did not infer or fabricate approval evidence."
            }));
        }
    }

    for item in review_execution_matrix
        .get("sirinLocalAutoExecutable")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let approved_local_only = item
            .get("requiredApproval")
            .and_then(Value::as_str)
            .is_some_and(|approval| approval.eq_ignore_ascii_case("APPROVED_OPERATOR_REQUEST"))
            && item
                .get("approvalRequestStatus")
                .and_then(Value::as_str)
                .is_some_and(|status| status.eq_ignore_ascii_case("APPROVED"));
        if !approved_local_only {
            continue;
        }
        approved_local_ack += 1;
        items.push(json!({
            "status": "APPROVAL_SATISFIED_LOCAL_ACK",
            "handoffId": item.get("itemId").cloned().unwrap_or(Value::Null),
            "title": item.get("title").cloned().unwrap_or(Value::Null),
            "targetActor": item.get("actor").cloned().unwrap_or(Value::Null),
            "track": item.get("track").cloned().unwrap_or(Value::Null),
            "approvalRequestId": item.get("approvalRequestId").cloned().unwrap_or(Value::Null),
            "approvalRequestStatus": item.get("approvalRequestStatus").cloned().unwrap_or(Value::Null),
            "localTool": item.get("localTool").cloned().unwrap_or(Value::Null),
            "toolCallTemplate": item.get("toolCallTemplate").cloned().unwrap_or(Value::Null),
            "nextAction": "operator_approved_local_ack_allowed",
            "boundary": "Operator approval permits only the provided Sirin-local ACK metadata update; no external actor call or marketplace mutation is approved."
        }));
    }

    for packet in operator_approval_packets
        .get("packets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let approval_pending = packet
            .get("approvalRequestStatus")
            .and_then(Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("PENDING_OPERATOR_APPROVAL"));
        let approval_approved = packet
            .get("approvalRequestStatus")
            .and_then(Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("APPROVED"));
        let approval_closed = packet
            .get("approvalRequestStatus")
            .and_then(Value::as_str)
            .is_some_and(|status| {
                matches!(
                    status.to_ascii_uppercase().as_str(),
                    "REJECTED" | "CANCELLED"
                )
            });
        approval_required += 1;
        items.push(json!({
            "status": "APPROVAL_REQUIRED",
            "handoffId": packet.get("handoffId").cloned().unwrap_or(Value::Null),
            "title": packet.get("title").cloned().unwrap_or(Value::Null),
            "targetActor": packet.get("targetActor").cloned().unwrap_or(Value::Null),
            "track": packet.get("track").cloned().unwrap_or(Value::Null),
            "approvalRequestId": packet.get("approvalRequestId").cloned().unwrap_or(Value::Null),
            "approvalRequestStatus": packet.get("approvalRequestStatus").cloned().unwrap_or(Value::Null),
            "approvalPacket": packet,
            "nextAction": if approval_approved {
                "operator_approved_local_ack_allowed"
            } else if approval_pending {
                "wait_for_operator_approval"
            } else if approval_closed {
                "approval_request_closed_do_not_ack_handoff"
            } else {
                "request_operator_approval_before_chatgpt_or_codex_review"
            },
            "boundary": "Approval required; Sirin did not ACK or call any external actor."
        }));
    }

    items.sort_by_key(handoff_review_report_item_sort_key);

    let next_gate = if ready_for_review > 0 {
        "chatgpt_or_codex_review"
    } else if approved_local_ack > 0 {
        "sirin_local_ack"
    } else if approval_required > 0 {
        "operator_approval"
    } else if legacy_ack_missing_execution_package > 0 {
        "legacy_ack_audit"
    } else {
        "clear"
    };

    json!({
        "status": if items.is_empty() { "clear" } else { "open" },
        "count": items.len(),
        "readyForReview": ready_for_review,
        "approvedLocalAck": approved_local_ack,
        "approvalRequired": approval_required,
        "legacyAckMissingExecutionPackage": legacy_ack_missing_execution_package,
        "nextGate": next_gate,
        "nextItem": items.first().cloned().unwrap_or(Value::Null),
        "items": items,
        "boundary": "Report-only handoff review inbox; Sirin did not ACK handoffs, call ChatGPT/Codex/GitHub, or mutate AgoraMarket/AgoraMarketAPI."
    })
}

fn handoff_review_report_item_sort_key(item: &Value) -> usize {
    match item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "READY_FOR_REVIEW" => 0,
        "APPROVAL_SATISFIED_LOCAL_ACK" => 1,
        "APPROVAL_REQUIRED" => {
            let status = item
                .get("approvalRequestStatus")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_uppercase();
            match status.as_str() {
                "APPROVED" => 2,
                "PENDING_OPERATOR_APPROVAL" => 3,
                "REJECTED" | "CANCELLED" => 5,
                _ => 4,
            }
        }
        "LEGACY_ACK_MISSING_EXECUTION_PACKAGE" => 6,
        _ => 9,
    }
}

fn operations_gate_status_from_report(
    schedule_continuity: &Value,
    review_execution_matrix: &Value,
    operator_approval_packets: &Value,
    codex_review_readiness: &Value,
    cycle_completion: &Value,
) -> Value {
    let achieved = cycle_completion
        .get("achieved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let local_schedule_can_continue = schedule_continuity
        .get("localScheduleCanContinue")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let operator_count = review_execution_matrix
        .get("operatorApprovalRequiredCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let codex_count = review_execution_matrix
        .get("codexReviewableCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let local_auto_count = review_execution_matrix
        .get("sirinLocalAutoExecutableCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let packet_count = operator_approval_packets
        .get("packetCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let pending_request_count = operator_approval_packets
        .get("pendingRequestCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let codex_ready = codex_review_readiness
        .get("ready")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (status, next_gate, recommendation) = if achieved {
        (
            "goal_complete_review",
            "operator_cycle_close_review",
            "Review the completed cycle before archiving first-five operations work.",
        )
    } else if local_auto_count > 0 {
        (
            "ready_for_local_approved_handoff_ack",
            "sirin_local_ack",
            "Operator approval is recorded; Sirin may ACK the local handoff metadata and create the local executionPackage without external mutation.",
        )
    } else if operator_count > 0 {
        if pending_request_count > 0 {
            (
                "pending_operator_approval_request",
                "operator_approval",
                "Operator approval has already been requested; wait for explicit approval or rejection before ACKing the local handoff.",
            )
        } else {
            (
                "waiting_for_operator_approval",
                "operator_approval",
                "Review operatorApprovalPackets and explicitly approve or reject local handoff ACK before ChatGPT work proceeds.",
            )
        }
    } else if codex_count > 0 && codex_ready {
        (
            "ready_for_codex_review",
            "codex_review",
            "Claim and complete the Sirin-local Codex review item before creating more scouting noise.",
        )
    } else if local_schedule_can_continue {
        (
            "scheduled_patrol_active",
            "next_scheduled_patrol",
            "Wait for the next Sirin-local scheduled patrol or run it manually if a heartbeat is requested.",
        )
    } else {
        (
            "schedule_missing",
            "create_or_resume_schedule",
            "Create or resume a Sirin-local repeatable-order schedule.",
        )
    };

    json!({
        "status": status,
        "nextGate": next_gate,
        "localScheduleActive": local_schedule_can_continue,
        "operatorApprovalRequiredCount": operator_count,
        "operatorApprovalPacketCount": packet_count,
        "pendingOperatorApprovalRequestCount": pending_request_count,
        "codexReviewableCount": codex_count,
        "sirinLocalAutoExecutableCount": local_auto_count,
        "codexReviewReady": codex_ready,
        "nextOperatorPacket": operator_approval_packets.get("nextPacket").cloned().unwrap_or(Value::Null),
        "recommendation": recommendation,
        "boundary": "Gate status is read-only local coordination state; Sirin did not approve, ACK, call ChatGPT/Codex, or mutate external commerce systems."
    })
}

fn conversion_evidence_from_progress(progress: &Value) -> Value {
    json!({
        "goal": progress.get("goal").cloned().unwrap_or(Value::Null),
        "productionOrders30d": progress.get("productionOrders30d").cloned().unwrap_or(Value::Null),
        "targetProductionOrders": progress.get("targetProductionOrders").cloned().unwrap_or(Value::Null),
        "remainingProductionOrdersToTarget": progress
            .get("remainingProductionOrdersToTarget")
            .cloned()
            .unwrap_or(Value::Null),
        "productionGmv30d": progress.get("productionGmv30d").cloned().unwrap_or(Value::Null),
        "pendingShipmentCount": progress.get("pendingShipmentCount").cloned().unwrap_or(Value::Null),
        "sourceScheduleId": progress.get("sourceScheduleId").cloned().unwrap_or(Value::Null),
        "sourceFinishedAt": progress.get("sourceFinishedAt").cloned().unwrap_or(Value::Null),
        "evidenceLevel": if progress.is_null() {
            "missing"
        } else {
            "observed_from_latest_first_five_schedule"
        },
    })
}

fn cycle_exit_criteria_from_progress(progress: &Value) -> Value {
    let target = progress
        .get("targetProductionOrders")
        .and_then(Value::as_u64)
        .unwrap_or(5);
    json!({
        "goal": "FIRST_FIVE_PRODUCTION_ORDERS",
        "requiredProductionOrders30d": target,
        "requiresExternalMutationApproval": true,
        "requiresDryRunStopBeforePaymentConfirmation": true,
        "requiresSirinLocalScheduleContinuity": true,
        "requiresNoBlockingCheckoutCartIssue": true,
        "completionRule": "productionOrders30d >= requiredProductionOrders30d and no P0 checkout/cart or fulfillment blocker remains open in the Sirin-local ops report."
    })
}

fn cycle_completion_from_report(
    progress: &Value,
    operational_readiness: &Value,
    next_action_queue: &[Value],
) -> Value {
    let production_orders = progress
        .get("productionOrders30d")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let target = progress
        .get("targetProductionOrders")
        .and_then(Value::as_u64)
        .unwrap_or(5);
    let remaining = target.saturating_sub(production_orders);
    let has_p0_actions = next_action_queue.iter().any(|item| {
        item.get("priority")
            .and_then(Value::as_str)
            .is_some_and(|priority| priority == "P0")
    });
    let local_schedule_can_continue = progress
        .pointer("/localSchedule/canContinue")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let achieved = production_orders >= target && !has_p0_actions && local_schedule_can_continue;
    let mut missing = Vec::new();
    if production_orders < target {
        missing.push(json!({
            "requirement": "Reach first five production orders",
            "current": production_orders,
            "target": target,
            "remaining": remaining,
        }));
    }
    if has_p0_actions {
        missing.push(json!({
            "requirement": "Clear P0 operational blockers",
            "currentP0Tracks": next_action_queue
                .iter()
                .filter(|item| {
                    item.get("priority")
                        .and_then(Value::as_str)
                        .is_some_and(|priority| priority == "P0")
                })
                .filter_map(|item| item.get("track").cloned())
                .collect::<Vec<_>>(),
        }));
    }
    if !local_schedule_can_continue {
        missing.push(json!({
            "requirement": "Keep Sirin-local ops schedule continuity",
            "current": operational_readiness
                .get("nextFocus")
                .cloned()
                .unwrap_or(Value::Null),
        }));
    }

    json!({
        "achieved": achieved,
        "status": if achieved { "complete" } else { "in_progress" },
        "productionOrders30d": production_orders,
        "targetProductionOrders": target,
        "remainingProductionOrdersToTarget": remaining,
        "p0ActionCount": next_action_queue
            .iter()
            .filter(|item| {
                item.get("priority")
                    .and_then(Value::as_str)
                    .is_some_and(|priority| priority == "P0")
            })
            .count(),
        "missingRequirements": missing,
    })
}

fn schedule_continuity_from_report(
    scheduled: usize,
    recommended_next_schedule: &str,
    local_schedule_can_continue: bool,
    review_queue_pressure: &Value,
    cycle_completion: &Value,
) -> Value {
    let drain_before_next_scout = review_queue_pressure
        .get("drainBeforeNextScout")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let achieved = cycle_completion
        .get("achieved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let runner_mode = if achieved {
        "goal_complete_review"
    } else if drain_before_next_scout {
        "drain_review_queue"
    } else if local_schedule_can_continue {
        "continue_scheduled_patrol"
    } else {
        "create_or_resume_schedule"
    };
    let can_continue_without_external_mutation =
        !achieved && local_schedule_can_continue && !drain_before_next_scout;

    json!({
        "runnerMode": runner_mode,
        "scheduledSchedules": scheduled,
        "localScheduleCanContinue": local_schedule_can_continue,
        "drainBeforeNextScout": drain_before_next_scout,
        "canContinueWithoutExternalMutation": can_continue_without_external_mutation,
        "recommendedNextSchedule": recommended_next_schedule,
        "nextRunnerAction": match runner_mode {
            "goal_complete_review" => "Stop creating first-five activation work and archive the cycle after operator review.",
            "drain_review_queue" => "Process ready handoffs, Codex reviews, or issue candidates before running another scout.",
            "continue_scheduled_patrol" => "Wait for the next due Sirin-local scheduled patrol.",
            _ => "Create or resume a Sirin-local daily first-five operations schedule.",
        },
        "boundary": "Sirin may schedule/read local ops state and prepare handoffs only; production commerce mutations require approval."
    })
}

fn capability_gap_plan_from_progress(
    progress: &Value,
    external_issue_state_sync: &Value,
) -> Vec<Value> {
    progress
        .get("sirinCapabilityGaps")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|gap| {
                    let lower = gap.to_ascii_lowercase();
                    let (capability, priority, next_action, verification) =
                        if lower.contains("issue status") {
                            (
                                "external_issue_state_sync",
                                "P0",
                                "Sync tracked issue state before deciding whether to rerun checkout dry-runs.",
                                "operationsCycleReport can distinguish open, fixed, and unknown external blockers.",
                            )
                        } else if lower.contains("approval-gated")
                            || lower.contains("handoff execution")
                            || lower.contains("contact users")
                            || lower.contains("contact sellers")
                        {
                            (
                                "approval_gated_handoff_execution",
                                "P0",
                                "Add an approval-gated execution path for reviewed customer, seller, and fulfillment follow-ups.",
                                "Sirin can prepare but not send customer/seller/order actions until operator approval is recorded.",
                            )
                        } else if lower.contains("attribution")
                            || lower.contains("source/channel")
                            || lower.contains("traffic")
                        {
                            (
                                "acquisition_attribution",
                                "P1",
                                "Track richer source/channel evidence before recommending traffic actions.",
                                "Repeatable-order reports can explain which source or channel produced production orders.",
                            )
                        } else {
                            (
                                "ops_capability_gap",
                                "P2",
                                "Keep the gap visible in the Sirin-local operations cycle report.",
                                "The gap remains attached to the next operationsCycleReport until resolved or reclassified.",
                            )
                        };
                    let (gap_status, evidence) = if capability == "external_issue_state_sync" {
                        let sync_status = external_issue_state_sync
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let unknown_count = external_issue_state_sync
                            .get("unknownCount")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let tracked_count = external_issue_state_sync
                            .get("trackedIssueCount")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        if tracked_count > 0 && unknown_count == 0 {
                            (
                                "operational",
                                format!(
                                    "externalIssueStateSync status={sync_status}, tracked={tracked_count}, unknown={unknown_count}"
                                ),
                            )
                        } else {
                            (
                                "needs_work",
                                format!(
                                    "externalIssueStateSync status={sync_status}, tracked={tracked_count}, unknown={unknown_count}"
                                ),
                            )
                        }
                    } else {
                        ("needs_work", "capability gap still requires Sirin implementation or operating evidence".to_string())
                    };
                    json!({
                        "capability": capability,
                        "priority": priority,
                        "status": gap_status,
                        "gap": gap,
                        "owner": "SIRIN",
                        "nextAction": next_action,
                        "verification": verification,
                        "evidence": evidence,
                        "boundary": "Sirin-local capability planning only; AgoraMarket/AgoraMarketAPI code changes stay issue/handoff-only unless explicitly approved."
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn external_issue_state_sync_from_progress(progress: &Value) -> Value {
    let tracked = progress
        .get("trackedIssues")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut open_count = 0usize;
    let mut closed_count = 0usize;
    let mut unknown_count = 0usize;
    let mut seen = HashSet::new();
    let mut issues = Vec::new();

    for issue in tracked {
        let key = tracked_issue_value_key(&issue).unwrap_or_else(|| issue.to_string());
        if !seen.insert(key.clone()) {
            continue;
        }
        let state = tracked_issue_state(&issue);
        match state.as_str() {
            "open" => open_count += 1,
            "closed" | "fixed" | "resolved" => closed_count += 1,
            _ => unknown_count += 1,
        }
        let external = issue
            .get("externalIssue")
            .filter(|value| value.is_object())
            .unwrap_or(&issue);
        issues.push(json!({
            "key": key,
            "url": issue
                .as_str()
                .or_else(|| external.get("url").and_then(Value::as_str))
                .or_else(|| external.get("externalIssueUrl").and_then(Value::as_str))
                .unwrap_or(""),
            "project": external
                .get("project")
                .or_else(|| external.get("externalIssueProject"))
                .cloned()
                .unwrap_or(Value::Null),
            "number": external
                .get("number")
                .or_else(|| external.get("externalIssueNumber"))
                .cloned()
                .unwrap_or(Value::Null),
            "state": state,
            "evidenceSource": "sirin_local_tracked_issue_snapshot",
        }));
    }

    let tracked_count = issues.len();
    let status = if tracked_count == 0 {
        "not_configured"
    } else if open_count > 0 {
        "waiting_external_fix"
    } else if unknown_count > 0 {
        "needs_issue_state_sync"
    } else {
        "ready_to_rerun_dry_run"
    };
    let can_rerun = tracked_count > 0 && open_count == 0 && unknown_count == 0;
    let next_action = match status {
        "not_configured" => {
            "Attach tracked external issue metadata before deciding whether to rerun checkout dry-runs."
        }
        "waiting_external_fix" => {
            "Wait for the open external blocker to be fixed or update the issue/handoff with new evidence."
        }
        "needs_issue_state_sync" => {
            "Sync GitHub issue state for tracked blockers before rerunning checkout dry-runs."
        }
        _ => {
            "Rerun the checkout dry-run and stop before payment confirmation."
        }
    };

    json!({
        "status": status,
        "trackedIssueCount": tracked_count,
        "openCount": open_count,
        "closedCount": closed_count,
        "unknownCount": unknown_count,
        "issues": issues,
        "dryRunDecision": {
            "canRerunCheckoutDryRun": can_rerun,
            "reason": if can_rerun {
                "All tracked external blockers have local closed/fixed evidence."
            } else {
                "At least one tracked external blocker is open, unknown, or missing from the local snapshot."
            },
            "requiredBoundary": "Dry-run may only stop at checkout/payment-confirmation view; no order, payment, product, customer, or seller mutation is allowed."
        },
        "nextAction": next_action,
        "boundary": "Read-only Sirin-local issue-state decision support; Sirin did not call GitHub, close issues, or mutate AgoraMarket/AgoraMarketAPI."
    })
}

fn tracked_issue_state(issue: &Value) -> String {
    let external = issue
        .get("externalIssue")
        .filter(|value| value.is_object())
        .unwrap_or(issue);
    let raw = issue
        .get("state")
        .or_else(|| issue.get("status"))
        .or_else(|| issue.get("issueState"))
        .or_else(|| external.get("state"))
        .or_else(|| external.get("status"))
        .or_else(|| external.get("issueState"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .trim()
        .to_ascii_lowercase();

    match raw.as_str() {
        "open" | "opened" | "active" => "open".to_string(),
        "closed" => "closed".to_string(),
        "fixed" => "fixed".to_string(),
        "resolved" | "done" => "resolved".to_string(),
        _ => "unknown".to_string(),
    }
}

fn operator_brief_from_report(
    status: &str,
    conversion_evidence: &Value,
    largest_blocker: &Value,
    operational_readiness: &Value,
    schedule_continuity: &Value,
    next_action_queue: &[Value],
    capability_gap_plan: &[Value],
) -> Value {
    let production_orders = conversion_evidence
        .get("productionOrders30d")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let target_orders = conversion_evidence
        .get("targetProductionOrders")
        .and_then(Value::as_u64)
        .unwrap_or(5);
    let remaining_orders = conversion_evidence
        .get("remainingProductionOrdersToTarget")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| target_orders.saturating_sub(production_orders));
    let gmv = conversion_evidence
        .get("productionGmv30d")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let next_focus = operational_readiness
        .get("nextFocus")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let runner_mode = schedule_continuity
        .get("runnerMode")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let largest_blocker_type = largest_blocker
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let largest_blocker_summary = largest_blocker
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("No blocker summary is available.");
    let next_runner_action = schedule_continuity
        .get("nextRunnerAction")
        .and_then(Value::as_str)
        .unwrap_or("No runner action is available.");
    let first_action = next_action_queue.first().unwrap_or(&Value::Null);
    let first_action_actor = first_action
        .get("actor")
        .and_then(Value::as_str)
        .unwrap_or("SIRIN");
    let first_action_track = first_action
        .get("track")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let first_action_gate = first_action
        .pointer("/executionGate/allowedAction")
        .and_then(Value::as_str)
        .unwrap_or("SIRIN_LOCAL_MONITORING");
    let top_capability_gap = capability_gap_plan
        .iter()
        .find(|item| {
            item.get("status")
                .and_then(Value::as_str)
                .is_none_or(|status| status != "operational")
        })
        .or_else(|| capability_gap_plan.first())
        .and_then(|item| item.get("capability"))
        .and_then(Value::as_str)
        .unwrap_or("none");

    json!({
        "headline": format!(
            "Repeatable-order cycle is {status}: {production_orders}/{target_orders} production orders, {remaining_orders} remaining, {gmv:.4} production GMV in 30d."
        ),
        "largestBlocker": {
            "type": largest_blocker_type,
            "summary": largest_blocker_summary,
        },
        "nextFocus": next_focus,
        "firstAction": {
            "track": first_action_track,
            "actor": first_action_actor,
            "allowedAction": first_action_gate,
        },
        "runner": {
            "mode": runner_mode,
            "action": next_runner_action,
        },
        "topCapabilityGap": top_capability_gap,
        "handoffInstruction": "Sirin may continue local monitoring and prepare handoffs; ChatGPT/Codex/operator gated work must not mutate AgoraMarket, AgoraMarketAPI, orders, payment, or customer/seller messages without approval.",
    })
}

fn report_completeness_from_report(
    conversion_evidence: &Value,
    largest_blocker: &Value,
    handoff_plan: &Value,
    schedule_continuity: &Value,
    capability_gap_plan: &[Value],
    external_issue_state_sync: &Value,
    cycle_completion: &Value,
    review_queue_pressure: &Value,
    review_drain_plan: &Value,
    review_drain_progress: &Value,
    operator_review_readiness: &Value,
    codex_review_readiness: &Value,
    review_execution_matrix: &Value,
    operator_approval_packets: &Value,
    handoff_review_inbox: &Value,
    operations_gate_status: &Value,
    operator_brief: &Value,
) -> Value {
    let check = |name: &str, passed: bool, evidence: &str| {
        json!({
            "name": name,
            "passed": passed,
            "evidence": evidence,
        })
    };

    let has_conversion_evidence = conversion_evidence
        .get("productionOrders30d")
        .is_some_and(|value| !value.is_null())
        && conversion_evidence
            .get("targetProductionOrders")
            .is_some_and(|value| !value.is_null());
    let has_largest_blocker = largest_blocker
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let has_actor_handoff_plan = ["sirin", "chatgpt", "codex", "operator"]
        .iter()
        .all(|key| handoff_plan.get(*key).is_some_and(Value::is_array));
    let has_schedule_continuity = schedule_continuity
        .get("runnerMode")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && schedule_continuity
            .get("localScheduleCanContinue")
            .is_some_and(|value| !value.is_null());
    let has_capability_gap_plan = !capability_gap_plan.is_empty();
    let has_external_issue_state_sync = external_issue_state_sync
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && external_issue_state_sync
            .get("dryRunDecision")
            .is_some_and(|value| value.is_object());
    let has_cycle_completion = cycle_completion
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && cycle_completion
            .get("achieved")
            .is_some_and(|value| !value.is_null());
    let has_review_queue_pressure = review_queue_pressure
        .get("drainBeforeNextScout")
        .is_some_and(|value| !value.is_null())
        && review_queue_pressure
            .get("recommendedDrainOrder")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
    let drain_required = review_queue_pressure
        .get("drainBeforeNextScout")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_review_drain_plan = review_drain_plan
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && (!drain_required
            || review_drain_plan
                .get("steps")
                .and_then(Value::as_array)
                .is_some_and(|steps| !steps.is_empty()));
    let has_review_drain_progress = review_drain_progress
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && review_drain_progress
            .get("openItems")
            .is_some_and(|value| !value.is_null());
    let has_operator_review_readiness = operator_review_readiness
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && operator_review_readiness
            .get("ready")
            .is_some_and(|value| !value.is_null());
    let has_codex_review_readiness = codex_review_readiness
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && codex_review_readiness
            .get("ready")
            .is_some_and(|value| !value.is_null());
    let has_review_execution_matrix = review_execution_matrix
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && review_execution_matrix
            .get("totalOpenItems")
            .is_some_and(|value| !value.is_null());
    let has_operator_approval_packets = operator_approval_packets
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && operator_approval_packets
            .get("packetCount")
            .is_some_and(|value| !value.is_null())
        && operator_approval_packets
            .get("pendingRequestCount")
            .is_some_and(|value| !value.is_null());
    let has_handoff_review_inbox = handoff_review_inbox
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && handoff_review_inbox
            .get("count")
            .is_some_and(|value| !value.is_null())
        && handoff_review_inbox
            .get("nextGate")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
    let has_operations_gate_status = operations_gate_status
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && operations_gate_status
            .get("nextGate")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
    let has_operator_brief = operator_brief
        .get("headline")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && operator_brief
            .get("firstAction")
            .is_some_and(|value| value.is_object());
    let has_mutation_boundary = operator_brief
        .get("handoffInstruction")
        .and_then(Value::as_str)
        .is_some_and(|value| {
            value.contains("must not mutate") && value.contains("without approval")
        });

    let checks = vec![
        check(
            "conversion_evidence",
            has_conversion_evidence,
            "conversionEvidence includes productionOrders30d and targetProductionOrders",
        ),
        check(
            "largest_blocker",
            has_largest_blocker,
            "largestBlocker includes a typed blocker for next-order prioritization",
        ),
        check(
            "actor_handoff_plan",
            has_actor_handoff_plan,
            "handoffPlan separates Sirin, ChatGPT, Codex, and operator work",
        ),
        check(
            "schedule_continuity",
            has_schedule_continuity,
            "scheduleContinuity declares runnerMode and localScheduleCanContinue",
        ),
        check(
            "capability_gap_plan",
            has_capability_gap_plan,
            "capabilityGapPlan lists Sirin-local gaps before repeatable operations are mature",
        ),
        check(
            "external_issue_state_sync",
            has_external_issue_state_sync,
            "externalIssueStateSync declares whether tracked external blockers are open, closed, or still unknown before rerunning dry-runs",
        ),
        check(
            "cycle_completion",
            has_cycle_completion,
            "cycleCompletion declares status and achieved state",
        ),
        check(
            "review_queue_pressure",
            has_review_queue_pressure,
            "reviewQueuePressure declares whether handoffs/reviews should drain before scouting",
        ),
        check(
            "review_drain_plan",
            has_review_drain_plan,
            "reviewDrainPlan maps open queues to Sirin-local tools, approvals, and mutation boundaries",
        ),
        check(
            "review_drain_progress",
            has_review_drain_progress,
            "reviewDrainProgress summarizes queue drain progress and approval blockers",
        ),
        check(
            "operator_review_readiness",
            has_operator_review_readiness,
            "operatorReviewReadiness declares whether the next handoff is ready for human review",
        ),
        check(
            "codex_review_readiness",
            has_codex_review_readiness,
            "codexReviewReadiness declares whether Sirin-initiated review work is ready for Codex",
        ),
        check(
            "review_execution_matrix",
            has_review_execution_matrix,
            "reviewExecutionMatrix classifies open work by operator, Codex, ChatGPT, and Sirin lanes",
        ),
        check(
            "operator_approval_packets",
            has_operator_approval_packets,
            "operatorApprovalPackets prepares review-only handoff material and tracks whether approval was already requested",
        ),
        check(
            "operations_gate_status",
            has_operations_gate_status,
            "operationsGateStatus identifies the current coordination gate without mutating external systems",
        ),
        check(
            "operator_brief",
            has_operator_brief,
            "operatorBrief includes headline and firstAction for human-readable operations review",
        ),
        check(
            "mutation_boundary",
            has_mutation_boundary,
            "operatorBrief handoffInstruction preserves the no-external-mutation boundary",
        ),
        check(
            "handoff_review_inbox",
            has_handoff_review_inbox,
            "handoffReviewInbox classifies ready review packages, approval-required handoffs, and legacy ACK gaps",
        ),
    ];
    let passed_count = checks
        .iter()
        .filter(|item| item.get("passed").and_then(Value::as_bool).unwrap_or(false))
        .count();
    let required_count = checks.len();

    json!({
        "complete": passed_count == required_count,
        "passedCount": passed_count,
        "requiredCount": required_count,
        "checks": checks,
    })
}

fn operations_cycle_report_from_digest(
    status: &str,
    recommended_next_schedule: &str,
    scheduled: usize,
    progress: &Value,
    review_queues: &OpsReviewQueues,
) -> Value {
    let local_schedule_can_continue = scheduled > 0
        || progress
            .pointer("/localSchedule/canContinue")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let chatgpt: Vec<Value> = review_queues
        .ready_handoff_summary
        .iter()
        .filter(|item| {
            item.get("targetActor")
                .and_then(Value::as_str)
                .is_some_and(|actor| actor.eq_ignore_ascii_case("CHATGPT"))
        })
        .cloned()
        .collect();
    let codex = review_queues.pending_codex_review_summary.clone();
    let mut sirin = Vec::new();
    if local_schedule_can_continue {
        sirin.push(json!({
            "action": "Continue the Sirin-local repeatable-order patrol.",
            "verification": "Next digest refreshes order progress, queue status, script health, and blockers."
        }));
    }
    if !progress.is_null() {
        sirin.push(json!({
            "action": "Keep repeatable-order progress visible in ops_schedule_digest.",
            "verification": "repeatableOrderProgress remains populated from the latest FIRST_FIVE run."
        }));
    }
    if review_queues
        .script_health_summary
        .get("needsAttention")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        sirin.push(json!({
            "action": "Review active-smoke script health before expanding repeatable-order automation.",
            "verification": "scriptHealth staleOrNonDeterministicTests is reduced or converted into replay-refresh tasks."
        }));
    }
    let mut operator = Vec::new();
    if !chatgpt.is_empty() || !codex.is_empty() {
        operator.push(json!({
            "action": "Approve customer messages, seller follow-ups, order mutations, payment actions, and production business changes before execution.",
            "reason": "Sirin and ChatGPT/Codex handoffs are review-only until explicitly approved."
        }));
    }
    let (operational_readiness, next_action_queue) =
        operations_action_queue_from_digest(progress, review_queues);
    let review_queue_pressure = review_queue_pressure_from_digest(review_queues);
    let review_drain_plan = review_drain_plan_from_digest(review_queues);
    let review_drain_progress = review_drain_progress_from_plan(&review_drain_plan, review_queues);
    let operator_review_readiness =
        operator_review_readiness_from_report(&review_drain_plan, &review_drain_progress);
    let codex_review_readiness =
        codex_review_readiness_from_report(&review_drain_plan, &review_drain_progress);
    let review_execution_matrix = review_execution_matrix_from_report(&review_drain_plan);
    let operator_approval_packets = operator_approval_packets_from_matrix(&review_execution_matrix);
    let handoff_review_inbox = handoff_review_inbox_from_report(
        &review_drain_progress,
        &operator_approval_packets,
        &review_execution_matrix,
    );
    let conversion_evidence = conversion_evidence_from_progress(progress);
    let cycle_exit_criteria = cycle_exit_criteria_from_progress(progress);
    let cycle_completion =
        cycle_completion_from_report(progress, &operational_readiness, &next_action_queue);
    let schedule_continuity = schedule_continuity_from_report(
        scheduled,
        recommended_next_schedule,
        local_schedule_can_continue,
        &review_queue_pressure,
        &cycle_completion,
    );
    let operations_gate_status = operations_gate_status_from_report(
        &schedule_continuity,
        &review_execution_matrix,
        &operator_approval_packets,
        &codex_review_readiness,
        &cycle_completion,
    );
    let external_issue_state_sync = external_issue_state_sync_from_progress(progress);
    let capability_gap_plan =
        capability_gap_plan_from_progress(progress, &external_issue_state_sync);
    match external_issue_state_sync
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
    {
        "ready_to_rerun_dry_run" => sirin.push(json!({
            "action": "Rerun the gated checkout dry-run for resolved external blockers.",
            "verification": "The dry-run reaches checkout/payment-confirmation view and stops before payment."
        })),
        "needs_issue_state_sync" => sirin.push(json!({
            "action": "Sync tracked external issue state before rerunning checkout dry-runs.",
            "verification": "externalIssueStateSync no longer reports unknown tracked issue state."
        })),
        "waiting_external_fix" => sirin.push(json!({
            "action": "Keep the external blocker linked and wait for fix evidence before rerunning checkout dry-runs.",
            "verification": "externalIssueStateSync moves from waiting_external_fix to ready_to_rerun_dry_run after fix evidence."
        })),
        _ => {}
    }
    let operator_brief = operator_brief_from_report(
        status,
        &conversion_evidence,
        progress.get("largestBlocker").unwrap_or(&Value::Null),
        &operational_readiness,
        &schedule_continuity,
        &next_action_queue,
        &capability_gap_plan,
    );
    let handoff_plan = json!({
        "sirin": sirin,
        "chatgpt": chatgpt,
        "codex": codex,
        "operator": operator
    });
    let report_completeness = report_completeness_from_report(
        &conversion_evidence,
        progress.get("largestBlocker").unwrap_or(&Value::Null),
        &handoff_plan,
        &schedule_continuity,
        &capability_gap_plan,
        &external_issue_state_sync,
        &cycle_completion,
        &review_queue_pressure,
        &review_drain_plan,
        &review_drain_progress,
        &operator_review_readiness,
        &codex_review_readiness,
        &review_execution_matrix,
        &operator_approval_packets,
        &handoff_review_inbox,
        &operations_gate_status,
        &operator_brief,
    );

    json!({
        "reportType": "AGORA_MARKET_OPERATIONS_CYCLE_DIGEST",
        "status": status,
        "operatorBrief": operator_brief,
        "reportCompleteness": report_completeness,
        "externalIssueStateSync": external_issue_state_sync,
        "cycleCompletion": cycle_completion,
        "cycleExitCriteria": cycle_exit_criteria,
        "conversionEvidence": conversion_evidence,
        "orderProgress": progress,
        "largestBlocker": progress.get("largestBlocker").cloned().unwrap_or(Value::Null),
        "queueStatus": {
            "pendingIssueCandidates": review_queues.pending_issue_candidates,
            "pendingCodexReviewActions": review_queues.pending_codex_review_actions,
            "pendingOperatorApprovalRequests": review_queues.pending_operator_approval_requests,
            "pendingOperatorApprovalSummary": review_queues.pending_operator_approval_summary.clone(),
            "readyHandoffItems": review_queues.ready_handoff_items,
            "handoffStatusSummary": review_queues.handoff_status_summary.clone(),
            "recentlyDrainedHandoffSummary": review_queues.recently_drained_handoff_summary.clone()
        },
        "scriptHealth": review_queues.script_health_summary,
        "reviewQueuePressure": review_queue_pressure,
        "reviewDrainPlan": review_drain_plan,
        "reviewDrainProgress": review_drain_progress,
        "operatorReviewReadiness": operator_review_readiness,
        "codexReviewReadiness": codex_review_readiness,
        "reviewExecutionMatrix": review_execution_matrix,
        "operatorApprovalPackets": operator_approval_packets,
        "handoffReviewInbox": handoff_review_inbox,
        "operationsGateStatus": operations_gate_status,
        "scheduleContinuity": schedule_continuity,
        "operationalReadiness": operational_readiness,
        "nextActionQueue": next_action_queue,
        "handoffPlan": handoff_plan,
        "localSchedule": {
            "scheduledSchedules": scheduled,
            "canContinue": local_schedule_can_continue,
            "recommendedNextSchedule": recommended_next_schedule
        },
        "sirinCapabilityGaps": progress
            .get("sirinCapabilityGaps")
            .cloned()
            .unwrap_or_else(|| json!([])),
        "capabilityGapPlan": capability_gap_plan,
        "boundary": "Sirin-local operations digest only; no external system was mutated."
    })
}

fn build_ops_schedule_digest(
    records: &[Value],
    review_queues: &OpsReviewQueues,
    scheduled_total: Option<usize>,
) -> Value {
    let mut ready_for_review = 0usize;
    let mut blocked_integration = 0usize;
    let mut failed = 0usize;
    let mut scheduled = 0usize;
    let mut new_issue_candidates = 0u64;
    let mut handoff_only_candidates = 0u64;
    let mut tracked_external_issues = 0u64;
    let mut unique_tracked_external_issues = HashSet::new();
    let mut inbox_events_needing_review = 0u64;
    let mut healthy_checks = 0u64;
    let mut blocked_checks = 0u64;
    let mut latest_finished_at = Value::Null;
    let mut latest_schedule_id = Value::Null;
    let mut latest_result_status = Value::Null;
    let mut latest_action_summary = Value::Null;
    let mut latest_result_sort_key = String::new();
    let mut latest_repeatable_order_progress = Value::Null;
    let mut latest_repeatable_order_sort_key = String::new();
    let mut latest_tracked_issue_values: HashMap<String, (String, Value)> = HashMap::new();

    for record in records {
        match record
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "SCHEDULED" => scheduled += 1,
            "FAILED" => failed += 1,
            _ => {}
        }
        match record
            .get("last_result_status")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "READY_FOR_REVIEW" => ready_for_review += 1,
            "BLOCKED_INTEGRATION" => blocked_integration += 1,
            _ => {}
        }
        let action = record
            .pointer("/last_result/ops_report/actionSummary")
            .unwrap_or(&Value::Null);
        let finished_at = record
            .get("last_finished_at")
            .and_then(Value::as_str)
            .unwrap_or_default();
        new_issue_candidates += action
            .get("newIssueCandidates")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        handoff_only_candidates += action
            .get("handoffOnlyCandidates")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        tracked_external_issues += action
            .get("trackedExternalIssues")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        for key in tracked_issue_keys_from_record(record) {
            unique_tracked_external_issues.insert(key);
        }
        for issue in tracked_issue_values_from_record(record) {
            let Some(key) = tracked_issue_value_key(issue) else {
                continue;
            };
            let should_replace = latest_tracked_issue_values
                .get(&key)
                .is_none_or(|(seen_at, _)| finished_at >= seen_at.as_str());
            if should_replace {
                latest_tracked_issue_values.insert(key, (finished_at.to_string(), issue.clone()));
            }
        }
        inbox_events_needing_review += action
            .get("inboxEventsNeedingReview")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        healthy_checks += action
            .get("healthyChecks")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        blocked_checks += action
            .get("blockedChecks")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !action.is_null() && finished_at >= latest_result_sort_key.as_str() {
            latest_result_sort_key = finished_at.to_string();
            latest_action_summary = action.clone();
            latest_finished_at = record
                .get("last_finished_at")
                .cloned()
                .unwrap_or(Value::Null);
            latest_schedule_id = record.get("id").cloned().unwrap_or(Value::Null);
            latest_result_status = record
                .get("last_result_status")
                .cloned()
                .unwrap_or(Value::Null);
        }
        if let Some(progress) = repeatable_order_progress_from_record(record) {
            if finished_at >= latest_repeatable_order_sort_key.as_str() {
                latest_repeatable_order_sort_key = finished_at.to_string();
                latest_repeatable_order_progress = progress;
            }
        }
    }
    if let Some(total) = scheduled_total {
        scheduled = total;
    }
    if !latest_tracked_issue_values.is_empty() {
        merge_tracked_issue_values_into_progress(
            &mut latest_repeatable_order_progress,
            latest_tracked_issue_values
                .values()
                .map(|(_, issue)| issue.clone())
                .collect(),
        );
    }

    let latest_is_blocked = latest_result_status
        .as_str()
        .is_some_and(|status| status == "BLOCKED_INTEGRATION");
    let has_pending_issue_review = review_queues.pending_issue_candidates > 0;
    let has_active_review_queue =
        review_queues.pending_codex_review_actions > 0 || review_queues.ready_handoff_items > 0;
    let status = if latest_is_blocked {
        "attention_integration_blocked"
    } else if has_pending_issue_review {
        "attention_issue_review"
    } else if has_active_review_queue {
        "attention_handoff_review"
    } else if failed > 0 {
        "attention_schedule_failures"
    } else if records.is_empty() {
        "empty"
    } else {
        "healthy_monitoring"
    };
    let recommended_next_schedule = if latest_is_blocked {
        "AGORA_MARKET_ISSUE_SCOUT after resolving OPS/MCP integration"
    } else if has_pending_issue_review || has_active_review_queue {
        "Review current handoffs/issues before running another issue scout"
    } else if scheduled > 0 {
        "Wait for the next scheduled Sirin-local ops run"
    } else {
        "Create or resume a daily AGORA_MARKET_ISSUE_SCOUT schedule"
    };
    let operations_cycle_report = operations_cycle_report_from_digest(
        status,
        recommended_next_schedule,
        scheduled,
        &latest_repeatable_order_progress,
        review_queues,
    );

    json!({
        "status": status,
        "windowScheduleCount": records.len(),
        "readyForReviewRuns": ready_for_review,
        "blockedIntegrationRuns": blocked_integration,
        "failedSchedules": failed,
        "scheduledSchedules": scheduled,
        "newIssueCandidates": new_issue_candidates,
        "pendingIssueCandidates": review_queues.pending_issue_candidates,
        "handoffOnlyCandidates": handoff_only_candidates,
        "pendingCodexReviewActions": review_queues.pending_codex_review_actions,
        "pendingCodexReviewSummary": review_queues.pending_codex_review_summary,
        "readyHandoffItems": review_queues.ready_handoff_items,
        "pendingOperatorApprovalRequests": review_queues.pending_operator_approval_requests,
        "pendingOperatorApprovalSummary": review_queues.pending_operator_approval_summary.clone(),
        "readyHandoffSummary": review_queues.ready_handoff_summary,
        "scriptHealthSummary": review_queues.script_health_summary,
        "trackedExternalIssues": tracked_external_issues,
        "uniqueTrackedExternalIssues": unique_tracked_external_issues.len(),
        "inboxEventsNeedingReview": inbox_events_needing_review,
        "healthyChecks": healthy_checks,
        "blockedChecks": blocked_checks,
        "latestScheduleId": latest_schedule_id,
        "latestResultStatus": latest_result_status,
        "latestFinishedAt": latest_finished_at,
        "latestActionSummary": latest_action_summary,
        "repeatableOrderProgress": latest_repeatable_order_progress,
        "operationsCycleReport": operations_cycle_report,
        "recommendedNextSchedule": recommended_next_schedule,
    })
}

fn merge_tracked_issue_values_into_progress(progress: &mut Value, issues: Vec<Value>) {
    if issues.is_empty() {
        return;
    }
    let Some(obj) = progress.as_object_mut() else {
        return;
    };
    let mut merged_by_key: HashMap<String, Value> = HashMap::new();
    let mut order = Vec::new();
    if let Some(existing) = obj.get("trackedIssues").and_then(Value::as_array) {
        for issue in existing {
            let key = tracked_issue_value_key(issue).unwrap_or_else(|| issue.to_string());
            if !merged_by_key.contains_key(&key) {
                order.push(key.clone());
            }
            merged_by_key.insert(key, issue.clone());
        }
    }
    for issue in issues {
        let key = tracked_issue_value_key(&issue).unwrap_or_else(|| issue.to_string());
        if !merged_by_key.contains_key(&key) {
            order.push(key.clone());
        }
        merged_by_key.insert(key, issue);
    }
    let merged: Vec<Value> = order
        .into_iter()
        .filter_map(|key| merged_by_key.remove(&key))
        .collect();
    obj.insert("trackedIssues".into(), json!(merged));
}

fn repeatable_order_progress_from_record(record: &Value) -> Option<Value> {
    let activation = record.pointer("/last_result/first_order_activation")?;
    if activation.get("goal").and_then(Value::as_str) != Some("FIRST_FIVE_PRODUCTION_ORDERS") {
        return None;
    }
    let progress = activation
        .pointer("/operationsCycleReport/orderProgress")
        .unwrap_or(&Value::Null);
    let current_state = activation.get("currentState").unwrap_or(&Value::Null);
    let blockers = activation
        .get("topBlockers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let largest_blocker = blockers.first().cloned().unwrap_or(Value::Null);
    Some(json!({
        "goal": "FIRST_FIVE_PRODUCTION_ORDERS",
        "status": activation.get("status").cloned().unwrap_or(Value::Null),
        "summary": activation.get("summary").cloned().unwrap_or(Value::Null),
        "reportType": activation.pointer("/operationsCycleReport/reportType").cloned().unwrap_or(Value::Null),
        "productionOrders30d": progress
            .get("productionOrders30d")
            .or_else(|| current_state.get("productionOrders30d"))
            .cloned()
            .unwrap_or(Value::Null),
        "targetProductionOrders": progress
            .get("targetProductionOrders")
            .or_else(|| current_state.get("targetProductionOrders"))
            .cloned()
            .unwrap_or(Value::Null),
        "remainingProductionOrdersToTarget": progress
            .get("remainingProductionOrdersToTarget")
            .or_else(|| current_state.get("remainingProductionOrdersToTarget"))
            .cloned()
            .unwrap_or(Value::Null),
        "productionGmv30d": progress
            .get("productionGmv30d")
            .or_else(|| current_state.get("productionGmv30d"))
            .cloned()
            .unwrap_or(Value::Null),
        "pendingShipmentCount": current_state.get("pendingShipmentCount").cloned().unwrap_or(Value::Null),
        "trackedIssues": current_state.get("trackedIssues").cloned().unwrap_or_else(|| json!([])),
        "largestBlocker": largest_blocker,
        "localSchedule": activation.pointer("/operationsCycleReport/localSchedule").cloned().unwrap_or(Value::Null),
        "sirinCapabilityGaps": activation.pointer("/operationsCycleReport/sirinCapabilityGaps").cloned().unwrap_or_else(|| json!([])),
        "sourceScheduleId": record.get("id").cloned().unwrap_or(Value::Null),
        "sourceFinishedAt": record.get("last_finished_at").cloned().unwrap_or(Value::Null),
    }))
}

fn enrich_schedules_with_action_statuses(mut schedules: Vec<Value>) -> Result<Vec<Value>, String> {
    let actions = crate::ops_action_ledger::action_records_by_id()?;
    for schedule in &mut schedules {
        enrich_schedule_with_action_statuses(schedule, &actions);
    }
    Ok(schedules)
}

fn enrich_schedule_with_action_statuses(schedule: &mut Value, actions: &HashMap<String, Value>) {
    let Some(items) = schedule
        .pointer_mut("/last_result/codex_review_actions")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(action) = actions.get(id) else {
            item["statusSource"] = json!("schedule_snapshot");
            continue;
        };
        let snapshot_status = item.get("status").cloned().unwrap_or(Value::Null);
        let ledger_status = action.get("status").cloned().unwrap_or(Value::Null);
        item["snapshotStatus"] = snapshot_status;
        item["status"] = ledger_status.clone();
        item["ledgerStatus"] = ledger_status;
        item["statusSource"] = json!("ops_action_ledger");
        item["ledgerUpdatedAt"] = action.get("updatedAt").cloned().unwrap_or(Value::Null);
        item["ledgerCompletedAt"] = action.get("completedAt").cloned().unwrap_or(Value::Null);
        if let Some(result) = action.get("result").filter(|value| !value.is_null()) {
            item["ledgerResult"] = result.clone();
        }
    }
}

fn tracked_issue_keys_from_record(record: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(issues) = record
        .pointer("/last_result/ops_report/trackedExternalIssues")
        .and_then(Value::as_array)
    {
        keys.extend(issues.iter().filter_map(tracked_external_issue_key));
    }
    if let Some(issues) = record
        .pointer("/last_result/first_order_activation/currentState/trackedIssues")
        .and_then(Value::as_array)
    {
        keys.extend(issues.iter().filter_map(tracked_issue_value_key));
    }
    keys
}

fn tracked_issue_values_from_record(record: &Value) -> Vec<&Value> {
    let mut values = Vec::new();
    if let Some(issues) = record
        .pointer("/last_result/ops_report/trackedExternalIssues")
        .and_then(Value::as_array)
    {
        values.extend(issues.iter());
    }
    if let Some(issues) = record
        .pointer("/last_result/first_order_activation/currentState/trackedIssues")
        .and_then(Value::as_array)
    {
        values.extend(issues.iter());
    }
    values
}

fn tracked_external_issue_key(issue: &Value) -> Option<String> {
    let external = issue
        .get("externalIssue")
        .filter(|value| value.is_object())
        .unwrap_or(issue);
    if let Some(url) = external
        .get("url")
        .or_else(|| external.get("externalIssueUrl"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        return Some(url.to_ascii_lowercase());
    }

    let project = external
        .get("project")
        .or_else(|| external.get("externalIssueProject"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|project| !project.is_empty())?;
    let number = external
        .get("number")
        .or_else(|| external.get("externalIssueNumber"))
        .and_then(|value| {
            value
                .as_u64()
                .map(|n| n.to_string())
                .or_else(|| value.as_str().map(|s| s.trim().to_string()))
        })
        .filter(|number| !number.is_empty())?;
    Some(format!("{}#{}", project.to_ascii_lowercase(), number))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubIssueRef {
    owner: String,
    repo: String,
    number: u64,
}

impl GitHubIssueRef {
    fn from_value(value: &Value) -> Option<Self> {
        let external = value
            .get("externalIssue")
            .filter(|item| item.is_object())
            .unwrap_or(value);
        let url = value
            .as_str()
            .or_else(|| external.get("url").and_then(Value::as_str))
            .or_else(|| external.get("externalIssueUrl").and_then(Value::as_str));
        if let Some(reference) = value.as_str().and_then(Self::from_short_ref) {
            return Some(reference);
        }
        if let Some(reference) = url.and_then(Self::from_url) {
            return Some(reference);
        }

        let project = external
            .get("project")
            .or_else(|| external.get("externalIssueProject"))
            .and_then(Value::as_str)?;
        let number = external
            .get("number")
            .or_else(|| external.get("externalIssueNumber"))
            .and_then(|item| item.as_u64().or_else(|| item.as_str()?.parse().ok()))?;
        let repo = project.trim();
        if repo.is_empty() {
            return None;
        }
        Some(Self {
            owner: "Redandan".to_string(),
            repo: repo.to_string(),
            number,
        })
    }

    fn from_short_ref(reference: &str) -> Option<Self> {
        let (repo, number) = reference.trim().split_once('#')?;
        let repo = repo.trim();
        let number = number.trim().parse().ok()?;
        if repo.is_empty() {
            return None;
        }
        Some(Self {
            owner: "Redandan".to_string(),
            repo: repo.to_string(),
            number,
        })
    }

    fn from_url(url: &str) -> Option<Self> {
        let marker = "github.com/";
        let after = url.split_once(marker)?.1;
        let parts: Vec<&str> = after
            .trim_matches('/')
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() < 4 || parts[2] != "issues" {
            return None;
        }
        Some(Self {
            owner: parts[0].to_string(),
            repo: parts[1].to_string(),
            number: parts[3].parse().ok()?,
        })
    }

    fn key(&self) -> String {
        format!(
            "github.com/{}/{}/issues/{}",
            self.owner.to_ascii_lowercase(),
            self.repo.to_ascii_lowercase(),
            self.number
        )
    }

    fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/issues/{}",
            self.owner, self.repo, self.number
        )
    }
}

#[derive(Debug, Clone)]
struct GitHubIssueSnapshot {
    reference: GitHubIssueRef,
    state: String,
    title: Option<String>,
    updated_at: Option<String>,
    synced_at: String,
}

impl GitHubIssueSnapshot {
    fn to_value(&self) -> Value {
        json!({
            "key": self.reference.key(),
            "url": self.reference.url(),
            "project": self.reference.repo,
            "number": self.reference.number,
            "state": self.state,
            "title": self.title,
            "updatedAt": self.updated_at,
            "syncedAt": self.synced_at,
            "source": "github_rest",
        })
    }
}

fn fetch_github_issue_snapshot(reference: &GitHubIssueRef) -> Result<GitHubIssueSnapshot, String> {
    match fetch_github_issue_snapshot_rest(reference) {
        Ok(snapshot) => Ok(snapshot),
        Err(rest_error) => fetch_github_issue_snapshot_gh(reference)
            .map_err(|gh_error| format!("{rest_error}; gh fallback failed: {gh_error}")),
    }
}

fn fetch_github_issue_snapshot_rest(
    reference: &GitHubIssueRef,
) -> Result<GitHubIssueSnapshot, String> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/issues/{}",
        reference.owner, reference.repo, reference.number
    );
    let mut request = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("build GitHub client failed: {e}"))?
        .get(&url)
        .header(reqwest::header::USER_AGENT, "Sirin-Ops-AI")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.trim().is_empty() {
            request = request.bearer_auth(token.trim());
        }
    }
    let response = request
        .send()
        .map_err(|e| format!("GitHub issue request failed for {}: {e}", reference.url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "GitHub issue request failed for {} with HTTP {status}",
            reference.url()
        ));
    }
    let body: Value = response.json().map_err(|e| {
        format!(
            "parse GitHub issue JSON failed for {}: {e}",
            reference.url()
        )
    })?;
    let state = body
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    Ok(GitHubIssueSnapshot {
        reference: reference.clone(),
        state: match state.as_str() {
            "open" | "closed" => state,
            _ => "unknown".to_string(),
        },
        title: body
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        updated_at: body
            .get("updated_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        synced_at: Utc::now().to_rfc3339(),
    })
}

fn fetch_github_issue_snapshot_gh(
    reference: &GitHubIssueRef,
) -> Result<GitHubIssueSnapshot, String> {
    let repo = format!("{}/{}", reference.owner, reference.repo);
    let output = Command::new(gh_bin())
        .no_window()
        .args([
            "issue",
            "view",
            &reference.number.to_string(),
            "--repo",
            &repo,
            "--json",
            "title,state,updatedAt",
        ])
        .output()
        .map_err(|e| format!("gh issue view spawn failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "gh issue view exit {} for {}: {}",
            output.status,
            reference.url(),
            stderr.trim()
        ));
    }
    let body: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parse gh issue JSON failed for {}: {e}", reference.url()))?;
    let state = body
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    Ok(GitHubIssueSnapshot {
        reference: reference.clone(),
        state: match state.as_str() {
            "open" | "closed" => state,
            _ => "unknown".to_string(),
        },
        title: body
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        updated_at: body
            .get("updatedAt")
            .and_then(Value::as_str)
            .map(str::to_string),
        synced_at: Utc::now().to_rfc3339(),
    })
}

fn gh_bin() -> String {
    std::env::var("SIRIN_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

fn apply_external_issue_snapshots(
    records: &mut [Value],
    snapshots: &[GitHubIssueSnapshot],
) -> usize {
    let by_key: HashMap<String, &GitHubIssueSnapshot> = snapshots
        .iter()
        .map(|snapshot| (snapshot.reference.key(), snapshot))
        .collect();
    let mut updated = 0usize;
    for record in records {
        updated += update_issue_array_with_snapshots(
            record.pointer_mut("/last_result/ops_report/trackedExternalIssues"),
            &by_key,
        );
        updated += update_issue_array_with_snapshots(
            record.pointer_mut("/last_result/first_order_activation/currentState/trackedIssues"),
            &by_key,
        );
    }
    updated
}

fn update_issue_array_with_snapshots(
    value: Option<&mut Value>,
    by_key: &HashMap<String, &GitHubIssueSnapshot>,
) -> usize {
    let Some(items) = value.and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut updated = 0usize;
    for item in items {
        let Some(reference) = GitHubIssueRef::from_value(item) else {
            continue;
        };
        let Some(snapshot) = by_key.get(&reference.key()) else {
            continue;
        };
        let mut merged = if item.is_object() {
            item.clone()
        } else {
            json!({ "url": reference.url() })
        };
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("url".into(), json!(reference.url()));
            obj.insert("project".into(), json!(reference.repo));
            obj.insert("number".into(), json!(reference.number));
            obj.insert("state".into(), json!(snapshot.state));
            obj.insert("issueState".into(), json!(snapshot.state));
            obj.insert("stateSource".into(), json!("github_rest"));
            obj.insert("stateSyncedAt".into(), json!(snapshot.synced_at));
            if let Some(title) = &snapshot.title {
                obj.insert("title".into(), json!(title));
            }
            if let Some(updated_at) = &snapshot.updated_at {
                obj.insert("issueUpdatedAt".into(), json!(updated_at));
            }
            *item = merged;
            updated += 1;
        }
    }
    updated
}

fn tracked_issue_value_key(issue: &Value) -> Option<String> {
    if let Some(reference) = GitHubIssueRef::from_value(issue) {
        return Some(reference.key());
    }
    issue
        .as_str()
        .or_else(|| issue.get("url").and_then(Value::as_str))
        .or_else(|| issue.pointer("/externalIssue/url").and_then(Value::as_str))
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_ascii_lowercase)
        .or_else(|| tracked_external_issue_key(issue))
}

fn next_schedule_id(records: &[Value]) -> String {
    let date_str = Local::now().format("%Y%m%d").to_string();
    let seq = records
        .iter()
        .filter(|record| {
            record
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with(&format!("OPS-{date_str}")))
        })
        .count()
        + 1;
    format!("OPS-{date_str}-{seq:04}")
}

fn ops_schedule_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("tracking")
        .join(OPS_SCHEDULE_FILE)
}

fn ops_schedule_transaction_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn mutate_ops_schedule_records<T>(
    mutation: impl FnOnce(&mut Vec<Value>) -> Result<(T, bool), String>,
) -> Result<T, String> {
    let _guard = ops_schedule_transaction_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut records = load_ops_schedule_records()?;
    let (result, should_persist) = mutation(&mut records)?;
    if should_persist {
        write_ops_schedule_records(&records)?;
    }
    Ok(result)
}

fn load_ops_schedule_records() -> Result<Vec<Value>, String> {
    let path = ops_schedule_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("read ops schedule {} failed: {e}", path.display()))?;
    let mut rows = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "parse ops schedule {} line {} failed: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        rows.push(value);
    }
    Ok(rows)
}

fn write_ops_schedule_records(records: &[Value]) -> Result<(), String> {
    let path = ops_schedule_path();
    crate::jsonl_log::atomic_write_jsonl(&path, records)
        .map_err(|e| format!("replace ops schedule {} failed: {e}", path.display()))
}

fn arg_text(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn concurrent_schedule_creates_preserve_every_record() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let _ = std::fs::remove_file(&path);
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();
        for worker_id in 0..8 {
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                call_ops_schedule_create(json!({
                    "title": format!("parallel schedule {worker_id}"),
                    "taskType": "SIRIN_AGORA_TEST_HEALTH_SCOUT",
                    "cadence": "manual"
                }))
            }));
        }

        let mut ids = HashSet::new();
        for worker in workers {
            let created = worker.join().expect("create worker panicked").unwrap();
            ids.insert(created["schedule"]["id"].as_str().unwrap().to_string());
        }
        let records = load_ops_schedule_records().expect("load parallel schedules");
        assert_eq!(records.len(), 8);
        assert_eq!(ids.len(), 8, "transactional ID allocation must be unique");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn due_schedule_can_only_be_claimed_once() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let _ = std::fs::remove_file(&path);
        let created = call_ops_schedule_create(json!({
            "title": "single claim",
            "taskType": "SIRIN_AGORA_TEST_HEALTH_SCOUT",
            "cadence": "manual"
        }))
        .expect("create due schedule");
        let id = created["schedule"]["id"].as_str().unwrap();

        let first = claim_next_due_schedule(Some(id)).expect("first claim");
        assert!(first.task.is_some());
        let second = claim_next_due_schedule(Some(id)).expect("second claim");
        assert!(
            second.task.is_none(),
            "RUNNING schedule must not be claimed twice"
        );
        assert_eq!(second.schedule.unwrap()["status"], "RUNNING");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn runner_completion_does_not_overwrite_operator_status_change() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let _ = std::fs::remove_file(&path);
        let created = call_ops_schedule_create(json!({
            "title": "operator wins",
            "taskType": "SIRIN_AGORA_TEST_HEALTH_SCOUT",
            "cadence": "manual"
        }))
        .expect("create due schedule");
        let id = created["schedule"]["id"].as_str().unwrap();
        claim_next_due_schedule(Some(id)).expect("claim schedule");
        call_ops_schedule_update(json!({
            "scheduleId": id,
            "status": "CANCELLED",
            "note": "operator cancelled during execution"
        }))
        .expect("cancel claimed schedule");

        let result = json!({"summary": "late runner result"});
        let error =
            finish_claimed_schedule(id, "DONE", &Utc::now().to_rfc3339(), Some(&result), None, 0)
                .expect_err("late completion must fail closed");
        assert!(error.contains("changed to CANCELLED"));
        let records = load_ops_schedule_records().expect("load cancelled schedule");
        assert_eq!(records[0]["status"], "CANCELLED");
        assert!(records[0].get("last_result").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ops_schedule_create_list_and_update_stays_local() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let ledger_path = crate::platform::app_data_dir()
            .join("tracking")
            .join("ops_action_ledger.jsonl");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&ledger_path);

        let created = call_ops_schedule_create(json!({
            "title": "Daily Agora issue scout",
            "taskType": "AGORA_MARKET_ISSUE_SCOUT",
            "cadence": "daily",
            "priority": "P1",
            "params": { "maxCandidates": 12 }
        }))
        .expect("create schedule");
        let schedule_id = created["schedule"]["id"].as_str().expect("id").to_string();
        assert!(schedule_id.starts_with("OPS-"));
        assert_eq!(created["schedule"]["status"], "SCHEDULED");

        let listed = call_ops_schedule_list(json!({
            "status": "SCHEDULED",
            "taskType": "AGORA_MARKET_ISSUE_SCOUT"
        }))
        .expect("list schedules");
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["schedules"][0]["id"], schedule_id);

        let updated = call_ops_schedule_update(json!({
            "scheduleId": schedule_id,
            "status": "PAUSED",
            "note": "operator paused"
        }))
        .expect("update schedule");
        assert_eq!(updated["schedule"]["status"], "PAUSED");
        assert_eq!(updated["schedule"]["notes"][0]["note"], "operator paused");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&ledger_path);
    }

    #[test]
    fn ops_schedule_digest_summarizes_recent_action_summaries() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let _ = std::fs::remove_file(&path);

        let records = vec![
            json!({
                "id": "OPS-TEST-0002",
                "title": "Latest issue scout",
                "task_type": "AGORA_MARKET_ISSUE_SCOUT",
                "status": "DONE",
                "updated_at": "2026-06-15T02:00:00Z",
                "last_finished_at": "2026-06-15T02:00:00Z",
                "last_result_status": "READY_FOR_REVIEW",
                "last_result": {
                    "ops_report": {
                        "trackedExternalIssues": [
                            {
                                "externalIssue": {
                                    "project": "AgoraMarket",
                                    "number": 254,
                                    "url": "https://github.com/Redandan/AgoraMarket/issues/254"
                                }
                            },
                            {
                                "externalIssue": {
                                    "project": "AgoraMarketAPI",
                                    "number": 560
                                }
                            }
                        ],
                        "actionSummary": {
                            "status": "ready_for_issue_review",
                            "newIssueCandidates": 1,
                            "handoffOnlyCandidates": 1,
                            "trackedExternalIssues": 2,
                            "inboxEventsNeedingReview": 0,
                            "healthyChecks": 11,
                            "blockedChecks": 0,
                            "recommendedNextStep": "Review candidates"
                        }
                    }
                }
            }),
            json!({
                "id": "OPS-TEST-0001",
                "title": "Older issue scout",
                "task_type": "AGORA_MARKET_ISSUE_SCOUT",
                "status": "DONE",
                "updated_at": "2026-06-15T01:00:00Z",
                "last_finished_at": "2026-06-15T01:00:00Z",
                "last_result_status": "READY_FOR_REVIEW",
                "last_result": {
                    "ops_report": {
                        "trackedExternalIssues": [
                            {
                                "externalIssue": {
                                    "project": "AgoraMarket",
                                    "number": 254,
                                    "url": "https://github.com/Redandan/AgoraMarket/issues/254"
                                }
                            }
                        ],
                        "actionSummary": {
                            "status": "ready_for_handoff_review",
                            "newIssueCandidates": 0,
                            "handoffOnlyCandidates": 1,
                            "trackedExternalIssues": 1,
                            "inboxEventsNeedingReview": 0,
                            "healthyChecks": 10,
                            "blockedChecks": 0
                        }
                    }
                }
            }),
            json!({
                "id": "OPS-TEST-0000",
                "title": "Old blocked scout",
                "task_type": "AGORA_MARKET_ISSUE_SCOUT",
                "status": "DONE",
                "updated_at": "2026-06-15T00:00:00Z",
                "last_finished_at": "2026-06-15T00:00:00Z",
                "last_result_status": "BLOCKED_INTEGRATION",
                "last_result": {
                    "ops_report": {
                        "actionSummary": {
                            "status": "blocked_integration",
                            "newIssueCandidates": 0,
                            "handoffOnlyCandidates": 0,
                            "trackedExternalIssues": 0,
                            "inboxEventsNeedingReview": 0,
                            "healthyChecks": 0,
                            "blockedChecks": 1
                        }
                    }
                }
            }),
        ];
        write_ops_schedule_records(&records).expect("write test schedules");

        let digest = call_ops_schedule_digest(json!({
            "taskType": "AGORA_MARKET_ISSUE_SCOUT",
            "limit": 10
        }))
        .expect("digest");

        assert_eq!(digest["count"], 3);
        assert_eq!(digest["digest"]["status"], "healthy_monitoring");
        assert_eq!(digest["digest"]["blockedIntegrationRuns"], 1);
        assert_eq!(digest["digest"]["newIssueCandidates"], 1);
        assert_eq!(digest["digest"]["handoffOnlyCandidates"], 2);
        assert_eq!(digest["digest"]["trackedExternalIssues"], 3);
        assert_eq!(digest["digest"]["uniqueTrackedExternalIssues"], 2);
        assert_eq!(digest["digest"]["healthyChecks"], 21);
        assert_eq!(digest["digest"]["latestScheduleId"], "OPS-TEST-0002");
        assert_eq!(digest["digest"]["latestResultStatus"], "READY_FOR_REVIEW");
        assert_eq!(
            digest["digest"]["recommendedNextSchedule"],
            "Create or resume a daily AGORA_MARKET_ISSUE_SCOUT schedule"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ops_schedule_digest_uses_current_review_queues_for_handoff_attention() {
        let records = vec![json!({
            "id": "OPS-TEST-0030",
            "title": "Resolved handoff scout",
            "task_type": "AGORA_MARKET_ISSUE_SCOUT",
            "status": "DONE",
            "updated_at": "2026-06-16T08:00:00Z",
            "last_finished_at": "2026-06-16T08:00:00Z",
            "last_result_status": "READY_FOR_REVIEW",
            "last_result": {
                "ops_report": {
                    "actionSummary": {
                        "status": "ready_for_handoff_review",
                        "newIssueCandidates": 0,
                        "handoffOnlyCandidates": 2,
                        "trackedExternalIssues": 1,
                        "inboxEventsNeedingReview": 0,
                        "healthyChecks": 10,
                        "blockedChecks": 0
                    }
                }
            }
        })];

        let no_current_queue = OpsReviewQueues::default();
        let digest = build_ops_schedule_digest(&records, &no_current_queue, None);
        assert_eq!(digest["status"], "healthy_monitoring");
        assert_eq!(digest["handoffOnlyCandidates"], 2);
        assert_eq!(digest["readyHandoffItems"], 0);
        assert_eq!(digest["pendingCodexReviewActions"], 0);
        assert_eq!(
            digest["recommendedNextSchedule"],
            "Create or resume a daily AGORA_MARKET_ISSUE_SCOUT schedule"
        );

        let active_queue = OpsReviewQueues {
            pending_issue_candidates: 0,
            pending_codex_review_actions: 1,
            ready_handoff_items: 2,
            pending_operator_approval_requests: 1,
            pending_codex_review_summary: vec![json!({
                "id": "OPS-ACTION-TEST-0001",
                "title": "Codex review: first-order buyer path",
                "action": "Track checkout/cart blocker issues and rerun dry-run only after the external blocker changes.",
                "actor": "CODEX",
                "status": "PENDING_CODEX_REVIEW",
                "targetProject": "AgoraMarket",
                "verification": "agora_makro_th_checkout_dry stops before payment confirmation when rerun.",
            })],
            ready_handoff_summary: vec![
                json!({
                    "handoffId": "handoff:OPS-TEST-0030:abc",
                    "title": "Review pending shipment follow-up for repeatable orders",
                    "targetActor": "CHATGPT",
                    "allowedAction": "HANDOFF_REVIEW_ONLY",
                    "candidateType": "FULFILLMENT_FOLLOWUP",
                    "evidenceLevel": "observed",
                    "approvalRequestId": "approval:handoff:OPS-TEST-0030:abc",
                    "approvalRequestStatus": "PENDING_OPERATOR_APPROVAL",
                }),
                json!({
                    "handoffId": "handoff:OPS-TEST-0030:def",
                    "title": "Convert repeatable-order support gaps into FAQ/CTA handoff",
                    "targetActor": "CHATGPT",
                    "allowedAction": "HANDOFF_REVIEW_ONLY",
                    "candidateType": "GROWTH_GAP",
                    "evidenceLevel": "hypothesis",
                }),
            ],
            pending_operator_approval_summary: vec![json!({
                "requestId": "approval:handoff:OPS-TEST-0030:abc",
                "handoffId": "handoff:OPS-TEST-0030:abc",
                "status": "PENDING_OPERATOR_APPROVAL",
                "title": "Review pending shipment follow-up for repeatable orders",
                "targetActor": "CHATGPT",
                "track": "fulfillment_followup",
            })],
            handoff_status_summary: json!({
                "ready": 2,
                "acked": 0,
                "done": 0,
                "cancelled": 0,
                "other": 0,
                "drained": 0,
                "total": 2
            }),
            recently_drained_handoff_summary: Vec::new(),
            script_health_summary: json!({
                "status": "attention_llm_fallback",
                "suite": "active-smoke",
                "windowDays": 14,
                "totalTests": 3,
                "passedViaReplay": 1,
                "passedViaFixtureOnly": 1,
                "passedViaLlm": 1,
                "failed": 0,
                "needsAttention": true,
                "staleOrNonDeterministicTests": [{
                    "testId": "agora_checkout_smoke",
                    "status": "llm_only",
                    "hasScript": false,
                    "tags": ["active-smoke"]
                }]
            }),
        };
        let digest = build_ops_schedule_digest(&records, &active_queue, None);
        assert_eq!(digest["status"], "attention_handoff_review");
        assert_eq!(digest["readyHandoffItems"], 2);
        assert_eq!(digest["pendingCodexReviewActions"], 1);
        assert_eq!(digest["pendingCodexReviewSummary"][0]["actor"], "CODEX");
        assert_eq!(
            digest["pendingCodexReviewSummary"][0]["targetProject"],
            "AgoraMarket"
        );
        assert!(digest["pendingCodexReviewSummary"][0]["verification"]
            .as_str()
            .unwrap_or_default()
            .contains("stops before payment confirmation"));
        assert_eq!(digest["readyHandoffSummary"][0]["targetActor"], "CHATGPT");
        assert_eq!(
            digest["readyHandoffSummary"][0]["evidenceLevel"],
            "observed"
        );
        assert_eq!(
            digest["readyHandoffSummary"][0]["allowedAction"],
            "HANDOFF_REVIEW_ONLY"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportType"],
            "AGORA_MARKET_OPERATIONS_CYCLE_DIGEST"
        );
        assert_eq!(
            digest["operationsCycleReport"]["queueStatus"]["readyHandoffItems"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["queueStatus"]["pendingCodexReviewActions"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewQueuePressure"]["totalOpenReviewItems"],
            3
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewQueuePressure"]["readyHandoffsByActor"]
                ["chatgpt"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewQueuePressure"]["drainBeforeNextScout"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewQueuePressure"]["recommendedDrainOrder"],
            "ready_handoffs"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["status"],
            "drain_required"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["totalItems"],
            3
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["nextStep"]["track"],
            "fulfillment_followup"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["nextStep"]["localTool"],
            "a2a_handoff_approval_requests"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["nextStep"]["approvalRequestStatus"],
            "PENDING_OPERATOR_APPROVAL"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["nextStep"]["toolCallTemplate"]
                ["arguments"]["reviewer"],
            "<operator-id>"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["nextStep"]["toolCallTemplate"]
                ["arguments"]["decision"],
            "ACK_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["nextStep"]["toolCallTemplate"]
                ["arguments"]["approvedActions"][0],
            "sirin_local_metadata_update"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["nextStep"]["toolCallTemplate"]
                ["arguments"]["blockedMutations"][0],
            "customer_message"
        );
        assert!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["nextStep"]["toolCallTemplate"]
                ["arguments"]["approvedBoundary"]
                .as_str()
                .unwrap_or_default()
                .contains("metadata update only")
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["steps"][0]["itemId"],
            "handoff:OPS-TEST-0030:abc"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["steps"][1]["track"],
            "support_growth_review"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["steps"][2]["localTool"],
            "ops_review_claim/ops_review_complete"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["steps"][2]["allowedAction"],
            "READ_ONLY_VERIFICATION"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["status"],
            "waiting_for_review"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["openItems"],
            3
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["operatorApprovalItems"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["codexReviewItems"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["drainedHandoffItems"],
            0
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["nextRequiredApproval"],
            "OPERATOR"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["nextLocalTool"],
            "a2a_handoff_approval_requests"
        );
        assert_eq!(digest["pendingOperatorApprovalRequests"], 1);
        assert_eq!(
            digest["operationsCycleReport"]["queueStatus"]["pendingOperatorApprovalRequests"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["nextStep"]
                ["approvalRequestStatus"],
            "PENDING_OPERATOR_APPROVAL"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorReviewReadiness"]["status"],
            "ready_for_operator_review"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorReviewReadiness"]["ready"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorReviewReadiness"]["nextTrack"],
            "fulfillment_followup"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorReviewReadiness"]["template"]["arguments"]
                ["decision"],
            "ACK_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorReviewReadiness"]["checks"][0]["name"],
            "template_present"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorReviewReadiness"]["checks"][0]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["codexReviewReadiness"]["status"],
            "ready_for_codex_review"
        );
        assert_eq!(
            digest["operationsCycleReport"]["codexReviewReadiness"]["ready"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["codexReviewReadiness"]["actionId"],
            "OPS-ACTION-TEST-0001"
        );
        assert_eq!(
            digest["operationsCycleReport"]["codexReviewReadiness"]["claimTemplate"]["name"],
            "ops_review_claim"
        );
        assert_eq!(
            digest["operationsCycleReport"]["codexReviewReadiness"]["completeTemplate"]["name"],
            "ops_review_complete"
        );
        assert_eq!(
            digest["operationsCycleReport"]["codexReviewReadiness"]["checks"][3]["name"],
            "read_only_action"
        );
        assert_eq!(
            digest["operationsCycleReport"]["codexReviewReadiness"]["checks"][3]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewExecutionMatrix"]["status"],
            "classified"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewExecutionMatrix"]["totalOpenItems"],
            3
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewExecutionMatrix"]
                ["operatorApprovalRequiredCount"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewExecutionMatrix"]["codexReviewableCount"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewExecutionMatrix"]["chatgptHandoffOnlyCount"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewExecutionMatrix"]["nextRecommendedLane"],
            "operator_approval_required"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewExecutionMatrix"]["codexReviewable"][0]
                ["localTool"],
            "ops_review_claim/ops_review_complete"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorApprovalPackets"]["status"],
            "ready_for_operator"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorApprovalPackets"]["packetCount"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorApprovalPackets"]["nextPacket"]["track"],
            "fulfillment_followup"
        );
        assert!(
            digest["operationsCycleReport"]["operatorApprovalPackets"]["nextPacket"]
                ["chatgptPromptDraft"]
                .as_str()
                .unwrap_or_default()
                .contains("Do not send messages")
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorApprovalPackets"]["nextPacket"]
                ["blockedMutations"][0],
            "customer_message"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationsGateStatus"]["status"],
            "pending_operator_approval_request"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationsGateStatus"]["nextGate"],
            "operator_approval"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationsGateStatus"]["operatorApprovalPacketCount"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationsGateStatus"]
                ["pendingOperatorApprovalRequestCount"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationsGateStatus"]["nextOperatorPacket"]["track"],
            "fulfillment_followup"
        );
        assert_eq!(
            digest["operationsCycleReport"]["scheduleContinuity"]["runnerMode"],
            "drain_review_queue"
        );
        assert_eq!(
            digest["operationsCycleReport"]["scheduleContinuity"]["drainBeforeNextScout"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["scheduleContinuity"]
                ["canContinueWithoutExternalMutation"],
            false
        );
        assert_eq!(
            digest["scriptHealthSummary"]["status"],
            "attention_llm_fallback"
        );
        assert_eq!(
            digest["operationsCycleReport"]["scriptHealth"]["passedViaLlm"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["fulfillmentFollowupReady"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["growthSupportReviewReady"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["checkoutCodexReviewPending"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["scriptHealthNeedsAttention"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["autoExecutableActionCount"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["approvalGatedActionCount"],
            3
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["nextFocus"],
            "fulfillment_followup"
        );
        assert_eq!(
            digest["operationsCycleReport"]["cycleCompletion"]["achieved"],
            false
        );
        assert!(digest["operationsCycleReport"]["operatorBrief"]["headline"]
            .as_str()
            .unwrap_or_default()
            .contains("0/5 production orders"));
        assert_eq!(
            digest["operationsCycleReport"]["operatorBrief"]["firstAction"]["actor"],
            "CHATGPT"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorBrief"]["firstAction"]["allowedAction"],
            "HANDOFF_REVIEW_ONLY"
        );
        assert_eq!(
            digest["operationsCycleReport"]["handoffReviewInbox"]["status"],
            "open"
        );
        assert_eq!(
            digest["operationsCycleReport"]["handoffReviewInbox"]["approvalRequired"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["handoffReviewInbox"]["nextGate"],
            "operator_approval"
        );
        assert_eq!(
            digest["operationsCycleReport"]["handoffReviewInbox"]["nextItem"]["status"],
            "APPROVAL_REQUIRED"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorBrief"]["runner"]["mode"],
            "drain_review_queue"
        );
        assert_eq!(
            digest["operationsCycleReport"]["cycleCompletion"]["status"],
            "in_progress"
        );
        assert_eq!(
            digest["operationsCycleReport"]["cycleCompletion"]["p0ActionCount"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["cycleCompletion"]["missingRequirements"][1]
                ["currentP0Tracks"][0],
            "fulfillment_followup"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["complete"],
            false
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][2]["name"],
            "actor_handoff_plan"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][2]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][9]["name"],
            "review_drain_progress"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][9]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][10]["name"],
            "operator_review_readiness"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][10]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][11]["name"],
            "codex_review_readiness"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][11]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][12]["name"],
            "review_execution_matrix"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][12]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][13]["name"],
            "operator_approval_packets"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][13]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][14]["name"],
            "operations_gate_status"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][14]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][15]["name"],
            "operator_brief"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][15]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][16]["name"],
            "mutation_boundary"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][16]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][17]["name"],
            "handoff_review_inbox"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][17]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["nextActionQueue"][0]["track"],
            "fulfillment_followup"
        );
        assert_eq!(
            digest["operationsCycleReport"]["nextActionQueue"][0]["executionGate"]
                ["canAutoExecute"],
            false
        );
        assert_eq!(
            digest["operationsCycleReport"]["nextActionQueue"][0]["executionGate"]
                ["requiredApproval"],
            "OPERATOR"
        );
        assert_eq!(
            digest["operationsCycleReport"]["nextActionQueue"][1]["track"],
            "checkout_cart_verification"
        );
        assert_eq!(
            digest["operationsCycleReport"]["nextActionQueue"][1]["executionGate"]["allowedAction"],
            "READ_ONLY_VERIFICATION"
        );
        assert_eq!(
            digest["operationsCycleReport"]["nextActionQueue"][2]["track"],
            "support_growth_review"
        );
        assert_eq!(
            digest["operationsCycleReport"]["nextActionQueue"][3]["executionGate"]
                ["canAutoExecute"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["handoffPlan"]["chatgpt"][0]["targetActor"],
            "CHATGPT"
        );
        assert_eq!(
            digest["operationsCycleReport"]["handoffPlan"]["codex"][0]["actor"],
            "CODEX"
        );
        assert!(digest["operationsCycleReport"]["handoffPlan"]["operator"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        assert!(digest["operationsCycleReport"]["handoffPlan"]["sirin"]
            .as_array()
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("action")
                        .and_then(Value::as_str)
                        .is_some_and(|action| action.contains("script health"))
                })
            }));
        assert!(digest["recommendedNextSchedule"]
            .as_str()
            .unwrap_or_default()
            .contains("Review current handoffs/issues"));

        let mut approved_queue = active_queue.clone();
        approved_queue.pending_operator_approval_requests = 0;
        approved_queue.pending_operator_approval_summary = Vec::new();
        approved_queue.ready_handoff_summary[0]["approvalRequestStatus"] = json!("APPROVED");
        let approved_digest = build_ops_schedule_digest(&records, &approved_queue, None);
        assert_eq!(
            approved_digest["operationsCycleReport"]["operationsGateStatus"]["status"],
            "ready_for_local_approved_handoff_ack"
        );
        assert_eq!(
            approved_digest["operationsCycleReport"]["operationsGateStatus"]["nextGate"],
            "sirin_local_ack"
        );
        assert_eq!(
            approved_digest["operationsCycleReport"]["reviewDrainPlan"]["nextStep"]["localTool"],
            "a2a_update_handoff"
        );
        assert_eq!(
            approved_digest["operationsCycleReport"]["handoffReviewInbox"]["nextItem"]
                ["nextAction"],
            "operator_approved_local_ack_allowed"
        );

        let drained_queue = OpsReviewQueues {
            pending_issue_candidates: 0,
            pending_codex_review_actions: 0,
            ready_handoff_items: 0,
            pending_operator_approval_requests: 0,
            pending_codex_review_summary: Vec::new(),
            ready_handoff_summary: Vec::new(),
            pending_operator_approval_summary: Vec::new(),
            handoff_status_summary: json!({
                "ready": 0,
                "acked": 1,
                "done": 1,
                "cancelled": 0,
                "other": 0,
                "drained": 2,
                "total": 2
            }),
            recently_drained_handoff_summary: vec![
                json!({
                    "handoffId": "handoff:OPS-TEST-0030:def",
                    "title": "Convert repeatable-order support gaps into FAQ/CTA handoff",
                    "targetActor": "CHATGPT",
                    "status": "DONE",
                    "candidateType": "GROWTH_GAP",
                    "operatorReview": {
                        "reviewer": "operator-redan",
                        "decision": "DONE_AFTER_CHATGPT_REVIEW",
                        "approvedBoundary": "Sirin-local metadata update only; no FAQ publish approved.",
                        "approvedActions": ["sirin_local_metadata_update"],
                        "blockedMutations": ["public_faq_publish", "customer_message"],
                        "mutationApproved": false,
                        "reviewedAt": "2026-06-19T10:30:00Z",
                        "schemaVersion": 1
                    }
                }),
                json!({
                    "handoffId": "handoff:OPS-TEST-0030:abc",
                    "title": "Review pending shipment follow-up for repeatable orders",
                    "targetActor": "CHATGPT",
                    "status": "ACKED",
                    "candidateType": "FULFILLMENT_FOLLOWUP",
                    "operatorReview": {
                        "reviewer": "operator-redan",
                        "decision": "ACK_FOR_CHATGPT_REVIEW",
                        "approvedBoundary": "Sirin-local metadata update only; no customer or seller message approved.",
                        "approvedActions": ["sirin_local_metadata_update"],
                        "blockedMutations": ["customer_message", "seller_message", "order_update"],
                        "mutationApproved": false,
                        "reviewedAt": "2026-06-19T10:25:00Z",
                        "schemaVersion": 1
                    }
                }),
            ],
            script_health_summary: json!({
                "status": "healthy_deterministic",
                "needsAttention": false
            }),
        };
        let drained_digest = build_ops_schedule_digest(&records, &drained_queue, None);
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]["status"],
            "drained"
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]["openItems"],
            0
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]["drainedHandoffItems"],
            2
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]
                ["reviewedDrainedHandoffItems"],
            2
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]
                ["legacyDrainedHandoffItems"],
            0
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]
                ["structuredReviewCoverage"],
            1.0
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]["reviewEvidenceStatus"],
            "structured_review_complete"
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]
                ["hasReviewEvidenceForRecentDrains"],
            true
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]["handoffStatusSummary"]
                ["done"],
            1
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]
                ["recentlyDrainedHandoffs"][0]["status"],
            "DONE"
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]
                ["recentlyDrainedHandoffs"][0]["operatorReview"]["mutationApproved"],
            false
        );
        assert_eq!(
            drained_digest["operationsCycleReport"]["reviewDrainProgress"]
                ["recentlyDrainedHandoffs"][1]["operatorReview"]["decision"],
            "ACK_FOR_CHATGPT_REVIEW"
        );
    }

    #[test]
    fn drained_handoff_summary_exposes_operator_review_evidence() {
        let handoffs = json!({
            "handoffs": [{
                "handoff_id": "handoff:OPS-TEST-0099:abc",
                "dedupe_key": "agora-market:orders:test",
                "title": "Review pending shipment follow-up for repeatable orders",
                "target_actor": "CHATGPT",
                "status": "ACKED",
                "updated_at": "2026-06-19T10:25:00Z",
                "candidate": {
                    "allowedAction": "HANDOFF_REVIEW_ONLY",
                    "candidateType": "FULFILLMENT_FOLLOWUP",
                    "evidenceLevel": "observed"
                },
                "operator_review": {
                    "at": "2026-06-19T10:24:00Z",
                    "reviewer": "operator-redan",
                    "decision": "ACK_FOR_CHATGPT_REVIEW",
                    "approvedBoundary": "Sirin-local metadata update only; no customer message approved.",
                    "approvedActions": ["sirin_local_metadata_update"],
                    "blockedMutations": ["customer_message", "seller_message", "order_update"],
                    "mutationApproved": false,
                    "schemaVersion": 1
                },
                "executionPackage": {
                    "status": "READY_FOR_CHATGPT_REVIEW",
                    "targetActor": "CHATGPT",
                    "allowedAction": "HANDOFF_REVIEW_ONLY",
                    "boundary": "Local review package only. Do not message customers or sellers.",
                    "createdAt": "2026-06-19T10:24:00Z",
                    "updatedAt": "2026-06-19T10:24:00Z"
                }
            }]
        });

        let drained = summarize_drained_handoffs(&handoffs, 5);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0]["status"], "ACKED");
        assert_eq!(drained[0]["operatorReview"]["reviewer"], "operator-redan");
        assert_eq!(
            drained[0]["operatorReview"]["decision"],
            "ACK_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(drained[0]["operatorReview"]["mutationApproved"], false);
        assert_eq!(
            drained[0]["operatorReview"]["blockedMutations"][0],
            "customer_message"
        );
        assert_eq!(
            drained[0]["executionPackage"]["status"],
            "READY_FOR_CHATGPT_REVIEW"
        );
        assert_eq!(
            drained[0]["executionPackage"]["allowedAction"],
            "HANDOFF_REVIEW_ONLY"
        );

        let counts = summarize_handoff_status_counts(&handoffs);
        assert_eq!(counts["readyExecutionPackages"], 1);
    }

    #[test]
    fn review_drain_progress_flags_legacy_drained_handoffs_without_review_evidence() {
        let review_queues = OpsReviewQueues {
            pending_issue_candidates: 0,
            pending_codex_review_actions: 0,
            ready_handoff_items: 0,
            pending_operator_approval_requests: 0,
            pending_codex_review_summary: Vec::new(),
            ready_handoff_summary: Vec::new(),
            pending_operator_approval_summary: Vec::new(),
            handoff_status_summary: json!({
                "ready": 0,
                "acked": 1,
                "done": 1,
                "cancelled": 0,
                "other": 0,
                "drained": 2,
                "total": 2
            }),
            recently_drained_handoff_summary: vec![json!({
                "handoffId": "handoff:OPS-TEST-0100:abc",
                "title": "Legacy drained handoff",
                "status": "ACKED",
                "operatorReview": Value::Null
            })],
            script_health_summary: Value::Null,
        };
        let plan = json!({
            "status": "clear",
            "steps": []
        });

        let progress = review_drain_progress_from_plan(&plan, &review_queues);
        assert_eq!(progress["status"], "drained");
        assert_eq!(progress["drainedHandoffItems"], 2);
        assert_eq!(progress["reviewedDrainedHandoffItems"], 0);
        assert_eq!(progress["legacyDrainedHandoffItems"], 2);
        assert_eq!(progress["structuredReviewCoverage"], 0.0);
        assert_eq!(progress["reviewEvidenceStatus"], "legacy_gap");
        assert_eq!(progress["hasReviewEvidenceForRecentDrains"], false);
    }

    #[test]
    fn script_health_summary_flags_nondeterministic_active_smoke_tests() {
        let summary = summarize_script_health(&json!({
            "suite": "active-smoke",
            "aggregate": {
                "window_days": 14,
                "total_runs": 4,
                "passed_via_replay": 1,
                "passed_via_fixture_only": 1,
                "passed_deterministic": 2,
                "passed_via_llm": 1,
                "failed": 1,
                "success_rate_replay": 0.25,
                "success_rate_deterministic": 0.5,
                "previous_success_rate_replay": null,
                "drift": null
            },
            "per_test": [
                {
                    "test_id": "agora_home_smoke",
                    "status": "healthy",
                    "has_script": true,
                    "tags": ["active-smoke"]
                },
                {
                    "test_id": "agora_checkout_smoke",
                    "status": "stale_fallback",
                    "has_script": true,
                    "tags": ["active-smoke", "checkout"]
                },
                {
                    "test_id": "agora_seller_fulfillment",
                    "status": "llm_only",
                    "has_script": false,
                    "tags": ["active-smoke", "seller"]
                }
            ],
            "drift_warning_threshold": -0.10
        }));

        assert_eq!(summary["status"], "attention_failed_or_untested");
        assert_eq!(summary["needsAttention"], true);
        assert_eq!(summary["totalTests"], 4);
        assert_eq!(summary["passedViaReplay"], 1);
        assert_eq!(summary["passedViaLlm"], 1);
        assert_eq!(summary["failed"], 1);
        assert_eq!(
            summary["staleOrNonDeterministicTests"][0]["testId"],
            "agora_checkout_smoke"
        );
        assert_eq!(
            summary["staleOrNonDeterministicTests"][1]["status"],
            "llm_only"
        );
    }

    #[test]
    fn ops_schedule_digest_uses_current_issue_inbox_for_issue_attention() {
        let records = vec![json!({
            "id": "OPS-TEST-0040",
            "title": "Resolved issue candidate scout",
            "task_type": "AGORA_MARKET_ISSUE_SCOUT",
            "status": "DONE",
            "updated_at": "2026-06-16T08:00:00Z",
            "last_finished_at": "2026-06-16T08:00:00Z",
            "last_result_status": "READY_FOR_REVIEW",
            "last_result": {
                "ops_report": {
                    "actionSummary": {
                        "status": "ready_for_issue_review",
                        "newIssueCandidates": 3,
                        "handoffOnlyCandidates": 0,
                        "trackedExternalIssues": 1,
                        "inboxEventsNeedingReview": 0,
                        "healthyChecks": 10,
                        "blockedChecks": 0
                    }
                }
            }
        })];

        let no_current_issue = OpsReviewQueues::default();
        let digest = build_ops_schedule_digest(&records, &no_current_issue, None);
        assert_eq!(digest["status"], "healthy_monitoring");
        assert_eq!(digest["newIssueCandidates"], 3);
        assert_eq!(digest["pendingIssueCandidates"], 0);
        assert_eq!(
            digest["recommendedNextSchedule"],
            "Create or resume a daily AGORA_MARKET_ISSUE_SCOUT schedule"
        );

        let active_issue = OpsReviewQueues {
            pending_issue_candidates: 1,
            pending_codex_review_actions: 0,
            ready_handoff_items: 0,
            pending_operator_approval_requests: 0,
            pending_codex_review_summary: Vec::new(),
            ready_handoff_summary: Vec::new(),
            pending_operator_approval_summary: Vec::new(),
            handoff_status_summary: json!({
                "ready": 0,
                "acked": 0,
                "done": 0,
                "cancelled": 0,
                "other": 0,
                "drained": 0,
                "total": 0
            }),
            recently_drained_handoff_summary: Vec::new(),
            script_health_summary: Value::Null,
        };
        let digest = build_ops_schedule_digest(&records, &active_issue, None);
        assert_eq!(digest["status"], "attention_issue_review");
        assert_eq!(digest["pendingIssueCandidates"], 1);
        assert!(digest["recommendedNextSchedule"]
            .as_str()
            .unwrap_or_default()
            .contains("Review current handoffs/issues"));
    }

    #[test]
    fn ops_schedule_digest_does_not_alert_on_historical_inbox_events() {
        let records = vec![json!({
            "id": "OPS-TEST-0045",
            "title": "Resolved inbox event scout",
            "task_type": "AGORA_MARKET_ISSUE_SCOUT",
            "status": "DONE",
            "updated_at": "2026-06-16T08:00:00Z",
            "last_finished_at": "2026-06-16T08:00:00Z",
            "last_result_status": "READY_FOR_REVIEW",
            "last_result": {
                "ops_report": {
                    "actionSummary": {
                        "status": "ready_for_handoff_review",
                        "newIssueCandidates": 0,
                        "handoffOnlyCandidates": 0,
                        "trackedExternalIssues": 1,
                        "inboxEventsNeedingReview": 4,
                        "healthyChecks": 10,
                        "blockedChecks": 0
                    }
                }
            }
        })];

        let digest = build_ops_schedule_digest(&records, &OpsReviewQueues::default(), None);
        assert_eq!(digest["status"], "healthy_monitoring");
        assert_eq!(digest["inboxEventsNeedingReview"], 4);
        assert_eq!(digest["readyHandoffItems"], 0);
        assert_eq!(digest["pendingIssueCandidates"], 0);
        assert_eq!(
            digest["recommendedNextSchedule"],
            "Create or resume a daily AGORA_MARKET_ISSUE_SCOUT schedule"
        );
    }

    #[test]
    fn ops_schedule_digest_latest_result_uses_finished_time_not_metadata_update() {
        let records = vec![
            json!({
                "id": "OPS-OLD-PAUSED",
                "title": "Older paused schedule",
                "task_type": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
                "status": "PAUSED",
                "updated_at": "2026-06-16T09:16:00Z",
                "last_finished_at": "2026-06-16T05:56:00Z",
                "last_result_status": "READY_FOR_REVIEW",
                "last_result": {
                    "ops_report": {
                        "actionSummary": {
                            "status": "older_result",
                            "newIssueCandidates": 0,
                            "handoffOnlyCandidates": 0,
                            "trackedExternalIssues": 1,
                            "inboxEventsNeedingReview": 0,
                            "healthyChecks": 10,
                            "blockedChecks": 0
                        }
                    }
                }
            }),
            json!({
                "id": "OPS-LATEST-RUN",
                "title": "Latest executed schedule",
                "task_type": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
                "status": "DONE",
                "updated_at": "2026-06-16T08:41:00Z",
                "last_finished_at": "2026-06-16T08:41:00Z",
                "last_result_status": "READY_FOR_REVIEW",
                "last_result": {
                    "ops_report": {
                        "actionSummary": {
                            "status": "latest_result",
                            "newIssueCandidates": 0,
                            "handoffOnlyCandidates": 0,
                            "trackedExternalIssues": 1,
                            "inboxEventsNeedingReview": 0,
                            "healthyChecks": 11,
                            "blockedChecks": 0
                        }
                    }
                }
            }),
        ];

        let digest = build_ops_schedule_digest(&records, &OpsReviewQueues::default(), None);
        assert_eq!(digest["latestScheduleId"], "OPS-LATEST-RUN");
        assert_eq!(digest["latestFinishedAt"], "2026-06-16T08:41:00Z");
        assert_eq!(digest["latestActionSummary"]["status"], "latest_result");
    }

    #[test]
    fn ops_schedule_digest_counts_scheduled_outside_recent_window() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let tracking_dir = crate::platform::app_data_dir().join("tracking");
        let ledger_path = tracking_dir.join("ops_action_ledger.jsonl");
        let handoff_path = tracking_dir.join("a2a_handoff_queue.jsonl");
        let candidates_path = tracking_dir.join("a2a_issue_candidates.jsonl");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&ledger_path);
        let _ = std::fs::remove_file(&handoff_path);
        let _ = std::fs::remove_file(&candidates_path);

        let records = vec![
            json!({
                "id": "OPS-RECENT-0002",
                "title": "Recent heartbeat",
                "task_type": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
                "status": "DONE",
                "updated_at": "2026-06-16T09:42:00Z",
                "last_finished_at": "2026-06-16T09:42:00Z",
                "last_result_status": "READY_FOR_REVIEW",
                "last_result": {
                    "ops_report": {
                        "actionSummary": {
                            "newIssueCandidates": 0,
                            "handoffOnlyCandidates": 0,
                            "trackedExternalIssues": 1,
                            "inboxEventsNeedingReview": 0,
                            "healthyChecks": 1,
                            "blockedChecks": 0
                        }
                    }
                }
            }),
            json!({
                "id": "OPS-RECENT-0001",
                "title": "Recent paused cleanup",
                "task_type": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
                "status": "PAUSED",
                "updated_at": "2026-06-16T09:41:00Z"
            }),
            json!({
                "id": "OPS-OLD-SCHEDULED",
                "title": "Older valid daily monitor",
                "task_type": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
                "status": "SCHEDULED",
                "updated_at": "2026-06-13T09:00:00Z",
                "scheduled_for": "2026-06-17T09:00:00+08:00"
            }),
        ];
        write_ops_schedule_records(&records).expect("write test schedules");

        let digest = call_ops_schedule_digest(json!({
            "taskType": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
            "limit": 2
        }))
        .expect("digest");

        assert_eq!(digest["count"], 2);
        assert_eq!(digest["digest"]["windowScheduleCount"], 2);
        assert_eq!(digest["digest"]["scheduledSchedules"], 1);
        assert_eq!(
            digest["digest"]["recommendedNextSchedule"],
            "Wait for the next scheduled Sirin-local ops run"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&ledger_path);
        let _ = std::fs::remove_file(&handoff_path);
        let _ = std::fs::remove_file(&candidates_path);
    }

    #[test]
    fn ops_schedule_digest_counts_first_order_tracked_issues() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let _ = std::fs::remove_file(&path);

        let records = vec![json!({
            "id": "OPS-TEST-0100",
            "title": "First order activation",
            "task_type": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
            "status": "DONE",
            "updated_at": "2026-06-15T03:00:00Z",
            "last_finished_at": "2026-06-15T03:00:00Z",
            "last_result_status": "READY_FOR_REVIEW",
            "last_result": {
                "ops_report": {
                    "actionSummary": {
                        "status": "ready_for_handoff_review",
                        "newIssueCandidates": 0,
                        "handoffOnlyCandidates": 1,
                        "trackedExternalIssues": 1,
                        "inboxEventsNeedingReview": 0,
                        "healthyChecks": 11,
                        "blockedChecks": 0
                    }
                },
                "first_order_activation": {
                    "currentState": {
                        "trackedIssues": [
                            "https://github.com/Redandan/AgoraMarket/issues/254",
                            "https://github.com/Redandan/AgoraMarket/issues/256",
                            "https://github.com/Redandan/AgoraMarketAPI/issues/564"
                        ]
                    }
                }
            }
        })];
        write_ops_schedule_records(&records).expect("write test schedules");

        let digest = call_ops_schedule_digest(json!({
            "taskType": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
            "limit": 10
        }))
        .expect("digest");

        assert_eq!(digest["digest"]["trackedExternalIssues"], 1);
        assert_eq!(digest["digest"]["uniqueTrackedExternalIssues"], 3);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn github_issue_ref_parses_urls_and_project_numbers() {
        let from_url = GitHubIssueRef::from_value(&json!(
            "https://github.com/Redandan/AgoraMarket/issues/257"
        ))
        .expect("url issue ref");
        assert_eq!(from_url.owner, "Redandan");
        assert_eq!(from_url.repo, "AgoraMarket");
        assert_eq!(from_url.number, 257);
        assert_eq!(from_url.key(), "github.com/redandan/agoramarket/issues/257");

        let from_project = GitHubIssueRef::from_value(&json!({
            "externalIssue": {
                "project": "AgoraMarketAPI",
                "number": 560
            }
        }))
        .expect("project issue ref");
        assert_eq!(from_project.owner, "Redandan");
        assert_eq!(from_project.repo, "AgoraMarketAPI");
        assert_eq!(from_project.number, 560);
    }

    #[test]
    fn external_issue_snapshots_update_repeatable_order_digest_gate() {
        let mut records = vec![json!({
            "id": "OPS-TEST-0101",
            "title": "Repeatable order activation",
            "task_type": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
            "status": "DONE",
            "updated_at": "2026-06-19T10:15:00Z",
            "last_finished_at": "2026-06-19T10:15:00Z",
            "last_result_status": "READY_FOR_REVIEW",
            "last_result": {
                "ops_report": {
                    "actionSummary": {
                        "status": "ready_for_handoff_review",
                        "newIssueCandidates": 0,
                        "handoffOnlyCandidates": 0,
                        "trackedExternalIssues": 1,
                        "inboxEventsNeedingReview": 0,
                        "healthyChecks": 11,
                        "blockedChecks": 0
                    }
                },
                "first_order_activation": {
                    "goal": "FIRST_FIVE_PRODUCTION_ORDERS",
                    "status": "activation_required",
                    "summary": "Repeatable-order target is not reached yet.",
                    "currentState": {
                        "productionOrders30d": 1,
                        "targetProductionOrders": 5,
                        "remainingProductionOrdersToTarget": 4,
                        "productionGmv30d": 33.8301,
                        "pendingShipmentCount": 6,
                        "trackedIssues": [
                            "https://github.com/Redandan/AgoraMarket/issues/257"
                        ]
                    },
                    "topBlockers": [{
                        "type": "repeatable_order_gap",
                        "severity": "P0",
                        "summary": "Production orders are 1/5."
                    }],
                    "operationsCycleReport": {
                        "reportType": "AGORA_MARKET_REPEATABLE_ORDER_CYCLE",
                        "orderProgress": {
                            "productionOrders30d": 1,
                            "targetProductionOrders": 5,
                            "remainingProductionOrdersToTarget": 4,
                            "productionGmv30d": 33.8301
                        },
                        "localSchedule": {
                            "canContinue": true
                        },
                        "sirinCapabilityGaps": [
                            "External issue status is still supplied through tracked issue context.",
                            "Fulfillment and support follow-up are represented as review tasks; Sirin still needs an approval-gated handoff execution path before it can contact users or sellers."
                        ]
                    }
                }
            }
        })];
        let snapshot = GitHubIssueSnapshot {
            reference: GitHubIssueRef::from_value(&json!(
                "https://github.com/Redandan/AgoraMarket/issues/257"
            ))
            .unwrap(),
            state: "open".to_string(),
            title: Some("Buyer cart blocker".to_string()),
            updated_at: Some("2026-06-19T13:11:31Z".to_string()),
            synced_at: "2026-06-19T14:00:00Z".to_string(),
        };

        assert_eq!(apply_external_issue_snapshots(&mut records, &[snapshot]), 1);
        let digest = build_ops_schedule_digest(&records, &OpsReviewQueues::default(), None);

        assert_eq!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["status"],
            "waiting_external_fix"
        );
        assert_eq!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["openCount"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["dryRunDecision"]
                ["canRerunCheckoutDryRun"],
            false
        );
        assert_eq!(
            digest["repeatableOrderProgress"]["trackedIssues"][0]["state"],
            "open"
        );
        assert_eq!(
            digest["operationsCycleReport"]["capabilityGapPlan"][0]["status"],
            "operational"
        );
        assert_eq!(
            digest["operationsCycleReport"]["capabilityGapPlan"][1]["capability"],
            "approval_gated_handoff_execution"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorBrief"]["topCapabilityGap"],
            "approval_gated_handoff_execution"
        );
    }

    #[test]
    fn repeatable_order_digest_merges_latest_tracked_issue_snapshots() {
        let records = vec![
            json!({
                "id": "OPS-FIRST-FIVE",
                "status": "DONE",
                "last_finished_at": "2026-06-19T10:15:00Z",
                "last_result_status": "READY_FOR_REVIEW",
                "last_result": {
                    "first_order_activation": {
                        "goal": "FIRST_FIVE_PRODUCTION_ORDERS",
                        "status": "in_progress",
                        "summary": "first five still in progress",
                        "currentState": {
                            "productionOrders30d": 1,
                            "targetProductionOrders": 5,
                            "remainingProductionOrdersToTarget": 4,
                            "productionGmv30d": 33.8301,
                            "trackedIssues": [{
                                "url": "https://github.com/Redandan/AgoraMarket/issues/254",
                                "project": "AgoraMarket",
                                "number": 254,
                                "state": "open"
                            }, "AgoraMarket#257"]
                        },
                        "topBlockers": [{
                            "type": "repeatable_order_gap",
                            "summary": "Need more production orders"
                        }]
                    }
                }
            }),
            json!({
                "id": "OPS-ISSUE-SYNC",
                "status": "DONE",
                "last_finished_at": "2026-06-19T10:16:00Z",
                "last_result_status": "READY_FOR_REVIEW",
                "last_result": {
                    "ops_report": {
                        "actionSummary": {
                            "newIssueCandidates": 0,
                            "handoffOnlyCandidates": 0,
                            "trackedExternalIssues": 1,
                            "inboxEventsNeedingReview": 0,
                            "healthyChecks": 1,
                            "blockedChecks": 0
                        },
                        "trackedExternalIssues": [{
                            "externalIssue": {
                                "url": "https://github.com/Redandan/AgoraMarket/issues/257",
                                "project": "AgoraMarket",
                                "number": 257,
                                "state": "open"
                            }
                        }]
                    }
                }
            }),
        ];

        let digest = build_ops_schedule_digest(&records, &OpsReviewQueues::default(), None);
        assert_eq!(
            digest["repeatableOrderProgress"]["trackedIssues"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["trackedIssueCount"],
            2
        );
        assert_eq!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["openCount"],
            2
        );
        assert!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["number"] == 257)
        );
    }

    #[test]
    fn ops_schedule_digest_exposes_repeatable_order_progress() {
        let records = vec![json!({
            "id": "OPS-TEST-0110",
            "title": "Repeatable order activation",
            "task_type": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
            "status": "DONE",
            "updated_at": "2026-06-19T10:15:00Z",
            "last_finished_at": "2026-06-19T10:15:00Z",
            "last_result_status": "READY_FOR_REVIEW",
            "last_result": {
                "ops_report": {
                    "actionSummary": {
                        "status": "ready_for_handoff_review",
                        "newIssueCandidates": 0,
                        "handoffOnlyCandidates": 2,
                        "trackedExternalIssues": 1,
                        "inboxEventsNeedingReview": 0,
                        "healthyChecks": 11,
                        "blockedChecks": 0
                    }
                },
                "first_order_activation": {
                    "goal": "FIRST_FIVE_PRODUCTION_ORDERS",
                    "status": "activation_required",
                    "summary": "Repeatable-order target is not reached yet: 1/5 production orders in 30d.",
                    "currentState": {
                        "productionOrders30d": 1,
                        "targetProductionOrders": 5,
                        "remainingProductionOrdersToTarget": 4,
                        "productionGmv30d": 33.8301,
                        "pendingShipmentCount": 6,
                        "trackedIssues": [
                            "https://github.com/Redandan/AgoraMarket/issues/257"
                        ]
                    },
                    "topBlockers": [{
                        "type": "repeatable_order_gap",
                        "severity": "P0",
                        "summary": "Production orders are 1/5."
                    }],
                    "operationsCycleReport": {
                        "reportType": "AGORA_MARKET_REPEATABLE_ORDER_CYCLE",
                        "orderProgress": {
                            "productionOrders30d": 1,
                            "targetProductionOrders": 5,
                            "remainingProductionOrdersToTarget": 4,
                            "productionGmv30d": 33.8301
                        },
                        "localSchedule": {
                            "canContinue": true,
                            "recommendedCadence": "daily"
                        },
                        "sirinCapabilityGaps": [
                            "External issue status is still supplied through tracked issue context."
                        ]
                    }
                }
            }
        })];

        let digest = build_ops_schedule_digest(&records, &OpsReviewQueues::default(), None);
        assert_eq!(
            digest["repeatableOrderProgress"]["goal"],
            "FIRST_FIVE_PRODUCTION_ORDERS"
        );
        assert_eq!(digest["repeatableOrderProgress"]["productionOrders30d"], 1);
        assert_eq!(
            digest["repeatableOrderProgress"]["targetProductionOrders"],
            5
        );
        assert_eq!(
            digest["repeatableOrderProgress"]["remainingProductionOrdersToTarget"],
            4
        );
        assert_eq!(digest["repeatableOrderProgress"]["pendingShipmentCount"], 6);
        assert_eq!(
            digest["repeatableOrderProgress"]["largestBlocker"]["type"],
            "repeatable_order_gap"
        );
        assert_eq!(
            digest["repeatableOrderProgress"]["localSchedule"]["canContinue"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportType"],
            "AGORA_MARKET_OPERATIONS_CYCLE_DIGEST"
        );
        assert_eq!(
            digest["operationsCycleReport"]["orderProgress"]["productionOrders30d"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["orderProgress"]["targetProductionOrders"],
            5
        );
        assert_eq!(
            digest["operationsCycleReport"]["orderProgress"]["remainingProductionOrdersToTarget"],
            4
        );
        assert_eq!(
            digest["operationsCycleReport"]["largestBlocker"]["type"],
            "repeatable_order_gap"
        );
        assert_eq!(
            digest["operationsCycleReport"]["localSchedule"]["canContinue"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["scheduleContinuity"]["runnerMode"],
            "continue_scheduled_patrol"
        );
        assert_eq!(
            digest["operationsCycleReport"]["scheduleContinuity"]
                ["canContinueWithoutExternalMutation"],
            true
        );
        assert!(digest["operationsCycleReport"]["handoffPlan"]["sirin"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["pendingShipmentCount"],
            6
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["operatorApprovalRequired"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["approvalGatedActionCount"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["operationalReadiness"]["nextFocus"],
            "fulfillment_followup"
        );
        assert_eq!(
            digest["operationsCycleReport"]["conversionEvidence"]["productionOrders30d"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["conversionEvidence"]["productionGmv30d"],
            33.8301
        );
        assert_eq!(
            digest["operationsCycleReport"]["conversionEvidence"]["evidenceLevel"],
            "observed_from_latest_first_five_schedule"
        );
        assert!(digest["operationsCycleReport"]["operatorBrief"]["headline"]
            .as_str()
            .unwrap_or_default()
            .contains("1/5 production orders"));
        assert_eq!(
            digest["operationsCycleReport"]["operatorBrief"]["largestBlocker"]["type"],
            "repeatable_order_gap"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorBrief"]["nextFocus"],
            "fulfillment_followup"
        );
        assert_eq!(
            digest["operationsCycleReport"]["operatorBrief"]["topCapabilityGap"],
            "external_issue_state_sync"
        );
        assert_eq!(
            digest["operationsCycleReport"]["cycleExitCriteria"]["requiredProductionOrders30d"],
            5
        );
        assert_eq!(
            digest["operationsCycleReport"]["cycleCompletion"]["remainingProductionOrdersToTarget"],
            4
        );
        assert_eq!(
            digest["operationsCycleReport"]["cycleCompletion"]["missingRequirements"][0]
                ["requirement"],
            "Reach first five production orders"
        );
        assert_eq!(
            digest["operationsCycleReport"]["nextActionQueue"][0]["track"],
            "fulfillment_followup"
        );
        assert_eq!(
            digest["operationsCycleReport"]["nextActionQueue"][0]["executionGate"]
                ["blockedMutations"][0],
            "customer_message"
        );
        assert_eq!(
            digest["operationsCycleReport"]["sirinCapabilityGaps"][0],
            "External issue status is still supplied through tracked issue context."
        );
        assert_eq!(
            digest["operationsCycleReport"]["capabilityGapPlan"][0]["capability"],
            "external_issue_state_sync"
        );
        assert_eq!(
            digest["operationsCycleReport"]["capabilityGapPlan"][0]["priority"],
            "P0"
        );
        assert_eq!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["status"],
            "needs_issue_state_sync"
        );
        assert_eq!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["trackedIssueCount"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["unknownCount"],
            1
        );
        assert_eq!(
            digest["operationsCycleReport"]["externalIssueStateSync"]["dryRunDecision"]
                ["canRerunCheckoutDryRun"],
            false
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][5]["name"],
            "external_issue_state_sync"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["status"],
            "clear"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainPlan"]["totalItems"],
            0
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["status"],
            "clear"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["drainCompletionRatio"],
            1.0
        );
        assert_eq!(
            digest["operationsCycleReport"]["reviewDrainProgress"]["canAdvanceWithoutApproval"],
            true
        );
        assert!(
            digest["operationsCycleReport"]["capabilityGapPlan"][0]["boundary"]
                .as_str()
                .unwrap_or_default()
                .contains("Sirin-local capability planning only")
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["complete"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["passedCount"],
            digest["operationsCycleReport"]["reportCompleteness"]["requiredCount"]
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][0]["name"],
            "conversion_evidence"
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][0]["passed"],
            true
        );
        assert_eq!(
            digest["operationsCycleReport"]["reportCompleteness"]["checks"][4]["name"],
            "capability_gap_plan"
        );
    }

    #[test]
    fn ops_schedule_list_enriches_codex_review_action_status_from_ledger() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let _ = std::fs::remove_file(&path);

        let playbook = json!({
            "goal": "FIRST_PRODUCTION_ORDER",
            "status": "activation_required",
            "summary": "First production order is not achieved yet.",
            "currentState": {
                "trackedIssues": [
                    "https://github.com/Redandan/AgoraMarket/issues/254"
                ]
            },
            "topBlockers": [],
            "todayActions": [{
                "actor": "CODEX",
                "action": "Verify buyer path read-only",
                "expectedResult": "Buyer path is confirmed.",
                "verification": "Sirin active-smoke remains healthy."
            }]
        });
        let actions = crate::ops_action_ledger::persist_codex_review_actions(
            "OPS-TEST-0200:1",
            Some("OPS-TEST-0200"),
            &playbook,
        )
        .expect("persist action");
        let action_id = actions[0]["id"].as_str().expect("action id").to_string();
        crate::ops_action_ledger::call_ops_review_complete(json!({
            "actionId": action_id,
            "status": "DONE",
            "finding": "verified"
        }))
        .expect("complete action");

        let records = vec![json!({
            "id": "OPS-TEST-0200",
            "title": "First order activation",
            "task_type": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
            "status": "DONE",
            "updated_at": "2026-06-15T04:00:00Z",
            "last_finished_at": "2026-06-15T04:00:00Z",
            "last_result_status": "READY_FOR_REVIEW",
            "last_result": {
                "codex_review_actions": [{
                    "id": action_id,
                    "status": "PENDING_CODEX_REVIEW"
                }]
            }
        })];
        write_ops_schedule_records(&records).expect("write test schedules");

        let listed = call_ops_schedule_list(json!({
            "taskType": "AGORA_MARKET_FIRST_ORDER_ACTIVATION",
            "limit": 1
        }))
        .expect("list schedules");
        let action = &listed["schedules"][0]["last_result"]["codex_review_actions"][0];

        assert_eq!(action["snapshotStatus"], "PENDING_CODEX_REVIEW");
        assert_eq!(action["status"], "DONE");
        assert_eq!(action["ledgerStatus"], "DONE");
        assert_eq!(action["statusSource"], "ops_action_ledger");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ops_schedule_run_due_marks_failed_task_locally() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let _ = std::fs::remove_file(&path);

        let created = call_ops_schedule_create(json!({
            "title": "Unsupported local inspection",
            "taskType": "UNKNOWN_LOCAL_TASK",
            "cadence": "manual",
            "priority": "P2"
        }))
        .expect("create schedule");
        let schedule_id = created["schedule"]["id"].as_str().expect("id").to_string();

        let ran = call_ops_schedule_run_due(json!({
            "scheduleId": schedule_id,
            "limit": 1
        }))
        .await
        .expect("run due schedule");
        assert_eq!(ran["ran"], 1);
        assert_eq!(ran["results"][0]["status"], "FAILED");

        let listed = call_ops_schedule_list(json!({
            "status": "FAILED",
            "limit": 10
        }))
        .expect("list failed schedules");
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["schedules"][0]["status"], "FAILED");
        assert_eq!(listed["schedules"][0]["last_run_status"], "FAILED");
        assert!(listed["schedules"][0]["last_error"]
            .as_str()
            .unwrap_or_default()
            .contains("Unsupported A2A task_type"));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ops_schedule_run_due_returns_specified_schedule_when_not_due() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_schedule_path();
        let _ = std::fs::remove_file(&path);

        let scheduled_for = (Utc::now() + Duration::hours(1)).to_rfc3339();
        let created = call_ops_schedule_create(json!({
            "title": "Future local inspection",
            "taskType": "SIRIN_AGORA_TEST_HEALTH_SCOUT",
            "cadence": "manual",
            "priority": "P2",
            "scheduledFor": scheduled_for
        }))
        .expect("create schedule");
        let schedule_id = created["schedule"]["id"].as_str().expect("id").to_string();

        let ran = call_ops_schedule_run_due(json!({
            "scheduleId": schedule_id,
            "limit": 1
        }))
        .await
        .expect("run due schedule");
        assert_eq!(ran["ran"], 0);
        assert_eq!(ran["results"].as_array().map(Vec::len), Some(0));
        assert_eq!(ran["schedule"]["id"], schedule_id);
        assert_eq!(ran["schedule"]["status"], "SCHEDULED");
        assert!(ran["boundary"]
            .as_str()
            .unwrap_or_default()
            .contains("not due or was already handled"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_schedule_finished_stores_ops_report_status() {
        let mut record = json!({
            "id": "OPS-TEST",
            "status": "RUNNING",
            "cadence": "manual",
            "run_count": 0
        });
        let result = json!({
            "summary": "waiting for approval",
            "ops_report": {
                "reportStatus": "BLOCKED_INTEGRATION"
            }
        });

        set_schedule_finished(
            &mut record,
            "DONE",
            "2026-06-14T00:00:00Z",
            Some(&result),
            None,
            0,
        );

        assert_eq!(record["status"], "DONE");
        assert_eq!(record["last_run_status"], "DONE");
        assert_eq!(record["last_result_status"], "BLOCKED_INTEGRATION");
    }
}
