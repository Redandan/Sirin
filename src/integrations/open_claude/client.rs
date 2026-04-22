//! Open Claude in Chrome MCP client — connects to open-claude-in-chrome mcp-server.js
//!
//! Architecture:
//!
//! ┌─────────────────────┐    TCP :18765    ┌──────────────────────────┐
//! │ Sirin executor      │ ─────────────→   │ mcp-server.js (primary)  │
//! │ (open_claude_client)│  client_hello    │   TCP server on :18765   │
//! │                     │  tool_request    └───────────┬──────────────┘
//! └─────────────────────┘                              │ newline JSON
//!                                                      ↓
//!                                         ┌──────────────────────────┐
//!                                         │ native-host.js           │
//!                                         │ (Chrome native host)     │
//!                                         └───────────┬──────────────┘
//!                                                      │ Native Messaging
//!                                                      ↓
//!                                         ┌──────────────────────────┐
//!                                         │ Open Claude extension    │
//!                                         │ in user's Chrome         │
//!                                         └──────────────────────────┘
//!
//! Protocol (newline-delimited JSON, NOT JSON-RPC):
//!   Client → Server: {"type":"client_hello"}\n
//!   Server → Client: {"type":"client_ack","clientId":"1"}\n
//!   Client → Server: {"id":"1","type":"tool_request","tool":"navigate","args":{...}}\n
//!   Server → Client: {"id":"1","type":"tool_response","result":{...}}\n
//!                 or {"id":"1","type":"tool_error","error":"..."}\n

use serde_json::Value;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Open Claude MCP client configuration
#[derive(Clone, Debug)]
pub struct OpenClaudeConfig {
    pub host: String,
    pub port: u16,
    pub timeout_secs: u64,
    pub enabled: bool,
}

impl Default for OpenClaudeConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 18765,
            timeout_secs: 10,
            enabled: true,
        }
    }
}

/// Computer tool result (kept for API compatibility with executors)
#[derive(Debug, Clone, PartialEq)]
pub struct ComputerToolResult {
    pub x: u32,
    pub y: u32,
    pub action: String,
    pub text: Option<String>,
}

impl ComputerToolResult {
    /// Parse Claude's response to extract coordinates and action.
    /// Expected format: natural language like "click at (345, 280)"
    pub fn from_claude_response(text: &str) -> Option<Self> {
        let lower = text.to_lowercase();

        let action = if lower.contains("type") || lower.contains("input") {
            "type"
        } else if lower.contains("scroll") {
            "scroll"
        } else if lower.contains("click") || lower.contains("button") || lower.contains("tap") {
            "click"
        } else if lower.contains("screenshot") {
            "screenshot"
        } else {
            "click"
        }
        .to_string();

        let mut x = None;
        let mut y = None;

        if let Some(start) = text.find('(') {
            if let Some(end) = text[start + 1..].find(')') {
                let coord_str = &text[start + 1..start + 1 + end];
                let parts: Vec<&str> = coord_str.split(',').collect();
                if parts.len() == 2 {
                    if let (Ok(x_val), Ok(y_val)) = (
                        parts[0].trim().parse::<u32>(),
                        parts[1].trim().parse::<u32>(),
                    ) {
                        x = Some(x_val);
                        y = Some(y_val);
                    }
                }
            }
        }

        let mut text_to_type = None;
        if action == "type" {
            if let Some(start) = text.find('\'') {
                if let Some(end) = text[start + 1..].find('\'') {
                    text_to_type = Some(text[start + 1..start + 1 + end].to_string());
                }
            } else if let Some(start) = text.find('"') {
                if let Some(end) = text[start + 1..].find('"') {
                    text_to_type = Some(text[start + 1..start + 1 + end].to_string());
                }
            }
        }

        if let (Some(x_val), Some(y_val)) = (x, y) {
            return Some(ComputerToolResult {
                x: x_val,
                y: y_val,
                action,
                text: text_to_type,
            });
        }

        None
    }
}

