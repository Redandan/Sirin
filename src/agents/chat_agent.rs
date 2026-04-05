use std::sync::Arc;

use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adk::{Agent, AgentContext, AgentRuntime};
use crate::llm::LlmConfig;
use crate::memory::{load_recent_context, looks_like_code_query};
use crate::persona::{Persona, TaskTracker};
use crate::researcher;
use crate::telegram::commands::{extract_search_query, should_search};
use crate::telegram::language::{
    chinese_fallback_reply, contains_cjk, is_code_access_question, is_direct_answer_request,
    is_identity_question,
};
use crate::telegram::llm::{build_ai_reply_prompt, generate_ai_reply};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub user_text: String,
    #[serde(default)]
    pub execution_result: Option<String>,
    #[serde(default)]
    pub context_block: Option<String>,
    #[serde(default)]
    pub fallback_reply: Option<String>,
    #[serde(default)]
    pub peer_id: Option<i64>,
    /// Intent family string forwarded by the Router from the Planner (snake_case).
    /// Used to skip the LLM understanding step when the Planner already classified the intent.
    #[serde(default)]
    pub planner_intent_family: Option<String>,
    /// Recommended skill IDs forwarded by the Router from the Planner.
    #[serde(default)]
    pub planner_skills: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatAgentResponse {
    pub reply: String,
    #[serde(default)]
    pub used_search: bool,
    #[serde(default)]
    pub used_memory: bool,
    #[serde(default)]
    pub used_code_context: bool,
    #[serde(default)]
    pub tools_used: Vec<String>,
    #[serde(default)]
    pub trace: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Intent {
    LocalFile,
    ProjectOverview,
    CodeAnalysis,
    CapabilityQuery,
    Correction,
    WebSearch,
    General,
}

struct MessageUnderstanding {
    intent: Intent,
    is_correction: bool,
    target_files: Vec<String>,
}

pub struct ChatAgent;

impl Agent for ChatAgent {
    fn name(&self) -> &'static str {
        "chat_agent"
    }

    fn run<'a>(
        &'a self,
        ctx: &'a AgentContext,
        input: Value,
    ) -> futures::future::BoxFuture<'a, Result<Value, String>> {
        async move {
            let request: ChatRequest = serde_json::from_value(input)
                .map_err(|e| format!("Invalid chat request payload: {e}"))?;

            let client = Arc::clone(&ctx.http);
            let llm = Arc::clone(&ctx.llm);
            let persona = Persona::load().ok();

            let behavior = ctx
                .call_tool(
                    "behavior_evaluate",
                    json!({
                        "source": ctx.source,
                        "msg": request.user_text,
                        "estimated_value": 0.0,
                        "record": ctx.tracker().is_some()
                    }),
                )
                .await
                .unwrap_or_else(|_| json!({}));
            if let Some(tier) = behavior.get("tier").and_then(Value::as_str) {
                ctx.record_system_event(
                    "adk_chat_behavior_evaluated",
                    Some(preview_text(&request.user_text)),
                    Some("RUNNING"),
                    Some(format!("tier={tier}")),
                );
            }
            let fallback_reply = request
                .fallback_reply
                .clone()
                .unwrap_or_else(|| chinese_fallback_reply(&request.user_text, request.execution_result.as_deref()));

            // ── Fast path: identity / meta questions need no LLM reasoning ──
            if request.execution_result.is_none() && is_simple_meta_request(&request.user_text) {
                let reply = shortcut_reply(&request.user_text, persona.as_ref())
                    .unwrap_or_else(|| fallback_reply.clone());
                ctx.record_system_event(
                    "adk_chat_meta_reply",
                    Some(preview_text(&request.user_text)),
                    Some("DONE"),
                    Some("identity_or_code_capability".to_string()),
                );
                let response = ChatAgentResponse { reply, ..Default::default() };
                return serde_json::to_value(response).map_err(|e| e.to_string());
            }

            // Resolve conversation context early — needed by the understanding step.
            let context_block = resolve_context_block(&request, ctx);

            // ── LLM understanding step ──────────────────────────────────────
            // Ask a compact LLM call to classify what the user wants before
            // doing any tool calls.  This replaces brittle keyword matching.
            let understanding = if request.execution_result.is_none() {
                let u = understand_message(ctx, &request.user_text, context_block.as_deref(), request.planner_intent_family.as_deref()).await;
                ctx.record_system_event(
                    "adk_chat_understood",
                    Some(preview_text(&request.user_text)),
                    Some("RUNNING"),
                    Some(format!(
                        "intent={:?} correction={} files={}",
                        u.intent, u.is_correction, u.target_files.len()
                    )),
                );
                u
            } else {
                // When an execution result is already present, skip understanding
                // and go straight to the General (linear LLM) path.
                MessageUnderstanding {
                    intent: Intent::General,
                    is_correction: false,
                    target_files: Vec::new(),
                }
            };

            // ── Route by understanding ──────────────────────────────────────
            let final_reply = dispatch_by_understanding(
                &understanding,
                &request,
                ctx,
                context_block.as_deref(),
                client.as_ref(),
                llm.as_ref(),
                persona.as_ref(),
            )
            .await
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| {
                ctx.record_system_event(
                    "adk_chat_dispatch_fallback",
                    Some(preview_text(&request.user_text)),
                    Some("FOLLOWUP_NEEDED"),
                    None,
                );
                fallback_reply.clone()
            });

            let tools_used = ctx.tool_calls_snapshot();
            let response = ChatAgentResponse {
                reply: final_reply.clone(),
                used_search: tools_used.iter().any(|t| t == "web_search"),
                used_memory: tools_used.iter().any(|t| t == "memory_search"),
                used_code_context: used_code_tools(&tools_used),
                tools_used,
                trace: ctx.event_trace_snapshot(),
            };

            crate::events::publish(crate::events::AgentEvent::ChatAgentReplied {
                peer_id: request.peer_id,
                preview: final_reply.chars().take(80).collect(),
            });

            serde_json::to_value(response).map_err(|e| e.to_string())
        }
        .boxed()
    }
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

