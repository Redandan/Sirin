//! `GET /gateway` &mdash; HTML gateway page for the MCP endpoint.
//!
//! Purpose: give Claude in Chrome (Beta) a same-origin page it can `navigate`
//! to and then drive with `javascript_tool` (or plain `read_page` + form
//! `click`) to call any MCP tool without hitting CORS.
//!
//! The HTML is fully self-contained &mdash; no external CDN, no build step.
//! On load it `fetch('/mcp', tools/list)` and renders one `<details>` card
//! per tool with a form generated from `inputSchema.properties`.  Tools
//! whose name contains a write/side-effect keyword (write/delete/create/
//! spawn/send/approve/reset/kill) are listed but their form is suppressed.
//!
//! See: <https://github.com/Redandan/Sirin/issues/90>

use axum::response::Html;

/// Embedded gateway page.  Served verbatim &mdash; all dynamic behaviour lives
/// in the inline JS which calls `POST /mcp tools/list` at load time.
const GATEWAY_HTML: &str = include_str!("mcp_gateway.html");

/// Axum handler for `GET /gateway`.  Returns the embedded HTML with
/// `Content-Type: text/html; charset=utf-8` (set by `Html<_>`'s
/// `IntoResponse` impl).
pub async fn gateway_handler() -> Html<&'static str> {
    Html(GATEWAY_HTML)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{response::IntoResponse, http::StatusCode};

    #[tokio::test]
    async fn gateway_returns_html_with_marker_and_endpoint_meta() {
        let resp = gateway_handler().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/html"),
            "expected text/html content-type, got {ct:?}"
        );

        let body_bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("read body");
        let body = std::str::from_utf8(&body_bytes).expect("utf8 body");

        // Tier-1 marker: page identifies itself.
        assert!(
            body.contains("Sirin MCP Gateway"),
            "body missing title marker"
        );
        // Future-proof discovery hint &mdash; CiC / scrapers can locate the
        // JSON-RPC endpoint without hard-coding `/mcp`.
        assert!(
            body.contains(r#"name="sirin-mcp-endpoint""#)
                && body.contains(r#"content="/mcp""#),
            "body missing sirin-mcp-endpoint meta tag"
        );
        // Tier-2 marker: the dynamic-tools script is wired up.
        assert!(
            body.contains("tools/list"),
            "body missing tools/list bootstrapping JS"
        );
    }

    #[test]
    fn gateway_html_is_nonempty() {
        // include_str! resolution sanity check &mdash; if the asset path ever
        // moves, the build fails; this also guards against an empty file
        // sneaking in via a bad merge.
        assert!(GATEWAY_HTML.len() > 500, "embedded HTML suspiciously short");
        assert!(GATEWAY_HTML.contains("<!DOCTYPE html>"));
    }
}
