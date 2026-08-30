//! Evidence-led supervisor state for Codex desktop tasks.
//!
//! Sirin cannot safely infer task completion or authorization from the local
//! token counter. A Codex-side heartbeat reads recent turns with the desktop
//! task tools and submits only structured evidence here. This module stores no
//! prompts, assistant messages, task titles, credentials, or command output.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = "codex_supervisor.json";
const MAX_RECORDS: usize = 100;
const RECORD_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;
const DEDUPE_WINDOW_SECS: i64 = 6 * 60 * 60;
const CLAIM_LEASE_SECS: i64 = 10 * 60;
const REPORT_STALE_SECS: i64 = 15 * 60;
const MAX_KEY_LEN: usize = 160;
const CONTINUATION_PROMPT: &str = "先重新閱讀本任務的原始對話與最新回合。只延續仍未完成、且原本已獲使用者要求的範圍；不要重做已完成工作，也不得擴大權限。若工作其實已完成、正在等待正確時點，或下一步需要使用者決策、核准、憑證或新增權限，請立即停止並在本任務說明。否則請從最後一個已驗證的進度點繼續，完成必要驗證後交付結果。";

static FILE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

// ── Evidence model ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SupervisorClassification {
    HealthyRunning,
    Completed,
    WaitingCorrectTime,
    UserDecisionRequired,
    HighRiskAuthUnclear,
    SuspectedStall,
    RecoverableInterruption,
    ControlStateMismatch,
    UnreadableOrTimeout,
    #[default]
    Unknown,
}

