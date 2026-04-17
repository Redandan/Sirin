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
/// **Flutter trap — two distinct ≤2-node situations**:
///
/// 1. **Cold start**: semantics never activated → need bootstrap (A/B).
/// 2. **Post-navigation teardown** (Issue #20): Flutter SPA rebuilt the
///    Semantics tree after an `ax_click` route change; tree is transiently 1
///    node and will recover on its own within ~1s.  Bootstrapping here fires
///    Tab×2 which resets the URL to about:blank.
///
/// Fix: before bootstrapping, poll 3× × 400ms to let Flutter self-recover
/// (covers case 2).  If tree stays tiny after recovery window, bootstrap
/// A/B only — Tab×2 (Strategy C) has been permanently removed.
pub fn get_full_tree(include_ignored: bool) -> Result<Vec<AxNode>, String> {
    enable()?;
    let mut tree = fetch_tree_raw()?;

    if raw_node_count(&tree) <= 2 {
        // Wait for potential post-navigation transient teardown to resolve.
        // Flutter rebuilds the Semantics tree after SPA route changes; the
        // window is usually 300-800ms.  3 × 400ms = 1.2s covers it.
        if !poll_tree_recovery(3, 400) {
            // Still tiny — attempt bootstrap (A/B; Tab×2 removed, see Issue #20).
            let _ = enable_flutter_semantics();
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
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

/// Count nodes in a raw `getFullAXTree` response.
fn raw_node_count(raw: &serde_json::Value) -> usize {
    raw.get("nodes").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0)
}

/// Poll the raw AX tree up to `attempts` times, sleeping `interval_ms`
/// between each attempt.  Returns `true` as soon as the tree grows beyond
/// 2 nodes (indicating Flutter Semantics has recovered from teardown).
fn poll_tree_recovery(attempts: u32, interval_ms: u64) -> bool {
    for _ in 0..attempts {
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        if let Ok(raw) = fetch_tree_raw() {
            if raw_node_count(&raw) > 2 {
                return true;
            }
        }
    }
    false
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

/// Poll until the AX tree contains at least `min_nodes` non-ignored nodes.
/// Returns `(elapsed_ms, actual_count)` on success.
///
/// Use after Flutter navigation to wait for the semantics tree to populate:
/// ```json
/// {"action":"wait_for_ax_ready","min_nodes":20,"timeout":10000}
/// ```
pub fn wait_for_ax_ready(min_nodes: usize, timeout_ms: u64) -> Result<(u64, usize), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let t0 = std::time::Instant::now();
    loop {
        match get_full_tree(false) {
            Ok(tree) if tree.len() >= min_nodes => {
                return Ok((t0.elapsed().as_millis() as u64, tree.len()));
            }
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            let count = get_full_tree(false).map(|t| t.len()).unwrap_or(0);
            return Err(format!(
                "wait_for_ax_ready: timeout after {timeout_ms}ms (got {count} nodes, need {min_nodes})"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
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

/// Scroll-aware find: search the AX tree, scrolling down by `scroll_step_px`
/// up to `scroll_max` times if the node is not yet visible.
///
/// Flutter ListView / infinite scroll pages only expose on-screen items in
/// the AX tree.  This helper scrolls incrementally to reveal them.
///
/// Returns `(node, scrolled_times)`.  `scrolled_times = 0` means found
/// immediately without scrolling.
pub fn find_scrolling_by_role_and_name(
    role: Option<&str>,
    name_substring: Option<&str>,
    name_regex: Option<&str>,
    not_name_matches: &[String],
    scroll_max: usize,
    scroll_step_px: f64,
) -> Result<(Option<AxNode>, usize), String> {
    for scroll_count in 0..=scroll_max {
        let tree = get_full_tree(false)?;
        if let Some(node) = tree
            .into_iter()
            .find(|n| n.matches(role, name_substring, name_regex, not_name_matches))
        {
            return Ok((Some(node), scroll_count));
        }
        if scroll_count < scroll_max {
            crate::browser::scroll_by(0.0, scroll_step_px)?;
            // Wait for Flutter ListView to load newly-visible items into the tree
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }
    Ok((None, scroll_max))
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
/// called by `get_full_tree` after the teardown-recovery poll window.
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
/// **C — REMOVED** (Issue #20):
/// Tab×2 was the original last resort, but it causes Flutter's active router
/// to intercept the Tab key event and reset the page URL to about:blank on
/// any page with active routing (post-ax_click navigation).  If A and B both
/// fail, we warn and return Ok(()) — the caller surfaces a small tree and the
/// agent can use `wait_for_ax_ready` to wait for tree recovery.
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

    // ── Strategy C: Tab×2 — PERMANENTLY REMOVED (Issue #20) ────────────
    //
    // Tab×2 sent keyboard events that Flutter's active router intercepted,
    // resetting the URL to about:blank on any page visited after an ax_click
    // navigation (post-click teardown: tree = 1 node, placeholder detached,
    // both A and B fail → Tab×2 fires into the new route → about:blank).
    //
    // Removing it means: if A and B both fail, we return Ok(()) and the tree
    // stays collapsed.  The caller (get_full_tree) will surface a small tree,
    // which the agent handles via wait_for_ax_ready + retry rather than
    // silently corrupting browser state.
    tracing::warn!(
        "[browser_ax] enable_flutter_semantics: A and B both failed \
         (no 'Enable accessibility' button, flt-semantics-placeholder absent/detached). \
         Tab×2 fallback disabled (Issue #20). \
         Use wait_for_ax_ready to wait for tree recovery."
    );
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
