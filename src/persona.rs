use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfessionalTone {
    Brief,
    Detailed,
    Casual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub professional_tone: ProfessionalTone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoiThresholds {
    pub min_usd_to_notify: f64,
    pub min_usd_to_call_remote_llm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseStyle {
    #[serde(default = "default_voice")]
    pub voice: String,
    #[serde(default = "default_ack_prefix")]
    pub ack_prefix: String,
    #[serde(default = "default_compliance_line")]
    pub compliance_line: String,
}

impl Default for ResponseStyle {
    fn default() -> Self {
        Self {
            voice: default_voice(),
            ack_prefix: default_ack_prefix(),
            compliance_line: default_compliance_line(),
        }
    }
}

/// Configuration for the local AI Coding Agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingAgentConfig {
    /// Whether the coding agent is enabled at all.
    #[serde(default = "default_coding_enabled")]
    pub enabled: bool,
    /// Root directory that file operations are allowed to touch.
    /// Relative paths are resolved from the process working directory.
    #[serde(default = "default_coding_project_root")]
    pub project_root: String,
    /// Skip user confirmation for read-only operations.
    #[serde(default = "default_true")]
    pub auto_approve_reads: bool,
    /// When false, the UI will show a confirmation dialog before any file write.
    #[serde(default)]
    pub auto_approve_writes: bool,
    /// Shell commands (exact prefix match) the agent is allowed to execute.
    #[serde(default = "default_allowed_commands")]
    pub allowed_commands: Vec<String>,
    /// Maximum number of ReAct loop iterations per task.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// Maximum bytes that a single file write may contain.
    #[serde(default = "default_max_file_write_bytes")]
    pub max_file_write_bytes: usize,
}

fn default_coding_enabled() -> bool {
    true
}
fn default_coding_project_root() -> String {
    ".".to_string()
}
fn default_true() -> bool {
    true
}
fn default_allowed_commands() -> Vec<String> {
    vec![
        "cargo check".to_string(),
        "cargo test".to_string(),
        "cargo build --release".to_string(),
    ]
}
fn default_max_iterations() -> usize {
    10
}
fn default_max_file_write_bytes() -> usize {
    102_400
}

impl Default for CodingAgentConfig {
    fn default() -> Self {
        Self {
            enabled: default_coding_enabled(),
            project_root: default_coding_project_root(),
            auto_approve_reads: default_true(),
            auto_approve_writes: false,
            allowed_commands: default_allowed_commands(),
            max_iterations: default_max_iterations(),
            max_file_write_bytes: default_max_file_write_bytes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub identity: Identity,
    pub objectives: Vec<String>,
    pub roi_thresholds: RoiThresholds,
    #[serde(default)]
    pub response_style: ResponseStyle,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub coding_agent: CodingAgentConfig,
    /// When true, escalation to a remote/large model is suppressed at runtime.
    /// The main LLM backend (set via `LLM_PROVIDER` env var) is still used.
    #[serde(default)]
    pub disable_remote_ai: bool,
}

fn default_version() -> String {
    "1.0".to_string()
}

fn default_voice() -> String {
    "自然、禮貌、專業".to_string()
}

fn default_ack_prefix() -> String {
    "已收到你的訊息。".to_string()
}

fn default_compliance_line() -> String {
    "我會按照你的要求處理。".to_string()
}

impl Persona {
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let content = fs::read_to_string("config/persona.yaml")?;
        let persona = serde_yaml::from_str(&content)?;
        Ok(persona)
    }

    pub fn name(&self) -> &str {
        &self.identity.name
    }

    pub fn objective_match(&self, text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        self.objectives
            .iter()
            .find(|o| lower.contains(&o.to_lowercase()))
            .cloned()
    }
}

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub source: String,
    pub msg: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionTier {
    Ignore,
    LocalProcess,
    Escalate,
}

#[derive(Debug, Clone)]
pub struct BehaviorDecision {
    pub draft: String,
    pub high_priority: bool,
    pub matched_objective: Option<String>,
    pub tier: ActionTier,
    pub reason: String,
}

pub struct BehaviorEngine;

pub fn determine_action_tier(estimated_value: f64, p: &Persona) -> ActionTier {
    if estimated_value < p.roi_thresholds.min_usd_to_notify {
        ActionTier::Ignore
    } else if estimated_value > p.roi_thresholds.min_usd_to_call_remote_llm {
        ActionTier::Escalate
    } else {
        ActionTier::LocalProcess
    }
}

pub fn generate_response_draft(msg: String, p: &Persona) -> String {
    let high_priority = p.objective_match(&msg).is_some();

    match p.identity.professional_tone {
        ProfessionalTone::Brief => {
            let mut base = if msg.len() > 64 {
                format!("已收到，重點：{}...", &msg[..64])
            } else {
                format!("已收到：{msg}")
            };
            if high_priority {
                base.push_str("（高優先）");
            }
            base
        }
        ProfessionalTone::Detailed => {
            let priority = if high_priority { "高" } else { "一般" };
            format!(
                "已收到訊息，將依 Persona 目標進行分析。\n優先級：{priority}\n內容：{msg}\n下一步：評估 ROI 後決定 Ignore / LocalProcess / Escalate。"
            )
        }
        ProfessionalTone::Casual => {
            if high_priority {
                format!("收到，這題很重要，我先優先看：{msg}")
            } else {
                format!("OK 收到，我來處理：{msg}")
            }
        }
    }
}

impl BehaviorEngine {
    pub fn evaluate(msg: IncomingMessage, estimated_value: f64, p: &Persona) -> BehaviorDecision {
        let matched_objective = p.objective_match(&msg.msg);
        let high_priority = matched_objective.is_some();
        let draft = generate_response_draft(msg.msg.clone(), p);
        let tier = determine_action_tier(estimated_value, p);

        let threshold_reason = match tier {
            ActionTier::Ignore => format!(
                "estimated_value={estimated_value:.2} < min_usd_to_notify={:.2}",
                p.roi_thresholds.min_usd_to_notify
            ),
            ActionTier::LocalProcess => format!(
                "{:.2} <= estimated_value={estimated_value:.2} <= {:.2}",
                p.roi_thresholds.min_usd_to_notify, p.roi_thresholds.min_usd_to_call_remote_llm
            ),
            ActionTier::Escalate => format!(
                "estimated_value={estimated_value:.2} > min_usd_to_call_remote_llm={:.2}",
                p.roi_thresholds.min_usd_to_call_remote_llm
            ),
        };

        let objective_reason = if let Some(obj) = matched_objective.as_ref() {
            format!("matched objective='{obj}'")
        } else {
            "no objective matched".to_string()
        };

        let reason = format!(
            "persona='{}', source='{}', {objective_reason}, {threshold_reason}",
            p.name(),
            msg.source
        );

        BehaviorDecision {
            draft,
            high_priority,
            matched_objective,
            tier,
            reason,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEntry {
    pub timestamp: String,
    pub event: String,
    pub persona: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_remote_ai: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_profit_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_tier: Option<ActionTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_priority: Option<bool>,
}

impl TaskEntry {
    pub fn heartbeat(persona_name: &str) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: "heartbeat".to_string(),
            persona: persona_name.to_string(),
            correlation_id: None,
            message_preview: None,
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: None,
            reason: None,
            action_tier: None,
            high_priority: None,
        }
    }

    pub fn ai_decision(persona_name: &str, message_preview: Option<String>) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: "ai_decision".to_string(),
            persona: persona_name.to_string(),
            correlation_id: None,
            message_preview,
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: None,
            reason: None,
            action_tier: None,
            high_priority: None,
        }
    }

    pub fn behavior_decision(
        persona: &Persona,
        estimated_value: f64,
        decision: &BehaviorDecision,
    ) -> Self {
        let status = match decision.tier {
            ActionTier::Ignore => Some("DONE".to_string()),
            ActionTier::LocalProcess => Some("FOLLOWING".to_string()),
            ActionTier::Escalate => Some("PENDING".to_string()),
        };

        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: "behavior_decision".to_string(),
            persona: persona.name().to_string(),
            correlation_id: None,
            message_preview: None,
            trigger_remote_ai: Some(matches!(decision.tier, ActionTier::Escalate)),
            estimated_profit_usd: Some(estimated_value),
            status,
            reason: Some(decision.reason.clone()),
            action_tier: Some(decision.tier),
            high_priority: Some(decision.high_priority),
        }
    }

    pub fn system_event(
        persona_name: &str,
        event: impl Into<String>,
        message_preview: Option<String>,
        status: Option<&str>,
        reason: Option<String>,
        correlation_id: Option<String>,
    ) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: event.into(),
            persona: persona_name.to_string(),
            correlation_id,
            message_preview,
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: status.map(|s| s.to_string()),
            reason,
            action_tier: None,
            high_priority: None,
        }
    }
}

