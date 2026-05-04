//! `sirin doctor` — pure-Rust diagnostic subcommand (Issue #261).
//!
//! Mirrors `scripts/preflight.sh` but doesn't depend on bash, curl, awk, find,
//! or platform-specific date arithmetic.  Useful on Windows bare cmd / pwsh
//! and CI where the shell helpers aren't available.
//!
//! Usage:
//!   sirin doctor                # human-readable report, exit 0/1 on FAIL
//!   sirin doctor --format=json  # structured JSON for CI / cron consumption
//!
//! Six sections mirror preflight.sh:
//!   1. Core LLM keys (env-loaded, validated for length)
//!   2. Sirin gateway HTTP probe (:7700/gateway)
//!   3. Vision specialist smoke (1×1 PNG round-trip if configured)
//!   4. YAML config sync (repo `config/` vs LOCALAPPDATA)
//!   5. Recent Chrome stability (sirin.err.log crash count)
//!   6. Action registry consistency (browser_exec.rs vs builtins.rs)

use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus { Pass, Warn, Fail, Skip }

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub label: String,
    pub status: CheckStatus,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSection {
    pub name: String,
    pub checks: Vec<CheckResult>,
}

impl DoctorSection {
    /// Worst-case status across the section's checks (Fail > Warn > Pass).
    pub fn status(&self) -> CheckStatus {
        if self.checks.iter().any(|c| c.status == CheckStatus::Fail) {
            CheckStatus::Fail
        } else if self.checks.iter().any(|c| c.status == CheckStatus::Warn) {
            CheckStatus::Warn
        } else if self.checks.iter().all(|c| c.status == CheckStatus::Skip) {
            CheckStatus::Skip
        } else {
            CheckStatus::Pass
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub version:  String,
    pub sections: Vec<DoctorSection>,
    pub passed:   u32,
    pub warnings: u32,
    pub failed:   u32,
    pub skipped:  u32,
}

impl DoctorReport {
    pub fn exit_code(&self) -> i32 {
        if self.failed > 0 { 1 } else { 0 }
    }
}

/// Run all sections.  Reads `.env` from `LOCALAPPDATA/Sirin/.env` (already
/// loaded by the time `main()` calls us).  Network probes have short timeouts
/// so the whole run completes in a few seconds.
pub fn run() -> DoctorReport {
    let sections = vec![
        section_llm_keys(),
        section_sirin_gateway(),
        section_vision_smoke(),
        section_yaml_sync(),
        section_chrome_stability(),
        section_action_registry(),
    ];

    let mut passed = 0u32; let mut warnings = 0u32;
    let mut failed = 0u32; let mut skipped = 0u32;
    for s in &sections {
        for c in &s.checks {
            match c.status {
                CheckStatus::Pass => passed += 1,
                CheckStatus::Warn => warnings += 1,
                CheckStatus::Fail => failed += 1,
                CheckStatus::Skip => skipped += 1,
            }
        }
    }

    DoctorReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        sections, passed, warnings, failed, skipped,
    }
}

// ─── 1. Core LLM keys ───────────────────────────────────────────────────────
fn section_llm_keys() -> DoctorSection {
    let mut checks = Vec::new();

    let provider = std::env::var("LLM_PROVIDER").unwrap_or_default();
    checks.push(if provider.is_empty() {
        CheckResult { label: "LLM_PROVIDER".into(), status: CheckStatus::Fail,
                      message: "not set in .env".into() }
    } else {
        CheckResult { label: "LLM_PROVIDER".into(), status: CheckStatus::Pass,
                      message: provider.clone() }
    });

    let key_for = |provider: &str| -> (&'static str, usize) {
        match provider {
            "gemini"    => ("GEMINI_API_KEY",     30),
            "anthropic" => ("ANTHROPIC_API_KEY",  50),
            "lmstudio"  => ("LM_STUDIO_API_KEY",  4),  // local-only, often "lm-studio"
            "deepseek"  => ("DEEPSEEK_API_KEY",   30),
            "openai"    => ("OPENAI_API_KEY",     30),
            _           => ("",                    0),
        }
    };
    let (var, min_len) = key_for(provider.as_str());
    if !var.is_empty() {
        let val = std::env::var(var).unwrap_or_default();
        let status = if val.is_empty() {
            (CheckStatus::Fail, format!("{var} EMPTY"))
        } else if val.len() < min_len {
            (CheckStatus::Fail, format!("{var} too short ({} chars, expected >={min_len})", val.len()))
        } else {
            (CheckStatus::Pass, format!("{var} set ({} chars)", val.len()))
        };
        checks.push(CheckResult { label: var.to_string(), status: status.0, message: status.1 });
    }

    // Fallback chain
    let fb_url = std::env::var("LLM_FALLBACK_BASE_URL").unwrap_or_default();
    let fb_key = std::env::var("LLM_FALLBACK_API_KEY").unwrap_or_default();
    if fb_url.is_empty() {
        checks.push(CheckResult {
            label: "LLM_FALLBACK".into(), status: CheckStatus::Warn,
            message: "not configured — 429 retries will stall 35s+".into(),
        });
    } else if fb_key.len() < 20 {
        checks.push(CheckResult {
            label: "LLM_FALLBACK".into(), status: CheckStatus::Fail,
            message: format!("URL set but API key empty/short ({} chars)", fb_key.len()),
        });
    } else {
        checks.push(CheckResult {
            label: "LLM_FALLBACK".into(), status: CheckStatus::Pass,
            message: format!("{fb_url} ({} chars key)", fb_key.len()),
        });
    }

    DoctorSection { name: "1. Core LLM keys".into(), checks }
}

// ─── 2. Sirin gateway HTTP probe ────────────────────────────────────────────
fn section_sirin_gateway() -> DoctorSection {
    let port = std::env::var("SIRIN_RPC_PORT").unwrap_or_else(|_| "7700".into());
    let url = format!("http://127.0.0.1:{port}/gateway");
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .connect_timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(e) => return DoctorSection {
            name: "2. Sirin process".into(),
            checks: vec![CheckResult {
                label: "HTTP client init".into(), status: CheckStatus::Fail,
                message: format!("reqwest builder error: {e}"),
            }],
        },
    };

