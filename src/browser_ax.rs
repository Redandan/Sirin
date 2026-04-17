//! CDP Accessibility tree wrapper.
//!
//! ## Why
//! Vision-based screenshot analysis describes UI ("balance shows about
//! 7377 USDT"); accessibility-tree extraction returns the **literal string**
//! ("$7376.80"). Required for K14/K15-style tests that compare exact numbers,
//! error messages, or token counts.
//!
//! Works on plain HTML, React/Vue SPAs, and Flutter Web (after triggering
//! the semantics bridge — see [`enable_flutter_semantics`]).
//!
//! ## API surface
//! - [`enable`] — `Accessibility.enable` once per page lifecycle
//! - [`get_full_tree`] — full a11y tree as Vec<[`AxNode`]> with literal text
//! - [`find_by_role_and_name`] — common case: locate element by role + text
//! - [`click_backend`] — click element via CDP `DOM.getBoxModel` + center point
//! - [`focus_backend`] — `DOM.focus` for input fields
//! - [`type_into_backend`] — focus + insertText (multi-char)
//! - [`enable_flutter_semantics`] — trigger Flutter's a11y bridge

use headless_chrome::protocol::cdp::{Accessibility, DOM, Input};
use serde::{Deserialize, Serialize};

use crate::browser::with_tab;

// ── Custom raw CDP method (workaround for headless_chrome 1.0.21 strict enums) ─
//
// `Accessibility.getFullAXTree` in current Chrome returns properties like
// `uninteresting` that the crate's `AXPropertyName` enum doesn't include.
// Strict deserialization fails the entire response.  We bypass that by calling
// the method with a custom struct that returns raw `serde_json::Value` and
// pulling only the fields we care about.

#[derive(Debug, serde::Serialize)]
struct RawGetFullAxTree {}

impl headless_chrome::protocol::cdp::types::Method for RawGetFullAxTree {
    const NAME: &'static str = "Accessibility.getFullAXTree";
    type ReturnObject = serde_json::Value;
}

// ── Public types ─────────────────────────────────────────────────────────────

/// Slim, serialisable view of a CDP `AXNode` — only the fields agents care
/// about.  Drops `ignored` nodes by default (filter at [`get_full_tree`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxNode {
    /// CDP a11y node id (string).
    pub node_id: String,
    /// DOM backend node id — pass to [`click_backend`] / [`focus_backend`].
    pub backend_id: Option<u32>,
    /// e.g. "button", "textbox", "text", "heading", "link".
    pub role: Option<String>,
    /// Accessible name (visible label / aria-label / inner text).
    pub name: Option<String>,
    /// Current value (input value, slider position, etc).
    pub value: Option<String>,
    /// Long description / tooltip.
    pub description: Option<String>,
    /// Child a11y node ids — caller can build the tree if needed.
    pub child_ids: Vec<String>,
}

impl AxNode {
    /// True if `role` matches and `name` contains the substring (case-insensitive).
    pub fn matches(&self, role: Option<&str>, name_substring: Option<&str>) -> bool {
        if let Some(r) = role {
            if self.role.as_deref().unwrap_or("") != r { return false; }
        }
        if let Some(needle) = name_substring {
            let hay = self.name.as_deref().unwrap_or("").to_lowercase();
            if !hay.contains(&needle.to_lowercase()) { return false; }
        }
        true
    }
}

// ── Enable / fetch ───────────────────────────────────────────────────────────

/// Enable the Accessibility domain.  Idempotent — safe to call before every
/// `get_full_tree`.  Some Chrome versions auto-enable, but explicit call is
/// the documented contract.
pub fn enable() -> Result<(), String> {
    with_tab(|tab| {
        tab.call_method(Accessibility::Enable(None))
            .map_err(|e| format!("Accessibility.enable: {e}"))?;
        Ok(())
    })
}

/// Get the full a11y tree.  `include_ignored=false` filters out
/// `ignored` and `generic` / `none` role nodes which are usually noise
/// (containers without semantic meaning).
///
/// Uses a raw JSON return type to tolerate AXPropertyName values the
/// crate's strict enum doesn't yet know about (e.g. `uninteresting`).
///
/// **Flutter trap**: Flutter Web's semantics tree **collapses to a single
/// RootWebArea** if it doesn't see ongoing AT activity.  We probe the first
/// result, and if it's just the root, re-trigger semantics and retry once.
pub fn get_full_tree(include_ignored: bool) -> Result<Vec<AxNode>, String> {
    enable()?;
    let mut tree = fetch_tree_raw()?;

    // Flutter collapse detection: if only 1-2 nodes and the root has empty
    // name, the semantics bridge has gone to sleep.  Wake it and retry.
    if tree.get("nodes").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0) <= 2 {
        let _ = enable_flutter_semantics();
        std::thread::sleep(std::time::Duration::from_millis(300));
        tree = fetch_tree_raw()?;
    }

    let raw = tree;

    let nodes = raw.get("nodes").and_then(|v| v.as_array())
        .ok_or_else(|| format!("getFullAXTree: missing nodes array; got {raw}"))?;

    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes {
        let role = json_ax_value(n, "role");
        let ignored = n.get("ignored").and_then(|v| v.as_bool()).unwrap_or(false);
        if !include_ignored {
            if ignored { continue; }
            if matches!(role.as_deref(), Some("none") | Some("generic")) {
                continue;
            }
        }
        let backend_id = n.get("backendDOMNodeId").and_then(|v| v.as_u64()).map(|n| n as u32);
        let child_ids = n.get("childIds").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let node_id = n.get("nodeId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        out.push(AxNode {
            node_id,
            backend_id,
            role,
            name:        json_ax_value(n, "name"),
            value:       json_ax_value(n, "value"),
            description: json_ax_value(n, "description"),
            child_ids,
        });
    }
    Ok(out)
}

