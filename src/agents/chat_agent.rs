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
    is_identity_question, is_mixed_language_reply,
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
            let direct_answer_request = is_direct_answer_request(&request.user_text);
            let meta_request =
                is_identity_question(&request.user_text) || is_code_access_question(&request.user_text);

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

            if request.execution_result.is_none() {
                if is_simple_meta_request(&request.user_text) {
                    let reply = shortcut_reply(&request.user_text, persona.as_ref())
                        .unwrap_or_else(|| fallback_reply.clone());
                    ctx.record_system_event(
                        "adk_chat_meta_reply",
                        Some(preview_text(&request.user_text)),
                        Some("DONE"),
                        Some("identity_or_code_capability".to_string()),
                    );
                    let response = ChatAgentResponse {
                        reply,
                        ..Default::default()
                    };
                    return serde_json::to_value(response).map_err(|e| e.to_string());
                }

                if let Some(response) = maybe_build_direct_code_reply(&request.user_text, ctx, request.peer_id).await {
                    return serde_json::to_value(response).map_err(|e| e.to_string());
                }
            }

            let context_block = resolve_context_block(&request, ctx);

            // ── Route: ReAct loop for open-ended queries; linear path otherwise ──
            let use_react = !direct_answer_request
                && !meta_request
                && request.execution_result.is_none()
                && (should_search(&request.user_text)
                    || looks_like_code_query(&request.user_text));

            let ai_reply = if use_react {
                ctx.record_system_event(
                    "adk_chat_react_start",
                    Some(preview_text(&request.user_text)),
                    Some("RUNNING"),
                    None,
                );
                let persona_name = persona.as_ref().map(|p| p.name()).unwrap_or("Sirin");
                let reply = react_loop(ctx, &request.user_text, persona_name, context_block.as_deref()).await;
                if reply.trim().is_empty() { fallback_reply.clone() } else { reply }
            } else {
                // Linear path: deterministic context resolution + single LLM call.
                let search_context = resolve_search_context(&request, ctx, client.as_ref(), llm.as_ref(), direct_answer_request).await;
                let memory_context = resolve_memory_context(&request.user_text, ctx).await;
                let code_context = resolve_code_context(&request.user_text, ctx).await;

                match generate_ai_reply(
                    client.as_ref(),
                    llm.as_ref(),
                    persona.as_ref(),
                    &request.user_text,
                    request.execution_result.as_deref(),
                    search_context.as_deref(),
                    context_block.as_deref(),
                    memory_context.as_deref(),
                    code_context.as_deref(),
                    direct_answer_request,
                    false,
                )
                .await
                {
                    Ok(v) if !v.trim().is_empty() => v,
                    Ok(_) => fallback_reply.clone(),
                    Err(e) => {
                        ctx.record_system_event(
                            "adk_chat_llm_error",
                            Some(preview_text(&request.user_text)),
                            Some("FOLLOWUP_NEEDED"),
                            Some(e.to_string()),
                        );
                        fallback_reply.clone()
                    }
                }
            };

            let final_reply = if contains_cjk(&request.user_text)
                && (!contains_cjk(&ai_reply) || is_mixed_language_reply(&ai_reply))
            {
                match generate_ai_reply(
                    client.as_ref(),
                    llm.as_ref(),
                    persona.as_ref(),
                    &request.user_text,
                    request.execution_result.as_deref(),
                    None,
                    context_block.as_deref(),
                    None,
                    None,
                    direct_answer_request,
                    true,
                )
                .await
                {
                    Ok(v) if !v.trim().is_empty() && contains_cjk(&v) => v,
                    Ok(_) | Err(_) => chinese_fallback_reply(
                        &request.user_text,
                        request.execution_result.as_deref(),
                    ),
                }
            } else {
                ai_reply
            };

            let tools_used = ctx.tool_calls_snapshot();
            let response = ChatAgentResponse {
                reply: final_reply,
                used_search: use_react || tools_used.iter().any(|t| t == "web_search"),
                used_memory: tools_used.iter().any(|t| t == "memory_search"),
                used_code_context: used_code_tools(&tools_used),
                tools_used,
                trace: ctx.event_trace_snapshot(),
            };

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
    let asks_explain = ["是什麼", "是啥", "說明", "说明", "解釋", "解释", "用途", "作用", "幹嘛", "做什麼", "做什么"]
        .iter()
        .any(|needle| user_text.contains(needle));

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
    lower.contains("skill") || lower.contains("skills.rs") || user_text.contains("技能")
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

