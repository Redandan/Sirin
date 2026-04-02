use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfessionalTone {
    Brief,
    Detailed,
    Casual,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Identity {
    pub name: String,
    pub professional_tone: ProfessionalTone,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoiThresholds {
    pub min_usd_to_notify: f64,
    pub min_usd_to_call_remote_llm: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Persona {
    pub identity: Identity,
    pub objectives: Vec<String>,
    pub roi_thresholds: RoiThresholds,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
}

fn default_version() -> String {
    "1.0".to_string()
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

    pub fn should_trigger_remote_ai(&self, estimated_profit: f64) -> bool {
        estimated_profit > self.roi_thresholds.min_usd_to_call_remote_llm
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
                p.roi_thresholds.min_usd_to_notify,
                p.roi_thresholds.min_usd_to_call_remote_llm
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
            p.name(), msg.source
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

#[derive(Debug, Serialize, Deserialize)]
pub struct TaskEntry {
    pub timestamp: String,
    pub event: String,
    pub persona: String,
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
            trigger_remote_ai: None,
            estimated_profit_usd: None,
            status: None,
            reason: None,
            action_tier: None,
            high_priority: None,
        }
    }

    pub fn ai_decision(persona_name: &str, estimated_profit: f64, triggered: bool) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event: "ai_decision".to_string(),
            persona: persona_name.to_string(),
            trigger_remote_ai: Some(triggered),
            estimated_profit_usd: Some(estimated_profit),
            status: if triggered { Some("PENDING".to_string()) } else { None },
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
            trigger_remote_ai: Some(matches!(decision.tier, ActionTier::Escalate)),
            estimated_profit_usd: Some(estimated_value),
            status,
            reason: Some(decision.reason.clone()),
            action_tier: Some(decision.tier),
            high_priority: Some(decision.high_priority),
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

    pub fn record(&self, entry: &TaskEntry) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let path = self.path.lock().expect("TaskTracker mutex poisoned");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(entry)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&*path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub fn read_last_n(&self, n: usize) -> Result<Vec<TaskEntry>, Box<dyn std::error::Error + Send + Sync>> {
        let path = self.path.lock().expect("TaskTracker mutex poisoned").clone();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&path)?;
        let reader = BufReader::new(file);

        let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::with_capacity(n);
        for line in reader.lines() {
            let line = line?;
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
        let path = self.path.lock().expect("TaskTracker mutex poisoned").clone();
        if !path.exists() {
            return Ok(());
        }

        let raw: Vec<String> = {
            let file = fs::File::open(&path)?;
            BufReader::new(file).lines().filter_map(|l| l.ok()).collect()
        };

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
            version: "1.0".to_string(),
            description: "test".to_string(),
        }
    }

    #[test]
    fn action_tier_thresholds() {
        let p = test_persona();
        assert!(matches!(determine_action_tier(1.0, &p), ActionTier::Ignore));
        assert!(matches!(determine_action_tier(10.0, &p), ActionTier::LocalProcess));
        assert!(matches!(determine_action_tier(99.0, &p), ActionTier::Escalate));
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
}
