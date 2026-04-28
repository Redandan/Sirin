//! HTTP wire types and transport functions for the Ollama and OpenAI-compatible
//! backends.  The public `call_prompt*` / `call_prompt_stream` functions in
//! [`super`] dispatch to these based on [`super::LlmBackend`].
//!
//! Streaming implementations handle both Ollama's one-JSON-per-newline format
//! and OpenAI's SSE `data: {…}\n\n` framing.  429 rate-limit responses are
//! retried up to 3 times with exponential back-off.
//!
//! ## Gemini concurrency limiting
//!
//! Gemini's free tier (and even most paid tiers) are aggressive about parallel
//! requests — the `gemini-2.5-flash` family caps at ~15 RPM and will silently
//! return empty `choices[0].message.content` (not 429) when several requests
//! arrive within the same second.  This breaks the test runner's batch mode
//! (default 8 parallel YAML tests, each issuing many `screenshot_analyze`
//! calls).
//!
//! The fix:
//! 1. A process-wide `tokio::sync::Semaphore` ([`gemini_semaphore`]) caps the
//!    number of in-flight Gemini calls at `GEMINI_CONCURRENCY` (default 3).
//!    Permits are held only for the duration of one HTTP round-trip; backoff
//!    sleep happens with the permit released so other waiters can proceed.
//! 2. Empty responses (HTTP 200 with no text) are treated as transient and
//!    retried with exponential backoff (2 s → 4 s, max 2 retries).  This is
//!    additive to the existing 429 retry loop.
//! 3. Both behaviours apply only when [`is_gemini_url`] matches the configured
//!    base URL — local Ollama / LM Studio paths are unaffected.

use std::sync::{Arc, OnceLock};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

// ── Gemini concurrency / empty-response retry helpers ────────────────────────

/// Returns `true` when `base_url` points at Google's Gemini API (any of the
/// `generativelanguage.googleapis.com` / `ai.google.dev` host families).
///
/// Used to scope rate-limiter and empty-retry behaviour to Gemini only — local
/// backends (Ollama, LM Studio) and other OpenAI-compatible providers keep
/// their original (unthrottled) call path.
fn is_gemini_url(base_url: &str) -> bool {
    let lower = base_url.to_ascii_lowercase();
    lower.contains("generativelanguage.googleapis.com")
        || lower.contains("ai.google.dev")
}

/// Process-wide semaphore that caps the number of concurrent Gemini requests.
///
/// Initialised on first use from the `GEMINI_CONCURRENCY` env var (default 3,
/// must be >0).  The cap is intentionally conservative — Gemini's free tier
/// allows ~15 RPM and parallel calls beyond ~3-5 reliably trigger empty
/// responses or 429s.
fn gemini_semaphore() -> &'static Arc<tokio::sync::Semaphore> {
    static SEM: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    SEM.get_or_init(|| {
        let n = std::env::var("GEMINI_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(3);
        crate::sirin_log!("[llm] gemini concurrency limit = {} (set GEMINI_CONCURRENCY to override)", n);
        Arc::new(tokio::sync::Semaphore::new(n))
    })
}

/// Max number of empty-response retries before giving up.
const GEMINI_EMPTY_MAX_RETRIES: u32 = 2;

// ── Token-bucket rate limiter for Gemini RPM ─────────────────────────────────
//
// The semaphore above caps *concurrent* requests; this bucket caps *requests
// per minute* (RPM).  Together they prevent both:
//   (a) thundering-herd when many tests run in parallel, and
//   (b) steady-state 429s when sequential tests exhaust the per-minute quota.
//
// Design: sliding-window token bucket.  Tokens refill continuously at
// `rpm / 60` per second.  Each API call consumes one token.  If the bucket
// is empty the call sleeps for exactly the time until the next token is
// available — no wasted time, no 429, no retry loop.
//
// Configure via `GEMINI_RPM` (default 8, leaving a 20% buffer on the 10 RPM
// free tier).  Set to 0 or omit to disable rate limiting.

struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_rate: f64,          // tokens per second
    last: std::time::Instant,
}

impl TokenBucket {
    fn new(rpm: u32) -> Self {
        let capacity = rpm as f64;
        Self {
            capacity,
            tokens: capacity,            // start full
            refill_rate: capacity / 60.0,
            last: std::time::Instant::now(),
        }
    }

    /// Returns the duration to sleep before proceeding (zero if immediately available).
    fn wait_duration(&mut self) -> std::time::Duration {
        let elapsed = self.last.elapsed().as_secs_f64();
        self.last = std::time::Instant::now();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            std::time::Duration::ZERO
        } else {
            // Time until we accumulate one token
            let wait_secs = (1.0 - self.tokens) / self.refill_rate;
            self.tokens = 0.0;
            std::time::Duration::from_secs_f64(wait_secs)
        }
    }
}

fn gemini_rate_limiter() -> Option<&'static std::sync::Mutex<TokenBucket>> {
    static BUCKET: OnceLock<Option<std::sync::Mutex<TokenBucket>>> = OnceLock::new();
    BUCKET
        .get_or_init(|| {
            let rpm = std::env::var("GEMINI_RPM")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(8); // 80% of the 10 RPM free-tier cap
            if rpm == 0 {
                None
            } else {
                crate::sirin_log!("[llm] gemini rate limiter = {} RPM (set GEMINI_RPM to override)", rpm);
                Some(std::sync::Mutex::new(TokenBucket::new(rpm)))
            }
        })
        .as_ref()
}

// ── HTTP request / response types (private to this module) ───────────────────

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    prompt: String,
    stream: bool,
    /// Controls how long Ollama keeps the model loaded after the request.
    /// Use `json!(-1)` to keep the model resident permanently (ideal for the
    /// small routing model), or a duration string like `"5m"` to unload it
    /// after a period of inactivity.  `None` uses the Ollama server default.
    /// Ignored by LM Studio / OpenAI-compatible backends.
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct OllamaStreamChunk {
    #[serde(default)]
    response: String,
    #[serde(default)]
    done: bool,
}

#[derive(Serialize)]
struct OpenAiStreamRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage>,
    stream: bool,
}

#[derive(Deserialize)]
struct OpenAiStreamChunk {
    choices: Vec<OpenAiStreamChoice>,
}

#[derive(Deserialize)]
struct OpenAiStreamChoice {
    delta: OpenAiDelta,
}

#[derive(Deserialize, Default)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage>,
    stream: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub(super) struct OpenAiMessage {
    pub role: String,
    /// Text string OR multimodal content array (for vision).
    pub content: serde_json::Value,
}

