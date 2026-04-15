//! Behavior engine — classify incoming messages, draft responses, and
//! decide the action tier (Ignore / LocalProcess / Escalate) based on the
//! Persona's ROI thresholds and objectives.

use serde::{Deserialize, Serialize};

use super::{Persona, ProfessionalTone};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::{CodingAgentConfig, Identity, ResponseStyle, RoiThresholds};

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
}