/// Returns true when the user just wants to *view* a file (no specific question
/// about its contents), so a formatted excerpt is the right answer.
/// Returns false when the user is asking *about* something in the file — e.g.
/// "src/main.rs 裡的 main 函數做了什麼？" — in which case the LLM should read
/// the file and answer the question directly.
fn is_file_view_request(user_text: &str) -> bool {
    let lower = user_text.trim().to_lowercase();

    // Presence of any question / analysis word means the user wants an answer,
    // not just a file dump.
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

    // Explicit "show / read" verbs with no question → view request.
    let compact: String = lower.split_whitespace().collect::<String>();
    compact.starts_with("幫我看")
        || compact.starts_with("看一下")
        || compact.starts_with("看看")
        || compact.starts_with("讀取")
        || compact.starts_with("show")
        || compact.starts_with("read")
        || compact.starts_with("open")
        || compact.starts_with("cat")
        // Bare "看 src/..." — just the verb followed immediately by a path
        || (lower.trim_start().starts_with("看 ") && !has_question)
}

fn is_simple_meta_request(user_text: &str) -> bool {
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

fn looks_like_project_overview_query(user_text: &str) -> bool {
    let lower = user_text.to_lowercase();
    [
        "專案", "项目", "項目", "架構", "architecture", "結構", "模組", "module",
        "檔案", "files", "codebase", "這是啥", "這是什麼",
        "能看到什麼", "看到什麼", "看得到什麼", "哪些檔案",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

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

fn extract_file_reference(user_text: &str) -> Option<String> {
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

fn extract_file_references_from_text(text: &str) -> Vec<String> {
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

fn is_contextual_file_explanation_request(user_text: &str) -> bool {
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

fn looks_like_skill_query(user_text: &str) -> bool {
    let lower = user_text.to_lowercase();
    lower.contains("skill")
        || lower.contains("skills.rs")
        || user_text.contains("技能")
        || user_text.contains("能力目錄")
}

fn looks_like_capability_query(user_text: &str) -> bool {
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

fn is_skill_inventory_request(user_text: &str) -> bool {
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

fn looks_like_analysis_request(user_text: &str) -> bool {
    let lower = user_text.to_lowercase();
    [
        "分析", "解釋", "解释", "說明", "说明", "是什麼", "是啥", "用途", "作用", "如何", "怎麼", "为什么", "為什麼",
        "analyze", "explain", "how", "why",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn push_unique_path(paths: &mut Vec<String>, path: &str) {
    let normalized = path.replace('\\', "/");
    if !normalized.is_empty() && !paths.contains(&normalized) {
        paths.push(normalized);
    }
}

fn infer_focus_paths_from_query(
    user_text: &str,
    peer_id: Option<i64>,
    intent_family: Option<&str>,
) -> Vec<String> {
    let lower = user_text.to_lowercase();
    let mut paths = Vec::new();

    // ── Step 1: Planner intent → deterministic file sets (no keyword needed) ──
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
        Some("local_file") | Some("code_analysis") => {
            // Planner said it's about local files but didn't pin down which ones —
            // fall through to keyword / reference extraction below.
        }
        _ => {
            // No planner hint — fall through to full keyword matching.
        }
    }

    // ── Step 2: Explicit file reference in the message ─────────────────────────
    if let Some(path) = extract_file_reference(user_text) {
        push_unique_path(&mut paths, &path);
    }

    if is_contextual_file_explanation_request(user_text) {
        for path in recent_context_file_references(peer_id) {
            push_unique_path(&mut paths, &path);
        }
    }

    // ── Step 3: Keyword fallback (only when Planner gave no strong signal) ─────
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
        push_unique_path(&mut paths, "src/agents/chat_agent.rs");
    }
    if lower.contains("telegram") {
        push_unique_path(&mut paths, "src/telegram/mod.rs");
    }
    if lower.contains("memory") || user_text.contains("記憶") {
        push_unique_path(&mut paths, "src/memory.rs");
    }

    // Fallback: vague "code / local" query with no specific target.
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

fn shortcut_reply(user_text: &str, persona: Option<&Persona>) -> Option<String> {
    let asks_identity = is_identity_question(user_text);
    let asks_code_access = is_code_access_question(user_text);

    if !(asks_identity || asks_code_access) {
        return None;
    }

    let persona_name = persona.map(|p| p.name()).unwrap_or("Sirin");
    let use_chinese = contains_cjk(user_text);
    let mut parts = Vec::new();

    if asks_identity {
        if use_chinese {
            parts.push(format!(
                "我是 {persona_name}，你的本地 AI 助手，會協助聊天、研究、任務追蹤，並分析這個專案。"
            ));
        } else {
            parts.push(format!(
                "I'm {persona_name}, your local AI assistant. I help with chat, research, task tracking, and analysing this project's codebase."
            ));
        }
    }

    if asks_code_access {
        if use_chinese {
            parts.push(
                "可以，我能真的查本地專案的程式碼與檔案。你可以直接說像 `幫我看 src/main.rs`、`解釋 src/ui.rs`、`這個專案是什麼架構`，我會根據實際檔案內容回覆，不只是模板答案。"
                    .to_string(),
            );
        } else {
            parts.push(
                "Yes — I can read and analyse the actual local project files. Try: `show me src/main.rs`, `explain src/ui.rs`, or `what's the project architecture?`. I reply from real file content, not templates."
                    .to_string(),
            );
        }
    }

    Some(parts.join(" "))
}

fn used_code_tools(tools: &[String]) -> bool {
    tools.iter().any(|tool| {
        matches!(
            tool.as_str(),
            "codebase_search" | "local_file_read" | "project_overview" | "skill_catalog" | "skill_execute"
        )
    })
}

fn extract_labeled_value<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty())
}

fn summarize_file_report_line(report: &str) -> Option<String> {
    let path = extract_labeled_value(report, "File: ")?;
    let role = extract_labeled_value(report, "Role: ").unwrap_or("No summary available");
    let kind = extract_labeled_value(report, "Kind: ").unwrap_or("text");
    Some(format!("- `{path}`：{role}（{kind}）"))
}

fn extract_excerpt_block(text: &str) -> Option<&str> {
    text.split_once("\nExcerpt:\n")
        .map(|(_, excerpt)| excerpt.trim())
        .filter(|excerpt| !excerpt.is_empty())
}

fn code_fence_language(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "rs" => "rust",
        "toml" => "toml",
        "ts" => "ts",
        "tsx" => "tsx",
        "js" => "javascript",
        "jsx" => "jsx",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        _ => "text",
    }
}

fn truncate_for_reply(text: &str, max_lines: usize, max_chars: usize) -> String {
    let joined = text
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if joined.chars().count() <= max_chars {
        joined
    } else {
        let head: String = joined.chars().take(max_chars).collect();
        format!("{head}\n...")
    }
}

fn format_local_file_reply(file_report: &str) -> Option<String> {
    let path = extract_labeled_value(file_report, "File: ")?;
    let kind = extract_labeled_value(file_report, "Kind: ").unwrap_or("text");
    let role = extract_labeled_value(file_report, "Role: ").unwrap_or("No summary available");
    let excerpt = truncate_for_reply(extract_excerpt_block(file_report)?, 20, 1400);
    let fence = code_fence_language(path);

    Some(format!(
        "我已實際讀取 `{path}`。\n- 類型：{kind}\n- 作用：{role}\n\n檔案片段：\n```{fence}\n{excerpt}\n```\n\n如果你要，我可以再繼續解釋這個檔案的結構或逐段說明。"
    ))
}

async fn load_local_file_reports(
    ctx: &AgentContext,
    user_text: &str,
    paths: &[String],
    max_files: usize,
    max_chars: usize,
) -> Vec<String> {
    let mut reports = Vec::new();

    for path in paths.iter().take(max_files) {
        match ctx
            .call_tool("local_file_read", json!({ "path": path, "max_chars": max_chars }))
            .await
        {
            Ok(result) => {
                if let Some(content) = result.get("content").and_then(Value::as_str) {
                    reports.push(content.to_string());
                }
            }
            Err(err) => {
                ctx.record_system_event(
                    "adk_chat_local_file_read_error",
                    Some(preview_text(user_text)),
                    Some("FOLLOWUP_NEEDED"),
                    Some(err),
                );
            }
        }
    }

    if !reports.is_empty() {
        ctx.record_system_event(
            "adk_chat_local_files_inspected",
            Some(preview_text(user_text)),
            Some("RUNNING"),
            Some(format!("files={}", reports.len())),
        );
    }

    reports
}

/// Map a raw category string from skills.rs to a user-friendly display label.
/// Unknown categories fall through with their raw name, so new categories in
/// skills.rs automatically appear in the reply without any code change here.
fn category_display_label(category: &str) -> &str {
    match category {
        "code-understanding" | "context-retrieval" => "理解 / 查詢程式碼",
        "code-optimization" => "分析 / 修正 / 驗證",
        "external-research" | "external" => "外部能力",
        other => other,
    }
}

fn format_skill_catalog_reply(catalog: &Value, user_text: &str) -> Option<String> {
    let skills = catalog.as_array()?;
    if skills.is_empty() {
        return None;
    }

    // Group skill IDs by their actual category value (dynamic — no hardcoded enum).
    // BTreeMap keeps groups in a stable alphabetical order.
    let mut groups: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for skill in skills {
        let id = skill.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let category = skill
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("other")
            .to_string();
        groups.entry(category).or_default().push(format!("`{id}`"));
    }

    let use_chinese = contains_cjk(user_text);
    let sep = if use_chinese { "、" } else { ", " };

    let (header, footer) = if use_chinese {
        (
            "我目前在 `src/skills.rs` 裡有這些可用 skill：".to_string(),
            "如果你要，我可以直接示範其中一個，例如：`幫我看 src/main.rs`、`先分析再改`、`改完幫我測一下`。".to_string(),
        )
    } else {
        (
            "Here are the available skills defined in `src/skills.rs`:".to_string(),
            "I can demonstrate any of these — try: `show me src/main.rs`, `analyse then fix`, or `run tests after changes`.".to_string(),
        )
    };

    let mut lines = vec![header];
    for (category, ids) in &groups {
        lines.push(format!("- {}: {}", category_display_label(category), ids.join(sep)));
    }
    lines.push(footer);
    Some(lines.join("\n"))
}

fn resolve_context_block(request: &ChatRequest, ctx: &AgentContext) -> Option<String> {
    if request.context_block.is_some() {
        return request.context_block.clone();
    }

    if is_simple_meta_request(&request.user_text) {
        return None;
    }

    match load_recent_context(5, request.peer_id) {
        Ok(entries) if !entries.is_empty() => {
            ctx.record_system_event(
                "adk_chat_context_loaded",
                Some(preview_text(&request.user_text)),
                Some("RUNNING"),
                Some(format!("history_entries={}", entries.len())),
            );
            Some(
                entries
                    .iter()
                    .map(|e| format!("User: {}\nAssistant: {}", e.user_msg, e.assistant_reply))
                    .collect::<Vec<_>>()
                    .join("\n---\n"),
            )
        }
        Ok(_) => None,
        Err(err) => {
            ctx.record_system_event(
                "adk_chat_context_error",
                Some(preview_text(&request.user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err.to_string()),
            );
            None
        }
    }
}

async fn resolve_search_context(
    request: &ChatRequest,
    ctx: &AgentContext,
    client: &reqwest::Client,
    llm: &LlmConfig,
    direct_answer_request: bool,
) -> Option<String> {
    if direct_answer_request || !should_search(&request.user_text) {
        return None;
    }

    let query = extract_search_query(client, llm, &request.user_text).await;
    match ctx
        .call_tool("web_search", json!({ "query": query.clone(), "limit": 3 }))
        .await
    {
        Ok(results) => {
            let formatted = format_search_results(&results);
            if let Some(ref block) = formatted {
                let result_count = block.lines().count();
                ctx.record_system_event(
                    "adk_chat_search",
                    Some(preview_text(&request.user_text)),
                    Some("RUNNING"),
                    Some(format!("query={query}, result_lines={result_count}")),
                );
            }
            formatted
        }
        Err(err) => {
            ctx.record_system_event(
                "adk_chat_search_error",
                Some(preview_text(&request.user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err),
            );
            None
        }
    }
}

async fn resolve_memory_context(user_text: &str, ctx: &AgentContext) -> Option<String> {
    let mut blocks = Vec::new();
    let mut notes = Vec::new();

    match ctx
        .call_tool("memory_search", json!({ "query": user_text, "limit": 2 }))
        .await
    {
        Ok(results) => {
            if let Some(entries) = results.as_array() {
                let lines: Vec<String> = entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect();
                if !lines.is_empty() {
                    notes.push(format!("memory_hits={}", lines.len()));
                    blocks.push(lines.join("\n\n---\n\n"));
                }
            }
        }
        Err(err) => {
            ctx.record_system_event(
                "adk_chat_memory_error",
                Some(preview_text(user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err),
            );
        }
    }

    if let Some(report) = related_research_snippet(user_text) {
        notes.push("research_hit=1".to_string());
        blocks.push(format!("Recent related research:\n{report}"));
    }

    if !notes.is_empty() {
        ctx.record_system_event(
            "adk_chat_memory_loaded",
            Some(preview_text(user_text)),
            Some("RUNNING"),
            Some(notes.join(", ")),
        );
    }

    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n---\n\n"))
    }
}

// ── ReAct tool-use loop ──────────────────────────────���────────────────────────

/// Maximum number of tool-call iterations before forcing a final answer.
const REACT_MAX_TURNS: usize = 3;

/// Run a ReAct loop: the LLM decides which tools to call, we execute them and
/// feed results back until it produces a `[ANSWER]` tag or `REACT_MAX_TURNS`
/// is reached.
///
/// Bracket protocol (works with most local models):
/// ```
/// [SEARCH] query text        → calls web_search
/// [MEMORY] query text        → calls memory_search
/// [CODE]   query text        → calls codebase_search
/// [ANSWER] final reply text  → stops and returns the text
/// ```
/// Any response that contains none of the above tags is treated as a final answer.
pub async fn react_loop(
    ctx: &AgentContext,
    user_text: &str,
    persona_name: &str,
    initial_context: Option<&str>,
) -> String {
    use crate::llm::call_prompt;

    let tool_instructions = "\
You are an AI assistant. You can use these tools before answering:\n\
  [SEARCH] <query>  — search the web\n\
  [MEMORY] <query>  — recall past knowledge\n\
  [CODE]   <query>  — search this project's source code\n\
\n\
When you have enough information, reply with:\n\
  [ANSWER] <your response to the user>\n\
\n\
Use at most one tool per turn. If no tool is needed, go straight to [ANSWER].
These bracket tags are internal only; never mention them in the final user-facing reply.";

    let context_block = initial_context
        .map(|c| format!("\nContext:\n{c}\n"))
        .unwrap_or_default();

    // Conversation accumulated across iterations.
    let mut history = format!(
        "System: You are {persona_name}. {tool_instructions}{context_block}\n\nUser: {user_text}\n"
    );

    for _turn in 0..REACT_MAX_TURNS {
        let raw = match call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), history.clone()).await {
            Ok(r) => r,
            Err(_) => break,
        };

        // ── Parse the first recognised tag ─────────���─────────────────────────
        let mut tool_name: Option<&str> = None;
        let mut tool_query = String::new();
        let mut final_answer: Option<String> = None;

        for line in raw.lines() {
            let trimmed = line.trim();
            if let Some(q) = trimmed.strip_prefix("[SEARCH]") {
                tool_name = Some("web_search");
                tool_query = q.trim().to_string();
                break;
            } else if let Some(q) = trimmed.strip_prefix("[MEMORY]") {
                tool_name = Some("memory_search");
                tool_query = q.trim().to_string();
                break;
            } else if let Some(q) = trimmed.strip_prefix("[CODE]") {
                tool_name = Some("codebase_search");
                tool_query = q.trim().to_string();
                break;
            } else if let Some(ans) = trimmed.strip_prefix("[ANSWER]") {
                final_answer = Some(ans.trim().to_string());
                break;
            }
        }

        // ── Final answer ─────────────────────────��───────────────────────���────
        if let Some(ans) = final_answer {
            ctx.record_system_event(
                "adk_react_final_answer",
                Some(user_text.chars().take(60).collect()),
                Some("DONE"),
                None,
            );
            return ans;
        }

        // ── No recognised tag → treat whole response as answer ────────────────
        let Some(tool) = tool_name else {
            ctx.record_system_event(
                "adk_react_untagged_answer",
                Some(user_text.chars().take(60).collect()),
                Some("DONE"),
                None,
            );
            return raw;
        };

        // ── Execute tool ──────────────────────────────────────────────────────
        ctx.record_system_event(
            "adk_react_tool_call",
            Some(user_text.chars().take(60).collect()),
            Some("RUNNING"),
            Some(format!("tool={tool} query={}", &tool_query.chars().take(60).collect::<String>())),
        );

        let tool_result = ctx
            .call_tool(tool, serde_json::json!({ "query": tool_query, "limit": 3 }))
            .await
            .unwrap_or_else(|_| serde_json::Value::String("(no results)".into()));

        let result_text = match &tool_result {
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|v| {
                    if let serde_json::Value::Object(m) = v {
                        let title = m.get("title").and_then(|t| t.as_str()).unwrap_or("");
                        let snippet = m.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
                        Some(format!("- {title}: {snippet}"))
                    } else {
                        v.as_str().map(|s| format!("- {s}"))
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        // Append tool result to history and continue.
        history.push_str(&format!(
            "\nAssistant (tool call): [{tool_name_upper}] {tool_query}\nTool result:\n{result_text}\n",
            tool_name_upper = tool.to_uppercase(),
        ));
    }

    // Exhausted turns — ask for final answer without tools.
    let final_prompt = format!(
        "{history}\nSystem: You have used all available tool turns. \
         Provide a final [ANSWER] now based on what you know.\n"
    );
    let final_raw = call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), final_prompt)
        .await
        .unwrap_or_default();
    // Use [ANSWER] tag if present; otherwise use the whole response rather than
    // returning an empty string that silently falls through to the fallback reply.
    final_raw
        .lines()
        .find(|l| l.trim().starts_with("[ANSWER]"))
        .and_then(|l| l.trim().strip_prefix("[ANSWER]").map(|s| s.trim().to_string()))
        .unwrap_or_else(|| final_raw.trim().to_string())
}

/// Ask the LLM to classify what the user wants so we can route to the right path.
///
/// Fast paths (no LLM call):
///   1. Keyword classification gives a confident non-General result → use it directly.
///   2. Planner already forwarded a non-general intent_family → trust it.
///
/// Only falls through to a real LLM call for ambiguous `General` messages where
/// neither keywords nor the Planner produced a clear classification.
async fn understand_message(
    ctx: &AgentContext,
    user_text: &str,
    context_block: Option<&str>,
    planner_intent: Option<&str>,
) -> MessageUnderstanding {
    use crate::llm::call_prompt;

    // ── Fast path 1: Planner already classified the intent ───────────────
    // The Planner ran a dedicated LLM call; trust it over keyword heuristics.
    // Keyword matching only runs when the Planner is absent or returned General.
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
            // Still extract any explicit file references from the text so
            // the Chat Agent can read the right file even when Planner wins.
            let files = extract_file_references_from_text(user_text);
            return MessageUnderstanding {
                intent,
                is_correction: false,
                target_files: files,
            };
        }
    }

    // ── Fast path 2: keyword classification ──────────────────────────────
    // Exact token matches are cheaper and reliable for structured inputs like
    // file paths or capability phrases.  Only fall through to LLM for General.
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

    // Strip markdown fences some models add
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

fn related_research_snippet(user_text: &str) -> Option<String> {
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

fn format_search_results(results: &Value) -> Option<String> {
    let lines: Vec<String> = results
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let title = item.get("title").and_then(Value::as_str)?.trim();
            let snippet = item
                .get("snippet")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            let url = item.get("url").and_then(Value::as_str).unwrap_or_default().trim();

            if title.is_empty() {
                None
            } else {
                Some(format!("- {title}: {snippet} ({url})"))
            }
        })
        .collect();

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Core routing function: given the LLM's understanding of the message, call the
/// appropriate tools and produce a reply string.  Returns `None` when all paths
/// fail so the caller can substitute the fallback reply.
#[allow(clippy::too_many_arguments)]
async fn dispatch_by_understanding(
    understanding: &MessageUnderstanding,
    request: &ChatRequest,
    ctx: &AgentContext,
    context_block: Option<&str>,
    client: &reqwest::Client,
    llm: &LlmConfig,
    persona: Option<&crate::persona::Persona>,
) -> Option<String> {
    let persona_name = persona.map(|p| p.name()).unwrap_or("Sirin");
    let direct_answer = is_direct_answer_request(&request.user_text);

    match &understanding.intent {
        // ── Read a specific local file ──────────────────────────────────────
        //
        // Two sub-cases:
        //  • View request ("幫我看 src/main.rs") → formatted excerpt template.
        //  • Question about the file ("src/main.rs 裡的 main 函數做了什麼？")
        //    → pass file contents to LLM so it can actually answer the question.
        Intent::LocalFile => {
            let path = understanding
                .target_files
                .first()
                .cloned()
                .or_else(|| extract_file_reference(&request.user_text))?;

            match ctx
                .call_tool("local_file_read", json!({ "path": path, "max_chars": 2200 }))
                .await
            {
                Ok(result) => {
                    let content = result.get("content").and_then(Value::as_str)?;

                    if is_file_view_request(&request.user_text) {
                        // User just wants to see the file — use the concise template.
                        let reply = format_local_file_reply(content)?;
                        ctx.record_system_event(
                            "adk_chat_direct_file_reply",
                            Some(preview_text(&request.user_text)),
                            Some("DONE"),
                            Some(format!("path={path}")),
                        );
                        Some(reply)
                    } else {
                        // User has a specific question — let LLM answer using the file.
                        let excerpt = extract_excerpt_block(content).unwrap_or(content);
                        let fence = code_fence_language(&path);
                        let code_ctx =
                            format!("Contents of `{path}`:\n```{fence}\n{excerpt}\n```");
                        ctx.record_system_event(
                            "adk_chat_file_question_llm",
                            Some(preview_text(&request.user_text)),
                            Some("RUNNING"),
                            Some(format!("path={path}")),
                        );
                        generate_ai_reply(
                            client,
                            llm,
                            persona,
                            &request.user_text,
                            None,
                            None,
                            context_block,
                            None,
                            Some(&code_ctx),
                            direct_answer,
                            false,
                        )
                        .await
                        .ok()
                        .or_else(|| format_local_file_reply(content))
                    }
                }
                Err(err) => {
                    ctx.record_system_event(
                        "adk_chat_direct_file_reply_error",
                        Some(preview_text(&request.user_text)),
                        Some("FOLLOWUP_NEEDED"),
                        Some(err),
                    );
                    None
                }
            }
        }

        // ── User says the previous reply was wrong — re-examine ─────────────
        Intent::Correction => {
            let correction_ctx = context_block.map(|c| {
                format!(
                    "IMPORTANT: The user says the previous reply was inaccurate or outdated. \
                     Re-examine the current state of the codebase and provide a correct, \
                     up-to-date answer.\n\nPrevious conversation:\n{c}"
                )
            });
            ctx.record_system_event(
                "adk_chat_correction_react",
                Some(preview_text(&request.user_text)),
                Some("RUNNING"),
                None,
            );
            let reply = react_loop(
                ctx,
                &request.user_text,
                persona_name,
                correction_ctx.as_deref().or(context_block),
            )
            .await;
            if reply.trim().is_empty() { None } else { Some(reply) }
        }

        // ── Project structure / module overview ─────────────────────────────
        Intent::ProjectOverview => {
            let result = ctx
                .call_tool("project_overview", json!({ "limit": 8 }))
                .await
                .ok()?;
            let summary = result
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let files: Vec<String> = result
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();

            let reports = load_local_file_reports(ctx, &request.user_text, &files, 4, 1200).await;
            ctx.record_system_event(
                "adk_chat_project_overview_loaded",
                Some(preview_text(&request.user_text)),
                Some("RUNNING"),
                Some(format!("files={}, inspected={}", files.len(), reports.len())),
            );

            // Build rich code context and let the LLM synthesise a natural reply
            let mut code_ctx = String::new();
            if !summary.is_empty() {
                code_ctx.push_str("Project summary: ");
                code_ctx.push_str(summary);
                code_ctx.push_str("\n\n");
            }
            if !reports.is_empty() {
                code_ctx.push_str("Key files inspected:\n");
                for report in &reports {
                    if let Some(line) = summarize_file_report_line(report) {
                        code_ctx.push_str(&line);
                        code_ctx.push('\n');
                    }
                }
            }
            if !files.is_empty() {
                code_ctx.push_str("\nAll discovered files:\n");
                for f in files.iter().take(8) {
                    code_ctx.push_str(&format!("- {f}\n"));
                }
            }

            let memory_ctx = resolve_memory_context(&request.user_text, ctx).await;
            generate_ai_reply(
                client,
                llm,
                persona,
                &request.user_text,
                None,
                None,
                context_block,
                memory_ctx.as_deref(),
                Some(&code_ctx),
                direct_answer,
                false,
            )
            .await
            .ok()
        }

        // ── Code analysis — load files then reason with ReAct ───────────────
        Intent::CodeAnalysis => {
            let paths = if !understanding.target_files.is_empty() {
                understanding.target_files.clone()
            } else {
                infer_focus_paths_from_query(
                    &request.user_text,
                    request.peer_id,
                    request.planner_intent_family.as_deref(),
                )
            };

            let reports = load_local_file_reports(ctx, &request.user_text, &paths, 4, 1600).await;

            let memory_ctx = resolve_memory_context(&request.user_text, ctx).await;
            let combined_ctx = {
                let mut parts: Vec<&str> = Vec::new();
                let grounded;
                if !reports.is_empty() {
                    grounded = format!("Grounded local evidence:\n{}", reports.join("\n\n===\n\n"));
                    parts.push(&grounded);
                }
                if let Some(c) = context_block {
                    parts.push(c);
                }
                if let Some(ref m) = memory_ctx {
                    parts.push(m);
                }
                if parts.is_empty() { None } else { Some(parts.join("\n\n")) }
            };

            ctx.record_system_event(
                "adk_chat_code_analysis_react",
                Some(preview_text(&request.user_text)),
                Some("RUNNING"),
                Some(format!("files={}", reports.len())),
            );

            let reply = react_loop(ctx, &request.user_text, persona_name, combined_ctx.as_deref()).await;
            if reply.trim().is_empty() { None } else { Some(reply) }
        }

        // ── What skills / capabilities does the agent have? ─────────────────
        //
        // If the user asked for an explicit skill list ("你有啥skill"), use the
        // fixed template which is clear and concise.
        // For any other capability question ("你能幹嘛", "你是幹什麼的") pass the
        // catalog to the LLM so it can explain in natural language suited to the
        // question being asked.
        Intent::CapabilityQuery => {
            match ctx
                .call_tool("skill_catalog", json!({ "query": request.user_text }))
                .await
            {
                Ok(catalog) => {
                    let count = catalog.as_array().map(|v| v.len()).unwrap_or(0);
                    ctx.record_system_event(
                        "adk_chat_skill_catalog_reply",
                        Some(preview_text(&request.user_text)),
                        Some("DONE"),
                        Some(format!("count={count}")),
                    );

                    // Explicit skill-list request → use the concise template.
                    if is_skill_inventory_request(&request.user_text) {
                        format_skill_catalog_reply(&catalog, &request.user_text)
                    } else {
                        // General capability / identity question → let LLM synthesise.
                        let skill_lines: Vec<String> = catalog
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|s| {
                                let id = s.get("id").and_then(|v| v.as_str())?;
                                let desc = s.get("description").and_then(|v| v.as_str()).unwrap_or("");
                                Some(format!("- {id}: {desc}"))
                            })
                            .collect();
                        let catalog_ctx = format!(
                            "Available skills and capabilities:\n{}",
                            skill_lines.join("\n")
                        );
                        generate_ai_reply(
                            client,
                            llm,
                            persona,
                            &request.user_text,
                            None,
                            None,
                            context_block,
                            None,
                            Some(&catalog_ctx),
                            direct_answer,
                            false,
                        )
                        .await
                        .ok()
                        // Graceful degradation: if LLM is unavailable, fall back to the template.
                        .or_else(|| format_skill_catalog_reply(&catalog, &request.user_text))
                    }
                }
                Err(err) => {
                    ctx.record_system_event(
                        "adk_chat_skill_catalog_error",
                        Some(preview_text(&request.user_text)),
                        Some("FOLLOWUP_NEEDED"),
                        Some(err),
                    );
                    None
                }
            }
        }

        // ── Web / external information — use ReAct search loop ──────────────
        Intent::WebSearch => {
            ctx.record_system_event(
                "adk_chat_web_search_react",
                Some(preview_text(&request.user_text)),
                Some("RUNNING"),
                None,
            );
            let reply = react_loop(ctx, &request.user_text, persona_name, context_block).await;
            if reply.trim().is_empty() { None } else { Some(reply) }
        }

        // ── General conversation — linear LLM call with memory context ──────
        Intent::General => {
            let memory_ctx = resolve_memory_context(&request.user_text, ctx).await;
            let search_ctx = if should_search(&request.user_text) {
                resolve_search_context(request, ctx, client, llm, direct_answer).await
            } else {
                None
            };

            generate_ai_reply(
                client,
                llm,
                persona,
                &request.user_text,
                request.execution_result.as_deref(),
                search_ctx.as_deref(),
                context_block,
                memory_ctx.as_deref(),
                None,
                direct_answer,
                false,
            )
            .await
            .ok()
        }
    }
}

pub async fn run_chat_response_via_adk_with_tracker(
    request: ChatRequest,
    tracker: Option<TaskTracker>,
) -> ChatAgentResponse {
    let fallback_reply = request
        .fallback_reply
        .clone()
        .unwrap_or_else(|| chinese_fallback_reply(&request.user_text, request.execution_result.as_deref()));

    // Chat Agent uses a read-only registry — write tools (file_write, file_patch,
    // plan_execute, shell_exec) are intentionally excluded so the LLM cannot
    // accidentally or maliciously modify files through this agent.
    let runtime = AgentRuntime::new(crate::adk::tool::read_only_tool_registry());
    let ctx = runtime
        .context("chat_request")
        .with_optional_tracker(tracker)
        .with_metadata("agent", "chat_agent")
        .with_metadata("peer_id", request.peer_id.map(|id| id.to_string()).unwrap_or_else(|| "none".to_string()));

    match runtime.run(&ChatAgent, ctx, json!(request)).await {
        Ok(output) => serde_json::from_value(output).unwrap_or_else(|_| ChatAgentResponse {
            reply: fallback_reply,
            ..Default::default()
        }),
        Err(_) => ChatAgentResponse {
            reply: fallback_reply,
            ..Default::default()
        },
    }
}

pub async fn run_chat_via_adk_with_tracker(
    request: ChatRequest,
    tracker: Option<TaskTracker>,
) -> String {
    run_chat_response_via_adk_with_tracker(request, tracker)
        .await
        .reply
}

/// Streaming variant for the GUI chat tab.
///
/// All intents except `General` produce a complete reply (non-streaming) via
/// `dispatch_by_understanding`.  Only `General` streams tokens progressively so
/// the chat bubble updates as the LLM writes.
pub async fn stream_chat_response<F>(request: ChatRequest, on_token: F) -> ChatAgentResponse
where
    F: Fn(String) + Send + 'static,
{
    use crate::llm::call_prompt_stream;

    let runtime = AgentRuntime::default();
    let ctx = runtime
        .context("chat_stream")
        .with_metadata("agent", "chat_agent_stream");

    let persona = Persona::load().ok();
    let fallback_reply = request
        .fallback_reply
        .clone()
        .unwrap_or_else(|| chinese_fallback_reply(&request.user_text, request.execution_result.as_deref()));

    // ── Fast path: identity / meta ────────────────────────────────────────
    if request.execution_result.is_none() && is_simple_meta_request(&request.user_text) {
        let reply = shortcut_reply(&request.user_text, persona.as_ref())
            .unwrap_or_else(|| fallback_reply.clone());
        on_token(reply.clone());
        return ChatAgentResponse { reply, ..Default::default() };
    }

    let client = Arc::clone(&ctx.http);
    let llm = Arc::clone(&ctx.llm);
    let context_block = resolve_context_block(&request, &ctx);

    // ── LLM understanding step ────────────────────────────────────────────
    let understanding = if request.execution_result.is_none() {
        let u = understand_message(&ctx, &request.user_text, context_block.as_deref(), request.planner_intent_family.as_deref()).await;
        ctx.record_system_event(
            "adk_chat_understood",
            Some(preview_text(&request.user_text)),
            Some("RUNNING"),
            Some(format!(
                "intent={:?} correction={} files={}",
                u.intent, u.is_correction, u.target_files.len()
            )),
        );
        u
    } else {
        MessageUnderstanding {
            intent: Intent::General,
            is_correction: false,
            target_files: Vec::new(),
        }
    };

    // ── General intent: stream tokens; all others: block then deliver ─────
    let reply = if understanding.intent == Intent::General {
        let direct_answer_request = is_direct_answer_request(&request.user_text);
        let memory_ctx = resolve_memory_context(&request.user_text, &ctx).await;
        let search_ctx = if should_search(&request.user_text) {
            resolve_search_context(&request, &ctx, client.as_ref(), llm.as_ref(), direct_answer_request).await
        } else {
            None
        };

        let prompt = build_ai_reply_prompt(
            persona.as_ref(),
            &request.user_text,
            request.execution_result.as_deref(),
            search_ctx.as_deref(),
            context_block.as_deref(),
            memory_ctx.as_deref(),
            None,
            direct_answer_request,
            false,
        );

        call_prompt_stream(client.as_ref(), llm.as_ref(), prompt, on_token)
            .await
            .unwrap_or_default()
    } else {
        // Non-general intents: run dispatch (may call tools), deliver complete reply.
        let complete = dispatch_by_understanding(
            &understanding,
            &request,
            &ctx,
            context_block.as_deref(),
            client.as_ref(),
            llm.as_ref(),
            persona.as_ref(),
        )
        .await
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| fallback_reply.clone());

        on_token(complete.clone());
        complete
    };

    let reply = if reply.trim().is_empty() { fallback_reply } else { reply.trim().to_string() };

    let tools_used = ctx.tool_calls_snapshot();
    ChatAgentResponse {
        reply,
        used_search: tools_used.iter().any(|t| t == "web_search"),
        used_memory: tools_used.iter().any(|t| t == "memory_search"),
        used_code_context: used_code_tools(&tools_used),
        tools_used,
        trace: ctx.event_trace_snapshot(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::append_context;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_peer_id() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| (d.as_nanos() % 1_000_000_000) as i64)
            .unwrap_or(42)
    }

    #[test]
    fn formats_search_results_into_prompt_block() {
        let value = json!([
            {
                "title": "Rust async",
                "snippet": "Tokio drives the runtime",
                "url": "https://example.com/rust"
            }
        ]);

        let formatted = format_search_results(&value).expect("should format results");
        assert!(formatted.contains("Rust async"));
        assert!(formatted.contains("Tokio drives the runtime"));
    }

    #[test]
    fn shortcut_reply_covers_identity_and_code_access() {
        let reply = shortcut_reply("你是誰？你能看到程式碼嗎？", None)
            .expect("should provide a deterministic meta reply");
        assert!(reply.contains("Sirin"));
        assert!(reply.contains("本地 AI 助手"));
        assert!(reply.contains("程式碼"));
    }

    #[test]
    fn simple_meta_request_detection_is_narrow() {
        assert!(is_simple_meta_request("你是誰"));
        assert!(is_simple_meta_request("你能看到程序運行的代碼嗎"));
        assert!(!is_simple_meta_request("你現在能看到什麼檔案"));
        assert!(!is_simple_meta_request("請幫我分析 src/main.rs 的架構"));
    }

    #[test]
    fn detects_project_overview_queries() {
        assert!(looks_like_project_overview_query("這是什麼專案架構"));
        assert!(looks_like_project_overview_query("你現在能看到什麼檔案"));
        assert!(!looks_like_project_overview_query("今天天氣如何"));
    }

    #[test]
    fn extracts_file_references_from_queries() {
        assert_eq!(extract_file_reference("幫我看 src/main.rs"), Some("src/main.rs".to_string()));
        assert_eq!(extract_file_reference("請解釋 `src/ui.rs`"), Some("src/ui.rs".to_string()));
        assert_eq!(extract_file_reference("這個專案是什麼"), None);
    }

    #[test]
    fn extracts_file_references_from_assistant_reply() {
        let refs = extract_file_references_from_text(
            "目前可直接查的關鍵檔案：\n- `src/main.rs`\n- `src/ui.rs`\n- `Cargo.toml`\nNative egui/eframe UI for Sirin."
        );
        assert!(refs.contains(&"src/main.rs".to_string()));
        assert!(refs.contains(&"src/ui.rs".to_string()));
        assert!(refs.contains(&"Cargo.toml".to_string()));
        assert!(!refs.contains(&"egui/eframe".to_string()));
        assert!(!refs.contains(&"/".to_string()));
    }

    #[test]
    fn detects_contextual_file_explanation_requests() {
        assert!(is_contextual_file_explanation_request("說明這些都是啥"));
        assert!(is_contextual_file_explanation_request("上面那些檔案是做什麼的"));
        assert!(!is_contextual_file_explanation_request("幫我看 src/main.rs"));
    }

    #[test]
    fn infers_skills_rs_as_primary_evidence_for_skill_questions() {
        let paths = infer_focus_paths_from_query("分析 skill", None, None);
        assert!(paths.contains(&"src/skills.rs".to_string()));
        assert!(paths.contains(&"src/agents/planner_agent.rs".to_string()));
    }

    #[test]
    fn detects_skill_inventory_requests() {
        assert!(is_skill_inventory_request("你有哪些skill"));
        assert!(is_skill_inventory_request("你有什麼技能？"));
        assert!(!is_skill_inventory_request("分析 skill"));
    }

    #[test]
    fn detects_capability_queries() {
        assert!(looks_like_capability_query("你能做什麼？"));
        assert!(looks_like_capability_query("你可以幫我做什麼"));
        assert!(looks_like_capability_query("你有哪些 skill"));
        assert!(!looks_like_capability_query("分析 src/main.rs"));
    }

    #[test]
    fn formats_direct_local_file_reply_with_excerpt() {
        let reply = format_local_file_reply(
            "File: src/main.rs\nKind: rust-source\nRole: App bootstrap\n\nExcerpt:\nfn main() {\n    println!(\"hi\");\n}"
        )
        .expect("should build a direct local file reply");

        assert!(reply.contains("`src/main.rs`"));
        assert!(reply.contains("檔案片段"));
        assert!(reply.contains("println!"));
    }

    #[test]
    fn formats_skill_catalog_reply_with_actual_skill_ids() {
        let reply = format_skill_catalog_reply(&json!([
            {"id": "project_overview", "category": "code-understanding"},
            {"id": "local_file_read", "category": "code-understanding"},
            {"id": "grounded_fix", "category": "code-optimization"},
            {"id": "web_search", "category": "external-research"}
        ]), "有哪些 skills？")
        .expect("should build a skill inventory reply");

        assert!(reply.contains("`project_overview`"));
        assert!(reply.contains("`grounded_fix`"));
        assert!(reply.contains("`web_search`"));
    }

    #[tokio::test]
    async fn end_to_end_project_question_returns_grounded_overview() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "這個專案大概是怎麼運作的？".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
            },
            None,
        )
        .await;

        println!("overview reply:\n{}", response.reply);
        assert!(!response.reply.trim().is_empty(), "should return a non-empty overview");
        assert!(response.used_code_context, "should have used code context tools");
    }

    #[tokio::test]
    async fn end_to_end_file_question_reads_src_main() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "幫我看 src/main.rs".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
            },
            None,
        )
        .await;

        println!("file reply:\n{}", response.reply);
        assert!(response.reply.contains("src/main.rs"));
        assert!(response.reply.contains("檔案片段"));
        assert!(response.used_code_context);
    }

    #[tokio::test]
    async fn end_to_end_followup_question_explains_previous_files() {
        let peer_id = Some(unique_peer_id());
        let first = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "這個專案大概是怎麼運作的？".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id,
                planner_intent_family: None,
                planner_skills: Vec::new(),
            },
            None,
        )
        .await;
        append_context("這個專案大概是怎麼運作的？", &first.reply, peer_id)
            .expect("should store the prior assistant reply for follow-up testing");

        let second = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "說明這些檔案是做什麼的".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id,
                planner_intent_family: None,
                planner_skills: Vec::new(),
            },
            None,
        )
        .await;

        println!("follow-up reply:\n{}", second.reply);
        assert!(!second.reply.trim().is_empty(), "follow-up should return a non-empty reply");
        // The LLM should reference at least one of the files mentioned in the prior context
        assert!(
            second.reply.contains("src/main.rs") || second.reply.contains("src/ui.rs") || second.used_code_context,
            "follow-up should reference prior files or use code tools"
        );
    }

    #[tokio::test]
    async fn end_to_end_skill_question_returns_grounded_local_analysis() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "分析 skill".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
            },
            None,
        )
        .await;

        println!("skill reply:\n{}", response.reply);
        assert!(!response.reply.trim().is_empty(), "should return a non-empty skill analysis");
        assert!(response.used_code_context, "should have used code context tools");
    }

    #[tokio::test]
    async fn end_to_end_skill_inventory_question_lists_available_skills() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "你有哪些skill".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
            },
            None,
        )
        .await;

        println!("skill inventory reply:\n{}", response.reply);
        assert!(response.reply.contains("`project_overview`"));
        assert!(response.reply.contains("`local_file_read`"));
        assert!(response.reply.contains("`grounded_fix`"));
        assert!(response.used_code_context);
    }

    #[tokio::test]
    async fn end_to_end_capability_question_lists_available_skills() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "你可以幫我做什麼？".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
            },
            None,
        )
        .await;

        println!("capability reply:\n{}", response.reply);
        assert!(!response.reply.trim().is_empty(), "should return a non-empty capability reply");
        assert!(response.used_code_context, "should have used skill_catalog or code tools");
    }

    #[tokio::test]
    async fn end_to_end_generic_code_analysis_returns_grounded_summary() {
        let response = run_chat_response_via_adk_with_tracker(
            ChatRequest {
                user_text: "先分析 chat agent 的流程與可能問題".to_string(),
                execution_result: None,
                context_block: None,
                fallback_reply: None,
                peer_id: Some(unique_peer_id()),
                planner_intent_family: None,
                planner_skills: Vec::new(),
            },
            None,
        )
        .await;

        println!("generic analysis reply:\n{}", response.reply);
        assert!(!response.reply.trim().is_empty(), "should return a non-empty analysis");
        assert!(response.used_code_context, "should have used code context tools");
    }
}