fn fetch_tree_raw() -> Result<serde_json::Value, String> {
    with_tab(|tab| {
        tab.call_method(RawGetFullAxTree {})
            .map_err(|e| format!("Accessibility.getFullAXTree: {e}"))
    })
}

/// Pull `node[field].value` from a raw AXNode JSON.  CDP wraps text values as
/// `{type: "string"|"number"|..., value: <Json>}`.
fn json_ax_value(node: &serde_json::Value, field: &str) -> Option<String> {
    let outer = node.get(field)?;
    let inner = outer.get("value")?;
    match inner {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b)   => Some(b.to_string()),
        serde_json::Value::Null      => None,
        other                         => Some(other.to_string()),
    }
}

/// Convenience: find the first node matching role + name (substring, case-insensitive).
pub fn find_by_role_and_name(role: Option<&str>, name_substring: Option<&str>) -> Result<Option<AxNode>, String> {
    let tree = get_full_tree(false)?;
    Ok(tree.into_iter().find(|n| n.matches(role, name_substring)))
}

/// Read the literal `name` or `value` text of a node (used for assertions).
/// Returns `value` if present, else `name`.  Empty string if neither.
pub fn read_node_text(backend_id: u32) -> Result<String, String> {
    let tree = get_full_tree(true)?;  // include ignored — text nodes sometimes are
    let node = tree.into_iter()
        .find(|n| n.backend_id == Some(backend_id))
        .ok_or_else(|| format!("ax node with backend_id={backend_id} not found"))?;
    Ok(node.value.or(node.name).unwrap_or_default())
}

// ── Interaction by backend_id ────────────────────────────────────────────────

/// Click an element given its DOM backend node id.  Resolves via `DOM.getBoxModel`
/// to the element's centre and dispatches a synthetic mouse event there.  More
/// reliable than CSS selectors on Flutter Canvas / shadow DOM.
pub fn click_backend(backend_id: u32) -> Result<(), String> {
    let (x, y) = center_of_backend(backend_id)?;
    with_tab(|tab| {
        // Mouse pressed
        tab.call_method(Input::DispatchMouseEvent {
            Type: Input::DispatchMouseEventTypeOption::MousePressed,
            x, y,
            button: Some(Input::MouseButton::Left),
            buttons: Some(1),
            click_count: Some(1),
            modifiers: None, timestamp: None,
            delta_x: None, delta_y: None, pointer_Type: None,
            force: None, tangential_pressure: None,
            tilt_x: None, tilt_y: None, twist: None,
        }).map_err(|e| format!("ax_click pressed: {e}"))?;
        // Mouse released
        tab.call_method(Input::DispatchMouseEvent {
            Type: Input::DispatchMouseEventTypeOption::MouseReleased,
            x, y,
            button: Some(Input::MouseButton::Left),
            buttons: Some(0),
            click_count: Some(1),
            modifiers: None, timestamp: None,
            delta_x: None, delta_y: None, pointer_Type: None,
            force: None, tangential_pressure: None,
            tilt_x: None, tilt_y: None, twist: None,
        }).map_err(|e| format!("ax_click released: {e}"))?;
        Ok(())
    })
}

/// Focus an element by backend id.
pub fn focus_backend(backend_id: u32) -> Result<(), String> {
    with_tab(|tab| {
        tab.call_method(DOM::Focus {
            node_id: None,
            backend_node_id: Some(backend_id),
            object_id: None,
        }).map_err(|e| format!("DOM.focus({backend_id}): {e}"))?;
        Ok(())
    })
}

/// Focus an input + insert text.  Use this instead of [`crate::browser::type_text`]
/// when you have an a11y backend_id but no CSS selector (Flutter, shadow DOM).
pub fn type_into_backend(backend_id: u32, text: &str) -> Result<(), String> {
    focus_backend(backend_id)?;
    with_tab(|tab| {
        tab.call_method(Input::InsertText { text: text.to_string() })
            .map_err(|e| format!("Input.insertText: {e}"))?;
        Ok(())
    })
}

// ── Flutter semantics bridge ─────────────────────────────────────────────────

