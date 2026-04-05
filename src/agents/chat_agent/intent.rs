//! Intent classification for incoming chat messages.
//!
//! Converts a raw user message into a structured [`MessageUnderstanding`]
//! describing what the user wants (intent) and which files they referenced.
//!
//! Two fast-paths (no LLM call) are attempted before falling through to a
//! compact LLM classification prompt:
//!
//! 1. **Planner hint** — the upstream Planner already ran its own LLM call;
//!    trust it directly.
//! 2. **Keyword matching** — deterministic heuristics for common patterns.

use crate::adk::AgentContext;
use crate::llm::call_prompt;
use crate::memory::{load_recent_context, looks_like_code_query};
use crate::researcher;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Intent {
    LocalFile,
    ProjectOverview,
    CodeAnalysis,
    CapabilityQuery,
    Correction,
    WebSearch,
    General,
}

pub(super) struct MessageUnderstanding {
    pub(super) intent: Intent,
    pub(super) is_correction: bool,
    pub(super) target_files: Vec<String>,
}

// ── Public helpers (used by dispatch and mod) ─────────────────────────────────

pub(super) fn is_file_view_request(user_text: &str) -> bool {
    let lower = user_text.trim().to_lowercase();

    let has_question = [
        "什麼", "是啥", "怎麼", "如何", "為什麼", "哪裡", "問題", "分析", "解釋", "說明",
        "用途", "作用", "幹嘛", "做什麼", "怎樣", "有沒有", "會不會",
        "what", "how", "why", "where", "explain", "analyze", "describe", "problem", "issue",
    ]
    .iter()
    .any(|q| lower.contains(q));

    if has_question {
        return false;
    }

    let compact: String = lower.split_whitespace().collect::<String>();
    compact.starts_with("幫我看")
        || compact.starts_with("看一下")
        || compact.starts_with("看看")
        || compact.starts_with("讀取")
        || compact.starts_with("show")
        || compact.starts_with("read")
        || compact.starts_with("open")
        || compact.starts_with("cat")
        || (lower.trim_start().starts_with("看 ") && !has_question)
}

pub(super) fn is_simple_meta_request(user_text: &str) -> bool {
    use crate::telegram::language::{is_code_access_question, is_identity_question};

    let trimmed = user_text.trim();
    if trimmed.chars().count() > 64 {
        return false;
    }

    if is_identity_question(trimmed) {
        return true;
    }

    is_code_access_question(trimmed)
        && !looks_like_project_overview_query(trimmed)
        && extract_file_reference(trimmed).is_none()
}

pub(super) fn looks_like_project_overview_query(user_text: &str) -> bool {
    let lower = user_text.to_lowercase();
    [
        "專案", "项目", "項目", "架構", "architecture", "結構", "模組", "module",
        "檔案", "files", "codebase", "這是啥", "這是什麼",
        "能看到什麼", "看到什麼", "看得到什麼", "哪些檔案",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn extract_file_reference(user_text: &str) -> Option<String> {
    user_text
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|c: char| {
                matches!(c, '`' | '"' | '\'' | ',' | '，' | '。' | '?' | '？' | ':' | '：' | '(' | ')')
            })
        })
        .find(|token| !token.is_empty() && looks_like_file_token(token))
        .map(|token| token.replace('\\', "/"))
}

pub(super) fn extract_file_references_from_text(text: &str) -> Vec<String> {
    let mut matches = Vec::new();

    for segment in text.split('`').skip(1).step_by(2) {
        let cleaned = segment.trim().replace('\\', "/");
        if !cleaned.is_empty() && looks_like_file_token(&cleaned) && !matches.contains(&cleaned) {
            matches.push(cleaned);
        }
    }

    for token in text.split_whitespace() {
        let cleaned = token
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '"' | '\'' | ',' | '，' | '。' | '?' | '？' | ':' | '：' | '(' | ')' | '-' | '•'
                )
            })
            .replace('\\', "/");

        if !cleaned.is_empty() && looks_like_file_token(&cleaned) && !matches.contains(&cleaned) {
            matches.push(cleaned);
        }
    }

    matches
}

