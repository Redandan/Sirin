use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::llm::call_prompt;
use crate::persona::TaskTracker;
use crate::telegram::commands::detect_research_intent;
use crate::telegram::language::{is_code_access_question, is_identity_question};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerRequest {
    pub user_text: String,
    #[serde(default)]
    pub context_block: Option<String>,
    #[serde(default)]
    pub peer_id: Option<i64>,
    #[serde(default)]
    pub fallback_reply: Option<String>,
    #[serde(default)]
    pub execution_result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanIntent {
    Answer,
    Research,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IntentFamily {
    Capability,
    LocalFile,
    ProjectOverview,
    SkillArchitecture,
    CodeAnalysis,
    Research,
    #[default]
    GeneralChat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowPlan {
    pub intent: PlanIntent,
    #[serde(default)]
    pub intent_family: IntentFamily,
    pub steps: Vec<String>,
    #[serde(default)]
    pub should_start_research: bool,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub recommended_skills: Vec<String>,
}

pub struct PlannerAgent;

impl Agent for PlannerAgent {
    fn name(&self) -> &'static str {
        "planner_agent"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        input: Value,
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: PlannerRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid planner request payload: {e}"))?;

            let recommended_skills = recommended_skill_ids(ctx, &request.user_text).await;
            let family = classify_intent_family(&request.user_text, &recommended_skills);

            let plan = if family != IntentFamily::GeneralChat {
                let p = apply_skill_hints(build_family_plan(family.clone()), &recommended_skills);
                ctx.record_system_event(
                    "adk_planner_plan_ready",
                    Some(preview_text(&request.user_text)),
                    Some("DONE"),
                    Some(format!(
                        "intent={:?} family={:?} source=intent_family skills={}",
                        p.intent,
                        p.intent_family,
                        p.recommended_skills.join(",")
                    )),
                );
                p
            } else {
                match llm_plan(ctx, &request, &recommended_skills).await {
                    Some(p) => {
                        let p = apply_skill_hints(p, &recommended_skills);
                        ctx.record_system_event(
                            "adk_planner_plan_ready",
                            Some(preview_text(&request.user_text)),
                            Some("DONE"),
                            Some(format!(
                                "intent={:?} family={:?} source=llm skills={}",
                                p.intent,
                                p.intent_family,
                                p.recommended_skills.join(",")
                            )),
                        );
                        p
                    }
                    None => {
                        let p = apply_skill_hints(build_family_plan(IntentFamily::GeneralChat), &recommended_skills);
                        ctx.record_system_event(
                            "adk_planner_plan_ready",
                            Some(preview_text(&request.user_text)),
                            Some("DONE"),
                            Some(format!(
                                "intent={:?} family={:?} source=heuristic skills={}",
                                p.intent,
                                p.intent_family,
                                p.recommended_skills.join(",")
                            )),
                        );
                        p
                    }
                }
            };

            serde_json::to_value(plan).map_err(|e| e.to_string())
        }
        .boxed()
    }
}

/// Ask the LLM to classify the user message and return a structured plan.
/// Returns `None` on LLM error or unparseable output so the caller can fall back.
async fn recommended_skill_ids(ctx: &AgentContext, user_text: &str) -> Vec<String> {
    match ctx.call_tool("skill_catalog", json!({ "query": user_text })).await {
        Ok(value) => value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("id").and_then(Value::as_str))
            .map(|id| id.to_string())
            .collect(),
        Err(_) => crate::skills::recommended_skills(user_text)
            .into_iter()
            .map(|skill| skill.id)
            .collect(),
    }
}

fn looks_like_repo_file_reference(text: &str) -> bool {
    text.split_whitespace().any(|token| {
        let cleaned = token
            .trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | ',' | '，' | '。' | '?' | '？' | ':' | '：' | '(' | ')'))
            .replace('\\', "/");

        cleaned.starts_with("src/")
            || cleaned.starts_with("app/")
            || cleaned.starts_with("docs/")
            || [".rs", ".toml", ".md", ".json", ".yaml", ".yml", ".ts", ".tsx"]
                .iter()
                .any(|suffix| cleaned.ends_with(suffix))
    })
}