/// Trigger Flutter Web's a11y semantics tree.
///
/// Without this, `Accessibility.getFullAXTree` on a Flutter app returns only
/// `<flt-glass-pane>` and the placeholder.  Worse, Flutter periodically
/// **collapses** the semantics tree if it doesn't see ongoing AT activity —
/// so this function is called every time `get_full_tree` notices a tree of
/// ≤2 nodes (likely collapsed state).
///
/// Strategy: click the canonical placeholder + Tab × 2 + Shift+Tab to make
/// Flutter believe a screen reader is exploring the page.  Then 800ms settle.
pub fn enable_flutter_semantics() -> Result<(), String> {
    // Method 1: click the placeholder if present.
    let clicked = crate::browser::evaluate_js(
        r#"(() => {
            const ph = document.querySelector('flt-semantics-placeholder');
            if (!ph) return false;
            ph.click();
            return true;
        })()"#
    ).unwrap_or_default();

    // Method 2: synthetic Tab/Shift-Tab keys — Flutter watches for these as
    // an "AT exploring page" signal.  Multiple presses helps — single Tab is
    // sometimes ignored.
    let _ = crate::browser::press_key("Tab");
    std::thread::sleep(std::time::Duration::from_millis(120));
    let _ = crate::browser::press_key("Tab");
    std::thread::sleep(std::time::Duration::from_millis(120));

    // Settle so Flutter has time to rebuild semantics.
    std::thread::sleep(std::time::Duration::from_millis(800));

    if clicked == "false" {
        tracing::info!("[browser_ax] flt-semantics-placeholder not found (Tab fallback used)");
    }
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Extract a string from CDP's nested AXValue { type, value: Json }.  Returns
/// `None` if absent or not stringifiable.
fn ax_value_to_string(v: &Option<Accessibility::AXValue>) -> Option<String> {
    let val = v.as_ref()?;
    match &val.value {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(serde_json::Value::Bool(b)) => Some(b.to_string()),
        Some(other) => Some(other.to_string()),
        None => None,
    }
}

/// Compute the centre point of an element's content box via CDP.
fn center_of_backend(backend_id: u32) -> Result<(f64, f64), String> {
    let model = with_tab(|tab| {
        tab.call_method(DOM::GetBoxModel {
            node_id: None,
            backend_node_id: Some(backend_id),
            object_id: None,
        })
        .map_err(|e| format!("DOM.getBoxModel({backend_id}): {e}"))
    })?;

    // content quad = [x1,y1, x2,y2, x3,y3, x4,y4]
    let q = &model.model.content;
    if q.len() < 8 {
        return Err(format!("invalid quad length: {}", q.len()));
    }
    let cx = (q[0] + q[2] + q[4] + q[6]) / 4.0;
    let cy = (q[1] + q[3] + q[5] + q[7]) / 4.0;
    Ok((cx, cy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ax_node_matches_by_role() {
        let n = AxNode {
            node_id: "1".into(),
            backend_id: Some(42),
            role: Some("button".into()),
            name: Some("Submit".into()),
            value: None, description: None, child_ids: vec![],
        };
        assert!(n.matches(Some("button"), None));
        assert!(!n.matches(Some("textbox"), None));
    }

    #[test]
    fn ax_node_matches_by_name_substring_case_insensitive() {
        let n = AxNode {
            node_id: "1".into(),
            backend_id: None,
            role: Some("text".into()),
            name: Some("Wallet Balance: $7376.80".into()),
            value: None, description: None, child_ids: vec![],
        };
        assert!(n.matches(None, Some("balance")));
        assert!(n.matches(None, Some("$7376.80")));
        assert!(n.matches(Some("text"), Some("WALLET")));
        assert!(!n.matches(None, Some("staking")));
    }

    #[test]
    fn ax_node_matches_no_filter_accepts_all() {
        let n = AxNode {
            node_id: "1".into(),
            backend_id: None,
            role: None, name: None,
            value: None, description: None, child_ids: vec![],
        };
        assert!(n.matches(None, None));
    }

    #[test]
    fn ax_value_to_string_extracts_string() {
        // Build an AXValue with a String JSON value
        let v = Some(Accessibility::AXValue {
            Type: Accessibility::AXValueType::String,
            value: Some(serde_json::Value::String("$7376.80".into())),
            related_nodes: None,
            sources: None,
        });
        assert_eq!(ax_value_to_string(&v).as_deref(), Some("$7376.80"));
    }

    #[test]
    fn ax_value_to_string_handles_number() {
        let v = Some(Accessibility::AXValue {
            Type: Accessibility::AXValueType::Number,
            value: Some(serde_json::json!(99.30)),
            related_nodes: None,
            sources: None,
        });
        assert_eq!(ax_value_to_string(&v).as_deref(), Some("99.3"));
    }

    #[test]
    fn ax_value_to_string_returns_none_for_empty() {
        assert_eq!(ax_value_to_string(&None), None);
    }
}