pub(super) fn is_contextual_file_explanation_request(user_text: &str) -> bool {
    let has_reference = ["這些", "这些", "那些", "上面", "前面", "剛剛", "刚刚", "它們", "它们"]
        .iter()
        .any(|needle| user_text.contains(needle));
    let asks_explain = [
        "是什麼", "是啥", "說明", "说明", "解釋", "解释", "用途", "作用", "幹嘛", "做什麼", "做什么",
        "分析", "analyze", "explain",
    ]
    .iter()
    .any(|needle| user_text.to_lowercase().contains(needle));

    has_reference && asks_explain
}

pub(super) fn looks_like_skill_query(user_text: &str) -> bool {
    let lower = user_text.to_lowercase();
    lower.contains("skill")
        || lower.contains("skills.rs")
        || user_text.contains("技能")
        || user_text.contains("能力目錄")
}

pub(super) fn looks_like_capability_query(user_text: &str) -> bool {
    let lower = user_text.to_lowercase();
    let compact = lower.split_whitespace().collect::<String>();

    [
        "你能做什麼", "你可以做什麼", "你可以幫我做什麼", "你能幫我做什麼", "你會做什麼", "你會什麼",
        "有什麼能力", "有哪些能力", "有什麼功能", "有哪些功能", "能幹嘛", "能做啥",
        "whatcanyoudo", "howcanyouhelp", "capabilities", "abilities",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
        || is_skill_inventory_request(user_text)
}

pub(super) fn looks_like_analysis_request(user_text: &str) -> bool {
    let lower = user_text.to_lowercase();
    [
        "分析", "解釋", "解释", "說明", "说明", "是什麼", "是啥", "用途", "作用", "如何", "怎麼", "为什么", "為什麼",
        "analyze", "explain", "how", "why",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn is_skill_inventory_request(user_text: &str) -> bool {
    let lower = user_text.to_lowercase();
    let compact = lower.split_whitespace().collect::<String>();
    let asks_what = ["有哪些", "有什麼", "有什么", "哪些", "會什麼", "会什么", "what", "list"]
        .iter()
        .any(|needle| compact.contains(needle));
    let mentions_skills = compact.contains("skill")
        || compact.contains("skills")
        || user_text.contains("技能")
        || user_text.contains("能力");

    mentions_skills && asks_what
}

pub(super) fn infer_focus_paths_from_query(
    user_text: &str,
    peer_id: Option<i64>,
    intent_family: Option<&str>,
) -> Vec<String> {
    let lower = user_text.to_lowercase();
    let mut paths = Vec::new();

    match intent_family {
        Some("capability") | Some("skill_architecture") => {
            for path in [
                "src/skills.rs",
                "src/agents/planner_agent.rs",
                "src/agents/router_agent.rs",
                "src/adk/tool.rs",
                "README.md",
            ] {
                push_unique_path(&mut paths, path);
            }
            return paths;
        }
        Some("project_overview") => {
            for path in ["README.md", "docs/ROADMAP.md", "src/main.rs", "src/agents/mod.rs"] {
                push_unique_path(&mut paths, path);
            }
            return paths;
        }
        Some("local_file") | Some("code_analysis") => {}
        _ => {}
    }

    if let Some(path) = extract_file_reference(user_text) {
        push_unique_path(&mut paths, &path);
    }

    if is_contextual_file_explanation_request(user_text) {
        for path in recent_context_file_references(peer_id) {
            push_unique_path(&mut paths, &path);
        }
    }

    if looks_like_skill_query(user_text) || looks_like_capability_query(user_text) {
        for path in [
            "src/skills.rs",
            "src/agents/planner_agent.rs",
            "src/agents/router_agent.rs",
            "src/adk/tool.rs",
        ] {
            push_unique_path(&mut paths, path);
        }
    }

    if looks_like_capability_query(user_text) || looks_like_purpose_query(user_text) {
        for path in ["README.md", "docs/ROADMAP.md"] {
            push_unique_path(&mut paths, path);
        }
    }

    if looks_like_project_overview_query(user_text)
        || lower.contains("怎麼運作")
        || lower.contains("如何運作")
    {
        for path in ["src/main.rs", "src/ui.rs", "src/llm.rs", "src/memory.rs"] {
            push_unique_path(&mut paths, path);
        }
    }

    if lower.contains("planner") {
        push_unique_path(&mut paths, "src/agents/planner_agent.rs");
    }
    if lower.contains("router") {
        push_unique_path(&mut paths, "src/agents/router_agent.rs");
    }
    if lower.contains("chat") {
        push_unique_path(&mut paths, "src/agents/chat_agent/mod.rs");
    }
    if lower.contains("telegram") {
        push_unique_path(&mut paths, "src/telegram/mod.rs");
    }
    if lower.contains("memory") || user_text.contains("記憶") {
        push_unique_path(&mut paths, "src/memory.rs");
    }

    if paths.is_empty()
        && (lower.contains("代碼")
            || lower.contains("程式碼")
            || lower.contains("code")
            || lower.contains("本地"))
    {
        for path in [
            "src/main.rs",
            "src/agents/mod.rs",
            "src/telegram/mod.rs",
            "src/memory.rs",
        ] {
            push_unique_path(&mut paths, path);
        }
    }

    paths
}

/// Ask the LLM to classify what the user wants so we can route to the right path.
///
/// Fast paths (no LLM call):
///   1. Keyword classification gives a confident non-General result → use it directly.
///   2. Planner already forwarded a non-general intent_family → trust it.
///
/// Only falls through to a real LLM call for ambiguous `General` messages where
/// neither keywords nor the Planner produced a clear classification.
pub(super) async fn understand_message(
    ctx: &AgentContext,
    user_text: &str,
    context_block: Option<&str>,
    planner_intent: Option<&str>,
) -> MessageUnderstanding {
    // ── Fast path 1: Planner already classified the intent ───────────────
    if let Some(family) = planner_intent {
        let mapped = match family {
            "local_file" => Some(Intent::LocalFile),
            "project_overview" => Some(Intent::ProjectOverview),
            "code_analysis" | "skill_architecture" => Some(Intent::CodeAnalysis),
            "capability" => Some(Intent::CapabilityQuery),
            "research" => Some(Intent::WebSearch),
            _ => None,
        };
        if let Some(intent) = mapped {
            let files = extract_file_references_from_text(user_text);
            return MessageUnderstanding {
                intent,
                is_correction: false,
                target_files: files,
            };
        }
    }

    // ── Fast path 2: keyword classification ──────────────────────────────
    let keyword_files = extract_file_references_from_text(user_text);
    let keyword_intent = if !keyword_files.is_empty() {
        Intent::LocalFile
    } else if looks_like_project_overview_query(user_text) {
        Intent::ProjectOverview
    } else if looks_like_capability_query(user_text) || looks_like_skill_query(user_text) {
        Intent::CapabilityQuery
    } else if looks_like_analysis_request(user_text) && looks_like_code_query(user_text) {
        Intent::CodeAnalysis
    } else {
        Intent::General
    };

    if keyword_intent != Intent::General {
        return MessageUnderstanding {
            intent: keyword_intent,
            is_correction: false,
            target_files: keyword_files,
        };
    }

    // ── Keyword fallback for LLM failure ─────────────────────────────────
    let default = MessageUnderstanding {
        intent: Intent::General,
        is_correction: false,
        target_files: Vec::new(),
    };

    let context_section = context_block
        .map(|c| {
            let preview: String = c.chars().take(600).collect();
            format!("\nRecent conversation (for context only):\n{preview}\n")
        })
        .unwrap_or_default();

    let prompt = format!(
        "You are a message intent classifier. Read the user message and output ONLY a JSON object — no markdown, no explanation.\n\
\n\
Fields:\n\
- intent: one of \"local_file\" | \"project_overview\" | \"code_analysis\" | \"capability_query\" | \"correction\" | \"web_search\" | \"general\"\n\
- is_correction: true if the user says the previous reply was wrong, outdated, or inaccurate\n\
- target_files: array of file paths explicitly mentioned (empty array if none)\n\
- summary: one short sentence describing what the user wants\n\
\n\
Intent rules:\n\
- \"correction\": user indicates the previous reply was wrong/outdated/inaccurate — e.g. \"不是這樣\", \"應該不對\", \"你說錯了\", \"wrong\", \"not accurate\", \"that's outdated\", \"應該更新了\"\n\
- \"local_file\": user names a specific file or asks to read/show/explain a particular file\n\
- \"project_overview\": user asks about project structure, modules, architecture, or what files exist\n\
- \"code_analysis\": user asks to analyze, explain, debug, trace, or understand code behaviour\n\
- \"capability_query\": user asks what you can do, what skills or features you have\n\
- \"web_search\": user asks about external information not found in the codebase\n\
- \"general\": everything else\n\
{context_section}\n\
User message: {user_text}\n\
\n\
Output ONLY valid JSON."
    );

    let raw = match call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), prompt).await {
        Ok(r) => r,
        Err(_) => return default,
    };

    let json_str = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return default;
    };

    let intent = match parsed.get("intent").and_then(|v| v.as_str()).unwrap_or("general") {
        "local_file" => Intent::LocalFile,
        "project_overview" => Intent::ProjectOverview,
        "code_analysis" => Intent::CodeAnalysis,
        "capability_query" => Intent::CapabilityQuery,
        "correction" => Intent::Correction,
        "web_search" => Intent::WebSearch,
        _ => Intent::General,
    };

    let is_correction = parsed.get("is_correction").and_then(|v| v.as_bool()).unwrap_or(false)
        || intent == Intent::Correction;

    let target_files: Vec<String> = parsed
        .get("target_files")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect())
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| extract_file_references_from_text(user_text));

    MessageUnderstanding { intent, is_correction, target_files }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn looks_like_file_token(token: &str) -> bool {
    let normalized = token.trim().replace('\\', "/");
    if normalized.is_empty() || normalized == "/" {
        return false;
    }

    let has_known_extension = [".rs", ".toml", ".md", ".yaml", ".yml", ".json", ".ts", ".tsx", ".js", ".jsx"]
        .iter()
        .any(|suffix| normalized.ends_with(suffix));

    let has_known_prefix = [
        "src/", "app/", "config/", "data/", "docs/", "tests/", ".cargo/", ".claude/", "icons/",
    ]
    .iter()
    .any(|prefix| normalized.starts_with(prefix));

    has_known_extension
        || has_known_prefix
        || matches!(normalized.as_str(), "Cargo.toml" | "README.md" | "tauri.conf.json" | "build.rs")
}

