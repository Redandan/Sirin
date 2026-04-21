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
    /// Expected format in response text: "click at (123, 456)" or "type 'hello' at input"
    pub fn from_claude_response(text: &str) -> Option<Self> {
        // Simple regex-free parsing for robustness
        // Look for patterns like "(123, 456)" or "click" or "type"
        
        let mut x = None;
        let mut y = None;
        let mut action = "click".to_string();
        let mut text_to_type = None;

        // Extract action word
        if text.contains("type") {
            action = "type".to_string();
        } else if text.contains("scroll") {
            action = "scroll".to_string();
        } else if text.contains("click") {
            action = "click".to_string();
        }

        // Extract coordinates (look for pattern like "(123, 456)")
        for word in text.split(|c: char| !c.is_numeric() && c != ',' && c != '(' && c != ')' && c != ' ') {
            if let Ok(num) = word.parse::<u32>() {
                if x.is_none() {
                    x = Some(num);
                } else if y.is_none() {
                    y = Some(num);
                    break;
                }
            }
        }

        // Extract text for type action (between quotes)
        if let Some(start) = text.find('\'') {
            if let Some(end) = text[start+1..].find('\'') {
                text_to_type = Some(text[start+1..start+1+end].to_string());
            }
        }

        // If we have coordinates, return result
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

    /// Send JSON-RPC request to MCP server and get response
    async fn send_request(&self, request: &MpcRequest) -> Result<String, String> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        
        let mut stream = tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs),
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| format!("Timeout connecting to Open Claude at {}", addr))?
        .map_err(|e| format!("Failed to connect to Open Claude: {}", e))?;

        // Send request
        let request_json = serde_json::to_string(request)
            .map_err(|e| format!("Failed to serialize request: {}", e))?;
        
        stream.write_all(request_json.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to socket: {}", e))?;
        
        stream.write_all(b"\n")
            .await
            .map_err(|e| format!("Failed to write newline: {}", e))?;

        // Read response (with timeout)
        let mut buffer = vec![0u8; 8192];
        let n = tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs),
            stream.read(&mut buffer),
        )
        .await
        .map_err(|_| "Timeout reading Open Claude response".to_string())?
        .map_err(|e| format!("Failed to read from socket: {}", e))?;

        let response_str = String::from_utf8_lossy(&buffer[..n]).to_string();
        
        // Parse JSON response
        let _response: MpcResponse = serde_json::from_str(&response_str)
            .map_err(|e| format!("Failed to parse response JSON: {}", e))?;

        // Extract text from response content
        if let Some(result) = _response.result {
            if let Some(content) = result.content.first() {
                if let Some(text) = &content.text {
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
