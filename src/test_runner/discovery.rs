//! Auto-discovery crawler — Issue #247.
//!
//! Sirin opens the running app, walks clickable widgets / routes via CDP,
//! and writes what it finds to SQLite. The Coverage panel diffs the
//! discovered set against `config/coverage/agora_market.yaml` to surface:
//!
//!   • Discovered → real number for the funnel's tier 1
//!   • Discovery Gaps — features Sirin saw but YAML doesn't list
//!
//! Iteration 1 (this commit) ships the data layer:
//!   • Types: DiscoveredFeature, DiscoveryRun
//!   • SQLite table schemas (in store.rs)
//!   • Storage helpers: insert / list / latest run / clear
//!
//! Iteration 2 wires the actual browser crawl using BrowserService.

use rusqlite::params;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredFeature {
    pub route:     String,
    pub label:     String,
    /// "button" | "link" | "form_input" | "page" | "tab" | "menuitem"
    pub kind:      String,
    pub selector:  Option<String>,
    pub last_seen: String,
    pub run_id:    Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryRun {
    pub run_id:        String,
    pub started_at:    String,
    pub finished_at:   Option<String>,
    /// "running" | "done" | "failed"
    pub status:        String,
    pub total_widgets: Option<u32>,
    pub error:         Option<String>,
    pub seed_url:      Option<String>,
    pub max_depth:     Option<u32>,
}

// ── DB access ─────────────────────────────────────────────────────────────────

fn db() -> &'static std::sync::Mutex<rusqlite::Connection> {
    // Reuse the test_runner::store DB connection (shared schema location).
    crate::test_runner::store::__shared_db()
}

/// Begin a new discovery run. Returns the row id.
pub fn begin_run(run_id: &str, seed_url: &str, max_depth: u32) -> Result<(), String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO discovery_runs (run_id, started_at, status, seed_url, max_depth) \
         VALUES (?1, ?2, 'running', ?3, ?4)",
        params![run_id, now_rfc3339(), seed_url, max_depth as i64],
    ).map_err(|e| format!("insert discovery_run: {e}"))?;
    Ok(())
}

/// Mark a run finished — status either "done" or "failed".
pub fn finish_run(
    run_id:        &str,
    status:        &str,
    total_widgets: Option<u32>,
    error:         Option<&str>,
) -> Result<(), String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE discovery_runs \
         SET finished_at = ?1, status = ?2, total_widgets = ?3, error = ?4 \
         WHERE run_id = ?5",
        params![now_rfc3339(), status, total_widgets.map(|n| n as i64), error, run_id],
    ).map_err(|e| format!("update discovery_run: {e}"))?;
    Ok(())
}

/// Insert (or update last_seen on) a discovered feature.
/// Dedups on (route, label, kind).
pub fn record_feature(feat: &DiscoveredFeature) -> Result<(), String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO discovered_features (route, label, kind, selector, last_seen, run_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(route, label, kind) DO UPDATE SET \
            last_seen = excluded.last_seen, \
            run_id    = excluded.run_id, \
            selector  = COALESCE(excluded.selector, selector)",
        params![
            feat.route, feat.label, feat.kind,
            feat.selector, feat.last_seen, feat.run_id,
        ],
    ).map_err(|e| format!("upsert feature: {e}"))?;
    Ok(())
}

/// List all discovered features (newest last_seen first).
pub fn list_features() -> Result<Vec<DiscoveredFeature>, String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    let mut stmt = conn.prepare(
        "SELECT route, label, kind, selector, last_seen, run_id \
         FROM discovered_features \
         ORDER BY last_seen DESC",
    ).map_err(|e| format!("prepare list_features: {e}"))?;
    let rows = stmt.query_map([], |r| {
        Ok(DiscoveredFeature {
            route:     r.get(0)?,
            label:     r.get(1)?,
            kind:      r.get(2)?,
            selector:  r.get(3)?,
            last_seen: r.get(4)?,
            run_id:    r.get(5)?,
        })
    }).map_err(|e| format!("query list_features: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

/// Total distinct features ever discovered.
pub fn feature_count() -> Result<u32, String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM discovered_features", [], |r| r.get(0),
    ).map_err(|e| format!("count features: {e}"))?;
    Ok(n.max(0) as u32)
}