/// Client for interacting with Open Claude's mcp-server.js on TCP :18765
#[derive(Debug)]
pub struct OpenClaudeClient {
    config: OpenClaudeConfig,
    request_id: std::sync::atomic::AtomicU64,
}

impl Clone for OpenClaudeClient {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            request_id: std::sync::atomic::AtomicU64::new(
                self.request_id.load(std::sync::atomic::Ordering::SeqCst),
            ),
        }
    }
}

impl OpenClaudeClient {
    pub fn new(config: OpenClaudeConfig) -> Self {
        Self {
            config,
            request_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Call the `computer` tool — takes a screenshot first to confirm the connection,
    /// and tries to parse coordinates from returned text.
    pub async fn computer_tool(&self, prompt: &str) -> Result<ComputerToolResult, String> {
        if !self.config.enabled {
            return Err("Open Claude client is disabled".to_string());
        }

        tracing::debug!("[open_claude_client] computer_tool prompt: {}", prompt);

        let tab_id = self.ensure_tab_id(None).await?;
        let result = self
            .send_tool_request(
                "computer",
                serde_json::json!({ "action": "screenshot", "tabId": tab_id }),
            )
            .await?;

        let text = Self::extract_primary_text(&result)
            .ok_or_else(|| "computer tool returned no text content".to_string())?;

        ComputerToolResult::from_claude_response(&text).ok_or_else(|| {
            format!(
                "computer_tool could not parse coordinates from response (tab={}, prompt='{}'): {}",
                tab_id, prompt, text
            )
        })
    }

    /// Get tab context from the extension (tabs_context_mcp tool).
    pub async fn get_tabs(&self) -> Result<Value, String> {
        self.send_tool_request(
            "tabs_context_mcp",
            serde_json::json!({ "createIfEmpty": true }),
        )
        .await
    }

    /// Read the page accessibility tree as text.
    pub async fn read_page(
        &self,
        tab_id: Option<u64>,
        filter: Option<&str>,
        depth: Option<u64>,
        max_chars: Option<u64>,
    ) -> Result<String, String> {
        let tid = self.ensure_tab_id(tab_id).await?;
        let result = self
            .send_tool_request(
                "read_page",
                serde_json::json!({
                    "tabId": tid,
                    "filter": filter.unwrap_or("all"),
                    "depth": depth.unwrap_or(12),
                    "max_chars": max_chars.unwrap_or(30000),
                }),
            )
            .await?;
        Self::extract_primary_text(&result)
            .ok_or_else(|| "read_page returned no text content".to_string())
    }

    /// Get page text for simple state verification.
    pub async fn get_page_text(&self, tab_id: Option<u64>) -> Result<String, String> {
        let tid = self.ensure_tab_id(tab_id).await?;
        let result = self
            .send_tool_request(
                "get_page_text",
                serde_json::json!({ "tabId": tid }),
            )
            .await?;
        Self::extract_primary_text(&result)
            .ok_or_else(|| "get_page_text returned no text content".to_string())
    }

    /// Fill a form field by ref.
    pub async fn form_input(
        &self,
        tab_id: Option<u64>,
        reference: &str,
        value: &str,
    ) -> Result<String, String> {
        let tid = self.ensure_tab_id(tab_id).await?;
        let result = self
            .send_tool_request(
                "form_input",
                serde_json::json!({
                    "ref": reference,
                    "value": value,
                    "tabId": tid,
                }),
            )
            .await?;
        Ok(Self::extract_primary_text(&result)
            .unwrap_or_else(|| format!("form_input completed on {}", reference)))
    }

    /// Click an element by ref via computer tool.
    pub async fn click_ref(&self, tab_id: Option<u64>, reference: &str) -> Result<String, String> {
        let tid = self.ensure_tab_id(tab_id).await?;
        let result = self
            .send_tool_request(
                "computer",
                serde_json::json!({
                    "action": "left_click",
                    "ref": reference,
                    "tabId": tid,
                }),
            )
            .await?;
        Ok(Self::extract_primary_text(&result)
            .unwrap_or_else(|| format!("click_ref completed on {}", reference)))
    }

    /// Press a keyboard key via computer tool.
    pub async fn press_key(&self, tab_id: Option<u64>, key: &str) -> Result<String, String> {
        let tid = self.ensure_tab_id(tab_id).await?;
        let result = self
            .send_tool_request(
                "computer",
                serde_json::json!({
                    "action": "key",
                    "text": key,
                    "tabId": tid,
                }),
            )
            .await?;
        Ok(Self::extract_primary_text(&result)
            .unwrap_or_else(|| format!("press_key completed for {}", key)))
    }

    /// Execute JavaScript in the current page and return textual result.
    pub async fn javascript_exec(&self, tab_id: Option<u64>, script: &str) -> Result<String, String> {
        let tid = self.ensure_tab_id(tab_id).await?;
        let result = self
            .send_tool_request(
                "javascript_tool",
                serde_json::json!({
                    "action": "javascript_exec",
                    "text": script,
                    "tabId": tid,
                }),
            )
            .await?;
        Ok(Self::extract_primary_text(&result)
            .unwrap_or_else(|| "javascript_exec completed".to_string()))
    }

    /// Wait for a small number of seconds via computer tool.
    pub async fn wait_seconds(&self, tab_id: Option<u64>, secs: u64) -> Result<String, String> {
        let tid = self.ensure_tab_id(tab_id).await?;
        let duration = secs.min(30);
        let result = self
            .send_tool_request(
                "computer",
                serde_json::json!({
                    "action": "wait",
                    "duration": duration,
                    "tabId": tid,
                }),
            )
            .await?;
        Ok(Self::extract_primary_text(&result)
            .unwrap_or_else(|| format!("wait_seconds completed for {}s", duration)))
    }

    /// Navigate to a URL in the given tab (or auto-selects first available tab).
    pub async fn navigate_url(&self, url: &str, tab_id: Option<u64>) -> Result<Value, String> {
        let tid = self.ensure_tab_id(tab_id).await?;
        self.send_tool_request(
            "navigate",
            serde_json::json!({ "url": url, "tabId": tid }),
        )
        .await
    }

    /// Take a screenshot of the given tab (or auto-selects first available tab).
    pub async fn take_screenshot(&self, tab_id: Option<u64>) -> Result<Value, String> {
        let tid = self.ensure_tab_id(tab_id).await?;
        self.send_tool_request(
            "computer",
            serde_json::json!({ "action": "screenshot", "tabId": tid }),
        )
        .await
    }

    async fn ensure_tab_id(&self, tab_id: Option<u64>) -> Result<u64, String> {
        if let Some(id) = tab_id {
            return Ok(id);
        }
        let tabs = self.get_tabs().await?;
        tabs.as_array()
            .and_then(|a| a.first())
            .and_then(|t| t.get("tabId").or_else(|| t.get("id")))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "No tab available".to_string())
    }

    fn extract_primary_text(result: &Value) -> Option<String> {
        result
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("text"))
            .and_then(|txt| txt.as_str())
            .map(|s| s.to_string())
    }