    let check = match client.get(&url).send() {
        Ok(r) if r.status().as_u16() == 200 => CheckResult {
            label: "Gateway HTTP 200".into(), status: CheckStatus::Pass,
            message: url.clone(),
        },
        Ok(r) => CheckResult {
            label: format!("Gateway HTTP {}", r.status().as_u16()),
            status: CheckStatus::Warn,
            message: format!("{url} returned non-200"),
        },
        Err(e) => CheckResult {
            label: "Gateway unreachable".into(), status: CheckStatus::Fail,
            message: format!("{url}: {e} (Sirin not running?)"),
        },
    };
    DoctorSection { name: "2. Sirin process".into(), checks: vec![check] }
}

// ─── 3. Vision specialist smoke ─────────────────────────────────────────────
fn section_vision_smoke() -> DoctorSection {
    let url = std::env::var("LLM_VISION_BASE_URL").unwrap_or_default();
    let key = std::env::var("LLM_VISION_API_KEY").unwrap_or_default();
    let model = std::env::var("LLM_VISION_MODEL").unwrap_or_default();

    if url.is_empty() {
        return DoctorSection {
            name: "3. Vision smoke".into(),
            checks: vec![CheckResult {
                label: "Vision specialist".into(), status: CheckStatus::Skip,
                message: "LLM_VISION_BASE_URL not set — using main LLM for vision".into(),
            }],
        };
    }
    if key.len() < 4 {
        return DoctorSection {
            name: "3. Vision smoke".into(),
            checks: vec![CheckResult {
                label: "Vision API key".into(), status: CheckStatus::Fail,
                message: format!("LLM_VISION_API_KEY too short ({} chars)", key.len()),
            }],
        };
    }

    // 1×1 transparent PNG (smallest valid).
    let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let body = serde_json::json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "reply OK"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{png_b64}")}},
            ],
        }],
        "max_tokens": 5,
    });
    let endpoint = format!("{url}/chat/completions");
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => return DoctorSection {
            name: "3. Vision smoke".into(),
            checks: vec![CheckResult {
                label: "HTTP client init".into(), status: CheckStatus::Fail,
                message: format!("reqwest builder error: {e}"),
            }],
        },
    };

    let result = client.post(&endpoint)
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send();

    let check = match result {
        Ok(r) if r.status().as_u16() == 200 => CheckResult {
            label: format!("Vision {model}"), status: CheckStatus::Pass,
            message: "1×1 PNG round-trip OK".into(),
        },
        Ok(r) => CheckResult {
            label: format!("Vision HTTP {}", r.status().as_u16()),
            status: CheckStatus::Warn,
            message: format!("{endpoint} returned {}", r.status().as_u16()),
        },
        Err(e) => CheckResult {
            label: "Vision request error".into(), status: CheckStatus::Fail,
            message: format!("{e}"),
        },
    };
    DoctorSection { name: "3. Vision smoke".into(), checks: vec![check] }
}