fn recent_context_file_references(peer_id: Option<i64>) -> Vec<String> {
    let Ok(entries) = load_recent_context(5, peer_id) else {
        return Vec::new();
    };

    let mut matches = Vec::new();
    for entry in entries.into_iter().rev() {
        for path in extract_file_references_from_text(&entry.assistant_reply) {
            if !matches.contains(&path) {
                matches.push(path);
            }
        }
        if !matches.is_empty() {
            break;
        }
    }

    matches
}

fn looks_like_purpose_query(user_text: &str) -> bool {
    let lower = user_text.to_lowercase();
    let compact = lower.split_whitespace().collect::<String>();
    [
        "是幹嘛用的", "是幹啥用的", "是做什麼的", "是做啥的",
        "是什麼軟體", "是什麼工具", "是什麼系統", "是什麼程式",
        "幹嘛用的", "有什麼用", "有啥用途",
        "whatisthis", "whatdoesitdo", "whatsirin",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
}

fn push_unique_path(paths: &mut Vec<String>, path: &str) {
    let normalized = path.replace('\\', "/");
    if !normalized.is_empty() && !paths.contains(&normalized) {
        paths.push(normalized);
    }
}

pub(super) fn related_research_snippet(user_text: &str) -> Option<String> {
    let lower_text = user_text.to_lowercase();
    researcher::list_research()
        .ok()?
        .into_iter()
        .filter(|task| task.status == researcher::ResearchStatus::Done)
        .filter(|task| {
            task.topic
                .to_lowercase()
                .split_whitespace()
                .filter(|word| word.len() > 2)
                .any(|word| lower_text.contains(word))
        })
        .filter_map(|task| task.final_report)
        .map(|report| report.chars().take(600).collect::<String>())
        .next()
}