    /// Low-level: send a tool_request to mcp-server.js and return the result JSON.
    ///
    /// Protocol:
    ///   1. TCP connect to :18765
    ///   2. → `{"type":"client_hello"}\n`
    ///   3. ← `{"type":"client_ack","clientId":"..."}\n`
    ///   4. → `{"id":"N","type":"tool_request","tool":"<name>","args":{...}}\n`
    ///   5. ← `{"id":"N","type":"tool_response","result":{...}}\n`
    ///       or `{"id":"N","type":"tool_error","error":"..."}\n`
    pub async fn send_tool_request(&self, tool: &str, args: Value) -> Result<Value, String> {
        if !self.config.enabled {
            return Err("Open Claude client is disabled".to_string());
        }

        let addr = format!("{}:{}", self.config.host, self.config.port);

        let mut stream = tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs),
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| format!("Timeout connecting to mcp-server at {}", addr))?
        .map_err(|e| {
            format!(
                "mcp-server unavailable ({}): ensure Open Claude extension is running in Chrome",
                e
            )
        })?;

        // Step 1: client_hello
        let hello = format!("{}\n", serde_json::json!({"type": "client_hello"}));
        stream
            .write_all(hello.as_bytes())
            .await
            .map_err(|e| format!("Failed to send client_hello: {}", e))?;

        // Step 2: read client_ack
        let ack_line = Self::read_line_timeout(&mut stream, self.config.timeout_secs).await?;
        let ack: Value = serde_json::from_str(&ack_line)
            .map_err(|e| format!("Failed to parse client_ack '{}': {}", ack_line, e))?;
        if ack["type"] != "client_ack" {
            return Err(format!("Expected client_ack, got: {}", ack_line));
        }
        tracing::debug!(
            "[open_claude_client] client_ack: clientId={}",
            ack["clientId"]
        );

        // Step 3: send tool_request
        let id = self.next_id().to_string();
        let request = serde_json::json!({
            "id": id,
            "type": "tool_request",
            "tool": tool,
            "args": args,
        });
        let req_line = format!("{}\n", request);
        stream
            .write_all(req_line.as_bytes())
            .await
            .map_err(|e| format!("Failed to send tool_request: {}", e))?;

        tracing::info!(
            "[open_claude_client] sent tool_request: tool={} id={}",
            tool,
            id
        );

        // Step 4: read tool response (tool execution can take up to 60s)
        let resp_line = Self::read_line_timeout(&mut stream, 60)
            .await
            .map_err(|e| format!("Failed to read tool response: {}", e))?;
        let resp: Value = serde_json::from_str(&resp_line)
            .map_err(|e| format!("Failed to parse tool response '{}': {}", resp_line, e))?;

        if resp["type"] == "tool_error" {
            return Err(resp["error"]
                .as_str()
                .unwrap_or("Unknown tool error")
                .to_string());
        }

        tracing::info!("[open_claude_client] tool_response: tool={} ok", tool);
        Ok(resp["result"].clone())
    }

    /// Read a single newline-terminated line from the TCP stream with a timeout.
    async fn read_line_timeout(
        stream: &mut TcpStream,
        timeout_secs: u64,
    ) -> Result<String, String> {
        let mut buf = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("Timeout reading line from mcp-server".to_string());
            }
            let remaining = deadline - now;

            let mut byte = [0u8; 1];
            let n = tokio::time::timeout(remaining, stream.read(&mut byte))
                .await
                .map_err(|_| "Timeout reading line from mcp-server".to_string())?
                .map_err(|e| format!("Read error: {}", e))?;

            if n == 0 {
                if buf.is_empty() {
                    return Err("Connection closed before response".to_string());
                }
                break;
            }

            if byte[0] == b'\n' {
                break;
            }
            buf.push(byte[0]);
        }

        String::from_utf8(buf)
            .map_err(|e| format!("Non-UTF8 in mcp-server response: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::ComputerToolResult;

    #[test]
    fn parse_click_coordinates() {
        let input = "Click at (120, 345)";
        let parsed = ComputerToolResult::from_claude_response(input).expect("should parse");
        assert_eq!(parsed.action, "click");
        assert_eq!(parsed.x, 120);
        assert_eq!(parsed.y, 345);
    }

    #[test]
    fn parse_type_coordinates_and_text() {
        let input = "Type 'hello world' at (10, 20)";
        let parsed = ComputerToolResult::from_claude_response(input).expect("should parse");
        assert_eq!(parsed.action, "type");
        assert_eq!(parsed.x, 10);
        assert_eq!(parsed.y, 20);
        assert_eq!(parsed.text.as_deref(), Some("hello world"));
    }
}
