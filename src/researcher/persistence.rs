//! JSONL-backed persistence for research tasks.
//!
//! Single log file at `{app_data}/tracking/research.jsonl`.  `save_research`
//! updates an existing entry by id (atomic rewrite via `.tmp` swap) or
//! appends a new one.  `list_research` / `get_research` are read paths.
//! A process-wide mutex serialises writers so concurrent `save_research`
//! calls don't corrupt the file.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use super::ResearchTask;

fn research_log_path() -> PathBuf {
    crate::platform::app_data_dir().join("tracking").join("research.jsonl")
}

fn research_store_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn save_research(task: &ResearchTask) -> Result<(), String> {
    let _guard = research_store_lock()
        .lock()
        .map_err(|_| "research store lock poisoned".to_string())?;

    let path = research_log_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // Load all lines, replace matching id, rewrite.
    let existing: Vec<String> = if path.exists() {
        let file = fs::File::open(&path).map_err(|e| e.to_string())?;
        BufReader::new(file)
            .lines()
            .filter_map(|l| l.ok())
            .filter(|l| !l.trim().is_empty())
            .collect()
    } else {
        Vec::new()
    };

    let new_line = serde_json::to_string(task).map_err(|e| e.to_string())?;
    let mut found = false;
    let mut updated: Vec<String> = existing
        .into_iter()
        .map(|line| {
            if let Ok(t) = serde_json::from_str::<ResearchTask>(&line) {
                if t.id == task.id {
                    found = true;
                    return new_line.clone();
                }
            }
            line
        })
        .collect();

    if !found {
        updated.push(new_line);
    }

    let tmp = path.with_extension("jsonl.tmp");
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    for line in &updated {
        writeln!(f, "{line}").map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn list_research() -> Result<Vec<ResearchTask>, String> {
    let _guard = research_store_lock()
        .lock()
        .map_err(|_| "research store lock poisoned".to_string())?;

    let path = research_log_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(&path).map_err(|e| e.to_string())?;
    Ok(BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<ResearchTask>(&l).ok())
        .collect())
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