#[derive(Clone)]
pub struct TaskTracker {
    path: Arc<Mutex<PathBuf>>,
}

impl TaskTracker {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(Mutex::new(path.into())),
        }
    }

    pub fn record(
        &self,
        entry: &TaskEntry,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let path = self.path.lock().expect("TaskTracker mutex poisoned");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(entry)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&*path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    fn read_raw_lines_lossy(
        &self,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let path = self
            .path
            .lock()
            .expect("TaskTracker mutex poisoned")
            .clone();
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&path)?;
        let mut reader = BufReader::new(file);
        let mut lines = Vec::new();
        let mut buf = Vec::new();

        loop {
            buf.clear();
            let bytes = reader.read_until(b'\n', &mut buf)?;
            if bytes == 0 {
                break;
            }

            if matches!(buf.last(), Some(b'\n')) {
                buf.pop();
            }
            if matches!(buf.last(), Some(b'\r')) {
                buf.pop();
            }

            lines.push(String::from_utf8_lossy(&buf).into_owned());
        }

        Ok(lines)
    }

    pub fn read_last_n(
        &self,
        n: usize,
    ) -> Result<Vec<TaskEntry>, Box<dyn std::error::Error + Send + Sync>> {
        let mut ring: std::collections::VecDeque<String> =
            std::collections::VecDeque::with_capacity(n);
        for line in self.read_raw_lines_lossy()? {
            if line.trim().is_empty() {
                continue;
            }
            if ring.len() == n {
                ring.pop_front();
            }
            ring.push_back(line);
        }

        let entries = ring
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Ok(entries)
    }

    pub fn update_statuses(
        &self,
        updates: &HashMap<String, String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if updates.is_empty() {
            return Ok(());
        }
        let path = self
            .path
            .lock()
            .expect("TaskTracker mutex poisoned")
            .clone();
        if !path.exists() {
            return Ok(());
        }

        let raw = self.read_raw_lines_lossy()?;

        let tmp_path = path.with_extension("jsonl.tmp");
        {
            let mut tmp = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            for line in &raw {
                if line.trim().is_empty() {
                    writeln!(tmp, "{line}")?;
                    continue;
                }
                if let Ok(mut entry) = serde_json::from_str::<TaskEntry>(line) {
                    if let Some(new_status) = updates.get(&entry.timestamp) {
                        entry.status = Some(new_status.clone());
                        writeln!(tmp, "{}", serde_json::to_string(&entry)?)?;
                        continue;
                    }
                }
                writeln!(tmp, "{line}")?;
            }
        }

        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    pub fn find_by_timestamp(
        &self,
        timestamp: &str,
    ) -> Result<Option<TaskEntry>, Box<dyn std::error::Error + Send + Sync>> {
        for line in self.read_raw_lines_lossy()? {
            if line.trim().is_empty() {
                continue;
            }

            if let Ok(entry) = serde_json::from_str::<TaskEntry>(&line) {
                if entry.timestamp == timestamp {
                    return Ok(Some(entry));
                }
            }
        }

        Ok(None)
    }

    /// Keep only the newest `max_lines` entries, discarding the oldest.
    ///
    /// Returns the number of entries removed, or `0` if no trim was needed.
    /// The file is rewritten atomically via a `.tmp` swap.
    pub fn trim_to_max(
        &self,
        max_lines: usize,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let all = self.read_raw_lines_lossy()?;
        let non_empty: Vec<&str> = all
            .iter()
            .map(|l| l.as_str())
            .filter(|l| !l.trim().is_empty())
            .collect();

        if non_empty.len() <= max_lines {
            return Ok(0);
        }

        let removed = non_empty.len() - max_lines;
        let keep = &non_empty[removed..];

        let path = self
            .path
            .lock()
            .expect("TaskTracker mutex poisoned")
            .clone();
        let tmp_path = path.with_extension("jsonl.tmp");
        {
            let mut tmp = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            for line in keep {
                writeln!(tmp, "{line}")?;
            }
        }
        fs::rename(&tmp_path, &path)?;
        Ok(removed)
    }

}


