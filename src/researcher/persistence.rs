//! JSONL-backed persistence for research tasks.
//!
//! Single log file at `{app_data}/tracking/research.jsonl`, backed by
//! [`crate::jsonl_log::JsonlLog`].  `save_research` upserts by task id;
//! `list_research` / `get_research` are read paths.
//!
//! Concurrency: the shared [`RESEARCH_LOG`] is a process-wide singleton, so
//! concurrent writers serialise through the underlying JsonlLog mutex.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::jsonl_log::JsonlLog;

use super::ResearchTask;

fn research_log() -> &'static JsonlLog<ResearchTask> {
    static LOG: OnceLock<JsonlLog<ResearchTask>> = OnceLock::new();
    LOG.get_or_init(|| JsonlLog::new(research_log_path()))
}

fn research_log_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("tracking")
        .join("research.jsonl")
}

pub fn save_research(task: &ResearchTask) -> Result<(), String> {
    research_log()
        .upsert_by(task.clone(), |t| t.id.clone())
        .map_err(|e| e.to_string())
}

pub fn list_research() -> Result<Vec<ResearchTask>, String> {
    research_log().read_all().map_err(|e| e.to_string())
}

pub fn get_research(id: &str) -> Result<Option<ResearchTask>, String> {
    Ok(list_research()?.into_iter().find(|t| t.id == id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::researcher::{ResearchStatus, ResearchStep};

    fn make_task(id: &str, status: ResearchStatus) -> ResearchTask {
        ResearchTask {
            id: id.to_string(),
            topic: format!("test topic {id}"),
            url: None,
            status,
            steps: vec![ResearchStep {
                phase: "overview".into(),
                output: "Test output".into(),
            }],
            final_report: Some("Test report".into()),
            started_at: chrono::Utc::now().to_rfc3339(),
            finished_at: Some(chrono::Utc::now().to_rfc3339()),
        }
    }

    #[test]
    fn persistence_save_and_get() {
        let id = format!("unit-{}", chrono::Utc::now().timestamp_millis());
        let task = make_task(&id, ResearchStatus::Done);
        save_research(&task).expect("save failed");

        let found = get_research(&id).expect("get failed").expect("not found");
        assert_eq!(found.id, id);
        assert_eq!(found.final_report.as_deref(), Some("Test report"));

        println!("✅ save → get roundtrip OK (id={id})");
    }

    #[test]
    fn persistence_update_overwrites() {
        let id = format!("upd-{}", chrono::Utc::now().timestamp_millis());

        let mut task = make_task(&id, ResearchStatus::Running);
        task.final_report = None;
        save_research(&task).expect("initial save failed");

        task.status = ResearchStatus::Done;
        task.final_report = Some("Updated".into());
        save_research(&task).expect("update failed");

        let all = list_research().expect("list failed");
        let matches: Vec<_> = all.iter().filter(|t| t.id == id).collect();
        assert_eq!(matches.len(), 1, "expected 1 entry, got {}", matches.len());
        assert_eq!(matches[0].status, ResearchStatus::Done);
        assert_eq!(matches[0].final_report.as_deref(), Some("Updated"));

        println!("✅ update/overwrite OK (id={id})");
    }

    #[test]
    fn persistence_list_contains_saved() {
        let id = format!("lst-{}", chrono::Utc::now().timestamp_millis());
        let task = make_task(&id, ResearchStatus::Done);
        save_research(&task).expect("save failed");

        let list = list_research().expect("list failed");
        assert!(
            list.iter().any(|t| t.id == id),
            "saved task not found in list"
        );
        println!("✅ list contains saved task (id={id})");
    }
}