fn infer_focus_paths_from_query(user_text: &str, peer_id: Option<i64>) -> Vec<String> {
    let lower = user_text.to_lowercase();
    let mut paths = Vec::new();

    if let Some(path) = extract_file_reference(user_text) {
        push_unique_path(&mut paths, &path);
    }

    if is_contextual_file_explanation_request(user_text) {
        for path in recent_context_file_references(peer_id) {
            push_unique_path(&mut paths, &path);
        }
    }

    if looks_like_skill_query(user_text) {
        for path in [
            "src/skills.rs",
            "src/agents/planner_agent.rs",
            "src/agents/router_agent.rs",
            "src/adk/tool.rs",
        ] {
            push_unique_path(&mut paths, path);
        }
    }

    if looks_like_project_overview_query(user_text) || lower.contains("怎麼運作") || lower.contains("如何運作") {
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

    paths
}

fn build_analysis_focus_summary(user_text: &str, evidence_paths: &[String]) -> Option<String> {
    if evidence_paths.is_empty() {
        return None;
    }

    let focus = if looks_like_skill_query(user_text) {
        "分析這個專案裡 skill 的定義方式、能力目錄與 routing 用法"
    } else if looks_like_project_overview_query(user_text) {
        "分析專案架構與主要模組的分工"
    } else if looks_like_analysis_request(user_text) && looks_like_code_query(user_text) {
        "先查看相關本地檔案，再根據證據整理答案"
    } else {
        return None;
    };

    Some(format!("Focus: {focus}\nPrimary evidence files: {}", evidence_paths.join(", ")))
}

fn shortcut_reply(user_text: &str, persona: Option<&Persona>) -> Option<String> {
    let asks_identity = is_identity_question(user_text);
    let asks_code_access = is_code_access_question(user_text);

    if !(asks_identity || asks_code_access) {
        return None;
    }

    let persona_name = persona.map(|p| p.name()).unwrap_or("Sirin");
    let mut parts = Vec::new();

    if asks_identity {
        parts.push(format!(
            "我是 {persona_name}，你的本地 AI 助手，會協助聊天、研究、任務追蹤，並分析這個專案。"
        ));
    }

    if asks_code_access {
        parts.push(
            "可以，我能真的查本地專案的程式碼與檔案。你可以直接說像 `幫我看 src/main.rs`、`解釋 src/ui.rs`、`這個專案是什麼架構`，我會根據實際檔案內容回覆，不只是模板答案。"
                .to_string(),
        );
    }

    Some(parts.join(" "))
}

fn used_code_tools(tools: &[String]) -> bool {
    tools.iter().any(|tool| matches!(tool.as_str(), "codebase_search" | "local_file_read" | "project_overview"))
}

fn extract_labeled_value<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty())
}