#[cfg(test)]
mod tests {
    use super::*;

    fn test_persona() -> Persona {
        Persona {
            identity: Identity {
                name: "Sirin".to_string(),
                professional_tone: ProfessionalTone::Brief,
            },
            objectives: vec!["Monitor Agora".to_string(), "Maintain VIPs".to_string()],
            roi_thresholds: RoiThresholds {
                min_usd_to_notify: 5.0,
                min_usd_to_call_remote_llm: 25.0,
            },
            response_style: ResponseStyle::default(),
            version: "1.0".to_string(),
            description: "test".to_string(),
            coding_agent: CodingAgentConfig::default(),
            disable_remote_ai: false,
        }
    }

    #[test]
    fn action_tier_thresholds() {
        let p = test_persona();
        assert!(matches!(determine_action_tier(1.0, &p), ActionTier::Ignore));
        assert!(matches!(
            determine_action_tier(10.0, &p),
            ActionTier::LocalProcess
        ));
        assert!(matches!(
            determine_action_tier(99.0, &p),
            ActionTier::Escalate
        ));
    }

    #[test]
    fn brief_draft_is_concise() {
        let p = test_persona();
        let out = generate_response_draft("Monitor Agora now".to_string(), &p);
        assert!(out.contains("已收到"));
    }