impl OpenAiMessage {
    /// Create a text-only message.
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self { role: role.into(), content: serde_json::Value::String(content.into()) }
    }

    /// Create a message with text + image (base64 PNG).
    pub fn with_image(role: &str, text: &str, image_base64: &str, mime: &str) -> Self {
        Self {
            role: role.into(),
            content: serde_json::json!([
                { "type": "text", "text": text },
                { "type": "image_url", "image_url": {
                    "url": format!("data:{mime};base64,{image_base64}")
                }}
            ]),
        }
    }

    /// Extract text content (whether string or array).
    pub fn text_content(&self) -> String {
        match &self.content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => {
                arr.iter()
                    .filter_map(|part| {
                        if part.get("type")?.as_str()? == "text" {
                            part.get("text")?.as_str().map(|s| s.to_string())
                        } else { None }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => String::new(),
        }
    }
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

// ── Non-streaming transport ──────────────────────────────────────────────────

pub(super) async fn call_ollama(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    prompt: String,
    keep_alive: Option<serde_json::Value>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    crate::sirin_log!(
        "[llm] call  backend=ollama model={} chars={}",
        model,
        prompt.len()
    );
    let url = format!("{}/api/generate", base_url.trim_end_matches('/'));
    let body = OllamaRequest {
        model,
        prompt,
        stream: false,
        keep_alive,
    };
    let resp: OllamaResponse = client
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.response.trim().to_string())
}

pub(super) async fn call_openai(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    prompt: String,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    call_openai_messages(
        client,
        base_url,
        model,
        api_key,
        vec![OpenAiMessage::text("user", prompt)],
    )
    .await
}

/// Send a pre-built messages array to an OpenAI-compatible endpoint.
///
/// Retry behaviour:
/// - **429 Too Many Requests** — up to 3 retries with exponential back-off
///   (30 s → 60 s → 120 s), honouring the `Retry-After` response header when
///   present.  Applies to all OpenAI-compatible backends.
/// - **Empty response** (HTTP 200 with no `choices[0].message.content`) — up
///   to [`GEMINI_EMPTY_MAX_RETRIES`] retries with short back-off (2 s → 4 s).
///   Gemini-only — other backends return the empty string as-is.
///
/// Concurrency:
/// - Gemini calls additionally acquire a permit from [`gemini_semaphore`]
///   before each HTTP round-trip.  The permit is held only for the duration
///   of the request — backoff sleep happens with the permit released.
pub(super) async fn call_openai_messages(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    messages: Vec<OpenAiMessage>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let total_chars: usize = messages.iter().map(|m| m.text_content().len()).sum();
    let gemini = is_gemini_url(base_url);
    crate::sirin_log!(
        "[llm] call  backend=openai-compat model={} msgs={} chars={} gemini={}",
        model,
        messages.len(),
        total_chars,
        gemini
    );
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = OpenAiRequest {
        model,
        messages,
        stream: false,
    };

    let mut rate_limit_attempt = 0u32;
    let mut empty_attempt = 0u32;
    loop {
        // ── Token-bucket rate throttle (Gemini only) ─────────────────────────
        // Sleep the EXACT amount needed to stay within GEMINI_RPM.
        // This runs BEFORE acquiring the concurrency semaphore so the sleep
        // does not hold a permit while waiting.
        if gemini {
            if let Some(limiter) = gemini_rate_limiter() {
                let wait = limiter
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .wait_duration();
                if !wait.is_zero() {
                    crate::sirin_log!(
                        "[llm] rate-limiter: sleeping {:.1}s to stay within GEMINI_RPM model={}",
                        wait.as_secs_f64(),
                        model
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }

        // Acquire a permit for Gemini, dropped at end of this iteration so
        // backoff sleeps don't hold up other waiters.  Local backends skip.
        let _gemini_permit = if gemini {
            Some(
                gemini_semaphore()
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|e| format!("gemini semaphore closed: {e}"))?,
            )
        } else {
            None
        };

        let mut req = client.post(&url).json(&body);
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        // OpenRouter requires HTTP-Referer + X-Title to use free-tier models.
        // Without these headers free calls return 402 Payment Required.
        if base_url.contains("openrouter.ai") {
            req = req
                .header("HTTP-Referer", "https://sirin.local")
                .header("X-Title", "Sirin Test Runner");
        }
        let resp = req.send().await?;

        // 429 path — release permit before sleep, retry.
        // With the token-bucket rate limiter active, 429s should be rare;
        // these retries are a safety net for transient API hiccups.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Check BEFORE sleeping so we do exactly `max_retries` sleeps:
            //   attempt 0: 429 → check(0>=3? no) → sleep → attempt=1
            //   attempt 1: 429 → check(1>=3? no) → sleep → attempt=2
            //   attempt 2: 429 → check(2>=3? no) → sleep → attempt=3
            //   attempt 3: 429 → check(3>=3? YES) → return Err  (3 sleeps, 4 requests)
            // NOTE: the mock-server test in mod.rs validates this is exactly 4 requests.
            if rate_limit_attempt >= 3 {
                crate::sirin_log!("[llm] 429 max retries exceeded model={}", model);
                // Return a detectable error so callers can trigger LLM fallback
                // immediately rather than propagating an HTTP status error.
                return Err(format!(
                    "429 rate-limited: max retries exceeded for model={model}"
                )
                .into());
            }
            // Honour Retry-After if present; else use progressive backoff.
            // With the token-bucket limiter these are rare; keep waits short.
            let wait_secs = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(5u64 << rate_limit_attempt); // 5 → 10 → 20
            crate::sirin_log!(
                "[llm] 429 rate-limited — waiting {}s (attempt {}/3) model={}",
                wait_secs,
                rate_limit_attempt + 1,
                model
            );
            drop(_gemini_permit);
            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
            rate_limit_attempt += 1;
            continue;
        }

        let parsed: OpenAiResponse = resp.error_for_status()?.json().await?;
        let reply = parsed
            .choices
            .first()
            .map(|c| c.message.text_content().trim().to_string())
            .unwrap_or_default();

        // Empty-response retry — only applies to Gemini, where this is a
        // known concurrent-request failure mode (returns 200 + empty content
        // instead of 429 when the per-second budget is exceeded).
        if reply.is_empty() && gemini && empty_attempt < GEMINI_EMPTY_MAX_RETRIES {
            let wait_secs = 2u64 << empty_attempt; // 2 → 4
            crate::sirin_log!(
                "[llm] empty response from Gemini — retrying in {}s (attempt {}/{}) model={}",
                wait_secs,
                empty_attempt + 1,
                GEMINI_EMPTY_MAX_RETRIES,
                model
            );
            drop(_gemini_permit);
            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
            empty_attempt += 1;
            continue;
        }

        crate::sirin_log!(
            "[llm] resp  backend=openai-compat model={} reply_chars={}",
            model,
            reply.len()
        );
        return Ok(reply);
    }
}

// ── Streaming transport ──────────────────────────────────────────────────────

pub(super) async fn stream_ollama<F>(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    prompt: String,
    on_token: F,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(String) + Send,
{
    let url = format!("{}/api/generate", base_url.trim_end_matches('/'));
    let body = OllamaRequest {
        model,
        prompt,
        stream: true,
        keep_alive: None,
    };
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;

    let mut stream = resp.bytes_stream();
    let mut full = String::new();
    let mut buf = Vec::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buf.extend_from_slice(&bytes);

        // Ollama sends one JSON object per newline.
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes = buf.drain(..=pos).collect::<Vec<_>>();
            if let Ok(line) = std::str::from_utf8(&line_bytes) {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(chunk) = serde_json::from_str::<OllamaStreamChunk>(line) {
                    if !chunk.response.is_empty() {
                        on_token(chunk.response.clone());
                        full.push_str(&chunk.response);
                    }
                    if chunk.done {
                        break;
                    }
                }
            }
        }
    }

    Ok(full.trim().to_string())
}

pub(super) async fn stream_openai<F>(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    prompt: String,
    on_token: F,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(String) + Send,
{
    let gemini = is_gemini_url(base_url);
    crate::sirin_log!(
        "[llm] stream backend=openai-compat model={} chars={} gemini={}",
        model,
        prompt.len(),
        gemini
    );
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = OpenAiStreamRequest {
        model,
        messages: vec![OpenAiMessage::text("user", prompt)],
        stream: true,
    };

    // Streaming path: hold the Gemini permit for the duration of the stream
    // (not just the initial POST) — releasing mid-stream would let another
    // call start before the server is done writing tokens to us.
    let _gemini_permit = if gemini {
        Some(
            gemini_semaphore()
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| format!("gemini semaphore closed: {e}"))?,
        )
    } else {
        None
    };

    let mut attempt = 0u32;
    let resp = loop {
        let mut req = client.post(&url).json(&body);
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        let r = req.send().await?;
        if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if attempt >= 3 {
                crate::sirin_log!("[llm] 429 max retries exceeded model={} (stream)", model);
                return Err(r.error_for_status().unwrap_err().into());
            }
            let wait_secs = r
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(30u64 << attempt);
            crate::sirin_log!(
                "[llm] 429 rate-limited — waiting {}s (attempt {}/3) model={} (stream)",
                wait_secs,
                attempt + 1,
                model
            );
            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
            attempt += 1;
            continue;
        }
        break r.error_for_status()?;
    };

    let mut stream = resp.bytes_stream();
    let mut full = String::new();
    let mut buf = Vec::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buf.extend_from_slice(&bytes);

        // OpenAI SSE: each message is "data: <json>\n\n" or "data: [DONE]\n\n".
        while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
            let line_bytes = buf.drain(..pos + 2).collect::<Vec<_>>();
            if let Ok(line) = std::str::from_utf8(&line_bytes) {
                for line in line.lines() {
                    let data = line.trim_start_matches("data:").trim();
                    if data == "[DONE]" || data.is_empty() {
                        continue;
                    }
                    if let Ok(ch) = serde_json::from_str::<OpenAiStreamChunk>(data) {
                        if let Some(content) = ch
                            .choices
                            .first()
                            .and_then(|c| c.delta.content.as_deref())
                            .filter(|s| !s.is_empty())
                        {
                            on_token(content.to_string());
                            full.push_str(content);
                        }
                    }
                }
            }
        }
    }

    Ok(full.trim().to_string())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_gemini_url_matches_official_endpoints() {
        assert!(is_gemini_url(
            "https://generativelanguage.googleapis.com/v1beta/openai"
        ));
        assert!(is_gemini_url(
            "https://generativelanguage.googleapis.com/v1beta/openai/"
        ));
        assert!(is_gemini_url("https://ai.google.dev/api"));
        // Case-insensitive — env vars / configs sometimes mix case.
        assert!(is_gemini_url(
            "HTTPS://GENERATIVELANGUAGE.GOOGLEAPIS.COM/v1beta/openai"
        ));
    }

    #[test]
    fn is_gemini_url_rejects_other_backends() {
        assert!(!is_gemini_url("http://localhost:11434"));
        assert!(!is_gemini_url("http://localhost:1234/v1"));
        assert!(!is_gemini_url("https://api.anthropic.com/v1"));
        assert!(!is_gemini_url("https://api.openai.com/v1"));
    }

    #[test]
    fn gemini_semaphore_initialises_with_default_concurrency() {
        // First access should not panic; cap is set lazily from env or default 3.
        let sem = gemini_semaphore();
        // Default should be > 0 so we can acquire at least one permit.
        let permits_now = sem.available_permits();
        assert!(
            permits_now > 0,
            "gemini semaphore should have at least 1 permit available, got {permits_now}"
        );
    }

    #[tokio::test]
    async fn gemini_semaphore_caps_concurrent_acquires() {
        // Force-init the semaphore (env-driven cap, default 3).
        let sem = gemini_semaphore().clone();
        let cap = sem.available_permits();

        // Acquire `cap` permits — all should succeed without blocking.
        let mut held = Vec::with_capacity(cap);
        for _ in 0..cap {
            held.push(sem.clone().acquire_owned().await.expect("permit"));
        }
        assert_eq!(sem.available_permits(), 0, "all permits should be in flight");

        // try_acquire_owned should now fail (no slots left) instead of blocking.
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "extra acquire should fail when at capacity"
        );

        // Release one and verify a slot frees up.
        held.pop();
        assert_eq!(sem.available_permits(), 1, "1 permit should be back");

        // Drop remaining and verify full restoration.
        drop(held);
        assert_eq!(sem.available_permits(), cap, "all permits should be returned");
    }
}