/// Latest discovery run (any status), or None when none exist yet.
pub fn latest_run() -> Result<Option<DiscoveryRun>, String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    let mut stmt = conn.prepare(
        "SELECT run_id, started_at, finished_at, status, total_widgets, \
                error, seed_url, max_depth \
         FROM discovery_runs \
         ORDER BY started_at DESC LIMIT 1",
    ).map_err(|e| format!("prepare latest_run: {e}"))?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    if let Some(r) = rows.next().map_err(|e| e.to_string())? {
        Ok(Some(DiscoveryRun {
            run_id:        r.get(0).map_err(|e| e.to_string())?,
            started_at:    r.get(1).map_err(|e| e.to_string())?,
            finished_at:   r.get(2).map_err(|e| e.to_string())?,
            status:        r.get(3).map_err(|e| e.to_string())?,
            total_widgets: r.get::<_, Option<i64>>(4)
                .map_err(|e| e.to_string())?
                .map(|n| n.max(0) as u32),
            error:         r.get(5).map_err(|e| e.to_string())?,
            seed_url:      r.get(6).map_err(|e| e.to_string())?,
            max_depth:     r.get::<_, Option<i64>>(7)
                .map_err(|e| e.to_string())?
                .map(|n| n.max(0) as u32),
        }))
    } else {
        Ok(None)
    }
}

/// Wipe all discovered features and run history. Used for "fresh crawl"
/// resets and tests.
#[allow(dead_code)]
pub fn clear_all() -> Result<(), String> {
    let conn = db().lock().map_err(|e| e.to_string())?;
    conn.execute_batch(
        "DELETE FROM discovered_features; DELETE FROM discovery_runs;",
    ).map_err(|e| format!("clear: {e}"))?;
    Ok(())
}

// ── Crawler (iter 2) ──────────────────────────────────────────────────────────

/// Crawl the running app starting at `seed_url`, enumerate interactable
/// elements via Sirin's `dom_snapshot`, and persist each as a
/// `DiscoveredFeature`. Depth-1 only in iter 2 — recursion (click into
/// each widget, walk the resulting page, back up) is left for later.
///
/// Returns the number of features recorded (after dedup).
///
/// Caller is expected to wrap this in a thread; it blocks on browser I/O.
pub fn crawl_app(seed_url: &str, max_depth: u32, run_id: &str) -> Result<u32, String> {
    // Ensure Chrome is up.  `false` = headed so the user can watch; the
    // launch_discovery wrapper can override if it wants headless.
    let _ = crate::browser::ensure_open(false)?;
    crate::browser::navigate(seed_url)?;

    // Brief settle window — DOM is usually ready in 1-2s for SPAs;
    // hash-route apps with async splash screens may need longer (handled
    // by future "wait for DOM stable" follow-up).
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // Always nudge Flutter into building its semantics tree — idempotent
    // for non-Flutter sites, essential for Flutter sites where the regular
    // DOM is just a canvas + the "Enable accessibility" placeholder.
    let _ = crate::browser_ax::enable_flutter_semantics();
    std::thread::sleep(std::time::Duration::from_millis(800));

    let snap = crate::browser::dom_snapshot(200)?;
    let url = snap["url"].as_str().unwrap_or(seed_url).to_string();
    let route = extract_route(&url);

    let elements = snap["elements"].as_array().cloned().unwrap_or_default();
    let now = now_rfc3339();
    let mut count = 0u32;

    for el in elements {
        let name = el["name"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() { continue; }
        let role = el["role"].as_str().unwrap_or("generic");
        let tag  = el["tag"].as_str().unwrap_or("");
        let href = el["href"].as_str().map(String::from);
        let ref_id = el["ref"].as_str()
            .map(|s| format!("[data-sirin-ref=\"{s}\"]"));

        let kind = role_to_kind(role, tag);
        if kind == "generic" { continue; }

        // For links, the *destination* is the more useful "route" — it
        // tells us a future page exists. For everything else, current route.
        let route_for_feat = if kind == "link" {
            href.clone().unwrap_or_else(|| route.clone())
        } else {
            route.clone()
        };

        let feat = DiscoveredFeature {
            route: route_for_feat,
            label: name,
            kind: kind.to_string(),
            selector: ref_id,
            last_seen: now.clone(),
            run_id: Some(run_id.to_string()),
        };
        if record_feature(&feat).is_ok() {
            count += 1;
        }
    }

    // ── Flutter shadow DOM enumeration ──────────────────────────────────
    // After enable_flutter_semantics, flt-semantics-host populates with
    // [role]-tagged nodes that dom_snapshot can't see (open shadow root).
    // Walk those separately and merge into discovered features.
    if let Ok(shadow_entries) = crate::browser::shadow_dump() {
        for entry in shadow_entries {
            // shadow_dump format: "role:label"
            let (role, label) = match entry.split_once(':') {
                Some((r, l)) => (r.trim().to_string(), l.trim().to_string()),
                None => continue,
            };
            if label.is_empty() || role.starts_with("ERROR") || role.starts_with("EMPTY") {
                continue;
            }
            // Map Flutter ARIA roles → discovery kinds.
            let kind = match role.as_str() {
                "button"                       => "button",
                "link"                         => "link",
                "tab" | "menuitem"             => role.as_str(),
                "textbox" | "combobox"         => "form_input",
                "checkbox" | "radio"           => "form_input",
                _                              => continue, // skip generic/text
            };
            let feat = DiscoveredFeature {
                route: route.clone(),
                label,
                kind: kind.to_string(),
                selector: None, // shadow DOM nodes don't get data-sirin-ref
                last_seen: now.clone(),
                run_id: Some(run_id.to_string()),
            };
            if record_feature(&feat).is_ok() {
                count += 1;
            }
        }
    }

    // TODO(#247 follow-up): depth > 1 means click each kind=button, snapshot
    // again, then back. Needs visited-route tracking + state restore.
    let _ = max_depth;

    Ok(count)
}

fn extract_route(url: &str) -> String {
    // Flutter hash routes: http://app.com/#/buyer/checkout → "#/buyer/checkout"
    if let Some(idx) = url.find("#/") {
        return url[idx..].to_string();
    }
    // Standard path: http://app.com/foo/bar → "/foo/bar"
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        if let Some(slash) = after_scheme.find('/') {
            return after_scheme[slash..].to_string();
        }
        return "/".to_string();
    }
    url.to_string()
}