    #[test]
    fn behavior_engine_marks_objective_match() {
        let p = test_persona();
        let msg = IncomingMessage {
            source: "telegram".to_string(),
            msg: "Please Monitor Agora flow".to_string(),
        };
        let decision = BehaviorEngine::evaluate(msg, 30.0, &p);
        assert!(decision.high_priority);
        assert!(matches!(decision.tier, ActionTier::Escalate));
    }

    // ── Persona serialisation ─────────────────────────────────────────────────

    #[test]
    fn persona_load_reads_config_yaml() {
        let p = Persona::load();
        assert!(
            p.is_ok(),
            "config/persona.yaml should be loadable: {:?}",
            p.err()
        );
        let p = p.unwrap();
        assert!(
            !p.identity.name.is_empty(),
            "persona name must not be empty"
        );
    }

    #[test]
    fn persona_yaml_roundtrip() {
        let p = test_persona();
        let yaml = serde_yaml::to_string(&p).expect("serialization should not fail");
        let reloaded: Persona =
            serde_yaml::from_str(&yaml).expect("deserialization should not fail");
        assert_eq!(reloaded.identity.name, p.identity.name);
        assert_eq!(reloaded.objectives, p.objectives);
        assert!(
            (reloaded.roi_thresholds.min_usd_to_notify - p.roi_thresholds.min_usd_to_notify).abs()
                < f64::EPSILON
        );
    }

    // ── TaskTracker ───────────────────────────────────────────────────────────

