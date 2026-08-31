//! Sirin-local operations action ledger.
//!
//! The ledger is the boundary between Sirin-generated operations playbooks and
//! downstream Codex/ChatGPT/operator review. It stores local tasks only; it
//! never calls external agents or mutates AgoraMarket/API state.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use chrono::{Local, Utc};
use serde_json::{json, Value};

const OPS_ACTION_LEDGER_FILE: &str = "ops_action_ledger.jsonl";

pub(crate) fn persist_codex_review_actions(
    source_task_id: &str,
    source_schedule_id: Option<&str>,
    playbook: &Value,
) -> Result<Vec<Value>, String> {
    let actions = playbook
        .get("todayActions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let current_state = playbook
        .get("currentState")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let top_blockers = playbook
        .get("topBlockers")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let mut records = load_ops_action_records()?;
    let mut persisted = Vec::new();

    for action in actions {
        let actor = action
            .get("actor")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !actor.eq_ignore_ascii_case("CODEX") {
            continue;
        }
        let action_text = action
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("Codex review requested");
        let dedupe_key = codex_review_dedupe_key(source_schedule_id, action_text, &current_state);
        let now = Utc::now().to_rfc3339();
        let existing_idx = records.iter().position(|record| {
            record
                .get("dedupeKey")
                .and_then(Value::as_str)
                .is_some_and(|key| key == dedupe_key)
                || codex_review_context_matches(record, action_text, &current_state)
        });
        let record = json!({
            "id": existing_idx
                .and_then(|idx| records.get(idx))
                .and_then(|record| record.get("id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| next_action_id(&records)),
            "schemaVersion": 1,
            "type": "CODEX_REVIEW",
            "status": "PENDING_CODEX_REVIEW",
            "actor": "CODEX",
            "title": "Codex review: first-order buyer path",
            "action": action_text,
            "expectedResult": action.get("expectedResult").cloned().unwrap_or(Value::Null),
            "verification": action.get("verification").cloned().unwrap_or(Value::Null),
            "source": "sirin.first_order_activation",
            "sourceTaskId": source_task_id,
            "sourceScheduleId": source_schedule_id
                .map(|id| Value::String(id.to_string()))
                .unwrap_or(Value::Null),
            "targetProject": "AgoraMarket",
            "dedupeKey": dedupe_key,
            "evidence": {
                "goal": playbook.get("goal").cloned().unwrap_or(Value::Null),
                "playbookStatus": playbook.get("status").cloned().unwrap_or(Value::Null),
                "summary": playbook.get("summary").cloned().unwrap_or(Value::Null),
                "currentState": current_state,
                "topBlockers": top_blockers,
            },
            "boundary": "Codex must verify read-only first. Sirin-owned fixes may modify Sirin; AgoraMarket/AgoraMarketAPI findings should be handled by issue/handoff only unless explicitly approved.",
            "expectedOutput": [
                "is_real_issue",
                "evidence",
                "recommended_next_action",
                "issue_to_open_or_update"
            ],
            "createdAt": now,
            "updatedAt": now,
            "claimedBy": null,
            "claimedAt": null,
            "completedAt": null,
            "result": null,
            "notes": []
        });
        if let Some(idx) = existing_idx {
            records[idx]["updatedAt"] = json!(now);
            records[idx]["action"] = record["action"].clone();
            records[idx]["expectedResult"] = record["expectedResult"].clone();
            records[idx]["verification"] = record["verification"].clone();
            records[idx]["evidence"] = record["evidence"].clone();
            records[idx]["dedupeKey"] = record["dedupeKey"].clone();
            persisted.push(records[idx].clone());
        } else {
            records.push(record.clone());
            persisted.push(record);
        }
    }

    if !persisted.is_empty() {
        write_ops_action_records(&records)?;
    }
    Ok(persisted)
}

pub(crate) fn call_ops_review_queue_list(args: Value) -> Result<Value, String> {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let status = normalize_review_queue_status(
        arg_text(&args, "status")
            .unwrap_or_else(|| "PENDING_CODEX_REVIEW".into())
            .as_str(),
    );
    let actor = arg_text(&args, "actor");
    let target_project =
        arg_text(&args, "targetProject").or_else(|| arg_text(&args, "target_project"));

    let mut records = load_ops_action_records()?;
    records.sort_by(|a, b| {
        b.get("updatedAt")
            .and_then(Value::as_str)
            .cmp(&a.get("updatedAt").and_then(Value::as_str))
    });
    let actions: Vec<Value> = records
        .into_iter()
        .filter(|record| {
            status == "ALL"
                || record
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(&status))
        })
        .filter(|record| {
            actor.as_deref().is_none_or(|expected| {
                record
                    .get("actor")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
        })
        .filter(|record| {
            target_project.as_deref().is_none_or(|expected| {
                record
                    .get("targetProject")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
            })
        })
        .take(limit)
        .collect();

    Ok(json!({
        "count": actions.len(),
        "actions": actions,
        "storage_path": ops_action_ledger_path(),
        "boundary": "Sirin-local review queue only; listing does not call Codex, GitHub, ChatGPT, or AgoraMarket/API."
    }))
}

fn normalize_review_queue_status(status: &str) -> String {
    match status.trim().to_ascii_uppercase().as_str() {
        "PENDING" => "PENDING_CODEX_REVIEW".into(),
        other => other.to_string(),
    }
}

pub(crate) fn action_records_by_id() -> Result<HashMap<String, Value>, String> {
    Ok(load_ops_action_records()?
        .into_iter()
        .filter_map(|record| {
            let id = record.get("id").and_then(Value::as_str)?.to_string();
            Some((id, record))
        })
        .collect())
}

pub(crate) fn call_ops_review_claim(args: Value) -> Result<Value, String> {
    let id = arg_text(&args, "actionId")
        .or_else(|| arg_text(&args, "id"))
        .ok_or_else(|| "actionId is required".to_string())?;
    let claimed_by = arg_text(&args, "claimedBy").unwrap_or_else(|| "codex".into());
    update_action_record(&id, |record, now| {
        let status = record
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(status, "PENDING_CODEX_REVIEW" | "CLAIMED" | "IN_PROGRESS") {
            return Err(format!(
                "action {id} cannot be claimed from status {status}"
            ));
        }
        record["status"] = json!("CLAIMED");
        record["claimedBy"] = json!(claimed_by);
        record["claimedAt"] = json!(now);
        record["updatedAt"] = json!(now);
        Ok(())
    })
    .map(|action| {
        json!({
            "claimed": true,
            "action": action,
            "boundary": "Only Sirin-local action metadata was updated."
        })
    })
}

pub(crate) fn call_ops_review_update(args: Value) -> Result<Value, String> {
    let id = arg_text(&args, "actionId")
        .or_else(|| arg_text(&args, "id"))
        .ok_or_else(|| "actionId is required".to_string())?;
    let status = arg_text(&args, "status")
        .unwrap_or_else(|| "IN_PROGRESS".into())
        .to_ascii_uppercase();
    let status = normalize_review_queue_status(&status);
    if !matches!(
        status.as_str(),
        "PENDING_CODEX_REVIEW" | "CLAIMED" | "IN_PROGRESS" | "NEEDS_MORE_EVIDENCE" | "BLOCKED"
    ) {
        return Err("status must be PENDING_CODEX_REVIEW, CLAIMED, IN_PROGRESS, NEEDS_MORE_EVIDENCE, or BLOCKED".into());
    }
    let note = arg_text(&args, "note");
    update_action_record(&id, |record, now| {
        record["status"] = json!(status);
        record["updatedAt"] = json!(now);
        if let Some(note) = note.clone() {
            push_note(record, &now, &note);
        }
        Ok(())
    })
    .map(|action| {
        json!({
            "updated": true,
            "action": action,
            "boundary": "Only Sirin-local action metadata was updated."
        })
    })
}

pub(crate) fn call_ops_review_complete(args: Value) -> Result<Value, String> {
    let id = arg_text(&args, "actionId")
        .or_else(|| arg_text(&args, "id"))
        .ok_or_else(|| "actionId is required".to_string())?;
    let result = args
        .get("result")
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "finding": arg_text(&args, "finding").unwrap_or_else(|| "completed".into()),
                "evidence": arg_text(&args, "evidence").unwrap_or_default(),
                "recommendedNextAction": arg_text(&args, "recommendedNextAction")
                    .or_else(|| arg_text(&args, "recommended_next_action"))
                    .unwrap_or_default(),
                "issueToOpenOrUpdate": arg_text(&args, "issueToOpenOrUpdate")
                    .or_else(|| arg_text(&args, "issue_to_open_or_update"))
                    .unwrap_or_default()
            })
        });
    let status = arg_text(&args, "status")
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_else(|| "VERIFIED".into());
    if !matches!(
        status.as_str(),
        "VERIFIED" | "DONE" | "REJECTED" | "NEEDS_MORE_EVIDENCE"
    ) {
        return Err("status must be VERIFIED, DONE, REJECTED, or NEEDS_MORE_EVIDENCE".into());
    }
    update_action_record(&id, |record, now| {
        record["status"] = json!(status);
        record["result"] = result.clone();
        record["completedAt"] = json!(now);
        record["updatedAt"] = json!(now);
        Ok(())
    })
    .map(|action| {
        json!({
            "completed": true,
            "action": action,
            "boundary": "Codex result was written back to Sirin-local action ledger only."
        })
    })
}