// ─── 4. YAML config sync (repo vs LOCALAPPDATA) ─────────────────────────────
fn section_yaml_sync() -> DoctorSection {
    let mut checks = Vec::new();
    let probe = "config/tests/agora_regression/agora_pickup_time_picker.yaml";
    let repo_path  = std::path::PathBuf::from(probe);
    let local_path = crate::platform::app_data_dir().join(probe);

    let exists = (repo_path.exists(), local_path.exists());
    match exists {
        (true, true) => {
            let repo  = std::fs::read_to_string(&repo_path).unwrap_or_default();
            let local = std::fs::read_to_string(&local_path).unwrap_or_default();
            // Normalise Windows CRLF → LF before compare so LF-only repo files
            // don't false-positive against CRLF-converted LOCALAPPDATA copies.
            let r = repo.replace("\r\n", "\n");
            let l = local.replace("\r\n", "\n");
            if r == l {
                checks.push(CheckResult {
                    label: "pickup_time_picker.yaml".into(), status: CheckStatus::Pass,
                    message: "repo == LOCALAPPDATA".into(),
                });
            } else {
                checks.push(CheckResult {
                    label: "pickup_time_picker.yaml".into(), status: CheckStatus::Fail,
                    message: "repo != LOCALAPPDATA — call MCP sync_config or copy manually".into(),
                });
            }
        }
        (true, false) => checks.push(CheckResult {
            label: "LOCALAPPDATA missing".into(), status: CheckStatus::Warn,
            message: format!("{} doesn't exist — run Sirin once to populate", local_path.display()),
        }),
        (false, _) => checks.push(CheckResult {
            label: "Repo probe missing".into(), status: CheckStatus::Skip,
            message: "Run from Sirin repo root for YAML sync check".into(),
        }),
    }

    DoctorSection { name: "4. YAML config sync".into(), checks }
}

// ─── 5. Recent Chrome stability ─────────────────────────────────────────────
fn section_chrome_stability() -> DoctorSection {
    let log_paths = ["sirin.err.log", "sirin.log"];
    let mut checks = Vec::new();
    let mut found_any = false;
    for p in log_paths {
        let path = std::path::Path::new(p);
        if !path.exists() { continue; }
        found_any = true;
        let content = std::fs::read_to_string(path).unwrap_or_default();

        // Last 24h cutoff in ISO-8601 (matches preflight.sh awk regex).
        let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
            .format("%Y-%m-%dT%H:%M").to_string();

        let mut crashes = 0u32;
        let mut launches = 0u32;
        for line in content.lines() {
            // Extract leading timestamp marker — log format starts with
            // "[2026-05-03T..." or similar.  We strip ANSI escapes too since
            // tracing-subscriber colourises by default.
            let stripped: String = line
                .chars()
                .filter(|c| !c.is_control() || *c == ' ' || *c == '\t')
                .collect();
            // Find ISO timestamp in first ~30 chars.
            let ts_window = &stripped[..stripped.len().min(40)];
            let in_window = ts_window
                .find(|c: char| c == 'T')
                .and_then(|t| ts_window.get(t.saturating_sub(10)..t + 6))
                .map(|w| w >= cutoff.as_str())
                .unwrap_or(true);  // if we can't parse, count it (conservative)
            if !in_window { continue; }

            if line.contains("Chrome crashed") || line.contains("connection closed") && line.contains("recovering") {
                crashes += 1;
            }
            if line.contains("launched Chrome") {
                launches += 1;
            }
        }

        let status = if crashes > 5 {
            CheckStatus::Warn
        } else {
            CheckStatus::Pass
        };
        checks.push(CheckResult {
            label: format!("{p} (last 24h)"),
            status,
            message: format!("{crashes} crashes / {launches} launches"),
        });
    }
    if !found_any {
        checks.push(CheckResult {
            label: "Sirin log files".into(), status: CheckStatus::Skip,
            message: "Run from Sirin run directory to inspect logs".into(),
        });
    }
    DoctorSection { name: "5. Chrome stability".into(), checks }
}

