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
    code_context: Option<&str>,
    direct_answer_request: bool,
    force_traditional_chinese: bool,
    skill_context: Option<&str>,
) -> String {
    let persona_name = persona.map(|p| p.name()).unwrap_or("Sirin");
    let (voice, compliance) = persona
        .map(|p| {
            (
                p.response_style.voice.as_str(),
                p.response_style.compliance_line.as_str(),
            )
        })
        .unwrap_or((
            "natural, polite, professional",
            "Follow the user's request step by step.",
        ));

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

    let code_block = code_context
        .map(|v| format!("\nProject codebase context (use when the user asks about this app or its implementation):\n{v}"))
        .unwrap_or_default();

    let skill_block = skill_context
        .map(|s| format!("\n{s}\n"))
        .unwrap_or_default();

    let language_override = if force_traditional_chinese {
        "- Reply in Traditional Chinese only.\n- Use Traditional Chinese characters, not Simplified Chinese.\n"
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
- Keep response concise, but allow 3-6 sentences or a short bullet list when explaining code.\n\
- Be polite and human-like.\n\
- Reply in the same language as the user's message.\n\
- Always prioritise the latest user message over earlier chat history.\n\
- Use recent conversation context only when it is still relevant to the latest user message.\n\
- If the user asks who you are, answer clearly that you are {persona_name}, the local AI assistant for this project.\n\
- If the user asks whether you can inspect this app's code, answer yes: you can read and analyze the local project codebase and relevant files.\n\
- For local code questions, first synthesise the concrete evidence from the provided files/modules, then answer.\n\
- When project code context includes `Analysis focus`, `Grounded local evidence`, `File:`, or `Excerpt:`, explicitly cite the relevant file path and answer from that local content instead of giving a generic reply.\n\
- If the available local code context is insufficient, say which file you inspected and what is still missing.\n\
- Never mention internal tool tags or hidden reasoning such as [SEARCH], [MEMORY], or [CODE].\n\
- Do not self-introduce unless the user asks who you are.\n\
- Avoid sounding like a system prompt or policy statement.\n\
{language_override}
{direct_mode_constraints}
- If an internal action already ran, include a short result summary.\n\
{skill_block}\n\
User message: {user_text}\n\
{execution_block}{search_block}{history_block}{memory_block}{code_block}\n\
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
    code_context: Option<&str>,
    direct_answer_request: bool,
    force_traditional_chinese: bool,
    skill_context: Option<&str>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let prompt = build_ai_reply_prompt(
        persona,
        user_text,
        execution_result,
        search_context,
        context_block,
        memory_context,
        code_context,
        direct_answer_request,
        force_traditional_chinese,
        skill_context,
    );
    call_prompt(client, llm, prompt).await
}

#[cfg(test)]
mod tests {
    use super::build_ai_reply_prompt;

    #[test]
    fn prompt_includes_identity_and_code_rules() {
        let prompt = build_ai_reply_prompt(
            None,
            "你是誰？你能看到這個專案的程式碼嗎？",
            None,
            None,
            None,
            None,
            None,
            false,
            true,
            None,
        );

        assert!(prompt.contains("If the user asks who you are"));
        assert!(prompt.contains("If the user asks whether you can inspect this app's code"));
        assert!(prompt.contains("explicitly cite the relevant file path"));
        assert!(prompt.contains("Traditional Chinese"));
    }
}