fn update_action_record<F>(id: &str, apply: F) -> Result<Value, String>
where
    F: FnOnce(&mut Value, String) -> Result<(), String>,
{
    let mut records = load_ops_action_records()?;
    let idx = records
        .iter()
        .position(|record| {
            record
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|actual| actual == id)
        })
        .ok_or_else(|| format!("ops action not found: {id}"))?;
    let now = Utc::now().to_rfc3339();
    apply(&mut records[idx], now)?;
    let action = records[idx].clone();
    write_ops_action_records(&records)?;
    Ok(action)
}

fn push_note(record: &mut Value, at: &str, note: &str) {
    let entry = json!({ "at": at, "note": note });
    if let Some(notes) = record.get_mut("notes").and_then(Value::as_array_mut) {
        notes.push(entry);
    } else if let Some(obj) = record.as_object_mut() {
        obj.insert("notes".into(), json!([entry]));
    }
}

fn codex_review_dedupe_key(
    source_schedule_id: Option<&str>,
    action_text: &str,
    current_state: &Value,
) -> String {
    let _ = source_schedule_id;
    let (tracked_issues, known_blocker) = codex_review_context_signature(current_state);
    let primary_issue = tracked_issues
        .split('|')
        .next()
        .unwrap_or("no-tracked-issue");
    let context_hash = stable_hash(&format!("{tracked_issues}|{known_blocker}"));
    format!(
        "first-order:codex-review:{}:{}:{}",
        sanitize_key_part(primary_issue),
        context_hash,
        stable_hash(action_text)
    )
}

