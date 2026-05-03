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

// ── Tests ────────────────────────────────────────────────────────────────────
//
// Issue #252 — coverage for the intent-classification heuristics. All pure
// functions; table-driven where the rule is plain substring matching.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_file_reference_picks_up_src_paths() {
        assert!(looks_like_repo_file_reference("看看 src/llm/mod.rs 那段"));
        assert!(looks_like_repo_file_reference("docs/architecture.md 提到的 X"));
        assert!(looks_like_repo_file_reference("解釋 `Cargo.toml`"));
        // Path-like string with backslashes is normalised.
        assert!(looks_like_repo_file_reference("C:\\Users\\Redan\\src\\foo.rs"));
    }

    #[test]
    fn repo_file_reference_extension_only_match() {
        assert!(looks_like_repo_file_reference("foo.rs"));
        assert!(looks_like_repo_file_reference("config.yaml"));
        assert!(looks_like_repo_file_reference("App.tsx"));
        // No extension and no known prefix → not a file ref.
        assert!(!looks_like_repo_file_reference("這是一段純文字描述"));
        assert!(!looks_like_repo_file_reference("hello world"));
    }

    #[test]
    fn project_overview_query_matches_zh_cn_zh_tw_en() {
        assert!(looks_like_project_overview_query("這個專案怎麼運作？"));
        assert!(looks_like_project_overview_query("項目架構介紹一下"));
        assert!(looks_like_project_overview_query("解釋 codebase"));
        assert!(looks_like_project_overview_query("how is this Architecture organised"));
        // Negatives — no overview keyword.
        assert!(!looks_like_project_overview_query("修這個 bug"));
    }

    #[test]
    fn skill_query_recognises_skill_keyword_variants() {
        assert!(looks_like_skill_query("Sirin 有哪些 skill？"));
        assert!(looks_like_skill_query("看 skills.rs"));
        assert!(looks_like_skill_query("這個技能怎麼用"));
        assert!(looks_like_skill_query("能力目錄"));
        assert!(!looks_like_skill_query("修 bug"));
    }

    #[test]
    fn capability_query_matches_chinese_phrasings() {
        assert!(looks_like_capability_query("你能做什麼？"));
        assert!(looks_like_capability_query("你會什麼"));
        assert!(looks_like_capability_query("有什麼能力"));
        assert!(looks_like_capability_query("能幹嘛"));
        // English literal phrasings get normalised through the asks_what + skills path.
        assert!(looks_like_capability_query("what can you do"));
        assert!(looks_like_capability_query("list capabilities"));
    }

    #[test]
    fn capability_query_silent_for_unrelated_questions() {
        assert!(!looks_like_capability_query("天氣如何"));
        assert!(!looks_like_capability_query("修這個 bug"));
    }

    #[test]
    fn analysis_request_recognises_explain_analyze_etc() {
        assert!(looks_like_analysis_request("分析這段程式"));
        assert!(looks_like_analysis_request("解釋一下這個 function"));
        assert!(looks_like_analysis_request("how does this work"));
        assert!(looks_like_analysis_request("Why is the build failing"));
        assert!(!looks_like_analysis_request("hello"));
    }

    // ── classify_intent_family — priority-order tests ───────────────────────

    #[test]
    fn classify_capability_wins_over_local_file() {
        // A capability question that *also* mentions src/ would otherwise
        // classify as LocalFile — but capability runs first.
        let f = classify_intent_family("你能做什麼，特別是針對 src/ 那塊？", &[]);
        assert_eq!(f, IntentFamily::Capability);
    }

    #[test]
    fn classify_local_file_for_path_references() {
        let f = classify_intent_family("解釋 src/llm/mod.rs 那段邏輯", &[]);
        assert_eq!(f, IntentFamily::LocalFile);
    }

    #[test]
    fn classify_skill_architecture_for_skill_query() {
        // Priority: Research > Capability > LocalFile > SkillArchitecture.
        // The phrasing must trigger `looks_like_skill_query` without also
        // matching capability (no asks_what + skills combo) or LocalFile
        // (no .rs / src/ token).  "技能系統" lands cleanly in SkillArchitecture.
        let f = classify_intent_family("技能系統怎麼設計的", &[]);
        assert_eq!(f, IntentFamily::SkillArchitecture);
    }

    #[test]
    fn classify_project_overview_for_architecture_question() {
        let f = classify_intent_family("這個專案怎麼運作？", &[]);
        assert_eq!(f, IntentFamily::ProjectOverview);
    }

    #[test]
    fn classify_code_analysis_via_analysis_keywords() {
        let f = classify_intent_family("分析一下", &[]);
        assert_eq!(f, IntentFamily::CodeAnalysis);
    }

    #[test]
    fn classify_code_analysis_via_recommended_skill() {
        // No analysis keyword in the prompt, but the skill recommender said
        // "grounded_fix" — that's enough to route to CodeAnalysis.
        let f = classify_intent_family(
            "fix it",
            &["grounded_fix".to_string()],
        );
        assert_eq!(f, IntentFamily::CodeAnalysis);
    }

    #[test]
    fn classify_falls_back_to_general_chat() {
        let f = classify_intent_family("你好嗎", &[]);
        assert_eq!(f, IntentFamily::GeneralChat);
    }
}
