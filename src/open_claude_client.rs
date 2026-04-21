//! Open Claude in Chrome MCP client — fallback for precise coordinate-based browser control
//!
//! When Sirin's AXTree-based ui_automation fails (e.g., Canvas UI), this module
//! provides a fallback to Open Claude's `computer` tool via MCP, which offers:
//! - Precise pixel-level mouse/keyboard control (CDP dispatchMouseEvent)
//! - Full DOM traversal + JS execution
//! - No Canvas limitations
//!
//! Architecture:
//! ┌─────────────────────────────────────────┐
//! │ Sirin executor (ReAct loop)             │
//! │ (tries ax_find → fails 5 times)         │
//! └────────────┬────────────────────────────┘
//!              │ trigger_fallback()
//!              ↓
//! ┌─────────────────────────────────────────┐
//! │ open_claude_client (this module)        │
//! │ - Connect to MCP server (localhost:18765) │
//! │ - Call "computer" tool with prompt      │
//! │ - Parse response (coordinate + action)  │
//! └────────────┬────────────────────────────┘
//!              │ returns {x, y, action}
//!              ↓
//! ┌─────────────────────────────────────────┐
//! │ executor resumes (executes click)       │
//! │ (validated by subsequent ax_tree check) │
//! └─────────────────────────────────────────┘

use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Open Claude MCP client configuration
#[derive(Clone, Debug)]
pub struct OpenClaudeConfig {
    pub host: String,
    pub port: u16,
    pub timeout_secs: u64,
    pub enabled: bool,  // Can be disabled if Open Claude not available
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

/// Request sent to Open Claude MCP server
#[derive(Debug, Serialize)]
struct MpcRequest {
    jsonrpc: String,
    method: String,
    params: MpcParams,
    id: u64,
}

#[derive(Debug, Serialize)]
struct MpcParams {
    name: String,
    arguments: serde_json::Value,
}

/// Response from Open Claude MCP server
#[derive(Debug, Deserialize)]
struct MpcResponse {
    jsonrpc: String,
    result: Option<MpcResult>,
    error: Option<MpcError>,
    id: u64,
}

#[derive(Debug, Deserialize)]
struct MpcResult {
    content: Vec<MpcContent>,
}

#[derive(Debug, Deserialize)]
struct MpcContent {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MpcError {
    code: i32,
    message: String,
}

/// Computer tool result parsed from Open Claude
#[derive(Debug, Clone, PartialEq)]
pub struct ComputerToolResult {
    pub x: u32,
    pub y: u32,
    pub action: String,  // "click", "type", "screenshot", etc.
    pub text: Option<String>,  // For "type" action
}

impl ComputerToolResult {
    /// Parse Claude's response to extract coordinates and action
    /// Expected format: Claude will return natural language like:
    /// "I should click the 'Confirm Stake' button at coordinates (345, 280)"
    /// or "Type 'hello' in the input field at (200, 150)"
    pub fn from_claude_response(text: &str) -> Option<Self> {
        let lower = text.to_lowercase();
        
        // Determine action
        let action = if lower.contains("type") || lower.contains("input") {
            "type"
        } else if lower.contains("scroll") {
            "scroll"
        } else if lower.contains("click") || lower.contains("button") || lower.contains("tap") {
            "click"
        } else if lower.contains("screenshot") {
            "screenshot"
        } else {
            "click" // Default to click
        }.to_string();

        // Extract coordinates from patterns like "(123, 456)" or "123, 456"
        let mut x = None;
        let mut y = None;
        
        // Look for coordinate patterns: (X, Y)
        if let Some(start) = text.find('(') {
            if let Some(end) = text[start+1..].find(')') {
                let coord_str = &text[start+1..start+1+end];
                let parts: Vec<&str> = coord_str.split(',').collect();
                if parts.len() == 2 {
                    if let (Ok(x_val), Ok(y_val)) = (
                        parts[0].trim().parse::<u32>(),
                        parts[1].trim().parse::<u32>()
                    ) {
                        x = Some(x_val);
                        y = Some(y_val);
                    }
                }
            }
        }

        // Extract text for type action (between quotes)
        let mut text_to_type = None;
        if action == "type" {
            if let Some(start) = text.find('\'') {
                if let Some(end) = text[start+1..].find('\'') {
                    text_to_type = Some(text[start+1..start+1+end].to_string());
                }
            } else if let Some(start) = text.find('"') {
                if let Some(end) = text[start+1..].find('"') {
                    text_to_type = Some(text[start+1..start+1+end].to_string());
                }
            }
        }

        // Return result if we have coordinates
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

/// Client for interacting with Open Claude MCP server
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
                self.request_id.load(std::sync::atomic::Ordering::SeqCst)
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
        self.request_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Call Open Claude's "computer" tool to locate and interact with UI elements
    /// 
    /// Example prompt:
    /// "Take a screenshot and find the button with text 'Confirm Stake'.
    ///  Return the exact coordinates and action needed to click it."
    pub async fn computer_tool(&self, prompt: &str) -> Result<ComputerToolResult, String> {
        if !self.config.enabled {
            return Err("Open Claude client is disabled".to_string());
        }

        let request = MpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "tools/call".to_string(),
            params: MpcParams {
                name: "computer".to_string(),
                arguments: serde_json::json!({
                    "action": "screenshot_analyze",
                    "target": prompt
                }),
            },
            id: self.next_id(),
        };

        let response_text = self.send_request(&request).await?;
        
        // Parse response
        let result = ComputerToolResult::from_claude_response(&response_text)
            .ok_or_else(|| format!("Failed to parse computer tool response: {}", response_text))?;

        Ok(result)
    }