fn codex_review_context_matches(record: &Value, action_text: &str, current_state: &Value) -> bool {
    if record.get("type").and_then(Value::as_str) != Some("CODEX_REVIEW") {
        return false;
    }
    if !record
        .get("targetProject")
        .and_then(Value::as_str)
        .is_some_and(|project| project.eq_ignore_ascii_case("AgoraMarket"))
    {
        return false;
    }
    if record.get("action").and_then(Value::as_str) != Some(action_text) {
        return false;
    }
    let Some(previous_state) = record.pointer("/evidence/currentState") else {
        return false;
    };
    codex_review_context_signature(previous_state) == codex_review_context_signature(current_state)
}

fn codex_review_context_signature(current_state: &Value) -> (String, String) {
    let tracked_issues = current_state
        .get("trackedIssues")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join("|")
        })
        .filter(|issues| !issues.is_empty())
        .unwrap_or_else(|| "no-tracked-issue".to_string());
    let known_blocker = current_state
        .get("knownBlocker")
        .or_else(|| current_state.get("known_blocker"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (tracked_issues, known_blocker)
}

fn sanitize_key_part(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-")
}

fn stable_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn next_action_id(records: &[Value]) -> String {
    let date_str = Local::now().format("%Y%m%d").to_string();
    let seq = records
        .iter()
        .filter(|record| {
            record
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with(&format!("OPS-ACTION-{date_str}")))
        })
        .count()
        + 1;
    format!("OPS-ACTION-{date_str}-{seq:04}")
}

