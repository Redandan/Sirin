//! Screenshot capture helpers for the perception layer.
//!
//! Wraps `crate::browser::screenshot()` (which is blocking) in a
//! `spawn_blocking` so callers can `await` it from the async executor
//! loop, and base64-encodes the result for embedding in LLM prompts.

/// Capture the current viewport as a base64-encoded PNG.
///
/// Uses the same underlying CDP screenshot path as the rest of Sirin — no
/// extension, no bridge, no vendor dependency.
pub async fn screenshot_b64() -> Result<String, String> {
    let png = tokio::task::spawn_blocking(crate::browser::screenshot)
        .await
        .map_err(|e| format!("spawn_blocking: {e}"))??;
    Ok(base64_encode(&png))
}

/// Minimal base64 encoder (RFC 4648, standard alphabet, padded).
/// Mirrors the helper used in `crate::llm::mod.rs` so we don't introduce a
/// new dependency for this small amount of bytes.
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn encodes_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn encodes_known_values() {
        // RFC 4648 §10 test vectors
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
