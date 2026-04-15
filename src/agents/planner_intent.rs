//! Intent classification heuristics for [`super::planner_agent`].
//!
//! Collects the `looks_like_*` predicates that recognise specific user
//! phrasing (project overview questions, skill-inventory questions, capability
//! queries, etc.) and the top-level [`classify_intent_family`] that picks
//! the best [`IntentFamily`] from those signals plus the router's research
//! detection and code-access checks.

use crate::telegram::commands::detect_research_intent;
use crate::telegram::language::{is_code_access_question, is_identity_question};

use super::planner_agent::IntentFamily;

pub(super) fn looks_like_repo_file_reference(text: &str) -> bool {
    text.split_whitespace().any(|token| {
        let cleaned = token
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '"' | '\'' | ',' | '，' | '。' | '?' | '？' | ':' | '：' | '(' | ')'
                )
            })
            .replace('\\', "/");

        cleaned.starts_with("src/")
            || cleaned.starts_with("app/")
            || cleaned.starts_with("docs/")
            || [
                ".rs", ".toml", ".md", ".json", ".yaml", ".yml", ".ts", ".tsx",
            ]
            .iter()
            .any(|suffix| cleaned.ends_with(suffix))
    })
}

pub(super) fn looks_like_project_overview_query(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "專案",
        "项目",
        "項目",
        "架構",
        "architecture",
        "結構",
        "模組",
        "module",
        "檔案",
        "files",
        "codebase",
        "怎麼運作",
        "如何運作",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn looks_like_skill_query(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("skill")
        || lower.contains("skills.rs")
        || text.contains("技能")
        || text.contains("能力目錄")
}

pub(super) fn looks_like_capability_query(text: &str) -> bool {
    let lower = text.to_lowercase();
    let compact = lower.split_whitespace().collect::<String>();
    let asks_what = [
        "有哪些",
        "有什麼",
        "有什么",
        "哪些",
        "會什麼",
        "会什么",
        "what",
        "list",
    ]
    .iter()
    .any(|needle| compact.contains(needle));
    let mentions_skills = compact.contains("skill")
        || compact.contains("skills")
        || text.contains("技能")
        || text.contains("能力");

    [
        "你能做什麼",
        "你可以做什麼",
        "你可以幫我做什麼",
        "你能幫我做什麼",
        "你會做什麼",
        "你會什麼",
        "有什麼能力",
        "有哪些能力",
        "有什麼功能",
        "有哪些功能",
        "能幹嘛",
        "能做啥",
        "whatcanyoudo",
        "howcanyouhelp",
        "capabilities",
        "abilities",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
        || (asks_what && mentions_skills)
}

pub(super) fn looks_like_analysis_request(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "分析", "解釋", "解释", "說明", "说明", "用途", "作用", "how", "why", "analyze", "explain",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn classify_intent_family(
    user_text: &str,
    recommended_skills: &[String],
) -> IntentFamily {
    if detect_research_intent(user_text).is_some() {
        return IntentFamily::Research;
    }

    if is_identity_question(user_text)
        || is_code_access_question(user_text)
        || looks_like_capability_query(user_text)
    {
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
                "code_change_planning"
                    | "symbol_trace"
                    | "grounded_fix"
                    | "test_selector"
                    | "architecture_consistency_check"
            )
        })
    {
        return IntentFamily::CodeAnalysis;
    }

    IntentFamily::GeneralChat
}