    /// Send JSON-RPC request via Chrome bridge (stdin/stdout) to Open Claude
    /// 
    /// The bridge at localhost:18765 handles Native Messaging protocol translation:
    /// Sirin (TCP) ←→ Chrome Bridge (stdin/stdout) ←→ Open Claude Extension
    async fn send_request(&self, request: &MpcRequest) -> Result<String, String> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        
        // Connect to Chrome bridge
        let mut stream = tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs),
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| format!("Timeout connecting to Chrome bridge at {}", addr))?
        .map_err(|e| format!("Chrome bridge unavailable ({}): ensure Open Claude extension is installed and Claude Code session is active", e))?;

        // Serialize request as JSON (Chrome bridge will add 4-byte length prefix)
        let request_json = serde_json::to_string(request)
            .map_err(|e| format!("Failed to serialize request: {}", e))?;
        
        // Send request to bridge (raw JSON, no newline — bridge handles framing)
        stream.write_all(request_json.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to bridge: {}", e))?;
        
        stream.flush()
            .await
            .map_err(|e| format!("Failed to flush socket: {}", e))?;

        tracing::info!("[open_claude_client] Sent request to bridge: method={}", request.method);

        // Read response from bridge (raw JSON, no framing since bridge is TCP client-facing)
        let mut buffer = vec![0u8; 16384];
        let n = tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs),
            stream.read(&mut buffer),
        )
        .await
        .map_err(|_| "Timeout waiting for Chrome response (Open Claude tool timed out)".to_string())?
        .map_err(|e| format!("Failed to read bridge response: {}", e))?;

        if n == 0 {
            return Err("Bridge closed connection unexpectedly".to_string());
        }

        let response_str = String::from_utf8_lossy(&buffer[..n]).to_string();
        tracing::debug!("[open_claude_client] Raw response: {}", response_str);
        
        // Parse JSON response
        let _response: MpcResponse = serde_json::from_str(&response_str)
            .map_err(|e| format!("Failed to parse response JSON: {}", e))?;

        // Extract text from response content
        if let Some(result) = _response.result {
            if let Some(content) = result.content.first() {
                if let Some(text) = &content.text {
                    tracing::info!("[open_claude_client] Parsed response text: {}", text);
                    return Ok(text.clone());
                }
            }
        }

        if let Some(error) = _response.error {
            return Err(format!("MPC error {}: {}", error.code, error.message));
        }

        Err("No response content from Open Claude".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_click_response() {
        let text = "I can see the button at coordinates (234, 567). Click it.";
        let result = ComputerToolResult::from_claude_response(text).unwrap();
        assert_eq!(result.x, 234);
        assert_eq!(result.y, 567);
        assert_eq!(result.action, "click");
    }

    #[test]
    fn test_parse_type_response() {
        let text = "Input field at (100, 200). Type 'hello world'.";
        let result = ComputerToolResult::from_claude_response(text).unwrap();
        assert_eq!(result.x, 100);
        assert_eq!(result.y, 200);
        assert_eq!(result.action, "type");
        assert_eq!(result.text, Some("hello world".to_string()));
    }

    #[test]
    fn test_parse_scroll_response() {
        let text = "Need to scroll down. Current view ends at (400, 600).";
        let result = ComputerToolResult::from_claude_response(text).unwrap();
        assert_eq!(result.action, "scroll");
    }
}
