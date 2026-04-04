//! Telegram-specific LLM logic — prompt building and AI reply generation.
//!
//! The actual HTTP calls are delegated to [`crate::llm::call_prompt`].

use crate::llm::{call_prompt, LlmConfig};
use crate::persona::Persona;

// ── Prompt builder ────────────────────────────────────────────────────────────

pub fn build_ai_reply_prompt(
    persona: Option<&Persona>,
    user_text: &str,
    execution_result: Option<&str>,
    search_context: Option<&str>,
    context_block: Option<&str>,
    memory_context: Option<&str>,
    direct_answer_request: bool,
    force_traditional_chinese: bool,
) -> String {
    let persona_name = persona.map(|p| p.name()).unwrap_or("Sirin");
    let (voice, compliance) = persona
        .map(|p| {
            (
                p.response_style.voice.as_str(),
                p.response_style.compliance_line.as_str(),
            )
        })
        .unwrap_or(("natural, polite, professional", "Follow the user's request step by step."));

    let execution_block = execution_result
        .map(|v| format!("\nExecution result from internal action layer: {v}"))
        .unwrap_or_default();

    let search_block = search_context
        .map(|v| format!("\nWeb search results (use as reference, do not quote verbatim):\n{v}"))
        .unwrap_or_default();

    let history_block = context_block
        .map(|v| format!("\nRecent conversation history:\n{v}"))
        .unwrap_or_default();

    let memory_block = memory_context
        .map(|v| format!("\nPast research findings (reference only, summarise if relevant):\n{v}"))
        .unwrap_or_default();

    let language_override = if force_traditional_chinese {
        "- Reply in Traditional Chinese only.\n"
    } else {
        ""
    };

    let direct_mode_constraints = if direct_answer_request {
        "- The user asked for a direct answer: provide concrete steps immediately.\n\
- Do not include external links unless the user explicitly asks for links.\n\
"
    } else {
        ""
    };

    format!(
        "You are {persona_name}.\n\
Use this persona style: {voice}.\n\
Core rule: {compliance}\n\
Task: Reply to the latest user message naturally and helpfully.\n\
Constraints:\n\
- Keep response concise (1-3 sentences).\n\
- Be polite and human-like.\n\
- Reply in the same language as the user's message.\n\
- Continue from the recent conversation context instead of restarting the topic.\n\
- Do not self-introduce unless the user asks who you are.\n\
- Avoid sounding like a system prompt or policy statement.\n\
{language_override}
{direct_mode_constraints}
- If an internal action already ran, include a short result summary.\n\
\n\
User message: {user_text}\n\
{execution_block}{search_block}{history_block}{memory_block}\n\
\n\
Return only the final reply text."
    )
}

// ── Reply generator ───────────────────────────────────────────────────────────

pub async fn generate_ai_reply(
    client: &reqwest::Client,
    llm: &LlmConfig,
    persona: Option<&Persona>,
    user_text: &str,
    execution_result: Option<&str>,
    search_context: Option<&str>,
    context_block: Option<&str>,
    memory_context: Option<&str>,
    direct_answer_request: bool,
    force_traditional_chinese: bool,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = build_ai_reply_prompt(
        persona,
        user_text,
        execution_result,
        search_context,
        context_block,
        memory_context,
        direct_answer_request,
        force_traditional_chinese,
    );
    call_prompt(client, llm, prompt).await
}