fn extract_file_paths_from_reports(reports: &[String]) -> Vec<String> {
    let mut paths = Vec::new();

    for report in reports {
        if let Some(path) = extract_labeled_value(report, "File: ") {
            let path = path.to_string();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }

    paths
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

fn format_project_overview_reply(summary: &str, files: &[String], inspected_reports: &[String]) -> Option<String> {
    if summary.trim().is_empty() && files.is_empty() && inspected_reports.is_empty() {
        return None;
    }

    let inspected_list = inspected_reports
        .iter()
        .take(4)
        .filter_map(|report| summarize_file_report_line(report))
        .collect::<Vec<_>>()
        .join("\n");

    let file_list = files
        .iter()
        .take(8)
        .map(|path| format!("- `{path}`"))
        .collect::<Vec<_>>()
        .join("\n");

    Some(format!(
        "我已先查看幾個關鍵檔案，再整理目前工作區的大概：\n- 專案概況：{}\n\n{}\n\n目前可直接查的關鍵檔案：\n{}\n\n你可以直接叫我像 `幫我看 src/main.rs`、`解釋 src/ui.rs`，或問我某個模組的大致用途。",
        if summary.trim().is_empty() { "（未提供摘要）" } else { summary.trim() },
        if inspected_list.is_empty() {
            "已檢查的核心檔案：\n- （本輪未載入檔案內容）".to_string()
        } else {
            format!("已檢查的核心檔案：\n{inspected_list}")
        },
        if file_list.is_empty() { "- （暫無檔案列表）" } else { &file_list }
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

fn format_contextual_file_explanation_reply(file_reports: &[String]) -> Option<String> {
    if file_reports.is_empty() {
        return None;
    }

    let lines = file_reports
        .iter()
        .take(4)
        .filter_map(|report| summarize_file_report_line(report))
        .collect::<Vec<_>>();

    if lines.is_empty() {
        None
    } else {
        Some(format!(
            "你剛剛提到的那些檔案，我已根據本地內容整理如下：\n{}\n\n如果你要，我可以再挑其中一個繼續細講，像 `解釋 src/main.rs`。",
            lines.join("\n")
        ))
    }
}

fn format_skill_analysis_reply(file_reports: &[String]) -> Option<String> {
    if file_reports.is_empty() {
        return None;
    }

    let mut bullets = Vec::new();
    for report in file_reports.iter().take(4) {
        let Some(path) = extract_labeled_value(report, "File: ") else {
            continue;
        };
        let bullet = match path {
            "src/skills.rs" => Some("- `src/skills.rs`：定義 skill catalog，本質上是能力目錄，包含分類、背後 tools 與 example prompts。".to_string()),
            "src/agents/planner_agent.rs" => Some("- `src/agents/planner_agent.rs`：會先拿 `recommended_skills`，再把它們帶進 plan 與 steps。".to_string()),
            "src/agents/router_agent.rs" => Some("- `src/agents/router_agent.rs`：會參考 `recommended_skills` 來決定偏向走 `chat` 還是 `research`。".to_string()),
            "src/adk/tool.rs" => Some("- `src/adk/tool.rs`：把 `skill_catalog` / `skill_execute` 暴露成 ADK tools，讓 agents 可以查詢能力。".to_string()),
            _ => summarize_file_report_line(report),
        };

        if let Some(line) = bullet {
            bullets.push(line);
        }
    }

    if bullets.is_empty() {
        None
    } else {
        Some(format!(
            "我先查了本地相關檔案後，結論是：在這個專案裡，`skill` 比較像**能力目錄 + routing 提示**，不是獨立的大型執行引擎。\n{}\n\n如果你要，我可以再深入解釋 `src/skills.rs` 或 planner/router 的接法。",
            bullets.join("\n")
        ))
    }
}

async fn maybe_build_direct_code_reply(
    user_text: &str,
    ctx: &AgentContext,
    peer_id: Option<i64>,
) -> Option<ChatAgentResponse> {
    if let Some(path) = extract_file_reference(user_text) {
        match ctx
            .call_tool("local_file_read", json!({ "path": path, "max_chars": 2200 }))
            .await
        {
            Ok(result) => {
                if let Some(content) = result.get("content").and_then(Value::as_str) {
                    if let Some(reply) = format_local_file_reply(content) {
                        ctx.record_system_event(
                            "adk_chat_direct_file_reply",
                            Some(preview_text(user_text)),
                            Some("DONE"),
                            result
                                .get("path")
                                .and_then(Value::as_str)
                                .map(|path| format!("path={path}")),
                        );
                        let tools_used = ctx.tool_calls_snapshot();
                        return Some(ChatAgentResponse {
                            reply,
                            used_code_context: true,
                            tools_used,
                            trace: ctx.event_trace_snapshot(),
                            ..Default::default()
                        });
                    }
                }
            }
            Err(err) => {
                ctx.record_system_event(
                    "adk_chat_direct_file_reply_error",
                    Some(preview_text(user_text)),
                    Some("FOLLOWUP_NEEDED"),
                    Some(err),
                );
            }
        }
    }

    if is_contextual_file_explanation_request(user_text) {
        let paths = recent_context_file_references(peer_id);
        let reports = load_local_file_reports(ctx, user_text, &paths, 4, 1200).await;

        if let Some(reply) = format_contextual_file_explanation_reply(&reports) {
            ctx.record_system_event(
                "adk_chat_contextual_file_reply",
                Some(preview_text(user_text)),
                Some("DONE"),
                Some(format!("files={}", reports.len())),
            );
            let tools_used = ctx.tool_calls_snapshot();
            return Some(ChatAgentResponse {
                reply,
                used_code_context: true,
                tools_used,
                trace: ctx.event_trace_snapshot(),
                ..Default::default()
            });
        }
    }

    if looks_like_skill_query(user_text) {
        let paths = infer_focus_paths_from_query(user_text, peer_id);
        let reports = load_local_file_reports(ctx, user_text, &paths, 4, 1400).await;

        if let Some(reply) = format_skill_analysis_reply(&reports) {
            ctx.record_system_event(
                "adk_chat_skill_analysis_reply",
                Some(preview_text(user_text)),
                Some("DONE"),
                Some(format!("files={}", reports.len())),
            );
            let tools_used = ctx.tool_calls_snapshot();
            return Some(ChatAgentResponse {
                reply,
                used_code_context: true,
                tools_used,
                trace: ctx.event_trace_snapshot(),
                ..Default::default()
            });
        }
    }

    if looks_like_project_overview_query(user_text) {
        match ctx.call_tool("project_overview", json!({ "limit": 8 })).await {
            Ok(result) => {
                let summary = result
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let files = result
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                let inspected_reports = load_local_file_reports(ctx, user_text, &files, 4, 1200).await;

                if let Some(reply) = format_project_overview_reply(summary, &files, &inspected_reports) {
                    ctx.record_system_event(
                        "adk_chat_direct_project_overview",
                        Some(preview_text(user_text)),
                        Some("DONE"),
                        Some(format!("files={}, inspected={}", files.len(), inspected_reports.len())),
                    );
                    let tools_used = ctx.tool_calls_snapshot();
                    return Some(ChatAgentResponse {
                        reply,
                        used_code_context: true,
                        tools_used,
                        trace: ctx.event_trace_snapshot(),
                        ..Default::default()
                    });
                }
            }
            Err(err) => {
                ctx.record_system_event(
                    "adk_chat_direct_project_overview_error",
                    Some(preview_text(user_text)),
                    Some("FOLLOWUP_NEEDED"),
                    Some(err),
                );
            }
        }
    }

    None
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

async fn resolve_code_context(user_text: &str, ctx: &AgentContext) -> Option<String> {
    if !looks_like_code_query(user_text) {
        return None;
    }

    let mut blocks = Vec::new();
    let inferred_paths = infer_focus_paths_from_query(user_text, None);

    if let Some(summary) = build_analysis_focus_summary(user_text, &inferred_paths) {
        ctx.record_system_event(
            "adk_chat_analysis_focus",
            Some(preview_text(user_text)),
            Some("RUNNING"),
            Some(summary.clone()),
        );
        blocks.push(summary);
    }

    let inferred_reports = load_local_file_reports(ctx, user_text, &inferred_paths, 4, 1600).await;
    if !inferred_reports.is_empty() {
        blocks.push(format!(
            "Grounded local evidence:\n{}",
            inferred_reports.join("\n\n===\n\n")
        ));
    }

    if looks_like_project_overview_query(user_text) {
        match ctx.call_tool("project_overview", json!({ "limit": 5 })).await {
            Ok(result) => {
                let summary = result
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let files: Vec<String> = result
                    .get("files")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect();

                if !summary.is_empty() || !files.is_empty() {
                    ctx.record_system_event(
                        "adk_chat_project_overview_loaded",
                        Some(preview_text(user_text)),
                        Some("RUNNING"),
                        Some(format!("files={}", files.len())),
                    );

                    let mut overview = String::new();
                    if !summary.is_empty() {
                        overview.push_str("Project overview:\n");
                        overview.push_str(&summary);
                    }
                    if !files.is_empty() {
                        if !overview.is_empty() {
                            overview.push_str("\n\n");
                        }
                        overview.push_str("Key files:\n");
                        overview.push_str(&files.join("\n\n---\n\n"));
                    }
                    blocks.push(overview);
                }
            }
            Err(err) => {
                ctx.record_system_event(
                    "adk_chat_project_overview_error",
                    Some(preview_text(user_text)),
                    Some("FOLLOWUP_NEEDED"),
                    Some(err),
                );
            }
        }
    }

    match ctx
        .call_tool("codebase_search", json!({ "query": user_text, "limit": 3 }))
        .await
    {
        Ok(results) => {
            let entries: Vec<String> = results
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|s| s.to_string())
                .collect();

            if !entries.is_empty() {
                ctx.record_system_event(
                    "adk_chat_code_context_loaded",
                    Some(preview_text(user_text)),
                    Some("RUNNING"),
                    Some(format!("matches={}", entries.len())),
                );
                blocks.push(entries.join("\n\n---\n\n"));

                let relevant_paths = extract_file_paths_from_reports(&entries);
                let inspected_reports = load_local_file_reports(ctx, user_text, &relevant_paths, 3, 1400).await;
                if !inspected_reports.is_empty() {
                    blocks.push(format!(
                        "Relevant local file excerpts:\n{}",
                        inspected_reports.join("\n\n===\n\n")
                    ));
                }
            }
        }
        Err(err) => {
            ctx.record_system_event(
                "adk_chat_code_context_error",
                Some(preview_text(user_text)),
                Some("FOLLOWUP_NEEDED"),
                Some(err),
            );
        }
    }

    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n===\n\n"))
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
    call_prompt(ctx.http.as_ref(), ctx.llm.as_ref(), final_prompt)
        .await
        .unwrap_or_default()
        .lines()
        .find(|l| l.trim().starts_with("[ANSWER]"))
        .and_then(|l| l.trim().strip_prefix("[ANSWER]").map(|s| s.trim().to_string()))
        .unwrap_or_default()
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

pub async fn run_chat_response_via_adk_with_tracker(
    request: ChatRequest,
    tracker: Option<TaskTracker>,
) -> ChatAgentResponse {
    let fallback_reply = request
        .fallback_reply
        .clone()
        .unwrap_or_else(|| chinese_fallback_reply(&request.user_text, request.execution_result.as_deref()));

    let runtime = AgentRuntime::default();
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
/// For the linear (non-ReAct) path, tokens are delivered to `on_token` as they
/// arrive so the caller can update the chat bubble progressively.
/// For ReAct queries the function runs the standard blocking pipeline and
/// returns the full reply without streaming (tokens are too interleaved with
/// tool calls to stream meaningfully).
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
    let direct_answer_request = is_direct_answer_request(&request.user_text);
    let meta_request =
        is_identity_question(&request.user_text) || is_code_access_question(&request.user_text);

    if request.execution_result.is_none() {
        if is_simple_meta_request(&request.user_text) {
            let reply = shortcut_reply(&request.user_text, persona.as_ref())
                .unwrap_or_else(|| chinese_fallback_reply(&request.user_text, request.execution_result.as_deref()));
            on_token(reply.clone());
            return ChatAgentResponse {
                reply,
                ..Default::default()
            };
        }

        if let Some(response) = maybe_build_direct_code_reply(&request.user_text, &ctx, request.peer_id).await {
            on_token(response.reply.clone());
            return response;
        }
    }

    let use_react = !direct_answer_request
        && !meta_request
        && request.execution_result.is_none()
        && (should_search(&request.user_text) || looks_like_code_query(&request.user_text));

    if use_react {
        // ReAct path — interleaved tool calls; deliver complete reply at end.
        let persona_name = persona.as_ref().map(|p| p.name()).unwrap_or("Sirin");
        let context_block = resolve_context_block(&request, &ctx);
        let reply = react_loop(&ctx, &request.user_text, persona_name, context_block.as_deref()).await;
        let reply = if reply.trim().is_empty() {
            chinese_fallback_reply(&request.user_text, request.execution_result.as_deref())
        } else {
            reply
        };
        return ChatAgentResponse {
            reply,
            used_search: true,
            tools_used: ctx.tool_calls_snapshot(),
            trace: ctx.event_trace_snapshot(),
            ..Default::default()
        };
    }

    // Linear path — gather context, build prompt, stream tokens.
    let client = Arc::clone(&ctx.http);
    let llm = Arc::clone(&ctx.llm);
    let context_block = resolve_context_block(&request, &ctx);
    let search_ctx = resolve_search_context(
        &request,
        &ctx,
        client.as_ref(),
        llm.as_ref(),
        direct_answer_request,
    )
    .await;
    let memory_ctx = resolve_memory_context(&request.user_text, &ctx).await;
    let code_ctx = resolve_code_context(&request.user_text, &ctx).await;

    let prompt = build_ai_reply_prompt(
        persona.as_ref(),
        &request.user_text,
        request.execution_result.as_deref(),
        search_ctx.as_deref(),
        context_block.as_deref(),
        memory_ctx.as_deref(),
        code_ctx.as_deref(),
        direct_answer_request,
        false,
    );

    let reply = call_prompt_stream(client.as_ref(), llm.as_ref(), prompt, on_token)
        .await
        .unwrap_or_default();

    let reply = if reply.trim().is_empty() {
        chinese_fallback_reply(&request.user_text, request.execution_result.as_deref())
    } else {
        reply.trim().to_string()
    };

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
        let paths = infer_focus_paths_from_query("分析 skill", None);
        assert!(paths.contains(&"src/skills.rs".to_string()));
        assert!(paths.contains(&"src/agents/planner_agent.rs".to_string()));
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
    fn formats_direct_project_overview_reply() {
        let reply = format_project_overview_reply(
            "Rust desktop assistant with ADK-style agents.",
            &["src/main.rs".to_string(), "src/ui.rs".to_string()],
            &["File: src/main.rs\nKind: rust-source\nRole: App bootstrap\n\nExcerpt:\nfn main() {}".to_string()],
        )
        .expect("should build an overview reply");

        assert!(reply.contains("先查看幾個關鍵檔案"));
        assert!(reply.contains("`src/main.rs`"));
        assert!(reply.contains("ADK-style agents"));
    }

    #[test]
    fn extracts_file_paths_from_search_reports() {
        let paths = extract_file_paths_from_reports(&[
            "File: src/main.rs\nKind: rust-source\nRole: App bootstrap".to_string(),
            "File: src/ui.rs\nKind: rust-source\nRole: egui chat UI".to_string(),
        ]);

        assert_eq!(paths, vec!["src/main.rs".to_string(), "src/ui.rs".to_string()]);
    }

    #[test]
    fn formats_contextual_file_explanation_reply() {
        let reply = format_contextual_file_explanation_reply(&[
            "File: src/main.rs\nKind: rust-source\nRole: App bootstrap\n\nExcerpt:\nfn main() {}".to_string(),
            "File: src/ui.rs\nKind: rust-source\nRole: egui chat UI\n\nExcerpt:\nfn show_chat() {}".to_string(),
        ])
        .expect("should explain the referenced files");

        assert!(reply.contains("你剛剛提到的那些檔案"));
        assert!(reply.contains("`src/main.rs`"));
        assert!(reply.contains("egui chat UI"));
    }

    #[test]
    fn formats_skill_analysis_reply_from_local_evidence() {
        let reply = format_skill_analysis_reply(&[
            "File: src/skills.rs\nKind: rust-source\nRole: 技能與搜尋能力整合層。\n\nExcerpt:\npub fn list_skills() {}".to_string(),
            "File: src/agents/planner_agent.rs\nKind: rust-source\nRole: planner agent，先判斷使用者意圖與可能的步驟。\n\nExcerpt:\nrecommended_skills".to_string(),
        ])
        .expect("should build a grounded skill analysis reply");

        assert!(reply.contains("能力目錄"));
        assert!(reply.contains("`src/skills.rs`"));
        assert!(reply.contains("planner_agent.rs"));
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
            },
            None,
        )
        .await;

        println!("overview reply:\n{}", response.reply);
        assert!(response.reply.contains("專案概況"));
        assert!(response.reply.contains("已檢查的核心檔案"));
        assert!(response.used_code_context);
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
            },
            None,
        )
        .await;

        println!("file reply:\n{}", response.reply);
        assert!(response.reply.contains("`src/main.rs`"));
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
            },
            None,
        )
        .await;

        println!("follow-up reply:\n{}", second.reply);
        assert!(second.reply.contains("你剛剛提到的那些檔案"));
        assert!(second.reply.contains("`src/main.rs`") || second.reply.contains("`src/ui.rs`"));
        assert!(second.used_code_context);
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
            },
            None,
        )
        .await;

        println!("skill reply:\n{}", response.reply);
        assert!(response.reply.contains("`src/skills.rs`"));
        assert!(response.reply.contains("能力目錄") || response.reply.contains("routing 提示"));
        assert!(response.used_code_context);
    }
}