impl SupervisorClassification {
    fn as_str(self) -> &'static str {
        match self {
            Self::HealthyRunning => "HEALTHY_RUNNING",
            Self::Completed => "COMPLETED",
            Self::WaitingCorrectTime => "WAITING_CORRECT_TIME",
            Self::UserDecisionRequired => "USER_DECISION_REQUIRED",
            Self::HighRiskAuthUnclear => "HIGH_RISK_AUTH_UNCLEAR",
            Self::SuspectedStall => "SUSPECTED_STALL",
            Self::RecoverableInterruption => "RECOVERABLE_INTERRUPTION",
            Self::ControlStateMismatch => "CONTROL_STATE_MISMATCH",
            Self::UnreadableOrTimeout => "UNREADABLE_OR_TIMEOUT",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RecommendedAction {
    #[default]
    None,
    ObserveAgain,
    ContinueOnce,
    NotifyUser,
    FailClosed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TurnStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
    Waiting,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskIntent {
    Analyze,
    Change,
    DeepCheck,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AuthorizedAction {
    Read,
    Edit,
    Test,
    Commit,
    Push,
    Deploy,
    DeviceInspect,
    DeviceControl,
    ExternalMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CoverageSurface {
    Code,
    UnitTest,
    Integration,
    Runtime,
    WebUi,
    MobileUi,
    Simulator,
    PhysicalDevice,
    Deployment,
}

impl CoverageSurface {
    fn as_str(self) -> &'static str {
        match self {
            Self::Code => "CODE",
            Self::UnitTest => "UNIT_TEST",
            Self::Integration => "INTEGRATION",
            Self::Runtime => "RUNTIME",
            Self::WebUi => "WEB_UI",
            Self::MobileUi => "MOBILE_UI",
            Self::Simulator => "SIMULATOR",
            Self::PhysicalDevice => "PHYSICAL_DEVICE",
            Self::Deployment => "DEPLOYMENT",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CoverageStatus {
    Pass,
    Fail,
    Pending,
    MissingProof,
    NotApplicable,
    #[default]
    Unknown,
}

impl CoverageStatus {
    fn is_gap(self) -> bool {
        matches!(self, Self::Pending | Self::MissingProof | Self::Unknown)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageEvidence {
    pub surface: CoverageSurface,
    #[serde(default)]
    pub status: CoverageStatus,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub human_required: bool,
    #[serde(default)]
    pub observed_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskContract {
    #[serde(default)]
    pub intent: TaskIntent,
    #[serde(default)]
    pub authorized_actions: Vec<AuthorizedAction>,
    #[serde(default)]
    pub deep_check_requested: bool,
    #[serde(default)]
    pub read_only: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEvidenceReport {
    pub thread_id: String,
    #[serde(default)]
    pub host_id: Option<String>,
    /// Opaque digest/cursor for the latest user turn. Raw message text is rejected.
    pub latest_user_turn_key: String,
    /// Opaque digest for the unfinished scope. Raw task text is not stored.
    #[serde(default)]
    pub unfinished_scope_key: String,
    #[serde(default)]
    pub latest_turn_status: TurnStatus,
    #[serde(default = "default_true")]
    pub readable: bool,
    #[serde(default)]
    pub fresh_progress: bool,
    #[serde(default)]
    pub active_operation: bool,
    #[serde(default)]
    pub final_answer_fulfills: bool,
    #[serde(default)]
    pub objective_unfinished: bool,
    #[serde(default)]
    pub waiting_correct_time: bool,
    #[serde(default)]
    pub needs_user_decision: bool,
    #[serde(default)]
    pub needs_new_approval: bool,
    #[serde(default)]
    pub high_risk_next_action: bool,
    #[serde(default)]
    pub exact_authority_present: bool,
    #[serde(default)]
    pub bounded_observation_unchanged: bool,
    #[serde(default)]
    pub tool_result_gap: bool,
    #[serde(default)]
    pub assistant_declared_incomplete: bool,
    #[serde(default)]
    pub continuation_already_visible: bool,
    #[serde(default)]
    pub control_state_mismatch: bool,
    /// Stable error identifier such as THREAD_NOT_LOADED; never raw output.
    #[serde(default)]
    pub read_error_code: Option<String>,
    #[serde(default)]
    pub contract: TaskContract,
    #[serde(default)]
    pub coverage: Vec<CoverageEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorDecision {
    pub classification: SupervisorClassification,
    pub recommended_action: RecommendedAction,
    pub reason_codes: Vec<String>,
    pub fingerprint: String,
    pub coverage_gaps: Vec<CoverageSurface>,
    pub human_coverage_gaps: Vec<CoverageSurface>,
    pub evidence: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContinuationState {
    claim_id: Option<String>,
    claim_fingerprint: Option<String>,
    claimed_at_ms: Option<i64>,
    claim_expires_at_ms: Option<i64>,
    last_continuation_fingerprint: Option<String>,
    last_continuation_at_ms: Option<i64>,
    last_dispatch_status: Option<String>,
}

impl ContinuationState {
    fn has_active_claim(&self, now: i64) -> bool {
        self.claim_expires_at_ms
            .is_some_and(|expires| expires > now)
    }

    fn clear_claim(&mut self) {
        self.claim_id = None;
        self.claim_fingerprint = None;
        self.claimed_at_ms = None;
        self.claim_expires_at_ms = None;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskRecord {
    report: TaskEvidenceReport,
    decision: SupervisorDecision,
    reported_at_ms: i64,
    #[serde(default)]
    continuation: ContinuationState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SupervisorState {
    version: u32,
    updated_at_ms: i64,
    records: Vec<TaskRecord>,
}

impl Default for SupervisorState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            updated_at_ms: 0,
            records: Vec::new(),
        }
    }
}

// ── Read-only API views ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorPolicyView {
    pub scan_interval_secs: u64,
    pub deep_read_candidate_limit: u32,
    pub max_continuations_per_scan: u32,
    pub dedupe_window_secs: u64,
    pub report_stale_secs: u64,
}

impl Default for SupervisorPolicyView {
    fn default() -> Self {
        Self {
            scan_interval_secs: 1_800,
            deep_read_candidate_limit: 2,
            max_continuations_per_scan: 1,
            dedupe_window_secs: DEDUPE_WINDOW_SECS as u64,
            report_stale_secs: REPORT_STALE_SECS as u64,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorSafetyView {
    pub local_only: bool,
    pub conversation_content_persisted: bool,
    pub task_titles_persisted: bool,
    pub can_grant_approval: bool,
    pub can_dispatch_directly: bool,
    pub automatic_forking: bool,
}

impl Default for SupervisorSafetyView {
    fn default() -> Self {
        Self {
            local_only: true,
            conversation_content_persisted: false,
            task_titles_persisted: false,
            can_grant_approval: false,
            can_dispatch_directly: false,
            automatic_forking: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorTaskView {
    pub thread_id: String,
    pub display_name: String,
    pub host_id: Option<String>,
    pub reported_at_ms: i64,
    pub age_secs: u64,
    pub classification: SupervisorClassification,
    pub recommended_action: RecommendedAction,
    pub reason_codes: Vec<String>,
    pub fingerprint: String,
    pub coverage: Vec<CoverageEvidence>,
    pub coverage_gaps: Vec<CoverageSurface>,
    pub human_coverage_gaps: Vec<CoverageSurface>,
    pub claim_active: bool,
    pub last_dispatch_status: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorSnapshot {
    pub status: String,
    pub evidence: String,
    pub captured_at: String,
    pub last_reported_at_ms: Option<i64>,
    pub task_count: usize,
    pub actionable_count: usize,
    pub user_intervention_count: usize,
    pub counts: BTreeMap<String, usize>,
    pub tasks: Vec<SupervisorTaskView>,
    pub policy: SupervisorPolicyView,
    pub safety: SupervisorSafetyView,
    pub source_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ContinuationClaimView {
    status: String,
    claim_id: Option<String>,
    thread_id: Option<String>,
    host_id: Option<String>,
    fingerprint: Option<String>,
    prompt: Option<String>,
    expires_at_ms: Option<i64>,
    boundary: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimArgs {
    #[serde(default, alias = "thread_id")]
    thread_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum DispatchOutcome {
    Accepted,
    NotSent,
    Failed,
    TargetRunning,
}

impl DispatchOutcome {
    fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_uppercase().as_str() {
            "ACCEPTED" => Ok(Self::Accepted),
            "NOT_SENT" => Ok(Self::NotSent),
            "FAILED" => Ok(Self::Failed),
            "TARGET_RUNNING" => Ok(Self::TargetRunning),
            _ => Err("outcome must be ACCEPTED, NOT_SENT, FAILED, or TARGET_RUNNING".to_string()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "ACCEPTED",
            Self::NotSent => "NOT_SENT",
            Self::Failed => "FAILED",
            Self::TargetRunning => "TARGET_RUNNING",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompleteActionArgs {
    #[serde(alias = "claim_id")]
    claim_id: String,
    outcome: String,
}

// ── Classification ──────────────────────────────────────────────────────────

fn coverage_gaps(report: &TaskEvidenceReport) -> (Vec<CoverageSurface>, Vec<CoverageSurface>) {
    let mut gaps = Vec::new();
    let mut human = Vec::new();
    for item in &report.coverage {
        if item.required && item.status.is_gap() {
            gaps.push(item.surface);
            if item.human_required {
                human.push(item.surface);
            }
        }
    }
    gaps.sort();
    gaps.dedup();
    human.sort();
    human.dedup();
    (gaps, human)
}

fn fingerprint(report: &TaskEvidenceReport) -> String {
    let mut coverage = report
        .coverage
        .iter()
        .filter(|item| item.required)
        .map(|item| {
            format!(
                "{}:{:?}:{}",
                item.surface.as_str(),
                item.status,
                item.human_required
            )
        })
        .collect::<Vec<_>>();
    coverage.sort();
    let material = format!(
        "{}|{}|{:?}|{}|{}|{}",
        report.thread_id,
        report.latest_user_turn_key,
        report.latest_turn_status,
        report.unfinished_scope_key,
        report.objective_unfinished,
        coverage.join(",")
    );
    truncated_sha256_hex(&material, 16)
}

fn truncated_sha256_hex(material: &str, byte_count: usize) -> String {
    let digest = Sha256::digest(material.as_bytes());
    digest
        .iter()
        .take(byte_count)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn classify(report: &TaskEvidenceReport) -> SupervisorDecision {
    let (gaps, human_gaps) = coverage_gaps(report);
    let fp = fingerprint(report);
    let make = |classification: SupervisorClassification,
                recommended_action: RecommendedAction,
                reason_codes: Vec<&str>,
                evidence: &str| {
        SupervisorDecision {
            classification,
            recommended_action,
            reason_codes: reason_codes.into_iter().map(str::to_string).collect(),
            fingerprint: fp.clone(),
            coverage_gaps: gaps.clone(),
            human_coverage_gaps: human_gaps.clone(),
            evidence: evidence.to_string(),
        }
    };

    if !report.readable || report.read_error_code.is_some() {
        return make(
            SupervisorClassification::UnreadableOrTimeout,
            RecommendedAction::FailClosed,
            vec!["TASK_EVIDENCE_UNREADABLE"],
            "MISSING_PROOF",
        );
    }
    if report.control_state_mismatch {
        return make(
            SupervisorClassification::ControlStateMismatch,
            RecommendedAction::NotifyUser,
            vec!["CONTROL_STATE_MISMATCH"],
            "MEASURED",
        );
    }
    if report.waiting_correct_time {
        return make(
            SupervisorClassification::WaitingCorrectTime,
            RecommendedAction::None,
            vec!["VALID_CLOCK_OR_SCHEDULE"],
            "MEASURED",
        );
    }
    if report.needs_user_decision || report.needs_new_approval || !human_gaps.is_empty() {
        let reason = if !human_gaps.is_empty() {
            "HUMAN_COVERAGE_REQUIRED"
        } else if report.needs_new_approval {
            "NEW_APPROVAL_REQUIRED"
        } else {
            "USER_INPUT_REQUIRED"
        };
        return make(
            SupervisorClassification::UserDecisionRequired,
            RecommendedAction::NotifyUser,
            vec![reason],
            "INFERRED",
        );
    }
    if report.high_risk_next_action && !report.exact_authority_present {
        return make(
            SupervisorClassification::HighRiskAuthUnclear,
            RecommendedAction::NotifyUser,
            vec!["PROTECTED_ACTION_WITHOUT_EXACT_AUTHORITY"],
            "INFERRED",
        );
    }
    if report.fresh_progress || report.active_operation {
        let reason = if report.active_operation {
            "ACTIVE_OPERATION_OPEN"
        } else {
            "FRESH_PROGRESS"
        };
        return make(
            SupervisorClassification::HealthyRunning,
            RecommendedAction::None,
            vec![reason],
            "INFERRED",
        );
    }
    if !gaps.is_empty() {
        return make(
            SupervisorClassification::RecoverableInterruption,
            RecommendedAction::ContinueOnce,
            vec!["INCOMPLETE_REQUIRED_COVERAGE"],
            "INFERRED",
        );
    }
    if report.final_answer_fulfills {
        return make(
            SupervisorClassification::Completed,
            RecommendedAction::None,
            vec!["FULFILLING_FINAL_ANSWER"],
            "INFERRED",
        );
    }

    let failed_or_interrupted = matches!(
        report.latest_turn_status,
        TurnStatus::Failed | TurnStatus::Interrupted
    );
    let strong_gap = report.tool_result_gap && report.bounded_observation_unchanged;
    if report.objective_unfinished
        && (failed_or_interrupted || strong_gap || report.assistant_declared_incomplete)
    {
        let reason = if failed_or_interrupted {
            "LATEST_TURN_FAILED_OR_INTERRUPTED"
        } else if strong_gap {
            "TOOL_RESULT_WITHOUT_RESUMPTION"
        } else {
            "ASSISTANT_DECLARED_INCOMPLETE"
        };
        return make(
            SupervisorClassification::RecoverableInterruption,
            RecommendedAction::ContinueOnce,
            vec![reason],
            "INFERRED",
        );
    }

    make(
        SupervisorClassification::SuspectedStall,
        RecommendedAction::ObserveAgain,
        vec!["INSUFFICIENT_RECOVERABLE_EVIDENCE"],
        "MISSING_PROOF",
    )
}

fn validate_key(name: &str, value: &str, allow_empty: bool) -> Result<(), String> {
    if value.is_empty() && allow_empty {
        return Ok(());
    }
    if value.is_empty() || value.len() > MAX_KEY_LEN {
        return Err(format!("{name} must be 1..={MAX_KEY_LEN} characters"));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'))
    {
        return Err(format!(
            "{name} must be an opaque key/digest, not raw conversation text"
        ));
    }
    Ok(())
}

fn validate_report(report: &TaskEvidenceReport) -> Result<(), String> {
    validate_key("threadId", &report.thread_id, false)?;
    validate_key("latestUserTurnKey", &report.latest_user_turn_key, false)?;
    validate_key("unfinishedScopeKey", &report.unfinished_scope_key, true)?;
    if let Some(host_id) = &report.host_id {
        validate_key("hostId", host_id, false)?;
    }
    if let Some(code) = &report.read_error_code {
        validate_key("readErrorCode", code, false)?;
    }
    if report.coverage.len() > 16 {
        return Err("coverage accepts at most 16 surfaces".to_string());
    }
    if report.contract.authorized_actions.len() > 16 {
        return Err("authorizedActions accepts at most 16 entries".to_string());
    }
    Ok(())
}

// ── Persistence ─────────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn report_is_fresh(reported_at_ms: i64, now: i64) -> bool {
    now.saturating_sub(reported_at_ms) <= REPORT_STALE_SECS * 1_000
}

fn state_path() -> PathBuf {
    if let Some(path) =
        std::env::var_os("SIRIN_CODEX_SUPERVISOR_STATE_PATH").filter(|value| !value.is_empty())
    {
        return PathBuf::from(path);
    }
    crate::platform::app_data_dir()
        .join("tracking")
        .join(STATE_FILE)
}

fn load_state(path: &Path) -> Result<SupervisorState, String> {
    if !path.is_file() {
        return Ok(SupervisorState::default());
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read Codex supervisor state {}: {error}", path.display()))?;
    let mut state: SupervisorState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse Codex supervisor state {}: {error}", path.display()))?;
    if state.version != STATE_VERSION {
        return Err(format!(
            "unsupported Codex supervisor state version {}",
            state.version
        ));
    }
    if state.records.len() > MAX_RECORDS {
        state.records.truncate(MAX_RECORDS);
    }
    Ok(state)
}

fn save_state(path: &Path, state: &SupervisorState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "create Codex supervisor state directory {}: {error}",
                parent.display()
            )
        })?;
    }
    let temp = path.with_file_name(format!(
        ".{STATE_FILE}.{}.{}.tmp",
        std::process::id(),
        now_ms()
    ));
    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|error| format!("create Codex supervisor temp state: {error}"))?;
        serde_json::to_writer(&mut file, state)
            .map_err(|error| format!("encode Codex supervisor state: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("finish Codex supervisor state: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync Codex supervisor state: {error}"))?;
        drop(file);
        std::fs::rename(&temp, path)
            .map_err(|error| format!("replace Codex supervisor state: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn with_state_mut<T>(
    apply: impl FnOnce(&mut SupervisorState, i64) -> Result<T, String>,
) -> Result<T, String> {
    let lock = FILE_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
    let path = state_path();
    let mut state = load_state(&path)?;
    let now = now_ms();
    state.records.retain(|record| {
        now.saturating_sub(record.reported_at_ms) <= RECORD_RETENTION_SECS * 1_000
    });
    let output = apply(&mut state, now)?;
    state.updated_at_ms = now;
    state
        .records
        .sort_by_key(|record| Reverse(record.reported_at_ms));
    state.records.truncate(MAX_RECORDS);
    save_state(&path, &state)?;
    Ok(output)
}

fn apply_dedupe(
    record: &TaskRecord,
    mut decision: SupervisorDecision,
    now: i64,
) -> SupervisorDecision {
    if decision.recommended_action != RecommendedAction::ContinueOnce {
        return decision;
    }
    if record.report.continuation_already_visible {
        decision.recommended_action = RecommendedAction::None;
        decision
            .reason_codes
            .push("CONTINUATION_ALREADY_VISIBLE".to_string());
        return decision;
    }
    let duplicate = record.continuation.last_continuation_fingerprint.as_deref()
        == Some(decision.fingerprint.as_str())
        && record
            .continuation
            .last_continuation_at_ms
            .is_some_and(|at| now.saturating_sub(at) <= DEDUPE_WINDOW_SECS * 1_000);
    if duplicate {
        decision.recommended_action = RecommendedAction::None;
        decision
            .reason_codes
            .push("DEDUPE_WINDOW_ACTIVE".to_string());
    }
    decision
}

// ── MCP handlers ────────────────────────────────────────────────────────────

pub(crate) fn call_report(args: Value) -> Result<Value, String> {
    let report: TaskEvidenceReport = serde_json::from_value(args)
        .map_err(|error| format!("invalid supervisor report: {error}"))?;
    validate_report(&report)?;
    with_state_mut(|state, now| {
        let existing = state
            .records
            .iter()
            .position(|record| record.report.thread_id == report.thread_id);
        let mut record = existing
            .map(|idx| state.records.remove(idx))
            .unwrap_or_else(|| TaskRecord {
                report: report.clone(),
                decision: classify(&report),
                reported_at_ms: now,
                continuation: ContinuationState::default(),
            });
        let new_decision = classify(&report);
        if record.decision.fingerprint != new_decision.fingerprint {
            record.continuation.clear_claim();
        }
        record.report = report;
        record.reported_at_ms = now;
        record.decision = apply_dedupe(&record, new_decision, now);
        let response = json!({
            "status": "RECORDED",
            "threadId": record.report.thread_id,
            "decision": record.decision,
            "boundary": "Sirin stored structured local evidence only; no Codex task was messaged, approved, forked, committed, pushed, or deployed."
        });
        state.records.push(record);
        Ok(response)
    })
}

pub(crate) fn call_snapshot(_args: Value) -> Result<Value, String> {
    serde_json::to_value(snapshot()).map_err(|error| format!("encode supervisor snapshot: {error}"))
}

pub(crate) fn call_claim(args: Value) -> Result<Value, String> {
    let requested_thread = serde_json::from_value::<ClaimArgs>(args)
        .map_err(|error| format!("invalid supervisor claim: {error}"))?
        .thread_id;
    if let Some(thread_id) = &requested_thread {
        validate_key("threadId", thread_id, false)?;
    }
    with_state_mut(|state, now| {
        if let Some(existing) = state
            .records
            .iter()
            .find(|record| record.continuation.has_active_claim(now))
        {
            return Ok(serde_json::to_value(ContinuationClaimView {
                status: "CLAIM_ALREADY_ACTIVE".to_string(),
                claim_id: existing.continuation.claim_id.clone(),
                thread_id: Some(existing.report.thread_id.clone()),
                host_id: existing.report.host_id.clone(),
                fingerprint: existing.continuation.claim_fingerprint.clone(),
                prompt: None,
                expires_at_ms: existing.continuation.claim_expires_at_ms,
                boundary: "At most one continuation may be claimed at a time.".to_string(),
            })
            .map_err(|error| format!("encode existing claim: {error}"))?);
        }

        let candidate = state.records.iter_mut().find(|record| {
            requested_thread
                .as_deref()
                .is_none_or(|thread_id| record.report.thread_id == thread_id)
                && record.decision.classification
                    == SupervisorClassification::RecoverableInterruption
                && record.decision.recommended_action == RecommendedAction::ContinueOnce
                && report_is_fresh(record.reported_at_ms, now)
        });
        let Some(record) = candidate else {
            return Ok(serde_json::to_value(ContinuationClaimView {
                status: "NO_ACTION".to_string(),
                claim_id: None,
                thread_id: None,
                host_id: None,
                fingerprint: None,
                prompt: None,
                expires_at_ms: None,
                boundary: "No fresh, dedupe-safe recoverable interruption is available."
                    .to_string(),
            })
            .map_err(|error| format!("encode empty claim: {error}"))?);
        };
        let claim_material = format!(
            "{}|{}|{now}",
            record.report.thread_id, record.decision.fingerprint
        );
        let claim_id = truncated_sha256_hex(&claim_material, 12);
        let expires = now + CLAIM_LEASE_SECS * 1_000;
        record.continuation.claim_id = Some(claim_id.clone());
        record.continuation.claim_fingerprint = Some(record.decision.fingerprint.clone());
        record.continuation.claimed_at_ms = Some(now);
        record.continuation.claim_expires_at_ms = Some(expires);
        Ok(serde_json::to_value(ContinuationClaimView {
            status: "CLAIMED".to_string(),
            claim_id: Some(claim_id),
            thread_id: Some(record.report.thread_id.clone()),
            host_id: record.report.host_id.clone(),
            fingerprint: Some(record.decision.fingerprint.clone()),
            prompt: Some(CONTINUATION_PROMPT.to_string()),
            expires_at_ms: Some(expires),
            boundary: "The claim does not send a message or grant authority. The Codex heartbeat must re-check the target and dispatch with its task tool.".to_string(),
        })
        .map_err(|error| format!("encode continuation claim: {error}"))?)
    })
}

pub(crate) fn call_complete_action(args: Value) -> Result<Value, String> {
    let request = serde_json::from_value::<CompleteActionArgs>(args)
        .map_err(|error| format!("invalid supervisor completion: {error}"))?;
    validate_key("claimId", &request.claim_id, false)?;
    let outcome = DispatchOutcome::parse(&request.outcome)?;
    with_state_mut(|state, now| {
        let record = state
            .records
            .iter_mut()
            .find(|record| {
                record.continuation.claim_id.as_deref() == Some(request.claim_id.as_str())
            })
            .ok_or_else(|| "continuation claim not found".to_string())?;
        let fingerprint = record
            .continuation
            .claim_fingerprint
            .clone()
            .ok_or_else(|| "continuation claim has no fingerprint".to_string())?;
        if !record.continuation.has_active_claim(now) {
            return Err("continuation claim expired".to_string());
        }
        if outcome == DispatchOutcome::Accepted {
            record.continuation.last_continuation_fingerprint = Some(fingerprint);
            record.continuation.last_continuation_at_ms = Some(now);
            record.decision.recommended_action = RecommendedAction::None;
            record
                .decision
                .reason_codes
                .push("CONTINUATION_DISPATCH_ACCEPTED".to_string());
        }
        record.continuation.last_dispatch_status = Some(outcome.as_str().to_string());
        record.continuation.clear_claim();
        Ok(json!({
            "status": "RECORDED",
            "threadId": record.report.thread_id,
            "outcome": outcome,
            "boundary": "Only Sirin-local dispatch metadata was updated."
        }))
    })
}

// ── Snapshot projection ─────────────────────────────────────────────────────

pub fn snapshot() -> SupervisorSnapshot {
    let now = now_ms();
    let loaded = {
        let lock = FILE_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
        load_state(&state_path())
    };
    let (state, source_error) = match loaded {
        Ok(state) => (state, None),
        Err(error) => (SupervisorState::default(), Some(error)),
    };
    let mut counts = BTreeMap::new();
    let mut actionable = 0;
    let mut intervention = 0;
    let mut tasks = Vec::new();
    for record in state.records {
        if !report_is_fresh(record.reported_at_ms, now) {
            continue;
        }
        *counts
            .entry(record.decision.classification.as_str().to_string())
            .or_insert(0) += 1;
        if record.decision.recommended_action == RecommendedAction::ContinueOnce {
            actionable += 1;
        }
        if matches!(
            record.decision.classification,
            SupervisorClassification::UserDecisionRequired
                | SupervisorClassification::HighRiskAuthUnclear
                | SupervisorClassification::ControlStateMismatch
                | SupervisorClassification::UnreadableOrTimeout
        ) {
            intervention += 1;
        }
        let claim_active = record
            .continuation
            .claim_expires_at_ms
            .is_some_and(|expires| expires > now);
        tasks.push(SupervisorTaskView {
            display_name: format!(
                "Codex 工作 {}",
                record.report.thread_id.chars().take(8).collect::<String>()
            ),
            thread_id: record.report.thread_id,
            host_id: record.report.host_id,
            reported_at_ms: record.reported_at_ms,
            age_secs: now.saturating_sub(record.reported_at_ms).max(0) as u64 / 1_000,
            classification: record.decision.classification,
            recommended_action: record.decision.recommended_action,
            reason_codes: record.decision.reason_codes,
            fingerprint: record.decision.fingerprint,
            coverage: record.report.coverage,
            coverage_gaps: record.decision.coverage_gaps,
            human_coverage_gaps: record.decision.human_coverage_gaps,
            claim_active,
            last_dispatch_status: record.continuation.last_dispatch_status,
        });
    }
    tasks.sort_by_key(|task| Reverse(task.reported_at_ms));
    let status = if source_error.is_some() {
        "MISSING_PROOF"
    } else if intervention > 0 {
        "NEEDS_USER"
    } else if actionable > 0 {
        "RECOVERABLE"
    } else if tasks.is_empty() {
        "WAITING_FOR_HEARTBEAT"
    } else {
        "HEALTHY"
    };
    SupervisorSnapshot {
        status: status.to_string(),
        evidence: if source_error.is_some() || tasks.is_empty() {
            "MISSING_PROOF".to_string()
        } else {
            "INFERRED".to_string()
        },
        captured_at: Utc::now().to_rfc3339(),
        last_reported_at_ms: tasks.iter().map(|task| task.reported_at_ms).max(),
        task_count: tasks.len(),
        actionable_count: actionable,
        user_intervention_count: intervention,
        counts,
        tasks,
        policy: SupervisorPolicyView::default(),
        safety: SupervisorSafetyView::default(),
        source_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_report() -> TaskEvidenceReport {
        TaskEvidenceReport {
            thread_id: "01a04100-cfc1-7622-8a6b-826b77bf9ade".to_string(),
            host_id: Some("local".to_string()),
            latest_user_turn_key: "turn-a1".to_string(),
            unfinished_scope_key: "scope-a1".to_string(),
            latest_turn_status: TurnStatus::Active,
            readable: true,
            fresh_progress: false,
            active_operation: false,
            final_answer_fulfills: false,
            objective_unfinished: true,
            waiting_correct_time: false,
            needs_user_decision: false,
            needs_new_approval: false,
            high_risk_next_action: false,
            exact_authority_present: false,
            bounded_observation_unchanged: false,
            tool_result_gap: false,
            assistant_declared_incomplete: false,
            continuation_already_visible: false,
            control_state_mismatch: false,
            read_error_code: None,
            contract: TaskContract::default(),
            coverage: Vec::new(),
        }
    }

    #[test]
    fn fresh_active_task_is_healthy() {
        let mut report = base_report();
        report.fresh_progress = true;
        let decision = classify(&report);
        assert_eq!(
            decision.classification,
            SupervisorClassification::HealthyRunning
        );
        assert_eq!(decision.recommended_action, RecommendedAction::None);
    }

    #[test]
    fn fulfilling_final_answer_is_completed() {
        let mut report = base_report();
        report.latest_turn_status = TurnStatus::Completed;
        report.objective_unfinished = false;
        report.final_answer_fulfills = true;
        let decision = classify(&report);
        assert_eq!(decision.classification, SupervisorClassification::Completed);
    }

    #[test]
    fn clear_interruption_is_recoverable_once() {
        let mut report = base_report();
        report.latest_turn_status = TurnStatus::Interrupted;
        let decision = classify(&report);
        assert_eq!(
            decision.classification,
            SupervisorClassification::RecoverableInterruption
        );
        assert_eq!(decision.recommended_action, RecommendedAction::ContinueOnce);
    }

    #[test]
    fn ambiguous_slow_task_is_only_suspected() {
        let report = base_report();
        let decision = classify(&report);
        assert_eq!(
            decision.classification,
            SupervisorClassification::SuspectedStall
        );
        assert_eq!(decision.recommended_action, RecommendedAction::ObserveAgain);
    }

    #[test]
    fn awaiting_approval_never_auto_continues() {
        let mut report = base_report();
        report.needs_new_approval = true;
        let decision = classify(&report);
        assert_eq!(
            decision.classification,
            SupervisorClassification::UserDecisionRequired
        );
        assert_eq!(decision.recommended_action, RecommendedAction::NotifyUser);
    }

    #[test]
    fn protected_action_without_exact_authority_fails_closed() {
        let mut report = base_report();
        report.high_risk_next_action = true;
        let decision = classify(&report);
        assert_eq!(
            decision.classification,
            SupervisorClassification::HighRiskAuthUnclear
        );
        assert_eq!(decision.recommended_action, RecommendedAction::NotifyUser);
    }

    #[test]
    fn required_ui_gap_is_recoverable() {
        let mut report = base_report();
        report.latest_turn_status = TurnStatus::Completed;
        report.final_answer_fulfills = true;
        report.contract.intent = TaskIntent::DeepCheck;
        report.contract.deep_check_requested = true;
        report.coverage.push(CoverageEvidence {
            surface: CoverageSurface::WebUi,
            status: CoverageStatus::MissingProof,
            required: true,
            human_required: false,
            observed_at_ms: None,
        });
        let decision = classify(&report);
        assert_eq!(
            decision.classification,
            SupervisorClassification::RecoverableInterruption
        );
        assert_eq!(decision.coverage_gaps, vec![CoverageSurface::WebUi]);
    }

    #[test]
    fn physical_device_human_gap_requires_user() {
        let mut report = base_report();
        report.coverage.push(CoverageEvidence {
            surface: CoverageSurface::PhysicalDevice,
            status: CoverageStatus::MissingProof,
            required: true,
            human_required: true,
            observed_at_ms: None,
        });
        let decision = classify(&report);
        assert_eq!(
            decision.classification,
            SupervisorClassification::UserDecisionRequired
        );
        assert_eq!(decision.recommended_action, RecommendedAction::NotifyUser);
    }

    #[test]
    fn unreadable_task_is_not_restarted() {
        let mut report = base_report();
        report.readable = false;
        report.read_error_code = Some("THREAD_NOT_LOADED".to_string());
        let decision = classify(&report);
        assert_eq!(
            decision.classification,
            SupervisorClassification::UnreadableOrTimeout
        );
        assert_eq!(decision.recommended_action, RecommendedAction::FailClosed);
    }

    #[test]
    fn raw_conversation_text_is_rejected_as_key() {
        let mut report = base_report();
        report.latest_user_turn_key = "please deploy this now".to_string();
        assert!(validate_report(&report).is_err());
    }

    #[test]
    fn stale_reports_are_not_current_supervisor_evidence() {
        let now = 10_000_000;
        assert!(report_is_fresh(now - REPORT_STALE_SECS * 1_000, now));
        assert!(!report_is_fresh(now - REPORT_STALE_SECS * 1_000 - 1, now));
    }

    #[test]
    fn dispatch_outcomes_are_typed_and_case_insensitive() {
        assert_eq!(
            DispatchOutcome::parse("accepted"),
            Ok(DispatchOutcome::Accepted)
        );
        assert_eq!(
            DispatchOutcome::parse("TARGET_RUNNING"),
            Ok(DispatchOutcome::TargetRunning)
        );
        assert!(DispatchOutcome::parse("UNKNOWN").is_err());
    }

    #[test]
    fn clearing_a_claim_preserves_dispatch_history() {
        let mut state = ContinuationState {
            claim_id: Some("claim-a1".to_string()),
            claim_fingerprint: Some("fingerprint-a1".to_string()),
            claimed_at_ms: Some(1_000),
            claim_expires_at_ms: Some(2_000),
            last_dispatch_status: Some("ACCEPTED".to_string()),
            ..ContinuationState::default()
        };

        state.clear_claim();

        assert!(!state.has_active_claim(1_500));
        assert_eq!(state.last_dispatch_status.as_deref(), Some("ACCEPTED"));
    }

    #[test]
    fn claim_lease_expires_at_the_boundary() {
        let state = ContinuationState {
            claim_expires_at_ms: Some(2_000),
            ..ContinuationState::default()
        };

        assert!(state.has_active_claim(1_999));
        assert!(!state.has_active_claim(2_000));
    }

    #[test]
    fn persistence_replaces_an_existing_snapshot() {
        let path = std::env::temp_dir().join(format!(
            "sirin_codex_supervisor_replace_{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let mut state = SupervisorState {
            updated_at_ms: 1,
            ..SupervisorState::default()
        };
        save_state(&path, &state).expect("write initial supervisor snapshot");
        state.updated_at_ms = 2;
        save_state(&path, &state).expect("replace supervisor snapshot");

        let restored = load_state(&path).expect("load replaced supervisor snapshot");
        assert_eq!(restored.updated_at_ms, 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fingerprint_is_stable_and_changes_with_coverage() {
        let report = base_report();
        let first = fingerprint(&report);
        assert_eq!(first, fingerprint(&report));
        let mut changed = report;
        changed.coverage.push(CoverageEvidence {
            surface: CoverageSurface::MobileUi,
            status: CoverageStatus::Pending,
            required: true,
            human_required: false,
            observed_at_ms: None,
        });
        assert_ne!(first, fingerprint(&changed));
    }

    #[test]
    fn continuation_already_visible_is_deduped() {
        let mut report = base_report();
        report.latest_turn_status = TurnStatus::Interrupted;
        report.continuation_already_visible = true;
        let decision = classify(&report);
        let record = TaskRecord {
            report,
            decision: decision.clone(),
            reported_at_ms: 1_000,
            continuation: ContinuationState::default(),
        };
        let deduped = apply_dedupe(&record, decision, 2_000);
        assert_eq!(deduped.recommended_action, RecommendedAction::None);
        assert!(deduped
            .reason_codes
            .iter()
            .any(|reason| reason == "CONTINUATION_ALREADY_VISIBLE"));
    }

    #[test]
    fn accepted_continuation_suppresses_same_fingerprint_for_six_hours() {
        let mut report = base_report();
        report.latest_turn_status = TurnStatus::Interrupted;
        let decision = classify(&report);
        let record = TaskRecord {
            report,
            decision: decision.clone(),
            reported_at_ms: 1_000,
            continuation: ContinuationState {
                last_continuation_fingerprint: Some(decision.fingerprint.clone()),
                last_continuation_at_ms: Some(1_000),
                ..ContinuationState::default()
            },
        };
        let deduped = apply_dedupe(&record, decision, 2_000);
        assert_eq!(deduped.recommended_action, RecommendedAction::None);
        assert!(deduped
            .reason_codes
            .iter()
            .any(|reason| reason == "DEDUPE_WINDOW_ACTIVE"));
    }

    #[test]
    fn completed_tool_result_gap_requires_bounded_unchanged_observation() {
        let mut report = base_report();
        report.tool_result_gap = true;
        let first = classify(&report);
        assert_eq!(
            first.classification,
            SupervisorClassification::SuspectedStall
        );
        report.bounded_observation_unchanged = true;
        let second = classify(&report);
        assert_eq!(
            second.classification,
            SupervisorClassification::RecoverableInterruption
        );
    }
}
