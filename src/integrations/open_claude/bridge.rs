//! Native Messaging Host bridge — bidirectional communication with Chrome Open Claude extension
//!
//! Architecture:
//! ┌─────────────────────┐
//! │ Sirin executor      │
//! │ (port 18765 TCP)    │
//! └──────────┬──────────┘
//!            │ JSON-RPC 2.0 request
//!            ↓
//! ┌─────────────────────────────────────────┐
//! │ sirin_chrome_bridge (this module)       │
//! │ - TCP Server :18765 (listen)            │
//! │ - stdin/stdout (Chrome Native Messaging)│
//! └──────────┬──────────────────────────────┘
//!            │ message (4-byte length + JSON)
//!            ↓
//! ┌─────────────────────┐
//! │ Chrome extension    │
//! │ Open Claude         │
//! │ (computer tool)     │
//! └─────────────────────┘

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use serde_json::{json, Value};
use std::io::{BufReader, Read, Write};
use tracing::{info, warn, error};

/// Chrome Native Messaging protocol wrapper
pub struct ChromeNativeMessaging;

impl ChromeNativeMessaging {
    /// Encode message for Chrome (4-byte little-endian length + JSON)
    pub fn encode_message(msg: &Value) -> Vec<u8> {
        let json_str = msg.to_string();
        let len = json_str.len() as u32;
        let mut encoded = len.to_le_bytes().to_vec();
        encoded.extend_from_slice(json_str.as_bytes());
        encoded
    }

    /// Decode message from Chrome (read 4-byte length, then payload)
    pub fn decode_message(
        reader: &mut BufReader<std::io::Stdin>,
    ) -> Result<Value, String> {
        let mut len_bytes = [0u8; 4];
        reader
            .read_exact(&mut len_bytes)
            .map_err(|e| format!("Failed to read message length: {}", e))?;

        let msg_len = u32::from_le_bytes(len_bytes) as usize;
        if msg_len == 0 || msg_len > 10_000_000 {
            return Err("Invalid message length".into());
        }

        let mut buf = vec![0u8; msg_len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("Failed to read message payload: {}", e))?;

        let json_str = String::from_utf8(buf)
            .map_err(|e| format!("Invalid UTF-8 in message: {}", e))?;

        serde_json::from_str(&json_str)
            .map_err(|e| format!("Failed to parse JSON: {}", e))
    }
}

/// Start the Chrome bridge server
/// Listens on 127.0.0.1:18765 for TCP connections from Sirin executor
pub async fn start_bridge() {
    let listener = match TcpListener::bind("127.0.0.1:18765").await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind Chrome bridge: {}", e);
            return;
        }
    };
    info!("Chrome bridge listening on 127.0.0.1:18765");

    loop {
        match listener.accept().await {
            Ok((socket, _)) => {
                tokio::spawn(handle_client(socket));
            }
            Err(e) => {
                warn!("Chrome bridge accept error: {}", e);
            }
        }
    }
}

/// Handle a single TCP client (Sirin executor)
async fn handle_client(mut socket: TcpStream) {
    let peer = socket.peer_addr().ok();
    info!("Bridge: New client connected from {:?}", peer);

    // Prepare stdin/stdout for Chrome communication
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut stdin_reader = BufReader::new(stdin);

    loop {
        // 1. Read TCP request from Sirin executor
        let mut buf = [0u8; 16384];
        match socket.read(&mut buf).await {
            Ok(0) => {
                info!("Bridge: Client disconnected");
                break;
            }
            Ok(n) => {
                let request = match serde_json::from_slice::<Value>(&buf[..n]) {
                    Ok(req) => req,
                    Err(e) => {
                        warn!("Bridge: Failed to parse TCP request: {}", e);
                        continue;
                    }
                };

                info!("Bridge: Request from Sirin: {}", request.get("method").and_then(|m| m.as_str()).unwrap_or("unknown"));

                // 2. Send to Chrome via Native Messaging (length-prefixed)
                let encoded = ChromeNativeMessaging::encode_message(&request);
                if let Err(e) = stdout.write_all(&encoded) {
                    error!("Bridge: Failed to send to Chrome: {}", e);
                    break;
                }
                if let Err(e) = stdout.flush() {
                    error!("Bridge: Failed to flush stdout: {}", e);
                    break;
                }

                info!("Bridge: Sent request to Chrome, waiting for response...");

                // 3. Read response from Chrome via stdin
                match ChromeNativeMessaging::decode_message(&mut stdin_reader) {
                    Ok(response) => {
                        info!("Bridge: Received response from Chrome");

                        // 4. Send response back to TCP socket (raw JSON)
                        let response_json = response.to_string();
                        if let Err(e) = socket.write_all(response_json.as_bytes()).await {
                            error!("Bridge: Failed to send response to Sirin: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Bridge: Failed to read Chrome response: {}", e);

                        // Send error back to Sirin
                        let error_response = json!({
                            "error": {
                                "code": -32603,
                                "message": format!("Chrome communication failed: {}", e)
                            }
                        });

                        let error_json = error_response.to_string();
                        if let Err(e) = socket.write_all(error_json.as_bytes()).await {
                            error!("Bridge: Failed to send error to Sirin: {}", e);
                        }
                        break;
                    }
                }
            }
            Err(e) => {
                warn!("Bridge: Socket read error: {}", e);
                break;
            }
        }
    }

    info!("Bridge: Client handler shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "computer",
                "arguments": { "action": "screenshot" }
            }
        });

        let encoded = ChromeNativeMessaging::encode_message(&msg);

        // Verify length prefix
        let len = u32::from_le_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
        assert_eq!(len, encoded.len() - 4);

        // Verify JSON is intact
        let json_part = String::from_utf8(encoded[4..].to_vec()).unwrap();
        let decoded: Value = serde_json::from_str(&json_part).unwrap();
        assert_eq!(decoded, msg);
    }
}
