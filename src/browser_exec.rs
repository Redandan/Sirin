//! Single source of truth for all browser action dispatch.
//!
//! Called from both `mcp_server.rs` (the external MCP/RPC API) and the
//! `web_navigate` builtin in `adk/tool/builtins.rs` (the test-runner ReAct
//! loop).  Any action added here is automatically available in both callers
//! with zero duplication — which was the root cause of the drift that
//! prompted Issue #115.
//!
//! # Design notes
//!
//! * All parameters are read from `input: &serde_json::Value`.  Callers may
//!   add extra keys (e.g. `test_run_id`, `blocked_url_patterns`) that this
//!   module simply ignores — so caller-specific behaviour stays in the caller.
//!
//! * Actions that are **caller-specific** are NOT here:
//!   - `screenshot_analyze` — builtins has vision cache + SoM; mcp_server has
//!     a simpler sync path.  Both callers handle it before reaching this fn.
//!   - `ocr_find_text` — async OCR helper; handled in caller.
//!   - `dom_snapshot` — used only by builtins (test runner ref-id system).
//!   - `ext_status / ext_url / ext_tabs` — MCP-only extension probes.
//!
//! * `session_id` / `headless` pre-processing (goto + session_switch) happens
//!   in the caller before calling this fn, so this fn assumes the session is
//!   already correct.

use serde_json::{json, Value};

// ── Inline helpers ────────────────────────────────────────────────────────────

fn str_field<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(Value::as_str).unwrap_or("")
}

fn opt_str(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(String::from)
}

fn opt_str2<'a>(input: &'a Value, k1: &str, k2: &str) -> Option<&'a str> {
    input.get(k1).or_else(|| input.get(k2)).and_then(Value::as_str)
}

fn u64_field(input: &Value, key: &str, default: u64) -> u64 {
    input.get(key).and_then(Value::as_u64).unwrap_or(default)
}

/// Parse a backend_id that may be a JSON number OR a JSON string.
/// LLMs (especially DeepSeek) sometimes output `"backend_id":"94"` (string)
/// instead of `"backend_id":94` (number).  Both are accepted here.
fn parse_backend_id(input: &Value) -> Option<u32> {
    input.get("backend_id").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
    }).map(|n| n as u32)
}

fn f64_field(input: &Value, key: &str, default: f64) -> f64 {
    input.get(key).and_then(Value::as_f64).unwrap_or(default)
}