fn ops_action_ledger_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("tracking")
        .join(OPS_ACTION_LEDGER_FILE)
}

fn load_ops_action_records() -> Result<Vec<Value>, String> {
    let path = ops_action_ledger_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("read ops action ledger {} failed: {e}", path.display()))?;
    let mut rows = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "parse ops action ledger {} line {} failed: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        rows.push(value);
    }
    Ok(rows)
}

fn write_ops_action_records(records: &[Value]) -> Result<(), String> {
    let path = ops_action_ledger_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create ops action ledger dir {} failed: {e}",
                parent.display()
            )
        })?;
    }
    let tmp = path.with_extension(format!(
        "jsonl.{}.{}.tmp",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let mut file = std::fs::File::create(&tmp)
        .map_err(|e| format!("create ops action ledger {} failed: {e}", tmp.display()))?;
    for record in records {
        let line = serde_json::to_string(record)
            .map_err(|e| format!("serialize ops action ledger failed: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("write ops action ledger failed: {e}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        format!(
            "replace ops action ledger {} with {} failed: {e}",
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
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn sample_playbook() -> Value {
        json!({
            "goal": "FIRST_PRODUCTION_ORDER",
            "status": "activation_required",
            "summary": "First production order is not achieved yet.",
            "currentState": {
                "productionOrders30d": 0,
                "productionGmv30d": 0.0,
                "onSaleProducts": 357,
                "trackedIssues": ["https://github.com/Redandan/AgoraMarket/issues/254"]
            },
            "topBlockers": [
                { "type": "conversion_gap", "severity": "P0" }
            ],
            "todayActions": [
                { "actor": "OPERATOR", "action": "Publish CTA" },
                {
                    "actor": "CODEX",
                    "action": "Verify buyer path read-only",
                    "expectedResult": "Buyer path is confirmed.",
                    "verification": "Active-smoke remains healthy."
                }
            ]
        })
    }

    #[test]
    fn persist_codex_review_actions_creates_only_codex_action() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_action_ledger_path();
        let _ = std::fs::remove_file(&path);

        let actions =
            persist_codex_review_actions("OPS-TEST:123", Some("OPS-TEST"), &sample_playbook())
                .expect("persist actions");

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["type"], "CODEX_REVIEW");
        assert_eq!(actions[0]["status"], "PENDING_CODEX_REVIEW");
        assert_eq!(actions[0]["targetProject"], "AgoraMarket");

        let listed = call_ops_review_queue_list(json!({ "status": "PENDING_CODEX_REVIEW" }))
            .expect("list queue");
        assert_eq!(listed["count"], 1);
        assert_eq!(
            listed["actions"][0]["action"],
            "Verify buyer path read-only"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn codex_review_dedupe_key_changes_with_blocker_context() {
        let base = json!({
            "trackedIssues": [
                "https://github.com/Redandan/AgoraMarket/issues/254"
            ]
        });
        let with_backend_blocker = json!({
            "trackedIssues": [
                "https://github.com/Redandan/AgoraMarket/issues/254",
                "https://github.com/Redandan/AgoraMarket/issues/256",
                "https://github.com/Redandan/AgoraMarketAPI/issues/564"
            ],
            "knownBlocker": "Await AgoraMarketAPI#564 fix for TH address persistence."
        });

        let key_a = codex_review_dedupe_key(Some("OPS-TEST"), "Verify buyer path read-only", &base);
        let key_b = codex_review_dedupe_key(
            Some("OPS-TEST"),
            "Verify buyer path read-only",
            &with_backend_blocker,
        );

        assert_ne!(key_a, key_b);
        assert!(key_b.contains("redandan-agoramarket-issues-254"));
    }

    #[test]
    fn codex_review_dedupe_key_is_stable_across_heartbeat_schedules() {
        let current_state = json!({
            "trackedIssues": [
                "https://github.com/Redandan/AgoraMarket/issues/254",
                "https://github.com/Redandan/AgoraMarket/issues/257"
            ],
            "knownBlocker": "ACTIVE: AgoraMarket#257 remains OPEN."
        });

        let key_a = codex_review_dedupe_key(
            Some("OPS-HEARTBEAT-0001"),
            "Verify buyer path read-only",
            &current_state,
        );
        let key_b = codex_review_dedupe_key(
            Some("OPS-HEARTBEAT-0002"),
            "Verify buyer path read-only",
            &current_state,
        );

        assert_eq!(key_a, key_b);
        assert!(key_a.contains("redandan-agoramarket-issues-254"));
    }

    #[test]
    fn persist_codex_review_actions_dedupes_same_blocker_across_schedules() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_action_ledger_path();
        let _ = std::fs::remove_file(&path);

        let playbook = json!({
            "goal": "FIRST_PRODUCTION_ORDER",
            "status": "activation_required",
            "summary": "First production order is not achieved yet.",
            "currentState": {
                "trackedIssues": [
                    "https://github.com/Redandan/AgoraMarket/issues/254",
                    "https://github.com/Redandan/AgoraMarket/issues/257"
                ],
                "knownBlocker": "ACTIVE: AgoraMarket#257 remains OPEN."
            },
            "topBlockers": [],
            "todayActions": [{
                "actor": "CODEX",
                "action": "Verify buyer path read-only",
                "expectedResult": "Buyer path is confirmed.",
                "verification": "Sirin active-smoke remains healthy."
            }]
        });

        let first = persist_codex_review_actions(
            "OPS-HEARTBEAT-0001:1",
            Some("OPS-HEARTBEAT-0001"),
            &playbook,
        )
        .expect("first persist");
        let id = first[0]["id"].as_str().expect("id").to_string();
        call_ops_review_complete(json!({
            "actionId": id,
            "status": "VERIFIED",
            "result": {
                "is_real_issue": true,
                "recommended_next_action": "Keep tracking #257."
            }
        }))
        .expect("complete first");

        let second = persist_codex_review_actions(
            "OPS-HEARTBEAT-0002:1",
            Some("OPS-HEARTBEAT-0002"),
            &playbook,
        )
        .expect("second persist");
        assert_eq!(second[0]["id"], id);
        assert_eq!(second[0]["status"], "VERIFIED");

        let pending =
            call_ops_review_queue_list(json!({ "status": "PENDING" })).expect("pending queue");
        assert_eq!(pending["count"], 0);

        let all = call_ops_review_queue_list(json!({ "status": "ALL" })).expect("all queue");
        assert_eq!(all["count"], 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_codex_review_actions_matches_legacy_schedule_key_by_context() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_action_ledger_path();
        let _ = std::fs::remove_file(&path);

        let current_state = json!({
            "trackedIssues": [
                "https://github.com/Redandan/AgoraMarket/issues/254",
                "https://github.com/Redandan/AgoraMarket/issues/257"
            ],
            "knownBlocker": "ACTIVE: AgoraMarket#257 remains OPEN."
        });
        let old_key = "first-order:codex-review:OPS-OLD:https-github-com-redandan-agoramarket-issues-254:legacy:64d8a2f3b9e6fbe0";
        let old_record = json!({
            "id": "OPS-ACTION-LEGACY-0001",
            "schemaVersion": 1,
            "type": "CODEX_REVIEW",
            "status": "VERIFIED",
            "actor": "CODEX",
            "targetProject": "AgoraMarket",
            "action": "Verify buyer path read-only",
            "dedupeKey": old_key,
            "evidence": {
                "currentState": current_state,
                "topBlockers": []
            },
            "result": {
                "is_real_issue": true
            },
            "updatedAt": "2026-06-16T09:00:00Z"
        });
        write_ops_action_records(&[old_record]).expect("write legacy record");

        let playbook = json!({
            "goal": "FIRST_PRODUCTION_ORDER",
            "status": "activation_required",
            "summary": "First production order is not achieved yet.",
            "currentState": current_state,
            "topBlockers": [],
            "todayActions": [{
                "actor": "CODEX",
                "action": "Verify buyer path read-only",
                "expectedResult": "Buyer path is confirmed.",
                "verification": "Sirin active-smoke remains healthy."
            }]
        });

        let persisted = persist_codex_review_actions(
            "OPS-HEARTBEAT-NEW:1",
            Some("OPS-HEARTBEAT-NEW"),
            &playbook,
        )
        .expect("persist with legacy match");

        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0]["id"], "OPS-ACTION-LEGACY-0001");
        assert_eq!(persisted[0]["status"], "VERIFIED");
        assert_ne!(persisted[0]["dedupeKey"], old_key);
        assert_eq!(
            persisted[0]["dedupeKey"],
            codex_review_dedupe_key(
                Some("OPS-HEARTBEAT-NEW"),
                "Verify buyer path read-only",
                &playbook["currentState"],
            )
        );

        let pending =
            call_ops_review_queue_list(json!({ "status": "PENDING" })).expect("pending queue");
        assert_eq!(pending["count"], 0);

        let all = call_ops_review_queue_list(json!({ "status": "ALL" })).expect("all queue");
        assert_eq!(all["count"], 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn review_claim_update_complete_round_trip() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_action_ledger_path();
        let _ = std::fs::remove_file(&path);

        let actions =
            persist_codex_review_actions("OPS-TEST:123", Some("OPS-TEST"), &sample_playbook())
                .expect("persist actions");
        let id = actions[0]["id"].as_str().expect("id").to_string();

        let claimed = call_ops_review_claim(json!({
            "actionId": id,
            "claimedBy": "codex"
        }))
        .expect("claim");
        assert_eq!(claimed["action"]["status"], "CLAIMED");
        assert_eq!(claimed["action"]["claimedBy"], "codex");

        let updated = call_ops_review_update(json!({
            "actionId": id,
            "status": "IN_PROGRESS",
            "note": "checking buyer path"
        }))
        .expect("update");
        assert_eq!(updated["action"]["status"], "IN_PROGRESS");
        assert_eq!(updated["action"]["notes"][0]["note"], "checking buyer path");

        let completed = call_ops_review_complete(json!({
            "actionId": id,
            "status": "VERIFIED",
            "result": {
                "is_real_issue": true,
                "recommended_next_action": "Open or update issue"
            }
        }))
        .expect("complete");
        assert_eq!(completed["action"]["status"], "VERIFIED");
        assert_eq!(completed["action"]["result"]["is_real_issue"], true);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn review_queue_pending_alias_lists_pending_codex_review_actions() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = ops_action_ledger_path();
        let _ = std::fs::remove_file(&path);

        let actions =
            persist_codex_review_actions("OPS-TEST:123", Some("OPS-TEST"), &sample_playbook())
                .expect("persist actions");
        let id = actions[0]["id"].as_str().expect("id").to_string();

        let default_list = call_ops_review_queue_list(json!({})).expect("default list");
        assert_eq!(default_list["count"], 1);
        assert_eq!(default_list["actions"][0]["id"], id);

        let pending_alias = call_ops_review_queue_list(json!({
            "status": "PENDING"
        }))
        .expect("pending alias list");
        assert_eq!(pending_alias["count"], 1);
        assert_eq!(
            pending_alias["actions"][0]["status"],
            "PENDING_CODEX_REVIEW"
        );

        let _ = std::fs::remove_file(&path);
    }
}
