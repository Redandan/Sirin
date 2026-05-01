//! Real implementation of [`AppService`] — wraps actual backend modules.
//!
//! The trait impl block below is a thin delegator: each method forwards to a
//! domain-specific free function in one of the submodules.  This keeps the
//! method bodies with their related helpers instead of in one 600-line file.
//!
//! Submodules:
//! - [`agents`] — agent CRUD, objectives, behavior, per-agent skill toggles.
//! - [`pending`] — pending-reply queue (load / approve / reject / edit).
//! - [`workflow`] — workflow lifecycle + LLM stage generation.
//! - [`integrations`] — Telegram auth, Teams, MCP, Meeting, Chat, Research, Skills.
//! - [`system`] — logs, task tracker, system status, memory, persona, LLM, config, toasts.

mod agents;
mod browser;
mod integrations;
mod pending;
mod system;
mod team;
mod workflow;

use std::sync::Mutex;

use crate::persona::TaskTracker;
use crate::telegram_auth::TelegramAuthState;
use crate::ui_service::*;

pub struct RealService {
    pub tracker: TaskTracker,
    pub tg_auth: TelegramAuthState,
    toasts: Mutex<Vec<ToastEvent>>,
    toast_history: Mutex<Vec<ToastEvent>>,
}

impl RealService {
    pub fn new(tracker: TaskTracker, tg_auth: TelegramAuthState) -> Self {
        Self {
            tracker,
            tg_auth,
            toasts: Mutex::new(Vec::new()),
            toast_history: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn push_toast(&self, level: ToastLevel, text: impl Into<String>) {
        if let Ok(mut t) = self.toasts.lock() {
            t.push(ToastEvent { level, text: text.into() });
        }
    }

    /// Drain pending toasts and record them into history (trimmed to 50).
    pub(super) fn drain_toasts(&self) -> Vec<ToastEvent> {
        let new = self
            .toasts
            .lock()
            .ok()
            .map(|mut t| std::mem::take(&mut *t))
            .unwrap_or_default();
        if let Ok(mut h) = self.toast_history.lock() {
            h.extend(new.iter().cloned());
            if h.len() > 50 {
                let n = h.len() - 50;
                h.drain(..n);
            }
        }
        new
    }

    pub(super) fn toast_history_snapshot(&self) -> Vec<ToastEvent> {
        self.toast_history.lock().ok().map(|h| h.clone()).unwrap_or_default()
    }
}

impl AgentService for RealService {
    fn list_agents(&self) -> Vec<AgentSummary> { agents::list_agents(self) }
    fn agent_detail(&self, id: &str) -> Option<AgentDetailView> { agents::agent_detail(self, id) }
    fn create_agent(&self, id: &str, name: &str) { agents::create_agent(self, id, name) }
    fn rename_agent(&self, id: &str, name: &str) { agents::rename_agent(self, id, name) }
    fn toggle_agent(&self, id: &str, enabled: bool) { agents::toggle_agent(self, id, enabled) }
    fn delete_agent(&self, id: &str) { agents::delete_agent(self, id) }
    fn add_objective(&self, id: &str, text: &str) { agents::add_objective(self, id, text) }
    fn remove_objective(&self, id: &str, index: usize) { agents::remove_objective(self, id, index) }
    fn set_remote_ai(&self, id: &str, allowed: bool) { agents::set_remote_ai(self, id, allowed) }
    fn set_behavior(&self, id: &str, enabled: bool, min_delay: u64, max_delay: u64, max_hour: u32, max_day: u32) {
        agents::set_behavior(self, id, enabled, min_delay, max_delay, max_hour, max_day)
    }
    fn toggle_skill(&self, id: &str, skill_id: &str, enabled: bool) { agents::toggle_skill(self, id, skill_id, enabled) }
    fn disabled_skills(&self, id: &str) -> Vec<String> { agents::disabled_skills(self, id) }
}

impl PendingReplyService for RealService {
    fn pending_count(&self, id: &str) -> usize { pending::pending_count(self, id) }
    fn load_pending(&self, id: &str) -> Vec<PendingReplyView> { pending::load_pending(self, id) }
    fn approve_reply(&self, id: &str, reply_id: &str) { pending::approve_reply(self, id, reply_id) }
    fn reject_reply(&self, id: &str, reply_id: &str) { pending::reject_reply(self, id, reply_id) }
    fn edit_draft(&self, id: &str, reply_id: &str, new_text: &str) { pending::edit_draft(self, id, reply_id, new_text) }
}

impl WorkflowService for RealService {
    fn workflow_state(&self) -> Option<WorkflowView> { workflow::workflow_state(self) }
    fn workflow_create(&self, feature: &str, description: &str) { workflow::workflow_create(self, feature, description) }
    fn workflow_advance(&self) -> bool { workflow::workflow_advance(self) }
    fn workflow_stage_prompt(&self) -> Option<String> { workflow::workflow_stage_prompt(self) }
    fn workflow_reset(&self) { workflow::workflow_reset(self) }
    fn workflow_generate(&self) -> Option<String> { workflow::workflow_generate(self) }
    fn workflow_save_output(&self, stage_id: &str, output: &str) { workflow::workflow_save_output(self, stage_id, output) }
}

impl IntegrationService for RealService {
    fn tg_submit_code(&self, code: &str) -> bool { integrations::tg_submit_code(self, code) }
    fn tg_submit_password(&self, pwd: &str) -> bool { integrations::tg_submit_password(self, pwd) }
    fn tg_reconnect(&self) { integrations::tg_reconnect(self) }
    fn start_teams(&self) { integrations::start_teams(self) }
    fn teams_running(&self) -> bool { integrations::teams_running(self) }
    fn mcp_tools(&self) -> Vec<McpToolDetail> { integrations::mcp_tools(self) }
    fn mcp_call(&self, tool: &str, args: &str) -> Result<String, String> { integrations::mcp_call(self, tool, args) }
    fn sirin_mcp_call(&self, tool: &str, args: &str) -> Result<String, String> { integrations::sirin_mcp_call(self, tool, args) }
    fn meeting_active(&self) -> bool { integrations::meeting_active(self) }
    fn meeting_start(&self, participants: Vec<String>) -> String { integrations::meeting_start(self, participants) }
    fn meeting_end(&self) { integrations::meeting_end(self) }
    fn meeting_send(&self, speaker: &str, text: &str) { integrations::meeting_send(self, speaker, text) }
    fn meeting_history(&self) -> Vec<(String, String)> { integrations::meeting_history(self) }
    fn chat_send(&self, id: &str, message: &str) -> String { integrations::chat_send(self, id, message) }
    fn trigger_research(&self, topic: &str, url: Option<&str>) { integrations::trigger_research(self, topic, url) }
    fn execute_skill(&self, skill_id: &str, input: &str) -> String { integrations::execute_skill(self, skill_id, input) }
}

impl SystemService for RealService {
    fn recent_tasks(&self, limit: usize) -> Vec<TaskView> { system::recent_tasks(self, limit) }
    fn log_version(&self) -> usize { system::log_version(self) }
    fn log_recent(&self, limit: usize) -> Vec<LogLine> { system::log_recent(self, limit) }
    fn log_len(&self) -> usize { system::log_len(self) }
    fn log_clear(&self) { system::log_clear(self) }
    fn system_status(&self) -> SystemStatus { system::system_status(self) }
    fn search_memory(&self, query: &str, limit: usize) -> Vec<String> { system::search_memory(self, query, limit) }
    fn persona_name(&self) -> String { system::persona_name(self) }
    fn set_persona_name(&self, name: &str) { system::set_persona_name(self, name) }
    fn persona_objectives(&self) -> Vec<String> { system::persona_objectives(self) }
    fn set_persona_objectives(&self, objectives: Vec<String>) { system::set_persona_objectives(self, objectives) }
    fn persona_voice(&self) -> String { system::persona_voice(self) }
    fn set_persona_voice(&self, voice: &str) { system::set_persona_voice(self, voice) }
    fn available_models(&self) -> Vec<String> { system::available_models(self) }
    fn set_main_model(&self, model: &str) { system::set_main_model(self, model) }
    fn export_config(&self) -> String { system::export_config(self) }
    fn import_config(&self, yaml: &str) -> Result<(), String> { system::import_config(self, yaml) }
    fn poll_toasts(&self) -> Vec<ToastEvent> { system::poll_toasts(self) }
    fn toast_history(&self) -> Vec<ToastEvent> { system::toast_history(self) }
    fn config_check(&self) -> Vec<ConfigIssueView> { system::config_check(self) }
    fn config_ai_analyze(&self) -> Result<AiAdviceView, String> { system::config_ai_analyze(self) }
    fn config_apply_fixes(&self, fixes: Vec<ConfigFixView>) -> Result<Vec<String>, String> {
        system::config_apply_fixes(self, fixes)
    }
}

impl BrowserService for RealService {
    fn browser_is_open(&self) -> bool { browser::browser_is_open(self) }
    fn browser_open(&self, url: &str, headless: bool) { browser::browser_open(self, url, headless) }
    fn browser_navigate(&self, url: &str) -> Result<(), String> { browser::browser_navigate(self, url) }
    fn browser_click(&self, selector: &str) -> Result<(), String> { browser::browser_click(self, selector) }
    fn browser_type(&self, selector: &str, text: &str) -> Result<(), String> { browser::browser_type(self, selector, text) }
    fn browser_screenshot(&self) -> Option<Vec<u8>> { browser::browser_screenshot(self) }
    fn browser_eval(&self, js: &str) -> Result<String, String> { browser::browser_eval(self, js) }
    fn browser_read(&self, selector: &str) -> Result<String, String> { browser::browser_read(self, selector) }
    fn browser_close(&self) { browser::browser_close(self) }
    fn browser_url(&self) -> Option<String> { browser::browser_url(self) }
    fn browser_title(&self) -> Option<String> { browser::browser_title(self) }
    fn browser_click_point(&self, x: f64, y: f64) -> Result<(), String> { browser::browser_click_point(self, x, y) }
    fn browser_hover(&self, selector: &str) -> Result<(), String> { browser::browser_hover(self, selector) }
    fn browser_press_key(&self, key: &str) -> Result<(), String> { browser::browser_press_key(self, key) }
    fn browser_wait(&self, selector: &str, timeout_ms: u64) -> Result<(), String> { browser::browser_wait(self, selector, timeout_ms) }
    fn browser_exists(&self, selector: &str) -> bool { browser::browser_exists(self, selector) }
    fn browser_select(&self, selector: &str, value: &str) -> Result<(), String> { browser::browser_select(self, selector, value) }
    fn browser_scroll(&self, x: f64, y: f64) -> Result<(), String> { browser::browser_scroll(self, x, y) }
    fn browser_set_viewport(&self, width: u32, height: u32, mobile: bool) -> Result<(), String> { browser::browser_set_viewport(self, width, height, mobile) }
    fn browser_console(&self, limit: usize) -> String { browser::browser_console(self, limit) }
    fn browser_tab_count(&self) -> usize { browser::browser_tab_count(self) }
}

impl MultiAgentService for RealService {
    fn team_dashboard(&self)                          -> TeamDashView      { team::team_dashboard(self) }
    fn team_queue(&self)                              -> Vec<TeamTaskView> { team::team_queue(self) }
    fn team_enqueue(&self, desc: &str)                -> String            { team::team_enqueue(self, desc) }
    fn team_start_worker(&self)                       { team::team_start_worker(self) }
    fn team_clear_completed(&self)                    { team::team_clear_completed(self) }
    fn team_reset_member(&self, role: &str)           { team::team_reset_member(self, role) }
    fn team_token_usage(&self, window_secs: u64)      -> TokenUsageView    { team::team_token_usage(self, window_secs) }

    fn dev_team_read_issue(&self, gh_repo: &str, n: u32) -> Result<GhIssueView, String> {
        team::dev_team_read_issue(self, gh_repo, n)
    }
    fn dev_team_enqueue_issue(
        &self, project_key: &str, gh_repo: &str, n: u32, dry_run: bool, priority: u8,
    ) -> Result<String, String> {
        team::dev_team_enqueue_issue(self, project_key, gh_repo, n, dry_run, priority)
    }
    fn dev_team_list_previews(&self) -> Vec<DryRunPreviewView> {
        team::dev_team_list_previews(self)
    }
    fn dev_team_replay_preview(&self, task_id: &str) -> Result<(), String> {
        team::dev_team_replay_preview(self, task_id)
    }
}

impl TestRunnerService for RealService {
    fn recent_test_runs(&self, limit: usize) -> Vec<TestRunView> {
        let rows = crate::test_runner::store::recent_runs_all(limit);

        // Compute per-test_id pass rate over the visible window so the UI
        // can flag flaky tests inline (Issue #31).  Only counts terminal
        // statuses — running/queued rows are excluded from the denominator.
        use std::collections::HashMap;
        let mut totals: HashMap<String, (u32, u32)> = HashMap::new(); // (passed, total)
        for r in &rows {
            match r.status.as_str() {
                "passed" => {
                    let e = totals.entry(r.test_id.clone()).or_insert((0, 0));
                    e.0 += 1; e.1 += 1;
                }
                "failed" | "error" | "timeout" => {
                    let e = totals.entry(r.test_id.clone()).or_insert((0, 0));
                    e.1 += 1;
                }
                _ => {}
            }
        }
        // Need at least 2 terminal runs to call something flaky vs. a one-off.
        let pass_rate_for = |id: &str| -> Option<f32> {
            let (p, t) = totals.get(id).copied()?;
            if t < 2 { return None; }
            Some(p as f32 / t as f32)
        };

        rows.into_iter()
            .map(|r| TestRunView {
                pass_rate:        pass_rate_for(&r.test_id),
                test_id:          r.test_id,
                status:           r.status,
                started_at:       r.started_at,
                duration_ms:      r.duration_ms.map(|d| d as u64),
                analysis:         r.ai_analysis,
                step:             None,
                failure_category: r.failure_category,
                // Issue #222: populate console stats from RunRecord (#220 field)
                console_errors:   Some(r.console_errors),
                console_warnings: Some(r.console_warnings),
            })
            .collect()
    }

    fn active_test_runs(&self) -> Vec<TestRunView> {
        use crate::test_runner::runs::{list_active, get, RunPhase};
        list_active()
            .into_iter()
            .filter_map(|run_id| get(&run_id))
            .map(|s| {
                let (status, analysis, step) = match &s.phase {
                    RunPhase::Queued => ("queued".to_string(), None, None),
                    RunPhase::Running { current_action, step } =>
                        ("running".to_string(), Some(current_action.clone()), Some(*step)),
                    _ => ("unknown".to_string(), None, None),
                };
                TestRunView {
                    test_id:          s.test_id.clone(),
                    status,
                    started_at:       s.started_at.clone(),
                    duration_ms:      None,
                    analysis,
                    step,
                    failure_category: None,
                    pass_rate:        None,
                    console_errors:   None,  // not available for active runs
                    console_warnings: None,
                }
            })
            .collect()
    }

    fn list_test_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = crate::test_runner::list_tests()
            .into_iter()
            .map(|g| g.id)
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    fn launch_test_run(&self, test_id: &str) -> Result<String, String> {
        // Fire-and-forget — spawn_run_async returns the run_id immediately;
        // the actual ReAct loop runs in a detached thread.  Auto-fix off by
        // default for dashboard-launched runs (user can re-trigger via MCP if
        // they want auto-fix).
        crate::test_runner::spawn_run_async(test_id.to_string(), false)
    }

    fn test_coverage_data(&self) -> Result<CoverageData, String> {
        use serde_json::Value;

        let map_path = crate::platform::config_path("coverage/agora_market.yaml");
        let src = std::fs::read_to_string(&map_path)
            .map_err(|e| format!("Cannot read coverage map {map_path:?}: {e}"))?;
        // Parse via serde_yaml → serde_json::Value (same pattern as mcp_server).
        let map: Value = serde_yaml::from_str(&src)
            .map_err(|e| format!("Parse error in agora_market.yaml: {e}"))?;

        let product = map["product"].as_str().unwrap_or("agora_market").to_string();
        let version = map["version"].as_str().unwrap_or("?").to_string();

        let groups_raw = map["feature_groups"].as_array()
            .ok_or_else(|| "feature_groups missing or not array".to_string())?;

        let mut total_covered  = 0usize;
        let mut total_features = 0usize;
        let mut total_scripted = 0usize;
        let mut groups = Vec::new();

        for g in groups_raw {
            let gid   = g["id"].as_str().unwrap_or("?").to_string();
            let gname = g["name"].as_str().unwrap_or(&gid).to_string();
            let role  = match &g["role"] {
                Value::String(s)  => s.clone(),
                Value::Array(arr) => arr.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("+"),
                _                 => String::new(),
            };

            let features_raw = g["features"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let mut covered_count = 0usize;
            let mut features = Vec::new();

            for feat in features_raw {
                let fid    = feat["id"].as_str().unwrap_or("?").to_string();
                let fname  = feat["name"].as_str().unwrap_or(&fid).to_string();
                let status = feat["status"].as_str().unwrap_or("missing").to_string();
                let test_ids: Vec<String> = feat["test_ids"].as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();

                if status != "missing" && !test_ids.is_empty() {
                    covered_count += 1;
                    // Scaffold heuristic: "confirmed" = stable enough that the
                    // test can be promoted to a deterministic replay script.
                    // Real implementation will check per-test replay_mode in
                    // the run history (see plan §Coverage 3-Tier Model).
                    if status == "confirmed" {
                        total_scripted += 1;
                    }
                }
                features.push(CoverageFeatureView { id: fid, name: fname, status, test_ids });
            }

            total_features += features.len();
            total_covered  += covered_count;

            groups.push(CoverageGroupView {
                id: gid, name: gname, role,
                covered: covered_count,
                total: features.len(),
                features,
            });
        }

        // Discovery layer: until the auto-crawler ships, mock as
        // discovered == total_features (every feature in YAML is "known").
        let discovered = total_features;

        Ok(CoverageData {
            product, version,
            total_covered, total_features, groups,
            discovered,
            scripted: total_scripted,
            discovery_status: crate::ui_service::DiscoveryStatus::NotRun,
        })
    }
}

// `impl AppService for RealService` is satisfied automatically by the blanket
// impl in `ui_service` — no explicit block needed.
