//! Cheap JS-eval probe for current page URL, title, and canvas presence.
//!
//! Returns a best-effort result — any failure is swallowed and reported as
//! "unknown" rather than raising, so the perception layer can gracefully
//! fall back to legacy text observations.
//!
//! Canvas detection rules (any true → canvas_detected = true):
//!   - `window.flutter` truthy (Flutter web bootstrap sets this)
//!   - `document.querySelector('flt-glass-pane')` exists (Flutter CanvasKit)
//!   - `document.querySelector('canvas')` exists AND occupies >= 50% of the
//!     viewport area (generic canvas / WebGL app signal; avoids false-positive
//!     tiny chart canvases on classic DOM pages)

use serde_json::json;

#[derive(Debug, Clone, Default)]
pub struct PageProbe {
    pub url: String,
    pub title: String,
    pub canvas_detected: bool,
}

/// Single JS expression that returns a stringified JSON object so we don't
/// round-trip through the `eval` action multiple times.
const PROBE_SCRIPT: &str = r#"
JSON.stringify((() => {
    try {
        const hasFlutter = !!(self.flutter);
        const hasGlass = !!document.querySelector('flt-glass-pane');
        let hasBigCanvas = false;
        const cvs = document.querySelectorAll('canvas');
        if (cvs.length > 0) {
            const vw = Math.max(1, window.innerWidth || 1);
            const vh = Math.max(1, window.innerHeight || 1);
            const vArea = vw * vh;
            for (const c of cvs) {
                const r = c.getBoundingClientRect();
                if (r.width * r.height >= vArea * 0.5) { hasBigCanvas = true; break; }
            }
        }
        return {
            url: location.href,
            title: document.title || '',
            canvas: hasFlutter || hasGlass || hasBigCanvas
        };
    } catch (e) {
        return { url: location.href || '', title: '', canvas: false, error: String(e) };
    }
})())
"#;

pub async fn probe_page(ctx: &crate::adk::context::AgentContext) -> PageProbe {
    let eval_input = json!({ "action": "eval", "target": PROBE_SCRIPT });
    let raw = match ctx.call_tool("web_navigate", eval_input).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("[perception::probe_page] eval failed: {e}");
            return PageProbe::default();
        }
    };

    // `web_navigate::eval` wraps the return value; try a few common shapes
    // before giving up.  The script returns a JSON **string** (we used
    // JSON.stringify), so we need to parse it back out.
    let raw_str = extract_eval_value(&raw).unwrap_or_default();
    if raw_str.is_empty() {
        return PageProbe::default();
    }

    match serde_json::from_str::<serde_json::Value>(&raw_str) {
        Ok(v) => PageProbe {
            url: v.get("url").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            title: v.get("title").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            canvas_detected: v.get("canvas").and_then(|b| b.as_bool()).unwrap_or(false),
        },
        Err(e) => {
            tracing::debug!("[perception::probe_page] parse probe JSON failed: {e}; raw={raw_str}");
            PageProbe::default()
        }
    }
}

/// Pull the string result out of whatever shape `web_navigate::eval` returns.
/// Tolerates: direct string, {"result": "..."}, {"value": "..."}, {"text": "..."}.
fn extract_eval_value(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    for key in ["result", "value", "text", "output"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_from_plain_string() {
        let v = serde_json::json!("{\"url\":\"x\"}");
        assert_eq!(extract_eval_value(&v).as_deref(), Some("{\"url\":\"x\"}"));
    }

    #[test]
    fn extract_from_result_key() {
        let v = serde_json::json!({ "result": "payload" });
        assert_eq!(extract_eval_value(&v).as_deref(), Some("payload"));
    }

    #[test]
    fn extract_fallback_none() {
        let v = serde_json::json!({ "unrelated": 42 });
        assert!(extract_eval_value(&v).is_none());
    }
}