fn looks_like_project_overview_query(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "專案", "项目", "項目", "架構", "architecture", "結構", "模組", "module",
        "檔案", "files", "codebase", "怎麼運作", "如何運作",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn looks_like_skill_query(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("skill") || lower.contains("skills.rs") || text.contains("技能") || text.contains("能力目錄")
}

fn looks_like_capability_query(text: &str) -> bool {
    let lower = text.to_lowercase();
    let compact = lower.split_whitespace().collect::<String>();
    let asks_what = ["有哪些", "有什麼", "有什么", "哪些", "會什麼", "会什么", "what", "list"]
        .iter()
        .any(|needle| compact.contains(needle));
    let mentions_skills = compact.contains("skill") || compact.contains("skills") || text.contains("技能") || text.contains("能力");

    [
        "你能做什麼", "你可以做什麼", "你可以幫我做什麼", "你能幫我做什麼", "你會做什麼", "你會什麼",
        "有什麼能力", "有哪些能力", "有什麼功能", "有哪些功能", "能幹嘛", "能做啥",
        "whatcanyoudo", "howcanyouhelp", "capabilities", "abilities",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
        || (asks_what && mentions_skills)
}

fn looks_like_analysis_request(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["分析", "解釋", "解释", "說明", "说明", "用途", "作用", "how", "why", "analyze", "explain"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn classify_intent_family(user_text: &str, recommended_skills: &[String]) -> IntentFamily {
    if detect_research_intent(user_text).is_some() {
        return IntentFamily::Research;
    }

    if is_identity_question(user_text) || is_code_access_question(user_text) || looks_like_capability_query(user_text) {
        return IntentFamily::Capability;
    }

    if looks_like_repo_file_reference(user_text) {
        return IntentFamily::LocalFile;
    }

    if looks_like_skill_query(user_text) {
        return IntentFamily::SkillArchitecture;
    }

    if looks_like_project_overview_query(user_text) {
        return IntentFamily::ProjectOverview;
    }

    if looks_like_analysis_request(user_text)
        || recommended_skills.iter().any(|skill| {
            matches!(
                skill.as_str(),
                "code_change_planning" | "symbol_trace" | "grounded_fix" | "test_selector" | "architecture_consistency_check"
            )
        })
    {
        return IntentFamily::CodeAnalysis;
    }

    IntentFamily::GeneralChat
}

fn build_family_plan(intent_family: IntentFamily) -> WorkflowPlan {
    match intent_family {
        IntentFamily::Capability => WorkflowPlan {
            intent: PlanIntent::Answer,
            intent_family,
            steps: vec![
                "answer the capability / identity question directly".to_string(),
                "ground the answer in local skills or project context".to_string(),
                "offer a concrete next file or module to inspect".to_string(),
            ],
            should_start_research: false,
            summary: "Capability-style local answer workflow.".to_string(),
            recommended_skills: Vec::new(),
        },
        IntentFamily::LocalFile => WorkflowPlan {
            intent: PlanIntent::Answer,
            intent_family,
            steps: vec![
                "read the referenced local file".to_string(),
                "summarize its role and evidence".to_string(),
                "offer a follow-up explanation path".to_string(),
            ],
            should_start_research: false,
            summary: "Grounded local-file explanation workflow.".to_string(),
            recommended_skills: Vec::new(),
        },
        IntentFamily::ProjectOverview => WorkflowPlan {
            intent: PlanIntent::Answer,
            intent_family,
            steps: vec![
                "inspect core project files".to_string(),
                "summarize the architecture and module boundaries".to_string(),
                "suggest the next module to inspect".to_string(),
            ],
            should_start_research: false,
            summary: "Project-overview analysis workflow.".to_string(),
            recommended_skills: Vec::new(),
        },
        IntentFamily::SkillArchitecture => WorkflowPlan {
            intent: PlanIntent::Answer,
            intent_family,
            steps: vec![
                "inspect skill catalog and related planner/router modules".to_string(),
                "explain the capability model from local evidence".to_string(),
                "offer a deeper dive into the relevant module".to_string(),
            ],
            should_start_research: false,
            summary: "Skill-architecture explanation workflow.".to_string(),
            recommended_skills: Vec::new(),
        },
        IntentFamily::CodeAnalysis => WorkflowPlan {
            intent: PlanIntent::Answer,
            intent_family,
            steps: vec![
                "inspect relevant local modules".to_string(),
                "trace the likely data flow / symbol path".to_string(),
                "summarize the grounded findings before proposing changes".to_string(),
            ],
            should_start_research: false,
            summary: "Grounded code-analysis workflow.".to_string(),
            recommended_skills: Vec::new(),
        },
        IntentFamily::Research => WorkflowPlan {
            intent: PlanIntent::Research,
            intent_family,
            steps: vec![
                "launch background research".to_string(),
                "summarize research result".to_string(),
                "create follow-up task".to_string(),
                "prepare immediate acknowledgement".to_string(),
            ],
            should_start_research: true,
            summary: "Need research workflow (intent-family).".to_string(),
            recommended_skills: Vec::new(),
        },
        IntentFamily::GeneralChat => WorkflowPlan {
            intent: PlanIntent::Answer,
            intent_family,
            steps: vec![
                "route request".to_string(),
                "prepare direct answer".to_string(),
                "run chat response".to_string(),
            ],
            should_start_research: false,
            summary: "Direct answer workflow (heuristic fallback).".to_string(),
            recommended_skills: Vec::new(),
        },
    }
}

fn push_step_if_missing(steps: &mut Vec<String>, step: &str) {
    if !steps.iter().any(|item| item == step) {
        steps.push(step.to_string());
    }
}

fn apply_skill_hints(mut plan: WorkflowPlan, recommended_skills: &[String]) -> WorkflowPlan {
    plan.recommended_skills = recommended_skills.to_vec();

    if recommended_skills.iter().any(|skill| skill == "project_overview") {
        push_step_if_missing(&mut plan.steps, "inspect core project files");
    }
    if recommended_skills.iter().any(|skill| skill == "local_file_read") {
        push_step_if_missing(&mut plan.steps, "read the referenced local file");
    }
    if recommended_skills.iter().any(|skill| skill == "codebase_search" || skill == "symbol_trace") {
        push_step_if_missing(&mut plan.steps, "trace affected symbols/modules");
    }
    if recommended_skills.iter().any(|skill| skill == "code_change_planning") {
        push_step_if_missing(&mut plan.steps, "outline a safe change plan before editing");
    }
    if recommended_skills.iter().any(|skill| skill == "grounded_fix") {
        push_step_if_missing(&mut plan.steps, "identify root cause from local code context");
    }
    if recommended_skills.iter().any(|skill| skill == "test_selector") {
        push_step_if_missing(&mut plan.steps, "run targeted validation after changes");
    }
    if recommended_skills.iter().any(|skill| skill == "architecture_consistency_check") {
        push_step_if_missing(&mut plan.steps, "check architecture consistency after the change");
    }

    if !plan.recommended_skills.is_empty() {
        let skill_list = plan.recommended_skills.join(", ");
        if plan.summary.is_empty() {
            plan.summary = format!("Recommended skills: {skill_list}");
        } else {
            plan.summary = format!("{} Recommended skills: {}.", plan.summary.trim_end_matches('.'), skill_list);
        }
    }

    plan
}

/// Ask the LLM to classify the user message and return a structured plan.
/// Returns `None` on LLM error or unparseable output so the caller can fall back.
async fn llm_plan(ctx: &AgentContext, request: &PlannerRequest, recommended_skills: &[String]) -> Option<WorkflowPlan> {
    let context_hint = request
        .context_block
        .as_deref()
        .map(|c| format!("\nRecent context:\n{c}"))
        .unwrap_or_default();
    let skill_hint = if recommended_skills.is_empty() {
        String::new()
    } else {
        format!("\nRelevant local capabilities for this request: {}", recommended_skills.join(", "))
    };

    let prompt = format!(
        r#"You are a planning assistant. Classify the user's message and output ONLY valid JSON.

User message: "{msg}"{context_hint}{skill_hint}

JSON schema (fill every field):
{{
  "intent": "answer" | "research",
  "should_start_research": true | false,
  "steps": ["step 1", "step 2", ...],
  "summary": "one-sentence description of the workflow"
}}

Rules:
- Use "research" when the user wants an investigation, analysis, or information about a URL/topic.
- Use "answer" for greetings, simple questions, direct instructions, identity questions, or questions about whether you can inspect the local code/project files.
- If the relevant local capabilities include `project_overview`, `local_file_read`, `codebase_search`, `grounded_fix`, `symbol_trace`, or `test_selector`, prefer "answer" because the request can be handled locally inside this repo.
- Never classify `你是誰` or `能看到當前代碼嗎` style questions as research.
- steps must be an ordered list of 2-5 concrete actions.

Output only the JSON object, no explanation."#,
        msg = request.user_text,
        context_hint = context_hint,
    );

    let raw = call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt)
        .await
        .ok()?;

    // Extract JSON object from the response (model may wrap it in prose).
    let json_start = raw.find('{')?;
    let json_end = raw.rfind('}').map(|i| i + 1)?;
    let json_slice = &raw[json_start..json_end];

    #[derive(serde::Deserialize)]
    struct LlmPlanRaw {
        intent: String,
        #[serde(default)]
        should_start_research: bool,
        #[serde(default)]
        steps: Vec<String>,
        #[serde(default)]
        summary: String,
    }

    let parsed: LlmPlanRaw = serde_json::from_str(json_slice).ok()?;

    let intent = match parsed.intent.to_lowercase().as_str() {
        "research" => PlanIntent::Research,
        _ => PlanIntent::Answer,
    };

    Some(WorkflowPlan {
        should_start_research: parsed.should_start_research
            || matches!(intent, PlanIntent::Research),
        intent,
        intent_family: classify_intent_family(&request.user_text, recommended_skills),
        steps: if parsed.steps.is_empty() {
            vec!["route request".to_string(), "run chat response".to_string()]
        } else {
            parsed.steps
        },
        summary: if parsed.summary.is_empty() {
            "LLM-generated plan".to_string()
        } else {
            parsed.summary
        },
        recommended_skills: Vec::new(),
    })
}

fn preview_text(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

pub async fn run_planner_via_adk(
    request: PlannerRequest,
    tracker: Option<TaskTracker>,
) -> Result<WorkflowPlan, String> {
    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context("planner_request")
        .with_optional_tracker(tracker)
        .with_metadata("agent", "planner_agent");

    runtime
        .run(&PlannerAgent, ctx, json!(request))
        .await
        .and_then(|output| serde_json::from_value(output).map_err(|e| e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn planner_marks_research_flows() {
        let plan = run_planner_via_adk(
            PlannerRequest {
                user_text: "幫我研究 Rust async runtime".to_string(),
                context_block: None,
                peer_id: None,
                fallback_reply: None,
                execution_result: None,
            },
            None,
        )
        .await
        .expect("planner should succeed");

        assert_eq!(plan.intent, PlanIntent::Research);
        assert!(plan.should_start_research);
        assert!(plan.steps.iter().any(|step| step.contains("summarize research result")));
        assert!(plan.steps.iter().any(|step| step.contains("create follow-up task")));
    }

    #[tokio::test]
    async fn planner_keeps_meta_questions_as_answer() {
        let plan = run_planner_via_adk(
            PlannerRequest {
                user_text: "能看到當前代碼嗎".to_string(),
                context_block: None,
                peer_id: None,
                fallback_reply: None,
                execution_result: None,
            },
            None,
        )
        .await
        .expect("planner should succeed");

        assert_eq!(plan.intent, PlanIntent::Answer);
        assert_eq!(plan.intent_family, IntentFamily::Capability);
        assert!(!plan.should_start_research);
        assert!(plan.steps.iter().any(|step| step.contains("capability / identity")));
    }

    #[tokio::test]
    async fn planner_uses_skill_recommendations_for_local_optimization_requests() {
        let plan = run_planner_via_adk(
            PlannerRequest {
                user_text: "先分析再改，幫我安全優化這段 code 並跑測試".to_string(),
                context_block: None,
                peer_id: None,
                fallback_reply: None,
                execution_result: None,
            },
            None,
        )
        .await
        .expect("planner should succeed");

        assert_eq!(plan.intent, PlanIntent::Answer);
        assert_eq!(plan.intent_family, IntentFamily::CodeAnalysis);
        assert!(plan.recommended_skills.iter().any(|skill| skill == "code_change_planning"));
        assert!(plan.recommended_skills.iter().any(|skill| skill == "grounded_fix"));
        assert!(plan.steps.iter().any(|step| step.contains("safe change plan")));
        assert!(plan.steps.iter().any(|step| step.contains("targeted validation")));
    }

    #[tokio::test]
    async fn planner_classifies_project_overview_family() {
        let plan = run_planner_via_adk(
            PlannerRequest {
                user_text: "這個專案怎麼運作？".to_string(),
                context_block: None,
                peer_id: None,
                fallback_reply: None,
                execution_result: None,
            },
            None,
        )
        .await
        .expect("planner should succeed");

        assert_eq!(plan.intent, PlanIntent::Answer);
        assert_eq!(plan.intent_family, IntentFamily::ProjectOverview);
        assert!(plan.steps.iter().any(|step| step.contains("inspect core project files")));
    }
}