fn bool_field(input: &Value, key: &str, default: bool) -> bool {
    input.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// Try to resolve `ref_id` first (dom_snapshot stable ref), then fall back
/// to plain `target`.  Mirrors `resolve_target` in builtins.rs.
fn resolve_selector(input: &Value, target: &str, action: &str) -> Result<String, String> {
    if let Some(ref_id) = opt_str(input, "ref_id") {
        return crate::browser::resolve_ref(&ref_id)
            .map_err(|e| format!("ref_id lookup failed for '{action}': {e}"));
    }
    if target.is_empty() {
        return Err(format!("'{action}' requires 'target' selector or 'ref_id'"));
    }
    Ok(target.to_string())
}

// ── Public dispatch entry point ───────────────────────────────────────────────

/// Dispatch a browser action.
///
/// `input` is the full JSON object from the caller.  All parameters are read
/// from it by key name.  Unknown keys are silently ignored.
///
/// Returns `Err(String)` for both validation errors and browser errors.
///
/// # Caller responsibility
///
/// Before calling this function the caller MUST:
/// 1. Handle `screenshot_analyze`, `ocr_find_text`, `dom_snapshot`,
///    `ext_status/url/tabs` themselves (these are not in this dispatch).
/// 2. For `goto` with `session_id`: call `browser::ensure_open(headless)` then
///    `browser::session_switch(sid)` before dispatching.
pub(crate) fn dispatch(action: &str, input: &Value) -> Result<Value, String> {
    use crate::browser;

    let target  = str_field(input, "target");
    let text    = str_field(input, "text");

    match action {
        // ── Navigation ───────────────────────────────────────────────────
        "goto" => {
            if target.is_empty() { return Err("'goto' requires 'target' URL".into()); }
            // authz check is caller-responsibility (builtins does it pre-dispatch)
            let headless = input.get("browser_headless")
                .and_then(Value::as_bool)
                .unwrap_or_else(browser::default_headless);
            browser::ensure_open(headless)?;
            browser::navigate(target)?;
            Ok(json!({ "status": "navigated", "url": target }))
        }
        "screenshot" => {
            let png = browser::screenshot()?;
            let b64 = crate::llm::base64_encode_bytes(&png);
            let url = browser::current_url().unwrap_or_default();
            Ok(json!({
                "mime":         "image/png",
                "bytes_base64": b64,
                "size_bytes":   png.len(),
                "url":          url,
            }))
        }
        "title" => Ok(json!({ "title": browser::page_title()? })),
        "url"   => Ok(json!({ "url":   browser::current_url()? })),
        "close" => { browser::close(); Ok(json!({ "status": "closed" })) }

        // ── DOM interaction ──────────────────────────────────────────────
        "click" => {
            let sel = resolve_selector(input, target, "click")?;
            browser::click(&sel)?;
            Ok(json!({ "status": "clicked", "selector": sel }))
        }
        "type" => {
            let sel = resolve_selector(input, target, "type")?;
            browser::type_text(&sel, text)?;
            Ok(json!({ "status": "typed", "selector": sel, "length": text.len() }))
        }
        "read" => {
            let sel = resolve_selector(input, target, "read")?;
            Ok(json!({ "selector": sel, "text": browser::get_text(&sel)? }))
        }
        "eval" => {
            if target.is_empty() { return Err("'eval' requires 'target' JS expression".into()); }
            Ok(json!({ "result": browser::evaluate_js(target)? }))
        }
        "go_back" => {
            let wait_ms = u64_field(input, "wait", 0);
            browser::go_back(wait_ms)?;
            let url = browser::current_url().unwrap_or_default();
            Ok(json!({ "status": "went_back", "url": url }))
        }
        "wait" => {
            // Accept ms as: numeric target ("target":2000 or "target":"2000"),
            // "ms" field, or plain "ms" key.  LLMs sometimes send the number
            // directly as JSON integer rather than a quoted string.
            let ms_opt = input.get("target")
                .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
                .or_else(|| input.get("ms").and_then(Value::as_u64));
            if let Some(ms) = ms_opt {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                return Ok(json!({ "status": "slept", "ms": ms }));
            }
            // Selector wait (e.g. {"action":"wait","target":"#login-btn"})
            if target.is_empty() { return Err("'wait' requires 'target' (ms number or CSS selector)".into()); }
            let timeout_ms = input.get("timeout").and_then(Value::as_u64).unwrap_or(5000);
            browser::wait_for_ms(target, timeout_ms)?;
            Ok(json!({ "status": "found", "selector": target }))
        }
        "exists" => {
            if let Some(ref_id) = opt_str(input, "ref_id") {
                match browser::resolve_ref(&ref_id) {
                    Ok(sel) => Ok(json!({
                        "ref_id": ref_id, "selector": sel,
                        "exists": browser::element_exists(&sel)?
                    })),
                    Err(_) => Ok(json!({ "ref_id": ref_id, "exists": false })),
                }
            } else if !target.is_empty() {
                Ok(json!({ "selector": target, "exists": browser::element_exists(target)? }))
            } else {
                Err("'exists' requires 'target' selector or 'ref_id'".into())
            }
        }
        "count" => {
            if target.is_empty() { return Err("'count' requires 'target' selector".into()); }
            Ok(json!({ "selector": target, "count": browser::element_count(target)? }))
        }
        "attr" => {
            if target.is_empty() { return Err("'attr' requires 'target' selector".into()); }
            if text.is_empty()   { return Err("'attr' requires 'text' = attribute name".into()); }
            Ok(json!({ "selector": target, "attribute": text,
                        "value": browser::get_attribute(target, text)? }))
        }
        "value" => {
            if target.is_empty() { return Err("'value' requires 'target' selector".into()); }
            Ok(json!({ "selector": target, "value": browser::get_value(target)? }))
        }

        // ── Keyboard / input ─────────────────────────────────────────────
        "key" => {
            if target.is_empty() { return Err("'key' requires 'target' key name".into()); }
            browser::press_key(target)?;
            Ok(json!({ "status": "pressed", "key": target }))
        }
        "select" => {
            if target.is_empty() { return Err("'select' requires 'target' selector".into()); }
            if text.is_empty()   { return Err("'select' requires 'text' = option value".into()); }
            browser::select_option(target, text)?;
            Ok(json!({ "status": "selected", "selector": target, "value": text }))
        }
        "scroll" => {
            let x = f64_field(input, "x", 0.0);
            let y = f64_field(input, "y", 300.0);
            // flutter=true → use touch-drag gesture (works inside Flutter canvas)
            // flutter=false (default) → window.scrollBy (works for normal HTML pages)
            let flutter_mode = input.get("flutter")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if flutter_mode {
                browser::flutter_scroll(y)?;
            } else {
                browser::scroll_by(x, y)?;
            }
            Ok(json!({ "status": "scrolled", "x": x, "y": y, "flutter": flutter_mode }))
        }
        "flutter_scroll" => {
            // Dedicated Flutter canvas touch-scroll action.
            // delta_y > 0 = scroll down (page moves up), < 0 = scroll up.
            let delta_y = input.get("y").or_else(|| input.get("delta_y"))
                .and_then(Value::as_f64)
                .unwrap_or(400.0);
            browser::flutter_scroll(delta_y)?;
            Ok(json!({ "status": "scrolled", "delta_y": delta_y, "method": "touch-drag" }))
        }
        "flutter_scroll_until_visible" => {
            // Scroll Flutter canvas until a shadow DOM element appears in viewport.
            // Preferred over flutter_scroll y=<fixed> because it adapts to
            // different viewports and page layouts automatically.
            let role_s   = opt_str(input, "role");
            let role     = role_s.as_deref();
            let name_str = opt_str2(input, "name_regex", "name");
            let step = f64_field(input, "step", 300.0);
            let max  = f64_field(input, "max_scroll", 2000.0);
            let (x, y, label) = browser::flutter_scroll_until_visible(role, name_str, step, max)?;
            Ok(json!({ "found": true, "x": x, "y": y, "label": label }))
        }
        "scroll_to" => {
            if target.is_empty() { return Err("'scroll_to' requires 'target' selector".into()); }
            browser::scroll_into_view(target)?;
            Ok(json!({ "status": "scrolled_to", "selector": target }))
        }

        // ── Coordinate interaction ───────────────────────────────────────
        "click_point" => {
            let x = input.get("x").and_then(Value::as_f64)
                .ok_or("'click_point' requires 'x' (number)")?;
            let y = input.get("y").and_then(Value::as_f64)
                .ok_or("'click_point' requires 'y' (number)")?;
            // Issue #79: HiDPI screenshot coords need devicePixelRatio rescaling.
            let source = input.get("coord_source").and_then(Value::as_str).unwrap_or("css");
            match source {
                "screenshot" => browser::click_point_screenshot(x, y)?,
                _            => browser::click_point(x, y)?,
            }
            Ok(json!({ "status": "clicked", "x": x, "y": y, "coord_source": source }))
        }
        "hover" => {
            let sel = resolve_selector(input, target, "hover")?;
            browser::hover(&sel)?;
            Ok(json!({ "status": "hovered", "selector": sel }))
        }
        "hover_point" => {
            let x = input.get("x").and_then(Value::as_f64)
                .ok_or("'hover_point' requires 'x'")?;
            let y = input.get("y").and_then(Value::as_f64)
                .ok_or("'hover_point' requires 'y'")?;
            let source = input.get("coord_source").and_then(Value::as_str).unwrap_or("css");
            match source {
                "screenshot" => browser::hover_point_screenshot(x, y)?,
                _            => browser::hover_point(x, y)?,
            }
            Ok(json!({ "status": "hovered", "x": x, "y": y, "coord_source": source }))
        }

        // ── Tabs ─────────────────────────────────────────────────────────
        "new_tab" => {
            let idx = browser::new_tab()?;
            if !target.is_empty() { browser::navigate(target)?; }
            Ok(json!({ "tab_index": idx }))
        }
        "switch_tab" => {
            let idx = input.get("index").and_then(Value::as_u64)
                .ok_or("'switch_tab' requires 'index'")? as usize;
            browser::switch_tab(idx)?;
            Ok(json!({ "status": "switched", "tab_index": idx }))
        }
        "close_tab" => {
            let idx = input.get("index").and_then(Value::as_u64)
                .ok_or("'close_tab' requires 'index'")? as usize;
            browser::close_tab(idx)?;
            Ok(json!({ "status": "tab_closed", "index": idx }))
        }
        "list_tabs" => {
            let tabs = browser::list_tabs()?;
            let active = browser::active_tab()?;
            let arr: Vec<Value> = tabs.into_iter()
                .map(|(i, u)| json!({"index": i, "url": u, "active": i == active}))
                .collect();
            Ok(json!({ "tabs": arr }))
        }

        // ── Cookies ──────────────────────────────────────────────────────
        "cookies" => {
            let raw = browser::get_cookies()?;
            let val: Value = serde_json::from_str(&raw).unwrap_or(json!([]));
            Ok(json!({ "cookies": val }))
        }
        "set_cookie" => {
            let name   = str_field(input, "name");
            let value  = str_field(input, "value");
            let domain = str_field(input, "domain");
            let path   = input.get("path").and_then(Value::as_str).unwrap_or("/");
            browser::set_cookie(name, value, domain, path)?;
            Ok(json!({ "status": "cookie_set", "name": name }))
        }
        "delete_cookie" => {
            if target.is_empty() { return Err("'delete_cookie' requires 'target' cookie name".into()); }
            browser::delete_cookie(target)?;
            Ok(json!({ "status": "cookie_deleted", "name": target }))
        }

        // ── Storage ──────────────────────────────────────────────────────
        "localStorage_get" => {
            if target.is_empty() { return Err("requires 'target' key".into()); }
            Ok(json!({ "key": target, "value": browser::local_storage_get(target)? }))
        }
        "localStorage_set" => {
            if target.is_empty() { return Err("requires 'target' key".into()); }
            browser::local_storage_set(target, text)?;
            Ok(json!({ "status": "set", "key": target }))
        }

        // ── Network / Console ────────────────────────────────────────────
        "network" => {
            let limit = u64_field(input, "limit", 20) as usize;
            let raw = browser::captured_requests(limit)?;
            let val: Value = serde_json::from_str(&raw).unwrap_or(json!([]));
            Ok(json!({ "requests": val }))
        }
        "console" => {
            let limit = u64_field(input, "limit", 20) as usize;
            let raw = browser::console_messages(limit)?;
            let val: Value = serde_json::from_str(&raw).unwrap_or(json!([]));
            Ok(json!({ "messages": val }))
        }
        "install_capture" => {
            browser::install_console_capture()?;
            browser::install_network_capture()?;
            Ok(json!({ "status": "console+network capture installed" }))
        }

        // ── Advanced ─────────────────────────────────────────────────────
        "viewport" | "set_viewport" => {
            let w     = u64_field(input, "width",  1280) as u32;
            let h     = u64_field(input, "height",  800) as u32;
            let scale = f64_field(input, "scale",   1.0);
            let mobile = bool_field(input, "mobile", false);
            browser::set_viewport(w, h, scale, mobile)?;
            Ok(json!({ "status": "viewport_set", "width": w, "height": h }))
        }
        "pdf" => {
            let bytes = browser::pdf()?;
            Ok(json!({ "status": "pdf_exported", "bytes": bytes.len() }))
        }
        "drag" => {
            let fx = input.get("from_x").and_then(Value::as_f64).ok_or("requires 'from_x'")?;
            let fy = input.get("from_y").and_then(Value::as_f64).ok_or("requires 'from_y'")?;
            let tx = input.get("to_x").and_then(Value::as_f64).ok_or("requires 'to_x'")?;
            let ty = input.get("to_y").and_then(Value::as_f64).ok_or("requires 'to_y'")?;
            browser::drag(fx, fy, tx, ty)?;
            Ok(json!({ "status": "dragged" }))
        }
        "http_auth" => {
            let user = str_field(input, "username");
            let pass = str_field(input, "password");
            browser::set_http_auth(user, pass)?;
            Ok(json!({ "status": "auth_set" }))
        }

        // ── Accessibility tree ───────────────────────────────────────────
        "enable_a11y" => {
            // Always call enable_flutter_semantics() to trigger the placeholder
            // click that builds flt-semantics-host DOM elements.
            // Safety net: detect about:blank URL reset and restore.
            let saved_url = crate::browser::current_url().ok();
            let _ = crate::browser_ax::enable_flutter_semantics();
            // Poll until flt-semantics-host is non-empty (Flutter fills it async).
            let mut shadow_ready = false;
            for _ in 0..15 {
                let count = browser::evaluate_js(
                    "document.querySelector('flt-semantics-host')?.childElementCount||0"
                ).unwrap_or_default();
                if count.trim() != "0" {
                    shadow_ready = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            if let Some(ref url) = saved_url {
                let cur = crate::browser::current_url().unwrap_or_default();
                if cur.contains("about:blank") && !url.contains("about:blank") && !url.is_empty() {
                    tracing::warn!(
                        "[browser_exec] enable_a11y: URL reset detected (about:blank) — restoring {url:?}"
                    );
                    let _ = crate::browser::navigate(url);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
            let tree = crate::browser_ax::get_full_tree(false).unwrap_or_default();
            Ok(json!({ "status": "semantics enabled", "ax_node_count": tree.len(), "shadow_ready": shadow_ready }))
        }
        "ax_tree" => {
            let include_ignored = bool_field(input, "include_ignored", false);
            let nodes = crate::browser_ax::get_full_tree(include_ignored)?;
            Ok(json!({ "count": nodes.len(), "nodes": nodes }))
        }
        "ax_find" => {
            let role = opt_str(input, "role");
            let name = opt_str(input, "name");
            if role.is_none() && name.is_none() {
                return Err("'ax_find' requires 'role' and/or 'name'".into());
            }
            let name_regex = opt_str(input, "name_regex");
            let not_name_matches: Vec<String> = input
                .get("not_name_matches")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let limit  = u64_field(input, "limit", 1) as usize;
            let scroll = bool_field(input, "scroll", false);

            if scroll {
                let scroll_max = u64_field(input, "scroll_max", 10) as usize;
                let (node, scrolled) = crate::browser_ax::find_scrolling_by_role_and_name(
                    role.as_deref(), name.as_deref(),
                    name_regex.as_deref(), &not_name_matches,
                    scroll_max, 400.0,
                )?;
                return Ok(json!({
                    "found": node.is_some(),
                    "node": node,
                    "scrolled_times": scrolled,
                }));
            }

            if limit <= 1 {
                match crate::browser_ax::find_by_role_and_name(
                    role.as_deref(), name.as_deref(),
                    name_regex.as_deref(), &not_name_matches,
                )? {
                    Some(n) => Ok(json!({ "found": true,  "node": n    })),
                    None    => Ok(json!({ "found": false, "node": null })),
                }
            } else {
                let nodes = crate::browser_ax::find_all_by_role_and_name(
                    role.as_deref(), name.as_deref(),
                    name_regex.as_deref(), &not_name_matches, limit,
                )?;
                Ok(json!({
                    "found": !nodes.is_empty(),
                    "count": nodes.len(),
                    "nodes": nodes,
                }))
            }
        }
        "ax_snapshot" => {
            let snap_id = opt_str(input, "id");
            let id = crate::browser_ax::ax_snapshot(snap_id.as_deref())?;
            Ok(json!({ "snapshot_id": id }))
        }
        "ax_diff" => {
            let before = input["before_id"].as_str().ok_or("'ax_diff' requires 'before_id'")?;
            let after  = input["after_id"].as_str().ok_or("'ax_diff' requires 'after_id'")?;
            let diff = crate::browser_ax::ax_diff(before, after)?;
            Ok(json!({
                "added_count":   diff.added.len(),
                "removed_count": diff.removed.len(),
                "changed_count": diff.changed.len(),
                "added":   diff.added.iter().map(|n| json!({"node_id": n.node_id, "role": n.role, "name": n.name})).collect::<Vec<_>>(),
                "removed": diff.removed.iter().map(|n| json!({"node_id": n.node_id, "role": n.role, "name": n.name})).collect::<Vec<_>>(),
                "changed": diff.changed,
            }))
        }
        "wait_for_ax_change" => {
            let baseline_id = input["baseline_id"].as_str()
                .ok_or("'wait_for_ax_change' requires 'baseline_id'")?;
            let to_ms = input.get("timeout").and_then(Value::as_u64).unwrap_or(5000);
            let (new_id, diff) = crate::browser_ax::wait_for_ax_change(baseline_id, to_ms)?;
            Ok(json!({
                "new_snapshot_id": new_id,
                "added_count":   diff.added.len(),
                "removed_count": diff.removed.len(),
                "changed_count": diff.changed.len(),
            }))
        }
        "ax_value" => {
            let id = parse_backend_id(input)
                .ok_or("'ax_value' requires 'backend_id' (number)")?;
            Ok(json!({ "backend_id": id, "text": crate::browser_ax::read_node_text(id)? }))
        }
        "ax_click" => {
            let id = parse_backend_id(input)
                .ok_or("'ax_click' requires 'backend_id' (number)")?;
            crate::browser_ax::click_backend(id)?;
            Ok(json!({ "status": "clicked", "backend_id": id }))
        }
        "ax_focus" => {
            let id = parse_backend_id(input)
                .ok_or("'ax_focus' requires 'backend_id' (number)")?;
            crate::browser_ax::focus_backend(id)?;
            Ok(json!({ "status": "focused", "backend_id": id }))
        }
        "ax_type" => {
            let id = parse_backend_id(input)
                .ok_or("'ax_type' requires 'backend_id' (number)")?;
            crate::browser_ax::type_into_backend(id, text)?;
            Ok(json!({ "status": "typed", "backend_id": id, "length": text.len() }))
        }
        "ax_type_verified" => {
            let id = parse_backend_id(input)
                .ok_or("'ax_type_verified' requires 'backend_id' (number)")?;
            let r = crate::browser_ax::type_into_backend_verified(id, text)?;
            Ok(serde_json::to_value(&r).unwrap_or(json!({})))
        }

        // ── Test isolation ───────────────────────────────────────────────
        "clear_state" => {
            browser::clear_browser_state()?;
            Ok(json!({ "status": "cleared" }))
        }

        // ── Flutter Shadow DOM ───────────────────────────────────────────
        "shadow_find" => {
            let role = opt_str(input, "role");
            let name = opt_str2(input, "name_regex", "name").map(String::from);
            let (x, y, label) = browser::shadow_find(role.as_deref(), name.as_deref())?;
            Ok(json!({ "found": true, "x": x, "y": y, "label": label }))
        }
        "shadow_click" => {
            let role = opt_str(input, "role");
            let name = opt_str2(input, "name_regex", "name").map(String::from);
            let label = browser::shadow_click(role.as_deref(), name.as_deref())?;
            Ok(json!({ "status": "clicked", "label": label }))
        }
        "shadow_type" => {
            let role = opt_str(input, "role");
            let name = opt_str2(input, "name_regex", "name").map(String::from);
            let text_val = input.get("text").and_then(Value::as_str)
                .ok_or("'shadow_type' requires 'text'")?;
            browser::shadow_type(role.as_deref(), name.as_deref(), text_val)?;
            Ok(json!({ "status": "typed", "text": text_val }))
        }
        "flutter_type" => {
            // Accept both string "50" and number 50 (sirin-call key=value parses ints as JSON numbers).
            let text_owned = input.get("text")
                .map(|v| if let Some(s) = v.as_str() { s.to_string() } else { v.to_string().trim_matches('"').to_string() })
                .ok_or("'flutter_type' requires 'text'")?;
            browser::flutter_type(&text_owned)?;
            Ok(json!({ "status": "typed", "text": text_owned }))
        }
        "flutter_enter" => {
            let result = browser::flutter_enter()?;
            Ok(json!({ "status": "ok", "result": result }))
        }
        "shadow_type_flutter" => {
            let role = opt_str(input, "role");
            let name = opt_str2(input, "name_regex", "name").map(String::from);
            let text_owned = input.get("text")
                .map(|v| if let Some(s) = v.as_str() { s.to_string() } else { v.to_string().trim_matches('"').to_string() })
                .ok_or("'shadow_type_flutter' requires 'text'")?;
            let label = browser::shadow_click(role.as_deref(), name.as_deref())?;
            std::thread::sleep(std::time::Duration::from_millis(350));
            browser::flutter_type(&text_owned)?;
            Ok(json!({ "status": "typed", "label": label, "text": text_owned }))
        }
        "shadow_dump" => {
            let items = browser::shadow_dump()?;
            Ok(json!({ "count": items.len(), "elements": items }))
        }

        // ── Multi-tab / popup ────────────────────────────────────────────
        "wait_new_tab" => {
            let to_ms = input.get("timeout").and_then(Value::as_u64).unwrap_or(10000);
            let idx = browser::wait_for_new_tab(None, to_ms)?;
            Ok(json!({ "status": "new tab opened", "active_tab": idx }))
        }

        // ── Network conditions ───────────────────────────────────────────
        "wait_request" => {
            if target.is_empty() {
                return Err("'wait_request' requires 'target' = URL substring".into());
            }
            let to_ms = input.get("timeout").and_then(Value::as_u64).unwrap_or(10000);
            let raw = browser::wait_for_request(target, to_ms)?;
            let val: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
            Ok(json!({ "request": val }))
        }

        // ── Condition-based waits ────────────────────────────────────────
        "wait_for_url" => {
            if target.is_empty() {
                return Err("'wait_for_url' requires 'target' (URL substring or /regex/)".into());
            }
            let to_ms = input.get("timeout_ms")
                .or_else(|| input.get("timeout"))
                .and_then(Value::as_u64)
                .unwrap_or(10000);
            let elapsed = browser::wait_for_url(target, to_ms)?;
            let url = browser::current_url().unwrap_or_default();
            Ok(json!({ "status": "ready", "elapsed_ms": elapsed, "url": url }))
        }
        "wait_for_ax_ready" => {
            let min_nodes = input.get("min_nodes").and_then(Value::as_u64).unwrap_or(20) as usize;
            let to_ms = input.get("timeout_ms")
                .or_else(|| input.get("timeout"))
                .and_then(Value::as_u64)
                .unwrap_or(10000);
            let (elapsed, count) = crate::browser_ax::wait_for_ax_ready(min_nodes, to_ms)?;
            Ok(json!({ "status": "ready", "elapsed_ms": elapsed, "ax_node_count": count }))
        }
        "wait_for_network_idle" => {
            let idle_ms = input.get("idle_ms").and_then(Value::as_u64).unwrap_or(500);
            let to_ms = input.get("timeout_ms")
                .or_else(|| input.get("timeout"))
                .and_then(Value::as_u64)
                .unwrap_or(15000);
            let elapsed = browser::wait_for_network_idle(idle_ms, to_ms)?;
            Ok(json!({ "status": "idle", "elapsed_ms": elapsed }))
        }

        // ── Assertions ───────────────────────────────────────────────────
        "assert_ax_contains" => {
            if target.is_empty() {
                return Err("'assert_ax_contains' requires 'target' = text to find".into());
            }
            let tree = crate::browser_ax::get_full_tree(false)?;
            let needle = target.to_lowercase();
            let found = tree.iter().any(|n| {
                n.name.as_deref().unwrap_or("").to_lowercase().contains(&needle)
                    || n.value.as_deref().unwrap_or("").to_lowercase().contains(&needle)
            });
            let preview: Vec<String> = tree.iter().take(20)
                .filter_map(|n| n.name.clone().or_else(|| n.value.clone()))
                .collect();
            Ok(json!({
                "passed":                 found,
                "target":                 target,
                "actual_ax_tree_preview": preview.join(" | "),
            }))
        }
        "assert_url_matches" => {
            if target.is_empty() {
                return Err("'assert_url_matches' requires 'target' (URL substring or /regex/)".into());
            }
            let url = browser::current_url().unwrap_or_default();
            let is_regex = target.starts_with('/') && target.ends_with('/') && target.len() > 2;
            let passed = if is_regex {
                let pattern = &target[1..target.len() - 1];
                regex::Regex::new(pattern).map(|re| re.is_match(&url)).unwrap_or(false)
            } else {
                url.contains(target)
            };
            Ok(json!({ "passed": passed, "target": target, "actual_url": url }))
        }

        // ── Named sessions ───────────────────────────────────────────────
        "list_sessions" => {
            let sessions = browser::list_sessions().unwrap_or_default();
            let items: Vec<Value> = sessions.into_iter().map(|(id, idx, url)| {
                json!({ "session_id": id, "tab_index": idx, "url": url })
            }).collect();
            Ok(json!({ "count": items.len(), "sessions": items }))
        }
        "close_session" => {
            if target.is_empty() {
                return Err("'close_session' requires 'target' = session_id".into());
            }
            browser::close_session(target)?;
            Ok(json!({ "status": "closed", "session_id": target }))
        }

        other => Err(format!("Unknown browser action: {other}")),
    }
}
