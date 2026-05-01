//! Assistant mode — Sirin handles general web automation tasks via natural
//! language requests.
//!
//! Distinct from Test mode (`test_runner`):
//!   - Input: natural language request (not YAML)
//!   - Observation: vision-first (screenshot + LLM every turn)
//!   - Goal: complete the task and return a result, not pass/fail
//!   - Chrome: same singleton as test runner (CDP direct)
//!
//! ## Supported tasks (examples)
//!   - "在 Google Maps 找附近泰式餐廳，過濾評分 > 4，回傳前 5 筆"
//!   - "翻譯這段泰文評論"
//!   - "在 Facebook 找 XX 修車廠並查詢聯絡方式"
//!
//! ## Architecture
//!
//! ```
//! assistant_task(request, url?) →
//!   vision-first ReAct loop (screenshot → analyze → action → repeat)
//! ```
//!
//! The vision model (`LLM_VISION_*`) analyzes screenshots each turn.
//! The main model decides which browser action to take next.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantResult {
    /// Natural language summary of what was accomplished.
    pub summary: String,
    /// Structured data extracted during the task (if any).
    pub data: Option<Value>,
    /// Final screenshot as base64 PNG (for context).
    pub screenshot_b64: Option<String>,
    /// Number of browser steps taken.
    pub steps: u32,
    /// Whether the task was completed successfully.
    pub success: bool,
}

// ── Task runner ───────────────────────────────────────────────────────────────

const MAX_ASSISTANT_ITER: u32 = 25;
const ASSISTANT_TIMEOUT_SECS: u64 = 300;

