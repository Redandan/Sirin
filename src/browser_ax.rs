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
//! - [`find_all_by_role_and_name`] — multi-match variant with limit
//! - [`ax_snapshot`] — capture + store a named tree snapshot
//! - [`ax_diff`] — diff two stored snapshots (added/removed/changed)
//! - [`wait_for_ax_change`] — poll until tree mutates from a baseline
//! - [`click_backend`] — click element via CDP `DOM.getBoxModel` + center point
//! - [`focus_backend`] — `DOM.focus` for input fields
//! - [`type_into_backend`] — focus + insertText (multi-char)
//! - [`enable_flutter_semantics`] — trigger Flutter's a11y bridge

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

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
    /// True if node passes all supplied filters (all are optional / additive):
    /// - `role` — exact role match
    /// - `name_substring` — case-insensitive substring of name
    /// - `name_regex` — full Rust regex applied to name (invalid pattern → no match)
    /// - `not_name_matches` — exclude node if name contains ANY of these strings (case-insensitive)
    pub fn matches(
        &self,
        role: Option<&str>,
        name_substring: Option<&str>,
        name_regex: Option<&str>,
        not_name_matches: &[String],
    ) -> bool {
        if let Some(r) = role {
            if self.role.as_deref().unwrap_or("") != r { return false; }
        }
        if let Some(needle) = name_substring {
            let hay = self.name.as_deref().unwrap_or("").to_lowercase();
            if !hay.contains(&needle.to_lowercase()) { return false; }
        }
        if let Some(pattern) = name_regex {
            let hay = self.name.as_deref().unwrap_or("");
            match regex::Regex::new(pattern) {
                Ok(re) => { if !re.is_match(hay) { return false; } }
                Err(_) => { return false; } // invalid regex → no match
            }
        }
        for excl in not_name_matches {
            let hay = self.name.as_deref().unwrap_or("").to_lowercase();
            if hay.contains(&excl.to_lowercase()) { return false; }
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
/// Also supports `name_regex` (Rust regex) and `not_name_matches` exclusion list.
pub fn find_by_role_and_name(
    role: Option<&str>,
    name_substring: Option<&str>,
    name_regex: Option<&str>,
    not_name_matches: &[String],
) -> Result<Option<AxNode>, String> {
    let tree = get_full_tree(false)?;
    Ok(tree.into_iter().find(|n| n.matches(role, name_substring, name_regex, not_name_matches)))
}

/// Find all nodes matching the supplied filters, up to `limit`.
pub fn find_all_by_role_and_name(
    role: Option<&str>,
    name_substring: Option<&str>,
    name_regex: Option<&str>,
    not_name_matches: &[String],
    limit: usize,
) -> Result<Vec<AxNode>, String> {
    let tree = get_full_tree(false)?;
    let results: Vec<_> = tree
        .into_iter()
        .filter(|n| n.matches(role, name_substring, name_regex, not_name_matches))
        .take(limit)
        .collect();
    Ok(results)
}

// ── Snapshot store (T-M07) ───────────────────────────────────────────────────

/// In-memory snapshot store. Key = snapshot_id (user-provided or auto-generated).
static SNAPSHOTS: OnceLock<Mutex<HashMap<String, Vec<AxNode>>>> = OnceLock::new();

fn snapshots() -> &'static Mutex<HashMap<String, Vec<AxNode>>> {
    SNAPSHOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take an ax tree snapshot and store it under the given ID.
/// If `id` is None, generates one from the current timestamp.
pub fn ax_snapshot(id: Option<&str>) -> Result<String, String> {
    let tree = get_full_tree(false)?;
    let snap_id = id.map(String::from).unwrap_or_else(|| {
        format!(
            "snap_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        )
    });
    snapshots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(snap_id.clone(), tree);
    Ok(snap_id)
}

/// Describes nodes whose name or value changed between two snapshots.
#[derive(Debug, Serialize)]
pub struct AxNodeChange {
    pub node_id: String,
    pub before_name:  Option<String>,
    pub after_name:   Option<String>,
    pub before_value: Option<String>,
    pub after_value:  Option<String>,
}

/// Result of comparing two ax tree snapshots.
#[derive(Debug, Serialize)]
pub struct AxDiff {
    /// Nodes present in `after` but not in `before`.
    pub added:   Vec<AxNode>,
    /// Nodes present in `before` but not in `after`.
    pub removed: Vec<AxNode>,
    /// Nodes present in both whose name or value differs.
    pub changed: Vec<AxNodeChange>,
}

/// Compare two stored snapshots and return a diff.
pub fn ax_diff(before_id: &str, after_id: &str) -> Result<AxDiff, String> {
    let store = snapshots().lock().unwrap_or_else(|e| e.into_inner());
    let before = store
        .get(before_id)
        .ok_or_else(|| format!("snapshot '{before_id}' not found"))?;
    let after = store
        .get(after_id)
        .ok_or_else(|| format!("snapshot '{after_id}' not found"))?;

    let before_map: HashMap<&str, &AxNode> = before
        .iter()
        .map(|n| (n.node_id.as_str(), n))
        .collect();
    let after_map: HashMap<&str, &AxNode> = after
        .iter()
        .map(|n| (n.node_id.as_str(), n))
        .collect();

    let added: Vec<AxNode> = after
        .iter()
        .filter(|n| !before_map.contains_key(n.node_id.as_str()))
        .cloned()
        .collect();
    let removed: Vec<AxNode> = before
        .iter()
        .filter(|n| !after_map.contains_key(n.node_id.as_str()))
        .cloned()
        .collect();
    let changed: Vec<AxNodeChange> = after
        .iter()
        .filter_map(|n_after| {
            let n_before = before_map.get(n_after.node_id.as_str())?;
            if n_before.name != n_after.name || n_before.value != n_after.value {
                Some(AxNodeChange {
                    node_id:      n_after.node_id.clone(),
                    before_name:  n_before.name.clone(),
                    after_name:   n_after.name.clone(),
                    before_value: n_before.value.clone(),
                    after_value:  n_after.value.clone(),
                })
            } else {
                None
            }
        })
        .collect();

    Ok(AxDiff { added, removed, changed })
}

/// Poll until the ax tree differs from the stored baseline, or timeout.
///
/// Returns `(new_snapshot_id, diff)` on first detected change.
/// Runs synchronously (polling with `thread::sleep`) — safe inside `spawn_blocking`.
pub fn wait_for_ax_change(baseline_id: &str, timeout_ms: u64) -> Result<(String, AxDiff), String> {
    let baseline: Vec<AxNode> = {
        let store = snapshots().lock().unwrap_or_else(|e| e.into_inner());
        store
            .get(baseline_id)
            .ok_or_else(|| format!("snapshot '{baseline_id}' not found"))?
            .clone()
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!("wait_for_ax_change: timeout after {timeout_ms}ms"));
        }
        let current = get_full_tree(false)?;

        // Quick change detection: different node count or any name/value diff.
        let changed = current.len() != baseline.len()
            || current.iter().any(|n| {
                baseline
                    .iter()
                    .find(|b| b.node_id == n.node_id)
                    .map_or(true, |b| b.name != n.name || b.value != n.value)
            });

        if changed {
            let new_id = ax_snapshot(None)?;
            let diff = ax_diff(baseline_id, &new_id)?;
            return Ok((new_id, diff));
        }

        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

// ── Read helpers ─────────────────────────────────────────────────────────────

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

/// Outcome of a verified type — what we tried to type vs what the input
/// actually shows after a settle delay.  Use this when you need to confirm
/// the keystroke landed (Flutter Canvas inputs sometimes drop characters,
/// formatted/masked inputs may transform the value).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeVerifyResult {
    pub backend_id: u32,
    pub typed: String,
    pub actual: String,
    /// True iff `actual.contains(typed)` (substring match — handles masking
    /// like `tel: '+1 555 ____'` where the input adds formatting).
    pub matched: bool,
}

/// Type into a backend node, then read back the value via the a11y tree to
/// verify the keystroke actually landed.  300ms settle delay between insert
/// and read-back to allow framework re-render (React controlled inputs,
/// Flutter rebuild).
///
/// Returns a [`TypeVerifyResult`] so the caller can branch on `matched`.
/// Doesn't throw on mismatch — callers decide whether substring is enough
/// or they need exact equality.
pub fn type_into_backend_verified(backend_id: u32, text: &str) -> Result<TypeVerifyResult, String> {
    type_into_backend(backend_id, text)?;
    // Settle so React/Flutter has time to re-render + a11y tree updates.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let actual = read_node_text(backend_id).unwrap_or_default();
    let matched = actual.contains(text);
    Ok(TypeVerifyResult {
        backend_id,
        typed: text.to_string(),
        actual,
        matched,
    })
}

// ── Flutter semantics bridge ─────────────────────────────────────────────────

/// Trigger Flutter Web's a11y semantics tree.
///
/// Without this, `Accessibility.getFullAXTree` on a Flutter app returns only
/// `<flt-glass-pane>` and the placeholder.  Flutter also periodically
/// **collapses** the semantics tree if it doesn't see AT activity —
/// this function is called every time `get_full_tree` notices ≤2 nodes.
///
/// ## Strategy (in priority order)
///
/// **A — "Enable accessibility" button** (Flutter 3.x+ explicit opt-in):
/// Some Flutter builds surface an [button "Enable accessibility"] in the
/// minimal AX tree.  Clicking it is the cleanest way to activate semantics
/// without keyboard side-effects.
///
/// **B — flt-semantics-placeholder JS click**:
/// The traditional trigger.  Still present in most Flutter Web builds.
///
/// **C — Tab×2 keyboard fallback** (last resort, ONLY if B also failed):
/// ⚠️ Tab key events can cause navigation side-effects on pages that have
/// active Flutter routing (e.g. after login → `#/home`).  Use only when
/// A and B both fail.
pub fn enable_flutter_semantics() -> Result<(), String> {
    // ── Strategy A: "Enable accessibility" button in the AX tree ────────
    // Fetch the raw (possibly collapsed) tree and look for the button by name.
    if let Ok(raw) = fetch_tree_raw() {
        if let Some(nodes) = raw.get("nodes").and_then(|v| v.as_array()) {
            for node in nodes {
                let name = node.get("name")
                    .and_then(|n| n.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                if name.contains("enable accessibility") {
                    if let Some(bid) = node.get("backendDOMNodeId").and_then(|v| v.as_u64()) {
                        tracing::info!("[browser_ax] clicking 'Enable accessibility' button (backend_id={bid})");
                        let _ = click_backend(bid as u32);
                        std::thread::sleep(std::time::Duration::from_millis(800));
                        return Ok(());
                    }
                }
            }
        }
    }

    // ── Strategy B: flt-semantics-placeholder JS click ───────────────────
    let clicked = crate::browser::evaluate_js(
        r#"(() => {
            const ph = document.querySelector('flt-semantics-placeholder');
            if (!ph) return "false";
            ph.click();
            return "true";
        })()"#
    ).unwrap_or_default();

    if clicked == "true" {
        std::thread::sleep(std::time::Duration::from_millis(800));
        return Ok(());
    }

    // ── Strategy C: Tab×2 — last resort only ────────────────────────────
    // Only reached when neither A nor B found a trigger element.
    // Tab key events are safe only if the page has no active routing that
    // intercepts keyboard focus (plain HTML / login pages before navigation).
    tracing::warn!("[browser_ax] flt-semantics-placeholder not found; using Tab×2 fallback");
    let _ = crate::browser::press_key("Tab");
    std::thread::sleep(std::time::Duration::from_millis(120));
    let _ = crate::browser::press_key("Tab");
    std::thread::sleep(std::time::Duration::from_millis(120));
    std::thread::sleep(std::time::Duration::from_millis(800));

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
        assert!(n.matches(Some("button"), None, None, &[]));
        assert!(!n.matches(Some("textbox"), None, None, &[]));
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
        assert!(n.matches(None, Some("balance"), None, &[]));
        assert!(n.matches(None, Some("$7376.80"), None, &[]));
        assert!(n.matches(Some("text"), Some("WALLET"), None, &[]));
        assert!(!n.matches(None, Some("staking"), None, &[]));
    }

    #[test]
    fn ax_node_matches_no_filter_accepts_all() {
        let n = AxNode {
            node_id: "1".into(),
            backend_id: None,
            role: None, name: None,
            value: None, description: None, child_ids: vec![],
        };
        assert!(n.matches(None, None, None, &[]));
    }

    // ── T-M06 new filter tests ────────────────────────────────────────────────

    #[test]
    fn ax_node_matches_with_not_name() {
        let n = AxNode {
            node_id: "2".into(),
            backend_id: None,
            role: Some("textbox".into()),
            name: Some("Enter password".into()),
            value: None, description: None, child_ids: vec![],
        };
        // Should be excluded when "password" is in the not_name_matches list.
        let excl = vec!["password".to_string()];
        assert!(!n.matches(None, None, None, &excl));
        // Other exclusion terms that don't match should not exclude it.
        let excl2 = vec!["username".to_string()];
        assert!(n.matches(None, None, None, &excl2));
    }

    #[test]
    fn ax_node_matches_with_regex() {
        let n = AxNode {
            node_id: "3".into(),
            backend_id: None,
            role: Some("button".into()),
            name: Some("Confirm Order #1234".into()),
            value: None, description: None, child_ids: vec![],
        };
        // Regex that matches the full name.
        assert!(n.matches(None, None, Some(r"Confirm.*"), &[]));
        // Regex that does NOT match.
        assert!(!n.matches(None, None, Some(r"Cancel.*"), &[]));
        // Invalid regex → no match.
        assert!(!n.matches(None, None, Some(r"[invalid"), &[]));
    }

    // ── T-M07 diff tests ──────────────────────────────────────────────────────

    #[test]
    fn ax_diff_detects_added_removed() {
        let node_a = AxNode {
            node_id: "a".into(), backend_id: None,
            role: Some("button".into()), name: Some("A".into()),
            value: None, description: None, child_ids: vec![],
        };
        let node_b = AxNode {
            node_id: "b".into(), backend_id: None,
            role: Some("button".into()), name: Some("B".into()),
            value: None, description: None, child_ids: vec![],
        };
        let node_b_changed = AxNode {
            node_id: "b".into(), backend_id: None,
            role: Some("button".into()), name: Some("B-updated".into()),
            value: None, description: None, child_ids: vec![],
        };
        let node_c = AxNode {
            node_id: "c".into(), backend_id: None,
            role: Some("text".into()), name: Some("C".into()),
            value: None, description: None, child_ids: vec![],
        };

        // Manually insert snapshots into the store.
        {
            let mut store = snapshots().lock().unwrap_or_else(|e| e.into_inner());
            store.insert("test_before".to_string(), vec![node_a.clone(), node_b.clone()]);
            store.insert("test_after".to_string(),  vec![node_b_changed.clone(), node_c.clone()]);
        }

        let diff = ax_diff("test_before", "test_after").unwrap();
        // node_c added; node_a removed; node_b changed name.
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].node_id, "c");
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].node_id, "a");
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].node_id, "b");
        assert_eq!(diff.changed[0].before_name.as_deref(), Some("B"));
        assert_eq!(diff.changed[0].after_name.as_deref(), Some("B-updated"));
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