    fn tmp_tracker(label: &str) -> (TaskTracker, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "sirin_persona_test_{}_{}.jsonl",
            std::process::id(),
            label
        ));
        (TaskTracker::new(&path), path)
    }

    #[test]
    fn tracker_record_and_read_roundtrip() {
        let (tracker, path) = tmp_tracker("roundtrip");
        let entry = TaskEntry::heartbeat("TestPersona");
        tracker.record(&entry).expect("record should succeed");

        let entries = tracker.read_last_n(10).expect("read should succeed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].persona, "TestPersona");
        assert_eq!(entries[0].event, "heartbeat");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_read_last_n_returns_tail() {
        let (tracker, path) = tmp_tracker("tail");
        for i in 0..10usize {
            let mut e = TaskEntry::heartbeat("P");
            e.reason = Some(format!("entry {i}"));
            tracker.record(&e).expect("record ok");
        }
        let entries = tracker.read_last_n(3).expect("read ok");
        assert_eq!(entries.len(), 3);
        // The 3 newest entries should be entries 7, 8, 9.
        assert_eq!(entries[2].reason.as_deref(), Some("entry 9"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_read_missing_file_returns_empty() {
        let path = std::env::temp_dir().join("sirin_nonexistent_tracker.jsonl");
        let _ = std::fs::remove_file(&path); // ensure absent
        let tracker = TaskTracker::new(&path);
        let entries = tracker
            .read_last_n(10)
            .expect("should succeed even if file is absent");
        assert!(entries.is_empty());
    }

    #[test]
    fn tracker_update_statuses_rewrites_atomically() {
        let (tracker, path) = tmp_tracker("update");
        let mut entry = TaskEntry::heartbeat("P");
        entry.status = Some("PENDING".to_string());
        let ts = entry.timestamp.clone();
        tracker.record(&entry).expect("record ok");

        let mut updates = std::collections::HashMap::new();
        updates.insert(ts, "DONE".to_string());
        tracker.update_statuses(&updates).expect("update ok");

        let entries = tracker.read_last_n(10).expect("read ok");
        assert_eq!(entries[0].status.as_deref(), Some("DONE"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_update_statuses_noop_when_empty() {
        let (tracker, path) = tmp_tracker("noop");
        // Should not fail even when no updates provided.
        tracker
            .update_statuses(&std::collections::HashMap::new())
            .expect("noop ok");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_trim_to_max_removes_oldest_entries() {
        let (tracker, path) = tmp_tracker("trim");
        for i in 0..8usize {
            let mut e = TaskEntry::heartbeat("P");
            e.reason = Some(format!("entry {i}"));
            tracker.record(&e).expect("record ok");
        }
        let removed = tracker.trim_to_max(5).expect("trim ok");
        assert_eq!(removed, 3);
        let remaining = tracker.read_last_n(10).expect("read ok");
        assert_eq!(remaining.len(), 5);
        assert_eq!(
            remaining[0].reason.as_deref(),
            Some("entry 3"),
            "oldest kept should be entry 3"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_trim_to_max_noop_when_under_limit() {
        let (tracker, path) = tmp_tracker("trim_noop");
        for _ in 0..3 {
            tracker
                .record(&TaskEntry::heartbeat("P"))
                .expect("record ok");
        }
        let removed = tracker.trim_to_max(10).expect("trim ok");
        assert_eq!(removed, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tracker_find_by_timestamp_returns_correct_entry() {
        let (tracker, path) = tmp_tracker("find");
        let mut entry = TaskEntry::heartbeat("P");
        entry.reason = Some("unique-reason".to_string());
        let ts = entry.timestamp.clone();
        tracker.record(&entry).expect("record ok");

        let found = tracker.find_by_timestamp(&ts).expect("find ok");
        assert!(found.is_some());
        assert_eq!(found.unwrap().reason.as_deref(), Some("unique-reason"));
        std::fs::remove_file(&path).ok();
    }
}