// ─── 6. Action registry consistency ─────────────────────────────────────────
fn section_action_registry() -> DoctorSection {
    let mut checks = Vec::new();
    // Static list — must mirror scripts/preflight.sh BROWSER_ACTIONS for parity.
    // (Drift between the two is itself a smell — both should reference the
    // same source of truth eventually; for now we keep them in sync manually.)
    let browser_actions = [
        "goto", "screenshot", "screenshot_analyze", "click", "click_point", "type",
        "read", "eval", "wait", "exists", "attr", "scroll", "key", "console",
        "network", "url", "title", "close", "set_viewport", "enable_a11y",
        "ax_tree", "ax_find", "ax_value", "ax_click", "ax_focus", "ax_type",
        "ax_type_verified", "ax_snapshot", "ax_diff", "wait_for_ax_change",
        "wait_for_url", "wait_for_ax_ready", "wait_for_network_idle",
        "assert_ax_contains", "assert_url_matches", "shadow_find", "shadow_click",
        "shadow_dump", "flutter_type", "flutter_enter", "shadow_type_flutter",
        "go_back", "clear_state", "reset_session", "wait_new_tab", "wait_request",
        "list_sessions", "close_session", "dom_snapshot",
    ];
    let candidates = [
        std::path::PathBuf::from("src/adk/tool/builtins.rs"),
        std::path::PathBuf::from("src/browser_exec.rs"),
    ];
    let combined: String = candidates
        .iter()
        .filter(|p| p.exists())
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect::<Vec<_>>()
        .join("\n");
    if combined.is_empty() {
        checks.push(CheckResult {
            label: "Action registry source".into(), status: CheckStatus::Skip,
            message: "Run from Sirin repo root for registry check".into(),
        });
        return DoctorSection { name: "6. Action registry".into(), checks };
    }

    let mut missing = Vec::new();
    for action in browser_actions {
        let needle = format!("\"{action}\"");
        if !combined.contains(&needle) {
            missing.push(action);
        }
    }
    if missing.is_empty() {
        checks.push(CheckResult {
            label: "Browser actions".into(), status: CheckStatus::Pass,
            message: format!("all {} registered", browser_actions.len()),
        });
    } else {
        checks.push(CheckResult {
            label: "Browser actions".into(), status: CheckStatus::Fail,
            message: format!("missing: {}", missing.join(", ")),
        });
    }
    DoctorSection { name: "6. Action registry".into(), checks }
}

// ─── Output formatting ──────────────────────────────────────────────────────

pub fn print_text(r: &DoctorReport) {
    println!("=========================================");
    println!("  Sirin v{} doctor", r.version);
    println!("=========================================");
    for s in &r.sections {
        let glyph = match s.status() {
            CheckStatus::Pass => "✓", CheckStatus::Warn => "⚠", CheckStatus::Fail => "✗",
            CheckStatus::Skip => "·",
        };
        println!("\n{glyph} {}", s.name);
        for c in &s.checks {
            let g = match c.status {
                CheckStatus::Pass => "  [OK] ", CheckStatus::Warn => "  [!]  ",
                CheckStatus::Fail => "  [X]  ", CheckStatus::Skip => "  [-]  ",
            };
            println!("{g}{}: {}", c.label, c.message);
        }
    }
    println!("\nSummary: {} passed, {} warnings, {} failed, {} skipped",
        r.passed, r.warnings, r.failed, r.skipped);
}

pub fn print_json(r: &DoctorReport) {
    println!("{}", serde_json::to_string_pretty(r).unwrap_or_else(|_| "{}".into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_section(name: &str, checks: Vec<(&str, CheckStatus)>) -> DoctorSection {
        DoctorSection {
            name: name.to_string(),
            checks: checks.into_iter().map(|(label, status)| CheckResult {
                label: label.to_string(),
                status,
                message: "".into(),
            }).collect(),
        }
    }

    #[test]
    fn section_status_picks_worst() {
        // Pure pass
        let s = mk_section("a", vec![("x", CheckStatus::Pass), ("y", CheckStatus::Pass)]);
        assert_eq!(s.status(), CheckStatus::Pass);

        // Mixed pass/warn → warn
        let s = mk_section("b", vec![("x", CheckStatus::Pass), ("y", CheckStatus::Warn)]);
        assert_eq!(s.status(), CheckStatus::Warn);

        // Any fail → fail (even with passes)
        let s = mk_section("c", vec![("x", CheckStatus::Pass), ("y", CheckStatus::Warn), ("z", CheckStatus::Fail)]);
        assert_eq!(s.status(), CheckStatus::Fail);

        // All-skip → skip
        let s = mk_section("d", vec![("x", CheckStatus::Skip), ("y", CheckStatus::Skip)]);
        assert_eq!(s.status(), CheckStatus::Skip);
    }

    #[test]
    fn exit_code_is_one_on_any_fail() {
        let r = DoctorReport {
            version: "test".into(),
            sections: vec![],
            passed: 5, warnings: 0, failed: 1, skipped: 0,
        };
        assert_eq!(r.exit_code(), 1);

        let r = DoctorReport {
            version: "test".into(),
            sections: vec![],
            passed: 5, warnings: 3, failed: 0, skipped: 1,
        };
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn json_serialisation_uses_lowercase_status() {
        let r = DoctorReport {
            version: "test".into(),
            sections: vec![mk_section("x", vec![
                ("p", CheckStatus::Pass),
                ("w", CheckStatus::Warn),
                ("f", CheckStatus::Fail),
                ("s", CheckStatus::Skip),
            ])],
            passed: 1, warnings: 1, failed: 1, skipped: 1,
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains("\"status\":\"pass\""));
        assert!(j.contains("\"status\":\"warn\""));
        assert!(j.contains("\"status\":\"fail\""));
        assert!(j.contains("\"status\":\"skip\""));
    }
}