fn role_to_kind(role: &str, tag: &str) -> &'static str {
    match role {
        "button"                       => "button",
        "link"                         => "link",
        "textbox" | "combobox"         => "form_input",
        "tab"                          => "tab",
        "menuitem"                     => "menuitem",
        "checkbox" | "radio"           => "form_input",
        _ => match tag {
            "a"      => "link",
            "button" => "button",
            "input" | "select" | "textarea" => "form_input",
            _        => "generic",
        },
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() {
        let _ = clear_all();
    }

    #[test]
    fn record_and_list() {
        fresh();
        let f = DiscoveredFeature {
            route:     "/buyer/checkout".into(),
            label:     "Place Order".into(),
            kind:      "button".into(),
            selector:  Some("[data-test='place-order']".into()),
            last_seen: now_rfc3339(),
            run_id:    Some("disc_test_1".into()),
        };
        record_feature(&f).expect("record");
        let all = list_features().expect("list");
        assert!(all.iter().any(|g| g.label == "Place Order"));
    }

    #[test]
    fn upsert_dedups_by_route_label_kind() {
        fresh();
        let f = DiscoveredFeature {
            route:     "/seller/products".into(),
            label:     "Add Product".into(),
            kind:      "button".into(),
            selector:  None,
            last_seen: now_rfc3339(),
            run_id:    Some("disc_a".into()),
        };
        record_feature(&f).expect("first");
        let mut g = f.clone();
        g.run_id = Some("disc_b".into());
        g.selector = Some(".btn-add".into());
        record_feature(&g).expect("second");
        let all = list_features().expect("list");
        let matches: Vec<_> = all.iter()
            .filter(|x| x.route == "/seller/products" && x.label == "Add Product")
            .collect();
        assert_eq!(matches.len(), 1, "should dedup on (route,label,kind)");
        assert_eq!(matches[0].run_id.as_deref(), Some("disc_b"));
        assert_eq!(matches[0].selector.as_deref(), Some(".btn-add"));
    }

    #[test]
    fn run_lifecycle() {
        fresh();
        begin_run("disc_lifecycle_1", "http://localhost:3000", 3).expect("begin");
        let latest = latest_run().expect("latest").expect("some");
        assert_eq!(latest.status, "running");
        finish_run("disc_lifecycle_1", "done", Some(42), None).expect("finish");
        let latest = latest_run().expect("latest").expect("some");
        assert_eq!(latest.status, "done");
        assert_eq!(latest.total_widgets, Some(42));
    }
}