/// Run a natural-language assistant task using the vision-first ReAct loop.
///
/// `request` is the user's natural language request.
/// `start_url` is the optional starting page (e.g. "https://maps.google.com").
///
/// Returns [`AssistantResult`] with the outcome and any extracted data.
pub async fn run_task(
    ctx: &crate::adk::context::AgentContext,
    request: &str,
    start_url: Option<&str>,
) -> AssistantResult {
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(ASSISTANT_TIMEOUT_SECS);

    // Navigate to start URL if provided.
    if let Some(url) = start_url {
        let nav = json!({ "action": "goto", "target": url });
        let _ = ctx.call_tool("web_navigate", nav).await;
        std::thread::sleep(std::time::Duration::from_secs(3));
    }

    let system_prompt = build_system_prompt(request, start_url);
    let mut history: Vec<Value> = Vec::new();
    let mut step = 0u32;

    loop {
        if step >= MAX_ASSISTANT_ITER || std::time::Instant::now() >= deadline {
            break;
        }

        // Take screenshot every turn — passed to vision model for description.
        let screenshot_b64 = take_screenshot_b64(ctx).await;

        // Build observation: current URL (shown in prompt for context).
        let _obs = build_observation(ctx, &screenshot_b64).await;

        // Trim history to last 16 entries (8 turns) to control context size.
        if history.len() > 16 { history.drain(0..4); }

        // Add a "hurry up" nudge when approaching the step limit.
        let nudge = if step >= MAX_ASSISTANT_ITER.saturating_sub(3) {
            format!("\n\n⚠️ 只剩 {} 步！如果已有足夠資訊，現在就輸出 done=true。",
                MAX_ASSISTANT_ITER - step)
        } else { String::new() };

        // Ask LLM what to do next.
        let prompt = format!(
            "{}\n\n## 目前步驟 {}/{}{}",
            system_prompt, step + 1, MAX_ASSISTANT_ITER, nudge
        );

        let raw = match call_assistant_llm(ctx, &prompt, &history, screenshot_b64.as_deref()).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("[assistant] LLM error at step {step}: {e}");
                break;
            }
        };

        // Parse response — expect JSON with action or done.
        let parsed = parse_assistant_step(&raw);

        match parsed {
            AssistantStep::Done { summary, data } => {
                let final_ss = take_screenshot_b64(ctx).await;
                return AssistantResult {
                    summary,
                    data,
                    screenshot_b64: final_ss,
                    steps: step + 1,
                    success: true,
                };
            }
            AssistantStep::Action(action_input) => {
                history.push(json!({ "role": "assistant", "content": raw }));
                let label = action_input.get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                tracing::info!("[assistant] step {} — {}", step + 1, label);

                let result = ctx.call_tool("web_navigate", action_input).await;
                let obs = match result {
                    Ok(v) => format!("OK: {}", truncate(&v.to_string(), 400)),
                    Err(e) => format!("ERROR: {e}"),
                };
                history.push(json!({ "role": "user", "content": obs }));
            }
            AssistantStep::ParseError => {
                // LLM output wasn't valid JSON — tell it and retry.
                history.push(json!({
                    "role": "user",
                    "content": "上面的回應格式不對，請用 JSON 格式回應。"
                }));
            }
        }

        step += 1;
    }

    // Timed out or max iterations.
    let final_ss = take_screenshot_b64(ctx).await;
    AssistantResult {
        summary: format!(
            "任務未在 {} 步內完成（已執行 {} 步）。最後狀態見截圖。",
            MAX_ASSISTANT_ITER, step
        ),
        data: None,
        screenshot_b64: final_ss,
        steps: step,
        success: false,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn build_system_prompt(request: &str, start_url: Option<&str>) -> String {
    let url_hint = start_url
        .map(|u| format!("\n起始頁面：{u}"))
        .unwrap_or_default();

    format!(r#"你是 Sirin 助理，負責操作瀏覽器完成用戶的任務。

## 任務
{request}{url_hint}

## 重要：每次呼叫時，頁面的視覺描述已經提供在「視覺觀察」段落中。
❌ 禁止：不要輸出 screenshot_analyze action！視覺資訊已提前提供。
✅ 直接根據提供的視覺觀察決定下一步。

## 可用的 browser actions
- goto target=<url>           — 導航到 URL
- click_point x=<n> y=<n> coord_source=screenshot  — 點擊截圖座標
- shadow_click role=<r> name_regex=<re>  — 點擊 Flutter/JS shadow DOM 元素
- type target=<selector> text=<text>     — 在 input 欄位輸入文字
- eval target=<js>           — 執行 JavaScript（可提取頁面文字）
- scroll y=<px>              — 捲動頁面
- wait target=<ms>           — 等待毫秒

## 輸出格式（嚴格遵守）
每次回應**只能**輸出一個 JSON 物件，不要有任何其他文字：

行動時：
{{"action": "goto", "target": "https://..."}}
{{"action": "eval", "target": "document.title"}}
{{"action": "click_point", "x": 500, "y": 300, "coord_source": "screenshot"}}

完成時（有足夠資訊後立即輸出）：
{{"done": true, "summary": "任務完成說明", "data": {{"key": "value"}} }}

## 執行策略
1. 看視覺觀察 → 理解頁面狀態
2. 決定最直接的行動（或直接完成）
3. 如果頁面已有所需資訊，立即輸出 done=true
4. 使用 eval 提取文字比截圖更高效
5. 最多執行 {max} 步，到達上限前確保輸出 done=true
"#, max = MAX_ASSISTANT_ITER)
}

async fn build_observation(
    ctx: &crate::adk::context::AgentContext,
    _screenshot_b64: &Option<String>,
) -> String {
    // Get current URL for context.
    let url = ctx.call_tool("web_navigate", json!({ "action": "url" })).await
        .ok()
        .and_then(|v| v.get("url").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| "unknown".to_string());
    format!("當前 URL: {url}\n（截圖已隨 vision 分析提供）")
}

async fn take_screenshot_b64(ctx: &crate::adk::context::AgentContext) -> Option<String> {
    let result = ctx.call_tool("web_navigate", json!({ "action": "screenshot" })).await.ok()?;
    result.get("bytes_base64").and_then(Value::as_str).map(String::from)
}

/// Two-stage LLM call:
/// 1. Vision LLM describes what's on screen (if available)
/// 2. Main LLM decides what action to take based on the description
///
/// This avoids using a small vision model (qwen3-vl-8b) for JSON reasoning —
/// it's only used for visual observation, while the main model (DeepSeek/Gemini)
/// does the decision making.
async fn call_assistant_llm(
    _ctx: &crate::adk::context::AgentContext,
    prompt: &str,
    history: &[Value],
    screenshot_b64: Option<&str>,
) -> Result<String, String> {
    let llm = crate::llm::shared_llm();
    let http = crate::llm::shared_http();

    // Stage 1: Vision observation (if screenshot available and vision configured).
    let visual_desc = if let Some(b64) = screenshot_b64 {
        if let Some(vision_cfg) = crate::llm::vision_llm_config() {
            let vision_prompt = "簡短描述截圖上看到的頁面內容（50字以內）：標題、主要元素、頁面狀態。";
            crate::llm::call_vision(&http, &vision_cfg, vision_prompt, b64, "image/png").await.ok()
        } else {
            None
        }
    } else {
        None
    };

    // Stage 2: Main LLM decides action.
    // Include recent history so LLM can see previous eval results and decide.
    let history_str: String = history.iter().rev().take(8).collect::<Vec<_>>()
        .into_iter().rev()
        .filter_map(|msg| {
            let role = msg.get("role").and_then(Value::as_str)?;
            let content = msg.get("content").and_then(Value::as_str)?;
            Some(format!("[{role}]: {}", truncate(content, 500)))
        })
        .collect::<Vec<_>>()
        .join("\n");

    let full_prompt = format!(
        "{}\n\n## 行動記錄（最近）\n{}\n\n## 視覺觀察\n{}",
        prompt,
        if history_str.is_empty() { "（無）".to_string() } else { history_str },
        visual_desc.as_deref().unwrap_or("（截圖分析不可用）")
    );

    crate::llm::call_coding_prompt(&http, &llm, full_prompt).await.map_err(|e| e.to_string())
}

enum AssistantStep {
    Done { summary: String, data: Option<Value> },
    Action(Value),
    ParseError,
}

fn parse_assistant_step(raw: &str) -> AssistantStep {
    let cleaned = raw.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let v: Value = match serde_json::from_str(cleaned) {
        Ok(v) => v,
        Err(_) => {
            // Try to extract JSON from the response.
            if let Some(start) = cleaned.find('{') {
                if let Some(end) = cleaned.rfind('}') {
                    if let Ok(v) = serde_json::from_str(&cleaned[start..=end]) {
                        return parse_assistant_step_value(v);
                    }
                }
            }
            return AssistantStep::ParseError;
        }
    };

    parse_assistant_step_value(v)
}

fn parse_assistant_step_value(v: Value) -> AssistantStep {
    if v.get("done").and_then(Value::as_bool) == Some(true) {
        let summary = v.get("summary")
            .and_then(Value::as_str)
            .unwrap_or("完成")
            .to_string();
        let data = v.get("data").cloned();
        return AssistantStep::Done { summary, data };
    }

    if v.get("action").is_some() {
        return AssistantStep::Action(v);
    }

    AssistantStep::ParseError
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find a valid char boundary at or before `max` bytes.
        let boundary = s.char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= max)
            .last()
            .unwrap_or(0);
        &s[..boundary]
    }
}
